//! frp v1: one-byte type, signed big-endian i64 length, JSON payload.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_MESSAGE: usize = 10240;
pub const VERSION: &str = "0.70.0";

#[derive(Clone, Debug)]
pub struct Message {
    pub kind: u8,
    pub body: Value,
}
impl Message {
    pub fn text(&self, key: &str) -> &str {
        self.body[key].as_str().unwrap_or("")
    }
    pub fn check_error(&self) -> Result<()> {
        ensure!(
            self.text("error").is_empty(),
            "frps: {}",
            self.text("error")
        );
        Ok(())
    }
}
pub async fn read<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<Message> {
    let kind = r.read_u8().await.context("读取消息类型")?;
    ensure!(
        b"o1p2cwrsv3h4uinm56".contains(&kind),
        "未知 frp 消息类型: {kind}"
    );
    let len = r.read_i64().await.context("读取消息长度")?;
    ensure!(
        (0..=MAX_MESSAGE as i64).contains(&len),
        "frp 消息长度越界: {len}"
    );
    let mut bytes = vec![0; len as usize];
    r.read_exact(&mut bytes).await.context("读取消息内容")?;
    let body: Value = serde_json::from_slice(&bytes).context("无效 frp JSON 消息")?;
    ensure!(body.is_object(), "frp 消息必须为 JSON 对象");
    Ok(Message { kind, body })
}
pub async fn write<W: AsyncWrite + Unpin + ?Sized, T: Serialize>(
    w: &mut W,
    kind: u8,
    body: &T,
) -> Result<()> {
    let body = serde_json::to_vec(body)?;
    ensure!(body.len() <= MAX_MESSAGE, "frp 消息超过 {MAX_MESSAGE} 字节");
    let mut frame = Vec::with_capacity(body.len() + 9);
    frame.push(kind);
    frame.extend_from_slice(&(body.len() as i64).to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}
pub async fn expect<R: AsyncRead + Unpin + ?Sized>(r: &mut R, kind: u8) -> Result<Message> {
    let m = read(r).await?;
    ensure!(
        m.kind == kind,
        "消息类型不符: 收到 {}，预期 {}",
        m.kind,
        kind
    );
    m.check_error()?;
    Ok(m)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UdpAddress {
    #[serde(rename = "IP")]
    pub ip: String,
    #[serde(rename = "Port")]
    pub port: u16,
    #[serde(rename = "Zone", default)]
    pub zone: String,
}
impl UdpAddress {
    pub fn socket_addr(&self) -> Result<std::net::SocketAddr> {
        Ok(std::net::SocketAddr::new(self.ip.parse()?, self.port))
    }
}
impl From<std::net::SocketAddr> for UdpAddress {
    fn from(a: std::net::SocketAddr) -> Self {
        Self {
            ip: a.ip().to_string(),
            port: a.port(),
            zone: String::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UdpPacket {
    #[serde(rename = "c", default)]
    pub content: String,
    #[serde(rename = "l", skip_serializing_if = "Option::is_none")]
    pub local: Option<UdpAddress>,
    #[serde(rename = "r")]
    pub remote: UdpAddress,
}
