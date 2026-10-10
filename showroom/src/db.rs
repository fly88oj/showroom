//! db：`showroom db` 子命令（SPEC §9）。
//! sql/tables/preview 走签名 JSON；dump 为三段式（SPEC §10.4）：一次性 RSA-3072 密钥对
//! 上行公钥 → begin/分片 chunk 下载临时信封 → 本地流式解密写目标（私钥只在进程内存，
//! 服务端只见公钥 PEM）；restore 用平台公钥 seal 成信封后复用 push import 的流式签名上传。

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

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

/// dump 就绪等待上限（1h：≤256MB 的 pg_dump 分钟级；防孤儿会话卡死客户端）。
const DUMP_WAIT_SECS: u64 = 3600;

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
    if out.is_none() && !to_stdout {
        return Err("二进制备份默认不打印：-o <文件> 写文件，或 --stdout 显式输出（可重定向）".into());
    }
    // 一次性 RSA-3072 客户端密钥对（不落盘；服务端只见公钥，dump 端到端加密）
    let mut rng = rand::rngs::OsRng;
    let priv_key =
        RsaPrivateKey::new(&mut rng, 3072).map_err(|e| format!("生成一次性密钥对失败: {e}"))?;
    let pub_pem = priv_key
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("导出公钥 PEM 失败: {e}"))?;

    let api = Api::connect(cfg, force)?;
    // 三段式（SPEC §10.4）：begin → 轮询 ready → 4MB chunk 循环落临时信封 → 一次性解密写目标。
    // 本地内存恒定：每片 ≤4MB，信封与明文都不整包进内存。
    let v = api.signed_post_json(
        "/_api/db-dump-begin",
        &json!({ "project": project, "slot": slot, "client_pub_pem": pub_pem }),
    )?;
    let dump_id = v["dump_id"].as_str().ok_or("db-dump-begin 响应缺 dump_id")?.to_string();
    let total = wait_dump_ready(&api, &dump_id)?;

    let tmp = std::env::temp_dir().join(format!("showroom-dump-{}.bin", std::process::id()));
    let work = (|| -> Result<u64, String> {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("写临时文件失败: {e}"))?;
        let mut w = BufWriter::new(&mut f);
        let mut offset = 0u64;
        while offset < total {
            let want = api::DUMP_CHUNK_LEN.min(total - offset);
            let resp = api.signed_get_raw(&api::dump_chunk_query(&dump_id, offset, want))?;
            let bytes = resp.bytes().map_err(|e| format!("下载切片失败: {e}"))?;
            if bytes.len() as u64 != want {
                return Err(format!("切片长度不符：请求 {want} 字节，收到 {}", bytes.len()));
            }
            w.write_all(&bytes).map_err(|e| format!("写临时文件失败: {e}"))?;
            offset += want;
        }
        w.flush().map_err(|e| format!("写临时文件失败: {e}"))?;
        drop(w);
        // 信封完整后一次性私钥解密，流式写出（pg_dump -Fc）
        let mut sealed =
            fs::File::open(&tmp).map_err(|e| format!("读临时信封失败: {e}"))?;
        let n = match out {
            Some(p) => {
                let mut sink = fs::File::create(p)
                    .map_err(|e| format!("创建 {} 失败: {e}", p.display()))?;
                let n = showroom_common::open(&priv_key, &mut sealed, &mut sink)
                    .map_err(|e| format!("解密失败: {e}"))?;
                sink.flush().map_err(|e| format!("写 {} 失败: {e}", p.display()))?;
                n
            }
            None => {
                let so = std::io::stdout();
                let mut sink = so.lock();
                let n = showroom_common::open(&priv_key, &mut sealed, &mut sink)
                    .map_err(|e| format!("写 stdout 失败: {e}"))?;
                sink.flush().map_err(|e| format!("flush stdout 失败: {e}"))?;
                n
            }
        };
        Ok(n)
    })();
    let _ = fs::remove_file(&tmp);
    let n = match work {
        Ok(n) => n,
        Err(e) => {
            // 失败也要删半成品目标文件（临时信封已在上面删）
            if let Some(p) = out {
                let _ = fs::remove_file(p);
            }
            return Err(e);
        }
    };
    match out {
        Some(p) => eprintln!("已写入 {}（解密后 {n} 字节，pg_dump -Fc）", p.display()),
        None => eprintln!("（stdout 输出 {n} 字节，pg_dump -Fc）"),
    }
    Ok(())
}

/// 轮询 db-dump-status 至 ready（1s 间隔，DUMP_WAIT_SECS 上限）；failed 即时上报。
fn wait_dump_ready(api: &Api, dump_id: &str) -> Result<u64, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(DUMP_WAIT_SECS);
    loop {
        let st = api.signed_get(&api::dump_status_query(dump_id))?;
        if st["ready"].as_bool() == Some(true) {
            return st["total"].as_u64().ok_or_else(|| "db-dump-status 响应缺 total".to_string());
        }
        if let Some(e) = st["error"].as_str().filter(|s| !s.is_empty()) {
            return Err(format!("服务端 dump 失败: {e}"));
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("等待 dump 就绪超时（{}s），会话由服务端 24h 兜底清理", DUMP_WAIT_SECS));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
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
