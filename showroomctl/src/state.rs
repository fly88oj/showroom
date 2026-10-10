//! state 层：/srv/showroom/state 的全部读写（SPEC §5）+ 全部纯函数
//! （名字校验 §4 / policy 解析 / conf·env 序列化 / 端口与 redisdb 分配 / caddy 片段生成）。
//! 不做任何 subprocess——外部世界一律走 dockerops。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use showroom_common::{ed25519_dalek::VerifyingKey, sha256_hex, UserKey};

pub const PORT_MIN: u16 = 8100;
pub const PORT_MAX: u16 = 8199;

// —— 错误：code 对齐 SPEC §3 ——
#[derive(Debug)]
pub struct ApiError {
    pub code: &'static str,
    pub msg: String,
}

impl ApiError {
    pub fn new(code: &'static str, msg: impl Into<String>) -> Self {
        Self { code, msg: msg.into() }
    }
    pub fn state(msg: impl Into<String>) -> Self {
        Self::new("E_STATE", msg)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.msg)
    }
}
impl std::error::Error for ApiError {}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::state(format!("io: {e}"))
    }
}

pub type R<T> = Result<T, ApiError>;

pub fn root() -> PathBuf {
    std::env::var_os("SHOWROOM_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/srv/showroom"))
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn rand_hex(n: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

// —— 名字（SPEC §4 + §11；实现收口在 common::names）——

/// project：§11 放宽正则 + 保留名；user 等其余名字沿用旧小写正则（registry 路径需小写）。
pub fn check_name(kind: &str, s: &str) -> R<()> {
    let r = if kind == "project" {
        showroom_common::names::check_project(s)
    } else {
        showroom_common::names::check_aux(s)
    };
    r.map_err(|e| ApiError::new("E_NAME", format!("{kind}: {e}")))
}

// —— 路径（SPEC §5 + §11 v1.4：项目级三件套 <project>.conf|.env|.last2，去掉槽位目录层）——

pub fn projects_dir(root: &Path) -> PathBuf {
    root.join("state/projects")
}
pub fn conf_path(root: &Path, p: &str) -> PathBuf {
    projects_dir(root).join(format!("{p}.conf"))
}
pub fn env_path(root: &Path, p: &str) -> PathBuf {
    projects_dir(root).join(format!("{p}.env"))
}
pub fn last2_path(root: &Path, p: &str) -> PathBuf {
    projects_dir(root).join(format!("{p}.last2"))
}
pub fn routes_dir(root: &Path) -> PathBuf {
    root.join("state/routes")
}
pub fn route_path(root: &Path, p: &str) -> PathBuf {
    routes_dir(root).join(format!("{p}.caddy"))
}
/// v1.4 前旧路由文件 `<project>-<slot>.caddy`（仅迁移期删除用）。
fn legacy_route_path(root: &Path, p: &str, s: &str) -> PathBuf {
    routes_dir(root).join(format!("{p}-{s}.caddy"))
}
pub fn tokens_dir(root: &Path, user: &str) -> PathBuf {
    root.join("state/tokens").join(user)
}
/// registry 凭据目录：`state/users/<user>/reg.cred`（更名前为 `state/owners/<owner>/`；
/// 旧目录仅回退读取/清理用，写入一律 users；整体迁移见 migrate_state_dirs）。
pub fn users_dir(root: &Path, user: &str) -> PathBuf {
    root.join("state/users").join(user)
}
/// 更名前旧目录 `state/owners/<owner>/`（迁移前的回退读取与吊销清理用）。
pub fn legacy_owners_dir(root: &Path, user: &str) -> PathBuf {
    root.join("state/owners").join(user)
}
pub fn ports_reg(root: &Path) -> PathBuf {
    root.join("state/ports.reg")
}
pub fn htpasswd_path(root: &Path) -> PathBuf {
    root.join("state/registry.htpasswd")
}

// —— project.conf：key=value（owner tag port cport mem cpu redisdb updated [stopped] [legacyslot]）——
// 注：磁盘键名沿用存量 `owner=`（不 break 主机存量 conf，语义即下方 user 字段）。

#[derive(Debug, Clone, PartialEq)]
pub struct Conf {
    pub user: String,
    pub tag: String,
    pub port: u16,
    pub cport: u16,
    pub mem: String,
    pub cpu: f64,
    pub redisdb: u8,
    pub updated: u64,
    pub stopped: bool,
    /// v1.4 迁移来源槽位名（§11）：据此检测沿用老库 `sr_<p>_<slot>`（§9 双名思路），
    /// db-reset 重建为新名后清空。全新项目恒 None。
    pub legacyslot: Option<String>,
}

impl Conf {
    pub fn to_conf(&self) -> String {
        let mut s = format!(
            "owner={}\ntag={}\nport={}\ncport={}\nmem={}\ncpu={}\nredisdb={}\nupdated={}\n",
            self.user, self.tag, self.port, self.cport, self.mem, self.cpu, self.redisdb, self.updated
        );
        if self.stopped {
            s.push_str("stopped=1\n");
        }
        if let Some(ls) = &self.legacyslot {
            s.push_str(&format!("legacyslot={ls}\n"));
        }
        s
    }

    pub fn parse(text: &str) -> R<Self> {
        let map = parse_kv(text);
        let need = |k: &str| {
            map.get(k)
                .cloned()
                .ok_or_else(|| ApiError::state(format!("conf missing field '{k}'")))
        };
        Ok(Conf {
            user: need("owner")?,
            tag: need("tag")?,
            port: need("port")?.parse().map_err(|e| ApiError::state(format!("conf port: {e}")))?,
            cport: need("cport")?.parse().map_err(|e| ApiError::state(format!("conf cport: {e}")))?,
            mem: need("mem")?,
            cpu: need("cpu")?.parse().map_err(|e| ApiError::state(format!("conf cpu: {e}")))?,
            redisdb: need("redisdb")?.parse().map_err(|e| ApiError::state(format!("conf redisdb: {e}")))?,
            updated: need("updated")?.parse().map_err(|e| ApiError::state(format!("conf updated: {e}")))?,
            stopped: map.get("stopped").map(|v| v == "1").unwrap_or(false),
            legacyslot: map.get("legacyslot").cloned().filter(|v| !v.is_empty()),
        })
    }

    pub fn load(root: &Path, p: &str) -> R<Self> {
        Self::parse(&fs::read_to_string(conf_path(root, p))?)
    }

    pub fn save(&self, root: &Path, p: &str) -> R<()> {
        let path = conf_path(root, p);
        fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
        fs::write(&path, self.to_conf())?;
        Ok(())
    }

    /// ps / GET /_api/projects 共用的列表行（SPEC §3 projects 响应字段，§11 无 slot）。
    pub fn to_json(&self, p: &str, status: &str) -> Value {
        json!({
            "project": p, "user": self.user, "tag": self.tag,
            "url": public_url(p), "status": status,
            "port": self.port, "mem": self.mem, "cpu": self.cpu, "updated": self.updated,
        })
    }
}

pub fn read_last2(root: &Path, p: &str) -> Option<String> {
    fs::read_to_string(last2_path(root, p))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 全部项目 conf（项目名字典序）。v1.4 起为扁平布局 state/projects/<p>.conf；
/// 老布局 <p>/<s>.conf 由 migrate_legacy_layout 在 serve 启动/reconcile 前上提。
pub fn all_confs(root: &Path) -> Vec<(String, Conf)> {
    let Ok(rd) = fs::read_dir(projects_dir(root)) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().and_then(|o| o.to_str()) != Some("conf") || path.is_dir() {
            continue;
        }
        let Some(pname) = path.file_stem().and_then(|o| o.to_str()).map(str::to_string) else {
            continue;
        };
        if let Ok(c) = Conf::parse(&fs::read_to_string(&path).unwrap_or_default()) {
            out.push((pname, c));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// —— KEY=VALUE 解析（policy.env / project.env 通用，几行）——

pub fn parse_kv(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

// —— 环境变量契约（SPEC §6 + §11，值全部具体化；库名归一化）——

/// 归一化库名（§11）：`sr_` + project 的 `-`→`_`
/// （psql -d、CREATE、env 的 DB_NAME/DB_USER/DATABASE_URL 一律用它）。
pub fn db_ident(p: &str) -> String {
    format!("sr_{}", p.replace('-', "_"))
}

/// v1.4 前老库候选（§9 双名思路）：verbatim `sr_{p}_{s}` 与归一化 `sr_{p_n}_{s_n}`（相同去重）。
/// 仅用于 legacy 存量库的存在性查找（conf.legacyslot 指路）。
pub fn legacy_db_idents(p: &str, s: &str) -> Vec<String> {
    let mut v = vec![format!("sr_{p}_{s}")];
    let n = format!("sr_{}_{}", p.replace('-', "_"), s.replace('-', "_"));
    if n != v[0] {
        v.push(n);
    }
    v
}

/// 库名裁决（纯函数，存在性由调用方喂 dockerops::db_exists 的结果）：
/// names 按优先级排列（新名在前）；首个存在者 → (该名, true)；皆无 → (首名, false)（首建路径）。
pub fn resolve_db_ident(names: &[String], exists: &[bool]) -> (String, bool) {
    for (n, e) in names.iter().zip(exists) {
        if *e {
            return (n.clone(), true);
        }
    }
    (names[0].clone(), false)
}

/// URL 路径恒 /<project>/（§11：无槽位）。
pub fn base_path(p: &str) -> String {
    format!("/{p}/")
}

pub fn public_url(p: &str) -> String {
    let base = std::env::var("SHOWROOM_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".into());
    let bp = base_path(p);
    format!("{}/{}", base.trim_end_matches('/'), bp.trim_start_matches('/'))
}

/// env 文件内容（db 传**已解析**的库名：归一化新名，或沿用的 legacy 旧名）。
/// §11：无 SHOWROOM_SLOT，SHOWROOM_BASE_PATH 恒 /<project>/。
pub fn env_file_contents(p: &str, db: &str, db_pass: &str, redis_db: u8) -> String {
    format!(
        "DB_HOST=sr-postgres\n\
         DB_PORT=5432\n\
         DB_NAME={db}\n\
         DB_USER={db}\n\
         DB_PASSWORD={db_pass}\n\
         DATABASE_URL=postgresql://{db}:{db_pass}@sr-postgres:5432/{db}\n\
         REDIS_HOST=sr-redis\n\
         REDIS_PORT=6379\n\
         REDIS_DB={redis_db}\n\
         REDIS_URL=redis://sr-redis:6379/{redis_db}\n\
         SHOWROOM_PROJECT={p}\n\
         SHOWROOM_BASE_PATH={bp}\n",
        bp = base_path(p),
    )
}

// —— policy.env（SPEC §5 默认值）——

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// 项目数上限（§11 起=项目数；键名沿用存量 policy.env 的 MAX_SLOTS，语义=MAX_PROJECTS）。
    pub max_slots: u32,
    pub default_mem: String,
    pub max_mem: String,
    pub import_max_gb: u64,
    pub allow_lan_port: bool,
    /// 内存准入预留（§10.2，MB）：MemAvailable 减去它之后须仍满足本次发布的需求。
    pub reserve_mem_mb: u64,
    /// Redis 逻辑库池大小（§13.3；policy.env `REDIS_DBS=` 可调，默认 256）。
    /// 分配单位维持每项目一库（§13.3）；与 MAX_SLOTS 的一致性由文档说明（默认 30≤256）。
    pub redis_dbs: u16,
    /// 磁盘准入预留 GB（§14.4；policy.env `DISK_RESERVE_GB=` 可调，默认 20）：
    /// 根分区 df avail 减去它之后须仍满足本次上传的估算需求。
    pub disk_reserve_gb: u64,
    /// 上传空间估算膨胀因子（§14.4；policy.env `UPLOAD_SPACE_FACTOR=` 可调，默认 4）：
    /// sealed 尺寸 × 因子 ≈ docker load 未压缩层 + registry blob 的落盘需求。
    pub upload_space_factor: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_slots: 30,
            default_mem: "256m".into(),
            max_mem: "1g".into(),
            import_max_gb: 8,
            allow_lan_port: false,
            reserve_mem_mb: 1536,
            redis_dbs: 256,
            disk_reserve_gb: 20,
            upload_space_factor: 4,
        }
    }
}

impl Policy {
    pub fn load(root: &Path) -> Self {
        let text = fs::read_to_string(root.join("state/policy.env")).unwrap_or_default();
        let m = parse_kv(&text);
        Policy {
            max_slots: m.get("MAX_SLOTS").and_then(|v| v.parse().ok()).unwrap_or(30),
            default_mem: m.get("DEFAULT_MEM").cloned().unwrap_or_else(|| "256m".into()),
            max_mem: m.get("MAX_MEM").cloned().unwrap_or_else(|| "1g".into()),
            import_max_gb: m.get("IMPORT_MAX_GB").and_then(|v| v.parse().ok()).unwrap_or(8),
            allow_lan_port: m.get("ALLOW_LAN_PORT").map(|v| v != "0").unwrap_or(false),
            reserve_mem_mb: m.get("RESERVE_MEM_MB").and_then(|v| v.parse().ok()).unwrap_or(1536),
            redis_dbs: m.get("REDIS_DBS").and_then(|v| v.parse().ok()).unwrap_or(256),
            disk_reserve_gb: m.get("DISK_RESERVE_GB").and_then(|v| v.parse().ok()).unwrap_or(20),
            upload_space_factor: m.get("UPLOAD_SPACE_FACTOR").and_then(|v| v.parse().ok()).unwrap_or(4),
        }
    }

    pub fn import_cap_bytes(&self) -> u64 {
        self.import_max_gb << 30
    }

    /// 磁盘准入预留字节数（§14.4）：`DISK_RESERVE_GB << 30`。
    pub fn reserve_bytes(&self) -> u64 {
        self.disk_reserve_gb << 30
    }

    /// redisdb 分配池有效上界（§13.3）：Conf.redisdb 为 u8，库号至多 0-255（256 个）。
    pub fn redis_pool_cap(&self) -> u16 {
        self.redis_dbs.min(256)
    }
}

/// "256m"/"1g"/"1024k"/"4096"(裸字节数) → 字节数；非法返回 None。
pub fn parse_mem(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.as_bytes()[s.len() - 1] {
        b'k' | b'K' => (&s[..s.len() - 1], 1024u64),
        b'm' | b'M' => (&s[..s.len() - 1], 1024u64.pow(2)),
        b'g' | b'G' => (&s[..s.len() - 1], 1024u64.pow(3)),
        _ => (s, 1),
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    num.parse::<u64>().ok().and_then(|v| v.checked_mul(mult))
}

/// 生效内存：未指定用 DEFAULT_MEM；格式非法 → E_NAME；超 MAX_MEM → E_QUOTA。
pub fn effective_mem(policy: &Policy, want: Option<&str>) -> R<String> {
    let m = want.unwrap_or(&policy.default_mem);
    let bytes = parse_mem(m).ok_or_else(|| ApiError::new("E_NAME", format!("bad mem '{m}'")))?;
    let cap = parse_mem(&policy.max_mem).unwrap_or(u64::MAX);
    if bytes > cap {
        return Err(ApiError::new("E_QUOTA", format!("mem {m} exceeds MAX_MEM={}", policy.max_mem)));
    }
    Ok(m.to_string())
}

// —— 内存准入（SPEC §10.2）——

/// 准入判断（纯函数）：`MemAvailable(kB)/1024 − reserve < need` → Err（中文 msg 含 可用/预留/需求 三个数）。
/// 差额用有符号算（avail < reserve 且 need=0 时按公式也应拒，且免无符号下溢）；kB→MB 向下取整（保守侧）。
pub fn mem_admission(need_mb: u64, mem_available_kb: u64, reserve_mb: u64) -> Result<(), String> {
    let avail_mb = mem_available_kb / 1024;
    if (avail_mb as i64).saturating_sub(reserve_mb as i64) < need_mb as i64 {
        return Err(format!(
            "内存不足：可用 {avail_mb} MB（已预留 {reserve_mb} MB）＜ 需求 {need_mb} MB"
        ));
    }
    Ok(())
}

/// /proc/meminfo 的 MemAvailable 行（`MemAvailable:  12345678 kB`）→ KB 数；缺失/畸形 → None。
pub fn parse_meminfo_available_kb(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// /proc/meminfo 的 MemTotal 行 → KB 数（plan-15 hw 推导输入；缺失/畸形 → None）。
pub fn parse_meminfo_total_kb(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

// —— 磁盘准入（SPEC §14.2，上传导入/快通道复核共用）——

/// 磁盘准入判断（纯函数）：`sealed × UPLOAD_SPACE_FACTOR ≤ avail − DISK_RESERVE_GB` 不满足 → Err
///（§14.4 msg 模板：可用/预留/估算需求/膨胀因子四数 + 删除引导；import-begin/end 与 publish 复核共用）。
/// 差额有符号算（avail < reserve 免无符号下溢，仿 mem_admission）；字节→MB 一律向下取整（保守侧），
/// `need_mb` 向上取整（判定侧保守）。avail 为**未扣预留**的根分区 df avail。
pub fn disk_admission(
    sealed_bytes: u64,
    avail_bytes: u64,
    reserve_bytes: u64,
    factor: u64,
) -> Result<(), String> {
    let avail_mb = avail_bytes >> 20;
    let reserve_mb = reserve_bytes >> 20;
    let sealed_mb = sealed_bytes >> 20;
    let need_mb = sealed_bytes.saturating_mul(factor).saturating_add((1 << 20) - 1) >> 20;
    if (avail_mb as i64).saturating_sub(reserve_mb as i64) < need_mb as i64 {
        return Err(format!(
            "磁盘空间不足：可用 {avail_mb} MB（已预留 {reserve_mb} MB）＜ 估算需求 {need_mb} MB（{sealed_mb} MB × 膨胀因子 {factor}）；请先删除旧镜像后重试"
        ));
    }
    Ok(())
}

// —— ports.reg：每行 <port> <project>（§11 去 slot 列），端口 8100–8199 ——

pub fn read_ports(root: &Path) -> Vec<(u16, String)> {
    fs::read_to_string(ports_reg(root))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next()?.parse().ok(), it.next()) {
                (Some(p), Some(pj)) => Some((p, pj.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// 行的第二列（project）；按 token 精确比对（免 "demo" 误吞 "mydemo" 的后缀误伤）。
fn port_line_project(l: &str) -> Option<String> {
    l.split_whitespace().nth(1).map(str::to_string)
}

fn ports_lines(root: &Path) -> Vec<String> {
    fs::read_to_string(ports_reg(root))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// 找空闲端口（不落账；publish 在 docker run 成功后 register）。
/// ponytail: 无并发锁——本机唯一管理通道（systemd serve + 罕见管理员 CLI），不会并发分配。
pub fn alloc_port(root: &Path) -> R<u16> {
    let used: BTreeSet<u16> = read_ports(root).into_iter().map(|(p, _)| p).collect();
    (PORT_MIN..=PORT_MAX)
        .find(|p| !used.contains(p))
        .ok_or_else(|| ApiError::new("E_QUOTA", "no free port in 8100-8199"))
}

pub fn register_port(root: &Path, port: u16, project: &str) -> R<()> {
    let mut lines: Vec<String> = ports_lines(root)
        .into_iter()
        .filter(|l| port_line_project(l).as_deref() != Some(project))
        .collect();
    lines.push(format!("{port} {project}"));
    fs::create_dir_all(root.join("state"))?;
    fs::write(ports_reg(root), lines.join("\n") + "\n")?;
    Ok(())
}

pub fn release_port(root: &Path, project: &str) -> R<()> {
    let lines: Vec<String> = ports_lines(root)
        .into_iter()
        .filter(|l| port_line_project(l).as_deref() != Some(project))
        .collect();
    let out = if lines.is_empty() { String::new() } else { lines.join("\n") + "\n" };
    fs::write(ports_reg(root), out)?;
    Ok(())
}

/// redis 逻辑 DB 分配（§11 按项目一库；§13.3 池大小=policy.redis_dbs，默认 256）：
/// 扫现有项目 conf 找空闲；占满 → E_QUOTA（消息含 已用/总量）。存量兼容：已分配库号不变。
pub fn alloc_redisdb(root: &Path, policy: &Policy) -> R<u8> {
    let used: BTreeSet<u8> = all_confs(root).into_iter().map(|(_, c)| c.redisdb).collect();
    let cap = policy.redis_pool_cap();
    (0..cap)
        .map(|d| d as u8)
        .find(|d| !used.contains(d))
        .ok_or_else(|| ApiError::new("E_QUOTA", format!("redis db pool exhausted: {}/{} in use", used.len(), cap)))
}

/// 用户名下项目的 redisdb 集合（§13.2 浏览隔离）：扫全部项目 conf 按 conf.user 聚合，升序去重。
pub fn user_redis_dbs(root: &Path, user: &str) -> Vec<u16> {
    let mut v: Vec<u16> = all_confs(root)
        .into_iter()
        .filter(|(_, c)| c.user == user)
        .map(|(_, c)| c.redisdb as u16)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

// —— Redis 浏览纯函数（SPEC §10.3）——

/// db 参数：数字 0-(cap-1)（cap=policy.redis_pool_cap，§13.3 可调），否则 E_NAME。
pub fn parse_redis_db(s: &str, cap: u16) -> R<u8> {
    let cap = cap.min(256);
    s.parse::<u16>()
        .ok()
        .filter(|d| *d < cap)
        .map(|d| d as u8)
        .ok_or_else(|| ApiError::new("E_NAME", format!("db 须为 0-{} 的数字", cap.saturating_sub(1))))
}

/// q（前缀过滤）→ --scan pattern：空 → "*"（全部）；非空须过白名单 [a-zA-Z0-9:_*.-]，尾部补 *。
/// 双保险：redis-cli 经 docker exec argv 传参本无 shell 元字符注入面，白名单仍拦一道；
/// q 自带 * 时（如 "demo:*"）补成 "**"，glob 语义等同 "*"。
pub fn redis_scan_pattern(q: &str) -> Option<String> {
    let ok = q
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'*' | b'.' | b'-'));
    if !ok {
        return None;
    }
    Some(if q.is_empty() { "*".to_string() } else { format!("{q}*") })
}

/// key 白名单：与 q 同字符集且非空。取舍（SPEC §10.3）：redis key 理论可含任意字节，
/// 这里保守白名单——白名单外的 key（空格/引号/二进制等）请用 CLI 直查，端点如实报 E_NAME 提示。
pub fn valid_redis_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'*' | b'.' | b'-'))
}

/// redis 值统一转字符串后的截断帽（§10.3：超 4KB 截断加标记）。
pub const REDIS_VALUE_MAX: usize = 4096;

/// 超 4KB 按 UTF-8 边界截断并加 "...(截断)"（多字节字符不被切半）；不超则原样。
pub fn clip_value(s: &str) -> String {
    if s.len() <= REDIS_VALUE_MAX {
        return s.to_string();
    }
    let mut end = REDIS_VALUE_MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...(截断)", &s[..end])
}

// —— 镜像列表纯函数（SPEC §13.1，部署表单 ComboBox 数据源）——

/// GET /_api/images 的 repo 数与 tag 总量上限（防响应膨胀；超出静默截断）。
pub const IMAGES_MAX_REPOS: usize = 500;
pub const IMAGES_MAX_TAGS: usize = 500;

/// repo 是否属于 user：`<user>/` 命名空间（§4 镜像 127.0.0.1:5000/<user>/<project>:<tag>）。
/// split 精确比对首段，"alice-x/y" 不误吞给 "alice"。
pub fn repo_owned_by(repo: &str, user: &str) -> bool {
    repo.split_once('/').map(|(ns, _)| ns == user).unwrap_or(false)
}

/// catalog+tags 展平（§13.1 纯函数）：user=Some 仅留该用户 repo，None（admin 钥匙）全量；
/// repo 数与 tag 总量分别受 IMAGES_MAX_REPOS/IMAGES_MAX_TAGS 上限。
pub fn flatten_images(catalog: &[(String, Vec<String>)], user: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut repos = 0usize;
    for (repo, tags) in catalog {
        if let Some(o) = user {
            if !repo_owned_by(repo, o) {
                continue;
            }
        }
        repos += 1;
        if repos > IMAGES_MAX_REPOS {
            break;
        }
        for t in tags {
            if out.len() >= IMAGES_MAX_TAGS {
                return out;
            }
            out.push(format!("{repo}:{t}"));
        }
    }
    out
}

// —— 镜像删除纯函数（SPEC §14.1，/_api/image-rm 守卫与内核共用）——

/// `<repo>:<tag>` 一次性 `split_once(':')` → (repo, tag)；无冒号 → None
///（tag 合法性交 check_tag，repo 合法性交 valid_image_repo）。
pub fn split_image_ref(image: &str) -> Option<(String, String)> {
    let (repo, tag) = image.split_once(':')?;
    Some((repo.to_string(), tag.to_string()))
}

/// repo 白名单（进 registry URL 拼接前收口）：`'/'` 分段、每段 `[a-z0-9][a-z0-9._-]*`、
/// 非空、≤2 段、段内无 `..`（防穿越）。大写/空格/三段一律拒。
pub fn valid_image_repo(repo: &str) -> bool {
    let segs: Vec<&str> = repo.split('/').collect();
    if segs.is_empty() || segs.len() > 2 {
        return false;
    }
    segs.iter().all(|s| {
        !s.is_empty()
            && !s.contains("..")
            && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && s[1..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    })
}

/// digest 形状：`sha256:` + 64 位 hex（DELETE 路径 URL 拼接前收口，防注入/穿越）。
pub fn valid_digest(d: &str) -> bool {
    let Some(hex) = d.strip_prefix("sha256:") else { return false };
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// manifest JSON：`layers[].size` 求和 + `config.size`；无 layers（index/manifest list）→ None
///（调用方据此再取 manifests[0] 的子 manifest）；畸形 JSON → None。
pub fn sum_manifest_layers(body: &[u8]) -> Option<u64> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let layers = v.get("layers")?.as_array()?;
    let mut sum = v.get("config").and_then(|c| c.get("size")).and_then(|s| s.as_u64()).unwrap_or(0);
    for l in layers {
        sum = sum.checked_add(l.get("size")?.as_u64()?)?;
    }
    Some(sum)
}

// —— 发布者公钥 / registry 凭据文件 ——

pub fn kid_of(vk: &VerifyingKey) -> String {
    sha256_hex(&vk.to_bytes())[..8].to_string()
}

/// kid 形如 sha256(pubkey) 前 8 hex（user_add 生成）；入参校验防路径拼接穿越。
pub fn valid_kid(kid: &str) -> bool {
    kid.len() == 8 && kid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// user 全部公钥配 kid（`(kid, key)`，kid 由公钥内容派生，与文件名约定一致）。
/// 无法解析的 .pub（坏文件）跳过——它本就过不了验签。
pub fn user_keys_with_kid(root: &Path, user: &str) -> Vec<(String, VerifyingKey)> {
    let dir = tokens_dir(root, user);
    let Ok(rd) = fs::read_dir(&dir) else { return vec![] };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|o| o.to_str()) == Some("pub"))
        .filter_map(|p| {
            let s = fs::read_to_string(p).ok()?;
            let vk = UserKey::from_pub_string(s.trim()).ok()?;
            Some((kid_of(&vk), vk))
        })
        .collect()
}

/// 管理员标记：`tokens/<user>/<kid>.admin` 空文件存在 = 该钥匙为管理员（SPEC §12.2）。
pub fn admin_marker_path(root: &Path, user: &str, kid: &str) -> PathBuf {
    tokens_dir(root, user).join(format!("{kid}.admin"))
}

/// 文件 mtime（unix 秒；读不到给 0）——user_list 的 key 登记时间来源。
pub fn file_mtime(p: &Path) -> u64 {
    fs::metadata(p)
        .ok()
        .and_then(|md| md.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// bcrypt 行 upsert（同 user 只留最新一行）。
pub fn upsert_htpasswd(root: &Path, user: &str, hash: &str) -> R<()> {
    let path = htpasswd_path(root);
    let prefix = format!("{user}:");
    let mut lines: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with(&prefix))
        .map(str::to_string)
        .collect();
    lines.push(format!("{user}:{hash}"));
    fs::create_dir_all(root.join("state"))?;
    fs::write(&path, lines.join("\n") + "\n")?;
    Ok(())
}

pub fn remove_htpasswd_user(root: &Path, user: &str) -> R<()> {
    let path = htpasswd_path(root);
    let prefix = format!("{user}:");
    let lines: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .map(str::to_string)
        .collect();
    fs::write(&path, lines.join("\n") + "\n")?;
    Ok(())
}

pub fn write_private(path: &Path, contents: &str) -> R<()> {
    showroom_common::write_private(path, contents.as_bytes())?;
    Ok(())
}

// —— caddy 路由片段：文件名 <project>.caddy；路径恒 /<project>/（§11 无槽位）——

pub fn caddy_route(project: &str, port: u16) -> String {
    let base = format!("/{project}");
    // caddy matcher 名只允许字母数字下划线：连字符换下划线（大小写原样保留，§11）
    let m = format!("sr_{project}").replace('-', "_");
    // handle_path 自动剥掉 {base} 前缀再反代（容器收到的是 / 起始路径）
    format!(
        "@{m}_nos path {base}\n\
         redir @{m}_nos {base}/ 308\n\
         handle_path {base}/* {{\n\
           reverse_proxy 127.0.0.1:{port}\n\
         }}\n"
    )
}

// —— v1.4 槽位布局迁移（§11）：state/projects/<p>/<s>.* → <p>.*，纯状态层（不碰 docker）——

/// 多槽位上提计划（纯函数）：updated 最新者胜出上提，其余槽位弃置；
/// updated 并列时名字字典序大者胜（确定性）。输入须非空。
pub fn plan_project_lift(slots: &[(String, u64)]) -> (String, Vec<String>) {
    let mut v = slots.to_vec();
    v.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let (win, _) = v.pop().expect("plan_project_lift: non-empty");
    let losers = v.into_iter().map(|(s, _)| s).collect();
    (win, losers)
}

/// v1.4 布局迁移（§11，幂等；serve 启动与 reconcile/autoheal 前调用）：
/// - `state/projects/<p>/<slot>.{conf,env,last2}` → `state/projects/<p>.{conf,env,last2}`；
///   多槽位取 updated 最新者上提（conf 附 `legacyslot=<slot>` 供 DB 检测沿用），其余删除；
/// - ports.reg 三列 → 两列（被弃槽位的行一并移除，防端口被幽灵占用）；
/// - `routes/<p>-<s>.caddy` 旧文件删除，按胜出 conf 端口写新 `<p>.caddy`。
///
/// 全程 eprintln 如实记录；返回是否有变更（调用方据此 reload caddy）。
/// 容器 docker rename 收养在 ops::migrate_legacy（state 层不碰 docker）。
pub fn migrate_legacy_layout(root: &Path) -> bool {
    let pdir = projects_dir(root);
    let Ok(rd) = fs::read_dir(&pdir) else { return false };
    let mut changed = false;
    let mut winners: Vec<(String, String)> = vec![]; // (project, 胜出 slot)
    for e in rd.filter_map(|e| e.ok()) {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(p) = dir.file_name().and_then(|o| o.to_str()).map(str::to_string) else {
            continue;
        };
        let mut slots: Vec<(String, u64)> = vec![];
        if let Ok(sd) = fs::read_dir(&dir) {
            for f in sd.filter_map(|f| f.ok()) {
                let path = f.path();
                if path.extension().and_then(|o| o.to_str()) != Some("conf") {
                    continue;
                }
                let stem = path.file_stem().and_then(|o| o.to_str()).map(str::to_string);
                match (stem, fs::read_to_string(&path)) {
                    (Some(s), Ok(text)) => match Conf::parse(&text) {
                        Ok(c) => slots.push((s, c.updated)),
                        Err(e) => eprintln!("showroomctl: migrate: {p}/{s}.conf 解析失败，跳过: {e}"),
                    },
                    _ => eprintln!("showroomctl: migrate: {p}: unreadable conf skipped"),
                }
            }
        }
        if slots.is_empty() {
            // 无 conf 的残留目录（半迁移/手工残留）：整体删除
            if fs::remove_dir_all(&dir).is_ok() {
                eprintln!("showroomctl: migrate: removed conf-less project dir {p}/");
                changed = true;
            }
            continue;
        }
        let (win, losers) = plan_project_lift(&slots);
        eprintln!(
            "showroomctl: migrate: project {p}: lifted slot '{win}' (updated 最新), dropped [{}]",
            losers.join(", ")
        );
        // 上提三件套；conf 附 legacyslot（DB 检测沿用，§9 双名思路）
        let wconf = dir.join(format!("{win}.conf"));
        match fs::read_to_string(&wconf).map_err(ApiError::from).and_then(|t| Conf::parse(&t)) {
            Ok(mut c) => {
                c.legacyslot = Some(win.clone());
                if let Err(e) = c.save(root, &p) {
                    eprintln!("showroomctl: migrate: {p}: write {} 失败: {e}", conf_path(root, &p).display());
                    continue;
                }
            }
            Err(e) => {
                eprintln!("showroomctl: migrate: {p}/{win}.conf 重读失败，跳过该项目: {e}");
                continue;
            }
        }
        for ext in ["env", "last2"] {
            let src = dir.join(format!("{win}.{ext}"));
            let dst = match ext {
                "env" => env_path(root, &p),
                _ => last2_path(root, &p),
            };
            if src.exists() && fs::rename(&src, &dst).is_err() {
                // 同目录树内 rename 恒同盘；失败（并发占用等）退回拷贝语义
                if let Ok(b) = fs::read(&src) {
                    let _ = fs::write(&dst, b);
                }
            }
        }
        // 旧路由文件删除 + 新路由按胜出 conf 端口写
        let port = Conf::load(root, &p).ok().map(|c| c.port);
        for s in std::iter::once(&win).chain(losers.iter()) {
            let _ = fs::remove_file(legacy_route_path(root, &p, s));
        }
        if let Some(port) = port {
            let _ = fs::create_dir_all(routes_dir(root));
            let _ = fs::write(route_path(root, &p), caddy_route(&p, port));
        }
        // 旧目录整体清除（含被弃槽位与未知残留文件）
        if fs::remove_dir_all(&dir).is_ok() {
            changed = true;
            winners.push((p, win));
        } else {
            eprintln!("showroomctl: migrate: {p}: 旧目录删除失败（下次重试）");
        }
    }
    if changed && ports_reg(root).exists() {
        rewrite_ports_reg(root, &winners);
    }
    changed
}

/// ports.reg 三列（旧）→ 两列（§11）：迁移过的项目只保留胜出槽位的行，
/// 其余行剥掉第三列。已是两列的行原样保留。
fn rewrite_ports_reg(root: &Path, winners: &[(String, String)]) {
    let lines: Vec<String> = fs::read_to_string(ports_reg(root))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let port = it.next()?;
            let proj = it.next()?;
            if let Some(slot) = it.next() {
                // 三列旧行：本项目已迁移且行非胜出槽位 → 弃（端口随槽位一并退役）
                if winners.iter().any(|(p, w)| p == proj && slot != w) {
                    return None;
                }
            }
            Some(format!("{port} {proj}"))
        })
        .collect();
    let out = if lines.is_empty() { String::new() } else { lines.join("\n") + "\n" };
    let _ = fs::write(ports_reg(root), out);
}

// —— owner→user 更名的状态目录迁移：state/owners/ → state/users/ ——

/// `state/owners/` 整体改名为 `state/users/`（registry 凭据 reg.cred 的家）。
/// 幂等：owners 不存在（从未有/已迁移）恒 no-op；users 已存在时不覆盖
///（手动建过/并发已迁移），如实打印并跳过。serve 启动与 reconcile 前调用；
/// 读取侧另带双目录兼容（先 users 后 owners 回退），迁移前后行为一致。
pub fn migrate_state_dirs(root: &Path) -> bool {
    let old = root.join("state/owners");
    let new = root.join("state/users");
    if !old.is_dir() {
        return false;
    }
    if new.exists() {
        eprintln!("showroomctl: migrate: state/owners 与 state/users 并存，跳过整体改名（如需迁移请手工合并）");
        return false;
    }
    match fs::rename(&old, &new) {
        Ok(()) => {
            eprintln!("showroomctl: migrate: state/owners/ 已整体改名为 state/users/");
            true
        }
        Err(e) => {
            // 跨设备等场景退回逐目录拷贝语义；失败如实记录，下次启动重试
            let mut moved = true;
            if let Ok(rd) = fs::read_dir(&old) {
                for e in rd.flatten() {
                    let dst = new.join(e.file_name());
                    if fs::rename(e.path(), &dst).is_err() && fs::copy(e.path(), &dst).is_err() {
                        moved = false;
                    }
                }
            }
            if moved {
                let _ = fs::remove_dir_all(&old);
                eprintln!("showroomctl: migrate: state/owners/ 已逐项迁入 state/users/");
                true
            } else {
                eprintln!("showroomctl: migrate: state/owners/ → state/users/ 失败（下次启动重试）: {e}");
                false
            }
        }
    }
}

// —— _tmp 陈旧会话筛选（SPEC §8.1：>24h 由 gc-images 顺带删）——

pub const TMP_STALE_SECS: u64 = 24 * 3600;

/// _tmp 会话 id（upload/dump 共用）：32 位 hex，防路径穿越（会话文件名拼接入点）。
pub fn valid_session_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 陈旧 _tmp 文件筛选：给定 (名字, mtime) 与 now，返回 age>24h 应删的名字。
/// 纯函数——mtime 由调用方喂（gc 走文件系统，测试喂 fake）。
pub fn stale_tmp(names_mtimes: &[(String, u64)], now: u64) -> Vec<String> {
    names_mtimes
        .iter()
        .filter(|(_, mt)| now.saturating_sub(*mt) > TMP_STALE_SECS)
        .map(|(n, _)| n.clone())
        .collect()
}

/// 快照保留裁剪（SPEC §8.2）：名字按字典序（<前缀>YYYYMMDD-HHMM 即时间序）排序，
/// 保留最新 keep 个，返回应删的最旧列表。
pub fn prune_snapshots(names: &[String], keep: usize) -> Vec<String> {
    let mut v = names.to_vec();
    v.sort();
    let del = v.len().saturating_sub(keep);
    v.into_iter().take(del).collect()
}

// —— admin status 纯解析（SPEC §12.2：df / docker ps / list-timers 样例文本 → 结构）——

/// `df -B1 --output=size,used <path>` 输出 → (总字节, 已用字节)。
/// 注意：coreutils 9.4 无 `total` 字段，用 `size`（同为 1B 块总量）；取最后一个非空行，
/// 两列空白分隔的 u64——多路径调用时与传入路径一一对应，单路径取末行即本路径。
/// 解析为两列白名单、与列名无关：`--output=size,avail`（§14 space/磁盘准入口径）时
/// 第二列即 avail，返回 (size, avail)，同一函数复用。
pub fn parse_df_bytes(text: &str) -> Option<(u64, u64)> {
    let last = text.lines().rfind(|l| !l.trim().is_empty())?;
    let mut it = last.split_whitespace();
    let total: u64 = it.next()?.parse().ok()?;
    let used: u64 = it.next()?.parse().ok()?;
    Some((total, used))
}

/// `docker ps -a --format '{{.Names}}\t{{.State}}'` 输出 → (容器名, 状态) 列表。
pub fn parse_container_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.split_once('\t').map(|(n, s)| (n.to_string(), s.trim().to_string())))
        .collect()
}

/// `systemctl list-timers --no-legend` 一行 → (名, NEXT)。整表解析见 parse_list_timers。
///
/// 行形如 `Sun 2026-10-11 01:11:21 +08  23s Sun 2026-10-11 00:11:24 +08  59min ago x.timer x.service`：
/// NEXT=前 4 token（周几 日期 时间 时区；时区 token 个数随版本，这里按实测 4 个），
/// 名=行内唯一 `.timer` 结尾 token（`.service` 结尾的是 ACTIVATES，不误取）；LEFT/PASSED
/// 可含空格（"1h 32min"），故从 NEXT 端与 UNIT token 两头锚定、中段不碰。
/// NEXT 为 "n/a"（未排程）时原样带回。
fn parse_timer_line(line: &str) -> Option<(String, String)> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.is_empty() {
        return None;
    }
    let pos = toks.iter().position(|t| t.ends_with(".timer"))?;
    let name = toks[pos].trim_end_matches(".timer").to_string();
    let next = if toks[0] == "n/a" || toks.len() < 4 {
        toks[0].to_string()
    } else {
        toks[..4].join(" ")
    };
    Some((name, next))
}

pub fn parse_list_timers(text: &str) -> Vec<(String, String)> {
    text.lines().filter_map(parse_timer_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn troot(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("srctl-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn base_conf(updated: u64) -> Conf {
        Conf {
            user: "alice".into(),
            tag: "v1".into(),
            port: 8100,
            cport: 3000,
            mem: "256m".into(),
            cpu: 1.5,
            redisdb: 7,
            updated,
            stopped: false,
            legacyslot: None,
        }
    }

    // 1. 名字校验（转发 common::names）：§11 放宽正则 + 保留名；辅助名沿用旧小写正则
    #[test]
    fn names() {
        for good in ["abc", "a-b2", "0", "a", &"a".repeat(31), "pr-42-x1", "Demo42", "A-B"] {
            assert!(check_name("project", good).is_ok(), "{good} should be valid");
        }
        for bad in ["", "-a", "a_", "a b", "中文", &"a".repeat(32), "a.b", "a/b"] {
            assert!(check_name("project", bad).is_err(), "{bad} should be invalid");
        }
        // 保留名：project 拒（_api 等本就不合正则，分歧点在 v2）
        for r in ["v2", "_api", "_bootstrap", "_console"] {
            assert!(check_name("project", r).is_err(), "{r} reserved for project");
        }
        // 辅助名（user）：旧小写正则，大写拒绝
        assert!(check_name("user", "alice").is_ok());
        assert_eq!(check_name("user", "Alice").unwrap_err().code, "E_NAME");
        assert_eq!(check_name("project", "v2").unwrap_err().code, "E_NAME");
    }

    // 1b. Conf::to_json（ps 与 /_api/projects 共用行，§11 无 slot 字段）
    #[test]
    fn conf_to_json() {
        let c = base_conf(1_700_000_000);
        let v = c.to_json("demo", "running");
        assert_eq!(v["project"], "demo");
        assert!(v.get("slot").is_none(), "§11 响应不再含 slot");
        assert_eq!(v["user"], "alice");
        assert_eq!(v["tag"], "v1");
        assert_eq!(v["status"], "running");
        assert_eq!(v["url"], public_url("demo"));
        assert_eq!(v["port"], 8100);
        assert_eq!(v["mem"], "256m");
        assert_eq!(v["cpu"], 1.5);
        assert_eq!(v["updated"], 1_700_000_000);
    }

    // 2a. conf 序列化往返（全字段，含/不含 stopped 与 legacyslot）
    #[test]
    fn conf_roundtrip() {
        let mut c = base_conf(1_700_000_000);
        c.stopped = true;
        let t = c.to_conf();
        assert!(t.contains("stopped=1\n"));
        assert!(!t.contains("legacyslot"), "未迁移项目不写 legacyslot");
        assert_eq!(Conf::parse(&t).unwrap(), c);
        let mut c2 = c.clone();
        c2.stopped = false;
        assert!(!c2.to_conf().contains("stopped"));
        assert_eq!(Conf::parse(&c2.to_conf()).unwrap(), c2);
        // legacyslot（§11 迁移标记）往返
        let mut c3 = c.clone();
        c3.legacyslot = Some("stable".into());
        let t3 = c3.to_conf();
        assert!(t3.contains("legacyslot=stable\n"));
        assert_eq!(Conf::parse(&t3).unwrap(), c3);
        // 整数 cpu 往返
        let mut c4 = c.clone();
        c4.cpu = 1.0;
        assert_eq!(Conf::parse(&c4.to_conf()).unwrap(), c4);
    }

    // 2b. env 文件往返（§6 全字段具体化；§11 无 SHOWROOM_SLOT，BASE_PATH 恒 /<project>/）
    #[test]
    fn env_file_roundtrip() {
        let txt = env_file_contents("demo", &db_ident("demo"), "s3cret", 3);
        let m = parse_kv(&txt);
        assert_eq!(m["DB_HOST"], "sr-postgres");
        assert_eq!(m["DB_PORT"], "5432");
        assert_eq!(m["DB_NAME"], "sr_demo");
        assert_eq!(m["DB_USER"], "sr_demo");
        assert_eq!(m["DB_PASSWORD"], "s3cret");
        assert_eq!(m["DATABASE_URL"], "postgresql://sr_demo:s3cret@sr-postgres:5432/sr_demo");
        assert_eq!(m["REDIS_HOST"], "sr-redis");
        assert_eq!(m["REDIS_PORT"], "6379");
        assert_eq!(m["REDIS_DB"], "3");
        assert_eq!(m["REDIS_URL"], "redis://sr-redis:6379/3");
        assert_eq!(m["SHOWROOM_PROJECT"], "demo");
        assert_eq!(m["SHOWROOM_BASE_PATH"], "/demo/");
        assert!(!m.contains_key("SHOWROOM_SLOT"), "§11 移除 SHOWROOM_SLOT");
        // 大写/连字符项目：库名归一化，BASE_PATH 原样
        let up = parse_kv(&env_file_contents("Demo-9", &db_ident("Demo-9"), "pw", 0));
        assert_eq!(up["DB_NAME"], "sr_Demo_9");
        assert_eq!(up["SHOWROOM_BASE_PATH"], "/Demo-9/");
        // legacy 库名沿用时 env 同样指向旧名（§9→§11 过渡期）
        let lg = parse_kv(&env_file_contents("skill-demo", "sr_skill-demo_stable", "pw", 1));
        assert_eq!(lg["DB_NAME"], "sr_skill-demo_stable");
        for k in [
            "DB_HOST", "DB_PORT", "DB_NAME", "DB_USER", "DB_PASSWORD", "DATABASE_URL",
            "REDIS_HOST", "REDIS_PORT", "REDIS_DB", "REDIS_URL", "SHOWROOM_PROJECT",
            "SHOWROOM_BASE_PATH",
        ] {
            assert!(up.contains_key(k) || m.contains_key(k), "missing {k}");
        }
    }

    // 2c. 库名归一化 + legacy 检测沿用（§9→§11：候选按优先级，纯函数裁决）
    #[test]
    fn db_ident_normalization_and_legacy() {
        assert_eq!(db_ident("demo"), "sr_demo");
        assert_eq!(db_ident("skill-demo"), "sr_skill_demo");
        assert_eq!(db_ident("Demo-9"), "sr_Demo_9");
        // 旧候选：verbatim 与归一化双名，无连字符时同形去重
        assert_eq!(legacy_db_idents("skill-demo", "stable"), vec!["sr_skill-demo_stable", "sr_skill_demo_stable"]);
        assert_eq!(legacy_db_idents("demo", "pr-42"), vec!["sr_demo_pr-42", "sr_demo_pr_42"]);
        assert_eq!(legacy_db_idents("demo", "stable"), vec!["sr_demo_stable"], "无连字符时新旧同形");

        // 裁决：新名在 → 新名（并存时新名优先）；仅旧名在 → 沿用；皆无 → 新名（首建）
        let names = |p: &str, s: &str| {
            let mut v = vec![db_ident(p)];
            v.extend(legacy_db_idents(p, s));
            v
        };
        let n = names("skill-demo", "stable");
        assert_eq!(resolve_db_ident(&n, &[true, false, false]), ("sr_skill_demo".into(), true));
        assert_eq!(resolve_db_ident(&n, &[true, true, false]), ("sr_skill_demo".into(), true));
        assert_eq!(resolve_db_ident(&n, &[false, true, false]), ("sr_skill-demo_stable".into(), true));
        assert_eq!(resolve_db_ident(&n, &[false, false, true]), ("sr_skill_demo_stable".into(), true));
        assert_eq!(resolve_db_ident(&n, &[false, false, false]), ("sr_skill_demo".into(), false));
    }

    // 3. caddy 片段（§11 恒 /<project>/）：路径正确含 308 重定向行
    #[test]
    fn caddy_route_snippets() {
        let s = caddy_route("demo", 8100);
        assert!(s.contains("@sr_demo_nos path /demo\n"));
        assert!(s.contains("redir @sr_demo_nos /demo/ 308\n"));
        assert!(s.contains("handle_path /demo/* {\n"));
        assert!(s.contains("reverse_proxy 127.0.0.1:8100\n"));

        let n = caddy_route("Demo-9", 8101);
        assert!(n.contains("@sr_Demo_9_nos path /Demo-9\n"));
        assert!(n.contains("handle_path /Demo-9/* {\n"));
        assert!(n.contains("reverse_proxy 127.0.0.1:8101\n"));
    }

    // 4. policy 解析与限额判断
    #[test]
    fn policy_parse_and_limits() {
        let root = troot("policy");
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(
            root.join("state/policy.env"),
            "MAX_SLOTS=5\nDEFAULT_MEM=128m\nMAX_MEM=512m\nIMPORT_MAX_GB=2\nALLOW_LAN_PORT=1\nRESERVE_MEM_MB=512\nREDIS_DBS=8\nDISK_RESERVE_GB=5\nUPLOAD_SPACE_FACTOR=2\n",
        )
        .unwrap();
        let p = Policy::load(&root);
        assert_eq!(
            (
                p.max_slots,
                p.default_mem.as_str(),
                p.max_mem.as_str(),
                p.import_max_gb,
                p.allow_lan_port,
                p.reserve_mem_mb,
                p.redis_dbs,
                p.disk_reserve_gb,
                p.upload_space_factor,
            ),
            (5, "128m", "512m", 2, true, 512, 8, 5, 2)
        );
        assert_eq!(p.import_cap_bytes(), 2 << 30);
        assert_eq!(p.redis_pool_cap(), 8, "§13.3 REDIS_DBS 可调");
        assert_eq!(p.reserve_bytes(), 5 << 30, "§14.4 DISK_RESERVE_GB→字节");

        let d = Policy::load(&troot("policy-default"));
        assert_eq!(
            (
                d.max_slots,
                d.default_mem.as_str(),
                d.max_mem.as_str(),
                d.import_max_gb,
                d.allow_lan_port,
                d.reserve_mem_mb,
                d.redis_dbs,
                d.disk_reserve_gb,
                d.upload_space_factor,
            ),
            (30, "256m", "1g", 8, false, 1536, 256, 20, 4)
        );
        assert_eq!(d.redis_pool_cap(), 256, "§13.3 默认池 256");
        assert_eq!(d.reserve_bytes(), 20 << 30, "§14.4 缺省预留 20GB");
        // 非法值回默认；超 u8 承载（>256）的池 cap 收在 256（Conf.redisdb 为 u8）
        let root2 = troot("policy-redis");
        fs::create_dir_all(root2.join("state")).unwrap();
        fs::write(
            root2.join("state/policy.env"),
            "REDIS_DBS=abc\nDISK_RESERVE_GB=x\nUPLOAD_SPACE_FACTOR=-1\n",
        )
        .unwrap();
        assert_eq!(Policy::load(&root2).redis_dbs, 256);
        assert_eq!((Policy::load(&root2).disk_reserve_gb, Policy::load(&root2).upload_space_factor), (20, 4));
        fs::write(root2.join("state/policy.env"), "REDIS_DBS=1000\n").unwrap();
        assert_eq!(Policy::load(&root2).redis_pool_cap(), 256);

        assert_eq!(parse_mem("256m"), Some(256 * 1024 * 1024));
        assert_eq!(parse_mem("1g"), Some(1 << 30));
        assert_eq!(parse_mem("1024k"), Some(1 << 20));
        assert_eq!(parse_mem("4096"), Some(4096));
        for bad in ["abc", "", "10x", "m", "-1m"] {
            assert_eq!(parse_mem(bad), None, "{bad}");
        }

        assert_eq!(effective_mem(&p, None).unwrap(), "128m");
        assert_eq!(effective_mem(&p, Some("512m")).unwrap(), "512m");
        assert_eq!(effective_mem(&p, Some("64m")).unwrap(), "64m");
        assert_eq!(effective_mem(&p, Some("1g")).unwrap_err().code, "E_QUOTA");
        assert_eq!(effective_mem(&p, Some("big")).unwrap_err().code, "E_NAME");
    }

    // 5. 端口分配（两列 ports.reg）：顺序分配、幂等 upsert、占满 → E_QUOTA、释放后可复用
    #[test]
    fn port_alloc_and_quota() {
        let root = troot("ports");
        fs::create_dir_all(root.join("state")).unwrap();
        assert_eq!(alloc_port(&root).unwrap(), PORT_MIN);

        register_port(&root, 8100, "a").unwrap();
        register_port(&root, 8101, "b").unwrap();
        assert_eq!(alloc_port(&root).unwrap(), 8102);

        register_port(&root, 8100, "a").unwrap(); // 幂等（同项目 upsert）
        assert_eq!(read_ports(&root).len(), 2);
        register_port(&root, 8102, "a").unwrap(); // 同项目换端口 → 旧行替换
        assert_eq!(read_ports(&root).len(), 2);

        for p in PORT_MIN..=PORT_MAX {
            register_port(&root, p, &format!("x{p}")).unwrap();
        }
        assert_eq!(alloc_port(&root).unwrap_err().code, "E_QUOTA");

        release_port(&root, "x8150").unwrap();
        assert_eq!(alloc_port(&root).unwrap(), 8150);
        // 精确 token 比对：释放 "demo" 不影响 "mydemo"
        register_port(&root, 8150, "mydemo").unwrap();
        release_port(&root, "demo").unwrap();
        assert!(read_ports(&root).iter().any(|(p, pj)| *p == 8150 && pj == "mydemo"));
        // 行内容恒两列
        let reg = fs::read_to_string(ports_reg(&root)).unwrap();
        assert!(reg.lines().all(|l| l.split_whitespace().count() == 2), "{reg}");
    }

    // 附带：redisdb 分配（conf 扫描，按项目；§13.3 池大小=policy.redis_dbs，耗尽 E_QUOTA 含 已用/总量）
    #[test]
    fn redisdb_alloc() {
        let root = troot("redis");
        let mut a = base_conf(1);
        a.redisdb = 0;
        a.save(&root, "demo").unwrap();
        let mut n = base_conf(1);
        n.redisdb = 1;
        n.save(&root, "other").unwrap();
        // 默认池 256：0/1 已占 → 分 2
        assert_eq!(alloc_redisdb(&root, &Policy::default()).unwrap(), 2);

        // 小池耗尽（§13.3）：池 4 → 依次分 2、3；占满 0-3 后 E_QUOTA，msg 含 已用/总量（4/4）
        let small = Policy { redis_dbs: 4, ..Policy::default() };
        assert_eq!(alloc_redisdb(&root, &small).unwrap(), 2);
        let save = |p: &str, db: u8| {
            let mut c = base_conf(1);
            c.redisdb = db;
            c.save(&root, p).unwrap();
        };
        save("third", 2);
        assert_eq!(alloc_redisdb(&root, &small).unwrap(), 3);
        save("fourth", 3);
        let e = alloc_redisdb(&root, &small).unwrap_err();
        assert_eq!(e.code, "E_QUOTA", "{}", e.msg);
        assert!(e.msg.contains("4/4"), "msg 须含 已用/总量: {}", e.msg);
        // 池收窄（REDIS_DBS=2）后 0/1 均被占 → 本池如实报耗尽（已用 4 > 总量 2 也如实带出）
        let tiny = Policy { redis_dbs: 2, ..Policy::default() };
        let e2 = alloc_redisdb(&root, &tiny).unwrap_err();
        assert_eq!(e2.code, "E_QUOTA");
        assert!(e2.msg.contains("4/2"), "{}", e2.msg);
    }

    // 附带 b：user_redis_dbs 聚合（§13.2 浏览隔离）——按 conf.user 收集，升序去重；无项目空表
    #[test]
    fn user_redis_dbs_aggregation() {
        let root = troot("user-dbs");
        for (p, user, db) in [
            ("demo1", "alice", 3u8),
            ("demo2", "alice", 7),
            ("demo3", "alice", 3), // 重复 db 去重
            ("demo4", "bob", 2),
            ("demo5", "carol", 255),
        ] {
            let mut c = base_conf(1);
            c.user = user.into();
            c.redisdb = db;
            c.save(&root, p).unwrap();
        }
        assert_eq!(user_redis_dbs(&root, "alice"), vec![3, 7]);
        assert_eq!(user_redis_dbs(&root, "bob"), vec![2]);
        assert_eq!(user_redis_dbs(&root, "carol"), vec![255], "§13.3 大池库号原样聚合");
        assert!(user_redis_dbs(&root, "nobody").is_empty());
    }

    // 5b. 多槽位上提计划（纯函数）：updated 最新胜；并列名字大者胜；单槽位无败者
    #[test]
    fn plan_project_lift_selection() {
        let (w, l) = plan_project_lift(&[("stable".into(), 100), ("next".into(), 300), ("pr42".into(), 200)]);
        assert_eq!((w.as_str(), l.len()), ("next", 2));
        assert!(l.contains(&"stable".to_string()) && l.contains(&"pr42".to_string()));

        // 单槽位：直接上提
        let (w, l) = plan_project_lift(&[("stable".into(), 1)]);
        assert_eq!((w.as_str(), l.as_slice()), ("stable", [].as_slice()));

        // updated 并列：名字字典序大者胜（确定性）
        let (w, _) = plan_project_lift(&[("aaa".into(), 5), ("zzz".into(), 5)]);
        assert_eq!(w, "zzz");

        // 乱序输入不受影响
        let (w, l) = plan_project_lift(&[("c".into(), 1), ("a".into(), 9), ("b".into(), 5)]);
        assert_eq!((w.as_str(), l.len()), ("a", 2));
    }

    // 5c. v1.4 布局迁移（§11）：单槽位上提 + 双列 ports.reg + 新路由文件 + 幂等
    #[test]
    fn migrate_legacy_single_slot() {
        let root = troot("migrate1");
        fs::create_dir_all(root.join("state/projects/hello")).unwrap();
        let mut c = base_conf(1_000);
        c.tag = "v9".into();
        c.port = 8105;
        fs::write(root.join("state/projects/hello/stable.conf"), c.to_conf()).unwrap();
        fs::write(root.join("state/projects/hello/stable.env"), "DB_NAME=sr_hello_stable\n").unwrap();
        fs::write(root.join("state/projects/hello/stable.last2"), "v8\n").unwrap();
        fs::write(root.join("state/ports.reg"), "8105 hello stable\n8199 other stable\n").unwrap();
        fs::create_dir_all(root.join("state/routes")).unwrap();
        fs::write(root.join("state/routes/hello-stable.caddy"), "old junk\n").unwrap();

        assert!(migrate_legacy_layout(&root));
        // 三件套上提到项目级；conf 附 legacyslot
        let lifted = Conf::load(&root, "hello").unwrap();
        assert_eq!(lifted.tag, "v9");
        assert_eq!(lifted.legacyslot.as_deref(), Some("stable"));
        assert!(fs::read_to_string(env_path(&root, "hello")).unwrap().contains("sr_hello_stable"));
        assert_eq!(read_last2(&root, "hello").as_deref(), Some("v8"));
        assert!(!root.join("state/projects/hello").exists(), "旧目录清除");
        // ports.reg 两列；未迁移项目行剥列保留
        assert_eq!(read_ports(&root), vec![(8105, "hello".into()), (8199, "other".into())]);
        // 旧路由删、新路由写（按胜出端口）
        assert!(!root.join("state/routes/hello-stable.caddy").exists());
        let r = fs::read_to_string(route_path(&root, "hello")).unwrap();
        assert!(r.contains("reverse_proxy 127.0.0.1:8105"), "{r}");
        // 幂等：再跑无变更
        assert!(!migrate_legacy_layout(&root));
        assert_eq!(Conf::load(&root, "hello").unwrap(), lifted);
    }

    // 5d. 多槽位迁移：updated 最新上提，其余 conf/env/last2 删除；被弃槽位端口行退役
    #[test]
    fn migrate_legacy_multi_slot() {
        let root = troot("migrate2");
        for (s, updated, port) in [("stable", 100u64, 8100u16), ("next", 300, 8101), ("pr42", 200, 8102)] {
            let dir = root.join("state/projects/hello");
            fs::create_dir_all(&dir).unwrap();
            let mut c = base_conf(updated);
            c.port = port;
            fs::write(dir.join(format!("{s}.conf")), c.to_conf()).unwrap();
            fs::write(dir.join(format!("{s}.env")), format!("DB_NAME=sr_hello_{s}\n")).unwrap();
        }
        fs::write(root.join("state/projects/hello/stable.last2"), "v0\n").unwrap();
        fs::write(
            root.join("state/ports.reg"),
            "8100 hello stable\n8101 hello next\n8102 hello pr42\n8103 alone stable\n",
        )
        .unwrap();

        assert!(migrate_legacy_layout(&root));
        let lifted = Conf::load(&root, "hello").unwrap();
        assert_eq!(lifted.updated, 300, "updated 最新者（next）胜出");
        assert_eq!(lifted.legacyslot.as_deref(), Some("next"));
        assert_eq!(lifted.port, 8101);
        // 被弃槽位端口行退役，胜出行与无关项目保留
        assert_eq!(
            read_ports(&root),
            vec![(8101, "hello".into()), (8103, "alone".into())]
        );
        // 上提的 env 来自胜出槽位
        assert!(fs::read_to_string(env_path(&root, "hello")).unwrap().contains("sr_hello_next"));
        assert!(read_last2(&root, "hello").is_none(), "败者 last2 不上提（胜者无 last2）");
        assert!(!root.join("state/projects/hello").exists());
        // 幂等
        assert!(!migrate_legacy_layout(&root));
    }

    // 5e. owner→user 更名目录迁移：owners 存在且 users 不存在 → 整体 mv；幂等；两者并存不覆盖
    #[test]
    fn migrate_state_dirs_moves_owners_to_users() {
        let root = troot("dirs");
        // 无 owners：no-op
        assert!(!migrate_state_dirs(&root));
        // 有 owners：整体改名 + 内容原样
        fs::create_dir_all(root.join("state/owners/alice")).unwrap();
        fs::write(root.join("state/owners/alice/reg.cred"), "alice:pw\n").unwrap();
        fs::create_dir_all(root.join("state/owners/bob")).unwrap();
        assert!(migrate_state_dirs(&root));
        assert!(!root.join("state/owners").exists());
        assert_eq!(
            fs::read_to_string(root.join("state/users/alice/reg.cred")).unwrap(),
            "alice:pw\n"
        );
        assert!(root.join("state/users/bob").is_dir());
        // 幂等：再跑 no-op
        assert!(!migrate_state_dirs(&root));
        // users 已存在时不覆盖（手动建过/已迁移）
        fs::create_dir_all(root.join("state/owners/carol")).unwrap();
        assert!(!migrate_state_dirs(&root));
        assert!(root.join("state/owners/carol").is_dir(), "并存时保留原状");
    }

    // 6. _tmp 陈旧筛选：age>24h 才删；恰 24h / 未来 mtime（时钟偏移）不删
    #[test]
    fn stale_tmp_filter() {
        let now = 1_000_000u64;
        let h = 3600u64;
        let files = vec![
            ("a.bin".to_string(), now - 25 * h),
            ("b.bin".to_string(), now - 24 * h),
            ("c.meta".to_string(), now - 23 * h),
            ("d.bin".to_string(), now),
            ("e.bin".to_string(), now + 100),
        ];
        assert_eq!(stale_tmp(&files, now), vec!["a.bin".to_string()]);
        assert!(stale_tmp(&[], now).is_empty());
    }

    // 7. 快照保留裁剪：按名（=时间序）保最新 keep 个，删最旧的超出部分
    #[test]
    fn snapshot_prune() {
        let names: Vec<String> =
            (0..30u32).map(|i| format!("h-202601{:02}-{:02}00", 1 + i / 24, i % 24)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        let del = prune_snapshots(&names, 24);
        assert_eq!(del, sorted[..6].to_vec());
        assert_eq!(del.first().unwrap(), "h-20260101-0000");

        let daily: Vec<String> = (1..=10).map(|i| format!("d-2026010{i}-1200")).collect();
        assert_eq!(prune_snapshots(&daily, 7).len(), 3);

        assert!(prune_snapshots(&names, 100).is_empty(), "不足保留数不删");
        assert!(prune_snapshots(&[], 24).is_empty());
        // 乱序输入同样正确（内部排序）
        let mut shuf = names.clone();
        shuf.reverse();
        assert_eq!(prune_snapshots(&shuf, 24), del);
    }

    // 8. 内存准入边界（§10.2）：刚好满足 / 差 1MB / 超大 / avail<reserve / kB 向下取整保守
    #[test]
    fn mem_admission_boundaries() {
        let kb = |mb: u64| mb * 1024;

        // 刚好满足：avail - reserve == need → 放行
        assert_eq!(mem_admission(256, kb(1536 + 256), 1536), Ok(()));
        assert_eq!(mem_admission(0, kb(1536), 1536), Ok(()));

        // 差 1MB → 拒，msg 含 可用/预留/需求 三个数字
        let e = mem_admission(256, kb(1536 + 255), 1536).unwrap_err();
        for n in ["1791", "1536", "256"] {
            assert!(e.contains(n), "msg 缺数字 {n}: {e}");
        }

        // 超大需求 → 拒
        assert!(mem_admission(1_000_000, kb(32 * 1024), 1536).is_err());

        // avail < reserve：差额为负（有符号算，免下溢），need=0 也按公式拒
        assert!(mem_admission(0, kb(512), 1536).is_err());
        assert!(mem_admission(100, kb(1000), 1536).is_err());

        // kB → MB 向下取整走保守侧：1792MB 差 1kB 不够 1792MB 需求（floor 成 1791）
        assert!(mem_admission(1792, kb(1792) - 1, 0).is_err());
        assert_eq!(mem_admission(1792, kb(1792), 0), Ok(()));
    }

    // 8b. 磁盘准入边界（§14.2）：恰满足 / 差 1MB / factor=0 / avail<reserve / 需求向上取整
    #[test]
    fn disk_admission_boundaries() {
        let mb = |n: u64| n << 20;

        // 刚好满足：avail−reserve == need（4MB × 3 = 12MB）→ 放行
        assert_eq!(disk_admission(mb(4), mb(12), 0, 3), Ok(()));
        // 差 1MB → 拒，msg 含 可用/预留/需求/因子 四数与删除引导
        let e = disk_admission(mb(4), mb(11), 0, 3).unwrap_err();
        for n in ["11", "0", "12", "3"] {
            assert!(e.contains(n), "msg 缺数字 {n}: {e}");
        }
        assert!(e.contains("请先删除旧镜像"), "msg 须含删除引导: {e}");
        assert!(e.starts_with("磁盘空间不足"), "msg 模板前缀: {e}");

        // factor=0：需求 0 恒过（avail ≥ reserve 时）
        assert_eq!(disk_admission(mb(999), mb(10), mb(5), 0), Ok(()));
        // avail < reserve：差额为负（有符号算免下溢），need=0 也按公式拒
        assert!(disk_admission(0, mb(1), mb(100), 0).is_err());
        assert!(disk_admission(mb(1), mb(1), mb(100), 4).is_err());

        // 需求向上取整（判定侧保守）：sealed=1B × 1 → need=1MB；avail−reserve 差 1B（floor 成 0MB）不够
        assert!(disk_admission(1, mb(1) - 1, 0, 1).is_err());
        assert_eq!(disk_admission(1, mb(1), 0, 1), Ok(()));

        // 超大 factor（saturating 不 panic）：一律拒
        assert!(disk_admission(1, u64::MAX / 2, 0, u64::MAX).is_err());
    }

    // 11a. 镜像删除纯函数（§14.1）：split_image_ref / valid_image_repo / valid_digest
    #[test]
    fn image_ref_and_digest_validation() {
        // split：一次性 split_once(':')
        assert_eq!(
            split_image_ref("alice/demo:v1"),
            Some(("alice/demo".into(), "v1".into()))
        );
        assert_eq!(split_image_ref("demo:v1"), Some(("demo".into(), "v1".into())));
        assert_eq!(split_image_ref("bare"), None, "无冒号");
        assert_eq!(split_image_ref("alice-x/y"), None, "无冒号（斜杠不冒充分隔）");

        // repo 白名单
        for good in ["alice/demo", "alice", "a-b/c.d_e", "user1/proj-2.0", "0/9"] {
            assert!(valid_image_repo(good), "{good} 应合法");
        }
        for bad in [
            "a b/x", "../x", "A/b", "a/B", "", "/", "alice/", "/x", "a/b/c", "a..b", ".x/y", "-a/b",
        ] {
            assert!(!valid_image_repo(bad), "{bad:?} 应拒绝");
        }
        // 前缀不误吞（沿用 §13 测试法）："alice-x/y" 不属于 "alice"
        assert!(!repo_owned_by("alice-x/y", "alice"));

        // digest：sha256: + 64 hex
        let hex64 = "a".repeat(64);
        assert!(valid_digest(&format!("sha256:{hex64}")));
        assert!(!valid_digest(&format!("sha256:{}", "a".repeat(63))), "短");
        assert!(!valid_digest(&format!("sha256:{}", "a".repeat(65))), "长");
        assert!(!valid_digest(&format!("sha256:{}", "g".repeat(64))), "非 hex");
        assert!(!valid_digest(&format!("sha512:{}", "a".repeat(128))), "别的算法头");
        assert!(!valid_digest("sha256:"));
        assert!(!valid_digest(&hex64), "无算法头");
    }

    // 11b. manifest 层求和（§14.1）：v2 求和 / index→None / 畸形→None
    #[test]
    fn manifest_layers_sum() {
        let m = json!({
            "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 1784, "digest": "sha256:aaa"},
            "layers": [
                {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "size": 8192, "digest": "sha256:b1"},
                {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "size": 16384, "digest": "sha256:b2"},
                {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "size": 4096, "digest": "sha256:b3"},
            ],
        });
        assert_eq!(
            sum_manifest_layers(m.to_string().as_bytes()),
            Some(1784 + 8192 + 16384 + 4096)
        );
        // index（manifest list，无 layers）→ None
        let idx = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{"digest": "sha256:ccc", "size": 528}]});
        assert_eq!(sum_manifest_layers(idx.to_string().as_bytes()), None);
        // 畸形 JSON / 空 / 层缺 size → None
        assert_eq!(sum_manifest_layers(b"not json"), None);
        assert_eq!(sum_manifest_layers(b""), None);
        let bad = json!({"config": {"size": 1}, "layers": [{"digest": "sha256:x"}]});
        assert_eq!(sum_manifest_layers(bad.to_string().as_bytes()), None);
    }

    // 9. /proc/meminfo MemAvailable 行解析样例
    #[test]
    fn meminfo_parse() {
        let sample = "\
MemTotal:       32613828 kB
MemFree:         4319068 kB
MemAvailable:   18472896 kB
Buffers:          523264 kB
Cached:         13121516 kB
";
        assert_eq!(parse_meminfo_available_kb(sample), Some(18_472_896));
        assert_eq!(parse_meminfo_available_kb("MemAvailable:  1024 kB\n"), Some(1024));
        // 缺行 / 无数字 / 非数字 / 相近前缀（MemAvailableX 不匹配）→ None
        assert_eq!(parse_meminfo_available_kb("MemTotal: 100 kB\n"), None);
        assert_eq!(parse_meminfo_available_kb("MemAvailable:\n"), None);
        assert_eq!(parse_meminfo_available_kb("MemAvailable: abc kB\n"), None);
        assert_eq!(parse_meminfo_available_kb("MemAvailableX: 1 kB\n"), None);
        assert_eq!(parse_meminfo_available_kb(""), None);
    }

    // 10. Redis 浏览白名单（§10.3）：db / q / key 边界 + 值 4KB 截断
    #[test]
    fn redis_browse_validation() {
        // db：cap 内放行；≥cap / 负数 / 非数字 / 空 → E_NAME（cap=policy.redis_pool_cap，§13.3）
        assert_eq!(parse_redis_db("0", 16).unwrap(), 0);
        assert_eq!(parse_redis_db("15", 16).unwrap(), 15, "16 池上边界");
        for bad in ["16", "99", "-1", "abc", "", "1.5", " 1"] {
            assert_eq!(parse_redis_db(bad, 16).unwrap_err().code, "E_NAME", "{bad:?}");
        }
        // 默认 256 池：255 放行、256 拒（u8 承载上界）
        assert_eq!(parse_redis_db("255", 256).unwrap(), 255);
        assert_eq!(parse_redis_db("256", 256).unwrap_err().code, "E_NAME");
        // 超大 cap 也收在 256（防 u8 溢出）
        assert_eq!(parse_redis_db("255", 1000).unwrap(), 255);
        assert_eq!(parse_redis_db("256", 1000).unwrap_err().code, "E_NAME");

        // q → pattern：空 → "*"；白名单内补尾部 *；白名单外 → None
        assert_eq!(redis_scan_pattern("").as_deref(), Some("*"));
        assert_eq!(redis_scan_pattern("demo").as_deref(), Some("demo*"));
        assert_eq!(redis_scan_pattern("sr_demo:next").as_deref(), Some("sr_demo:next*"));
        assert_eq!(redis_scan_pattern("a.b-c_d*e").as_deref(), Some("a.b-c_d*e*"));
        for bad in ["a b", "a$b", "a;b", "a|b", "a\nb", "%", "héllo", "a'b", "a\"b", "a\\b"] {
            assert_eq!(redis_scan_pattern(bad), None, "{bad:?} 应拒绝");
        }

        // key：同字符集且非空
        for good in ["k", "sr_demo_next:field", "a.b-c_d*e", "0123"] {
            assert!(valid_redis_key(good), "{good} 应合法");
        }
        for bad in ["", "a b", "a$b", "k;SCAN", "中", "a\n"] {
            assert!(!valid_redis_key(bad), "{bad:?} 应拒绝");
        }

        // 值截断：不超原样；超 4KB 按 UTF-8 边界截断加标记（中文不被切半）
        assert_eq!(clip_value("short"), "short");
        let long = "a".repeat(REDIS_VALUE_MAX);
        assert_eq!(clip_value(&long), long);
        let over = "a".repeat(REDIS_VALUE_MAX + 10);
        let clipped = clip_value(&over);
        assert_eq!(clipped.len(), REDIS_VALUE_MAX + "...(截断)".len());
        assert!(clipped.ends_with("...(截断)"));
        let cjk: String = "中".repeat(REDIS_VALUE_MAX / 3 + 5); // 3 字节/字，必切在中间
        let clipped_cjk = clip_value(&cjk);
        assert!(clipped_cjk.ends_with("...(截断)"));
        assert!(clipped_cjk.len() < REDIS_VALUE_MAX + 15, "多字节按边界截短");
        assert_eq!(clipped_cjk.matches('中').count() * 3 + "...(截断)".len(), clipped_cjk.len());
    }

    // 10b. 镜像列表纯函数（§13.1）：user 前缀过滤 / admin 全量 / repo 与 tag 上限
    #[test]
    fn images_flatten_and_filter() {
        // catalog 样例：repo 名典序（registry:2 catalog 实际顺序）× 各自 tags
        let cat = |v: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
            v.iter()
                .map(|(r, ts)| (r.to_string(), ts.iter().map(|t| t.to_string()).collect()))
                .collect()
        };
        let catalog = cat(&[
            ("alice/api", &["v1"]),
            ("alice/web", &["v1", "v2"]),
            ("bob/web", &["v1"]),
            ("bare", &["latest"]), // 无 user 前缀 repo：任何用户过滤都不可见（admin 可见）
        ]);

        // admin（None）：全量展平 "repo:tag"
        assert_eq!(
            flatten_images(&catalog, None),
            ["alice/api:v1", "alice/web:v1", "alice/web:v2", "bob/web:v1", "bare:latest"]
                .map(String::from)
                .to_vec()
        );
        // alice：仅 alice/ 命名空间
        assert_eq!(
            flatten_images(&catalog, Some("alice")),
            ["alice/api:v1", "alice/web:v1", "alice/web:v2"].map(String::from).to_vec()
        );
        assert_eq!(flatten_images(&catalog, Some("bob")), ["bob/web:v1".to_string()]);
        assert!(flatten_images(&catalog, Some("nobody")).is_empty());
        // 前缀不误吞："ali" 不匹配 "alice/web"（split 精确比对首段）
        assert!(flatten_images(&catalog, Some("ali")).is_empty());
        assert!(repo_owned_by("alice/web", "alice") && !repo_owned_by("alice-x/y", "alice"));
        assert!(!repo_owned_by("bare", "alice"), "无命名空间 repo 不属于任何人");
        // 空 catalog / 空 tags
        assert!(flatten_images(&[], None).is_empty());
        assert!(flatten_images(&cat(&[("alice/x", &[])]), Some("alice")).is_empty());

        // 上限：repo > IMAGES_MAX_REPOS 只取前 500 个；tag 总量 > IMAGES_MAX_TAGS 截到 500
        let many_repos: Vec<(String, Vec<String>)> = (0..IMAGES_MAX_REPOS + 10)
            .map(|i| (format!("alice/r{i}"), vec!["v1".to_string()]))
            .collect();
        let out = flatten_images(&many_repos, Some("alice"));
        assert_eq!(out.len(), IMAGES_MAX_REPOS);
        assert_eq!(out.last().unwrap(), "alice/r499:v1");

        let many_tags = ("alice/big".to_string(), (0..IMAGES_MAX_TAGS + 10).map(|i| format!("t{i}")).collect::<Vec<_>>());
        let out2 = flatten_images(&[many_tags], None);
        assert_eq!(out2.len(), IMAGES_MAX_TAGS);
        assert_eq!(out2.last().unwrap(), "alice/big:t499");
        // 别人的 repo 不计入自己的 repo 上限（过滤先行）
        let mut mixed: Vec<(String, Vec<String>)> = (0..IMAGES_MAX_REPOS + 5)
            .map(|i| (format!("bob/r{i}"), vec!["v1".to_string()]))
            .collect();
        mixed.push(("alice/x".to_string(), vec!["v1".to_string()]));
        assert_eq!(flatten_images(&mixed, Some("alice")), ["alice/x:v1".to_string()]);
    }

    // 11. admin status 纯解析（SPEC §12.2）：df / docker ps / list-timers 样例文本
    #[test]
    fn admin_status_parsers() {
        // df -B1 --output=size,used：表头行 + 数据行（多行时取末行；1B 块无千分位逗号）
        let df = "    1B-blocks          Used\n3935730991104 3102217924608\n";
        assert_eq!(parse_df_bytes(df), Some((3_935_730_991_104, 3_102_217_924_608)));
        assert_eq!(parse_df_bytes("1B-blocks  Used\n1024 512\n2048 1536\n"), Some((2048, 1536)));
        // §14 size,avail 口径：两列白名单与列名无关，第二列即 avail → (size, avail)
        let dfa = "     1B-blocks         Avail\n3935730991104 643125334016\n";
        assert_eq!(parse_df_bytes(dfa), Some((3_935_730_991_104, 643_125_334_016)));
        assert_eq!(parse_df_bytes(""), None);
        assert_eq!(parse_df_bytes("only header\n"), None, "无数据行 → None");
        assert_eq!(parse_df_bytes("abc def\n"), None, "非数字 → None");
        assert_eq!(parse_df_bytes("1024\n"), None, "缺列 → None");

        // docker ps --format '{{.Names}}\t{{.State}}'
        let ps = "sr-demo\trunning\nsr-postgres\texited\n";
        assert_eq!(
            parse_container_lines(ps),
            vec![("sr-demo".into(), "running".into()), ("sr-postgres".into(), "exited".into())]
        );
        assert!(parse_container_lines("").is_empty());
        assert_eq!(parse_container_lines("garbage-no-tab\n"), vec![], "无 tab 的行跳过");

        // list-timers：NEXT 4 token（含时区）、LEFT/PASSED 含空格、.service 不误当 timer 名
        let timers = "\
Sun 2026-10-11 01:11:21 +08      23s Sun 2026-10-11 00:11:24 +08    59min ago fwupd-refresh.timer fwupd-refresh.service
Mon 2026-10-12 00:00:00 +08  1h 32min Sun 2026-10-10 23:00:12 +08   2h 3min ago showroom-snapshot-hourly.timer showroom-snapshot-hourly.service
";
        assert_eq!(
            parse_list_timers(timers),
            vec![
                ("fwupd-refresh".into(), "Sun 2026-10-11 01:11:21 +08".into()),
                ("showroom-snapshot-hourly".into(), "Mon 2026-10-12 00:00:00 +08".into()),
            ]
        );
        // n/a（未排程）与空行/无 timer 行
        assert_eq!(
            parse_list_timers("n/a  — n/a  — x.timer x.service\n"),
            vec![("x".into(), "n/a".into())]
        );
        assert!(parse_list_timers("no timers here\n\n").is_empty());
    }

    // 12. kid 形状（防 admin 标记路径拼接穿越）+ mtime 读取
    #[test]
    fn kid_shape_and_mtime() {
        for good in ["ab12cd34", "01234567", "deadbeef"] {
            assert!(valid_kid(good), "{good}");
        }
        for bad in ["", "ab12cd3", "ab12cd345", "zzzzzzzz", "aG12cd34", "../demo", "a b"] {
            assert!(!valid_kid(bad), "{bad}");
        }
        let root = troot("mtime");
        let p = root.join("k");
        fs::write(&p, "x").unwrap();
        assert!(file_mtime(&p) >= 1_700_000_000, "新文件 mtime 是近年时间戳");
        assert_eq!(file_mtime(&root.join("nope")), 0);
    }

    // 13. user_keys_with_kid：kid 由公钥内容派生；坏 .pub 跳过；无 user 空
    #[test]
    fn user_keys_with_kid_pairs() {
        let root = troot("kids");
        let k = showroom_common::UserKey::generate();
        let dir = tokens_dir(&root, "alice");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("whatever.pub"), format!("{}\n", k.pub_string())).unwrap();
        fs::write(dir.join("broken.pub"), "not a key\n").unwrap();
        let want = kid_of(&showroom_common::UserKey::from_pub_string(&k.pub_string()).unwrap());
        let got = user_keys_with_kid(&root, "alice");
        assert_eq!(got.len(), 1, "坏 .pub 跳过: {got:?}");
        assert_eq!(got[0].0, want);
        assert!(user_keys_with_kid(&root, "nobody").is_empty());
    }
}
