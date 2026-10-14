//! tunnel 集成测试：网关回环（明文直通 / 密封全双工）+ resolve 配置改写 + host.env 契约。

use std::sync::Arc;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::SeedableRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::Oaep;
use sha2::Sha256;

use showroom_common::srtp::{header, SessionRng, HDR_LEN};
use showroom_common::{PlatformKeys, KEY_LEN};

use super::{resolve, Source, GATEWAY_PORT};
use crate::middleware::{FrpMode, HostEnv};

/// 回环上游（返回实际端口）：echo 服务——读什么回什么，模拟 Caddy site B 的 HTTP 往返形状。
async fn echo_port() -> u16 {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = match l.accept().await {
                Ok(c) => c,
                Err(_) => return,
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 16384];
                loop {
                    match c.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if c.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

/// 网关实例（127.0.0.1:0 → echo 上游），返回网关端口。
async fn gateway_on(upstream_port: u16, keys: Option<Arc<PlatformKeys>>) -> u16 {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(super::gateway::serve(l, upstream_port, keys));
    port
}

#[tokio::test]
async fn plaintext_passthrough_is_byte_exact() {
    // Docker Push / 匿名公开访问路径：非 SRTP 前缀的字节必须原样直达上游、响应原样返回
    let up = echo_port().await;
    let gw = gateway_on(up, None).await;
    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", gw)).await.unwrap();

    let req = b"POST /_api/import-begin HTTP/1.1\r\nHost: 1.2.3.4:24000\r\nContent-Length: 11\r\n\r\nhello=world".to_vec();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    c.write_all(&req).await.unwrap();
    let mut buf = vec![0u8; req.len()];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf, req, "明文直通须逐字节回显（含 HTTP 头）");

    // 长流（模拟镜像 import 分块）：64KB 二进制双向
    let blob: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
    c.write_all(&blob).await.unwrap();
    let mut back = vec![0u8; blob.len()];
    tokio::time::timeout(Duration::from_secs(10), c.read_exact(&mut back))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back, blob);
}

#[tokio::test]
async fn srtp_sealed_full_duplex_roundtrip() {
    // 密封通道：上行 wrap+段 → 网关解封转上游；下行以同一 k 对称封装 → 客户端解出
    let keys = PlatformKeys::generate();
    let pub_pem = keys.public_pem();
    let up = echo_port().await;
    let gw = gateway_on(up, Some(Arc::new(keys))).await;

    // 客户端构造（镜像真实 wasm 侧形状：RSA-OAEP 包裹 k + 段流）
    let k = [0x5au8; KEY_LEN];
    let pubk = rsa::RsaPublicKey::from_public_key_pem(&pub_pem).unwrap();
    let mut rng = StdRng::from_seed([0xAA; 32]);
    let wrapped = pubk
        .encrypt(&mut rng, Oaep::new::<Sha256>(), &k)
        .unwrap();

    // SRTP v1 上行 = wrap + 传输记录层；记录内层 = [8B 大端长度][载荷] 应用帧
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 241) as u8).collect();
    let mut framed = (payload.len() as u64).to_be_bytes().to_vec();
    framed.extend_from_slice(&payload);
    let mut wire: Vec<u8> = header(false).to_vec();
    wire.extend_from_slice(&wrapped);
    let mut tw = showroom_common::srtp::TWriter::new(k, Box::new(rng) as Box<dyn SessionRng + Send>);
    for piece in framed.chunks(64 * 1024) {
        tw.record(piece, &mut wire).unwrap(); // 多条记录（分块），验证流式解密
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", gw)).await.unwrap();
    c.write_all(&wire).await.unwrap();
    // 半关闭写侧：上行 EOF 触发网关 flush 尾段
    c.shutdown().await.unwrap();

    // 读下行：6B 头（flags=1）+ 会话密钥段流
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), c.read_to_end(&mut resp))
        .await
        .expect("读下行超时：密封通道未在 10s 内收尾")
        .expect("读下行出错");
    assert!(resp.len() > HDR_LEN, "下行须含头+段: {}", resp.len());
    assert_eq!(&resp[..HDR_LEN], &header(true)[..]);
    let mut rd = showroom_common::srtp::TReader::new(k);
    let mut plain = Vec::new();
    rd.push(&resp[HDR_LEN..], &mut plain).unwrap();
    rd.finish().unwrap();
    assert_eq!(plain, payload, "下行解封须还原上行载荷（echo）");
}

#[tokio::test]
async fn srtp_without_platform_key_is_refused() {
    // 平台私钥缺失（keys=None）时 SRTP 连接被拒——明文直通不受影响（另有用例）
    let up = echo_port().await;
    let gw = gateway_on(up, None).await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", gw)).await.unwrap();
    c.write_all(&header(false)).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "网关应直接断开（无数据回显）");
}

/// 临时 TOML 文件（native 路径契约），返回路径。
fn toml_file(name: &str, contents: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("sr-tunnel-toml-{name}-{}-{n}.toml", std::process::id()));
    std::fs::write(&p, contents).unwrap();
    p
}

#[tokio::test]
async fn resolve_toml_rewrites_locals_to_gateway() {
    let toml_cfg = r#"
serverAddr = "10.1.2.3"
serverPort = 7000
auth.token = "tok-native"

[[proxies]]
name = "web"
type = "tcp"
localIP = "192.168.1.9"
localPort = 9999
remotePort = 24000

[[proxies]]
name = "game"
type = "udp"
localPort = 7777
remotePort = 25000
"#;
    let path = toml_file("mixed", toml_cfg);
    let (cfg, skipped) = resolve(&Source::Toml(path.clone())).await.unwrap();
    assert_eq!(skipped, vec!["game"]);
    assert_eq!(cfg.proxies.len(), 1);
    let p = &cfg.proxies[0];
    assert_eq!(p.local_ip, "127.0.0.1");
    assert_eq!(p.local_port, GATEWAY_PORT, "local 恒改指网关");
    assert_eq!(p.remote_port, 24000, "remote 原样保留");
    assert_eq!(cfg.server_addr, "10.1.2.3");
    let _ = std::fs::remove_file(&path);

    // 空 tcp 代理集 → E_NAME
    let only_udp = toml_file(
        "udp",
        "serverAddr = \"x\"\nserverPort = 1\nauth.token = \"t\"\n[[proxies]]\nname = \"u\"\ntype = \"udp\"\nlocalPort = 1\nremotePort = 2\n",
    );
    let e = resolve(&Source::Toml(only_udp.clone())).await.unwrap_err();
    assert_eq!(e.code, "E_NAME");
    let _ = std::fs::remove_file(&only_udp);

    // 文件缺失 → E_NAME 指名路径（resolve 失败面统一有 E_* 出口）
    let e2 = resolve(&Source::Toml(std::path::PathBuf::from("/nonexistent/frpc.toml"))).await.unwrap_err();
    assert_eq!(e2.code, "E_NAME");
    assert!(e2.msg.contains("FRP_TOML") && e2.msg.contains("/nonexistent"), "{}", e2.msg);
}

#[tokio::test]
async fn resolve_bad_toml_names_the_problem() {
    let path = toml_file("bad", "serverPrt=7000");
    let e = resolve(&Source::Toml(path.clone())).await.unwrap_err();
    assert_eq!(e.code, "E_NAME");
    assert!(e.msg.contains("FRP_TOML"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn host_env_native_contract() {
    // 路径契约：FRP_TOML=指向 0600 TOML 文件的单行路径（内联多行 TOML 在 parse_kv 逐行
    // 切分下存活不了首次落盘往返——评审实锤，契约收敛为路径）
    let h = HostEnv::parse(
        "FRP=native\nFRP_TOML=/etc/showroom/frpc-native.toml\nMIDDLEWARE=postgres\nCADDY=no\nTUNING=none\nDEDUP=none\n",
    )
    .unwrap();
    assert_eq!(h.frp, FrpMode::Native);
    assert_eq!(h.frp_toml_path, "/etc/showroom/frpc-native.toml");
    assert!(h.to_env().contains("FRP_TOML=/etc/showroom/frpc-native.toml"));
    assert_eq!(HostEnv::parse(&h.to_env()).unwrap(), h); // 单行路径落盘往返存活

    // native 无路径 → E_NAME
    let e = HostEnv::parse("FRP=native\n").unwrap_err();
    assert_eq!(e.code, "E_NAME");

    // FRP_TUNNEL2 折叠：单一 FRP_TUNNEL 逗号键存活（setup --apply 重写不丢第二隧道）
    let h2 = HostEnv::parse("FRP=chmlfrp\nFRP_TOKEN=t\nFRP_TUNNEL=111\nFRP_TUNNEL2=222\n").unwrap();
    assert_eq!(h2.frp_tunnel, "111,222");
    assert!(h2.to_env().contains("FRP_TUNNEL=111,222"));
    assert!(!h2.to_env().contains("FRP_TUNNEL2"), "历史键折叠后不再写出");
    // 重复折叠幂等
    let h3 = HostEnv::parse("FRP=chmlfrp\nFRP_TOKEN=t\nFRP_TUNNEL=111,222\nFRP_TUNNEL2=222\n").unwrap();
    assert_eq!(h3.frp_tunnel, "111,222");
}
