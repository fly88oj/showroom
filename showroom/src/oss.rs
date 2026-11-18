//! oss：`showroom oss` 子命令（plan-22 §4 浏览端点的 CLI 面，与控制台数据页 OSS tab 对齐）。
//! ls=列目录（非递归一层，前缀可选）；cat=文本预览（64KB 上限，二进制/超限如实说明）；
//! get=下载对象（8MB 内，-o 写文件或 --stdout）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
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
    /// 上传文件为对象（8 MB 内；对象键=FILE 拆出的"前缀/键"，覆盖式）
    Put {
        project: String,
        /// 本地文件（对象键取自路径：uploads/a.png → uploads/a.png）
        file: PathBuf,
    },
    /// 删除单个对象（需 --yes；不存在时报错）
    Rm {
        project: String,
        key: String,
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(cfg: &Config, force: Option<Which>, cmd: &OssCmd) -> Result<(), String> {
    match cmd {
        OssCmd::Ls { project, prefix } => ls(cfg, force, project, prefix.as_deref()),
        OssCmd::Cat { project, key } => cat(cfg, force, project, key),
        OssCmd::Get { project, key, out, stdout } => get(cfg, force, project, key, out.as_deref(), *stdout),
        OssCmd::Put { project, file } => put(cfg, force, project, file),
        OssCmd::Rm { project, key, yes } => rm(cfg, force, project, key, *yes),
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
    let data = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("content_b64 非法: {e}"))?;
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
/// 上传实现：读文件→base64→POST s3-put（8MB 上限由服务端收口，超限 E_STATE 原样透出）
fn put(cfg: &Config, force: Option<Which>, project: &str, file: &Path) -> Result<(), String> {
    check_project(project)?;
    let data = fs::read(file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    // 对象键=去首段 ./ 与绝对路径，保留目录结构（与控制台上传语义一致：文件路径即键）
    let key = file
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir | std::path::Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    if key.is_empty() {
        return Err("无法从文件路径推导对象键".into());
    }
    let api = Api::connect(cfg, force)?;
    let v = api.signed_post_json(
        "/_api/s3-put",
        &serde_json::json!({ "project": project, "key": key, "content_b64": base64::engine::general_purpose::STANDARD.encode(&data) }),
    )?;
    println!(
        "已上传 {} → {}（{}）",
        file.display(),
        v["key"].as_str().unwrap_or(&key),
        fmt_size(v["size"].as_u64().unwrap_or(data.len() as u64))
    );
    Ok(())
}

/// 删除实现：--yes 守门（与 rm 展项同款）
fn rm(cfg: &Config, force: Option<Which>, project: &str, key: &str, yes: bool) -> Result<(), String> {
    check_project(project)?;
    if !yes {
        return Err(format!("删除对象 {key} 需 --yes（不可恢复；目录随最后一个对象消失）"));
    }
    let api = Api::connect(cfg, force)?;
    api.signed_post_json("/_api/s3-rm", &serde_json::json!({ "project": project, "key": key }))?;
    println!("已删除 {key}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn mtime_shorten() {
        assert_eq!(short_mtime("2026-11-17T16:20:00.123456789Z"), "2026-11-17 16:20");
        assert_eq!(short_mtime("short"), "short");
    }
}
