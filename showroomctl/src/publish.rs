//! publish / import 内核（SPEC §7 全流程）：serve 与 CLI 子命令共用的唯一入口。
//! publish：校验→归属→配额→registry 存在→端口/redisdb→DB 重建→env→容器（-new 滚动）→路由→conf/last2。

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

/// docker tag 规则（转发 common::names：首字符字母/数字/_，余 [A-Za-z0-9_.-]，≤128）。
pub fn check_tag(tag: &str) -> R<()> {
    showroom_common::names::check_tag(tag).map_err(|e| ApiError::new("E_NAME", e))
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
    check_tag(tag)?;
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

    // DB 供给（SPEC §9.2 首建不重建，数据保留）：存量库（归一化新名或 legacy 旧名）存在则
    // 沿用并仅对齐角色密码（下方 env 全量重写 + 容器滚动重启使新凭据生效）；不存在才 CREATE。
    // 归一化切换（旧名 → 新名）由 db-reset 终结，publish 不做迁移。
    let new_db = state::db_ident(project, slot);
    let legacy_db = state::legacy_db_ident(project, slot);
    let new_exists = dockerops::db_exists(&new_db)?;
    let legacy_exists = legacy_db != new_db && dockerops::db_exists(&legacy_db)?;
    let db = state::resolve_db_ident(project, slot, new_exists, legacy_exists);
    let db_pass = state::rand_hex(16);
    let sql = if new_exists || legacy_exists {
        format!("ALTER ROLE \"{db}\" LOGIN PASSWORD '{db_pass}';\n")
    } else {
        format!(
            "CREATE ROLE \"{db}\" LOGIN PASSWORD '{db_pass}';\n\
             CREATE DATABASE \"{db}\" OWNER \"{db}\";\n"
        )
    };
    dockerops::psql(&sql)?;

    // env 文件（SPEC §6 全字段具体化；DB_NAME/DATABASE_URL 指向已解析的库名）
    fs::create_dir_all(state::projects_dir(root).join(project))?;
    fs::write(
        state::env_path(root, project, slot),
        state::env_file_contents(project, slot, &db, &db_pass, redisdb),
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
    let net = dockerops::DOCKER_NET;
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
    check_tag(tag)?;
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
    let mut zw = match zstd::stream::write::Decoder::new(stdin) {
        Ok(z) => z,
        Err(e) => {
            dockerops::kill_wait(&mut child);
            return Err(ApiError::state(format!("zstd decoder init: {e}")));
        }
    };
    let plain_bytes = {
        let sealed_file = match fs::File::open(sealed) {
            Ok(f) => f,
            Err(e) => {
                dockerops::kill_wait(&mut child);
                return Err(ApiError::state(format!("open sealed envelope: {e}")));
            }
        };
        match showroom_common::open(&priv_key, sealed_file, &mut zw) {
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
    if let Err(e) = zw.flush() {
        dockerops::kill_wait(&mut child);
        return Err(ApiError::state(format!("zstd flush: {e}")));
    }
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

// —— 分片导入会话（SPEC §8.1；_tmp/<id>.bin 追加 + <id>.meta JSON；陈旧 >24h 由 gc-images 清）——

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportMeta {
    pub owner: String,
    pub project: String,
    pub slot: String,
    pub tag: String,
    pub image_id: String,
    pub total: u64,
    pub received: Vec<u64>,
    pub created: u64,
}

/// upload_id = rand_hex(16)；只认 32 位 hex，防路径穿越。
fn valid_upload_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn upload_bin(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.bin"))
}

pub fn upload_meta(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.meta"))
}

/// 读会话 meta；id 非法或不存在 → E_NOTFOUND（不泄露会话内情）。
pub fn load_meta(root: &Path, id: &str) -> R<ImportMeta> {
    if !valid_upload_id(id) {
        return Err(ApiError::new("E_NOTFOUND", "no such upload"));
    }
    let text = fs::read_to_string(upload_meta(root, id))
        .map_err(|_| ApiError::new("E_NOTFOUND", "no such upload"))?;
    serde_json::from_str(&text).map_err(|e| ApiError::state(format!("upload meta parse: {e}")))
}

/// 会话归属：签名 owner 必须等于 begin 时的 owner。
pub fn load_meta_owned(root: &Path, id: &str, owner: &str) -> R<ImportMeta> {
    let m = load_meta(root, id)?;
    if m.owner != owner {
        return Err(ApiError::new("E_AUTH", format!("upload '{id}' belongs to {}", m.owner)));
    }
    Ok(m)
}

fn save_meta(root: &Path, id: &str, m: &ImportMeta) -> R<()> {
    let j = serde_json::to_string(m).map_err(|e| ApiError::state(format!("meta serialize: {e}")))?;
    fs::write(upload_meta(root, id), j)?;
    Ok(())
}

/// begin：名字/归属/tag 已由 serve 校验；限 total ≤ IMPORT_MAX_GB → 建 .meta + 空 .bin，返回 upload_id。
pub fn import_begin(
    root: &Path,
    owner: &str,
    project: &str,
    slot: &str,
    tag: &str,
    image_id: &str,
    total_bytes: u64,
) -> R<String> {
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
    check_tag(tag)?;
    fs::create_dir_all(root.join("_tmp"))?;
    let id = state::rand_hex(16);
    let m = ImportMeta {
        owner: owner.to_string(),
        project: project.to_string(),
        slot: slot.to_string(),
        tag: tag.to_string(),
        image_id: image_id.to_string(),
        total: total_bytes,
        received: vec![],
        created: state::unix_now(),
    };
    save_meta(root, &id, &m)?;
    fs::File::create(upload_bin(root, &id))?;
    Ok(id)
}

/// chunk：seq 必须连续（==received.len()）→ 追加 .bin、push seq、写回 meta；返回已收片数。
/// ponytail: 追加成功与 meta 写回之间崩溃会留多余字节，end 的 total 相等校验兜底（gc 清尸）。
pub fn import_chunk(root: &Path, owner: &str, id: &str, seq: u64, chunk: &Path) -> R<usize> {
    let res = import_chunk_inner(root, owner, id, seq, chunk);
    let _ = fs::remove_file(chunk); // 落盘切片用完即删，拒绝路径也不留
    res
}

fn import_chunk_inner(root: &Path, owner: &str, id: &str, seq: u64, chunk: &Path) -> R<usize> {
    let mut m = load_meta_owned(root, id, owner)?;
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
pub fn import_end(root: &Path, owner: &str, id: &str) -> R<String> {
    let m = load_meta_owned(root, id, owner)?;
    check_ownership(root, &m.project, owner)?;
    let bin = upload_bin(root, id);
    let have = fs::metadata(&bin).map(|s| s.len()).unwrap_or(0);
    if have != m.total {
        return Err(ApiError::new("E_QUOTA", format!("upload incomplete: {have}/{} bytes", m.total)));
    }
    let d = match import_from_file(root, owner, &m.project, &m.slot, &m.tag, &bin) {
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
        let d = std::env::temp_dir().join(format!("srctl-pub-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn chunk_file(root: &Path, bytes: &[u8]) -> PathBuf {
        let p = root.join(format!("{}.chunk", state::rand_hex(4)));
        fs::write(&p, bytes).unwrap();
        p
    }

    // 分片会话：meta 往返 / id 防穿越 / seq 顺序 / 超帽 / 归属 / end 字节校验（不碰网络与 docker）
    #[test]
    fn import_session_flow() {
        let root = troot("session");
        let id = import_begin(&root, "alice", "demo", "stable", "v1", "sha256:abcd", 10).unwrap();
        assert_eq!(id.len(), 32);

        // meta 读写往返
        let m = load_meta(&root, &id).unwrap();
        assert_eq!(
            (m.owner.as_str(), m.project.as_str(), m.slot.as_str(), m.tag.as_str(), m.image_id.as_str(), m.total),
            ("alice", "demo", "stable", "v1", "sha256:abcd", 10)
        );
        assert_eq!(m.received, Vec::<u64>::new());
        assert!(m.created > 0);

        // 归属：他人 owner 拒绝
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
        assert_eq!(import_begin(&root, "alice", "demo", "stable", "v1", "sha256:x", 9 << 30).unwrap_err().code, "E_QUOTA");
        assert_eq!(import_begin(&root, "alice", "demo", "stable", "v1", "", 10).unwrap_err().code, "E_NAME");
        assert_eq!(import_begin(&root, "alice", "demo", "stable", "a b", "i", 10).unwrap_err().code, "E_NAME");
        assert_eq!(import_begin(&root, "alice", "demo", "stable", "a/b", "i", 10).unwrap_err().code, "E_NAME");
        // 8GB 边界通过
        assert!(import_begin(&root, "alice", "demo", "stable", "v1", "i", 8 << 30).is_ok());
    }

    // A6 回归：end 进入 import 管道后失败（root 无 platform_rsa，解密前即败）→ .bin/.meta 双清
    #[test]
    fn import_end_failure_cleans_bin_and_meta() {
        let root = troot("end-clean");
        let id = import_begin(&root, "alice", "demo", "stable", "v1", "sha256:x", 4).unwrap();
        import_chunk(&root, "alice", &id, 0, &chunk_file(&root, b"abcd")).unwrap();
        let (bin, meta) = (upload_bin(&root, &id), upload_meta(&root, &id));
        assert!(bin.exists() && meta.exists());
        assert!(import_end(&root, "alice", &id).is_err());
        assert!(!bin.exists(), ".bin cleaned");
        assert!(!meta.exists(), ".meta cleaned");
        assert_eq!(import_end(&root, "alice", &id).unwrap_err().code, "E_NOTFOUND");
    }
}
