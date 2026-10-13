//! showroom-common：showroomctl / showroom / 本 crate 共享的协议契约层。
//! 实现 SPEC.md §1（SR-SIG1 签名认证）与 §2（加密信封 v1）。公共 API 见 README.md。

// wasm32 垫片（src/wasm_shim.rs）需裸指针 C-ABI，其余目标维持 forbid(unsafe_code)
#![cfg_attr(not(target_arch = "wasm32"), forbid(unsafe_code))]

use std::fmt;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
// wasm32-unknown-unknown 非 unix family，无该模块（save_seed/save_pem 的 0600 权限仅在原生生效）
#[cfg(not(target_arch = "wasm32"))]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::engine::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey, LineEnding};
#[cfg(not(target_arch = "wasm32"))]
use rsa::pkcs8::EncodePrivateKey;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

/// 下游直接复用 common 锁定的密码学依赖，避免另引版本导致类型不兼容。
pub use {ed25519_dalek, rsa};

/// wasm32 控制台 C-ABI 垫片（SPEC §8.3）；文件内以 `#![cfg(target_arch = "wasm32")]` 整体隔离。
pub mod opaque;
pub mod wasm_shim;

// wasm32：getrandom 在该目标无默认后端（"js" feature 需 wasm-bindgen，SPEC §8.3 禁引），
// 注册全局熵池为自定义后端——仅满足链接（lib.rs 里的 OsRng 路径垫片从不调用）；
// 垫片随机性一律走 StdRng（熵由 JS 经 sr_entropy 注入）。原生构建不启用 custom feature。
#[cfg(target_arch = "wasm32")]
getrandom::register_custom_getrandom!(wasm_shim::custom_getrandom);

/// 密钥文件读写/解析类操作的统一错误（seal/open 用 std::io::Result，verify 用 AuthError）。
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// SR-SIG1 时间戳容差：|now − ts| ≤ 300s（SPEC §1）。
pub const SIG_WINDOW_SECS: i64 = 300;

// —— 信封 v1 常量（SPEC §2；pub 供 wasm 垫片与控制台 JS 长度核算复用）——
pub const KEY_LEN: usize = 32; // AES-256
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const SEG: usize = 4 * 1024 * 1024; // 每段明文 4MiB
pub const WRAP_LEN: usize = 384; // RSA-3072 模长

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// 流式 sha256（64KiB 缓冲）：大文件先哈希再签名（sign_prehashed），避免整文件进内存。
pub fn sha256_hex_stream(mut r: impl Read) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// SR-SIG1 nonce：16 随机字节 hex（SPEC §1 nonce 由调用方生成）。
pub fn nonce_hex() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// 0600 落盘（覆盖旧文件偏宽权限），父目录惰性创建。凭据/密钥类文件统一走这里。
#[cfg(not(target_arch = "wasm32"))]
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    // 覆盖已存在文件可能偏宽的旧权限
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

// ———— 名字校验（SPEC §4 + §11 v1.4：project 正则放宽、随机项目名；辅助名沿用旧正则）————

pub mod names {
    /// project 正则（§11 放宽）：`^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$`（总长 1..=31，ASCII，
    /// 手写避免 regex 依赖）。大写放开提升随机名空间（62^8）与字典遍历门槛。
    pub fn valid_project(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 31
            && b[0].is_ascii_alphanumeric()
            && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-')
    }

    /// 辅助名字（user 等）沿用旧正则：`^[a-z0-9][a-z0-9-]{0,30}$`。
    /// user 进 registry 镜像路径与 htpasswd，保持小写（docker 仓库名禁大写）。
    pub fn valid_aux(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 31
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    }

    /// docker tag 规则：首字符 [A-Za-z0-9_]，其余 [A-Za-z0-9_.-]，总长 ≤128。
    pub fn valid_tag(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 128
            && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
            && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    }

    pub const RESERVED: [&str; 4] = ["v2", "_api", "_bootstrap", "_console"];

    /// 辅助名（user 等）仅正则（保留名不约束）。
    pub fn check_aux(s: &str) -> Result<(), String> {
        if valid_aux(s) {
            Ok(())
        } else {
            Err(format!("非法名字 '{s}'：须匹配 ^[a-z0-9][a-z0-9-]{{0,30}}$"))
        }
    }

    /// project = §11 放宽正则 + 保留名。
    pub fn check_project(s: &str) -> Result<(), String> {
        if !valid_project(s) {
            return Err(format!("非法 project '{s}'：须匹配 ^[a-zA-Z0-9][a-zA-Z0-9-]{{0,30}}$"));
        }
        if RESERVED.contains(&s) {
            return Err(format!("project '{s}' 是保留名"));
        }
        Ok(())
    }

    /// 随机项目名（§11 发布入口默认预填）：`[a-zA-Z0-9]{8}`。
    pub fn random_project() -> String {
        use rand::distributions::Alphanumeric;
        use rand::Rng;
        rand::rngs::OsRng
            .sample_iter(Alphanumeric)
            .take(8)
            .map(char::from)
            .collect()
    }

    pub fn check_tag(s: &str) -> Result<(), String> {
        if valid_tag(s) {
            Ok(())
        } else {
            Err(format!("非法 tag '{s}'：首字符字母/数字/_，仅 [A-Za-z0-9_.-]，≤128 字符"))
        }
    }
}

// ———— SR-SIG1（SPEC §1）————

pub struct UserKey(SigningKey);

impl UserKey {
    pub fn generate() -> Self {
        let mut seed = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut seed);
        UserKey(SigningKey::from_bytes(&seed))
    }

    /// 读取裸 32B seed 文件（`save_seed` 写出的格式）。
    pub fn from_seed_file(path: &Path) -> Result<Self> {
        let seed = fs::read(path)?;
        let seed: [u8; KEY_LEN] = seed.as_slice().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "seed file must be exactly 32 bytes")
        })?;
        Ok(UserKey(SigningKey::from_bytes(&seed)))
    }

    /// 写裸 32B seed（0600；wasm32 无文件系统语义，仅原生提供）。
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_seed(&self, path: &Path) -> Result<()> {
        write_private(path, &self.0.to_bytes())?;
        Ok(())
    }

    /// "ed25519:<base64(pubkey)>"（base64 无 padding）。
    pub fn pub_string(&self) -> String {
        format!(
            "ed25519:{}",
            STANDARD_NO_PAD.encode(self.0.verifying_key().as_bytes())
        )
    }

    pub fn from_pub_string(s: &str) -> Result<VerifyingKey> {
        let raw = STANDARD_NO_PAD
            .decode(s.strip_prefix("ed25519:").ok_or("missing ed25519: prefix")?)?;
        let raw: [u8; KEY_LEN] = raw
            .as_slice()
            .try_into()
            .map_err(|_| "public key must be exactly 32 bytes")?;
        Ok(VerifyingKey::from_bytes(&raw)?)
    }
}

/// canonical = "SR-SIG1\n<user>\n<ts>\n<nonce>\n<METHOD>\n<path_with_query>\n<sha256_hex(body)>"。
pub fn build_canonical(
    user: &str,
    ts: i64,
    nonce: &str,
    method: &str,
    path_with_query: &str,
    body_sha256_hex: &str,
) -> String {
    format!("SR-SIG1\n{user}\n{ts}\n{nonce}\n{method}\n{path_with_query}\n{body_sha256_hex}")
}

/// 生成完整 Authorization 值 `SR-SIG1 <user>:<ts>:<nonce>:<sig_b64url>`（sig 为 base64url 无 padding）。
/// nonce 由调用方生成（nonce_hex）；method 传大写。
pub fn sign(
    user: &str,
    key: &UserKey,
    method: &str,
    path_with_query: &str,
    body: &[u8],
    now_unix: i64,
    nonce: &str,
) -> String {
    sign_prehashed(user, key, method, path_with_query, &sha256_hex(body), now_unix, nonce)
}

/// 预哈希签名：body 哈希由调用方算好（sha256_hex/ sha256_hex_stream），
/// 与 sign 同格式——大文件流式上传先哈希再签，避免整文件进内存。
pub fn sign_prehashed(
    user: &str,
    key: &UserKey,
    method: &str,
    path_with_query: &str,
    body_sha256_hex: &str,
    ts: i64,
    nonce: &str,
) -> String {
    let canonical = build_canonical(user, ts, nonce, method, path_with_query, body_sha256_hex);
    let sig = key.0.sign(canonical.as_bytes()).to_bytes();
    format!("SR-SIG1 {user}:{ts}:{nonce}:{}", URL_SAFE_NO_PAD.encode(sig))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub user: String,
    pub ts: i64,
    pub nonce: String,
    /// 命中钥匙在 pubkeys 中的索引（Design#4：调用方据此直接定位 kid，无需逐把复验）。
    pub key_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Format,
    Stale,
    BadSignature,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AuthError::Format => "E_AUTH: malformed SR-SIG1 header",
            AuthError::Stale => "E_AUTH: timestamp outside ±300s window",
            AuthError::BadSignature => "E_AUTH: signature verification failed",
        })
    }
}

impl std::error::Error for AuthError {}

/// 验证 SR-SIG1：格式解析 → ±300s 时间窗 → 依次尝试登记公钥（user 可多把，任一命中即过，
/// `Verified.key_idx` 指明命中的是哪一把——调用方定位 kid 无需复验）。
/// nonce 查重（per-user LRU，SPEC §1）由调用方按返回的 `Verified.nonce` 自行完成。
pub fn verify(
    auth_value: &str,
    pubkeys: &[VerifyingKey],
    method: &str,
    path_with_query: &str,
    body: &[u8],
    now_unix: i64,
) -> std::result::Result<Verified, AuthError> {
    verify_with_hash(auth_value, pubkeys, method, path_with_query, &sha256_hex(body), now_unix)
}

/// 预计算哈希版验签：与 verify 完全同语义（格式 → ±300s → 任一登记公钥命中），
/// body 只认 sha256_hex——import 等只落盘+哈希、不留 body 本体的调用方复用。
pub fn verify_with_hash(
    auth_value: &str,
    pubkeys: &[VerifyingKey],
    method: &str,
    path_q: &str,
    body_sha_hex: &str,
    now: i64,
) -> std::result::Result<Verified, AuthError> {
    let rest = auth_value.strip_prefix("SR-SIG1 ").ok_or(AuthError::Format)?;
    let mut parts = rest.splitn(4, ':');
    let (user, ts, nonce, sig_b64) = match (parts.next(), parts.next(), parts.next(), parts.next())
    {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return Err(AuthError::Format),
    };
    if user.is_empty() || nonce.is_empty() {
        return Err(AuthError::Format);
    }
    let ts: i64 = ts.parse().map_err(|_| AuthError::Format)?;
    let raw = URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| AuthError::Format)?;
    let sig_arr: [u8; 64] = raw.try_into().map_err(|_| AuthError::Format)?;
    let sig = Signature::from_bytes(&sig_arr);
    if (now - ts).abs() > SIG_WINDOW_SECS {
        return Err(AuthError::Stale);
    }
    let canonical = build_canonical(user, ts, nonce, method, path_q, body_sha_hex);
    for (i, vk) in pubkeys.iter().enumerate() {
        if vk.verify(canonical.as_bytes(), &sig).is_ok() {
            return Ok(Verified {
                user: user.to_owned(),
                ts,
                nonce: nonce.to_owned(),
                key_idx: i,
            });
        }
    }
    Err(AuthError::BadSignature)
}

// ———— 加密信封 v1（SPEC §2）————

pub struct PlatformKeys(RsaPrivateKey);

impl PlatformKeys {
    pub fn generate() -> Self {
        PlatformKeys(RsaPrivateKey::new(&mut OsRng, 3072).expect("rsa-3072 keygen"))
    }

    /// 写 `<dir>/platform_rsa`（PKCS#8 PEM，0600）与 `<dir>/platform_rsa.pub`，返回私钥路径。
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_pem(&self, dir: &Path) -> Result<PathBuf> {
        let priv_path = dir.join("platform_rsa");
        let pem = self.0.to_pkcs8_pem(LineEnding::LF)?;
        write_private(&priv_path, pem.as_bytes())?;
        fs::write(dir.join("platform_rsa.pub"), self.public_pem().as_bytes())?;
        Ok(priv_path)
    }

    pub fn load_pem(dir: &Path) -> Result<Self> {
        let pem = fs::read_to_string(dir.join("platform_rsa"))?;
        Ok(PlatformKeys(RsaPrivateKey::from_pkcs8_pem(&pem)?))
    }

    pub fn public_pem(&self) -> String {
        self.0
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .expect("encode public pem")
            .as_str()
            .to_owned()
    }
}

/// 加密封装：`[384B RSA-OAEP-SHA256 封装的 k] || 每段[12B nonce][密文+16B tag]`，段=4MiB 明文。
/// 返回写出的总字节数（含 384B 封装头）。zstd 压缩不在本 crate（调用方在 docker save 管道自行压缩）。
pub fn seal<W: Write>(
    platform_pub: &RsaPublicKey,
    mut plain: impl Read,
    mut out: W,
) -> std::io::Result<u64> {
    let mut k = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut k);
    let wrapped = platform_pub
        .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &k)
        .map_err(|e| io::Error::other(format!("rsa-oaep wrap key: {e}")))?;
    if wrapped.len() != WRAP_LEN {
        return Err(io::Error::other(format!(
            "expected 384-byte rsa-3072 wrap, got {}",
            wrapped.len()
        )));
    }
    out.write_all(&wrapped)?;
    let mut total = wrapped.len() as u64;
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&k));
    let mut buf = vec![0u8; SEG];
    loop {
        let n = read_full(&mut plain, &mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(GenericArray::from_slice(&nonce), &buf[..n])
            .map_err(|_| io::Error::other("aes-gcm encrypt segment"))?;
        out.write_all(&nonce)?;
        out.write_all(&ct)?;
        total += (NONCE_LEN + ct.len()) as u64;
    }
}

/// 解封：读 384B 封装头解出 k，逐段解密写出。任何封装/tag 校验失败返回 Err（不 panic）。
/// 返回写出的明文总字节数。空信封（仅 384B 头）合法，对应 0 字节明文。
pub fn open(
    platform_priv: &RsaPrivateKey,
    sealed: impl Read,
    mut out: impl Write,
) -> std::io::Result<u64> {
    let mut r = BufReader::with_capacity(SEG + NONCE_LEN + TAG_LEN, sealed);
    let mut wrap = [0u8; WRAP_LEN];
    if read_full(&mut r, &mut wrap)? != WRAP_LEN {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "sealed: truncated 384-byte header",
        ));
    }
    let k = platform_priv
        .decrypt(Oaep::new::<Sha256>(), &wrap)
        .map_err(|e| io::Error::other(format!("rsa-oaep unwrap key: {e}")))?;
    let k: [u8; KEY_LEN] = k
        .as_slice()
        .try_into()
        .map_err(|_| io::Error::other("unwrapped key not 32 bytes"))?;
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&k));
    let mut total: u64 = 0;
    let mut nonce = [0u8; NONCE_LEN];
    let mut ct = vec![0u8; SEG + TAG_LEN];
    loop {
        match read_full(&mut r, &mut nonce)? {
            0 => return Ok(total), // 干净收尾（段边界上的 EOF）
            NONCE_LEN => {}
            _ => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sealed: truncated nonce")),
        }
        let n = read_full(&mut r, &mut ct)?;
        if n < TAG_LEN {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sealed: truncated segment",
            ));
        }
        let pt = cipher
            .decrypt(GenericArray::from_slice(&nonce), &ct[..n])
            .map_err(|_| {
                io::Error::other("aes-gcm segment auth failed (tag mismatch / tampered data)")
            })?;
        out.write_all(&pt)?;
        total += pt.len() as u64;
    }
}

/// 循环 read 直至填满 buf 或 EOF，返回已读字节数（管道短读安全）。
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..])? {
            0 => break,
            n => off += n,
        }
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn platform() -> &'static PlatformKeys {
        static K: std::sync::OnceLock<PlatformKeys> = std::sync::OnceLock::new();
        K.get_or_init(PlatformKeys::generate)
    }

    fn tmp(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录——pid 隔离并发进程、原子计数隔离
        // 同进程并行测试；共享父目录 + 固定文件名会让两个测试读写/清理同一批路径（config.rs 存量教训）。
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "showroom-common-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    // 1. sign→verify 往返（body 非空 / 空）
    #[test]
    fn sign_verify_roundtrip() {
        let k = UserKey::generate();
        let vk = [k.0.verifying_key()];
        for body in [b"{\"project\":\"demo\"}".as_slice(), b""] {
            let auth = sign("alice", &k, "POST", "/_api/publish", body, 1_000_000, "0123456789abcdef");
            let v = verify(&auth, &vk, "POST", "/_api/publish", body, 1_000_000).unwrap();
            assert_eq!(
                (v.user.as_str(), v.ts, v.nonce.as_str()),
                ("alice", 1_000_000, "0123456789abcdef")
            );
        }
    }

    // 2. 篡改 body 一个字节 → BadSignature
    #[test]
    fn tampered_body_rejected() {
        let k = UserKey::generate();
        let auth = sign("alice", &k, "POST", "/_api/publish", b"body-0", 1_000_000, "0123456789abcdef");
        assert_eq!(
            verify(&auth, &[k.0.verifying_key()], "POST", "/_api/publish", b"body-1", 1_000_000),
            Err(AuthError::BadSignature)
        );
    }

    // 3. ts 超窗（now±301）→ Stale；±300 边界通过
    #[test]
    fn stale_ts_rejected() {
        let k = UserKey::generate();
        let vk = [k.0.verifying_key()];
        let auth = sign("alice", &k, "POST", "/_api/publish", b"", 1_000_000, "0123456789abcdef");
        for now in [1_000_000 + 301, 1_000_000 - 301] {
            assert_eq!(
                verify(&auth, &vk, "POST", "/_api/publish", b"", now),
                Err(AuthError::Stale)
            );
        }
        for now in [1_000_000 + 300, 1_000_000 - 300] {
            assert!(verify(&auth, &vk, "POST", "/_api/publish", b"", now).is_ok());
        }
    }

    // 4. 错误公钥 → BadSignature；多把公钥时第二把命中可通过
    #[test]
    fn wrong_key_and_multi_key() {
        let k = UserKey::generate();
        let auth = sign("alice", &k, "GET", "/_api/logs?project=demo&tail=100", b"", 5, "aabbccddeeff0011");
        let wrong = UserKey::generate();
        assert_eq!(
            verify(
                &auth,
                &[wrong.0.verifying_key()],
                "GET",
                "/_api/logs?project=demo&tail=100",
                b"",
                5
            ),
            Err(AuthError::BadSignature)
        );
        verify(
            &auth,
            &[wrong.0.verifying_key(), k.0.verifying_key()],
            "GET",
            "/_api/logs?project=demo&tail=100",
            b"",
            5,
        )
        .unwrap();
    }

    // 5. seal→open 往返：空 / 恰好 4MiB / >8MiB（跨多段），并核对封装总长
    #[test]
    fn envelope_roundtrip() {
        for len in [0, SEG, SEG * 2 + 1234] {
            let plain: Vec<u8> = (0..len as u32).map(|i| (i % 251) as u8).collect();
            let mut sealed = Vec::new();
            let n = seal(&platform().0.to_public_key(), Cursor::new(plain.as_slice()), &mut sealed).unwrap();
            let segs = len.div_ceil(SEG);
            assert_eq!(n as usize, WRAP_LEN + segs * (NONCE_LEN + TAG_LEN) + len);
            let mut out = Vec::new();
            let m = open(&platform().0, Cursor::new(sealed.as_slice()), &mut out).unwrap();
            assert_eq!(m as usize, len);
            assert_eq!(out, plain);
        }
    }

    // 6. sealed 输入翻转一个字节（密文区 / 封装头区）→ 报错不 panic
    #[test]
    fn tampered_envelope_rejected() {
        let plain = vec![7u8; SEG + 100];
        let mut sealed = Vec::new();
        seal(&platform().0.to_public_key(), Cursor::new(plain.as_slice()), &mut sealed).unwrap();

        let mut t = sealed.clone();
        t[WRAP_LEN + NONCE_LEN + 10] ^= 1; // 第一段密文区内
        let mut out = Vec::new();
        let e = open(&platform().0, Cursor::new(t.as_slice()), &mut out);
        assert!(e.is_err(), "flipped ciphertext byte must error");
        assert!(out.is_empty());

        let mut t2 = sealed;
        t2[10] ^= 1; // 384B 封装头内
        let mut out2 = Vec::new();
        assert!(open(&platform().0, Cursor::new(t2.as_slice()), &mut out2).is_err());
    }

    // 7. UserKey seed save/load + pub_string/from_pub_string 往返（0600 权限）
    #[test]
    fn user_key_roundtrip() {
        let k = UserKey::generate();
        let p = tmp("id_ed25519");
        k.save_seed(&p).unwrap();
        let k2 = UserKey::from_seed_file(&p).unwrap();
        assert_eq!(k2.pub_string(), k.pub_string());
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);

        let vk = UserKey::from_pub_string(&k.pub_string()).unwrap();
        assert_eq!(vk.to_bytes(), *k.0.verifying_key().as_bytes());
        let _ = fs::remove_file(&p);
    }

    // 8. PlatformKeys pem save/load 往返（文件接缝）
    #[test]
    fn platform_pem_roundtrip() {
        let dir = tmp("platform-dir");
        let priv_path = platform().save_pem(&dir).unwrap();
        assert_eq!(priv_path, dir.join("platform_rsa"));
        assert!(dir.join("platform_rsa.pub").exists());
        let loaded = PlatformKeys::load_pem(&dir).unwrap();
        assert_eq!(loaded.public_pem(), platform().public_pem());
        let _ = fs::remove_dir_all(&dir);
    }

    // 9. 名字规则（SPEC §4 + §11 v1.4）：project 放宽正则（大写/连字符）+ 保留名；
    //    辅助名（user 等）沿用旧小写正则。tag 规则不变。
    #[test]
    fn names_project_regex_relaxed_and_reserved() {
        assert!(names::check_project("demo").is_ok());
        assert!(names::check_project("Demo42").is_ok(), "§11 放宽：大写合法");
        assert!(names::check_project("A-b-9").is_ok());
        assert!(names::check_project("0").is_ok());
        assert!(names::check_project(&"a".repeat(31)).is_ok());
        for r in names::RESERVED {
            assert!(names::check_project(r).is_err(), "project 保留名应拒: {r}");
        }
        for bad in [
            "", "-a", "a_b", "a b", "a.b", "a/b", "中文", &"a".repeat(32), "_x", "-demo",
        ] {
            assert!(names::check_project(bad).is_err(), "project 应拒: {bad}");
        }
        // 辅助名：旧小写正则（user 进 registry 路径，保持小写约束）
        assert!(names::check_aux("alice-2").is_ok());
        assert!(names::check_aux("pr42").is_ok());
        assert!(names::check_aux("v2").is_ok(), "辅助名不受保留名约束");
        for bad in ["", "-a", "Aa", "a_", "a b", "a.b", "a/b", "中文", &"a".repeat(32)] {
            assert!(names::check_aux(bad).is_err(), "辅助名应拒: {bad}");
        }
        for t in ["v1", "1.0.0", "latest", "a_b.c-d", "0", &"a".repeat(128)] {
            assert!(names::check_tag(t).is_ok(), "tag 应合法: {t}");
        }
        for t in ["", ".x", "-x", "a b", "a/b", "中", &"a".repeat(129)] {
            assert!(names::check_tag(t).is_err(), "tag 应拒: {t}");
        }
    }

    // 9b. 随机项目名（§11 发布入口默认预填）：恒 8 位、字符集 [A-Za-z0-9]、样本间有差异
    #[test]
    fn random_project_shape() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let p = names::random_project();
            assert_eq!(p.len(), 8, "长度恒 8");
            assert!(
                p.bytes().all(|b| b.is_ascii_alphanumeric()),
                "字符集 [a-zA-Z0-9]: {p}"
            );
            assert!(names::check_project(&p).is_ok(), "随机名必须通过 project 校验");
            seen.insert(p);
        }
        // 62^8 空间，64 个样本必有相异对（碰撞概率 ~1e-11）
        assert!(seen.len() > 1, "随机性：64 个样本出现 {}/64 个不同名字", seen.len());
    }

    // 10. 预哈希签名 / 流式哈希：与 sign/verify 同语义往返 + 已知向量
    #[test]
    fn prehashed_sign_verify_and_stream_hash() {
        let hash = sha256_hex_stream(Cursor::new(b"stream-me")).unwrap();
        assert_eq!(hash, sha256_hex(b"stream-me"));
        assert_eq!(
            sha256_hex_stream(Cursor::new(b"")).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let k = UserKey::generate();
        let pq = "/_api/import?project=demo&tag=v1";
        let auth = sign_prehashed("alice", &k, "POST", pq, &hash, 7, "aabb0011ccdd2233");
        let vk = [k.0.verifying_key()];
        let v = verify_with_hash(&auth, &vk, "POST", pq, &hash, 7).unwrap();
        assert_eq!((v.user.as_str(), v.nonce.as_str()), ("alice", "aabb0011ccdd2233"));
        assert_eq!(v.key_idx, 0, "单钥匙：命中索引 0");
        // Design#4：多钥匙——签名的钥匙排在第二位时返回索引 1（调用方免复验定位 kid）
        let other = UserKey::generate();
        let vk2 = [other.0.verifying_key(), vk[0]];
        assert_eq!(verify_with_hash(&auth, &vk2, "POST", pq, &hash, 7).unwrap().key_idx, 1);
        assert_eq!(
            verify_with_hash(&auth, &vk, "POST", pq, &sha256_hex(b"other"), 7),
            Err(AuthError::BadSignature)
        );
    }

    // 11. write_private：0600、内容、覆盖偏宽权限后收紧
    #[test]
    fn write_private_is_0600() {
        let p = tmp("priv.bin");
        write_private(&p, b"secret").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"secret");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&p, b"again").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_file(&p);
    }
}
