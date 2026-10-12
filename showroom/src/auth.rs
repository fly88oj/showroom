//! showroom login/logout（plan-16 §4）：密码一次、密钥常驻（gh/ssh 模型）。
//!
//! login = 本机生成临时 Ed25519 → OPAQUE（login-begin/finish，密码经盲化永不上线）→
//! key-bind（bind_token + 候选钥匙签名）→ GET /_api/reg-cred 自动落 config；
//! 默认持久化（seed 落 key_path 0600 + config 写 user/server_lan/registry），
//! `--ephemeral` 全不落盘（进程退出即弃，服务端靠 keep-N GC 收尾，每次使用需重新登录）。
//! 密码经 rpassword 交互读取（不回显、不进 argv/env/ps）。
//!
//! 覆盖语义：key_path 已有钥匙时先签名自测——通过则已是登录态（免重登，提示后退出），
//! 失败（钥匙被删/GC）则视为「找回/换设备」：落新钥匙重新绑定。

use std::time::Duration;

use serde_json::{json, Value};

use showroom_common::{nonce_hex, opaque, sha256_hex, sign, UserKey};

use crate::api::now_unix;
use crate::config::Config;

/// 密码错误/未设密码的统一文案（客户端无法也不应区分两者——防枚举的一致代价，
/// plan-16 §3；CLI 与 Web 同文案）。
const LOGIN_FAIL_TEXT: &str = "密码错误，或该账号尚未设置密码（请联系管理员）";

fn kid_of(key: &UserKey) -> String {
    let vk = UserKey::from_pub_string(&key.pub_string()).expect("own key parses");
    sha256_hex(&vk.to_bytes())[..8].to_string()
}

/// login 用的裸 HTTP 客户端（无签名语义；超时 30s——OPAQUE 报文很小）。
fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP 客户端构造失败：{e}"))
}

fn post_json(
    c: &reqwest::blocking::Client,
    base: &str,
    path: &str,
    body: &Value,
    headers: &[(&str, String)],
) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let mut req = c.post(format!("{}{path}", base.trim_end_matches('/')));
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let resp = req
        .header("Content-Type", "application/json")
        .body(bytes)
        .send()
        .map_err(|e| format!("请求失败（{base}{path}）：{e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map(|mut v: Value| {
        v["_status"] = json!(status.as_u16());
        v
    })
    .map_err(|_| {
        // 零信息暴露（SPEC §3）：404 + {"error":"E_AUTH"} 是限流/畸形的服务端形态
        if status.as_u16() == 404 {
            "服务端拒绝（E_AUTH：限流或请求被拒），稍后重试".to_string()
        } else {
            format!("HTTP {status}: {}", text.chars().take(200).collect::<String>())
        }
    })
}

fn get_json(
    c: &reqwest::blocking::Client,
    base: &str,
    path: &str,
    headers: &[(&str, String)],
) -> Result<Value, String> {
    let mut req = c.get(format!("{}{path}", base.trim_end_matches('/')));
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let resp = req.send().map_err(|e| format!("请求失败（{base}{path}）：{e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| e.to_string())?;
    serde_json::from_str::<Value>(&text).map_err(|_| format!("HTTP {status}: 非 JSON 响应"))
}

/// 签名头（本文件局部：login 流程专用，密码通道之外的请求全走候选/既有钥匙）。
fn authz(user: &str, key: &UserKey, method: &str, path_q: &str, body: &[u8]) -> String {
    sign(user, key, method, path_q, body, now_unix(), &nonce_hex())
}

/// `showroom login [账号] [--server-lan URL] [--ephemeral]`（plan-16 §4）。
pub fn login(
    cfg: &Config,
    account: Option<&str>,
    server_lan: Option<String>,
    ephemeral: bool,
) -> Result<(), String> {
    let user = account
        .map(str::to_string)
        .or_else(|| {
            let u = cfg.user.clone();
            (!u.is_empty()).then_some(u)
        })
        .ok_or_else(|| "缺少账号：showroom login <账号> --server-lan URL".to_string())?;
    let base = server_lan
        .clone()
        .or_else(|| cfg.server_lan.clone().filter(|s| !s.is_empty()))
        .or_else(|| cfg.server_public.clone().filter(|s| !s.is_empty()))
        .ok_or_else(|| {
            format!("未指定服务端：--server-lan URL（或先 showroom config --server-lan；当前账号 {user}）")
        })?;
    let base = base.trim_end_matches('/').to_string();

    // 既有钥匙：先签名自测——通过即已是登录态（免重登）；失败则覆盖走找回路径
    let existing = UserKey::from_seed_file(&cfg.key_file()).ok();
    if let Some(k) = &existing {
        if !ephemeral {
            let c = http()?;
            let h = authz(&user, k, "GET", "/_api/health", b"");
            if let Ok(v) = get_json(&c, &base, "/_api/health", &[("Authorization", h)]) {
                if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                    println!("已是登录态（账号 {user}，本机钥匙有效），无需重新登录。");
                    println!("（如需绑定新钥匙：删除 {} 后重新 showroom login）", cfg.key_file().display());
                    return Ok(());
                }
            }
        }
    }

    // 密码：不回显读取（不进 argv/env/ps）
    let pw = rpassword::prompt_password("密码: ").map_err(|e| format!("读取密码失败：{e}"))?;
    if pw.is_empty() {
        return Err("密码不能为空".into());
    }

    // OPAQUE：ke1 → ke2 → ke3（错密码在本地 finish 即失败，无第三请求）
    let mut rng = rand::rngs::OsRng;
    let (state, ke1) =
        opaque::login_start(&mut rng, &pw, &user).map_err(|e| format!("登录初始化失败：{e}"))?;
    let c = http()?;
    let v = post_json(&c, &base, "/_api/auth/login-begin", &json!({"user": user, "ke1": ke1}), &[])?;
    let ke2 = v["ke2"]
        .as_str()
        .ok_or("登录失败：服务端未返回 ke2")?
        .to_string();
    let ke3 = opaque::login_finish(state, &pw, &user, &ke2).map_err(|_| LOGIN_FAIL_TEXT.to_string())?;
    let v = post_json(&c, &base, "/_api/auth/login-finish", &json!({"user": user, "ke3": ke3}), &[])?;
    let bind_token = v["bind_token"]
        .as_str()
        .ok_or("登录失败：服务端未返回 bind_token")?
        .to_string();

    // key-bind：候选钥匙签名（服务端按 body 公钥验签）+ X-Sr-Bind
    let key = UserKey::generate();
    let bind_body = json!({"user": user, "pubkey": key.pub_string()});
    let bind_bytes = serde_json::to_vec(&bind_body).map_err(|e| e.to_string())?;
    let auth = authz(&user, &key, "POST", "/_api/auth/key-bind", &bind_bytes);
    let v = post_json(
        &c,
        &base,
        "/_api/auth/key-bind",
        &bind_body,
        &[("Authorization", auth), ("X-Sr-Bind", bind_token)],
    )?;
    let kid = v["kid"].as_str().unwrap_or("?").to_string();
    let is_admin = v["admin"].as_bool().unwrap_or(false);

    // 登录后信任通道下发 registry 凭据（CLI 免手工 config；plan-16 §2.2 末段）
    let h = authz(&user, &key, "GET", "/_api/reg-cred", b"");
    let (reg_user, reg_pw) = match get_json(&c, &base, "/_api/reg-cred", &[("Authorization", h)]) {
        Ok(v) => (
            v["user"].as_str().unwrap_or(&user).to_string(),
            v["registry_password"].as_str().unwrap_or_default().to_string(),
        ),
        Err(e) => {
            // 非致命：登录已成，registry 凭据可后补（config --registry-*）
            eprintln!("showroom: registry 凭据获取失败（{e}）；push 快通道稍后可用 showroom config --registry-password 补");
            (user.clone(), String::new())
        }
    };

    // 持久化（默认；--ephemeral 全不落盘，服务端 keep-N GC 收尾）
    if ephemeral {
        println!("登录成功（临时会话，不落盘）：账号 {user}，kid {kid}{}，服务端 {base}", admin_tag(is_admin));
        println!("（--ephemeral：本进程退出即弃；服务端多余绑定钥匙由 keep-N GC 回收）");
        return Ok(());
    }
    key.save_seed(&cfg.key_file())
        .map_err(|e| format!("写密钥 {} 失败：{e}", cfg.key_file().display()))?;
    let mut c2 = cfg.clone();
    c2.user = user.clone();
    if let Some(sl) = server_lan {
        c2.server_lan = Some(sl.trim_end_matches('/').to_string());
    } else if c2.server_lan.as_deref().map(str::is_empty).unwrap_or(true) {
        // config 无地址时把本次成功的 base 记为 server_lan（下次免输）
        c2.server_lan = Some(base.clone());
    }
    c2.registry.user = reg_user.clone();
    if !reg_pw.is_empty() {
        c2.registry.password = reg_pw;
    }
    c2.save()?;

    println!("登录成功：账号 {user}，kid {kid}{}", admin_tag(is_admin));
    println!("密钥: {}", cfg.key_file().display());
    println!("配置: {}（server_lan / registry 凭据已自动写入）", crate::config::config_path().display());
    if is_admin {
        println!("提示：本钥匙继承管理员权限（账号已有任一 admin 钥匙，SPEC §16）");
    }
    Ok(())
}

fn admin_tag(admin: bool) -> &'static str {
    if admin { " [ADMIN]" } else { "" }
}

/// `showroom logout`（plan-16 §4 + B7）：清本地钥匙与 config 身份字段，并恒为远程注销
/// 本机钥匙（key-selfrm 仅删请求签名所用 kid 自己；尽力而为，失败如实报但不阻塞本地清理）。
pub fn logout(cfg: &Config) -> Result<(), String> {
    let key_path = cfg.key_file();
    // 远端注销：需钥匙可读且服务端可达
    match UserKey::from_seed_file(&key_path) {
        Ok(key) if !cfg.user.is_empty() => {
            let kid = kid_of(&key);
            match crate::api::Api::connect(cfg, None) {
                Ok(api) => {
                    let body = json!({"kid": kid});
                    match api.signed_post_json("/_api/key-selfrm", &body) {
                        Ok(_) => println!("远端钥匙 {kid} 已注销"),
                        Err(e) => eprintln!("showroom: 远端注销失败（{e}）；本地仍将清理"),
                    }
                }
                Err(e) => eprintln!("showroom: 无法连接服务端（{e}）；本地仍将清理"),
            }
        }
        _ => eprintln!("showroom: 无本地钥匙/账号，跳过远端注销"),
    }
    // 本地清理：删钥匙；config 清身份（user/registry），server 地址保留（机器属性，下次 login 免输）
    if key_path.exists() {
        std::fs::remove_file(&key_path).map_err(|e| format!("删除 {} 失败：{e}", key_path.display()))?;
        println!("已删除密钥 {}", key_path.display());
    }
    let mut c = cfg.clone();
    if !c.user.is_empty() || !c.registry.password.is_empty() {
        c.user = String::new();
        c.registry = crate::config::Registry::default();
        c.save()?;
        println!("已清除配置中的身份信息（{}；server 地址保留）", crate::config::config_path().display());
    } else if !key_path.exists() {
        return Err("本机未登录（无可清理的钥匙与配置）".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // kid_of 与服务端 state::kid_of 同源（sha256(pubkey) 前 8 hex）
    #[test]
    fn kid_matches_server_derivation() {
        let k = UserKey::generate();
        let vk = showroom_common::UserKey::from_pub_string(&k.pub_string()).unwrap();
        assert_eq!(kid_of(&k), showroom_common::sha256_hex(&vk.to_bytes())[..8].to_string());
    }
}

/// 密钥过期自动重登（§密钥生命周期）：E_AUTH 时提示输入密码 → OPAQUE 新钥匙 → 重试原操作。
/// 由 Api::connect / 各命令的错误路径调用。返回 true=已重新登录可重试。
pub fn reauth_on_expired(cfg: &crate::config::Config, server_lan: Option<String>) -> Result<bool, String> {
    eprintln!("\n本机密钥已过期或被吊销（30 分钟无活动自动过期）。请重新输入密码获取新密钥。\n");
    // 复用 login 流程（密码 → OPAQUE → key-bind → seed 覆盖写入）
    login(cfg, None, server_lan, false)?;
    Ok(true)
}
