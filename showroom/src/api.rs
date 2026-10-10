//! 签名 HTTP 客户端 + 服务端选择 + URL/查询拼接。
//! 所有 /_api 端点（含 health/projects）均要求 SR-SIG1 签名（SPEC §3）。
//! 错误统一为已渲染的 String（E_AUTH 附登记/时钟提示与公钥串），调用点直接 `?`。

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use serde_json::Value;

use showroom_common::{nonce_hex, sign, sign_prehashed, sha256_hex_stream, OwnerKey};

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
        if probe(&l) {
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

// —— URL / 查询拼接（SPEC §5：stable 槽位路径不带 slot）——

pub fn slot_url(base: &str, project: &str, slot: &str) -> String {
    let b = base.trim_end_matches('/');
    if slot == "stable" {
        format!("{b}/{project}/")
    } else {
        format!("{b}/{project}/{slot}/")
    }
}

pub fn logs_query(project: &str, slot: &str, tail: u32) -> String {
    format!("/_api/logs?project={project}&slot={slot}&tail={tail}")
}

pub fn env_query(project: &str, slot: &str) -> String {
    format!("/_api/env?project={project}&slot={slot}")
}

pub fn import_query(project: &str, slot: &str, tag: &str) -> String {
    format!("/_api/import?project={project}&slot={slot}&tag={tag}")
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

/// 解析响应：2xx 取 JSON；4xx/5xx 取 {"error","msg"}。
fn check(resp: reqwest::blocking::Response) -> Result<Value, ApiError> {
    let status = resp.status();
    let text = resp.text().map_err(|e| net_err(e.to_string()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if status.is_success() => Ok(v),
        Ok(v) => Err(ApiError {
            code: v["error"].as_str().unwrap_or("E_NET").to_string(),
            msg: v["msg"].as_str().unwrap_or(&format!("HTTP {status}")).to_string(),
        }),
        Err(_) => Err(net_err(format!(
            "HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ))),
    }
}

/// 可达性探测：签名 GET /_api/health（3s 超时；收到任何 HTTP 响应即视为可达）。
pub fn signed_probe<'a>(owner: &'a str, key: &'a OwnerKey) -> impl Fn(&str) -> bool + 'a {
    move |base| {
        let Ok(c) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
        else {
            return false;
        };
        let auth = sign(owner, key, "GET", "/_api/health", b"", now_unix(), &nonce_hex());
        let url = format!("{}/_api/health", base.trim_end_matches('/'));
        c.get(&url).header("Authorization", auth).send().is_ok()
    }
}

// —— 客户端 ——

pub struct Api {
    pub base: String,
    owner: String,
    key: OwnerKey,
    http: reqwest::blocking::Client,
}

impl Api {
    fn load_key(path: &Path) -> Result<OwnerKey, String> {
        OwnerKey::from_seed_file(path)
            .map_err(|e| format!("读取密钥 {} 失败：{e}（先运行 showroom keygen）", path.display()))
    }

    /// 探测（签名 health）+ 选择服务端 + 加载密钥。
    pub fn connect(cfg: &Config, force: Option<Which>) -> Result<Api, String> {
        if cfg.owner.is_empty() {
            return Err("未配置 owner：showroom config --owner <name>".into());
        }
        let (base, _which) = pick_server(
            cfg.server_lan.as_deref(),
            cfg.server_public.as_deref(),
            force,
            signed_probe(&cfg.owner, &Self::load_key(&cfg.key_file())?),
        )?;
        Self::open(base, cfg)
    }

    /// 用已选定的服务端构造（push 等已探测过的路径复用）。
    pub fn open(base: String, cfg: &Config) -> Result<Api, String> {
        if cfg.owner.is_empty() {
            return Err("未配置 owner：showroom config --owner <name>".into());
        }
        let key = Self::load_key(&cfg.key_file())?;
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180)) // publish 服务端滚动发布可能较慢
            .build()
            .map_err(|e| format!("HTTP 客户端构造失败：{e}"))?;
        Ok(Api { base, owner: cfg.owner.clone(), key, http })
    }

    /// 错误渲染收口：E_AUTH 附登记/时间偏差提示与公钥串，其余原样。
    fn finish(&self, r: Result<Value, ApiError>) -> Result<Value, String> {
        r.map_err(|e| e.render(&self.key.pub_string()))
    }

    pub fn signed_get(&self, path_q: &str) -> Result<Value, String> {
        let auth = sign(&self.owner, &self.key, "GET", path_q, b"", now_unix(), &nonce_hex());
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
        let auth = sign(&self.owner, &self.key, "POST", path_q, &bytes, now_unix(), &nonce_hex());
        self.http
            .post(format!("{}{}", self.base, path_q))
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(bytes)
            .send()
            .map_err(|e| net_err(format!("{}{path_q}: {e}", self.base)))
            .and_then(check)
    }

    /// 大文件流式上传：先流式 sha256，再对哈希签名（与 sign 同格式，避免整文件进内存）。
    pub fn signed_post_file(&self, path_q: &str, file: &Path) -> Result<Value, String> {
        self.finish(self.signed_post_file_inner(path_q, file))
    }

    fn signed_post_file_inner(&self, path_q: &str, file: &Path) -> Result<Value, ApiError> {
        let f = File::open(file).map_err(|e| net_err(e.to_string()))?;
        let hash = sha256_hex_stream(&f).map_err(|e| net_err(e.to_string()))?;
        let auth = sign_prehashed(
            &self.owner, &self.key, "POST", path_q, &hash, now_unix(), &nonce_hex(),
        );
        let resp = reqwest::blocking::Client::new() // 上传走独立无超时语义：单请求放宽到 24h
            .post(format!("{}{}", self.base, path_q))
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .timeout(Duration::from_secs(24 * 3600))
            .body(reqwest::blocking::Body::from(f)) // 流式，不进内存
            .send()
            .map_err(|e| net_err(format!("{}{path_q}: {e}", self.base)))?;
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
    fn slot_url_rules() {
        assert_eq!(slot_url("http://h/", "demo", "stable"), "http://h/demo/");
        assert_eq!(slot_url("http://h", "demo", "pr42"), "http://h/demo/pr42/");
    }

    #[test]
    fn query_builders() {
        assert_eq!(
            logs_query("demo", "stable", 100),
            "/_api/logs?project=demo&slot=stable&tail=100"
        );
        assert_eq!(env_query("demo", "pr42"), "/_api/env?project=demo&slot=pr42");
        assert_eq!(
            import_query("demo", "stable", "v1"),
            "/_api/import?project=demo&slot=stable&tag=v1"
        );
    }
}
