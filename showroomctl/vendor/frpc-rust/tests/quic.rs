use anyhow::{Context, Result};
use frpc_rust::{config::Config, service::Client, stream::BoxStream, wire};
use serde_json::json;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct Stream(quinn::SendStream, quinn::RecvStream);
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.1).poll_read(cx, b)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        b: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.0), cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.0), cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.0), cx)
    }
}
#[tokio::test]
async fn quic_alpn_login_control_and_shutdown() -> Result<()> {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![generated.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der()).into(),
    )?;
    tls.alpn_protocols = vec![b"frp".to_vec()];
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
    ));
    let endpoint = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse()?)?;
    let port = endpoint.local_addr()?.port();
    let server = tokio::spawn(async move {
        let conn = endpoint
            .accept()
            .await
            .context("missing QUIC client")?
            .await?;
        let (send, recv) = conn.accept_bi().await?;
        let mut stream: BoxStream = Box::new(Stream(send, recv));
        let login = wire::expect(&mut stream, b'o').await?;
        assert_eq!(login.text("version"), wire::VERSION);
        wire::write(&mut stream, b'1', &json!({"run_id":"quic-run"})).await?;
        let mut stream = frpc_rust::stream::encryption(stream, b"");
        let ping = wire::expect(&mut stream, b'h').await?;
        assert_eq!(ping.body, json!({}));
        wire::write(&mut stream, b'4', &json!({})).await?;
        conn.closed().await;
        Ok::<_, anyhow::Error>(())
    });
    let mut cfg = Config {
        server_port: port,
        ..Default::default()
    };
    cfg.transport.protocol = "quic".into();
    cfg.transport.heartbeat_interval = 1;
    cfg.transport.heartbeat_timeout = 4;
    let client = Client::new(cfg, "unused".into());
    let shared = client.shared.clone();
    let task = tokio::spawn(client.run());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if shared.status.read().await.connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    shared.shutdown.cancel();
    task.await??;
    tokio::time::timeout(Duration::from_secs(5), server).await???;
    Ok(())
}
