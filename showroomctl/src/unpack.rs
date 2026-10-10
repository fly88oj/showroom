//! 解密后字节流的魔数自适应解包（zstd / gzip / 直通）：import（docker load，publish.rs）
//! 与 db-restore（pg_restore stdin，dbmaint.rs）共用的唯一探测封装（自 dbmaint 的
//! RestoreSink 泛化而来）。解密产物三种形态：
//! - zstd 帧流（28 B5 2F FD）：浏览器压缩上传（wasm 垫片逐 push 独立帧，拼接帧流）、
//!   CLI `docker save | zstd`、服务端 dump 生成侧的 zstd-19 包裹；
//! - gzip 流（1F 8B 08）：降级路径（浏览器侧 zstd 不可用时的 gzip 替代算法）；
//! - 裸流：`docker save` 原生 tar、原生 pg_dump -Fc（"PGDMP" 头）——直通交下游。
//! 按前 4 字节魔数一次性路由；不足 4 字节的残流 flush 时直通（非任一完整格式，
//! 交 docker load / pg_restore 如实报错）。

use std::io::Write;

/// zstd 帧魔数。
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
/// gzip 魔数前缀：1F 8B + 压缩方法 08（deflate；合法 gzip 恒为此三字节，比裸 2 字节
/// 更抗裸流误命中）。
pub const GZIP_MAGIC: [u8; 3] = [0x1F, 0x8B, 0x08];

/// 路径选择（纯函数）：前 4 字节命中 zstd 魔数 → 需 zstd 解压。
/// 不足 4 字节不可能是任一格式（zstd 帧头恰 4B、gzip 头 ≥3B 且 tar/-Fc 头更长），
/// 返回 false 由残流兜底。
pub fn is_zstd(prefix: &[u8]) -> bool {
    prefix.len() >= 4 && prefix[..4] == ZSTD_MAGIC
}

/// 路径选择（纯函数）：前 3 字节命中 gzip 魔数（1F 8B 08）→ 需 gzip 解压。
pub fn is_gzip(prefix: &[u8]) -> bool {
    prefix.len() >= 3 && prefix[..3] == GZIP_MAGIC
}

/// 魔数自适应数据面写入器：缓冲至 ≥4 字节后按魔数一次性选定 zstd / gzip 解压或直通，
/// 缓冲期字节不丢（首写可能只到 1 字节）。W 生产为下游 stdin（docker load / pg_restore；
/// 测试用 Vec）。Broken 仅为整状态置换时的瞬时占位（构造解码器失败即上抛 io 错，
/// 之后不会再被写）。
pub enum MagicSink<W: Write> {
    Buffering { buf: Vec<u8>, out: W },
    Plain(W),
    Zstd(zstd::stream::write::Decoder<'static, W>),
    Gzip(flate2::write::GzDecoder<W>),
    Broken,
}

impl<W: Write> MagicSink<W> {
    pub fn new(out: W) -> Self {
        MagicSink::Buffering { buf: Vec::new(), out }
    }
}

impl<W: Write> Write for MagicSink<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // 未决阶段：整状态取出，攒够 4 字节后定路（mem::replace 避免 &mut 下搬移泛型 W）
        if matches!(self, MagicSink::Buffering { .. }) {
            let st = std::mem::replace(self, MagicSink::Broken);
            let MagicSink::Buffering { mut buf, out } = st else { unreachable!() };
            buf.extend_from_slice(data);
            if buf.len() < 4 {
                *self = MagicSink::Buffering { buf, out };
                return Ok(data.len());
            }
            if is_zstd(&buf) {
                return match zstd::stream::write::Decoder::new(out) {
                    Ok(mut d) => {
                        d.write_all(&buf)?;
                        *self = MagicSink::Zstd(d);
                        Ok(data.len())
                    }
                    Err(e) => Err(std::io::Error::other(format!("zstd decoder init: {e}"))),
                };
            }
            if is_gzip(&buf) {
                let mut d = flate2::write::GzDecoder::new(out);
                d.write_all(&buf)?;
                *self = MagicSink::Gzip(d);
                return Ok(data.len());
            }
            let mut out = out;
            out.write_all(&buf)?;
            *self = MagicSink::Plain(out);
            Ok(data.len())
        } else {
            match self {
                MagicSink::Plain(w) => w.write(data),
                MagicSink::Zstd(d) => d.write(data),
                MagicSink::Gzip(d) => d.write(data),
                // 未决态必经上方分支 return；此处只剩 Broken（解码器构造失败后）
                _ => Err(std::io::Error::other("magic sink broken")),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            MagicSink::Buffering { buf, out } => {
                // 残流不足 4 字节：非任一完整格式，直通交下游如实报错
                let b = std::mem::take(buf);
                out.write_all(&b)?;
                out.flush()
            }
            MagicSink::Plain(w) => w.flush(),
            MagicSink::Zstd(d) => d.flush(),
            MagicSink::Gzip(d) => d.flush(),
            MagicSink::Broken => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    /// 裸 tar 样式样本：512B 头（含 ustar 魔数位）+ 伪数据（真实产物由 docker save 生成）。
    fn tar_sample() -> Vec<u8> {
        let mut v = vec![0u8; 512];
        v[..5].copy_from_slice(b"img1\0");
        v[257..262].copy_from_slice(b"ustar");
        v.extend((0..8192u32).map(|i| (i % 251) as u8));
        v
    }

    /// 共享缓冲写入器：让 MagicSink 泛型 W 可在测试里回收输出（Rc 单线程）。
    #[derive(Clone)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn sink_collect(sink: MagicSink<SharedBuf>, buf: &SharedBuf) -> Vec<u8> {
        drop(sink); // drop 关闭 zstd/gzip 解码器（排空内部缓冲）
        buf.0.borrow().clone()
    }

    // 魔数判断纯函数：zstd 压缩产物命中；gzip 三字节命中；裸 tar / 不足字节不命中
    #[test]
    fn magic_detection() {
        let tar = tar_sample();
        let packed = zstd::encode_all(tar.as_slice(), 19).unwrap();
        assert_eq!(&packed[..4], &ZSTD_MAGIC, "zstd 帧头 28 B5 2F FD");
        assert!(is_zstd(&packed));
        assert!(is_zstd(&packed[..8]));
        assert!(!is_zstd(&tar), "裸 tar 不命中 zstd");
        assert!(!is_zstd(&tar[..4]));
        assert!(!is_zstd(b"PG"), "不足 4 字节不判 zstd");
        assert!(!is_zstd(b""));

        let mut gz = GzEncoder::new(Vec::new(), Compression::best());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        assert_eq!(&gz[..3], &GZIP_MAGIC, "gzip 头 1F 8B 08");
        assert!(is_gzip(&gz));
        assert!(is_gzip(&gz[..4]));
        assert!(!is_gzip(&tar), "裸 tar 不命中 gzip");
        assert!(!is_gzip(b"\x1f\x8b"), "缺方法字节不判 gzip");
    }

    // 三魔数路由（真压缩样本）：zstd-19 → 解压还原；gzip → 解压还原；裸 tar → 直通。
    // 分片喂入跨魔数边界（1+3+余量）不丢字节。
    #[test]
    fn sink_routes_by_magic() {
        let tar = tar_sample();
        let packed = zstd::encode_all(tar.as_slice(), 19).unwrap();
        assert!(packed.len() < tar.len(), "可压缩样本应显著缩小说明 zstd-19 生效");
        let mut gze = GzEncoder::new(Vec::new(), Compression::best());
        gze.write_all(&tar).unwrap();
        let gz = gze.finish().unwrap();

        // 裸 tar：整段与分片喂入都直通
        for chunks in [vec![tar.clone()], tar.chunks(1000).map(|c| c.to_vec()).collect()] {
            let b = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
            let mut s = MagicSink::new(b.clone());
            for c in &chunks {
                s.write_all(c).unwrap();
            }
            s.flush().unwrap();
            assert_eq!(sink_collect(s, &b), tar, "裸 tar 直通");
        }

        // zstd-19：整段与跨魔数边界分片（1+3+余量）都正确解压
        let b = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
        let mut s = MagicSink::new(b.clone());
        s.write_all(&packed[..1]).unwrap();
        s.write_all(&packed[1..4]).unwrap();
        for chunk in packed[4..].chunks(700) {
            s.write_all(chunk).unwrap();
        }
        s.flush().unwrap();
        assert_eq!(sink_collect(s, &b), tar, "zstd 路径还原裸 tar 明文");

        // gzip：分片喂入也正确解压
        let b = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
        let mut s = MagicSink::new(b.clone());
        s.write_all(&gz[..2]).unwrap();
        s.write_all(&gz[2..5]).unwrap();
        for chunk in gz[5..].chunks(900) {
            s.write_all(chunk).unwrap();
        }
        s.flush().unwrap();
        assert_eq!(sink_collect(s, &b), tar, "gzip 路径还原裸 tar 明文");

        // 残流不足 4 字节：flush 直通（非任一完整格式，交下游报错）
        let b = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
        let mut s = MagicSink::new(b.clone());
        s.write_all(b"PG").unwrap();
        s.flush().unwrap();
        assert_eq!(sink_collect(s, &b), b"PG");
    }

    // 浏览器压缩形态（wasm 垫片逐 push 独立帧）：拼接帧流 = encode_all(a) || encode_all(b)
    // 的多帧解码——服务端流式解码器跨帧续解，这是「每次 push 一个独立完整 zstd 帧」
    // 设计的服务端兼容性证明。
    #[test]
    fn sink_decodes_concatenated_frames() {
        let a: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let b_: Vec<u8> = (0..123_457u32).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
        let mut stream = zstd::encode_all(a.as_slice(), 19).unwrap();
        stream.extend(zstd::encode_all(b_.as_slice(), 19).unwrap());
        stream.extend(zstd::encode_all(Vec::new().as_slice(), 19).unwrap()); // 空帧也合法

        let sb = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
        let mut s = MagicSink::new(sb.clone());
        for chunk in stream.chunks(64 * 1024) {
            s.write_all(chunk).unwrap();
        }
        s.flush().unwrap();
        let mut want = a.clone();
        want.extend(&b_);
        assert_eq!(sink_collect(s, &sb), want, "拼接独立帧按序整体还原");
    }

    // 生成侧一致性：dump 生成管道等价物（zstd-19 整体包裹）正是 zstd 路径的输入形态
    #[test]
    fn zstd19_wrap_roundtrip_with_sink() {
        let fc = tar_sample();
        let packed = zstd::encode_all(fc.as_slice(), 19).unwrap();
        let b = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
        let mut s = MagicSink::new(b.clone());
        s.write_all(&packed).unwrap();
        s.flush().unwrap();
        assert_eq!(sink_collect(s, &b), fc);
    }
}
