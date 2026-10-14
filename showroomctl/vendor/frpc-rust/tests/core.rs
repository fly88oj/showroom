use anyhow::Result;
use frpc_rust::{
    auth::auth_key,
    config::{Config, ProxyTransport},
    proxy::proxy_header,
    stream, wire,
};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn auth_matches_independent_md5_vector() {
    // printf 'secret1700000000' | openssl dgst -md5
    assert_eq!(
        auth_key("secret", 1700000000),
        "e37729af62f40f134311cffbc230178e"
    );
}
#[test]
fn official_style_toml_and_duplicate_names() {
    let raw = r#"serverAddr = "localhost"
serverPort = 7000
auth.token = "example"
[[proxies]]
name = "ssh"
type = "tcp"
localPort = 22
remotePort = 6000
transport.useEncryption = true
[[visitors]]
name = "private"
type = "stcp"
serverName = "ssh-private"
secretKey = "example"
bindPort = 6001
"#;
    let mut c: Config = toml::from_str(raw).unwrap();
    c.validate().unwrap();
    assert!(c.transport.tls.enable && c.transport.tcp_mux);
    assert_eq!(c.proxies[0].local_ip, "127.0.0.1");
    c.visitors[0].name = "ssh".into();
    assert!(c.validate().is_err());
}
#[test]
fn unknown_and_unsupported_configuration_fails_closed() {
    assert!(toml::from_str::<Config>("serverPrt=7000").is_err());
    assert!(toml::from_str::<Config>("transport.tls.enbale=true").is_err());
    let mut c = Config::default();
    c.transport.protocol = "kcp".into();
    assert!(c.validate().is_err());
    c = Config::default();
    c.transport.heartbeat_timeout = 1;
    assert!(c.validate().is_err());
    c = Config::default();
    c.web_server.port = 7400;
    c.web_server.addr = "0.0.0.0".into();
    assert!(c.validate().is_err());
    c = Config::default();
    c.udp_packet_size = 65535;
    assert!(c.validate().is_err());
}
#[test]
fn includes_resolve_relative_to_configuration_and_catch_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("parts")).unwrap();
    std::fs::write(
        dir.path().join("frpc.toml"),
        "includes = [\"parts/*.toml\"]",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("parts/ssh.toml"),
        "[[proxies]]\nname=\"ssh\"\nlocalPort=22\n",
    )
    .unwrap();
    assert_eq!(
        Config::load(&dir.path().join("frpc.toml"))
            .unwrap()
            .proxies
            .len(),
        1
    );
    std::fs::write(
        dir.path().join("parts/duplicate.toml"),
        "[[proxies]]\nname=\"ssh\"\nlocalPort=23\n",
    )
    .unwrap();
    assert!(Config::load(&dir.path().join("frpc.toml")).is_err());
}
#[test]
fn missing_environment_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("frpc.toml");
    std::fs::write(
        &file,
        "serverAddr=\"{{ .Envs.FRPC_RUST_MISSING_TEST_VARIABLE_934729 }}\"",
    )
    .unwrap();
    assert!(Config::load(&file).is_err());
}
#[test]
fn formats_and_source_paths() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        (
            "a.json",
            r#"{"auth":{"tokenSource":{"type":"file","file":{"path":"token.txt"}}}}"#,
        ),
        (
            "a.yaml",
            "auth:\n  tokenSource:\n    type: file\n    file:\n      path: token.txt\n",
        ),
    ] {
        let file = dir.path().join(name);
        std::fs::write(&file, text).unwrap();
        std::fs::write(dir.path().join("token.txt"), " example\n").unwrap();
        let c = Config::load(&file).unwrap();
        assert!(Path::new(&c.auth.token_source.as_ref().unwrap().file.path).is_absolute());
        assert_eq!(c.auth.token_source.unwrap().resolve().unwrap(), "example");
    }
}
#[test]
fn bandwidth_is_validated() {
    let mut t = ProxyTransport {
        bandwidth_limit: "1.5MB".into(),
        ..Default::default()
    };
    assert_eq!(t.bandwidth_bytes().unwrap(), 1572864);
    for bad in ["NaNMB", "-2KB", "0KB", "2mb", "1e99MB"] {
        t.bandwidth_limit = bad.into();
        assert!(t.bandwidth_bytes().is_err());
    }
}
#[test]
fn proxy_protocol_ipv4_and_ipv6() {
    let a = "192.0.2.10:1234".parse().unwrap();
    let b = "198.51.100.1:443".parse().unwrap();
    assert_eq!(
        proxy_header(a, b, "v1", false).unwrap(),
        b"PROXY TCP4 192.0.2.10 198.51.100.1 1234 443\r\n"
    );
    let v2 = proxy_header(a, b, "v2", true).unwrap();
    assert_eq!(&v2[..12], b"\r\n\r\n\0\r\nQUIT\n");
    assert_eq!(&v2[12..16], &[0x21, 0x12, 0, 12]);
    assert_eq!(&v2[16..], &[192, 0, 2, 10, 198, 51, 100, 1, 4, 210, 1, 187]);
    assert_eq!(
        proxy_header(
            "[::1]:80".parse().unwrap(),
            "[::2]:443".parse().unwrap(),
            "v2",
            false
        )
        .unwrap()
        .len(),
        52
    );
}
#[tokio::test]
async fn framing_matches_golden_and_handles_fragmentation() -> Result<()> {
    let (mut a, mut b) = tokio::io::duplex(128);
    wire::write(&mut a, b'h', &json!({})).await?;
    let mut golden = [0; 11];
    b.read_exact(&mut golden).await?;
    assert_eq!(&golden, b"h\0\0\0\0\0\0\0\x02{}");
    let writer = tokio::spawn(async move {
        for byte in b"4\0\0\0\0\0\0\0\x02{}" {
            a.write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    assert_eq!(wire::expect(&mut b, b'4').await?.kind, b'4');
    writer.await?;
    Ok(())
}
#[tokio::test]
async fn rejects_lengths_before_allocating_and_truncation() {
    for length in [-1i64, 10241, i64::MAX] {
        let (mut a, mut b) = tokio::io::duplex(32);
        a.write_all(b"o").await.unwrap();
        a.write_all(&length.to_be_bytes()).await.unwrap();
        assert!(wire::read(&mut b).await.is_err());
    }
    let mut truncated = &b"1\0\0\0\0\0\0\0\x08{}"[..];
    assert!(wire::read(&mut truncated).await.is_err());
    let mut bad_json = &b"1\0\0\0\0\0\0\0\x02[]"[..];
    assert!(wire::read(&mut bad_json).await.is_err());
}
#[tokio::test]
async fn compression_encryption_full_duplex_and_half_close() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (encrypt, compress) in [(true, false), (false, true), (true, true)] {
            let (a, b) = tokio::io::duplex(1024);
            let a = stream::wrap(Box::new(a), b"integration", encrypt, compress, None);
            let b = stream::wrap(Box::new(b), b"integration", encrypt, compress, None);
            let (mut ar, mut aw) = tokio::io::split(a);
            let (mut br, mut bw) = tokio::io::split(b);
            let data: Vec<u8> = (0..300000).map(|i| (i % 251) as u8).collect();
            let expected = data.clone();
            let sender = tokio::spawn(async move {
                for bytes in data.chunks(17) {
                    aw.write_all(bytes).await?;
                }
                aw.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            });
            let echo = tokio::spawn(async move {
                let mut data = vec![];
                br.read_to_end(&mut data).await?;
                bw.write_all(&data).await?;
                bw.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            });
            let mut out = vec![];
            ar.read_to_end(&mut out).await?;
            assert_eq!(
                out.len(),
                expected.len(),
                "encrypt={encrypt} compress={compress}"
            );
            assert!(out == expected, "payload mismatch");
            sender.await??;
            echo.await??;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
#[tokio::test]
async fn adapter_drop_closes_underlying_connection() -> Result<()> {
    let (a, mut b) = tokio::io::duplex(128);
    let a = stream::wrap(Box::new(a), b"test", true, true, None);
    drop(a);
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), b.read(&mut byte)).await??,
        0
    );
    Ok(())
}
#[tokio::test]
async fn corrupt_snappy_frame_is_rejected() -> Result<()> {
    let (a, mut b) = tokio::io::duplex(128);
    let mut a = stream::compression(Box::new(a));
    b.write_all(b"\xff\x06\0\0sNaPpY\x01\x05\0\0\0\0\0\0x")
        .await?;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), a.read(&mut byte)).await??,
        0
    );
    Ok(())
}

#[tokio::test]
async fn decrypts_node_crypto_golden_vector() -> Result<()> {
    // Independently generated using Node crypto.pbkdf2Sync('test-only', 'frp',
    // 64, 16, 'sha1') and crypto.createCipheriv('aes-128-cfb', key, iv).
    let hex = "000102030405060708090a0b0c0d0e0f43fcd4c286113ecec21adb3629e5a79b163586ad2c5cd983390d46e733";
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let (a, mut b) = tokio::io::duplex(128);
    b.write_all(&bytes).await?;
    b.shutdown().await?;
    let mut a = stream::encryption(Box::new(a), b"test-only");
    let mut clear = String::new();
    a.read_to_string(&mut clear).await?;
    assert_eq!(clear, "independent frp crypto vector");
    Ok(())
}
