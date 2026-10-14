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
    is_srtp, header, FLAG_UPLINK, HDR_LEN, OpenStream, SealStream, SrtpError,
};
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

/// 密封通道：阶段① 读至 wrap 解出 k（期间解出的明文即时转上游）；阶段② 全双工泵。
async fn sealed(mut conn: TcpStream, mut up: TcpStream, keys: &PlatformKeys) -> Result<(), SrtpError> {
    let mut open = OpenStream::begin_wrap(&keys.priv_pkcs8_pem())?;
    let mut plain = Vec::with_capacity(PUMP_BUF);
    let k = loop {
        let mut chunk = [0u8; PUMP_BUF];
        // 阶段① 的读失败/EOF 都按截断处理（wrap 未解出即断流）
        let n = tokio::time::timeout(HEADER_TIMEOUT, conn.read(&mut chunk))
            .await
            .map_err(|_| SrtpError::Truncated)? // wrap 到齐前停滞：同 20s 上限，不养滞留任务
            .map_err(|_| SrtpError::Truncated)?;
        if n == 0 {
            return Err(SrtpError::Truncated);
        }
        open.push(&chunk[..n], &mut plain)?;
        if !plain.is_empty() {
            up.write_all(&plain).await.map_err(|_| SrtpError::Truncated)?;
            plain.clear();
        }
        if let Some(k) = open.session_key() {
            break k;
        }
    };
    eprintln!("showroomctl: tunnel: SRTP 会话建立（wrap 已解，下行对称封装开始）");

    let (mut cr, mut cw) = conn.into_split();
    let (mut ur, mut uw) = up.into_split();

    // 下行先发传输头（flags=1：会话密钥直传，无 wrap）
    cw.write_all(&header(true)).await.map_err(|_| SrtpError::Truncated)?;

    let up_task = async move {
        let mut chunk = [0u8; PUMP_BUF];
        loop {
            match cr.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    open.push(&chunk[..n], &mut plain)?;
                    if !plain.is_empty() {
                        uw.write_all(&plain).await.map_err(|_| SrtpError::Truncated)?;
                        plain.clear();
                    }
                }
                Err(_) => return Err(SrtpError::Truncated),
            }
        }
        open.finish(&mut plain)?; // 上行 EOF：flush 尾段 + 半关闭上游写侧
        if !plain.is_empty() {
            uw.write_all(&plain).await.map_err(|_| SrtpError::Truncated)?;
        }
        let _ = uw.shutdown().await;
        Ok(())
    };

    let down_task = async move {
        let mut seal = SealStream::new(k, Box::new(rand::rngs::OsRng));
        let mut wire = Vec::with_capacity(PUMP_BUF);
        let mut chunk = [0u8; PUMP_BUF];
        loop {
            match ur.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    seal.push(&chunk[..n], &mut wire)?;
                    if !wire.is_empty() {
                        cw.write_all(&wire).await.map_err(|_| SrtpError::Truncated)?;
                        wire.clear();
                    }
                }
                Err(_) => return Err(SrtpError::Truncated),
            }
        }
        seal.finish(&mut wire)?; // 下行 EOF：flush 尾段 + 半关闭客户端写侧
        if !wire.is_empty() {
            cw.write_all(&wire).await.map_err(|_| SrtpError::Truncated)?;
        }
        let _ = cw.shutdown().await;
        Ok(())
    };

    // 收束语义（评审二轮修正）：正常链 = 客户端 EOF → 上行 flush+半关 → 上游 EOF →
    // 下行 flush+半关，两分支自然先后结束（等齐）；任一分支**错误**则立即返回——
    // 对侧 future 被 drop，其 socket 随之关闭，两端读到 EOF/FIN 级联拆除，无滞留任务
    // （join! 会把错误侧的对侧无限挂住：上游不关就永不返回，实锤）。
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
