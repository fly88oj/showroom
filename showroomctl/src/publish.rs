//! publish / import 内核（SPEC §7 全流程 + §11 v1.4 无槽位模型）：serve 与 CLI 子命令共用的唯一入口。
//! publish：校验→归属→配额→registry 存在→端口/redisdb→DB 供给→env→容器（-new 滚动）→路由→conf(version/version_seq)/versions.json(last2 老项目兜底)。

use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

use crate::dockerops;
use crate::state::{self, ApiError, R};

pub struct PublishOpts {
    /// 附加服务（plan-19 §B；None=沿用旧 conf，Some=按此对齐）。
    pub services: Option<Vec<state::Svc>>,
    pub mem: Option<String>,
    pub cpu: Option<f64>,
    pub cport: Option<u16>,
}

#[derive(Debug)]
pub struct PublishOut {
    pub url: String,
    pub port: u16,
}

/// 容器名（§11）：`sr-<project>`。
/// 附加服务容器名（plan-19）：sr-<project>-<svc>（仅内网，无 host 端口）。
pub fn svc_container_name(p: &str, svc: &str) -> String {
    format!("sr-{p}-{svc}")
}

/// 镜像引用拆 (repo, tag)（plan-19）：末段 `:tag`（tag 无 `/`）；缺省 tag=latest。
pub fn split_image_ref(image: &str) -> (String, String) {
    match image.rsplit_once(':') {
        Some((repo, t)) if !t.is_empty() && !t.contains('/') => (repo.to_string(), t.to_string()),
        _ => (image.to_string(), "latest".into()),
    }
}

/// 附加服务对齐（plan-19 §B.2）：目标集合内——先 rm 再 run（附加服务无公网端口，
/// 秒级重建可接受，免去配置漂移比对）；不在集合内的旧附加服务删除（由调用方扫描）。
/// 单服务失败只记日志不中止（主服务已健康；下次 publish/reconcile 收敛）。
fn align_services(root: &Path, project: &str, services: &[state::Svc], env_file: &str) {
    let net = dockerops::DOCKER_NET;
    // 删除不再存在的旧附加服务（防孤儿）
    let live: std::collections::BTreeSet<String> =
        services.iter().map(|v| v.name.clone()).collect();
    for n in dockerops::all_container_names() {
        if let Some(svc) = n.strip_prefix(&format!("sr-{project}-")) {
            if !svc.contains('-') && !live.contains(svc) {
                let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &n]);
            }
        }
    }
    for v in services {
        let cname = svc_container_name(project, &v.name);
        let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &cname]);
        let cpu_s = v.cpu.to_string();
        if let Err(e) = dockerops::docker(&[
            "run", "-d", "--name", &cname, "--network", net, "--restart", "unless-stopped",
            "--memory", &v.mem, "--cpus", &cpu_s, "--env-file", env_file, &v.image,
        ]) {
            eprintln!("showroomctl: publish: 附加服务 {} 启动失败: {}", v.name, e.msg);
        }
    }
    let _ = root;
}

#[cfg(test)]
mod svc_tests {
    use super::split_image_ref;

    #[test]
    fn image_ref_split_shapes() {
        assert_eq!(split_image_ref("u/img:v1"), ("u/img".into(), "v1".into()));
        assert_eq!(split_image_ref("u/img"), ("u/img".into(), "latest".into()));
        assert_eq!(split_image_ref("reg:5000/u/img:2"), ("reg:5000/u/img".into(), "2".into()));
        assert_eq!(split_image_ref("a:b:c"), ("a:b".into(), "c".into()));
    }
}

pub fn container_name(p: &str) -> String {
    format!("sr-{p}")
}

/// 镜像引用（§11）：`127.0.0.1:5000/<user>/<project>:<tag>`。
/// project 正则已放宽允许大写（§11），但 docker 仓库名强制小写 → repo 路径转小写
/// （同 user 下 "Demo" 与 "demo" 共享 registry 命名空间，容器/DB/conf 仍各自独立）。
pub fn image_ref(user: &str, p: &str, tag: &str) -> String {
    format!("127.0.0.1:5000/{user}/{}:{tag}", p.to_ascii_lowercase())
}

/// docker tag 规则（转发 common::names：首字符字母/数字/_，余 [A-Za-z0-9_.-]，≤128）。
pub fn check_tag(tag: &str) -> R<()> {
    showroom_common::names::check_tag(tag).map_err(|e| ApiError::new("E_NAME", e))
}

/// project 归属首个创建者：现存项目 conf 的 user 必须等于本次发布者。
pub fn check_ownership(root: &Path, project: &str, user: &str) -> R<()> {
    for (pj, c) in state::all_confs(root) {
        if pj == project {
            if c.user != user {
                return Err(ApiError::new(
                    "E_AUTH",
                    format!("project '{project}' belongs to {}", c.user),
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

/// /proc/meminfo 的 MemAvailable（KB；行解析纯函数在 state，单测覆盖）。读不到 → E_STATE（fail-closed）。
fn mem_available_kb() -> R<u64> {
    let text = fs::read_to_string("/proc/meminfo")
        .map_err(|e| ApiError::state(format!("read /proc/meminfo: {e}")))?;
    state::parse_meminfo_available_kb(&text)
        .ok_or_else(|| ApiError::state("/proc/meminfo missing MemAvailable"))
}

/// 磁盘准入（SPEC §14.2）：根 df avail − DISK_RESERVE_GB 须满足 sealed × UPLOAD_SPACE_FACTOR，
/// 否则 E_QUOTA（msg 由 state::disk_admission 统一产出，import-begin/end 与 publish 复核共用）。
/// sealed 即密文信封尺寸（import 通道）或 registry manifest 层和（快通道复核）。
fn disk_admit(root: &Path, sealed_bytes: u64) -> R<()> {
    let policy = state::Policy::load(root);
    let avail = crate::ops::disk_avail_bytes()?;
    state::disk_admission(sealed_bytes, avail, policy.reserve_bytes(), policy.upload_space_factor)
        .map_err(|m| ApiError::new("E_QUOTA", m))
}

/// 版本记账模式：Bump=用户发起的发布（版本自增 + 落发布历史）；Keep=原地重建/回滚式重部署
/// （不改 version_seq、不落历史；Some(v) 同时把当前版本置为 v——回滚语义，None 保持现状——重建）。
#[derive(Debug, Clone, PartialEq)]
pub enum VerMode {
    Bump,
    Keep(Option<String>),
}

/// 发布（版本自动自增入口）：版本号由服务端按项目单调分配（v{version_seq+1}），
/// `tag` 参数是**部署的镜像 tag**（镜像身份，不再是版本号）。分配结果落 conf.version/
/// version_seq 与 versions.json 发布历史。
pub fn publish(
    root: &Path,
    user: &str,
    project: &str,
    tag: &str,
    opts: PublishOpts,
) -> R<PublishOut> {
    publish_mode(root, user, project, tag, opts, VerMode::Bump)
}

/// 原地重部署（start 容器缺失重建 / reconcile / autoheal / 回滚）：不动版本计数与历史；
/// `version`=Some(v) 用于回滚（当前版本置为 v）。
pub fn redeploy(
    root: &Path,
    user: &str,
    project: &str,
    tag: &str,
    opts: PublishOpts,
    version: Option<String>,
) -> R<PublishOut> {
    publish_mode(root, user, project, tag, opts, VerMode::Keep(version))
}

fn publish_mode(
    root: &Path,
    user: &str,
    project: &str,
    tag: &str,
    opts: PublishOpts,
    mode: VerMode,
) -> R<PublishOut> {
    state::check_name("project", project)?;
    check_tag(tag)?;
    check_ownership(root, project, user)?;
    let policy = state::Policy::load(root);
    let old = state::Conf::load(root, project).ok();
    // ponytail: MAX_SLOTS 按全平台项目 conf 总数计（§11 语义=项目数上限；键名沿用存量 policy.env）
    if old.is_none() && state::all_confs(root).len() as u32 >= policy.max_slots {
        return Err(ApiError::new("E_QUOTA", format!("MAX_SLOTS={} reached", policy.max_slots)));
    }
    // 附加服务归一化（plan-19 §B.2）：None=沿用旧 conf；Some=按此对齐。
    // 名称合法 + 去重 + 总数（含主）≤ SERVICES_MAX + 镜像引用补全（无 / 前缀补 <user>/）
    // + mem/cpu 缺省 + 内存准入按全体和。
    let mut services = opts.services.clone().unwrap_or_else(|| {
        old.as_ref().map(|c| c.services.clone()).unwrap_or_default()
    });
    {
        use std::collections::BTreeSet;
        let mut seen = BTreeSet::new();
        for v in &mut services {
            state::check_name("service", &v.name)?;
            if !seen.insert(v.name.clone()) {
                return Err(ApiError::new("E_NAME", format!("服务名重复: {}", v.name)));
            }
            if v.image.is_empty() {
                return Err(ApiError::new("E_NAME", "服务镜像为空"));
            }
            if !v.image.contains('/') {
                v.image = format!("{user}/{}", v.image);
            }
            if !v.image.rsplit(':').next().is_some_and(|t| !t.contains('/') && !t.is_empty())
                || !v.image.contains(':')
            {
                v.image.push_str(":latest");
            }
            if v.mem.is_empty() {
                v.mem = "128m".into();
            }
            if v.cpu == 0.0 {
                v.cpu = 0.5;
            }
            state::effective_mem(&policy, Some(&v.mem))?;
        }
        if 1 + services.len() as u32 > policy.services_max {
            return Err(ApiError::new(
                "E_QUOTA",
                format!("SERVICES_MAX={} reached（含主服务）", policy.services_max),
            ));
        }
    }
    let mem = state::effective_mem(&policy, opts.mem.as_deref().or(old.as_ref().map(|c| c.mem.as_str())))?;
    // 内存准入（SPEC §10.2）：MemAvailable − RESERVE_MEM_MB ≥ 本次需求才放行，否则 E_QUOTA。
    // 只查**本次请求**的 need（当前可用减预留是否满足本次需求），不累加存量项目——
    // 存量由容器 --memory 硬限自保护。放在创建容器（含 DB 供给/env 写盘）之前，失败零副作用。
    let mut need_mb = state::parse_mem(&mem).unwrap_or(0) >> 20;
    for v in &services {
        need_mb += state::parse_mem(&v.mem).unwrap_or(0) >> 20;
    }
    if let Err(m) = state::mem_admission(need_mb, mem_available_kb()?, policy.reserve_mem_mb) {
        return Err(ApiError::new("E_QUOTA", m));
    }
    let cpu = opts.cpu.or(old.as_ref().map(|c| c.cpu)).unwrap_or(1.0);
    let cport = opts.cport.or(old.as_ref().map(|c| c.cport)).unwrap_or(8080);
    let port = match &old {
        Some(c) => c.port,
        None => state::alloc_port(root)?,
    };
    let redisdb = match &old {
        Some(c) => c.redisdb,
        None => state::alloc_redisdb(root, &policy)?,
    };

    let repo = format!("{user}/{}", project.to_ascii_lowercase());
    // §14 快通道复核：manifest_size 一次 HTTP 兼得存在性与体积——
    // None → E_NOTFOUND（文案不变）；Some(sz) → 磁盘准入（容器创建/DB 供给之前，失败零副作用）
    match dockerops::manifest_size(root, &repo, tag)? {
        None => {
            return Err(ApiError::new(
                "E_NOTFOUND",
                format!("image {} not in registry", image_ref(user, project, tag)),
            ));
        }
        Some(sz) => disk_admit(root, sz)?,
    }
    for v in &services {
        let (repo, stag) = split_image_ref(&v.image);
        match dockerops::manifest_size(root, &repo, &stag)? {
            None => {
                return Err(ApiError::new(
                    "E_NOTFOUND",
                    format!("service image {}:{} not in registry", repo, stag),
                ));
            }
            Some(sz) => disk_admit(root, sz)?,
        }
    }

    // DB 供给（SPEC §9.2 首建不重建，数据保留）：存量库（归一化新名或 §11 迁移沿用旧名）存在则
    // 沿用并仅对齐角色密码（下方 env 全量重写 + 容器滚动重启使新凭据生效）；不存在才 CREATE。
    // 归一化切换（旧名 → 新名）由 db-reset 终结，publish 不做迁移。
    let legacyslot = old.as_ref().and_then(|c| c.legacyslot.as_deref());
    let (db, db_exists) = crate::dbmaint::resolve_db(project, legacyslot)?;
    let db_pass = state::rand_hex(16);
    let sql = if db_exists {
        format!("ALTER ROLE \"{db}\" LOGIN PASSWORD '{db_pass}';\n")
    } else {
        format!(
            "CREATE ROLE \"{db}\" LOGIN PASSWORD '{db_pass}';\n\
             CREATE DATABASE \"{db}\" OWNER \"{db}\";\n"
        )
    };
    dockerops::psql(&sql)?;

    // env 文件（SPEC §6 全字段具体化；§11 无 SHOWROOM_SLOT；§15.2 禁用组不注入）
    fs::create_dir_all(state::projects_dir(root))?;
    let mw = crate::middleware::HostEnv::load(root).middleware;
    let main_container = container_name(project);
    let svc_containers: Vec<String> =
        services.iter().map(|v| svc_container_name(project, &v.name)).collect();
    let mut disc: Vec<(&str, &str, u16)> = vec![("main", main_container.as_str(), cport)];
    for (v, c) in services.iter().zip(svc_containers.iter()) {
        disc.push((v.name.as_str(), c.as_str(), v.cport));
    }
    fs::write(
        state::env_path(root, project),
        state::env_file_contents(project, &db, &db_pass, redisdb, &mw, &disc),
    )?;

    // 容器：同项目唯一 host 端口 → 先停旧、以 -new 起新、健康后删旧改名
    // ponytail: 无双活（两容器抢同一 8100 端口），停旧到起新之间秒级中断，够用
    let name = container_name(project);
    let newname = format!("{name}-new");
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", &newname]); // 清理上次失败残留
    let had_old = dockerops::container_status(&name).is_some();
    if had_old {
        dockerops::docker(&["stop", &name])?; // 释放 host 端口
    }
    let bind = if policy.allow_lan_port { "0.0.0.0" } else { "127.0.0.1" };
    let env_file = state::env_path(root, project);
    let env_file = env_file.to_str().ok_or_else(|| ApiError::state("non-utf8 env path"))?;
    let publish_port = format!("{bind}:{port}:{cport}");
    let cpu_s = cpu.to_string();
    let net = dockerops::DOCKER_NET;
    let image = image_ref(user, project, tag);
    let run_res = dockerops::docker(&[
        "run", "-d", "--name", &newname, "--network", net, "--restart", "unless-stopped",
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
    state::register_port(root, port, project)?;
    align_services(root, project, &services, env_file);

    // last2 = 上一 tag（回滚兜底：无发布历史的老项目据此回跳；幂等重发也刷新）
    if let Some(o) = &old {
        let _ = fs::write(state::last2_path(root, project), &o.tag);
    }
    // 版本记账：Bump=自增分配 v{seq+1} 并落历史（发布事件）；Keep=回滚/重建（计数与历史原样）。
    // 历史落条在容器切换成功之后（失败发布不留幽灵版本）。
    let prev_seq = old.as_ref().map(|c| c.version_seq).unwrap_or(0);
    let (version, version_seq) = match &mode {
        VerMode::Bump => {
            let seq = prev_seq + 1;
            let v = state::version_of(seq);
            state::append_version(
                root,
                project,
                state::VersionRec { version: v.clone(), image: image.clone(), time: state::unix_now() },
            )?;
            (v, seq)
        }
        VerMode::Keep(v) => (
            v.clone().or_else(|| old.as_ref().map(|c| c.version.clone())).unwrap_or_default(),
            prev_seq,
        ),
    };
    let conf = state::Conf {
        services,
        user: user.to_string(),
        tag: tag.to_string(),
        port,
        cport,
        mem,
        cpu,
        redisdb,
        updated: state::unix_now(),
        stopped: false,
        legacyslot: old.and_then(|c| c.legacyslot), // 迁移来源槽位保留（DB 检测沿用）
        version,
        version_seq,
    };
    conf.save(root, project)?;
    apply_route(root, project, port)?;
    Ok(PublishOut { url: state::public_url(project), port })
}

/// 写路由片段并 reload caddy；失败恢复旧片段重载后报 E_STATE（回滚说明）。
pub fn apply_route(root: &Path, project: &str, port: u16) -> R<()> {
    let path = state::route_path(root, project);
    let want = state::caddy_route(project, port);
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

/// 已落盘信封 → common::open 解密 →（魔数自适应：zstd / gzip / 裸 tar 直通，unpack::MagicSink）
/// 流式喂 `docker load` → tag → push → manifest digest。
/// 无论成败删除临时文件。common::PlatformKeys 未暴露私钥，这里经重导出的 rsa crate 直接读 PEM（同版本无类型分裂）。
pub fn import_from_file(
    root: &Path,
    user: &str,
    project: &str,
    tag: &str,
    sealed: &Path,
) -> R<String> {
    state::check_name("project", project)?;
    check_tag(tag)?;
    let res = import_inner(root, user, project, tag, sealed);
    let _ = fs::remove_file(sealed);
    res
}

fn import_inner(root: &Path, user: &str, project: &str, tag: &str, sealed: &Path) -> R<String> {
    use showroom_common::rsa::pkcs8::DecodePrivateKey;

    let cap = state::Policy::load(root).import_cap_bytes();
    let pem = fs::read_to_string(root.join("state/platform_rsa"))
        .map_err(|e| ApiError::state(format!("platform_rsa missing (run showroomctl init-keys): {e}")))?;
    let priv_key = showroom_common::rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .map_err(|e| ApiError::state(format!("platform_rsa parse: {e}")))?;

    // docker load ← MagicSink（解密后前 4 字节魔数自适应：zstd 帧流（浏览器压缩/CLI zstd）
    // / gzip 降级 / 裸 tar 直通）← common::open 解密（单进程管道，不落第二个文件）
    let mut child = Command::new("docker")
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn docker load: {e}")))?;
    let stdin = child.stdin.take().unwrap();
    let mut sink = crate::unpack::MagicSink::new(stdin);
    let plain_bytes = {
        let sealed_file = match fs::File::open(sealed) {
            Ok(f) => f,
            Err(e) => {
                dockerops::kill_wait(&mut child);
                return Err(ApiError::state(format!("open sealed envelope: {e}")));
            }
        };
        match showroom_common::open(&priv_key, sealed_file, &mut sink) {
            Ok(n) => n,
            Err(e) => {
                dockerops::kill_wait(&mut child);
                return Err(ApiError::state(format!("decrypt envelope: {e}")));
            }
        }
    };
    if plain_bytes > cap {
        dockerops::kill_wait(&mut child);
        return Err(ApiError::new(
            "E_QUOTA",
            format!("decrypted payload exceeds IMPORT_MAX_GB={}GB", cap >> 30),
        ));
    }
    if let Err(e) = sink.flush() {
        dockerops::kill_wait(&mut child);
        return Err(ApiError::state(format!("decode sink flush: {e}")));
    }
    drop(sink); // zstd/gzip 解码器在 drop 收尾帧并关闭 docker load 的 stdin（EOF）
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

    let target = image_ref(user, project, tag);
    dockerops::docker(&["tag", src, &target])?;
    dockerops::docker(&["push", &target])?; // 主机 root 已 docker login 127.0.0.1:5000
    let repo = format!("{}/{}", user.to_lowercase(), project.to_lowercase());
    dockerops::manifest_digest(root, &repo, tag)
}

// —— 分片导入会话（SPEC §8.1；_tmp/<id>.bin 追加 + <id>.meta JSON；陈旧 >24h 由 gc-images 清）——

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportMeta {
    pub user: String,
    pub project: String,
    pub tag: String,
    pub image_id: String,
    pub total: u64,
    pub received: Vec<u64>,
    pub created: u64,
}

/// upload_id = rand_hex(16)；合法性收口在 state::valid_session_id（与 dump 会话共用，防路径穿越）。
pub fn upload_bin(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.bin"))
}

pub fn upload_meta(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.meta"))
}

/// 读会话 meta；id 非法或不存在 → E_NOTFOUND（不泄露会话内情）。
pub fn load_meta(root: &Path, id: &str) -> R<ImportMeta> {
    if !state::valid_session_id(id) {
        return Err(ApiError::new("E_NOTFOUND", "no such upload"));
    }
    let text = fs::read_to_string(upload_meta(root, id))
        .map_err(|_| ApiError::new("E_NOTFOUND", "no such upload"))?;
    serde_json::from_str(&text).map_err(|e| ApiError::state(format!("upload meta parse: {e}")))
}

/// 会话归属：签名 user 必须等于 begin 时的 user。
pub fn load_meta_owned(root: &Path, id: &str, user: &str) -> R<ImportMeta> {
    let m = load_meta(root, id)?;
    if m.user != user {
        return Err(ApiError::new("E_AUTH", format!("upload '{id}' belongs to {}", m.user)));
    }
    Ok(m)
}

fn save_meta(root: &Path, id: &str, m: &ImportMeta) -> R<()> {
    let j = serde_json::to_string(m).map_err(|e| ApiError::state(format!("meta serialize: {e}")))?;
    fs::write(upload_meta(root, id), j)?;
    Ok(())
}

/// 控制台上传的镜像 tag（版本字段已从发布表单移除）：服务端生成 `u<8hex>`——镜像库内唯一即可，
/// 版本号另有专账（conf.version_seq）。CLI/单发 import 仍显式传 tag。
pub fn gen_upload_tag() -> String {
    format!("u{}", state::rand_hex(4))
}

/// begin：名字/归属/tag 已由 serve 校验（tag=None → 服务端生成上传 tag）；限 total ≤ IMPORT_MAX_GB
/// → 建 .meta + 空 .bin，返回 (upload_id, tag)。
pub fn import_begin(
    root: &Path,
    user: &str,
    project: &str,
    tag: Option<&str>,
    image_id: &str,
    total_bytes: u64,
) -> R<(String, String)> {
    let cap = state::Policy::load(root).import_cap_bytes();
    if total_bytes > cap {
        return Err(ApiError::new(
            "E_QUOTA",
            format!("total_bytes exceeds IMPORT_MAX_GB={}GB", cap >> 30),
        ));
    }
    if image_id.is_empty() {
        return Err(ApiError::new("E_NAME", "empty image_id"));
    }
    // 磁盘准入（§14.2）：sealed(total) × UPLOAD_SPACE_FACTOR ≤ avail − DISK_RESERVE_GB
    disk_admit(root, total_bytes)?;
    let tag = match tag {
        Some(t) => {
            check_tag(t)?;
            t.to_string()
        }
        None => gen_upload_tag(),
    };
    fs::create_dir_all(root.join("_tmp"))?;
    let id = state::rand_hex(16);
    let m = ImportMeta {
        user: user.to_string(),
        project: project.to_string(),
        tag: tag.clone(),
        image_id: image_id.to_string(),
        total: total_bytes,
        received: vec![],
        created: state::unix_now(),
    };
    save_meta(root, &id, &m)?;
    fs::File::create(upload_bin(root, &id))?;
    Ok((id, tag))
}

/// chunk：seq 必须连续（==received.len()）→ 追加 .bin、push seq、写回 meta；返回已收片数。
/// ponytail: 追加成功与 meta 写回之间崩溃会留多余字节，end 的 total 相等校验兜底（gc 清尸）。
pub fn import_chunk(root: &Path, user: &str, id: &str, seq: u64, chunk: &Path) -> R<usize> {
    let res = import_chunk_inner(root, user, id, seq, chunk);
    let _ = fs::remove_file(chunk); // 落盘切片用完即删，拒绝路径也不留
    res
}

fn import_chunk_inner(root: &Path, user: &str, id: &str, seq: u64, chunk: &Path) -> R<usize> {
    let mut m = load_meta_owned(root, id, user)?;
    if seq != m.received.len() as u64 {
        return Err(ApiError::new(
            "E_NAME",
            format!("expected seq {}, got {}", m.received.len(), seq),
        ));
    }
    let bin = upload_bin(root, id);
    let have = fs::metadata(&bin).map(|s| s.len()).unwrap_or(0);
    let n = fs::metadata(chunk).map(|s| s.len()).unwrap_or(0);
    // begin 已限 total ≤ IMPORT_MAX_GB，故对 total 的累计帽即 IMPORT_MAX_GB 帽
    if have + n > m.total {
        return Err(ApiError::new("E_QUOTA", format!("chunk overflow: {have}+{n} > total {}", m.total)));
    }
    let mut out = fs::OpenOptions::new().append(true).open(&bin)?;
    io::copy(&mut fs::File::open(chunk)?, &mut out)?;
    m.received.push(seq);
    save_meta(root, id, &m)?;
    Ok(m.received.len())
}

/// end：归属复核 + bin 字节数必须等于 total → 复用单发 import 管道 → 删会话。
/// .bin 一旦进入 import_from_file 即被消费（成败都删），.meta 随之无论成败一并清；
/// 字节不足（E_QUOTA）保留会话供续传（SPEC §8.1），归属不符（E_AUTH）不动他人会话。
pub fn import_end(root: &Path, user: &str, id: &str) -> R<String> {
    let m = load_meta_owned(root, id, user)?;
    check_ownership(root, &m.project, user)?;
    let bin = upload_bin(root, id);
    let have = fs::metadata(&bin).map(|s| s.len()).unwrap_or(0);
    if have != m.total {
        return Err(ApiError::new("E_QUOTA", format!("upload incomplete: {have}/{} bytes", m.total)));
    }
    // 磁盘准入二次判定（§14.2）：收 begin→end 期间空间被并发上传挤占的窗口
    disk_admit(root, m.total)?;
    let d = match import_from_file(root, user, &m.project, &m.tag, &bin) {
        Ok(d) => d,
        Err(e) => {
            let _ = fs::remove_file(upload_meta(root, id));
            return Err(e);
        }
    };
    let _ = fs::remove_file(upload_meta(root, id));
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn troot(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录（pid+原子计数），固定名复用会互相 remove_dir_all
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "srctl-pub-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn chunk_file(root: &Path, bytes: &[u8]) -> PathBuf {
        let p = root.join(format!("{}.chunk", state::rand_hex(4)));
        fs::write(&p, bytes).unwrap();
        p
    }

    /// 中性磁盘准入（单测用）：DISK_RESERVE_GB=0 + UPLOAD_SPACE_FACTOR=0 → 需求恒 0，
    /// 免测试机磁盘余量影响既有断言（df 本身仍真实执行；准入分支另见 disk_quota_gate 测试）。
    fn neutral_disk_policy(root: &Path) {
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state/policy.env"), "DISK_RESERVE_GB=0\nUPLOAD_SPACE_FACTOR=0\n").unwrap();
    }

    // 分片会话：meta 往返 / id 防穿越 / seq 顺序 / 超帽 / 归属 / end 字节校验（不碰网络与 docker）
    #[test]
    fn import_session_flow() {
        let root = troot("session");
        neutral_disk_policy(&root);
        let (id, tag) = import_begin(&root, "alice", "demo", Some("v1"), "sha256:abcd", 10).unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(tag, "v1");

        // meta 读写往返
        let m = load_meta(&root, &id).unwrap();
        assert_eq!(
            (m.user.as_str(), m.project.as_str(), m.tag.as_str(), m.image_id.as_str(), m.total),
            ("alice", "demo", "v1", "sha256:abcd", 10)
        );
        assert_eq!(m.received, Vec::<u64>::new());
        assert!(m.created > 0);

        // 归属：他人 user 拒绝
        assert_eq!(load_meta_owned(&root, &id, "bob").unwrap_err().code, "E_AUTH");

        // 非法 id（空 / 路径穿越 / 长度不符 / 非 hex）一律 E_NOTFOUND
        for bad in ["", "..", "../../etc/passwd", &"x".repeat(31), &"z".repeat(32)] {
            assert_eq!(load_meta(&root, bad).unwrap_err().code, "E_NOTFOUND", "{bad:?}");
        }

        // seq 顺序：0 → 1 通过；重复 0 与跳跃 2 拒绝
        let c0 = chunk_file(&root, b"1234");
        assert_eq!(import_chunk(&root, "alice", &id, 0, &c0).unwrap(), 1);
        assert_eq!(load_meta(&root, &id).unwrap().received, vec![0u64]);
        let c0b = chunk_file(&root, b"xxxx");
        assert_eq!(import_chunk(&root, "alice", &id, 0, &c0b).unwrap_err().code, "E_NAME");
        assert_eq!(import_chunk(&root, "alice", &id, 2, &c0b).unwrap_err().code, "E_NAME");
        assert!(!c0b.exists(), "rejected chunk tmp cleaned");
        assert_eq!(import_chunk(&root, "bob", &id, 1, &chunk_file(&root, b"xx")).unwrap_err().code, "E_AUTH");

        // 超帽：累计 6+5 > total 10
        let c1 = chunk_file(&root, b"56");
        assert_eq!(import_chunk(&root, "alice", &id, 1, &c1).unwrap(), 2);
        let c2 = chunk_file(&root, b"78901");
        assert_eq!(import_chunk(&root, "alice", &id, 2, &c2).unwrap_err().code, "E_QUOTA");
        assert_eq!(fs::read(upload_bin(&root, &id)).unwrap(), b"123456");
        assert_eq!(load_meta(&root, &id).unwrap().received, vec![0u64, 1]);

        // end：字节不等 total → E_QUOTA（在触 docker 前返回）；他人 end → E_AUTH
        assert_eq!(import_end(&root, "alice", &id).unwrap_err().code, "E_QUOTA");
        assert_eq!(import_end(&root, "bob", &id).unwrap_err().code, "E_AUTH");
    }

    // begin 上限与字段校验（默认 IMPORT_MAX_GB=8）
    #[test]
    fn import_begin_limits() {
        let root = troot("begin");
        neutral_disk_policy(&root);
        assert_eq!(import_begin(&root, "alice", "demo", Some("v1"), "sha256:x", 9 << 30).unwrap_err().code, "E_QUOTA");
        assert_eq!(import_begin(&root, "alice", "demo", Some("v1"), "", 10).unwrap_err().code, "E_NAME");
        assert_eq!(import_begin(&root, "alice", "demo", Some("a b"), "i", 10).unwrap_err().code, "E_NAME");
        assert_eq!(import_begin(&root, "alice", "demo", Some("a/b"), "i", 10).unwrap_err().code, "E_NAME");
        // tag=None → 服务端生成 u<8hex>（版本字段已从发布表单移除；tag 仅作镜像库内唯一身份）
        let (id2, gtag) = import_begin(&root, "alice", "demo2", None, "i", 10).unwrap();
        assert!(gtag.starts_with('u') && gtag.len() == 9 && check_tag(&gtag).is_ok(), "生成上传 tag 形状 u+8hex（{gtag}）");
        assert_eq!(load_meta(&root, &id2).unwrap().tag, gtag);
        // 8GB 边界通过
        assert!(import_begin(&root, "alice", "demo", Some("v1"), "i", 8 << 30).is_ok());
    }

    // §14.2 准入接线：DISK_RESERVE_GB 调大（远超任何真机 avail）→ total×factor 超限 →
    // E_QUOTA 且 msg 含膨胀因子（df 真实执行；「差 1MB」等纯判定边界在 state::disk_admission 单测）
    #[test]
    fn import_begin_disk_quota_gate() {
        let root = troot("disk-quota");
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state/policy.env"), "DISK_RESERVE_GB=999999\n").unwrap();
        let e = import_begin(&root, "alice", "demo", Some("v1"), "sha256:x", 10 << 20).unwrap_err();
        assert_eq!(e.code, "E_QUOTA", "{}", e.msg);
        assert!(e.msg.contains("膨胀因子"), "msg 含膨胀因子: {}", e.msg);
        assert!(e.msg.contains("磁盘空间不足"), "msg 前缀: {}", e.msg);
        assert!(e.msg.contains("请先删除旧镜像"), "msg 含删除引导: {}", e.msg);
        // 通过侧：factor=0（需求 0）在同 policy 下也过不了（reserve 巨大 → avail−reserve<0），
        // 恢复中性 policy 后同一上传立即可过——闭环「调整 policy → 可传」
        fs::write(root.join("state/policy.env"), "DISK_RESERVE_GB=0\nUPLOAD_SPACE_FACTOR=0\n").unwrap();
        assert!(import_begin(&root, "alice", "demo", Some("v1"), "sha256:x", 10 << 20).is_ok());
    }

    // A6 回归：end 进入 import 管道后失败（root 无 platform_rsa，解密前即败）→ .bin/.meta 双清
    #[test]
    fn import_end_failure_cleans_bin_and_meta() {
        let root = troot("end-clean");
        neutral_disk_policy(&root);
        let (id, _) = import_begin(&root, "alice", "demo", Some("v1"), "sha256:x", 4).unwrap();
        import_chunk(&root, "alice", &id, 0, &chunk_file(&root, b"abcd")).unwrap();
        let (bin, meta) = (upload_bin(&root, &id), upload_meta(&root, &id));
        assert!(bin.exists() && meta.exists());
        assert!(import_end(&root, "alice", &id).is_err());
        assert!(!bin.exists(), ".bin cleaned");
        assert!(!meta.exists(), ".meta cleaned");
        assert_eq!(import_end(&root, "alice", &id).unwrap_err().code, "E_NOTFOUND");
    }

    // §10.2 准入接线：/proc/meminfo 可读且 MemAvailable > 0（Linux 主机恒真；纯解析在 state 单测）
    #[test]
    fn meminfo_read_wiring() {
        assert!(mem_available_kb().unwrap() > 0);
    }
}
