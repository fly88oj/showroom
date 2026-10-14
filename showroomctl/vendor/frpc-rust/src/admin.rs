use crate::{
    config::Config,
    service::{Command, Shared},
};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<Shared>) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/api/status", get(status))
        .route("/api/reload", post(reload))
        .route("/api/stop", post(stop))
        .route("/metrics", get(metrics))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(state.shutdown.clone().cancelled_owned())
        .await?;
    Ok(())
}
async fn authorize(State(state): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let config = state.config.read().await;
    let web = &config.web_server;
    if !web.user.is_empty() {
        let expected = format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", web.user, web.password))
        );
        let actual = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !bool::from(expected.as_bytes().ct_eq(actual.as_bytes())) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"frpc-rust\"")],
                "需要认证",
            )
                .into_response();
        }
    }
    // Browser cross-origin requests cannot change client state, even on an
    // unauthenticated loopback admin listener.
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if origin.to_str().ok() != Some(&format!("http://{host}")) {
            return (StatusCode::FORBIDDEN, "拒绝跨源请求").into_response();
        }
    }
    if req
        .headers()
        .get("Sec-Fetch-Site")
        .and_then(|h| h.to_str().ok())
        == Some("cross-site")
    {
        return (StatusCode::FORBIDDEN, "拒绝跨站请求").into_response();
    }
    drop(config);
    next.run(req).await
}
async fn index() -> Html<&'static str> {
    Html(include_str!("admin.html"))
}
async fn health(State(state): State<Arc<Shared>>) -> Response {
    let s = state.status.read().await;
    if s.connected {
        (StatusCode::OK, Json(json!({"status":"ok"}))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"offline"})),
        )
            .into_response()
    }
}
async fn status(State(state): State<Arc<Shared>>) -> Json<Value> {
    let snapshot = state.status.read().await;
    let mut value = serde_json::to_value(&*snapshot).unwrap_or_default();
    for kind in ["tcp", "udp", "http", "https", "stcp", "sudp", "tcpmux"] {
        value[kind] = json!(
            snapshot
                .proxies
                .values()
                .filter(|p| p.r#type == kind)
                .collect::<Vec<_>>()
        );
    }
    Json(value)
}
async fn reload(State(state): State<Arc<Shared>>) -> Response {
    let result: Result<()> = async {
        let path = state.config_path.clone();
        let config = tokio::task::spawn_blocking(move || Config::load(&path)).await??;
        config.validate_resources()?;
        let (tx, rx) = oneshot::channel();
        state
            .commands
            .send(Command::Reload(Arc::new(config), tx))
            .await
            .context("客户端已停止")?;
        tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .context("重载超时")??
    }
    .await;
    match result {
        Ok(()) => Json(json!({"message":"配置已接受，正在重新连接并注册代理"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{e:#}")})),
        )
            .into_response(),
    }
}
async fn stop(State(state): State<Arc<Shared>>) -> Json<Value> {
    state.shutdown.cancel();
    Json(json!({"message":"客户端正在停止"}))
}
fn label(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
async fn metrics(State(state): State<Arc<Shared>>) -> Response {
    let s = state.status.read().await;
    let mut text = format!(
        "# TYPE frpc_connected gauge\nfrpc_connected {}\n# TYPE frpc_reconnects_total counter\nfrpc_reconnects_total {}\n",
        u8::from(s.connected),
        s.reconnects
    );
    for p in s.proxies.values() {
        let name = label(&p.name);
        text.push_str(&format!("frpc_proxy_active_connections{{proxy=\"{name}\"}} {}\nfrpc_proxy_connections_total{{proxy=\"{name}\"}} {}\nfrpc_proxy_sent_bytes_total{{proxy=\"{name}\"}} {}\nfrpc_proxy_received_bytes_total{{proxy=\"{name}\"}} {}\n",p.active_connections,p.total_connections,p.bytes_sent,p.bytes_received));
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response()
}
