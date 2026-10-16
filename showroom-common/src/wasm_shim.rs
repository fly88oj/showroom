//! wasm32 C-ABI 垫片（SPEC §8.3）：showroom-web 控制台的原生入口。
//! 不用 wasm-bindgen/wasm-pack——`rustup target wasm32-unknown-unknown` 直接构建，
//! JS 侧 fetch + instantiate 后按本文件注释的 ABI 调用。
//!
//! ABI 约定：
//! - 字符串参数：UTF-8 字节指针 + 长度（不含 NUL）；字节参数：指针 + 长度。
//! - 输出写入「调用方传入的缓冲区」：字符串 NUL 结尾。成功返回写出的字节数
//!   （字符串含 NUL）；容量不足时不写任何字节、返回 -(所需字节数)。
//!   所需长度恒 ≥ 40，与错误码区间（|e| ≤ 7）不重叠，可区分。
//! - 错误码（负数）：-1 内部 panic；-2 参数非法（空指针/越界/UTF-8）；
//!   -3 seed/PEM 无效；-5 会话不存在；-6 熵池未注入（先调 sr_entropy）；-7 加密失败。
//! - `sr_seal_push(sid, ptr, 0, …)`：len=0 表示输入流结束（flush 尾段），此后只应 sr_seal_end。
//! - `sr_open_push(sid, ptr, 0, …)`：同上——len=0 表示信封流结束（解出尾段），此后只应
//!   sr_open_end；容量不足返回 -(所需字节数)，但报告值下限 40（尾段明文可小至 1B，
//!   需与错误码区间 |e| ≤ 7 区分开）。
//!
//! §9 下行解密（db-dump）：`sr_keygen_rsa` 生成 RSA-3072 一次性对（`sr_rsa_pub_pem`
//! 上行给服务端加密 dump、`sr_rsa_priv_pem` 留本地）；返回信封经 `sr_open_begin/
//! sr_open_push/sr_open_end` 流式解封（首 384B RSA-OAEP 解出 k，后续段 AES-GCM），
//! 与原生 common::open 逐字节兼容。wasm 无文件 IO，全部内存缓冲。
//!
//! 随机性：wasm32-unknown-unknown 无 OS 熵源（getrandom 该目标无后端，SPEC §8.3 又禁引
//! wasm-bindgen），熵由 JS 侧 crypto.getRandomValues 生成后经 `sr_entropy()` 注入，
//! 驱动全局 StdRng（ChaCha12，CSPRNG）。seal 会话另支持确定性 seed（sr_seal_begin_seed）：
//! 同 seed + 同 push 序列 ⇒ 逐字节相同的信封，供 §8.1 断点续传在页面刷新后重放已传分片
//! （安全等级与 localStorage 存 user seed 的 P1 试验档一致）。
//!
//! zstd 压缩会话（sr_zstd_begin/push/end，与 seal 会话同构）——浏览器上传压缩统一走 zstd：
//! 每次 push 把输入压成**一个独立完整的 zstd 帧**（帧边界即 push 边界），输出按调用顺序
//! 拼接即为合法的拼接帧流——服务端魔数自适应的流式解码器天然跨帧续解；且每片输出仅由
//! (level, 输入字节) 决定（zstd 无随机性），断点续传重放与原会话逐字节一致——确定性来源
//! 是算法确定性（对照 seal 的 seed 确定性）。输出上界 = ZSTD_COMPRESSBOUND
//! （len + len/256 + 64）；容量不足返回 -(所需字节数)（下限 40，与错误码区间区分）；
//! 空输入（len=0）合法但不产帧（返回 0），会话无 flush/尾段概念，结束即 sr_zstd_end。
//!
//! ponytail 天花板（明知且接受）：sr_alloc/sr_free 为全局 HashMap 指针池，不合并碎片、
//! 不抗双重释放以外的误用；会话表为只增不回收的 Vec 槽位；锁在 wasm 单线程下无争用。
//! RSA 一次性密钥对会话（sr_keygen_rsa）同样不回收——单页生命周期内次数有限（db-dump
//! 每次备份一把），私钥材料滞留 wasm 内存属 P1 试验档接受面。

#![cfg(target_arch = "wasm32")]

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::engine::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use sha2::Sha256;

use crate::{build_canonical, sha256_hex, KEY_LEN, NONCE_LEN, SEG, TAG_LEN, WRAP_LEN};

const E_PANIC: i32 = -1;
const E_ARGS: i32 = -2;
const E_KEY: i32 = -3;
const E_SESSION: i32 = -5;
const E_ENTROPY: i32 = -6;
const E_CRYPTO: i32 = -7;

type R = Result<i32, i32>;

/// 统一兜底：panic → -1（目标默认 panic=abort 时 catch_unwind 不生效，但求不炸导出面）。
fn guarded(f: impl FnOnce() -> R) -> i32 {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => e,
        Err(_) => E_PANIC,
    }
}

// —— 指针/字符串小工具 ——

unsafe fn slice_const<'a>(p: *const u8, len: usize) -> Result<&'a [u8], i32> {
    if p.is_null() {
        // 空指针 + 0 长度 = 空切片（JS 侧 null 传参容错）；带长度则非法
        if len == 0 {
            return Ok(&[]);
        }
        return Err(E_ARGS);
    }
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

fn utf8(p: *const u8, len: usize) -> Result<String, i32> {
    std::str::from_utf8(unsafe { slice_const(p, len)?})
        .map(|s| s.to_owned())
        .map_err(|_| E_ARGS)
}

fn seed32(p: *const u8) -> Result<[u8; KEY_LEN], i32> {
    let s = unsafe { slice_const(p, KEY_LEN)?};
    s.try_into().map_err(|_| E_ARGS)
}

/// 写 NUL 结尾字符串；容量不足返回 -(needed)，不写任何字节。
fn write_cstr(s: &str, out: *mut u8, cap: usize) -> R {
    let needed = s.len() + 1;
    if cap < needed {
        return Err(-(needed as i32));
    }
    if out.is_null() {
        return Err(E_ARGS);
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(out, needed) };
    buf[..s.len()].copy_from_slice(s.as_bytes());
    buf[s.len()] = 0;
    Ok(needed as i32)
}

/// 输出区写入器（容量已预先核算）。
struct W<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> W<'a> {
    fn put(&mut self, b: &[u8]) {
        self.buf[self.pos..self.pos + b.len()].copy_from_slice(b);
        self.pos += b.len();
    }
}

// —— 全局熵池（JS 注入）——

static GLOBAL_RNG: Mutex<Option<StdRng>> = Mutex::new(None);

/// 注入熵（JS：crypto.getRandomValues ≥32B，xor-rotate 折叠进 32B seed）。可重复调用（重置）。
#[no_mangle]
pub extern "C" fn sr_entropy(src: *const u8, len: usize) -> i32 {
    guarded(|| {
        let bytes = unsafe { slice_const(src, len)?};
        if bytes.is_empty() {
            return Err(E_ARGS);
        }
        let mut seed = [0u8; KEY_LEN];
        for (i, b) in bytes.iter().enumerate() {
            // 位置相关旋转：超长输入的每个字节都影响 seed，避免同位置字节相消
            seed[i % KEY_LEN] ^= b.rotate_left(((i / KEY_LEN) % 8) as u32);
        }
        *GLOBAL_RNG.lock().unwrap() = Some(StdRng::from_seed(seed));
        Ok(0)
    })
}

fn with_global_rng<T>(f: impl FnOnce(&mut StdRng) -> T) -> Result<T, i32> {
    let mut g = GLOBAL_RNG.lock().unwrap();
    g.as_mut().map(f).ok_or(E_ENTROPY)
}

/// getrandom 自定义后端（wasm32 链接需要；见 lib.rs 注册处）：从全局熵池取字节。
/// 垫片自身不调用 OsRng——这是 lib.rs 中 OsRng 路径若被误用时的兜底，未注入熵则报错。
pub fn custom_getrandom(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    let mut g = GLOBAL_RNG.lock().unwrap();
    match g.as_mut() {
        Some(r) => {
            r.fill_bytes(buf);
            Ok(())
        }
        None => Err(getrandom::Error::UNSUPPORTED),
    }
}

/// getrandom 0.4 自定义后端钩子（0.4 起注册宏改为符号约定；开关 cfg
/// `getrandom_backend="custom"` 在根 .cargo/config.toml 的 wasm32 段）。链路：
/// ed25519-dalek 3 → curve25519-dalek 5 → rand_core 0.10 → getrandom 0.4——该链
/// 在本目标需要可链接的熵符号；与上方 0.2 后端同一熵池（未注入熵即报错）。
#[no_mangle]
extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom_04::Error> {
    let buf = unsafe { core::slice::from_raw_parts_mut(dest, len) };
    let mut g = GLOBAL_RNG.lock().unwrap();
    match g.as_mut() {
        Some(r) => {
            r.fill_bytes(buf);
            Ok(())
        }
        None => Err(getrandom_04::Error::new_custom(1)),
    }
}

// —— 内存池（调用方缓冲区模式的配套：JS 大块进出的暂存）——

static POOL: std::sync::LazyLock<Mutex<HashMap<usize, Vec<u8>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 分配 len 字节（零填充），返回指针；0 表示失败。仅可 sr_free 释放，不可 realloc。
/// ponytail：HashMap<ptr, Vec> 池——Vec 缓冲区地址稳定（移动 Vec 不移动堆块）。
#[no_mangle]
pub extern "C" fn sr_alloc(len: usize) -> *mut u8 {
    let r = panic::catch_unwind(AssertUnwindSafe(|| -> Result<*mut u8, i32> {
        if len == 0 || len > (1 << 30) {
            return Err(E_ARGS);
        }
        let v = vec![0u8; len];
        let p = v.as_ptr() as usize;
        POOL.lock().unwrap().insert(p, v);
        Ok(p as *mut u8)
    }));
    match r {
        Ok(Ok(p)) => p,
        _ => std::ptr::null_mut(),
    }
}

/// 释放 sr_alloc 的指针。len 仅用于 ABI 对账（以池内记录为准），重复释放为无害 no-op。
#[no_mangle]
pub extern "C" fn sr_free(ptr: *mut u8, len: usize) {
    let _ = len;
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        if !ptr.is_null() {
            POOL.lock().unwrap().remove(&(ptr as usize));
        }
    }));
}

// —— 密钥 ——

/// 生成 Ed25519 seed：向 seed_out 写 32 随机字节（熵池驱动）。返回 0。
#[no_mangle]
pub extern "C" fn sr_generate(seed_out: *mut u8) -> i32 {
    guarded(|| {
        if seed_out.is_null() {
            return Err(E_ARGS);
        }
        let out = unsafe { std::slice::from_raw_parts_mut(seed_out, KEY_LEN) };
        with_global_rng(|r| r.fill_bytes(out))?;
        Ok(0)
    })
}

/// 由 32B seed 导出公钥串 "ed25519:<b64-nopad>"（与 common::UserKey::pub_string 逐字节一致）。
#[no_mangle]
pub extern "C" fn sr_pub_string(seed: *const u8, pub_out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        let arr = seed32(seed)?;
        let sk = SigningKey::from_bytes(&arr);
        let s = format!("ed25519:{}", STANDARD_NO_PAD.encode(sk.verifying_key().as_bytes()));
        write_cstr(&s, pub_out, cap)
    })
}

/// 生成完整 Authorization 头（与 common::sign 逐字节一致）：
/// `SR-SIG1 <user>:<ts>:<nonce>:<sig_b64url>`；body 取原始字节（sha256 在内部计算）。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn sr_sign_header(
    seed: *const u8,
    user: *const u8,
    user_len: usize,
    method: *const u8,
    method_len: usize,
    path_q: *const u8,
    path_len: usize,
    ts: i64,
    nonce: *const u8,
    nonce_len: usize,
    body: *const u8,
    body_len: usize,
    out: *mut u8,
    cap: usize,
) -> i32 {
    guarded(|| {
        let arr = seed32(seed)?;
        let user = utf8(user, user_len)?;
        let method = utf8(method, method_len)?;
        let path = utf8(path_q, path_len)?;
        let nonce = utf8(nonce, nonce_len)?;
        let body = unsafe { slice_const(body, body_len)?};
        if user.is_empty() || nonce.is_empty() || method.is_empty() || path.is_empty() {
            return Err(E_ARGS);
        }
        let sk = SigningKey::from_bytes(&arr);
        let canonical =
            build_canonical(&user, ts, &nonce, &method, &path, &sha256_hex(body));
        let sig = sk.sign(canonical.as_bytes());
        let header =
            format!("SR-SIG1 {user}:{ts}:{nonce}:{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
        write_cstr(&header, out, cap)
    })
}

// —— seal 会话（SPEC §2 信封，流式分片版本）——

struct SealState {
    cipher: Aes256Gcm,
    /// 会话随机流：k / RSA-OAEP padding / 每段 nonce 全部依序取自它 → 确定性可重放。
    rng: StdRng,
    /// 未满 4MiB 的剩余明文（跨 push 保持，段边界=4MiB 与 common::seal 一致）。
    carry: Vec<u8>,
    /// 384B RSA-OAEP wrap 头（首个 push 时先写出）。
    pending_header: Option<Vec<u8>>,
}

static SESSIONS: Mutex<Vec<Option<SealState>>> = Mutex::new(Vec::new());

fn seal_begin(pem: &[u8], seed: Option<[u8; KEY_LEN]>) -> Result<i32, i32> {
    let pem = std::str::from_utf8(pem).map_err(|_| E_KEY)?;
    let platform_pub = RsaPublicKey::from_public_key_pem(pem).map_err(|_| E_KEY)?;
    let mut rng = match seed {
        Some(s) => StdRng::from_seed(s),
        None => StdRng::from_seed(with_global_rng(|r| {
            let mut b = [0u8; KEY_LEN];
            r.fill_bytes(&mut b);
            b
        })?),
    };
    let mut k = [0u8; KEY_LEN];
    rng.fill_bytes(&mut k);
    let wrapped = platform_pub
        .encrypt(&mut rng, Oaep::new::<Sha256>(), &k)
        .map_err(|_| E_CRYPTO)?;
    if wrapped.len() != WRAP_LEN {
        return Err(E_KEY); // 非 RSA-3072（模长≠384）
    }
    let st = SealState {
        cipher: Aes256Gcm::new(crate::gcm_key(&k)),
        rng,
        carry: Vec::new(),
        pending_header: Some(wrapped),
    };
    let mut v = SESSIONS.lock().unwrap();
    v.push(Some(st)); // 槽位只增不回收（ponytail；句柄从 1 起）
    Ok(v.len() as i32)
}

fn seal_seg(st: &mut SealState, plain: &[u8], w: &mut W<'_>) -> Result<(), i32> {
    let mut nonce = [0u8; NONCE_LEN];
    st.rng.fill_bytes(&mut nonce);
    let ct = st
        .cipher
        .encrypt(crate::gcm_nonce(&nonce), plain)
        .map_err(|_| E_CRYPTO)?;
    w.put(&nonce);
    w.put(&ct);
    Ok(())
}

fn seal_push(sid: i32, data: &[u8], out: *mut u8, cap: usize) -> Result<i32, i32> {
    if sid <= 0 {
        return Err(E_SESSION);
    }
    let mut v = SESSIONS.lock().unwrap();
    let st = match v.get_mut(sid as usize - 1) {
        Some(Some(s)) => s,
        _ => return Err(E_SESSION),
    };
    // 先按 §2 精确核算本次输出量并检查容量，再消耗随机流——失败路径可原样重试，
    // 且确定性重放不因容量探测产生分叉。
    let flush = data.is_empty();
    let full_segs = (st.carry.len() + data.len()) / SEG;
    let tail = if flush && !st.carry.is_empty() { st.carry.len() } else { 0 };
    let header = if st.pending_header.is_some() { WRAP_LEN } else { 0 };
    let needed =
        header + full_segs * (SEG + NONCE_LEN + TAG_LEN) + if tail > 0 { NONCE_LEN + TAG_LEN + tail } else { 0 };
    if out.is_null() || cap < needed {
        return Err(-(needed as i32));
    }
    let mut w = W { buf: unsafe { std::slice::from_raw_parts_mut(out, cap) }, pos: 0 };
    if let Some(h) = st.pending_header.take() {
        w.put(&h);
    }
    st.carry.extend_from_slice(data);
    while st.carry.len() >= SEG {
        let seg: Vec<u8> = st.carry.drain(..SEG).collect();
        seal_seg(st, &seg, &mut w)?;
    }
    if tail > 0 {
        let seg = std::mem::take(&mut st.carry);
        seal_seg(st, &seg, &mut w)?;
    }
    Ok(w.pos as i32)
}

/// 开启 seal 会话（随机性取自全局熵池）。platform_pub_pem = 管理员分发的 RSA-3072 公钥 PEM。
/// 返回会话句柄（>0）或负错误码。
#[no_mangle]
pub extern "C" fn sr_seal_begin(platform_pub_pem: *const u8, pem_len: usize) -> i32 {
    guarded(|| {
        let pem = unsafe { slice_const(platform_pub_pem, pem_len)?};
        seal_begin(pem, None)
    })
}

/// 确定性 seal 会话（§8.1 断点续传重放用）：seed 由 JS 持久化，同 seed+同 push 序列
/// ⇒ 与原会话逐字节相同的信封输出。
#[no_mangle]
pub extern "C" fn sr_seal_begin_seed(
    platform_pub_pem: *const u8,
    pem_len: usize,
    seed: *const u8,
) -> i32 {
    guarded(|| {
        let pem = unsafe { slice_const(platform_pub_pem, pem_len)?};
        let s = seed32(seed)?;
        seal_begin(pem, Some(s))
    })
}

/// 推入明文分片：内部按 4MiB 段边界流式 AES-GCM 加密（段=[12B nonce][密文||16B tag]），
/// 首次调用先写 384B wrap 头。返回本次写出的字节数（可能为 0——不足一段时暂存）。
/// len=0 = 输入结束：flush 尾段（若有）并返回其字节数。
#[no_mangle]
pub extern "C" fn sr_seal_push(
    sid: i32,
    data: *const u8,
    len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i32 {
    guarded(|| {
        let data = unsafe { slice_const(data, len)?};
        seal_push(sid, data, out, out_cap)
    })
}

/// 结束并丢弃会话（信封已由调用方收尾 flush 后完整取得）。
#[no_mangle]
pub extern "C" fn sr_seal_end(sid: i32) -> i32 {
    guarded(|| {
        if sid <= 0 {
            return Err(E_SESSION);
        }
        let mut v = SESSIONS.lock().unwrap();
        match v.get_mut(sid as usize - 1) {
            Some(slot) => {
                *slot = None;
                Ok(0)
            }
            None => Err(E_SESSION),
        }
    })
}

// —— RSA 一次性密钥对会话（SPEC §9：db-dump 下行信封的客户端侧）——

static RSA_SESSIONS: Mutex<Vec<Option<RsaPrivateKey>>> = Mutex::new(Vec::new());

fn rsa_with<T>(sid: i32, f: impl FnOnce(&RsaPrivateKey) -> Result<T, i32>) -> Result<T, i32> {
    if sid <= 0 {
        return Err(E_SESSION);
    }
    let v = RSA_SESSIONS.lock().unwrap();
    match v.get(sid as usize - 1) {
        Some(Some(k)) => f(k),
        _ => Err(E_SESSION),
    }
}

/// 生成 RSA-3072 一次性密钥对（熵池驱动；仅用于 db-dump 信封的公钥上行）。
/// 返回会话句柄（>0）或负错误码。
#[no_mangle]
pub extern "C" fn sr_keygen_rsa() -> i32 {
    guarded(|| {
        // 3072 位 → 模长 384B，与 §2 wrap 头长度契约一致
        let k = with_global_rng(|r| RsaPrivateKey::new(r, 3072))
            .map_err(|_| E_ENTROPY)?
            .map_err(|_| E_CRYPTO)?;
        let mut v = RSA_SESSIONS.lock().unwrap();
        v.push(Some(k)); // 槽位只增不回收（ponytail；见文件头注）
        Ok(v.len() as i32)
    })
}

/// 导出会话公钥 PEM（SPKI，"-----BEGIN PUBLIC KEY-----"）。
#[no_mangle]
pub extern "C" fn sr_rsa_pub_pem(sid: i32, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        rsa_with(sid, |k| {
            let pem = k
                .to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .map_err(|_| E_CRYPTO)?;
            write_cstr(&pem, out, cap)
        })
    })
}

/// 导出会话私钥 PEM（PKCS#8，"-----BEGIN PRIVATE KEY-----"；一次性，用后即弃）。
#[no_mangle]
pub extern "C" fn sr_rsa_priv_pem(sid: i32, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        rsa_with(sid, |k| {
            let pem = k.to_pkcs8_pem(LineEnding::LF).map_err(|_| E_CRYPTO)?;
            write_cstr(pem.as_str(), out, cap)
        })
    })
}

// —— open 会话（SPEC §2 信封解封，流式版本；与原生 common::open 逐字节兼容）——

struct OpenState {
    /// 首个 384B wrap 头解封时取用一次，之后置 None（私钥材料尽早离场）。
    priv_key: Option<RsaPrivateKey>,
    cipher: Option<Aes256Gcm>,
    /// 未消费的信封字节 carry（可能仍含未解封的 384B wrap 头）。
    buf: Vec<u8>,
    /// flush 之后流已结束，再 push 属 API 误用。
    finished: bool,
}

static OPEN_SESSIONS: Mutex<Vec<Option<OpenState>>> = Mutex::new(Vec::new());

/// 开启解封会话：priv_pem 为 PKCS#8 PEM（"-----BEGIN PRIVATE KEY-----"；
/// WebCrypto exportKey('pkcs8') 与 sr_rsa_priv_pem 产物均此格式）。
/// 返回会话句柄（>0）或负错误码。
#[no_mangle]
pub extern "C" fn sr_open_begin(priv_pem: *const u8, pem_len: usize) -> i32 {
    guarded(|| {
        let pem = utf8(priv_pem, pem_len)?;
        let k = RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|_| E_KEY)?;
        let st = OpenState { priv_key: Some(k), cipher: None, buf: Vec::new(), finished: false };
        let mut v = OPEN_SESSIONS.lock().unwrap();
        v.push(Some(st)); // 槽位只增不回收（ponytail；句柄从 1 起）
        Ok(v.len() as i32)
    })
}

fn open_push(sid: i32, data: &[u8], out: *mut u8, cap: usize) -> Result<i32, i32> {
    if sid <= 0 {
        return Err(E_SESSION);
    }
    let mut v = OPEN_SESSIONS.lock().unwrap();
    let st = match v.get_mut(sid as usize - 1) {
        Some(Some(s)) => s,
        _ => return Err(E_SESSION),
    };
    if st.finished {
        return Err(E_ARGS);
    }
    let flush = data.is_empty();
    // 先按 §2 精确核算本次明文产出并检查容量，再动状态——失败路径可原样重试，
    // 不会因容量探测把 data 重复追加进 carry。
    let blk = NONCE_LEN + SEG + TAG_LEN; // 一个满段在信封中的字节数
    let hdr = if st.cipher.is_none() { WRAP_LEN } else { 0 };
    let rem = (st.buf.len() + data.len()).saturating_sub(hdr);
    let needed = (rem / blk) * SEG + if flush && rem % blk >= NONCE_LEN + TAG_LEN {
        rem % blk - NONCE_LEN - TAG_LEN // 尾段明文
    } else {
        0
    };
    if (needed > 0 && out.is_null()) || cap < needed {
        // 报告值下限 40：尾段明文可小至 1B，须与错误码区间（|e| ≤ 7）区分
        return Err(-(needed.max(40) as i32));
    }
    st.buf.extend_from_slice(data);
    if st.cipher.is_none() {
        if st.buf.len() < WRAP_LEN {
            if flush {
                return Err(E_CRYPTO); // 流结束但 wrap 头不齐（截断信封）
            }
            return Ok(0); // 头未到齐，暂存等待后续 push
        }
        let wrap: Vec<u8> = st.buf.drain(..WRAP_LEN).collect();
        let pk = st.priv_key.take().ok_or(E_CRYPTO)?;
        let k = pk.decrypt(Oaep::new::<Sha256>(), &wrap).map_err(|_| E_CRYPTO)?;
        drop(pk);
        let k: [u8; KEY_LEN] = k.as_slice().try_into().map_err(|_| E_CRYPTO)?;
        st.cipher = Some(Aes256Gcm::new(crate::gcm_key(&k)));
    }
    let cipher = st.cipher.clone().expect("cipher 已建立");
    let mut w = W { buf: unsafe { std::slice::from_raw_parts_mut(out, cap) }, pos: 0 };
    while st.buf.len() >= blk {
        // 满段：[12B nonce][SEG 密文+16B tag]
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&st.buf[..NONCE_LEN]);
        st.buf.drain(..NONCE_LEN);
        let ct: Vec<u8> = st.buf.drain(..SEG + TAG_LEN).collect();
        let pt = cipher
            .decrypt(crate::gcm_nonce(&nonce), ct.as_slice())
            .map_err(|_| E_CRYPTO)?;
        w.put(&pt);
    }
    if flush {
        if !st.buf.is_empty() {
            if st.buf.len() < NONCE_LEN + TAG_LEN {
                return Err(E_CRYPTO); // 尾段连 nonce+tag 都不齐
            }
            let mut nonce = [0u8; NONCE_LEN];
            nonce.copy_from_slice(&st.buf[..NONCE_LEN]);
            st.buf.drain(..NONCE_LEN);
            let ct: Vec<u8> = std::mem::take(&mut st.buf);
            let pt = cipher
                .decrypt(crate::gcm_nonce(&nonce), ct.as_slice())
                .map_err(|_| E_CRYPTO)?;
            w.put(&pt);
        }
        st.finished = true;
    }
    Ok(w.pos as i32)
}

/// 推入信封字节分片：内部按段边界流式解密（首 384B wrap 头在到齐时解出 k）。
/// 返回本次写出的明文字节数（可能为 0——头/段未到齐时暂存）。
/// len=0 = 输入结束：解出尾段并返回其明文长度；此后只应 sr_open_end。
#[no_mangle]
pub extern "C" fn sr_open_push(
    sid: i32,
    data: *const u8,
    len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i32 {
    guarded(|| {
        let data = unsafe { slice_const(data, len)?};
        open_push(sid, data, out, out_cap)
    })
}

/// 结束并丢弃解封会话。
#[no_mangle]
pub extern "C" fn sr_open_end(sid: i32) -> i32 {
    guarded(|| {
        if sid <= 0 {
            return Err(E_SESSION);
        }
        let mut v = OPEN_SESSIONS.lock().unwrap();
        match v.get_mut(sid as usize - 1) {
            Some(slot) => {
                *slot = None;
                Ok(0)
            }
            None => Err(E_SESSION),
        }
    })
}

// —— SRTP 传输会话（plan-18：浏览器 WS 载波；与 CLI 的 TCP 载波共用记录层）——

struct SrtpState {
    writer: crate::srtp::TWriter,
    reader: crate::srtp::TReader,
}

static SRTP_SESSIONS: Mutex<Vec<Option<SrtpState>>> = Mutex::new(Vec::new());

/// 从全局熵池抽种新建独立 StdRng（会话持有；SRTP 会话随机性与 seal 会话同源同法）。
fn fresh_rng() -> Result<StdRng, i32> {
    let mut seed = [0u8; 32];
    with_global_rng(|r| r.fill_bytes(&mut seed))?;
    Ok(StdRng::from_seed(seed))
}

/// 开启 SRTP 会话：per-user 包裹公钥 PEM → 生成 k、写出 384B wrap（v1.1：JS 自行
/// 在 wrap 前拼 8B srtp_id）。随机性取全局熵池（StdRng；wasm32 无 OS 熵源）。
/// 返回会话句柄（>0）或负错误码（容量不足报所需，≤-40 协议）。
#[no_mangle]
pub extern "C" fn sr_srtp_begin(pub_pem: *const u8, pem_len: usize, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        let pem = utf8(pub_pem, pem_len)?;
        let mut rng = fresh_rng()?;
        let mut k = [0u8; crate::KEY_LEN];
        rng.fill_bytes(&mut k);
        let pk = RsaPublicKey::from_public_key_pem(&pem).map_err(|_| E_KEY)?;
        let wrapped = pk.encrypt(&mut rng, Oaep::new::<Sha256>(), &k).map_err(|_| E_CRYPTO)?;
        if wrapped.len() > cap {
            return Err(-(wrapped.len() as i32));
        }
        let st = SrtpState {
            writer: crate::srtp::TWriter::new(k, Box::new(rng)),
            reader: crate::srtp::TReader::new(k),
        };
        let mut v = SRTP_SESSIONS.lock().unwrap();
        v.push(Some(st));
        let sid = v.len() as i32;
        let mut w = W { buf: unsafe { std::slice::from_raw_parts_mut(out, cap) }, pos: 0 };
        w.put(&wrapped);
        Ok(sid)
    })
}

/// 上行：明文 chunk → 记录字节（每调用一条记录；JS 侧按 ≤64KiB 分片调用，
/// 与 CLI 一致）。输出上界 = 输入 + 32。
#[no_mangle]
pub extern "C" fn sr_srtp_seal(sid: i32, data: *const u8, len: usize, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        let data = unsafe { slice_const(data, len)? };
        let mut v = SRTP_SESSIONS.lock().unwrap();
        let st = v.get_mut(sid as usize - 1).and_then(|s| s.as_mut()).ok_or(E_SESSION)?;
        let need = data.len() + 32;
        if need > cap {
            return Err(-(need.max(40) as i32));
        }
        let mut buf = Vec::with_capacity(need);
        st.writer.record(data, &mut buf).map_err(|_| E_CRYPTO)?;
        let n = buf.len();
        if n > cap {
            return Err(-(n as i32));
        }
        let w = unsafe { std::slice::from_raw_parts_mut(out, cap) };
        w[..n].copy_from_slice(&buf);
        Ok(n as i32)
    })
}

/// 下行：记录字节（任意分片）→ 明文追加写出。返回本次写出的明文字节数（可 0）。
#[no_mangle]
pub extern "C" fn sr_srtp_open(sid: i32, data: *const u8, len: usize, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        let data = if len == 0 { &[][..] } else { unsafe { slice_const(data, len)? } };
        let mut v = SRTP_SESSIONS.lock().unwrap();
        let st = v.get_mut(sid as usize - 1).and_then(|s| s.as_mut()).ok_or(E_SESSION)?;
        let mut plain = Vec::with_capacity(data.len());
        st.reader.push(data, &mut plain).map_err(|_| E_CRYPTO)?;
        if plain.len() > cap {
            return Err(-(plain.len().max(40) as i32));
        }
        let w = unsafe { std::slice::from_raw_parts_mut(out, cap) };
        w[..plain.len()].copy_from_slice(&plain);
        Ok(plain.len() as i32)
    })
}

/// 结束并丢弃 SRTP 会话。
#[no_mangle]
pub extern "C" fn sr_srtp_end(sid: i32) -> i32 {
    guarded(|| {
        let mut v = SRTP_SESSIONS.lock().unwrap();
        match v.get_mut(sid as usize - 1) {
            Some(slot) => {
                *slot = None;
                Ok(0)
            }
            None => Err(E_SESSION),
        }
    })
}

/// HMAC-SHA256（plan-18 §3.3：key_mac 绑定验证；非安全上下文无 WebCrypto 的回落）。
/// out 恒 32B；容量不足报 -32。
#[no_mangle]
pub extern "C" fn sr_hmac_sha256(
    key: *const u8,
    key_len: usize,
    data: *const u8,
    data_len: usize,
    out: *mut u8,
    cap: usize,
) -> i32 {
    guarded(|| {
        let key = unsafe { slice_const(key, key_len)? };
        let data = unsafe { slice_const(data, data_len)? };
        use hmac::Hmac;
        // wasm_shim 顶部已入 KeyInit（new_from_slice 同名）——显式指定 trait 消歧
        let mut m = <Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(key).map_err(|_| E_ARGS)?;
        use hmac::Mac as _;
        m.update(data);
        let mac: [u8; 32] = m.finalize().into_bytes().into();
        if mac.len() > cap {
            return Err(-(mac.len() as i32));
        }
        let mut w = W { buf: unsafe { std::slice::from_raw_parts_mut(out, cap) }, pos: 0 };
        w.put(&mac);
        Ok(w.pos as i32)
    })
}

// —— zstd 压缩会话（上传压缩统一；与 seal 会话同构的流式 push 模型）——

struct ZstdState {
    /// 可复用压缩上下文（跨 push 摊平 level-19 的工作区分配；每次 compress 重置参数）。
    comp: zstd::bulk::Compressor<'static>,
    /// push 输出暂存（容量按 ZSTD_COMPRESSBOUND 预留，跨 push 复用）。
    buf: Vec<u8>,
}

static ZSTD_SESSIONS: Mutex<Vec<Option<ZstdState>>> = Mutex::new(Vec::new());

/// zstd 压缩输出上界（ZSTD_COMPRESSBOUND 公式）：len + len/256 +（小输入固定余量 64）。
fn zstd_bound(len: usize) -> usize {
    len + (len >> 8) + 64
}

fn zstd_push(sid: i32, data: &[u8], out: *mut u8, cap: usize) -> Result<i32, i32> {
    if sid <= 0 {
        return Err(E_SESSION);
    }
    let mut v = ZSTD_SESSIONS.lock().unwrap();
    let st = match v.get_mut(sid as usize - 1) {
        Some(Some(s)) => s,
        _ => return Err(E_SESSION),
    };
    if data.is_empty() {
        return Ok(0); // 空输入不产空帧（流结束由 sr_zstd_end 承担，无尾段概念）
    }
    // 先压缩到内部暂存再验容量：容量不足时不写任何字节、不改变会话状态（可扩容原样重试）
    st.buf.clear();
    st.buf.resize(zstd_bound(data.len()), 0);
    let n = st.comp.compress_to_buffer(data, st.buf.as_mut_slice()).map_err(|_| E_CRYPTO)?;
    if out.is_null() || cap < n {
        // 报告值下限 40：小输入的帧可 < 40B，须与错误码区间（|e| ≤ 7）区分
        return Err(-(n.max(40) as i32));
    }
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out, n) };
    out_slice.copy_from_slice(&st.buf[..n]);
    Ok(n as i32)
}

/// 开启 zstd 压缩会话：level 1..=22（控制台上传用 19）。返回会话句柄（>0）或负错误码。
#[no_mangle]
pub extern "C" fn sr_zstd_begin(level: i32) -> i32 {
    guarded(|| {
        if !(1..=22).contains(&level) {
            return Err(E_ARGS);
        }
        let comp = zstd::bulk::Compressor::new(level).map_err(|_| E_CRYPTO)?;
        let st = ZstdState { comp, buf: Vec::new() };
        let mut v = ZSTD_SESSIONS.lock().unwrap();
        v.push(Some(st)); // 槽位只增不回收（ponytail；句柄从 1 起）
        Ok(v.len() as i32)
    })
}

/// 推入待压缩字节：本次输入被压成**一个独立完整的 zstd 帧**（多次 push 的输出按序拼接
/// 即为合法拼接帧流，服务端流式解码器跨帧续解）。返回本次写出的字节数；容量不足返回
/// -(所需字节数)（下限 40）。空输入（len=0）合法但产出 0 字节；结束即 sr_zstd_end。
#[no_mangle]
pub extern "C" fn sr_zstd_push(
    sid: i32,
    data: *const u8,
    len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i32 {
    guarded(|| {
        let data = unsafe { slice_const(data, len)?};
        zstd_push(sid, data, out, out_cap)
    })
}

/// 结束并丢弃压缩会话（输出为逐帧拼接的确定产物，无需收尾 flush）。
#[no_mangle]
pub extern "C" fn sr_zstd_end(sid: i32) -> i32 {
    guarded(|| {
        if sid <= 0 {
            return Err(E_SESSION);
        }
        let mut v = ZSTD_SESSIONS.lock().unwrap();
        match v.get_mut(sid as usize - 1) {
            Some(slot) => {
                *slot = None;
                Ok(0)
            }
            None => Err(E_SESSION),
        }
    })
}

// —— OPAQUE 登录/注册会话（plan-16 / SPEC §16；与 seal 会话同构的句柄模型）——
//
// 浏览器侧一步登录（账号+密码）与登录态改密码的 native 内核入口：
// - `sr_login_start/sr_login_ke1/sr_login_finish`：登录三步 ke1→ke2→ke3（rwd 硬化在垫片内部完成）；
// - `sr_reg_start/sr_reg_msg/sr_reg_finish`：改密码 m1→m2→record（pw-commit 上行）。
// ABI 惯例与本文件其余部分一致：b64 串输入指针+长度、输出 b64 串 NUL 结尾写入调用方
// 缓冲（容量不足返回 -(所需字节数)）；盲化因子与 KE 临时份额取全局熵池；会话槽位只增
// 不回收、finish 一次性消费（ponytail）；密码错误/协议失败 = -7（本地失败，线上无差异）。

use crate::opaque::Suite;
use opaque_ke::{ClientLogin, ClientRegistration};

struct OpaqueLoginSession {
    state: ClientLogin<Suite>,
    /// rwd 盐与三方标识都要 user；状态对象不含它，会话保存。
    user: String,
    /// ke1（b64）——幂等可重复取。
    ke1: String,
}

struct OpaqueRegSession {
    state: ClientRegistration<Suite>,
    user: String,
    /// m1（b64）。
    m1: String,
}

enum OpaqueState {
    Login(OpaqueLoginSession),
    Reg(OpaqueRegSession),
}

static OPAQUE_SESSIONS: Mutex<Vec<Option<OpaqueState>>> = Mutex::new(Vec::new());

fn opaque_slot(st: OpaqueState) -> Result<i32, i32> {
    let mut v = OPAQUE_SESSIONS.lock().unwrap();
    v.push(Some(st)); // 槽位只增不回收（ponytail；句柄从 1 起）
    Ok(v.len() as i32)
}

/// 一次性取走会话（finish 语义：取后槽位即空，重复 finish = E_SESSION）。
fn opaque_take(sid: i32) -> Result<OpaqueState, i32> {
    if sid <= 0 {
        return Err(E_SESSION);
    }
    let mut v = OPAQUE_SESSIONS.lock().unwrap();
    match v.get_mut(sid as usize - 1) {
        Some(slot) => slot.take().ok_or(E_SESSION),
        None => Err(E_SESSION),
    }
}

/// 读会话首条消息（ke1/m1，幂等只读）。
fn opaque_msg(sid: i32) -> Result<String, i32> {
    if sid <= 0 {
        return Err(E_SESSION);
    }
    let v = OPAQUE_SESSIONS.lock().unwrap();
    match v.get(sid as usize - 1) {
        Some(Some(OpaqueState::Login(s))) => Ok(s.ke1.clone()),
        Some(Some(OpaqueState::Reg(s))) => Ok(s.m1.clone()),
        _ => Err(E_SESSION),
    }
}

/// 开启登录会话（熵池驱动盲化）：返回句柄（>0）或负错误码（-6 熵未注入 / -7 协议失败）。
#[no_mangle]
pub extern "C" fn sr_login_start(
    pw: *const u8,
    pw_len: usize,
    user: *const u8,
    user_len: usize,
) -> i32 {
    guarded(|| {
        let pw = utf8(pw, pw_len)?;
        let user = utf8(user, user_len)?;
        if pw.is_empty() || user.is_empty() {
            return Err(E_ARGS);
        }
        let got = with_global_rng(|r| crate::opaque::login_start(r, &pw, &user));
        let (state, ke1) = match got {
            Err(_) => return Err(E_ENTROPY),
            Ok(Err(_)) => return Err(E_CRYPTO),
            Ok(Ok(v)) => v,
        };
        opaque_slot(OpaqueState::Login(OpaqueLoginSession { state, user, ke1 }))
    })
}

/// 取登录会话的 ke1（b64，NUL 结尾写 out；幂等可重复取）。
#[no_mangle]
pub extern "C" fn sr_login_ke1(sid: i32, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        let ke1 = opaque_msg(sid)?;
        write_cstr(&ke1, out, cap)
    })
}

/// 登录收尾：喂 ke2（b64）+ 密码 → 写出 ke3（b64，NUL 结尾）。密码错误 = -7（本地失败，
/// 不产生第三请求）。成功后会话终结（句柄不可复用）。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn sr_login_finish(
    sid: i32,
    pw: *const u8,
    pw_len: usize,
    ke2: *const u8,
    ke2_len: usize,
    out: *mut u8,
    cap: usize,
) -> i32 {
    guarded(|| {
        let pw = utf8(pw, pw_len)?;
        let ke2 = utf8(ke2, ke2_len)?;
        if pw.is_empty() || ke2.is_empty() {
            return Err(E_ARGS);
        }
        let st = opaque_take(sid)?;
        let OpaqueState::Login(s) = st else {
            return Err(E_ARGS); // 注册会话句柄误用
        };
        let user = s.user.clone();
        let (ke3, sk) =
            with_global_rng(|r| crate::opaque::login_finish(r, s.state, &pw, &user, &ke2))
                .map_err(|_| E_CRYPTO)?
                .map_err(|_| E_CRYPTO)?;
        // AKE 会话密钥留槽（sr_login_session_key 取用；plan-18 密钥绑定验证）
        login_key_set(sid, sk);
        write_cstr(&ke3, out, cap)
    })
}

// —— 登录 AKE 会话密钥槽（与 OPAQUE 会话同 sid 索引；end 即清）——

static LOGIN_KEYS: Mutex<Vec<Option<Vec<u8>>>> = Mutex::new(Vec::new());

fn login_key_set(sid: i32, key: Vec<u8>) {
    let mut v = LOGIN_KEYS.lock().unwrap();
    while v.len() < sid as usize {
        v.push(None);
    }
    v[sid as usize - 1] = Some(key);
}

/// 取本次登录的 AKE 会话密钥（32B；plan-18：验证 srtp 下发绑定的 key_mac）。
/// 返回写出的字节数或负错误码（容量不足报所需，≤-40 协议）。
#[no_mangle]
pub extern "C" fn sr_login_session_key(sid: i32, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        if sid <= 0 {
            return Err(E_SESSION);
        }
        let v = LOGIN_KEYS.lock().unwrap();
        let key = v.get(sid as usize - 1).and_then(|s| s.as_ref()).ok_or(E_SESSION)?;
        if key.len() > cap {
            return Err(-(key.len() as i32));
        }
        let mut w = W { buf: unsafe { std::slice::from_raw_parts_mut(out, cap) }, pos: 0 };
        w.put(key);
        Ok(w.pos as i32)
    })
}

/// 开启注册（改密码）会话：新密码+user → m1。熵池驱动。
#[no_mangle]
pub extern "C" fn sr_reg_start(
    pw: *const u8,
    pw_len: usize,
    user: *const u8,
    user_len: usize,
) -> i32 {
    guarded(|| {
        let pw = utf8(pw, pw_len)?;
        let user = utf8(user, user_len)?;
        if pw.is_empty() || user.is_empty() {
            return Err(E_ARGS);
        }
        let got = with_global_rng(|r| crate::opaque::reg_start(r, &pw, &user));
        let (state, m1) = match got {
            Err(_) => return Err(E_ENTROPY),
            Ok(Err(_)) => return Err(E_CRYPTO),
            Ok(Ok(v)) => v,
        };
        opaque_slot(OpaqueState::Reg(OpaqueRegSession { state, user, m1 }))
    })
}

/// 取注册会话的 m1（b64；幂等可重复取）。
#[no_mangle]
pub extern "C" fn sr_reg_msg(sid: i32, out: *mut u8, cap: usize) -> i32 {
    guarded(|| {
        let m1 = opaque_msg(sid)?;
        write_cstr(&m1, out, cap)
    })
}

/// 注册收尾：喂 m2（b64）+ 新密码 → 写出 record（b64，pw-commit 上行）。成功后会话终结。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn sr_reg_finish(
    sid: i32,
    pw: *const u8,
    pw_len: usize,
    m2: *const u8,
    m2_len: usize,
    out: *mut u8,
    cap: usize,
) -> i32 {
    guarded(|| {
        let pw = utf8(pw, pw_len)?;
        let m2 = utf8(m2, m2_len)?;
        if pw.is_empty() || m2.is_empty() {
            return Err(E_ARGS);
        }
        let st = opaque_take(sid)?;
        let OpaqueState::Reg(s) = st else {
            return Err(E_ARGS); // 登录会话句柄误用
        };
        let record = with_global_rng(|r| crate::opaque::reg_finish(r, s.state, &pw, &s.user, &m2))
            .map_err(|_| E_CRYPTO)?
            .map_err(|_| E_CRYPTO)?;
        write_cstr(&record, out, cap)
    })
}
