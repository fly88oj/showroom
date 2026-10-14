use anyhow::{Context, Result};
use axum::{Router, routing::get};
use frpc_rust::{
    auth::Authenticator,
    config::{Auth, Config, Plugin},
    service::Client,
    stream::BoxStream,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn oidc_client_credentials_scopes_and_cache() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = Router::new().route(
        "/token",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(headers["authorization"], "Basic Y2xpZW50OnNlY3JldA==");
                assert!(
                    body.contains("grant_type=client_credentials")
                        && body.contains("audience=frp")
                        && body.contains("scope=tunnel")
                );
                axum::Json(json!({"access_token":"issued-token","expires_in":3600}))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut auth = Auth {
        method: "oidc".into(),
        additional_scopes: vec!["HeartBeats".into()],
        ..Default::default()
    };
    auth.oidc.client_id = "client".into();
    auth.oidc.client_secret = "secret".into();
    auth.oidc.audience = "frp".into();
    auth.oidc.scope = "tunnel".into();
    auth.oidc.token_endpoint_url = format!("http://{address}/token");
    let authenticator = Authenticator::new(&auth)?;
    assert!(authenticator.encryption_key().is_empty());
    assert_eq!(
        authenticator.fields("Login").await?["privilege_key"],
        "issued-token"
    );
    assert_eq!(
        authenticator.fields("HeartBeats").await?["privilege_key"],
        "issued-token"
    );
    assert_eq!(authenticator.fields("NewWorkConns").await?, json!({}));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    server.abort();
    Ok(())
}
#[tokio::test]
async fn admin_auth_status_metrics_and_invalid_reload() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("frpc.toml");
    std::fs::write(&path, "serverPort = \"invalid\"")?;
    let mut cfg = Config::default();
    cfg.web_server.user = "admin".into();
    cfg.web_server.password = "test-only".into();
    let client = Client::new(cfg, path);
    let state = client.shared.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = state.shutdown.clone();
    let server = tokio::spawn(frpc_rust::admin::serve(listener, state));
    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{address}");
    assert_eq!(
        http.get(format!("{url}/api/status")).send().await?.status(),
        401
    );
    let get = |path: &str| {
        http.get(format!("{url}{path}"))
            .basic_auth("admin", Some("test-only"))
    };
    let status: serde_json::Value = get("/api/status").send().await?.json().await?;
    assert_eq!(status["connected"], false);
    assert_eq!(get("/healthz").send().await?.status(), 503);
    assert!(
        get("/metrics")
            .send()
            .await?
            .text()
            .await?
            .contains("frpc_connected 0")
    );
    assert_eq!(get("/api/reload").send().await?.status(), 405);
    assert_eq!(
        http.post(format!("{url}/api/reload"))
            .basic_auth("admin", Some("test-only"))
            .header("Origin", "https://untrusted.example")
            .send()
            .await?
            .status(),
        403
    );
    let response = http
        .post(format!("{url}/api/reload"))
        .basic_auth("admin", Some("test-only"))
        .send()
        .await?;
    assert_eq!(response.status(), 400);
    stop.cancel();
    server.await??;
    Ok(())
}
async fn plugin_connection(plugin: Plugin) -> (BoxStream, tokio::task::JoinHandle<Result<()>>) {
    let (a, b) = tokio::io::duplex(65536);
    let task =
        tokio::spawn(async move { frpc_rust::plugin::handle(Box::new(b), &plugin, None).await });
    (Box::new(a), task)
}
async fn request(mut stream: BoxStream, data: &str) -> Result<String> {
    stream.write_all(data.as_bytes()).await?;
    stream.flush().await?;
    let mut result = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut result)).await??;
    Ok(result)
}
#[tokio::test]
async fn static_file_auth_range_and_traversal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join("public"))?;
    std::fs::write(dir.path().join("public/hello.txt"), "hello-world")?;
    std::fs::write(dir.path().join("outside.txt"), "private")?;
    let plugin = Plugin {
        r#type: "static_file".into(),
        local_path: dir.path().join("public").to_string_lossy().into(),
        http_user: "user".into(),
        http_password: "pass".into(),
        ..Default::default()
    };
    let (s, t) = plugin_connection(plugin.clone()).await;
    assert!(
        request(
            s,
            "GET /hello.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .await?
        .starts_with("HTTP/1.1 401")
    );
    t.await??;
    let (s, t) = plugin_connection(plugin.clone()).await;
    let r=request(s,"GET /hello.txt HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic dXNlcjpwYXNz\r\nRange: bytes=0-4\r\nConnection: close\r\n\r\n").await?;
    assert!(r.starts_with("HTTP/1.1 206") && r.ends_with("hello"));
    t.await??;
    let (s, t) = plugin_connection(plugin).await;
    let r=request(s,"GET /%2e%2e/outside.txt HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic dXNlcjpwYXNz\r\nConnection: close\r\n\r\n").await?;
    assert!(!r.contains("private"));
    assert!(!r.starts_with("HTTP/1.1 200"));
    t.await??;
    Ok(())
}
#[tokio::test]
async fn socks5_authentication_connect_and_rejection() -> Result<()> {
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let port = backend.local_addr()?.port();
    let echo = tokio::spawn(async move {
        let (mut s, _) = backend.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        tokio::io::copy(&mut r, &mut w).await.unwrap();
    });
    let p = Plugin {
        r#type: "socks5".into(),
        username: "user".into(),
        password: "pass".into(),
        ..Default::default()
    };
    let (mut s, t) = plugin_connection(p.clone()).await;
    s.write_all(&[5, 1, 2]).await?;
    let mut response = [0; 2];
    s.read_exact(&mut response).await?;
    assert_eq!(response, [5, 2]);
    s.write_all(b"\x01\x04user\x04pass").await?;
    s.read_exact(&mut response).await?;
    assert_eq!(response, [1, 0]);
    let mut connect = vec![5, 1, 0, 1, 127, 0, 0, 1];
    connect.extend(port.to_be_bytes());
    s.write_all(&connect).await?;
    let mut reply = [0; 10];
    s.read_exact(&mut reply).await?;
    assert_eq!(reply[1], 0);
    s.write_all(b"socks-test").await?;
    let mut echo_data = [0; 10];
    s.read_exact(&mut echo_data).await?;
    assert_eq!(&echo_data, b"socks-test");
    s.shutdown().await?;
    drop(s);
    t.await??;
    echo.await?;
    let (mut s, t) = plugin_connection(p).await;
    s.write_all(&[5, 1, 0]).await?;
    s.read_exact(&mut response).await?;
    assert_eq!(response, [5, 255]);
    assert!(t.await?.is_err());
    Ok(())
}
#[tokio::test]
async fn http_proxy_and_http_bridge_stream_requests() -> Result<()> {
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let address = backend.local_addr()?;
    let app = Router::new().route(
        "/echo",
        get(|headers: axum::http::HeaderMap| async move {
            format!(
                "{}:{}",
                headers.get("host").unwrap().to_str().unwrap(),
                headers
                    .get("x-test")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("none")
            )
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(backend, app).await.unwrap();
    });
    let plugin = Plugin {
        r#type: "http_proxy".into(),
        http_user: "user".into(),
        http_password: "pass".into(),
        ..Default::default()
    };
    let (s, t) = plugin_connection(plugin).await;
    let r=request(s,&format!("GET http://{address}/echo HTTP/1.1\r\nHost: {address}\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\nConnection: close\r\n\r\n")).await?;
    assert!(r.starts_with("HTTP/1.1 200") && r.contains(&format!("{address}:none")));
    t.await??;
    let mut plugin = Plugin {
        r#type: "http2http".into(),
        local_addr: address.to_string(),
        host_header_rewrite: "internal.example".into(),
        ..Default::default()
    };
    plugin
        .request_headers
        .set
        .insert("x-test".into(), "bridge".into());
    let (s, t) = plugin_connection(plugin).await;
    let r = request(
        s,
        "GET /echo HTTP/1.1\r\nHost: public.example\r\nConnection: close\r\n\r\n",
    )
    .await?;
    assert!(r.contains("internal.example:bridge"));
    t.await??;
    server.abort();
    Ok(())
}
#[tokio::test]
async fn http_connect_tunnel_survives_upgrade_and_closes() -> Result<()> {
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let address = backend.local_addr()?;
    let echo = tokio::spawn(async move {
        let (mut s, _) = backend.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        tokio::io::copy(&mut r, &mut w).await.unwrap();
    });
    let (mut s, t) = plugin_connection(Plugin {
        r#type: "http_proxy".into(),
        ..Default::default()
    })
    .await;
    s.write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await?;
    let header = frpc_rust::transport::read_http_header(&mut s).await?;
    assert!(header.starts_with("HTTP/1.1 200"));
    s.write_all(b"tunnel-data").await?;
    let mut b = [0; 11];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut b)).await??;
    assert_eq!(&b, b"tunnel-data");
    s.shutdown().await?;
    drop(s);
    tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .context("tunnel cleanup")???;
    echo.await?;
    Ok(())
}

#[tokio::test]
async fn https_bridge_accepts_http2_and_tls2raw_preserves_proxy_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, generated.cert.pem())?;
    std::fs::write(&key, generated.key_pair.serialize_pem())?;
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let address = backend.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(
            backend,
            Router::new().route("/", get(|| async { "bridge-ok" })),
        )
        .await
        .unwrap();
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let plugin = Plugin {
        r#type: "https2http".into(),
        local_addr: address.to_string(),
        crt_path: cert.to_string_lossy().into(),
        key_path: key.to_string_lossy().into(),
        ..Default::default()
    };
    let bridge = tokio::spawn(async move {
        let (s, _) = listener.accept().await?;
        frpc_rust::plugin::handle(Box::new(s), &plugin, None).await
    });
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()?;
    let response = http
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .await?;
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.text().await?, "bridge-ok");
    drop(http);
    bridge.abort();
    server.abort();
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let address = backend.local_addr()?;
    let raw = tokio::spawn(async move {
        let (mut s, _) = backend.accept().await.unwrap();
        let mut b = [0; 15];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"PROXY UNKNOWN\r\n");
        s.write_all(b"raw-ok").await.unwrap();
    });
    let p = Plugin {
        r#type: "tls2raw".into(),
        local_addr: address.to_string(),
        crt_path: cert.to_string_lossy().into(),
        key_path: key.to_string_lossy().into(),
        ..Default::default()
    };
    let (a, b) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        frpc_rust::plugin::handle(Box::new(b), &p, Some(b"PROXY UNKNOWN\r\n".to_vec())).await
    });
    let tls = frpc_rust::transport::tls_config(&frpc_rust::config::Tls::default())?;
    let mut s = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(rustls::pki_types::ServerName::try_from("localhost")?, a)
        .await?;
    let mut out = [0; 6];
    s.read_exact(&mut out).await?;
    assert_eq!(&out, b"raw-ok");
    drop(s);
    let _ = server.await;
    raw.await?;
    Ok(())
}
