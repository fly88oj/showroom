//! Run explicitly with FRPS_BIN pointing to an official frps executable:
//! cargo test --test interop -- --ignored --nocapture
use anyhow::{Context, Result, ensure};
use frpc_rust::{
    config::{Config, Proxy, ProxyTransport, Visitor},
    service::Client,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinSet,
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start_server(binary: &Path, config: &Path, log: &Path) -> Result<Server> {
    let file = std::fs::File::create(log)?;
    let mut command = Command::new(binary);
    command
        .arg("-c")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    Ok(Server(command.spawn().context("无法运行 FRPS_BIN")?))
}
async fn port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port())
}
async fn echo(mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin) -> Result<()> {
    let data: Vec<u8> = (0..100000).map(|i| (i % 251) as u8).collect();
    stream.write_all(&data).await?;
    stream.flush().await?;
    let mut output = vec![0; data.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut output)).await??;
    ensure!(data == output, "TCP 转发内容不一致");
    stream.shutdown().await?;
    Ok(())
}
async fn udp_echo(port: u16) -> Result<()> {
    let s = UdpSocket::bind("127.0.0.1:0").await?;
    s.send_to(b"frp-udp", ("127.0.0.1", port)).await?;
    let mut b = [0; 128];
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), s.recv_from(&mut b)).await??;
    ensure!(&b[..n] == b"frp-udp", "UDP 转发内容不一致");
    Ok(())
}

#[tokio::test]
#[ignore = "需要显式提供官方 frps 可执行文件，见 README"]
async fn official_frps_all_proxy_types_and_transports() -> Result<()> {
    let binary = PathBuf::from(
        std::env::var("FRPS_BIN").context("请设置 FRPS_BIN 为官方 frps 可执行文件绝对路径")?,
    );
    ensure!(binary.is_file(), "FRPS_BIN 文件不存在");
    let dir = tempfile::tempdir()?;
    let bind = port().await?;
    let http_port = port().await?;
    let https_port = port().await?;
    let mux_port = port().await?;
    let quic_port = port().await?;
    let server_config = dir.path().join("frps.toml");
    let log = dir.path().join("frps.log");
    std::fs::write(
        &server_config,
        format!(
            r#"bindAddr = "127.0.0.1"
bindPort = {bind}
quicBindPort = {quic_port}
vhostHTTPPort = {http_port}
vhostHTTPSPort = {https_port}
tcpmuxHTTPConnectPort = {mux_port}
auth.token = "interop-only"
auth.additionalScopes = ["HeartBeats", "NewWorkConns"]
log.level = "warn"
"#
        ),
    )?;
    let mut server = start_server(&binary, &server_config, &log)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if TcpStream::connect(("127.0.0.1", bind)).await.is_ok() {
                return Ok::<_, anyhow::Error>(());
            }
            if let Some(exit) = server.0.try_wait()? {
                anyhow::bail!(
                    "frps 启动失败 {exit}: {}",
                    std::fs::read_to_string(&log).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    let mut backends = JoinSet::new();
    let tcp = TcpListener::bind("127.0.0.1:0").await?;
    let local_tcp = tcp.local_addr()?.port();
    backends.spawn(async move {
        let mut jobs = JoinSet::new();
        while let Ok((mut s, _)) = tcp.accept().await {
            jobs.spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
            while jobs.try_join_next().is_some() {}
        }
    });
    let udp = UdpSocket::bind("127.0.0.1:0").await?;
    let local_udp = udp.local_addr()?.port();
    backends.spawn(async move {
        let mut b = [0; 1500];
        while let Ok((n, addr)) = udp.recv_from(&mut b).await {
            let _ = udp.send_to(&b[..n], addr).await;
        }
    });
    let http = TcpListener::bind("127.0.0.1:0").await?;
    let local_http = http.local_addr()?.port();
    backends.spawn(async move {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                format!(
                    "{}:{}",
                    headers["host"].to_str().unwrap(),
                    headers["x-test"].to_str().unwrap()
                )
            }),
        );
        let _ = axum::serve(http, app).await;
    });
    let generated = rcgen::generate_simple_self_signed(vec!["secure.example".into()])?;
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![generated.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der()).into(),
    )?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let tls = TcpListener::bind("127.0.0.1:0").await?;
    let local_tls = tls.local_addr()?.port();
    backends.spawn(async move {
        let mut jobs = JoinSet::new();
        while let Ok((s, _)) = tls.accept().await {
            let acceptor = acceptor.clone();
            jobs.spawn(async move {
                if let Ok(mut s) = acceptor.accept(s).await {
                    let (mut r, mut w) = tokio::io::split(&mut s);
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                }
            });
            while jobs.try_join_next().is_some() {}
        }
    });
    for protocol in ["tcp", "websocket", "wss", "quic"] {
        let mut cfg = Config {
            server_port: if protocol == "quic" { quic_port } else { bind },
            user: format!("rust-{protocol}"),
            login_fail_exit: false,
            ..Default::default()
        };
        cfg.auth.token = "interop-only".into();
        cfg.auth.additional_scopes = vec!["HeartBeats".into(), "NewWorkConns".into()];
        cfg.transport.protocol = protocol.into();
        cfg.transport.heartbeat_interval = 1;
        cfg.transport.heartbeat_timeout = 5;
        let mut tcp_ports = vec![];
        for (encryption, compression) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let remote = port().await?;
            tcp_ports.push(remote);
            cfg.proxies.push(Proxy {
                name: format!("tcp-{encryption}-{compression}"),
                local_port: local_tcp,
                remote_port: remote,
                transport: ProxyTransport {
                    use_encryption: encryption,
                    use_compression: compression,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        let udp_port = port().await?;
        let stcp_port = port().await?;
        let sudp_port = port().await?;
        for (name, kind, local, remote) in [
            ("udp", "udp", local_udp, udp_port),
            ("stcp", "stcp", local_tcp, 0),
            ("sudp", "sudp", local_udp, 0),
            ("http", "http", local_http, 0),
            ("https", "https", local_tls, 0),
            ("tcpmux", "tcpmux", local_tcp, 0),
        ] {
            let mut p = Proxy {
                name: name.into(),
                r#type: kind.into(),
                local_port: local,
                remote_port: remote,
                secret_key: "visitor-only".into(),
                transport: ProxyTransport {
                    use_encryption: true,
                    use_compression: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            if matches!(kind, "http" | "https" | "tcpmux") {
                p.custom_domains = vec![format!("{name}.example")];
            }
            if kind == "http" {
                p.host_header_rewrite = "backend.example".into();
                p.request_headers
                    .set
                    .insert("x-test".into(), "official".into());
                p.response_headers
                    .set
                    .insert("x-response".into(), "works".into());
            }
            if kind == "tcpmux" {
                p.multiplexer = "httpconnect".into();
            }
            cfg.proxies.push(p);
        }
        for (kind, bind_port) in [("stcp", stcp_port), ("sudp", sudp_port)] {
            cfg.visitors.push(Visitor {
                name: format!("{kind}-visitor"),
                r#type: kind.into(),
                server_name: kind.into(),
                secret_key: "visitor-only".into(),
                bind_port,
                transport: frpc_rust::config::VisitorTransport {
                    use_encryption: true,
                    use_compression: true,
                },
                ..Default::default()
            });
        }
        cfg.validate()?;
        let expected = cfg.proxies.len();
        let client = Client::new(cfg, "unused".into());
        let shared = client.shared.clone();
        let mut clients = JoinSet::new();
        clients.spawn(client.run());
        let ready = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status = shared.status.read().await;
                if status.connected
                    && status.proxies.len() == expected
                    && status.proxies.values().all(|p| p.status == "running")
                {
                    break;
                }
                drop(status);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if ready.is_err() {
            anyhow::bail!(
                "{protocol} 注册失败: {:?}; frps 日志: {}",
                *shared.status.read().await,
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        for port in tcp_ports {
            echo(TcpStream::connect(("127.0.0.1", port)).await?).await?;
        }
        echo(TcpStream::connect(("127.0.0.1", stcp_port)).await?).await?;
        udp_echo(udp_port).await?;
        udp_echo(sudp_port).await?;
        let http = reqwest::Client::builder().no_proxy().build()?;
        let response = http
            .get(format!("http://127.0.0.1:{http_port}/"))
            .header("host", "http.example")
            .send()
            .await?;
        ensure!(
            response.headers()["x-response"] == "works",
            "HTTP 响应头未透传"
        );
        ensure!(
            response.text().await? == "backend.example:official",
            "HTTP Host/请求头未改写"
        );
        let tls = frpc_rust::transport::tls_config(&frpc_rust::config::Tls::default())?;
        let s = TcpStream::connect(("127.0.0.1", https_port)).await?;
        let s = tokio_rustls::TlsConnector::from(Arc::new(tls))
            .connect(rustls::pki_types::ServerName::try_from("https.example")?, s)
            .await?;
        echo(s).await?;
        let mut s = TcpStream::connect(("127.0.0.1", mux_port)).await?;
        s.write_all(b"CONNECT tcpmux.example HTTP/1.1\r\nHost: tcpmux.example\r\n\r\n")
            .await?;
        let header = frpc_rust::transport::read_http_header(&mut s).await?;
        ensure!(
            header.starts_with("HTTP/1.1 200"),
            "TCPMUX CONNECT 失败: {header}"
        );
        echo(s).await?;
        shared.shutdown.cancel();
        while let Some(result) = clients.join_next().await {
            result??;
        }
        println!("通过：{protocol} / 7 类代理 / 4 种加密压缩组合");
    }
    backends.shutdown().await;
    drop(server);
    Ok(())
}
