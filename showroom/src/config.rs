//! ~/.showroom/config.toml（0600）：user（账号）、密钥路径、双服务端地址、registry 凭据。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix('~') {
        let rest = rest.trim_start_matches('/');
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(p)
}

pub fn config_path() -> PathBuf {
    expand("~/.showroom/config.toml")
}

fn default_key_path() -> String {
    "~/.showroom/id_ed25519".to_string()
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// 发布者账号。磁盘键更名前为 `owner =`（serde alias 兼容读取旧配置；保存一律写 `user =`）。
    #[serde(default, alias = "owner")]
    pub user: String,
    #[serde(default = "default_key_path")]
    pub key_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_lan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_public: Option<String>,
    #[serde(default)]
    pub registry: Registry,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            user: String::new(),
            key_path: default_key_path(),
            server_lan: None,
            server_public: None,
            registry: Registry::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Config, String> {
        Config::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Config, String> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("解析 {} 失败：{e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("读取 {} 失败：{e}", path.display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let s = toml::to_string_pretty(self).map_err(|e| format!("序列化失败：{e}"))?;
        showroom_common::write_private(path, s.as_bytes())
            .map_err(|e| format!("写 {} 失败：{e}", path.display()))
    }

    pub fn key_file(&self) -> PathBuf {
        expand(&self.key_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn tmpdir() -> PathBuf {
        // 每次调用独立目录（调用序号隔离）：并行测试下固定名会互相 remove_dir_all（存量 flaky）
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("showroom-cfg-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn roundtrip_and_0600() {
        let d = tmpdir();
        let p = d.join("config.toml");
        let mut c = Config {
            user: "alice".into(),
            server_lan: Some("http://your-host.example.com/".into()),
            ..Config::default()
        };
        c.registry.user = "alice".into();
        c.registry.password = "s3cret".into();
        c.save_to(&p).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let c2 = Config::load_from(&p).unwrap();
        assert_eq!(c, c2);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load_from(Path::new("/nonexistent/showroom-test-config.toml")).unwrap();
        assert_eq!(c.user, "");
        assert_eq!(c.key_path, "~/.showroom/id_ed25519");
        assert_eq!(c.server_lan, None);
        assert_eq!(c.registry, Registry::default());
    }

    #[test]
    fn parses_spec_example() {
        let raw = r#"
user = "alice"
key_path = "~/.showroom/id_ed25519"
server_lan = "http://your-host.example.com"
server_public = ""

[registry]
user = "alice"
password = "pw"
"#;
        let c: Config = toml::from_str(raw).unwrap();
        assert_eq!(c.user, "alice");
        assert_eq!(c.server_lan.as_deref(), Some("http://your-host.example.com"));
        assert_eq!(c.server_public.as_deref(), Some(""));
        assert_eq!(c.registry.password, "pw");
    }

    // owner→user 更名兼容：旧配置文件的 `owner =` 键仍可读（serde alias）；保存后翻写为 `user =`
    #[test]
    fn legacy_owner_key_still_parses() {
        let c: Config = toml::from_str("owner = \"alice\"\n").unwrap();
        assert_eq!(c.user, "alice");
        let d = tmpdir();
        let p = d.join("legacy-config.toml");
        c.save_to(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("user = \"alice\"") && !text.contains("owner ="), "{text}");
        let _ = fs::remove_dir_all(&d);
    }
}
