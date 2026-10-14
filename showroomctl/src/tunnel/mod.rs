//! tunnel：隧道模块（frp v1 客户端 + SRTP 信封网关，合并进 serve——用户裁决 C2）。
//!
//! 拓扑：公网访客 → [标准 frps 中转（chmlfrp 节点 / 任何原生 frps）] → 本进程内
//! frp 客户端（frpc-rust，tcp 型代理）→ 网关 127.0.0.1:8082 → Caddy site B :8081。
//! 网关混合模式：连接首批字节为 SRTP 传输头 → 信封解封/对称封装（wasm/cli 专用通道）；
//! 否则字节级明文直通——**原有 Docker Push（公网加密 import 通道）与匿名公开访问零变化**。
//!
//! 配置源（provider 抽象，扩展点）：Chmlfrp = getcfg 拉取面板 ini；Toml = host.env FRP_TOML
//! 指向的 0600 文件（原生 frps / 其它中转商走此路）。两者都在**会话重建时**重新读取——
//! frpc-rust 内层网络重连复用旧配置不重读，故面板/文件改动在致命错误后的外层重试或
//! `systemctl restart showroom-api` 后才生效（诚实边界，勿写成「热生效」）。
//! 旧 chmlfrp.service（独立 frpc 单元）由 setup 收敛移除；多隧道用 FRP_TUNNEL 逗号列表
//!（getcfg 原生支持多 ID 一次返回）。

pub mod chmlfrp;
pub mod gateway;

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::middleware::{FrpMode, HostEnv};
use crate::state::{ApiError, R};

/// 网关监听（frp 客户端全部代理的 local 落点；仅回环，公网只能经隧道进入）。
pub const GATEWAY_PORT: u16 = 8082;
/// 网关上游：Caddy site B（公网面，纯端口匹配 :8081）。
pub const UPSTREAM_PORT: u16 = 8081;
/// 外层重试间隔（getcfg 失败 / 客户端致命退出后的会话级退避；连接级重连由客户端自理）。
const RETRY: Duration = Duration::from_secs(30);

/// 隧道配置源。
#[derive(Debug, Clone)]
pub enum Source {
    /// chmlfrp：面板 usertoken + 隧道 ID（可逗号分隔多个）→ getcfg ini。
    Chmlfrp { token: String, ids: String },
    /// 原生 frp / 其它中转商：frpc TOML 文件路径（host.env FRP_TOML；会话重建时重读——
    /// 内层重连复用旧配置，致命错误后 ≤30s 外层重试或 restart 生效）。
    Toml(std::path::PathBuf),
}

/// 解析一份可运行的客户端配置：local 统一改指网关，校验至少一条启用的 tcp 代理。
/// 返回 (配置, 跳过的非 tcp 代理名)——跳过只记日志不失败（http/udp 型隧道非本平台场景）。
async fn resolve(source: &Source) -> R<(frpc_rust::config::Config, Vec<String>)> {
    let mut pre_skipped = Vec::new();
    let mut cfg = match source {
        Source::Chmlfrp { token, ids } => {
            let ini = chmlfrp::fetch_ini(token, ids)
                .await
                .map_err(|e| ApiError::new("E_NET", format!("getcfg: {e}")))?;
            let (cfg, ini_skipped) = chmlfrp::ini_to_config(&ini)?;
            pre_skipped = ini_skipped; // ini 里本就非 tcp 的节（面板侧名字，进日志）
            cfg
        }
        Source::Toml(path) => {
            let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
                ApiError::new("E_NAME", format!("FRP_TOML 文件不可读（{}）: {e}", path.display()))
            })?;
            ::toml::from_str(&raw)
                .map_err(|e| ApiError::new("E_NAME", format!("FRP_TOML 解析失败: {e}")))?
        }
    };
    // 单趟分流：非 tcp 代理记名跳过；tcp 代理 local 恒改指网关（面板侧 local 端口是客户端私有信息）
    let mut skipped = pre_skipped;
    cfg.proxies = cfg
        .proxies
        .into_iter()
        .filter_map(|mut p| {
            if p.r#type != "tcp" {
                skipped.push(p.name.clone());
                return None;
            }
            p.local_ip = "127.0.0.1".into();
            p.local_port = GATEWAY_PORT;
            Some(p)
        })
        .collect();
    cfg.validate().map_err(|e| ApiError::new("E_NAME", format!("隧道配置无效: {e}")))?;
    if cfg.proxies.is_empty() {
        return Err(ApiError::new(
            "E_NAME",
            format!("隧道配置无启用的 tcp 代理（跳过: {:?}）", skipped),
        ));
    }
    Ok((cfg, skipped))
}

/// 隧道主管：serve 拉起（进程生命周期即隧道生命周期；serve Restart=always 兜底）。
/// 永不返回 Err——所有失败记日志后按 RETRY 重试，控制面照常服务。
pub async fn supervise(root: PathBuf) {
    let host = HostEnv::load(&root);
    let source = match host.frp {
        FrpMode::None => {
            eprintln!("showroomctl: tunnel: FRP=none，隧道未启用");
            return;
        }
        FrpMode::Chmlfrp => Source::Chmlfrp { token: host.frp_token.clone(), ids: host.frp_tunnel.clone() },
        FrpMode::Native => Source::Toml(std::path::PathBuf::from(host.frp_toml_path)),
    };
    if host.public_host.is_empty() {
        eprintln!("showroomctl: tunnel: 警告：PUBLIC_HOST 为空（site B 未渲染），隧道流量将无处落地");
    }
    // 平台私钥（serve 启动序列已确保生成；此处缺失只禁用密封通道，不拦明文直通）
    let keys = showroom_common::PlatformKeys::load_pem(&root.join("state")).ok();
    if keys.is_none() {
        eprintln!("showroomctl: tunnel: 警告：state/platform_rsa 缺失，SRTP 密封通道不可用（明文直通不受影响）");
    }
    // 网关（上游=Caddy site B）
    let listener = match TcpListener::bind(("127.0.0.1", GATEWAY_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("showroomctl: tunnel: 网关绑定 127.0.0.1:{GATEWAY_PORT} 失败: {e}——隧道停用");
            return;
        }
    };
    eprintln!(
        "showroomctl: tunnel: 网关 127.0.0.1:{GATEWAY_PORT} → 127.0.0.1:{UPSTREAM_PORT}（SRTP/明文混合）"
    );
    let keys = keys.map(std::sync::Arc::new);
    tokio::spawn(gateway::serve(listener, UPSTREAM_PORT, keys));
    // 会话循环：会话重建时重读配置（getcfg/FRP_TOML 文件）——内层网络重连不触发此处
    loop {
        match run_once(&source, &root).await {
            Ok(info) => {
                // 短暂态不落盘：新会话 poller ≤10s 内会写 connecting/ok，避免 error 语义污染
                eprintln!("showroomctl: tunnel: {info}——{RETRY:?} 后重建会话");
            }
            Err(e) => {
                // resolve（getcfg/读文件/解析）失败时 poller 尚未存在——失败详情必须进
                // state/tunnel.status，否则 frp.tunnel 视图恒显「未运行」而非真实原因
                status_write(&root, "error", "-", &e.msg);
                eprintln!("showroomctl: tunnel: {e}——{RETRY:?} 后重试");
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// 单次客户端会话（阻塞至致命错误/退出）。正常情况由 frpc-rust 内部无限重连，
/// 返回即代表需要外层重建（重新 getcfg / 重新读 FRP_TOML）。
async fn run_once(source: &Source, root: &std::path::Path) -> R<String> {
    let (cfg, skipped) = resolve(source).await?;
    if !skipped.is_empty() {
        eprintln!("showroomctl: tunnel: 跳过非 tcp 代理: {skipped:?}");
    }
    let names: Vec<&str> = cfg.proxies.iter().map(|p| p.name.as_str()).collect();
    let node = format!("{}:{}", cfg.server_addr, cfg.server_port);
    eprintln!("showroomctl: tunnel: 连接 {node}（代理 {names:?}，tls={}）", cfg.transport.tls.enable);
    // config_path 仅客户端 admin/reload 使用（未启用 web_server，此路径不被读取）
    let client = frpc_rust::service::Client::new(cfg, root.join("state/tunnel-client.toml"));
    // 会话存活旗：run 返回即置 false——poller 必须随会话退出，否则旧轮询任务持有已死
    // Shared 持续写陈旧状态，与新会话的 poller 互相踩踏（评审实锤）
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    tokio::spawn(status_poller(root.to_path_buf(), client.shared.clone(), node.clone(), alive.clone()));
    let r = client.run().await;
    alive.store(false, std::sync::atomic::Ordering::Relaxed);
    r.map_err(|e| ApiError::new("E_NET", format!("隧道会话结束: {e:#}")))?;
    Ok(format!("客户端会话正常退出（{node}）"))
}

// —— 隧道状态落盘（host_view 的 frp.tunnel 数据源；比 grep journald 稳定可测）——

/// 状态文件行形状：`<unix秒>|<ok|connecting|error>|<节点>|<详情>`。
fn status_write(root: &std::path::Path, kind: &str, node: &str, detail: &str) {
    let _ = std::fs::write(
        root.join("state/tunnel.status"),
        format!("{}|{}|{}|{}\n", crate::state::unix_now(), kind, node, detail),
    );
}

/// 周期采样客户端 Snapshot（10s；**每轮恒写盘**刷新时间戳——读取侧的 90s 新鲜度判定
/// 依赖它，change-only 会把健康会话误判成 stale（评审实锤）；alive=false 即随会话退出，
/// 防旧轮询任务持有已死 Shared 与新会话 poller 踩踏）。
async fn status_poller(
    root: PathBuf,
    shared: std::sync::Arc<frpc_rust::service::Shared>,
    node: String,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let (connected, err, reconnects, proxies) = {
            let s = shared.status.read().await;
            (s.connected, s.last_error.clone(), s.reconnects, s.proxies.len())
        };
        let cur = if connected {
            ("ok", node.as_str(), format!("{proxies} 条代理在线"))
        } else if err.is_empty() {
            ("connecting", "-", "等待首次登录".to_string())
        } else {
            ("error", node.as_str(), format!("{err}（重连 {reconnects} 次）"))
        };
        status_write(&root, cur.0, cur.1, &cur.2);
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
mod tests;
