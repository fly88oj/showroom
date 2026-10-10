//! showroomctl：主机侧唯一管理工具（SPEC §3 HTTP 控制面 + §7 全部运维行为）。
//! 子命令是唯一对外接缝；内部 state / dockerops / publish / ops / serve 深模块。

mod dbmaint;
mod dockerops;
mod ops;
mod publish;
mod serve;
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
    /// 登记发布者公钥 + 生成 registry 凭据（bcrypt 入 registry.htpasswd，明文 0600 入 reg.cred）
    OwnerAdd {
        name: String,
        #[arg(long)]
        pubkey: String,
        #[arg(long)]
        replace_key: bool,
    },
    /// 删除发布者某把公钥
    OwnerKeyRm { owner: String, kid: String },
    /// 吊销发布者（auth 与 registry 登录全失效，已发布展项不动）
    OwnerRevoke { owner: String },
    /// 发布/滚动更新（管理员直发通道，serve 的 /_api/publish 复用同一内核）
    Publish {
        owner: String,
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
    Import { owner: String, project: String, tag: String },
    /// 检查镜像是否已在本机 docker（present true/false）
    ImportCheck { image_id: String },
    /// 生成平台 RSA 对（serve 启动缺失时自动生成；已存在不覆盖）
    InitKeys,
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

        Cmd::OwnerAdd { name, pubkey, replace_key } => {
            let kid = ops::owner_add(&root, name, pubkey, *replace_key)?;
            out(
                j,
                &format!(
                    "owner {name} added: kid={kid}; registry creds in state/owners/{name}/reg.cred (user {name})"
                ),
                json!({"ok": true, "owner": name, "kid": kid}),
            );
            Ok(())
        }
        Cmd::OwnerKeyRm { owner, kid } => {
            ops::owner_key_rm(&root, owner, kid)?;
            out(j, &format!("removed {owner}/{kid}"), json!({"ok": true}));
            Ok(())
        }
        Cmd::OwnerRevoke { owner } => {
            ops::owner_revoke(&root, owner)?;
            out(j, &format!("owner {owner} revoked"), json!({"ok": true}));
            Ok(())
        }

        Cmd::Publish { owner, project, tag, mem, cpu, container_port } => {
            let o = publish::publish(
                &root,
                owner,
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
                    "{:<24} {:<12} {:<20} {:<9} {:<5} {:<6} {:<5} {:<10} {}",
                    "PROJECT", "OWNER", "TAG", "STATUS", "PORT", "MEM", "CPU", "UPDATED", "URL"
                );
                for (p, c, st) in rows {
                    println!(
                        "{:<24} {:<12} {:<20} {:<9} {:<5} {:<6} {:<5} {:<10} {}",
                        p, c.owner, c.tag, st, c.port, c.mem, c.cpu, c.updated,
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
        Cmd::Import { owner, project, tag } => {
            let (path, _sha) = publish::receive_envelope(&root, std::io::stdin())?;
            let d = publish::import_from_file(&root, owner, project, tag, &path)?;
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
