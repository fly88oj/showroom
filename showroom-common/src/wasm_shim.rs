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
//!
//! 随机性：wasm32-unknown-unknown 无 OS 熵源（getrandom 该目标无后端，SPEC §8.3 又禁引
//! wasm-bindgen），熵由 JS 侧 crypto.getRandomValues 生成后经 `sr_entropy()` 注入，
//! 驱动全局 StdRng（ChaCha12，CSPRNG）。seal 会话另支持确定性 seed（sr_seal_begin_seed）：
//! 同 seed + 同 push 序列 ⇒ 逐字节相同的信封，供 §8.1 断点续传在页面刷新后重放已传分片
//! （安全等级与 localStorage 存 owner seed 的 P1 试验档一致）。
//!
//! ponytail 天花板（明知且接受）：sr_alloc/sr_free 为全局 HashMap 指针池，不合并碎片、
//! 不抗双重释放以外的误用；会话表为只增不回收的 Vec 槽位；锁在 wasm 单线程下无争用。

#![cfg(target_arch = "wasm32")]

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Mutex;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::engine::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey};
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

/// 由 32B seed 导出公钥串 "ed25519:<b64-nopad>"（与 common::OwnerKey::pub_string 逐字节一致）。
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
/// `SR-SIG1 <owner>:<ts>:<nonce>:<sig_b64url>`；body 取原始字节（sha256 在内部计算）。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn sr_sign_header(
    seed: *const u8,
    owner: *const u8,
    owner_len: usize,
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
        let owner = utf8(owner, owner_len)?;
        let method = utf8(method, method_len)?;
        let path = utf8(path_q, path_len)?;
        let nonce = utf8(nonce, nonce_len)?;
        let body = unsafe { slice_const(body, body_len)?};
        if owner.is_empty() || nonce.is_empty() || method.is_empty() || path.is_empty() {
            return Err(E_ARGS);
        }
        let sk = SigningKey::from_bytes(&arr);
        let canonical =
            build_canonical(&owner, ts, &nonce, &method, &path, &sha256_hex(body));
        let sig = sk.sign(canonical.as_bytes());
        let header =
            format!("SR-SIG1 {owner}:{ts}:{nonce}:{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
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
        cipher: Aes256Gcm::new(GenericArray::from_slice(&k)),
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
        .encrypt(GenericArray::from_slice(&nonce), plain)
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
