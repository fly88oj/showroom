use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

pub type Labels = BTreeMap<String, String>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub server_addr: String,
    pub server_port: u16,
    pub user: String,
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub auth: Auth,
    pub transport: Transport,
    pub login_fail_exit: bool,
    pub udp_packet_size: usize,
    pub metadatas: Labels,
    pub start: Vec<String>,
    pub includes: Vec<String>,
    pub proxies: Vec<Proxy>,
    pub visitors: Vec<Visitor>,
    pub log: Log,
    pub web_server: WebServer,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            server_addr: "127.0.0.1".into(),
            server_port: 7000,
            user: String::new(),
            client_id: String::new(),
            auth: Auth::default(),
            transport: Transport::default(),
            login_fail_exit: true,
            udp_packet_size: 1500,
            metadatas: Labels::new(),
            start: vec![],
            includes: vec![],
            proxies: vec![],
            visitors: vec![],
            log: Log::default(),
            web_server: WebServer::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Transport {
    pub protocol: String,
    pub wire_protocol: String,
    pub tcp_mux: bool,
    pub tcp_mux_keepalive_interval: u64,
    pub pool_count: u32,
    pub dial_server_timeout: u64,
    pub dial_server_keepalive: u64,
    #[serde(rename = "connectServerLocalIP")]
    pub connect_server_local_ip: String,
    #[serde(rename = "proxyURL")]
    pub proxy_url: String,
    pub heartbeat_interval: i64,
    pub heartbeat_timeout: i64,
    pub tls: Tls,
    pub quic: Quic,
}
impl Default for Transport {
    fn default() -> Self {
        Self {
            protocol: "tcp".into(),
            wire_protocol: "v1".into(),
            tcp_mux: true,
            tcp_mux_keepalive_interval: 30,
            pool_count: 1,
            dial_server_timeout: 10,
            dial_server_keepalive: 30,
            connect_server_local_ip: String::new(),
            proxy_url: String::new(),
            heartbeat_interval: 30,
            heartbeat_timeout: 90,
            tls: Tls::default(),
            quic: Quic::default(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Quic {
    pub keepalive_period: u64,
    pub max_idle_timeout: u64,
    pub max_incoming_streams: u32,
}
impl Default for Quic {
    fn default() -> Self {
        Self {
            keepalive_period: 10,
            max_idle_timeout: 30,
            max_incoming_streams: 100000,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Tls {
    pub enable: bool,
    #[serde(rename = "disableCustomTLSFirstByte")]
    pub disable_custom_tls_first_byte: bool,
    pub cert_file: String,
    pub key_file: String,
    pub trusted_ca_file: String,
    pub server_name: String,
}
impl Default for Tls {
    fn default() -> Self {
        Self {
            enable: true,
            disable_custom_tls_first_byte: true,
            cert_file: String::new(),
            key_file: String::new(),
            trusted_ca_file: String::new(),
            server_name: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Auth {
    pub method: String,
    pub token: String,
    pub token_source: Option<ValueSource>,
    pub additional_scopes: Vec<String>,
    pub oidc: Oidc,
}
impl Default for Auth {
    fn default() -> Self {
        Self {
            method: "token".into(),
            token: String::new(),
            token_source: None,
            additional_scopes: vec![],
            oidc: Oidc::default(),
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ValueSource {
    pub r#type: String,
    pub file: FileSource,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FileSource {
    pub path: String,
}
impl ValueSource {
    pub fn resolve(&self) -> Result<String> {
        ensure!(self.r#type == "file", "tokenSource.type 仅支持 file");
        let value = std::fs::read_to_string(&self.file.path).context("读取 tokenSource 失败")?;
        ensure!(!value.trim().is_empty(), "tokenSource 文件为空");
        Ok(value.trim().to_owned())
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Oidc {
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub client_secret: String,
    pub audience: String,
    pub scope: String,
    #[serde(rename = "tokenEndpointURL")]
    pub token_endpoint_url: String,
    pub additional_endpoint_params: Labels,
    pub trusted_ca_file: String,
    pub insecure_skip_verify: bool,
    #[serde(rename = "proxyURL")]
    pub proxy_url: String,
    pub token_source: Option<ValueSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Proxy {
    pub name: String,
    pub r#type: String,
    pub enabled: bool,
    #[serde(rename = "localIP")]
    pub local_ip: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub custom_domains: Vec<String>,
    pub subdomain: String,
    pub locations: Vec<String>,
    pub http_user: String,
    pub http_password: String,
    pub host_header_rewrite: String,
    pub request_headers: Headers,
    pub response_headers: Headers,
    #[serde(rename = "routeByHTTPUser")]
    pub route_by_http_user: String,
    pub secret_key: String,
    pub allow_users: Vec<String>,
    pub multiplexer: String,
    pub transport: ProxyTransport,
    pub load_balancer: LoadBalancer,
    pub health_check: HealthCheck,
    pub metadatas: Labels,
    pub annotations: Labels,
    pub plugin: Plugin,
}
impl Default for Proxy {
    fn default() -> Self {
        Self {
            name: String::new(),
            r#type: "tcp".into(),
            enabled: true,
            local_ip: "127.0.0.1".into(),
            local_port: 0,
            remote_port: 0,
            custom_domains: vec![],
            subdomain: String::new(),
            locations: vec![],
            http_user: String::new(),
            http_password: String::new(),
            host_header_rewrite: String::new(),
            request_headers: Headers::default(),
            response_headers: Headers::default(),
            route_by_http_user: String::new(),
            secret_key: String::new(),
            allow_users: vec![],
            multiplexer: String::new(),
            transport: ProxyTransport::default(),
            load_balancer: LoadBalancer::default(),
            health_check: HealthCheck::default(),
            metadatas: Labels::new(),
            annotations: Labels::new(),
            plugin: Plugin::default(),
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Headers {
    pub set: Labels,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadBalancer {
    pub group: String,
    pub group_key: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyTransport {
    pub use_encryption: bool,
    pub use_compression: bool,
    pub bandwidth_limit: String,
    pub bandwidth_limit_mode: String,
    pub proxy_protocol_version: String,
}
impl Default for ProxyTransport {
    fn default() -> Self {
        Self {
            use_encryption: false,
            use_compression: false,
            bandwidth_limit: String::new(),
            bandwidth_limit_mode: "client".into(),
            proxy_protocol_version: String::new(),
        }
    }
}
impl ProxyTransport {
    pub fn bandwidth_bytes(&self) -> Result<u64> {
        if self.bandwidth_limit.is_empty() {
            return Ok(0);
        }
        let (n, factor) = if let Some(n) = self.bandwidth_limit.strip_suffix("MB") {
            (n, 1024.0 * 1024.0)
        } else if let Some(n) = self.bandwidth_limit.strip_suffix("KB") {
            (n, 1024.0)
        } else {
            bail!("bandwidthLimit 必须以 KB 或 MB 结尾")
        };
        let n: f64 = n.trim().parse().context("bandwidthLimit 数值无效")?;
        ensure!(
            n.is_finite() && n > 0.0 && n * factor >= 1.0 && n * factor <= 1e12,
            "bandwidthLimit 超出范围"
        );
        Ok((n * factor) as u64)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct HealthCheck {
    pub r#type: String,
    pub timeout_seconds: u64,
    pub max_failed: u32,
    pub interval_seconds: u64,
    pub path: String,
    pub http_headers: Vec<HttpHeader>,
}
impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            r#type: String::new(),
            timeout_seconds: 3,
            max_failed: 1,
            interval_seconds: 10,
            path: "/".into(),
            http_headers: vec![],
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Plugin {
    pub r#type: String,
    pub username: String,
    pub password: String,
    pub http_user: String,
    pub http_password: String,
    pub local_path: String,
    pub strip_prefix: String,
    pub unix_path: String,
    pub local_addr: String,
    pub crt_path: String,
    pub key_path: String,
    pub host_header_rewrite: String,
    pub request_headers: Headers,
    #[serde(rename = "enableHTTP2")]
    pub enable_http2: Option<bool>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Visitor {
    pub name: String,
    pub r#type: String,
    pub enabled: bool,
    pub server_name: String,
    pub server_user: String,
    pub secret_key: String,
    pub bind_addr: String,
    pub bind_port: u16,
    pub transport: VisitorTransport,
}
impl Default for Visitor {
    fn default() -> Self {
        Self {
            name: String::new(),
            r#type: "stcp".into(),
            enabled: true,
            server_name: String::new(),
            server_user: String::new(),
            secret_key: String::new(),
            bind_addr: "127.0.0.1".into(),
            bind_port: 0,
            transport: VisitorTransport::default(),
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct VisitorTransport {
    pub use_encryption: bool,
    pub use_compression: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Log {
    pub to: String,
    pub level: String,
    pub max_days: usize,
    pub disable_print_color: bool,
}
impl Default for Log {
    fn default() -> Self {
        Self {
            to: "console".into(),
            level: "info".into(),
            max_days: 3,
            disable_print_color: false,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WebServer {
    pub addr: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}
impl Default for WebServer {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1".into(),
            port: 0,
            user: String::new(),
            password: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut c = Self::parse(path)?;
        let base = path.parent().unwrap_or(Path::new("."));
        let mut included = HashSet::new();
        for pattern in c.includes.clone() {
            let pattern = base.join(pattern);
            let mut paths = glob::glob(pattern.to_str().context("配置路径非 UTF-8")?)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            paths.sort();
            ensure!(
                !paths.is_empty(),
                "includes 未匹配文件: {}",
                pattern.display()
            );
            for file in paths {
                let real = file.canonicalize()?;
                ensure!(real != path.canonicalize()?, "includes 不能包含主配置");
                if included.insert(real) {
                    let extra = Self::parse(&file)?;
                    ensure!(extra.includes.is_empty(), "不支持嵌套 includes");
                    c.proxies.extend(extra.proxies);
                    c.visitors.extend(extra.visitors);
                }
            }
        }
        c.validate()?;
        Ok(c)
    }
    fn parse(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置 {}", path.display()))?;
        let re = regex::Regex::new(r"\{\{\s*\.Envs\.([A-Za-z_][A-Za-z0-9_]*)\s*\}\}")?;
        for cap in re.captures_iter(&raw) {
            std::env::var(&cap[1]).with_context(|| format!("缺少环境变量 {}", &cap[1]))?;
        }
        let rendered = re.replace_all(&raw, |caps: &regex::Captures| {
            std::env::var(&caps[1]).unwrap_or_default()
        });
        ensure!(
            !rendered.contains("{{"),
            "仅支持 {{{{ .Envs.NAME }}}} 环境变量模板"
        );
        let mut c: Self = match path.extension().and_then(|s| s.to_str()) {
            Some("json") => serde_json::from_str(&rendered).context("JSON 配置无效")?,
            Some("yaml" | "yml") => serde_yaml::from_str(&rendered).context("YAML 配置无效")?,
            Some("ini") => bail!("请将旧版 INI 配置转换为 TOML、JSON 或 YAML"),
            _ => toml::from_str(&rendered).map_err(|e| {
                anyhow::anyhow!("TOML 配置无效，位置 {:?}: {}", e.span(), e.message())
            })?,
        };
        let base = path.parent().unwrap_or(Path::new("."));
        let resolve = |s: &mut String| {
            if !s.is_empty() && Path::new(s).is_relative() {
                *s = base.join(&*s).to_string_lossy().into_owned();
            }
        };
        resolve(&mut c.transport.tls.cert_file);
        resolve(&mut c.transport.tls.key_file);
        resolve(&mut c.transport.tls.trusted_ca_file);
        resolve(&mut c.auth.oidc.trusted_ca_file);
        for source in [&mut c.auth.token_source, &mut c.auth.oidc.token_source]
            .into_iter()
            .flatten()
        {
            resolve(&mut source.file.path);
        }
        for p in &mut c.proxies {
            resolve(&mut p.plugin.local_path);
            resolve(&mut p.plugin.unix_path);
            resolve(&mut p.plugin.crt_path);
            resolve(&mut p.plugin.key_path);
        }
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.server_addr.is_empty() && self.server_port > 0,
            "serverAddr/serverPort 无效"
        );
        ensure!(
            matches!(
                self.transport.protocol.as_str(),
                "tcp" | "quic" | "websocket" | "wss"
            ),
            "暂不支持 transport.protocol={}",
            self.transport.protocol
        );
        ensure!(
            self.transport.wire_protocol == "v1",
            "目前支持 wireProtocol=v1"
        );
        ensure!(
            self.transport.dial_server_timeout > 0 && self.transport.tcp_mux_keepalive_interval > 0,
            "连接超时和多路复用心跳间隔必须大于 0"
        );
        ensure!(
            self.transport.heartbeat_interval > 0 || self.transport.heartbeat_interval == -1,
            "heartbeatInterval 必须为正数或 -1"
        );
        if self.transport.heartbeat_interval > 0 {
            ensure!(
                self.transport.heartbeat_timeout > self.transport.heartbeat_interval,
                "heartbeatTimeout 必须大于 heartbeatInterval"
            );
        }
        ensure!(
            self.transport.heartbeat_interval > 0
                || self.transport.tcp_mux
                || self.transport.protocol == "quic",
            "关闭心跳时必须启用 tcpMux 或 QUIC"
        );
        ensure!(
            self.transport.quic.max_idle_timeout > 0,
            "QUIC 空闲超时必须大于 0"
        );
        ensure!(
            self.transport.tls.cert_file.is_empty() == self.transport.tls.key_file.is_empty(),
            "TLS certFile/keyFile 必须成对配置"
        );
        ensure!(
            (1..=6500).contains(&self.udp_packet_size),
            "udpPacketSize 必须在 1..6500 内（受 v1 消息长度限制）"
        );
        ensure!(
            matches!(self.auth.method.as_str(), "token" | "oidc"),
            "auth.method 无效"
        );
        ensure!(
            self.auth.token.is_empty() || self.auth.token_source.is_none(),
            "token 与 tokenSource 互斥"
        );
        for source in [&self.auth.token_source, &self.auth.oidc.token_source]
            .into_iter()
            .flatten()
        {
            ensure!(
                source.r#type == "file" && !source.file.path.is_empty(),
                "tokenSource 需要 type=file 和 file.path"
            );
        }
        for s in &self.auth.additional_scopes {
            ensure!(
                matches!(s.as_str(), "HeartBeats" | "NewWorkConns"),
                "additionalScopes 无效: {s}"
            );
        }
        if self.auth.method == "oidc" && self.auth.oidc.token_source.is_none() {
            ensure!(
                !self.auth.oidc.client_id.is_empty()
                    && !self.auth.oidc.token_endpoint_url.is_empty(),
                "OIDC 缺少 clientID/tokenEndpointURL"
            );
        }
        if self.transport.protocol == "quic" {
            ensure!(self.transport.proxy_url.is_empty(), "QUIC 不支持 proxyURL");
        }
        if !self.transport.proxy_url.is_empty() {
            let u = url::Url::parse(&self.transport.proxy_url)?;
            ensure!(
                matches!(u.scheme(), "http" | "socks5"),
                "proxyURL 仅支持 http/socks5"
            );
        }
        let mut names = HashSet::new();
        for p in &self.proxies {
            ensure!(
                !p.name.is_empty() && names.insert(&p.name),
                "代理或访问者名称为空/重复: {}",
                p.name
            );
            p.validate().with_context(|| format!("代理 {}", p.name))?;
        }
        for v in &self.visitors {
            ensure!(
                !v.name.is_empty() && names.insert(&v.name),
                "代理或访问者名称为空/重复: {}",
                v.name
            );
            ensure!(
                matches!(v.r#type.as_str(), "stcp" | "sudp"),
                "暂不支持访问者类型 {}",
                v.r#type
            );
            ensure!(
                !v.server_name.is_empty() && v.bind_port > 0 && !v.secret_key.is_empty(),
                "访问者 {} 缺少 serverName/bindPort/secretKey",
                v.name
            );
        }
        for s in &self.start {
            ensure!(names.contains(s), "start 引用不存在的名称: {s}");
        }
        if self.web_server.port > 0 {
            ensure!(
                self.web_server.user.is_empty() == self.web_server.password.is_empty(),
                "webServer user/password 必须成对配置"
            );
            let local = self
                .web_server
                .addr
                .parse::<std::net::IpAddr>()
                .map(|a| a.is_loopback())
                .unwrap_or(self.web_server.addr == "localhost");
            ensure!(
                local || !self.web_server.password.is_empty(),
                "非回环地址的管理接口必须配置用户名和密码"
            );
        }
        Ok(())
    }
    pub fn active(&self, name: &str, enabled: bool) -> bool {
        enabled && (self.start.is_empty() || self.start.iter().any(|n| n == name))
    }
    pub fn validate_resources(&self) -> Result<()> {
        crate::auth::Authenticator::new(&self.auth)?;
        if self.transport.tls.enable || matches!(self.transport.protocol.as_str(), "wss" | "quic") {
            crate::transport::tls_config(&self.transport.tls)?;
        }
        for p in &self.proxies {
            if matches!(
                p.plugin.r#type.as_str(),
                "tls2raw" | "https2http" | "https2https"
            ) {
                rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(
                    crate::transport::certificates(&p.plugin.crt_path)?,
                    crate::transport::private_key(&p.plugin.key_path)?,
                )?;
            }
        }
        Ok(())
    }
    pub fn proxy_name(&self, name: &str) -> String {
        prefix(&self.user, name)
    }
}
pub fn prefix(user: &str, name: &str) -> String {
    if user.is_empty() {
        name.into()
    } else {
        format!("{user}.{name}")
    }
}
impl Proxy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.r#type.as_str(),
                "tcp" | "udp" | "http" | "https" | "stcp" | "sudp" | "tcpmux"
            ),
            "暂不支持代理类型 {}",
            self.r#type
        );
        ensure!(
            self.plugin.r#type.is_empty() || !matches!(self.r#type.as_str(), "udp" | "sudp"),
            "UDP 不支持客户端插件"
        );
        ensure!(
            self.local_port > 0 || !self.plugin.r#type.is_empty(),
            "需要 localPort 或 plugin"
        );
        if matches!(self.r#type.as_str(), "http" | "https" | "tcpmux") {
            ensure!(
                !self.custom_domains.is_empty() || !self.subdomain.is_empty(),
                "需要 customDomains/subdomain"
            );
        }
        if matches!(self.r#type.as_str(), "stcp" | "sudp") {
            ensure!(!self.secret_key.is_empty(), "需要 secretKey");
        }
        if self.r#type == "tcpmux" {
            ensure!(
                self.multiplexer == "httpconnect",
                "tcpmux 需要 multiplexer=httpconnect"
            );
        }
        ensure!(
            matches!(
                self.transport.bandwidth_limit_mode.as_str(),
                "client" | "server"
            ),
            "bandwidthLimitMode 无效"
        );
        self.transport.bandwidth_bytes()?;
        ensure!(
            matches!(
                self.transport.proxy_protocol_version.as_str(),
                "" | "v1" | "v2"
            ),
            "proxyProtocolVersion 无效"
        );
        ensure!(
            !(matches!(self.r#type.as_str(), "udp" | "sudp")
                && self.transport.proxy_protocol_version == "v1"),
            "UDP 仅支持 PROXY protocol v2"
        );
        ensure!(
            matches!(self.health_check.r#type.as_str(), "" | "tcp" | "http"),
            "healthCheck.type 无效"
        );
        ensure!(
            self.health_check.interval_seconds > 0
                && self.health_check.timeout_seconds > 0
                && self.health_check.max_failed > 0,
            "healthCheck 时间/次数必须大于 0"
        );
        ensure!(
            matches!(
                self.plugin.r#type.as_str(),
                "" | "socks5"
                    | "http_proxy"
                    | "static_file"
                    | "unix_domain_socket"
                    | "tls2raw"
                    | "http2http"
                    | "http2https"
                    | "https2http"
                    | "https2https"
            ),
            "暂不支持插件 {}",
            self.plugin.r#type
        );
        match self.plugin.r#type.as_str() {
            "static_file" => ensure!(
                PathBuf::from(&self.plugin.local_path).is_dir(),
                "static_file.localPath 不是有效目录"
            ),
            "unix_domain_socket" => {
                ensure!(cfg!(unix), "Windows 不支持 unix_domain_socket");
                ensure!(!self.plugin.unix_path.is_empty(), "缺少 unixPath");
            }
            "tls2raw" | "https2http" | "https2https" => ensure!(
                !self.plugin.local_addr.is_empty()
                    && !self.plugin.crt_path.is_empty()
                    && !self.plugin.key_path.is_empty(),
                "TLS 插件缺少 localAddr/crtPath/keyPath"
            ),
            "http2http" | "http2https" => ensure!(
                !self.plugin.local_addr.is_empty(),
                "HTTP 插件缺少 localAddr"
            ),
            _ => {}
        }
        ensure!(
            self.plugin.username.is_empty() == self.plugin.password.is_empty(),
            "plugin username/password 必须成对配置"
        );
        ensure!(
            self.plugin.http_user.is_empty() == self.plugin.http_password.is_empty(),
            "plugin httpUser/httpPassword 必须成对配置"
        );
        Ok(())
    }
    pub fn registration(&self, name: &str) -> Value {
        json!({"proxy_name":name,"proxy_type":self.r#type,"remote_port":self.remote_port,
        "use_encryption":self.transport.use_encryption,"use_compression":self.transport.use_compression,
        "bandwidth_limit":self.transport.bandwidth_limit,"bandwidth_limit_mode":self.transport.bandwidth_limit_mode,
        "group":self.load_balancer.group,"group_key":self.load_balancer.group_key,"metas":self.metadatas,"annotations":self.annotations,
        "custom_domains":self.custom_domains,"subdomain":self.subdomain,"locations":self.locations,
        "http_user":self.http_user,"http_pwd":self.http_password,"host_header_rewrite":self.host_header_rewrite,
        "headers":self.request_headers.set,"response_headers":self.response_headers.set,"route_by_http_user":self.route_by_http_user,
        "sk":self.secret_key,"allow_users":self.allow_users,"multiplexer":self.multiplexer})
    }
}
