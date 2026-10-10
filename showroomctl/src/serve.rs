//! serve：axum 控制面，127.0.0.1:8090（SPEC §3，经 Caddy /_api/* 代理）。
//!
//! 零信息暴露（SPEC §3）：/_api 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；
//! 未认证或验签失败的任何请求——无论路径是否存在、方法是否支持——统一返回
//! 404 + {"error":"E_AUTH"}，无 msg、与路径不存在不可区分。实现为一个统一 fallback：
//! 先收 body 算哈希（import 流式落盘、其余留内存 1MB；超限后丢弃只算哈希，硬上限 2x 截断），
//! 验签通过后才按 (method, path) 分发，不认识的报 404 E_NOTFOUND。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::engine::Engine as _;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use showroom_common as common;
use showroom_common::ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::state::{self, ApiError, R};
use crate::{dockerops, ops, publish};

pub const MAX_JSON_BODY: usize = 1024 * 1024;

pub struct AppState {
    pub root: PathBuf,
    /// nonce 查重（SPEC §1）："owner:nonce" → 过期时刻；惰性清理。
    /// ponytail: 全局 HashSet 语义，非严格 LRU——重启清零可接受（SPEC 明示）
    pub nonces: Mutex<HashMap<String, i64>>,
}
type S = Arc<AppState>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 零信息暴露：E_AUTH 一律 404 + {"error":"E_AUTH"}，不带 msg；细节只进日志
        if self.code == "E_AUTH" {
            eprintln!("showroomctl: E_AUTH: {}", self.msg);
            return (StatusCode::NOT_FOUND, Json(json!({"error": "E_AUTH"}))).into_response();
        }
        let st = match self.code {
            "E_NAME" => StatusCode::BAD_REQUEST,
            "E_QUOTA" => StatusCode::TOO_MANY_REQUESTS,
            "E_NOTFOUND" => StatusCode::NOT_FOUND,
            "E_NET" => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (st, Json(json!({"error": self.code, "msg": self.msg}))).into_response()
    }
}

pub async fn run(root: PathBuf) -> R<()> {
    // 平台 RSA 缺失自动生成（首次）
    if !root.join("state/platform_rsa").exists() {
        let p = ops::init_keys(&root)?;
        eprintln!("showroomctl: platform rsa generated at {}", p.display());
    }
    let st: S = Arc::new(AppState { root, nonces: Mutex::new(HashMap::new()) });
    let app = Router::new().fallback(api).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8090")
        .await
        .map_err(|e| ApiError::state(format!("bind 127.0.0.1:8090: {e}")))?;
    eprintln!("showroomctl: serving on 127.0.0.1:8090");
    axum::serve(listener, app)
        .await
        .map_err(|e| ApiError::state(format!("serve: {e}")))
}

// —— 统一入口 ——

async fn api(
    axum::extract::State(s): axum::extract::State<S>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    api_inner(&s, &method, &uri, &headers, body)
        .await
        .into_response()
}

async fn api_inner(s: &S, method: &Method, uri: &OriginalUri, headers: &HeaderMap, body: Body) -> Result<Response, ApiError> {
    let path = uri.path().to_string();
    let to_file = method == Method::POST && path == "/_api/import";
    let (sha, collected) = collect_body(&s.root, body, to_file).await?;
    // 一切端点先验签（含未知路径/方法；SPEC §3 零信息暴露）
    let owner = auth(s, method.as_str(), &uri_str(uri), &sha, headers).await?;
    if matches!(collected, Collected::Overflow) {
        return Err(ApiError::new("E_QUOTA", "body exceeds limit (json 1MB / import IMPORT_MAX_GB)"));
    }
    let q = qmap(uri);
    let r: Result<Json<Value>, ApiError> = match (method.as_str(), path.as_str(), collected) {
        ("GET", "/_api/health", _) => Ok(Json(json!({"ok": true}))),

        ("GET", "/_api/projects", _) => {
            let root = s.root.clone();
            blk(move || {
                let mut v = vec![];
                for (p, sl, c) in state::all_confs(&root) {
                    let st = dockerops::container_status(&publish::container_name(&p, &sl))
                        .unwrap_or_else(|| "missing".into());
                    v.push(json!({
                        "project": p, "slot": sl, "owner": c.owner, "tag": c.tag,
                        "url": state::public_url(&p, &sl), "status": st,
                        "port": c.port, "mem": c.mem, "cpu": c.cpu, "updated": c.updated,
                    }));
                }
                Ok(Json(json!({"projects": v})))
            })
            .await
        }

        ("POST", "/_api/publish", Collected::Mem(b)) => {
            let req: PubReq = jbody(&b)?;
            let root = s.root.clone();
            let o = blk(move || {
                publish::publish(
                    &root,
                    &owner,
                    &req.project,
                    &req.slot,
                    &req.tag,
                    publish::PublishOpts { mem: req.mem, cpu: req.cpu, cport: req.container_port },
                )
            })
            .await?;
            Ok(Json(json!({"ok": true, "url": o.url, "port": o.port})))
        }

        (
            "POST",
            "/_api/stop" | "/_api/start" | "/_api/rm" | "/_api/rollback",
            Collected::Mem(b),
        ) => {
            let req: SlotReq = jbody(&b)?;
            let conf = state::SlotConf::load(&s.root, &req.project, &req.slot)
                .map_err(|_| ApiError::new("E_NOTFOUND", format!("no slot '{}/{}'", req.project, req.slot)))?;
            if conf.owner != owner {
                return Err(ApiError::new(
                    "E_AUTH",
                    format!("project '{}' belongs to {}", req.project, conf.owner),
                ));
            }
            let act: &'static str = match path.as_str() {
                "/_api/stop" => "stop",
                "/_api/start" => "start",
                "/_api/rm" => "rm",
                _ => "rollback",
            };
            let root = s.root.clone();
            let (p, sl) = (req.project, req.slot);
            let r = blk(move || match act {
                "stop" => ops::stop(&root, &p, &sl).map(|_| None),
                "start" => ops::start(&root, &p, &sl).map(|_| None),
                "rm" => ops::rm(&root, &p, &sl).map(|_| None),
                _ => ops::rollback(&root, &p, &sl).map(Some),
            })
            .await?;
            Ok(match r {
                Some(o) => Json(json!({"ok": true, "url": o.url, "port": o.port})),
                None => Json(json!({"ok": true})),
            })
        }

        ("POST", "/_api/import", Collected::File(tmp)) => {
            let project = qget(&q, "project")?;
            let slot = qget(&q, "slot")?;
            let tag = qget(&q, "tag")?;
            state::check_name("project", &project)?;
            state::check_name("slot", &slot)?;
            publish::check_ownership(&s.root, &project, &owner)?;
            let root = s.root.clone();
            let o = owner.clone();
            let digest = blk(move || publish::import_from_file(&root, &o, &project, &slot, &tag, &tmp)).await?;
            Ok(Json(json!({"ok": true, "digest": digest})))
        }

        ("POST", "/_api/import-check", Collected::Mem(b)) => {
            let req: IdReq = jbody(&b)?;
            let present = blk(move || Ok(ops::import_check(&req.image_id))).await?;
            Ok(Json(json!({"present": present})))
        }

        ("GET", "/_api/logs", _) => {
            let project = qget(&q, "project")?;
            let slot = qget(&q, "slot")?;
            own_check(&s.root, &project, &slot, &owner)?;
            let tail: u32 = q.get("tail").and_then(|t| t.parse().ok()).unwrap_or(100).min(10000);
            let root = s.root.clone();
            let logs = blk(move || ops::logs(&root, &project, &slot, tail)).await?;
            Ok(Json(json!({"logs": logs})))
        }

        ("GET", "/_api/env", _) => {
            let project = qget(&q, "project")?;
            let slot = qget(&q, "slot")?;
            own_check(&s.root, &project, &slot, &owner)?;
            let root = s.root.clone();
            let env = blk(move || ops::env_of(&root, &project, &slot)).await?;
            Ok(Json(json!({"env": env})))
        }

        _ => Err(ApiError::new("E_NOTFOUND", "no such endpoint")),
    };
    Ok(r.map(|j| j.into_response())?)
}

// —— body 收集：哈希始终可算完，验签失败者只见统一 404 ——

enum Collected {
    Mem(Vec<u8>),
    File(PathBuf),
    Overflow,
}

async fn collect_body(root: &Path, body: Body, to_file: bool) -> R<(String, Collected)> {
    let cap: u64 = if to_file {
        state::Policy::load(root).import_cap_bytes()
    } else {
        MAX_JSON_BODY as u64
    };
    let mut tmp: Option<(PathBuf, tokio::fs::File)> = None;
    if to_file {
        tokio::fs::create_dir_all(root.join("_tmp"))
            .await
            .map_err(|e| ApiError::state(e.to_string()))?;
        let p = root.join("_tmp").join(format!("{}.bin", state::rand_hex(8)));
        let f = tokio::fs::File::create(&p).await.map_err(|e| ApiError::state(e.to_string()))?;
        tmp = Some((p, f));
    }
    let mut hasher = Sha256::new();
    let mut mem: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut over = false;
    let mut stream = body.into_data_stream();
    let mut err: Option<ApiError> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                err = Some(ApiError::new("E_NET", format!("body read: {e}")));
                break;
            }
        };
        total += chunk.len() as u64;
        hasher.update(&chunk);
        // ponytail: 硬上限 2x cap——超限仍继续算哈希以便验签（合法超大可拿到 E_QUOTA），
        // 但不能让未认证者无限流；截断后无法验签 → 统一 E_AUTH 404
        if total > cap.saturating_mul(2) {
            err = Some(ApiError::new("E_AUTH", "body exceeds hard cap"));
            break;
        }
        if !over {
            if total <= cap {
                match tmp.as_mut() {
                    Some((_, f)) => {
                        if let Err(e) = f.write_all(&chunk).await {
                            err = Some(ApiError::state(format!("tmp write: {e}")));
                            break;
                        }
                    }
                    None => mem.extend_from_slice(&chunk),
                }
            } else {
                over = true; // 继续读但不再保存
                if let Some((p, _)) = tmp.take() {
                    let _ = tokio::fs::remove_file(&p).await;
                }
            }
        }
    }
    if let Some(e) = err {
        if let Some((p, _)) = &tmp {
            let _ = std::fs::remove_file(p);
        }
        return Err(e);
    }
    if let Some((_, f)) = tmp.as_mut() {
        f.flush().await.map_err(|e| ApiError::state(format!("tmp flush: {e}")))?;
    }
    let sha = hex::encode(hasher.finalize());
    let collected = if over {
        Collected::Overflow
    } else if let Some((p, _)) = tmp {
        Collected::File(p)
    } else {
        Collected::Mem(mem)
    };
    Ok((sha, collected))
}

// —— SR-SIG1 验签（预计算哈希版）+ nonce 查重 ——

/// common::verify 需要 body 本体；import 只落盘+哈希，故此处按同一 canonical 语义自验。
/// 语义与 common::verify 完全一致：格式 → ±300s → 任一登记公钥命中。
fn verify_with_hash(
    auth_value: &str,
    pubkeys: &[VerifyingKey],
    method: &str,
    path_q: &str,
    body_sha_hex: &str,
    now: i64,
) -> std::result::Result<common::Verified, common::AuthError> {
    let rest = auth_value.strip_prefix("SR-SIG1 ").ok_or(common::AuthError::Format)?;
    let mut it = rest.splitn(4, ':');
    let (owner, ts, nonce, sig_b64) = match (it.next(), it.next(), it.next(), it.next()) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return Err(common::AuthError::Format),
    };
    if owner.is_empty() || nonce.is_empty() {
        return Err(common::AuthError::Format);
    }
    let ts: i64 = ts.parse().map_err(|_| common::AuthError::Format)?;
    let raw = URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| common::AuthError::Format)?;
    let arr: [u8; 64] = raw.try_into().map_err(|_| common::AuthError::Format)?;
    let sig = Signature::from_bytes(&arr);
    if (now - ts).abs() > common::SIG_WINDOW_SECS {
        return Err(common::AuthError::Stale);
    }
    let canonical = common::build_canonical(owner, ts, nonce, method, path_q, body_sha_hex);
    for vk in pubkeys {
        if vk.verify(canonical.as_bytes(), &sig).is_ok() {
            return Ok(common::Verified { owner: owner.to_owned(), ts, nonce: nonce.to_owned() });
        }
    }
    Err(common::AuthError::BadSignature)
}

/// nonce 查重 + 惰性清理（过期条目随每次插入移除）。
fn nonce_fresh(m: &mut HashMap<String, i64>, owner: &str, nonce: &str, now: i64) -> bool {
    m.retain(|_, exp| *exp > now);
    let key = format!("{owner}:{nonce}");
    if m.contains_key(&key) {
        return false;
    }
    m.insert(key, now + common::SIG_WINDOW_SECS);
    true
}

async fn auth(s: &S, method: &str, uri: &str, body_sha: &str, headers: &HeaderMap) -> R<String> {
    let now = state::unix_now() as i64;
    let authv = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new("E_AUTH", "missing Authorization"))?;
    let owner = authv.strip_prefix("SR-SIG1 ").and_then(|r| r.split(':').next()).unwrap_or("");
    let pubkeys = if owner.is_empty() { vec![] } else { state::owner_pubkeys(&s.root, owner) };
    let v = verify_with_hash(authv, &pubkeys, method, uri, body_sha, now)
        .map_err(|e| ApiError::new("E_AUTH", e.to_string()))?;
    let mut m = s.nonces.lock().unwrap();
    if !nonce_fresh(&mut m, &v.owner, &v.nonce, now) {
        return Err(ApiError::new("E_AUTH", format!("nonce replay: {}:{}", v.owner, v.nonce)));
    }
    Ok(v.owner)
}

// —— 小工具 ——

async fn blk<T, F>(f: F) -> R<T>
where
    F: FnOnce() -> R<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::state(format!("join: {e}")))?
}

fn uri_str(uri: &OriginalUri) -> String {
    uri.0
        .path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| uri.0.path().to_string())
}

fn qmap(uri: &OriginalUri) -> HashMap<String, String> {
    // 参数字符集受 SPEC §4 限制（小写字母数字连字符），免 percent-decode
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

fn qget(m: &HashMap<String, String>, k: &str) -> R<String> {
    m.get(k).cloned().ok_or_else(|| ApiError::new("E_NAME", format!("missing query param '{k}'")))
}

fn jbody<T: serde::de::DeserializeOwned>(b: &[u8]) -> R<T> {
    serde_json::from_slice(b).map_err(|e| ApiError::new("E_NAME", format!("bad json body: {e}")))
}

fn own_check(root: &Path, project: &str, slot: &str, owner: &str) -> R<()> {
    let conf = state::SlotConf::load(root, project, slot)
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no slot '{project}/{slot}'")))?;
    if conf.owner != owner {
        return Err(ApiError::new("E_AUTH", format!("project '{project}' belongs to {}", conf.owner)));
    }
    Ok(())
}

#[derive(Deserialize)]
struct PubReq {
    project: String,
    slot: String,
    tag: String,
    mem: Option<String>,
    cpu: Option<f64>,
    container_port: Option<u16>,
}

#[derive(Deserialize)]
struct SlotReq {
    project: String,
    slot: String,
}

#[derive(Deserialize)]
struct IdReq {
    image_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use showroom_common::sha256_hex;

    // 预计算哈希验签：往返 / body 哈希不匹配 / 过期
    #[test]
    fn verify_hash_roundtrip() {
        let k = showroom_common::OwnerKey::generate();
        let body = br#"{"project":"demo"}"#;
        let authv = showroom_common::sign("alice", &k, "POST", "/_api/publish", body, 1000, "aabb0011ccdd2233");
        let vk = [showroom_common::OwnerKey::from_pub_string(&k.pub_string()).unwrap()];
        let v = verify_with_hash(&authv, &vk, "POST", "/_api/publish", &sha256_hex(body), 1000).unwrap();
        assert_eq!((v.owner.as_str(), v.nonce.as_str()), ("alice", "aabb0011ccdd2233"));

        assert_eq!(
            verify_with_hash(&authv, &vk, "POST", "/_api/publish", &sha256_hex(b"x"), 1000),
            Err(common::AuthError::BadSignature)
        );
        assert_eq!(
            verify_with_hash(&authv, &vk, "POST", "/_api/publish", &sha256_hex(body), 1000 + 301),
            Err(common::AuthError::Stale)
        );
        assert_eq!(
            verify_with_hash("garbage", &vk, "POST", "/_api/publish", &sha256_hex(body), 1000),
            Err(common::AuthError::Format)
        );
    }

    // nonce 重放拒绝 + 过期惰性清理
    #[test]
    fn nonce_freshness() {
        let mut m = HashMap::new();
        assert!(nonce_fresh(&mut m, "alice", "n1", 1000));
        assert!(!nonce_fresh(&mut m, "alice", "n1", 1005), "replay must fail");
        assert!(nonce_fresh(&mut m, "alice", "n2", 1005));
        assert!(nonce_fresh(&mut m, "bob", "n1", 1005), "per-owner namespace");
        // 过期条目被清理后同 nonce 可再用（签名本身已过窗，无法重放）
        assert!(nonce_fresh(&mut m, "alice", "n1", 1000 + 2 * common::SIG_WINDOW_SECS + 1));
    }
}
