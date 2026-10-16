//! srtp_ws：`/_srtp` WebSocket 载波端点（plan-18 §2/§3.2）。
//!
//! WS 二进制消息承载 SRTP：msg[0]=srtp_id(8B)+wrap(384B)，其后每条消息=记录字节
//! （与 CLI 的 TCP 载波共用记录层/应用帧）。每完成一帧（一个 HTTP/1.1 请求）即经
//! inner Router 分发——SR-SIG1 验签、nonce 防重放、E_AUTH 404 零信息暴露全部原样。
//! 会话核心泛型于消息通道（真 WS 胶水薄层，状态机可 futures-channel 直测）。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::Router;
use futures_util::{SinkExt, StreamExt};

use showroom_common::srtp::{TReader, TWriter, SrtpError};
use showroom_common::{srtp::unwrap_key, WRAP_LEN};

use crate::tunnel::gateway::FrameFwd;

/// WS 帧载荷上限（16MiB：1MiB JSON + 8MiB 切片 + 余量；plan-18 §2）。
const WS_CAP: usize = 16 << 20;

pub fn srtp_ws_upgrades(
    ws: WebSocketUpgrade,
    inner: Router,
    root: std::path::PathBuf,
) -> axum::response::Response {
    // 子协议即版本协商；不符按零信息纪律 404 空体（不升级即断）
    ws.protocols(["showroom-srtp-v1"]).on_upgrade(move |sock| async move {
        if let Err(e) = srtp_ws_session(sock, inner, root).await {
            eprintln!("showroomctl: srtp-ws: 会话结束: {e}");
        }
    })
}

async fn srtp_ws_session(sock: WebSocket, inner: Router, root: std::path::PathBuf) -> Result<(), String> {
    let (mut sink, mut stream) = sock.split();

    // msg0：8B id + 384B wrap → per-user 私钥解 wrap
    let first = stream
        .next()
        .await
        .ok_or("closed before preamble")?
        .map_err(|e| e.to_string())?;
    let bytes = match first {
        Message::Binary(b) => b,
        _ => return Err("preamble not binary".into()),
    };
    if bytes.len() != 8 + WRAP_LEN {
        return Err(format!("bad preamble len {}", bytes.len()));
    }
    let id: [u8; 8] = bytes[..8].try_into().expect("8B");
    let pem = crate::tunnel::srtpkeys::lookup_pem(&root, &id).ok_or("unknown srtp_id")?;
    let k = unwrap_key(&pem, &bytes[8..]).map_err(|e| e.to_string())?;
    eprintln!("showroomctl: srtp-ws: 会话建立（wrap 已解，下行对称封装开始）");

    let mut reader = TReader::new(k);
    let mut writer = TWriter::new(k, Box::new(rand::rngs::OsRng));
    let mut fwd = FrameFwd::default();
    let mut plain = Vec::new();

    while let Some(msg) = stream.next().await {
        let bytes = match msg.map_err(|e| e.to_string())? {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => return Err("non-binary frame".into()),
        };
        if bytes.len() > WS_CAP {
            return Err("frame over cap".into());
        }
        reader.push(&bytes, &mut plain).map_err(|e: SrtpError| e.to_string())?;
        if plain.is_empty() {
            continue;
        }
        let req_bytes = fwd.feed(&plain).map_err(|e| e.to_string())?;
        plain.clear();
        if req_bytes.is_empty() {
            continue; // 帧未完，继续收
        }
        // 一帧 = 一个 HTTP/1.1 请求 → inner Router 分发 → 响应序列化 → 记录回发
        let resp = dispatch(inner.clone(), &req_bytes).await?;
        let mut wire = Vec::with_capacity(resp.len() + 64);
        writer.record(&resp, &mut wire).map_err(|e| e.to_string())?;
        sink.send(Message::Binary(wire.into())).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 解析 HTTP/1.1 请求（请求行+头+Content-Length 体）→ 构造 Request → inner.call
/// → 收集响应（≤WS_CAP）→ 序列化回 HTTP/1.1 字节（Content-Length 定长）。
pub(crate) async fn dispatch(inner: Router, raw: &[u8]) -> Result<Vec<u8>, String> {
    use axum::body::Body;
    use axum::http::{HeaderName, HeaderValue, Request};

    let head_end = find(raw, b"\r\n\r\n").ok_or("no header end")?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| "head utf8")?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next().ok_or("no request line")?;
    let mut parts = req_line.split_ascii_whitespace();
    let method = parts.next().ok_or("no method")?;
    let uri = parts.next().ok_or("no uri")?;
    let mut headers = Vec::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        headers.push((
            HeaderName::from_bytes(k.trim().as_bytes()).map_err(|_| "bad header name")?,
            HeaderValue::from_str(v.trim()).map_err(|_| "bad header value")?,
        ));
    }
    // 体长：Content-Length（无则 0——本端点客户端恒带 CL）
    let body_len: usize = headers
        .iter()
        .find(|(k, _)| k.as_str() == "content-length")
        .and_then(|(_, v)| v.to_str().ok().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    let body = raw
        .get(head_end + 4..head_end + 4 + body_len)
        .ok_or("body short")?;

    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in &headers {
        if k.as_str() == "host" || k.as_str() == "content-length" {
            continue; // 宿主头由本进程身份决定；体长由 Body 定
        }
        req = req.header(k, v);
    }
    let mut req = req
        .body(Body::from(body.to_vec()))
        .map_err(|e| format!("build request: {e}"))?;
    // axum fallback 依赖 OriginalUri 提取——与真实链路一致补上
    req.extensions_mut()
        .insert(axum::extract::OriginalUri(uri.parse().map_err(|e| format!("uri: {e}"))?));

    use tower::Service as _;
    let mut inner = inner;
    let resp = inner
        .call(req)
        .await
        .map_err(|e| format!("dispatch: {e}"))?;
    let status = resp.status();
    let reason = status.canonical_reason().unwrap_or("");
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let body_out = axum::body::to_bytes(resp.into_body(), WS_CAP)
        .await
        .map_err(|e| format!("body collect: {e}"))?
        .to_vec();

    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\n\r\n",
        status.as_u16(),
        reason,
        body_out.len(),
        content_type
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(&body_out);
    Ok(out)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_roundtrip_headers_body_and_404_shape() {
        // 最小 inner：echo 端点返回签名头回显 + 体；未匹配路径 → 404（零信息形状）
        async fn echo(
            axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
            headers: axum::http::HeaderMap,
            body: String,
        ) -> (axum::http::StatusCode, String) {
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            (axum::http::StatusCode::OK, format!("{uri}|{auth}|{body}"))
        }
        let inner = axum::Router::new()
            .route("/_api/echo", axum::routing::post(echo));

        let body = r#"{"a":1}"#;
        let req = format!(
            "POST /_api/echo?x=2 HTTP/1.1\r\nHost: h\r\nAuthorization: SR-SIG1 u\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = dispatch(inner.clone(), req.as_bytes()).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Length: 32"), "{text}");
        assert!(text.ends_with(&format!("/_api/echo?x=2|SR-SIG1 u|{body}")), "URI/头/体原样过桥：{text}");

        // 未匹配路径：404 形状经序列化保持
        let miss = b"GET /nope HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n";
        let resp2 = dispatch(inner, miss).await.unwrap();
        let t2 = String::from_utf8_lossy(&resp2);
        assert!(t2.starts_with("HTTP/1.1 404"), "{t2}");
    }
}
