//! ops：子命令级操作（stop/start/rm/rollback/ps/logs/env/reconcile/autoheal/gc*/owner*/init-keys）。
//! serve 复用同一批函数；owner 归属校验在 serve 层做（CLI 是管理员直发通道）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::dockerops;
use crate::publish::{self, PublishOpts, PublishOut};
use crate::state::{self, ApiError, R, SlotConf};

fn conf_of(root: &Path, p: &str, s: &str) -> R<SlotConf> {
    state::SlotConf::load(root, p, s)
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no slot '{p}/{s}'")))
}

/// stop：停容器 + conf 标记 stopped=1（autoheal/reconcile 据此跳过）。
pub fn stop(root: &Path, p: &str, s: &str) -> R<()> {
    let mut c = conf_of(root, p, s)?;
    let name = publish::container_name(p, s);
    if dockerops::container_status(&name).is_some() {
        dockerops::docker(&["stop", &name])?;
    }
    c.stopped = true;
    c.save(root, p, s)
}

/// start：清 stopped 标记；容器缺失则按 conf 的 tag 走 publish 内核重建。
pub fn start(root: &Path, p: &str, s: &str) -> R<()> {
    let mut c = conf_of(root, p, s)?;
    let name = publish::container_name(p, s);
    if dockerops::container_status(&name).is_some() {
        dockerops::docker(&["start", &name])?;
    } else {
        publish::publish(root, &c.owner, p, s, &c.tag, PublishOpts {
            mem: Some(c.mem.clone()),
            cpu: Some(c.cpu),
            cport: Some(c.cport),
        })?;
    }
    c.stopped = false;
    c.save(root, p, s)
}

/// rm：容器、路由、conf/env/last2、端口与 redisdb 登记、DB/角色，全清（幂等尽力）。
/// DB 新旧名（归一化 + legacy，SPEC §9）都清，防存量旧名库残留。
pub fn rm(root: &Path, p: &str, s: &str) -> R<()> {
    conf_of(root, p, s)?; // 确认槽位存在（未登记的槽位不产生副作用）
    let name = publish::container_name(p, s);
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &name]);
    let new_db = state::db_ident(p, s);
    let legacy_db = state::legacy_db_ident(p, s);
    let mut sql = String::new();
    for db in [&legacy_db, &new_db] {
        sql.push_str(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\nDROP ROLE IF EXISTS \"{db}\";\n"));
    }
    let _ = dockerops::psql(&sql);
    let _ = fs::remove_file(state::route_path(root, p, s));
    for path in [
        state::conf_path(root, p, s),
        state::env_path(root, p, s),
        state::last2_path(root, p, s),
    ] {
        let _ = fs::remove_file(path);
    }
    state::release_port(root, p, s)?; // redisdb 随 conf 删除自动释放
    let _ = dockerops::caddy_reload();
    Ok(())
}

/// rollback：读 last2 按其 tag 重跑 publish 内核；内核自身会把旧 tag 写回 last2（SPEC §7）。
pub fn rollback(root: &Path, p: &str, s: &str) -> R<PublishOut> {
    let c = conf_of(root, p, s)?;
    let prev = state::read_last2(root, p, s)
        .ok_or_else(|| ApiError::new("E_NOTFOUND", format!("no last2 for '{p}/{s}'")))?;
    publish::publish(root, &c.owner, p, s, &prev, PublishOpts {
        mem: Some(c.mem.clone()),
        cpu: Some(c.cpu),
        cport: Some(c.cport),
    })
}

/// ps / GET /_api/projects 的数据源：conf 全量 + docker 实况。
pub fn ps(root: &Path) -> Vec<(String, String, SlotConf, String)> {
    state::all_confs(root)
        .into_iter()
        .map(|(p, s, c)| {
            let st = dockerops::container_status(&publish::container_name(&p, &s))
                .unwrap_or_else(|| "missing".into());
            (p, s, c, st)
        })
        .collect()
}

pub fn logs(root: &Path, p: &str, s: &str, tail: u32) -> R<String> {
    let _ = conf_of(root, p, s)?;
    let name = publish::container_name(p, s);
    if dockerops::container_status(&name).is_none() {
        return Err(ApiError::new("E_NOTFOUND", format!("no container '{name}'")));
    }
    dockerops::logs(&name, tail)
}

pub fn env_of(root: &Path, p: &str, s: &str) -> R<BTreeMap<String, String>> {
    let _ = conf_of(root, p, s)?;
    let text = fs::read_to_string(state::env_path(root, p, s))
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no env for '{p}/{s}'")))?;
    Ok(state::parse_kv(&text))
}

// —— Redis 浏览内核（SPEC §10.3，排障用；跨 owner 不隔离——SPEC 已注明）——

/// GET /_api/redis-keys：q 过白名单转 pattern 后 --scan 增量收集前 limit 个（读满即杀，见 dockerops）。
pub fn redis_keys(db: u8, q: &str, limit: usize) -> R<(Vec<String>, bool)> {
    let pattern = state::redis_scan_pattern(q)
        .ok_or_else(|| ApiError::new("E_NAME", format!("q '{q}' 含白名单 [a-zA-Z0-9:_*.-] 外字符")))?;
    dockerops::redis_scan(db, &pattern, limit)
}

/// GET /_api/redis-get：TYPE → PTTL → 按类型取值，值统一转字符串并 4KB 截断。
/// list/zset 取前 100 条；其他类型（stream 等）type 字段如实带回、值留空。
pub fn redis_get(db: u8, key: &str) -> R<(String, i64, String)> {
    // 取舍：redis key 理论可含任意字节，此处保守白名单——argv 传参本无注入面，
    // 白名单外的 key（空格/引号/二进制等）请用 CLI 直查（错误信息如实提示）。
    if !state::valid_redis_key(key) {
        return Err(ApiError::new("E_NAME", format!("key '{key}' 含特殊字符，请用 CLI 查看")));
    }
    let t = dockerops::redis_cli(db, &["TYPE", key])?.trim().to_string();
    let ttl_s = dockerops::redis_cli(db, &["PTTL", key])?;
    let ttl_ms: i64 = ttl_s
        .trim()
        .parse()
        .map_err(|e| ApiError::state(format!("PTTL 输出异常 ({ttl_s:?}): {e}")))?;
    let raw = match t.as_str() {
        "string" => dockerops::redis_cli(db, &["GET", key])?,
        "list" => dockerops::redis_cli(db, &["LRANGE", key, "0", "99"])?,
        "hash" => dockerops::redis_cli(db, &["HGETALL", key])?,
        "set" => dockerops::redis_cli(db, &["SMEMBERS", key])?,
        "zset" => dockerops::redis_cli(db, &["ZRANGE", key, "0", "99", "WITHSCORES"])?,
        _ => String::new(), // none（键不存在）/ stream 等：type 已如实带回
    };
    Ok((t, ttl_ms, state::clip_value(&raw)))
}

/// reconcile：遍历 slot.conf，容器/路由缺失按 conf 重建（幂等）。
pub fn reconcile(root: &Path) -> Vec<String> {
    let mut log = vec![];
    for (p, s, c) in state::all_confs(root) {
        if c.stopped {
            continue;
        }
        let name = publish::container_name(&p, &s);
        match dockerops::container_status(&name) {
            None => match publish::publish(root, &c.owner, &p, &s, &c.tag, PublishOpts {
                mem: Some(c.mem.clone()),
                cpu: Some(c.cpu),
                cport: Some(c.cport),
            }) {
                Ok(_) => log.push(format!("{p}/{s}: rebuilt (container was missing)")),
                Err(e) => log.push(format!("{p}/{s}: E_STATE rebuild failed: {e}")),
            },
            Some(st) if st != "running" => {
                if dockerops::run_fail_ok("docker", &["start", &name]) {
                    log.push(format!("{p}/{s}: started (was {st})"));
                } else {
                    log.push(format!("{p}/{s}: E_STATE start failed (was {st})"));
                }
            }
            Some(_) => {
                let want = state::caddy_route(&p, &s, c.port);
                let rp = state::route_path(root, &p, &s);
                if fs::read_to_string(&rp).unwrap_or_default() != want {
                    match publish::apply_route(root, &p, &s, c.port) {
                        Ok(()) => log.push(format!("{p}/{s}: route rewritten + caddy reloaded")),
                        Err(e) => log.push(format!("{p}/{s}: E_STATE route: {e}")),
                    }
                }
            }
        }
    }
    if log.is_empty() {
        log.push("nothing to do".into());
    }
    log
}

/// autoheal：stopped=1 跳过；容器缺失→重建；退出→start；unhealthy→restart。
pub fn autoheal(root: &Path) -> Vec<String> {
    let mut log = vec![];
    for (p, s, c) in state::all_confs(root) {
        if c.stopped {
            continue;
        }
        let name = publish::container_name(&p, &s);
        match dockerops::container_status(&name) {
            None => match publish::publish(root, &c.owner, &p, &s, &c.tag, PublishOpts {
                mem: Some(c.mem.clone()),
                cpu: Some(c.cpu),
                cport: Some(c.cport),
            }) {
                Ok(_) => log.push(format!("{p}/{s}: rebuilt (container gone)")),
                Err(e) => log.push(format!("{p}/{s}: E_STATE rebuild failed: {e}")),
            },
            Some(st) if st == "exited" || st == "dead" || st == "created" => {
                if dockerops::run_fail_ok("docker", &["start", &name]) {
                    log.push(format!("{p}/{s}: started (was {st})"));
                } else {
                    log.push(format!("{p}/{s}: E_STATE start failed (was {st})"));
                }
            }
            Some(_) => {
                if dockerops::health_status(&name).as_deref() == Some("unhealthy")
                    && dockerops::run_fail_ok("docker", &["restart", &name])
                {
                    log.push(format!("{p}/{s}: restarted (unhealthy)"));
                }
            }
        }
    }
    if log.is_empty() {
        log.push("nothing to do".into());
    }
    log
}

/// 本地镜像/构建缓存/悬空卷清理（systemd timer 调用），末尾顺带清 _tmp 陈旧会话（SPEC §8.1）。
pub fn gc_images(root: &Path) -> R<String> {
    let a = dockerops::docker(&["image", "prune", "-a", "--force"])?;
    let b = dockerops::docker(&["builder", "prune", "-f"])?;
    let c = dockerops::docker(&["volume", "prune", "-f"])?;
    Ok(format!("{a}{b}{c}{}", gc_tmp(root)))
}

/// 删 _tmp 下 mtime>24h 的 *.bin/*.meta/*.status（分片/dump 会话残留 + 单发 import 落盘残留 + 孤儿，含验签失败泄留）。
fn gc_tmp(root: &Path) -> String {
    let dir = root.join("_tmp");
    let Ok(rd) = fs::read_dir(&dir) else { return String::new() };
    let now = state::unix_now();
    let mut pairs = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if !matches!(
            p.extension().and_then(|o| o.to_str()),
            Some("bin") | Some("meta") | Some("status")
        ) {
            continue;
        }
        let Some(name) = p.file_name().and_then(|o| o.to_str()).map(str::to_string) else { continue };
        let mtime = e
            .metadata()
            .ok()
            .and_then(|md| md.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        pairs.push((name, mtime));
    }
    let mut log = String::new();
    for name in state::stale_tmp(&pairs, now) {
        match fs::remove_file(dir.join(&name)) {
            Ok(()) => log.push_str(&format!("removed stale _tmp/{name}\n")),
            Err(e) => log.push_str(&format!("stale _tmp/{name}: {e}\n")),
        }
    }
    log
}

/// btrfs 只读快照 /@showroom → /@showroom-snaps/<前缀>YYYYMMDD-HHMM，按名排序裁掉超保留数的最旧（SPEC §8.2）。
/// btrfs 失败经 dockerops::run 带原始 stderr 返回非零，不 panic。
pub fn snapshot(kind: &str) -> R<String> {
    let (prefix, keep) = match kind {
        "hourly" => ("h-", 24usize),
        "daily" => ("d-", 7usize),
        _ => return Err(ApiError::new("E_NAME", format!("kind must be hourly|daily, got '{kind}'"))),
    };
    if !Path::new("/@showroom-snaps").is_dir() {
        dockerops::run("btrfs", &["subvolume", "create", "/@showroom-snaps"])?; // 已存在由上面的 is_dir 跳过
    }
    // 本地时间戳经 date 子进程（免引 chrono；本工具本就 subprocess 密集）
    let stamp = dockerops::run("date", &["+%Y%m%d-%H%M"])?.trim().to_string();
    let name = format!("{prefix}{stamp}");
    dockerops::run(
        "btrfs",
        &["subvolume", "snapshot", "-r", "/@showroom", &format!("/@showroom-snaps/{name}")],
    )?;
    let mut out = format!("created /@showroom-snaps/{name}\n");

    let mut names = vec![];
    if let Ok(rd) = fs::read_dir("/@showroom-snaps") {
        for e in rd.flatten() {
            if let Some(n) = e.file_name().to_str() {
                if n.starts_with(prefix) {
                    names.push(n.to_string());
                }
            }
        }
    }
    for old in state::prune_snapshots(&names, keep) {
        match dockerops::run("btrfs", &["subvolume", "delete", &format!("/@showroom-snaps/{old}")]) {
            Ok(_) => out.push_str(&format!("pruned /@showroom-snaps/{old}\n")),
            Err(e) => eprintln!("showroomctl: prune /@showroom-snaps/{old}: {e}"),
        }
    }
    Ok(out)
}

/// gc-registry：每个 repo 字典序降序保留前 5 个 tag，其余按 digest DELETE。
/// ponytail: registry:2 无时间戳 API，字典序近似最新（"v10"<"v9" 类版本号会排错，tag 语义化时升级）
pub fn gc_registry(root: &Path) -> R<usize> {
    let (code, _, body) = dockerops::http_registry(root, "GET", "/v2/_catalog", None)?;
    if code != 200 {
        return Err(ApiError::state(format!("registry catalog: HTTP {code}")));
    }
    let cat: Value = serde_json::from_slice(&body).map_err(|e| ApiError::state(format!("catalog json: {e}")))?;
    let repos: Vec<String> = cat["repositories"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let mut deleted = 0usize;
    for repo in repos {
        let (code, _, body) =
            dockerops::http_registry(root, "GET", &format!("/v2/{repo}/tags/list"), None)?;
        if code != 200 {
            eprintln!("gc-registry: {repo}: tags/list HTTP {code}");
            continue;
        }
        let tags: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(mut tags) = tags["tags"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()) else {
            continue;
        };
        tags.sort();
        tags.reverse(); // 字典序降序，前 5 保留
        for t in tags.into_iter().skip(5) {
            let (_, hdrs, _) = dockerops::http_registry(
                root,
                "GET",
                &format!("/v2/{repo}/manifests/{t}"),
                Some(dockerops::MANIFEST_ACCEPT),
            )?;
            let Some(digest) = dockerops::header(&hdrs, "docker-content-digest") else { continue };
            let (code, _, _) =
                dockerops::http_registry(root, "DELETE", &format!("/v2/{repo}/manifests/{digest}"), None)?;
            if code == 202 || code == 204 || code == 200 {
                deleted += 1;
            } else {
                eprintln!("gc-registry: {repo}:{t}: delete HTTP {code}");
            }
        }
    }
    Ok(deleted)
}

/// registry-gc：停 registry → registry:2 garbage-collect --delete-untagged → 重启。
/// config 路径与实际镜像不符时这里会如实带上 subprocess 报错，不做任何吞掉。
pub fn registry_gc(root: &Path) -> R<()> {
    dockerops::docker(&["stop", "sr-registry"])?;
    let mw = root.join("_middleware/registry");
    let bind = format!("{}:/var/lib/registry", mw.display());
    let out = dockerops::run(
        "docker",
        &[
            "run", "--rm", "-v", &bind, "registry:2",
            "garbage-collect", "--delete-untagged", "/etc/docker/registry/config.yml",
        ],
    );
    if let Err(e) = out {
        let _ = dockerops::docker(&["start", "sr-registry"]);
        return Err(e);
    }
    dockerops::docker(&["start", "sr-registry"])?;
    Ok(())
}

pub fn import_check(image_id: &str) -> bool {
    dockerops::run_fail_ok("docker", &["image", "inspect", image_id])
}

// —— owner 管理 ——

/// 登记公钥（kid=sha256(pubkey) 前 8 hex）+ 生成 registry 密码（bcrypt cost 10 进 htpasswd，明文 0600 进 reg.cred）。
pub fn owner_add(root: &Path, name: &str, pubkey: &str, replace: bool) -> R<String> {
    state::check_name("owner", name)?;
    let vk = showroom_common::OwnerKey::from_pub_string(pubkey.trim())
        .map_err(|e| ApiError::new("E_NAME", format!("bad pubkey: {e}")))?;
    let kid = state::kid_of(&vk);
    let dir = state::tokens_dir(root, name);
    if replace {
        let _ = fs::remove_dir_all(&dir);
    }
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{kid}.pub")), format!("{}\n", pubkey.trim()))?;
    let pw = state::rand_hex(16);
    let hash = bcrypt::hash(&pw, 10).map_err(|e| ApiError::state(format!("bcrypt: {e}")))?;
    state::upsert_htpasswd(root, name, &hash)?;
    state::write_private(
        &root.join(format!("state/owners/{name}/reg.cred")),
        &format!("{name}:{pw}\n"),
    )?;
    Ok(kid)
}

pub fn owner_key_rm(root: &Path, owner: &str, kid: &str) -> R<()> {
    let p = state::tokens_dir(root, owner).join(format!("{kid}.pub"));
    if !p.exists() {
        return Err(ApiError::new("E_NOTFOUND", format!("no kid '{kid}' for owner '{owner}'")));
    }
    Ok(fs::remove_file(p)?)
}

/// 吊销：registry 登录失效 + 全部公钥失效；已发布的展项不动。
pub fn owner_revoke(root: &Path, owner: &str) -> R<()> {
    state::remove_htpasswd_user(root, owner)?;
    let _ = fs::remove_dir_all(state::tokens_dir(root, owner));
    let _ = fs::remove_dir_all(root.join(format!("state/owners/{owner}")));
    Ok(())
}

/// 平台 RSA 对（首次生成；已存在则不覆盖）。serve 启动时缺失会自动调用。
pub fn init_keys(root: &Path) -> R<PathBuf> {
    let dir = root.join("state");
    let privk = dir.join("platform_rsa");
    if privk.exists() {
        return Ok(privk);
    }
    showroom_common::PlatformKeys::generate()
        .save_pem(&dir)
        .map_err(|e| ApiError::state(format!("save platform rsa: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Redis 内核的白名单拒绝路径（校验先于 docker 调用，不碰容器）：
    // q / key 出白名单 → E_NAME；合法入参不在此测（需 sr-redis，e2e 域）
    #[test]
    fn redis_kernels_reject_bad_input() {
        for bad_q in ["a b", "a$b", "a;b"] {
            let e = redis_keys(0, bad_q, 10).unwrap_err();
            assert_eq!(e.code, "E_NAME", "{bad_q}: {}", e.msg);
        }
        for bad_k in ["", "a b", "k;SCAN", "中"] {
            let e = redis_get(0, bad_k).unwrap_err();
            assert_eq!(e.code, "E_NAME", "{bad_k:?}: {}", e.msg);
        }
    }
}
