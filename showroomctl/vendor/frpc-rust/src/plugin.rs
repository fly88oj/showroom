use crate::{
    config::Plugin,
    stream::BoxStream,
    transport::{self, tcp_connect},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{convert::Infallible, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

pub async fn handle(
    mut stream: BoxStream,
    plugin: &Plugin,
    proxy_header: Option<Vec<u8>>,
) -> Result<()> {
    match plugin.r#type.as_str() {
        "socks5" => socks5(stream, plugin).await,
        "http_proxy" => http_proxy(stream, plugin).await,
        "static_file" => static_file(stream, plugin.clone()).await,
        "http2http" | "http2https" | "https2http" | "https2https" => {
            http_bridge(stream, plugin.clone()).await
        }
        "tls2raw" => {
            let tls = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    transport::certificates(&plugin.crt_path)?,
                    transport::private_key(&plugin.key_path)?,
                )?;
            let mut remote = tokio::time::timeout(
                Duration::from_secs(10),
                tokio_rustls::TlsAcceptor::from(Arc::new(tls)).accept(stream),
            )
            .await
            .context("tls2raw 握手超时")??;
            let mut local = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::TcpStream::connect(&plugin.local_addr),
            )
            .await
            .context("tls2raw 本地连接超时")??;
            if let Some(header) = proxy_header {
                local.write_all(&header).await?;
            }
            tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
            Ok(())
        }
        "unix_domain_socket" => {
            #[cfg(unix)]
            {
                let mut local = tokio::net::UnixStream::connect(&plugin.unix_path).await?;
                if let Some(header) = proxy_header {
                    local.write_all(&header).await?;
                }
                tokio::io::copy_bidirectional(&mut stream, &mut local).await?;
                Ok(())
            }
            #[cfg(not(unix))]
            {
                let _ = &mut stream;
                bail!("当前平台不支持 Unix socket")
            }
        }
        _ => bail!("未知插件 {}", plugin.r#type),
    }
}
async fn socks5(mut s: BoxStream, p: &Plugin) -> Result<()> {
    let (host, port) = tokio::time::timeout(Duration::from_secs(10), async {
        ensure!(s.read_u8().await? == 5, "SOCKS5 版本错误");
        let n = s.read_u8().await? as usize;
        let mut methods = vec![0; n];
        s.read_exact(&mut methods).await?;
        let method = if p.username.is_empty() { 0 } else { 2 };
        if !methods.contains(&method) {
            s.write_all(&[5, 255]).await?;
            bail!("SOCKS5 无可用认证方法");
        }
        s.write_all(&[5, method]).await?;
        if method == 2 {
            ensure!(s.read_u8().await? == 1, "SOCKS5 用户认证版本错误");
            let n = s.read_u8().await? as usize;
            let mut username = vec![0; n];
            s.read_exact(&mut username).await?;
            let n = s.read_u8().await? as usize;
            let mut password = vec![0; n];
            s.read_exact(&mut password).await?;
            let valid = bool::from(
                username.ct_eq(p.username.as_bytes()) & password.ct_eq(p.password.as_bytes()),
            );
            s.write_all(&[1, if valid { 0 } else { 1 }]).await?;
            ensure!(valid, "SOCKS5 认证失败");
        }
        let mut header = [0; 4];
        s.read_exact(&mut header).await?;
        if header[0] != 5 || header[1] != 1 || header[2] != 0 {
            s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            bail!("SOCKS5 仅支持 CONNECT");
        }
        let host = match header[3] {
            1 => {
                let mut b = [0; 4];
                s.read_exact(&mut b).await?;
                std::net::Ipv4Addr::from(b).to_string()
            }
            4 => {
                let mut b = [0; 16];
                s.read_exact(&mut b).await?;
                std::net::Ipv6Addr::from(b).to_string()
            }
            3 => {
                let n = s.read_u8().await? as usize;
                let mut b = vec![0; n];
                s.read_exact(&mut b).await?;
                String::from_utf8(b)?
            }
            _ => bail!("SOCKS5 地址类型错误"),
        };
        let port = s.read_u16().await?;
        Ok::<_, anyhow::Error>((host, port))
    })
    .await
    .context("SOCKS5 握手超时")??;
    let remote =
        tokio::time::timeout(Duration::from_secs(10), tcp_connect(&host, port, "", 30)).await;
    let mut remote = match remote {
        Ok(Ok(s)) => s,
        _ => {
            s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            bail!("SOCKS5 目标不可达");
        }
    };
    let local = remote.local_addr()?;
    let mut reply = vec![5, 0, 0];
    match local.ip() {
        std::net::IpAddr::V4(a) => {
            reply.push(1);
            reply.extend(a.octets());
        }
        std::net::IpAddr::V6(a) => {
            reply.push(4);
            reply.extend(a.octets());
        }
    }
    reply.extend(local.port().to_be_bytes());
    s.write_all(&reply).await?;
    tokio::io::copy_bidirectional(&mut s, &mut remote).await?;
    Ok(())
}
fn authorized(headers: &hyper::HeaderMap, user: &str, password: &str, key: &str) -> bool {
    if user.is_empty() {
        return true;
    }
    let expected = format!("Basic {}", STANDARD.encode(format!("{user}:{password}")));
    let actual = headers.get(key).and_then(|v| v.to_str().ok()).unwrap_or("");
    bool::from(expected.as_bytes().ct_eq(actual.as_bytes()))
}
type Body =
    http_body_util::combinators::UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
fn text_response(code: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(
            Full::new(Bytes::copy_from_slice(body.as_bytes()))
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}
fn strip_hop_headers(headers: &mut hyper::HeaderMap) {
    if let Some(value) = headers
        .get("connection")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
    {
        for name in value.split(',') {
            headers.remove(name.trim());
        }
    }
    for key in [
        "connection",
        "proxy-connection",
        "proxy-authorization",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(key);
    }
}
async fn http_proxy(stream: BoxStream, plugin: &Plugin) -> Result<()> {
    let plugin = plugin.clone();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .no_proxy()
        .build()?;
    let upgrades = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));
    let upgrade_jobs = upgrades.clone();
    let service = service_fn(move |mut request: Request<Incoming>| {
        let upgrades = upgrade_jobs.clone();
        let plugin = plugin.clone();
        let http = http.clone();
        async move {
            if !authorized(
                request.headers(),
                &plugin.http_user,
                &plugin.http_password,
                "proxy-authorization",
            ) {
                let mut r = text_response(
                    StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                    "Proxy authentication required",
                );
                r.headers_mut().insert(
                    "proxy-authenticate",
                    "Basic realm=\"frpc\"".parse().unwrap(),
                );
                return Ok::<_, Infallible>(r);
            }
            if request.method() == hyper::Method::CONNECT {
                let target = request.uri().authority().map(|v| v.as_str().to_owned());
                let Some(target) = target else {
                    return Ok(text_response(
                        StatusCode::BAD_REQUEST,
                        "CONNECT requires host:port",
                    ));
                };
                let remote = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(target),
                )
                .await;
                let Ok(Ok(mut remote)) = remote else {
                    return Ok(text_response(
                        StatusCode::BAD_GATEWAY,
                        "Target connection failed",
                    ));
                };
                upgrades.lock().await.spawn(async move {
                    if let Ok(upgrade) = hyper::upgrade::on(&mut request).await {
                        let mut io = TokioIo::new(upgrade);
                        let _ = tokio::io::copy_bidirectional(&mut io, &mut remote).await;
                    }
                });
                return Ok(text_response(StatusCode::OK, ""));
            }
            let url = request.uri().to_string();
            let valid = url::Url::parse(&url)
                .map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                .unwrap_or(false);
            if !valid {
                return Ok(text_response(
                    StatusCode::BAD_REQUEST,
                    "Absolute HTTP URL required",
                ));
            }
            let (mut parts, body) = request.into_parts();
            strip_hop_headers(&mut parts.headers);
            let result = http
                .request(parts.method, url)
                .headers(parts.headers)
                .body(reqwest::Body::wrap_stream(body.into_data_stream()))
                .send()
                .await;
            let response = match result {
                Ok(mut upstream) => {
                    strip_hop_headers(upstream.headers_mut());
                    let status = upstream.status();
                    let headers = upstream.headers().clone();
                    let stream = upstream
                        .bytes_stream()
                        .map(|r| r.map(hyper::body::Frame::data));
                    let body = http_body_util::StreamBody::new(stream)
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                        .boxed_unsync();
                    let mut response = Response::new(body);
                    *response.status_mut() = status;
                    *response.headers_mut() = headers;
                    response
                }
                Err(_) => text_response(StatusCode::BAD_GATEWAY, "Upstream request failed"),
            };
            Ok(response)
        }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await?;
    while upgrades.lock().await.join_next().await.is_some() {}
    Ok(())
}
async fn http_bridge(mut stream: BoxStream, p: Plugin) -> Result<()> {
    let incoming_tls = p.r#type.starts_with("https2");
    if incoming_tls {
        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                transport::certificates(&p.crt_path)?,
                transport::private_key(&p.key_path)?,
            )?;
        if p.enable_http2.unwrap_or(true) {
            tls.alpn_protocols.push(b"h2".to_vec());
        }
        tls.alpn_protocols.push(b"http/1.1".to_vec());
        stream = Box::new(
            tokio::time::timeout(
                Duration::from_secs(10),
                tokio_rustls::TlsAcceptor::from(Arc::new(tls)).accept(stream),
            )
            .await
            .context("HTTPS 插件握手超时")??,
        );
    }
    // Matches upstream frpc's HTTPS bridge behavior for self-signed backends.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()?;
    let upgrades = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));
    let jobs = upgrades.clone();
    let service = service_fn(move |mut request: Request<Incoming>| {
        let p = p.clone();
        let http = http.clone();
        let jobs = jobs.clone();
        async move {
            let scheme = if p.r#type.ends_with("2https") {
                "https"
            } else {
                "http"
            };
            let url = format!(
                "{scheme}://{}{}",
                p.local_addr,
                request
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/")
            );
            let is_upgrade = request.headers().contains_key("upgrade");
            let on_upgrade = hyper::upgrade::on(&mut request);
            let (mut parts, body) = request.into_parts();
            let upgrade = parts.headers.get("upgrade").cloned();
            strip_hop_headers(&mut parts.headers);
            if is_upgrade {
                if let Some(upgrade) = upgrade {
                    parts.headers.insert("upgrade", upgrade);
                    parts
                        .headers
                        .insert("connection", "upgrade".parse().unwrap());
                }
            }
            if !p.host_header_rewrite.is_empty() {
                match p.host_header_rewrite.parse() {
                    Ok(value) => {
                        parts.headers.insert("host", value);
                    }
                    Err(_) => {
                        return Ok::<_, Infallible>(text_response(
                            StatusCode::BAD_GATEWAY,
                            "Invalid hostHeaderRewrite",
                        ));
                    }
                }
            }
            for (name, value) in &p.request_headers.set {
                let Ok(name) = name.parse::<hyper::header::HeaderName>() else {
                    return Ok(text_response(
                        StatusCode::BAD_GATEWAY,
                        "Invalid request header",
                    ));
                };
                let Ok(value) = value.parse() else {
                    return Ok(text_response(
                        StatusCode::BAD_GATEWAY,
                        "Invalid request header value",
                    ));
                };
                parts.headers.insert(name, value);
            }
            let response = http
                .request(parts.method, url)
                .headers(parts.headers)
                .body(reqwest::Body::wrap_stream(body.into_data_stream()))
                .send()
                .await;
            let response = match response {
                Ok(mut upstream) => {
                    if is_upgrade && upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
                        let mut response = text_response(StatusCode::SWITCHING_PROTOCOLS, "");
                        *response.headers_mut() = upstream.headers().clone();
                        jobs.lock().await.spawn(async move {
                            if let (Ok(client), Ok(server)) =
                                tokio::join!(on_upgrade, upstream.upgrade())
                            {
                                let mut client = TokioIo::new(client);
                                let mut server = server;
                                let _ =
                                    tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            }
                        });
                        return Ok(response);
                    }
                    strip_hop_headers(upstream.headers_mut());
                    let status = upstream.status();
                    let headers = upstream.headers().clone();
                    let frames = upstream
                        .bytes_stream()
                        .map(|r| r.map(hyper::body::Frame::data));
                    let body = http_body_util::StreamBody::new(frames)
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                        .boxed_unsync();
                    let mut response = Response::new(body);
                    *response.status_mut() = status;
                    *response.headers_mut() = headers;
                    response
                }
                Err(_) => text_response(StatusCode::BAD_GATEWAY, "Upstream request failed"),
            };
            Ok(response)
        }
    });
    hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(stream), service)
        .await
        .map_err(|e| anyhow::anyhow!("HTTP bridge: {e}"))?;
    while upgrades.lock().await.join_next().await.is_some() {}
    Ok(())
}
use futures::StreamExt;
async fn static_file(stream: BoxStream, p: Plugin) -> Result<()> {
    let dir =
        tower_http::services::ServeDir::new(&p.local_path).append_index_html_on_directories(true);
    let service = service_fn(move |mut req: Request<Incoming>| {
        let p = p.clone();
        let dir = dir.clone();
        async move {
            if !authorized(
                req.headers(),
                &p.http_user,
                &p.http_password,
                "authorization",
            ) {
                let mut response =
                    text_response(StatusCode::UNAUTHORIZED, "Authentication required");
                response
                    .headers_mut()
                    .insert("www-authenticate", "Basic realm=\"frpc\"".parse().unwrap());
                return Ok::<_, Infallible>(response);
            }
            if !p.strip_prefix.is_empty() {
                let Some(path) = req
                    .uri()
                    .path_and_query()
                    .and_then(|u| u.as_str().strip_prefix(&p.strip_prefix))
                else {
                    return Ok(text_response(StatusCode::NOT_FOUND, "Not found"));
                };
                let path = if path.starts_with('/') {
                    path.to_owned()
                } else {
                    format!("/{path}")
                };
                match path.parse() {
                    Ok(uri) => *req.uri_mut() = uri,
                    Err(_) => return Ok(text_response(StatusCode::BAD_REQUEST, "Invalid path")),
                }
            }
            let response = dir.oneshot(req).await.unwrap();
            Ok(response.map(|body| {
                body.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    .boxed_unsync()
            }))
        }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await?;
    Ok(())
}
