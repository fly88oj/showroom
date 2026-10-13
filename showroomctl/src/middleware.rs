//! middleware：host.env 期望状态与中间件集合编排（plan-15 §2.1 / SPEC §15）。
//! HostEnv 纯函数（parse/to_env/base_url）+ fs 薄层（load/save）；容器编排与扩容状态机在后续段落。
//! host.env 不含业务限额（分层论证见 plan-15 §1：限额归 policy.env）；
//! 但含 frp 凭据（补充需求：FRP_TOKEN/FRP_TUNNEL），故整文件 0600（root）。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::dockerops;
use crate::setup;
use crate::state::{self, ApiError, R};

/// 中间件名单（host.env MIDDLEWARE 的合法成员）。
pub const MW_NAMES: [&str; 3] = ["postgres", "redis", "registry"];

/// host.env 文件位置：`state/host.env`（setup 首跑生成；plan-15 §1）。
pub fn host_env_path(root: &Path) -> PathBuf {
    root.join("state/host.env")
}

/// frp 通道形态（补充需求①：FRP=none|chmlfrp）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrpMode {
    None,
    Chmlfrp,
}

impl FrpMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            FrpMode::None => "none",
            FrpMode::Chmlfrp => "chmlfrp",
        }
    }

    pub fn parse(s: &str) -> R<Self> {
        match s {
            "none" => Ok(FrpMode::None),
            "chmlfrp" => Ok(FrpMode::Chmlfrp),
            _ => Err(ApiError::new("E_NAME", format!("FRP 须为 none/chmlfrp（got '{s}'）"))),
        }
    }
}

/// 主机期望状态（plan-15 §1 键表 + frp 三键）。
/// 不含任何业务限额/秘密之外的设施开关；pg 超级用户密码在 state/middleware.cred（0600）。
#[derive(Debug, Clone, PartialEq)]
pub struct HostEnv {
    /// MIDDLEWARE：空格集合 ⊆ {postgres,redis,registry}
    pub middleware: BTreeSet<String>,
    /// CADDY=yes/no：安装(pacman)+渲染 Caddyfile
    pub caddy: bool,
    /// site A 站点地址（Caddyfile 渲染）
    pub lan_host: String,
    /// site B 站点地址；空=纯内网主机（不渲染 site B）
    pub public_host: String,
    /// TUNING=sysctl：内核调优
    pub tuning: bool,
    /// DEDUP=weekly：周日 05:00 duperemove timer
    pub dedup: bool,
    /// FRP=chmlfrp：frpc 自举（二进制下载 + chmlfrp.service）
    pub frp: FrpMode,
    /// ChmlFrp usertoken（凭据：host.env 整文件 0600）
    pub frp_token: String,
    /// ChmlFrp 隧道 ID
    pub frp_tunnel: String,
}

impl HostEnv {
    /// 缺省形状（plan-15 §1 默认列）：全部中间件、开 caddy/调优/dedup、无 frp。
    pub fn default_for(lan: &str) -> Self {
        HostEnv {
            middleware: MW_NAMES.iter().map(|s| s.to_string()).collect(),
            caddy: true,
            lan_host: lan.to_string(),
            public_host: String::new(),
            tuning: true,
            dedup: true,
            frp: FrpMode::None,
            frp_token: String::new(),
            frp_tunnel: String::new(),
        }
    }

    /// 纯解析（规则 plan-15 §1）：坏值 → E_NAME 且指名键；未知键 → warning 不失败（前向兼容）。
    /// 缺键取默认（LAN_HOST 缺省空=绑全部接口；127.0.0.1 曾致 Caddy 只听回环）。
    pub fn parse(text: &str) -> R<Self> {
        let mut out = Self::default_for("");
        for (k, v) in state::parse_kv(text) {
            match k.as_str() {
                "MIDDLEWARE" => {
                    let mut set = BTreeSet::new();
                    for name in v.split_whitespace() {
                        validate_mw(name).map_err(|e| ApiError::new("E_NAME", format!("MIDDLEWARE: {}", e.msg)))?;
                        set.insert(name.to_string());
                    }
                    out.middleware = set;
                }
                "CADDY" => {
                    out.caddy = match v.as_str() {
                        "yes" => true,
                        "no" => false,
                        _ => {
                            return Err(ApiError::new(
                                "E_NAME",
                                format!("CADDY 须为 yes/no（got '{v}'）"),
                            ))
                        }
                    };
                }
                "LAN_HOST" => out.lan_host = v,
                "PUBLIC_HOST" => out.public_host = v,
                "TUNING" => {
                    out.tuning = match v.as_str() {
                        "sysctl" => true,
                        "none" => false,
                        _ => {
                            return Err(ApiError::new(
                                "E_NAME",
                                format!("TUNING 须为 sysctl/none（got '{v}'）"),
                            ))
                        }
                    };
                }
                "DEDUP" => {
                    out.dedup = match v.as_str() {
                        "weekly" => true,
                        "none" => false,
                        _ => {
                            return Err(ApiError::new(
                                "E_NAME",
                                format!("DEDUP 须为 weekly/none（got '{v}'）"),
                            ))
                        }
                    };
                }
                "FRP" => out.frp = FrpMode::parse(&v)?,
                "FRP_TOKEN" => out.frp_token = v,
                "FRP_TUNNEL" => out.frp_tunnel = v,
                unknown => eprintln!("showroomctl: host.env: 未知键 {unknown}（忽略）"),
            }
        }
        Ok(out)
    }

    /// 键序固定的序列化（幂等写盘前提）；MIDDLEWARE 按字典序（集合语义，与书写顺序无关）。
    pub fn to_env(&self) -> String {
        format!(
            "MIDDLEWARE={}\nCADDY={}\nLAN_HOST={}\nPUBLIC_HOST={}\nTUNING={}\nDEDUP={}\nFRP={}\nFRP_TOKEN={}\nFRP_TUNNEL={}\n",
            self.middleware.iter().cloned().collect::<Vec<_>>().join(" "),
            if self.caddy { "yes" } else { "no" },
            self.lan_host,
            self.public_host,
            if self.tuning { "sysctl" } else { "none" },
            if self.dedup { "weekly" } else { "none" },
            self.frp.as_str(),
            self.frp_token,
            self.frp_tunnel,
        )
    }

    /// 读盘；缺文件 → default_for(first_lan_ip())（首跑，不落盘——写盘是 setup 的 WriteFile Action）。
    /// 坏文件如实告警并按缺省形状处理（setup diff 会显示将重写的 host.env）。
    pub fn load(root: &Path) -> Self {
        match fs::read_to_string(host_env_path(root)) {
            Ok(t) => match Self::parse(&t) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("showroomctl: host.env 解析失败，按缺省形状处理: {e}");
                    Self::default_for(&first_lan_ip())
                }
            },
            Err(_) => Self::default_for(&first_lan_ip()),
        }
    }

    pub fn save(&self, root: &Path) -> R<()> {
        fs::create_dir_all(root.join("state"))?;
        state::write_private(&host_env_path(root), &self.to_env())
    }

    /// BASE_URL 派生自 LAN_HOST（plan-15 §1：不单设键）。
    pub fn base_url(&self) -> String {
        format!("http://{}", self.lan_host)
    }

    /// GET /_api/admin/host 的 host_env 视图；FRP_TOKEN 打码（不回显凭据）。
    pub fn to_json(&self) -> Value {
        json!({
            "MIDDLEWARE": self.middleware.iter().cloned().collect::<Vec<_>>(),
            "CADDY": self.caddy,
            "LAN_HOST": self.lan_host,
            "PUBLIC_HOST": self.public_host,
            "TUNING": if self.tuning { "sysctl" } else { "none" },
            "DEDUP": if self.dedup { "weekly" } else { "none" },
            "FRP": self.frp.as_str(),
            "FRP_TUNNEL": self.frp_tunnel,
            "FRP_TOKEN": if self.frp_token.is_empty() { "" } else { "<set>" },
        })
    }
}

/// `hostname -I` 首地址（探测失败回落 127.0.0.1）。
pub fn first_lan_ip() -> String {
    dockerops::run_tolerant("hostname", &["-I"])
        .split_whitespace()
        .next()
        .unwrap_or("127.0.0.1")
        .to_string()
}

/// 中间件成员校验：∈ {postgres,redis,registry}，否则 E_NAME（plan-15 §2.1）。
pub fn validate_mw(name: &str) -> R<()> {
    if MW_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(ApiError::new(
            "E_NAME",
            format!("unknown middleware '{name}'（合法：postgres/redis/registry）"),
        ))
    }
}

// —— 编排：status / enable / disable / set / 依赖（plan-15 §2.1、§3、§5）——

/// 引用该中间件的项目清单（disable 门槛与 --force 影响列表）：
/// postgres/redis=env 注入对应变量组的项目（redis：conf.redisdb 已分配即算，当前=全部项目）；
/// registry=全部项目（镜像拉取依赖）。纯读 all_confs。
pub fn mw_dependents(root: &Path, mw: &str) -> Vec<String> {
    let confs = state::all_confs(root);
    match mw {
        "postgres" => confs
            .into_iter()
            .filter(|(p, _)| {
                fs::read_to_string(state::env_path(root, p))
                    .map(|t| t.contains("DB_HOST=sr-postgres"))
                    .unwrap_or(true) // env 缺失按依赖计（publish 下次会注入）
            })
            .map(|(p, _)| p)
            .collect(),
        "redis" | "registry" => confs.into_iter().map(|(p, _)| p).collect(),
        _ => vec![],
    }
}

/// 禁用门槛文案（registry 提示镜像拉取依赖；plan-15 §8 测试点）。
fn mw_scope_text(mw: &str) -> &'static str {
    match mw {
        "postgres" => "DB_* 环境变量注入",
        "redis" => "REDIS_* 环境变量注入",
        "registry" => "镜像拉取（registry 推拉依赖）",
        _ => "平台依赖",
    }
}

/// 期望(host.env) / 实际(container_status + inspect args) 对照表（CLI `middleware status`）。
pub fn middleware_status(root: &Path) -> R<Value> {
    let host = HostEnv::load(root);
    let policy = state::Policy::load(root);
    let mut list = vec![];
    for name in MW_NAMES {
        let ctr = format!("sr-{name}");
        let actual = dockerops::container_status(&ctr).unwrap_or_else(|| "missing".into());
        let mut o = json!({
            "name": name,
            "expected": if host.middleware.contains(name) { "enabled" } else { "disabled" },
            "actual": actual,
        });
        if name == "redis" {
            o["databases"] = json!({
                "instance": dockerops::redis_databases(),
                "policy": policy.redis_dbs,
            });
        }
        list.push(o);
    }
    Ok(json!({"middleware": list}))
}

/// enable：host.env 集合加成员（先落盘）→ setup::apply 仅该 mw 相关 Action（局部收敛）。
pub fn middleware_enable(root: &Path, name: &str) -> R<setup::Report> {
    validate_mw(name)?;
    let mut host = HostEnv::load(root);
    host.middleware.insert(name.to_string());
    host.save(root)?;
    let obs = setup::observe(root, &host);
    let actions: Vec<setup::Action> = setup::plan_setup(&host, &obs)
        .into_iter()
        .filter(|a| action_touches_mw(a, name, root))
        .collect();
    setup::apply(root, &actions, false)
}

/// enable/disable 局部 apply 的相关性谓词：该 mw 的目录/凭据/容器 + 网络；
/// registry 额外含 htpasswd 与 Caddyfile（/v2 handle 重渲染）及其 caddy Cmd。
fn action_touches_mw(a: &setup::Action, mw: &str, root: &Path) -> bool {
    match a {
        setup::Action::NetworkCreate { .. } => true, // 容器前置
        setup::Action::CreateDir { path, .. } => {
            let base = root.join(format!("_middleware/{mw}"));
            path.starts_with(&base)
        }
        setup::Action::WriteFile { path, .. } => match mw {
            "postgres" => *path == setup::pg_cred_path(root),
            "registry" => *path == state::htpasswd_path(root) || path == Path::new(dockerops::CADDY_FILE),
            _ => false,
        },
        setup::Action::ContainerUp { spec } => spec.name == format!("sr-{mw}"),
        setup::Action::Cmd { what, argv } => {
            mw == "registry"
                && (what.contains("caddy") || argv.iter().any(|x| x.contains("caddy")))
        }
        _ => false,
    }
}

/// disable：门槛（dependents 非空且无 --force → E_STATE 拒绝并列项目）→ host.env 收敛
/// → 容器停删（bind 数据卷不动，尽力如实记录）→ registry 时重渲染 Caddyfile 去 /v2。
pub fn middleware_disable(root: &Path, name: &str, force: bool) -> R<Value> {
    validate_mw(name)?;
    let mut host = HostEnv::load(root);
    if !host.middleware.contains(name) {
        return Ok(json!({"ok": true, "removed": name, "note": "already disabled"}));
    }
    let deps = mw_dependents(root, name);
    if !deps.is_empty() && !force {
        return Err(ApiError::state(format!(
            "middleware {name} 被 {} 个项目依赖（{}）：{}；确认禁用请加 --force",
            deps.len(),
            mw_scope_text(name),
            deps.join(", ")
        )));
    }
    host.middleware.remove(name);
    host.save(root)?;
    let mut notes = vec![];
    let ctr = format!("sr-{name}");
    if dockerops::container_status(&ctr).is_some() {
        if dockerops::run_fail_ok("docker", &["rm", "-f", &ctr]) {
            notes.push(format!("container {ctr} removed（bind 数据卷保留）"));
        } else {
            notes.push(format!("E_STATE container {ctr} 移除失败（下次 setup 收敛）"));
        }
    }
    if name == "registry" && host.caddy {
        // /v2 handle 随集合消失：重渲染 + validate/reload（失败如实记录，不回滚 host.env）
        let want = setup::render_caddyfile(&host.middleware, &host.lan_host, &host.public_host);
        match fs::write(dockerops::CADDY_FILE, &want)
            .map_err(ApiError::from)
            .and_then(|_| dockerops::caddy_validate())
            .and_then(|_| dockerops::caddy_reload())
        {
            Ok(()) => notes.push("Caddyfile 重渲染（/v2 移除）+ reload".into()),
            Err(e) => notes.push(format!("E_STATE Caddyfile 重渲染: {e}")),
        }
    }
    Ok(json!({"ok": true, "removed": name, "affected_projects": deps, "notes": notes}))
}

/// set：仅 redis.databases（其余 E_NAME）→ §5 扩容状态机。
pub fn middleware_set(root: &Path, name: &str, k: &str, v: &str, confirm: bool) -> R<Value> {
    if name != "redis" || k != "databases" {
        return Err(ApiError::new(
            "E_NAME",
            format!("middleware set 仅支持 redis.databases（got {name}.{k}）"),
        ));
    }
    let n: u16 = v
        .parse()
        .map_err(|_| ApiError::new("E_NAME", format!("databases 须为整数（got '{v}'）")))?;
    redis_expand(root, n, confirm)
}

// —— Redis 逻辑库扩容确认流（plan-15 §5 状态机）——

/// 扩容校验（纯函数，plan-15 §8 测试点）：
/// - 16 ≤ N ≤ 256（上界=Conf.redisdb u8 承载，超界 E_NAME 注明类型约束）；
/// - N ≤ max(已分配 db)+1 → E_NAME「会截断在用库」；
/// - N == 当前实例值 → Ok(false)（no-op 报告）。
pub fn redis_expand_check(new_dbs: u16, max_used: Option<u8>, current: u16) -> R<bool> {
    if !(16..=256).contains(&new_dbs) {
        return Err(ApiError::new(
            "E_NAME",
            format!(
                "databases={new_dbs} 超界：须为 16-256 的整数（Conf.redisdb 为 u8，单实例至多 256 库；\
                 更大需先迁移 Conf.redisdb 为 u16，见 plan-15 §9）"
            ),
        ));
    }
    if let Some(m) = max_used {
        let floor = m as u16 + 1;
        if new_dbs <= floor {
            return Err(ApiError::new(
                "E_NAME",
                format!("databases={new_dbs} 会截断在用库（最高已分配 db {m}，需 > {floor}）"),
            ));
        }
    }
    Ok(new_dbs != current)
}

/// 提示文案（plan-15 §5 原文；from=policy 当前池，to=目标）。
pub fn redis_expand_prompt(from: u16, to: u16) -> String {
    format!(
        "Redis 逻辑库池将 {from} → {to}：需重启 Redis 实例（约 3 秒）。\n\
         - 数据已开 AOF 持久化（appendonly yes），重启不丢已确认写入；\n\
         - 使用 REDIS_* 的应用连接会闪断一次并自动重连（客户端重试期间个别请求可能报错）；\n\
         - 数据卷 _middleware/redis 为 bind 挂载，容器重建不触碰数据。\n\
         确认执行请加 --confirm。"
    )
}

/// policy.env 的 REDIS_DBS 行替换/追加（其余行 verbatim 保留；纯函数）。
pub fn policy_text_with_redis_dbs(existing: &str, n: u16) -> String {
    let mut out = String::new();
    let mut found = false;
    for l in existing.lines() {
        if l.trim_start().starts_with("REDIS_DBS=") {
            out.push_str(&format!("REDIS_DBS={n}\n"));
            found = true;
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !found {
        out.push_str(&format!("REDIS_DBS={n}\n"));
    }
    out
}

fn docker_run(spec: &setup::ContainerSpec) -> R<()> {
    let args = spec.run_args();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    dockerops::docker(&refs).map(|_| ())
}

/// 步骤 4 验证（30s 上限）：redis-cli ping == PONG 且 CONFIG GET databases == N。
fn verify_redis(n: u16) -> R<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let pong = dockerops::docker(&["exec", "sr-redis", "redis-cli", "ping"])
            .map(|s| s.trim() == "PONG")
            .unwrap_or(false);
        if pong {
            let out = dockerops::docker(&["exec", "sr-redis", "redis-cli", "CONFIG", "GET", "databases"])
                .unwrap_or_default();
            let got = out.lines().nth(1).and_then(|l| l.trim().parse::<u16>().ok());
            if got == Some(n) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(ApiError::state(format!(
                "sr-redis 验证超时（30s 上限）：ping/CONFIG GET databases != {n}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// 步骤 6 失败回滚：rm -f 新容器 → 按旧 N 重启同规格 → 仍失败 E_STATE 带手工恢复命令。
fn rollback_old(root: &Path, old_n: u16, cause: ApiError) -> ApiError {
    let spec = setup::redis_spec(root, old_n, &setup::hw_from_obs(&setup::observe(root, &HostEnv::load(root))));
    let _ = dockerops::run_fail_ok("docker", &["rm", "-f", "sr-redis"]);
    let restored = docker_run(&spec).is_ok()
        && dockerops::docker(&["exec", "sr-redis", "redis-cli", "ping"])
            .map(|s| s.trim() == "PONG")
            .unwrap_or(false);
    if restored {
        return ApiError::state(format!(
            "扩容失败已回滚（旧 --databases {old_n} 实例已恢复，数据卷未动）：{cause}"
        ));
    }
    ApiError::state(format!(
        "{cause}；回滚失败：数据卷 {} 未动，手工恢复命令：docker {}",
        root.join("_middleware/redis").display(),
        spec.run_args().join(" ")
    ))
}

/// §5 状态机入口（CLI `middleware set redis databases=N` 与 POST /_api/admin/host 第二形态）。
/// policy 恒在验证成功后才写——任何失败路径下 policy 与实例状态不自相矛盾（最坏=原状）。
pub fn redis_expand(root: &Path, new_dbs: u16, confirm: bool) -> R<Value> {
    let policy = state::Policy::load(root);
    let instance = dockerops::redis_databases();
    let current = instance.unwrap_or(policy.redis_dbs);
    let max_used = state::all_confs(root).iter().map(|(_, c)| c.redisdb).max();
    if !redis_expand_check(new_dbs, max_used, current)? {
        return Ok(json!({"ok": true, "no_op": true, "databases": new_dbs, "note": "实例与 policy 已是该值"}));
    }
    if !confirm {
        return Ok(json!({"prompt": redis_expand_prompt(current, new_dbs), "confirm_required": true}));
    }
    let t0 = std::time::Instant::now();
    // 步骤 3：停旧 → 删旧 → 按 §2.2 redis 规格带 --databases N 起新（同名同卷同网络）
    let exec = (|| -> R<()> {
        dockerops::docker(&["stop", "sr-redis"])?;
        dockerops::docker(&["rm", "sr-redis"])?;
        let hw = setup::hw_from_obs(&setup::observe(root, &HostEnv::load(root)));
        docker_run(&setup::redis_spec(root, new_dbs, &hw))?;
        verify_redis(new_dbs)
    })();
    match exec {
        Ok(()) => {
            // 步骤 5 收尾：验证通过才写 policy
            let existing = fs::read_to_string(root.join("state/policy.env")).unwrap_or_default();
            fs::create_dir_all(root.join("state"))?;
            fs::write(root.join("state/policy.env"), policy_text_with_redis_dbs(&existing, new_dbs))?;
            Ok(json!({"ok": true, "databases": new_dbs, "restart_secs": t0.elapsed().as_secs()}))
        }
        Err(e) => Err(rollback_old(root, current, e)),
    }
}

/// 实例 databases vs policy 的黄条数据（drift 一项；实例 ≥ policy → None）。
pub fn redis_drift(root: &Path) -> Option<Value> {
    let policy = state::Policy::load(root);
    let actual = dockerops::redis_databases()?;
    redis_drift_value(policy.redis_dbs, actual)
}

/// redis_drift 的纯半（host_view 复用 obs.redis_databases）。
pub fn redis_drift_value(expected: u16, actual: u16) -> Option<Value> {
    (actual < expected).then(|| {
        json!({
            "item": "sr-redis",
            "expected": format!("--databases {expected}"),
            "actual": format!("--databases {actual}"),
            "severity": "warn",
            "advice": format!("showroomctl middleware set redis databases={expected}"),
        })
    })
}

/// redis 池水位（§15.3：admin/status 与 /_api/admin/host 同源）。
pub fn redis_pool_json(used: u16, total: u16) -> Value {
    let headroom = total.saturating_sub(used);
    json!({
        "used": used,
        "total": total,
        "headroom": headroom,
        "suggest_expand": headroom * 5 < total, // 余量 < 20%
    })
}

/// 池水位快照（used=已分配库数：conf 计数去重；total=policy.redis_pool_cap）。
pub fn redis_pool_snapshot(root: &Path) -> Value {
    let policy = state::Policy::load(root);
    let used: u16 = state::all_confs(root)
        .iter()
        .map(|(_, c)| c.redisdb)
        .collect::<BTreeSet<_>>()
        .len() as u16;
    redis_pool_json(used, policy.redis_pool_cap())
}

/// 黄条首条文案（§15.3 原文；池已至上限时提示 u8 承载约束）。
pub fn redis_pool_suggestion(pool: &Value) -> Option<String> {
    if !pool["suggest_expand"].as_bool().unwrap_or(false) {
        return None;
    }
    let total = pool["total"].as_u64()?;
    let headroom = pool["headroom"].as_u64()?;
    if total >= 256 {
        return Some(
            "Redis 逻辑库池已至上限 256（Conf.redisdb u8 承载）：请控制项目数或规划 u16 迁移".into(),
        );
    }
    let z = (total * 2).min(256);
    Some(format!(
        "Redis 逻辑库余量不足（剩 {headroom}/{total}）：建议扩容 `middleware set redis databases={z}`"
    ))
}

/// 禁用的中间件名单（§15.2：publish env 注入与 ps/env 标注「未启用」用）。
pub fn disabled_middleware(root: &Path) -> Vec<String> {
    let host = HostEnv::load(root);
    MW_NAMES
        .iter()
        .copied()
        .filter(|m| !host.middleware.contains(*m))
        .map(str::to_string)
        .collect()
}

// —— GET /_api/admin/host 视图（plan-15 §4）——

/// host_view 纯核（drift 来源：mw 实况 + redis 漂移 + caddy/单元存在性 via observe）。
/// mw_actual=(mw, 容器实况)；pool 由调用方喂（redis_pool_snapshot/json）；
/// frp_status 为 chmlfrp.service is-active（FRP=none 时忽略）。
pub fn host_view_json(
    host: &HostEnv,
    policy: &state::Policy,
    obs: &setup::Obs,
    mw_actual: &[(String, String)],
    pool: &Value,
    frp_status: &str,
) -> Value {
    let mut drift = vec![];
    for (name, actual) in mw_actual {
        let enabled = host.middleware.contains(name.as_str());
        let ok = if enabled { actual == "running" } else { actual == "missing" };
        if !ok {
            // 期望启用却缺/停 → error；期望禁用却还在 → warn（多余容器，收敛用）
            drift.push(json!({
                "item": format!("sr-{name}"),
                "expected": if enabled { "enabled" } else { "disabled" },
                "actual": actual,
                "severity": if enabled { "error" } else { "warn" },
                "advice": "showroomctl setup --apply",
            }));
        }
    }
    if let (true, Some(actual)) = (host.middleware.contains("redis"), obs.redis_databases) {
        if let Some(d) = redis_drift_value(policy.redis_dbs, actual) {
            drift.push(d);
        }
    }
    if host.caddy {
        let want = setup::render_caddyfile(&host.middleware, &host.lan_host, &host.public_host);
        match (obs.pkgs.contains("caddy"), obs.files.get(Path::new(dockerops::CADDY_FILE))) {
            (false, _) => drift.push(json!({
                "item": "caddy", "expected": "installed", "actual": "missing",
                "severity": "error", "advice": "showroomctl setup --apply",
            })),
            (true, Some(have)) if have != &want => drift.push(json!({
                "item": "caddyfile", "expected": "render", "actual": "drifted",
                "severity": "warn", "advice": "showroomctl setup --apply",
            })),
            _ => {}
        }
    }
    let api_unit = setup::unit_path("showroom-api.service");
    match obs.files.get(&api_unit) {
        None => drift.push(json!({
            "item": "showroom-api.service", "expected": "installed", "actual": "missing",
            "severity": "error", "advice": "showroomctl setup --apply",
        })),
        Some(t) if t != &setup::render_unit_api(host) => drift.push(json!({
            "item": "showroom-api.service", "expected": "render", "actual": "drifted",
            "severity": "warn", "advice": "showroomctl setup --apply",
        })),
        _ => {}
    }
    if !obs.units_enabled.contains("showroom-api.service") {
        drift.push(json!({
            "item": "showroom-api.service", "expected": "enabled", "actual": "disabled",
            "severity": "error", "advice": "showroomctl setup --apply",
        }));
    }
    if host.frp == FrpMode::Chmlfrp && frp_status != "active" {
        drift.push(json!({
            "item": "chmlfrp.service", "expected": "active", "actual": frp_status,
            "severity": "error", "advice": "showroomctl setup --apply（或检查 frp 凭据/隧道）",
        }));
    }

    let mut suggestions = vec![];
    if let Some(s) = redis_pool_suggestion(pool) {
        suggestions.push(s);
    }
    for d in &drift {
        if let Some(a) = d["advice"].as_str() {
            let line = format!("{}: {}", d["item"].as_str().unwrap_or("?"), a);
            if !suggestions.contains(&line) {
                suggestions.push(line);
            }
        }
    }

    json!({
        "host_env": host.to_json(),
        "policy": {
            "MAX_SLOTS": policy.max_slots,
            "REDIS_DBS": policy.redis_dbs,
            "RESERVE_MEM_MB": policy.reserve_mem_mb,
        },
        "drift": drift,
        "redis_pool": pool,
        "frp": {
            "mode": host.frp.as_str(),
            "service": if host.frp == FrpMode::Chmlfrp { Value::String(frp_status.to_string()) } else { Value::Null },
        },
        "suggestions": suggestions,
    })
}

/// GET /_api/admin/host 数据源（subprocess 装配）。
pub fn host_view(root: &Path) -> R<Value> {
    let host = HostEnv::load(root);
    let policy = state::Policy::load(root);
    let obs = setup::observe(root, &host);
    let mw_actual: Vec<(String, String)> = MW_NAMES
        .iter()
        .map(|n| {
            (
                n.to_string(),
                dockerops::container_status(&format!("sr-{n}")).unwrap_or_else(|| "missing".into()),
            )
        })
        .collect();
    let frp_status = dockerops::run_tolerant("systemctl", &["is-active", "chmlfrp"]);
    let frp_status = if frp_status.is_empty() { "unknown" } else { frp_status.as_str() };
    Ok(host_view_json(&host, &policy, &obs, &mw_actual, &redis_pool_snapshot(root), frp_status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn troot(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录（pid+原子计数），固定名复用会互相 remove_dir_all
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "srctl-mw-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mwset(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // host.env 解析：缺省（空文本）→ 全默认形状（plan-15 §1 默认列 + FRP=none）
    #[test]
    fn hostenv_parse_defaults() {
        let d = HostEnv::parse("").unwrap();
        assert_eq!(d, HostEnv::default_for(""));
        assert_eq!(d.middleware, mwset(&["postgres", "redis", "registry"]));
        assert!(d.caddy && d.tuning && d.dedup);
        assert_eq!((d.lan_host.as_str(), d.public_host.as_str()), ("", ""));
        assert_eq!(d.frp, FrpMode::None);
        assert_eq!((d.frp_token.as_str(), d.frp_tunnel.as_str()), ("", ""));
    }

    // 全键 + 非默认值（含 frp 三键）
    #[test]
    fn hostenv_parse_full() {
        let h = HostEnv::parse(
            "MIDDLEWARE=postgres registry\nCADDY=no\nLAN_HOST=10.1.2.3\nPUBLIC_HOST=pub.example\n\
             TUNING=none\nDEDUP=none\nFRP=chmlfrp\nFRP_TOKEN=tok123\nFRP_TUNNEL=t-456\n",
        )
        .unwrap();
        assert!(!h.caddy && !h.tuning && !h.dedup);
        assert_eq!(h.lan_host, "10.1.2.3");
        assert_eq!(h.public_host, "pub.example");
        assert_eq!(h.middleware, mwset(&["postgres", "registry"]));
        assert_eq!(h.base_url(), "http://10.1.2.3");
        assert_eq!(h.frp, FrpMode::Chmlfrp);
        assert_eq!((h.frp_token.as_str(), h.frp_tunnel.as_str()), ("tok123", "t-456"));
    }

    // 坏值 → E_NAME 且指名键；未知键不失败（前向兼容）
    #[test]
    fn hostenv_bad_values_and_unknown_keys() {
        for (text, key) in [
            ("MIDDLEWARE=mysql\n", "MIDDLEWARE"),
            ("CADDY=true\n", "CADDY"),
            ("TUNING=bbr\n", "TUNING"),
            ("DEDUP=daily\n", "DEDUP"),
            ("FRP=frps\n", "FRP"),
        ] {
            let e = HostEnv::parse(text).unwrap_err();
            assert_eq!(e.code, "E_NAME", "{text}");
            assert!(e.msg.contains(key), "msg 指名键 {key}: {}", e.msg);
        }
        assert!(HostEnv::parse("SOMETHING_NEW=1\n").is_ok(), "未知键不失败");
        // MIDDLEWARE 部分坏名也拒（指名集合键）
        assert!(HostEnv::parse("MIDDLEWARE=redis mysql\n").is_err());
    }

    // to_env↔parse 往返 + 键序固定 + MIDDLEWARE 集合与书写顺序无关
    #[test]
    fn hostenv_roundtrip_and_order() {
        let h = HostEnv::default_for("192.168.1.9");
        let t = h.to_env();
        assert_eq!(HostEnv::parse(&t).unwrap(), h);
        // 键序固定（幂等写盘前提）；注释行（hw 推导审计行）不碍解析
        let keys: Vec<&str> = t
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split('=').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "MIDDLEWARE", "CADDY", "LAN_HOST", "PUBLIC_HOST", "TUNING", "DEDUP", "FRP",
                "FRP_TOKEN", "FRP_TUNNEL"
            ]
        );
        let with_comment = format!("# 由 hw_defaults(12G/953G/4 核) 推导于 2026-10-11\n{t}");
        assert_eq!(HostEnv::parse(&with_comment).unwrap(), h);

        // 集合顺序无关 → 同一 HostEnv；to_env 输出字典序稳定形态
        let a = HostEnv::parse("MIDDLEWARE=redis postgres registry\n").unwrap();
        let b = HostEnv::parse("MIDDLEWARE=registry redis postgres\n").unwrap();
        let c = HostEnv::parse("MIDDLEWARE=postgres redis registry\n").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.to_env().starts_with("MIDDLEWARE=postgres redis registry\n"));
    }

    // load/save 落盘往返（0600；缺文件走探测缺省不落盘）
    #[test]
    fn hostenv_save_load_roundtrip() {
        let root = troot("roundtrip");
        let h = HostEnv {
            middleware: mwset(&["postgres"]),
            caddy: false,
            lan_host: "10.0.0.1".into(),
            public_host: String::new(),
            tuning: false,
            dedup: false,
            frp: FrpMode::Chmlfrp,
            frp_token: "tk".into(),
            frp_tunnel: "tid".into(),
        };
        h.save(&root).unwrap();
        assert_eq!(HostEnv::load(&root), h);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(host_env_path(&root)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "host.env 含 frp 凭据，整文件 0600");
    }

    // validate_mw：三名合法；大小写/未知拒绝
    #[test]
    fn validate_mw_names() {
        for good in MW_NAMES {
            assert!(validate_mw(good).is_ok(), "{good}");
        }
        for bad in ["mysql", "", "Postgres", "REDIS", "pg"] {
            assert_eq!(validate_mw(bad).unwrap_err().code, "E_NAME", "{bad:?}");
        }
    }

    // —— 编排与扩容（plan-15 §8 测试清单）——

    fn put_conf(root: &Path, p: &str) {
        let c = state::Conf {
            user: "alice".into(),
            tag: "v1".into(),
            port: 8100,
            cport: 8080,
            mem: "256m".into(),
            cpu: 1.0,
            redisdb: 0,
            updated: 1,
            stopped: false,
            legacyslot: None,
                version: String::new(),
                version_seq: 0,
        };
        c.save(root, p).unwrap();
    }

    // 依赖门槛：有项目 conf 时 disable redis 无 --force → E_STATE 且 msg 列全部项目名；
    // registry 文案点名镜像拉取；--force 放行且 host.env 收敛
    #[test]
    fn disable_gate_and_force() {
        let root = troot("disable");
        for p in ["demo1", "demo2"] {
            put_conf(&root, p);
        }
        HostEnv::default_for("10.0.0.1").save(&root).unwrap();

        let e = middleware_disable(&root, "redis", false).unwrap_err();
        assert_eq!(e.code, "E_STATE", "{}", e.msg);
        assert!(e.msg.contains("demo1") && e.msg.contains("demo2"), "msg 列全部项目: {}", e.msg);

        let e = middleware_disable(&root, "registry", false).unwrap_err();
        assert_eq!(e.code, "E_STATE");
        assert!(e.msg.contains("镜像拉取"), "registry 影响文案: {}", e.msg);

        // --force 放行：host.env 收敛（redis 移出集合）；无 docker 环境下容器清理尽力如实记录
        let v = middleware_disable(&root, "redis", true).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["affected_projects"].as_array().map(Vec::len), Some(2));
        let h = HostEnv::load(&root);
        assert!(!h.middleware.contains("redis"), "host.env 已收敛");
        assert!(h.middleware.contains("postgres"));

        // 已禁用再 disable：no-op；坏名 E_NAME
        let again = middleware_disable(&root, "redis", true).unwrap();
        assert_eq!(again["note"], "already disabled");
        assert_eq!(middleware_disable(&root, "mysql", false).unwrap_err().code, "E_NAME");
    }

    // redis_expand 校验纯函数（plan-15 §8）：N≤maxused+1 拒；N>256 拒（msg 含 u8 约束）；N==当前 no-op
    #[test]
    fn redis_expand_check_boundaries() {
        // 越界：>256 / <16
        let e = redis_expand_check(512, None, 256).unwrap_err();
        assert_eq!(e.code, "E_NAME");
        assert!(e.msg.contains("u8"), "msg 含类型约束: {}", e.msg);
        assert_eq!(redis_expand_check(8, None, 256).unwrap_err().code, "E_NAME");

        // N ≤ max_used+1 → 拒（会截断在用库）；N > max_used+1 → 放行
        let m = Some(28u8);
        let e = redis_expand_check(29, m, 30).unwrap_err(); // 29 ≤ 28+1
        assert_eq!(e.code, "E_NAME");
        assert!(e.msg.contains("截断"), "{}", e.msg);
        assert!(redis_expand_check(30, m, 256).unwrap());
        // 无在用库：16 起放行
        assert!(redis_expand_check(16, None, 256).unwrap());
        // N == 当前实例值 → no-op（Ok(false)）
        assert!(!redis_expand_check(200, m, 200).unwrap());
        // 池顶满（max=255）：N 至多 256 ≤ floor 256 → 恒拒（类型钉死，如实提示 u8 上限路径）
        assert_eq!(redis_expand_check(256, Some(255), 100).unwrap_err().code, "E_NAME");
        assert_eq!(redis_expand_check(256, Some(255), 256).unwrap_err().code, "E_NAME");
    }

    // 提示文案原文（§5 步骤 2）
    #[test]
    fn redis_expand_prompt_text() {
        let t = redis_expand_prompt(256, 300);
        assert!(t.starts_with("Redis 逻辑库池将 256 → 300：需重启 Redis 实例（约 3 秒）。"));
        assert!(t.contains("- 数据已开 AOF 持久化（appendonly yes），重启不丢已确认写入；"));
        assert!(t.contains("- 使用 REDIS_* 的应用连接会闪断一次并自动重连（客户端重试期间个别请求可能报错）；"));
        assert!(t.contains("- 数据卷 _middleware/redis 为 bind 挂载，容器重建不触碰数据。"));
        assert!(t.ends_with("确认执行请加 --confirm。"));
    }

    // policy 行替换/追加（其余行保留）
    #[test]
    fn policy_text_redis_dbs() {
        let a = policy_text_with_redis_dbs("MAX_SLOTS=5\nREDIS_DBS=30\nDEFAULT_MEM=256m\n", 512);
        assert_eq!(a, "MAX_SLOTS=5\nREDIS_DBS=512\nDEFAULT_MEM=256m\n");
        let b = policy_text_with_redis_dbs("MAX_SLOTS=5\n", 64);
        assert_eq!(b, "MAX_SLOTS=5\nREDIS_DBS=64\n");
        let c = policy_text_with_redis_dbs("# comment\nREDIS_DBS=1\n", 8);
        assert_eq!(c, "# comment\nREDIS_DBS=8\n");
    }

    // middleware_set：仅 redis.databases；形状非法 E_NAME
    #[test]
    fn middleware_set_shape() {
        let root = troot("set-shape");
        assert_eq!(middleware_set(&root, "postgres", "databases", "8", false).unwrap_err().code, "E_NAME");
        assert_eq!(middleware_set(&root, "redis", "maxmemory", "1g", false).unwrap_err().code, "E_NAME");
        // redis.databases 走校验（bad int → E_NAME；越界在纯函数层已测）
        assert_eq!(middleware_set(&root, "redis", "databases", "abc", false).unwrap_err().code, "E_NAME");
        // 999 越界：E_NAME（u8 约束）——校验先于 docker
        let e = middleware_set(&root, "redis", "databases", "999", false).unwrap_err();
        assert_eq!(e.code, "E_NAME");
        assert!(e.msg.contains("u8"));
    }

    // drift/池水位纯函数：黄条数据 + <20% 触发建议（§15.3）
    #[test]
    fn drift_and_pool() {
        let d = redis_drift_value(256, 16).unwrap();
        assert_eq!(d["item"], "sr-redis");
        assert_eq!(d["expected"], "--databases 256");
        assert_eq!(d["actual"], "--databases 16");
        assert_eq!(d["severity"], "warn");
        assert_eq!(d["advice"], "showroomctl middleware set redis databases=256");
        assert!(redis_drift_value(256, 256).is_none(), "实例=-policy 不算漂移");
        assert!(redis_drift_value(256, 512).is_none(), "实例>policy 无害");

        // 池水位：27/256 余量足；29/30 → suggest_expand + 黄条文案（Z=60=30*2）
        let ok = redis_pool_json(27, 256);
        assert_eq!(
            (ok["used"].as_u64(), ok["total"].as_u64(), ok["headroom"].as_u64()),
            (Some(27), Some(256), Some(229))
        );
        assert_eq!(ok["suggest_expand"], false);
        assert!(redis_pool_suggestion(&ok).is_none());

        let tight = redis_pool_json(29, 30);
        assert_eq!(tight["suggest_expand"], true);
        let s = redis_pool_suggestion(&tight).unwrap();
        assert!(s.contains("剩 1/30"), "{s}");
        assert!(s.contains("middleware set redis databases=60"), "{s}");
        // 池已至 256 上限：黄条改提示 u8 承载
        let cap = redis_pool_json(250, 256);
        assert_eq!(cap["suggest_expand"], true);
        assert!(redis_pool_suggestion(&cap).unwrap().contains("u8"));
    }

    // host_view_json：漂移装配（redis 黄条 + caddy missing error + api 单元缺失）+ 池/建议
    #[test]
    fn host_view_json_drift_assembly() {
        let root = troot("host-view");
        let host = HostEnv::default_for("10.0.0.9");
        let policy = state::Policy { max_slots: 30, redis_dbs: 256, ..state::Policy::default() };
        let mut obs = setup::Obs {
            redis_databases: Some(16), // 现网遗留实例 → 黄条
            ..setup::Obs::default()
        };
        obs.containers.insert("sr-postgres".into());
        obs.containers.insert("sr-redis".into());
        obs.containers.insert("sr-registry".into());
        // caddy 未装、api 单元缺失 → error
        let mw_actual = vec![
            ("postgres".to_string(), "running".to_string()),
            ("redis".to_string(), "running".to_string()),
            ("registry".to_string(), "running".to_string()),
        ];
        let pool = redis_pool_json(27, 256);
        let v = host_view_json(&host, &policy, &obs, &mw_actual, &pool, "inactive");
        let drift = v["drift"].as_array().unwrap();
        assert!(drift.iter().any(|d| d["item"] == "sr-redis" && d["severity"] == "warn"), "{drift:?}");
        assert!(drift.iter().any(|d| d["item"] == "caddy" && d["severity"] == "error"), "{drift:?}");
        assert!(drift.iter().any(|d| d["item"] == "showroom-api.service"), "{drift:?}");
        assert_eq!(v["redis_pool"]["used"], 27);
        assert_eq!(v["policy"]["REDIS_DBS"], 256);
        assert_eq!(v["host_env"]["MIDDLEWARE"].as_array().map(Vec::len), Some(3));
        // FRP=none：frp.service 为 null 且无 chmlfrp 漂移项
        assert!(v["frp"]["service"].is_null());
        assert!(!drift.iter().any(|d| d["item"] == "chmlfrp.service"));

        // 全绿 Obs + 池紧张 → 无 error 级漂移，黄条建议在 suggestions 首条
        let mut obs_ok = setup::Obs {
            redis_databases: Some(256),
            ..setup::Obs::default()
        };
        obs_ok.pkgs.insert("caddy".into());
        obs_ok
            .files
            .insert(setup::unit_path("showroom-api.service"), setup::render_unit_api(&host));
        obs_ok.units_enabled.insert("showroom-api.service".into());
        obs_ok.files.insert(
            std::path::Path::new(dockerops::CADDY_FILE).to_path_buf(),
            setup::render_caddyfile(&host.middleware, &host.lan_host, &host.public_host),
        );
        let tight = redis_pool_json(29, 30);
        let v2 = host_view_json(&host, &policy, &obs_ok, &mw_actual, &tight, "inactive");
        assert_eq!(v2["drift"].as_array().map(Vec::len), Some(0), "{:?}", v2["drift"]);
        let sugg = v2["suggestions"].as_array().unwrap();
        assert!(sugg[0].as_str().unwrap().contains("剩 1/30"), "{sugg:?}");
        let _ = fs::remove_dir_all(&root);
    }

    // mw_dependents：conf 计数；无项目空表；未知 mw 空表
    #[test]
    fn dependents_by_conf() {
        let root = troot("deps");
        assert!(mw_dependents(&root, "redis").is_empty());
        put_conf(&root, "solo");
        for mw in MW_NAMES {
            assert_eq!(mw_dependents(&root, mw), vec!["solo".to_string()], "{mw}");
        }
        assert!(mw_dependents(&root, "mysql").is_empty());
    }

    // disabled_middleware：全集启用 → 空；去 redis → ["redis"]（§15.2 标注数据源）
    #[test]
    fn disabled_middleware_list() {
        let root = troot("disabled");
        HostEnv::default_for("10.0.0.1").save(&root).unwrap();
        assert!(disabled_middleware(&root).is_empty());
        let mut h = HostEnv::load(&root);
        h.middleware.remove("redis");
        h.save(&root).unwrap();
        assert_eq!(disabled_middleware(&root), vec!["redis".to_string()]);
    }
}
