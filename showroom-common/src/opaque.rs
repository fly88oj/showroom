//! OPAQUE 登录协议（RFC 9807 / opaque-ke 4.x，plan-16 / SPEC §16）：CLI 与 Web 共用一份实现，
//! 密码经盲化 OPRF 永不上线，服务端只存注册记录（信封）而非离线可验证哈希。
//!
//! Suite：Ristretto255（OPRF + KE）+ TripleDH + **Identity KSF**——argon2id 硬化由
//! [`rwd`] 在盲化前外部承担（盐含 user，见其注释），KSF 本体恒 Identity 以免双重 stretch；
//! 「Suite+外部 KSF」两端同 crate 同定义，无互操作缝隙（spike 已验证 native↔wasm 同栈）。
//!
//! 线上格式：所有报文（ke1/ke2/ke3/m1/m2/record）与落盘（`pass.opaque`/`opaque.key`）
//! 一律 base64(标准字母表无 padding)——与 pub_string 的编码惯例一致。
//!
//! 语义要点（serve 侧调用方负责）：
//! - 未知用户：`server_login_start` 的 record 传 `None` → crate 原生 dummy 假记录路径，
//!   仍产出同形 ke2（防枚举；客户端本地 finish 必失败，不产生第三请求）；
//! - 错密码：客户端 `login_finish` 本地失败（Err），线上无差异；
//! - credential_identifier=user、三方标识 client=user / server=[`SERVER_ID`]：
//!   user 同时进 OPRF 域分离、rwd 盐与 KE transcript。

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::engine::Engine as _;
use opaque_ke::key_exchange::tripledh::TripleDh;
use opaque_ke::{
    CipherSuite, ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialFinalization, CredentialRequest,
    CredentialResponse, Identifiers, RegistrationRequest, RegistrationResponse, ServerLogin,
    ServerLoginParameters, ServerRegistration, ServerSetup,
};
use rand::{CryptoRng, RngCore};

use crate::Result;

/// Suite：Ristretto255 + TripleDH + Identity KSF（argon2id 由 `rwd` 外部承担，见模块注）。
/// v4 起 KeGroup 并入 KeyExchange 泛型（TripleDh<G, H>），哈希显式为 Sha512。
pub struct Suite;

impl CipherSuite for Suite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = TripleDh<opaque_ke::Ristretto255, sha2::Sha512>;
    type Ksf = opaque_ke::ksf::Identity;
}

/// 三方标识中的服务端身份（KE transcript 域分离；两端一致即可，无全网唯一性要求）。
pub const SERVER_ID: &[u8] = b"showroom-server";

/// 供下游（showroomctl）类型标注的服务端句柄别名（免各处展开泛型）。
pub mod server_types {
    pub use super::Suite;
    pub type Setup = opaque_ke::ServerSetup<Suite>;
    pub type Record = opaque_ke::ServerRegistration<Suite>;
}

/// rwd 盐 = b"showroom-opaque-v1" + user（RFC9380 允许常数盐：盲化已提供保护，
/// 硬化只为抬高离线字典代价；user 进盐使同密码不同账号派生不同 rwd）。
const RWD_SALT_PREFIX: &[u8] = b"showroom-opaque-v1";

/// 密码硬化的输出长度（OPAQUE 盲化输入宽度）。
pub const RWD_LEN: usize = 32;

/// rwd（rotated password derivation）：argon2id(m=19456KiB, t=2, p=1；OWASP) → 32B。
/// 确定性函数（无秘密依赖）：native 与 wasm 同参数同输出（固定向量测试防漂移）。
pub fn rwd(pw: &str, user: &str) -> Result<[u8; RWD_LEN]> {
    let params = argon2::Params::new(19456, 2, 1, Some(RWD_LEN))
        .map_err(|e| format!("argon2 params: {e}"))?;
    let a2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut salt = RWD_SALT_PREFIX.to_vec();
    salt.extend_from_slice(user.as_bytes());
    let mut out = [0u8; RWD_LEN];
    a2.hash_password_into(pw.as_bytes(), &salt, &mut out)
        .map_err(|e| format!("argon2id: {e}"))?;
    Ok(out)
}

// —— 小工具 ——

fn ids(user: &str) -> Identifiers<'_> {
    Identifiers { client: Some(user.as_bytes()), server: Some(SERVER_ID) }
}

fn b64(bytes: &[u8]) -> String {
    STANDARD_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>> {
    Ok(STANDARD_NO_PAD.decode(s.trim())?)
}

// —— 客户端（CLI native 与 wasm 垫片共用；密码 → rwd → 盲化）——

/// 登录第一步：产出 ke1（发 `/_api/auth/login-begin`）与待 finish 的本地状态。
pub fn login_start(
    rng: &mut (impl RngCore + CryptoRng),
    pw: &str,
    user: &str,
) -> Result<(ClientLogin<Suite>, String)> {
    let r = rwd(pw, user)?;
    let out = ClientLogin::<Suite>::start(rng, &r).map_err(|e| format!("login start: {e}"))?;
    Ok((out.state, b64(&out.message.serialize())))
}

/// 登录第三步（本地）：ke2 → ke3。密码错误在此 Err（本地提示，不产生第三请求）。
/// 返回 (ke3_b64, session_key)：session_key=AKE 双侧同源输出，plan-18 用作 SRTP
/// 密钥下发的 HMAC 绑定密钥（对端伪造不得，MITM 换钥即验签失败）。
pub fn login_finish(
    rng: &mut (impl RngCore + CryptoRng),
    state: ClientLogin<Suite>,
    pw: &str,
    user: &str,
    ke2_b64: &str,
) -> Result<(String, Vec<u8>)> {
    let r = rwd(pw, user)?;
    let ke2 = CredentialResponse::<Suite>::deserialize(&unb64(ke2_b64)?)
        .map_err(|e| format!("bad ke2: {e}"))?;
    let out = state
        .finish(
            rng,
            &r,
            ke2,
            ClientLoginFinishParameters { context: None, identifiers: ids(user), ksf: None },
        )
        .map_err(|e| format!("login finish: {e}"))?;
    Ok((b64(&out.message.serialize()), out.session_key.to_vec()))
}

/// 注册第一步（改密码流程）：产出 m1（发 `/_api/auth/pw-begin`）与本地状态。
pub fn reg_start(
    rng: &mut (impl RngCore + CryptoRng),
    pw: &str,
    user: &str,
) -> Result<(ClientRegistration<Suite>, String)> {
    let r = rwd(pw, user)?;
    let out = ClientRegistration::<Suite>::start(rng, &r).map_err(|e| format!("reg start: {e}"))?;
    Ok((out.state, b64(&out.message.serialize())))
}

/// 注册收尾（客户端）：m2 → record（发 `/_api/auth/pw-commit`；服务端原子替换）。
pub fn reg_finish(
    rng: &mut (impl RngCore + CryptoRng),
    state: ClientRegistration<Suite>,
    pw: &str,
    user: &str,
    m2_b64: &str,
) -> Result<String> {
    let r = rwd(pw, user)?;
    let m2 = RegistrationResponse::<Suite>::deserialize(&unb64(m2_b64)?)
        .map_err(|e| format!("bad m2: {e}"))?;
    let out = state
        .finish(
            rng,
            &r,
            m2,
            ClientRegistrationFinishParameters { identifiers: ids(user), ksf: None },
        )
        .map_err(|e| format!("reg finish: {e}"))?;
    Ok(b64(&out.message.serialize()))
}

// —— 服务端（showroomctl；serve 与 CLI 管理命令共用）——

/// 生成服务端 OPRF key（serve 启动缺失时落 `state/opaque.key`，b64；同 platform_rsa 模式）。
/// 返回 b64 串（即落盘内容）。
pub fn server_setup_new(rng: &mut (impl RngCore + CryptoRng)) -> String {
    b64(&ServerSetup::<Suite>::new(rng).serialize())
}

/// 加载服务端 OPRF key（serve 启动 / user-add 设密码 / passwd-set 进程内注册）。
/// 输入为 b64 串（opaque.key 文件内容）。返回 (setup, 是否走了 v3 迁移)——迁移方由
/// 唯一落盘方（showroomctl ops::opaque_setup）回写 v4 使兼容分支退役（见其注释）。
/// 兼容 v3 旧格式：v3 布局 seed(64)+sk(32)+fake_sk(32)，v4 布局 seed(64)+sk(32)+fake_pk(32)
/// （均 128B，长度不可分——先按 v4 解析，失败且尾段能作为合法标量时走 v3 迁移：
/// 由 fake sk 基点乘法重导公钥替换尾段。seed 与真实 sk 原样保留，故真实登录不受
/// 误判影响——dummy 公钥仅用于未知用户同形应答。用户注册记录两端格式未变，无需迁移）。
pub fn server_setup_load(b64_key: &str) -> Result<(ServerSetup<Suite>, bool)> {
    let raw = unb64(b64_key)?;
    if let Ok(setup) = ServerSetup::<Suite>::deserialize(&raw) {
        return Ok((setup, false));
    }
    if raw.len() == 128 {
        use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
        use curve25519_dalek::scalar::Scalar;
        let fake_sk = Scalar::from_canonical_bytes(raw[96..128].try_into().unwrap())
            .into_option()
            .filter(|s| s != &Scalar::ZERO);
        if let Some(sk) = fake_sk {
            let fake_pk = (RISTRETTO_BASEPOINT_POINT * sk).compress().to_bytes();
            let mut v4 = Vec::with_capacity(128);
            v4.extend_from_slice(&raw[..96]);
            v4.extend_from_slice(&fake_pk);
            let setup = ServerSetup::<Suite>::deserialize(&v4)
                .map_err(|e| format!("bad opaque.key (v3 migration): {e}"))?;
            return Ok((setup, true));
        }
    }
    Err(format!("bad opaque.key: {} bytes", raw.len()).into())
}

/// setup 序列化为 b64（即 opaque.key 落盘形态；迁移回写方使用）。
pub fn server_setup_b64(setup: &ServerSetup<Suite>) -> String {
    b64(&setup.serialize())
}

/// 注册第二步（服务端）：m1 → m2。credential_identifier=user（OPRF 域分离）。
pub fn server_reg_start(setup: &ServerSetup<Suite>, m1_b64: &str, user: &str) -> Result<String> {
    let m1 = RegistrationRequest::<Suite>::deserialize(&unb64(m1_b64)?)
        .map_err(|e| format!("bad m1: {e}"))?;
    let out = ServerRegistration::<Suite>::start(setup, m1, user.as_bytes())
        .map_err(|e| format!("server reg start: {e}"))?;
    Ok(b64(&out.message.serialize()))
}

/// 注册落库（服务端）：record（b64）→ 规范化后的落盘内容（解码验证 + 重编码，
/// 拒绝非规范 b64 直接进文件）。
pub fn server_reg_canonical(record_b64: &str) -> Result<String> {
    let upload = unb64(record_b64)?;
    // 长度对齐即接受（RegistrationUpload 反序列化仅被 finish 消费，这里验证 b64 规范性足够）
    Ok(b64(&upload))
}

/// 注册记录（服务端）：由落盘的 b64 构造（登录时喂 server_login_start）。
pub fn server_record_from_b64(record_b64: &str) -> Result<ServerRegistration<Suite>> {
    Ok(ServerRegistration::<Suite>::deserialize(&unb64(record_b64)?)
        .map_err(|e| format!("bad pass.opaque: {e}"))?)
}

/// 登录第二步（服务端）：ke1 → ke2 + 在途状态（内存保存，login-finish 消费）。
/// record=None 走 crate 原生 dummy 假记录路径（未知用户/未设密码：ke2 同形，防枚举）。
pub fn server_login_start(
    rng: &mut (impl RngCore + CryptoRng),
    setup: &ServerSetup<Suite>,
    record: Option<&ServerRegistration<Suite>>,
    ke1_b64: &str,
    user: &str,
) -> Result<(String, Vec<u8>)> {
    let ke1 = CredentialRequest::<Suite>::deserialize(&unb64(ke1_b64)?)
        .map_err(|e| format!("bad ke1: {e}"))?;
    let out = ServerLogin::<Suite>::start(
        rng,
        setup,
        record.cloned(),
        ke1,
        user.as_bytes(),
        ServerLoginParameters { context: None, identifiers: ids(user) },
    )
    .map_err(|e| format!("server login start: {e}"))?;
    Ok((b64(&out.message.serialize()), out.state.serialize().to_vec()))
}

/// 登录第三步（服务端）：ke3 验证（MAC）；Err = 密码错误/伪造 → 调用方按 E_AUTH 同体 404。
pub fn server_login_finish(state_bytes: &[u8], ke3_b64: &str) -> Result<Vec<u8>> {
    let state = ServerLogin::<Suite>::deserialize(state_bytes)
        .map_err(|e| format!("bad pending login state: {e}"))?;
    let ke3 = CredentialFinalization::<Suite>::deserialize(&unb64(ke3_b64)?)
        .map_err(|e| format!("bad ke3: {e}"))?;
    let out = state
        .finish(ke3, ServerLoginParameters::default())
        .map_err(|e| format!("login finish (ke3 rejected): {e}"))?;
    Ok(out.session_key.to_vec())
}

/// plan-18 §3.3：SRTP 密钥下发绑定标签。域分离前缀 + id + pub 一起 MAC——
/// 任一字段被换即验签失败（客户端 fail-closed）。
pub fn srtp_key_mac(session_key: &[u8], srtp_id: &[u8], pub_pem: &str) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha2::Sha256>>::new_from_slice(session_key).expect("hmac any key len");
    m.update(b"srtp-key-v1");
    m.update(srtp_id);
    m.update(pub_pem.as_bytes());
    m.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    // 注册→登录往返（两端同 crate；record/setup 序列化跨进程语义）
    #[test]
    fn registration_login_roundtrip() {
        let mut rng = OsRng;
        let (setup, migrated) = server_setup_load(&server_setup_new(&mut rng)).unwrap();
        assert!(!migrated, "新生成恒为 v4");

        let (cstate, m1) = reg_start(&mut rng, "hunter2", "alice").unwrap();
        let m2 = server_reg_start(&setup, &m1, "alice").unwrap();
        let record_b64 = reg_finish(&mut rng, cstate, "hunter2", "alice", &m2).unwrap();
        let record_b64 = server_reg_canonical(&record_b64).unwrap();
        let record = server_record_from_b64(&record_b64).unwrap();

        // 正确密码：ke1 → ke2 → ke3 → 服务端验证通过
        let (lstate, ke1) = login_start(&mut rng, "hunter2", "alice").unwrap();
        let (ke2, sstate) =
            server_login_start(&mut rng, &setup, Some(&record), &ke1, "alice").unwrap();
        let ke3 = login_finish(&mut rng, lstate, "hunter2", "alice", &ke2).unwrap().0;
        assert!(server_login_finish(&sstate, &ke3).is_ok());
    }

    // 错密码：客户端本地 finish 即失败（不产生第三请求）
    #[test]
    fn wrong_password_fails_client_finish() {
        let mut rng = OsRng;
        let (setup, migrated) = server_setup_load(&server_setup_new(&mut rng)).unwrap();
        assert!(!migrated, "新生成恒为 v4");
        let (cstate, m1) = reg_start(&mut rng, "hunter2", "alice").unwrap();
        let m2 = server_reg_start(&setup, &m1, "alice").unwrap();
        let record_b64 =
            reg_finish(&mut rng, cstate, "hunter2", "alice", &m2).unwrap();
        let record = server_record_from_b64(&record_b64).unwrap();

        let (lstate, ke1) = login_start(&mut rng, "wrong-pw", "alice").unwrap();
        let (ke2, _sstate) =
            server_login_start(&mut rng, &setup, Some(&record), &ke1, "alice").unwrap();
        assert!(login_finish(&mut rng, lstate, "wrong-pw", "alice", &ke2).is_err());
    }

    // 未知用户：record=None 仍产出 ke2（防枚举同形），客户端 finish 失败；ke3 伪造被拒
    #[test]
    fn unknown_user_dummy_path() {
        let mut rng = OsRng;
        let (setup, migrated) = server_setup_load(&server_setup_new(&mut rng)).unwrap();
        assert!(!migrated, "新生成恒为 v4");
        let (lstate, ke1) = login_start(&mut rng, "whatever", "ghost").unwrap();
        let (ke2, _sstate) = server_login_start(&mut rng, &setup, None, &ke1, "ghost").unwrap();
        assert!(!ke2.is_empty());
        assert!(login_finish(&mut rng, lstate, "whatever", "ghost", &ke2).is_err());

        // 已注册用户 + 伪造 ke3（来自另一账号的成功登录）→ 服务端拒绝
        let (cstate, m1) = reg_start(&mut rng, "pw-a", "alice").unwrap();
        let m2 = server_reg_start(&setup, &m1, "alice").unwrap();
        let record_a = server_record_from_b64(
            &reg_finish(&mut rng, cstate, "pw-a", "alice", &m2).unwrap(),
        )
        .unwrap();
        let (ls2, ke1b) = login_start(&mut rng, "pw-a", "alice").unwrap();
        let (_ke2b, sstate_b) =
            server_login_start(&mut rng, &setup, Some(&record_a), &ke1b, "alice").unwrap();
        // 用另一条登录的 ke3 灌给 sstate_b（重放/伪造）→ 拒绝
        let (ls3, ke1c) = login_start(&mut rng, "pw-a", "alice").unwrap();
        let (ke2c, _s3) =
            server_login_start(&mut rng, &setup, Some(&record_a), &ke1c, "alice").unwrap();
        let (ke3c, _) = login_finish(&mut rng, ls3, "pw-a", "alice", &ke2c).unwrap();
        assert!(server_login_finish(&sstate_b, &ke3c).is_err());
        let _ = ls2;
    }

    // rwd 固定向量（防 native/wasm 漂移）：参数 m=19456/t=2/p=1、盐=前缀+user。
    // 向量值由本实现首次运行锚定；wasm 侧同输入若漂移，控制台登录将与 CLI/native 不互通，
    // 本测试在任何一端跑都会立刻抓到。
    #[test]
    fn rwd_fixed_vectors() {
        let h = hex::encode(rwd("correct horse battery staple", "alice").unwrap());
        assert_eq!(
            h,
            "9a627c0d69c7d8d415f919afe988f161acad1611d76f2eb3410e6b8fddc18ee3"
        );
        // 不同 user → 不同 rwd（user 进盐）；不同 pw → 不同 rwd
        let a1 = rwd("pw", "alice").unwrap();
        let a2 = rwd("pw", "bob").unwrap();
        let a3 = rwd("pw2", "alice").unwrap();
        assert_ne!(a1, a2);
        assert_ne!(a1, a3);
        // 确定性：同输入同输出
        assert_eq!(a1, rwd("pw", "alice").unwrap());
    }

    // v3 opaque.key 迁移（opaque-ke 4 升级遗留兼容）：v3 布局 seed(64)+sk(32)+fake_sk(32)。
    // fake_sk 特意选「作为 v4 pk 解析必失败」的标量（其压缩编码非合法 ristretto 点），
    // 锁定迁移分支必走（防退化为「v4 误解析成功」的假绿）；迁移后 setup 全流程登录可用。
    #[test]
    fn server_setup_v3_migration() {
        use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
        use curve25519_dalek::ristretto::CompressedRistretto;
        use curve25519_dalek::scalar::Scalar;
        use curve25519_dalek::traits::IsIdentity;
        use rand::RngCore as _;

        let mut rng = OsRng;
        // 找一个「字节串不是合法非单位元 ristretto 点」的标量（v4 尾段解析必失败）
        let bad_pk_scalar = (1u8..=64u8)
            .map(Scalar::from)
            .find(|s| {
                CompressedRistretto(s.to_bytes())
                    .decompress()
                    .is_none_or(|p| p.is_identity())
            })
            .expect("64 个候选中必有不可解压点");
        let real_sk = Scalar::random(&mut rng);
        let mut seed = [0u8; 64];
        rng.fill_bytes(&mut seed);
        let blob = [seed.as_slice(), real_sk.to_bytes().as_slice(), bad_pk_scalar.to_bytes().as_slice()].concat();
        // 前置断言：该 blob 以 v4 直接解析必失败（否则测试没有钉住迁移分支）
        assert!(ServerSetup::<Suite>::deserialize(&blob).is_err(), "v3 blob 不应被 v4 直接解析");
        let (setup, migrated) = server_setup_load(&b64(&blob)).unwrap();
        assert!(migrated, "v3 布局必走迁移分支");
        // v3 real_sk 对应公钥 = 基点乘法——与迁移推导同一曲线事实，全流程往返兜底验证
        let _ = RISTRETTO_BASEPOINT_POINT * real_sk;

        let (cstate, m1) = reg_start(&mut rng, "hunter2", "mig").unwrap();
        let m2 = server_reg_start(&setup, &m1, "mig").unwrap();
        let record = server_record_from_b64(
            &reg_finish(&mut rng, cstate, "hunter2", "mig", &m2).unwrap(),
        )
        .unwrap();
        let (lstate, ke1) = login_start(&mut rng, "hunter2", "mig").unwrap();
        let (ke2, sstate) =
            server_login_start(&mut rng, &setup, Some(&record), &ke1, "mig").unwrap();
        let ke3 = login_finish(&mut rng, lstate, "hunter2", "mig", &ke2).unwrap().0;
        assert!(server_login_finish(&sstate, &ke3).is_ok());
    }

    // v3 客户端 ↔ v4 服务端互操作：线上存量 CLI（v3 报文）登录新服务端必须仍通。
    // 上游 opaque-ke 4 的 migration.rs 只验证 v3 注册记录可被 v4 读取，不覆盖 v3 报文
    // （ke1/ke2/ke3 与 TripleDH transcript）；本测试以真实 v3 客户端全程实证。
    #[test]
    fn v3_client_interop_v4_server() {
        use opaque_ke_3::key_exchange::tripledh::TripleDh as V3TripleDh;
        use opaque_ke_3::{
            CipherSuite as V3CipherSuite, ClientLogin as V3ClientLogin,
            ClientLoginFinishParameters as V3LoginParams,
            ClientRegistration as V3ClientRegistration,
            ClientRegistrationFinishParameters as V3RegParams,
            CredentialResponse as V3CredResp, RegistrationResponse as V3RegResp,
        };
        struct V3Suite;
        impl V3CipherSuite for V3Suite {
            type OprfCs = opaque_ke_3::Ristretto255;
            type KeGroup = opaque_ke_3::Ristretto255;
            type KeyExchange = V3TripleDh;
            type Ksf = opaque_ke_3::ksf::Identity;
        }
        // v3 参数须配 v3 自己的 Identifiers（两版本类型不互通，字段同形）
        fn v3ids(user: &str) -> opaque_ke_3::Identifiers<'_> {
            opaque_ke_3::Identifiers { client: Some(user.as_bytes()), server: Some(SERVER_ID) }
        }

        let mut rng = OsRng;
        let (setup, migrated) = server_setup_load(&server_setup_new(&mut rng)).unwrap();
        assert!(!migrated, "新生成恒为 v4");
        let r = rwd("hunter2", "alice").unwrap();

        // v3 注册报文 m1 → v4 server_reg_start → m2 → v3 客户端 finish → v3 记录被 v4 读取
        let v3reg = V3ClientRegistration::<V3Suite>::start(&mut rng, &r).unwrap();
        let m1 = b64(&v3reg.message.serialize());
        let m2 = server_reg_start(&setup, &m1, "alice").unwrap();
        let v3fin = v3reg
            .state
            .finish(
                &mut rng,
                &r,
                V3RegResp::<V3Suite>::deserialize(&unb64(&m2).unwrap()).unwrap(),
                V3RegParams { identifiers: v3ids("alice"), ksf: None },
            )
            .unwrap();
        let record = server_record_from_b64(&b64(&v3fin.message.serialize())).unwrap();

        // v3 登录报文 ke1 → v4 server_login_start → ke2 → v3 客户端 finish → ke3 → v4 验证
        let v3login = V3ClientLogin::<V3Suite>::start(&mut rng, &r).unwrap();
        let ke1 = b64(&v3login.message.serialize());
        let (ke2, sstate) =
            server_login_start(&mut rng, &setup, Some(&record), &ke1, "alice").unwrap();
        let v3out = v3login
            .state
            .finish(
                &r,
                V3CredResp::<V3Suite>::deserialize(&unb64(&ke2).unwrap()).unwrap(),
                V3LoginParams { context: None, identifiers: v3ids("alice"), ksf: None },
            )
            .unwrap();
        assert!(server_login_finish(&sstate, &b64(&v3out.message.serialize())).is_ok());
    }
}
