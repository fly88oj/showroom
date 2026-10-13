//! ops：子命令级操作（stop/start/rm/rollback/ps/logs/env/reconcile/autoheal/gc*/user*/init-keys）。
//! serve 复用同一批函数；user 归属校验在 serve 层做（CLI 是管理员直发通道）。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use showroom_common::opaque;

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

/// start：清 stopped 标记；容器缺失则按 conf 的 tag 原地重建（版本记账不动——重建不是发布）。
pub fn start(root: &Path, p: &str) -> R<()> {
    let mut c = conf_of(root, p)?;
    let name = publish::container_name(p);
    if dockerops::container_status(&name).is_some() {
        dockerops::docker(&["start", &name])?;
    } else {
        publish::redeploy(root, &c.user, p, &c.tag, PublishOpts {
            mem: Some(c.mem.clone()),
            cpu: Some(c.cpu),
            cport: Some(c.cport),
        }, None)?;
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

/// 上一版本选取（纯函数）：发布历史中当前版本之前最近的一条（append-only，倒序首个 ≠ 当前）。
pub fn previous_version(all: &[state::VersionRec], current: &str) -> Option<String> {
    all.iter().rev().find(|r| r.version != current).map(|r| r.version.clone())
}

/// rollback：回滚到上一版本（无显式版本号的入口——CLI 与旧客户端）。
/// 有发布历史 → 取历史中当前版本之前最近的一条按其镜像重部署；无历史的老项目按 last2 兜底。
/// 回滚语义（修订）：回滚到 vM = 用 vM 记录的镜像重部署 + 当前版本置为 vM；version_seq 不动
/// （单调不复用，回滚后再发布 = v{seq+1}）。
pub fn rollback(root: &Path, p: &str) -> R<PublishOut> {
    let c = conf_of(root, p)?;
    let all = state::read_versions(root, p);
    if let Some(v) = previous_version(&all, &c.version) {
        return rollback_to(root, p, &v);
    }
    if !all.is_empty() {
        return Err(ApiError::new("E_STATE", format!("'{p}' 无可回滚的历史版本（仅一个版本）")));
    }
    // 升级前发布的老项目：无发布历史 → last2 兜底（原地重部署，版本记账不动）
    let prev = state::read_last2(root, p)
        .ok_or_else(|| ApiError::new("E_NOTFOUND", format!("no last2 for '{p}'")))?;
    publish::redeploy(root, &c.user, p, &prev, PublishOpts {
        mem: Some(c.mem.clone()),
        cpu: Some(c.cpu),
        cport: Some(c.cport),
    }, None)
}

/// 回滚到指定版本：按 versions.json 该条记录的镜像重部署 + 当前版本置为该版本（不落新历史、不动计数）。
pub fn rollback_to(root: &Path, p: &str, version: &str) -> R<PublishOut> {
    let c = conf_of(root, p)?;
    let all = state::read_versions(root, p);
    let rec = state::find_version(&all, version)
        .ok_or_else(|| ApiError::new("E_NOTFOUND", format!("no version '{version}' for '{p}'")))?;
    let tag = state::tag_of_image(&rec.image)
        .ok_or_else(|| ApiError::new("E_STATE", format!("version '{version}' 记录的镜像引用无 tag：{}", rec.image)))?;
    publish::redeploy(root, &c.user, p, &tag, PublishOpts {
        mem: Some(c.mem.clone()),
        cpu: Some(c.cpu),
        cport: Some(c.cport),
    }, Some(version.to_string()))
}

/// GET /_api/versions 数据源：发布历史 + 当前版本 + 每条附 imagePresent
/// （复用镜像删除的 manifest 检查通道：404 → false「镜像已删除」；registry 不可达等错误 →
/// null「未知」——不冒充已删除，也不因一次抖动拖垮整个回滚对话框）。
pub fn versions_of(root: &Path, p: &str) -> R<Value> {
    let c = conf_of(root, p)?;
    let all = state::read_versions(root, p);
    let repo = format!("{}/{}", c.user, p.to_ascii_lowercase());
    let mut items = Vec::with_capacity(all.len());
    for r in &all {
        let present = match state::tag_of_image(&r.image) {
            Some(t) => match dockerops::manifest_size(root, &repo, &t) {
                Ok(Some(_)) => json!(true),
                Ok(None) => json!(false),
                Err(_) => Value::Null,
            },
            None => json!(false),
        };
        items.push(json!({
            "version": r.version, "image": r.image, "time": r.time, "imagePresent": present,
        }));
    }
    let current = if c.version.is_empty() { c.tag.clone() } else { c.version.clone() };
    Ok(json!({"project": p, "current": current, "versions": items}))
}

/// ps / GET /_api/projects 的数据源：conf 全量 + docker 实况。
pub fn ps(root: &Path) -> Vec<(String, Conf, String)> {
    let statuses = dockerops::all_container_status(); // 一次 docker ps，N+1→1（P1 性能修复）
    state::all_confs(root)
        .into_iter()
        .map(|(p, c)| {
            let name = publish::container_name(&p);
            let st = statuses.get(&name).cloned().unwrap_or_else(|| "missing".into());
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

// —— Redis 浏览内核（SPEC §10.3，排障用；跨 user 不隔离——SPEC 已注明）——

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

/// reconcile：先跑 v1.4 迁移编排（§11，幂等）与 owner→user 状态目录迁移，再遍历项目 conf，容器/路由缺失按 conf 重建。
pub fn reconcile(root: &Path) -> Vec<String> {
    if state::migrate_state_dirs(root) {
        eprintln!("showroomctl: reconcile: state dirs migrated (owners -> users)");
    }
    let mut log = migrate_legacy(root);
    let statuses = dockerops::all_container_status(); // 批量取（P2：N+1→1）
    for (p, c) in state::all_confs(root) {
        if c.stopped {
            continue;
        }
        let name = publish::container_name(&p);
        match statuses.get(&name).map(|s| s.as_str()).map(str::to_string) {
            None => match publish::redeploy(root, &c.user, &p, &c.tag, PublishOpts {
                mem: Some(c.mem.clone()),
                cpu: Some(c.cpu),
                cport: Some(c.cport),
            }, None) {
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
    let statuses = dockerops::all_container_status(); // 批量取（P2：N+1→1）
    for (p, c) in state::all_confs(root) {
        if c.stopped {
            continue;
        }
        let name = publish::container_name(&p);
        match statuses.get(&name).map(|s| s.as_str()).map(str::to_string) {
            None => match publish::redeploy(root, &c.user, &p, &c.tag, PublishOpts {
                mem: Some(c.mem.clone()),
                cpu: Some(c.cpu),
                cport: Some(c.cport),
            }, None) {
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
/// user=Some 仅取 `<user>/` 命名空间 repo（提前过滤省 HTTP 往返）；None（admin）全量。
/// repo 数与 tag 总量受 state::IMAGES_MAX_REPOS/TAGS 上限；单 repo tags/list 失败跳过如实记录。
pub fn images(root: &Path, user: Option<&str>) -> R<Vec<String>> {
    let (code, _, body) = dockerops::http_registry(root, "GET", "/v2/_catalog", None)?;
    if code != 200 {
        return Err(ApiError::state(format!("registry catalog: HTTP {code}")));
    }
    let cat: Value = serde_json::from_slice(&body).map_err(|e| ApiError::state(format!("catalog json: {e}")))?;
    let repos: Vec<String> = cat["repositories"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    // 过滤到目标 repos，并行拉 tags（P1 并发修复：N repo 串行→并行）
    let targets: Vec<String> = repos
        .into_iter()
        .filter(|repo| user.map(|o| state::repo_owned_by(repo, o)).unwrap_or(true))
        .take(state::IMAGES_MAX_REPOS)
        .collect();
    let results: Vec<(String, Vec<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|repo| {
                scope.spawn(move || {
                    let (code, _, body) = match dockerops::http_registry(root, "GET", &format!("/v2/{repo}/tags/list"), None) {
                        Ok(v) => v,
                        Err(_) => return (repo.clone(), vec![]),
                    };
                    if code != 200 {
                        eprintln!("showroomctl: images: {repo}: tags/list HTTP {code}");
                        return (repo.clone(), vec![]);
                    }
                    let tags: Vec<String> = serde_json::from_slice::<Value>(&body)
                        .ok()
                        .and_then(|v| v["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect()))
                        .unwrap_or_default();
                    (repo.clone(), tags)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    // 排序保持确定性（scope 并行完成顺序不确定）
    let mut catalog: Vec<(String, Vec<String>)> = results;
    catalog.sort_by(|a, b| a.0.cmp(&b.0));
    // 上限裁剪（并行拉完后统一截）
    let mut total = 0usize;
    catalog.retain(|(_, tags)| {
        total += tags.len();
        total <= state::IMAGES_MAX_TAGS
    });
    Ok(state::flatten_images(&catalog, user))
}

// —— 磁盘空间与镜像删除（SPEC §14；serve /_api/space 与 /_api/image-rm 内核，CLI 共用）——

/// 根分区 df 一次调用 → (size, avail)（§14.2 口径：镜像/docker 层/registry 同属一个 btrfs
/// 文件系统，`df /` 即三者并集真值）。失败/畸形 → E_STATE（fail-closed，msg 带 "df /"）。
fn df_root_size_avail() -> R<(u64, u64)> {
    let out = dockerops::run("df", &["-B1", "--output=size,avail", "/"])
        .map_err(|e| ApiError::state(format!("df /: {e}")))?;
    state::parse_df_bytes(&out).ok_or_else(|| ApiError::state("df /: malformed output"))
}

/// GET /_api/space 数据源：`{"available_mb","reserve_mb","total_mb"}` 三元组。
/// available_mb＝根 df avail**未扣预留**（MB 向下取整）；reserve_mb＝DISK_RESERVE_GB×1024；
/// total_mb＝根 df size。控制台显示 available_mb − reserve_mb。
pub fn space_snapshot(root: &Path) -> R<Value> {
    let (size, avail) = df_root_size_avail()?;
    let policy = state::Policy::load(root);
    Ok(json!({
        "available_mb": avail >> 20,
        "reserve_mb": policy.disk_reserve_gb * 1024,
        "total_mb": size >> 20,
    }))
}

/// 根分区当前 avail 字节（image-rm 前后差用；与 space_snapshot 各自独立调用，不缓存）。
pub fn disk_avail_bytes() -> R<u64> {
    df_root_size_avail().map(|(_, a)| a)
}

/// image-rm 内核（§14.3 内联 GC 流程）：manifest digest → DELETE manifest → registry-gc → df 前后差。
/// 返回 freed_est_mb（i64≥0；btrfs 偶发回吐为负时钳 0，如实不虚报）。
/// 失败路径语义：DELETE 已成而 registry-gc 失败 → E_STATE 如实报部分成功（每周 timer 会补收）；
/// manifest GET 404 → E_NOTFOUND（镜像不存在），其余非 200 → E_STATE（dockerops::manifest_digest 分档）。
pub fn image_rm(root: &Path, repo: &str, tag: &str) -> R<i64> {
    let avail0 = disk_avail_bytes()?;
    let digest = dockerops::manifest_digest(root, repo, tag)?;
    if !state::valid_digest(&digest) {
        return Err(ApiError::state(format!("registry 返回非法 digest '{digest}'（拒拼接进 DELETE URL）")));
    }
    dockerops::delete_manifest(root, repo, &digest)?;
    // 复用 Cmd::RegistryGc 实现体（停 sr-registry → garbage-collect --delete-untagged → 重启；
    // 秒级，live-restore 保运行中容器；--delete-untagged 只清无引用 blob，多 tag 共享层保留）
    if let Err(e) = registry_gc(root) {
        return Err(ApiError::state(format!(
            "manifest 已删但 registry-gc 失败：{e}；空间暂未回收，每周 registry-gc timer 会补收，或稍后重试"
        )));
    }
    let freed = (disk_avail_bytes()? as i64).saturating_sub(avail0 as i64) >> 20;
    Ok(freed.max(0))
}

// —— user 管理（CLI 与 /_api/admin/* 端点共用；user/kid 均过名字校验防路径拼接）——

/// user_add 的初始登录密码（plan-16 §1.4）：统一账号密码认证（B7）——发卡必须设初始密码。
#[derive(Debug, Clone)]
pub enum InitPw {
    /// 管理员显式给定。
    Set(String),
    /// 自动生成（一次性返回，24 hex 字符）。
    Gen,
}

/// user_add 产物：登录密码（只此一次返回）+ registry 明文
/// 密码（只此一次返回给远程发卡方；本机 reg.cred 常驻）。
#[derive(Debug)]
pub struct UserAdded {
    pub login_password: Option<String>,
    pub password: String,
}

/// 服务端 OPRF key（plan-16 §1.2）：`state/opaque.key`（b64 0600）缺失自动生成（幂等；
/// 同 platform_rsa 模式，serve 启动与 CLI 设密码路径共用）。
pub fn opaque_setup(root: &Path) -> R<opaque::server_types::Setup> {
    let p = state::opaque_key_path(root);
    let b64_key = match fs::read_to_string(&p) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            let s = opaque::server_setup_new(&mut rand::rngs::OsRng);
            state::write_private(&p, &s)?;
            s
        }
    };
    opaque::server_setup_load(&b64_key).map_err(|e| ApiError::state(format!("{e}")))
}

/// 进程内 OPAQUE 注册（user-add 设密码 / passwd-set 替换共用，plan-16 §1.4）：注册协议
/// 双端在本地一次跑完（ke1/ke2/ke3 不过网线），明文初始密码只过管理员终端与主机进程。
fn register_in_process(root: &Path, user: &str, pw: &str) -> R<()> {
    let setup = opaque_setup(root)?;
    let mut rng = rand::rngs::OsRng;
    let (cstate, m1) =
        opaque::reg_start(&mut rng, pw, user).map_err(|e| ApiError::state(format!("{e}")))?;
    let m2 = opaque::server_reg_start(&setup, &m1, user)
        .map_err(|e| ApiError::state(format!("{e}")))?;
    let record = opaque::reg_finish(&mut rng, cstate, pw, user, &m2)
        .map_err(|e| ApiError::state(format!("{e}")))?;
    let record = opaque::server_reg_canonical(&record)
        .map_err(|e| ApiError::state(format!("{e}")))?;
    state::write_private(&state::pass_opaque_path(root, user), &record)
}

/// 生成登录初始密码（plan-16 §1.4 --gen-password / 面板默认）：24 hex 字符（96bit 熵）。
pub fn gen_login_password() -> String {
    state::rand_hex(12)
}

/// 重置/补设登录密码（plan-16 §1.4）：进程内注册替换 pass.opaque，不动既有钥匙
///（疑似盗号走 user-revoke/key-rm）。账号须已存在（tokens 或 users 目录其一）。
pub fn user_passwd_set(root: &Path, user: &str, pw: &str) -> R<()> {
    state::check_name("user", user)?;
    if pw.is_empty() {
        return Err(ApiError::new("E_NAME", "password 不能为空"));
    }
    if !state::tokens_dir(root, user).is_dir() && !state::users_dir(root, user).is_dir() {
        return Err(ApiError::new("E_NOTFOUND", format!("no user '{user}'")));
    }
    register_in_process(root, user, pw)
}

/// 发账号（B7 统一账号密码认证）：设初始登录密码 + 发 registry 凭据。
/// registry 密码：bcrypt cost 10 进 htpasswd、明文 0600 进 reg.cred；
/// admin=true 时记账号级待继承标记，首把登录绑定钥匙继承（SPEC §16）。
pub fn user_add(root: &Path, name: &str, admin: bool, init_pw: InitPw) -> R<UserAdded> {
    state::check_name("user", name)?;
    // 管理员是账号属性（R4 裁决）：直接落账号级标记，权限不随钥匙生灭
    if admin {
        state::set_user_admin(root, name, true);
    }
    let login_password = match init_pw {
        InitPw::Set(pw) => {
            register_in_process(root, name, &pw)?;
            Some(pw)
        }
        InitPw::Gen => {
            let pw = gen_login_password();
            register_in_process(root, name, &pw)?;
            Some(pw)
        }
    };
    let pw = state::rand_hex(16);
    let hash = bcrypt::hash(&pw, 10).map_err(|e| ApiError::state(format!("bcrypt: {e}")))?;
    state::upsert_htpasswd(root, name, &hash)?;
    state::write_private(
        &state::users_dir(root, name).join("reg.cred"),
        &format!("{name}:{pw}\n"),
    )?;
    Ok(UserAdded { login_password, password: pw })
}

/// 密码登录后绑定新钥匙（plan-16 §2.2）：幂等（同钥匙重绑=ok 补 .bound）+ .bound 标记
///（受 keep-N GC 约束的密码绑定钥匙）+ admin 继承（账号已有任一 admin 钥匙 → 新钥匙
/// 继承：密码即账号级最高凭证，SPEC §16）+ GC（policy BOUND_KEYS_MAX）。
/// 返回 (kid, admin)。
pub fn key_bind(root: &Path, user: &str, pubkey: &str) -> R<(String, bool)> {
    state::check_name("user", user)?;
    let vk = showroom_common::UserKey::from_pub_string(pubkey.trim())
        .map_err(|e| ApiError::new("E_NAME", format!("bad pubkey: {e}")))?;
    let kid = state::kid_of(&vk);
    let dir = state::tokens_dir(root, user);
    fs::create_dir_all(&dir)?;
    // 幂等：同钥匙重绑 = 重写 .pub + 补 .bound 标记
    fs::write(dir.join(format!("{kid}.pub")), format!("{}\n", pubkey.trim()))?;
    fs::write(state::bound_marker_path(root, user, &kid), "")?;
    // R4 裁决：admin 是账号属性（users/<user>/admin），钥匙无级——登录响应只回传账号结论
    let admin = state::user_is_admin(root, user);
    let policy = state::Policy::load(root);
    for gone in state::bound_keys_gc(root, user, policy.bound_keys_max, Some(&kid)) {
        eprintln!("showroomctl: key-bind: GC 旧绑定钥匙 {user}/{gone}（保最近 {} 把）", policy.bound_keys_max);
    }
    Ok((kid, admin))
}

/// 删除发布者某把公钥；`.admin` 标记随钥匙一并清（无钥匙的孤儿标记无意义）。
pub fn user_key_rm(root: &Path, user: &str, kid: &str) -> R<()> {
    state::check_name("user", user)?;
    if !state::valid_kid(kid) {
        return Err(ApiError::new("E_NAME", format!("bad kid '{kid}'")));
    }
    let p = state::tokens_dir(root, user).join(format!("{kid}.pub"));
    if !p.exists() {
        return Err(ApiError::new("E_NOTFOUND", format!("no kid '{kid}' for user '{user}'")));
    }
    fs::remove_file(p)?;
    let _ = fs::remove_file(state::admin_marker_path(root, user, kid));
    Ok(())
}

/// 授权/收权管理员（R4 裁决：账号级，无钥匙维度）；账号须存在；收权幂等。
pub fn user_set_admin(root: &Path, user: &str, admin: bool) -> R<()> {
    state::check_name("user", user)?;
    if !state::users_dir(root, user).exists() && !state::tokens_dir(root, user).exists() {
        return Err(ApiError::new("E_NOTFOUND", format!("no user '{user}'")));
    }
    state::set_user_admin(root, user, admin);
    Ok(())
}

/// user_list 一行：账号（admin 账号级）/ 钥匙（kid+绑定时间=.pub mtime）/ 项目数。
/// R5 D2：钥匙级 admin 字段已随 web 批次同步移除（admin 恒为账号级，见 UserInfo.admin）。
#[derive(serde::Serialize)]
pub struct UserKeyInfo {
    pub kid: String,
    pub since: u64,
}

#[derive(serde::Serialize)]
pub struct UserInfo {
    pub user: String,
    pub keys: Vec<UserKeyInfo>,
    pub projects: usize,
    /// 管理员（R4 裁决：账号级，钥匙无级）。
    pub admin: bool,
    /// 是否已设登录密码（pass.opaque 存在；plan-16 §1.5 面板「未设密码」徽标→一键补发）。
    pub has_password: bool,
}

/// 全部账号聚合（GET /_api/admin/users 与 CLI admin users 共用）。
/// 钥匙 kid 由公钥内容派生（与验签侧一致）；坏 .pub 跳过；user 目录即账号。
pub fn user_list(root: &Path) -> Vec<UserInfo> {
    state::migrate_admin_markers(root); // serve 启动已迁；此处兜底 CLI 直跑路径
    let projects: BTreeMap<String, usize> = state::all_confs(root).into_iter().fold(
        BTreeMap::new(),
        |mut m, (_, c)| {
            *m.entry(c.user).or_default() += 1;
            m
        },
    );
    // B7 统一账号密码认证：账号可先于首把钥匙存在（纯密码发卡，登录时才绑定钥匙）——
    // 账号清单取 state/tokens（有钥匙）与 state/users（有记录）两目录并集
    let mut users: BTreeSet<String> = BTreeSet::new();
    for d in [root.join("state/tokens"), root.join("state/users")] {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.filter_map(|e| e.ok()) {
                let path = e.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(user) = path.file_name().and_then(|o| o.to_str()) {
                    users.insert(user.to_string());
                }
            }
        }
    }
    let mut out = users
        .into_iter()
        .map(|user| {
            state::sweep_stale_keys(root, &user); // 管理页每次查看即清扫（过期僵尸/旧机制存量/孤儿标记）
            let keys = state::user_keys_with_kid(root, &user)
                .into_iter()
                .map(|(kid, _)| UserKeyInfo {
                    since: state::file_mtime(&state::tokens_dir(root, &user).join(format!("{kid}.pub"))),
                    kid,
                })
                .collect();
            UserInfo {
                admin: state::user_is_admin(root, &user), // R4：账号级
                has_password: state::read_pass_opaque(root, &user).is_some(),
                projects: projects.get(&user).copied().unwrap_or(0),
                user,
                keys,
            }
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.user.cmp(&b.user));
    out
}

/// 吊销：registry 登录失效 + 全部公钥失效；已发布的展项不动。
/// 凭据目录双清（state/users 与更名前遗留的 state/owners）。
pub fn user_revoke(root: &Path, user: &str) -> R<()> {
    state::check_name("user", user)?;
    state::remove_htpasswd_user(root, user)?;
    let _ = fs::remove_dir_all(state::tokens_dir(root, user));
    let _ = fs::remove_dir_all(state::users_dir(root, user));
    let _ = fs::remove_dir_all(state::legacy_owners_dir(root, user));
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
    // §15.3：redis 池水位常驻（与 /_api/admin/host 同源）
    let redis_pool = crate::middleware::redis_pool_snapshot(root);
    Ok(json!({
        "services": services, "disk": disk,
        "mem": {"available_mb": mem_mb},
        "containers": containers, "timers": timers,
        "redis_pool": redis_pool,
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
    use std::os::unix::fs::PermissionsExt as _;

    fn troot(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录（pid+原子计数），固定名复用会互相 remove_dir_all
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "srctl-ops-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 直接落一对钥匙文件（免 bcrypt，单测用）；返回 kid。
    fn put_key(root: &Path, user: &str, admin: bool) -> String {
        let k = showroom_common::UserKey::generate();
        let vk = showroom_common::UserKey::from_pub_string(&k.pub_string()).unwrap();
        let kid = state::kid_of(&vk);
        let dir = state::tokens_dir(root, user);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{kid}.pub")), k.pub_string()).unwrap();
        // B7 后真实形状：登录绑定钥匙必有 .bound（无 .bound=旧机制存量，user_list 清扫会删）
        fs::write(dir.join(format!("{kid}.bound")), "").unwrap();
        if admin {
            state::set_user_admin(root, user, true); // R4：admin 是账号属性
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

    // 回滚按版本：上一版本选取（纯函数）+ rollback_to/versions_of 的校验与数据形状（不触 docker 的路径）
    #[test]
    fn rollback_version_selection() {
        let rec = |v: &str| state::VersionRec {
            version: v.into(),
            image: format!("127.0.0.1:5000/alice/demo:u{v}"),
            time: 1,
        };
        let all: Vec<_> = ["v1", "v2", "v3"].iter().map(|v| rec(v)).collect();
        // 当前 v3 → 上一版本 v2；当前 v2（已回滚态）→ 最近一个非当前 = v3
        assert_eq!(previous_version(&all, "v3").as_deref(), Some("v2"));
        assert_eq!(previous_version(&all, "v2").as_deref(), Some("v3"));
        assert_eq!(previous_version(&all, "v1").as_deref(), Some("v3"));
        assert_eq!(previous_version(&[], "v1"), None, "无历史 → None（走 last2 兜底）");
        let single = vec![rec("v1")];
        assert_eq!(previous_version(&single, "v1"), None, "仅一个版本 → 无可回");

        // rollback_to：未知版本 → E_NOTFOUND；项目不存在 → E_NOTFOUND（先于版本查）
        let root = troot("rollback-ver");
        fs::create_dir_all(state::projects_dir(&root)).unwrap();
        let c = state::Conf {
            user: "alice".into(), tag: "u3".into(), port: 8100, cport: 8080,
            mem: "256m".into(), cpu: 1.0, redisdb: 1, updated: 1, stopped: false,
            legacyslot: None, version: "v3".into(), version_seq: 3,
        };
        c.save(&root, "demo").unwrap();
        state::append_version(&root, "demo", rec("v1")).unwrap();
        state::append_version(&root, "demo", rec("v2")).unwrap();
        state::append_version(&root, "demo", rec("v3")).unwrap();
        assert_eq!(rollback_to(&root, "demo", "v9").unwrap_err().code, "E_NOTFOUND");
        assert_eq!(rollback_to(&root, "noproj", "v1").unwrap_err().code, "E_NOTFOUND");

        // versions_of：历史 + 当前版本 + imagePresent（测试环境无 registry → null「未知」，不阻塞）
        let v = versions_of(&root, "demo").unwrap();
        assert_eq!(v["current"], "v3");
        let items = v["versions"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["version"], "v1");
        assert_eq!(items[2]["image"], "127.0.0.1:5000/alice/demo:uv3");
        assert!(items[0]["imagePresent"].is_null(), "registry 不可达 → imagePresent=null（未知，不冒充已删除）");
        // 无历史的项目：current 回落 tag（存量 conf 无 version 键）
        let mut legacy = c.clone();
        legacy.version = String::new();
        legacy.version_seq = 0;
        legacy.save(&root, "old").unwrap();
        let lv = versions_of(&root, "old").unwrap();
        assert_eq!(lv["current"], "u3", "current 回落 tag");
        assert!(lv["versions"].as_array().unwrap().is_empty());
    }

    // R4 账号级 admin：users/<user>/admin 存在即管理员；他人 user/非法名恒假；
    // 旧 kid 级标记经 migrate_admin_markers 迁移为账号级并清除
    #[test]
    fn user_admin_account_level() {
        let root = troot("is-admin");
        put_key(&root, "alice", true);
        put_key(&root, "alice", false);
        assert!(state::user_is_admin(&root, "alice"));
        assert!(!state::user_is_admin(&root, "bob"), "他人 user 恒假");
        assert!(!state::user_is_admin(&root, "Bad Name"), "非法 user 名恒假");
        state::set_user_admin(&root, "alice", false);
        assert!(!state::user_is_admin(&root, "alice"), "收权即移除");
        // 旧 kid 标记迁移：手工放一个 kid .admin → migrate 后账号级成立、kid 标记清除
        let kid = "deadbeef";
        fs::create_dir_all(state::tokens_dir(&root, "bob")).unwrap();
        fs::write(state::admin_marker_path(&root, "bob", kid), "").unwrap();
        state::migrate_admin_markers(&root);
        assert!(state::user_is_admin(&root, "bob"), "kid 标记迁移为账号级");
        assert!(!state::admin_marker_path(&root, "bob", kid).exists(), "旧 kid 标记清除");
    }

    // user_add（上收公共函数）：admin 标记、htpasswd/reg.cred、密码只此一次返回。
    // B7 统一账号密码认证：无 pubkey 路径，发卡必须设初始密码；login_password 只此一次返回。
    #[test]
    fn password_only_admin_pending_flow() {
        let root = troot("admin_pending");
        fs::create_dir_all(root.join("state")).unwrap();
        // 密码发卡 + admin：直接落账号级标记（R4：admin 是账号属性）
        let added = user_add(&root, "bob", true, InitPw::Set("pw123456".into())).unwrap();
        assert!(added.login_password.is_some());
        assert!(state::user_is_admin(&root, "bob"));
        // 登录绑定钥匙回传账号结论；admin 不随钥匙生灭
        let k1 = showroom_common::UserKey::generate();
        let (_, a1) = key_bind(&root, "bob", &k1.pub_string()).unwrap();
        assert!(a1, "首把绑定钥匙应继承 admin");
        assert!(!root.join("state/users/bob/admin.pending").exists(), "pending 应一次性消费");
        // k1 已是 admin 钥：后续钥匙按既有规则继续继承（pending 只是首把的引导，账号从此是管理员）
        let k2 = showroom_common::UserKey::generate();
        let (_, a2) = key_bind(&root, "bob", &k2.pub_string()).unwrap();
        assert!(a2, "已有 admin 钥匙（k1）时后续钥匙应按既有规则继承");
    }

    #[test]
    fn user_add_with_admin_flag() {
        let root = troot("user-add");

        let added = user_add(&root, "root", true, InitPw::Set("pw-123456".into())).unwrap();
        assert!(added.login_password.as_deref() == Some("pw-123456"), "登录密码只此一次返回");
        assert!(!added.password.is_empty());
        // R4：admin 是账号属性——直接落账号级标记，不再预登记公钥
        assert!(state::user_is_admin(&root, "root"));
        assert!(state::user_keys_with_kid(&root, "root").is_empty(), "发卡不落任何公钥");
        // 密码与 reg.cred 一致；htpasswd 有 bcrypt 行
        let cred = fs::read_to_string(state::users_dir(&root, "root").join("reg.cred")).unwrap();
        assert_eq!(cred, format!("root:{}\n", added.password));
        let htpasswd = fs::read_to_string(state::htpasswd_path(&root)).unwrap();
        assert!(htpasswd.starts_with("root:$2"), "bcrypt 行: {htpasswd}");

        // 非 admin：无标记
        user_add(&root, "bob", false, InitPw::Set("pw-654321".into())).unwrap();
        assert!(!state::users_dir(&root, "bob").join("admin.pending").exists());

        // 坏 user 名 → E_NAME
        assert_eq!(user_add(&root, "Bad", false, InitPw::Gen).unwrap_err().code, "E_NAME");
    }

    // plan-16 §6.5：user_add(Gen) 落记录且密码仅返一次（文件只是 OPAQUE 信封，无明文/哈希）
    #[test]
    fn user_add_gen_writes_pass_opaque() {
        let root = troot("user-add-gen");
        let added = user_add(&root, "carol", false, InitPw::Gen).unwrap();
        let login_pw = added.login_password.clone().expect("Gen 返回一次性登录密码");
        assert_eq!(login_pw.len(), 24, "24 hex 字符");
        // 记录落位：b64 一行、0600；内容可被服务端反序列化（可登录）
        let p = state::pass_opaque_path(&root, "carol");
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.trim().len() > 100, "OPAQUE 记录非平凡: {}", text.len());
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(showroom_common::opaque::server_record_from_b64(text.trim()).is_ok());
        // 明文密码不在任何文件里出现
        for f in ["state/users/carol/reg.cred", "state/registry.htpasswd"] {
            let t = fs::read_to_string(root.join(f)).unwrap();
            assert!(!t.contains(&login_pw), "{f} 不含登录密码明文");
        }
        // 进程内注册的记录真的能登录：双端往返
        let setup = opaque_setup(&root).unwrap();
        let mut rng = rand::rngs::OsRng;
        let record = showroom_common::opaque::server_record_from_b64(text.trim()).unwrap();
        let (st, ke1) = showroom_common::opaque::login_start(&mut rng, &login_pw, "carol").unwrap();
        let (ke2, sst) =
            showroom_common::opaque::server_login_start(&mut rng, &setup, Some(&record), &ke1, "carol").unwrap();
        let ke3 = showroom_common::opaque::login_finish(st, &login_pw, "carol", &ke2).unwrap();
        assert!(showroom_common::opaque::server_login_finish(&sst, &ke3).is_ok());
    }

    // plan-16 §6.5：user_passwd_set 替换后旧钥匙仍可用（不动既有钥匙）；
    // 旧密码失效、新密码可用（记录被替换）
    #[test]
    fn passwd_set_replaces_record_keys_untouched() {
        let root = troot("passwd-set");
        let kid = put_key(&root, "alice", true);
        user_add(&root, "alice", false, InitPw::Set("old-pw".into())).unwrap();
        // 旧密码可登录
        let setup = opaque_setup(&root).unwrap();
        let mut rng = rand::rngs::OsRng;
        let rec1 = showroom_common::opaque::server_record_from_b64(
            &fs::read_to_string(state::pass_opaque_path(&root, "alice")).unwrap(),
        )
        .unwrap();
        let (st, ke1) = showroom_common::opaque::login_start(&mut rng, "old-pw", "alice").unwrap();
        let (ke2, _) = showroom_common::opaque::server_login_start(
            &mut rng, &setup, Some(&rec1), &ke1, "alice").unwrap();
        assert!(showroom_common::opaque::login_finish(st, "old-pw", "alice", &ke2).is_ok());

        // 替换
        user_passwd_set(&root, "alice", "new-pw").unwrap();
        // 旧密码失效、新密码可用（走新记录）
        let rec2 = showroom_common::opaque::server_record_from_b64(
            &fs::read_to_string(state::pass_opaque_path(&root, "alice")).unwrap(),
        )
        .unwrap();
        let (st2, ke1b) = showroom_common::opaque::login_start(&mut rng, "old-pw", "alice").unwrap();
        let (ke2b, _) = showroom_common::opaque::server_login_start(
            &mut rng, &setup, Some(&rec2), &ke1b, "alice").unwrap();
        assert!(showroom_common::opaque::login_finish(st2, "old-pw", "alice", &ke2b).is_err(), "旧密码失效");
        let (st3, ke1c) = showroom_common::opaque::login_start(&mut rng, "new-pw", "alice").unwrap();
        let (ke2c, sst) = showroom_common::opaque::server_login_start(
            &mut rng, &setup, Some(&rec2), &ke1c, "alice").unwrap();
        let ke3 = showroom_common::opaque::login_finish(st3, "new-pw", "alice", &ke2c).unwrap();
        assert!(showroom_common::opaque::server_login_finish(&sst, &ke3).is_ok(), "新密码可用");
        // 既有钥匙与账号级 admin 不受影响（R4）
        assert!(state::tokens_dir(&root, "alice").join(format!("{kid}.pub")).exists());
        assert!(state::user_is_admin(&root, "alice"));
        // 空密码/坏名/不存在账号
        assert_eq!(user_passwd_set(&root, "alice", "").unwrap_err().code, "E_NAME");
        assert_eq!(user_passwd_set(&root, "Bad", "x").unwrap_err().code, "E_NAME");
        assert_eq!(user_passwd_set(&root, "ghost", "x").unwrap_err().code, "E_NOTFOUND");
    }

    // plan-16 §6.5：key_bind 幂等 / GC 保 5 弃旧 / 有 admin 钥则继承 / 无 .bound 的 CI 钥不受 GC
    #[test]
    fn key_bind_semantics() {
        let root = troot("key-bind");
        // alice 已有一把 admin 钥（B7 后统一密码绑定路径，put_key 现写 .bound）
        let admin_kid = put_key(&root, "alice", true);
        // 绑 5 把（密码路径）
        let mut kids = vec![];
        for _ in 0..5 {
            let k = showroom_common::UserKey::generate();
            let (kid, admin) = key_bind(&root, "alice", &k.pub_string()).unwrap();
            assert!(admin, "已有 admin 钥 → 新绑定钥匙继承 admin");
            assert!(state::bound_marker_path(&root, "alice", &kid).exists());
            kids.push(kid);
        }
        // 第 7 把：GC 保 5 弃旧——总数守恒在 BOUND_KEYS_MAX=5。先跨秒再绑，保证新钥匙
        // mtime 严格最新（file_mtime 秒级精度；并列无妨，kid 字典序定序确定），
        // 淘汰者必在旧 6 把中，故只断言数量与新钥匙保留、不断言具体淘汰者。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let k6 = showroom_common::UserKey::generate();
        let (kid6, _) = key_bind(&root, "alice", &k6.pub_string()).unwrap();
        let dir = state::tokens_dir(&root, "alice");
        let bound_left = fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().and_then(|o| o.to_str()) == Some("bound"))
            .count();
        assert_eq!(bound_left, 5, "GC 后恰好剩 5 把 .bound");
        assert!(dir.join(format!("{kid6}.pub")).exists(), "新绑钥匙永不淘汰");
        let prev: Vec<&String> = std::iter::once(&admin_kid).chain(kids.iter()).collect();
        let survived = prev.iter().filter(|k| ***k != kid6 && dir.join(format!("{}.pub", k)).exists()).count();
        assert_eq!(survived, 4, "旧 6 把（admin+5 绑定）恰幸存 4 把");

        // 幂等：同钥匙重绑 = ok（重写 .pub + 补标记，不报错不 GC 误删）
        let before = state::user_keys_with_kid(&root, "alice").len();
        let (again, _) = key_bind(&root, "alice", &k6.pub_string()).unwrap();
        assert_eq!(again, kid6);
        assert_eq!(state::user_keys_with_kid(&root, "alice").len(), before);

        // bob 无 admin 钥 → 绑定钥匙不继承
        let kb = showroom_common::UserKey::generate();
        let (_, admin_b) = key_bind(&root, "bob", &kb.pub_string()).unwrap();
        assert!(!admin_b);
        // 坏名 / 坏公钥
        assert_eq!(key_bind(&root, "Bad", &kb.pub_string()).unwrap_err().code, "E_NAME");
        assert_eq!(key_bind(&root, "ok", "junk").unwrap_err().code, "E_NAME");
    }

    // plan-16 §6.5：user_revoke 连记录删（users 目录整删）+ user_list has_password
    #[test]
    fn revoke_deletes_record_and_list_has_password() {
        let root = troot("revoke-list");
        user_add(&root, "alice", false, InitPw::Set("pw".into())).unwrap();
        put_key(&root, "alice", false);
        user_add(&root, "bob", false, InitPw::Gen).unwrap();

        let l = user_list(&root);
        let by = |u: &str| l.iter().find(|i| i.user == u).unwrap();
        assert!(by("alice").has_password, "设了密码");
        assert!(by("bob").has_password, "B7 后发卡恒设密码（无 pubkey-only 账号）");

        user_revoke(&root, "alice").unwrap();
        assert!(!state::users_dir(&root, "alice").exists(), "users 目录整删（记录随之消失）");
        assert!(!state::tokens_dir(&root, "alice").exists());
        assert!(state::users_dir(&root, "bob").exists());
        let l2 = user_list(&root);
        assert!(l2.iter().all(|i| i.user != "alice"));
    }

    // user_set_admin（R4 账号级）：授权/收权幂等；账号不存在 E_NOTFOUND
    #[test]
    fn user_set_admin_grant_revoke() {
        let root = troot("set-admin");
        put_key(&root, "alice", false);

        assert!(!state::user_is_admin(&root, "alice"));
        user_set_admin(&root, "alice", true).unwrap();
        assert!(state::user_is_admin(&root, "alice"));
        user_set_admin(&root, "alice", true).unwrap(); // 重复授权幂等
        assert!(state::user_is_admin(&root, "alice"));

        user_set_admin(&root, "alice", false).unwrap();
        assert!(!state::user_is_admin(&root, "alice"));
        user_set_admin(&root, "alice", false).unwrap(); // 重复收权幂等

        assert_eq!(user_set_admin(&root, "nobody", true).unwrap_err().code, "E_NOTFOUND");
        assert_eq!(user_set_admin(&root, "../evil", true).unwrap_err().code, "E_NAME", "非法账号名防穿越");
    }

    // key_rm：公钥+admin 标记双清；不存在 E_NOTFOUND；kid 非法 E_NAME
    #[test]
    fn key_rm_clears_both_files() {
        let root = troot("key-rm");
        let kid = put_key(&root, "alice", true);
        let plain = put_key(&root, "alice", false);

        user_key_rm(&root, "alice", &kid).unwrap();
        assert!(!state::tokens_dir(&root, "alice").join(format!("{kid}.pub")).exists());
        assert!(!state::admin_marker_path(&root, "alice", &kid).exists(), "标记随钥匙清");

        assert_eq!(user_key_rm(&root, "alice", &kid).unwrap_err().code, "E_NOTFOUND");
        assert_eq!(user_key_rm(&root, "alice", "../x").unwrap_err().code, "E_NAME");
        assert_eq!(user_key_rm(&root, "Bad", &plain).unwrap_err().code, "E_NAME");
    }

    // user_list 聚合：账号（admin 账号级）/钥匙（kid+since）/项目数；无钥匙账号也列出；字典序
    #[test]
    fn user_list_aggregation() {
        let root = troot("list");
        let a_admin = put_key(&root, "alice", true);
        let a_plain = put_key(&root, "alice", false);
        put_key(&root, "bob", false);
        fs::create_dir_all(state::tokens_dir(&root, "empty")).unwrap(); // 钥匙删光的账号
        // 两个项目归 alice、一个归 bob（user_list 只读 conf，不碰 docker）
        for (p, user) in [("demo1", "alice"), ("demo2", "alice"), ("demo3", "bob")] {
            let mut c = crate::state::Conf {
                user: user.into(), tag: "v1".into(), port: 8100, cport: 8080,
                mem: "256m".into(), cpu: 1.0, redisdb: 0, updated: 1,
                stopped: false, legacyslot: None,
                version: String::new(),
                version_seq: 0,
            };
            c.user = user.into();
            c.save(&root, p).unwrap();
        }

        let l = user_list(&root);
        assert_eq!(l.iter().map(|o| o.user.as_str()).collect::<Vec<_>>(), ["alice", "bob", "empty"]);
        assert!(l.iter().all(|o| !o.has_password), "本测试未设密码");
        let alice = &l[0];
        assert_eq!(alice.projects, 2);
        assert_eq!(alice.keys.len(), 2);
        assert!(alice.admin, "put_key(admin=true) → 账号级 admin（R4）");
        let admin_key = alice.keys.iter().find(|k| k.kid == a_admin).unwrap();
        assert!(admin_key.since > 0, "since=登记 mtime");
        assert_eq!(l[1].projects, 1);
        assert!(l[2].keys.is_empty(), "无钥匙账号仍列出（projects=0）");
        assert_eq!(l[2].projects, 0);
    }
}
