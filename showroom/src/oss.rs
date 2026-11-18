//! oss：`showroom oss` 子命令（plan-22 §4 浏览端点的 CLI 面，与控制台数据页 OSS tab 对齐）。
//! ls=列目录（非递归一层，前缀可选）；cat=文本预览（64KB 上限，二进制/超限如实说明）；
//! get=下载对象（8MB 内，-o 写文件或 --stdout）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use showroom_common::names::check_project;

use crate::api::{self, Api, Which};
use crate::config::Config;

#[derive(Subcommand)]
pub enum OssCmd {
    /// 列出对象（非递归一层；可选前缀，如 data/；目录以 / 结尾）
    Ls {
        project: String,
        /// 对象前缀（缺省桶根）
        prefix: Option<String>,
    },
    /// 预览对象文本（64 KB 上限；二进制/超限给出说明）
    Cat {
        project: String,
        key: String,
    },
    /// 下载对象到文件（8 MB 内；-o 写文件或 --stdout）
    Get {
        project: String,
        key: String,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// 直接写二进制到标准输出（可重定向；与 -o 互斥）
        #[arg(long, conflicts_with = "out")]
        stdout: bool,
    },
}

pub fn run(cfg: &Config, force: Option<Which>, cmd: &OssCmd) -> Result<(), String> {
    match cmd {
        OssCmd::Ls { project, prefix } => ls(cfg, force, project, prefix.as_deref()),
        OssCmd::Cat { project, key } => cat(cfg, force, project, key),
        OssCmd::Get { project, key, out, stdout } => get(cfg, force, project, key, out.as_deref(), *stdout),
    }
}

/// 字节数可读化（与控制台 fmtSize 同档：B/KB/MB/GB）
fn fmt_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", n as f64 / 1024.0 / 1024.0 / 1024.0)
    }
}

fn ls(cfg: &Config, force: Option<Which>, project: &str, prefix: Option<&str>) -> Result<(), String> {
    check_project(project)?;
    let prefix = prefix.unwrap_or("");
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get(&api::s3_objects_query(project, prefix))?;
    if let Some(list) = v["objects"].as_array() {
        if list.is_empty() {
            println!("（无对象）");
        } else {
            // 对齐 ps 风格的定宽表头；mtime 取 ISO 串的日期+时分（秒截断降噪）
            println!("{:<40} {:>10}  MODIFIED", "KEY", "SIZE");
            for o in list {
                let name = o["name"].as_str().unwrap_or("?");
                let is_dir = o["is_dir"].as_bool().unwrap_or(false);
                let size = if is_dir { "-".to_string() } else { fmt_size(o["size"].as_u64().unwrap_or(0)) };
                let mtime = short_mtime(o["last_modified"].as_str().unwrap_or(""));
                println!("{:<40} {:>10}  {}", name, size, mtime);
            }
        }
    }
    if v["truncated"].as_bool() == Some(true) {
        eprintln!("（列表上限 500 已截断，可用前缀缩小范围）");
    }
    Ok(())
}

fn short_mtime(s: &str) -> String {
    // "2026-11-17T16:20:00.123456789Z" → "2026-11-17 16:20"
    if s.len() >= 16 {
        format!("{} {}", &s[0..10], &s[11..16])
    } else {
        s.to_string()
    }
}

fn cat(cfg: &Config, force: Option<Which>, project: &str, key: &str) -> Result<(), String> {
    check_project(project)?;
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get(&api::s3_get_query(project, key))?;
    match v["is_text"].as_bool() {
        Some(true) => {
            print!("{}", v["text"].as_str().unwrap_or(""));
            let _ = std::io::stdout().flush();
            Ok(())
        }
        _ => {
            let size = v["size"].as_u64().unwrap_or(0);
            Err(format!(
                "{}（{}）",
                v["note"].as_str().unwrap_or("二进制或超限对象，无法预览"),
                fmt_size(size)
            ))
        }
    }
}

fn get(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    key: &str,
    out: Option<&Path>,
    stdout: bool,
) -> Result<(), String> {
    check_project(project)?;
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get(&api::s3_download_query(project, key))?;
    let b64 = v["content_b64"]
        .as_str()
        .ok_or("响应缺少内容（对象超下载上限或不存在）")?;
    let data = b64_decode(b64)?;
    let name = key.rsplit('/').next().unwrap_or(key);
    if stdout {
        std::io::stdout().write_all(&data).map_err(|e| format!("写 stdout 失败: {e}"))?;
        let _ = std::io::stdout().flush();
    } else {
        let path = out.map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(name));
        fs::write(&path, &data).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;
        println!("已下载 {}（{}，{} 字节）", path.display(), fmt_size(data.len() as u64), data.len());
    }
    Ok(())
}

/// 标准 base64 解码（无外部依赖：字母表查表）
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in TBL.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=' && b != b'\n' && b != b'\r').collect();
    if bytes.len() % 4 == 1 {
        return Err("base64 长度非法".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            let v = rev[b as usize];
            if v == 255 {
                return Err(format!("base64 非法字符: {:?}", b as char));
            }
            n |= (v as u32) << (18 - i * 6);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip_known_vectors() {
        assert_eq!(b64_decode("").unwrap(), b"");
        assert_eq!(b64_decode("YQ==").unwrap(), b"a");
        assert_eq!(b64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(b64_decode("YWJj").unwrap(), b"abc");
        assert_eq!(b64_decode("YWJjZA==").unwrap(), b"abcd");
        // 二进制全字节域
        let all: Vec<u8> = (0u16..=255).map(|i| i as u8).collect();
        let enc = {
            // 就地编码（RFC 4648 标准字母表）
            const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            all.chunks(3).map(|c| {
                let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                let s: String = [n >> 18 & 63, n >> 12 & 63, n >> 6 & 63, n & 63]
                    .iter().map(|&i| TBL[i as usize] as char).collect();
                match c.len() { 1 => s[..2].to_string() + "==", 2 => s[..3].to_string() + "=", _ => s }
            }).collect::<String>()
        };
        assert_eq!(b64_decode(&enc).unwrap(), all);
        assert!(b64_decode("!!!").is_err());
    }

    #[test]
    fn mtime_shorten() {
        assert_eq!(short_mtime("2026-11-17T16:20:00.123456789Z"), "2026-11-17 16:20");
        assert_eq!(short_mtime("short"), "short");
    }
}
