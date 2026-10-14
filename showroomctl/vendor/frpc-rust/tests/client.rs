//! Local protocol-peer tests. These are not a substitute for the optional
//! interop test against an independently built official frps binary.
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use frpc_rust::{
    auth::auth_key,
    config::{Config, Proxy, ProxyTransport, Visitor},
    service::{Client, Command, Shared},
    stream::{self, BoxStream, TaskStream},
    wire::{self, Message, UdpPacket},
};
use futures::{SinkExt, StreamExt, future::poll_fn};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{broadcast, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::{
    compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt},
    sync::CancellationToken,
};

const TOKEN: &str = "test-only-token";
const SECRET: &str = "test-only-visitor";
struct Incoming {
    message: Message,
    stream: BoxStream,
}
struct Peer {
    port: u16,
    certificate: String,
    cancel: CancellationToken,
    commands: broadcast::Sender<(u8, Value)>,
    incoming: mpsc::Receiver<Incoming>,
    events: mpsc::Receiver<Message>,
    logins: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}
impl Peer {
    async fn start(protocol: &str, mux: bool, tls: bool) -> Result<Self> {
        Self::start_with_tls_prefix(protocol, mux, tls, false).await
    }
    async fn start_with_tls_prefix(
        protocol: &str,
        mux: bool,
        tls: bool,
        custom_prefix: bool,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let generated =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
        let certificate = generated.cert.pem();
        let tls_cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![generated.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der()).into(),
        )?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_cfg));
        let (commands, _) = broadcast::channel(128);
        let (incoming_tx, incoming) = mpsc::channel(128);
        let (events_tx, events) = mpsc::channel(128);
        let logins = Arc::new(AtomicUsize::new(0));
        let cmd = commands.clone();
        let count = logins.clone();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let protocol = protocol.to_owned();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    _=stop.cancelled()=>break,
                    accepted=listener.accept()=>{
                        let Ok((socket,_))=accepted else{break;};let acceptor=acceptor.clone();let commands=cmd.clone();let incoming=incoming_tx.clone();let events=events_tx.clone();let count=count.clone();let protocol=protocol.clone();
                        tasks.spawn(async move{
                            let result:Result<()>=async{
                                let mut socket:BoxStream=Box::new(socket);
                                if protocol=="wss"{socket=Box::new(acceptor.accept(socket).await?);}
                                if protocol=="websocket"||protocol=="wss"{socket=accept_ws(socket).await?;}
                                if tls&&protocol!="wss"{
                                    if custom_prefix {anyhow::ensure!(socket.read_u8().await?==0x17,"missing frp TLS prefix");}
                                    socket=Box::new(acceptor.accept(socket).await?);
                                }
                                if mux {
                                    let mut connection=yamux::Connection::new(socket.compat(),yamux::Config::default(),yamux::Mode::Server);let mut streams=JoinSet::new();
                                    while let Some(stream)=poll_fn(|cx|connection.poll_next_inbound(cx)).await {let stream=stream?;let commands=commands.subscribe();let incoming=incoming.clone();let events=events.clone();let count=count.clone();streams.spawn(async move{let _=serve_stream(Box::new(stream.compat()),commands,incoming,events,count).await;});while streams.try_join_next().is_some(){}}
                                } else {serve_stream(socket,commands.subscribe(),incoming,events,count).await?;}
                                Ok(())
                            }.await;
                            if let Err(e)=result {eprintln!("test peer closed: {e}");}
                        });
                    },
                    Some(_)=tasks.join_next()=>{},
                }
            }
            tasks.shutdown().await;
        });
        Ok(Self {
            port,
            certificate,
            cancel,
            commands,
            incoming,
            events,
            logins,
            task,
        })
    }
    fn config(&self, protocol: &str, mux: bool, tls: bool) -> Config {
        let mut cfg = Config {
            server_port: self.port,
            ..Default::default()
        };
        cfg.auth.token = TOKEN.into();
        cfg.auth.additional_scopes = vec!["HeartBeats".into(), "NewWorkConns".into()];
        cfg.transport.protocol = protocol.into();
        cfg.transport.tcp_mux = mux;
        cfg.transport.tls.enable = tls;
        cfg.transport.heartbeat_interval = 1;
        cfg.transport.heartbeat_timeout = 4;
        cfg.login_fail_exit = false;
        cfg
    }
    async fn work(&mut self, name: &str) -> Result<BoxStream> {
        self.commands.send((b'r', json!({})))?;
        let mut incoming = tokio::time::timeout(Duration::from_secs(5), self.incoming.recv())
            .await?
            .context("missing work stream")?;
        assert_eq!(incoming.message.kind, b'w');
        assert_eq!(incoming.message.text("run_id"), "test-run");
        let timestamp = incoming.message.body["timestamp"].as_i64().unwrap();
        assert_eq!(
            incoming.message.text("privilege_key"),
            auth_key(TOKEN, timestamp)
        );
        wire::write(&mut incoming.stream,b's',&json!({"proxy_name":name,"src_addr":"192.0.2.8","src_port":1234,"dst_addr":"127.0.0.1","dst_port":6000})).await?;
        Ok(incoming.stream)
    }
}
async fn serve_stream(
    mut stream: BoxStream,
    mut commands: broadcast::Receiver<(u8, Value)>,
    incoming: mpsc::Sender<Incoming>,
    events: mpsc::Sender<Message>,
    logins: Arc<AtomicUsize>,
) -> Result<()> {
    let message = wire::read(&mut stream).await?;
    if message.kind == b'o' {
        let timestamp = message.body["timestamp"]
            .as_i64()
            .context("login timestamp")?;
        if message.text("privilege_key") != auth_key(TOKEN, timestamp) {
            wire::write(&mut stream, b'1', &json!({"error":"authentication failed"})).await?;
            return Ok(());
        }
        logins.fetch_add(1, Ordering::Relaxed);
        wire::write(
            &mut stream,
            b'1',
            &json!({"version":"0.70.0","run_id":"test-run"}),
        )
        .await?;
        let stream = stream::encryption(stream, TOKEN.as_bytes());
        let (mut r, mut w) = tokio::io::split(stream);
        let mut tasks = JoinSet::new();
        let (tx, mut rx) = mpsc::channel(128);
        tasks.spawn(async move {
            loop {
                let m = wire::read(&mut r).await;
                let end = m.is_err();
                if tx.send(m).await.is_err() || end {
                    break;
                }
            }
        });
        loop {
            tokio::select! {
                raw=rx.recv()=>{let message=raw.context("closed")??;match message.kind{
                    b'p'=>{wire::write(&mut w,b'2',&json!({"proxy_name":message.text("proxy_name"),"remote_addr":"127.0.0.1:6000"})).await?;},
                    b'h'=>{let timestamp=message.body["timestamp"].as_i64().context("ping timestamp")?;assert_eq!(message.text("privilege_key"),auth_key(TOKEN,timestamp));wire::write(&mut w,b'4',&json!({})).await?;},_=>{}}
                    events.send(message).await?;
                },
                Ok((kind,body))=commands.recv()=>{if kind==0{break;}wire::write(&mut w,kind,&body).await?;},
            }
        }
    } else if message.kind == b'v' {
        let timestamp = message.body["timestamp"]
            .as_i64()
            .context("visitor timestamp")?;
        assert_eq!(message.text("sign_key"), auth_key(SECRET, timestamp));
        wire::write(&mut stream, b'3', &json!({})).await?;
        stream = stream::wrap(
            stream,
            SECRET.as_bytes(),
            message.body["use_encryption"].as_bool().unwrap_or(false),
            message.body["use_compression"].as_bool().unwrap_or(false),
            None,
        );
        incoming
            .send(Incoming { message, stream })
            .await
            .map_err(|_| anyhow::anyhow!("incoming channel closed"))?;
    } else {
        incoming
            .send(Incoming { message, stream })
            .await
            .map_err(|_| anyhow::anyhow!("incoming channel closed"))?;
    }
    Ok(())
}
async fn accept_ws(stream: BoxStream) -> Result<BoxStream> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut tx, mut rx) = ws.split();
    let (public, internal) = tokio::io::duplex(65536);
    let (mut r, mut w) = tokio::io::split(internal);
    let reader = tokio::spawn(async move {
        while let Some(Ok(m)) = rx.next().await {
            if m.is_binary() && w.write_all(&m.into_data()).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });
    let writer = tokio::spawn(async move {
        let mut b = [0; 16384];
        while let Ok(n) = r.read(&mut b).await {
            if n == 0 {
                break;
            }
            if tx
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    b[..n].to_vec().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = tx.close().await;
    });
    Ok(Box::new(TaskStream::new(public, vec![reader, writer])))
}
struct Running {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<Result<()>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.shared.shutdown.cancel();
        self.task.abort();
    }
}
impl Running {
    fn start(cfg: Config) -> Self {
        cfg.validate().unwrap();
        let client = Client::new(cfg, "unused.toml".into());
        let shared = client.shared.clone();
        let task = tokio::spawn(client.run());
        Self { shared, task }
    }
    async fn ready(&self, name: &str) -> Result<()> {
        wait_for(|| async {
            self.shared
                .status
                .read()
                .await
                .proxies
                .get(name)
                .is_some_and(|p| p.status == "running")
        })
        .await
    }
}
async fn wait_for<F, Fu>(mut predicate: F) -> Result<()>
where
    F: FnMut() -> Fu,
    Fu: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(8), async {
        while !predicate().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("condition timeout")
}
async fn echo_backend() -> Result<(u16, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        let mut jobs = JoinSet::new();
        while let Ok((mut s, _)) = listener.accept().await {
            jobs.spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    Ok((port, task))
}
async fn echo_roundtrip(mut s: BoxStream, data: &[u8]) -> Result<()> {
    s.write_all(data).await?;
    s.flush().await?;
    let mut out = vec![0; data.len()];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut out)).await??;
    assert!(out == data, "echo payload mismatch");
    s.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn tcp_transport_matrix_and_concurrent_work_connections() -> Result<()> {
    let (port, backend) = echo_backend().await?;
    for (protocol, mux, tls) in [
        ("tcp", false, false),
        ("tcp", true, false),
        ("tcp", true, true),
        ("websocket", true, false),
        ("websocket", true, true),
        ("wss", true, true),
    ] {
        let mut peer = Peer::start(protocol, mux, tls).await?;
        let mut cfg = peer.config(protocol, mux, tls);
        cfg.user = "tester".into();
        cfg.proxies.push(Proxy {
            name: "echo".into(),
            local_port: port,
            transport: ProxyTransport {
                use_encryption: true,
                use_compression: true,
                ..Default::default()
            },
            ..Default::default()
        });
        let client = Running::start(cfg);
        client
            .ready("tester.echo")
            .await
            .with_context(|| format!("{protocol} mux={mux} tls={tls}"))?;
        let mut jobs = JoinSet::new();
        for i in 0..8 {
            let stream = peer.work("tester.echo").await?;
            let stream = stream::wrap(stream, TOKEN.as_bytes(), true, true, None);
            jobs.spawn(async move { echo_roundtrip(stream, &vec![i; 100_000]).await });
        }
        while let Some(r) = jobs.join_next().await {
            r??;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(client.shared.status.read().await.connected);
        client.shared.shutdown.cancel();
    }
    backend.abort();
    Ok(())
}
#[tokio::test]
async fn trusted_ca_validation_accepts_valid_and_rejects_wrong_hostname() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let peer = Peer::start("tcp", true, true).await?;
    let cert = dir.path().join("ca.pem");
    std::fs::write(&cert, &peer.certificate)?;
    let mut cfg = peer.config("tcp", true, true);
    cfg.transport.tls.trusted_ca_file = cert.to_string_lossy().into();
    cfg.transport.tls.server_name = "localhost".into();
    let client = Running::start(cfg.clone());
    wait_for(|| async { client.shared.status.read().await.connected }).await?;
    client.shared.shutdown.cancel();
    cfg.transport.tls.server_name = "invalid.example".into();
    cfg.login_fail_exit = true;
    let client = Client::new(cfg, "unused".into());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), client.run())
            .await?
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn custom_tls_prefix_precedes_tls_inside_the_correct_transport() -> Result<()> {
    let (port, backend) = echo_backend().await?;
    for (protocol, mux) in [
        ("tcp", false),
        ("tcp", true),
        ("websocket", false),
        ("websocket", true),
        ("wss", true),
    ] {
        let mut peer = Peer::start_with_tls_prefix(protocol, mux, true, protocol != "wss").await?;
        let mut cfg = peer.config(protocol, mux, true);
        cfg.transport.tls.disable_custom_tls_first_byte = false;
        cfg.proxies.push(Proxy {
            name: "custom-tls".into(),
            local_port: port,
            ..Default::default()
        });
        let client = Running::start(cfg);
        client
            .ready("custom-tls")
            .await
            .with_context(|| format!("custom prefix: {protocol}, mux={mux}"))?;
        echo_roundtrip(peer.work("custom-tls").await?, b"custom-tls-data").await?;
        client.shared.shutdown.cancel();
    }
    backend.abort();
    Ok(())
}

#[tokio::test]
async fn non_tls_endpoint_explains_invalid_content_type_without_downgrading() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut header = [0; 5];
        socket.read_exact(&mut header).await?;
        assert_eq!(header[0], 0x16, "client must send TLS ClientHello");
        let mut hello = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
        socket.read_exact(&mut hello).await?;
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        let mut byte = [0];
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut byte)).await;
        // A TLS failure must not cause an implicit plaintext retry.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        Ok::<_, anyhow::Error>(())
    });
    let mut cfg = Config {
        server_port: port,
        ..Default::default()
    };
    cfg.transport.tcp_mux = false;
    let client = Client::new(cfg, "unused".into());
    let error = tokio::time::timeout(Duration::from_secs(5), client.run())
        .await?
        .expect_err("plaintext endpoint must fail TLS");
    let error = format!("{error:#}");
    assert!(error.contains("InvalidContentType"), "{error}");
    assert!(
        error.contains(&format!("127.0.0.1:{port}")),
        "missing target: {error}"
    );
    assert!(
        error.contains("frps.bindPort") && error.contains("disableCustomTLSFirstByte"),
        "missing configuration hint: {error}"
    );
    server.await??;
    Ok(())
}
#[tokio::test]
async fn reconnect_and_reload_preserve_session_on_rejected_change() -> Result<()> {
    let (port, backend) = echo_backend().await?;
    let peer = Peer::start("tcp", true, true).await?;
    let mut cfg = peer.config("tcp", true, true);
    cfg.proxies.push(Proxy {
        name: "echo".into(),
        local_port: port,
        ..Default::default()
    });
    let client = Running::start(cfg.clone());
    client.ready("echo").await?;
    let mut invalid = cfg.clone();
    invalid.web_server.port = 12345;
    let (tx, rx) = oneshot::channel();
    client
        .shared
        .commands
        .send(Command::Reload(Arc::new(invalid), tx))
        .await?;
    assert!(rx.await?.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(peer.logins.load(Ordering::Relaxed), 1);
    cfg.proxies[0].name = "new-echo".into();
    let (tx, rx) = oneshot::channel();
    client
        .shared
        .commands
        .send(Command::Reload(Arc::new(cfg), tx))
        .await?;
    rx.await??;
    client.ready("new-echo").await?;
    assert!(
        !client
            .shared
            .status
            .read()
            .await
            .proxies
            .contains_key("echo")
    );
    let logins = peer.logins.load(Ordering::Relaxed);
    peer.commands.send((0, json!({})))?;
    wait_for(|| async {
        peer.logins.load(Ordering::Relaxed) > logins && client.shared.status.read().await.connected
    })
    .await?;
    assert!(client.shared.status.read().await.reconnects >= 1);
    backend.abort();
    Ok(())
}
#[tokio::test]
async fn bad_login_exits_and_udp_peers_are_isolated() -> Result<()> {
    let mut peer = Peer::start("tcp", false, false).await?;
    let mut cfg = peer.config("tcp", false, false);
    cfg.auth.token = "wrong".into();
    cfg.login_fail_exit = true;
    let client = Client::new(cfg, "unused".into());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), client.run())
            .await?
            .is_err()
    );
    let udp = UdpSocket::bind("127.0.0.1:0").await?;
    let port = udp.local_addr()?.port();
    let backend = tokio::spawn(async move {
        let mut b = [0; 1500];
        while let Ok((n, addr)) = udp.recv_from(&mut b).await {
            let _ = udp.send_to(&b[..n], addr).await;
        }
    });
    let mut cfg = peer.config("tcp", false, false);
    cfg.proxies.push(Proxy {
        name: "dns".into(),
        r#type: "udp".into(),
        local_port: port,
        transport: ProxyTransport {
            use_encryption: true,
            use_compression: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let client = Running::start(cfg);
    client.ready("dns").await?;
    let stream = peer.work("dns").await?;
    let mut stream = stream::wrap(stream, TOKEN.as_bytes(), true, true, None);
    for i in 1..=3 {
        wire::write(
            &mut stream,
            b'u',
            &UdpPacket {
                content: STANDARD.encode(format!("packet-{i}")),
                local: None,
                remote: format!("192.0.2.{i}:1234")
                    .parse::<std::net::SocketAddr>()?
                    .into(),
            },
        )
        .await?;
    }
    let mut replies = std::collections::BTreeSet::new();
    while replies.len() < 3 {
        let m = tokio::time::timeout(Duration::from_secs(5), wire::read(&mut stream)).await??;
        if m.kind != b'u' {
            continue;
        }
        let p: UdpPacket = serde_json::from_value(m.body)?;
        replies.insert((p.remote.ip, String::from_utf8(STANDARD.decode(p.content)?)?));
    }
    assert_eq!(replies.len(), 3);
    assert!(replies.contains(&("192.0.2.1".into(), "packet-1".into())));
    backend.abort();
    Ok(())
}
async fn unused_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port())
}
#[tokio::test]
async fn stcp_and_sudp_visitors_sign_and_forward() -> Result<()> {
    let mut peer = Peer::start("tcp", true, true).await?;
    for kind in ["stcp", "sudp"] {
        let port = unused_port().await?;
        let mut cfg = peer.config("tcp", true, true);
        cfg.user = "client".into();
        cfg.visitors.push(Visitor {
            name: kind.into(),
            r#type: kind.into(),
            server_name: "private".into(),
            server_user: "owner".into(),
            secret_key: SECRET.into(),
            bind_port: port,
            transport: frpc_rust::config::VisitorTransport {
                use_encryption: true,
                use_compression: true,
            },
            ..Default::default()
        });
        let client = Running::start(cfg);
        wait_for(|| async {
            client
                .shared
                .status
                .read()
                .await
                .visitors
                .contains_key(kind)
        })
        .await?;
        if kind == "stcp" {
            let mut local = TcpStream::connect(("127.0.0.1", port)).await?;
            local.write_all(b"visitor-data").await?;
            let mut incoming = tokio::time::timeout(Duration::from_secs(5), peer.incoming.recv())
                .await?
                .unwrap();
            assert_eq!(incoming.message.text("proxy_name"), "owner.private");
            let mut b = [0; 12];
            incoming.stream.read_exact(&mut b).await?;
            incoming.stream.write_all(&b).await?;
            incoming.stream.flush().await?;
            let mut echo = [0; 12];
            local.read_exact(&mut echo).await?;
            assert_eq!(echo, b);
        } else {
            let local = UdpSocket::bind("127.0.0.1:0").await?;
            local.send_to(b"udp-visitor", ("127.0.0.1", port)).await?;
            let mut incoming = tokio::time::timeout(Duration::from_secs(5), peer.incoming.recv())
                .await?
                .unwrap();
            let m = wire::expect(&mut incoming.stream, b'u').await?;
            wire::write(&mut incoming.stream, b'u', &m.body).await?;
            let mut b = [0; 32];
            let (n, _) =
                tokio::time::timeout(Duration::from_secs(5), local.recv_from(&mut b)).await??;
            assert_eq!(&b[..n], b"udp-visitor");
        }
        client.shared.shutdown.cancel();
    }
    Ok(())
}
#[tokio::test]
async fn health_check_unregisters_and_recovers() -> Result<()> {
    let mut peer = Peer::start("tcp", true, false).await?;
    let port = unused_port().await?;
    let mut cfg = peer.config("tcp", true, false);
    let mut p = Proxy {
        name: "healthy".into(),
        local_port: port,
        ..Default::default()
    };
    p.health_check.r#type = "tcp".into();
    p.health_check.interval_seconds = 1;
    cfg.proxies.push(p);
    let client = Running::start(cfg);
    wait_for(|| async {
        client
            .shared
            .status
            .read()
            .await
            .proxies
            .get("healthy")
            .is_some_and(|p| p.status == "unhealthy")
    })
    .await?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    client.ready("healthy").await?;
    task.abort();
    wait_for(|| async {
        client
            .shared
            .status
            .read()
            .await
            .proxies
            .get("healthy")
            .is_some_and(|p| p.status == "unhealthy")
    })
    .await?;
    let mut closed = false;
    while let Ok(event) = peer.events.try_recv() {
        if event.kind == b'c' {
            closed = true;
        }
    }
    assert!(closed);
    Ok(())
}
