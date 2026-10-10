//! ops：子命令级操作（stop/start/rm/rollback/ps/logs/env/reconcile/autoheal/gc*/owner*/init-keys）。
//! serve 复用同一批函数；owner 归属校验在 serve 层做（CLI 是管理员直发通道）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::dockerops;
use crate::publish::{self, PublishOpts, PublishOut};
use crate::state::{self, ApiError, Conf, R};

fn conf_of(root: &Path, p: &str) -> R<Conf> {
    state::Conf::load(root, p)
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no project '{p}'")))
}

/// stop：停容器 + conf 标记 stopped=1（autoheal/reconcile 据此跳过）。
pub fn stop(root: &Path, p: &str) -> R<()> {
    let mut c = conf_of(root, p)?;
    let name = publish::container_name(p);
    if dockerops::container_status(&name).is_some() {
        dockerops::docker(&["stop", &name])?;
    }
    c.stopped = true;
    c.save(root, p)
}

/// start：清 stopped 标记；容器缺失则按 conf 的 tag 走 publish 内核重建。
pub fn start(root: &Path, p: &str) -> R<()> {
    let mut c = conf_of(root, p)?;
    let name = publish::container_name(p);
    if dockerops::container_status(&name).is_some() {
        dockerops::docker(&["start", &name])?;
    } else {
        publish::publish(root, &c.owner, p, &c.tag, PublishOpts {
            mem: Some(c.mem.clone()),
            cpu: Some(c.cpu),
            cport: Some(c.cport),
        })?;
    }
    c.stopped = false;
    c.save(root, p)
}

/// rm：容器（新旧名）、路由、conf/env/last2、端口与 redisdb 登记、DB/角色，全清（幂等尽力）。
/// DB：归一化新名 + legacyslot 旧候选（§9 双名）都清，防存量旧名库残留。
pub fn rm(root: &Path, p: &str) -> R<()> {
    let c = conf_of(root, p)?; // 确认项目存在（未登记的项目不产生副作用）
    let name = publish::container_name(p);
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &name]);
    if let Some(s) = &c.legacyslot {
        let old = format!("sr-{p}-{s}");
        let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &old]);
    }
    let mut dbs = vec![state::db_ident(p)];
    if let Some(s) = &c.legacyslot {
        for d in state::legacy_db_idents(p, s) {
            if !dbs.contains(&d) {
                dbs.push(d);
            }
        }
    }
    let mut sql = String::new();
    for db in &dbs {
        sql.push_str(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\nDROP ROLE IF EXISTS \"{db}\";\n"));
    }
    let _ = dockerops::psql(&sql);
    let _ = fs::remove_file(state::route_path(root, p));
    for path in [
        state::conf_path(root, p),
        state::env_path(root, p),
        state::last2_path(root, p),
    ] {
        let _ = fs::remove_file(path);
    }
    state::release_port(root, p)?; // redisdb 随 conf 删除自动释放
    let _ = dockerops::caddy_reload();
    Ok(())
}

/// rollback：读 last2 按其 tag 重跑 publish 内核；内核自身会把旧 tag 写回 last2（SPEC §7）。
pub fn rollback(root: &Path, p: &str) -> R<PublishOut> {
    let c = conf_of(root, p)?;
    let prev = state::read_last2(root, p)
        .ok_or_else(|| ApiError::new("E_NOTFOUND", format!("no last2 for '{p}'")))?;
    publish::publish(root, &c.owner, p, &prev, PublishOpts {
        mem: Some(c.mem.clone()),
        cpu: Some(c.cpu),
        cport: Some(c.cport),
    })
}

/// ps / GET /_api/projects 的数据源：conf 全量 + docker 实况。
pub fn ps(root: &Path) -> Vec<(String, Conf, String)> {
    state::all_confs(root)
        .into_iter()
        .map(|(p, c)| {
            let st = dockerops::container_status(&publish::container_name(&p))
                .unwrap_or_else(|| "missing".into());
            (p, c, st)
        })
        .collect()
}

pub fn logs(root: &Path, p: &str, tail: u32) -> R<String> {
    let _ = conf_of(root, p)?;
    let name = publish::container_name(p);
    if dockerops::container_status(&name).is_none() {
        return Err(ApiError::new("E_NOTFOUND", format!("no container '{name}'")));
    }
    dockerops::logs(&name, tail)
}

pub fn env_of(root: &Path, p: &str) -> R<BTreeMap<String, String>> {
    let _ = conf_of(root, p)?;
    let text = fs::read_to_string(state::env_path(root, p))
        .map_err(|_| ApiError::new("E_NOTFOUND", format!("no env for '{p}'")))?;
    Ok(state::parse_kv(&text))
}

// —— v1.4 迁移编排（§11）：state 布局上提（state 层）+ caddy reload + 容器收养 ——

/// 迁移编排（幂等；serve 启动、reconcile/autoheal 前调用）：
/// 1. state::migrate_legacy_layout 上提 conf/env/last2、去 ports.reg slot 列、重写路由文件（stderr 记录）；
/// 2. 有变更则 reload caddy（旧路由文件已删，新路由须重载生效）；
/// 3. 容器收养：旧名 sr-<p>-<legacyslot> 存在且新名 sr-<p> 不存在 → docker rename；
///    其余 sr-<p>-<x> 孤儿容器（被弃槽位）删除（排除滚动中的 sr-<p>-new）。
pub fn migrate_legacy(root: &Path) -> Vec<String> {
    let mut log = vec![];
    if state::migrate_legacy_layout(root) {
        log.push("legacy slot layout migrated to project-level".into());
        match dockerops::caddy_reload() {
            Ok(()) => log.push("caddy reloaded".into()),
            Err(e) => log.push(format!("E_STATE caddy reload: {e}")),
        }
    }
    let names = dockerops::all_container_names();
    for (p, c) in state::all_confs(root) {
        let new_name = format!("sr-{p}");
        let legacy: Option<String> = c.legacyslot.map(|s| format!("sr-{p}-{s}"));
        if legacy.is_some() && dockerops::container_status(&new_name).is_none() {
            if let Some(old) = &legacy {
                if dockerops::container_status(old).is_some() {
                    match dockerops::docker(&["rename", old, &new_name]) {
                        Ok(_) => log.push(format!("{p}: container renamed {old} -> {new_name}")),
                        Err(e) => log.push(format!("{p}: E_STATE rename {old}: {e}")),
                    }
                }
            }
        }
        if legacy.is_some() {
            let prefix = format!("sr-{p}-");
            let rolling = format!("{new_name}-new");
            for n in &names {
                if n.starts_with(&prefix) && Some(n) != legacy.as_ref() && *n != rolling {
                    if dockerops::run_fail_ok("docker", &["rm", "-f", n]) {
                        log.push(format!("{p}: removed legacy container {n}"));
                    } else {
                        log.push(format!("{p}: E_STATE rm legacy container {n}"));
                    }
                }
            }
        }
    }
    log
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

/// reconcile：先跑 v1.4 迁移编排（§11，幂等），再遍历项目 conf，容器/路由缺失按 conf 重建。
pub fn reconcile(root: &Path) -> Vec<String> {
    let mut log = migrate_legacy(root);
    for (p, c) in state::all_confs(root) {
        if c.stopped {
            continue;
        }
        let name = publish::container_name(&p);
        match dockerops::container_status(&name) {
            None => match publish::publish(root, &c.owner, &p, &c.tag, PublishOpts {
                mem: Some(c.mem.clone()),
                cpu: Some(c.cpu),
                cport: Some(c.cport),
            }) {
                Ok(_) => log.push(format!("{p}: rebuilt (container was missing)")),
                Err(e) => log.push(format!("{p}: E_STATE rebuild failed: {e}")),
            },
            Some(st) if st != "running" => {
                if dockerops::run_fail_ok("docker", &["start", &name]) {
                    log.push(format!("{p}: started (was {st})"));
                } else {
                    log.push(format!("{p}: E_STATE start failed (was {st})"));
                }
            }
            Some(_) => {
                let want = state::caddy_route(&p, c.port);
                let rp = state::route_path(root, &p);
                if fs::read_to_string(&rp).unwrap_or_default() != want {
                    match publish::apply_route(root, &p, c.port) {
                        Ok(()) => log.push(format!("{p}: route rewritten + caddy reloaded")),
                        Err(e) => log.push(format!("{p}: E_STATE route: {e}")),
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

/// autoheal：先跑 v1.4 迁移编排（§11，幂等；容器收养防误重建双份），再自愈：
/// stopped=1 跳过；容器缺失→重建；退出→start；unhealthy→restart。
pub fn autoheal(root: &Path) -> Vec<String> {
    let mut log = migrate_legacy(root);
    for (p, c) in state::all_confs(root) {
        if c.stopped {
            continue;
        }
        let name = publish::container_name(&p);
        match dockerops::container_status(&name) {
            None => match publish::publish(root, &c.owner, &p, &c.tag, PublishOpts {
                mem: Some(c.mem.clone()),
                cpu: Some(c.cpu),
                cport: Some(c.cport),
            }) {
                Ok(_) => log.push(format!("{p}: rebuilt (container gone)")),
                Err(e) => log.push(format!("{p}: E_STATE rebuild failed: {e}")),
            },
            Some(st) if st == "exited" || st == "dead" || st == "created" => {
                if dockerops::run_fail_ok("docker", &["start", &name]) {
                    log.push(format!("{p}: started (was {st})"));
                } else {
                    log.push(format!("{p}: E_STATE start failed (was {st})"));
                }
            }
            Some(_) => {
                if dockerops::health_status(&name).as_deref() == Some("unhealthy")
                    && dockerops::run_fail_ok("docker", &["restart", &name])
                {
                    log.push(format!("{p}: restarted (unhealthy)"));
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

// —— 镜像列表（SPEC §13.1，GET /_api/images 内核；与 gc_registry 同源的 catalog/tags HTTP）——

/// registry catalog → 每 repo tags/list → 展平为 `["<repo>:<tag>",…]`。
/// owner=Some 仅取 `<owner>/` 命名空间 repo（提前过滤省 HTTP 往返）；None（admin）全量。
/// repo 数与 tag 总量受 state::IMAGES_MAX_REPOS/TAGS 上限；单 repo tags/list 失败跳过如实记录。
pub fn images(root: &Path, owner: Option<&str>) -> R<Vec<String>> {
    let (code, _, body) = dockerops::http_registry(root, "GET", "/v2/_catalog", None)?;
    if code != 200 {
        return Err(ApiError::state(format!("registry catalog: HTTP {code}")));
    }
    let cat: Value = serde_json::from_slice(&body).map_err(|e| ApiError::state(format!("catalog json: {e}")))?;
    let repos: Vec<String> = cat["repositories"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let mut catalog: Vec<(String, Vec<String>)> = vec![];
    for repo in repos {
        if let Some(o) = owner {
            if !state::repo_owned_by(&repo, o) {
                continue; // 别人的 repo 不取 tags（省 HTTP，也免 tag 计入上限）
            }
        }
        if catalog.len() >= state::IMAGES_MAX_REPOS {
            break;
        }
        let (code, _, body) = dockerops::http_registry(root, "GET", &format!("/v2/{repo}/tags/list"), None)?;
        if code != 200 {
            eprintln!("showroomctl: images: {repo}: tags/list HTTP {code}");
            continue;
        }
        let tags: Vec<String> = match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v["tags"]
                .as_array()
                .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            Err(e) => {
                eprintln!("showroomctl: images: {repo}: tags json: {e}");
                continue;
            }
        };
        catalog.push((repo, tags));
        if catalog.iter().map(|(_, t)| t.len()).sum::<usize>() >= state::IMAGES_MAX_TAGS {
            break; // 已够上限，不再发起剩余 repo 的 tags/list
        }
    }
    Ok(state::flatten_images(&catalog, owner))
}

// —— owner 管理（CLI 与 /_api/admin/* 端点共用；owner/kid 均过名字校验防路径拼接）——

/// owner_add 产物：kid + registry 明文密码（密码只此一次返回给远程发卡方；本机 reg.cred 常驻）。
#[derive(Debug)]
pub struct OwnerAdded {
    pub kid: String,
    pub password: String,
}

/// 登记发布者公钥（kid=sha256(pubkey) 前 8 hex）+ 生成 registry 密码（bcrypt cost 10 进
/// htpasswd，明文 0600 进 reg.cred）；admin=true 时同时落 `<kid>.admin` 空标记（SPEC §12.2）。
pub fn owner_add(root: &Path, name: &str, pubkey: &str, admin: bool, replace: bool) -> R<OwnerAdded> {
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
    if admin {
        fs::write(state::admin_marker_path(root, name, &kid), "")?;
    }
    let pw = state::rand_hex(16);
    let hash = bcrypt::hash(&pw, 10).map_err(|e| ApiError::state(format!("bcrypt: {e}")))?;
    state::upsert_htpasswd(root, name, &hash)?;
    state::write_private(
        &root.join(format!("state/owners/{name}/reg.cred")),
        &format!("{name}:{pw}\n"),
    )?;
    Ok(OwnerAdded { kid, password: pw })
}

/// 删除发布者某把公钥；`.admin` 标记随钥匙一并清（无钥匙的孤儿标记无意义）。
pub fn owner_key_rm(root: &Path, owner: &str, kid: &str) -> R<()> {
    state::check_name("owner", owner)?;
    if !state::valid_kid(kid) {
        return Err(ApiError::new("E_NAME", format!("bad kid '{kid}'")));
    }
    let p = state::tokens_dir(root, owner).join(format!("{kid}.pub"));
    if !p.exists() {
        return Err(ApiError::new("E_NOTFOUND", format!("no kid '{kid}' for owner '{owner}'")));
    }
    fs::remove_file(p)?;
    let _ = fs::remove_file(state::admin_marker_path(root, owner, kid));
    Ok(())
}

/// 授权/收权管理员（SPEC §12.2）：钥匙须存在；收权幂等（标记不存在也 Ok）。
pub fn key_set_admin(root: &Path, owner: &str, kid: &str, admin: bool) -> R<()> {
    state::check_name("owner", owner)?;
    if !state::valid_kid(kid) {
        return Err(ApiError::new("E_NAME", format!("bad kid '{kid}'")));
    }
    let dir = state::tokens_dir(root, owner);
    if !dir.join(format!("{kid}.pub")).exists() {
        return Err(ApiError::new("E_NOTFOUND", format!("no kid '{kid}' for owner '{owner}'")));
    }
    let marker = state::admin_marker_path(root, owner, kid);
    if admin {
        fs::write(&marker, "")?;
    } else {
        let _ = fs::remove_file(&marker);
    }
    Ok(())
}

/// 该钥匙是否管理员（标记文件存在即真；owner/kid 非法恒假——serve 只喂自家派生的值）。
pub fn key_is_admin(root: &Path, owner: &str, kid: &str) -> bool {
    state::check_name("owner", owner).is_ok() && state::admin_marker_path(root, owner, kid).exists()
}

/// owner_list 一行：账号 / 钥匙（kid+admin+登记时间=.pub mtime）/ 项目数（conf 归属计数）。
#[derive(serde::Serialize)]
pub struct OwnerKeyInfo {
    pub kid: String,
    pub admin: bool,
    pub since: u64,
}

#[derive(serde::Serialize)]
pub struct OwnerInfo {
    pub owner: String,
    pub keys: Vec<OwnerKeyInfo>,
    pub projects: usize,
}

/// 全部账号聚合（GET /_api/admin/owners 与 CLI admin owners 共用）。
/// 钥匙 kid 由公钥内容派生（与验签侧一致）；坏 .pub 跳过；owner 目录即账号。
pub fn owner_list(root: &Path) -> Vec<OwnerInfo> {
    let projects: BTreeMap<String, usize> = state::all_confs(root).into_iter().fold(
        BTreeMap::new(),
        |mut m, (_, c)| {
            *m.entry(c.owner).or_default() += 1;
            m
        },
    );
    let mut out = vec![];
    if let Ok(rd) = fs::read_dir(root.join("state/tokens")) {
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let Some(owner) = path.file_name().and_then(|o| o.to_str()).map(str::to_string) else {
                continue;
            };
            let keys = state::owner_keys_with_kid(root, &owner)
                .into_iter()
                .map(|(kid, _)| OwnerKeyInfo {
                    admin: key_is_admin(root, &owner, &kid),
                    since: state::file_mtime(&path.join(format!("{kid}.pub"))),
                    kid,
                })
                .collect();
            out.push(OwnerInfo { projects: projects.get(&owner).copied().unwrap_or(0), owner, keys });
        }
    }
    out.sort_by(|a, b| a.owner.cmp(&b.owner));
    out
}

/// 吊销：registry 登录失效 + 全部公钥失效；已发布的展项不动。
pub fn owner_revoke(root: &Path, owner: &str) -> R<()> {
    state::check_name("owner", owner)?;
    state::remove_htpasswd_user(root, owner)?;
    let _ = fs::remove_dir_all(state::tokens_dir(root, owner));
    let _ = fs::remove_dir_all(root.join(format!("state/owners/{owner}")));
    Ok(())
}

// —— admin status（SPEC §12.2）：systemctl/df/docker 子进程 + state 纯解析拼装 ——

/// 平台依赖的 systemd 服务（is-active 四件：容器引擎/运行时/边缘/控制面）。
pub const STATUS_SERVICES: [&str; 4] = ["docker", "containerd", "caddy", "showroom-api"];

/// GET /_api/admin/status 数据源（全部 subprocess，serve 经 blk() 调用）：
/// 服务 systemctl is-active（非零退出仍取 stdout，"inactive" 也是答案）；磁盘 df -B1
/// --output=size,used（coreutils 无 total 字段，size 同义；根分区 + @showroom 子卷各一行）；
/// 内存 /proc/meminfo MemAvailable；容器 docker ps -a；timers systemctl list-timers（'showroom-*'，
/// 只列平台自身，不外泄宿主无关单元）。
pub fn admin_status(root: &Path) -> R<Value> {
    let mut services = serde_json::Map::new();
    for svc in STATUS_SERVICES {
        let st = dockerops::run_tolerant("systemctl", &["is-active", svc]);
        services.insert(svc.into(), Value::String(if st.is_empty() { "unknown".into() } else { st }));
    }
    let mut disk = serde_json::Map::new();
    let paths = [("root", "/"), ("showroom", &root.to_string_lossy())];
    for (label, path) in paths {
        let entry = match dockerops::run("df", &["-B1", "--output=size,used", path])
            .ok()
            .and_then(|out| state::parse_df_bytes(&out))
        {
            Some((total, used)) => json!({"total": total, "used": used}),
            None => Value::Null,
        };
        disk.insert(label.into(), entry);
    }
    let mem_mb = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| state::parse_meminfo_available_kb(&t))
        .map(|kb| kb / 1024);
    let containers = dockerops::run("docker", &["ps", "-a", "--format", "{{.Names}}\t{{.State}}"])
        .map(|out| state::parse_container_lines(&out).into_iter()
            .map(|(name, status)| json!({"name": name, "status": status}))
            .collect::<Vec<_>>())
        .unwrap_or_default();
    let timers = dockerops::run_tolerant("systemctl", &["list-timers", "--no-legend", "showroom-*"]);
    let timers: Vec<_> = state::parse_list_timers(&timers)
        .into_iter()
        .map(|(name, next)| json!({"name": name, "next": next}))
        .collect();
    Ok(json!({
        "services": services, "disk": disk,
        "mem": {"available_mb": mem_mb},
        "containers": containers, "timers": timers,
    }))
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

    fn troot(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("srctl-ops-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 直接落一对钥匙文件（免 bcrypt，单测用）；返回 kid。
    fn put_key(root: &Path, owner: &str, admin: bool) -> String {
        let k = showroom_common::OwnerKey::generate();
        let vk = showroom_common::OwnerKey::from_pub_string(&k.pub_string()).unwrap();
        let kid = state::kid_of(&vk);
        let dir = state::tokens_dir(root, owner);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{kid}.pub")), k.pub_string()).unwrap();
        if admin {
            fs::write(state::admin_marker_path(root, owner, &kid), "").unwrap();
        }
        kid
    }

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

    // admin 标记存在性判定：有 `<kid>.admin` 才是管理员钥匙；普通钥匙/他人 owner/非法名恒假
    #[test]
    fn admin_marker_existence() {
        let root = troot("is-admin");
        let admin_kid = put_key(&root, "alice", true);
        let plain_kid = put_key(&root, "alice", false);
        assert!(key_is_admin(&root, "alice", &admin_kid));
        assert!(!key_is_admin(&root, "alice", &plain_kid));
        assert!(!key_is_admin(&root, "alice", "ffffffff"), "不存在的 kid 恒假");
        assert!(!key_is_admin(&root, "bob", &admin_kid), "他人 owner 恒假");
        assert!(!key_is_admin(&root, "Bad Name", &admin_kid), "非法 owner 名恒假");
        assert!(!key_is_admin(&root, "alice", "../x"), "非法 kid 恒假（防穿越探测）");
    }

    // owner_add（上收公共函数）：admin 标记、htpasswd/reg.cred、密码只此一次返回
    #[test]
    fn owner_add_with_admin_flag() {
        let root = troot("owner-add");
        let k = showroom_common::OwnerKey::generate();

        let added = owner_add(&root, "root", &k.pub_string(), true, false).unwrap();
        assert_eq!(added.kid.len(), 8);
        assert!(!added.password.is_empty());
        // 标记 + 公钥落位
        assert!(state::admin_marker_path(&root, "root", &added.kid).exists());
        assert!(state::tokens_dir(&root, "root").join(format!("{}.pub", added.kid)).exists());
        // 密码与 reg.cred 一致；htpasswd 有 bcrypt 行
        let cred = fs::read_to_string(root.join("state/owners/root/reg.cred")).unwrap();
        assert_eq!(cred, format!("root:{}\n", added.password));
        let htpasswd = fs::read_to_string(state::htpasswd_path(&root)).unwrap();
        assert!(htpasswd.starts_with("root:$2"), "bcrypt 行: {htpasswd}");

        // 非 admin：无标记
        let k2 = showroom_common::OwnerKey::generate();
        let added2 = owner_add(&root, "bob", &k2.pub_string(), false, false).unwrap();
        assert!(!state::admin_marker_path(&root, "bob", &added2.kid).exists());

        // 坏 owner 名 / 坏公钥
        assert_eq!(owner_add(&root, "Bad", &k2.pub_string(), false, false).unwrap_err().code, "E_NAME");
        assert_eq!(owner_add(&root, "ok", "junk", false, false).unwrap_err().code, "E_NAME");
    }

    // key_set_admin：授权/收权幂等；钥匙不存在 E_NOTFOUND；kid 非法 E_NAME
    #[test]
    fn key_set_admin_grant_revoke() {
        let root = troot("set-admin");
        let kid = put_key(&root, "alice", false);

        assert!(!key_is_admin(&root, "alice", &kid));
        key_set_admin(&root, "alice", &kid, true).unwrap();
        assert!(key_is_admin(&root, "alice", &kid));
        key_set_admin(&root, "alice", &kid, true).unwrap(); // 重复授权幂等
        assert!(key_is_admin(&root, "alice", &kid));

        key_set_admin(&root, "alice", &kid, false).unwrap();
        assert!(!key_is_admin(&root, "alice", &kid));
        key_set_admin(&root, "alice", &kid, false).unwrap(); // 重复收权幂等

        assert_eq!(key_set_admin(&root, "alice", "ffffffff", true).unwrap_err().code, "E_NOTFOUND");
        assert_eq!(key_set_admin(&root, "alice", "../evil", true).unwrap_err().code, "E_NAME");
        assert_eq!(key_set_admin(&root, "Bad", &kid, true).unwrap_err().code, "E_NAME");
    }

    // key_rm：公钥+admin 标记双清；不存在 E_NOTFOUND；kid 非法 E_NAME
    #[test]
    fn key_rm_clears_both_files() {
        let root = troot("key-rm");
        let kid = put_key(&root, "alice", true);
        let plain = put_key(&root, "alice", false);

        owner_key_rm(&root, "alice", &kid).unwrap();
        assert!(!state::tokens_dir(&root, "alice").join(format!("{kid}.pub")).exists());
        assert!(!state::admin_marker_path(&root, "alice", &kid).exists(), "标记随钥匙清");

        assert_eq!(owner_key_rm(&root, "alice", &kid).unwrap_err().code, "E_NOTFOUND");
        assert_eq!(owner_key_rm(&root, "alice", "../x").unwrap_err().code, "E_NAME");
        assert_eq!(owner_key_rm(&root, "Bad", &plain).unwrap_err().code, "E_NAME");
    }

    // owner_list 聚合：账号/钥匙（kid+admin+since）/项目数；无钥匙账号也列出；字典序
    #[test]
    fn owner_list_aggregation() {
        let root = troot("list");
        let a_admin = put_key(&root, "alice", true);
        let a_plain = put_key(&root, "alice", false);
        put_key(&root, "bob", false);
        fs::create_dir_all(state::tokens_dir(&root, "empty")).unwrap(); // 钥匙删光的账号
        // 两个项目归 alice、一个归 bob（owner_list 只读 conf，不碰 docker）
        for (p, owner) in [("demo1", "alice"), ("demo2", "alice"), ("demo3", "bob")] {
            let mut c = crate::state::Conf {
                owner: owner.into(), tag: "v1".into(), port: 8100, cport: 8080,
                mem: "256m".into(), cpu: 1.0, redisdb: 0, updated: 1,
                stopped: false, legacyslot: None,
            };
            c.owner = owner.into();
            c.save(&root, p).unwrap();
        }

        let l = owner_list(&root);
        assert_eq!(l.iter().map(|o| o.owner.as_str()).collect::<Vec<_>>(), ["alice", "bob", "empty"]);
        let alice = &l[0];
        assert_eq!(alice.projects, 2);
        assert_eq!(alice.keys.len(), 2);
        let admin_key = alice.keys.iter().find(|k| k.kid == a_admin).unwrap();
        assert!(admin_key.admin);
        assert!(admin_key.since > 0, "since=登记 mtime");
        let plain_key = alice.keys.iter().find(|k| k.kid == a_plain).unwrap();
        assert!(!plain_key.admin);
        assert_eq!(l[1].projects, 1);
        assert!(l[2].keys.is_empty(), "无钥匙账号仍列出（projects=0）");
        assert_eq!(l[2].projects, 0);
    }
}
