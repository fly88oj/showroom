//! streams：SPEC §10.4 实时下行——ndjson 流（fetch 流式消费，SSE 风格）。
//!
//! 方向性决策：控制台上行全是短请求，无需双向，不引 WebSocket。
//!
//! A. GET /_api/logs-stream：`docker logs -f --tail n` 桥接（stdout+stderr 合流）。
//! B. GET /_api/events：`docker events`（event 过滤 + {{json .}}）桥接 + 30s 心跳。
//!
//! 两者共用「子进程 + 双读线程 + mpsc 桥」：客户端断开（响应流 drop → 接收端关闭 →
//! 发送失败）即 kill_wait 子进程；双流 EOF 后监工线程再 kill_wait 收尸——绝不留孤儿
//! （参照 import_inner 的 kill_wait 模式）。

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tokio::sync::mpsc;

use crate::dockerops;
use crate::publish;
use crate::state::{self, ApiError, R};

/// 事件流心跳间隔（SPEC §10.4 B：防代理空闲断连）。
pub const HEARTBEAT_SECS: u64 = 30;

/// 单行 → ndjson 载荷的封装器（None = 该行静默丢弃）。
type LineWrap = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

// —— 纯函数（单测覆盖，不碰 docker）——

/// 一行日志 → ndjson：`{"t":<unix_ms>,"l":"<行>"}\n`（l 经 JSON 转义，日志内容不丢；
/// 字段顺序与 SPEC 示例一致，手拼免 json! 的 BTreeMap 键排序）。
pub fn ndjson_log_line(t_ms: u64, line: &str) -> String {
    let l = serde_json::to_string(line).unwrap_or_else(|_| "\"\"".to_string());
    format!("{{\"t\":{t_ms},\"l\":{l}}}\n")
}

/// `docker events --format {{json .}}` 的一行 → (t_ms, e, actor)。
/// t 优先 timeNano/1e6，回落 time*1000；e=Action；actor=Actor.Attributes.name（缺省 Actor.ID）；
/// 任一关键字段缺失 → None（非 JSON 噪声 / 异形事件静默丢弃）。
pub fn parse_event_line(line: &str) -> Option<(u64, String, String)> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let t = v
        .get("timeNano")
        .and_then(|x| x.as_u64())
        .map(|n| n / 1_000_000)
        .or_else(|| v.get("time").and_then(|x| x.as_u64()).map(|n| n.saturating_mul(1000)))?;
    let e = v.get("Action").and_then(|x| x.as_str())?.to_string();
    let actor = v
        .pointer("/Actor/Attributes/name")
        .or_else(|| v.pointer("/Actor/ID"))
        .and_then(|x| x.as_str())?
        .to_string();
    Some((t, e, actor))
}

/// 事件三元组 → ndjson：`{"t":..,"e":..,"actor":..}\n`。
pub fn ndjson_event_line(t_ms: u64, e: &str, actor: &str) -> String {
    let esc = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    format!("{{\"t\":{t_ms},\"e\":{},\"actor\":{}}}\n", esc(e), esc(actor))
}

/// 心跳行（B：每 HEARTBEAT_SECS 推一行）。
pub fn heartbeat_line() -> String {
    "{\"hb\":true}\n".to_string()
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// —— 流桥（mpsc::Receiver → futures Stream；Body::from_stream 消费）——

struct ChanStream(mpsc::Receiver<Vec<u8>>);

impl futures_util::stream::Stream for ChanStream {
    type Item = std::io::Result<Vec<u8>>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx).map(|o| o.map(Ok))
    }
}

fn ndjson_response(rx: mpsc::Receiver<Vec<u8>>) -> R<Response> {
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(ChanStream(rx)))
        .map_err(|e| ApiError::state(e.to_string()))
}

/// 单流桥接线程：按行读（read_until，容非 UTF-8 日志——lossy 转字符串）、封装 ndjson、
/// 送进通道。发送失败（客户端断开）或读错误 → kill_wait 子进程后退出；EOF 自然退出
/// （收尸由监工线程统一完成）。
fn bridge(r: Box<dyn Read + Send>, child: Arc<Mutex<Child>>, tx: mpsc::Sender<Vec<u8>>, wrap: LineWrap) {
    let mut reader = BufReader::new(r);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return, // EOF
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                    buf.pop();
                }
                if let Some(payload) = wrap(&String::from_utf8_lossy(&buf)) {
                    if tx.blocking_send(payload.into_bytes()).is_err() {
                        dockerops::kill_wait(&mut child.lock().unwrap()); // 客户端断开
                        return;
                    }
                }
            }
            Err(_) => {
                dockerops::kill_wait(&mut child.lock().unwrap());
                return;
            }
        }
    }
}

/// 起「stdout+stderr 双桥」子进程并返回 (ndjson Response, 发送端克隆)。
/// 发送端全部释放（含心跳任务）且双桥 EOF 后，响应流以 None 收尾；
/// 监工线程 join 双桥后 kill_wait 收尸（对已退出进程 kill/wait 只是 Err，忽略）。
fn bridged_response(cmd: &mut Command, what: &str, wrap: LineWrap) -> R<(Response, mpsc::Sender<Vec<u8>>)> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn {what}: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));

    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
    let mut handles = vec![];
    // stdout/stderr 统一为 Box<dyn Read>（ChildStdout/ChildStderr 异型入同一循环）
    for stream in [
        stdout.map(|s| Box::new(s) as Box<dyn Read + Send>),
        stderr.map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let (tx, child, wrap) = (tx.clone(), child.clone(), wrap.clone());
        handles.push(std::thread::spawn(move || bridge(stream, child, tx, wrap)));
    }
    {
        let child = child.clone();
        std::thread::spawn(move || {
            for h in handles {
                let _ = h.join();
            }
            dockerops::kill_wait(&mut child.lock().unwrap());
        });
    }
    Ok((ndjson_response(rx)?, tx))
}

// —— A：GET /_api/logs-stream ?project&tail（§11 无 slot）——

/// 统一验签 + own_check 已由 serve 完成；这里做存在性校验后直接桥 docker logs -f。
pub fn logs_stream(root: &Path, project: &str, tail: u32) -> R<Response> {
    if state::Conf::load(root, project).is_err() {
        return Err(ApiError::new("E_NOTFOUND", format!("no project '{project}'")));
    }
    let name = publish::container_name(project);
    if dockerops::container_status(&name).is_none() {
        return Err(ApiError::new("E_NOTFOUND", format!("no container '{name}'")));
    }
    let tail_s = tail.to_string();
    let mut cmd = Command::new("docker");
    cmd.args(["logs", "-f", "--tail", &tail_s, &name]);
    let wrap: LineWrap = Arc::new(move |l| Some(ndjson_log_line(unix_now_ms(), l)));
    let (resp, _tx) = bridged_response(&mut cmd, "docker logs -f", wrap)?;
    Ok(resp)
}

// —— B：GET /_api/events ——

/// 仅统一验签（全局事件流，无 own_check；控制台按 sr- 前缀自行过滤展示）。
/// 事件量小不做容器名过滤；解析失败的行（stderr 噪声等）静默丢弃。
pub fn events_stream() -> R<Response> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "events",
        "--filter", "event=start",
        "--filter", "event=stop",
        "--filter", "event=die",
        "--filter", "event=health_status",
        "--format", "{{json .}}",
    ]);
    let wrap: LineWrap =
        Arc::new(|l: &str| parse_event_line(l).map(|(t, e, a)| ndjson_event_line(t, &e, &a)));
    let (resp, tx) = bridged_response(&mut cmd, "docker events", wrap)?;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)).await;
            if tx.send(heartbeat_line().into_bytes()).await.is_err() {
                return; // 流已断（接收端 drop），心跳任务自灭
            }
        }
    });
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ndjson 日志行封装：定形 / JSON 转义 / 多字节 / 每行可独立 JSON 解析
    #[test]
    fn ndjson_log_line_wrapping() {
        assert_eq!(ndjson_log_line(1700000000123, "hello"), "{\"t\":1700000000123,\"l\":\"hello\"}\n");
        // 引号 / 反斜杠 / 制表符经 JSON 转义，内容不丢
        let l = ndjson_log_line(1, "a\"b\\c\td\x01");
        assert_eq!(l, "{\"t\":1,\"l\":\"a\\\"b\\\\c\\td\\u0001\"}\n");
        // 中文原样（UTF-8 直出）
        assert_eq!(ndjson_log_line(2, "中文日志"), "{\"t\":2,\"l\":\"中文日志\"}\n");
        // 逐行独立可解析：t/l 值与输入一致
        for (raw, t) in [("plain", 7u64), ("with \"quote\"", 8), ("tab\there", 9)] {
            let line = ndjson_log_line(t, raw);
            assert!(line.ends_with('\n'));
            let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(v["t"].as_u64(), Some(t));
            assert_eq!(v["l"].as_str(), Some(raw));
        }
    }

    // 事件行：定形 + 转义 + 与 parse_event_line 往返
    #[test]
    fn ndjson_event_line_wrapping() {
        assert_eq!(
            ndjson_event_line(1700000000123, "start", "sr-hello-stable"),
            "{\"t\":1700000000123,\"e\":\"start\",\"actor\":\"sr-hello-stable\"}\n"
        );
        assert_eq!(
            ndjson_event_line(1, "health_status: unhealthy", "a\"b"),
            "{\"t\":1,\"e\":\"health_status: unhealthy\",\"actor\":\"a\\\"b\"}\n"
        );
        let line = ndjson_event_line(5, "die", "sr-x");
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["e"].as_str(), Some("die"));
        assert_eq!(v["actor"].as_str(), Some("sr-x"));
    }

    // docker events --format {{json .}} 样例解析（真实字段布局）
    #[test]
    fn docker_event_line_parse() {
        let start = r#"{"status":"start","id":"9f2c4d","from":"demo:v1","Type":"container","Action":"start","Actor":{"ID":"9f2c4d","Attributes":{"image":"demo:v1","name":"sr-hello-stable"}},"scope":"local","time":1700000000,"timeNano":1700000000123456789}"#;
        assert_eq!(
            parse_event_line(start),
            Some((1700000000123, "start".into(), "sr-hello-stable".into()))
        );

        let die = r#"{"Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"exitCode":"137","image":"demo:v1","name":"sr-hello-next"}},"scope":"local","time":1699999999,"timeNano":1699999999998765432}"#;
        assert_eq!(
            parse_event_line(die),
            Some((1699999999998, "die".into(), "sr-hello-next".into()))
        );

        let health = r#"{"Type":"container","Action":"health_status: healthy","Actor":{"ID":"abc","Attributes":{"name":"sr-hello-stable"}},"scope":"local","time":1700000001,"timeNano":1700000001999999999}"#;
        assert_eq!(
            parse_event_line(health),
            Some((1700000001999, "health_status: healthy".into(), "sr-hello-stable".into()))
        );

        // 无 timeNano → time*1000 回落；无 name → Actor.ID 回落
        let no_nano = r#"{"Action":"stop","Actor":{"ID":"netid","Attributes":{}},"time":1700000002}"#;
        assert_eq!(parse_event_line(no_nano), Some((1700000002000, "stop".into(), "netid".into())));

        // 关键字段缺失 / 非 JSON / 空行 → None（静默丢弃）
        for bad in [
            "",
            "not json at all",
            r#"{"Action":"start","time":1}"#,                      // 无 actor
            r#"{"Actor":{"ID":"x"},"time":1}"#,                    // 无 Action
            r#"{"Action":"start","Actor":{"ID":"x"}}"#,            // 无时间
            r#"{"Action":"start","Actor":{"ID":"x"},"time":"x"}"#, // 时间非数字
        ] {
            assert_eq!(parse_event_line(bad), None, "{bad:?} 应丢弃");
        }

        // 尾部空白容错（逐行读已去 \n，这里再验证 trim 语义）
        assert_eq!(
            parse_event_line(&format!("{start}\n")),
            Some((1700000000123, "start".into(), "sr-hello-stable".into()))
        );
    }

    // 心跳行定形
    #[test]
    fn heartbeat_shape() {
        assert_eq!(heartbeat_line(), "{\"hb\":true}\n");
        let v: serde_json::Value = serde_json::from_str(heartbeat_line().trim_end()).unwrap();
        assert_eq!(v["hb"].as_bool(), Some(true));
    }
}
