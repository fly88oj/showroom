//! SRTP 一级传输（公网通道协议层密封，SPEC §2.1）+ 两级自动回退。
//!
//! CLI 拥有 socket：对公网入口直接 TCP 连接，`"SRTP"\x01\x00` + wrap(k) + 密封的
//! HTTP/1.1 请求（Connection: close）；服务端网关解封转 Caddy，响应以同一 k 对称
//! 密封返回。中转全程只见高熵字节（URL/账号名/JSON 体一并密封）。
//!
//! 两级处理（用户裁决）：**协议层失败即回退原有 reqwest 明文逻辑，零配置零感知**。
//! 回退判定分级：收到字节但非 SRTP 应答 = 对端网关不支持（进程内粘性降级，本次
//! 及后续请求直接走二级）；网络类失败 = 不粘性（下个请求可再试一级）。
//! 仅公网通道启用（LAN 直连不过网关，一级无从谈起）。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use showroom_common::rsa::{Oaep, RsaPublicKey};
use showroom_common::srtp::{header, SessionRng, TReader, TWriter, HDR_LEN};
use showroom_common::{KEY_LEN, WRAP_LEN};
use sha2::Sha256;

/// 连接/读/写超时（dump 切片 ≤4MB、import 切片 ≤8MB，600s 覆盖慢链路；
/// 原有 reqwest 路径的 24h 宽超时语义不变——二级回退仍在）。
const IO_TIMEOUT: Duration = Duration::from_secs(600);

/// SRTP 一级响应（状态码 + 明文体）。
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// base（http://host[:port]）→ (host 头值, SocketAddr)；缺省端口 80。
fn base_parts(base: &str) -> Option<(String, SocketAddr)> {
    let rest = base.strip_prefix("http://").or_else(|| base.strip_prefix("https://"))?;
    if base.starts_with("https://") {
        return None; // 一级只做明文 TCP 入口（隧道场景即 http）；https 交给二级
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h.to_string(), p.parse().ok()?),
        _ => (rest.trim_end_matches('/').to_string(), 80),
    };
    use std::net::ToSocketAddrs as _;
    let addr = host_with_port(&host, port).to_socket_addrs().ok()?.next()?;
    Some((host, addr))
}

fn host_with_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// HTTP/1.1 请求序列化（Connection: close——一级每请求一连接，语义最简）。
fn http_request_bytes(method: &str, path_q: &str, host: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {path_q} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// HTTP/1.1 响应解析：状态行 + 头 + 体（Content-Length / chunked / 读到 EOF）。
fn parse_http_response(raw: &[u8]) -> Option<Response> {
    let head_end = find_sub(raw, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let status: u16 = status_line.split_ascii_whitespace().nth(1)?.parse().ok()?;
    let mut chunked = false;
    let mut content_len: Option<usize> = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        if k == "content-length" {
            content_len = v.parse().ok();
        }
    }
    let rest = &raw[head_end + 4..];
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(n) = content_len {
        rest.get(..n)?.to_vec()
    } else {
        rest.to_vec() // Connection: close：体到 EOF
    };
    Some(Response { status, body })
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// chunked 解码（大小块 + 尾部 trailer 行）。
fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find_sub(b, b"\r\n")?;
        let size = usize::from_str_radix(std::str::from_utf8(&b[..line_end]).ok()?.split(';').next()?.trim(), 16).ok()?;
        b = &b[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        let chunk = b.get(..size)?;
        out.extend_from_slice(chunk);
        b = &b[size..];
        let sep = b.get(..2)?;
        if sep != b"\r\n" {
            return None;
        }
        b = &b[2..];
    }
}

/// 一级请求（可注入 tier 原子量——单测各自持有，免进程级静态互扰）。
/// 返回 None = 一级不可用（调用方回退二级）。
pub fn request_with_tier(
    tier: &AtomicU8,
    base: &str,
    method: &str,
    path_q: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    platform_pub: &RsaPublicKey,
) -> Option<Response> {
    if tier.load(std::sync::atomic::Ordering::Relaxed) == 2 {
        return None; // 粘性降级：对端网关不支持
    }
    let (host, addr) = base_parts(base)?;
    // SRTP v1 上行记录 = [8B 大端长度][请求字节]：网关按帧转发（中继不支持半关闭，
    // 帧界是唯一的请求完结信号，真机实弹教训——见 gateway::FrameFwd）
    let req = http_request_bytes(method, path_q, &host, headers, body);
    let mut framed = (req.len() as u64).to_be_bytes().to_vec();
    framed.extend_from_slice(&req);

    // 会话密钥 k：RSA-OAEP 包裹给平台（即 §2 信封 wrap；下行以同 k 对称返回）
    let mut rng = rand::rngs::OsRng;
    let mut k = [0u8; KEY_LEN];
    use rand::RngCore as _;
    rng.fill_bytes(&mut k);
    let wrapped = platform_pub.encrypt(&mut rng, Oaep::new::<Sha256>(), &k).ok()?;
    if wrapped.len() != WRAP_LEN {
        return None;
    }

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(20)).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_nodelay(true).ok()?;

    // 上行：帧头(flags=0) + wrap + 密封请求 + 流结束尾段
    let mut up = header(false).to_vec();
    up.extend_from_slice(&wrapped);
    let mut tw = TWriter::new(k, Box::new(rand::rngs::OsRng) as Box<dyn SessionRng + Send>);
    for piece in framed.chunks(64 * 1024) {
        tw.record(piece, &mut up).ok()?;
    }
    stream.write_all(&up).ok()?;
    // 关键（真机实弹教训）：**不做 shutdown(Write) 半关闭**——frp 中继在用户侧 EOF 时
    // 拆整条工作流（而非向后端半关闭），网关下行写会在到达前被丢弃。请求自带
    // Content-Length + Connection: close，服务端据此应答并收向；网关 down_task 在上游
    // EOF 后 flush+shutdown，客户端 read_to_end 以 FIN 自然收尾。

    // 下行：帧头(flags=1) + 密封响应，读到 EOF。上行已完整发出（含 FIN），此后读阶段
    // 任何失败或非 SRTP 应答（明文 400/RST/空回显）都只有一个解释：对端链上无 SRTP
    // 网关——粘性降级（网络抖动误伤可接受：tier 生命周期=本进程，下次运行重新探测）。
    let mut down = Vec::new();
    let _ = stream.read_to_end(&mut down);
    if down.len() < HDR_LEN || down[..HDR_LEN] != header(true) {
        tier.store(2, std::sync::atomic::Ordering::Relaxed);
        return None;
    }
    let mut rd = TReader::new(k);
    let mut plain = Vec::new();
    rd.push(&down[HDR_LEN..], &mut plain).ok()?;
    rd.finish().ok()?;
    let resp = parse_http_response(&plain)?;
    tier.store(1, std::sync::atomic::Ordering::Relaxed);
    Some(resp)
}

/// 进程级 tier（0=未定 1=一级可用 2=对端不支持——粘性；网络失败不粘性）。
static TIER: AtomicU8 = AtomicU8::new(0);

pub fn request(
    base: &str,
    method: &str,
    path_q: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    platform_pub: &RsaPublicKey,
) -> Option<Response> {
    request_with_tier(&TIER, base, method, path_q, headers, body, platform_pub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use showroom_common::PlatformKeys;
    use std::net::TcpListener;

    fn http_bytes_ok() -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec()
    }

    #[test]
    fn response_parse_shapes() {
        let r = parse_http_response(&http_bytes_ok()).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"hello".as_slice()));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2\r\nlo\r\n0\r\n\r\n";
        let r2 = parse_http_response(chunked).unwrap();
        assert_eq!(r2.body, b"hello");
        let eof = b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n{\"error\":\"E_AUTH\"}";
        let r3 = parse_http_response(eof).unwrap();
        assert_eq!(r3.status, 404);
    }

    #[test]
    fn request_serialize_roundtrip_fields() {
        let b = http_request_bytes("POST", "/_api/x?a=1", "h:80", &[("Authorization", "SR-SIG1 u")], b"body");
        let s = String::from_utf8(b).unwrap();
        assert!(s.starts_with("POST /_api/x?a=1 HTTP/1.1\r\n"));
        assert!(s.contains("Host: h:80\r\n"));
        assert!(s.contains("Connection: close\r\n"));
        assert!(s.contains("Content-Length: 4\r\n"));
        assert!(s.contains("Authorization: SR-SIG1 u\r\n"));
        assert!(s.ends_with("\r\n\r\nbody"));
    }

    /// 回环伪网关：读 6B 头 + wrap + 密封流 → 解封出 HTTP → 以同 k 密封响应
    ///（与 ctl 侧 gateway::sealed 同构，交叉验证两端实现）。
    use showroom_common::rsa::pkcs8::DecodePublicKey as _;

    /// 回环伪网关（与 ctl 侧 gateway::sealed 同构，交叉验证两端实现）。
    fn spawn_fake_gateway() -> (String, RsaPublicKey) {
        let keys = PlatformKeys::generate();
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let priv_pem = keys.priv_pkcs8_pem();
        let pub_pem = keys.public_pem();
        std::thread::spawn(move || {
            use showroom_common::srtp::unwrap_key;
            let (mut c, _) = l.accept().unwrap();
            let mut hdr = [0u8; HDR_LEN];
            c.read_exact(&mut hdr).unwrap();
            assert_eq!(&hdr[..4], b"SRTP");
            assert_eq!(hdr[5], 0); // 上行 flags=0（带 wrap）
            // 真机网关同构：字节级取 wrap → TReader 流式解密 → 8B 帧界取请求
            let mut raw = Vec::new();
            let mut buf = [0u8; 65536];
            while raw.len() < WRAP_LEN {
                let n = c.read(&mut buf).expect("wrap 到齐前断开");
                raw.extend_from_slice(&buf[..n]);
            }
            let k = unwrap_key(&priv_pem, &raw[..WRAP_LEN]).unwrap();
            let mut rd = TReader::new(k);
            let mut req_plain = Vec::new();
            let mut rest = raw[WRAP_LEN..].to_vec();
            let req = loop {
                if rest.is_empty() {
                    let n = c.read(&mut buf).expect("请求未完即断开");
                    rest.extend_from_slice(&buf[..n]);
                }
                let chunk = std::mem::take(&mut rest);
                rd.push(&chunk, &mut req_plain).unwrap();
                if req_plain.len() >= 8 {
                    let n = u64::from_be_bytes(req_plain[..8].try_into().unwrap()) as usize;
                    if req_plain.len() >= 8 + n {
                        break req_plain[8..8 + n].to_vec();
                    }
                }
            };
            let resp_plain = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Echo: ok\r\n\r\n",
                req.len()
            )
            .into_bytes()
            .into_iter()
            .chain(req)
            .collect::<Vec<u8>>();
            let mut out = header(true).to_vec();
            let mut tw = TWriter::new(k, Box::new(rand::rngs::OsRng));
            tw.record(&resp_plain, &mut out).unwrap();
            c.write_all(&out).unwrap();
            let _ = c.shutdown(std::net::Shutdown::Write);
        });
        (format!("http://127.0.0.1:{port}"), RsaPublicKey::from_public_key_pem(&pub_pem).unwrap())
    }

    #[test]
    fn loopback_roundtrip_and_sticky_downgrade() {
        let (base, pubk) = spawn_fake_gateway();
        let tier = AtomicU8::new(0);
        let resp = request_with_tier(
            &tier, &base, "GET", "/_api/health", &[("Authorization", "SR-SIG1 u")], b"", &pubk,
        )
        .expect("一级可用");
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("GET /_api/health"));
        assert_eq!(tier.load(std::sync::atomic::Ordering::Relaxed), 1);

        // 非 SRTP 应答 → None + 粘性降级
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 64];
            let _ = c.read(&mut b);
            let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        });
        let tier2 = AtomicU8::new(0);
        let r = request_with_tier(&tier2, &format!("http://127.0.0.1:{port}"), "GET", "/x", &[], b"", &pubk);
        assert!(r.is_none());
        assert_eq!(tier2.load(std::sync::atomic::Ordering::Relaxed), 2, "对端不支持须粘性降级");
    }
}
