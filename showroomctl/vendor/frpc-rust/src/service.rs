use crate::{
    auth::Authenticator,
    config::Config,
    proxy::{self, ProxyRuntime},
    stream,
    transport::Connector,
    wire,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    sync::{RwLock, mpsc, oneshot},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatus {
    pub name: String,
    pub r#type: String,
    pub status: String,
    pub remote_addr: String,
    pub local_addr: String,
    pub error: String,
    pub active_connections: u64,
    pub total_connections: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub connected: bool,
    pub run_id: String,
    pub server_version: String,
    pub last_error: String,
    pub reconnects: u64,
    pub proxies: BTreeMap<String, ProxyStatus>,
    pub visitors: BTreeMap<String, String>,
}
pub struct Shared {
    pub status: RwLock<Snapshot>,
    pub config: RwLock<Arc<Config>>,
    pub config_path: PathBuf,
    pub shutdown: CancellationToken,
    pub commands: mpsc::Sender<Command>,
}
pub enum Command {
    Reload(Arc<Config>, oneshot::Sender<Result<()>>),
}
pub struct Client {
    pub shared: Arc<Shared>,
    commands: mpsc::Receiver<Command>,
}
impl Client {
    pub fn new(config: Config, path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel(8);
        Self {
            shared: Arc::new(Shared {
                status: RwLock::new(Snapshot::default()),
                config: RwLock::new(Arc::new(config)),
                config_path: path,
                shutdown: CancellationToken::new(),
                commands: tx,
            }),
            commands: rx,
        }
    }
    pub async fn run(mut self) -> Result<()> {
        {
            let cfg = self.shared.config.read().await;
            cfg.validate()?;
            cfg.validate_resources()?;
        }
        let mut first = true;
        let mut delay = 1u64;
        let mut previous_id = String::new();
        'reconnect: loop {
            let cfg = self.shared.config.read().await.clone();
            let started = Instant::now();
            let shared = self.shared.clone();
            let cancel = CancellationToken::new();
            let session = session(
                cfg.clone(),
                shared.clone(),
                previous_id.clone(),
                cancel.clone(),
            );
            tokio::pin!(session);
            let result = loop {
                tokio::select! {
                    _=self.shared.shutdown.cancelled()=>{cancel.cancel();let _=session.await;return Ok(());},
                    command=self.commands.recv()=>{
                        if let Some(Command::Reload(next,reply))=command {
                            // The admin listener and log writer live outside the session.
                            if let Err(error)=validate_reload(&cfg,&next) {
                                let _=reply.send(Err(error));
                                continue;
                            }
                            previous_id = self.shared.status.read().await.run_id.clone();
                            cancel.cancel();let _=session.await;
                            *self.shared.config.write().await=next;
                            let _=reply.send(Ok(()));first=false;delay=1;
                            tracing::info!("配置已重载，正在重新注册代理");
                            continue 'reconnect;
                        } else {return Ok(());}
                    },
                    result=&mut session=>break result,
                }
            };
            let mut status = self.shared.status.write().await;
            previous_id = status.run_id.clone();
            let logged_in = status.connected;
            status.connected = false;
            for p in status.proxies.values_mut() {
                p.status = "offline".into();
                p.active_connections = 0;
            }
            if let Err(e) = result {
                status.last_error = format!("{e:#}");
                tracing::warn!(error=%status.last_error,"frps 会话结束");
                if first && cfg.login_fail_exit && !logged_in {
                    return Err(e);
                }
            }
            status.reconnects += 1;
            drop(status);
            first = false;
            if started.elapsed() > Duration::from_secs(30) {
                delay = 1;
            }
            let jitter = rand::random::<u64>() % 500;
            tokio::select! {
                _=self.shared.shutdown.cancelled()=>return Ok(()),
                _=tokio::time::sleep(Duration::from_millis(delay*1000+jitter))=>{},
                Some(Command::Reload(next,reply))=self.commands.recv()=>{
                    if let Err(error)=validate_reload(&cfg,&next){let _=reply.send(Err(error));}
                    else{*self.shared.config.write().await=next;let _=reply.send(Ok(()));delay=1;}
                },
            }
            delay = (delay * 2).min(30);
        }
    }
}
async fn session(
    cfg: Arc<Config>,
    shared: Arc<Shared>,
    previous_id: String,
    cancel: CancellationToken,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let mut work_tasks = JoinSet::new();
    let result = tokio::select! {
        _=cancel.cancelled()=>Ok(()),
        result=session_inner(cfg,shared,previous_id,&mut tasks,&mut work_tasks)=>result,
    };
    work_tasks.shutdown().await;
    tasks.shutdown().await;
    result
}
async fn session_inner(
    cfg: Arc<Config>,
    shared: Arc<Shared>,
    previous_id: String,
    tasks: &mut JoinSet<()>,
    work_tasks: &mut JoinSet<()>,
) -> Result<()> {
    {
        let mut status = shared.status.write().await;
        status.connected = false;
        status.proxies.clear();
        status.visitors.clear();
    }
    let auth = Authenticator::new(&cfg.auth)?;
    let connector = Connector::open(cfg.clone()).await?;
    let mut control = connector.connect().await?;
    let mut login = auth.fields("Login").await?;
    let fields = login.as_object_mut().context("认证字段无效")?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        other => other,
    };
    fields.extend(json!({"version":wire::VERSION,"hostname":std::env::var("COMPUTERNAME").or_else(|_|std::env::var("HOSTNAME")).unwrap_or_default(),"os":std::env::consts::OS,"arch":arch,"user":cfg.user,"client_id":cfg.client_id,"run_id":previous_id,"pool_count":cfg.transport.pool_count,"metas":cfg.metadatas}).as_object().context("登录字段无效")?.clone());
    wire::write(&mut control, b'o', &login).await?;
    let response = tokio::time::timeout(Duration::from_secs(10), wire::expect(&mut control, b'1'))
        .await
        .context("登录超时")??;
    let run_id = response.text("run_id").to_owned();
    ensure!(!run_id.is_empty(), "frps 未返回 run_id");
    {
        let mut status = shared.status.write().await;
        status.connected = true;
        status.run_id = run_id.clone();
        status.server_version = response.text("version").into();
        status.last_error.clear();
    }
    tracing::info!(run_id=%run_id,server_version=response.text("version"),"登录 frps 成功");
    let control = stream::encryption(control, auth.encryption_key());
    let (mut reader, mut writer) = tokio::io::split(control);
    // A dedicated reader owns the entire frame read. Timer ticks cannot cancel
    // read_exact halfway through a frame and corrupt the next message.
    let (tx, mut rx) = mpsc::channel(256);
    tasks.spawn(async move {
        loop {
            let m = wire::read(&mut reader).await;
            let end = m.is_err();
            if tx.send(m).await.is_err() || end {
                break;
            }
        }
    });
    let mut runtimes = BTreeMap::new();
    let mut ready = BTreeMap::new();
    let (health_tx, mut health_rx) = mpsc::channel(256);
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    for p in cfg
        .proxies
        .iter()
        .filter(|p| cfg.active(&p.name, p.enabled))
    {
        let name = cfg.proxy_name(&p.name);
        let runtime = ProxyRuntime::new(p.clone())?;
        shared.status.write().await.proxies.insert(
            name.clone(),
            ProxyStatus {
                name: name.clone(),
                r#type: p.r#type.clone(),
                status: "checking".into(),
                remote_addr: String::new(),
                local_addr: crate::transport::format_host(&p.local_ip, p.local_port),
                error: String::new(),
                active_connections: 0,
                total_connections: 0,
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
        runtimes.insert(name.clone(), runtime);
        if p.health_check.r#type.is_empty() {
            ready.insert(name.clone(), true);
            send_control(&mut writer, b'p', &p.registration(&name)).await?;
            shared
                .status
                .write()
                .await
                .proxies
                .get_mut(&name)
                .unwrap()
                .status = "registering".into();
        } else {
            ready.insert(name.clone(), false);
            let p = p.clone();
            let tx = health_tx.clone();
            let http = http.clone();
            tasks.spawn(async move {
                let mut failed = 0;
                let mut last = None;
                loop {
                    let ok = proxy::health_check(&p, &http).await;
                    if ok {
                        failed = 0;
                    } else {
                        failed += 1;
                    }
                    let healthy = ok || failed < p.health_check.max_failed;
                    if last != Some(healthy) {
                        if tx.send((name.clone(), healthy)).await.is_err() {
                            break;
                        }
                        last = Some(healthy);
                    }
                    tokio::time::sleep(Duration::from_secs(p.health_check.interval_seconds)).await;
                }
            });
        }
    }
    for visitor in cfg
        .visitors
        .iter()
        .filter(|v| cfg.active(&v.name, v.enabled))
    {
        let listener = proxy::bind_visitor(visitor)
            .await
            .with_context(|| format!("监听 visitor {} 失败", visitor.name))?;
        shared
            .status
            .write()
            .await
            .visitors
            .insert(visitor.name.clone(), "running".into());
        let v = visitor.clone();
        let cfg = cfg.clone();
        let connector = connector.clone();
        let run_id = run_id.clone();
        let shared = shared.clone();
        tasks.spawn(async move {
            let name = v.name.clone();
            if let Err(e) = proxy::run_visitor(v, listener, cfg, connector, run_id).await {
                tracing::warn!(visitor=%name,error=%e,"访问者停止");
                shared
                    .status
                    .write()
                    .await
                    .visitors
                    .insert(name, format!("error: {e}"));
            }
        });
    }
    let mut last_pong = Instant::now();
    let mut last_ping = Instant::now();
    let mut last_retry = Instant::now();
    let heartbeat = if cfg.transport.heartbeat_interval > 0 {
        cfg.transport.heartbeat_interval as u64
    } else {
        cfg.transport.tcp_mux_keepalive_interval
    };
    let heartbeat_timeout = if cfg.transport.heartbeat_timeout > 0 {
        cfg.transport.heartbeat_timeout as u64
    } else {
        heartbeat * 3
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            message=rx.recv()=>{
                let m=message.context("控制通道关闭")??;
                match m.kind {
                    b'r'=>{
                        ensure!(work_tasks.len()<4096,"工作连接数量超过上限");
                        let connector=connector.clone();let run_id=run_id.clone();let auth=auth.clone();let runtimes=runtimes.clone();let packet_size=cfg.udp_packet_size;
                        work_tasks.spawn(async move {
                            let result:Result<()>=async{
                                let mut stream=connector.connect().await?;
                                let mut msg=auth.fields("NewWorkConns").await?;msg["run_id"]=json!(run_id);
                                wire::write(&mut stream,b'w',&msg).await?;
                                // Idle pool connections are allowed to wait until the session closes.
                                let start=wire::expect(&mut stream,b's').await?;
                                let runtime=runtimes.get(start.text("proxy_name")).context("工作连接引用未知代理")?;
                                tokio::select!{_=runtime.cancel.cancelled()=>Ok(()),r=runtime.handle(stream,start,&auth,packet_size)=>r}
                            }.await;
                            if let Err(e)=result{tracing::debug!(error=%e,"工作连接结束");}
                        });
                    },
                    b'2'=>{
                        if let Some(p)=shared.status.write().await.proxies.get_mut(m.text("proxy_name")){
                            p.error=m.text("error").into();p.remote_addr=m.text("remote_addr").into();p.status=if p.error.is_empty(){"running"}else{"error"}.into();
                            if p.error.is_empty(){tracing::info!(proxy=%p.name,remote=%p.remote_addr,"代理注册成功");}else{tracing::warn!(proxy=%p.name,error=%p.error,"代理注册失败，将自动重试");}
                        }
                    },
                    b'4'=>{m.check_error()?;last_pong=Instant::now();},
                    _=>tracing::debug!(kind=m.kind,"忽略非控制消息"),
                }
            },
            Some((name,healthy))=health_rx.recv()=>{
                ready.insert(name.clone(),healthy);
                if healthy {send_control(&mut writer,b'p',&runtimes[&name].config.registration(&name)).await?;shared.status.write().await.proxies.get_mut(&name).unwrap().status="registering".into();}
                else{send_control(&mut writer,b'c',&json!({"proxy_name":name})).await?;shared.status.write().await.proxies.get_mut(&name).unwrap().status="unhealthy".into();}
            },
            _=tick.tick()=>{
                ensure!(last_pong.elapsed()<Duration::from_secs(heartbeat_timeout),"frps 心跳超时");
                if last_ping.elapsed()>=Duration::from_secs(heartbeat){let ping=auth.fields("HeartBeats").await?;tokio::time::timeout(Duration::from_secs(10),send_control(&mut writer,b'h',&ping)).await.context("发送心跳超时")??;last_ping=Instant::now();}
                let mut retry=Vec::new();
                {let mut snapshot=shared.status.write().await;for(name,p)in &mut snapshot.proxies{let metrics=&runtimes[name].metrics;p.active_connections=metrics.active.load(Ordering::Relaxed);p.total_connections=metrics.total.load(Ordering::Relaxed);p.bytes_sent=metrics.sent.load(Ordering::Relaxed);p.bytes_received=metrics.received.load(Ordering::Relaxed);if last_retry.elapsed()>=Duration::from_secs(10)&&p.status=="error"&&ready[name]{retry.push(name.clone());p.status="registering".into();}}}
                for name in retry{send_control(&mut writer,b'p',&runtimes[&name].config.registration(&name)).await?;}
                if last_retry.elapsed()>=Duration::from_secs(10){last_retry=Instant::now();}
            },
            Some(result)=work_tasks.join_next()=>{if let Err(e)=result{tracing::warn!(error=%e,"工作任务异常");}},
            Some(result)=tasks.join_next()=>{if let Err(e)=result{return Err(e.into());}},
        }
    }
}

async fn send_control<W: tokio::io::AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    kind: u8,
    body: &T,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), wire::write(writer, kind, body))
        .await
        .context("控制消息写入超时")?
}
fn validate_reload(current: &Config, next: &Config) -> Result<()> {
    ensure!(
        next.web_server == current.web_server && next.log == current.log,
        "修改 webServer/log 需要重启客户端"
    );
    next.validate()?;
    next.validate_resources()
}
