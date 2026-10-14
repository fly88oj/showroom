use crate::{
    auth::{Authenticator, auth_key, timestamp},
    config::{Config, Proxy, Visitor, prefix},
    stream::{self, BoxStream, RateLimit},
    transport::{Connector, tcp_connect},
    wire::{self, Message, UdpPacket},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, UdpSocket},
    sync::{Mutex, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Metrics {
    pub active: AtomicU64,
    pub total: AtomicU64,
    pub sent: Arc<AtomicU64>,
    pub received: Arc<AtomicU64>,
}
struct ConnectionGuard(Arc<Metrics>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}
pub struct ProxyRuntime {
    pub config: Proxy,
    pub cancel: CancellationToken,
    pub rate: Option<Arc<RateLimit>>,
    pub metrics: Arc<Metrics>,
    udp_cancel: Mutex<Option<CancellationToken>>,
}
impl ProxyRuntime {
    pub fn new(config: Proxy) -> Result<Arc<Self>> {
        let bytes = config.transport.bandwidth_bytes()?;
        let rate = if bytes > 0 && config.transport.bandwidth_limit_mode == "client" {
            Some(RateLimit::new(bytes))
        } else {
            None
        };
        Ok(Arc::new(Self {
            config,
            cancel: CancellationToken::new(),
            rate,
            metrics: Arc::new(Metrics::default()),
            udp_cancel: Mutex::new(None),
        }))
    }
    pub async fn handle(
        &self,
        raw: BoxStream,
        start: Message,
        auth: &Authenticator,
        packet_size: usize,
    ) -> Result<()> {
        let mut remote = stream::wrap(
            raw,
            auth.encryption_key(),
            self.config.transport.use_encryption,
            self.config.transport.use_compression,
            self.rate.clone(),
        );
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        self.metrics.total.fetch_add(1, Ordering::Relaxed);
        let _guard = ConnectionGuard(self.metrics.clone());
        if matches!(self.config.r#type.as_str(), "udp" | "sudp") {
            let cancel = self.cancel.child_token();
            if self.config.r#type == "udp" {
                if let Some(previous) = self.udp_cancel.lock().await.replace(cancel.clone()) {
                    previous.cancel();
                }
            }
            return tokio::select! {_=cancel.cancelled()=>Ok(()),r=udp_forward(remote,&self.config,packet_size,self.metrics.clone())=>r};
        }
        remote = stream::MeteredStream::wrap(
            remote,
            self.metrics.sent.clone(),
            self.metrics.received.clone(),
        );
        let header =
            proxy_header_from_start(&start, &self.config.transport.proxy_protocol_version)?;
        if !self.config.plugin.r#type.is_empty() {
            return crate::plugin::handle(remote, &self.config.plugin, header).await;
        }
        let mut local = tokio::time::timeout(
            Duration::from_secs(10),
            tcp_connect(&self.config.local_ip, self.config.local_port, "", 30),
        )
        .await
        .context("本地连接超时")??;
        if let Some(header) = header {
            local.write_all(&header).await?;
        }
        tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
        Ok(())
    }
}
pub fn proxy_header_from_start(m: &Message, version: &str) -> Result<Option<Vec<u8>>> {
    if version.is_empty() || m.text("src_addr").is_empty() {
        return Ok(None);
    }
    let src: IpAddr = m.text("src_addr").parse()?;
    let dst: IpAddr = if m.text("dst_addr").is_empty() {
        if src.is_ipv4() { "127.0.0.1" } else { "::1" }
    } else {
        m.text("dst_addr")
    }
    .parse()?;
    let sport = u16::try_from(m.body["src_port"].as_u64().unwrap_or(0))?;
    let dport = u16::try_from(m.body["dst_port"].as_u64().unwrap_or(0))?;
    Ok(Some(proxy_header(
        SocketAddr::new(src, sport),
        SocketAddr::new(dst, dport),
        version,
        false,
    )?))
}
pub fn proxy_header(src: SocketAddr, dst: SocketAddr, version: &str, udp: bool) -> Result<Vec<u8>> {
    ensure!(
        src.is_ipv4() == dst.is_ipv4(),
        "PROXY protocol 源/目标地址族不同"
    );
    if version == "v1" {
        ensure!(!udp, "PROXY v1 不支持 UDP");
        return Ok(format!(
            "PROXY {} {} {} {} {}\r\n",
            if src.is_ipv4() { "TCP4" } else { "TCP6" },
            src.ip(),
            dst.ip(),
            src.port(),
            dst.port()
        )
        .into_bytes());
    }
    ensure!(version == "v2", "无效 PROXY protocol 版本");
    let mut out = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    out.push(0x21);
    out.push((if src.is_ipv4() { 0x10 } else { 0x20 }) | if udp { 2 } else { 1 });
    out.extend((if src.is_ipv4() { 12u16 } else { 36u16 }).to_be_bytes());
    match (src.ip(), dst.ip()) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            out.extend(a.octets());
            out.extend(b.octets());
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            out.extend(a.octets());
            out.extend(b.octets());
        }
        _ => unreachable!(),
    }
    out.extend(src.port().to_be_bytes());
    out.extend(dst.port().to_be_bytes());
    Ok(out)
}
async fn udp_forward(
    stream: BoxStream,
    p: &Proxy,
    packet_size: usize,
    metrics: Arc<Metrics>,
) -> Result<()> {
    let destination = tokio::net::lookup_host((p.local_ip.as_str(), p.local_port))
        .await?
        .next()
        .context("UDP 本地目标无法解析")?;
    let (mut r, mut w) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<UdpPacket>(256);
    let mut tasks = JoinSet::new();
    let (input, mut packets) = mpsc::channel(256);
    tasks.spawn(async move {
        loop {
            let m = wire::read(&mut r).await;
            let end = m.is_err();
            if input.send(m).await.is_err() || end {
                break;
            }
        }
    });
    struct UdpPeer {
        socket: Arc<UdpSocket>,
        last: Arc<std::sync::Mutex<tokio::time::Instant>>,
        cancel: CancellationToken,
    }
    impl Drop for UdpPeer {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
    let mut peers: HashMap<SocketAddr, UdpPeer> = HashMap::new();
    let mut timer = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            raw=packets.recv()=>{
                let m=raw.context("UDP 隧道读取结束")??;
                if m.kind==b'h'{continue;}ensure!(m.kind==b'u',"UDP 隧道收到非 UDP 消息");
                let packet:UdpPacket=serde_json::from_value(m.body)?;
                let remote=packet.remote.socket_addr()?;
                let mut data=STANDARD.decode(&packet.content)?;
                ensure!(data.len()<=packet_size,"UDP 数据包超出 udpPacketSize");
                if !peers.contains_key(&remote) {
                    if peers.len()>=4096 {continue;}
                    let socket=Arc::new(UdpSocket::bind(if destination.is_ipv4(){"0.0.0.0:0"}else{"[::]:0"}).await?);
                    socket.connect(destination).await?;
                    let reader=socket.clone();let responses=tx.clone();let address=packet.remote.clone();let counts=metrics.clone();
                    let last=Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
                    let last_read=last.clone();let cancel=CancellationToken::new();let stop=cancel.clone();
                    tasks.spawn(async move {
                        let mut buf=vec![0;packet_size+1];
                        loop {
                            let result=tokio::select!{_=stop.cancelled()=>break,result=reader.recv(&mut buf)=>result};
                            let n=match result{Ok(n)=>n,Err(_)=>break};
                            if n>packet_size {continue;}
                            *last_read.lock().unwrap()=tokio::time::Instant::now();
                            counts.sent.fetch_add(n as u64,Ordering::Relaxed);
                            let _=responses.try_send(UdpPacket{content:STANDARD.encode(&buf[..n]),local:None,remote:address.clone()});
                        }
                        stop.cancel();
                    });
                    peers.insert(remote,UdpPeer{socket,last,cancel});
                    if !p.transport.proxy_protocol_version.is_empty(){let mut header=proxy_header(remote,destination,&p.transport.proxy_protocol_version,true)?;header.extend(data);data=header;}
                }
                if let Some(peer)=peers.get(&remote){
                    *peer.last.lock().unwrap()=tokio::time::Instant::now();
                    if peer.socket.send(&data).await.is_err(){peers.remove(&remote);}
                    else{metrics.received.fetch_add(data.len() as u64,Ordering::Relaxed);}
                }
            },
            Some(packet)=rx.recv()=>{wire::write(&mut w,b'u',&packet).await?;},
            _=timer.tick()=>{peers.retain(|_,peer|!peer.cancel.is_cancelled()&&peer.last.lock().unwrap().elapsed()<Duration::from_secs(60));wire::write(&mut w,b'h',&json!({})).await?;},
            Some(_)=tasks.join_next()=>{},
        }
    }
}
pub async fn visitor_stream(
    v: &Visitor,
    cfg: &Config,
    connector: &Connector,
    run_id: &str,
) -> Result<BoxStream> {
    let mut conn = connector.connect().await?;
    let now = timestamp();
    let user = if v.server_user.is_empty() {
        &cfg.user
    } else {
        &v.server_user
    };
    wire::write(&mut conn,b'v',&json!({"run_id":run_id,"proxy_name":prefix(user,&v.server_name),"sign_key":auth_key(&v.secret_key,now),"timestamp":now,"use_encryption":v.transport.use_encryption,"use_compression":v.transport.use_compression})).await?;
    tokio::time::timeout(Duration::from_secs(10), wire::expect(&mut conn, b'3'))
        .await
        .context("visitor 握手超时")??;
    Ok(stream::wrap(
        conn,
        v.secret_key.as_bytes(),
        v.transport.use_encryption,
        v.transport.use_compression,
        None,
    ))
}
pub enum VisitorListener {
    Tcp(TcpListener),
    Udp(UdpSocket),
}
pub async fn bind_visitor(v: &Visitor) -> Result<VisitorListener> {
    match v.r#type.as_str() {
        "stcp" => Ok(VisitorListener::Tcp(
            TcpListener::bind((v.bind_addr.as_str(), v.bind_port)).await?,
        )),
        "sudp" => Ok(VisitorListener::Udp(
            UdpSocket::bind((v.bind_addr.as_str(), v.bind_port)).await?,
        )),
        _ => bail!("不支持的 visitor 类型"),
    }
}
pub async fn run_visitor(
    v: Visitor,
    listener: VisitorListener,
    cfg: Arc<Config>,
    connector: Arc<Connector>,
    run_id: String,
) -> Result<()> {
    match listener {
        VisitorListener::Tcp(listener) => {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (mut local,_)=accepted?;let v=v.clone();let cfg=cfg.clone();let connector=connector.clone();let run_id=run_id.clone();
                        if tasks.len()>=4096 {continue;}
                        tasks.spawn(async move {let result:Result<()>=async{let mut remote=visitor_stream(&v,&cfg,&connector,&run_id).await?;tokio::io::copy_bidirectional(&mut local,&mut remote).await?;Ok(())}.await;if let Err(e)=result {tracing::warn!(visitor=%v.name,error=%e,"访问者连接失败");}});
                    },
                    Some(_)=tasks.join_next()=>{},
                }
            }
        }
        VisitorListener::Udp(socket) => {
            let mut buf = vec![0; cfg.udp_packet_size + 1];
            loop {
                let (n, peer) = socket.recv_from(&mut buf).await?;
                if n > cfg.udp_packet_size {
                    continue;
                }
                let connection = visitor_stream(&v, &cfg, &connector, &run_id).await;
                let conn = match connection {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(visitor=%v.name,error=%e,"SUDP 连接失败");
                        continue;
                    }
                };
                let (mut reader, mut writer) = tokio::io::split(conn);
                wire::write(
                    &mut writer,
                    b'u',
                    &UdpPacket {
                        content: STANDARD.encode(&buf[..n]),
                        local: None,
                        remote: peer.into(),
                    },
                )
                .await?;
                let mut peers = HashMap::from([(peer, tokio::time::Instant::now())]);
                let (tx, mut rx) = mpsc::channel(256);
                let mut reader_tasks = JoinSet::new();
                reader_tasks.spawn(async move {
                    loop {
                        let m = wire::read(&mut reader).await;
                        let end = m.is_err();
                        if tx.send(m).await.is_err() || end {
                            break;
                        }
                    }
                });
                let mut idle = tokio::time::interval(Duration::from_secs(15));
                loop {
                    tokio::select! {
                        packet=socket.recv_from(&mut buf)=>{let (n,peer)=packet?;if n>cfg.udp_packet_size{continue;}
                        if peers.len()>=4096 && !peers.contains_key(&peer){continue;}peers.insert(peer,tokio::time::Instant::now());if wire::write(&mut writer,b'u',&UdpPacket{content:STANDARD.encode(&buf[..n]),local:None,remote:peer.into()}).await.is_err(){break;}},
                        result=rx.recv()=>{let msg=match result{Some(Ok(m))=>m,_=>break};if msg.kind!=b'u'{continue;}let packet:UdpPacket=serde_json::from_value(msg.body)?;let peer=packet.remote.socket_addr()?;if peers.contains_key(&peer){let data=STANDARD.decode(&packet.content)?;if data.len()<=cfg.udp_packet_size{socket.send_to(&data,peer).await?;}}},
                        _=idle.tick()=>{peers.retain(|_,time|time.elapsed()<Duration::from_secs(60));if peers.is_empty(){break;}},
                    }
                }
            }
        }
    }
}
pub async fn health_check(p: &Proxy, http: &reqwest::Client) -> bool {
    let check = async {
        if p.health_check.r#type == "tcp" {
            tcp_connect(&p.local_ip, p.local_port, "", 0)
                .await
                .map(|_| ())
        } else {
            let path = if p.health_check.path.starts_with('/') {
                p.health_check.path.clone()
            } else {
                format!("/{}", p.health_check.path)
            };
            let mut req = http.get(format!(
                "http://{}{path}",
                crate::transport::format_host(&p.local_ip, p.local_port)
            ));
            for h in &p.health_check.http_headers {
                req = req.header(&h.name, &h.value);
            }
            let resp = req.send().await?;
            ensure!(
                resp.status() == reqwest::StatusCode::OK,
                "健康检查未返回 200"
            );
            Ok(())
        }
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(p.health_check.timeout_seconds), check).await,
        Ok(Ok(()))
    )
}
