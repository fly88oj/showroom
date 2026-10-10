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
pub fn rm(root: &Path, p: &str, s: &str) -> R<()> {
    conf_of(root, p, s)?; // 确认槽位存在（未登记的槽位不产生副作用）
    let name = publish::container_name(p, s);
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &name]);
    let db = state::db_ident(p, s);
    let _ = dockerops::psql(
        &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\nDROP ROLE IF EXISTS \"{db}\";\n"),
    );
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

/// 本地镜像/构建缓存/悬空卷清理（systemd timer 调用）。
pub fn gc_images() -> R<String> {
    let a = dockerops::docker(&["image", "prune", "-a", "--force"])?;
    let b = dockerops::docker(&["builder", "prune", "-f"])?;
    let c = dockerops::docker(&["volume", "prune", "-f"])?;
    Ok(format!("{a}{b}{c}"))
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
