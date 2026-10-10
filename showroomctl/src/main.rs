//! showroomctl：主机侧唯一管理工具（SPEC §3 HTTP 控制面 + §7 全部运维行为）。
//! 子命令是唯一对外接缝；内部 state / dockerops / publish / ops / serve 深模块。

mod dbmaint;
mod dockerops;
// plan-15 步 1-3 先落纯函数与编排内核，步 4 CLI/serve 接线后移除本 allow
#[allow(dead_code)]
mod middleware;
mod ops;
mod publish;
mod serve;
#[allow(dead_code)]
mod setup;
mod state;
mod streams;
mod unpack;

use clap::{Parser, Subcommand};
use serde_json::json;

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
    /// 登记发布者公钥 + 生成 registry 凭据（bcrypt 入 registry.htpasswd，明文 0600 入 reg.cred）；
    /// --admin 同时授管理员钥匙（SPEC §12.2，首个管理员引导用）
    UserAdd {
        name: String,
        /// 发布者公钥串（showroom keygen 打印的那串；省略时给出提示）
        #[arg(long)]
        pubkey: Option<String>,
        #[arg(long)]
        replace_key: bool,
        /// 授予管理员钥匙（可远程授权/发卡）
        #[arg(long)]
        admin: bool,
    },
    /// 删除发布者某把公钥（admin 标记随钥匙一并清）
    UserKeyRm { user: String, kid: String },
    /// 授/收管理员权限（--admin true|false，SPEC §12.2）
    KeyAdmin {
        user: String,
        kid: String,
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
}

#[derive(Subcommand)]
enum AdminCmd {
    /// 账号 / 钥匙（kid+admin+登记时间）/ 项目数清单
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

        Cmd::UserAdd { name, pubkey, replace_key, admin } => {
            let pubkey = pubkey.as_deref().ok_or_else(|| {
                ApiError::new("E_NAME", format!("缺少 --pubkey <公钥串>（发布者跑 showroom keygen 打印的那串；示例：showroomctl user-add {name} --pubkey <串> --admin）"))
            })?;
            let added = ops::user_add(&root, name, pubkey, *admin, *replace_key)?;
            out(
                j,
                &format!(
                    "user {name} added: kid={}{}; registry creds in state/users/{name}/reg.cred (user {name})",
                    added.kid,
                    if *admin { " [ADMIN]" } else { "" }
                ),
                json!({"ok": true, "user": name, "kid": added.kid, "admin": admin}),
            );
            Ok(())
        }
        Cmd::UserKeyRm { user, kid } => {
            ops::user_key_rm(&root, user, kid)?;
            out(j, &format!("removed {user}/{kid}"), json!({"ok": true}));
            Ok(())
        }
        Cmd::KeyAdmin { user, kid, admin } => {
            ops::key_set_admin(&root, user, kid, *admin)?;
            out(
                j,
                &format!("{} admin: {user}/{kid}", if *admin { "granted" } else { "revoked" }),
                json!({"ok": true, "user": user, "kid": kid, "admin": admin}),
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
                        println!("{:<16} projects={:<3} keys={}", o.user, o.projects, o.keys.len());
                        for k in &o.keys {
                            println!("  {:<10} {} since={}", k.kid, if k.admin { "[ADMIN]" } else { "      " }, k.since);
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
                publish::PublishOpts { mem: mem.clone(), cpu: *cpu, cport: *container_port },
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
            if j {
                let v: Vec<_> = rows.iter().map(|(p, c, st)| c.to_json(p, st)).collect();
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
            }
            Ok(())
        }
        Cmd::Logs { project, tail } => {
            print!("{}", ops::logs(&root, project, *tail)?);
            Ok(())
        }
        Cmd::Env { project } => {
            let m = ops::env_of(&root, project)?;
            if j {
                println!("{}", json!({"env": m}));
            } else {
                for (k, v) in m {
                    println!("{k}={v}");
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
    }
}
