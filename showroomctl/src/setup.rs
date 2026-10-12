//! setup：声明式期望状态（plan-15 §2.2 / SPEC §15.1）——observe 收集现状、
//! plan_setup 纯函数出 Action、apply 逐项执行；渲染产物（policy/Caddyfile/单元/脚本）常量内嵌。
//! 纯函数（渲染/推导/幂等判定）与 subprocess 薄层（observe/apply）分区放置。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::dockerops;
use crate::middleware::{host_env_path, FrpMode, HostEnv};
use crate::state::{self, ApiError, R};

// —— policy.env 渲染（plan-15 §1：setup 负责缺省生成——缺键补默认、已有键一律保留）——

/// policy 键表（§13.3/§14.4 + 存量键）。RESERVE_MEM_MB/DISK_RESERVE_GB 首跑缺省
/// 可被 hw_defaults 覆盖（见 render_policy_hw）。
pub const POLICY_KEYS: [(&str, &str); 9] = [
    ("MAX_SLOTS", "30"),
    ("DEFAULT_MEM", "256m"),
    ("MAX_MEM", "1g"),
    ("IMPORT_MAX_GB", "8"),
    ("ALLOW_LAN_PORT", "0"),
    ("RESERVE_MEM_MB", "1536"),
    ("REDIS_DBS", "256"),
    ("DISK_RESERVE_GB", "20"),
    ("UPLOAD_SPACE_FACTOR", "4"),
];

/// 固定缺省渲染（plan-15 §2.2 签名）：已有键 verbatim 保留，缺键补默认，键序固定（幂等写盘）。
pub fn render_policy(existing: Option<&str>) -> String {
    render_policy_hw(existing, None)
}

/// hw 自适应渲染（补充需求：首跑按实测硬件推导缺省）：
/// 仅当**文件缺失**（existing=None，整文件首生成）时用 hw 值补 RESERVE_MEM_MB/DISK_RESERVE_GB；
/// 已有文件一律保留既有键、缺键补固定缺省（用户显式值永远优先）。
pub fn render_policy_hw(existing: Option<&str>, hw: Option<&HwTuning>) -> String {
    let m = existing.map(state::parse_kv).unwrap_or_default();
    let hw = if existing.is_some() { None } else { hw };
    POLICY_KEYS
        .iter()
        .map(|(k, d)| {
            let v = m.get(*k).cloned().unwrap_or_else(|| match (*k, hw) {
                ("RESERVE_MEM_MB", Some(h)) => h.reserve_mem_mb.to_string(),
                ("DISK_RESERVE_GB", Some(h)) => h.disk_reserve_gb.to_string(),
                _ => d.to_string(),
            });
            format!("{k}={v}")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// —— 硬件自适应缺省（补充需求：首跑推导，纯函数可测）——

/// 首跑硬件推导产物：限额缺省 + 容器规格派生参数。
/// 规则（补充需求原文）：内存/磁盘百分比 + clamp；取整一律向下（保守侧）。
#[derive(Debug, Clone, PartialEq)]
pub struct HwTuning {
    /// clamp(总内存×15%, 1024, 6144) MB（12G≈1843；4G 兜底 1024）
    pub reserve_mem_mb: u64,
    /// clamp(总磁盘×3%, 10, 60) GB
    pub disk_reserve_gb: u64,
    /// clamp(总内存×12%, 64, 1GB) MB（向下取整）
    pub pg_shared_buffers_mb: u64,
    /// 总内存×35% MB
    pub pg_effective_cache_mb: u64,
    /// clamp(30+cores×10, 40, 120)
    pub pg_max_connections: u32,
    /// clamp(总内存×2%, 64, 512) MB
    pub redis_maxmemory_mb: u64,
}

fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    v.max(lo).min(hi)
}

/// 首跑硬件推导（纯函数；补充需求三档测试：4G 小机 / 12G 本机 / 64G 大机）。
pub fn hw_defaults(mem_mb: u64, disk_mb: u64, cores: u32) -> HwTuning {
    let disk_gb = disk_mb / 1024;
    HwTuning {
        reserve_mem_mb: clamp(mem_mb * 15 / 100, 1024, 6144),
        disk_reserve_gb: clamp(disk_gb * 3 / 100, 10, 60),
        pg_shared_buffers_mb: clamp(mem_mb * 12 / 100, 64, 1024),
        pg_effective_cache_mb: mem_mb * 35 / 100,
        pg_max_connections: (30 + cores.saturating_mul(10)).clamp(40, 120),
        redis_maxmemory_mb: clamp(mem_mb * 2 / 100, 64, 512),
    }
}

/// Obs 的硬件观测 → 推导（零/缺失观测走 clamp 下限，不 panic）。
pub fn hw_from_obs(obs: &Obs) -> HwTuning {
    hw_defaults(obs.mem_total_mb, obs.disk_total_mb, obs.cores.max(1))
}

/// 审计注释的硬件摘要：「12G/953G/4 核」（内存/磁盘向下取整 GB）。
pub fn hw_summary(mem_mb: u64, disk_mb: u64, cores: u32) -> String {
    format!("{}G/{}G/{} 核", mem_mb / 1024, disk_mb / 1024, cores)
}

/// unix 秒 → UTC `YYYY-MM-DD`（审计注释用；civil-from-days 算法，免引 chrono）。
pub fn date_label(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// —— 调优常量（补充需求②修正：sysctl 全表 / journald 400M+30d / watchdog）——

/// 内核调优（BBR+fq、连接队列、rmem/wmem、tw_reuse/fastopen/mtu_probing、vm 与 inotify）。
/// tcp_max_syn_backlog=4096（修正 plan 的 8192 笔误，按仓库调优清单键位全集）。
pub const SYSCTL_CONF: &str = "/etc/sysctl.d/99-showroom.conf";
pub const SYSCTL_CONTENTS: &str = "\
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 4096
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 87380 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216
net.ipv4.ip_local_port_range = 10240 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fastopen = 3
net.ipv4.tcp_mtu_probing = 1
net.ipv4.tcp_slow_start_after_idle = 0
vm.swappiness = 10
vm.vfs_cache_pressure = 50
fs.file-max = 131072
fs.inotify.max_user_instances = 512
fs.inotify.max_user_watches = 1048576
";

pub const JOURNALD_CONF: &str = "/etc/systemd/journald.conf.d/showroom.conf";
pub const JOURNALD_CONTENTS: &str = "[Journal]\nSystemMaxUse=400M\nMaxRetentionSec=30d\n";

pub const WATCHDOG_CONF: &str = "/etc/systemd/system.conf.d/showroom.conf";
pub const WATCHDOG_CONTENTS: &str = "[Manager]\nRuntimeWatchdogSec=10s\n";

/// btrfs 快照父卷（snapshot hourly/daily 惰性创建；setup 顺手收敛，plan-15 §2.2）。
pub const SNAPS_SUBVOL: &str = "/@showroom-snaps";

// —— 内嵌单元与脚本常量（deploy/ 模板 include_str；IP 遗留已清洗）——

pub const DEDUP_SCRIPT_PATH: &str = "/usr/local/bin/showroom-dedup";
pub const DEDUP_SCRIPT: &str = "\
#!/usr/bin/env bash
# showroom 周度 dedup（duperemove 增量 hashfile 模式；Nice=19 由单元保证）
set -euo pipefail
ROOT=\"${SHOWROOM_ROOT:-/srv/showroom}\"
exec duperemove -d --hashfile=\"$ROOT/state/dedup.hash\" \"$ROOT\"
";

pub const PRECOMPRESS_PATH: &str = "/usr/local/bin/showroom-precompress";
pub const PRECOMPRESS_SCRIPT: &str = "\
#!/usr/bin/env bash
# _console 静态资源 zstd+gzip 双预压缩（Caddy file_server precompressed 直接下发；代理链路 Accept-Encoding 常被收敛为 gzip）
# 递归覆盖 css/ js/ 等子目录——控制台拆分 ES Modules 后静态面是多文件树）
set -euo pipefail
DIR=\"${1:-${SHOWROOM_ROOT:-/srv/showroom}/_console}\"
cd \"$DIR\"
find . -type f ! -name '*.zst' ! -name '*.gz' -print0 | xargs -0 -r zstd -19 -k -q -f
find . -type f ! -name '*.zst' ! -name '*.gz' -print0 | xargs -0 -r gzip -9 -k -f
";

/// frpc 下载源（ChmlFrp linux/amd64；URL 可能回 HTML 壳——FrpcFetch 执行时 file 校验 ELF，
/// 非 ELF 报错并提示手工放置后重跑 setup，如实降级见 docs/deployment.md）。
pub const FRPC_URL: &str = "https://chmlfrp.cn/dw/linux/amd64/frpc";
pub const FRPC_BIN: &str = "/usr/local/frpc/frpc";

/// 恒装的 timer 对（plan-15 §2.2：reconcile/autoheal/gc-images/gc-registry/registry-gc/
/// snapshot-hourly/snapshot-daily；deploy/ 原文内嵌）。
pub const TIMER_UNITS: &[(&str, &str)] = &[
    ("showroom-reconcile.service", include_str!("../../deploy/showroom-reconcile.service")),
    ("showroom-reconcile.timer", include_str!("../../deploy/showroom-reconcile.timer")),
    ("showroom-autoheal.service", include_str!("../../deploy/showroom-autoheal.service")),
    ("showroom-autoheal.timer", include_str!("../../deploy/showroom-autoheal.timer")),
    ("showroom-gc-images.service", include_str!("../../deploy/showroom-gc-images.service")),
    ("showroom-gc-images.timer", include_str!("../../deploy/showroom-gc-images.timer")),
    ("showroom-gc-registry.service", include_str!("../../deploy/showroom-gc-registry.service")),
    ("showroom-gc-registry.timer", include_str!("../../deploy/showroom-gc-registry.timer")),
    ("showroom-registry-gc.service", include_str!("../../deploy/showroom-registry-gc.service")),
    ("showroom-registry-gc.timer", include_str!("../../deploy/showroom-registry-gc.timer")),
    ("showroom-snapshot-hourly.service", include_str!("../../deploy/showroom-snapshot-hourly.service")),
    ("showroom-snapshot-hourly.timer", include_str!("../../deploy/showroom-snapshot-hourly.timer")),
    ("showroom-snapshot-daily.service", include_str!("../../deploy/showroom-snapshot-daily.service")),
    ("showroom-snapshot-daily.timer", include_str!("../../deploy/showroom-snapshot-daily.timer")),
];

/// 恒启用的 timer（enable --now）。
pub const TIMER_ENABLE: &[&str] = &[
    "showroom-reconcile.timer",
    "showroom-autoheal.timer",
    "showroom-gc-images.timer",
    "showroom-gc-registry.timer",
    "showroom-registry-gc.timer",
    "showroom-snapshot-hourly.timer",
    "showroom-snapshot-daily.timer",
];

/// dedup 单元对（DEDUP=weekly 装启用；none → UnitDisableRm 收敛）。
pub const DEDUP_UNITS: &[(&str, &str)] = &[
    ("showroom-dedup.service", include_str!("../../deploy/showroom-dedup.service")),
    ("showroom-dedup.timer", include_str!("../../deploy/showroom-dedup.timer")),
];

const UNIT_API_TEMPLATE: &str = include_str!("../../deploy/showroom-api.service");

/// 全部单元名（observe 探测候选）。
pub fn all_unit_names() -> Vec<String> {
    let mut v = vec!["showroom-api.service".to_string()];
    for (n, _) in TIMER_UNITS {
        v.push(n.to_string());
    }
    for (n, _) in DEDUP_UNITS {
        v.push(n.to_string());
    }
    v.push("chmlfrp.service".to_string());
    v
}

pub fn unit_path(name: &str) -> PathBuf {
    Path::new("/etc/systemd/system").join(name)
}

/// api 单元渲染：deploy 模板 + SHOWROOM_BASE_URL 注入（host.base_url()；模板 IP 遗留已清洗）。
pub fn render_unit_api(host: &HostEnv) -> String {
    UNIT_API_TEMPLATE.replace(
        "EnvironmentFile=-/etc/showroom/showroom.env",
        &format!("Environment=SHOWROOM_BASE_URL={}", host.base_url()),
    )
}

/// chmlfrp 单元渲染（补充需求①）：凭据经 EnvironmentFile=host.env（0600）注入，
/// 不落明文进 644 的单元文件。
pub fn render_unit_chmlfrp(root: &Path) -> String {
    format!(
        "\
[Unit]
Description=ChmlFrp tunnel (frpc)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# 凭据在 host.env（0600，root）：EnvironmentFile 注入 FRP_TOKEN/FRP_TUNNEL
EnvironmentFile=-{env}
ExecStart={frpc} -u ${{FRP_TOKEN}} -p ${{FRP_TUNNEL}}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
",
        env = host_env_path(root).display(),
        frpc = FRPC_BIN,
    )
}

// —— Caddyfile 渲染（plan-15 §2.2；快照测试覆盖三组合）——

/// registry ∈ 集合才有 site A 的 /v2 handle；/_bootstrap、/_console、routes import 恒在；
/// PUBLIC_HOST 空 → 不渲染 site B（纯内网主机）。非空 → site B 用 :8080 纯端口匹配（frpc 转发流量 Host 是公网 IP，Host 匹配会漏）。
pub fn render_caddyfile(mw: &BTreeSet<String>, lan: &str, public: &str) -> String {
    let mut s = String::from(
        "# ————— 公共片段：两个站点共用 —————\n\
         (showroom-app) {\n\
         \t# 控制面：全部端点要求 SR-SIG1 签名（零信息暴露由应用层保证）\n\
         \thandle /_api/* {\n\
         \t\treverse_proxy 127.0.0.1:8090\n\
         \t}\n\
         \t# WASM 控制台（登录成功前不加载任何管理资源）\n\
         \thandle_path /_console/* {\n\
         \t\troot * /srv/showroom/_console\n\
         \t\tfile_server {\n\
         \t\t\tprecompressed zstd gzip\n\
         \t\t}\n\
         \t}\n\
         \t# 展项路由（showroomctl 按项目生成到 state/routes/<project>.caddy 并 reload）\n\
         \timport /srv/showroom/state/routes/*.caddy\n\
             \t# /_bootstrap：CLI / Skill 分发（R8：移入共用片段——公网隧道用户同样可下载，公开产物无泄露面）\n\
             \thandle_path /_bootstrap/* {\n\
             \t\troot * /srv/showroom/_bootstrap\n\
             \t\tfile_server\n\
             \t}\n\
         \t# 根路径与未匹配路径一律 404（零信息暴露：不重定向、不暴露站点信息）\n\
         }\n\
         \n\
         # ————— site A：内网面 —————\n",
    );
    s.push_str(&format!("http://{lan} {{\n\timport showroom-app\n"));
    if mw.contains("registry") {
        s.push_str(
            "\t# /v2：registry 推拉快通道（basic auth 由 registry 自身校验）\n\
             \thandle /v2/* {\n\
             \t\treverse_proxy 127.0.0.1:5000\n\
             \t}\n",
        );
    }
    s.push_str("}\n"); // R8：/_bootstrap 移入共用片段，site A 无私有块
    if !public.is_empty() {
        s.push_str(
            "\n# ————— site B：公网面（frp 等公网隧道流量命中的站点）—————\n\
             # 公网发布走加密 import 通道，不放 /v2。\n\
             http://:8080, :8081 {\n\
             \timport showroom-app\n\
             }\n",
        );
    }
    s
}

// —— 容器规格常量表（plan-15 §2.2；pg 12 个 -c 参数与 redis maxmemory 随 hw 推导）——

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// docker run 旗标（network/restart/env/volumes/ports；不含 -d 与 --name）
    pub flags: Vec<String>,
    /// 镜像后的命令参数（postgres -c … / redis --appendonly …）
    pub cmd_args: Vec<String>,
}

impl ContainerSpec {
    /// diff/报告的单行摘要（如 "redis:7-alpine --databases 256 --maxmemory 245mb …"）。
    pub fn detail(&self) -> String {
        if self.cmd_args.is_empty() {
            self.image.clone()
        } else {
            format!("{} {}", self.image, self.cmd_args.join(" "))
        }
    }

    /// docker run 完整参数（含 run -d --name）。
    pub fn run_args(&self) -> Vec<String> {
        let mut v = vec!["run".to_string(), "-d".to_string(), "--name".to_string(), self.name.clone()];
        v.extend(self.flags.iter().cloned());
        v.push(self.image.clone());
        v.extend(self.cmd_args.iter().cloned());
        v
    }
}

fn flags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// pg 12 个 -c 参数（补充需求②：实配全集；前三个随 hw 推导，其余固定）。
pub fn postgres_spec(root: &Path, pg_pass: &str, hw: &HwTuning) -> ContainerSpec {
    let pw = format!("POSTGRES_PASSWORD={pg_pass}");
    let data = format!("{}:/var/lib/postgresql/data", root.join("_middleware/postgres").display());
    ContainerSpec {
        name: "sr-postgres".into(),
        image: "postgres:16-alpine".into(),
        flags: flags(&[
            "--network", dockerops::DOCKER_NET, "--restart", "unless-stopped", "-e", &pw, "-v", &data,
        ]),
        cmd_args: flags(&[
            "-c", &format!("shared_buffers={}MB", hw.pg_shared_buffers_mb),
            "-c", &format!("effective_cache_size={}MB", hw.pg_effective_cache_mb),
            "-c", "work_mem=16MB",
            "-c", &format!("max_connections={}", hw.pg_max_connections),
            "-c", "autovacuum_naptime=30s",
            "-c", "max_wal_size=2GB",
            "-c", "min_wal_size=256MB",
            "-c", "checkpoint_completion_target=0.9",
            "-c", "random_page_cost=1.1",
            "-c", "effective_io_concurrency=200",
            "-c", "wal_compression=on",
            "-c", "maintenance_work_mem=128MB",
        ]),
    }
}

/// redis 规格：--databases 与 policy.REDIS_DBS 一致；--maxmemory 随 hw 推导（补充需求②）。
pub fn redis_spec(root: &Path, dbs: u16, hw: &HwTuning) -> ContainerSpec {
    let data = format!("{}:/data", root.join("_middleware/redis").display());
    ContainerSpec {
        name: "sr-redis".into(),
        image: "redis:7-alpine".into(),
        flags: flags(&[
            "--network", dockerops::DOCKER_NET, "--restart", "unless-stopped", "-v", &data,
        ]),
        cmd_args: flags(&[
            "--appendonly", "yes",
            "--databases", &dbs.to_string(),
            "--maxmemory", &format!("{}mb", hw.redis_maxmemory_mb),
            "--maxmemory-policy", "allkeys-lru",
        ]),
    }
}

pub fn registry_spec(root: &Path) -> ContainerSpec {
    let reg = format!("{}:/var/lib/registry", root.join("_middleware/registry").display());
    let htpasswd = format!("{}:/etc/docker/registry/htpasswd:ro", state::htpasswd_path(root).display());
    ContainerSpec {
        name: "sr-registry".into(),
        image: "registry:2".into(),
        flags: flags(&[
            "--network", dockerops::DOCKER_NET, "--restart", "unless-stopped",
            "-p", "127.0.0.1:5000:5000",
            "-e", "REGISTRY_STORAGE_DELETE_ENABLED=true",
            "-e", "REGISTRY_AUTH=htpasswd",
            "-e", "REGISTRY_AUTH_HTPASSWD_REALM=ShowroomRegistry",
            "-e", "REGISTRY_AUTH_HTPASSWD_PATH=/etc/docker/registry/htpasswd",
            "-v", &reg,
            "-v", &htpasswd,
        ]),
        cmd_args: vec![],
    }
}

// —— Obs：现状观察值（plan 的唯一世界输入；observe 收集，单测喂 fake）——

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Obs {
    pub dirs: BTreeSet<PathBuf>,
    pub files: BTreeMap<PathBuf, String>,
    pub net: bool,
    pub containers: BTreeSet<String>,
    /// sr-redis 实例的 --databases 实际值（容器缺失 → None）
    pub redis_databases: Option<u16>,
    pub pkgs: BTreeSet<String>,
    pub units_installed: BTreeSet<String>,
    pub units_enabled: BTreeSet<String>,
    /// 二进制资产存在性（frpc；文本 map 之外的 ELF）
    pub binaries: BTreeSet<PathBuf>,
    /// 硬件观测（hw 推导输入；/proc/meminfo MemTotal、df /、nproc）
    pub mem_total_mb: u64,
    pub disk_total_mb: u64,
    pub cores: u32,
}

// —— Action（plan-15 §2.2 枚举 + 补充需求① FrpcFetch）——

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    CreateDir { path: PathBuf, mode: u32 },
    WriteFile { path: PathBuf, contents: String, mode: u32 },
    BtrfsSubvol { path: PathBuf },
    NetworkCreate { name: String },
    ContainerUp { spec: ContainerSpec },
    ContainerRm { name: String },
    PackageInstall { pkg: &'static str },
    UnitInstall { name: String, contents: String },
    UnitEnable { name: String, now: bool },
    UnitDisableRm { name: String },
    Cmd { what: String, argv: Vec<String> },
    /// frpc 下载（补充需求①）：下载后 file 校验 ELF 再 chmod 0755；非 ELF 报错并提示手工放置。
    FrpcFetch { url: String, dest: PathBuf },
}

/// 去掉行首 # 注释行（host.env 审计注释不参与幂等比较）。
fn strip_comment_lines(s: &str) -> String {
    s.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n")
}

/// 幂等判定（每类一臂，纯函数，单测直测；plan-15 §2.2）。
/// 两个定向例外：host.env 比较前剥注释行（审计注释只在首生成落盘，不引发重写churn）；
/// contents 为空的 WriteFile 是 touch 语义（registry.htpasswd 前置——存在即 clean，
/// 绝不覆盖 user-add 管理的活账本）。
pub fn action_needed(a: &Action, obs: &Obs) -> bool {
    match a {
        Action::CreateDir { path, .. } => !obs.dirs.contains(path),
        Action::WriteFile { path, contents, .. } => match obs.files.get(path) {
            None => true,
            Some(cur) => {
                if path.file_name().is_some_and(|n| n == "host.env") {
                    strip_comment_lines(cur) != strip_comment_lines(contents)
                } else if contents.is_empty() {
                    false // touch：存在即 clean
                } else {
                    cur != contents
                }
            }
        },
        Action::BtrfsSubvol { path } => !obs.dirs.contains(path),
        Action::NetworkCreate { .. } => !obs.net,
        Action::ContainerUp { spec } => !obs.containers.contains(&spec.name),
        Action::ContainerRm { name } => obs.containers.contains(name),
        Action::PackageInstall { pkg } => !obs.pkgs.contains(*pkg),
        Action::UnitInstall { name, contents } => {
            obs.files.get(&unit_path(name)).map(String::as_str) != Some(contents.as_str())
        }
        Action::UnitEnable { name, .. } => !obs.units_enabled.contains(name),
        Action::UnitDisableRm { name } => obs.units_installed.contains(name) || obs.units_enabled.contains(name),
        Action::Cmd { .. } => true, // 派生类：只在触发条件成立时进 plan（见 plan_setup）
        Action::FrpcFetch { dest, .. } => !obs.binaries.contains(dest),
    }
}

/// 期望目录全集（顺序即 plan 顺序；权限按 deployment.md：state 三件 0700，其余 0755）。
pub fn expected_dirs(root: &Path, host: &HostEnv) -> Vec<(PathBuf, u32)> {
    let mut v = vec![
        (root.join("state"), 0o700),
        (root.join("state/tokens"), 0o700),
        (root.join("state/users"), 0o700),
        (root.join("state/projects"), 0o755),
        (root.join("state/routes"), 0o755),
        (root.join("_middleware"), 0o755),
    ];
    for mw in crate::middleware::MW_NAMES {
        if host.middleware.contains(mw) {
            v.push((root.join(format!("_middleware/{mw}")), 0o755));
        }
    }
    v.push((root.join("_console"), 0o755));
    v.push((root.join("_tmp"), 0o755));
    v.push((root.join("_bootstrap"), 0o755));
    v.push((root.join("_bootstrap/public"), 0o755)); // /_api/tool/* 下发源（控制台内下载）
    if host.frp == FrpMode::Chmlfrp {
        v.push((PathBuf::from("/usr/local/frpc"), 0o755));
    }
    v
}

/// pg 超级用户密码：优先既有 middleware.cred（沿用，绝不轮换）；缺文件 → 生成 32hex（首跑）。
fn pg_password(root: &Path, obs: &Obs) -> (String, bool) {
    if let Some(t) = obs.files.get(&pg_cred_path(root)) {
        if let Some((_, pw)) = t.trim().split_once(':') {
            if !pw.is_empty() {
                return (pw.to_string(), false);
            }
        }
    }
    (state::rand_hex(16), true)
}

pub fn pg_cred_path(root: &Path) -> PathBuf {
    root.join("state/middleware.cred")
}

/// 期望状态全程序 → 过滤出 needed（幂等核心；满 Obs → 空 plan）。
/// redis 实例 databases < policy 不产 Action、产 Warning（见 plan_warnings）——数据安全优先。
pub fn plan_setup(host: &HostEnv, obs: &Obs) -> Vec<Action> {
    let root = state::root();
    let hw = hw_from_obs(obs);
    let mut full: Vec<Action> = vec![];

    for (p, mode) in expected_dirs(&root, host) {
        full.push(Action::CreateDir { path: p, mode });
    }
    full.push(Action::BtrfsSubvol { path: PathBuf::from(SNAPS_SUBVOL) });

    // host.env：首跑（文件缺失）带 hw 审计注释行；已存在则纯 to_env 比较（剥注释幂等）
    let host_first_run = !obs.files.contains_key(&host_env_path(&root));
    let host_contents = if host_first_run {
        format!(
            "# 由 hw_defaults({}) 推导于 {}\n{}",
            hw_summary(obs.mem_total_mb, obs.disk_total_mb, obs.cores),
            date_label(state::unix_now()),
            host.to_env()
        )
    } else {
        host.to_env()
    };
    full.push(Action::WriteFile { path: host_env_path(&root), contents: host_contents, mode: 0o600 });

    let policy_path = root.join("state/policy.env");
    let policy_existing = obs.files.get(&policy_path).cloned();
    let policy_contents = if policy_existing.is_some() {
        render_policy(policy_existing.as_deref())
    } else {
        render_policy_hw(None, Some(&hw))
    };
    full.push(Action::WriteFile { path: policy_path, contents: policy_contents, mode: 0o644 });

    if host.middleware.contains("registry") {
        // registry 前置：touch 语义（contents=""，存在即 clean——见 action_needed）
        full.push(Action::WriteFile { path: state::htpasswd_path(&root), contents: String::new(), mode: 0o644 });
    }

    full.push(Action::NetworkCreate { name: dockerops::DOCKER_NET.to_string() });

    // 多则停删（SPEC §15.1）：已禁用中间件的残留容器收敛（bind 数据卷不随 -v 删除；
    // 禁用门槛/影响面在 middleware disable 与 POST 两段式 impact 中把关）
    for mw in crate::middleware::MW_NAMES {
        if !host.middleware.contains(mw) {
            full.push(Action::ContainerRm { name: format!("sr-{mw}") });
        }
    }

    if host.middleware.contains("postgres") {
        let (pass, fresh) = pg_password(&root, obs);
        if fresh {
            full.push(Action::WriteFile {
                path: pg_cred_path(&root),
                contents: format!("postgres:{pass}\n"),
                mode: 0o600,
            });
        }
        full.push(Action::ContainerUp { spec: postgres_spec(&root, &pass, &hw) });
    }
    if host.middleware.contains("redis") {
        let dbs = state::Policy::load(&root).redis_dbs;
        full.push(Action::ContainerUp { spec: redis_spec(&root, dbs, &hw) });
    }
    if host.middleware.contains("registry") {
        full.push(Action::ContainerUp { spec: registry_spec(&root) });
    }

    if host.caddy {
        full.push(Action::PackageInstall { pkg: "caddy" });
        full.push(Action::WriteFile {
            path: PathBuf::from(dockerops::CADDY_FILE),
            contents: render_caddyfile(&host.middleware, &host.lan_host, &host.public_host),
            mode: 0o644,
        });
    }

    if host.tuning {
        for (path, contents) in [
            (SYSCTL_CONF, SYSCTL_CONTENTS),
            (JOURNALD_CONF, JOURNALD_CONTENTS),
            (WATCHDOG_CONF, WATCHDOG_CONTENTS),
        ] {
            full.push(Action::WriteFile { path: PathBuf::from(path), contents: contents.to_string(), mode: 0o644 });
        }
    }

    // 单元文件（api 渲染 + 7 timer 对恒装；dedup 按 DEDUP 分支；chmlfrp 按 FRP 分支）
    full.push(Action::UnitInstall {
        name: "showroom-api.service".into(),
        contents: render_unit_api(host),
    });
    for (name, contents) in TIMER_UNITS {
        full.push(Action::UnitInstall { name: name.to_string(), contents: contents.to_string() });
    }
    if host.dedup {
        for (name, contents) in DEDUP_UNITS {
            full.push(Action::UnitInstall { name: name.to_string(), contents: contents.to_string() });
        }
        full.push(Action::WriteFile {
            path: PathBuf::from(DEDUP_SCRIPT_PATH),
            contents: DEDUP_SCRIPT.to_string(),
            mode: 0o755,
        });
    } else {
        full.push(Action::UnitDisableRm { name: "showroom-dedup.timer".into() });
        full.push(Action::UnitDisableRm { name: "showroom-dedup.service".into() });
    }
    if host.frp == FrpMode::Chmlfrp {
        full.push(Action::UnitInstall {
            name: "chmlfrp.service".into(),
            contents: render_unit_chmlfrp(&root),
        });
    }

    // 预压缩脚本（补充需求③：/usr/local/bin/showroom-precompress；file_server precompressed 已渲染进 Caddyfile）
    full.push(Action::WriteFile {
        path: PathBuf::from(PRECOMPRESS_PATH),
        contents: PRECOMPRESS_SCRIPT.to_string(),
        mode: 0o755,
    });

    // —— 过滤出 needed，再派生 Cmd（只在触发条件成立时追加——保证二次运行为 clean）——
    let mut needed: Vec<Action> = full.into_iter().filter(|a| action_needed(a, obs)).collect();

    fn has(list: &[Action], pred: &dyn Fn(&Action) -> bool) -> bool {
        list.iter().any(pred)
    }
    fn wf_pred(p: &str) -> impl Fn(&Action) -> bool + '_ {
        move |a: &Action| matches!(a, Action::WriteFile { path, .. } if path == Path::new(p))
    }

    let mut enables: Vec<Action> = vec![Action::UnitEnable { name: "showroom-api.service".into(), now: true }];
    for t in TIMER_ENABLE {
        enables.push(Action::UnitEnable { name: t.to_string(), now: true });
    }
    if host.dedup {
        enables.push(Action::UnitEnable { name: "showroom-dedup.timer".into(), now: true });
    }
    if host.frp == FrpMode::Chmlfrp {
        let fetch = Action::FrpcFetch { url: FRPC_URL.to_string(), dest: PathBuf::from(FRPC_BIN) };
        if action_needed(&fetch, obs) {
            needed.insert(0, fetch); // 下载先于单元启用
        }
        enables.push(Action::UnitEnable { name: "chmlfrp.service".into(), now: true });
    }
    let enables: Vec<Action> = enables.into_iter().filter(|a| action_needed(a, obs)).collect();

    // 单元文件有变更 → daemon-reload（先于 enable --now）
    if has(&needed, &|a| matches!(a, Action::UnitInstall { .. } | Action::UnitDisableRm { .. })) {
        needed.push(Action::Cmd {
            what: "daemon-reload".into(),
            argv: vec!["systemctl".into(), "daemon-reload".into()],
        });
    }
    needed.extend(enables);

    if host.caddy {
        if has(&needed, &|a| matches!(a, Action::PackageInstall { .. })) {
            needed.push(Action::Cmd {
                what: "caddy enable --now（包自带单元）".into(),
                argv: vec!["systemctl".into(), "enable".into(), "--now".into(), "caddy".into()],
            });
        }
        if has(&needed, &wf_pred(dockerops::CADDY_FILE)) {
            needed.push(Action::Cmd {
                what: "caddy validate".into(),
                argv: vec!["caddy".into(), "validate".into(), "--config".into(), dockerops::CADDY_FILE.into(), "--adapter".into(), "caddyfile".into()],
            });
            needed.push(Action::Cmd {
                what: "caddy reload".into(),
                argv: vec!["caddy".into(), "reload".into(), "--config".into(), dockerops::CADDY_FILE.into(), "--adapter".into(), "caddyfile".into()],
            });
        }
    }
    if host.tuning && has(&needed, &wf_pred(SYSCTL_CONF)) {
        needed.push(Action::Cmd {
            what: "sysctl --system".into(),
            argv: vec!["sysctl".into(), "--system".into()],
        });
    }
    if has(&needed, &wf_pred(PRECOMPRESS_PATH)) {
        needed.push(Action::Cmd {
            what: "precompress _console（首跑对存在文件执行一次）".into(),
            argv: vec!["bash".into(), PRECOMPRESS_PATH.into(), root.join("_console").display().to_string()],
        });
    }
    needed
}

/// redis 漂移黄条（plan-15 §9 冲突点：现网实例 --databases 16 手工遗留——setup 不重建，只引导走 §5 扩容流）。
pub fn plan_warnings(root: &Path, obs: &Obs) -> Vec<String> {
    let mut w = vec![];
    let policy = state::Policy::load(root);
    if let Some(actual) = obs.redis_databases {
        if actual < policy.redis_dbs {
            w.push(format!(
                "redis 实例 databases={actual} < policy REDIS_DBS={} → `showroomctl middleware set redis databases={}`",
                policy.redis_dbs, policy.redis_dbs
            ));
        }
    }
    w
}

// —— observe / apply（subprocess 薄层）——

/// 现状探测：fs + docker/network/systemctl/pacman + 硬件观测。
/// 探测失败一律取「缺失/零值」而非报错（diff 如实显示将做的动作）。
pub fn observe(root: &Path, host: &HostEnv) -> Obs {
    let mut obs = Obs::default();
    for (p, _) in expected_dirs(root, host) {
        if p.is_dir() {
            obs.dirs.insert(p);
        }
    }
    if Path::new(SNAPS_SUBVOL).is_dir() {
        obs.dirs.insert(PathBuf::from(SNAPS_SUBVOL));
    }
    let mut files: Vec<PathBuf> = vec![
        root.join("state/policy.env"),
        host_env_path(root),
        state::htpasswd_path(root),
        pg_cred_path(root),
        PathBuf::from(dockerops::CADDY_FILE),
        PathBuf::from(SYSCTL_CONF),
        PathBuf::from(JOURNALD_CONF),
        PathBuf::from(WATCHDOG_CONF),
        PathBuf::from(DEDUP_SCRIPT_PATH),
        PathBuf::from(PRECOMPRESS_PATH),
    ];
    for name in all_unit_names() {
        files.push(unit_path(&name));
    }
    for p in files {
        if let Ok(t) = fs::read_to_string(&p) {
            obs.files.insert(p, t);
        }
    }
    obs.net = dockerops::run_fail_ok("docker", &["network", "inspect", dockerops::DOCKER_NET]);
    obs.containers = dockerops::all_container_names().into_iter().collect();
    obs.redis_databases = dockerops::redis_databases();
    if Path::new(FRPC_BIN).exists() {
        obs.binaries.insert(PathBuf::from(FRPC_BIN));
    }
    if host.caddy && dockerops::run_fail_ok("pacman", &["-Qi", "caddy"]) {
        obs.pkgs.insert("caddy".to_string());
    }
    for name in all_unit_names() {
        if unit_path(&name).exists() {
            obs.units_installed.insert(name.clone());
        }
        if dockerops::run_tolerant("systemctl", &["is-enabled", &name]) == "enabled" {
            obs.units_enabled.insert(name);
        }
    }
    // 硬件观测：MemTotal / df / size / nproc（失败零值 → hw clamp 下限）
    if let Ok(t) = fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = state::parse_meminfo_total_kb(&t) {
            obs.mem_total_mb = kb / 1024;
        }
    }
    if let Ok(out) = dockerops::run("df", &["-B1", "--output=size,used", "/"]) {
        if let Some((size, _)) = state::parse_df_bytes(&out) {
            obs.disk_total_mb = size >> 20;
        }
    }
    if let Ok(n) = dockerops::run_tolerant("nproc", &[]).trim().parse::<u32>() {
        obs.cores = n;
    }
    obs
}

/// 执行结果报告：逐项 (action, ok, detail) + 变更计数 + 警告（plan-15 §2.2）。
#[derive(Debug, Clone)]
pub struct Report {
    pub items: Vec<(Action, bool, String)>,
    pub changed: usize,
    pub warnings: Vec<String>,
}

/// dry=只列 needed 不执行；逐项执行，单项失败记录继续（容器/单元类失败即中止余项）。
pub fn apply(root: &Path, actions: &[Action], dry: bool) -> R<Report> {
    let host = HostEnv::load(root);
    let obs = observe(root, &host);
    let mut items: Vec<(Action, bool, String)> = vec![];
    let mut changed = 0usize;
    let mut warnings: Vec<String> = vec![];
    for a in actions {
        if !action_needed(a, &obs) {
            items.push((a.clone(), true, "clean".into()));
            continue;
        }
        if dry {
            items.push((a.clone(), true, "dry-run (needed)".into()));
            continue;
        }
        match exec_action(a) {
            Ok(detail) => {
                changed += 1;
                items.push((a.clone(), true, detail));
            }
            Err(e) => {
                let (kind, target, _) = action_brief(a);
                warnings.push(format!("{kind} {target} 失败，中止余项: {e}"));
                items.push((a.clone(), false, e.to_string()));
                if matches!(
                    a,
                    Action::ContainerUp { .. }
                        | Action::ContainerRm { .. }
                        | Action::UnitInstall { .. }
                        | Action::UnitEnable { .. }
                        | Action::UnitDisableRm { .. }
                ) {
                    break;
                }
            }
        }
    }
    Ok(Report { items, changed, warnings })
}

fn exec_action(a: &Action) -> R<String> {
    use std::os::unix::fs::PermissionsExt;
    match a {
        Action::CreateDir { path, mode } => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
            Ok(format!("created ({:o})", mode))
        }
        Action::WriteFile { path, contents, mode } => {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(path, contents)?;
            fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
            Ok(format!("written {} bytes ({:o})", contents.len(), mode))
        }
        Action::BtrfsSubvol { path } => {
            let p = path.to_string_lossy();
            if dockerops::run("btrfs", &["subvolume", "show", &p]).is_ok() {
                Ok("subvolume exists".into())
            } else {
                dockerops::run("btrfs", &["subvolume", "create", &p]).map(|_| "subvolume created".to_string())
            }
        }
        Action::NetworkCreate { name } => {
            dockerops::docker(&["network", "create", name]).map(|_| "network created".to_string())
        }
        Action::ContainerUp { spec } => {
            let args = spec.run_args();
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            dockerops::docker(&refs).map(|_| format!("started {}", spec.name))
        }
        Action::ContainerRm { name } => dockerops::docker(&["rm", "-f", "-v", name]).map(|_| format!("removed {name}")),
        Action::PackageInstall { pkg } => dockerops::run("pacman", &["-S", "--noconfirm", pkg]).map(|_| format!("installed {pkg}")),
        Action::UnitInstall { name, contents } => {
            fs::write(unit_path(name), contents)?;
            Ok(format!("{name} installed"))
        }
        Action::UnitEnable { name, now } => {
            let mut argv = vec!["enable"];
            if *now {
                argv.push("--now");
            }
            argv.push(name.as_str());
            dockerops::run("systemctl", &argv).map(|_| format!("{name} enabled"))
        }
        Action::UnitDisableRm { name } => {
            let _ = dockerops::run("systemctl", &["disable", "--now", name]);
            let _ = fs::remove_file(unit_path(name));
            Ok(format!("{name} disabled + removed"))
        }
        Action::Cmd { what, argv } => {
            let (prog, rest) = argv
                .split_first()
                .ok_or_else(|| ApiError::state(format!("cmd '{what}' 空 argv")))?;
            dockerops::run(prog, &rest.iter().map(String::as_str).collect::<Vec<_>>())
                .map(|o| o.trim().to_string())
        }
        Action::FrpcFetch { url, dest } => exec_frpc_fetch(url, &dest.display().to_string()),
    }
}

/// frpc 下载（补充需求①）：curl 下载 → `file` 校验 ELF → chmod 0755；
/// 非 ELF（站点回 HTML 壳等）→ 删除产物并报错，提示手工放置后重跑 setup。
fn exec_frpc_fetch(url: &str, dest: &str) -> R<String> {
    let _ = fs::create_dir_all(Path::new(dest).parent().unwrap_or(Path::new("/usr/local/frpc")));
    dockerops::run("curl", &["-fsSL", "-o", dest, url])?;
    let file_out = dockerops::run("file", &[dest]).unwrap_or_default();
    if !file_out.contains("ELF") {
        let _ = fs::remove_file(dest);
        return Err(ApiError::state(format!(
            "frpc 下载内容非 ELF（file 输出：{file_out}；下载源 {url} 可能回 HTML 壳）：\
             请手工放置 linux/amd64 frpc 二进制到 {dest} 后重跑 `showroomctl setup --apply`"
        )));
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
    Ok(format!("frpc downloaded + ELF verified -> {dest}"))
}

// —— diff / 报告呈现（人读行 + JSON 同构）——

pub fn action_brief(a: &Action) -> (&'static str, String, String) {
    match a {
        Action::CreateDir { path, mode } => ("CreateDir", path.display().to_string(), format!("{mode:o}")),
        Action::WriteFile { path, contents, .. } => {
            let name = path.file_name().and_then(|o| o.to_str()).unwrap_or("");
            let detail = match name {
                "policy.env" => format!("{} keys", contents.lines().filter(|l| l.contains('=')).count()),
                "host.env" => format!("{} keys", contents.lines().filter(|l| l.contains('=') && !l.starts_with('#')).count()),
                _ => format!("{} lines", contents.lines().count()),
            };
            ("WriteFile", path.display().to_string(), detail)
        }
        Action::BtrfsSubvol { path } => ("BtrfsSubvol", path.display().to_string(), "btrfs subvolume".into()),
        Action::NetworkCreate { name } => ("NetworkCreate", name.clone(), "docker network".into()),
        Action::ContainerUp { spec } => ("ContainerUp", spec.name.clone(), spec.detail()),
        Action::ContainerRm { name } => ("ContainerRm", name.clone(), "docker rm -f -v".into()),
        Action::PackageInstall { pkg } => ("PackageInstall", pkg.to_string(), "pacman -S".into()),
        Action::UnitInstall { name, .. } => ("UnitInstall", name.clone(), "render".into()),
        Action::UnitEnable { name, now } => {
            ("UnitEnable", name.clone(), if *now { "enable --now".into() } else { "enable".into() })
        }
        Action::UnitDisableRm { name } => ("UnitDisableRm", name.clone(), "disable --now + rm".into()),
        Action::Cmd { what, .. } => ("Cmd", what.clone(), what.clone()),
        Action::FrpcFetch { url, dest } => ("FrpcFetch", dest.display().to_string(), url.to_string()),
    }
}

/// 人读单行（plan-15 §3 diff 样例的对齐风格）。
pub fn action_line(a: &Action) -> String {
    if let Action::ContainerUp { spec } = a {
        return format!("{:<9}{} ({})", "ctr", spec.name, spec.detail());
    }
    let (kind, target, detail) = action_brief(a);
    let label = match a {
        Action::CreateDir { .. } => "dir",
        Action::WriteFile { path, .. } if path.ends_with("99-showroom.conf") => "sysctl",
        Action::WriteFile { .. } => "file",
        Action::BtrfsSubvol { .. } => "subvol",
        Action::NetworkCreate { .. } => "network",
        Action::ContainerUp { .. } => unreachable!(),
        Action::ContainerRm { .. } => "ctr-rm",
        Action::PackageInstall { .. } => "pkg",
        Action::UnitInstall { .. } => "unit",
        Action::UnitEnable { .. } => "enable",
        Action::UnitDisableRm { .. } => "unit-rm",
        Action::Cmd { .. } => "cmd",
        Action::FrpcFetch { .. } => "frpc",
    };
    let _ = kind;
    if detail == target || detail.is_empty() {
        format!("{label:<9}{target}")
    } else {
        format!("{label:<9}{target} ({detail})")
    }
}

/// diff JSON（plan-15 §3 --json 同构）：{"host_env":…, "actions":[{kind,target,needed,detail}], "warnings":[]}。
/// actions 传入的即 needed 集（CLI 先过滤），needed 恒 true。
pub fn diff_json(host: &HostEnv, actions: &[Action], warnings: &[String]) -> Value {
    json!({
        "host_env": host.to_json(),
        "actions": actions.iter().map(|a| {
            let (kind, target, detail) = action_brief(a);
            json!({"kind": kind, "target": target, "needed": true, "detail": detail})
        }).collect::<Vec<_>>(),
        "warnings": warnings,
    })
}

/// Report JSON（apply 后与 diff 同构，加 ok/result；plan-15 §3）。
pub fn report_json(r: &Report) -> Value {
    json!({
        "ok": r.items.iter().all(|(_, ok, _)| *ok),
        "changed": r.changed,
        "items": r.items.iter().map(|(a, ok, detail)| {
            let (kind, target, _) = action_brief(a);
            json!({"kind": kind, "target": target, "ok": ok, "result": detail})
        }).collect::<Vec<_>>(),
        "warnings": r.warnings,
    })
}

// —— upgrade：二进制自替换安全顺序（plan-15 §6）——

const CTL_BIN: &str = "/usr/local/bin/showroomctl";
const CTL_NEW: &str = "/usr/local/bin/showroomctl.new";
/// 备份保留份数（更旧自删）。
const UPGRADE_KEEP_BAKS: usize = 3;

/// 备份裁剪（纯 fs，可测）：dir 下 `showroomctl.bak-*` 按名字（含时间戳，字典序即时间序）
/// 降序保留前 keep 份，返回被删除的名字。
pub fn prune_backups_in(dir: &Path, keep: usize) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("showroomctl.bak-"))
        .collect();
    names.sort();
    names.reverse();
    let mut removed = vec![];
    for n in names.into_iter().skip(keep) {
        if fs::remove_file(dir.join(&n)).is_ok() {
            removed.push(n);
        }
    }
    removed
}

fn is_active_api() -> bool {
    dockerops::run_tolerant("systemctl", &["is-active", "showroom-api"]) == "active"
}

/// _bootstrap/public 刷新（/_api/tool/* 下发源，控制台内下载）：幂等拷贝两件——
/// - platform_rsa.pub：state/ → _bootstrap/public/（公钥本就公开，whoami 亦随发）
/// - showroom-publisher.md：宿主 ~/.agents/skills/showroom-publisher/SKILL.md（Agent Skill；
///   缺源静默跳过——部署脚本可直拷，serve 启动 / setup upgrade 时再收敛）
/// 返回备注行（upgrade notes / serve 启动日志各取所需）。
pub fn refresh_bootstrap_public(root: &Path) -> Vec<String> {
    let dir = root.join("_bootstrap/public");
    let mut notes = vec![];
    if fs::create_dir_all(&dir).is_err() {
        return notes; // 建目录失败（root 只读等）：留给调用方环境，静默不阻断启动/升级
    }
    if let Ok(n) = fs::copy(root.join("state/platform_rsa.pub"), dir.join("platform_rsa.pub")) {
        notes.push(format!("platform_rsa.pub → _bootstrap/public（{n}B）"));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let skill = Path::new(&home).join(".agents/skills/showroom-publisher/SKILL.md");
    if skill.is_file() {
        if let Ok(n) = fs::copy(&skill, dir.join("showroom-publisher.md")) {
            notes.push(format!("showroom-publisher.md → _bootstrap/public（{n}B）"));
        }
    }
    notes
}

/// `showroomctl upgrade [--bin <path>]`（root；默认 /tmp/showroomctl，不存在 E_NOTFOUND）：
/// 1 探针（--help 退出码 0，失败拒替）→ 2 落 .new（0755）→ 3 备份旧（保最近 3 份）→
/// 4 同文件系统 rename 原子替换（禁 cp 覆盖式写入，不受运行中进程 ETXTBSY 影响）→
/// 5 restart showroom-api + 3s 探活 → 6 失败回滚旧备份（仍失败 E_STATE 报手工路径）→
/// 7 内联收敛（setup 语义重渲染 diff 后 apply）→ 8 /tmp/showroom 同步 + _bootstrap 刷新
///（**不碰 _console**：wasm 产物随发布流程单独更新）。
pub fn upgrade(bin: Option<&Path>) -> R<Value> {
    use std::os::unix::fs::PermissionsExt;
    let src = bin.unwrap_or(Path::new("/tmp/showroomctl"));
    if !src.exists() {
        return Err(ApiError::new(
            "E_NOTFOUND",
            format!("upgrade 源二进制不存在：{}（默认 /tmp/showroomctl，可 --bin 指定）", src.display()),
        ));
    }
    let probe = std::process::Command::new(src)
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !probe {
        return Err(ApiError::state(format!(
            "新二进制探针失败（{} --help 非零退出或不可执行），拒替", src.display()
        )));
    }
    fs::copy(src, CTL_NEW)?;
    fs::set_permissions(CTL_NEW, fs::Permissions::from_mode(0o755))?;

    let ts = dockerops::run("date", &["+%Y%m%d-%H%M"])?.trim().to_string();
    let bak = PathBuf::from(format!("{CTL_BIN}.bak-{ts}"));
    let had_old = Path::new(CTL_BIN).exists();
    if had_old {
        fs::copy(CTL_BIN, &bak)?; // cp -a 语义（权限随行）
        let removed = prune_backups_in(Path::new("/usr/local/bin"), UPGRADE_KEEP_BAKS);
        if !removed.is_empty() {
            eprintln!("showroomctl: upgrade: 裁剪旧备份: {}", removed.join(", "));
        }
    }

    fs::rename(CTL_NEW, CTL_BIN)?; // 原子替换

    let _ = dockerops::run("systemctl", &["restart", "showroom-api"]);
    std::thread::sleep(std::time::Duration::from_secs(3));
    if !is_active_api() {
        if bak.exists() {
            fs::rename(&bak, CTL_BIN)?;
            let _ = dockerops::run("systemctl", &["restart", "showroom-api"]);
            std::thread::sleep(std::time::Duration::from_secs(3));
            if is_active_api() {
                return Ok(json!({
                    "ok": true, "rolled_back": true,
                    "note": "restart 后非 active，已回滚旧二进制并恢复 active",
                    "backup": Value::Null,
                }));
            }
            return Err(ApiError::state(format!(
                "回滚后仍非 active；两份二进制共存磁盘供手工切换：\n  mv {} {CTL_BIN} && systemctl restart showroom-api",
                bak.display()
            )));
        }
        return Err(ApiError::state(format!(
            "替换后 showroom-api 非 active 且无备份可回滚（首次安装路径）；手工处置：\n  重新放置可用二进制到 {CTL_BIN} 后 systemctl restart showroom-api"
        )));
    }

    // 内联收敛：对新二进制跑 setup 语义的重渲染（单元/Caddyfile 模板若有变，diff 后 apply）
    let root = state::root();
    let host = HostEnv::load(&root);
    let obs = observe(&root, &host);
    let actions = plan_setup(&host, &obs);
    let report = apply(&root, &actions, false)?;

    // 附带：user CLI 同步 + _bootstrap 刷新；不碰 _console
    let mut notes: Vec<String> = vec![];
    if Path::new("/tmp/showroom").exists() {
        fs::copy("/tmp/showroom", "/usr/local/bin/showroom")?;
        notes.push("user CLI 已同步 /usr/local/bin/showroom".into());
    }
    fs::create_dir_all(root.join("_bootstrap"))?;
    fs::copy(CTL_BIN, root.join("_bootstrap/showroomctl"))?;
    if Path::new("/usr/local/bin/showroom").exists() {
        fs::copy("/usr/local/bin/showroom", root.join("_bootstrap/showroom"))?;
    }
    // _bootstrap/public（/_api/tool/* 下发源）：平台公钥 + Agent Skill（幂等收敛）
    for n in refresh_bootstrap_public(&root) {
        notes.push(n);
    }
    notes.push("_bootstrap 已刷新（不碰 _console，wasm 随发布流程更新）".into());

    Ok(json!({
        "ok": true,
        "backup": if had_old { Value::String(bak.display().to_string()) } else { Value::Null },
        "had_old_binary": had_old,
        "converged": report.changed,
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn troot(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录（pid+原子计数），固定名复用会互相 remove_dir_all
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "srctl-setup-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn default_host() -> HostEnv {
        HostEnv::default_for("10.0.0.5")
    }

    /// 喂固定硬件的 Obs（12G/953G/4 核），免测试机差异。
    fn obs_with_hw(base: Obs) -> Obs {
        Obs { mem_total_mb: 12288, disk_total_mb: 976_128, cores: 4, ..base }
    }

    // —— render_policy / hw（步1 已覆盖，此处仅回归键数）——
    #[test]
    fn render_policy_variants() {
        let fresh = render_policy(None);
        assert_eq!(state::parse_kv(&fresh).len(), POLICY_KEYS.len());
        assert_eq!(render_policy(Some(&fresh)), fresh);
        let m = state::parse_kv(&render_policy(Some("MAX_SLOTS=5\n")));
        assert_eq!(m["MAX_SLOTS"], "5");
    }

    // —— plan_setup：空 Obs（全新主机）→ 全量序列（每类 ≥1；快照断言 kinds 序列）——
    #[test]
    fn plan_fresh_host_covers_all_action_classes() {
        let host = default_host();
        let plan = plan_setup(&host, &obs_with_hw(Obs::default()));
        assert!(!plan.is_empty());
        // 每类至少一个（plan-15 §8 清单）
        for want in [
            "CreateDir", "WriteFile", "BtrfsSubvol", "NetworkCreate", "ContainerUp",
            "PackageInstall", "UnitInstall", "UnitEnable", "Cmd",
        ] {
            assert!(
                plan.iter().any(|a| action_brief(a).0 == want),
                "fresh plan 缺 {want} 类：{:?}",
                plan.iter().map(|a| action_brief(a).0).collect::<Vec<_>>()
            );
        }
        // 三容器齐全；pg 12 -c 参数与 redis --databases 256（policy 默认）随 hw
        let ctrs: Vec<&str> = plan
            .iter()
            .filter_map(|a| match a {
                Action::ContainerUp { spec } => Some(spec.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ctrs, vec!["sr-postgres", "sr-redis", "sr-registry"]);
        for a in &plan {
            if let Action::ContainerUp { spec } = a {
                if spec.name == "sr-postgres" {
                    assert!(spec.detail().contains("shared_buffers=1024MB"), "{}", spec.detail());
                    assert!(spec.detail().contains("max_connections=70"), "{}", spec.detail());
                }
                if spec.name == "sr-redis" {
                    assert!(spec.detail().contains("--databases 256"), "{}", spec.detail());
                    assert!(spec.detail().contains("--maxmemory 245mb"), "{}", spec.detail());
                }
            }
        }
        // host.env 首跑带 hw 审计注释；policy.env 用 hw 缺省（RESERVE_MEM_MB=1843）
        let host_wf = plan.iter().find_map(|a| match a {
            Action::WriteFile { path, contents, .. } if path.file_name().is_some_and(|n| n == "host.env") => {
                Some(contents.clone())
            }
            _ => None,
        });
        let hw_env = host_wf.unwrap();
        assert!(hw_env.starts_with("# 由 hw_defaults(12G/953G/4 核) 推导于 "), "{hw_env}");
        let policy_wf = plan.iter().find_map(|a| match a {
            Action::WriteFile { path, contents, .. } if path.ends_with("policy.env") => Some(contents.clone()),
            _ => None,
        });
        let pm = state::parse_kv(&policy_wf.unwrap());
        assert_eq!((pm["RESERVE_MEM_MB"].as_str(), pm["DISK_RESERVE_GB"].as_str()), ("1843", "28"));
        // 快照：kinds 序列稳定（长度 + 首尾锚定，免路径受 SHOWROOM_ROOT 影响的脆断言）
        let kinds: Vec<&str> = plan.iter().map(|a| action_brief(a).0).collect();
        assert_eq!(kinds.first(), Some(&"CreateDir"));
        assert!(!kinds.contains(&"FrpcFetch"), "FRP=none 不产 frpc 动作");
        assert!(kinds.len() > 30, "全量程序规模合理：{}", kinds.len());
    }

    // 满 Obs → 空 plan（幂等核心）
    #[test]
    fn plan_satisfied_obs_is_empty() {
        let host = default_host();
        let plan = plan_setup(&host, &obs_with_hw(Obs::default()));
        let mut obs = obs_with_hw(Obs::default());
        for a in &plan {
            match a {
                Action::CreateDir { path, .. } => {
                    obs.dirs.insert(path.clone());
                }
                Action::BtrfsSubvol { path } => {
                    obs.dirs.insert(path.clone());
                }
                Action::WriteFile { path, contents, .. } => {
                    obs.files.insert(path.clone(), contents.clone());
                }
                Action::ContainerUp { spec } => {
                    obs.containers.insert(spec.name.clone());
                }
                Action::PackageInstall { pkg } => {
                    obs.pkgs.insert(pkg.to_string());
                }
                Action::UnitInstall { name, contents } => {
                    obs.files.insert(unit_path(name), contents.clone());
                    obs.units_installed.insert(name.clone());
                }
                Action::UnitEnable { name, .. } => {
                    obs.units_enabled.insert(name.clone());
                }
                _ => {}
            }
        }
        obs.net = true;
        obs.redis_databases = Some(256);
        let again = plan_setup(&host, &obs);
        assert!(again.is_empty(), "满 Obs 应得空 plan，实得：{:?}", again.iter().map(|a| action_brief(a).0).collect::<Vec<_>>());
    }

    // 变体：去 redis / CADDY=no / DEDUP=none
    #[test]
    fn plan_variants() {
        let mk = |host: &HostEnv| plan_setup(host, &obs_with_hw(Obs::default()));
        // MIDDLEWARE 去 redis：无 sr-redis Action 且无 _middleware/redis 目录 Action
        let mut h = default_host();
        h.middleware.remove("redis");
        for a in mk(&h) {
            let (_, target, _) = action_brief(&a);
            assert!(!target.contains("sr-redis") && !target.contains("_middleware/redis"), "{target}");
        }
        assert!(mk(&h).iter().any(|a| matches!(a, Action::ContainerUp { spec } if spec.name == "sr-postgres")));

        // CADDY=no：无 caddy 相关 Action（pkg/Caddyfile/caddy cmds）
        let mut h = default_host();
        h.caddy = false;
        for a in mk(&h) {
            let (_, target, _) = action_brief(&a);
            let is_caddy = matches!(a, Action::PackageInstall { pkg: "caddy" })
                || target.contains("Caddyfile")
                || target.to_lowercase().contains("caddy");
            assert!(!is_caddy, "CADDY=no 不应有 caddy 动作：{target}");
        }

        // DEDUP=none：无 dedup 单元/脚本，且对已装 dedup 单元产 UnitDisableRm 收敛
        let mut h = default_host();
        h.dedup = false;
        let mut obs = obs_with_hw(Obs::default());
        obs.units_installed.insert("showroom-dedup.timer".into());
        obs.units_enabled.insert("showroom-dedup.timer".into());
        let plan = plan_setup(&h, &obs);
        assert!(!plan.iter().any(|a| matches!(a, Action::UnitInstall { name, .. } if name.contains("dedup"))));
        assert!(!plan.iter().any(|a| matches!(a, Action::WriteFile { path, .. } if path.ends_with("showroom-dedup"))));
        assert!(plan.iter().any(|a| matches!(a, Action::UnitDisableRm { .. })), "DEDUP=none 须产 UnitDisableRm");
        assert!(!plan.iter().any(|a| matches!(a, Action::UnitEnable { name, .. } if name.contains("dedup"))));

        // FRP=chmlfrp：frpc 目录/下载/单元/启用；凭据不落单元明文（EnvironmentFile）
        let mut h = default_host();
        h.frp = FrpMode::Chmlfrp;
        h.frp_token = "tok".into();
        h.frp_tunnel = "tid".into();
        let plan = mk(&h);
        assert!(plan.iter().any(|a| matches!(a, Action::FrpcFetch { .. })));
        assert!(plan.iter().any(|a| matches!(a, Action::UnitInstall { name, .. } if name == "chmlfrp.service")));
        let unit = plan
            .iter()
            .find_map(|a| match a {
                Action::UnitInstall { name, contents } if name == "chmlfrp.service" => Some(contents.clone()),
                _ => None,
            })
            .unwrap();
        assert!(unit.contains("EnvironmentFile="), "{unit}");
        assert!(!unit.contains("tok"), "凭据不进 644 单元文件");
        assert!(unit.contains("${FRP_TOKEN}"));
    }

    // action_needed 每类正反例（plan-15 §8 清单）
    #[test]
    fn action_needed_per_class() {
        let d = PathBuf::from("/x/d");
        let f = PathBuf::from("/x/f");
        let mut obs = Obs::default();
        obs.dirs.insert(d.clone());
        obs.files.insert(f.clone(), "same".into());
        obs.net = true;
        obs.containers.insert("sr-demo".into());
        obs.containers.insert("sr-old".into());
        obs.pkgs.insert("caddy".into());
        obs.units_installed.insert("t.timer".into());
        obs.units_enabled.insert("t.timer".into());
        obs.files.insert(unit_path("u.service"), "unit-body".into());
        obs.units_installed.insert("u.service".into());
        obs.binaries.insert(PathBuf::from("/usr/local/frpc/frpc"));

        assert!(!action_needed(&Action::CreateDir { path: d.clone(), mode: 0o755 }, &obs));
        assert!(action_needed(&Action::CreateDir { path: PathBuf::from("/x/e"), mode: 0o755 }, &obs));
        assert!(!action_needed(&Action::WriteFile { path: f.clone(), contents: "same".into(), mode: 0o644 }, &obs));
        assert!(action_needed(&Action::WriteFile { path: f.clone(), contents: "diff".into(), mode: 0o644 }, &obs));
        assert!(action_needed(&Action::WriteFile { path: PathBuf::from("/x/g"), contents: String::new(), mode: 0o644 }, &obs));
        // host.env：注释行差异不算漂移（审计注释只在首生成落盘）
        let hp = PathBuf::from("/srv/showroom/state/host.env");
        obs.files.insert(hp.clone(), "# 由 hw_defaults 推导\nMIDDLEWARE=redis\n".into());
        assert!(!action_needed(&Action::WriteFile { path: hp, contents: "MIDDLEWARE=redis\n".into(), mode: 0o600 }, &obs));
        // registry.htpasswd：touch 语义——存在即 clean（不为空也不覆盖）
        let htp = PathBuf::from("/srv/showroom/state/registry.htpasswd");
        obs.files.insert(htp.clone(), "alice:$2b$…\n".into());
        assert!(!action_needed(&Action::WriteFile { path: htp, contents: String::new(), mode: 0o644 }, &obs));
        // BtrfsSubvol 存在/缺
        assert!(!action_needed(&Action::BtrfsSubvol { path: d.clone() }, &obs));
        assert!(action_needed(&Action::BtrfsSubvol { path: PathBuf::from("/x/e") }, &obs));
        // NetworkCreate（Obs 只跟踪 showroom-net 一个网络）：net 在→clean / 缺→needed
        assert!(!action_needed(&Action::NetworkCreate { name: "showroom-net".into() }, &obs));
        let mut obs_no_net = obs.clone();
        obs_no_net.net = false;
        assert!(action_needed(&Action::NetworkCreate { name: "showroom-net".into() }, &obs_no_net));
        assert!(!action_needed(&Action::PackageInstall { pkg: "caddy" }, &obs));
        assert!(action_needed(&Action::PackageInstall { pkg: "duperemove" }, &obs));
        // ContainerUp 在/缺；ContainerRm 在/缺
        let spec = registry_spec(Path::new("/r"));
        assert!(!obs.containers.contains(&spec.name));
        assert!(action_needed(&Action::ContainerUp { spec }, &obs));
        assert!(action_needed(&Action::ContainerRm { name: "sr-old".into() }, &obs));
        assert!(!action_needed(&Action::ContainerRm { name: "sr-none".into() }, &obs));
        // UnitInstall 内容同/异；UnitEnable 已启用/未；UnitDisableRm 已装/无
        assert!(!action_needed(
            &Action::UnitInstall { name: "u.service".into(), contents: "unit-body".into() },
            &obs
        ));
        assert!(action_needed(
            &Action::UnitInstall { name: "u.service".into(), contents: "changed".into() },
            &obs
        ));
        assert!(action_needed(
            &Action::UnitInstall { name: "v.service".into(), contents: "x".into() },
            &obs
        ));
        assert!(!action_needed(&Action::UnitEnable { name: "t.timer".into(), now: true }, &obs));
        assert!(action_needed(&Action::UnitEnable { name: "u.service".into(), now: false }, &obs));
        assert!(action_needed(&Action::UnitDisableRm { name: "u.service".into() }, &obs));
        assert!(!action_needed(&Action::UnitDisableRm { name: "zz.timer".into() }, &obs));
        // Cmd 恒 needed（派生类只在触发时进 plan）；FrpcFetch 在/缺
        assert!(action_needed(&Action::Cmd { what: "x".into(), argv: vec!["echo".into()] }, &obs));
        assert!(action_needed(
            &Action::FrpcFetch { url: "u".into(), dest: PathBuf::from("/x/other") },
            &obs
        ));
    }

    // render_caddyfile 三组合快照（plan-15 §8：registry 缺 → 无 /v2；PUBLIC_HOST 空 → 无 site B）
    #[test]
    fn render_caddyfile_snapshots() {
        let full: BTreeSet<String> = ["postgres", "redis", "registry"].iter().map(|s| s.to_string()).collect();
        let no_reg: BTreeSet<String> = ["postgres", "redis"].iter().map(|s| s.to_string()).collect();

        let a = render_caddyfile(&full, "lan.example", "pub.example");
        assert!(a.contains("http://lan.example {"));
        assert!(a.contains("handle /v2/* {"), "registry 在集合 → site A 有 /v2");
        assert!(a.contains("reverse_proxy 127.0.0.1:5000"));
        assert!(a.contains("http://:8080, :8081 {"), "site B 双端口（8080=旧隧道遗留，8081=frpc local_port；R7：8081 曾因重渲染丢失致公网断连）");
        assert!(a.contains("precompressed zstd gzip"), "file_server precompressed 恒渲染（zstd+gzip 双格式：代理客户端常只发 gzip）");
        assert!(a.contains("import /srv/showroom/state/routes/*.caddy"));
        assert!(a.contains("handle_path /_bootstrap/* {"));

        let b = render_caddyfile(&no_reg, "lan.example", "pub.example");
        assert!(!b.contains("handle /v2/*"), "registry 缺 → 无 /v2 块");
        assert!(!b.contains("127.0.0.1:5000"), "registry 缺 → 不反代 registry");
        assert!(b.contains("http://:8080, :8081 {"));

        let c = render_caddyfile(&full, "lan.example", "");
        assert!(!c.contains("site B"), "PUBLIC_HOST 空 → 不渲染 site B");
        assert!(c.contains("handle /v2/* {"));

        // 三组合完整文本快照（组合 a 全文）
        assert_eq!(
            a,
            "# ————— 公共片段：两个站点共用 —————\n\
             (showroom-app) {\n\
             \t# 控制面：全部端点要求 SR-SIG1 签名（零信息暴露由应用层保证）\n\
             \thandle /_api/* {\n\
             \t\treverse_proxy 127.0.0.1:8090\n\
             \t}\n\
             \t# WASM 控制台（登录成功前不加载任何管理资源）\n\
             \thandle_path /_console/* {\n\
             \t\troot * /srv/showroom/_console\n\
             \t\tfile_server {\n\
             \t\t\tprecompressed zstd gzip\n\
             \t\t}\n\
             \t}\n\
             \t# 展项路由（showroomctl 按项目生成到 state/routes/<project>.caddy 并 reload）\n\
             \timport /srv/showroom/state/routes/*.caddy\n\
             \t# /_bootstrap：CLI / Skill 分发（R8：移入共用片段——公网隧道用户同样可下载，公开产物无泄露面）\n\
             \thandle_path /_bootstrap/* {\n\
             \t\troot * /srv/showroom/_bootstrap\n\
             \t\tfile_server\n\
             \t}\n\
             \t# 根路径与未匹配路径一律 404（零信息暴露：不重定向、不暴露站点信息）\n\
             }\n\
             \n\
             # ————— site A：内网面 —————\n\
             http://lan.example {\n\
             \timport showroom-app\n\
             \t# /v2：registry 推拉快通道（basic auth 由 registry 自身校验）\n\
             \thandle /v2/* {\n\
             \t\treverse_proxy 127.0.0.1:5000\n\
             \t}\n\
             }\n\
             \n\
             # ————— site B：公网面（frp 等公网隧道流量命中的站点）—————\n\
             # 公网发布走加密 import 通道，不放 /v2。\n\
             http://:8080, :8081 {\n\
             \timport showroom-app\n\
             }\n"
        );
    }

    // render_unit_api：BASE_URL 注入 + 模板占位行替换
    #[test]
    fn render_unit_api_injects_base_url() {
        let host = default_host();
        let unit = render_unit_api(&host);
        assert!(unit.contains(&format!("Environment=SHOWROOM_BASE_URL={}", host.base_url())), "{unit}");
        assert!(!unit.contains("EnvironmentFile"), "{unit}");
        assert!(unit.contains("ExecStart=/usr/local/bin/showroomctl serve"));
        assert!(unit.contains("Restart=always"));
    }

    // 容器规格三张常量表（plan-15 §2.2 + 补充需求② hw 派生）
    #[test]
    fn container_specs_match_constant_tables() {
        let root = Path::new("/srv/showroom");
        let hw = hw_defaults(12288, 976_128, 4);

        let pg = postgres_spec(root, "deadbeef", &hw);
        assert_eq!(pg.name, "sr-postgres");
        assert_eq!(pg.image, "postgres:16-alpine");
        assert!(pg.flags.contains(&"POSTGRES_PASSWORD=deadbeef".to_string()));
        assert!(pg.flags.contains(&"/srv/showroom/_middleware/postgres:/var/lib/postgresql/data".to_string()));
        // 12 个 -c 参数（补充需求②实配全集）
        let c_params: Vec<&String> = pg.cmd_args.iter().filter(|a| *a != "-c").collect();
        assert_eq!(c_params.len(), 12, "{:?}", pg.cmd_args);
        let pair = |k: &str, v: &str| pg.flags.windows(2).any(|w| w[0] == k && w[1] == v);
        assert!(pair("--network", "showroom-net"));
        assert!(pair("--restart", "unless-stopped"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "shared_buffers=1024MB"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "effective_cache_size=4300MB"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "work_mem=16MB"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "max_connections=70"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "autovacuum_naptime=30s"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "max_wal_size=2GB"));
        assert!(pg.cmd_args.windows(2).any(|w| w[0] == "-c" && w[1] == "wal_compression=on"));

        let rd = redis_spec(root, 256, &hw);
        assert_eq!(rd.image, "redis:7-alpine");
        assert!(rd.flags.contains(&"/srv/showroom/_middleware/redis:/data".to_string()));
        assert!(rd.detail().contains("--appendonly yes --databases 256 --maxmemory 245mb --maxmemory-policy allkeys-lru"));

        let rg = registry_spec(root);
        assert_eq!(rg.image, "registry:2");
        assert!(rg.flags.contains(&"127.0.0.1:5000:5000".to_string()));
        assert!(rg.flags.contains(&"REGISTRY_STORAGE_DELETE_ENABLED=true".to_string()));
        assert!(rg.flags.contains(&"REGISTRY_AUTH=htpasswd".to_string()));
        assert!(rg.flags.contains(&"REGISTRY_AUTH_HTPASSWD_REALM=ShowroomRegistry".to_string()));
        assert!(rg.flags.contains(&"REGISTRY_AUTH_HTPASSWD_PATH=/etc/docker/registry/htpasswd".to_string()));
        assert!(rg.flags.contains(&"/srv/showroom/_middleware/registry:/var/lib/registry".to_string()));
        assert!(rg.flags.contains(&"/srv/showroom/state/registry.htpasswd:/etc/docker/registry/htpasswd:ro".to_string()));
        assert_eq!(rg.cmd_args, Vec::<String>::new());
    }

    // 备份裁剪：名字时间序保留最近 keep 份，更旧删除；非备份名不碰
    #[test]
    fn prune_upgrade_backups() {
        let root = troot("bak");
        for n in [
            "showroomctl.bak-20260101-0000",
            "showroomctl.bak-20260601-1200",
            "showroomctl.bak-20261001-2359",
            "showroomctl.bak-20261010-0800",
            "showroomctl.bak-20261011-0900",
            "showroomctl.new",
            "unrelated",
        ] {
            fs::write(root.join(n), "x").unwrap();
        }
        let removed = prune_backups_in(&root, 3);
        assert_eq!(
            removed,
            vec![
                "showroomctl.bak-20260601-1200".to_string(), // 删除序=从旧到新遍历（保留序为新到旧）
                "showroomctl.bak-20260101-0000".to_string(),
            ]
        );
        assert!(root.join("showroomctl.bak-20261011-0900").exists(), "最近 3 份保留");
        assert!(root.join("showroomctl.bak-20261010-0800").exists());
        assert!(root.join("showroomctl.bak-20261001-2359").exists());
        assert!(root.join("showroomctl.new").exists() && root.join("unrelated").exists(), "非备份名不碰");
        // 不足 keep 份：全保留
        assert!(prune_backups_in(&root, 5).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    // upgrade 前置：源不存在 E_NOTFOUND（替换路径需 root/真实 systemd，e2e 域）
    #[test]
    fn upgrade_missing_source() {
        let e = upgrade(Some(Path::new("/nonexistent/showroomctl"))).unwrap_err();
        assert_eq!(e.code, "E_NOTFOUND", "{}", e.msg);
        assert!(e.msg.contains("/nonexistent/showroomctl"));
    }

    // 探针失败拒替：指向一个存在但 --help 非零的"二进制"（shell 脚本模拟）
    #[test]
    fn upgrade_rejects_failing_probe() {
        let root = troot("probe");
        let fake = root.join("fakectl");
        fs::write(&fake, "#!/bin/sh\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let e = upgrade(Some(&fake)).unwrap_err();
        assert_eq!(e.code, "E_STATE", "{}", e.msg);
        assert!(e.msg.contains("拒替"), "{}", e.msg);
        let _ = fs::remove_dir_all(&root);
    }

    // apply 冒烟：dry 只列不执行；真执行 dir/file/cmd 落地（subprocess 仅此薄层冒烟；
    // 幂等性由 plan_satisfied_obs_is_empty 在纯函数层证明——apply 的 clean 复核走 observe，
    // observe 只探测 SHOWROOM_ROOT 期望集，临时目录不在其中，故此处不做二次 clean 断言）
    #[test]
    fn apply_smoke_dry_then_execute() {
        let root = troot("apply");
        let dir = root.join("nested/dir");
        let file = dir.join("f.txt");
        let actions = vec![
            Action::CreateDir { path: dir.clone(), mode: 0o755 },
            Action::WriteFile { path: file.clone(), contents: "hi".into(), mode: 0o644 },
            Action::Cmd { what: "echo".into(), argv: vec!["echo".into(), "smoke".into()] },
        ];
        let dry = apply(&root, &actions, true).unwrap();
        assert_eq!(dry.changed, 0);
        assert!(dry.items.iter().all(|(_, ok, _)| *ok));
        assert!(!dir.exists(), "dry 不落盘");
        let rep = apply(&root, &actions, false).unwrap();
        assert_eq!(rep.changed, 3, "{:?}", rep.items);
        assert!(file.exists());
        assert_eq!(fs::read_to_string(&file).unwrap(), "hi");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
        let _ = fs::remove_dir_all(&root);
    }
}
