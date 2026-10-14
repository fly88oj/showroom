//! gateway：SRTP/明文混合网关（隧道 → Caddy site B 的唯一落点）。
//!
//! 每连接首批 6B 探测：`"SRTP" 0x01 0x00` → 密封通道（上行解封→明文转上游；
//! 下行以同一会话密钥 k 对称封装——TLS 式：两端知 k，中转只见高熵字节）；
//! 其余（HTTP 方法、TLS 0x16、任意字节）→ **字节级明文直通**——Docker Push 的
//! 公网加密 import 通道与匿名公开访问在此路径零变化。
//!
//! 密封通道按请求顺序语义工作：客户端先发（HTTP 请求在密封流内），wrap 头解出 k
//! 之前不产生下行；随后全双工泵（上行尾段/下行尾段在各自 EOF 时 flush）。
//! 压缩不在本层（实测结论：传输级压缩对高熵/动态内容得不偿失；内层 HTTP gzip
//! 天然落在正确的先压后封顺序）。

use std::net::SocketAddr;
use std::sync::Arc;

use showroom_common::srtp::{
    is_srtp, header, FLAG_UPLINK, HDR_LEN, SrtpError, TReader, TWriter,
};

/// 上行记录分帧（SRTP v1）：解封明文 = 重复的 `[8B 大端长度][载荷]`。中继不支持
/// 客户端半关闭（用户侧 EOF 即拆整流，真机实弹），网关不能靠 EOF 判请求完结——
/// 故由帧界驱动转发：读满一帧即转发，无需等流尾。单帧上限 1GiB 防御坏帧。
const MAX_FRAME: u64 = 1 << 30;

/// 分帧转发状态机：喂入解封明文 → 返回本次应写给上游的字节（帧头已剥，调用方写完即弃）。
/// `finish` 在客户端 EOF 时校验无残帧。
#[derive(Default)]
struct FrameFwd {
    buf: Vec<u8>,
    /// None=等帧头；Some(n)=当前帧剩余载荷字节
    pending: Option<u64>,
}

impl FrameFwd {
    fn feed(&mut self, data: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            match self.pending {
                None => {
                    if self.buf.len() < 8 {
                        break;
                    }
                    let n = u64::from_be_bytes(self.buf[..8].try_into().expect("8B"));
                    if n > MAX_FRAME {
                        return Err(SrtpError::Truncated);
                    }
                    self.pending = Some(n);
                    self.buf.drain(..8);
                }
                Some(0) => {
                    self.pending = None; // 帧完结 → 立即进入下一帧头态（多帧流水线）
                }
                Some(n) => {
                    let take = (n as usize).min(self.buf.len());
                    out.extend_from_slice(&self.buf[..take]);
                    self.buf.drain(..take);
                    self.pending = Some(n - take as u64);
                    if take == 0 {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    fn finish(&self) -> Result<(), SrtpError> {
        if self.buf.is_empty() && self.pending.is_none() {
            Ok(())
        } else {
            Err(SrtpError::Truncated)
        }
    }
}
use showroom_common::PlatformKeys;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 首批字节（传输头）读取时限：到点未齐即断（防慢速占位）。
const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// 泵缓冲（与常见内核缓冲对齐；明文直通用 copy_bidirectional 内建）。
const PUMP_BUF: usize = 64 * 1024;

/// 网关主循环（serve 派生的每连接一个 task；本函数永不返回）。
pub async fn serve(listener: TcpListener, upstream_port: u16, keys: Option<Arc<PlatformKeys>>) {
    let upstream = SocketAddr::from(([127, 0, 0, 1], upstream_port));
    loop {
        let (conn, _peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("showroomctl: tunnel: 网关 accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await; // EMFILE 等忙退避
                continue;
            }
        };
        let keys = keys.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(conn, upstream, keys).await {
                eprintln!("showroomctl: tunnel: 网关连接: {e}");
            }
        });
    }
}

async fn handle(mut conn: TcpStream, upstream: SocketAddr, keys: Option<Arc<PlatformKeys>>) -> Result<(), String> {
    let mut prefix = [0u8; HDR_LEN];
    tokio::time::timeout(HEADER_TIMEOUT, conn.read_exact(&mut prefix))
        .await
        .map_err(|_| "读取传输头超时".to_string())?
        .map_err(|e| format!("读取传输头: {e}"))?;
    let mut up = TcpStream::connect(upstream).await.map_err(|e| format!("连上游 {upstream}: {e}"))?;

    if is_srtp(&prefix) {
        if prefix[5] != FLAG_UPLINK {
            return Err("上行 SRTP 头 flags 非法（须为 0）".into());
        }
        let keys = keys.ok_or_else(|| "SRTP 到达但平台私钥缺失（state/platform_rsa）".to_string())?;
        sealed(conn, up, &keys).await.map_err(|e| format!("密封通道: {e}"))
    } else {
        // 明文直通：先回放已读前缀，再双向透传（字节级，含 Docker Push 长流）
        up.write_all(&prefix).await.map_err(|e| format!("直通前缀回放: {e}"))?;
        tokio::io::copy_bidirectional(&mut conn, &mut up)
            .await
            .map_err(|e| format!("直通双向泵: {e}"))?;
        Ok(())
    }
}

/// 密封通道：字节级取 384B wrap 解出 k（不与 §2 流式状态机混交接，杜绝混包窗口），
/// 其后全部字节进传输记录层 TReader → 帧转发（FrameFwd）→ 上游；下行 TWriter 对称
/// 封装。中继无半关闭语义（真机实弹）：请求完结只由帧界驱动，连接关闭即整体收束。
async fn sealed(mut conn: TcpStream, up: TcpStream, keys: &PlatformKeys) -> Result<(), SrtpError> {
    use showroom_common::{srtp::unwrap_key, WRAP_LEN};
    let mut raw: Vec<u8> = Vec::with_capacity(PUMP_BUF);
    let mut chunk = [0u8; PUMP_BUF];
    let k = loop {
        let n = tokio::time::timeout(HEADER_TIMEOUT, conn.read(&mut chunk))
            .await
            .map_err(|_| SrtpError::Truncated)?
            .map_err(|_| SrtpError::Truncated)?;
        if n == 0 {
            return Err(SrtpError::Truncated); // wrap 未到齐即断流
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() >= WRAP_LEN {
            break unwrap_key(&keys.priv_pkcs8_pem(), &raw[..WRAP_LEN])?;
        }
    };
    eprintln!("showroomctl: tunnel: SRTP 会话建立（wrap 已解，下行对称封装开始）");

    let (mut cr, mut cw) = conn.into_split();
    let (mut ur, mut uw) = up.into_split();

    // 下行先发传输头（flags=1：会话密钥直传，无 wrap）
    cw.write_all(&header(true)).await.map_err(|_| SrtpError::Truncated)?;

    let up_task = async move {
        let mut reader = TReader::new(k);
        let mut fwd = FrameFwd::default();
        let mut plain = Vec::with_capacity(PUMP_BUF);
        let mut chunk = [0u8; PUMP_BUF];
        // wrap 之后的残余字节（同包到达的记录）先喂入
        reader.push(&raw[WRAP_LEN..], &mut plain)?;
        loop {
            let out = fwd.feed(&plain)?;
            if !out.is_empty() {
                uw.write_all(&out).await.map_err(|_| SrtpError::Truncated)?;
            }
            plain.clear();
            match cr.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => reader.push(&chunk[..n], &mut plain)?,
                Err(_) => return Err(SrtpError::Truncated),
            }
        }
        reader.finish()?;
        fwd.finish()?;
        let _ = uw.shutdown().await;
        Ok(())
    };

    let down_task = async move {
        let mut writer = TWriter::new(k, Box::new(rand::rngs::OsRng));
        let mut wire = Vec::with_capacity(PUMP_BUF);
        let mut chunk = [0u8; PUMP_BUF];
        loop {
            match ur.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    writer.record(&chunk[..n], &mut wire)?;
                    if !wire.is_empty() {
                        cw.write_all(&wire).await.map_err(|_| SrtpError::Truncated)?;
                        wire.clear();
                    }
                }
                Err(_) => return Err(SrtpError::Truncated),
            }
        }
        let _ = cw.shutdown().await;
        Ok(())
    };

    // 收束语义：任一分支完成/错误即整体结束（对侧 drop → socket 关闭级联拆除；
    // join! 会把先完成侧的对侧挂到对端关闭为止，滞留任务——评审二轮实锤）
    tokio::pin!(up_task);
    tokio::pin!(down_task);
    let up_first = tokio::select! {
        biased;
        r = &mut up_task => r,
        r = &mut down_task => return r,
    };
    up_first?;
    down_task.await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_fwd_multiframe_and_partial_feed() {
        let mut f = FrameFwd::default();
        let frame = |p: &[u8]| {
            let mut v = (p.len() as u64).to_be_bytes().to_vec();
            v.extend_from_slice(p);
            v
        };
        let wire = [frame(b"hello"), frame(b"world!")].concat();
        let mut got = Vec::new();
        for c in wire.chunks(3) {
            got.extend_from_slice(&f.feed(c).unwrap());
        }
        assert_eq!(got, b"helloworld!");
        f.finish().unwrap();
    }

    #[test]
    fn frame_fwd_trailing_partial_rejected() {
        let mut f = FrameFwd::default();
        let mut v = (5u64).to_be_bytes().to_vec();
        v.extend_from_slice(b"abc");
        assert_eq!(f.feed(&v).unwrap(), b"abc");
        assert_eq!(f.finish().unwrap_err(), SrtpError::Truncated);
    }

    #[test]
    fn frame_fwd_oversize_frame_rejected() {
        let mut f = FrameFwd::default();
        assert_eq!(f.feed(&(MAX_FRAME + 1).to_be_bytes()).unwrap_err(), SrtpError::Truncated);
    }
}
