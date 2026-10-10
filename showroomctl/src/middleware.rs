//! middleware：host.env 期望状态与中间件集合编排（plan-15 §2.1 / SPEC §15）。
//! HostEnv 纯函数（parse/to_env/base_url）+ fs 薄层（load/save）；容器编排与扩容状态机在后续段落。
//! host.env 不含业务限额（分层论证见 plan-15 §1：限额归 policy.env）；
//! 但含 frp 凭据（补充需求：FRP_TOKEN/FRP_TUNNEL），故整文件 0600（root）。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::dockerops;
use crate::state::{self, ApiError, R};

/// 中间件名单（host.env MIDDLEWARE 的合法成员）。
pub const MW_NAMES: [&str; 3] = ["postgres", "redis", "registry"];

/// host.env 文件位置：`state/host.env`（setup 首跑生成；plan-15 §1）。
pub fn host_env_path(root: &Path) -> PathBuf {
    root.join("state/host.env")
}

/// frp 通道形态（补充需求①：FRP=none|chmlfrp）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrpMode {
    None,
    Chmlfrp,
}

impl FrpMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            FrpMode::None => "none",
            FrpMode::Chmlfrp => "chmlfrp",
        }
    }

    pub fn parse(s: &str) -> R<Self> {
        match s {
            "none" => Ok(FrpMode::None),
            "chmlfrp" => Ok(FrpMode::Chmlfrp),
            _ => Err(ApiError::new("E_NAME", format!("FRP 须为 none/chmlfrp（got '{s}'）"))),
        }
    }
}

/// 主机期望状态（plan-15 §1 键表 + frp 三键）。
/// 不含任何业务限额/秘密之外的设施开关；pg 超级用户密码在 state/middleware.cred（0600）。
#[derive(Debug, Clone, PartialEq)]
pub struct HostEnv {
    /// MIDDLEWARE：空格集合 ⊆ {postgres,redis,registry}
    pub middleware: BTreeSet<String>,
    /// CADDY=yes/no：安装(pacman)+渲染 Caddyfile
    pub caddy: bool,
    /// site A 站点地址（Caddyfile 渲染）
    pub lan_host: String,
    /// site B 站点地址；空=纯内网主机（不渲染 site B）
    pub public_host: String,
    /// TUNING=sysctl：内核调优
    pub tuning: bool,
    /// DEDUP=weekly：周日 05:00 duperemove timer
    pub dedup: bool,
    /// FRP=chmlfrp：frpc 自举（二进制下载 + chmlfrp.service）
    pub frp: FrpMode,
    /// ChmlFrp usertoken（凭据：host.env 整文件 0600）
    pub frp_token: String,
    /// ChmlFrp 隧道 ID
    pub frp_tunnel: String,
}

impl HostEnv {
    /// 缺省形状（plan-15 §1 默认列）：全部中间件、开 caddy/调优/dedup、无 frp。
    pub fn default_for(lan: &str) -> Self {
        HostEnv {
            middleware: MW_NAMES.iter().map(|s| s.to_string()).collect(),
            caddy: true,
            lan_host: lan.to_string(),
            public_host: String::new(),
            tuning: true,
            dedup: true,
            frp: FrpMode::None,
            frp_token: String::new(),
            frp_tunnel: String::new(),
        }
    }

    /// 纯解析（规则 plan-15 §1）：坏值 → E_NAME 且指名键；未知键 → warning 不失败（前向兼容）。
    /// 缺键取默认（LAN_HOST 缺省 127.0.0.1；load 的缺文件路径另以探测值生成）。
    pub fn parse(text: &str) -> R<Self> {
        let mut out = Self::default_for("127.0.0.1");
        for (k, v) in state::parse_kv(text) {
            match k.as_str() {
                "MIDDLEWARE" => {
                    let mut set = BTreeSet::new();
                    for name in v.split_whitespace() {
                        validate_mw(name).map_err(|e| ApiError::new("E_NAME", format!("MIDDLEWARE: {}", e.msg)))?;
                        set.insert(name.to_string());
                    }
                    out.middleware = set;
                }
                "CADDY" => {
                    out.caddy = match v.as_str() {
                        "yes" => true,
                        "no" => false,
                        _ => {
                            return Err(ApiError::new(
                                "E_NAME",
                                format!("CADDY 须为 yes/no（got '{v}'）"),
                            ))
                        }
                    };
                }
                "LAN_HOST" => out.lan_host = v,
                "PUBLIC_HOST" => out.public_host = v,
                "TUNING" => {
                    out.tuning = match v.as_str() {
                        "sysctl" => true,
                        "none" => false,
                        _ => {
                            return Err(ApiError::new(
                                "E_NAME",
                                format!("TUNING 须为 sysctl/none（got '{v}'）"),
                            ))
                        }
                    };
                }
                "DEDUP" => {
                    out.dedup = match v.as_str() {
                        "weekly" => true,
                        "none" => false,
                        _ => {
                            return Err(ApiError::new(
                                "E_NAME",
                                format!("DEDUP 须为 weekly/none（got '{v}'）"),
                            ))
                        }
                    };
                }
                "FRP" => out.frp = FrpMode::parse(&v)?,
                "FRP_TOKEN" => out.frp_token = v,
                "FRP_TUNNEL" => out.frp_tunnel = v,
                unknown => eprintln!("showroomctl: host.env: 未知键 {unknown}（忽略）"),
            }
        }
        Ok(out)
    }

    /// 键序固定的序列化（幂等写盘前提）；MIDDLEWARE 按字典序（集合语义，与书写顺序无关）。
    pub fn to_env(&self) -> String {
        format!(
            "MIDDLEWARE={}\nCADDY={}\nLAN_HOST={}\nPUBLIC_HOST={}\nTUNING={}\nDEDUP={}\nFRP={}\nFRP_TOKEN={}\nFRP_TUNNEL={}\n",
            self.middleware.iter().cloned().collect::<Vec<_>>().join(" "),
            if self.caddy { "yes" } else { "no" },
            self.lan_host,
            self.public_host,
            if self.tuning { "sysctl" } else { "none" },
            if self.dedup { "weekly" } else { "none" },
            self.frp.as_str(),
            self.frp_token,
            self.frp_tunnel,
        )
    }

    /// 读盘；缺文件 → default_for(first_lan_ip())（首跑，不落盘——写盘是 setup 的 WriteFile Action）。
    /// 坏文件如实告警并按缺省形状处理（setup diff 会显示将重写的 host.env）。
    pub fn load(root: &Path) -> Self {
        match fs::read_to_string(host_env_path(root)) {
            Ok(t) => match Self::parse(&t) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("showroomctl: host.env 解析失败，按缺省形状处理: {e}");
                    Self::default_for(&first_lan_ip())
                }
            },
            Err(_) => Self::default_for(&first_lan_ip()),
        }
    }

    pub fn save(&self, root: &Path) -> R<()> {
        fs::create_dir_all(root.join("state"))?;
        state::write_private(&host_env_path(root), &self.to_env())
    }

    /// BASE_URL 派生自 LAN_HOST（plan-15 §1：不单设键）。
    pub fn base_url(&self) -> String {
        format!("http://{}", self.lan_host)
    }

    /// GET /_api/admin/host 的 host_env 视图；FRP_TOKEN 打码（不回显凭据）。
    pub fn to_json(&self) -> Value {
        json!({
            "MIDDLEWARE": self.middleware.iter().cloned().collect::<Vec<_>>(),
            "CADDY": self.caddy,
            "LAN_HOST": self.lan_host,
            "PUBLIC_HOST": self.public_host,
            "TUNING": if self.tuning { "sysctl" } else { "none" },
            "DEDUP": if self.dedup { "weekly" } else { "none" },
            "FRP": self.frp.as_str(),
            "FRP_TUNNEL": self.frp_tunnel,
            "FRP_TOKEN": if self.frp_token.is_empty() { "" } else { "<set>" },
        })
    }
}

/// `hostname -I` 首地址（探测失败回落 127.0.0.1）。
pub fn first_lan_ip() -> String {
    dockerops::run_tolerant("hostname", &["-I"])
        .split_whitespace()
        .next()
        .unwrap_or("127.0.0.1")
        .to_string()
}

/// 中间件成员校验：∈ {postgres,redis,registry}，否则 E_NAME（plan-15 §2.1）。
pub fn validate_mw(name: &str) -> R<()> {
    if MW_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(ApiError::new(
            "E_NAME",
            format!("unknown middleware '{name}'（合法：postgres/redis/registry）"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn troot(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("srctl-mw-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mwset(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // host.env 解析：缺省（空文本）→ 全默认形状（plan-15 §1 默认列 + FRP=none）
    #[test]
    fn hostenv_parse_defaults() {
        let d = HostEnv::parse("").unwrap();
        assert_eq!(d, HostEnv::default_for("127.0.0.1"));
        assert_eq!(d.middleware, mwset(&["postgres", "redis", "registry"]));
        assert!(d.caddy && d.tuning && d.dedup);
        assert_eq!((d.lan_host.as_str(), d.public_host.as_str()), ("127.0.0.1", ""));
        assert_eq!(d.frp, FrpMode::None);
        assert_eq!((d.frp_token.as_str(), d.frp_tunnel.as_str()), ("", ""));
    }

    // 全键 + 非默认值（含 frp 三键）
    #[test]
    fn hostenv_parse_full() {
        let h = HostEnv::parse(
            "MIDDLEWARE=postgres registry\nCADDY=no\nLAN_HOST=10.1.2.3\nPUBLIC_HOST=pub.example\n\
             TUNING=none\nDEDUP=none\nFRP=chmlfrp\nFRP_TOKEN=tok123\nFRP_TUNNEL=t-456\n",
        )
        .unwrap();
        assert!(!h.caddy && !h.tuning && !h.dedup);
        assert_eq!(h.lan_host, "10.1.2.3");
        assert_eq!(h.public_host, "pub.example");
        assert_eq!(h.middleware, mwset(&["postgres", "registry"]));
        assert_eq!(h.base_url(), "http://10.1.2.3");
        assert_eq!(h.frp, FrpMode::Chmlfrp);
        assert_eq!((h.frp_token.as_str(), h.frp_tunnel.as_str()), ("tok123", "t-456"));
    }

    // 坏值 → E_NAME 且指名键；未知键不失败（前向兼容）
    #[test]
    fn hostenv_bad_values_and_unknown_keys() {
        for (text, key) in [
            ("MIDDLEWARE=mysql\n", "MIDDLEWARE"),
            ("CADDY=true\n", "CADDY"),
            ("TUNING=bbr\n", "TUNING"),
            ("DEDUP=daily\n", "DEDUP"),
            ("FRP=frps\n", "FRP"),
        ] {
            let e = HostEnv::parse(text).unwrap_err();
            assert_eq!(e.code, "E_NAME", "{text}");
            assert!(e.msg.contains(key), "msg 指名键 {key}: {}", e.msg);
        }
        assert!(HostEnv::parse("SOMETHING_NEW=1\n").is_ok(), "未知键不失败");
        // MIDDLEWARE 部分坏名也拒（指名集合键）
        assert!(HostEnv::parse("MIDDLEWARE=redis mysql\n").is_err());
    }

    // to_env↔parse 往返 + 键序固定 + MIDDLEWARE 集合与书写顺序无关
    #[test]
    fn hostenv_roundtrip_and_order() {
        let h = HostEnv::default_for("192.168.1.9");
        let t = h.to_env();
        assert_eq!(HostEnv::parse(&t).unwrap(), h);
        // 键序固定（幂等写盘前提）；注释行（hw 推导审计行）不碍解析
        let keys: Vec<&str> = t
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split('=').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "MIDDLEWARE", "CADDY", "LAN_HOST", "PUBLIC_HOST", "TUNING", "DEDUP", "FRP",
                "FRP_TOKEN", "FRP_TUNNEL"
            ]
        );
        let with_comment = format!("# 由 hw_defaults(12G/953G/4 核) 推导于 2026-10-11\n{t}");
        assert_eq!(HostEnv::parse(&with_comment).unwrap(), h);

        // 集合顺序无关 → 同一 HostEnv；to_env 输出字典序稳定形态
        let a = HostEnv::parse("MIDDLEWARE=redis postgres registry\n").unwrap();
        let b = HostEnv::parse("MIDDLEWARE=registry redis postgres\n").unwrap();
        let c = HostEnv::parse("MIDDLEWARE=postgres redis registry\n").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.to_env().starts_with("MIDDLEWARE=postgres redis registry\n"));
    }

    // load/save 落盘往返（0600；缺文件走探测缺省不落盘）
    #[test]
    fn hostenv_save_load_roundtrip() {
        let root = troot("roundtrip");
        let h = HostEnv {
            middleware: mwset(&["postgres"]),
            caddy: false,
            lan_host: "10.0.0.1".into(),
            public_host: String::new(),
            tuning: false,
            dedup: false,
            frp: FrpMode::Chmlfrp,
            frp_token: "tk".into(),
            frp_tunnel: "tid".into(),
        };
        h.save(&root).unwrap();
        assert_eq!(HostEnv::load(&root), h);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(host_env_path(&root)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "host.env 含 frp 凭据，整文件 0600");
    }

    // validate_mw：三名合法；大小写/未知拒绝
    #[test]
    fn validate_mw_names() {
        for good in MW_NAMES {
            assert!(validate_mw(good).is_ok(), "{good}");
        }
        for bad in ["mysql", "", "Postgres", "REDIS", "pg"] {
            assert_eq!(validate_mw(bad).unwrap_err().code, "E_NAME", "{bad:?}");
        }
    }
}
