//! chmlfrp：面板 getcfg 配置源（frp 客户端配置的 chmlfrp 形态）。
//!
//! 契约来自魔改 frpc 第一手源码（TechCat-Team/ChmlFrp-Frp，上游 fork）：
//! `GET https://cf-v2.uapis.cn/cfg?action=getcfg&token=<usertoken>&id=<id[,id…]>`
//! → `{"success":bool,"message":str,"cfg":str}`，`cfg` 为一段**老式 frpc ini**
//!（[common] + 每隧道一节）；线协议为原版 frp（社区以原版 v0.70.1 实证可用）。
//! 本模块只取我们所需的最小字段面；未知键忽略（面板加键不致失败），关键键缺失/坏值
//! → E_NAME 指名键。`use_encryption/use_compression` 原样透传（面板侧开关与
//! frps 的 checkproxy 上报一致；平台信封层在其上独立工作，两者不冲突）。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::state::{ApiError, R};

pub const API_URL: &str = "https://cf-v2.uapis.cn/cfg";

#[derive(Deserialize)]
struct GetCfgResp {
    success: bool,
    #[serde(default)]
    message: String,
    /// 失败响应无此字段（实测：面板恒 HTTP 200，成败只看 success 旗标）——必选会先炸
    /// 反序列化、吞掉面板的清晰拒绝原因（真机实弹教训）。
    #[serde(default)]
    cfg: Option<String>,
}

/// 拉取单条隧道的面板 ini（getcfg，**每 ID 一次调用**）。
/// 实测：多 ID 合并调用仅支持同节点隧道（跨节点报「所有隧道必须属于同一节点」）——
/// 故多隧道=每 ID 独立会话（见 tunnel::supervise），不做合并调用。
pub async fn fetch_ini(token: &str, id: &str) -> Result<String, String> {
    if token.trim().is_empty() || id.trim().is_empty() {
        return Err("FRP_TOKEN/FRP_TUNNEL 为空".into());
    }
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("http 客户端构建: {e}"))?
        .get(API_URL)
        .query(&[("action", "getcfg"), ("token", token), ("id", id)])
        .send()
        .await
        .map_err(|e| format!("请求 cf-v2.uapis.cn: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("读响应体（{status}）: {e}"))?;
    let body: GetCfgResp = serde_json::from_str(&text).map_err(|e| format!("响应解析（{status}）: {e}；响应体前 120 字: {}", text.chars().take(120).collect::<String>()))?;
    if !body.success {
        return Err(format!("面板拒绝（隧道 {id}）: {}", body.message));
    }
    body.cfg.ok_or_else(|| format!("面板 success 但缺 cfg 字段（隧道 {id}）"))
}

/// 宽容的老式 ini 解析：[节] / key = value / ; 与 # 注释行 / 空行；内联注释不剥离
///（值可能含 #；老 frpc 同语义）。返回 (节名, 键值表) 序列，保持出现顺序。
pub fn parse_ini(text: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let mut out: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    let mut cur: Option<usize> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name.trim().to_string(), BTreeMap::new()));
            cur = Some(out.len() - 1);
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let idx = match cur {
                Some(i) => i,
                None => continue, // 节外键值：忽略（非合法 frpc ini 形状）
            };
            out[idx].1.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

fn need<'a>(sec: &'a str, kv: &'a BTreeMap<String, String>, key: &str) -> R<&'a str> {
    kv.get(key)
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::new("E_NAME", format!("getcfg ini：[{sec}] 缺 {key}")))
}

fn need_u16(sec: &str, kv: &BTreeMap<String, String>, key: &str) -> R<u16> {
    need(sec, kv, key)?
        .parse::<u16>()
        .map_err(|_| ApiError::new("E_NAME", format!("[{sec}] {key} 非 u16")))
}

fn opt_bool(kv: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    kv.get(key).map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(default)
}

/// 面板 ini → 客户端配置（local 暂按面板值解析，随后由 mod::resolve 统一改指网关）。
/// 非 tcp 节跳过（返回名列表供日志）；[common] 的 protocol 仅接受 tcp。
pub fn ini_to_config(ini: &str) -> R<(frpc_rust::config::Config, Vec<String>)> {
    let sections = parse_ini(ini);
    let common = sections
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("common"))
        .map(|(_, kv)| kv)
        .ok_or_else(|| ApiError::new("E_NAME", "getcfg ini：缺 [common]"))?;

    let mut cfg = frpc_rust::config::Config {
        server_addr: need("common", common, "server_addr")?.to_string(),
        server_port: need_u16("common", common, "server_port")?,
        auth: frpc_rust::config::Auth {
            token: need("common", common, "token")?.to_string(),
            ..Default::default()
        },
        user: common.get("user").cloned().unwrap_or_default(),
        ..Default::default()
    };
    let protocol = common.get("protocol").cloned().unwrap_or_else(|| "tcp".into());
    if protocol != "tcp" {
        return Err(ApiError::new(
            "E_NAME",
            format!("getcfg ini：[common] protocol={protocol}（仅支持 tcp）"),
        ));
    }
    // TLS：面板/魔改 frpc 默认开；首字节 0x17 显式启用（与实证可用的原版 0.70 客户端行为一致）
    cfg.transport.tls.enable = opt_bool(common, "tls_enable", true);
    cfg.transport.tls.disable_custom_tls_first_byte = false;
    if let Some(v) = common.get("heartbeat_interval").and_then(|v| v.parse::<i64>().ok()) {
        if v > 0 {
            cfg.transport.heartbeat_interval = v;
        }
    }
    if let Some(v) = common.get("heartbeat_timeout").and_then(|v| v.parse::<i64>().ok()) {
        if v > 0 {
            cfg.transport.heartbeat_timeout = v;
        }
    }

    let mut skipped = Vec::new();
    for (name, kv) in &sections {
        if name.eq_ignore_ascii_case("common") {
            continue;
        }
        let ptype = kv.get("type").cloned().unwrap_or_else(|| "tcp".into());
        if ptype != "tcp" {
            skipped.push(name.clone());
            continue;
        }
        let p = frpc_rust::config::Proxy {
            name: name.clone(),
            remote_port: need_u16(name, kv, "remote_port")?,
            local_ip: kv.get("local_ip").cloned().unwrap_or_else(|| "127.0.0.1".into()),
            local_port: need_u16(name, kv, "local_port")?,
            transport: frpc_rust::config::ProxyTransport {
                use_encryption: opt_bool(kv, "use_encryption", false),
                use_compression: opt_bool(kv, "use_compression", false),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.proxies.push(p);
    }
    Ok((cfg, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\r
[common]\r
server_addr = node1.chmlfrp.cn\r
server_port = 7000\r
token = abc123\r
user = jeffrey\r
tls_enable = true\r
; 注释行\r
\r
[tunnel-main]\r
type = tcp\r
local_ip = 127.0.0.1\r
local_port = 8081\r
remote_port = 24000\r
use_encryption = false\r
\r
[tunnel-web]\r
type = http\r
local_port = 8081\r
remote_port = 80\r
";

    #[test]
    fn ini_maps_common_and_tcp_proxies() {
        let (cfg, skipped) = ini_to_config(SAMPLE).unwrap();
        assert_eq!(cfg.server_addr, "node1.chmlfrp.cn");
        assert_eq!(cfg.server_port, 7000);
        assert_eq!(cfg.auth.token, "abc123");
        assert_eq!(cfg.user, "jeffrey");
        assert!(cfg.transport.tls.enable);
        assert!(!cfg.transport.tls.disable_custom_tls_first_byte);
        assert_eq!(skipped, vec!["tunnel-web"]); // http 型跳过
        assert_eq!(cfg.proxies.len(), 1);
        let p = &cfg.proxies[0];
        assert_eq!((p.name.as_str(), p.remote_port, p.local_port), ("tunnel-main", 24000, 8081));
        assert!(!p.transport.use_encryption);
    }

    #[test]
    fn ini_missing_keys_name_the_key() {
        let e = ini_to_config("[common]\nserver_addr = x\n[t]\ntype = tcp\n").unwrap_err();
        assert_eq!(e.code, "E_NAME");
        assert!(e.msg.contains("server_port"), "指名缺失键: {}", e.msg);
    }

    #[test]
    fn ini_non_tcp_protocol_rejected() {
        let e =
            ini_to_config("[common]\nserver_addr = x\nserver_port = 1\ntoken = t\nprotocol = kcp\n").unwrap_err();
        assert!(e.msg.contains("kcp"));
    }

    #[test]
    fn ini_parser_tolerant_shapes() {
        let secs = parse_ini("#hash\nstray = no-section-yet\n[ a ]\nk = v w \n;semi\n");
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].0, "a");
        assert_eq!(secs[0].1.get("k").map(|s| s.as_str()), Some("v w")); // 值内部空格保留
        assert!(!secs[0].1.contains_key("stray")); // 节外键忽略
    }
}
