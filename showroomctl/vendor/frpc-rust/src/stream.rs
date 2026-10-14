use aes::{
    Aes128,
    cipher::{BlockEncrypt, KeyInit},
};
use anyhow::{Result, ensure};
use rand::RngCore;
use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::Mutex,
    task::JoinHandle,
    time::Instant,
};

pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}
pub type BoxStream = Box<dyn AsyncStream>;

pub struct MeteredStream {
    stream: BoxStream,
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}
impl MeteredStream {
    pub fn wrap(stream: BoxStream, sent: Arc<AtomicU64>, received: Arc<AtomicU64>) -> BoxStream {
        Box::new(Self {
            stream,
            sent,
            received,
        })
    }
}
impl AsyncRead for MeteredStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        self.received
            .fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        result
    }
}
impl AsyncWrite for MeteredStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            self.sent.fetch_add(n as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

// Dropping a stream must also terminate all adapter tasks and close their sockets.
pub struct TaskStream {
    stream: DuplexStream,
    tasks: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}
impl TaskStream {
    pub fn new(stream: DuplexStream, mut tasks: Vec<JoinHandle<()>>) -> Self {
        let writer = tasks.pop();
        Self {
            stream,
            tasks,
            writer,
        }
    }
}
impl Drop for TaskStream {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some(writer) = &self.writer {
            writer.abort();
        }
    }
}
impl AsyncRead for TaskStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, b)
    }
}
impl AsyncWrite for TaskStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(Pin::new(&mut self.stream).poll_shutdown(cx))?;
        // Closing the duplex only enqueues EOF. Wait until the adapter has
        // actually flushed all queued bytes before callers can drop this stream.
        if let Some(writer) = &mut self.writer {
            let result = std::task::ready!(std::future::Future::poll(Pin::new(writer), cx));
            self.writer.take();
            result.map_err(io::Error::other)?;
        }
        Poll::Ready(Ok(()))
    }
}

// CFB128 keeps byte position across partial reads/writes, as Go cipher.Stream does.
struct Cfb {
    cipher: Aes128,
    feedback: [u8; 16],
    block: [u8; 16],
    pos: usize,
}
impl Cfb {
    fn new(key: &[u8; 16], iv: [u8; 16]) -> Self {
        Self {
            cipher: Aes128::new(key.into()),
            feedback: iv,
            block: [0; 16],
            pos: 0,
        }
    }
    fn apply(&mut self, data: &mut [u8], decrypt: bool) {
        for b in data {
            if self.pos == 0 {
                self.block = self.feedback;
                self.cipher.encrypt_block((&mut self.block).into());
            }
            let input = *b;
            *b ^= self.block[self.pos];
            self.feedback[self.pos] = if decrypt { input } else { *b };
            self.pos = (self.pos + 1) % 16;
        }
    }
}
pub fn encryption(stream: BoxStream, password: &[u8]) -> BoxStream {
    let mut key = [0; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"frp", 64, &mut key);
    let mut iv = [0; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let (public, internal) = tokio::io::duplex(64 * 1024);
    let (mut net_r, mut net_w) = tokio::io::split(stream);
    let (mut app_r, mut app_w) = tokio::io::split(internal);
    let writer = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut cfb = Cfb::new(&key, iv);
            let mut buf = vec![0; 16384];
            let mut started = false;
            loop {
                let n = app_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                if !started {
                    net_w.write_all(&iv).await?;
                    started = true;
                }
                cfb.apply(&mut buf[..n], false);
                net_w.write_all(&buf[..n]).await?;
                net_w.flush().await?;
            }
            net_w.shutdown().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!(error=%e, "加密写入结束");
        }
    });
    let reader = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut iv = [0; 16];
            net_r.read_exact(&mut iv).await?;
            let mut cfb = Cfb::new(&key, iv);
            let mut buf = vec![0; 16384];
            loop {
                let n = net_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                cfb.apply(&mut buf[..n], true);
                app_w.write_all(&buf[..n]).await?;
            }
            app_w.shutdown().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!(error=%e,"加密读取结束");
        }
        let _ = app_w.shutdown().await;
    });
    Box::new(TaskStream::new(public, vec![reader, writer]))
}

fn masked_crc(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
        .rotate_right(15)
        .wrapping_add(0xa282ead8)
}
pub fn compression(stream: BoxStream) -> BoxStream {
    let (public, internal) = tokio::io::duplex(64 * 1024);
    let (mut net_r, mut net_w) = tokio::io::split(stream);
    let (mut app_r, mut app_w) = tokio::io::split(internal);
    let writer = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut started = false;
            let mut buf = vec![0; 65536];
            let mut encoder = snap::raw::Encoder::new();
            loop {
                let n = app_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                if !started {
                    net_w.write_all(b"\xff\x06\x00\x00sNaPpY").await?;
                    started = true;
                }
                let packed = encoder.compress_vec(&buf[..n])?;
                let (kind, bytes) = if packed.len() < n {
                    (0u8, packed.as_slice())
                } else {
                    (1u8, &buf[..n])
                };
                let len = bytes.len() + 4;
                net_w
                    .write_all(&[kind, len as u8, (len >> 8) as u8, (len >> 16) as u8])
                    .await?;
                net_w
                    .write_all(&masked_crc(&buf[..n]).to_le_bytes())
                    .await?;
                net_w.write_all(bytes).await?;
                net_w.flush().await?;
            }
            net_w.shutdown().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!(error=%e,"压缩写入结束");
        }
    });
    let reader = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut started = false;
            let mut decoder = snap::raw::Decoder::new();
            loop {
                let kind = match net_r.read_u8().await {
                    Ok(v) => v,
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                };
                let mut l = [0; 3];
                net_r.read_exact(&mut l).await?;
                let len = l[0] as usize | (l[1] as usize) << 8 | (l[2] as usize) << 16;
                ensure!(len <= 1_048_576, "Snappy 分块过大");
                let mut body = vec![0; len];
                net_r.read_exact(&mut body).await?;
                if kind == 0xff {
                    ensure!(body == b"sNaPpY", "Snappy 标识无效");
                    started = true;
                    continue;
                }
                ensure!(started, "缺少 Snappy 标识");
                if kind >= 0x80 {
                    continue;
                }
                ensure!(kind <= 1 && len >= 4, "Snappy 分块无效");
                let data = if kind == 0 {
                    ensure!(
                        snap::raw::decompress_len(&body[4..])? <= 65536,
                        "Snappy 解压长度越界"
                    );
                    decoder.decompress_vec(&body[4..])?
                } else {
                    ensure!(len - 4 <= 65536, "Snappy 原始块越界");
                    body[4..].to_vec()
                };
                ensure!(
                    masked_crc(&data) == u32::from_le_bytes(body[..4].try_into()?),
                    "Snappy 校验失败"
                );
                app_w.write_all(&data).await?;
            }
            app_w.shutdown().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!(error=%e,"压缩读取结束");
        }
        let _ = app_w.shutdown().await;
    });
    Box::new(TaskStream::new(public, vec![reader, writer]))
}

#[derive(Debug)]
pub struct RateLimit {
    bytes_per_second: u64,
    next: Mutex<Instant>,
}
impl RateLimit {
    pub fn new(bytes_per_second: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes_per_second,
            next: Mutex::new(Instant::now()),
        })
    }
    pub async fn consume(&self, bytes: usize) {
        if self.bytes_per_second == 0 {
            return;
        }
        let when = {
            let mut next = self.next.lock().await;
            let when = (*next).max(Instant::now());
            *next = when + Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
            when
        };
        tokio::time::sleep_until(when).await;
    }
}
pub fn limited(stream: BoxStream, rate: Arc<RateLimit>) -> BoxStream {
    let (public, internal) = tokio::io::duplex(32768);
    let (net_r, net_w) = tokio::io::split(stream);
    let (app_r, app_w) = tokio::io::split(internal);
    async fn pipe<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        mut r: R,
        mut w: W,
        rate: Arc<RateLimit>,
    ) {
        let mut buf = [0; 16384];
        loop {
            let n = match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            rate.consume(n).await;
            if w.write_all(&buf[..n]).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    }
    let a = tokio::spawn(pipe(net_r, app_w, rate.clone()));
    let b = tokio::spawn(pipe(app_r, net_w, rate));
    Box::new(TaskStream::new(public, vec![a, b]))
}
pub fn wrap(
    mut stream: BoxStream,
    key: &[u8],
    encrypt: bool,
    compress: bool,
    rate: Option<Arc<RateLimit>>,
) -> BoxStream {
    if let Some(rate) = rate {
        stream = limited(stream, rate);
    }
    if encrypt {
        stream = encryption(stream, key);
    }
    if compress {
        stream = compression(stream);
    }
    stream
}
