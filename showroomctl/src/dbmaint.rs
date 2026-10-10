//! dbmaint：SPEC §9 数据库维护内核（db-sql / db-tables / db-preview / db-dump / db-restore / db-reset）。
//! 纪律（ponytail）：psql/pg_dump/pg_restore 全走 `docker exec sr-postgres` 子进程，不引 postgres 客户端库；
//! 用户 SQL 恒走 stdin（绝不进 argv）；表名走白名单 + 双引号包裹（psql 无参数化）。
//! serve 在调用前完成统一验签与 own_check；本模块只做名字/上限校验与容器内执行。

use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use base64::engine::general_purpose::STANDARD;
use base64::engine::Engine as _;

use crate::dockerops;
use crate::state::{self, ApiError, R};

pub const PREVIEW_MAX_LIMIT: u32 = 200;
/// db-dump 解封前字节数上限（pg_dump -Fc 自定义格式）。
pub const DUMP_MAX_BYTES: usize = 256 << 20;

// —— 纯校验 / 解析（单测覆盖，不碰 docker）——

/// 危险语句检测（SPEC §9）：词边界 + 大小写不敏感匹配 DROP/TRUNCATE/DELETE。
/// 字符串字面量里的裸词会保守触发（宁可多要一次 confirm）；dropped_at 这类标识符不误报。
pub fn sql_needs_confirm(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    ["drop", "truncate", "delete"].iter().any(|w| has_word(&lower, w))
}

/// 大小写不敏感的 `\bword\b` 匹配（词字符 = [A-Za-z0-9_]，免引 regex 依赖）。
fn has_word(hay: &str, word: &str) -> bool {
    let (h, w) = (hay.as_bytes(), word.as_bytes());
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut from = 0usize;
    while from + w.len() <= h.len() {
        let Some(rel) = hay[from..].find(word) else { break };
        let at = from + rel; // word 为 ASCII，find 命中处必为字符边界
        let before_ok = at == 0 || !is_word(h[at - 1]);
        let end = at + w.len();
        let after_ok = end >= h.len() || !is_word(h[end]);
        if before_ok && after_ok {
            return true;
        }
        from = at + 1; // 仍在 ASCII 命中区内，切片安全
    }
    false
}

/// 表名白名单：`^[a-zA-Z_][a-zA-Z0-9_]*$` 且 ≤63（postgres 标识符长度上限）。
pub fn valid_table_name(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// preview limit 边界：1..=200。
pub fn valid_limit(n: u32) -> bool {
    (1..=PREVIEW_MAX_LIMIT).contains(&n)
}

/// psql `-t -A -F '\t'` 输出 → 行×列（去尾换行；空输出=零行；空单元格保留。
/// 取舍：单列空串值会退化为空行仍按一行解析；单元格内嵌 \t/\n 无法区分，预览够用）。
pub fn parse_tsv(data: &str) -> Vec<Vec<String>> {
    let data = data.strip_suffix('\n').unwrap_or(data);
    if data.is_empty() {
        return vec![];
    }
    data.split('\n').map(|l| l.split('\t').map(str::to_string).collect()).collect()
}

// —— 库名解析（SPEC §9 归一化 + legacy 存量沿用）——

/// 归一化新名与旧名（连字符原样）的存在性查找，交纯函数裁决。
fn resolve_db(p: &str, s: &str) -> R<String> {
    let new = state::db_ident(p, s);
    let legacy = state::legacy_db_ident(p, s);
    let new_exists = dockerops::db_exists(&new)?;
    let legacy_exists = legacy != new && dockerops::db_exists(&legacy)?;
    Ok(state::resolve_db_ident(p, s, new_exists, legacy_exists))
}

// —— 六端点内核 ——

/// POST /_api/db-sql：confirm 门 + psql 定库执行（stdin），返回 stdout 全文（psql 文本格式）。
pub fn sql(p: &str, s: &str, sql: &str, confirm: bool) -> R<String> {
    if !confirm && sql_needs_confirm(sql) {
        return Err(ApiError::new(
            "E_STATE",
            "语句含 DROP/TRUNCATE/DELETE：需 confirm=true 二次确认后重发",
        ));
    }
    let db = resolve_db(p, s)?;
    dockerops::psql_db(&db, &[], sql.as_bytes())
}

/// GET /_api/db-tables：public schema 的用户表（\dt 等价 SQL，非系统表）。
pub fn tables(p: &str, s: &str) -> R<Vec<String>> {
    let db = resolve_db(p, s)?;
    let q = "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
             ORDER BY table_name";
    let out = dockerops::psql_db(&db, &["-t", "-A"], q.as_bytes())?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// POST /_api/db-preview：白名单表名 + `SELECT * LIMIT n`，返回 (columns, rows)。
pub fn preview(p: &str, s: &str, table: &str, limit: u32) -> R<(Vec<String>, Vec<Vec<String>>)> {
    if !valid_table_name(table) {
        return Err(ApiError::new(
            "E_NAME",
            format!("非法表名 '{table}'：须匹配 ^[a-zA-Z_][a-zA-Z0-9_]*$"),
        ));
    }
    if !valid_limit(limit) {
        return Err(ApiError::new("E_NAME", format!("limit 须在 1..={PREVIEW_MAX_LIMIT}")));
    }
    let db = resolve_db(p, s)?;
    // 表名已过白名单（仅 [A-Za-z0-9_]，无引号/分号），插入定形 SQL 安全
    let cols_sql = format!(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = '{table}' ORDER BY ordinal_position"
    );
    let cols_out = dockerops::psql_db(&db, &["-t", "-A"], cols_sql.as_bytes())?;
    let columns: Vec<String> =
        cols_out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    if columns.is_empty() {
        return Err(ApiError::new("E_NOTFOUND", format!("public schema 无表 '{table}'")));
    }
    let data_sql = format!("SELECT * FROM \"{table}\" LIMIT {limit}");
    let data = dockerops::psql_db(&db, &["-t", "-A", "-F", "\t"], data_sql.as_bytes())?;
    Ok((columns, parse_tsv(&data)))
}

/// POST /_api/db-dump：pg_dump -Fc → 按 §2 信封用 client_pub 加密 → base64（>256MB 拒 E_QUOTA）。
pub fn dump(p: &str, s: &str, client_pub_pem: &str) -> R<String> {
    use showroom_common::rsa::pkcs8::DecodePublicKey;
    let pub_key = showroom_common::rsa::RsaPublicKey::from_public_key_pem(client_pub_pem.trim())
        .map_err(|e| ApiError::new("E_NAME", format!("client_pub_pem 解析失败: {e}")))?;
    let db = resolve_db(p, s)?;
    let dump = dockerops::pg_dump_fc(&db)?;
    if dump.len() > DUMP_MAX_BYTES {
        return Err(ApiError::new(
            "E_QUOTA",
            format!("dump {} 字节，超过上限 {} 字节", dump.len(), DUMP_MAX_BYTES),
        ));
    }
    let mut sealed = Vec::new();
    showroom_common::seal(&pub_key, std::io::Cursor::new(&dump), &mut sealed)
        .map_err(|e| ApiError::state(format!("seal dump: {e}")))?;
    Ok(STANDARD.encode(sealed))
}

/// POST /_api/db-restore：confirm 门 → 平台私钥流式解密喂 pg_restore stdin（--clean --if-exists）。
/// sealed（serve 收 body 落的 _tmp 文件）由调用方兜底删除；本函数不产生第二个落盘文件。
pub fn restore(root: &Path, p: &str, s: &str, confirm: bool, sealed: &Path) -> R<()> {
    if !confirm {
        return Err(ApiError::new("E_STATE", "restore 需 confirm=true（--clean --if-exists 会先清目标库）"));
    }
    restore_inner(root, p, s, sealed)
}

fn restore_inner(root: &Path, p: &str, s: &str, sealed: &Path) -> R<()> {
    use showroom_common::rsa::pkcs8::DecodePrivateKey;
    let pem = fs::read_to_string(root.join("state/platform_rsa"))
        .map_err(|e| ApiError::state(format!("platform_rsa missing (run showroomctl init-keys): {e}")))?;
    let priv_key = showroom_common::rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .map_err(|e| ApiError::state(format!("platform_rsa parse: {e}")))?;
    let cap = state::Policy::load(root).import_cap_bytes();
    let db = resolve_db(p, s)?;

    // pg_restore ← common::open 解密（单进程管道：dump 不落第二份盘、不整包进内存）
    let mut child = Command::new("docker")
        .args([
            "exec", "-i", "sr-postgres", "pg_restore", "-U", "postgres", "-d", &db, "--clean", "--if-exists",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn pg_restore: {e}")))?;
    let stdin = child.stdin.take().unwrap();
    let sp = sealed.to_path_buf();
    let writer = std::thread::spawn(move || -> R<u64> {
        let mut w = BufWriter::new(stdin);
        let mut sf = fs::File::open(&sp)?;
        let n = showroom_common::open(&priv_key, &mut sf, &mut w)
            .map_err(|e| ApiError::state(format!("decrypt envelope: {e}")))?;
        w.flush().map_err(|e| ApiError::state(format!("pg_restore stdin flush: {e}")))?;
        Ok(n)
    });
    let out = child
        .wait_with_output()
        .map_err(|e| ApiError::state(format!("pg_restore wait: {e}")))?;
    // 解密失败是根因，优先于 pg_restore 的连带失败上报
    let written = match writer.join() {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(ApiError::state("pg_restore stdin writer panicked")),
    };
    if !out.status.success() {
        let tail = dockerops::tail_lines(&String::from_utf8_lossy(&out.stderr), 8);
        return Err(ApiError::state(format!("pg_restore -d {db} failed: {tail}")));
    }
    // 与 import 同语义的事后帽（信封本体已在 body 收集阶段受 IMPORT_MAX_GB 限制）
    if written > cap {
        return Err(ApiError::new(
            "E_QUOTA",
            format!("decrypted payload exceeds IMPORT_MAX_GB={}GB", cap >> 30),
        ));
    }
    Ok(())
}

/// POST /_api/db-reset：DROP（新旧名都清，归一化切换在此终结）+ 以归一化新名重建；
/// 密码从 env 文件重读保持不变（容器不重启、凭据继续有效），env 文件按新名重发。
pub fn reset(root: &Path, p: &str, s: &str, confirm: bool) -> R<()> {
    if !confirm {
        return Err(ApiError::new("E_STATE", "reset 需 confirm=true（库与数据会被清空重建）"));
    }
    let env = state::parse_kv(
        &fs::read_to_string(state::env_path(root, p, s))
            .map_err(|e| ApiError::state(format!("read env: {e}")))?,
    );
    let db_pass = env
        .get("DB_PASSWORD")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::state("env 文件缺 DB_PASSWORD"))?
        .clone();
    let redisdb: u8 = env
        .get("REDIS_DB")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::state("env 文件缺 REDIS_DB"))?;

    let new_db = state::db_ident(p, s);
    let legacy_db = state::legacy_db_ident(p, s);
    let mut sql = String::new();
    for db in [&legacy_db, &new_db] {
        sql.push_str(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\n"));
    }
    sql.push_str(&format!(
        "DROP ROLE IF EXISTS \"{legacy_db}\";\n\
         DROP ROLE IF EXISTS \"{new_db}\";\n\
         CREATE ROLE \"{new_db}\" LOGIN PASSWORD '{db_pass}';\n\
         CREATE DATABASE \"{new_db}\" OWNER \"{new_db}\";\n"
    ));
    dockerops::psql(&sql)?;
    fs::write(state::env_path(root, p, s), state::env_file_contents(p, s, &new_db, &db_pass, redisdb))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 危险语句检测：大小写/空白变体触发；标识词缀（dropped_at/my_delete）不误报；
    // 字符串字面量里的裸词保守触发（取舍见函数注释）
    #[test]
    fn danger_sql_detection() {
        for yes in [
            "DROP TABLE t",
            "drop table t",
            "Drop Table t",
            "TRUNCATE t",
            "truncate table t",
            "delete from t",
            "  DELETE\nFROM t",
            "ALTER TABLE t DROP CONSTRAINT c",
            "-- c\nDROP SCHEMA x CASCADE",
            "SELECT 'x'; DELETE FROM t",
        ] {
            assert!(sql_needs_confirm(yes), "{yes:?} 应需 confirm");
        }
        for no in [
            "SELECT 1",
            "select * from t",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "CREATE TABLE t2 (id int)",
            "ALTER TABLE t ADD COLUMN dropped_at int",
            "SELECT dropped_at, deleted_flag FROM t",
            "GRANT SELECT ON t TO u",
            "explain delete_plan(t)",
        ] {
            assert!(!sql_needs_confirm(no), "{no:?} 不应触发");
        }
        assert!(sql_needs_confirm("SELECT 'drop'"), "字面量裸词保守触发（取舍已记录）");
    }

    // 表名白名单：首字符字母/_，余 [A-Za-z0-9_]，≤63；注入形态全拒
    #[test]
    fn table_name_whitelist() {
        for good in ["users", "_t", "T1", "a_b_2", &"t".repeat(63)] {
            assert!(valid_table_name(good), "{good} 应合法");
        }
        for bad in [
            "", "1t", "a-b", "a b", "t;--", "t; DROP TABLE x", "dro.p", "\"t\"", "中", "t\n",
            &"t".repeat(64),
        ] {
            assert!(!valid_table_name(bad), "{bad:?} 应拒绝");
        }
    }

    // preview limit 边界：1..=200
    #[test]
    fn limit_bounds() {
        assert!(!valid_limit(0));
        assert!(valid_limit(1));
        assert!(valid_limit(50));
        assert!(valid_limit(200));
        assert!(!valid_limit(201));
        assert!(!valid_limit(u32::MAX));
    }

    // psql -tA -F\t 输出解析：空/零行/多行/空单元格/空行（单列空串）
    #[test]
    fn tsv_parsing() {
        let rows = |v: &[&[&str]]| -> Vec<Vec<String>> {
            v.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
        };
        assert!(parse_tsv("").is_empty());
        assert!(parse_tsv("\n").is_empty(), "仅尾换行 = 零行");
        assert_eq!(parse_tsv("a\tb\nc\td\n"), rows(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(parse_tsv("a\t\tb\n"), rows(&[&["a", "", "b"]]));
        assert_eq!(parse_tsv("x\n\ny\n"), rows(&[&["x"], &[""], &["y"]]));
    }

    // dump 信道往返（不碰 docker）：server 侧 parse client PEM + seal（dump 的密码学路径）
    // ↔ client 侧 base64 解码 + 一次性私钥 open；坏 PEM 解析失败即 dump 的 E_NAME 分支
    #[test]
    fn dump_envelope_roundtrip() {
        use base64::read::DecoderReader;
        use showroom_common::rsa::pkcs8::{DecodePublicKey, EncodePublicKey, LineEnding};
        use showroom_common::rsa::{RsaPrivateKey, RsaPublicKey};

        let mut rng = rand::rngs::OsRng;
        let sk = RsaPrivateKey::new(&mut rng, 3072).unwrap();
        let pem = sk.to_public_key().to_public_key_pem(LineEnding::LF).unwrap().to_string();
        let pk = RsaPublicKey::from_public_key_pem(pem.trim()).unwrap();

        let dump_bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut sealed = Vec::new();
        showroom_common::seal(&pk, std::io::Cursor::new(&dump_bytes), &mut sealed).unwrap();
        assert!(sealed.len() > dump_bytes.len(), "信封含 384B 封装头与分段 nonce/tag");

        let b64 = STANDARD.encode(&sealed);
        let mut out = Vec::new();
        showroom_common::open(&sk, DecoderReader::new(b64.as_bytes(), &STANDARD), &mut out).unwrap();
        assert_eq!(out, dump_bytes);

        assert!(RsaPublicKey::from_public_key_pem("not a pem").is_err(), "坏 PEM → dump 的 E_NAME 分支");
    }
}
