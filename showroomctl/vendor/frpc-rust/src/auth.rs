use crate::config::Auth;
use anyhow::{Context, Result, ensure};
use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Mutex, time::Instant};

pub fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn auth_key(secret: &str, timestamp: i64) -> String {
    format!("{:x}", Md5::digest(format!("{secret}{timestamp}")))
}
pub struct Authenticator {
    cfg: Auth,
    key: Vec<u8>,
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
}
impl Authenticator {
    pub fn new(cfg: &Auth) -> Result<Arc<Self>> {
        let mut cfg = cfg.clone();
        if let Some(source) = &cfg.token_source {
            cfg.token = source.resolve()?;
        }
        let mut http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(cfg.oidc.insecure_skip_verify);
        if !cfg.oidc.trusted_ca_file.is_empty() {
            http = http.add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(
                &cfg.oidc.trusted_ca_file,
            )?)?);
        }
        if !cfg.oidc.proxy_url.is_empty() {
            http = http.proxy(reqwest::Proxy::all(&cfg.oidc.proxy_url)?);
        }
        let key = cfg.token.as_bytes().to_vec();
        Ok(Arc::new(Self {
            cfg,
            key,
            http: http.build()?,
            cached: Mutex::new(None),
        }))
    }
    pub fn encryption_key(&self) -> &[u8] {
        &self.key
    }
    pub async fn fields(&self, scope: &str) -> Result<Value> {
        if scope != "Login" && !self.cfg.additional_scopes.iter().any(|s| s == scope) {
            return Ok(json!({}));
        }
        let now = timestamp();
        let key = if self.cfg.method == "token" {
            auth_key(&self.cfg.token, now)
        } else {
            self.access_token().await?
        };
        Ok(json!({"privilege_key":key,"timestamp":now}))
    }
    async fn access_token(&self) -> Result<String> {
        if let Some(source) = &self.cfg.oidc.token_source {
            return source.resolve();
        }
        let mut cached = self.cached.lock().await;
        if let Some((token, until)) = &*cached {
            if *until > Instant::now() {
                return Ok(token.clone());
            }
        }
        let oidc = &self.cfg.oidc;
        let mut params = oidc.additional_endpoint_params.clone();
        params.insert("grant_type".into(), "client_credentials".into());
        if !oidc.audience.is_empty() {
            params.insert("audience".into(), oidc.audience.clone());
        }
        if !oidc.scope.is_empty() {
            params.insert("scope".into(), oidc.scope.clone());
        }
        let response = self
            .http
            .post(&oidc.token_endpoint_url)
            .basic_auth(&oidc.client_id, Some(&oidc.client_secret))
            .form(&params)
            .send()
            .await
            .context("OIDC token 请求失败")?;
        ensure!(
            response.status().is_success(),
            "OIDC token 端点返回 {}",
            response.status()
        );
        let body: Value = response.json().await.context("OIDC token 响应无效")?;
        let token = body["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("OIDC 响应缺少 access_token")?
            .to_owned();
        let ttl = body["expires_in"].as_u64().unwrap_or(60).min(86400);
        *cached = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(ttl.saturating_sub(10)),
        ));
        Ok(token)
    }
}
