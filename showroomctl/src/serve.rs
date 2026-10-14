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
use crate::{middleware, ops, publish, setup, streams};

pub const MAX_JSON_BODY: usize = 1024 * 1024;

pub struct AppState {
    pub root: PathBuf,
    /// nonce 查重（SPEC §1）："user:nonce" → 过期时刻；惰性清理。
    /// ponytail: 全局 HashSet 语义，非严格 LRU——重启清零可接受（SPEC 明示）
    pub nonces: Mutex<HashMap<String, i64>>,
    /// login-begin 双令牌桶（plan-16 §2.3）：per-user 10 次/10 分 + 全局 120 次/10 分
    ///（滑动窗口时间戳；内存，重启清零；超限同 404 E_AUTH）。
    pub login_rate: Mutex<LoginRate>,
    /// 在途登录态（plan-16 §2.2）：login-begin 存（60s，同 user 覆盖）、login-finish
    /// 一次性消费；重启丢失重试即可（方案明示）。
    pub pending_logins: Mutex<HashMap<String, PendingLogin>>,
    /// 一次性 bind_token（plan-16 §2.2）：token → (user, 过期时刻)；key-bind 消费即删。
    pub bind_tokens: Mutex<HashMap<String, (String, u64)>>,
    // 钥匙不缓存：Linux 页缓存使 .pub 读取近乎零开销，且缓存失效时序与 key 操作耦合脆弱
    /// pw-begin 在途注册（plan-16 §2.2）：user → 过期时刻；pw-commit 一次性消费（60s）。
    pub pending_pw: Mutex<HashMap<String, u64>>,
}
type S = Arc<AppState>;

/// 滑动窗口双令牌桶（login-begin 专用；纯内存）。
pub struct LoginRate {
    per_user: HashMap<String, Vec<u64>>,
    global: Vec<u64>,
}

/// 窗口（秒）与双桶容量（plan-16 §2.3：user 10 次/10 分 + 全局 120 次/10 分）。
pub const LOGIN_RATE_WINDOW_SECS: u64 = 600;
pub const LOGIN_RATE_PER_USER: usize = 10;
pub const LOGIN_RATE_GLOBAL: usize = 120;

impl LoginRate {
    pub fn new() -> Self {
        LoginRate { per_user: HashMap::new(), global: Vec::new() }
    }

    /// 判定并记账：窗口外时间戳先修剪，两桶任一满即拒（不记账）。
    pub fn allow(&mut self, user: &str, now: u64) -> bool {
        let cutoff = now.saturating_sub(LOGIN_RATE_WINDOW_SECS);
        let bucket = self.per_user.entry(user.to_string()).or_default();
        bucket.retain(|t| *t > cutoff);
        self.global.retain(|t| *t > cutoff);
        if bucket.len() >= LOGIN_RATE_PER_USER || self.global.len() >= LOGIN_RATE_GLOBAL {
            return false;
        }
        bucket.push(now);
        self.global.push(now);
        true
    }
}

impl Default for LoginRate {
    fn default() -> Self {
        Self::new()
    }
}

/// login-begin 存下的服务端登录状态（序列化字节 + 过期时刻）。
pub struct PendingLogin {
    pub state: Vec<u8>,
    pub expiry: u64,
}

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
    // R4 账号级 admin：启动即迁移旧 kid 标记（先于任何鉴权判定，防首请求卡死）
    state::migrate_admin_markers(&root);
    // 平台 RSA 缺失自动生成（首次）
    if !root.join("state/platform_rsa").exists() {
        let p = ops::init_keys(&root)?;
        eprintln!("showroomctl: platform rsa generated at {}", p.display());
    }
    // 服务端 OPRF key 缺失自动生成（首次；plan-16 §1.2 同 platform_rsa 模式）
    if !state::opaque_key_path(&root).exists() {
        ops::opaque_setup(&root)?;
        eprintln!("showroomctl: opaque key generated at {}", state::opaque_key_path(&root).display());
    }
    // owner→user 更名的状态目录迁移（幂等：state/owners/ → state/users/，SPEC §5）
    state::migrate_state_dirs(&root);
    // v1.4 槽位布局迁移（§11，幂等；state 上提 + caddy reload + 容器收养，stderr/日志如实记录）
    for l in ops::migrate_legacy(&root) {
        eprintln!("showroomctl: migrate: {l}");
    }
    // 工具分发源刷新（/_api/tool/*）：平台公钥 + Agent Skill 落 _bootstrap/public/
    //（幂等拷贝；首跑生成的 platform_rsa 此刻同步，Skill 宿主缺源时静默跳过待部署侧补）
    for l in setup::refresh_bootstrap_public(&root) {
        eprintln!("showroomctl: bootstrap-public: {l}");
    }
    // 隧道（C2 合并进 serve）：FRP≠none 时网关+frp 客户端常驻。失败只记日志按 30s 重试，
    // 永不拖累控制面（supervise 内部兜底；serve Restart=always 覆盖进程级故障）。
    tokio::spawn(crate::tunnel::supervise(root.clone()));

    let root_ws = root.clone(); // /_srtp 路由闭包需要 root（srtp_id 注册表查询）
    let st: S = Arc::new(AppState {
        root,
        nonces: Mutex::new(HashMap::new()),
        login_rate: Mutex::new(LoginRate::new()),
        pending_logins: Mutex::new(HashMap::new()),
        bind_tokens: Mutex::new(HashMap::new()),
        pending_pw: Mutex::new(HashMap::new()),
    });
    // /_srtp（plan-18 §3.2）：WS 载波端点——真实路由置于统一 fallback 之前；
    // 内层分发复用同一 Router（SR-SIG1/限流/零信息暴露原样）。
    let inner = Router::new().fallback(api).with_state(st);
    let app = inner
        .clone()
        .route("/_srtp", axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
            let inner = inner.clone();
            let root = root_ws.clone();
            async move { crate::tunnel::srtp_ws::srtp_ws_upgrades(ws, inner, root) }
        }));
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
    // plan-16 §2.2/§2.3：/_api/auth/* 先于常规验签——login-begin/login-finish/key-bind
    // 走密码（OPAQUE）/bind_token 通道；pw-begin/pw-commit 在同前缀下但走常规 SR-SIG1
    //（主 match 处理）；族内其余路径/方法一律 404 E_AUTH（零信息，防子路径枚举）。
    if path.starts_with("/_api/auth/") {
        if matches!((method.as_str(), path),
            ("POST", "/_api/auth/login-begin") | ("POST", "/_api/auth/login-finish") | ("POST", "/_api/auth/key-bind"))
        {
            let r = auth_family(s, method.as_str(), path, &uri_str(uri), sha, headers, collected).await?;
            return Ok(r.into_response());
        }
        if !matches!((method.as_str(), path), ("POST", "/_api/auth/pw-begin") | ("POST", "/_api/auth/pw-commit")) {
            return Err(ApiError::new("E_AUTH", format!("auth family: {method} {path}")));
        }
        // pw-begin/pw-commit 落常规验签（下方主 match）
    }
    // 一切端点先验签（含未知路径/方法；SPEC §3 零信息暴露）
    let Verified { user, kid } = auth(s, method.as_str(), &uri_str(uri), sha, headers).await?;
    if matches!(collected, Collected::Overflow) {
        return Err(ApiError::new("E_QUOTA", "body exceeds limit (json 1MB / import IMPORT_MAX_GB)"));
    }
    let q = qmap(uri);
    // —— 工具分发（控制台内下载）：GET /_api/tool/<filename> ——
    // 白名单三件（CLI 二进制 / Agent Skill / 平台公钥），文件在 _bootstrap/public/
    //（setup 升级与服务启动时刷新）。白名单=精确字符串匹配：名单外（含 ../ 穿越形状、
    // 空名字）一律 E_NOTFOUND，与未知端点同形；统一走上方 SR-SIG1 验签（无额外豁免）。
    if method == "GET" {
        if let Some(name) = path.strip_prefix("/_api/tool/") {
            return tool_get(s, name).await;
        }
    }
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
        // plan-17 S1：随身份附带平台公钥 PEM（读 state/platform_rsa.pub；RSA 公钥本就公开，
        // 经签名端点下发零信息暴露；控制台自动缓存 sr.pempub 免手工粘贴，缺文件=空串）
        ("GET", "/_api/whoami", _) => {
            // 心跳续期已统一在 auth() 中处理（每次认证 touch）
            Ok(Json(json!({
            "user": user, "kid": kid,
            "admin": state::user_is_admin(&s.root, &user),
            "platform_pem": std::fs::read_to_string(s.root.join("state/platform_rsa.pub")).unwrap_or_default(),
        })))
        }

        // —— 登录后信任通道下发 registry 凭据（plan-16 §2.2 末段）：CLI login 免手工 config；
        //     暴露面=已绑定钥匙=账号主人（服务端 reg.cred 本就明文常驻，方案论证）——
        ("GET", "/_api/reg-cred", _) => {
            let root = s.root.clone();
            let o = user.clone();
            let (ru, rp) = blk(move || {
                let cred = std::fs::read_to_string(state::users_dir(&root, &o).join("reg.cred"))
                    .map_err(|e| ApiError::state(format!("reg.cred 读取失败（user 未必有 registry 凭据）: {e}")))?;
                let (u, p) = cred.trim().split_once(':')
                    .ok_or_else(|| ApiError::state("reg.cred 格式异常（须 user:password）"))?;
                Ok::<_, ApiError>((u.to_string(), p.to_string()))
            })
            .await?;
            Ok(Json(json!({"user": ru, "registry_password": rp})))
        }

        // —— 自助下线（plan-16 §2.2）：只能删除请求签名所用 kid 自己（.pub/.bound/.admin 一并清）——
        ("POST", "/_api/key-selfrm", Collected::Mem(b)) => {
            let req: KidReq = jbody(b)?;
            if req.kid != kid {
                return Err(ApiError::new(
                    "E_AUTH",
                    format!("key-selfrm 仅允许删除请求签名所用钥匙自身（签名 kid={kid}，请求 kid={}）", req.kid),
                ));
            }
            let root = s.root.clone();
            let o = user.clone();
            blk(move || ops::user_key_rm(&root, &o, &req.kid)).await?;
            Ok(Json(json!({"ok": true})))
        }

        // —— 改密码（登录态，旧密码免输——已绑定钥匙即会话证明；plan-16 §2.2）——
        ("POST", "/_api/auth/pw-begin", Collected::Mem(b)) => {
            let req: PwBeginReq = jbody(b)?;
            if req.user != user {
                return Err(ApiError::new("E_NAME", format!("只能改自己的密码（签名者 {user}）")));
            }
            let setup = ops::opaque_setup(&s.root)?;
            let m2 = common::opaque::server_reg_start(&setup, &req.m1, &user)
                .map_err(|e| ApiError::state(format!("pw-begin: {e}")))?;
            // 在途注册态（60s，pw-commit 一次性消费；服务重启即失效重试）
            let mut pw = s.pending_pw.lock().unwrap();
            pw.retain(|_, exp| *exp > state::unix_now());
            pw.insert(user.clone(), state::unix_now() + 60);
            Ok(Json(json!({"m2": m2})))
        }

        ("POST", "/_api/auth/pw-commit", Collected::Mem(b)) => {
            let req: PwCommitReq = jbody(b)?;
            if req.user != user {
                return Err(ApiError::new("E_NAME", format!("只能改自己的密码（签名者 {user}）")));
            }
            // 60s 内 pw-begin 过（一次性消费）
            let began = s
                .pending_pw
                .lock()
                .unwrap()
                .remove(&user)
                .is_some_and(|exp| exp > state::unix_now());
            if !began {
                return Err(ApiError::state("pw-begin 会话缺失或已过期（请重试改密码流程）"));
            }
            // 原子替换记录（b64 规范化在 common 侧验证）
            let record = common::opaque::server_reg_canonical(&req.record)
                .map_err(|e| ApiError::state(format!("pw-commit: {e}")))?;
            state::write_private(&state::pass_opaque_path(&s.root, &user), &record)?;
            Ok(Json(json!({"ok": true})))
        }

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
            // plan-16 §1.5 + B7 统一账号密码认证：发卡必须设初始密码（password 或 gen_password）
            let init_pw = match (req.password, req.gen_password.unwrap_or(false)) {
                (Some(p), _) => ops::InitPw::Set(p),
                (None, true) => ops::InitPw::Gen,
                (None, false) => {
                    return Err(ApiError::new(
                        "E_NAME",
                        format!("缺少初始密码：password 与 gen_password 至少其一（示例：{{\"user\":\"{}\",\"gen_password\":true,\"admin\":false}}）", req.user),
                    ));
                }
            };
            let root = s.root.clone();
            let added = blk(move || ops::user_add(&root, &req.user, req.admin.unwrap_or(false), init_pw))
                .await?;
            // 两把密码各只此一次返回（plan-16 §1.3：分开发放；本机 reg.cred 常驻不再可查）
            let mut v = json!({"ok": true, "registry_password": added.password});
            if let Some(lp) = added.login_password {
                v["login_password"] = json!(lp);
            }
            Ok(Json(v))
        }

        // —— 登录密码补发/重置（plan-16 §1.5）：一次性返回新密码；不动既有钥匙 ——
        ("POST", "/_api/admin/passwd-set", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body（同上）
            let req: AdminPasswdSetReq = jbody(b)?;
            let pw = match (req.password, req.gen.unwrap_or(false)) {
                (Some(p), _) => p,
                (None, true) => ops::gen_login_password(),
                (None, false) => {
                    return Err(ApiError::new("E_NAME", "password 或 gen:true 至少其一"));
                }
            };
            let root = s.root.clone();
            let pwc = pw.clone();
            blk(move || ops::user_passwd_set(&root, &req.user, &pwc)).await?;
            // 密码只此一次返回
            Ok(Json(json!({"ok": true, "password": pw})))
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

        ("POST", "/_api/admin/user-admin", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body：非管理员不因 body 畸形泄露 E_NAME/E_AUTH 差异
            let req: AdminUserAdminReq = jbody(b)?;
            let root = s.root.clone();
            blk(move || ops::user_set_admin(&root, &req.user, req.admin)).await?;
            Ok(Json(json!({"ok": true})))
        }
        // R5 D2：旧 key-admin 兼容垫片路由已移除（web 已同步走 user-admin；admin 恒为账号级）

        // R5 E2：全部项目（管理页「全部项目」面板）——admin_guard + ops::ps 全量，
        // 每行附 owner=user（to_json 自带）；/_api/projects 仍只回本人（32c3484 裁决不回退）
        ("GET", "/_api/admin/projects", _) => {
            admin_guard(s, &user, &kid)?;
            let root = s.root.clone();
            let v = blk(move || {
                let disabled = middleware::disabled_middleware(&root);
                let v: Vec<_> = ops::ps(&root)
                    .into_iter()
                    .map(|(p, c, st)| {
                        let mut row = c.to_json(&p, &st);
                        if !disabled.is_empty() {
                            row["middleware_disabled"] = json!(disabled);
                        }
                        row
                    })
                    .collect();
                Ok(Json(json!({"projects": v})))
            })
            .await?;
            Ok(v)
        }

        ("GET", "/_api/admin/status", _) => {
            admin_guard(s, &user, &kid)?;
            let root = s.root.clone();
            let status = blk(move || ops::admin_status(&root)).await?;
            Ok(Json(status))
        }

        // —— 主机期望状态（plan-15 §4）：GET 视图 / POST 两段式（diff→confirm 落盘+apply）——

        ("GET", "/_api/admin/host", _) => {
            admin_guard(s, &user, &kid)?;
            let root = s.root.clone();
            let v = blk(move || middleware::host_view(&root)).await?;
            Ok(Json(v))
        }

        ("POST", "/_api/admin/host", Collected::Mem(b)) => {
            admin_guard(s, &user, &kid)?; // 先守卫后解析 body：非管理员不因 body 畸形泄露差异
            let root = s.root.clone();
            let b = b.clone();
            let v = blk(move || admin_host_post(&root, &b)).await?;
            Ok(Json(v))
        }

        ("GET", "/_api/projects", _) => {
            let root = s.root.clone();
            blk(move || {
                // 「我的项目」恒只回本人项目（R4 用户裁决；亦符合零信息暴露——跨用户项目名/
                // 账号名不再对普通登录者枚举）；管理员跨项目视图归管理页
                // §15.2：禁用中间件的行附 middleware_disabled 标注「未启用」
                let disabled = middleware::disabled_middleware(&root);
                let v: Vec<_> = ops::ps(&root)
                    .into_iter()
                    .filter(|(_, c, _)| c.user == user)
                    .map(|(p, c, st)| {
                        let mut row = c.to_json(&p, &st);
                        if !disabled.is_empty() {
                            row["middleware_disabled"] = json!(disabled);
                        }
                        row
                    })
                    .collect();
                Ok(Json(json!({"projects": v})))
            })
            .await
        }

        // —— 镜像列表（SPEC §13.1，部署表单 ComboBox）：catalog+tags 展平；
        //     按签名用户 `<user>/` 前缀过滤；admin 钥匙不过滤 ——
        ("GET", "/_api/images", _) => {
            let admin = state::user_is_admin(&s.root, &user);
            let root = s.root.clone();
            let o = user.clone();
            let images = blk(move || ops::images(&root, if admin { None } else { Some(&o) })).await?;
            Ok(Json(json!({"images": images})))
        }

        // —— 磁盘空间与镜像删除（SPEC §14）：space 无归属校验（验签即可，全员可看全局磁盘）——

        ("GET", "/_api/space", _) => {
            let root = s.root.clone();
            let v = blk(move || ops::space_snapshot(&root)).await?;
            Ok(Json(v))
        }

        ("POST", "/_api/image-rm", Collected::Mem(b)) => {
            let req: ImageRmReq = jbody(b)?;
            let (repo, tag) = image_guard(s, &req.image, &user)?; // 先守卫后进内核
            let root = s.root.clone();
            let freed = blk(move || ops::image_rm(&root, &repo, &tag)).await?;
            Ok(Json(json!({"freed_est_mb": freed})))
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
            "/_api/stop" | "/_api/start" | "/_api/rm",
            Collected::Mem(b),
        ) => {
            let req: ProjectReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            user_check(&s.root, &req.project, &user)?;
            let act: &'static str = match path {
                "/_api/stop" => "stop",
                "/_api/start" => "start",
                _ => "rm",
            };
            let root = s.root.clone();
            let p = req.project;
            blk(move || match act {
                "stop" => ops::stop(&root, &p),
                "start" => ops::start(&root, &p),
                _ => ops::rm(&root, &p),
            })
            .await?;
            Ok(Json(json!({"ok": true})))
        }

        // —— 回滚（版本选择语义）：{project, version?}；version 缺省 = 上一版本（CLI/旧客户端兼容），
        //     显式 version = 回滚到该版本（按发布历史记录的镜像重部署 + 当前版本置为该版本） ——
        ("POST", "/_api/rollback", Collected::Mem(b)) => {
            let req: RollbackReq = jbody(b)?;
            state::check_name("project", &req.project)?;
            user_check(&s.root, &req.project, &user)?;
            let root = s.root.clone();
            let p = req.project;
            let r = blk(move || match &req.version {
                Some(v) => ops::rollback_to(&root, &p, v).map(Some),
                None => ops::rollback(&root, &p).map(Some),
            })
            .await?;
            Ok(match r {
                Some(o) => Json(json!({"ok": true, "url": o.url, "port": o.port})),
                None => Json(json!({"ok": true})),
            })
        }

        // —— 发布历史（回滚对话框数据源）：历史 + 当前版本 + 每条 imagePresent（registry 存在性） ——
        ("GET", "/_api/versions", _) => {
            let project = qget(&q, "project")?;
            state::check_name("project", &project)?;
            user_check(&s.root, &project, &user)?;
            let root = s.root.clone();
            let v = blk(move || ops::versions_of(&root, &project)).await?;
            Ok(Json(v))
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
            publish::check_ownership(&s.root, &req.project, &user)?;
            let root = s.root.clone();
            let o = user.clone();
            let (id, tag) = blk(move || {
                publish::import_begin(&root, &o, &req.project, req.tag.as_deref(), &req.image_id, req.total_bytes)
            })
            .await?;
            Ok(Json(json!({"upload_id": id, "tag": tag})))
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
            let r = blk(move || dbmaint::sql(&root, &p, &sql, confirm)).await?;
            Ok(Json(json!({"out": r.out, "csv": r.csv})))
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
            let admin = state::user_is_admin(&s.root, &user);
            // R8 用户裁决：下拉显示项目名而非库号——附 db→项目映射（管理员全池，未分配库 project=null）
            let own: Vec<(String, u16)> = state::all_confs(&s.root)
                .into_iter()
                .filter(|(_, c)| admin || c.user == user)
                .map(|(p, c)| (p, c.redisdb as u16))
                .collect();
            let dbs: Vec<u16> = if admin {
                (0..policy.redis_pool_cap()).collect()
            } else {
                state::user_redis_dbs(&s.root, &user)
            };
            let entries: Vec<_> = dbs.iter().map(|db| {
                let project = own.iter().find(|(_, d)| d == db).map(|(p, _)| p.clone());
                json!({"db": db, "project": project})
            }).collect();
            let mut v = json!({"dbs": dbs, "entries": entries});
            if admin {
                v["admin_view"] = json!(true);
            }
            Ok(Json(v))
        }

        ("GET", "/_api/redis-keys", _) => {
            let db = redis_db_guard(s, &q, &user)?;
            let pat = q.get("q").cloned().unwrap_or_default();
            // 默认 100 上限 200（--scan 无 limit 参数，内核读满即杀并标截断）
            let limit: usize = q.get("limit").and_then(|t| t.parse().ok()).unwrap_or(100).min(200);
            let (keys, truncated) = blk(move || ops::redis_keys(db, &pat, limit)).await?;
            Ok(Json(json!({"keys": keys, "truncated": truncated})))
        }

        ("GET", "/_api/redis-get", _) => {
            let db = redis_db_guard(s, &q, &user)?;
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
            let (env, disabled) = blk(move || {
                Ok((ops::env_of(&root, &project)?, middleware::disabled_middleware(&root)))
            })
            .await?;
            let mut v = json!({"env": env});
            if !disabled.is_empty() {
                v["middleware_disabled"] = json!(disabled); // §15.2 标注「未启用」
            }
            Ok(Json(v))
        }

        _ => Err(ApiError::new("E_NOTFOUND", "no such endpoint")),
    };
    r.map(|j| j.into_response())
}

// —— 工具分发（GET /_api/tool/<filename>）：控制台内下载 CLI / Skill / 平台公钥 ——

/// /_api/tool 白名单（精确字符串匹配；路径 = _bootstrap/public/<filename>）。
pub const TOOL_WHITELIST: [&str; 3] = ["showroom", "showroom-publisher.md", "platform_rsa.pub"];

/// 白名单内文件全量读取返回（showroom 二进制 ~5MB 内可接受，免流式分片）；
/// Content-Type 按扩展名：无扩展名（二进制）→ octet-stream、.md → text/markdown、.pub → text/plain。
/// 缺文件 → E_NOTFOUND（验签后的如实报错：提示部署侧未刷新 _bootstrap/public）。
async fn tool_get(s: &S, name: &str) -> Result<Response, ApiError> {
    let name = name.to_string(); // blk 闭包需 'static：先收所有权
    if !TOOL_WHITELIST.contains(&name.as_str()) {
        return Err(ApiError::new("E_NOTFOUND", format!("no such tool: {name}")));
    }
    let ct = if name.ends_with(".md") {
        "text/markdown; charset=utf-8"
    } else if name.ends_with(".pub") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let p = s.root.join("_bootstrap/public").join(&name);
    let bytes = blk(move || {
        std::fs::read(&p).map_err(|e| ApiError::new("E_NOTFOUND", format!("tool '{name}' 读取失败（_bootstrap/public 未刷新？）: {e}")))
    })
    .await?;
    let resp = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, ct)
        .header(axum::http::header::CONTENT_LENGTH, bytes.len().to_string())
        .body(Body::from(bytes))
        .map_err(|e| ApiError::state(e.to_string()))?;
    Ok(resp)
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

// —— /_api/auth/* 免签家族（plan-16 §2.2：login-begin / login-finish / key-bind）——
//
// 零信息语义（§2.3）：畸形 body / 限流超限 / ke3 拒绝 / bind_token 无效一律
// 404 {"error":"E_AUTH"}（经 IntoResponse 的 E_AUTH 同体惯例），与未知路径不可区分；
// 未知用户/未设密码 → login-begin 走 crate 原生 dummy 假记录路径仍 200 ke2（防枚举，
// 客户端本地 finish 必失败）；密码错误在客户端本地判（不产生第三请求）。

async fn auth_family(
    s: &S,
    method: &str,
    path: &str,
    uri: &str,
    sha: &str,
    headers: &HeaderMap,
    collected: &Collected,
) -> Result<Json<serde_json::Value>, ApiError> {
    debug_assert!(matches!(
        (method, path),
        ("POST", "/_api/auth/login-begin") | ("POST", "/_api/auth/login-finish") | ("POST", "/_api/auth/key-bind")
    ));
    let eauth = |msg: &str| ApiError::new("E_AUTH", msg);
    let Collected::Mem(b) = collected else {
        return Err(eauth("auth family: body not in memory"));
    };
    let now = state::unix_now();
    match path {
        // 第一步：ke1 → ke2（未知用户同形 dummy；限流双桶）
        "/_api/auth/login-begin" => {
            let req: LoginBeginReq = jbody(b).map_err(|e| eauth(&format!("login-begin body: {e}")))?;
            if !showroom_common::names::valid_aux(&req.user) || req.ke1.is_empty() {
                return Err(eauth("login-begin: bad user or empty ke1"));
            }
            if !s.login_rate.lock().unwrap().allow(&req.user, now) {
                return Err(eauth("login-begin: rate limited"));
            }
            let setup = ops::opaque_setup(&s.root)?;
            let record = state::read_pass_opaque(&s.root, &req.user)
                .and_then(|b64| common::opaque::server_record_from_b64(&b64).ok());
            let mut rng = rand::rngs::OsRng;
            let (ke2, state_bytes) =
                common::opaque::server_login_start(&mut rng, &setup, record.as_ref(), &req.ke1, &req.user)
                    .map_err(|e| eauth(&format!("login-start: {e}")))?;
            // 在途登录态（60s；同 user 覆盖；过期惰性清理）
            {
                let mut pl = s.pending_logins.lock().unwrap();
                pl.retain(|_, p| p.expiry > now);
                pl.insert(req.user.clone(), PendingLogin { state: state_bytes, expiry: now + 60 });
            }
            Ok(Json(json!({"ke2": ke2})))
        }

        // 第三步：ke3 验证 → 一次性 bind_token（60s）
        "/_api/auth/login-finish" => {
            let req: LoginFinishReq = jbody(b).map_err(|e| eauth(&format!("login-finish body: {e}")))?;
            let pending = s
                .pending_logins
                .lock()
                .unwrap()
                .remove(&req.user) // 一次性消费（ke3 不可重放）
                .filter(|p| p.expiry > now)
                .ok_or_else(|| eauth("login-finish: no pending login (expired or replayed)"))?;
            let session_key = common::opaque::server_login_finish(&pending.state, &req.ke3)
                .map_err(|e| eauth(&format!("ke3 rejected: {e}")))?;
            let token = state::rand_hex(16);
            {
                let mut bt = s.bind_tokens.lock().unwrap();
                bt.retain(|_, (_, exp)| *exp > now);
                bt.insert(token.clone(), (req.user.clone(), now + 60));
            }
            // plan-18 §3.3：SRTP 密钥下发（OPAQUE 绑定）——per-user 包裹公钥 + 新鲜
            // srtp_id + key_mac；客户端以 AKE 会话密钥验签，MITM 换任一字段即失败
            let (srtp_id, srtp_pub) = crate::tunnel::srtpkeys::issue(&s.root, &req.user)
                .map_err(|e| ApiError::state(format!("srtp issue: {}", e.msg)))?;
            let key_mac = common::opaque::srtp_key_mac(&session_key, &srtp_id, &srtp_pub);
            Ok(Json(json!({
                "bind_token": token,
                "srtp_id": hex::encode(srtp_id),
                "srtp_pub": srtp_pub,
                "key_mac": hex::encode(key_mac),
            })))
        }

        // 绑定：bind_token 单次有效 + SR-SIG1 按 body 内公钥验签（候选钥匙，非已登记钥匙）
        "/_api/auth/key-bind" => {
            let req: KeyBindReq = jbody(b).map_err(|e| eauth(&format!("key-bind body: {e}")))?;
            let token = headers
                .get("x-sr-bind")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| eauth("key-bind: missing X-Sr-Bind"))?
                .to_string();
            let token_user = s
                .bind_tokens
                .lock()
                .unwrap()
                .remove(&token) // 单次使用
                .filter(|(_, exp)| *exp > now)
                .map(|(u, _)| u)
                .ok_or_else(|| eauth("key-bind: bad or expired bind_token"))?;
            if token_user != req.user {
                return Err(eauth("key-bind: bind_token belongs to another user"));
            }
            // 验签按 body 内公钥（plan-16 §2.2：服务端按 body 公钥验签而非已登记钥匙）
            let vk = showroom_common::UserKey::from_pub_string(req.pubkey.trim())
                .map_err(|e| eauth(&format!("key-bind pubkey: {e}")))?;
            let authv = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| eauth("key-bind: missing Authorization"))?;
            let v = common::verify_with_hash(authv, &[vk], method, uri, sha, now as i64)
                .map_err(|e| eauth(&format!("key-bind verify: {e}")))?;
            if v.user != req.user {
                return Err(eauth("key-bind: signature user mismatch"));
            }
            let mut m = s.nonces.lock().unwrap();
            if !nonce_fresh(&mut m, &v.user, &v.nonce, now as i64) {
                return Err(eauth("key-bind: nonce replay"));
            }
            let (kid, admin) = ops::key_bind(&s.root, &req.user, &req.pubkey)?;
            Ok(Json(json!({"kid": kid, "admin": admin})))
        }

        _ => Err(eauth("unreachable auth family arm")),
    }
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
    // 定位命中钥匙：verify_with_hash 返回命中索引（Design#4：TTL 检查/touch/Verified.kid 同用
    // 一次验签结果——不再逐把复验三遍；ed25519 验签在 auth 内只发生一轮）
    let kid = keys
        .get(v.key_idx)
        .map(|(k, _)| k.clone())
        .ok_or_else(|| ApiError::new("E_AUTH", "matching key vanished mid-auth"))?;
    // 绑定钥匙 TTL：每次成功认证 = 活跃信号 → touch .bound 续期；
    // 30 分钟无任何认证 → 过期吊销 → E_AUTH（CLI push 的每个 chunk、控制台每次请求都自动续期）
    if !state::bound_key_alive(&s.root, &v.user, &kid) {
        return Err(ApiError::new("E_AUTH", "bound key expired (no activity >30min)"));
    }
    // 每次认证 touch（CLI 每个 API 调用自动续期，长操作 push 的 chunk 天然保活）
    state::touch_bound_key(&s.root, &v.user, &kid);
    let mut m = s.nonces.lock().unwrap();
    if !nonce_fresh(&mut m, &v.user, &v.nonce, now) {
        return Err(ApiError::new("E_AUTH", format!("nonce replay: {}:{}", v.user, v.nonce)));
    }
    Ok(Verified { user: v.user, kid })
}

/// 管理员钥匙校验（SPEC §12.2）：非管理员返回 E_AUTH → 与未登记/验签失败同一 404
/// `{"error":"E_AUTH"}`（IntoResponse 对 E_AUTH 丢弃 msg），不泄露 /_api/admin/* 端点存在。
fn admin_guard(s: &S, user: &str, _kid: &str) -> R<()> {
    if state::user_is_admin(&s.root, user) {
        Ok(())
    } else {
        Err(ApiError::new("E_AUTH", format!("not an admin user: {user}")))
    }
}

/// Redis 浏览归属校验（SPEC §13.2）：请求 db 须 ∈ 签名用户名下项目的 redisdb 集合
/// （state::user_redis_dbs）；admin 钥匙放开全量（0..policy 池）。
/// 非归属 → E_AUTH（404 同体，不泄露池内容；与 user_check 语义一致）。
fn redis_db_guard(s: &S, q: &HashMap<String, String>, user: &str) -> R<u8> {
    let policy = state::Policy::load(&s.root);
    let db = state::parse_redis_db(&qget(q, "db")?, policy.redis_pool_cap())?;
    if state::user_is_admin(&s.root, user) || state::user_redis_dbs(&s.root, user).contains(&(db as u16)) {
        Ok(db)
    } else {
        Err(ApiError::new("E_AUTH", format!("redis db {db} not in '{user}' projects' pool")))
    }
}

/// image-rm 归属校验（SPEC §14.1，仿 redis_db_guard）：`<repo>:<tag>` 形状拆分（无冒号 E_NAME）、
/// repo 白名单与 tag 校验；repo 属签名用户命名空间（`<user>/` 前缀，split 精确比对）或
/// admin 钥匙 → 放行，否则 E_AUTH（404 同体，与未登记钥匙不可区分）。
fn image_guard(s: &S, image: &str, user: &str) -> R<(String, String)> {
    let (repo, tag) = state::split_image_ref(image)
        .ok_or_else(|| ApiError::new("E_NAME", format!("image '{image}' 须形如 <user>/<project>:<tag>")))?;
    if !state::valid_image_repo(&repo) {
        return Err(ApiError::new("E_NAME", format!("bad image repo '{repo}'")));
    }
    publish::check_tag(&tag)?;
    if state::repo_owned_by(&repo, user) || state::user_is_admin(&s.root, user) {
        Ok((repo, tag))
    } else {
        Err(ApiError::new("E_AUTH", format!("image '{image}' not in '{user}' namespace")))
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
    // Redis key 含 `:` 等符号，前端 encodeURIComponent 会编码——form_urlencoded 标准解码
    // （%XX + '+'→空格；B10：原手搓 percent_decode 删除，语义不变）
    form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn qget(m: &HashMap<String, String>, k: &str) -> R<String> {
    m.get(k).cloned().ok_or_else(|| ApiError::new("E_NAME", format!("missing query param '{k}'")))
}

fn jbody<T: serde::de::DeserializeOwned>(b: &[u8]) -> R<T> {
    serde_json::from_slice(b).map_err(|e| ApiError::new("E_NAME", format!("bad json body: {e}")))
}

/// 项目归属校验：conf.user 属主比对；admin 钥匙旁路（第2轮xiaozhi P2：管理员有跨项目管理
/// 需求，服务端应允许而非拒绝——与 redis_db_guard / image_guard 的 admin 放行语义一致）。
/// 非属主且非 admin → E_AUTH（404 同体，与未登记钥匙不可区分）。
fn user_check(root: &Path, project: &str, user: &str) -> R<()> {
    let conf = state::Conf::load(root, project)
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no project '{project}'")))?;
    if conf.user != user && !state::user_is_admin(root, user) {
        return Err(ApiError::new("E_AUTH", format!("project '{project}' belongs to {}", conf.user)));
    }
    Ok(())
}

// —— POST /_api/admin/host（plan-15 §4 两段式）——

/// body：`{"host_env":{"MIDDLEWARE":"postgres registry",…}, "confirm":false|true}`
/// 或第二形态 `{"redis_databases":512,"confirm":…}`（进 §5 扩容状态机）。
/// 无 confirm：写盘前预演——返回 actions(diff 同构)+impact(removed/affected)+confirm_required；
/// confirm=true：HostEnv 落盘 → apply → {"ok":true,"report":…}。
fn admin_host_post(root: &Path, body: &[u8]) -> R<serde_json::Value> {
    let req: AdminHostPostReq = jbody(body)?;
    let confirm = req.confirm.unwrap_or(false);
    if let Some(n) = req.redis_databases {
        return middleware::redis_expand(root, n, confirm);
    }
    let Some(b) = req.host_env else {
        return Err(ApiError::new("E_NAME", "body 须含 host_env 或 redis_databases"));
    };

    let prev = middleware::HostEnv::load(root);
    let mut host = prev.clone();
    if let Some(m) = &b.middleware {
        let mut set = std::collections::BTreeSet::new();
        for name in m.split_whitespace() {
            middleware::validate_mw(name)
                .map_err(|e| ApiError::new("E_NAME", format!("MIDDLEWARE: {}", e.msg)))?;
            set.insert(name.to_string());
        }
        host.middleware = set;
    }
    if let Some(c) = &b.caddy {
        host.caddy = matches!(c.as_str(), "yes");
        if !matches!(c.as_str(), "yes" | "no") {
            return Err(ApiError::new("E_NAME", format!("CADDY 须为 yes/no（got '{c}'）")));
        }
    }
    if let Some(h) = &b.lan_host {
        host.lan_host = h.clone();
    }
    if let Some(h) = &b.public_host {
        host.public_host = h.clone();
    }
    if let Some(t) = &b.tuning {
        host.tuning = matches!(t.as_str(), "sysctl");
        if !matches!(t.as_str(), "sysctl" | "none") {
            return Err(ApiError::new("E_NAME", format!("TUNING 须为 sysctl/none（got '{t}'）")));
        }
    }
    if let Some(d) = &b.dedup {
        host.dedup = matches!(d.as_str(), "weekly");
        if !matches!(d.as_str(), "weekly" | "none") {
            return Err(ApiError::new("E_NAME", format!("DEDUP 须为 weekly/none（got '{d}'）")));
        }
    }
    if let Some(f) = &b.frp {
        host.frp = middleware::FrpMode::parse(f)?;
    }
    if let Some(t) = &b.frp_token {
        if t != "<set>" && !t.is_empty() { host.frp_token = t.clone(); } // P1："<set>"=GET 打码值回传，跳过
    }
    if let Some(t) = &b.frp_tunnel {
        host.frp_tunnel = t.clone();
    }
    if let Some(t) = &b.frp_toml {
        if !t.is_empty() { host.frp_toml_path = t.clone(); } // 值=文件路径（非凭据，无打码回传问题）
    }

    // 落盘前往返校验（评审二轮实锤）：非法组合（如 native 无 FRP_TOML）或含换行/等号
    // 破坏 kv 形状的值，在 parse(to_env()) 往返上必然暴露——杜绝「落盘即解析失败→
    // 静默回退默认形状→隧道悄悄停摆」的链路。预演分支同样被拦（不允许预演非法状态）。
    let roundtrip = middleware::HostEnv::parse(&host.to_env())
        .map_err(|e| ApiError::new("E_NAME", format!("host.env 组合校验失败: {}", e.msg)))?;
    if roundtrip != host {
        return Err(ApiError::new("E_NAME", "host.env 值无法安全落盘（含换行等会破坏单行 kv 的字符）"));
    }

    let removed: Vec<String> = prev.middleware.difference(&host.middleware).cloned().collect();
    let mut affected: Vec<String> = vec![];
    for mw in &removed {
        for p in middleware::mw_dependents(root, mw) {
            if !affected.contains(&p) {
                affected.push(p);
            }
        }
    }

    if !confirm {
        // 预演：不落盘（host.env 字节不变），返回 diff + impact
        let obs = setup::observe(root, &host);
        let actions = setup::plan_setup(&host, &obs);
        let warnings = setup::plan_warnings(root, &obs);
        let mut v = setup::diff_json(&host, &actions, &warnings);
        v["impact"] = json!({
            "removed_middleware": removed,
            "affected_projects": affected,
        });
        v["confirm_required"] = json!(true);
        return Ok(v);
    }
    host.save(root)?;
    let obs = setup::observe(root, &host);
    let actions = setup::plan_setup(&host, &obs);
    let report = setup::apply(root, &actions, false)?;
    Ok(json!({"ok": true, "report": setup::report_json(&report)}))
}

#[derive(Deserialize)]
struct AdminHostPostReq {
    host_env: Option<AdminHostEnvBody>,
    redis_databases: Option<u16>,
    confirm: Option<bool>,
}

#[derive(Deserialize)]
struct AdminHostEnvBody {
    #[serde(rename = "MIDDLEWARE", alias = "middleware")]
    middleware: Option<String>,
    #[serde(rename = "CADDY", alias = "caddy")]
    caddy: Option<String>,
    #[serde(rename = "LAN_HOST", alias = "lan_host")]
    lan_host: Option<String>,
    #[serde(rename = "PUBLIC_HOST", alias = "public_host")]
    public_host: Option<String>,
    #[serde(rename = "TUNING", alias = "tuning")]
    tuning: Option<String>,
    #[serde(rename = "DEDUP", alias = "dedup")]
    dedup: Option<String>,
    #[serde(rename = "FRP", alias = "frp")]
    frp: Option<String>,
    #[serde(rename = "FRP_TOKEN", alias = "frp_token")]
    frp_token: Option<String>,
    #[serde(rename = "FRP_TUNNEL", alias = "frp_tunnel")]
    frp_tunnel: Option<String>,
    #[serde(rename = "FRP_TOML", alias = "frp_toml")]
    frp_toml: Option<String>,
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

/// POST /_api/rollback body：`{"project":p,"version":"v2"}`；version 缺省 = 上一版本（CLI 兼容）。
#[derive(Deserialize)]
struct RollbackReq {
    project: String,
    version: Option<String>,
}

#[derive(Deserialize)]
struct IdReq {
    image_id: String,
}

/// POST /_api/image-rm body：`{"image":"<repo>:<tag>"}`（如 "alice/demo:v1"，SPEC §14.1）。
#[derive(Deserialize)]
struct ImageRmReq {
    image: String,
}

#[derive(Deserialize)]
struct BeginReq {
    project: String,
    /// 镜像 tag（镜像库内身份）。控制台已不收集版本——None → 服务端生成 u<8hex>。
    tag: Option<String>,
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
    admin: Option<bool>,
    /// 初始登录密码（明示）或自动生成（gen_password:true）——B7 后必填其一（统一账号密码认证）。
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    gen_password: Option<bool>,
}

/// POST /_api/admin/passwd-set body（plan-16 §1.5）：`{"user":u,"password":pw}` 或
/// `{"user":u,"gen":true}` → 响应 `{password}`（一次性）。
#[derive(Deserialize)]
struct AdminPasswdSetReq {
    user: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    gen: Option<bool>,
}

/// POST /_api/auth/login-begin body（plan-16 §2.2）：`{"user":u,"ke1":b64}`。
#[derive(Deserialize)]
struct LoginBeginReq {
    user: String,
    ke1: String,
}

/// POST /_api/auth/login-finish body：`{"user":u,"ke3":b64}`。
#[derive(Deserialize)]
struct LoginFinishReq {
    user: String,
    ke3: String,
}

/// POST /_api/auth/key-bind body：`{"user":u,"pubkey":"ed25519:…"}` + X-Sr-Bind 头。
#[derive(Deserialize)]
struct KeyBindReq {
    user: String,
    pubkey: String,
}

/// POST /_api/auth/pw-begin body（登录态改密码第一步）：`{"user":u,"m1":b64}`。
#[derive(Deserialize)]
struct PwBeginReq {
    user: String,
    m1: String,
}

/// POST /_api/auth/pw-commit body：`{"user":u,"record":b64}`（原子替换记录）。
#[derive(Deserialize)]
struct PwCommitReq {
    user: String,
    record: String,
}

/// POST /_api/key-selfrm body：`{"kid":k}`（仅允许签名所用 kid 自己）。
#[derive(Deserialize)]
struct KidReq {
    kid: String,
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
struct AdminUserAdminReq {
    user: String,
    admin: bool,
}


#[cfg(test)]
mod tests {
    /// 测试用 AppState（plan-16 新字段全零值）。
    fn test_state(root: PathBuf) -> AppState {
        AppState {
            root,
            nonces: Mutex::new(HashMap::new()),
            login_rate: Mutex::new(LoginRate::new()),
            pending_logins: Mutex::new(HashMap::new()),
            bind_tokens: Mutex::new(HashMap::new()),
            pending_pw: Mutex::new(HashMap::new()),
        }
    }

    /// 直调 api_inner（免网络）：返回 (status, body bytes)。
    async fn call(s: &S, method: &str, path: &str, body: &[u8], headers: &[(&str, &str)]) -> (u16, Vec<u8>) {
        let uri: axum::http::Uri = path.parse().unwrap();
        let mut hm = HeaderMap::new();
        for (k, v) in headers {
            hm.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        let r = api_inner(
            s,
            &Method::from_bytes(method.as_bytes()).unwrap(),
            &OriginalUri(uri),
            &hm,
            Body::from(body.to_vec()),
        )
        .await
        .into_response();
        let (parts, b) = r.into_parts();
        let bytes = axum::body::to_bytes(b, 1 << 20).await.unwrap().to_vec();
        (parts.status.as_u16(), bytes)
    }

    /// 走完整 OPAQUE 客户端拿到 bind_token（login→login-finish），返回 (bind_token, 候选钥匙)。
    async fn full_login(s: &S, user: &str, pw: &str) -> (String, showroom_common::UserKey) {
        let mut rng = rand::rngs::OsRng;
        let (st, ke1) = showroom_common::opaque::login_start(&mut rng, pw, user).unwrap();
        let (code, body) = call(
            s,
            "POST",
            "/_api/auth/login-begin",
            serde_json::to_vec(&json!({"user": user, "ke1": ke1})).unwrap().as_slice(),
            &[],
        )
        .await;
        assert_eq!(code, 200, "login-begin: {}", String::from_utf8_lossy(&body));
        let ke2 = serde_json::from_slice::<Value>(&body).unwrap()["ke2"].as_str().unwrap().to_string();
        let (ke3, _sk) = showroom_common::opaque::login_finish(st, pw, user, &ke2).unwrap();
        let (code, body) = call(
            s,
            "POST",
            "/_api/auth/login-finish",
            serde_json::to_vec(&json!({"user": user, "ke3": ke3})).unwrap().as_slice(),
            &[],
        )
        .await;
        assert_eq!(code, 200, "login-finish: {}", String::from_utf8_lossy(&body));
        let token = serde_json::from_slice::<Value>(&body).unwrap()["bind_token"].as_str().unwrap().to_string();
        (token, showroom_common::UserKey::generate())
    }

    /// 生成一个已设密码用户（进程内注册），返回 root。
    fn mkuser(root: &Path, user: &str, pw: &str) {
        crate::ops::user_add(root, user, false, crate::ops::InitPw::Set(pw.into())).unwrap();
    }

    /// B7 语义：发卡（恒设密码）→ 登录绑定钥匙。admin=账号属性（首把钥匙继承）。返回 (kid, 是否 admin)。
    fn mk_key_user(root: &Path, user: &str, admin: bool) -> (String, bool) {
        crate::ops::user_add(root, user, admin, crate::ops::InitPw::Gen).unwrap();
        let k = showroom_common::UserKey::generate();
        crate::ops::key_bind(root, user, &k.pub_string()).unwrap()
    }

    /// 直接落一把公钥（模拟存量预登记/遗留钥匙，无 pass.opaque），返回 kid。
    fn put_key_legacy(root: &Path, user: &str, k: &showroom_common::UserKey) -> String {
        let vk = showroom_common::UserKey::from_pub_string(&k.pub_string()).unwrap();
        let kid = crate::state::kid_of(&vk);
        let dir = crate::state::tokens_dir(root, user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{kid}.pub")), format!("{}\n", k.pub_string())).unwrap();
        kid
    }

    fn troot(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录（pid+原子计数），固定名复用会互相 remove_dir_all
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "srctl-serve-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
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
        assert_eq!(v.key_idx, 0, "Design#4：命中钥匙索引随验签返回（auth 据此定位 kid 免复验）");

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
        let root = troot("guard");
        let s: S = Arc::new(test_state(root.clone()));

        let (admin_kid, a) = mk_key_user(&root, "root", true);
        assert!(a, "管理员账号的 key-bind 回传 admin=true（R4 账号属性语义）");
        assert!(admin_guard(&s, "root", &admin_kid).is_ok());

        let (plain_kid, a2) = mk_key_user(&root, "carl", false);
        assert!(!a2);
        assert_eq!(admin_guard(&s, "carl", &plain_kid).unwrap_err().code, "E_AUTH");
        assert_eq!(admin_guard(&s, "nobody", &admin_kid).unwrap_err().code, "E_AUTH");
        // R4 账号级：管理员账号任意有效钥匙放行（kid 不再参与判定）
        assert!(admin_guard(&s, "root", "ffffffff").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    // redis_db_guard 边界（§13.2）：自己的 db 放行；别人的/未分配的 → E_AUTH（404 同体）；
    // admin 钥匙放开全量；形状非法（非数字/超池）→ E_NAME
    #[test]
    fn redis_db_guard_boundaries() {
        let root = troot("redis-guard");
        let s: S = Arc::new(test_state(root.clone()));

        // alice 名下项目占 db 3/7；bob 占 db 2
        for (p, user, db) in [("demo1", "alice", 3u8), ("demo2", "alice", 7), ("demo3", "bob", 2)] {
            let c = crate::state::Conf {
                user: user.into(), tag: "v1".into(), port: 8100, cport: 8080,
                mem: "256m".into(), cpu: 1.0, redisdb: db, updated: 1,
                stopped: false, legacyslot: None,
                version: String::new(),
                version_seq: 0,
            };
            c.save(&root, p).unwrap();
        }
        let qq = |db: &str| HashMap::from([("db".to_string(), db.to_string())]);

        // 自己的 db 放行；别人的（bob 的 2）/未分配的（200）→ E_AUTH
        assert_eq!(redis_db_guard(&s, &qq("3"), "alice").unwrap(), 3);
        assert_eq!(redis_db_guard(&s, &qq("7"), "alice").unwrap(), 7);
        assert_eq!(redis_db_guard(&s, &qq("2"), "alice").unwrap_err().code, "E_AUTH");
        assert_eq!(redis_db_guard(&s, &qq("200"), "alice").unwrap_err().code, "E_AUTH");
        assert_eq!(redis_db_guard(&s, &qq("3"), "nobody").unwrap_err().code, "E_AUTH");

        // admin 钥匙放开全量（含别人的 2 与池内任意 255）；超池/非数字仍 E_NAME
        let (_admin_kid, _) = mk_key_user(&root, "root", true);
        assert_eq!(redis_db_guard(&s, &qq("2"), "root").unwrap(), 2);
        assert_eq!(redis_db_guard(&s, &qq("255"), "root").unwrap(), 255);
        assert_eq!(redis_db_guard(&s, &qq("256"), "root").unwrap_err().code, "E_NAME");
        assert_eq!(redis_db_guard(&s, &qq("abc"), "root").unwrap_err().code, "E_NAME");
        assert_eq!(redis_db_guard(&s, &HashMap::new(), "root").unwrap_err().code, "E_NAME");

        // 非管理员账号的钥匙不越权（无 E_AUTH 与 404 之外的信息差）
        let (_plain_kid, _) = mk_key_user(&root, "carl", false);
        assert_eq!(redis_db_guard(&s, &qq("3"), "carl").unwrap_err().code, "E_AUTH");

        // 小池（REDIS_DBS=8）：池内 7 放行、池外 15 → E_NAME（alice 名下 db 7 < 8 仍可用）
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("state/policy.env"), "REDIS_DBS=8\n").unwrap();
        assert_eq!(redis_db_guard(&s, &qq("7"), "alice").unwrap(), 7);
        assert_eq!(redis_db_guard(&s, &qq("15"), "root").unwrap_err().code, "E_NAME");
        let _ = std::fs::remove_dir_all(&root);
    }

    // image_guard 边界（§14.1，仿 redis_db_guard_boundaries）：本人 repo 放行；
    // 他人 repo/无命名空间 → E_AUTH（404 同体）；admin 钥匙放行他人；形状非法 → E_NAME
    #[test]
    fn image_guard_boundaries() {
        let root = troot("image-guard");
        let s: S = Arc::new(test_state(root.clone()));

        // 本人命名空间：拆分出 (repo, tag) 放行
        assert_eq!(
            image_guard(&s, "alice/demo:v1", "alice").unwrap(),
            ("alice/demo".into(), "v1".into())
        );
        // 他人 repo / 无命名空间 repo → E_AUTH（404 同体）
        assert_eq!(image_guard(&s, "bob/demo:v1", "alice").unwrap_err().code, "E_AUTH");
        assert_eq!(image_guard(&s, "bare:v1", "alice").unwrap_err().code, "E_AUTH");
        // 前缀不误吞："alice-x/…" 不属于 alice
        assert_eq!(image_guard(&s, "alice-x/demo:v1", "alice").unwrap_err().code, "E_AUTH");
        // 无 user 档案的签名者同样 E_AUTH
        assert_eq!(image_guard(&s, "alice/demo:v1", "nobody").unwrap_err().code, "E_AUTH");

        // admin 钥匙放行他人命名空间
        let (_admin_kid, _) = mk_key_user(&root, "root", true);
        assert_eq!(
            image_guard(&s, "bob/demo:v1", "root").unwrap(),
            ("bob/demo".into(), "v1".into())
        );

        // 形状非法 → E_NAME：无冒号 / repo 白名单外（空格、..、大写、三段）/ tag 非法
        for bad in [
            "alice/demo", "a b/x:v", "../x:v", "A/b:v", "a/b/c:v", "alice/demo:a b", "alice/demo:",
        ] {
            assert_eq!(image_guard(&s, bad, "alice").unwrap_err().code, "E_NAME", "{bad}");
        }
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
            &Arc::new(test_state(PathBuf::from("/nonexistent"))),
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

    // POST /_api/admin/host 两段式（plan-15 §8 serve 层）：无 confirm 不落盘且返回 impact；
    // schema 坏值 E_NAME；host/redis 两形态分发
    #[tokio::test]
    async fn admin_host_post_two_phase_without_confirm() {
        let root = troot("host-post");
        std::fs::create_dir_all(root.join("state/projects")).unwrap();
        // 现状：全集中间件 + 两个项目（redis 依赖演示）
        crate::middleware::HostEnv::default_for("10.0.0.1").save(&root).unwrap();
        for (p, db) in [("demo1", 0u8), ("demo2", 1u8)] {
            let c = crate::state::Conf {
                user: "alice".into(), tag: "v1".into(), port: 8100, cport: 8080,
                mem: "256m".into(), cpu: 1.0, redisdb: db, updated: 1,
                stopped: false, legacyslot: None,
                version: String::new(),
                version_seq: 0,
            };
            c.save(&root, p).unwrap();
        }
        let before = std::fs::read(crate::middleware::host_env_path(&root)).unwrap();

        let v = admin_host_post(&root, br#"{"host_env":{"MIDDLEWARE":"postgres registry"},"confirm":false}"#)
            .unwrap();
        // 不落盘：字节不变
        assert_eq!(std::fs::read(crate::middleware::host_env_path(&root)).unwrap(), before, "无 confirm 不落盘");
        assert_eq!(v["confirm_required"], true);
        assert_eq!(v["impact"]["removed_middleware"], serde_json::json!(["redis"]));
        let affected = v["impact"]["affected_projects"].as_array().unwrap().clone();
        assert!(affected.iter().any(|x| x == "demo1") && affected.iter().any(|x| x == "demo2"), "{affected:?}");
        // 预演返回 diff 同构 actions（含将写的 host.env）
        assert!(v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["target"].as_str().is_some_and(|t| t.ends_with("host.env"))));

        // schema 校验：坏中间件名 / 坏 CADDY → E_NAME；空 body → E_NAME
        assert_eq!(
            admin_host_post(&root, br#"{"host_env":{"MIDDLEWARE":"mysql"}}"#).unwrap_err().code,
            "E_NAME"
        );
        // 落盘前往返校验（评审二轮）：FRP=native 缺 FRP_TOML → E_NAME（否则落盘即解析失败
        // → load 静默回退默认 → 隧道悄悄停摆）；值含换行（kv 注入）同样拒绝
        let e = admin_host_post(&root, br#"{"host_env":{"FRP":"native"}}"#).unwrap_err();
        assert_eq!(e.code, "E_NAME");
        assert!(e.msg.contains("FRP_TOML"), "{}", e.msg);
        let e2 = admin_host_post(&root, br#"{"host_env":{"LAN_HOST":"10.0.0.1
FRP=none"}}"#).unwrap_err();
        assert_eq!(e2.code, "E_NAME", "换行注入须被往返校验拦截");
        assert_eq!(
            admin_host_post(&root, br#"{"host_env":{"CADDY":"true"}}"#).unwrap_err().code,
            "E_NAME"
        );
        assert_eq!(admin_host_post(&root, br#"{}"#).unwrap_err().code, "E_NAME");

        // 第二形态 redis_databases：越界走扩容校验（E_NAME，u8 约束；不碰 docker）
        let e = admin_host_post(&root, br#"{"redis_databases":999,"confirm":true}"#).unwrap_err();
        assert_eq!(e.code, "E_NAME");
        assert!(e.msg.contains("u8"), "{}", e.msg);
        // host.env 仍字节不变（任何拒绝路径零副作用）
        assert_eq!(std::fs::read(crate::middleware::host_env_path(&root)).unwrap(), before);

        // 非管理员访问该端点：与未登记逐字节一致（复用现测法——admin_guard 同一守卫）
        let guard = admin_guard(
            &Arc::new(test_state(root.clone())),
            "mallory", "ab12cd34",
        )
        .unwrap_err()
        .into_response();
        let (parts, b) = guard.into_parts();
        let bytes = axum::body::to_bytes(b, 1024).await.unwrap().to_vec();
        assert_eq!(parts.status.as_u16(), 404);
        assert_eq!(bytes, br#"{"error":"E_AUTH"}"#.to_vec(), "host 端点 guard 404 同体");

        let _ = std::fs::remove_dir_all(&root);
    }
    // ———— plan-16 §6.5：serve 层（tokio）————

    use serde_json::Value;
    use showroom_common::{nonce_hex, sign};

    fn now_i64() -> i64 {
        state::unix_now() as i64
    }

    /// 常规 SR-SIG1 签名头（serve 测试用）。
    fn authz(user: &str, key: &showroom_common::UserKey, method: &str, path: &str, body: &[u8]) -> String {
        sign(user, key, method, path, body, now_i64(), &nonce_hex())
    }

    const E_AUTH_BODY: &[u8] = br#"{"error":"E_AUTH"}"#;

    // login-begin：已注册/未知用户均 200 且 ke2 同长度（防枚举不可区分）；畸形/未知子路径/
    // 错误方法 → 404 E_AUTH 逐字节一致（复用现有测法）
    #[tokio::test]
    async fn login_begin_indistinguishable_and_zero_info() {
        let root = troot("lb");
        mkuser(&root, "alice", "pw-a");
        let s: S = Arc::new(test_state(root));

        let ke1_of = |user: &str, pw: &str| {
            let mut rng = rand::rngs::OsRng;
            let (_st, ke1) = showroom_common::opaque::login_start(&mut rng, pw, user).unwrap();
            ke1
        };
        let begin = |user: &str, ke1: &str| {
            serde_json::to_vec(&json!({"user": user, "ke1": ke1})).unwrap()
        };

        // 已注册用户 → 200 {"ke2"}
        let (c1, b1) = call(&s, "POST", "/_api/auth/login-begin", &begin("alice", &ke1_of("alice", "pw-a")), &[]).await;
        assert_eq!(c1, 200);
        let ke2a = serde_json::from_slice::<Value>(&b1).unwrap()["ke2"].as_str().unwrap().to_string();
        assert!(!ke2a.is_empty());

        // 未知用户 → 200 {"ke2"}，ke2 与已注册用户同长度（结构同形；内容不同）
        let (c2, b2) = call(&s, "POST", "/_api/auth/login-begin", &begin("ghost", &ke1_of("ghost", "x")), &[]).await;
        assert_eq!(c2, 200, "未知用户须 200（dummy 假记录路径防枚举）");
        let ke2b = serde_json::from_slice::<Value>(&b2).unwrap()["ke2"].as_str().unwrap().to_string();
        assert_eq!(ke2a.len(), ke2b.len(), "ke2 同形（长度不可区分）");
        assert_ne!(ke2a, ke2b);

        // 遗留无密码账号（存量 pubkey-only 形态：只有 .pub 无 pass.opaque）→ 同 200（防枚举不变，B7 后不再新增此类账号）
        let k = showroom_common::UserKey::generate();
        let _kid = put_key_legacy(&s.root, "ci", &k);
        let (c3, b3) = call(&s, "POST", "/_api/auth/login-begin", &begin("ci", &ke1_of("ci", "x")), &[]).await;
        assert_eq!(c3, 200);
        let ke2c = serde_json::from_slice::<Value>(&b3).unwrap()["ke2"].as_str().unwrap().to_string();
        assert_eq!(ke2a.len(), ke2c.len());

        // 畸形 body / 非法 user / 错误方法 / 未知子路径 → 404 E_AUTH 逐字节一致
        for (m, path, body) in [
            ("POST", "/_api/auth/login-begin", b"{not json".as_slice()),
            ("POST", "/_api/auth/login-begin", br#"{"user":"Bad Name","ke1":"x"}"#.as_slice()),
            ("POST", "/_api/auth/login-begin", br#"{"user":"alice"}"#.as_slice()),
            ("GET", "/_api/auth/login-begin", b"".as_slice()),
            ("POST", "/_api/auth/bogus", b"{}".as_slice()),
            ("GET", "/_api/auth/login-finish", b"".as_slice()),
            ("DELETE", "/_api/auth/key-bind", b"".as_slice()),
        ] {
            let (code, bytes) = call(&s, m, path, body, &[]).await;
            assert_eq!((code, bytes.as_slice()), (404, E_AUTH_BODY), "{m} {path}");
        }
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // login-finish：伪造 ke3 拒绝、ke3 重放拒绝（pending 一次性）、正确登录发 bind_token；
    // bind_token 单次使用（key-bind 第二次 404）
    #[tokio::test]
    async fn login_finish_reject_replay_and_token_single_use() {
        let root = troot("lf");
        mkuser(&root, "alice", "pw-a");
        let s: S = Arc::new(test_state(root));
        let mut rng = rand::rngs::OsRng;

        // 伪造 ke3 → 404 E_AUTH（无 pending 也 404）
        let (c, b) = call(&s, "POST", "/_api/auth/login-finish",
            &serde_json::to_vec(&json!({"user":"alice","ke3":"QUFBQQ"})).unwrap(), &[]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY));

        // 正确登录拿 token + 用它 key-bind
        let (token, key) = full_login(&s, "alice", "pw-a").await;
        assert_eq!(token.len(), 32);
        let bind_body = serde_json::to_vec(&json!({"user":"alice","pubkey": key.pub_string()})).unwrap();
        let auth = authz("alice", &key, "POST", "/_api/auth/key-bind", &bind_body);
        let (c, b) = call(&s, "POST", "/_api/auth/key-bind", &bind_body,
            &[("Authorization", auth.as_str()), ("X-Sr-Bind", token.as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["kid"].as_str().map(|s| s.len()), Some(8));
        assert_eq!(v["admin"], false);

        // 同一 bind_token 第二次 → 404 E_AUTH（单次使用）
        let key2 = showroom_common::UserKey::generate();
        let bind_body2 = serde_json::to_vec(&json!({"user":"alice","pubkey": key2.pub_string()})).unwrap();
        let auth2 = authz("alice", &key2, "POST", "/_api/auth/key-bind", &bind_body2);
        let (c, b) = call(&s, "POST", "/_api/auth/key-bind", &bind_body2,
            &[("Authorization", auth2.as_str()), ("X-Sr-Bind", token.as_str())]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "bind_token 单次使用");

        // ke3 重放：再走一次正确登录，把同一 ke3 发两次——第二次须 404（pending 一次性）
        let (st, ke1) = showroom_common::opaque::login_start(&mut rng, "pw-a", "alice").unwrap();
        let (_, b) = call(&s, "POST", "/_api/auth/login-begin",
            &serde_json::to_vec(&json!({"user":"alice","ke1": ke1})).unwrap(), &[]).await;
        let ke2 = serde_json::from_slice::<Value>(&b).unwrap()["ke2"].as_str().unwrap().to_string();
        let (ke3, _sk) = showroom_common::opaque::login_finish(st, "pw-a", "alice", &ke2).unwrap();
        let body3 = serde_json::to_vec(&json!({"user":"alice","ke3": ke3})).unwrap();
        let (c, _) = call(&s, "POST", "/_api/auth/login-finish", &body3, &[]).await;
        assert_eq!(c, 200);
        let (c, b) = call(&s, "POST", "/_api/auth/login-finish", &body3, &[]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "ke3 重放拒绝");
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // key-bind：验签必须按 body 内公钥（候选钥匙），而非已登记钥匙——
    // 用未登记钥匙 B 签名 + body=B → 200；body=A 签名=B（错位）→ 404
    #[tokio::test]
    async fn key_bind_verifies_body_pubkey() {
        let root = troot("kb");
        mkuser(&root, "alice", "pw-a");
        // alice 已有一把登记钥匙 A（旧设备，经 key-bind 绑定）
        let key_a = showroom_common::UserKey::generate();
        crate::ops::user_add(&s_unused(), "x", false, crate::ops::InitPw::Gen).ok();
        let s: S = Arc::new(test_state(root.clone()));
        crate::ops::key_bind(&s.root, "alice", &key_a.pub_string()).unwrap();

        let (token, key_b) = full_login(&s, "alice", "pw-a").await;
        // 未登记钥匙 B 签名 + body=B → 200（按 body 公钥验签通过）
        let body = serde_json::to_vec(&json!({"user":"alice","pubkey": key_b.pub_string()})).unwrap();
        let auth = authz("alice", &key_b, "POST", "/_api/auth/key-bind", &body);
        let (c, b) = call(&s, "POST", "/_api/auth/key-bind", &body,
            &[("Authorization", auth.as_str()), ("X-Sr-Bind", token.clone().as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));

        // 错位：token 有效但 body=A、签名=B → 404（签名不匹配 body 公钥）
        let (token2, _) = full_login(&s, "alice", "pw-a").await;
        let _ = token2;
        let (token3, key_c) = full_login(&s, "alice", "pw-a").await;
        let body3 = serde_json::to_vec(&json!({"user":"alice","pubkey": key_a.pub_string()})).unwrap();
        let auth3 = authz("alice", &key_c, "POST", "/_api/auth/key-bind", &body3);
        let (c, b) = call(&s, "POST", "/_api/auth/key-bind", &body3,
            &[("Authorization", auth3.as_str()), ("X-Sr-Bind", token3.as_str())]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "签名与 body 公钥错位须拒");

        // nonce 重放（同签名串二次提交、新 token）→ 404
        let (token4, key_d) = full_login(&s, "alice", "pw-a").await;
        let body4 = serde_json::to_vec(&json!({"user":"alice","pubkey": key_d.pub_string()})).unwrap();
        let auth4 = authz("alice", &key_d, "POST", "/_api/auth/key-bind", &body4);
        let (c, _) = call(&s, "POST", "/_api/auth/key-bind", &body4,
            &[("Authorization", auth4.as_str()), ("X-Sr-Bind", token4.as_str())]).await;
        assert_eq!(c, 200);
        let (token5, _) = full_login(&s, "alice", "pw-a").await;
        let (c, b) = call(&s, "POST", "/_api/auth/key-bind", &body4,
            &[("Authorization", auth4.as_str()), ("X-Sr-Bind", token5.as_str())]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "key-bind nonce 重放拒绝");
        let _ = std::fs::remove_dir_all(&s.root);
    }

    fn s_unused() -> PathBuf {
        PathBuf::from("/nonexistent-s_unused")
    }

    // login-begin 限流：per-user 10 次/10 分 → 第 11 次 404 E_AUTH（同体）；全局 120 次
    #[tokio::test]
    async fn login_begin_rate_limit() {
        let root = troot("rate");
        mkuser(&root, "alice", "pw-a");
        let s: S = Arc::new(test_state(root.clone()));
        let mut rng = rand::rngs::OsRng;
        let (_st, ke1) = showroom_common::opaque::login_start(&mut rng, "pw-a", "alice").unwrap();
        let body = serde_json::to_vec(&json!({"user":"alice","ke1": ke1})).unwrap();
        for i in 0..10 {
            let (c, _) = call(&s, "POST", "/_api/auth/login-begin", &body, &[]).await;
            assert_eq!(c, 200, "第 {i} 次应放行");
        }
        let (c, b) = call(&s, "POST", "/_api/auth/login-begin", &body, &[]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "user 桶第 11 次拒绝");
        // 别的 user 不受影响
        let (_st2, ke1b) = showroom_common::opaque::login_start(&mut rng, "x", "bob").unwrap();
        let (c, _) = call(&s, "POST", "/_api/auth/login-begin",
            &serde_json::to_vec(&json!({"user":"bob","ke1": ke1b})).unwrap(), &[]).await;
        assert_eq!(c, 200, "per-user 桶互不影响");

        // 全局桶：12 user × 10 次（各 user 桶恰好不超）灌满 120 后，第 121 次（新 user）→ 404。
        // ke1 复用同一串（内容与 user 无一致性校验，login-begin 只做 OPRF evaluate）。
        let s2: S = Arc::new(test_state(root.clone()));
        for u in 0..12u32 {
            let bu = serde_json::to_vec(&json!({"user": format!("g{u:02}"), "ke1": ke1})).unwrap();
            for i in 0..10 {
                let (c, _) = call(&s2, "POST", "/_api/auth/login-begin", &bu, &[]).await;
                assert_eq!(c, 200, "g{u:02} 第 {i} 次应放行");
            }
        }
        let b121 = serde_json::to_vec(&json!({"user": "g99", "ke1": ke1})).unwrap();
        let (c, b) = call(&s2, "POST", "/_api/auth/login-begin", &b121, &[]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "全局桶第 121 次拒绝");
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // 登录态改密码（pw-begin/pw-commit）：替换后旧密码失效新密码可用、既有钥匙不受影响；
    // 裸 pw-commit（无 pw-begin）→ E_STATE；key-selfrm 只能删签名所用 kid 自己
    #[tokio::test]
    async fn pw_change_and_key_selfrm() {
        let root = troot("pwch");
        let old_key = showroom_common::UserKey::generate();
        crate::ops::user_add(&root, "alice", false, crate::ops::InitPw::Set("old-pw".into())).unwrap();
        crate::ops::key_bind(&root, "alice", &old_key.pub_string()).unwrap();
        let s: S = Arc::new(test_state(root.clone()));
        let mut rng = rand::rngs::OsRng;

        // pw-begin（签名）→ m2 → reg_finish → pw-commit（签名）
        let (cst, m1) = showroom_common::opaque::reg_start(&mut rng, "new-pw", "alice").unwrap();
        let b1 = serde_json::to_vec(&json!({"user":"alice","m1": m1})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/auth/pw-begin", &b1,
            &[("Authorization", authz("alice", &old_key, "POST", "/_api/auth/pw-begin", &b1).as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let m2 = serde_json::from_slice::<Value>(&b).unwrap()["m2"].as_str().unwrap().to_string();
        let record = showroom_common::opaque::reg_finish(&mut rng, cst, "new-pw", "alice", &m2).unwrap();
        let b2 = serde_json::to_vec(&json!({"user":"alice","record": record})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/auth/pw-commit", &b2,
            &[("Authorization", authz("alice", &old_key, "POST", "/_api/auth/pw-commit", &b2).as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));

        // 新密码可登录、旧密码客户端 finish 失败
        let (st, ke1) = showroom_common::opaque::login_start(&mut rng, "new-pw", "alice").unwrap();
        let (_, b) = call(&s, "POST", "/_api/auth/login-begin",
            &serde_json::to_vec(&json!({"user":"alice","ke1": ke1})).unwrap(), &[]).await;
        let ke2 = serde_json::from_slice::<Value>(&b).unwrap()["ke2"].as_str().unwrap().to_string();
        assert!(showroom_common::opaque::login_finish(st, "new-pw", "alice", &ke2).is_ok());
        let (st2, ke1b) = showroom_common::opaque::login_start(&mut rng, "old-pw", "alice").unwrap();
        let (_, b) = call(&s, "POST", "/_api/auth/login-begin",
            &serde_json::to_vec(&json!({"user":"alice","ke1": ke1b})).unwrap(), &[]).await;
        let ke2b = serde_json::from_slice::<Value>(&b).unwrap()["ke2"].as_str().unwrap().to_string();
        assert!(showroom_common::opaque::login_finish(st2, "old-pw", "alice", &ke2b).is_err(), "旧密码失效");

        // 裸 pw-commit（无在途 pw-begin）→ E_STATE（登录态端点，如实报错）
        let (c, b) = call(&s, "POST", "/_api/auth/pw-commit", &b2,
            &[("Authorization", authz("alice", &old_key, "POST", "/_api/auth/pw-commit", &b2).as_str())]).await;
        assert_eq!(c, 500, "E_STATE → 500");
        assert!(b.starts_with(br#"{"error":"E_STATE""#));

        // 旧钥匙仍可用（改密码不动钥匙）：GET /_api/reg-cred（读 reg.cred）
        let (c, b) = call(&s, "GET", "/_api/reg-cred", b"",
            &[("Authorization", authz("alice", &old_key, "GET", "/_api/reg-cred", b"").as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["user"], "alice");
        assert!(v["registry_password"].as_str().is_some_and(|p| !p.is_empty()));

        // key-selfrm：只能删签名所用 kid 自己——先绑定第二把，再各自签名试删
        let (token, key_b) = full_login(&s, "alice", "new-pw").await;
        let bb = serde_json::to_vec(&json!({"user":"alice","pubkey": key_b.pub_string()})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/auth/key-bind", &bb,
            &[("Authorization", authz("alice", &key_b, "POST", "/_api/auth/key-bind", &bb).as_str()),
              ("X-Sr-Bind", token.as_str())]).await;
        assert_eq!(c, 200);
        let kid_b = serde_json::from_slice::<Value>(&b).unwrap()["kid"].as_str().unwrap().to_string();

        // 用 key_b 签名删 old_key 的 kid → E_AUTH（仅允许删自己）
        let (kid_old, _) = {
            let vk = showroom_common::UserKey::from_pub_string(&old_key.pub_string()).unwrap();
            (state::kid_of(&vk), 0)
        };
        let bad = serde_json::to_vec(&json!({"kid": kid_old})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/key-selfrm", &bad,
            &[("Authorization", authz("alice", &key_b, "POST", "/_api/key-selfrm", &bad).as_str())]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY), "key-selfrm 仅允许删除签名所用钥匙自身");

        // 删自己 → ok，钥匙消失
        let good = serde_json::to_vec(&json!({"kid": kid_b})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/key-selfrm", &good,
            &[("Authorization", authz("alice", &key_b, "POST", "/_api/key-selfrm", &good).as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        assert!(!state::tokens_dir(&s.root, "alice").join(format!("{kid_b}.pub")).exists());
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // admin/passwd-set：gen:true 一次性返回密码且 has_password 翻真；非管理员 404 同体
    #[tokio::test]
    async fn admin_passwd_set_endpoint() {
        let root = troot("pwset");
        let admin_key = showroom_common::UserKey::generate();
        crate::ops::user_add(&root, "root", true, crate::ops::InitPw::Gen).unwrap();
        crate::ops::key_bind(&root, "root", &admin_key.pub_string()).unwrap();
        // alice：遗留无密码账号（存量 pubkey-only 形态，B7 后不再新增）
        let plain_key = showroom_common::UserKey::generate();
        let _kid = put_key_legacy(&root, "alice", &plain_key);
        let s: S = Arc::new(test_state(root.clone()));
        assert!(!crate::ops::user_list(&s.root).iter().find(|u| u.user == "alice").unwrap().has_password);

        // 非管理员 → 404 E_AUTH 同体（不泄露端点存在）
        let b0 = serde_json::to_vec(&json!({"user":"alice","gen":true})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/admin/passwd-set", &b0,
            &[("Authorization", authz("alice", &plain_key, "POST", "/_api/admin/passwd-set", &b0).as_str())]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY));

        // 管理员 gen:true → {password} 只此一次
        let (c, b) = call(&s, "POST", "/_api/admin/passwd-set", &b0,
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/passwd-set", &b0).as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let pw = serde_json::from_slice::<Value>(&b).unwrap()["password"].as_str().unwrap().to_string();
        assert_eq!(pw.len(), 24);
        assert!(crate::ops::user_list(&s.root).iter().find(|u| u.user == "alice").unwrap().has_password);
        // 新密码可登录
        let mut rng = rand::rngs::OsRng;
        let (st, ke1) = showroom_common::opaque::login_start(&mut rng, &pw, "alice").unwrap();
        let (_, b) = call(&s, "POST", "/_api/auth/login-begin",
            &serde_json::to_vec(&json!({"user":"alice","ke1": ke1})).unwrap(), &[]).await;
        let ke2 = serde_json::from_slice::<Value>(&b).unwrap()["ke2"].as_str().unwrap().to_string();
        assert!(showroom_common::opaque::login_finish(st, &pw, "alice", &ke2).is_ok());

        // 全无密码参数 → E_NAME
        let b1 = br#"{"user":"alice"}"#;
        let (c, _) = call(&s, "POST", "/_api/admin/passwd-set", b1,
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/passwd-set", b1).as_str())]).await;
        assert_eq!(c, 400, "E_NAME → 400");
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // admin/user-add（plan-16 §1.5 + B7 统一账号密码认证）：password/gen_password → 响应两把
    // 密码一次性返回；pubkey 字段已删除（serde 忽略未知键=前向兼容）；全无密码 → E_NAME
    #[tokio::test]
    async fn admin_user_add_with_password() {
        let root = troot("ua");
        let admin_key = showroom_common::UserKey::generate();
        crate::ops::user_add(&root, "root", true, crate::ops::InitPw::Gen).unwrap();
        // 管理员钥匙：root 走 pending 继承——首把绑定钥匙拿 admin
        let (_, admin) = crate::ops::key_bind(&root, "root", &admin_key.pub_string()).unwrap();
        assert!(admin, "首把登录绑定钥匙继承账号级 admin");
        let s: S = Arc::new(test_state(root.clone()));

        // password-only
        let b1 = serde_json::to_vec(&json!({"user":"carol","password":"pw-1"})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/admin/user-add", &b1,
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/user-add", &b1).as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["login_password"], "pw-1");
        assert!(v["registry_password"].as_str().is_some_and(|p| !p.is_empty()));
        assert!(v.get("kid").is_none(), "B7 后响应无 kid 字段（不再预登记公钥）");

        // gen_password + admin（pubkey 字段被忽略，不生效）
        let b2 = serde_json::to_vec(&json!({"user":"dave","gen_password":true,"admin":true})).unwrap();
        let (c, b) = call(&s, "POST", "/_api/admin/user-add", &b2,
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/user-add", &b2).as_str())]).await;
        assert_eq!(c, 200);
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert!(v.get("kid").is_none(), "pubkey 已删除：带旧字段也不落公钥");
        assert_eq!(v["login_password"].as_str().map(|s| s.len()), Some(24));
        assert!(v["registry_password"].as_str().is_some_and(|p| !p.is_empty()));
        assert!(root.join("state/users/dave/admin").exists(), "admin 落账号级标记（R4）");

        // 全无密码 → E_NAME（含新示例指引）
        let b3 = br#"{"user":"erin"}"#;
        let (c, b) = call(&s, "POST", "/_api/admin/user-add", b3,
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/user-add", b3).as_str())]).await;
        assert_eq!(c, 400);
        assert!(b.starts_with(br#"{"error":"E_NAME""#));
        assert!(String::from_utf8_lossy(&b).contains("gen_password"), "E_NAME 须带新示例指引");
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // R5 E2：GET /_api/admin/projects——admin_guard 拦非管理员（404 与 E_AUTH 逐字节一致）；
    // 管理员得跨用户全量（每行附 owner=user）；/_api/projects 仍只回本人（32c3484 裁决不回退）。
    // R5 D2：key-admin 兼容垫片路由已移除——任何签名者均 404 E_AUTH 同体。
    #[tokio::test]
    async fn admin_projects_endpoint_and_key_admin_shim_removed() {
        let root = troot("admp");
        // 两个账号各一个项目（conf 层；docker 实况不可用时 status 回落 missing，不影响断言）
        for (p, user) in [("demo1", "alice"), ("demo2", "bob")] {
            let c = crate::state::Conf {
                user: user.into(), tag: "v1".into(), port: 8100, cport: 8080,
                mem: "256m".into(), cpu: 1.0, redisdb: 0, updated: 1,
                stopped: false, legacyslot: None,
                version: String::new(),
                version_seq: 0,
            };
            c.save(&root, p).unwrap();
        }
        let admin_key = showroom_common::UserKey::generate();
        crate::ops::user_add(&root, "root", true, crate::ops::InitPw::Gen).unwrap();
        crate::ops::key_bind(&root, "root", &admin_key.pub_string()).unwrap();
        let plain_key = showroom_common::UserKey::generate();
        crate::ops::user_add(&root, "alice", false, crate::ops::InitPw::Gen).unwrap();
        crate::ops::key_bind(&root, "alice", &plain_key.pub_string()).unwrap();
        let s: S = Arc::new(test_state(root.clone()));

        // 非管理员 → 404 E_AUTH 同体（不泄露端点存在）
        let (c, b) = call(&s, "GET", "/_api/admin/projects", b"",
            &[("Authorization", authz("alice", &plain_key, "GET", "/_api/admin/projects", b"").as_str())]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY));

        // 管理员 → 跨用户全量，每行附 owner=user
        let (c, b) = call(&s, "GET", "/_api/admin/projects", b"",
            &[("Authorization", authz("root", &admin_key, "GET", "/_api/admin/projects", b"").as_str())]).await;
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let v: Value = serde_json::from_slice(&b).unwrap();
        let projects = v["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 2, "{projects:?}");
        assert!(projects.iter().any(|p| p["project"] == "demo1" && p["user"] == "alice"), "owner=user 附行");
        assert!(projects.iter().any(|p| p["project"] == "demo2" && p["user"] == "bob"), "跨账号全量");

        // /_api/projects 仍只回本人（alice 只见 demo1）
        let (c, b) = call(&s, "GET", "/_api/projects", b"",
            &[("Authorization", authz("alice", &plain_key, "GET", "/_api/projects", b"").as_str())]).await;
        assert_eq!(c, 200);
        let v: Value = serde_json::from_slice(&b).unwrap();
        let own: Vec<&str> = v["projects"].as_array().unwrap().iter().map(|p| p["project"].as_str().unwrap()).collect();
        assert_eq!(own, ["demo1"]);

        // key-admin 垫片已移除：管理员携带合法签名也落入通用未知端点分支（404 E_NOTFOUND 同体，
        // 与任意不存在路径不可区分——不再有 key-admin 专属行为）
        let b4 = br#"{"user":"bob","kid":"deadbeef","admin":true}"#;
        let (c, b) = call(&s, "POST", "/_api/admin/key-admin", b4,
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/key-admin", b4).as_str())]).await;
        assert_eq!(c, 404);
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["error"], "E_NOTFOUND", "垫片路由不复活（一律走 user-admin）");
        let (c2, b2) = call(&s, "POST", "/_api/admin/no-such-endpoint", b"",
            &[("Authorization", authz("root", &admin_key, "POST", "/_api/admin/no-such-endpoint", b"").as_str())]).await;
        assert_eq!((c2, b2.as_slice()), (c, b.as_slice()), "与任意未知端点逐字节一致（零专属信息）");
        let _ = std::fs::remove_dir_all(&s.root);
    }

    // ———— /_api/tool/*（控制台内下载）：白名单 / 验签 / 缺文件 / CT ————

    /// 直调 api_inner 带响应头（call 只回 body；CT 断言用）。
    async fn call_hdrs(s: &S, method: &str, path: &str, body: &[u8], headers: &[(&str, &str)])
        -> (u16, axum::http::HeaderMap, Vec<u8>) {
        let uri: axum::http::Uri = path.parse().unwrap();
        let mut hm = HeaderMap::new();
        for (k, v) in headers {
            hm.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        let r = api_inner(
            s,
            &Method::from_bytes(method.as_bytes()).unwrap(),
            &OriginalUri(uri),
            &hm,
            Body::from(body.to_vec()),
        )
        .await
        .into_response();
        let (parts, b) = r.into_parts();
        let bytes = axum::body::to_bytes(b, 1 << 20).await.unwrap().to_vec();
        (parts.status.as_u16(), parts.headers, bytes)
    }

    // 白名单三件可取（字节一致 + CT 按扩展名）；未签名/错签名 → 404 E_AUTH 零信息；
    // 白名单外（含 ../ 穿越形状、空名）与缺文件 → E_NOTFOUND；POST 方法 → E_NOTFOUND。
    #[tokio::test]
    async fn tool_endpoint_whitelist_auth_and_missing() {
        let root = troot("tool");
        let k = showroom_common::UserKey::generate();
        let _kid = put_key_legacy(&root, "alice", &k);
        let s: S = Arc::new(test_state(root.clone()));

        // 未签名 → 404 E_AUTH（与一切未签名请求同体，零信息）
        let (c, b) = call(&s, "GET", "/_api/tool/showroom", b"", &[]).await;
        assert_eq!((c, b.as_slice()), (404, E_AUTH_BODY));

        // 部署态：_bootstrap/public 三件齐（内容可区分）
        let dir = root.join("_bootstrap/public");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("showroom"), b"\x7fELF-fake-bin").unwrap();
        std::fs::write(dir.join("showroom-publisher.md"), "# Showroom 发布 skill").unwrap();
        std::fs::write(dir.join("platform_rsa.pub"), "-----BEGIN PUBLIC KEY-----\n").unwrap();

        // 签名 + 白名单三件：200 + 字节一致 + CT 按扩展名
        let files = [
            ("showroom", &b"\x7fELF-fake-bin"[..], "application/octet-stream"),
            ("showroom-publisher.md", "# Showroom 发布 skill".as_bytes(), "text/markdown; charset=utf-8"),
            ("platform_rsa.pub", "-----BEGIN PUBLIC KEY-----\n".as_bytes(), "text/plain; charset=utf-8"),
        ];
        for (name, want, ct) in files {
            let path = format!("/_api/tool/{name}");
            let (c, h, b) = call_hdrs(&s, "GET", &path, b"",
                &[("Authorization", authz("alice", &k, "GET", &path, b"").as_str())]).await;
            assert_eq!(c, 200, "{name}: {}", String::from_utf8_lossy(&b));
            assert_eq!(b, want, "{name} 字节一致");
            assert_eq!(h.get(axum::http::header::CONTENT_TYPE).unwrap(), ct, "{name} CT 按扩展名");
        }

        // 白名单外：未知名 / ../ 穿越形状（路径未解码也须拒）/ 空名 → E_NOTFOUND（404 + msg）
        for bad in ["/_api/tool/showroomctl", "/_api/tool/../../../etc/passwd", "/_api/tool/", "/_api/tool/.hidden"] {
            let (c, b) = call(&s, "GET", bad, b"",
                &[("Authorization", authz("alice", &k, "GET", bad, b"").as_str())]).await;
            assert_eq!(c, 404, "{bad}");
            assert!(b.starts_with(br#"{"error":"E_NOTFOUND""#), "{bad}: {}", String::from_utf8_lossy(&b));
        }

        // 缺文件（白名单内但部署侧未刷新）：E_NOTFOUND 如实报（验签后，非零信息面）
        std::fs::remove_file(dir.join("showroom-publisher.md")).unwrap();
        let p2 = "/_api/tool/showroom-publisher.md";
        let (c, b) = call(&s, "GET", p2, b"",
            &[("Authorization", authz("alice", &k, "GET", p2, b"").as_str())]).await;
        assert_eq!(c, 404);
        assert!(b.starts_with(br#"{"error":"E_NOTFOUND""#));

        // POST 方法 → 落主 match 未知端点 → E_NOTFOUND（只允许 GET）
        let (c, b) = call(&s, "POST", "/_api/tool/showroom", b"",
            &[("Authorization", authz("alice", &k, "POST", "/_api/tool/showroom", b"").as_str())]).await;
        assert_eq!(c, 404);
        assert!(b.starts_with(br#"{"error":"E_NOTFOUND""#));
        let _ = std::fs::remove_dir_all(&s.root);
    }
}

