//! serve：axum 控制面，127.0.0.1:8090（SPEC §3，经 Caddy /_api/* 代理）。
//!
//! 零信息暴露（SPEC §3）：/_api 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；
//! 未认证或验签失败的任何请求——无论路径是否存在、方法是否支持——统一返回
//! 404 + {"error":"E_AUTH"}，无 msg、与路径不存在不可区分。实现为一个统一 fallback：
//! 先收 body 算哈希（import/import-chunk 流式落盘、其余留内存 1MB；超限后丢弃只算哈希，硬上限 2x 截断），
//! 验签通过后才按 (method, path) 分发，不认识的报 404 E_NOTFOUND。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use showroom_common as common;

use crate::dbmaint;
use crate::state::{self, ApiError, R};
use crate::{ops, publish, streams};

pub const MAX_JSON_BODY: usize = 1024 * 1024;

pub struct AppState {
    pub root: PathBuf,
    /// nonce 查重（SPEC §1）："user:nonce" → 过期时刻；惰性清理。
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
    // owner→user 更名的状态目录迁移（幂等：state/owners/ → state/users/，SPEC §5）
    state::migrate_state_dirs(&root);
    // v1.4 槽位布局迁移（§11，幂等；state 上提 + caddy reload + 容器收养，stderr/日志如实记录）
    for l in ops::migrate_legacy(&root) {
        eprintln!("showroomctl: migrate: {l}");
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
    let to_file = method == Method::POST
        && matches!(path.as_str(), "/_api/import" | "/_api/import-chunk" | "/_api/db-restore");
    let (sha, collected) = collect_body(&s.root, body, to_file).await?;
    // _tmp 落盘文件的兜底清理：验签失败/名字校验拒绝等任何提前返回都不留尸体
    //（import/import_chunk 正常路径自删；remove 幂等，二次删除 NotFound 忽略）
    let tmp_file = if let Collected::File(p) = &collected { Some(p.clone()) } else { None };
    let r = dispatch(s, method, uri, &path, &sha, headers, &collected).await;
    if let Some(p) = tmp_file {
        let _ = std::fs::remove_file(p);
    }
    r
}

async fn dispatch(
    s: &S,
    method: &Method,
    uri: &OriginalUri,
    path: &str,
    sha: &str,
    headers: &HeaderMap,
    collected: &Collected,
) -> Result<Response, ApiError> {
    // 一切端点先验签（含未知路径/方法；SPEC §3 零信息暴露）
    let Verified { user, kid } = auth(s, method.as_str(), &uri_str(uri), sha, headers).await?;
    if matches!(collected, Collected::Overflow) {
        return Err(ApiError::new("E_QUOTA", "body exceeds limit (json 1MB / import IMPORT_MAX_GB)"));
    }
    let q = qmap(uri);
    // 流式下行早返回（SPEC §10.4）：ndjson 流 / octet-stream 切片不走统一 JSON 响应体
    if method == "GET" && path == "/_api/logs-stream" {
        let project = qget(&q, "project")?;
        state::check_name("project", &project)?;
        user_check(&s.root, &project, &user)?;
        let tail: u32 = q.get("tail").and_then(|t| t.parse().ok()).unwrap_or(100).min(10000);
        let root = s.root.clone();
        return blk(move || streams::logs_stream(&root, &project, tail)).await;
    }
    if method == "GET" && path == "/_api/events" {
        return streams::events_stream();
    }
    if method == "GET" && path == "/_api/db-dump-chunk" {
        let id = qget(&q, "dump")?;
        let offset: u64 = qget(&q, "offset")?
            .parse()
            .map_err(|_| ApiError::new("E_NAME", "bad offset"))?;
        let len: u64 = qget(&q, "len")?
            .parse()
            .map_err(|_| ApiError::new("E_NAME", "bad len"))?;
        let root = s.root.clone();
        let o = user.clone();
        let id2 = id.clone();
        let (bytes, total, finished) = blk(move || dbmaint::dump_chunk(&root, &o, &id2, offset, len)).await?;
        if finished {
            // 读完最后一片即删会话（SPEC §10.4 C；24h gc 兜底，多客户端并发拉取后到者 E_NOTFOUND 重走 begin）
            dbmaint::dump_cleanup(&s.root, &id);
        }
        let resp = axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .header("X-Sr-Total", total.to_string())
            .body(axum::body::Body::from(bytes))
            .map_err(|e| ApiError::state(e.to_string()))?;
        return Ok(resp);
    }
    let r: Result<Json<serde_json::Value>, ApiError> = match (method.as_str(), path, collected) {
        ("GET", "/_api/health", _) => Ok(Json(json!({"ok": true}))),

        // —— 身份（SPEC §12.2）：whoami 用当前请求命中的 kid 判定 admin ——
        ("GET", "/_api/whoami", _) => Ok(Json(json!({
            "user": user, "kid": kid,
            "admin": ops::key_is_admin(&s.root, &user, &kid),
        }))),

        // —— 管理组（SPEC §12.2）：统一验签后再过 admin_guard，非管理员与 E_AUTH 同 404 ——

        ("GET", "/_api/admin/users", _) => {
            admin_guard(s, &user, &kid)?;
            let root = s.root.clone();
            let users = blk(move || Ok(ops::user_list(&root))).await?;
            Ok(Json(json!({"users": users})))
        }

        ("POST", "/_api/admin/user-add", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body：非管理员不因 body 畸形泄露 E_NAME/E_AUTH 差异
            let req: AdminUserAddReq = jbody(b)?;
            let root = s.root.clone();
            let added = blk(move || {
                ops::user_add(&root, &req.user, &req.pubkey, req.admin.unwrap_or(false), false)
            })
            .await?;
            // 密码只此一次返回（远程发卡；本机 reg.cred 常驻，不再可查）
            Ok(Json(json!({"ok": true, "kid": added.kid, "registry_password": added.password})))
        }

        ("POST", "/_api/admin/user-revoke", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body：非管理员不因 body 畸形泄露 E_NAME/E_AUTH 差异
            let req: AdminUserReq = jbody(b)?;
            let root = s.root.clone();
            blk(move || ops::user_revoke(&root, &req.user)).await?;
            Ok(Json(json!({"ok": true})))
        }

        ("POST", "/_api/admin/key-rm", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body：非管理员不因 body 畸形泄露 E_NAME/E_AUTH 差异
            let req: AdminKeyReq = jbody(b)?;
            let root = s.root.clone();
            blk(move || ops::user_key_rm(&root, &req.user, &req.kid)).await?;
            Ok(Json(json!({"ok": true})))
        }

        ("POST", "/_api/admin/key-admin", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body：非管理员不因 body 畸形泄露 E_NAME/E_AUTH 差异
            let req: AdminKeyAdminReq = jbody(b)?;
            let root = s.root.clone();
            blk(move || ops::key_set_admin(&root, &req.user, &req.kid, req.admin)).await?;
            Ok(Json(json!({"ok": true})))
        }

        ("GET", "/_api/admin/status", _) => {
            admin_guard(s, &user, &kid)?;
            let root = s.root.clone();
            let status = blk(move || ops::admin_status(&root)).await?;
            Ok(Json(status))
        }

        ("GET", "/_api/projects", _) => {
            let root = s.root.clone();
            blk(move || {
                let v: Vec<_> = ops::ps(&root)
                    .into_iter()
                    .map(|(p, c, st)| c.to_json(&p, &st))
                    .collect();
                Ok(Json(json!({"projects": v})))
            })
            .await
        }

        // —— 镜像列表（SPEC §13.1，部署表单 ComboBox）：catalog+tags 展平；
        //     按签名用户 `<user>/` 前缀过滤；admin 钥匙不过滤 ——
        ("GET", "/_api/images", _) => {
            let admin = ops::key_is_admin(&s.root, &user, &kid);
            let root = s.root.clone();
            let o = user.clone();
            let images = blk(move || ops::images(&root, if admin { None } else { Some(&o) })).await?;
            Ok(Json(json!({"images": images})))
        }

        ("POST", "/_api/publish", Collected::Mem(b)) => {
            let req: PubReq = jbody(b)?;
            let root = s.root.clone();
            let o = blk(move || {
                publish::publish(
                    &root,
                    &user,
                    &req.project,
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
            let req: ProjectReq = jbody(b)?;
            user_check(&s.root, &req.project, &user)?;
            let act: &'static str = match path {
                "/_api/stop" => "stop",
                "/_api/start" => "start",
                "/_api/rm" => "rm",
                _ => "rollback",
            };
            let root = s.root.clone();
            let p = req.project;
            let r = blk(move || match act {
                "stop" => ops::stop(&root, &p).map(|_| None),
                "start" => ops::start(&root, &p).map(|_| None),
                "rm" => ops::rm(&root, &p).map(|_| None),
                _ => ops::rollback(&root, &p).map(Some),
            })
            .await?;
            Ok(match r {
                Some(o) => Json(json!({"ok": true, "url": o.url, "port": o.port})),
                None => Json(json!({"ok": true})),
            })
        }

        ("POST", "/_api/import", Collected::File(tmp)) => {
            let project = qget(&q, "project")?;
            let tag = qget(&q, "tag")?;
            state::check_name("project", &project)?;
            publish::check_ownership(&s.root, &project, &user)?;
            let root = s.root.clone();
            let o = user.clone();
            let tmp = tmp.clone();
            let digest = blk(move || publish::import_from_file(&root, &o, &project, &tag, &tmp)).await?;
            Ok(Json(json!({"ok": true, "digest": digest})))
        }

        ("POST", "/_api/import-check", Collected::Mem(b)) => {
            let req: IdReq = jbody(b)?;
            let present = blk(move || Ok(ops::import_check(&req.image_id))).await?;
            Ok(Json(json!({"present": present})))
        }

        // —— 分片导入（SPEC §8.1；控制台用，CLI 单发不变）——

        ("POST", "/_api/import-begin", Collected::Mem(b)) => {
            let req: BeginReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            publish::check_tag(&req.tag)?;
            publish::check_ownership(&s.root, &req.project, &user)?;
            let root = s.root.clone();
            let o = user.clone();
            let id = blk(move || {
                publish::import_begin(&root, &o, &req.project, &req.tag, &req.image_id, req.total_bytes)
            })
            .await?;
            Ok(Json(json!({"upload_id": id})))
        }

        ("POST", "/_api/import-chunk", Collected::File(tmp)) => {
            let id = qget(&q, "upload")?;
            let seq: u64 = qget(&q, "seq")?
                .parse()
                .map_err(|_| ApiError::new("E_NAME", "bad seq"))?;
            let root = s.root.clone();
            let o = user.clone();
            let tmp = tmp.clone();
            let n = blk(move || publish::import_chunk(&root, &o, &id, seq, &tmp)).await?;
            Ok(Json(json!({"ok": true, "received": n})))
        }

        ("GET", "/_api/import-status", _) => {
            let id = qget(&q, "upload")?;
            let m = publish::load_meta_owned(&s.root, &id, &user)?;
            Ok(Json(json!({"received": m.received, "total": m.total})))
        }

        ("POST", "/_api/import-end", _) => {
            let id = qget(&q, "upload")?;
            let root = s.root.clone();
            let o = user.clone();
            let digest = blk(move || publish::import_end(&root, &o, &id)).await?;
            Ok(Json(json!({"ok": true, "digest": digest})))
        }

        // —— 数据库维护（SPEC §9）：统一验签 + user_check 后进 dbmaint 内核 ——

        ("POST", "/_api/db-sql", Collected::Mem(b)) => {
            let req: DbSqlReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            user_check(&s.root, &req.project, &user)?;
            let (p, sql, confirm) = (req.project, req.sql, req.confirm.unwrap_or(false));
            let root = s.root.clone();
            let out = blk(move || dbmaint::sql(&root, &p, &sql, confirm)).await?;
            Ok(Json(json!({"out": out})))
        }

        ("GET", "/_api/db-tables", _) => {
            let project = qget(&q, "project")?;
            state::check_name("project", &project)?;
            user_check(&s.root, &project, &user)?;
            let root = s.root.clone();
            let tables = blk(move || dbmaint::tables(&root, &project)).await?;
            Ok(Json(json!({"tables": tables})))
        }

        ("POST", "/_api/db-preview", Collected::Mem(b)) => {
            let req: DbPreviewReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            user_check(&s.root, &req.project, &user)?;
            let (p, table, limit) = (req.project, req.table, req.limit.unwrap_or(50));
            let root = s.root.clone();
            let (columns, rows) = blk(move || dbmaint::preview(&root, &p, &table, limit)).await?;
            Ok(Json(json!({"columns": columns, "rows": rows})))
        }


        ("POST", "/_api/db-dump-begin", Collected::Mem(b)) => {
            let req: DbDumpBeginReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            user_check(&s.root, &req.project, &user)?;
            let root = s.root.clone();
            let o = user.clone();
            let (p, pem) = (req.project, req.client_pub_pem);
            let id = blk(move || dbmaint::dump_begin(&root, &o, &p, &pem)).await?;
            Ok(Json(json!({"dump_id": id})))
        }

        ("GET", "/_api/db-dump-status", _) => {
            let id = qget(&q, "dump")?;
            let root = s.root.clone();
            let o = user.clone();
            let st = blk(move || dbmaint::dump_status(&root, &o, &id)).await?;
            Ok(Json(json!({"ready": st.state == "ready", "total": st.total, "error": st.error})))
        }

        ("POST", "/_api/db-restore", Collected::File(tmp)) => {
            let project = qget(&q, "project")?;
            let confirm = q.get("confirm").map(|v| v == "true").unwrap_or(false);
            state::check_name("project", &project)?;
            user_check(&s.root, &project, &user)?;
            let root = s.root.clone();
            let p = project;
            let tmp = tmp.clone();
            blk(move || dbmaint::restore(&root, &p, confirm, &tmp)).await?;
            Ok(Json(json!({"ok": true})))
        }

        ("POST", "/_api/db-reset", Collected::Mem(b)) => {
            let req: DbResetReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            user_check(&s.root, &req.project, &user)?;
            let root = s.root.clone();
            let (p, confirm) = (req.project, req.confirm.unwrap_or(false));
            blk(move || dbmaint::reset(&root, &p, confirm)).await?;
            Ok(Json(json!({"ok": true})))
        }

        // —— Redis 浏览（SPEC §10.3 排障 + §13.2 用户隔离）：db 须 ∈ 名下项目 redisdb 集合；admin 放开全量 ——

        ("GET", "/_api/redis-dbs", _) => {
            let policy = state::Policy::load(&s.root);
            let admin = ops::key_is_admin(&s.root, &user, &kid);
            let dbs: Vec<u16> = if admin {
                (0..policy.redis_pool_cap()).collect()
            } else {
                state::user_redis_dbs(&s.root, &user)
            };
            let mut v = json!({"dbs": dbs});
            if admin {
                v["admin_view"] = json!(true); // 控制台据此标注「管理员视图」
            }
            Ok(Json(v))
        }

        ("GET", "/_api/redis-keys", _) => {
            let db = redis_db_guard(s, &q, &user, &kid)?;
            let pat = q.get("q").cloned().unwrap_or_default();
            // 默认 100 上限 200（--scan 无 limit 参数，内核读满即杀并标截断）
            let limit: usize = q.get("limit").and_then(|t| t.parse().ok()).unwrap_or(100).min(200);
            let (keys, truncated) = blk(move || ops::redis_keys(db, &pat, limit)).await?;
            Ok(Json(json!({"keys": keys, "truncated": truncated})))
        }

        ("GET", "/_api/redis-get", _) => {
            let db = redis_db_guard(s, &q, &user, &kid)?;
            let key = qget(&q, "key")?;
            let (t, ttl_ms, value) = blk(move || ops::redis_get(db, &key)).await?;
            Ok(Json(json!({"type": t, "ttl_ms": ttl_ms, "value": value})))
        }

        ("GET", "/_api/logs", _) => {
            let project = qget(&q, "project")?;
            user_check(&s.root, &project, &user)?;
            let tail: u32 = q.get("tail").and_then(|t| t.parse().ok()).unwrap_or(100).min(10000);
            let root = s.root.clone();
            let logs = blk(move || ops::logs(&root, &project, tail)).await?;
            Ok(Json(json!({"logs": logs})))
        }

        ("GET", "/_api/env", _) => {
            let project = qget(&q, "project")?;
            user_check(&s.root, &project, &user)?;
            let root = s.root.clone();
            let env = blk(move || ops::env_of(&root, &project)).await?;
            Ok(Json(json!({"env": env})))
        }

        _ => Err(ApiError::new("E_NOTFOUND", "no such endpoint")),
    };
    r.map(|j| j.into_response())
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
    if let Some((p, f)) = tmp.as_mut() {
        if let Err(e) = f.flush().await {
            let _ = std::fs::remove_file(p);
            return Err(ApiError::state(format!("tmp flush: {e}")));
        }
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

// —— SR-SIG1 验签（common::verify_with_hash，预计算哈希版）+ nonce 查重 ——

/// auth 产物：user + 命中钥匙的 kid（whoami / admin_guard 据此判定，SPEC §12.2）。
pub struct Verified {
    pub user: String,
    pub kid: String,
}

/// nonce 查重 + 惰性清理（过期条目随每次插入移除）。
fn nonce_fresh(m: &mut HashMap<String, i64>, user: &str, nonce: &str, now: i64) -> bool {
    m.retain(|_, exp| *exp > now);
    let key = format!("{user}:{nonce}");
    if m.contains_key(&key) {
        return false;
    }
    m.insert(key, now + common::SIG_WINDOW_SECS);
    true
}

async fn auth(s: &S, method: &str, uri: &str, body_sha: &str, headers: &HeaderMap) -> R<Verified> {
    let now = state::unix_now() as i64;
    let authv = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new("E_AUTH", "missing Authorization"))?;
    let user = authv.strip_prefix("SR-SIG1 ").and_then(|r| r.split(':').next()).unwrap_or("");
    // 钥匙带 kid（kid 由公钥内容派生，与 tokens/<user>/<kid>.pub 文件名约定一致）
    let keys = if user.is_empty() { vec![] } else { state::user_keys_with_kid(&s.root, user) };
    let pubkeys: Vec<_> = keys.iter().map(|(_, vk)| *vk).collect();
    let v = common::verify_with_hash(authv, &pubkeys, method, uri, body_sha, now).map_err(|e| {
        eprintln!("showroomctl: auth fail: {e:?} {method} {uri}");
        ApiError::new("E_AUTH", e.to_string())
    })?;
    // 定位命中钥匙：单把复验（user 钥匙数 handful、ed25519 验签微秒级，开销可忽略）
    let kid = keys
        .iter()
        .find(|(_, vk)| {
            common::verify_with_hash(authv, std::slice::from_ref(vk), method, uri, body_sha, now)
                .is_ok()
        })
        .map(|(kid, _)| kid.clone())
        .ok_or_else(|| ApiError::new("E_AUTH", "matching key vanished mid-auth"))?;
    let mut m = s.nonces.lock().unwrap();
    if !nonce_fresh(&mut m, &v.user, &v.nonce, now) {
        return Err(ApiError::new("E_AUTH", format!("nonce replay: {}:{}", v.user, v.nonce)));
    }
    Ok(Verified { user: v.user, kid })
}

/// 管理员钥匙校验（SPEC §12.2）：非管理员返回 E_AUTH → 与未登记/验签失败同一 404
/// `{"error":"E_AUTH"}`（IntoResponse 对 E_AUTH 丢弃 msg），不泄露 /_api/admin/* 端点存在。
fn admin_guard(s: &S, user: &str, kid: &str) -> R<()> {
    if ops::key_is_admin(&s.root, user, kid) {
        Ok(())
    } else {
        Err(ApiError::new("E_AUTH", format!("not an admin key: {user}/{kid}")))
    }
}

/// Redis 浏览归属校验（SPEC §13.2）：请求 db 须 ∈ 签名用户名下项目的 redisdb 集合
/// （state::user_redis_dbs）；admin 钥匙放开全量（0..policy 池）。
/// 非归属 → E_AUTH（404 同体，不泄露池内容；与 user_check 语义一致）。
fn redis_db_guard(s: &S, q: &HashMap<String, String>, user: &str, kid: &str) -> R<u8> {
    let policy = state::Policy::load(&s.root);
    let db = state::parse_redis_db(&qget(q, "db")?, policy.redis_pool_cap())?;
    if ops::key_is_admin(&s.root, user, kid) || state::user_redis_dbs(&s.root, user).contains(&(db as u16)) {
        Ok(db)
    } else {
        Err(ApiError::new("E_AUTH", format!("redis db {db} not in '{user}' projects' pool")))
    }
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

fn user_check(root: &Path, project: &str, user: &str) -> R<()> {
    let conf = state::Conf::load(root, project)
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no project '{project}'")))?;
    if conf.user != user {
        return Err(ApiError::new("E_AUTH", format!("project '{project}' belongs to {}", conf.user)));
    }
    Ok(())
}

#[derive(Deserialize)]
struct PubReq {
    project: String,
    tag: String,
    mem: Option<String>,
    cpu: Option<f64>,
    container_port: Option<u16>,
}

#[derive(Deserialize)]
struct ProjectReq {
    project: String,
}

#[derive(Deserialize)]
struct IdReq {
    image_id: String,
}

#[derive(Deserialize)]
struct BeginReq {
    project: String,
    tag: String,
    image_id: String,
    total_bytes: u64,
}

#[derive(Deserialize)]
struct DbSqlReq {
    project: String,
    sql: String,
    confirm: Option<bool>,
}

#[derive(Deserialize)]
struct DbPreviewReq {
    project: String,
    table: String,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct DbDumpBeginReq {
    project: String,
    client_pub_pem: String,
}

#[derive(Deserialize)]
struct DbResetReq {
    project: String,
    confirm: Option<bool>,
}

#[derive(Deserialize)]
struct AdminUserAddReq {
    user: String,
    pubkey: String,
    admin: Option<bool>,
}

#[derive(Deserialize)]
struct AdminUserReq {
    user: String,
}

#[derive(Deserialize)]
struct AdminKeyReq {
    user: String,
    kid: String,
}

#[derive(Deserialize)]
struct AdminKeyAdminReq {
    user: String,
    kid: String,
    admin: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use showroom_common::sha256_hex;

    // 预计算哈希验签（common）：往返 / body 哈希不匹配 / 过期
    #[test]
    fn verify_hash_roundtrip() {
        let k = showroom_common::UserKey::generate();
        let body = br#"{"project":"demo"}"#;
        let authv = showroom_common::sign("alice", &k, "POST", "/_api/publish", body, 1000, "aabb0011ccdd2233");
        let vk = [showroom_common::UserKey::from_pub_string(&k.pub_string()).unwrap()];
        let v = common::verify_with_hash(&authv, &vk, "POST", "/_api/publish", &sha256_hex(body), 1000).unwrap();
        assert_eq!((v.user.as_str(), v.nonce.as_str()), ("alice", "aabb0011ccdd2233"));

        assert_eq!(
            common::verify_with_hash(&authv, &vk, "POST", "/_api/publish", &sha256_hex(b"x"), 1000),
            Err(common::AuthError::BadSignature)
        );
        assert_eq!(
            common::verify_with_hash(&authv, &vk, "POST", "/_api/publish", &sha256_hex(body), 1000 + 301),
            Err(common::AuthError::Stale)
        );
        assert_eq!(
            common::verify_with_hash("garbage", &vk, "POST", "/_api/publish", &sha256_hex(body), 1000),
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
        assert!(nonce_fresh(&mut m, "bob", "n1", 1005), "per-user namespace");
        // 过期条目被清理后同 nonce 可再用（签名本身已过窗，无法重放）
        assert!(nonce_fresh(&mut m, "alice", "n1", 1000 + 2 * common::SIG_WINDOW_SECS + 1));
    }

    // admin_guard 判定：管理员钥匙放行；普通钥匙 / 他人 user / 未登记 → E_AUTH
    #[test]
    fn admin_guard_allows_admin_key_only() {
        let root = std::env::temp_dir().join(format!("srctl-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let s: S = Arc::new(AppState { root: root.clone(), nonces: Mutex::new(HashMap::new()) });

        let k = showroom_common::UserKey::generate();
        let added = crate::ops::user_add(&s.root, "root", &k.pub_string(), true, false).unwrap();
        assert!(admin_guard(&s, "root", &added.kid).is_ok());

        let k2 = showroom_common::UserKey::generate();
        let plain = crate::ops::user_add(&s.root, "root", &k2.pub_string(), false, false).unwrap();
        assert_eq!(admin_guard(&s, "root", &plain.kid).unwrap_err().code, "E_AUTH");
        assert_eq!(admin_guard(&s, "nobody", &added.kid).unwrap_err().code, "E_AUTH");
        assert_eq!(admin_guard(&s, "root", "ffffffff").unwrap_err().code, "E_AUTH");
        let _ = std::fs::remove_dir_all(&root);
    }

    // redis_db_guard 边界（§13.2）：自己的 db 放行；别人的/未分配的 → E_AUTH（404 同体）；
    // admin 钥匙放开全量；形状非法（非数字/超池）→ E_NAME
    #[test]
    fn redis_db_guard_boundaries() {
        let root = std::env::temp_dir().join(format!("srctl-redis-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let s: S = Arc::new(AppState { root: root.clone(), nonces: Mutex::new(HashMap::new()) });

        // alice 名下项目占 db 3/7；bob 占 db 2
        for (p, user, db) in [("demo1", "alice", 3u8), ("demo2", "alice", 7), ("demo3", "bob", 2)] {
            let c = crate::state::Conf {
                user: user.into(), tag: "v1".into(), port: 8100, cport: 8080,
                mem: "256m".into(), cpu: 1.0, redisdb: db, updated: 1,
                stopped: false, legacyslot: None,
            };
            c.save(&root, p).unwrap();
        }
        let qq = |db: &str| HashMap::from([("db".to_string(), db.to_string())]);

        // 自己的 db 放行；别人的（bob 的 2）/未分配的（200）→ E_AUTH
        assert_eq!(redis_db_guard(&s, &qq("3"), "alice", "ab12cd34").unwrap(), 3);
        assert_eq!(redis_db_guard(&s, &qq("7"), "alice", "ab12cd34").unwrap(), 7);
        assert_eq!(redis_db_guard(&s, &qq("2"), "alice", "ab12cd34").unwrap_err().code, "E_AUTH");
        assert_eq!(redis_db_guard(&s, &qq("200"), "alice", "ab12cd34").unwrap_err().code, "E_AUTH");
        assert_eq!(redis_db_guard(&s, &qq("3"), "nobody", "ab12cd34").unwrap_err().code, "E_AUTH");

        // admin 钥匙放开全量（含别人的 2 与池内任意 255）；超池/非数字仍 E_NAME
        let k = showroom_common::UserKey::generate();
        let added = crate::ops::user_add(&s.root, "root", &k.pub_string(), true, false).unwrap();
        assert_eq!(redis_db_guard(&s, &qq("2"), "root", &added.kid).unwrap(), 2);
        assert_eq!(redis_db_guard(&s, &qq("255"), "root", &added.kid).unwrap(), 255);
        assert_eq!(redis_db_guard(&s, &qq("256"), "root", &added.kid).unwrap_err().code, "E_NAME");
        assert_eq!(redis_db_guard(&s, &qq("abc"), "root", &added.kid).unwrap_err().code, "E_NAME");
        assert_eq!(redis_db_guard(&s, &HashMap::new(), "root", &added.kid).unwrap_err().code, "E_NAME");

        // 非管理员钥匙即便 user 有钥匙档案也不越权（无 E_AUTH 与 404 之外的信息差）
        let k2 = showroom_common::UserKey::generate();
        let plain = crate::ops::user_add(&s.root, "root", &k2.pub_string(), false, false).unwrap();
        assert_eq!(redis_db_guard(&s, &qq("3"), "root", &plain.kid).unwrap_err().code, "E_AUTH");

        // 小池（REDIS_DBS=8）：池内 7 放行、池外 15 → E_NAME（alice 名下 db 7 < 8 仍可用）
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("state/policy.env"), "REDIS_DBS=8\n").unwrap();
        assert_eq!(redis_db_guard(&s, &qq("7"), "alice", "ab12cd34").unwrap(), 7);
        assert_eq!(redis_db_guard(&s, &qq("15"), "root", &added.kid).unwrap_err().code, "E_NAME");
        let _ = std::fs::remove_dir_all(&root);
    }

    // 零信息暴露（SPEC §3/§12.2）：admin_guard 的 404 与普通 E_AUTH 逐字节一致
    //（状态码同 404、body 同 {"error":"E_AUTH"}、无 msg——非管理员探测管理端点不可区分）
    #[tokio::test]
    async fn admin_guard_404_byte_identical_to_auth() {
        let body = |r: Response| async move {
            let (parts, b) = r.into_parts();
            let bytes = axum::body::to_bytes(b, 1024).await.unwrap();
            (parts.status.as_u16(), bytes.to_vec())
        };
        let guard = admin_guard(
            &Arc::new(AppState { root: PathBuf::from("/nonexistent"), nonces: Mutex::new(HashMap::new()) }),
            "alice", "ab12cd34",
        )
        .unwrap_err()
        .into_response();
        assert_eq!(
            body(guard).await,
            body(ApiError::new("E_AUTH", "missing Authorization").into_response()).await,
            "guard 与普通 E_AUTH 响应逐字节一致"
        );
        let (_, bytes) = body(ApiError::new("E_AUTH", "x").into_response()).await;
        assert_eq!(bytes, br#"{"error":"E_AUTH"}"#.to_vec(), "body 恰为无 msg 的 E_AUTH");
    }
}
