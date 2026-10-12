//! 签名 HTTP 客户端 + 服务端选择 + URL/查询拼接。
//! 所有 /_api 端点（含 health/projects）均要求 SR-SIG1 签名（SPEC §3）。
//! 错误统一为已渲染的 String（E_AUTH 附登记/时钟提示与公钥串），调用点直接 `?`。

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use serde_json::Value;

use showroom_common::{nonce_hex, sign, sign_prehashed, sha256_hex_stream, UserKey};

use crate::config::Config;

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Which {
    Lan,
    Public,
}

/// 服务端选择：`--server` 强制 > lan 探测可达 > public 回落 > 仅 lan（探不通也强于没有）。
/// probe 由调用方注入（真实实现发签名 GET /_api/health），纯函数便于单测。
pub fn pick_server(
    lan: Option<&str>,
    public: Option<&str>,
    force: Option<Which>,
    probe: impl Fn(&str) -> bool,
) -> Result<(String, Which), String> {
    let lan = lan.filter(|s| !s.is_empty());
    let public = public.filter(|s| !s.is_empty());
    let norm = |s: &str| s.trim_end_matches('/').to_string();
    match force {
        Some(Which::Lan) => {
            return lan
                .map(|s| (norm(s), Which::Lan))
                .ok_or_else(|| "未配置 server_lan（showroom config --server-lan URL）".to_string())
        }
        Some(Which::Public) => {
            return public.map(|s| (norm(s), Which::Public)).ok_or_else(|| {
                "未配置 server_public（showroom config --server-public URL）".to_string()
            })
        }
        None => {}
    }
    if let Some(l) = lan {
        if probe(l) {
            return Ok((norm(l), Which::Lan));
        }
    }
    if let Some(p) = public {
        return Ok((norm(p), Which::Public));
    }
    if let Some(l) = lan {
        return Ok((norm(l), Which::Lan));
    }
    Err("未配置任何服务端：showroom config --server-lan URL（或 --server-public）".to_string())
}

// —— URL / 查询拼接（§11：URL 恒 /<project>/，请求不再带 slot）——

pub fn project_url(base: &str, project: &str) -> String {
    format!("{}/{project}/", base.trim_end_matches('/'))
}

pub fn logs_query(project: &str, tail: u32) -> String {
    format!("/_api/logs?project={project}&tail={tail}")
}

pub fn env_query(project: &str) -> String {
    format!("/_api/env?project={project}")
}

pub fn import_query(project: &str, tag: &str) -> String {
    format!("/_api/import?project={project}&tag={tag}")
}

pub fn db_tables_query(project: &str) -> String {
    format!("/_api/db-tables?project={project}")
}

// —— db-dump 三段式（SPEC §10.4）：status 轮询 + chunk 切片 ——

/// CLI 请求的切片长度（服务端上限 4MB，SPEC §10.4）。
pub const DUMP_CHUNK_LEN: u64 = 4 << 20;

pub fn dump_status_query(dump_id: &str) -> String {
    format!("/_api/db-dump-status?dump={dump_id}")
}

pub fn dump_chunk_query(dump_id: &str, offset: u64, len: u64) -> String {
    format!("/_api/db-dump-chunk?dump={dump_id}&offset={offset}&len={len}")
}

pub fn db_restore_query(project: &str, confirm: bool) -> String {
    format!("/_api/db-restore?project={project}&confirm={confirm}")
}

// —— 错误 ——

pub struct ApiError {
    pub code: String,
    pub msg: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.msg)
    }
}

impl ApiError {
    /// E_AUTH 附带登记/时间偏差提示与公钥串。
    pub fn render(&self, pub_str: &str) -> String {
        if self.code == "E_AUTH" {
            format!("{self}（公钥未登记或时间偏差：showroom doctor；公钥串：{pub_str}）")
        } else {
            self.to_string()
        }
    }
}

fn net_err(msg: String) -> ApiError {
    ApiError { code: "E_NET".into(), msg }
}

/// 非 2xx 响应 → ApiError（取 {"error","msg"}；非 JSON 退回 E_NET 带状态与片段）。
fn error_from(resp: reqwest::blocking::Response) -> ApiError {
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => ApiError {
            code: v["error"].as_str().unwrap_or("E_NET").to_string(),
            msg: v["msg"].as_str().unwrap_or(&format!("HTTP {status}")).to_string(),
        },
        Err(_) => net_err(format!("HTTP {status}: {}", text.chars().take(200).collect::<String>())),
    }
}

/// 解析响应：2xx 取 JSON；4xx/5xx 取 {"error","msg"}。
fn check(resp: reqwest::blocking::Response) -> Result<Value, ApiError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(error_from(resp));
    }
    let text = resp.text().map_err(|e| net_err(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| net_err(format!("HTTP {status}: 非 JSON 响应: {e}")))
}

/// 可达性探测：签名 GET /_api/health（3s 超时；收到任何 HTTP 响应即视为可达）。
pub fn signed_probe<'a>(user: &'a str, key: &'a UserKey) -> impl Fn(&str) -> bool + 'a {
    move |base| {
        let Ok(c) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
        else {
            return false;
        };
        let auth = sign(user, key, "GET", "/_api/health", b"", now_unix(), &nonce_hex());
        let url = format!("{}/_api/health", base.trim_end_matches('/'));
        c.get(&url).header("Authorization", auth).send().is_ok()
    }
}

// —— 客户端 ——

pub struct Api {
    pub base: String,
    user: String,
    key: UserKey,
    http: reqwest::blocking::Client,
}

impl Api {
    fn load_key(path: &Path) -> Result<UserKey, String> {
        UserKey::from_seed_file(path)
            .map_err(|e| format!("读取密钥 {} 失败：{e}（先运行 showroom keygen）", path.display()))
    }

    /// 探测（签名 health）+ 选择服务端 + 加载密钥。
    pub fn connect(cfg: &Config, force: Option<Which>) -> Result<Api, String> {
        if cfg.user.is_empty() {
            return Err("未配置 user：showroom config --user <name>".into());
        }
        let (base, _which) = pick_server(
            cfg.server_lan.as_deref(),
            cfg.server_public.as_deref(),
            force,
            signed_probe(&cfg.user, &Self::load_key(&cfg.key_file())?),
        )?;
        Self::open(base, cfg)
    }

    /// 用已选定的服务端构造（push 等已探测过的路径复用）。
    pub fn open(base: String, cfg: &Config) -> Result<Api, String> {
        if cfg.user.is_empty() {
            return Err("未配置 user：showroom config --user <name>".into());
        }
        let key = Self::load_key(&cfg.key_file())?;
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180)) // publish 服务端滚动发布可能较慢
            .build()
            .map_err(|e| format!("HTTP 客户端构造失败：{e}"))?;
        Ok(Api { base, user: cfg.user.clone(), key, http })
    }

    /// 错误渲染收口：E_AUTH 附登记/时间偏差提示与公钥串，其余原样。
    fn finish<T>(&self, r: Result<T, ApiError>) -> Result<T, String> {
        r.map_err(|e| e.render(&self.key.pub_string()))
    }

    pub fn signed_get(&self, path_q: &str) -> Result<Value, String> {
        let auth = sign(&self.user, &self.key, "GET", path_q, b"", now_unix(), &nonce_hex());
        self.finish(
            self.http
                .get(format!("{}{}", self.base, path_q))
                .header("Authorization", auth)
                .send()
                .map_err(|e| net_err(format!("{}/{}`: {e}", self.base, path_q)))
                .and_then(check),
        )
    }

    pub fn signed_post_json(&self, path_q: &str, body: &Value) -> Result<Value, String> {
        self.finish(self.post_json_inner(path_q, body))
    }

    fn post_json_inner(&self, path_q: &str, body: &Value) -> Result<Value, ApiError> {
        let bytes = serde_json::to_vec(body).map_err(|e| net_err(e.to_string()))?;
        let auth = sign(&self.user, &self.key, "POST", path_q, &bytes, now_unix(), &nonce_hex());
        self.http
            .post(format!("{}{}", self.base, path_q))
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(bytes)
            .send()
            .map_err(|e| net_err(format!("{}{path_q}: {e}", self.base)))
            .and_then(check)
    }

    /// signed GET，2xx 时返回**原始响应**供调用方流式读取（db-dump-chunk 切片下载用）。
    /// 下载走 24h 宽超时客户端（同大传输语义）。
    pub fn signed_get_raw(&self, path_q: &str) -> Result<reqwest::blocking::Response, String> {
        self.finish(self.signed_get_raw_inner(path_q))
    }

    fn signed_get_raw_inner(&self, path_q: &str) -> Result<reqwest::blocking::Response, ApiError> {
        let auth = sign(&self.user, &self.key, "GET", path_q, b"", now_unix(), &nonce_hex());
        let resp = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(24 * 3600))
            .build()
            .map_err(|e| net_err(e.to_string()))?
            .get(format!("{}{}", self.base, path_q))
            .header("Authorization", auth)
            .send()
            .map_err(|e| net_err(format!("{}{path_q}: {e}", self.base)))?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(error_from(resp))
        }
    }

    /// 大文件流式上传：先流式 sha256，再对哈希签名（与 sign 同格式，避免整文件进内存）。
    pub fn signed_post_file(&self, path_q: &str, file: &Path) -> Result<Value, String> {
        self.finish(self.signed_post_file_inner(path_q, file))
    }

    fn signed_post_file_inner(&self, path_q: &str, file: &Path) -> Result<Value, ApiError> {
        // 哈希与请求体各开各的句柄：同一句柄读完哈希后游标在 EOF，作为 body 只会发出 0 字节
        let hash = {
            let f = File::open(file).map_err(|e| net_err(e.to_string()))?;
            sha256_hex_stream(&f).map_err(|e| net_err(e.to_string()))?
        };
        let auth = sign_prehashed(
            &self.user, &self.key, "POST", path_q, &hash, now_unix(), &nonce_hex(),
        );
        let f = File::open(file).map_err(|e| net_err(e.to_string()))?;
        let resp = reqwest::blocking::Client::new() // 上传走独立无超时语义：单请求放宽到 24h
            .post(format!("{}{}", self.base, path_q))
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .timeout(Duration::from_secs(24 * 3600))
            .body(reqwest::blocking::Body::from(f)) // 流式，不进内存
            .send()
            .map_err(|e| {
                use std::error::Error as _;
                let mut msg = format!("{e}");
                let mut src = e.source();
                while let Some(s) = src {
                    msg.push_str(&format!("; caused by: {s}"));
                    src = s.source();
                }
                net_err(format!("{}{path_q}: {msg}", self.base))
            })?;
        check(resp)
    }
}

/// 平台 RSA 公钥（import 通道信封用；由管理员提供，放 ~/.showroom/platform_rsa.pub）。
pub fn load_platform_pub() -> Result<showroom_common::rsa::RsaPublicKey, String> {
    use showroom_common::rsa::pkcs8::DecodePublicKey as _;
    let p = crate::config::expand("~/.showroom/platform_rsa.pub");
    let pem = fs::read_to_string(&p)
        .map_err(|_| format!("缺少平台公钥 {}（向管理员索取后放入该路径）", p.display()))?;
    showroom_common::rsa::RsaPublicKey::from_public_key_pem(&pem)
        .map_err(|e| format!("平台公钥解析失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_server_prefers_reachable_lan() {
        let (b, w) = pick_server(Some("http://lan/"), Some("http://pub/"), None, |_| true).unwrap();
        assert_eq!((b.as_str(), w), ("http://lan", Which::Lan));
    }

    #[test]
    fn pick_server_falls_back_to_public_when_lan_down() {
        let (b, w) = pick_server(Some("http://lan"), Some("http://pub/"), None, |_| false).unwrap();
        assert_eq!((b.as_str(), w), ("http://pub", Which::Public));
    }

    #[test]
    fn pick_server_force_public_overrides_reachable_lan() {
        let (b, w) =
            pick_server(Some("http://lan"), Some("http://pub"), Some(Which::Public), |_| true)
                .unwrap();
        assert_eq!((b.as_str(), w), ("http://pub", Which::Public));
    }

    #[test]
    fn pick_server_force_missing_config_errs() {
        assert!(pick_server(None, Some("http://pub"), Some(Which::Lan), |_| true).is_err());
        assert!(pick_server(Some("http://lan"), None, Some(Which::Public), |_| true).is_err());
    }

    #[test]
    fn pick_server_lan_only_stays_lan_even_down() {
        let (b, w) = pick_server(Some("http://lan"), None, None, |_| false).unwrap();
        assert_eq!((b.as_str(), w), ("http://lan", Which::Lan));
    }

    #[test]
    fn pick_server_nothing_configured_errs() {
        assert!(pick_server(None, None, None, |_| true).is_err());
    }

    #[test]
    fn empty_string_counts_as_unconfigured() {
        let (_, w) = pick_server(Some(""), Some("http://pub"), None, |_| true).unwrap();
        assert_eq!(w, Which::Public);
    }

    #[test]
    fn project_url_rules() {
        assert_eq!(project_url("http://h/", "demo"), "http://h/demo/");
        assert_eq!(project_url("http://h", "Demo-9"), "http://h/Demo-9/");
    }

    #[test]
    fn query_builders() {
        assert_eq!(logs_query("demo", 100), "/_api/logs?project=demo&tail=100");
        assert_eq!(env_query("demo"), "/_api/env?project=demo");
        assert_eq!(import_query("demo", "v1"), "/_api/import?project=demo&tag=v1");
        assert_eq!(db_tables_query("demo"), "/_api/db-tables?project=demo");
        assert_eq!(
            db_restore_query("demo", true),
            "/_api/db-restore?project=demo&confirm=true"
        );
        assert_eq!(
            db_restore_query("demo", false),
            "/_api/db-restore?project=demo&confirm=false"
        );
        assert_eq!(
            dump_status_query(&"a".repeat(32)),
            format!("/_api/db-dump-status?dump={}", "a".repeat(32))
        );
        assert_eq!(
            dump_chunk_query(&"b".repeat(32), 4096, DUMP_CHUNK_LEN),
            format!("/_api/db-dump-chunk?dump={}&offset=4096&len={}", "b".repeat(32), DUMP_CHUNK_LEN)
        );
    }
}

/// E_AUTH 检测：密钥过期/吊销时返回特殊错误码供 main.rs 触发 reauth
pub fn is_key_expired(err: &str) -> bool {
    // 零信息暴露下过期/吊销与普通验签失败不可区分——有本地密钥时的 E_AUTH 一律视为需要重登
    err.contains("E_AUTH")
}
