//! showroomctl：主机侧唯一管理工具（SPEC §3 HTTP 控制面 + §7 全部运维行为）。
//! 子命令是唯一对外接缝；内部 state / dockerops / publish / ops / serve 深模块。

mod dbmaint;
mod dockerops;
mod middleware;
mod ops;
mod publish;
mod serve;
mod setup;
mod state;
mod streams;
mod tunnel;
mod unpack;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use state::{root, ApiError, R};

#[derive(Parser)]
#[command(name = "showroomctl", about = "showroom host control plane (SPEC.md)")]
struct Cli {
    /// 机器可读 JSON 输出（默认人读文本；serve 错误响应恒为 JSON）
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动 HTTP 控制面（127.0.0.1:8090；所有 /_api 端点强制 SR-SIG1 签名）
    Serve,
    /// 发账号：设初始登录密码（--password/--gen-password 必填其一）+
    /// 生成 registry 凭据（bcrypt 入 registry.htpasswd，明文 0600 入 reg.cred）。
    /// --admin 授管理员（账号属性：该账号登录后的钥匙自动继承；SPEC §12.2 + plan-16 §1.4 + B7）
    UserAdd {
        name: String,
        /// 显式初始登录密码（明文只过管理员终端，不上网线）
        #[arg(long)]
        password: Option<String>,
        /// 自动生成初始登录密码（一次性返回）
        #[arg(long)]
        gen_password: bool,
        /// 授予管理员（账号属性：该账号登录后的钥匙自动继承管理权限）
        #[arg(long)]
        admin: bool,
    },
    /// 重置/补设登录密码（plan-16 §1.4）：替换 pass.opaque，不动既有钥匙；
    /// 疑似盗号走 user-revoke/key-rm
    UserPasswd {
        user: String,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        gen_password: bool,
    },
    /// 删除发布者某把公钥（admin 标记随钥匙一并清）
    UserKeyRm { user: String, kid: String },
    /// 授/收管理员权限（账号级，R4 裁决；--admin true|false）
    UserAdmin {
        user: String,
        #[arg(long, action = clap::ArgAction::Set)]
        admin: bool,
    },
    /// 吊销发布者（auth 与 registry 登录全失效，已发布展项不动）
    UserRevoke { user: String },
    /// 管理员视图（本机调试用，幂等）：users=账号/钥匙清单，status=主机状态面板
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// 发布/滚动更新（管理员直发通道，serve 的 /_api/publish 复用同一内核）
    Publish {
        user: String,
        project: String,
        tag: String,
        #[arg(long)]
        mem: Option<String>,
        #[arg(long)]
        cpu: Option<f64>,
        #[arg(long)]
        container_port: Option<u16>,
    },
    /// 停止项目容器（conf 标记 stopped=1，autoheal 跳过）
    Stop { project: String },
    /// 启动项目容器（容器缺失则按 conf 重建）
    Start { project: String },
    /// 彻底移除项目（容器/路由/DB/凭据/登记全清）
    Rm { project: String },
    /// 回滚到 last2 记录的上一 tag
    Rollback { project: String },
    /// 列出全部项目与容器实况
    Ps,
    /// 查看容器日志
    Logs {
        project: String,
        #[arg(long, default_value_t = 100)]
        tail: u32,
    },
    /// 查看项目环境变量（§6）
    Env { project: String },
    /// 遍历 project.conf 幂等重建缺失容器/路由；先跑 v1.4 槽位布局迁移（systemd timer）
    Reconcile,
    /// 自愈：容器缺失重建 / 退出拉起 / unhealthy 重启（systemd timer）
    Autoheal,
    /// docker 镜像/构建缓存/悬空卷清理 + _tmp 陈旧会话（>24h）清理（systemd timer）
    GcImages,
    /// btrfs 快照 /@showroom（systemd timer）：hourly 保留 24 / daily 保留 7（SPEC §8.2）
    Snapshot { kind: String },
    /// registry 每 repo 字典序降序保留前 5 个 tag，其余 DELETE
    GcRegistry,
    /// registry:2 garbage-collect --delete-untagged（停-扫-启）
    RegistryGc,
    /// 从 stdin 读加密信封导入镜像（内部通道；serve 的 /_api/import 复用同一内核）
    Import { user: String, project: String, tag: String },
    /// 检查镜像是否已在本机 docker（present true/false）
    ImportCheck { image_id: String },
    /// 生成平台 RSA 对（serve 启动缺失时自动生成；已存在不覆盖）
    InitKeys,
    /// 主机自举（plan-15/SPEC §15.1）：无 --apply=diff；--apply 幂等收敛期望状态。
    /// 首跑缺 host.env 时按探测值生成并打印（LAN/PUBLIC_HOST 可覆写；限额缺省随实测硬件推导）
    Setup {
        #[arg(long)]
        apply: bool,
        /// 覆写 site A 地址（Caddyfile 渲染；BASE_URL 派生）
        #[arg(long)]
        lan_host: Option<String>,
        /// 覆写 site B 地址（空串=纯内网主机，不渲染 site B）
        #[arg(long)]
        public_host: Option<String>,
    },
    /// 中间件编排（plan-15 §3：status/enable/disable/set）
    Middleware {
        #[command(subcommand)]
        cmd: MiddlewareCmd,
    },
    /// 就地升级二进制（plan-15 §6：探针→.new→备份→原子 rename→restart 探活→
    /// 失败回滚；默认取 /tmp/showroomctl；同步 user CLI 与 _bootstrap，不碰 _console）
    Upgrade {
        #[arg(long)]
        bin: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum MiddlewareCmd {
    /// 期望(host.env) / 实际(容器) / 漂移对照表
    Status,
    /// 集合加成员 → 仅该中间件相关 Action 局部 apply
    Enable { name: String },
    /// 禁用（有项目依赖需 --force 并列受影响项目；bind 数据卷保留）
    Disable {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// set redis databases=N [--confirm]（§5 扩容确认流：无 --confirm 只见提示不动容器）
    Set {
        name: String,
        /// 形如 databases=256
        kv: String,
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// 账号 / 钥匙（kid+登记时间）/ 项目数清单（admin 为账号级）
    Users,
    /// 服务 / 磁盘 / 内存 / 容器 / timers 状态面板（GET /_api/admin/status 同源）
    Status,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(&cli) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn out(json: bool, human: &str, v: serde_json::Value) {
    if json {
        println!("{v}");
    } else {
        println!("{human}");
    }
}

fn dispatch(cli: &Cli) -> R<()> {
    let j = cli.json;
    let root = root();
    match &cli.cmd {
        Cmd::Serve => tokio::runtime::Runtime::new()
            .map_err(|e| ApiError::state(e.to_string()))?
            .block_on(serve::run(root)),

        Cmd::UserAdd { name, password, gen_password, admin } => {
            // B7 统一账号密码认证：发卡必须设初始密码（plan-16 §1.4 同款 E_NAME 指引）
            let init_pw = match (password, *gen_password) {
                (Some(p), _) => ops::InitPw::Set(p.clone()),
                (None, true) => ops::InitPw::Gen,
                (None, false) => {
                    return Err(ApiError::new(
                        "E_NAME",
                        format!("缺少初始密码：--password <pw> 与 --gen-password 至少其一（示例：showroomctl user-add {name} --gen-password --admin）"),
                    ));
                }
            };
            let added = ops::user_add(&root, name, *admin, init_pw)?;
            // 两把密码各自只此一次显示（plan-16 §1.3：分开发放）
            let human = format!(
                "user {name} added{}\n  登录密码（只此一次，控制台/showroom login 用）: {}\n  registry 密码（只此一次，docker login 用）: {}\n  registry creds in state/users/{name}/reg.cred",
                if *admin { " [ADMIN]" } else { "" },
                added.login_password.as_deref().unwrap_or(""),
                added.password,
            );
            let mut v = json!({"ok": true, "user": name, "admin": admin, "registry_password": added.password});
            if let Some(lp) = &added.login_password {
                v["login_password"] = json!(lp);
            }
            out(j, &human, v);
            Ok(())
        }
        Cmd::UserPasswd { user, password, gen_password } => {
            // --password 与 --gen-password 至少其一（plan-16 §1.4）
            let pw = match (password, *gen_password) {
                (Some(p), _) => p.clone(),
                (None, true) => ops::gen_login_password(),
                (None, false) => {
                    return Err(ApiError::new(
                        "E_NAME",
                        "缺少密码：--password <pw> 或 --gen-password 至少其一",
                    ));
                }
            };
            ops::user_passwd_set(&root, user, &pw)?;
            out(
                j,
                &format!(
                    "user {user} 登录密码已重置（旧密码失效，既有钥匙不受影响）；新密码（只此一次）: {pw}"
                ),
                json!({"ok": true, "user": user, "password": pw}),
            );
            Ok(())
        }
        Cmd::UserKeyRm { user, kid } => {
            ops::user_key_rm(&root, user, kid)?;
            out(j, &format!("removed {user}/{kid}"), json!({"ok": true}));
            Ok(())
        }
        Cmd::UserAdmin { user, admin } => {
            ops::user_set_admin(&root, user, *admin)?;
            out(
                j,
                &format!("{} admin: {user}", if *admin { "granted" } else { "revoked" }),
                json!({"ok": true, "user": user, "admin": admin}),
            );
            Ok(())
        }
        Cmd::UserRevoke { user } => {
            ops::user_revoke(&root, user)?;
            out(j, &format!("user {user} revoked"), json!({"ok": true}));
            Ok(())
        }
        Cmd::Admin { cmd } => match cmd {
            AdminCmd::Users => {
                let users = ops::user_list(&root);
                if j {
                    println!("{}", json!({"users": users}));
                } else {
                    for o in &users {
                        println!("{:<16} {} projects={:<3} keys={}", o.user, if o.admin { "[ADMIN]" } else { "" }, o.projects, o.keys.len());
                        for k in &o.keys {
                            println!("  {:<10} since={}", k.kid, k.since);
                        }
                    }
                }
                Ok(())
            }
            AdminCmd::Status => {
                let v = ops::admin_status(&root)?;
                if j {
                    println!("{v}");
                } else {
                    let services = ops::STATUS_SERVICES
                        .iter()
                        .map(|s| format!("{s}={}", v["services"][s].as_str().unwrap_or("?")))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("services: {services}");
                    for label in ["root", "showroom"] {
                        let d = &v["disk"][label];
                        println!("disk {label}: total={} used={}", d["total"], d["used"]);
                    }
                    println!("mem: available_mb={}", v["mem"]["available_mb"]);
                    let gs = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or("?").to_string();
                    let cons: Vec<String> = v["containers"]
                        .as_array()
                        .map(|a| a.iter().map(|c| format!("{}({})", gs(c, "name"), gs(c, "status"))).collect())
                        .unwrap_or_default();
                    println!("containers: {}", if cons.is_empty() { "-".to_string() } else { cons.join(" ") });
                    if let Some(ts) = v["timers"].as_array() {
                        for t in ts {
                            println!("timer: {} next={}", gs(t, "name"), gs(t, "next"));
                        }
                    }
                }
                Ok(())
            }
        },

        Cmd::Publish { user, project, tag, mem, cpu, container_port } => {
            let o = publish::publish(
                &root,
                user,
                project,
                tag,
                publish::PublishOpts { services: Some(vec![]), env: None, mem: mem.clone(), cpu: *cpu, cport: *container_port }, // 发布=完整期望状态：不带 svc 即清空（2026-10-16 裁决）
            )?;
            out(
                j,
                &format!("published {project} tag {tag} -> {} (port {})", o.url, o.port),
                json!({"ok": true, "url": o.url, "port": o.port}),
            );
            Ok(())
        }
        Cmd::Stop { project } => {
            ops::stop(&root, project)?;
            out(j, &format!("stopped {project}"), json!({"ok": true}));
            Ok(())
        }
        Cmd::Start { project } => {
            ops::start(&root, project)?;
            out(j, &format!("started {project}"), json!({"ok": true}));
            Ok(())
        }
        Cmd::Rm { project } => {
            ops::rm(&root, project)?;
            out(j, &format!("removed {project}"), json!({"ok": true}));
            Ok(())
        }
        Cmd::Rollback { project } => {
            let o = ops::rollback(&root, project)?;
            out(
                j,
                &format!("rolled back {project} -> {} (port {})", o.url, o.port),
                json!({"ok": true, "url": o.url, "port": o.port}),
            );
            Ok(())
        }

        Cmd::Ps => {
            let rows = ops::ps(&root);
            let disabled = middleware::disabled_middleware(&root);
            if j {
                let v: Vec<_> = rows
                    .iter()
                    .map(|(p, c, st)| {
                        let mut row = c.to_json(p, st);
                        if !disabled.is_empty() {
                            row["middleware_disabled"] = json!(disabled);
                        }
                        row
                    })
                    .collect();
                println!("{}", json!({"projects": v}));
            } else {
                println!(
                    "{:<24} {:<12} {:<20} {:<9} {:<5} {:<6} {:<5} {:<10} URL",
                    "PROJECT", "USER", "TAG", "STATUS", "PORT", "MEM", "CPU", "UPDATED"
                );
                for (p, c, st) in rows {
                    println!(
                        "{:<24} {:<12} {:<20} {:<9} {:<5} {:<6} {:<5} {:<10} {}",
                        p, c.user, c.tag, st, c.port, c.mem, c.cpu, c.updated,
                        state::public_url(&p)
                    );
                }
                if !disabled.is_empty() {
                    println!("note: middleware 未启用: {}（对应 env 变量组不再注入）", disabled.join(", "));
                }
            }
            Ok(())
        }
        Cmd::Logs { project, tail } => {
            print!("{}", ops::logs(&root, project, None, *tail)?);
            Ok(())
        }
        Cmd::Env { project } => {
            let m = ops::env_of(&root, project)?;
            let disabled = middleware::disabled_middleware(&root);
            if j {
                let mut v = json!({"env": m});
                if !disabled.is_empty() {
                    v["middleware_disabled"] = json!(disabled);
                }
                println!("{v}");
            } else {
                for (k, v) in m {
                    println!("{k}={v}");
                }
                if !disabled.is_empty() {
                    println!("# middleware 未启用: {}（对应 env 变量组不再注入）", disabled.join(", "));
                }
            }
            Ok(())
        }
        Cmd::Reconcile => {
            for l in ops::reconcile(&root) {
                println!("{l}");
            }
            Ok(())
        }
        Cmd::Autoheal => {
            for l in ops::autoheal(&root) {
                println!("{l}");
            }
            Ok(())
        }
        Cmd::GcImages => {
            let o = ops::gc_images(&root)?;
            if j {
                println!("{}", json!({"ok": true, "output": o}));
            } else {
                print!("{o}");
            }
            Ok(())
        }
        Cmd::Snapshot { kind } => {
            let o = ops::snapshot(kind)?;
            if j {
                println!("{}", json!({"ok": true, "output": o}));
            } else {
                print!("{o}");
            }
            Ok(())
        }
        Cmd::GcRegistry => {
            let n = ops::gc_registry(&root)?;
            out(j, &format!("gc-registry: deleted {n} tags"), json!({"ok": true, "deleted": n}));
            Ok(())
        }
        Cmd::RegistryGc => {
            ops::registry_gc(&root)?;
            out(j, "registry garbage-collect done", json!({"ok": true}));
            Ok(())
        }
        Cmd::Import { user, project, tag } => {
            let (path, _sha) = publish::receive_envelope(&root, std::io::stdin())?;
            let d = publish::import_from_file(&root, user, project, tag, &path)?;
            out(j, &format!("imported digest={d}"), json!({"ok": true, "digest": d}));
            Ok(())
        }
        Cmd::ImportCheck { image_id } => {
            let present = ops::import_check(image_id);
            out(j, &format!("present={present}"), json!({"present": present}));
            Ok(())
        }
        Cmd::InitKeys => {
            let existed = root.join("state/platform_rsa").exists();
            let p = ops::init_keys(&root)?;
            out(
                j,
                &format!("platform rsa: {} ({})", p.display(), if existed { "already present" } else { "generated" }),
                json!({"ok": true, "path": p, "generated": !existed}),
            );
            Ok(())
        }

        Cmd::Setup { apply, lan_host, public_host } => {
            let host_path = middleware::host_env_path(&root);
            let first_run = !host_path.exists();
            let mut host = middleware::HostEnv::load(&root);
            if first_run {
                println!(
                    "host.env 未找到：首跑按探测值生成（LAN_HOST={}；--lan-host/--public-host 可覆写；\
                     frp 凭据与限额可随后手工编辑 {}）",
                    host.lan_host,
                    host_path.display()
                );
            }
            if let Some(h) = lan_host {
                host.lan_host = h.clone();
            }
            if let Some(h) = public_host {
                host.public_host = h.clone();
            }
            let obs = setup::observe(&root, &host);
            let actions = setup::plan_setup(&host, &obs);
            let warnings = setup::plan_warnings(&root, &obs);
            if *apply {
                let report = setup::apply(&root, &actions, false)?;
                if j {
                    let mut v = setup::report_json(&report);
                    v["host_env"] = host.to_json();
                    println!("{v}");
                } else {
                    for (a, ok, detail) in &report.items {
                        println!("  {} {} [{}]", if *ok { "+" } else { "x" }, setup::action_line(a), detail);
                    }
                    println!("changed: {}", report.changed);
                    for w in &report.warnings {
                        println!("warning: {w}");
                    }
                }
            } else if j {
                println!("{}", setup::diff_json(&host, &actions, &warnings));
            } else {
                if actions
                    .iter()
                    .any(|a| matches!(a, setup::Action::WriteFile { path, .. } if *path == host_path))
                {
                    println!("host.env: will create {} (9 keys)", host_path.display());
                }
                let total = setup::plan_setup(&host, &setup::Obs::default()).len();
                if actions.is_empty() {
                    println!("plan: 0 actions needed (clean)");
                } else {
                    println!("plan: {total} actions, {} needed, {} clean", actions.len(), total - actions.len());
                    for a in &actions {
                        println!("  + {}", setup::action_line(a));
                    }
                }
                if !warnings.is_empty() {
                    println!("warnings:");
                    for w in &warnings {
                        println!("  {w}");
                    }
                }
                if !actions.is_empty() {
                    println!("run `showroomctl setup --apply` to apply");
                }
            }
            Ok(())
        }

        Cmd::Middleware { cmd } => match cmd {
            MiddlewareCmd::Status => {
                let v = middleware::middleware_status(&root)?;
                let drift = middleware::redis_drift(&root);
                if j {
                    let mut o = v;
                    o["redis_drift"] = drift.unwrap_or(Value::Null);
                    println!("{o}");
                } else {
                    let rows = v["middleware"].as_array().cloned().unwrap_or_default();
                    for m in &rows {
                        println!(
                            "{:<10} expected={:<9} actual={}",
                            m["name"].as_str().unwrap_or("?"),
                            m["expected"].as_str().unwrap_or("?"),
                            m["actual"].as_str().unwrap_or("?")
                        );
                        if m["name"] == "redis" {
                            println!(
                                "{:<10} databases: instance={:?} policy={}",
                                "", m["databases"]["instance"], m["databases"]["policy"]
                            );
                        }
                    }
                    if let Some(d) = middleware::redis_drift(&root) {
                        println!(
                            "drift: {} expected={} actual={} → 建议: {}",
                            d["item"], d["expected"], d["actual"], d["advice"]
                        );
                    }
                }
                Ok(())
            }
            MiddlewareCmd::Enable { name } => {
                let r = middleware::middleware_enable(&root, name)?;
                if j {
                    println!("{}", setup::report_json(&r));
                } else {
                    for (a, ok, detail) in &r.items {
                        println!("  {} {} [{}]", if *ok { "+" } else { "x" }, setup::action_line(a), detail);
                    }
                    println!("changed: {}", r.changed);
                    for w in &r.warnings {
                        println!("warning: {w}");
                    }
                }
                Ok(())
            }
            MiddlewareCmd::Disable { name, force } => {
                let v = middleware::middleware_disable(&root, name, *force)?;
                if j {
                    println!("{v}");
                } else {
                    let affected =
                        v["affected_projects"].as_array().map(Vec::len).unwrap_or(0);
                    println!("disabled {name}（受影响项目 {affected} 个）");
                    if let Some(notes) = v["notes"].as_array() {
                        for n in notes {
                            if let Some(s) = n.as_str() {
                                println!("  {s}");
                            }
                        }
                    }
                }
                Ok(())
            }
            MiddlewareCmd::Set { name, kv, confirm } => {
                let (k, val) = kv.split_once('=').ok_or_else(|| {
                    ApiError::new("E_NAME", format!("kv 须形如 databases=N（got '{kv}'）"))
                })?;
                let v = middleware::middleware_set(&root, name, k, val, *confirm)?;
                if j {
                    println!("{v}");
                } else if let Some(p) = v["prompt"].as_str() {
                    println!("{p}");
                } else {
                    println!("{v}");
                }
                Ok(())
            }
        },

        Cmd::Upgrade { bin } => {
            let v = setup::upgrade(bin.as_deref())?;
            if j {
                println!("{v}");
            } else {
                let rolled = v["rolled_back"].as_bool().unwrap_or(false);
                let backup = v["backup"].as_str().unwrap_or("-");
                let converged = v["converged"].as_u64().unwrap_or(0);
                if rolled {
                    println!("upgrade: restart 后非 active，已回滚旧二进制（backup 已消耗）");
                } else {
                    println!("upgraded: backup={backup} converged={converged}");
                }
                if let Some(notes) = v["notes"].as_array() {
                    for n in notes {
                        if let Some(s) = n.as_str() {
                            println!("  {s}");
                        }
                    }
                }
            }
            Ok(())
        }
    }
}
