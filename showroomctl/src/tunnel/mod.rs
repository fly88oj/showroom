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
pub mod srtpkeys;
pub mod srtp_ws;

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

/// 隧道配置源（**一会话一源**：每个源一条独立 getcfg/客户端/重试循环——实测教训：
/// getcfg 多 ID 合并调用仅支持同节点，跨节点双入口必须分会话；代理名保持面板原名，
/// 不触碰面板 checkproxy 校验）。
#[derive(Debug, Clone)]
pub enum Source {
    /// chmlfrp：面板 usertoken + **单条**隧道 ID → getcfg ini。
    Chmlfrp { token: String, id: String },
    /// 原生 frp / 其它中转商：frpc TOML 文件路径（host.env FRP_TOML；会话重建时重读——
    /// 内层重连复用旧配置，致命错误后 ≤30s 外层重试或 restart 生效）。
    Toml(std::path::PathBuf),
}

/// host.env 的 FRP_TUNNEL（逗号列表）+ FRP_TOML → 会话源清单。
/// chmlfrp 每 ID 一源；native 单文件一源（多节点 native 需多文件，host.env 单键单文件）。
fn sources_from(host: &HostEnv) -> Vec<Source> {
    match host.frp {
        FrpMode::None => vec![],
        FrpMode::Chmlfrp => host
            .frp_tunnel
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|id| Source::Chmlfrp { token: host.frp_token.clone(), id: id.to_string() })
            .collect(),
        FrpMode::Native => vec![Source::Toml(std::path::PathBuf::from(host.frp_toml_path.clone()))],
    }
}

/// 解析一份可运行的客户端配置：local 统一改指网关，校验至少一条启用的 tcp 代理。
/// 返回 (配置, 跳过的非 tcp 代理名)——跳过只记日志不失败（http/udp 型隧道非本平台场景）。
async fn resolve(source: &Source) -> R<(frpc_rust::config::Config, Vec<String>)> {
    let mut pre_skipped = Vec::new();
    let mut cfg = match source {
        Source::Chmlfrp { token, id } => {
            let ini = chmlfrp::fetch_ini(token, id)
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
/// 网关恒驻（FRP=none 时无会话但监听无害，免去模式切换重绑）；会话随 FRP 签名热重载。
pub async fn supervise(root: PathBuf) {
    let host0 = HostEnv::load(&root);
    if host0.frp != crate::middleware::FrpMode::None && host0.public_host.is_empty() {
        eprintln!("showroomctl: tunnel: 警告：PUBLIC_HOST 为空（site B 未渲染），隧道流量将无处落地");
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
    tokio::spawn(gateway::serve(listener, UPSTREAM_PORT, root.clone()));
    // 热重载（用户裁决）：每 2s 对比 FRP 签名（mode/token/tunnels/toml）——变化即中止旧会话
    // 任务、按新源重建。控制台保存（admin POST 落盘 host.env）、setup --apply、手工编辑
    // 三种途径统一走此路径，无需重启 serve。try_load 失败（如 setup 非原子写的中间态）
    // 保持上一轮会话不动，坏文件不自拆。
    let mut cur_sig: Option<String> = None;
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    loop {
        if let Ok(host) = HostEnv::try_load(&root) {
            let sig = frp_signature(&host);
            if cur_sig.as_deref() != Some(sig.as_str()) {
                for h in handles.drain(..) {
                    h.abort(); // 会话任务与其 poller（watch 通道随 drop 关闭）一并退出
                }
                let sources = sources_from(&host);
                let live: Vec<String> = sources.iter().map(status_key).collect();
                status_gc(&root, &live); // 移除已下线会话的状态文件（防聚合视图读尸）
                if sources.is_empty() {
                    eprintln!("showroomctl: tunnel: FRP=none，隧道停用");
                }
                for source in sources {
                    let root = root.clone();
                    handles.push(tokio::spawn(session_loop(root, source)));
                }
                let n = handles.len();
                eprintln!("showroomctl: tunnel: 配置已生效（{n} 会话，签名 {sig}）");
                cur_sig = Some(sig);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// FRP 相关键的签名（变化才重建会话——无关键改动如 MIDDLEWARE 不扰隧道）。
/// token 只进「设/空」位（打码）——签名会进 journal，凭据不入日志。
fn frp_signature(host: &HostEnv) -> String {
    format!(
        "{}|{}|{}|{}",
        host.frp.as_str(),
        if host.frp_token.is_empty() { "空" } else { "<set>" },
        host.frp_tunnel,
        host.frp_toml_path
    )
}

/// 单源会话循环（外层 30s 重试；由 supervise 在配置变化时 abort 整任务）。
async fn session_loop(root: PathBuf, source: Source) {
    let key = status_key(&source);
    loop {
        match run_once(&source, &root).await {
            Ok(info) => {
                // 短暂态不落盘：新会话 poller ≤10s 内会写 connecting/ok，避免 error 语义污染
                eprintln!("showroomctl: tunnel[{key}]: {info}——{RETRY:?} 后重建会话");
            }
            Err(e) => {
                // resolve（getcfg/读文件/解析）失败时 poller 尚未存在——失败详情必须进
                // 状态文件，否则 frp.tunnel 视图恒显「未运行」而非真实原因
                status_write(&root, &key, "error", "-", &e.msg);
                eprintln!("showroomctl: tunnel[{key}]: {e}——{RETRY:?} 后重试");
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// 清掉不在当前会话键集合里的状态文件（会话下线后旧文件会让聚合视图读到尸体）。
fn status_gc(root: &std::path::Path, live_keys: &[String]) {
    let dir = status_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let keep = path
            .file_stem()
            .is_some_and(|s| live_keys.iter().any(|k| k.as_str() == s.to_string_lossy()));
        if !keep {
            let _ = std::fs::remove_file(&path);
        }
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
    // 会话存活通道：正常返回 send 关闭；被 supervise abort 时 tx 随本 future drop——
    // 两条路径下 poller 的 rx.changed() 都立即到头退出（旧 AtomicBool 方案在 abort
    // 路径不会置旗，poller 变孤儿持续写已死客户端的状态，评审实锤）
    let (alive_tx, alive_rx) = tokio::sync::watch::channel(());
    tokio::spawn(status_poller(
        root.to_path_buf(),
        status_key(source),
        client.shared.clone(),
        node.clone(),
        alive_rx,
    ));
    let r = client.run().await;
    drop(alive_tx);
    r.map_err(|e| ApiError::new("E_NET", format!("隧道会话结束: {e:#}")))?;
    Ok(format!("客户端会话正常退出（{node}）"))
}

// —— 隧道状态落盘（host_view 的 frp.tunnel 数据源；比 grep journald 稳定可测）——

/// 会话状态目录（每会话一文件；读取侧聚合，见 middleware::frp_tunnel_status）。
pub fn status_dir(root: &std::path::Path) -> std::path::PathBuf {
    root.join("state/tunnel")
}

/// 源 → 稳定文件键（chmlfrp=隧道 ID；native=TOML 文件名去扩展——避免路径斜杠进文件名）。
fn status_key(source: &Source) -> String {
    match source {
        Source::Chmlfrp { id, .. } => id.clone(),
        Source::Toml(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "native".into()),
    }
}

/// 状态文件行形状：`<unix秒>|<ok|connecting|error>|<节点>|<详情>`。
fn status_write(root: &std::path::Path, key: &str, kind: &str, node: &str, detail: &str) {
    let dir = status_dir(root);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{key}.status")),
        format!("{}|{}|{}|{}\n", crate::state::unix_now(), kind, node, detail),
    );
}

/// 周期采样客户端 Snapshot（10s；**每轮恒写盘**刷新时间戳——读取侧的 90s 新鲜度判定
/// 依赖它，change-only 会把健康会话误判成 stale（评审实锤）；会话结束（正常 send 关闭
/// 或 abort 后 tx drop）时 rx.changed() 到头即退出，防孤儿 poller 写已死客户端的状态）。
async fn status_poller(
    root: PathBuf,
    key: String,
    shared: std::sync::Arc<frpc_rust::service::Shared>,
    node: String,
    mut alive: tokio::sync::watch::Receiver<()>,
) {
    loop {
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
        status_write(&root, &key, cur.0, cur.1, &cur.2);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            r = alive.changed() => {
                if r.is_err() {
                    return; // tx 已 drop（会话结束/abort）
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
