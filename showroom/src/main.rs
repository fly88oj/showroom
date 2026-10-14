//! showroom：Showroom 发布者 CLI。
//! push/publish/ps/url/logs/env/rollback/stop/start/rm/doctor，全部走 SR-SIG1 签名 API。
//! §11 v1.4：无槽位模型——project 即展项唯一键，URL 恒 /<project>/；publish 省略项目名时自动随机。

mod api;
mod auth;
mod config;
mod db;

use std::fs;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use api::{Api, Which};
use config::Config;
use showroom_common::names::{check_project, check_tag, random_project};
use showroom_common::UserKey;

#[derive(Parser)]
#[command(name = "showroom", about = "Showroom 发布者 CLI（push/publish/查看/运维）", version)]
struct Cli {
    /// 强制服务端通道：lan 或 public（默认自动探测，优先 lan）
    #[arg(long, global = true, value_enum)]
    server: Option<Which>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Via {
    Auto,
    Registry,
    Import,
}

#[derive(Subcommand)]
enum Cmd {
    /// 生成/重置本机签名密钥（调试用：排查签名问题、强制换钥）；日常登录无需手动执行
    /// （showroom login 自动生成并绑定，B7 统一账号密码认证）
    #[command(hide = true)]
    Keygen {
        #[arg(long)]
        force: bool,
    },
    /// 查看/更新配置（仅更新给出的字段；无参数时打印当前配置）
    Config {
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        server_lan: Option<String>,
        /// 传空串清除
        #[arg(long)]
        server_public: Option<String>,
        #[arg(long)]
        registry_user: Option<String>,
        #[arg(long)]
        registry_password: Option<String>,
    },
    /// 推送镜像（auto：lan 可达走 registry，否则走加密 import 通道）
    Push {
        project: String,
        tag: String,
        #[arg(long, value_enum, default_value = "auto")]
        via: Via,
    },
    /// 安装 Agent Skill 到 ~/.agents/skills/showroom-publisher/
    Skill {
        /// 子动作：install（默认）
        #[arg(default_value = "install")]
        action: String,
    },
    /// 一键部署（SPEC §12.1）= push（--via auto 语义）+ publish --wait + 打印最终 URL；
    /// 任一步失败即停，报错标明已到第几步
    Deploy {
        /// [项目名] tag：给 1 个参数视为 tag（自动生成随机 8 位项目名并醒目展示），2 个为 项目名 tag
        #[arg(required = true, num_args = 1..=2)]
        project_tag: Vec<String>,
        #[arg(long)]
        mem: Option<String>,
        #[arg(long, value_parser = parse_cpu)]
        cpu: Option<f64>,
        #[arg(long)]
        container_port: Option<u16>,
    },
    /// 发布/更新展项（一个参数=tag，项目名自动随机；两个=项目名 tag）
    Publish {
        /// [项目名] tag：给 1 个参数视为 tag（自动生成随机 8 位项目名并醒目展示），2 个为 项目名 tag
        #[arg(required = true, num_args = 1..=2)]
        project_tag: Vec<String>,
        #[arg(long)]
        mem: Option<String>,
        /// CPU 配额（核数，如 0.5、2；与服务端 cpu 数字字段对齐）
        #[arg(long, value_parser = parse_cpu)]
        cpu: Option<f64>,
        #[arg(long)]
        container_port: Option<u16>,
        /// 等待 status=running（2s 轮询，90s 超时）后打印完整 URL
        #[arg(long)]
        wait: bool,
    },
    /// 列出所有展项
    Ps {
        #[arg(long)]
        json: bool,
    },
    /// 列出自己的镜像 <user>/<project>:<tag>（admin 钥匙列出全部）
    Images {
        #[arg(long)]
        json: bool,
    },
    /// 查看服务端磁盘空间（可用/预留/总量，可用行含已扣预留值）
    Space {
        #[arg(long)]
        json: bool,
    },
    /// 删除自己的镜像 <user/project:tag>（服务端内联 GC，空间立即回收）
    RmImage {
        image: String,
    },
    /// 打印展项 URL（不开浏览器）
    Url {
        project: String,
    },
    /// 查看日志
    Logs {
        project: String,
        #[arg(long, default_value_t = 100)]
        tail: u32,
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// 查看注入容器的环境变量（DB/Redis 连接信息，SPEC §6）
    Env {
        project: String,
    },
    /// 回滚到上一个 tag
    Rollback {
        project: String,
    },
    /// 停止展项
    Stop {
        project: String,
    },
    /// 启动展项
    Start {
        project: String,
    },
    /// 删除展项（需 --yes）
    Rm {
        project: String,
        #[arg(long)]
        yes: bool,
    },
    /// 数据库维护（SPEC §9）：sql/tables/preview/dump/restore/reset
    Db {
        #[command(subcommand)]
        cmd: db::DbCmd,
    },
    /// 登录（plan-16 §4：账号+密码一次，密码经 OPAQUE 盲化永不上线）——绑定本机新钥匙、
    /// 自动配好 registry 凭据，此后全部子命令免密码（gh/ssh 模型）
    Login {
        /// 账号（缺省用 config 里的 user）
        account: Option<String>,
        /// 服务端地址（http://host；缺省用 config 的 server_lan/server_public）
        #[arg(long)]
        server_lan: Option<String>,
        /// 临时会话：不落任何盘，进程退出即弃（服务端靠 keep-N GC 收尾；一次性/共享机器用）
        #[arg(long)]
        ephemeral: bool,
    },
    /// 退出登录（清本地钥匙与 config 身份，并远程注销本机钥匙——B7 后恒为完整登出）
    Logout,
    /// 体检：密钥/配置/服务端可达/签名自测/docker
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    // 密钥过期自动重登+原命令重试（§密钥生命周期）：E_AUTH+expired → 密码重登 → 自动重执行原命令
    let mut retried_reauth = false;
    loop {
        match execute(&cli) {
            Ok(()) => break,
            Err(e) => {
                let err_str = e.to_string();
                if !retried_reauth && crate::api::is_key_expired(&err_str) {
                    retried_reauth = true;
                    eprintln!("\n━━━ 本机密钥已过期（30 分钟无活动自动过期）━━━");
                    eprintln!("请输入密码获取新密钥，原命令将自动继续执行。\n");
                    match crate::auth::reauth_on_expired(
                        &crate::config::Config::load().unwrap_or_default(), None
                    ) {
                        Ok(_) => {
                            eprintln!("━━━ 重新登录成功，继续执行 ━━━\n");
                            continue; // 重试原命令
                        }
                        Err(re_err) => {
                            eprintln!("重新登录失败：{re_err}");
                            std::process::exit(1);
                        }
                    }
                }
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn execute(cli: &Cli) -> Result<(), String> {
    // 密钥生命周期：main 的 retry loop 在 E_AUTH+expired 时自动重登后重新调用此函数
    run(cli)
}

// ———— publish/deploy 共用的 positional 解析 ————

/// positional（clap num_args=1..=2）→ (Option<project>, tag)：末位恒为 tag，前位（有则）为项目名。
/// 单参数即「项目名省略=自动随机」（§11/§12.1）。
fn split_project_tag(args: &[String]) -> (Option<String>, String) {
    match args.split_last() {
        Some((tag, head)) => (head.first().cloned(), tag.clone()),
        None => (None, String::new()), // 不可达：clap required + num_args ≥1
    }
}

/// None → 生成随机 8 位项目名并醒目打印；Some 原样返回。
fn pick_project(project: Option<String>) -> String {
    match project {
        Some(p) => p,
        None => {
            let p = random_project();
            println!("项目名：{p}（已自动生成，可显式指定）");
            p
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let cfg = Config::load()?;
    match &cli.cmd {
        Cmd::Keygen { force } => keygen(*force, &cfg),
        Cmd::Config { user, server_lan, server_public, registry_user, registry_password } => {
            config_cmd(&cfg, user, server_lan, server_public, registry_user, registry_password)
        }
        Cmd::Push { project, tag, via } => push(&cfg, cli.server, project, tag, *via),
        Cmd::Skill { action } => {
            if action != "install" { return Err(format!("未知动作：{action}（当前仅支持 install）")); }
            let skill_dir = std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".agents/skills/showroom-publisher"))
                .map_err(|_| "无法确定 HOME 目录")?;
            std::fs::create_dir_all(&skill_dir).map_err(|e| format!("创建目录失败：{e}"))?;
            // SKILL.md 从 /_bootstrap 下载或内嵌
            const SKILL_MD: &str = r##"---
name: showroom-publisher
description: 把 Docker 镜像形式的 Demo/演示程序发布到 showroom 展示平台（dejoy-node，内网 your-host / 公网 your-public-host:port）。凡用户提到发布 demo、部署演示环境、showroom、发布展项、demo 上线、更新 demo 版本、看 demo 日志、回滚 demo、管理 demo 数据库——一律用本 skill 的 showroom CLI 完成，不要手写 curl 或 docker 命令。
---

# Showroom 发布

所有操作通过 `showroom` CLI 完成（工具优先，不要自己拼 HTTP/签名）。CLI 未安装时先跑 `/home/jeffrey/.agents/skills/showroom-publisher/scripts/install-cli.sh`（需内网可达 your-host）。

## 0. 前置检查（每次任务开始先跑）

```bash
showroom doctor
```

- 全绿 → 直接干活。
- 提示缺密钥/未登录 → `showroom login <账号>`（密码一次、密钥常驻）；没有账号 → 找平台管理员 `showroomctl user-add <名> --gen-password` 发放（得到账号与初始密码），再 `showroom login`。
- 提示缺 config → 让用户提供账号名和 registry 密码，然后：
  `showroom config --user <名> --server-lan http://your-host --server-public http://your-public-host:port --registry-user <名> --registry-password <密码>`
  （`--server-public` 可选：只有需要从公网发布时才配。）
- **签名自测 ✗**（密钥/config 都在、其余全 ✓）→ 钥匙被服务端删除/GC 或本地时钟偏差：重新 `showroom login <账号>` 重绑本机钥匙后重跑 doctor；仍 ✗ 再查时钟。此时不要 push/publish（会 E_AUTH）。

## 1. 镜像规范（构建前自检，不满足会发布失败或页面 404）

- 单进程 HTTP 服务，默认监听容器内 **8080**（不同则发布时带 `--container-port`）。
- 支持子路径部署：资源用相对路径引用；需要绝对路径的框架消费 `SHOWROOM_BASE_PATH` 环境变量（值恒为 `/<项目名>/`）。
- 不要写死数据库配置——发布时自动注入（PostgreSQL/Redis）：`DB_HOST DB_PORT DB_NAME DB_USER DB_PASSWORD DATABASE_URL REDIS_HOST REDIS_PORT REDIS_DB REDIS_URL SHOWROOM_PROJECT SHOWROOM_BASE_PATH`。运行 `showroom env <项目>` 可随时查看。
- 建议在 Dockerfile 里写 HEALTHCHECK。
- 本地镜像名随意，tag 用不可变命名（版本号或 git sha，禁止 latest）。

## 2. 发布（标准流）

```bash
showroom push   [项目] <tag>              # 项目名可省略=自动随机 8 位；上传走 zstd-19 压缩+加密
showroom publish [项目] <tag> --wait      # 项目名可省略=自动随机并打印；起容器→健康→打印 URL
```

- URL 规则：`http://your-host/<项目名>/`（公网同路径换域名）。项目名规则 `^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$`。
- **多版本并存 = 多个项目**（自命名区分，如 `demo` / `demo-pr42`）；要替换就同名重发（数据库数据保留）。
- 内存为「内存需求」语义：系统按 可用内存−预留(1536MB) 是否满足需求做准入，不足报 E_QUOTA（消息含三个数字）。

## 3. 日常操作

```bash
showroom ps                          # 全部展项（--json 可解析）
showroom logs <项目>                 # 日志
showroom env <项目>                  # 数据库/Redis 连接信息
showroom rollback <项目>             # 回上一版本（需用户确认再执行）
showroom stop|start <项目>
showroom rm <项目> --yes             # 删除（破坏性：必须先向用户确认）
```

## 4. 数据库维护（演示数据/Schema 自助管理）

```bash
showroom db sql <项目> [--file f.sql] [--confirm]     # DROP/TRUNCATE/DELETE/ALTER 需 --confirm
showroom db tables <项目>
showroom db preview <项目> <表> [--limit 50]
showroom db dump <项目> -o x.dump                     # zstd-19 高压缩加密备份
showroom db restore <项目> <file.dump> --confirm      # 兼容 zstd 压缩包与原生格式
showroom db reset <项目> --confirm                    # 清库重建（破坏性：先确认）
```

要准备「期望演示效果」：SQL 建表灌数据 → dump 存档 → 随时 restore 还原现场。

## 5. 硬规则

- `rm`/`rollback`/`db reset`/`db restore` 执行前必须获得用户明确确认。
- 私钥/seed、registry 密码不得写入仓库、日志或聊天记录。
- 保留名：v2/_api/_bootstrap/_console。
- 平台侧出问题（502/路由 404 等）：报告用户，让管理员上主机处理（`ssh dejoy@your-host`，服务名 showroom-api/caddy/docker；公网隧道在 showroom-api 内，排查看 journalctl -u showroom-api 的 tunnel 行），不要尝试直接改主机。
"##;
            let skill_path = skill_dir.join("SKILL.md");
            std::fs::write(&skill_path, SKILL_MD).map_err(|e| format!("写入失败：{e}"))?;
            println!("Skill 已安装到 {}", skill_path.display());
            println!("Agent 将在用户提到「发布 demo」「部署演示环境」等时自动使用此 skill。");
            Ok(())
        }
        Cmd::Deploy { project_tag, mem, cpu, container_port } => {
            let (project, tag) = split_project_tag(project_tag);
            let project = pick_project(project);
            deploy(&cfg, cli.server, &project, &tag, mem.as_deref(), *cpu, *container_port)
        }
        Cmd::Publish { project_tag, mem, cpu, container_port, wait } => {
            let (project, tag) = split_project_tag(project_tag);
            let project = pick_project(project);
            publish(&cfg, cli.server, &project, &tag, mem.as_deref(), *cpu, *container_port, *wait)?;
            Ok(())
        }
        Cmd::Ps { json } => ps(&cfg, cli.server, *json),
        Cmd::Images { json } => images_cmd(&cfg, cli.server, *json),
        Cmd::Space { json } => space_cmd(&cfg, cli.server, *json),
        Cmd::RmImage { image } => rm_image(&cfg, cli.server, image),
        Cmd::Url { project } => url_cmd(&cfg, cli.server, project),
        Cmd::Logs { project, tail, follow } => {
            logs_cmd(&cfg, cli.server, project, *tail, *follow)
        }
        Cmd::Env { project } => env_cmd(&cfg, cli.server, project),
        Cmd::Rollback { project } => {
            project_action(&cfg, cli.server, "/_api/rollback", project)
        }
        Cmd::Stop { project } => project_action(&cfg, cli.server, "/_api/stop", project),
        Cmd::Start { project } => project_action(&cfg, cli.server, "/_api/start", project),
        Cmd::Rm { project, yes } => {
            if !yes {
                return Err(format!("删除不可逆，需显式确认：showroom rm {project} --yes"));
            }
            project_action(&cfg, cli.server, "/_api/rm", project)
        }
        Cmd::Db { cmd } => db::run(&cfg, cli.server, cmd),
        Cmd::Login { account, server_lan, ephemeral } => {
            auth::login(&cfg, account.as_deref(), server_lan.clone(), *ephemeral)
        }
        Cmd::Logout => auth::logout(&cfg),
        Cmd::Doctor => doctor(&cfg),
    }
}

// ———— keygen / config ————

fn keygen(force: bool, cfg: &Config) -> Result<(), String> {
    let p = cfg.key_file();
    if p.exists() && !force {
        return Err(format!("密钥 {} 已存在，拒绝覆盖（--force 强制）", p.display()));
    }
    let k = UserKey::generate();
    k.save_seed(&p).map_err(|e| format!("写密钥失败：{e}"))?;
    println!("密钥: {}", p.display());
    println!("公钥: {}", k.pub_string());
    println!("调试用途（生成/重置本机签名密钥）：日常登录用 showroom login <账号>（密码一次、密钥自动绑定）。");
    Ok(())
}

fn config_cmd(
    cfg: &Config,
    user: &Option<String>,
    server_lan: &Option<String>,
    server_public: &Option<String>,
    registry_user: &Option<String>,
    registry_password: &Option<String>,
) -> Result<(), String> {
    let check_url = |u: &str| -> Result<(), String> {
        if !u.is_empty() && !u.starts_with("http") {
            Err(format!("地址需以 http(s):// 开头：{u}"))
        } else {
            Ok(())
        }
    };
    let mut c = cfg.clone();
    if let (None, None, None, None, None) = (user, server_lan, server_public, registry_user, registry_password) {
        println!("{}", toml::to_string_pretty(&c).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if let Some(o) = user {
        c.user = o.clone();
    }
    if let Some(u) = server_lan {
        check_url(u)?;
        c.server_lan = if u.is_empty() { None } else { Some(u.clone()) };
    }
    if let Some(u) = server_public {
        check_url(u)?;
        c.server_public = if u.is_empty() { None } else { Some(u.clone()) };
    }
    if let Some(u) = registry_user {
        c.registry.user = u.clone();
    }
    if let Some(p) = registry_password {
        c.registry.password = p.clone();
    }
    c.save()?;
    println!("已保存 {}", config::config_path().display());
    Ok(())
}

// ———— push ————

/// import 通道 zstd 压缩级别（压缩率优先，与服务端 dump 管线一致）。
const IMPORT_ZSTD_LEVEL: i32 = 19;

fn push(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    tag: &str,
    via: Via,
) -> Result<(), String> {
    check_project(project)?;
    check_tag(tag)?;
    let t0 = Instant::now();
    let (base, which) = select(cfg, force)?;
    match via {
        Via::Registry => push_registry(cfg, project, tag, t0),
        Via::Import => push_import(cfg, base, project, tag, t0),
        Via::Auto if which == Which::Lan => match push_registry(cfg, project, tag, t0) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("showroom: registry 快通道失败（{e}），自动切换加密 import 通道");
                push_import(cfg, base, project, tag, t0)
            }
        },
        Via::Auto => push_import(cfg, base, project, tag, t0),
    }
}

fn select(cfg: &Config, force: Option<Which>) -> Result<(String, Which), String> {
    // 探测需签名，故先加载密钥（失败即报错，引导登录）
    let key = UserKey::from_seed_file(&cfg.key_file())
        .map_err(|e| format!("读取密钥 {} 失败：{e}（先运行 showroom login <账号>）", cfg.key_file().display()))?;
    api::pick_server(
        cfg.server_lan.as_deref(),
        cfg.server_public.as_deref(),
        force,
        api::signed_probe(&cfg.user, &key),
    )
}

/// `http://your-host.example.com/base` → `your-host.example.com`（registry 经服务端 :80 /v2 代理，SPEC §4）。
fn registry_host(server: &str) -> String {
    let rest = server.split("://").nth(1).unwrap_or(server);
    rest.split('/').next().unwrap_or(rest).trim_end_matches('/').to_string()
}

fn docker_run(args: &[&str]) -> Result<(), String> {
    let st = Command::new("docker")
        .args(args)
        .status()
        .map_err(|e| format!("无法启动 docker：{e}（push 需要 docker 命令行）"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("docker {} 失败（退出码 {:?}）", args[0], st.code()))
    }
}

fn docker_capture(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("无法启动 docker：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker {} 失败: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// docker login：密码只走 stdin，绝不进 argv/环境变量。
fn docker_login(host: &str, user: &str, password: &str) -> Result<(), String> {
    let mut child = Command::new("docker")
        .args(["login", host, "--username", user, "--password-stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 docker login：{e}"))?;
    if let Some(mut sin) = child.stdin.take() {
        sin.write_all(password.as_bytes()).ok();
    }
    let st = child.wait().map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err("docker login 失败（检查 --registry-user/--registry-password）".into())
    }
}

fn push_registry(cfg: &Config, project: &str, tag: &str, t0: Instant) -> Result<(), String> {
    let lan = cfg
        .server_lan
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("via=registry 需要 server_lan（showroom config --server-lan URL）")?;
    if cfg.registry.password.is_empty() {
        return Err("未配置 registry 密码：showroom config --registry-password …".into());
    }
    let user = if cfg.registry.user.is_empty() { cfg.user.clone() } else { cfg.registry.user.clone() };
    let host = registry_host(lan);
    let src = format!("{project}:{tag}");
    // docker 仓库名强制小写（§11 项目名允许大写 → repo 路径转小写，与服务端 image_ref 对齐）
    let reg_tag = format!("{host}/{}/{}:{tag}", cfg.user, project.to_ascii_lowercase());
    docker_run(&["tag", &src, &reg_tag])?;
    docker_login(&host, &user, &cfg.registry.password)?;
    docker_run(&["push", &reg_tag])?;
    println!("通道: registry（{host}）");
    println!("registry tag: {reg_tag}");
    println!("耗时: {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}

fn push_import(
    cfg: &Config,
    base: String,
    project: &str,
    tag: &str,
    t0: Instant,
) -> Result<(), String> {
    let api = Api::open(base, cfg)?;
    let src = format!("{project}:{tag}");
    let image_id = docker_capture(&["image", "inspect", &src, "--format", "{{.Id}}"])?;
    let present = api
        .signed_post_json("/_api/import-check", &json!({ "image_id": image_id }))?["present"]
        .as_bool()
        .unwrap_or(false);

    let mut uploaded: u64 = 0;
    if present {
        println!("镜像已在服务端登记（{image_id}），跳过上传");
    } else {
        let platform_pub = api::load_platform_pub()?;
        let tmp = std::env::temp_dir().join(format!("showroom-import-{}.bin", std::process::id()));
        // docker save | zstd-19 | seal → 临时文件；失败也要删（ponytail: 用闭包收口清理）
        let work = (|| -> Result<(), String> {
            let mut child = Command::new("docker")
                .args(["save", &src])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| format!("无法启动 docker save：{e}"))?;
            let z = zstd::stream::read::Encoder::new(child.stdout.take().unwrap(), IMPORT_ZSTD_LEVEL)
                .map_err(|e| format!("zstd 初始化失败：{e}"))?;
            let f = fs::File::create(&tmp).map_err(|e| format!("写临时文件失败: {e}"))?;
            uploaded = showroom_common::seal(&platform_pub, z, f)
                .map_err(|e| format!("加密信封失败：{e}"))?;
            if !child.wait().map_err(|e| e.to_string())?.success() {
                return Err("docker save 失败".into());
            }
            api.signed_post_file(&api::import_query(project, tag), &tmp)?;
            Ok(())
        })();
        let _ = fs::remove_file(&tmp);
        work?;
    }
    println!("通道: import（{}）", api.base);
    if uploaded > 0 {
        println!("上传: {uploaded} 字节（加密信封）");
    }
    println!("耗时: {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}

// ———— publish / 查询 ————

/// --cpu 解析：正有限 f64（与服务端 PubReq.cpu: Option<f64> 对齐；docker --cpus 须为正）。
fn parse_cpu(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("E_NAME: 非法 cpu “{s}”：须为正数（如 0.5、2）"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(format!("E_NAME: 非法 cpu “{s}”：须为正数（如 0.5、2）"))
    }
}

#[allow(clippy::too_many_arguments)] // 展开自 publish/deploy 共用的参数组（§12.1）
fn publish(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    tag: &str,
    mem: Option<&str>,
    cpu: Option<f64>,
    container_port: Option<u16>,
    wait: bool,
) -> Result<String, String> {
    check_project(project)?;
    check_tag(tag)?;
    let api = Api::connect(cfg, force)?;
    let mut body = json!({ "project": project, "tag": tag });
    if let Some(x) = mem {
        body["mem"] = json!(x);
    }
    if let Some(x) = cpu {
        body["cpu"] = json!(x);
    }
    if let Some(x) = container_port {
        body["container_port"] = json!(x);
    }
    let v = api
        .signed_post_json("/_api/publish", &body)?;
    let url = match v["url"].as_str().filter(|s| !s.is_empty()) {
        Some(u) => {
            println!("发布成功: {u}");
            u.to_string()
        }
        None => {
            println!("发布成功");
            String::new()
        }
    };
    if wait {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut last = String::new();
        loop {
            let projects = api
                .signed_get("/_api/projects")?;
            match project_status(&projects, project) {
                Some(st) if st == "running" => {
                    let final_url = api::project_url(&api.base, project);
                    println!("{final_url}");
                    return Ok(final_url);
                }
                Some(st) => {
                    if st != last {
                        eprintln!("状态: {st}");
                        last = st;
                    }
                }
                None => eprintln!("等待 {project} 出现…"),
            }
            if Instant::now() >= deadline {
                return Err("等待 status=running 超时（90s）".into());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    Ok(url)
}

// ———— deploy（SPEC §12.1：push + publish --wait + URL 大字，任一步失败即停）————

/// 失败步骤报错前缀：「deploy: 第 N 步(name) 失败：原始错误」（部署者据此知道停在哪步）。
fn deploy_err(step: u8, name: &str, e: &str) -> String {
    format!("deploy: 第 {step} 步({name}) 失败：{e}")
}

/// 最终 URL 大字横幅（成功收尾，醒目展示）。
fn banner(url: &str) -> String {
    let bar = "═".repeat(url.chars().count().max(20) + 4);
    format!("\n{bar}\n  部署成功\n  {url}\n{bar}")
}

fn deploy(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    tag: &str,
    mem: Option<&str>,
    cpu: Option<f64>,
    container_port: Option<u16>,
) -> Result<(), String> {
    // 复用 push/publish 现有函数（不复制逻辑）；push 内部自带 --via auto 的探测与回落
    if let Err(e) = push(cfg, force, project, tag, Via::Auto) {
        return Err(deploy_err(1, "push", &e));
    }
    match publish(cfg, force, project, tag, mem, cpu, container_port, true) {
        Ok(url) => {
            println!("{}", banner(&url));
            Ok(())
        }
        Err(e) => Err(deploy_err(2, "publish", &e)),
    }
}

fn project_status(projects: &Value, project: &str) -> Option<String> {
    projects["projects"].as_array()?.iter()
        .find(|e| e["project"].as_str() == Some(project))
        .and_then(|e| e["status"].as_str().map(str::to_string))
}

fn ps(cfg: &Config, force: Option<Which>, json_out: bool) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get("/_api/projects")?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v["projects"]).unwrap_or_default());
        return Ok(());
    }
    let arr = v["projects"].as_array().cloned().unwrap_or_default();
    println!("{:<26} {:<10} {:<26} USER", "PROJECT", "STATUS", "TAG");
    for e in &arr {
        let g = |k: &str| e[k].as_str().unwrap_or("");
        println!(
            "{:<26} {:<10} {:<26} {}",
            trunc(g("project"), 25),
            trunc(g("status"), 9),
            trunc(g("tag"), 25),
            g("user")
        );
    }
    Ok(())
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect()
    } else {
        s.to_string()
    }
}

// ———— 镜像与磁盘（SPEC §14：images/space/rm-image，走既有签名 GET/POST）————

fn images_cmd(cfg: &Config, force: Option<Which>, json_out: bool) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get("/_api/images")?;
    if json_out {
        println!("{v}");
        return Ok(());
    }
    let arr = v["images"].as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("（无镜像；先 showroom push <PROJECT> <TAG>）");
        return Ok(());
    }
    for e in &arr {
        println!("{}", e.as_str().unwrap_or_default());
    }
    Ok(())
}

fn space_cmd(cfg: &Config, force: Option<Which>, json_out: bool) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get("/_api/space")?;
    if json_out {
        println!("{v}");
        return Ok(());
    }
    let g = |k: &str| v[k].as_u64().unwrap_or(0);
    let (avail, reserve, total) = (g("available_mb"), g("reserve_mb"), g("total_mb"));
    println!("可用空间：{avail} MB（已扣预留 {} MB）", avail.saturating_sub(reserve));
    println!("预留：{reserve} MB");
    println!("总量：{total} MB");
    Ok(())
}

fn rm_image(cfg: &Config, force: Option<Which>, image: &str) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    // CLI 显式操作：打印待删镜像名，无交互 confirm（服务端另有归属守卫）
    println!("待删镜像：{image}");
    let v = api.signed_post_json("/_api/image-rm", &json!({ "image": image }))?;
    let freed = v["freed_est_mb"].as_i64().unwrap_or(0);
    println!("已删除 {image}，回收约 {freed} MB");
    Ok(())
}

fn url_cmd(cfg: &Config, force: Option<Which>, project: &str) -> Result<(), String> {
    check_project(project)?;
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get("/_api/projects")?;
    let found = v["projects"].as_array().is_some_and(|a| {
        a.iter().any(|e| e["project"].as_str() == Some(project))
    });
    if !found {
        return Err(format!("未找到 {project}（先 showroom publish）"));
    }
    println!("{}", api::project_url(&api.base, project));
    Ok(())
}

fn logs_cmd(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    tail: u32,
    follow: bool,
) -> Result<(), String> {
    check_project(project)?;
    let api = Api::connect(cfg, force)?;
    let q = api::logs_query(project, tail);
    let mut printed = 0usize;
    loop {
        let v = api.signed_get(&q)?;
        let s = v["logs"].as_str().unwrap_or("").to_string();
        // 追加式输出；日志被截短/重置时整体重打
        match s.get(printed..) {
            Some(new) if !new.is_empty() => {
                print!("{new}");
                printed = s.len();
            }
            _ if s.len() < printed => {
                print!("{s}");
                printed = s.len();
            }
            _ => {}
        }
        let _ = std::io::stdout().flush();
        if !follow {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn env_cmd(cfg: &Config, force: Option<Which>, project: &str) -> Result<(), String> {
    check_project(project)?;
    let api = Api::connect(cfg, force)?;
    let v = api
        .signed_get(&api::env_query(project))?;
    let map = v["env"].as_object().ok_or("响应缺少 env 对象")?;
    for (k, val) in map {
        let s = val.as_str().map(str::to_string).unwrap_or_else(|| val.to_string());
        println!("{k}={s}");
    }
    Ok(())
}

fn project_action(
    cfg: &Config,
    force: Option<Which>,
    path: &str,
    project: &str,
) -> Result<(), String> {
    check_project(project)?;
    let api = Api::connect(cfg, force)?;
    let v = api
        .signed_post_json(path, &json!({ "project": project }))?;
    println!("ok");
    for key in ["msg", "url"] {
        if let Some(s) = v[key].as_str().filter(|s| !s.is_empty()) {
            println!("{key}: {s}");
        }
    }
    Ok(())
}

// ———— doctor ————

fn doctor(cfg: &Config) -> Result<(), String> {
    let mut all_ok = true;

    // 1. 密钥（plan-16 + B7：未登录 → 提示 showroom login；无账号找管理员发卡）
    let key = match UserKey::from_seed_file(&cfg.key_file()) {
        Ok(k) => {
            println!("✓ 密钥 {} 可读，公钥串：{}", cfg.key_file().display(), k.pub_string());
            Some(k)
        }
        Err(e) => {
            all_ok = false;
            println!("✗ 密钥不可读：{e}（showroom login <账号> 一键登录；无账号找管理员 showroomctl user-add <名> --gen-password 发放）");
            None
        }
    };

    // 2. 配置
    if cfg.user.is_empty() {
        all_ok = false;
        println!("✗ 未登录（showroom login <账号>；或 showroom config --user … 手工配置）");
    } else {
        println!("✓ user = {}", cfg.user);
    }

    // 3/4. 服务端可达（签名 health 探测）
    let probe = key.as_ref().map(|k| api::signed_probe(&cfg.user, k));
    let mut lan_ok = false;
    let mut pub_ok = false;
    for (label, url, ok_flag) in [
        ("server_lan", cfg.server_lan.as_deref(), &mut lan_ok),
        ("server_public", cfg.server_public.as_deref(), &mut pub_ok),
    ] {
        let Some(u) = url.filter(|s| !s.is_empty()) else { continue };
        let reachable = probe.as_ref().is_some_and(|p| p(u));
        if reachable {
            println!("✓ {label} {u} 可达");
            *ok_flag = true;
        } else {
            println!("✗ {label} {u} 不可达");
        }
    }
    if !(lan_ok || pub_ok) {
        all_ok = false;
    }

    // 5. 签名自测：签名请求不存在的 project。服务端零信息暴露（SPEC §3）：验签失败统一
    //    404 E_AUTH 无 msg，故 E_NOTFOUND = 验签通过走到业务层 = 密钥已登记；E_AUTH = 未登记/时钟偏差。
    if key.is_some() && (lan_ok || pub_ok) {
        let base = if lan_ok {
            cfg.server_lan.as_deref().unwrap().trim_end_matches('/')
        } else {
            cfg.server_public.as_deref().unwrap().trim_end_matches('/')
        };
        match Api::open(base.to_string(), cfg)
            .and_then(|api| api.signed_get(&api::logs_query("__selftest", 100)))
        {
            Ok(_) => println!("✓ 签名自测通过（密钥已登记）"),
            Err(msg) if msg.starts_with("E_NOTFOUND") => {
                println!("✓ 签名自测通过（密钥已登记）");
            }
            Err(msg) if msg.starts_with("E_AUTH") => {
                all_ok = false;
                println!("✗ 签名自测：{msg}（钥匙被服务端删除/GC——重新 showroom login <账号> 即可重绑本机钥匙；其次才是本地时钟偏差）");
            }
            Err(msg) => {
                all_ok = false;
                println!("✗ 签名自测：{msg}");
            }
        }
    }

    // 6. registry 快通道探测（非关键：失败时 push 自动回落 import）
    let reg_user = (!cfg.registry.user.is_empty()).then_some(cfg.registry.user.as_str());
    let reg_pass = (!cfg.registry.password.is_empty()).then_some(cfg.registry.password.as_str());
    if let (Some(lan), Some(u), Some(pw)) = (&cfg.server_lan, reg_user, reg_pass) {
        let url = format!("{}/v2/", lan.trim_end_matches('/'));
        match reqwest::blocking::Client::builder().timeout(Duration::from_secs(5)).build()
            .map(|c| c.get(&url).basic_auth(u, Some(pw)).send())
        {
            Ok(Ok(resp)) if resp.status().as_u16() == 200 => println!("✓ registry 快通道可用（凭据有效）"),
            Ok(Ok(resp)) if resp.status().as_u16() == 401 => println!("✗ registry 凭据无效（push 将回落加密通道；用 showroom config --registry-password 更新）"),
            Ok(Ok(resp)) => println!("~ registry 返回 {}（push 将回落加密通道）", resp.status()),
            _ => println!("~ registry 不可达（push 将回落加密通道）"),
        }
    } else if cfg.server_lan.is_some() {
        println!("~ registry 凭据未配置（push 将走加密通道；如需快通道：showroom config --registry-user/--registry-password）");
    }

    // 7. docker（仅 push 需要，失败不算关键项）
    match Command::new("docker").arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status()
    {
        Ok(st) if st.success() => println!("✓ docker 可用"),
        _ => println!("✗ docker 不可用（仅 push 需要）"),
    }

    if all_ok {
        println!("doctor: 全部关键项通过");
        Ok(())
    } else {
        Err("doctor: 存在未通过的关键项（见上）".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A1 回归：--cpu 解析为 f64（正有限），且 JSON 值可被服务端 PubReq.cpu: Option<f64> 反序列化
    #[test]
    fn cpu_option_parses_to_f64() {
        assert_eq!(parse_cpu("0.5").unwrap(), 0.5);
        assert_eq!(parse_cpu("2").unwrap(), 2.0);
        for bad in ["abc", "", "-1", "0", "nan", "inf"] {
            let e = parse_cpu(bad).unwrap_err();
            assert!(e.starts_with("E_NAME"), "{bad}: {e}");
        }
        let cpu: Option<f64> = serde_json::from_value(json!(parse_cpu("2").unwrap())).unwrap();
        assert_eq!(cpu, Some(2.0));
    }

    // §12.1 deploy 参数解析：项目可省略（=None 自动随机）；flags 与 publish 对齐。
    // 注：publish/deploy 用单一 1..=2 positional + split_project_tag——可选位在必选位前的
    // 双 positional 会触发 clap debug 断言（既单有 Publish 的隐性坑，本次一并修正）。
    #[test]
    fn deploy_arg_parsing() {
        // 仅 tag → project_tag=[tag]（项目自动随机）
        let c = Cli::try_parse_from(["showroom", "deploy", "v1"]).unwrap();
        match &c.cmd {
            Cmd::Deploy { project_tag, mem, cpu, container_port } => {
                assert_eq!(project_tag, &["v1".to_string()]);
                assert_eq!((mem.is_none(), cpu.is_none(), container_port.is_none()), (true, true, true));
            }
            _ => panic!("expected Deploy"),
        }
        // 项目 + tag + 全量 flags
        let c = Cli::try_parse_from([
            "showroom", "deploy", "demo", "v2", "--mem", "512m", "--cpu", "0.5",
            "--container-port", "3000",
        ])
        .unwrap();
        match &c.cmd {
            Cmd::Deploy { project_tag, mem, cpu, container_port } => {
                assert_eq!(project_tag, &["demo".to_string(), "v2".to_string()]);
                assert_eq!(mem.as_deref(), Some("512m"));
                assert_eq!(*cpu, Some(0.5));
                assert_eq!(*container_port, Some(3000));
            }
            _ => panic!("expected Deploy"),
        }
        // publish 同形：单参数 = tag
        let c = Cli::try_parse_from(["showroom", "publish", "v9", "--wait"]).unwrap();
        match &c.cmd {
            Cmd::Publish { project_tag, wait, .. } => {
                assert_eq!(project_tag, &["v9".to_string()]);
                assert!(*wait);
            }
            _ => panic!("expected Publish"),
        }
        // 缺参 / 超量 / 非法 cpu / 非 u16 端口 → 解析失败
        assert!(Cli::try_parse_from(["showroom", "deploy"]).is_err());
        assert!(Cli::try_parse_from(["showroom", "deploy", "a", "b", "c"]).is_err());
        assert!(Cli::try_parse_from(["showroom", "deploy", "demo", "v1", "--cpu", "-1"]).is_err());
        assert!(Cli::try_parse_from(["showroom", "deploy", "demo", "v1", "--container-port", "x"]).is_err());
    }

    // positional → (project, tag) 拆分
    #[test]
    fn split_project_tag_rules() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            split_project_tag(&s(&["v1"])),
            (None, "v1".to_string()),
            "单参数 = tag，项目自动随机"
        );
        assert_eq!(split_project_tag(&s(&["demo", "v1"])), (Some("demo".into()), "v1".into()));
    }

    // §12.1 步骤报错格式 + URL 横幅
    #[test]
    fn deploy_error_and_banner() {
        assert_eq!(deploy_err(1, "push", "docker push 失败"), "deploy: 第 1 步(push) 失败：docker push 失败");
        assert_eq!(deploy_err(2, "publish", "E_NOTFOUND: xx"), "deploy: 第 2 步(publish) 失败：E_NOTFOUND: xx");
        let b = banner("http://h/demo/");
        assert!(b.contains("部署成功"));
        assert!(b.contains("http://h/demo/"));
        assert!(b.trim_start().starts_with('═') && b.trim_end().ends_with('═'), "上下双线包夹: {b}");
        assert!(banner("").lines().count() == banner("http://h/demo/").lines().count(), "空 URL 同样成形");
    }
}
