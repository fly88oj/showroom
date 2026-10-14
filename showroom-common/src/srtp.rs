//! srtp：传输层信封流式状态机（隧道网关用；SPEC §2 信封格式的流式适配）。
//!
//! 字节格式与 lib.rs `seal`/`open` 及 wasm_shim 会话完全同构：
//! 上行 = `[384B RSA-OAEP-SHA256 封装的 k] || 每段[12B nonce][密文+16B tag]`（段=4MiB 明文）；
//! 下行 = 复用上行解出的同一 k（对称会话，TLS 式：两端知道 k，管道只见高熵字节），
//! 无 wrap 头、段格式相同。传输帧在信封外再加 6B 头：`"SRTP" 0x01 <flags>`
//!（flags bit0=0 上行带 wrap / 1 下行会话密钥直传），用于网关与明文 HTTP 混流探测。
//!
//! 本模块是纯状态机（push 进 / Vec 出，无 I/O、无 OS 熵源引用——wasm32 可编译）；
//! 随机数由构造方注入（网关注入 OsRng；测试注入种子 StdRng 求可复现）。
//! wasm_shim 的 C-ABI 会话是同一格式的 wasm 投影（容量协商语义见彼处注释），
//! 客户端侧 SRTP 采用时在垫片补对称入口，格式以本模块 + §2 为单源。

use std::fmt;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use rand::{CryptoRng, RngCore};
use rsa::pkcs8::DecodePrivateKey;
use sha2::Sha256;
use rsa::{Oaep, RsaPrivateKey};

use crate::{KEY_LEN, NONCE_LEN, SEG, TAG_LEN, WRAP_LEN};

/// 会话随机源（RngCore+CryptoRng 的 trait 对象别名；网关注入 OsRng、测试注入种子 StdRng）。
/// +Send：网关的密封泵任务跨 await 持有（tokio::spawn 的 Send 界）。
pub trait SessionRng: RngCore + CryptoRng {}
impl<T: RngCore + CryptoRng> SessionRng for T {}

/// 传输帧头：`"SRTP" <版本=0x01> <flags>`。魔数与 HTTP 方法（GET/POST/…）及 TLS 0x16 无碰撞。
pub const MAGIC: &[u8; 4] = b"SRTP";
pub const HDR_LEN: usize = 6;
pub const V1: u8 = 0x01;
/// flags bit0：0 = 上行（首段前有 384B wrap）；1 = 下行（会话密钥直传，无 wrap）。
pub const FLAG_UPLINK: u8 = 0x00;
pub const FLAG_DOWNLINK: u8 = 0x01;

/// 6B 传输头（上行/下行）。
pub fn header(downlink: bool) -> [u8; HDR_LEN] {
    let mut h = [0u8; HDR_LEN];
    h[..4].copy_from_slice(MAGIC);
    h[4] = V1;
    h[5] = if downlink { FLAG_DOWNLINK } else { FLAG_UPLINK };
    h
}

/// 是否以 SRTP 传输头开头（网关混合模式探测；参数=连接首批字节）。
pub fn is_srtp(prefix: &[u8]) -> bool {
    prefix.len() >= HDR_LEN
        && &prefix[..4] == MAGIC
        && prefix[4] == V1
        && (prefix[5] & !FLAG_DOWNLINK) == 0 // 仅 bit0 允许置位
}

#[derive(Debug, PartialEq, Eq)]
pub enum SrtpError {
    /// 流截断（wrap/段/尾段不齐）。
    Truncated,
    /// 段完整性校验失败（数据损坏或密钥不匹配）。
    AuthFailed,
    /// wrap 解封失败（非本平台私钥所封 / 模长不符）。
    BadKey,
    /// 流已结束后继续推入（API 误用）。
    Misuse,
}

impl fmt::Display for SrtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SrtpError::Truncated => write!(f, "srtp: 截断的信封流"),
            SrtpError::AuthFailed => write!(f, "srtp: 段校验失败（损坏或密钥不匹配）"),
            SrtpError::BadKey => write!(f, "srtp: wrap 头解封失败"),
            SrtpError::Misuse => write!(f, "srtp: 流已结束"),
        }
    }
}
impl std::error::Error for SrtpError {}

/// wrap 头解封（网关用：384B RSA-OAEP → 会话密钥 k）。
pub fn unwrap_key(priv_pem: &str, wrap: &[u8]) -> Result<[u8; KEY_LEN], SrtpError> {
    let k = RsaPrivateKey::from_pkcs8_pem(priv_pem)
        .map_err(|_| SrtpError::BadKey)?
        .decrypt(Oaep::new::<Sha256>(), wrap)
        .map_err(|_| SrtpError::BadKey)?;
    k.as_slice().try_into().map_err(|_| SrtpError::BadKey)
}

/// 下行封装流（已知会话密钥 k；段=4MiB 缓冲，nonce 每段随机）。
pub struct SealStream {
    cipher: Aes256Gcm,
    rng: Box<dyn SessionRng + Send>,
    carry: Vec<u8>,
    finished: bool,
}

impl SealStream {
    pub fn new(k: [u8; KEY_LEN], rng: Box<dyn SessionRng + Send>) -> Self {
        SealStream {
            cipher: Aes256Gcm::new(GenericArray::from_slice(&k)),
            rng,
            carry: Vec::new(),
            finished: false,
        }
    }

    /// 推入明文：满 4MiB 段即时封装写出；不足一段暂存。
    pub fn push(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), SrtpError> {
        if self.finished {
            return Err(SrtpError::Misuse);
        }
        self.carry.extend_from_slice(plain);
        while self.carry.len() >= SEG {
            let seg: Vec<u8> = self.carry.drain(..SEG).collect();
            self.seg(&seg, out)?;
        }
        Ok(())
    }

    /// 流结束：flush 尾段（若 carry 非空）；此后流已关闭，再次调用为 Misuse。
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), SrtpError> {
        if self.finished {
            return Err(SrtpError::Misuse);
        }
        if !self.carry.is_empty() {
            let seg = std::mem::take(&mut self.carry);
            self.seg(&seg, out)?;
        }
        self.finished = true;
        Ok(())
    }

    fn seg(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), SrtpError> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(GenericArray::from_slice(&nonce), plain)
            .map_err(|_| SrtpError::AuthFailed)?;
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(())
    }
}

/// 解封流：上行模式（wrap 头 + 段）或会话密钥模式（段直传，客户端下行接收用）。
/// 任意分片边界推入；`session_key()` 在 wrap 解出（或密钥模式构造）后可用。
pub struct OpenStream {
    priv_key: Option<RsaPrivateKey>,
    key: Option<[u8; KEY_LEN]>,
    cipher: Option<Aes256Gcm>,
    buf: Vec<u8>,
    finished: bool,
}

impl OpenStream {
    /// 上行模式：wrap 头到齐时以平台私钥解出 k（私钥材料随即离场）。
    pub fn begin_wrap(priv_pem: &str) -> Result<Self, SrtpError> {
        let k = RsaPrivateKey::from_pkcs8_pem(priv_pem).map_err(|_| SrtpError::BadKey)?;
        Ok(OpenStream {
            priv_key: Some(k),
            key: None,
            cipher: None,
            buf: Vec::new(),
            finished: false,
        })
    }

    /// 会话密钥模式：直接以 k 建段解密（无 wrap）。
    pub fn begin_key(k: [u8; KEY_LEN]) -> Self {
        OpenStream {
            priv_key: None,
            key: Some(k),
            cipher: Some(Aes256Gcm::new(GenericArray::from_slice(&k))),
            buf: Vec::new(),
            finished: false,
        }
    }

    /// 会话密钥（wrap 解出或密钥模式构造后可见；网关下行封装取用）。
    pub fn session_key(&self) -> Option<[u8; KEY_LEN]> {
        self.key
    }

    /// 推入信封字节：任意分片边界；解出的明文追加到 out。
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), SrtpError> {
        if self.finished {
            return Err(SrtpError::Misuse);
        }
        self.buf.extend_from_slice(data);
        if self.cipher.is_none() {
            if self.buf.len() < WRAP_LEN {
                return Ok(()); // 头未到齐，暂存
            }
            let wrap: Vec<u8> = self.buf.drain(..WRAP_LEN).collect();
            let pk = self.priv_key.take().ok_or(SrtpError::Misuse)?;
            let k = pk
                .decrypt(Oaep::new::<Sha256>(), &wrap)
                .map_err(|_| SrtpError::BadKey)?;
            let k: [u8; KEY_LEN] = k.as_slice().try_into().map_err(|_| SrtpError::BadKey)?;
            self.key = Some(k);
            self.cipher = Some(Aes256Gcm::new(GenericArray::from_slice(&k)));
        }
        let cipher = self.cipher.clone().expect("cipher 已建立");
        let blk = NONCE_LEN + SEG + TAG_LEN;
        while self.buf.len() >= blk {
            let mut nonce = [0u8; NONCE_LEN];
            nonce.copy_from_slice(&self.buf[..NONCE_LEN]);
            self.buf.drain(..NONCE_LEN);
            let ct: Vec<u8> = self.buf.drain(..SEG + TAG_LEN).collect();
            let pt = cipher
                .decrypt(GenericArray::from_slice(&nonce), ct.as_slice())
                .map_err(|_| SrtpError::AuthFailed)?;
            out.extend_from_slice(&pt);
        }
        Ok(())
    }

    /// 上行流结束（对端 EOF）：剩余 buf 为尾段（段边界上的干净收尾则 buf 为空）。
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), SrtpError> {
        if self.finished {
            return Err(SrtpError::Misuse);
        }
        if self.cipher.is_none() {
            // wrap 头都未到齐
            return Err(SrtpError::Truncated);
        }
        if !self.buf.is_empty() {
            if self.buf.len() < NONCE_LEN + TAG_LEN {
                return Err(SrtpError::Truncated);
            }
            let cipher = self.cipher.clone().expect("cipher 已建立");
            let mut nonce = [0u8; NONCE_LEN];
            nonce.copy_from_slice(&self.buf[..NONCE_LEN]);
            self.buf.drain(..NONCE_LEN);
            let ct = std::mem::take(&mut self.buf);
            let pt = cipher
                .decrypt(GenericArray::from_slice(&nonce), ct.as_slice())
                .map_err(|_| SrtpError::AuthFailed)?;
            out.extend_from_slice(&pt);
        }
        self.finished = true;
        Ok(())
    }
}


// —— 传输记录层（TLS 式自定界记录；与上面 §2 流式状态机并存的传输专用形态）——
// §2 信封的定长段（4MiB 全段 + EOF 尾段）适合整包上传，不适合流式小请求：尾段只在
// 流结束时才可解密，而中继又不支持半关闭（真机实弹）——记录必须逐条即时可解。
// 记录 = [12B nonce][4B 大端密文长][密文||16B tag]，密文长=明文长+16。

/// 传输记录写入端（已知会话密钥；每条记录独立随机 nonce，无跨记录状态）。
pub struct TWriter {
    cipher: Aes256Gcm,
    rng: Box<dyn SessionRng + Send>,
}

impl TWriter {
    pub fn new(k: [u8; KEY_LEN], rng: Box<dyn SessionRng + Send>) -> Self {
        TWriter { cipher: Aes256Gcm::new(GenericArray::from_slice(&k)), rng }
    }

    /// 把一段明文封成一条完整记录追加到 out（立即产出，无缓冲）。
    pub fn record(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), SrtpError> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(GenericArray::from_slice(&nonce), plain)
            .map_err(|_| SrtpError::AuthFailed)?;
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&(ct.len() as u32).to_be_bytes());
        out.extend_from_slice(&ct);
        Ok(())
    }
}

/// 传输记录读取端（任意分片喂入；完整记录即时解出）。
pub struct TReader {
    cipher: Aes256Gcm,
    buf: Vec<u8>,
}

impl TReader {
    pub fn new(k: [u8; KEY_LEN]) -> Self {
        TReader { cipher: Aes256Gcm::new(GenericArray::from_slice(&k)), buf: Vec::new() }
    }

    /// 喂入网络字节；每解出一条记录即把明文追加到 out。
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), SrtpError> {
        self.buf.extend_from_slice(data);
        loop {
            if self.buf.len() < NONCE_LEN + 4 {
                return Ok(());
            }
            let ct_len =
                u32::from_be_bytes(self.buf[NONCE_LEN..NONCE_LEN + 4].try_into().expect("4B")) as usize;
            if ct_len < TAG_LEN {
                return Err(SrtpError::Truncated); // 坏长度：密文至少含 tag
            }
            let total = NONCE_LEN + 4 + ct_len;
            if self.buf.len() < total {
                return Ok(()); // 记录未到齐
            }
            let mut nonce = [0u8; NONCE_LEN];
            nonce.copy_from_slice(&self.buf[..NONCE_LEN]);
            let pt = self
                .cipher
                .decrypt(GenericArray::from_slice(&nonce), &self.buf[NONCE_LEN + 4..total])
                .map_err(|_| SrtpError::AuthFailed)?;
            out.extend_from_slice(&pt);
            self.buf.drain(..total);
        }
    }

    /// 流结束校验：无残字节。
    pub fn finish(&self) -> Result<(), SrtpError> {
        if self.buf.is_empty() { Ok(()) } else { Err(SrtpError::Truncated) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{seal, PlatformKeys};
    use rsa::pkcs8::DecodePublicKey;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn seeded() -> Box<dyn SessionRng + Send> {
        Box::new(StdRng::from_seed([7u8; 32]))
    }

    #[test]
    fn uplink_roundtrip_chunked_against_canonical_seal() {
        // canonical lib::seal 输出（随机 k + wrap）经 OpenStream 任意分片解封须逐字节还原
        let keys = PlatformKeys::generate();
        let priv_pem = keys.priv_pkcs8_pem();
        let pubk = rsa::RsaPublicKey::from_public_key_pem(&keys.public_pem()).unwrap();
        let plain: Vec<u8> = (0..(SEG as u32 + 100)).map(|i| (i % 251) as u8).collect();
        let mut sealed = Vec::new();
        seal(&pubk, &plain[..], &mut sealed).unwrap();

        let mut st = OpenStream::begin_wrap(&priv_pem).unwrap();
        let mut out = Vec::new();
        for chunk in sealed.chunks(97) {
            // 97 与段/头边界互质，覆盖任意分片路径
            st.push(chunk, &mut out).unwrap();
        }
        assert!(st.session_key().is_some());
        st.finish(&mut out).unwrap();
        assert_eq!(out, plain);
    }

    #[test]
    fn downlink_roundtrip_with_session_key() {
        let k = [9u8; KEY_LEN];
        let plain = b"HTTP/1.1 200 OK\r\n\r\nhello".to_vec();
        let mut w = SealStream::new(k, seeded());
        let mut wire = Vec::new();
        w.push(&plain, &mut wire).unwrap();
        w.finish(&mut wire).unwrap();

        let mut r = OpenStream::begin_key(k);
        let mut out = Vec::new();
        for chunk in wire.chunks(13) {
            r.push(chunk, &mut out).unwrap();
        }
        r.finish(&mut out).unwrap();
        assert_eq!(out, plain);
    }

    #[test]
    fn corruption_and_truncation_fail() {
        let k = [3u8; KEY_LEN];
        let mut w = SealStream::new(k, seeded());
        let mut wire = Vec::new();
        w.push(b"payload", &mut wire).unwrap();
        w.finish(&mut wire).unwrap();
        let bad = wire.len() / 2;
        wire[bad] ^= 0xff;

        let mut r = OpenStream::begin_key(k);
        let mut out = Vec::new();
        // 小载荷整体是尾段：push 只缓冲，损坏在 finish 解密时暴露
        r.push(&wire, &mut out).unwrap();
        assert_eq!(r.finish(&mut out), Err(SrtpError::AuthFailed));

        // 截断用干净 wire（上一段的翻转会先触发 AuthFailed，两个失败模式分开验证）
        let mut w3 = SealStream::new(k, seeded());
        let mut wire3 = Vec::new();
        w3.push(b"payload", &mut wire3).unwrap();
        w3.finish(&mut wire3).unwrap();
        let mut r3 = OpenStream::begin_key(k);
        let mut o3 = Vec::new();
        // 段内截 1B 在 GCM 侧表现为 AuthFailed（与损坏不可区分）；真正的 Truncated =
        // 连 nonce+tag 都不齐。这里截到 27B（< 12+16）验证后者。
        r3.push(&wire3[..NONCE_LEN + TAG_LEN - 1], &mut o3).unwrap();
        assert_eq!(r3.finish(&mut o3), Err(SrtpError::Truncated));
    }

    #[test]
    fn transport_records_streaming_roundtrip() {
        let k = [0x1fu8; KEY_LEN];
        let mut w = TWriter::new(k, seeded());
        let mut wire = Vec::new();
        for chunk in [b"GET /".as_slice(), b"_api/he", b"alth HTTP/1.1\r\n\r\n"] {
            w.record(chunk, &mut wire).unwrap(); // 每小块独立成记录
        }
        let mut r = TReader::new(k);
        let mut out = Vec::new();
        for c in wire.chunks(7) {
            r.push(c, &mut out).unwrap();
        }
        r.finish().unwrap();
        assert_eq!(out, b"GET /_api/health HTTP/1.1\r\n\r\n");
        // 损坏检测
        let mut bad = wire.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        let mut r2 = TReader::new(k);
        assert!(r2.push(&bad, &mut Vec::new()).is_err());
    }

    #[test]
    fn magic_detects_srtp_not_http() {
        assert!(is_srtp(&header(false)));
        assert!(is_srtp(&header(true)));
        assert!(!is_srtp(b"GET / HTTP/1.1\r\n"));
        assert!(!is_srtp(b"POST /_api/x HTTP/1.1"));
        assert!(!is_srtp(&[0x16, 0x03, 0x01, 0, 0, 0, 0, 0]));
        assert!(!is_srtp(b"SRTP")); // 长度不足
    }
}
