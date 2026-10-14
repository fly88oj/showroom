use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use frpc_rust::{config::Config, service::Client};
use std::{path::PathBuf, time::Duration};
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(
    version,
    about = "原生 Rust frp 客户端",
    long_about = "连接官方 frps 的原生 Rust 客户端。支持 TCP/UDP/HTTP/HTTPS/STCP/SUDP/TCPMUX。"
)]
struct Cli {
    #[arg(
        short = 'c',
        long = "config",
        default_value = "frpc.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    #[command(about = "校验配置，不建立连接")]
    Verify,
    #[command(about = "查询运行中客户端的状态")]
    Status,
    #[command(about = "重新加载配置（重建会话）")]
    Reload,
    #[command(about = "停止运行中客户端")]
    Stop,
    #[command(about = "输出版本和兼容协议")]
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Version)) {
        println!(
            "frpc-rust {} (frp v1; compatibility baseline {})",
            env!("CARGO_PKG_VERSION"),
            frpc_rust::wire::VERSION
        );
        return Ok(());
    }
    let cfg = Config::load(&cli.config)?;
    cfg.validate_resources()?;
    if let Some(command) = cli.command {
        match command {
            Command::Verify => {
                frpc_rust::auth::Authenticator::new(&cfg.auth)?;
                if cfg.transport.tls.enable
                    || matches!(cfg.transport.protocol.as_str(), "wss" | "quic")
                {
                    frpc_rust::transport::tls_config(&cfg.transport.tls)?;
                }
                println!(
                    "配置校验通过：{} 个代理，{} 个访问者",
                    cfg.proxies.len(),
                    cfg.visitors.len()
                );
                return Ok(());
            }
            Command::Status | Command::Reload | Command::Stop => {
                ensure!(cfg.web_server.port > 0, "需要配置 webServer.port");
                let addr = match cfg.web_server.addr.as_str() {
                    "0.0.0.0" => "127.0.0.1",
                    "::" => "::1",
                    a => a,
                };
                let (method, path) = match command {
                    Command::Status => (reqwest::Method::GET, "status"),
                    Command::Reload => (reqwest::Method::POST, "reload"),
                    _ => (reqwest::Method::POST, "stop"),
                };
                let http = reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .no_proxy()
                    .build()?;
                let mut request = http.request(
                    method,
                    format!(
                        "http://{}/api/{path}",
                        frpc_rust::transport::format_host(addr, cfg.web_server.port)
                    ),
                );
                if !cfg.web_server.user.is_empty() {
                    request =
                        request.basic_auth(&cfg.web_server.user, Some(&cfg.web_server.password));
                }
                let response = request.send().await.context("无法访问管理接口")?;
                let status = response.status();
                let body = response.text().await?;
                ensure!(status.is_success(), "管理接口 {status}: {body}");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(
                        &body
                    )?)?
                );
                return Ok(());
            }
            Command::Version => unreachable!(),
        }
    }
    let level = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or(
        tracing_subscriber::EnvFilter::try_new(&cfg.log.level).context("log.level 无效")?,
    );
    let guard;
    if cfg.log.to == "console" {
        guard = None;
        tracing_subscriber::registry()
            .with(level)
            .with(tracing_subscriber::fmt::layer().with_ansi(!cfg.log.disable_print_color))
            .init();
    } else {
        let path = std::path::Path::new(&cfg.log.to);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(
                path.file_name()
                    .context("log.to 需要文件名")?
                    .to_string_lossy(),
            )
            .max_log_files(cfg.log.max_days.max(1))
            .build(parent)?;
        let (writer, g) = tracing_appender::non_blocking(appender);
        guard = Some(g);
        tracing_subscriber::registry()
            .with(level)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer),
            )
            .init();
    }
    let _guard = guard;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = if cfg.web_server.port > 0 {
        Some(
            tokio::net::TcpListener::bind((cfg.web_server.addr.as_str(), cfg.web_server.port))
                .await
                .context("绑定管理接口失败")?,
        )
    } else {
        None
    };
    let client = Client::new(cfg, cli.config);
    let shared = client.shared.clone();
    let mut tasks = tokio::task::JoinSet::new();
    if let Some(listener) = listener {
        tracing::info!(address=%listener.local_addr()?,"管理接口已启动");
        let state = shared.clone();
        tasks.spawn(async move { frpc_rust::admin::serve(listener, state).await });
    }
    let result = tokio::select! {
        result=client.run()=>result,
        _=shutdown_signal()=>{tracing::info!("收到退出信号");Ok(())},
        Some(result)=tasks.join_next()=>result.context("管理接口任务异常")?,
    };
    shared.shutdown.cancel();
    // Give the admin server time to flush the /api/stop response before exiting.
    if tokio::time::timeout(Duration::from_secs(3), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    result
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}};
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
