//! showroom：Showroom 发布者 CLI。
//! push/publish/ps/url/logs/env/rollback/stop/start/rm/doctor，全部走 SR-SIG1 签名 API。

mod api;
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
use showroom_common::names::{check_project, check_slot, check_tag};
use showroom_common::OwnerKey;

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
    /// 生成发布者密钥（把打印的公钥串发给管理员登记）
    Keygen {
        #[arg(long)]
        force: bool,
    },
    /// 查看/更新配置（仅更新给出的字段；无参数时打印当前配置）
    Config {
        #[arg(long)]
        owner: Option<String>,
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
        /// import 通道登记到的槽位
        #[arg(long, default_value = "stable")]
        slot: String,
    },
    /// 发布/更新展项槽位
    Publish {
        project: String,
        slot: String,
        tag: String,
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
    /// 打印展项 URL（不开浏览器；省略 slot 时打印该 project 全部槽位）
    Url {
        project: String,
        slot: Option<String>,
    },
    /// 查看日志
    Logs {
        project: String,
        slot: Option<String>,
        #[arg(long, default_value_t = 100)]
        tail: u32,
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// 查看注入容器的环境变量（DB/Redis 连接信息，SPEC §6）
    Env {
        project: String,
        slot: Option<String>,
    },
    /// 回滚到上一个 tag
    Rollback {
        project: String,
        slot: String,
    },
    /// 停止展项
    Stop {
        project: String,
        slot: String,
    },
    /// 启动展项
    Start {
        project: String,
        slot: String,
    },
    /// 删除展项（需 --yes）
    Rm {
        project: String,
        slot: String,
        #[arg(long)]
        yes: bool,
    },
    /// 数据库维护（SPEC §9）：sql/tables/preview/dump/restore/reset
    Db {
        #[command(subcommand)]
        cmd: db::DbCmd,
    },
    /// 体检：密钥/配置/服务端可达/签名自测/docker
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let cfg = Config::load()?;
    match &cli.cmd {
        Cmd::Keygen { force } => keygen(*force, &cfg),
        Cmd::Config { owner, server_lan, server_public, registry_user, registry_password } => {
            config_cmd(&cfg, owner, server_lan, server_public, registry_user, registry_password)
        }
        Cmd::Push { project, tag, via, slot } => {
            push(&cfg, cli.server, project, tag, *via, slot)
        }
        Cmd::Publish { project, slot, tag, mem, cpu, container_port, wait } => publish(
            &cfg,
            cli.server,
            project,
            slot,
            tag,
            mem.as_deref(),
            *cpu,
            *container_port,
            *wait,
        ),
        Cmd::Ps { json } => ps(&cfg, cli.server, *json),
        Cmd::Url { project, slot } => url_cmd(&cfg, cli.server, project, slot.as_deref()),
        Cmd::Logs { project, slot, tail, follow } => {
            logs_cmd(&cfg, cli.server, project, slot.as_deref(), *tail, *follow)
        }
        Cmd::Env { project, slot } => env_cmd(&cfg, cli.server, project, slot.as_deref()),
        Cmd::Rollback { project, slot } => {
            slot_action(&cfg, cli.server, "/_api/rollback", project, slot)
        }
        Cmd::Stop { project, slot } => slot_action(&cfg, cli.server, "/_api/stop", project, slot),
        Cmd::Start { project, slot } => slot_action(&cfg, cli.server, "/_api/start", project, slot),
        Cmd::Rm { project, slot, yes } => {
            if !yes {
                return Err(format!("删除不可逆，需显式确认：showroom rm {project} {slot} --yes"));
            }
            slot_action(&cfg, cli.server, "/_api/rm", project, slot)
        }
        Cmd::Db { cmd } => db::run(&cfg, cli.server, cmd),
        Cmd::Doctor => doctor(&cfg),
    }
}

// ———— keygen / config ————

fn keygen(force: bool, cfg: &Config) -> Result<(), String> {
    let p = cfg.key_file();
    if p.exists() && !force {
        return Err(format!("密钥 {} 已存在，拒绝覆盖（--force 强制）", p.display()));
    }
    let k = OwnerKey::generate();
    k.save_seed(&p).map_err(|e| format!("写密钥失败：{e}"))?;
    println!("密钥: {}", p.display());
    println!("公钥: {}", k.pub_string());
    println!("把上面的公钥串发给管理员登记。");
    Ok(())
}

fn config_cmd(
    cfg: &Config,
    owner: &Option<String>,
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
    if let (None, None, None, None, None) = (owner, server_lan, server_public, registry_user, registry_password) {
        println!("{}", toml::to_string_pretty(&c).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if let Some(o) = owner {
        c.owner = o.clone();
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

fn push(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    tag: &str,
    via: Via,
    slot: &str,
) -> Result<(), String> {
    check_project(project)?;
    check_tag(tag)?;
    check_slot(slot)?;
    let t0 = Instant::now();
    let (base, which) = select(cfg, force)?;
    match via {
        Via::Registry => push_registry(cfg, project, tag, t0),
        Via::Import => push_import(cfg, base, project, tag, slot, t0),
        Via::Auto if which == Which::Lan => match push_registry(cfg, project, tag, t0) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("showroom: registry 快通道失败（{e}），自动切换加密 import 通道");
                push_import(cfg, base, project, tag, slot, t0)
            }
        },
        Via::Auto => push_import(cfg, base, project, tag, slot, t0),
    }
}

fn select(cfg: &Config, force: Option<Which>) -> Result<(String, Which), String> {
    // 探测需签名，故先加载密钥（失败即报错，引导 keygen）
    let key = OwnerKey::from_seed_file(&cfg.key_file())
        .map_err(|e| format!("读取密钥 {} 失败：{e}（先运行 showroom keygen）", cfg.key_file().display()))?;
    api::pick_server(
        cfg.server_lan.as_deref(),
        cfg.server_public.as_deref(),
        force,
        api::signed_probe(&cfg.owner, &key),
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
    let user = if cfg.registry.user.is_empty() { cfg.owner.clone() } else { cfg.registry.user.clone() };
    let host = registry_host(lan);
    let src = format!("{project}:{tag}");
    let reg_tag = format!("{host}/{}/{}:{tag}", cfg.owner, project);
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
    slot: &str,
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
        // docker save | zstd -3 | seal → 临时文件；失败也要删（ponytail: 用闭包收口清理）
        let work = (|| -> Result<(), String> {
            let mut child = Command::new("docker")
                .args(["save", &src])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| format!("无法启动 docker save：{e}"))?;
            let z = zstd::stream::read::Encoder::new(child.stdout.take().unwrap(), 3)
                .map_err(|e| format!("zstd 初始化失败：{e}"))?;
            let f = fs::File::create(&tmp).map_err(|e| format!("写临时文件失败：{e}"))?;
            uploaded = showroom_common::seal(&platform_pub, z, f)
                .map_err(|e| format!("加密信封失败：{e}"))?;
            if !child.wait().map_err(|e| e.to_string())?.success() {
                return Err("docker save 失败".into());
            }
            api.signed_post_file(&api::import_query(project, slot, tag), &tmp)?;
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

fn publish(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: &str,
    tag: &str,
    mem: Option<&str>,
    cpu: Option<f64>,
    container_port: Option<u16>,
    wait: bool,
) -> Result<(), String> {
    check_project(project)?;
    check_slot(slot)?;
    check_tag(tag)?;
    let api = Api::connect(cfg, force)?;
    let mut body = json!({ "project": project, "slot": slot, "tag": tag });
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
    if let Some(u) = v["url"].as_str().filter(|s| !s.is_empty()) {
        println!("发布成功: {u}");
    } else {
        println!("发布成功");
    }
    if wait {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut last = String::new();
        loop {
            let projects = api
                .signed_get("/_api/projects")?;
            match slot_status(&projects, project, slot) {
                Some(st) if st == "running" => {
                    println!("{}", api::slot_url(&api.base, project, slot));
                    return Ok(());
                }
                Some(st) => {
                    if st != last {
                        eprintln!("状态: {st}");
                        last = st;
                    }
                }
                None => eprintln!("等待 {project}/{slot} 出现…"),
            }
            if Instant::now() >= deadline {
                return Err("等待 status=running 超时（90s）".into());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    Ok(())
}

fn slot_status(projects: &Value, project: &str, slot: &str) -> Option<String> {
    projects["projects"].as_array()?.iter()
        .find(|e| e["project"].as_str() == Some(project) && e["slot"].as_str() == Some(slot))
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
    println!("{:<26} {:<12} {:<10} {:<26} {}", "PROJECT", "SLOT", "STATUS", "TAG", "OWNER");
    for e in &arr {
        let g = |k: &str| e[k].as_str().unwrap_or("");
        println!(
            "{:<26} {:<12} {:<10} {:<26} {}",
            trunc(g("project"), 25),
            trunc(g("slot"), 11),
            trunc(g("status"), 9),
            trunc(g("tag"), 25),
            g("owner")
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

fn url_cmd(cfg: &Config, force: Option<Which>, project: &str, slot: Option<&str>) -> Result<(), String> {
    check_project(project)?;
    if let Some(s) = slot {
        check_slot(s)?;
    }
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get("/_api/projects")?;
    let entries: Vec<&Value> = match v["projects"].as_array() {
        Some(a) => a
            .iter()
            .filter(|e| {
                e["project"].as_str() == Some(project)
                    && slot.map_or(true, |s| e["slot"].as_str() == Some(s))
            })
            .collect(),
        None => vec![],
    };
    if entries.is_empty() {
        return Err(format!(
            "未找到 {project}{}（先 showroom publish）",
            slot.map(|s| format!("/{s}")).unwrap_or_default()
        ));
    }
    for e in entries {
        let s = e["slot"].as_str().unwrap_or("stable");
        println!("{}", api::slot_url(&api.base, project, s));
    }
    Ok(())
}

fn logs_cmd(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: Option<&str>,
    tail: u32,
    follow: bool,
) -> Result<(), String> {
    check_project(project)?;
    let slot = slot.unwrap_or("stable");
    check_slot(slot)?;
    let api = Api::connect(cfg, force)?;
    let q = api::logs_query(project, slot, tail);
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

fn env_cmd(cfg: &Config, force: Option<Which>, project: &str, slot: Option<&str>) -> Result<(), String> {
    check_project(project)?;
    let slot = slot.unwrap_or("stable");
    check_slot(slot)?;
    let api = Api::connect(cfg, force)?;
    let v = api
        .signed_get(&api::env_query(project, slot))?;
    let map = v["env"].as_object().ok_or("响应缺少 env 对象")?;
    for (k, val) in map {
        let s = val.as_str().map(str::to_string).unwrap_or_else(|| val.to_string());
        println!("{k}={s}");
    }
    Ok(())
}

fn slot_action(
    cfg: &Config,
    force: Option<Which>,
    path: &str,
    project: &str,
    slot: &str,
) -> Result<(), String> {
    check_project(project)?;
    check_slot(slot)?;
    let api = Api::connect(cfg, force)?;
    let v = api
        .signed_post_json(path, &json!({ "project": project, "slot": slot }))?;
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

    // 1. 密钥
    let key = match OwnerKey::from_seed_file(&cfg.key_file()) {
        Ok(k) => {
            println!("✓ 密钥 {} 可读，公钥串：{}", cfg.key_file().display(), k.pub_string());
            Some(k)
        }
        Err(e) => {
            all_ok = false;
            println!("✗ 密钥不可读：{e}（showroom keygen）");
            None
        }
    };

    // 2. 配置
    if cfg.owner.is_empty() {
        all_ok = false;
        println!("✗ 未配置 owner（showroom config --owner …）");
    } else {
        println!("✓ owner = {}", cfg.owner);
    }

    // 3/4. 服务端可达（签名 health 探测）
    let probe = key.as_ref().map(|k| api::signed_probe(&cfg.owner, k));
    let mut lan_ok = false;
    let mut pub_ok = false;
    for (label, url, ok_flag) in [
        ("server_lan", cfg.server_lan.as_deref(), &mut lan_ok),
        ("server_public", cfg.server_public.as_deref(), &mut pub_ok),
    ] {
        let Some(u) = url.filter(|s| !s.is_empty()) else { continue };
        let reachable = probe.as_ref().map_or(false, |p| p(u));
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
            .and_then(|api| api.signed_get(&api::logs_query("__selftest", "stable", 100)))
        {
            Ok(_) => println!("✓ 签名自测通过（密钥已登记）"),
            Err(msg) if msg.starts_with("E_NOTFOUND") => {
                println!("✓ 签名自测通过（密钥已登记）");
            }
            Err(msg) if msg.starts_with("E_AUTH") => {
                all_ok = false;
                println!("✗ 签名自测：{msg}（最常见：公钥未登记——把 doctor 顶部的公钥串交给管理员；其次才是本地时钟偏差）");
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
            .and_then(|c| Ok(c.get(&url).basic_auth(u, Some(pw)).send()))
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
}
