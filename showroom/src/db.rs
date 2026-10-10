//! db：`showroom db` 子命令（SPEC §9）。
//! sql/tables/preview 走签名 JSON；dump 用一次性 RSA-3072 客户端密钥对端到端加密
//! （私钥只在进程内存，服务端只见公钥 PEM），restore 用平台公钥 seal 成信封后
//! 复用 push import 的流式签名上传路径（先落临时信封再 signed_post_file）。

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use clap::Subcommand;
use serde_json::json;

use showroom_common::names::{check_project, check_slot};

use crate::api::{self, Api, Which};
use crate::config::Config;

#[derive(Subcommand)]
pub enum DbCmd {
    /// 执行 SQL（stdin 或 --file；含 DROP/TRUNCATE/DELETE 需 --confirm）
    Sql {
        project: String,
        slot: String,
        /// SQL 文件（缺省读 stdin）
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        confirm: bool,
    },
    /// 列出 public schema 的表
    Tables { project: String, slot: String },
    /// 预览表数据（psql -tA 风格，制表符分隔）
    Preview {
        project: String,
        slot: String,
        table: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// 下载加密备份（pg_dump -Fc；一次性密钥对端到端加密，-o 写文件或 --stdout）
    Dump {
        project: String,
        slot: String,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// 直接写二进制到标准输出（可重定向；与 -o 互斥）
        #[arg(long, conflicts_with = "out")]
        stdout: bool,
    },
    /// 恢复备份（pg_restore --clean --if-exists，先清目标库；需 --confirm）
    Restore {
        project: String,
        slot: String,
        /// pg_dump -Fc 备份文件
        file: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    /// 重置数据库（DROP+重建为空库，凭据不变；需 --confirm）
    Reset {
        project: String,
        slot: String,
        #[arg(long)]
        confirm: bool,
    },
}

pub fn run(cfg: &Config, force: Option<Which>, cmd: &DbCmd) -> Result<(), String> {
    match cmd {
        DbCmd::Sql { project, slot, file, confirm } => {
            sql(cfg, force, project, slot, file.as_deref(), *confirm)
        }
        DbCmd::Tables { project, slot } => tables(cfg, force, project, slot),
        DbCmd::Preview { project, slot, table, limit } => {
            preview(cfg, force, project, slot, table, *limit)
        }
        DbCmd::Dump { project, slot, out, stdout } => {
            dump(cfg, force, project, slot, out.as_deref(), *stdout)
        }
        DbCmd::Restore { project, slot, file, confirm } => {
            restore(cfg, force, project, slot, file, *confirm)
        }
        DbCmd::Reset { project, slot, confirm } => reset(cfg, force, project, slot, *confirm),
    }
}

fn sql(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: &str,
    file: Option<&Path>,
    confirm: bool,
) -> Result<(), String> {
    check_project(project)?;
    check_slot(slot)?;
    let sql = match file {
        Some(f) => fs::read_to_string(f).map_err(|e| format!("读取 {} 失败: {e}", f.display()))?,
        None => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                .map_err(|e| format!("读 stdin 失败: {e}"))?;
            s
        }
    };
    if sql.trim().is_empty() {
        return Err("SQL 为空（经 stdin 或 --file 提供）".into());
    }
    let api = Api::connect(cfg, force)?;
    let v = api.signed_post_json(
        "/_api/db-sql",
        &json!({ "project": project, "slot": slot, "sql": sql, "confirm": confirm }),
    )?;
    if let Some(out) = v["out"].as_str() {
        print!("{out}");
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

fn tables(cfg: &Config, force: Option<Which>, project: &str, slot: &str) -> Result<(), String> {
    check_project(project)?;
    check_slot(slot)?;
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get(&api::db_tables_query(project, slot))?;
    if let Some(arr) = v["tables"].as_array() {
        for t in arr {
            if let Some(name) = t.as_str() {
                println!("{name}");
            }
        }
    }
    Ok(())
}

fn preview(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: &str,
    table: &str,
    limit: u32,
) -> Result<(), String> {
    check_project(project)?;
    check_slot(slot)?;
    let api = Api::connect(cfg, force)?;
    let v = api.signed_post_json(
        "/_api/db-preview",
        &json!({ "project": project, "slot": slot, "table": table, "limit": limit }),
    )?;
    let cell = |c: &serde_json::Value| c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string());
    if let Some(cols) = v["columns"].as_array() {
        let names: Vec<String> = cols.iter().map(|c| cell(c)).collect();
        println!("{}", names.join("\t"));
    }
    if let Some(rows) = v["rows"].as_array() {
        for row in rows {
            if let Some(cells) = row.as_array() {
                let vals: Vec<String> = cells.iter().map(cell).collect();
                println!("{}", vals.join("\t"));
            }
        }
    }
    Ok(())
}

fn dump(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: &str,
    out: Option<&Path>,
    to_stdout: bool,
) -> Result<(), String> {
    use showroom_common::rsa::pkcs8::{EncodePublicKey, LineEnding};
    use showroom_common::rsa::RsaPrivateKey;

    check_project(project)?;
    check_slot(slot)?;
    // 一次性 RSA-3072 客户端密钥对（不落盘；服务端只见公钥，dump 端到端加密）
    let mut rng = rand::rngs::OsRng;
    let priv_key =
        RsaPrivateKey::new(&mut rng, 3072).map_err(|e| format!("生成一次性密钥对失败: {e}"))?;
    let pub_pem = priv_key
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("导出公钥 PEM 失败: {e}"))?;

    let api = Api::connect(cfg, force)?;
    let resp = api.signed_post_json_raw(
        "/_api/db-dump",
        &json!({ "project": project, "slot": slot, "client_pub_pem": pub_pem }),
    )?;

    match (out, to_stdout) {
        (Some(p), _) => {
            // 失败也要删半成品文件（ponytail: 闭包收口清理）
            let work = (|| -> Result<u64, String> {
                let mut f =
                    fs::File::create(p).map_err(|e| format!("创建 {} 失败: {e}", p.display()))?;
                let n = decode_open(&priv_key, resp, &mut f)?;
                f.flush().map_err(|e| format!("写 {} 失败: {e}", p.display()))?;
                Ok(n)
            })();
            let n = match work {
                Ok(n) => n,
                Err(e) => {
                    let _ = fs::remove_file(p);
                    return Err(e);
                }
            };
            eprintln!("已写入 {}（解密后 {n} 字节，pg_dump -Fc）", p.display());
            Ok(())
        }
        (None, true) => {
            let so = std::io::stdout();
            let mut lock = so.lock();
            let n = decode_open(&priv_key, resp, &mut lock).map_err(|e| format!("写 stdout 失败: {e}"))?;
            lock.flush().map_err(|e| format!("flush stdout 失败: {e}"))?;
            eprintln!("（stdout 输出 {n} 字节，pg_dump -Fc）");
            Ok(())
        }
        (None, false) => Err("二进制备份默认不打印：-o <文件> 写文件，或 --stdout 显式输出（可重定向）".into()),
    }
}

/// 响应体 payload_b64 → base64 流式解码 → 一次性私钥解密信封 → 写出（不整包进内存）。
fn decode_open<W: Write>(
    sk: &showroom_common::rsa::RsaPrivateKey,
    resp: reqwest::blocking::Response,
    w: &mut W,
) -> Result<u64, String> {
    let dec = base64::read::DecoderReader::new(resp, &STANDARD);
    showroom_common::open(sk, dec, w).map_err(|e| format!("解密失败: {e}"))
}

fn restore(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: &str,
    file: &Path,
    confirm: bool,
) -> Result<(), String> {
    if !confirm {
        return Err("恢复会以 --clean --if-exists 先清目标库，需显式确认：--confirm".into());
    }
    check_project(project)?;
    check_slot(slot)?;
    if !file.is_file() {
        return Err(format!("备份文件不存在: {}", file.display()));
    }
    let platform_pub = api::load_platform_pub()?;
    let api = Api::connect(cfg, force)?;
    // seal → 临时信封 → 复用 push import 的流式签名上传（24h 宽超时）
    let tmp = std::env::temp_dir().join(format!("showroom-restore-{}.bin", std::process::id()));
    let work = (|| -> Result<(), String> {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("写临时文件失败: {e}"))?;
        let mut w = BufWriter::new(&mut f);
        let mut src = fs::File::open(file).map_err(|e| format!("打开 {} 失败: {e}", file.display()))?;
        showroom_common::seal(&platform_pub, &mut src, &mut w)
            .map_err(|e| format!("加密信封失败: {e}"))?;
        w.flush().map_err(|e| format!("写临时文件失败: {e}"))?;
        drop(w);
        drop(f);
        api.signed_post_file(&api::db_restore_query(project, slot, true), &tmp)?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    work?;
    println!("恢复完成: {project}/{slot}");
    Ok(())
}

fn reset(
    cfg: &Config,
    force: Option<Which>,
    project: &str,
    slot: &str,
    confirm: bool,
) -> Result<(), String> {
    if !confirm {
        return Err("重置会清空并重建数据库（凭据不变），需显式确认：--confirm".into());
    }
    check_project(project)?;
    check_slot(slot)?;
    let api = Api::connect(cfg, force)?;
    api.signed_post_json(
        "/_api/db-reset",
        &json!({ "project": project, "slot": slot, "confirm": true }),
    )?;
    println!("已重置 {project}/{slot}（空库，凭据不变）");
    Ok(())
}
