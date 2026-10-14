use crate::{
    config::{Config, Tls},
    stream::{BoxStream, TaskStream},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt, future::poll_fn};
use rustls::{
    ClientConfig, RootCertStore,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{
    io::{self, BufReader},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpSocket, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::{
    compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt},
    sync::CancellationToken,
};

#[derive(Debug)]
struct FrpCertificateVerifier;
impl ServerCertVerifier for FrpCertificateVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub fn tls_config(tls: &Tls) -> Result<ClientConfig> {
    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?;
    // Upstream frps generates a self-signed certificate by default. Supplying
    // trustedCaFile switches to normal certificate and server-name validation.
    let builder = if tls.trusted_ca_file.is_empty() {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(FrpCertificateVerifier))
    } else {
        let mut roots = RootCertStore::empty();
        for cert in certificates(&tls.trusted_ca_file)? {
            roots.add(cert)?;
        }
        ensure!(!roots.is_empty(), "trustedCaFile 没有有效证书");
        builder.with_root_certificates(roots)
    };
    Ok(if tls.cert_file.is_empty() {
        builder.with_no_client_auth()
    } else {
        builder.with_client_auth_cert(certificates(&tls.cert_file)?, private_key(&tls.key_file)?)?
    })
}
pub fn certificates(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let file = std::fs::File::open(path).with_context(|| format!("读取证书 {path}"))?;
    Ok(rustls_pemfile::certs(&mut BufReader::new(file)).collect::<io::Result<_>>()?)
}
pub fn private_key(path: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut BufReader::new(
        std::fs::File::open(path).context("读取 TLS 私钥失败")?,
    ))?
    .context("未找到 TLS 私钥")
}

enum Kind {
    Direct,
    Mux(mpsc::Sender<oneshot::Sender<Result<BoxStream>>>),
    Quic(quinn::Connection, quinn::Endpoint),
}
pub struct Connector {
    cfg: Arc<Config>,
    kind: Kind,
    task: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}
impl Drop for Connector {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(t) = &self.task {
            t.abort();
        }
        if let Kind::Quic(c, e) = &self.kind {
            c.close(0u32.into(), b"client closed");
            e.close(0u32.into(), b"client closed");
        }
    }
}
impl Connector {
    pub async fn open(cfg: Arc<Config>) -> Result<Arc<Self>> {
        let cancel = CancellationToken::new();
        if cfg.transport.protocol == "quic" {
            let mut tls = tls_config(&cfg.transport.tls)?;
            tls.alpn_protocols = vec![b"frp".to_vec()];
            let mut qc = quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
            ));
            let mut transport = quinn::TransportConfig::default();
            transport.max_idle_timeout(Some(
                Duration::from_secs(cfg.transport.quic.max_idle_timeout).try_into()?,
            ));
            transport.keep_alive_interval(Some(Duration::from_secs(
                cfg.transport.quic.keepalive_period,
            )));
            transport.max_concurrent_bidi_streams(cfg.transport.quic.max_incoming_streams.into());
            qc.transport_config(Arc::new(transport));
            let remote = tokio::net::lookup_host((cfg.server_addr.as_str(), cfg.server_port))
                .await?
                .next()
                .context("无法解析 frps 地址")?;
            let bind = if cfg.transport.connect_server_local_ip.is_empty() {
                if remote.is_ipv4() {
                    "0.0.0.0:0".parse()?
                } else {
                    "[::]:0".parse()?
                }
            } else {
                std::net::SocketAddr::new(cfg.transport.connect_server_local_ip.parse()?, 0)
            };
            let mut endpoint = quinn::Endpoint::client(bind)?;
            endpoint.set_default_client_config(qc);
            let name = if cfg.transport.tls.server_name.is_empty() {
                &cfg.server_addr
            } else {
                &cfg.transport.tls.server_name
            };
            let conn = tokio::time::timeout(
                Duration::from_secs(cfg.transport.dial_server_timeout),
                endpoint.connect(remote, name)?,
            )
            .await
            .context("QUIC 握手超时")??;
            return Ok(Arc::new(Self {
                cfg,
                kind: Kind::Quic(conn, endpoint),
                task: None,
                cancel,
            }));
        }
        if !cfg.transport.tcp_mux {
            return Ok(Arc::new(Self {
                cfg,
                kind: Kind::Direct,
                task: None,
                cancel,
            }));
        }
        let socket = real_connect(&cfg).await?;
        let mut yc = yamux::Config::default();
        yc.set_max_num_streams(4096);
        let mut connection = yamux::Connection::new(socket.compat(), yc, yamux::Mode::Client);
        let (tx, mut rx) = mpsc::channel::<oneshot::Sender<Result<BoxStream>>>(128);
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            let mut pending = None;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    request = rx.recv(), if pending.is_none() => match request { Some(r)=>pending=Some(r), None=>break },
                    result = poll_fn(|cx| {
                        // Always poll inbound to drive yamux I/O, including while
                        // an outbound stream is waiting on flow-control credits.
                        match connection.poll_next_inbound(cx) {
                            Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(anyhow::anyhow!(e))),
                            Poll::Ready(None) => return Poll::Ready(Err(anyhow::anyhow!("yamux 会话关闭"))),
                            Poll::Ready(Some(Ok(stream))) => { drop(stream); cx.waker().wake_by_ref(); },
                            Poll::Pending => {},
                        }
                        if pending.is_some() { connection.poll_new_outbound(cx).map(|r| r.map(|s| Box::new(s.compat()) as BoxStream).map_err(Into::into)) } else { Poll::Pending }
                    }) => {
                        let failed = result.is_err();
                        if let Some(reply)=pending.take() { let _=reply.send(result); }
                        if failed { break; }
                    }
                }
            }
        });
        Ok(Arc::new(Self {
            cfg,
            kind: Kind::Mux(tx),
            task: Some(task),
            cancel,
        }))
    }
    pub async fn connect(&self) -> Result<BoxStream> {
        tokio::time::timeout(
            Duration::from_secs(self.cfg.transport.dial_server_timeout),
            async {
                match &self.kind {
                    Kind::Direct => real_connect(&self.cfg).await,
                    Kind::Mux(tx) => {
                        let (reply, rx) = oneshot::channel();
                        tx.send(reply).await.context("yamux 会话已关闭")?;
                        rx.await.context("yamux 打开流失败")?
                    }
                    Kind::Quic(conn, _) => {
                        let (send, recv) = conn.open_bi().await?;
                        Ok(Box::new(QuicStream { send, recv }) as BoxStream)
                    }
                }
            },
        )
        .await
        .context("连接 frps 超时")?
    }
}
struct QuicStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}
impl AsyncRead for QuicStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, b)
    }
}
impl AsyncWrite for QuicStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        b: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

pub async fn tcp_connect(host: &str, port: u16, local: &str, keepalive: u64) -> Result<TcpStream> {
    let addresses = tokio::net::lookup_host((host, port)).await?;
    let mut last = None;
    for addr in addresses {
        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        if !local.is_empty() {
            socket.bind(std::net::SocketAddr::new(local.parse()?, 0))?;
        }
        match socket.connect(addr).await {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                if keepalive > 0 {
                    socket2::SockRef::from(&stream).set_tcp_keepalive(
                        &socket2::TcpKeepalive::new()
                            .with_time(Duration::from_secs(keepalive))
                            .with_interval(Duration::from_secs(keepalive)),
                    )?;
                }
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("地址解析无结果")))
}
async fn real_connect(cfg: &Config) -> Result<BoxStream> {
    tokio::time::timeout(
        Duration::from_secs(cfg.transport.dial_server_timeout),
        async {
            let mut stream: BoxStream = if cfg.transport.proxy_url.is_empty() {
                Box::new(
                    tcp_connect(
                        &cfg.server_addr,
                        cfg.server_port,
                        &cfg.transport.connect_server_local_ip,
                        cfg.transport.dial_server_keepalive,
                    )
                    .await?,
                )
            } else {
                via_proxy(cfg).await?
            };
            let protocol = cfg.transport.protocol.as_str();
            if protocol == "websocket" {
                stream = websocket(stream, cfg, false).await?;
            }
            if cfg.transport.tls.enable || protocol == "wss" {
                if protocol != "wss" && !cfg.transport.tls.disable_custom_tls_first_byte {
                    stream.write_all(&[0x17]).await?;
                }
                let name = if cfg.transport.tls.server_name.is_empty() {
                    cfg.server_addr.clone()
                } else {
                    cfg.transport.tls.server_name.clone()
                };
                let tls =
                    tokio_rustls::TlsConnector::from(Arc::new(tls_config(&cfg.transport.tls)?));
                stream = Box::new(
                    tls.connect(ServerName::try_from(name)?, stream)
                        .await
                        .map_err(|error| tls_handshake_error(error, cfg))?,
                );
            }
            if protocol == "wss" {
                stream = websocket(stream, cfg, true).await?;
            }
            Ok(stream)
        },
    )
    .await
    .context("连接或握手超时")?
}
fn tls_handshake_error(error: io::Error, cfg: &Config) -> anyhow::Error {
    let invalid_content_type = matches!(
        error
            .get_ref()
            .and_then(|e| e.downcast_ref::<rustls::Error>()),
        Some(rustls::Error::InvalidMessage(
            rustls::InvalidMessage::InvalidContentType
        ))
    );
    let target = format_host(&cfg.server_addr, cfg.server_port);
    let mut context = format!(
        "TLS 握手失败：目标 {target}，transport.protocol={}",
        cfg.transport.protocol
    );
    if invalid_content_type {
        context.push_str("；对端返回了非 TLS 格式的数据（例如明文 HTTP 响应）。请核对 serverPort 是否指向 frps.bindPort，以及反向代理的转发协议");
        if cfg.transport.protocol == "wss" {
            context.push_str("；WSS 入口必须提供 TLS，并将 /~!frp 转发到 frps 的 WebSocket 入口");
        } else {
            context.push_str("；旧版 frps 或 bindPort 与 vhostHTTPSPort 复用时，请核对 transport.tls.disableCustomTLSFirstByte=false");
        }
    }
    anyhow::Error::new(error).context(context)
}
async fn websocket(stream: BoxStream, cfg: &Config, secure: bool) -> Result<BoxStream> {
    let scheme = if secure { "wss" } else { "ws" };
    let authority = format_host(&cfg.server_addr, cfg.server_port);
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "{scheme}://{authority}/~!frp"
        ))?;
    request
        .headers_mut()
        .insert("Origin", format!("http://{authority}").parse()?);
    let (ws, _) = tokio_tungstenite::client_async(request, stream).await?;
    let (mut tx, mut rx) = ws.split();
    let (public, internal) = tokio::io::duplex(65536);
    let (mut r, mut w) = tokio::io::split(internal);
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                tokio_tungstenite::tungstenite::Message::Binary(b) => {
                    if w.write_all(&b).await.is_err() {
                        break;
                    }
                }
                tokio_tungstenite::tungstenite::Message::Text(b) => {
                    if w.write_all(b.as_bytes()).await.is_err() {
                        break;
                    }
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = w.shutdown().await;
    });
    let writer = tokio::spawn(async move {
        let mut buf = [0; 16384];
        loop {
            let n = match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if tx
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    buf[..n].to_vec().into(),
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
pub fn format_host(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
async fn via_proxy(cfg: &Config) -> Result<BoxStream> {
    let u = url::Url::parse(&cfg.transport.proxy_url)?;
    let mut s: BoxStream = Box::new(
        tcp_connect(
            u.host_str().context("proxyURL 缺少主机")?,
            u.port()
                .unwrap_or(if u.scheme() == "http" { 80 } else { 1080 }),
            &cfg.transport.connect_server_local_ip,
            cfg.transport.dial_server_keepalive,
        )
        .await?,
    );
    let user = percent_decode(u.username())?;
    let pass = percent_decode(u.password().unwrap_or(""))?;
    match u.scheme() {
        "http" => {
            let target = format_host(&cfg.server_addr, cfg.server_port);
            let auth = if user.is_empty() {
                String::new()
            } else {
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    STANDARD.encode(format!("{user}:{pass}"))
                )
            };
            s.write_all(
                format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n").as_bytes(),
            )
            .await?;
            let header = read_http_header(&mut s).await?;
            ensure!(
                header
                    .lines()
                    .next()
                    .and_then(|s| s.split_whitespace().nth(1))
                    == Some("200"),
                "HTTP 代理拒绝 CONNECT"
            );
        }
        "socks5" => {
            let method = if user.is_empty() { 0 } else { 2 };
            s.write_all(&[5, 1, method]).await?;
            let mut reply = [0; 2];
            s.read_exact(&mut reply).await?;
            ensure!(reply == [5, method], "SOCKS5 认证方法不匹配");
            if method == 2 {
                ensure!(user.len() <= 255 && pass.len() <= 255, "SOCKS5 凭据过长");
                let mut msg = vec![1, user.len() as u8];
                msg.extend(user.as_bytes());
                msg.push(pass.len() as u8);
                msg.extend(pass.as_bytes());
                s.write_all(&msg).await?;
                s.read_exact(&mut reply).await?;
                ensure!(reply == [1, 0], "SOCKS5 认证失败");
            }
            ensure!(cfg.server_addr.len() <= 255, "SOCKS5 目标域名过长");
            let mut msg = vec![5, 1, 0, 3, cfg.server_addr.len() as u8];
            msg.extend(cfg.server_addr.as_bytes());
            msg.extend(cfg.server_port.to_be_bytes());
            s.write_all(&msg).await?;
            let mut header = [0; 4];
            s.read_exact(&mut header).await?;
            ensure!(header[0] == 5 && header[1] == 0, "SOCKS5 连接失败");
            let n = match header[3] {
                1 => 4,
                4 => 16,
                3 => s.read_u8().await? as usize,
                _ => bail!("SOCKS5 地址无效"),
            };
            let mut discard = vec![0; n + 2];
            s.read_exact(&mut discard).await?;
        }
        _ => bail!("不支持的 proxyURL 协议"),
    }
    Ok(s)
}
pub async fn read_http_header<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<String> {
    let mut buf = Vec::new();
    loop {
        ensure!(buf.len() < 32768, "HTTP 头超过 32KiB");
        buf.push(r.read_u8().await?);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8(buf)?);
        }
    }
}
pub fn percent_decode(input: &str) -> Result<String> {
    let b = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            ensure!(i + 2 < b.len(), "URL 转义无效");
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).context("URL 转义无效")?;
            out.push(u8::from_str_radix(hex, 16)?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}
