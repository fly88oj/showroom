//! publish / import 内核（SPEC §7 全流程）：serve 与 CLI 子命令共用的唯一入口。
//! publish：校验→归属→配额→registry 存在→端口/redisdb→DB 重建→env→容器（-new 滚动）→路由→conf/last2。

use std::fs;
use std::io::Read;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::dockerops;
use crate::state::{self, ApiError, R};

pub struct PublishOpts {
    pub mem: Option<String>,
    pub cpu: Option<f64>,
    pub cport: Option<u16>,
}

pub struct PublishOut {
    pub url: String,
    pub port: u16,
}

pub fn container_name(p: &str, s: &str) -> String {
    format!("sr-{p}-{s}")
}

pub fn image_ref(owner: &str, p: &str, tag: &str) -> String {
    format!("127.0.0.1:5000/{owner}/{p}:{tag}")
}

/// project 归属首个创建者：任一现存 slot.conf 的 owner 必须等于本次发布者。
pub fn check_ownership(root: &Path, project: &str, owner: &str) -> R<()> {
    for (pj, _, c) in state::all_confs(root) {
        if pj == project {
            if c.owner != owner {
                return Err(ApiError::new(
                    "E_AUTH",
                    format!("project '{project}' belongs to {}", c.owner),
                ));
            }
            return Ok(());
        }
    }
    Ok(())
}

/// 健康探测：TCP 127.0.0.1:port 可连或 60s 超时放行（SPEC §7）。
fn health_probe(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..60 {
        if TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false // 超时放行
}

pub fn publish(
    root: &Path,
    owner: &str,
    project: &str,
    slot: &str,
    tag: &str,
    opts: PublishOpts,
) -> R<PublishOut> {
    state::check_name("project", project)?;
    state::check_name("slot", slot)?;
    if tag.is_empty() || tag.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(ApiError::new("E_NAME", format!("bad tag '{tag}'")));
    }
    check_ownership(root, project, owner)?;
    let policy = state::Policy::load(root);
    let old = state::SlotConf::load(root, project, slot).ok();
    // ponytail: MAX_SLOTS 按全平台 slot conf 总数计（100 端口池之上更紧的整机配额）
    if old.is_none() && state::all_confs(root).len() as u32 >= policy.max_slots {
        return Err(ApiError::new("E_QUOTA", format!("MAX_SLOTS={} reached", policy.max_slots)));
    }
    let mem = state::effective_mem(&policy, opts.mem.as_deref().or(old.as_ref().map(|c| c.mem.as_str())))?;
    let cpu = opts.cpu.or(old.as_ref().map(|c| c.cpu)).unwrap_or(1.0);
    let cport = opts.cport.or(old.as_ref().map(|c| c.cport)).unwrap_or(8080);
    let port = match &old {
        Some(c) => c.port,
        None => state::alloc_port(root)?,
    };
    let redisdb = match &old {
        Some(c) => c.redisdb,
        None => state::alloc_redisdb(root)?,
    };

    let repo = format!("{owner}/{project}");
    if !dockerops::image_exists(root, &repo, tag)? {
        return Err(ApiError::new(
            "E_NOTFOUND",
            format!("image {} not in registry", image_ref(owner, project, tag)),
        ));
    }

    // DB 供给：幂等重建（WITH (FORCE) 清掉旧容器残留连接；PostgreSQL >= 13）
    let db = state::db_ident(project, slot);
    let db_pass = state::rand_hex(16);
    let sql = format!(
        "DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\n\
         DROP ROLE IF EXISTS \"{db}\";\n\
         CREATE ROLE \"{db}\" LOGIN PASSWORD '{db_pass}';\n\
         CREATE DATABASE \"{db}\" OWNER \"{db}\";\n"
    );
    dockerops::psql(&sql)?;

    // env 文件（SPEC §6 全字段具体化）
    fs::create_dir_all(state::projects_dir(root).join(project))?;
    fs::write(
        state::env_path(root, project, slot),
        state::env_file_contents(project, slot, &db_pass, redisdb),
    )?;

    // 容器：同槽位唯一 host 端口 → 先停旧、以 -new 起新、健康后删旧改名
    // ponytail: 无双活（两容器抢同一 8100 端口），停旧到起新之间秒级中断，够用
    let name = container_name(project, slot);
    let newname = format!("{name}-new");
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &newname]); // 清理上次失败残留
    let had_old = dockerops::container_status(&name).is_some();
    if had_old {
        dockerops::docker(&["stop", &name])?; // 释放 host 端口
    }
    let bind = if policy.allow_lan_port { "0.0.0.0" } else { "127.0.0.1" };
    let env_file = state::env_path(root, project, slot);
    let env_file = env_file.to_str().ok_or_else(|| ApiError::state("non-utf8 env path"))?;
    let publish_port = format!("{bind}:{port}:{cport}");
    let cpu_s = cpu.to_string();
    let net = dockerops::docker_network();
    let image = image_ref(owner, project, tag);
    let run_res = dockerops::docker(&[
        "run", "-d", "--name", &newname, "--network", &net, "--restart", "unless-stopped",
        "--memory", &mem, "--cpus", &cpu_s, "--env-file", env_file, "-p", &publish_port, &image,
    ]);
    if let Err(e) = run_res {
        if had_old {
            let _ = dockerops::run_fail_ok("docker", &["start", &name]); // 尽力拉回旧容器
        }
        return Err(e);
    }
    health_probe(port);
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &name]); // 旧容器（可能本就不存在）
    dockerops::docker(&["rename", &newname, &name])?;
    state::register_port(root, port, project, slot)?;

    // last2 = 上一 tag（rollback 据此回跳；幂等重发也刷新）
    if let Some(o) = &old {
        let _ = fs::write(state::last2_path(root, project, slot), &o.tag);
    }
    let conf = state::SlotConf {
        owner: owner.to_string(),
        tag: tag.to_string(),
        port,
        cport,
        mem,
        cpu,
        redisdb,
        updated: state::unix_now(),
        stopped: false,
    };
    conf.save(root, project, slot)?;
    apply_route(root, project, slot, port)?;
    Ok(PublishOut { url: state::public_url(project, slot), port })
}

/// 写路由片段并 reload caddy；失败恢复旧片段重载后报 E_STATE（回滚说明）。
pub fn apply_route(root: &Path, project: &str, slot: &str, port: u16) -> R<()> {
    let path = state::route_path(root, project, slot);
    let want = state::caddy_route(project, slot, port);
    let prev = fs::read_to_string(&path).ok();
    fs::create_dir_all(state::routes_dir(root))?;
    fs::write(&path, &want)?;
    if let Err(e) = dockerops::caddy_validate().and_then(|_| dockerops::caddy_reload()) {
        match &prev {
            Some(p) => {
                let _ = fs::write(&path, p);
            }
            None => {
                let _ = fs::remove_file(&path);
            }
        }
        let _ = dockerops::caddy_reload();
        return Err(ApiError::state(format!(
            "caddy reload failed ({e}); route file rolled back, new container left running"
        )));
    }
    Ok(())
}

// —— import（SPEC §7：验签→解密→docker load→tag→push→删临时）——
// 验签由调用方完成（serve 在流式收完 body 后验 sha256；CLI 是本地管理员通道不验）。

/// 信封流式落盘 `_tmp/`，边收边算 sha256（供调用方验签）；超 IMPORT_MAX_GB → 删文件 E_QUOTA。
pub fn receive_envelope(root: &Path, mut r: impl Read) -> R<(PathBuf, String)> {
    let policy = state::Policy::load(root);
    let cap = policy.import_cap_bytes();
    fs::create_dir_all(root.join("_tmp"))?;
    let path = root.join("_tmp").join(format!("{}.bin", state::rand_hex(8)));
    let mut f = fs::File::create(&path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            drop(f);
            let _ = fs::remove_file(&path);
            return Err(ApiError::new(
                "E_QUOTA",
                format!("import body exceeds IMPORT_MAX_GB={}GB", policy.import_max_gb),
            ));
        }
        hasher.update(&buf[..n]);
        f.write_all(&buf[..n])?;
    }
    Ok((path, hex::encode(hasher.finalize())))
}

/// 已落盘信封 → common::open 解密 → zstd 解压流式喂 `docker load` → tag → push → manifest digest。
/// 无论成败删除临时文件。common::PlatformKeys 未暴露私钥，这里经重导出的 rsa crate 直接读 PEM（同版本无类型分裂）。
pub fn import_from_file(
    root: &Path,
    owner: &str,
    project: &str,
    slot: &str,
    tag: &str,
    sealed: &Path,
) -> R<String> {
    state::check_name("project", project)?;
    state::check_name("slot", slot)?;
    if tag.is_empty() || tag.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(ApiError::new("E_NAME", format!("bad tag '{tag}'")));
    }
    let res = import_inner(root, owner, project, slot, tag, sealed);
    let _ = fs::remove_file(sealed);
    res
}

fn import_inner(root: &Path, owner: &str, project: &str, _slot: &str, tag: &str, sealed: &Path) -> R<String> {
    use showroom_common::rsa::pkcs8::DecodePrivateKey;

    let cap = state::Policy::load(root).import_cap_bytes();
    let pem = fs::read_to_string(root.join("state/platform_rsa"))
        .map_err(|e| ApiError::state(format!("platform_rsa missing (run showroomctl init-keys): {e}")))?;
    let priv_key = showroom_common::rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .map_err(|e| ApiError::state(format!("platform_rsa parse: {e}")))?;

    // docker load ← zstd 解压 ← common::open 解密（单进程管道，不落第二个文件）
    let mut child = Command::new("docker")
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn docker load: {e}")))?;
    let stdin = child.stdin.take().unwrap();
    let mut zw = zstd::stream::write::Decoder::new(stdin)
        .map_err(|e| ApiError::state(format!("zstd decoder init: {e}")))?;
    let plain_bytes = showroom_common::open(&priv_key, fs::File::open(sealed)?, &mut zw)
        .map_err(|e| ApiError::state(format!("decrypt envelope: {e}")))?;
    if plain_bytes > cap {
        return Err(ApiError::new(
            "E_QUOTA",
            format!("decrypted payload exceeds IMPORT_MAX_GB={}GB", cap >> 30),
        ));
    }
    zw.flush().map_err(|e| ApiError::state(format!("zstd flush: {e}")))?;
    drop(zw); // write::Decoder 无 finish()：flush 排空缓冲，drop 关闭 docker load 的 stdin（EOF）
    let out = child
        .wait_with_output()
        .map_err(|e| ApiError::state(format!("docker load wait: {e}")))?;
    if !out.status.success() {
        return Err(ApiError::state(format!(
            "docker load failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // "Loaded image ID: sha256:…"（无 tag 时）或 "Loaded image: repo:tag"（有 tag 时）
    let src = stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("Loaded image ID: "))
        .or_else(|| stdout.lines().rev().find_map(|l| l.strip_prefix("Loaded image: ")))
        .map(str::trim)
        .ok_or_else(|| ApiError::state(format!("docker load: no image ref in output: {}", stdout.trim())))?;

    let target = image_ref(owner, project, tag);
    dockerops::docker(&["tag", src, &target])?;
    dockerops::docker(&["push", &target])?; // 主机 root 已 docker login 127.0.0.1:5000
    dockerops::manifest_digest(root, &format!("{owner}/{project}"), tag)
}
