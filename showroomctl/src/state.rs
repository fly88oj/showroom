//! state 层：/srv/showroom/state 的全部读写（SPEC §5）+ 全部纯函数
//! （名字校验 §4 / policy 解析 / conf·env 序列化 / 端口与 redisdb 分配 / caddy 片段生成）。
//! 不做任何 subprocess——外部世界一律走 dockerops。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use showroom_common::{ed25519_dalek::VerifyingKey, sha256_hex, OwnerKey};

pub const PORT_MIN: u16 = 8100;
pub const PORT_MAX: u16 = 8199;
pub const REDIS_DBS: u8 = 16;

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

// —— 名字（SPEC §4；实现收口在 common::names）——

/// project：正则 + 保留名；slot/owner 等其余名字仅正则（保留名只约束 project）。
pub fn check_name(kind: &str, s: &str) -> R<()> {
    let r = if kind == "project" {
        showroom_common::names::check_project(s)
    } else {
        showroom_common::names::check_slot(s)
    };
    r.map_err(|e| ApiError::new("E_NAME", format!("{kind}: {e}")))
}

// —— 路径（SPEC §5）——

pub fn projects_dir(root: &Path) -> PathBuf {
    root.join("state/projects")
}
pub fn conf_path(root: &Path, p: &str, s: &str) -> PathBuf {
    projects_dir(root).join(p).join(format!("{s}.conf"))
}
pub fn env_path(root: &Path, p: &str, s: &str) -> PathBuf {
    projects_dir(root).join(p).join(format!("{s}.env"))
}
pub fn last2_path(root: &Path, p: &str, s: &str) -> PathBuf {
    projects_dir(root).join(p).join(format!("{s}.last2"))
}
pub fn routes_dir(root: &Path) -> PathBuf {
    root.join("state/routes")
}
pub fn route_path(root: &Path, p: &str, s: &str) -> PathBuf {
    routes_dir(root).join(format!("{p}-{s}.caddy"))
}
pub fn tokens_dir(root: &Path, owner: &str) -> PathBuf {
    root.join("state/tokens").join(owner)
}
pub fn ports_reg(root: &Path) -> PathBuf {
    root.join("state/ports.reg")
}
pub fn htpasswd_path(root: &Path) -> PathBuf {
    root.join("state/registry.htpasswd")
}

// —— slot.conf：key=value（owner tag port cport mem cpu redisdb updated [stopped]）——

#[derive(Debug, Clone, PartialEq)]
pub struct SlotConf {
    pub owner: String,
    pub tag: String,
    pub port: u16,
    pub cport: u16,
    pub mem: String,
    pub cpu: f64,
    pub redisdb: u8,
    pub updated: u64,
    pub stopped: bool,
}

impl SlotConf {
    pub fn to_conf(&self) -> String {
        let mut s = format!(
            "owner={}\ntag={}\nport={}\ncport={}\nmem={}\ncpu={}\nredisdb={}\nupdated={}\n",
            self.owner, self.tag, self.port, self.cport, self.mem, self.cpu, self.redisdb, self.updated
        );
        if self.stopped {
            s.push_str("stopped=1\n");
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
        Ok(SlotConf {
            owner: need("owner")?,
            tag: need("tag")?,
            port: need("port")?.parse().map_err(|e| ApiError::state(format!("conf port: {e}")))?,
            cport: need("cport")?.parse().map_err(|e| ApiError::state(format!("conf cport: {e}")))?,
            mem: need("mem")?,
            cpu: need("cpu")?.parse().map_err(|e| ApiError::state(format!("conf cpu: {e}")))?,
            redisdb: need("redisdb")?.parse().map_err(|e| ApiError::state(format!("conf redisdb: {e}")))?,
            updated: need("updated")?.parse().map_err(|e| ApiError::state(format!("conf updated: {e}")))?,
            stopped: map.get("stopped").map(|v| v == "1").unwrap_or(false),
        })
    }

    pub fn load(root: &Path, p: &str, s: &str) -> R<Self> {
        Self::parse(&fs::read_to_string(conf_path(root, p, s))?)
    }

    pub fn save(&self, root: &Path, p: &str, s: &str) -> R<()> {
        let path = conf_path(root, p, s);
        fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
        fs::write(&path, self.to_conf())?;
        Ok(())
    }

    /// ps / GET /_api/projects 共用的列表行（SPEC §3 projects 响应字段）。
    pub fn to_json(&self, p: &str, s: &str, status: &str) -> Value {
        json!({
            "project": p, "slot": s, "owner": self.owner, "tag": self.tag,
            "url": public_url(p, s), "status": status,
            "port": self.port, "mem": self.mem, "cpu": self.cpu, "updated": self.updated,
        })
    }
}

pub fn read_last2(root: &Path, p: &str, s: &str) -> Option<String> {
    fs::read_to_string(last2_path(root, p, s))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

pub fn all_confs(root: &Path) -> Vec<(String, String, SlotConf)> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(projects_dir(root)) else {
        return out;
    };
    let mut projs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    projs.sort();
    for pd in projs {
        let Some(pname) = pd.file_name().and_then(|o| o.to_str()).map(str::to_string) else {
            continue;
        };
        let Ok(rd) = fs::read_dir(&pd) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.extension().and_then(|o| o.to_str()) == Some("conf") {
                if let Some(stem) = path.file_stem().and_then(|o| o.to_str()) {
                    if let Ok(c) = SlotConf::load(root, &pname, stem) {
                        out.push((pname.clone(), stem.to_string(), c));
                    }
                }
            }
        }
    }
    out
}

// —— KEY=VALUE 解析（policy.env / slot.env 通用，几行）——

pub fn parse_kv(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

// —— 环境变量契约（SPEC §6，值全部具体化；SPEC §9 库名归一化）——

/// 归一化库名/角色名（SPEC §9）：`sr_` + project/slot 中 `-`→`_`
/// （psql -d、CREATE、env 的 DB_NAME/DB_USER/DATABASE_URL 一律用它）。
pub fn db_ident(p: &str, s: &str) -> String {
    format!("sr_{}_{}", p.replace('-', "_"), s.replace('-', "_"))
}

/// v1.2 前旧命名（连字符原样保留）。仅用于 legacy 存量库的存在性查找。
pub fn legacy_db_ident(p: &str, s: &str) -> String {
    format!("sr_{p}_{s}")
}

/// 库名解析（纯函数，存在性由调用方喂 dockerops::db_exists 的结果）：
/// 新名已存在 → 新名；仅旧名存在 → 沿用旧名（存量库直到 db-reset 重建为新名）；皆无 → 新名（首建路径）。
pub fn resolve_db_ident(p: &str, s: &str, new_exists: bool, legacy_exists: bool) -> String {
    if new_exists || !legacy_exists {
        db_ident(p, s)
    } else {
        legacy_db_ident(p, s)
    }
}

/// stable 槽位路径不带 slot（SPEC §5 URL 规则）。
pub fn base_path(p: &str, s: &str) -> String {
    if s == "stable" {
        format!("/{p}/")
    } else {
        format!("/{p}/{s}/")
    }
}

pub fn public_url(p: &str, s: &str) -> String {
    let base = std::env::var("SHOWROOM_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".into());
    let bp = base_path(p, s);
    format!("{}/{}", base.trim_end_matches('/'), bp.trim_start_matches('/'))
}

/// env 文件内容（db 传**已解析**的库名：归一化新名，或沿用的 legacy 旧名）。
pub fn env_file_contents(p: &str, s: &str, db: &str, db_pass: &str, redis_db: u8) -> String {
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
         SHOWROOM_SLOT={s}\n\
         SHOWROOM_BASE_PATH={bp}\n",
        bp = base_path(p, s),
    )
}

// —— policy.env（SPEC §5 默认值）——

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub max_slots: u32,
    pub default_mem: String,
    pub max_mem: String,
    pub import_max_gb: u64,
    pub allow_lan_port: bool,
    /// 内存准入预留（§10.2，MB）：MemAvailable 减去它之后须仍满足本次发布的需求。
    pub reserve_mem_mb: u64,
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
        }
    }

    pub fn import_cap_bytes(&self) -> u64 {
        self.import_max_gb << 30
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

// —— ports.reg：每行 <port> <project> <slot>，槽位端口 8100–8199 ——

pub fn read_ports(root: &Path) -> Vec<(u16, String, String)> {
    fs::read_to_string(ports_reg(root))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next()?.parse().ok(), it.next(), it.next()) {
                (Some(p), Some(pj), Some(sl)) => Some((p, pj.to_string(), sl.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// 找空闲端口（不落账；publish 在 docker run 成功后 register）。
/// ponytail: 无并发锁——本机唯一管理通道（systemd serve + 罕见管理员 CLI），不会并发分配。
pub fn alloc_port(root: &Path) -> R<u16> {
    let used: BTreeSet<u16> = read_ports(root).into_iter().map(|(p, _, _)| p).collect();
    (PORT_MIN..=PORT_MAX)
        .find(|p| !used.contains(p))
        .ok_or_else(|| ApiError::new("E_QUOTA", "no free port in 8100-8199"))
}

pub fn register_port(root: &Path, port: u16, project: &str, slot: &str) -> R<()> {
    let suffix = format!(" {project} {slot}");
    let mut lines: Vec<String> = fs::read_to_string(ports_reg(root))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.ends_with(&suffix))
        .map(str::to_string)
        .collect();
    lines.push(format!("{port} {project} {slot}"));
    fs::create_dir_all(root.join("state"))?;
    fs::write(ports_reg(root), lines.join("\n") + "\n")?;
    Ok(())
}

pub fn release_port(root: &Path, project: &str, slot: &str) -> R<()> {
    let suffix = format!(" {project} {slot}");
    let lines: Vec<String> = fs::read_to_string(ports_reg(root))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.ends_with(&suffix))
        .map(str::to_string)
        .collect();
    fs::write(ports_reg(root), lines.join("\n") + "\n")?;
    Ok(())
}

/// redis 逻辑 DB 0-15：扫现有 conf 找空闲；占满 → E_QUOTA。
pub fn alloc_redisdb(root: &Path) -> R<u8> {
    let used: BTreeSet<u8> = all_confs(root).into_iter().map(|(_, _, c)| c.redisdb).collect();
    (0..REDIS_DBS)
        .find(|d| !used.contains(d))
        .ok_or_else(|| ApiError::new("E_QUOTA", "no free redis db in 0-15"))
}

// —— Redis 浏览纯函数（SPEC §10.3）——

/// db 参数：数字 0-15（REDIS_DBS-1），否则 E_NAME。
pub fn parse_redis_db(s: &str) -> R<u8> {
    s.parse::<u8>()
        .ok()
        .filter(|d| *d < REDIS_DBS)
        .ok_or_else(|| ApiError::new("E_NAME", format!("db 须为 0-{} 的数字", REDIS_DBS - 1)))
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

// —— 发布者公钥 / registry 凭据文件 ——

pub fn kid_of(vk: &VerifyingKey) -> String {
    sha256_hex(&vk.to_bytes())[..8].to_string()
}

pub fn owner_pubkeys(root: &Path, owner: &str) -> Vec<VerifyingKey> {
    let dir = tokens_dir(root, owner);
    let Ok(rd) = fs::read_dir(&dir) else { return vec![] };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|o| o.to_str()) == Some("pub"))
        .filter_map(|p| fs::read_to_string(p).ok())
        .filter_map(|s| OwnerKey::from_pub_string(s.trim()).ok())
        .collect()
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

// —— caddy 路由片段：文件名 <project>-<slot>.caddy；stable 的匹配路径不带 slot（SPEC §5）——

pub fn caddy_route(project: &str, slot: &str, port: u16) -> String {
    let base = if slot == "stable" {
        format!("/{project}")
    } else {
        format!("/{project}/{slot}")
    };
    // caddy matcher 名只允许字母数字下划线：连字符换下划线
    let m = format!("sr_{project}_{slot}").replace('-', "_");
    // handle_path 自动剥掉 {base} 前缀再反代（容器收到的是 / 起始路径）
    format!(
        "@{m}_nos path {base}\n\
         redir @{m}_nos {base}/ 308\n\
         handle_path {base}/* {{\n\
           reverse_proxy 127.0.0.1:{port}\n\
         }}\n"
    )
}

// —— _tmp 陈旧会话筛选（SPEC §8.1：>24h 由 gc-images 顺带删）——

pub const TMP_STALE_SECS: u64 = 24 * 3600;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn troot(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("srctl-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    // 1. 名字校验（转发 common::names）：正则 / 保留名只约束 project
    #[test]
    fn names() {
        for good in ["abc", "a-b2", "0", "a", &"a".repeat(31), "pr-42-x1"] {
            assert!(check_name("project", good).is_ok(), "{good} should be valid");
        }
        for bad in ["", "Abc", "aBc", "-a", "a_", "a b", "中文", &"a".repeat(32), "a.b", "a/b"] {
            assert!(check_name("slot", bad).is_err(), "{bad} should be invalid");
        }
        // 保留名：project 拒、slot 放行（SPEC §4 统一语义；_api 等本就不合正则，分歧点在 v2）
        for r in ["v2", "_api", "_bootstrap", "_console"] {
            assert!(check_name("project", r).is_err(), "{r} reserved for project");
        }
        assert!(check_name("slot", "v2").is_ok(), "slot not bound by reserved names");
        assert_eq!(check_name("project", "v2").unwrap_err().code, "E_NAME");
    }

    // 1b. SlotConf::to_json（ps 与 /_api/projects 共用行）
    #[test]
    fn conf_to_json() {
        let c = SlotConf {
            owner: "alice".into(),
            tag: "v1.2".into(),
            port: 8123,
            cport: 3000,
            mem: "256m".into(),
            cpu: 1.5,
            redisdb: 7,
            updated: 1700000000,
            stopped: false,
        };
        let v = c.to_json("demo", "pr42", "running");
        assert_eq!(v["project"], "demo");
        assert_eq!(v["slot"], "pr42");
        assert_eq!(v["owner"], "alice");
        assert_eq!(v["tag"], "v1.2");
        assert_eq!(v["status"], "running");
        assert_eq!(v["url"], public_url("demo", "pr42"));
        assert_eq!(v["port"], 8123);
        assert_eq!(v["mem"], "256m");
        assert_eq!(v["cpu"], 1.5);
        assert_eq!(v["updated"], 1700000000);
    }

    // 2a. conf 序列化往返（§5 全字段，含/不含 stopped）
    #[test]
    fn conf_roundtrip() {
        let c = SlotConf {
            owner: "alice".into(),
            tag: "v1.2".into(),
            port: 8123,
            cport: 3000,
            mem: "256m".into(),
            cpu: 1.5,
            redisdb: 7,
            updated: 1700000000,
            stopped: true,
        };
        let t = c.to_conf();
        assert!(t.contains("stopped=1\n"));
        assert_eq!(SlotConf::parse(&t).unwrap(), c);
        let mut c2 = c.clone();
        c2.stopped = false;
        assert!(!c2.to_conf().contains("stopped"));
        assert_eq!(SlotConf::parse(&c2.to_conf()).unwrap(), c2);
        // 整数 cpu 往返
        let mut c3 = c.clone();
        c3.cpu = 1.0;
        assert_eq!(SlotConf::parse(&c3.to_conf()).unwrap(), c3);
    }

    // 2b. env 文件往返（§6 全字段具体化；stable 与非 stable 的 BASE_PATH）
    #[test]
    fn env_file_roundtrip() {
        let txt = env_file_contents("demo", "next", &db_ident("demo", "next"), "s3cret", 3);
        let m = parse_kv(&txt);
        assert_eq!(m["DB_HOST"], "sr-postgres");
        assert_eq!(m["DB_PORT"], "5432");
        assert_eq!(m["DB_NAME"], "sr_demo_next");
        assert_eq!(m["DB_USER"], "sr_demo_next");
        assert_eq!(m["DB_PASSWORD"], "s3cret");
        assert_eq!(
            m["DATABASE_URL"],
            "postgresql://sr_demo_next:s3cret@sr-postgres:5432/sr_demo_next"
        );
        assert_eq!(m["REDIS_HOST"], "sr-redis");
        assert_eq!(m["REDIS_PORT"], "6379");
        assert_eq!(m["REDIS_DB"], "3");
        assert_eq!(m["REDIS_URL"], "redis://sr-redis:6379/3");
        assert_eq!(m["SHOWROOM_PROJECT"], "demo");
        assert_eq!(m["SHOWROOM_SLOT"], "next");
        assert_eq!(m["SHOWROOM_BASE_PATH"], "/demo/next/");

        let stable = parse_kv(&env_file_contents("demo", "stable", &db_ident("demo", "stable"), "x", 0));
        assert_eq!(stable["SHOWROOM_BASE_PATH"], "/demo/");
        assert_eq!(stable["DB_NAME"], "sr_demo_stable");
        // legacy 库名沿用时 env 同样指向旧名（§9 归一化过渡期）
        let lg = parse_kv(&env_file_contents("skill-demo", "stable", &legacy_db_ident("skill-demo", "stable"), "pw", 1));
        assert_eq!(lg["DB_NAME"], "sr_skill-demo_stable");
        assert_eq!(lg["DATABASE_URL"], "postgresql://sr_skill-demo_stable:pw@sr-postgres:5432/sr_skill-demo_stable");
        for k in [
            "DB_HOST", "DB_PORT", "DB_NAME", "DB_USER", "DB_PASSWORD", "DATABASE_URL",
            "REDIS_HOST", "REDIS_PORT", "REDIS_DB", "REDIS_URL", "SHOWROOM_PROJECT",
            "SHOWROOM_SLOT", "SHOWROOM_BASE_PATH",
        ] {
            assert!(stable.contains_key(k), "missing {k}");
        }
    }

    // 2c. 库名归一化 + legacy 查找（§9：纯函数，存在性由调用方喂）
    #[test]
    fn db_ident_normalization_and_legacy() {
        assert_eq!(db_ident("demo", "stable"), "sr_demo_stable");
        assert_eq!(db_ident("skill-demo", "pr-42"), "sr_skill_demo_pr_42");
        assert_eq!(legacy_db_ident("skill-demo", "stable"), "sr_skill-demo_stable");
        assert_eq!(db_ident("demo", "stable"), legacy_db_ident("demo", "stable"), "无连字符时新旧同形");

        // 解析：新名在 → 新名（两者并存时新名优先）；仅旧名在 → 沿用旧名；皆无 → 新名（首建）
        assert_eq!(resolve_db_ident("skill-demo", "stable", true, false), "sr_skill_demo_stable");
        assert_eq!(resolve_db_ident("skill-demo", "stable", true, true), "sr_skill_demo_stable");
        assert_eq!(resolve_db_ident("skill-demo", "stable", false, true), "sr_skill-demo_stable");
        assert_eq!(resolve_db_ident("skill-demo", "stable", false, false), "sr_skill_demo_stable");
        assert_eq!(resolve_db_ident("demo", "stable", false, true), "sr_demo_stable", "无连字符新旧同形");
    }

    // 3. caddy 片段：stable 与非 stable，路径正确含 308 重定向行
    #[test]
    fn caddy_route_snippets() {
        let s = caddy_route("demo", "stable", 8100);
        assert!(s.contains("@sr_demo_stable_nos path /demo\n"));
        assert!(s.contains("redir @sr_demo_stable_nos /demo/ 308\n"));
        assert!(s.contains("handle_path /demo/* {\n"));
        assert!(s.contains("reverse_proxy 127.0.0.1:8100\n"));

        let n = caddy_route("demo", "pr42", 8101);
        assert!(n.contains("@sr_demo_pr42_nos path /demo/pr42\n"));
        assert!(n.contains("redir @sr_demo_pr42_nos /demo/pr42/ 308\n"));
        assert!(n.contains("handle_path /demo/pr42/* {\n"));
        assert!(n.contains("reverse_proxy 127.0.0.1:8101\n"));
    }

    // 4. policy 解析与限额判断
    #[test]
    fn policy_parse_and_limits() {
        let root = troot("policy");
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(
            root.join("state/policy.env"),
            "MAX_SLOTS=5\nDEFAULT_MEM=128m\nMAX_MEM=512m\nIMPORT_MAX_GB=2\nALLOW_LAN_PORT=1\nRESERVE_MEM_MB=512\n",
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
            ),
            (5, "128m", "512m", 2, true, 512)
        );
        assert_eq!(p.import_cap_bytes(), 2 << 30);

        let d = Policy::load(&troot("policy-default"));
        assert_eq!(
            (
                d.max_slots,
                d.default_mem.as_str(),
                d.max_mem.as_str(),
                d.import_max_gb,
                d.allow_lan_port,
                d.reserve_mem_mb,
            ),
            (30, "256m", "1g", 8, false, 1536)
        );

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

    // 5. 端口分配：顺序分配、幂等 upsert、占满 8100-8199 → E_QUOTA、释放后可复用
    #[test]
    fn port_alloc_and_quota() {
        let root = troot("ports");
        fs::create_dir_all(root.join("state")).unwrap();
        assert_eq!(alloc_port(&root).unwrap(), PORT_MIN);

        register_port(&root, 8100, "a", "stable").unwrap();
        register_port(&root, 8101, "b", "stable").unwrap();
        assert_eq!(alloc_port(&root).unwrap(), 8102);

        register_port(&root, 8100, "a", "stable").unwrap(); // 幂等
        assert_eq!(read_ports(&root).len(), 2);

        for p in PORT_MIN..=PORT_MAX {
            register_port(&root, p, "x", &format!("s{p}")).unwrap();
        }
        assert_eq!(alloc_port(&root).unwrap_err().code, "E_QUOTA");

        release_port(&root, "x", "s8150").unwrap();
        assert_eq!(alloc_port(&root).unwrap(), 8150);
    }

    // 附带：redisdb 分配（conf 扫描）
    #[test]
    fn redisdb_alloc() {
        let root = troot("redis");
        let base = SlotConf {
            owner: "o".into(), tag: "t".into(), port: 8100, cport: 80, mem: "256m".into(),
            cpu: 1.0, redisdb: 0, updated: 1, stopped: false,
        };
        base.save(&root, "demo", "stable").unwrap();
        let mut n = base.clone();
        n.redisdb = 1;
        n.save(&root, "demo", "next").unwrap();
        assert_eq!(alloc_redisdb(&root).unwrap(), 2);
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
        // db：0 与 15 放行；16 / 负数 / 非数字 / 空 → E_NAME
        assert_eq!(parse_redis_db("0").unwrap(), 0);
        assert_eq!(parse_redis_db("15").unwrap(), 15);
        for bad in ["16", "99", "-1", "abc", "", "1.5", " 1"] {
            assert_eq!(parse_redis_db(bad).unwrap_err().code, "E_NAME", "{bad:?}");
        }

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
}
