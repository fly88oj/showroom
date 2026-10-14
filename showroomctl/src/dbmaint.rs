//! dbmaint：SPEC §9 数据库维护内核（db-sql / db-tables / db-preview / db-dump / db-restore / db-reset）。
//! 纪律（ponytail）：psql/pg_dump/pg_restore 全走 `docker exec sr-postgres` 子进程，不引 postgres 客户端库；
//! 用户 SQL 恒走 stdin（绝不进 argv）；表名走白名单 + 双引号包裹（psql 无参数化）。
//! serve 在调用前完成统一验签与 user_check；本模块只做名字/上限校验与容器内执行。
//! db-dump 为三段式（SPEC §10.4）：begin 起 pg_dump|seal 流式后台线程，chunk/status 切片下发。

use std::fs;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dockerops;
use crate::state::{self, ApiError, R};

pub const PREVIEW_MAX_LIMIT: u32 = 200;
/// db-dump 解封前字节数上限（pg_dump -Fc 自定义格式）。
pub const DUMP_MAX_BYTES: usize = 256 << 20;

// —— 纯校验 / 解析（单测覆盖，不碰 docker）——

/// 危险语句检测（SPEC §9；谓词单源——控制台/CLI 的确认弹层都由本判定经 E_STATE 触发）：
/// 词边界 + 大小写不敏感匹配 DROP/TRUNCATE/DELETE/ALTER（ALTER 为用户裁决追加）。
/// 字符串字面量里的裸词会保守触发（宁可多要一次 confirm）；dropped_at 这类标识符不误报。
pub fn sql_needs_confirm(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    ["drop", "truncate", "delete", "alter"].iter().any(|w| has_word(&lower, w))
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

// —— 库名解析（§11：归一化新名 + legacyslot 指路的旧名检测沿用）——

/// 库名解析（publish 与六端点共用）：候选按优先级 = [新名 `sr_<p_n>`] + legacyslot 的
/// 两个旧名（verbatim `sr_{p}_{s}` / 归一化 `sr_{p_n}_{s_n}`，§9 双名思路）。
/// 返回 (库名, 是否存量)：存量 → 调用方 ALTER/直接操作；首建 → CREATE。
pub fn resolve_db(project: &str, legacyslot: Option<&str>) -> R<(String, bool)> {
    let new = state::db_ident(project);
    let mut names = vec![new.clone()];
    for d in legacyslot.map(|s| state::legacy_db_idents(project, s)).unwrap_or_default() {
        if !names.contains(&d) {
            names.push(d);
        }
    }
    let mut exists = Vec::with_capacity(names.len());
    for n in &names {
        exists.push(dockerops::db_exists(n)?);
    }
    Ok(state::resolve_db_ident(&names, &exists))
}

/// 项目的 legacyslot（§11 迁移标记；conf 缺失按 None，存在性校验由 serve user_check 先行）。
fn legacyslot_of(root: &Path, p: &str) -> Option<String> {
    state::Conf::load(root, p).ok().and_then(|c| c.legacyslot)
}

// —— 六端点内核 ——

/// db-sql 响应：`out` 为 psql 输出全文（CLI/旧客户端兼容通道）；单条 SELECT 时额外附
/// `csv`（psql --csv 机器可读表：首行表头，引号/双引号/换行标准 CSV 转义）——控制台据此
/// 渲染结果表格（多语句由前端按分号拆分逐条请求 → 多结果 Tab）。
pub struct SqlResult {
    pub out: String,
    pub csv: Option<String>,
}

/// 单条 SELECT 判定：trim 后以 SELECT 开头（前端已按分号拆分；带 CTE 的 WITH…SELECT
/// 走文本通道——csv 通道只收最平凡形状，宁缺勿错）。
fn is_plain_select(sql: &str) -> bool {
    sql.trim_start().get(..6).map(|s| s.eq_ignore_ascii_case("select")).unwrap_or(false)
}

#[cfg(test)]
mod sql_csv_tests {
    use super::is_plain_select;

    #[test]
    fn plain_select_shapes() {
        assert!(is_plain_select("SELECT 1"));
        assert!(is_plain_select("select * from t"));
        assert!(is_plain_select("  \n SELECT x")); // 前导空白
        assert!(is_plain_select("SeLeCt 1")); // 大小写不敏感
    }

    #[test]
    fn needs_confirm_word_list() {
        use super::sql_needs_confirm;
        assert!(sql_needs_confirm("DROP TABLE t"));
        assert!(sql_needs_confirm("delete from t"));
        assert!(sql_needs_confirm("TRUNCATE t"));
        assert!(sql_needs_confirm("alter table t add c int")); // 用户裁决追加
        assert!(!sql_needs_confirm("SELECT * FROM t"));
        assert!(sql_needs_confirm("SELECT 'delete' FROM t")); // 字面量保守触发（文档化行为）
    }

    #[test]
    fn non_select_goes_text_channel() {
        assert!(!is_plain_select("WITH c AS (SELECT 1) SELECT * FROM c")); // CTE 走文本通道
        assert!(!is_plain_select("UPDATE t SET x=1"));
        assert!(!is_plain_select("-- comment\nSELECT 1"));
        assert!(!is_plain_select(""));
        assert!(!is_plain_select("EXPLAIN SELECT 1"));
    }
}

/// POST /_api/db-sql：confirm 门 + psql 定库执行（stdin）。
/// SELECT（单条）用 --csv 执行：out 与 csv 同文（CLI 亦直读）；其余语句走默认文本格式。
pub fn sql(root: &Path, p: &str, sql: &str, confirm: bool) -> R<SqlResult> {
    if !confirm && sql_needs_confirm(sql) {
        return Err(ApiError::new(
            "E_CONFIRM", // 专用码：控制台据此弹确认层（勿用文案匹配——文案会漂移）
            "语句含 DROP/TRUNCATE/DELETE/ALTER：需确认后重发（CLI 加 --confirm）",
        ));
    }
    let db = resolve_db(p, legacyslot_of(root, p).as_deref())?.0;
    if is_plain_select(sql) {
        let csv = dockerops::psql_db(&db, &["--csv"], sql.as_bytes())?;
        Ok(SqlResult { out: csv.clone(), csv: Some(csv) })
    } else {
        Ok(SqlResult { out: dockerops::psql_db(&db, &[], sql.as_bytes())?, csv: None })
    }
}

/// GET /_api/db-tables：public schema 的用户表（\dt 等价 SQL，非系统表）。
pub fn tables(root: &Path, p: &str) -> R<Vec<String>> {
    let db = resolve_db(p, legacyslot_of(root, p).as_deref())?.0;
    let q = "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
             ORDER BY table_name";
    let out = dockerops::psql_db(&db, &["-t", "-A"], q.as_bytes())?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// POST /_api/db-preview：白名单表名 + `SELECT * LIMIT n`，返回 (columns, rows)。
pub fn preview(root: &Path, p: &str, table: &str, limit: u32) -> R<(Vec<String>, Vec<Vec<String>>)> {
    if !valid_table_name(table) {
        return Err(ApiError::new(
            "E_NAME",
            format!("非法表名 '{table}'：须匹配 ^[a-zA-Z_][a-zA-Z0-9_]*$"),
        ));
    }
    if !valid_limit(limit) {
        return Err(ApiError::new("E_NAME", format!("limit 须在 1..={PREVIEW_MAX_LIMIT}")));
    }
    let db = resolve_db(p, legacyslot_of(root, p).as_deref())?.0;
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

// —— db-dump 三段式（SPEC §10.4 C；替代 §9 单发 base64——pg_dump 全量进 Vec 再 base64 的 4× 内存）——

/// 单次切片上限（SPEC §10.4：len ≤ 4MB）。
pub const DUMP_CHUNK_MAX: u64 = 4 << 20;

/// dump 外层 zstd 压缩级别（压缩率优先：zstd-19 对 -Fc 内容显著优于内置 zlib）。
pub const DUMP_ZSTD_LEVEL: i32 = 19;

/// 会话三件套：_tmp/<id>.bin（sealed 信封）+ <id>.meta（归属）+ <id>.status（就绪状态）。
/// 陈旧（>24h）由 gc-images 顺带清理（ops::gc_tmp 已扫 .bin/.meta/.status）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpMeta {
    pub user: String,
    pub project: String,
    pub db: String,
    pub created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpStatus {
    /// running | ready | failed
    pub state: String,
    pub total: u64,
    pub error: Option<String>,
}

pub fn dump_bin(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.bin"))
}
fn dump_meta_path(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.meta"))
}
fn dump_status_path(root: &Path, id: &str) -> PathBuf {
    root.join("_tmp").join(format!("{id}.status"))
}

/// 会话归属：id 非法/不存在 → E_NOTFOUND；user 不符 → E_AUTH（不泄露会话内情）。
fn load_dump_meta_owned(root: &Path, id: &str, user: &str) -> R<DumpMeta> {
    if !state::valid_session_id(id) {
        return Err(ApiError::new("E_NOTFOUND", "no such dump"));
    }
    let text = fs::read_to_string(dump_meta_path(root, id))
        .map_err(|_| ApiError::new("E_NOTFOUND", "no such dump"))?;
    let m: DumpMeta =
        serde_json::from_str(&text).map_err(|e| ApiError::state(format!("dump meta parse: {e}")))?;
    if m.user != user {
        return Err(ApiError::new("E_AUTH", format!("dump '{id}' belongs to {}", m.user)));
    }
    Ok(m)
}

/// 读状态文件；缺失或畸形按 running 处理（begin 已同步写 running；畸形=线程写一半崩溃）。
fn load_dump_status(root: &Path, id: &str) -> DumpStatus {
    let text = fs::read_to_string(dump_status_path(root, id)).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(DumpStatus { state: "running".into(), total: 0, error: None })
}

fn write_dump_status(root: &Path, id: &str, st: &DumpStatus) -> R<()> {
    let j = serde_json::to_string(st).map_err(|e| ApiError::state(format!("dump status serialize: {e}")))?;
    fs::write(dump_status_path(root, id), j)?;
    Ok(())
}

/// 读侧限额：累计超 cap 后返回 io 错误并置 over（dump 的 E_QUOTA 判定走标记，
/// 免从 io::Error 字符串反解错误码）。
struct CapReader<R> {
    inner: R,
    cap: u64,
    total: u64,
    over: Arc<AtomicBool>,
}

impl<R: Read> CapReader<R> {
    fn new(inner: R, cap: u64, over: Arc<AtomicBool>) -> Self {
        CapReader { inner, cap, total: 0, over }
    }
}

impl<R: Read> Read for CapReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        if self.total > self.cap {
            self.over.store(true, Ordering::Relaxed);
            return Err(std::io::Error::other(format!("payload exceeds {} bytes", self.cap)));
        }
        Ok(n)
    }
}

/// dump 生成管道（压缩率优先，§B）：`pg_dump -Z0 -Fc`（custom 格式零压缩层）→
/// zstd-19 外层压缩（对 -Fc 内容压缩率显著高于内置 zlib-9）→ seal(客户端公钥) → out 文件。
/// 信封整体一次生成（解密侧 common::open 必须拿到完整信封），但生成全程流式（内存恒定 ~4MiB）。
/// 返回 sealed 总字节数；任何失败由调用方删 out。**明文**（-Fc 原始字节，解压前帽按 zstd
/// 前语义不变）超 cap → E_QUOTA——CapReader 挂在 zstd 解压后的明文流上。
fn dump_seal_to(db: &str, pub_key: &showroom_common::rsa::RsaPublicKey, out: &Path, cap: u64) -> R<u64> {
    let mut child = Command::new("docker")
        .args(["exec", "-i", "sr-postgres", "pg_dump", "-U", "postgres", "-Z0", "-Fc", db])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn pg_dump: {e}")))?;
    let stdout = child.stdout.take().unwrap();
    // stderr 独立线程排空（防 pg_dump 报错写满管道互锁），失败时取尾部几行
    let mut stderr = child.stderr.take().unwrap();
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    // pg_dump(-Z0 原生 -Fc) → zstd-19 → seal；zstd Encoder 读取失败（pg_dump 崩）表现为 EOF，
    // 由下方 child.wait 的退出状态兜底判错
    let zr = zstd::stream::read::Encoder::new(stdout, DUMP_ZSTD_LEVEL)
        .map_err(|e| {
            dockerops::kill_wait(&mut child);
            ApiError::state(format!("zstd encoder init: {e}"))
        })?;

    let over = Arc::new(AtomicBool::new(false));
    let mut w = BufWriter::new(fs::File::create(out)?);
    let sealed = showroom_common::seal(
        pub_key,
        CapReader::new(zr, cap, over.clone()),
        &mut w,
    );
    let n = match sealed {
        Ok(n) => n,
        Err(e) => {
            dockerops::kill_wait(&mut child);
            return Err(if over.load(Ordering::Relaxed) {
                ApiError::new("E_QUOTA", format!("dump 明文超过上限 {cap} 字节"))
            } else {
                ApiError::state(format!("seal dump: {e}"))
            });
        }
    };
    w.flush().map_err(|e| ApiError::state(format!("seal dump flush: {e}")))?;
    let status = child.wait().map_err(|e| ApiError::state(format!("pg_dump wait: {e}")))?;
    let errout = err_t.join().unwrap_or_default();
    if !status.success() {
        return Err(ApiError::state(format!(
            "pg_dump -Z0 -Fc {db} failed: {}",
            dockerops::tail_lines(&errout, 8)
        )));
    }
    Ok(n)
}

/// POST /_api/db-dump-begin 内核：解析客户端公钥（E_NAME 先拒）→ 定库 → 落 meta/status
/// → 起后台线程 pg_dump|zstd-19|seal 流式写 .bin，立即返回 dump_id（不等待）。
/// 名字校验与 user_check 由 serve 完成。线程无论成败终态都写回 .status；失败删 .bin。
pub fn dump_begin(root: &Path, user: &str, project: &str, client_pub_pem: &str) -> R<String> {
    use showroom_common::rsa::pkcs8::DecodePublicKey;

    let pub_key = showroom_common::rsa::RsaPublicKey::from_public_key_pem(client_pub_pem.trim())
        .map_err(|e| ApiError::new("E_NAME", format!("client_pub_pem 解析失败: {e}")))?;
    let db = resolve_db(project, legacyslot_of(root, project).as_deref())?.0;
    let id = state::rand_hex(16);
    fs::create_dir_all(root.join("_tmp"))?;
    // meta/status 先于 .bin 落盘：chunk/status 以 meta 判归属，bin 未就绪只表现为 running
    let meta = DumpMeta {
        user: user.to_string(),
        project: project.to_string(),
        db: db.clone(),
        created: state::unix_now(),
    };
    let j = serde_json::to_string(&meta).map_err(|e| ApiError::state(format!("dump meta serialize: {e}")))?;
    fs::write(dump_meta_path(root, &id), j)?;
    write_dump_status(root, &id, &DumpStatus { state: "running".into(), total: 0, error: None })?;

    let (r2, i2) = (root.to_path_buf(), id.clone());
    std::thread::spawn(move || {
        let bin = dump_bin(&r2, &i2);
        let st = match dump_seal_to(&db, &pub_key, &bin, DUMP_MAX_BYTES as u64) {
            Ok(total) => DumpStatus { state: "ready".into(), total, error: None },
            Err(e) => {
                let _ = fs::remove_file(&bin);
                DumpStatus { state: "failed".into(), total: 0, error: Some(format!("{e}")) }
            }
        };
        if write_dump_status(&r2, &i2, &st).is_err() {
            eprintln!("showroomctl: dump {}: status write failed", i2);
        }
    });
    Ok(id)
}

/// 切片计划（纯函数）：→ (有效长度, 是否读到最后一片)。
/// len 0 或 >4MB → E_NAME；offset 超 total → E_NAME；offset==total → 0 字节且
/// finished=true（幂等收尾探测，同样触发会话删除）。
pub fn plan_chunk(total: u64, offset: u64, len: u64) -> R<(u64, bool)> {
    if len == 0 || len > DUMP_CHUNK_MAX {
        return Err(ApiError::new("E_NAME", format!("len 须在 1..={DUMP_CHUNK_MAX} 字节")));
    }
    if offset > total {
        return Err(ApiError::new("E_NAME", format!("offset {offset} 超出总长 {total}")));
    }
    let n = len.min(total - offset);
    Ok((n, offset + n >= total))
}

/// GET /_api/db-dump-chunk 内核：归属校验 → ready 门 → 读 [offset, offset+n) 字节。
/// 返回 (字节, total, finished)；finished（读完最后一片）由调用方触发会话删除。
pub fn dump_chunk(root: &Path, user: &str, id: &str, offset: u64, len: u64) -> R<(Vec<u8>, u64, bool)> {
    load_dump_meta_owned(root, id, user)?;
    let st = load_dump_status(root, id);
    match st.state.as_str() {
        "ready" => {}
        "failed" => {
            return Err(ApiError::new("E_STATE", format!("dump failed: {}", st.error.unwrap_or_default())))
        }
        _ => return Err(ApiError::new("E_STATE", "dump not ready")),
    }
    let (n, finished) = plan_chunk(st.total, offset, len)?;
    let mut buf = vec![0u8; n as usize];
    if n > 0 {
        let mut f = fs::File::open(dump_bin(root, id))?;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(&mut buf)?;
    }
    Ok((buf, st.total, finished))
}

/// GET /_api/db-dump-status 内核：归属校验后读状态。
pub fn dump_status(root: &Path, user: &str, id: &str) -> R<DumpStatus> {
    load_dump_meta_owned(root, id, user)?;
    Ok(load_dump_status(root, id))
}

/// 会话三件套删除（读完最后一片 / 陈旧 gc 共用；幂等）。
pub fn dump_cleanup(root: &Path, id: &str) {
    for p in [dump_bin(root, id), dump_meta_path(root, id), dump_status_path(root, id)] {
        let _ = fs::remove_file(p);
    }
}

// —— restore 输入解包：解密后的字节流经 unpack::MagicSink 魔数自适应（zstd / gzip / 直通，
// 与 import_inner 共用同一探测封装）——原生 pg_dump -Fc（"PGDMP" 头）直喂 pg_restore
// （原生兼容 -Z0/-Z9 内压全系）；zstd-19 包裹（服务端 dump 生成侧或浏览器压缩）先解压。

/// POST /_api/db-restore：confirm 门 → 平台私钥流式解密 →（zstd 魔数探测）→ pg_restore
/// stdin（--clean --if-exists）。sealed（serve 收 body 落的 _tmp 文件）由调用方兜底删除；
/// 本函数不产生第二个落盘文件。
pub fn restore(root: &Path, p: &str, confirm: bool, sealed: &Path) -> R<()> {
    if !confirm {
        return Err(ApiError::new("E_STATE", "restore 需 confirm=true（--clean --if-exists 会先清目标库）"));
    }
    restore_inner(root, p, sealed)
}

fn restore_inner(root: &Path, p: &str, sealed: &Path) -> R<()> {
    use showroom_common::rsa::pkcs8::DecodePrivateKey;
    let pem = fs::read_to_string(root.join("state/platform_rsa"))
        .map_err(|e| ApiError::state(format!("platform_rsa missing (run showroomctl init-keys): {e}")))?;
    let priv_key = showroom_common::rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .map_err(|e| ApiError::state(format!("platform_rsa parse: {e}")))?;
    let cap = state::Policy::load(root).import_cap_bytes();
    let db = resolve_db(p, legacyslot_of(root, p).as_deref())?.0;

    // pg_restore ← MagicSink（魔数自适应：zstd / gzip / 直通）← common::open 解密
    // （单进程管道：dump 不落第二份盘、不整包进内存）
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
        let mut sink = crate::unpack::MagicSink::new(BufWriter::new(stdin));
        let mut sf = fs::File::open(&sp)?;
        let n = showroom_common::open(&priv_key, &mut sf, &mut sink)
            .map_err(|e| ApiError::state(format!("decrypt envelope: {e}")))?;
        sink.flush().map_err(|e| ApiError::state(format!("pg_restore stdin flush: {e}")))?;
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

/// POST /_api/db-reset：DROP（新名 + legacyslot 旧候选都清，归一化切换在此终结）+ 以归一化
/// 新名重建；密码从 env 文件重读保持不变（容器不重启、凭据继续有效），env 文件按新名重发，
/// conf 的 legacyslot 标记同步清空。
pub fn reset(root: &Path, p: &str, confirm: bool) -> R<()> {
    if !confirm {
        return Err(ApiError::new("E_STATE", "reset 需 confirm=true（库与数据会被清空重建）"));
    }
    let env = state::parse_kv(
        &fs::read_to_string(state::env_path(root, p))
            .map_err(|e| ApiError::state(format!("read env: {e}")))?,
    );
    let db_pass = env
        .get("DB_PASSWORD")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::state("env 文件缺 DB_PASSWORD"))?
        .clone();
    // §15.2：redis 禁用时 env 无 REDIS_* —— 库号回落 conf.redisdb（重发 env 用同一集合判定）
    let redisdb: u8 = env
        .get("REDIS_DB")
        .and_then(|v| v.parse().ok())
        .or_else(|| state::Conf::load(root, p).ok().map(|c| c.redisdb))
        .ok_or_else(|| ApiError::state("env 文件缺 REDIS_DB"))?;

    let mut conf = state::Conf::load(root, p)
        .map_err(|e| ApiError::state(format!("read conf: {e}")))?;
    let new_db = state::db_ident(p);
    let mut dbs = vec![new_db.clone()];
    if let Some(s) = &conf.legacyslot {
        for d in state::legacy_db_idents(p, s) {
            if !dbs.contains(&d) {
                dbs.push(d);
            }
        }
    }
    let mut sql = String::new();
    for db in &dbs {
        sql.push_str(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\n"));
    }
    for db in &dbs {
        sql.push_str(&format!("DROP ROLE IF EXISTS \"{db}\";\n"));
    }
    sql.push_str(&format!(
        "CREATE ROLE \"{new_db}\" LOGIN PASSWORD '{db_pass}';\n\
         CREATE DATABASE \"{new_db}\" OWNER \"{new_db}\";\n"
    ));
    dockerops::psql(&sql)?;
    conf.legacyslot = None; // 归一化在此终结
    conf.save(root, p)?;
    let mw = crate::middleware::HostEnv::load(root).middleware;
    // 发现键随 conf 重建（plan-19：db-reset 场景 conf 已在——按其服务集重建 env）
    let conf = state::Conf::load(root, p)?;
    let main_c = crate::publish::container_name(p);
    let svc_c: Vec<String> = conf
        .services
        .iter()
        .map(|v| crate::publish::svc_container_name(p, &v.name))
        .collect();
    let mut disc: Vec<(&str, &str, u16)> = vec![("main", main_c.as_str(), conf.cport)];
    for (v, c) in conf.services.iter().zip(svc_c.iter()) {
        disc.push((v.name.as_str(), c.as_str(), v.cport));
    }
    fs::write(state::env_path(root, p), state::env_file_contents(p, &new_db, &db_pass, redisdb, &mw, &disc))?;
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
            "ALTER TABLE t ADD COLUMN c int", // 用户裁决：裸 ALTER 亦需确认
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
            "SELECT dropped_at, deleted_flag FROM t",
            "GRANT SELECT ON t TO u",
            "explain delete_plan(t)",
            "SELECT alter_plan(t)", // 标识词缀不误报（alter_plan 是函数名）
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

    // dump 信封密码学路径往返（不碰 docker）：server 侧 parse client PEM + seal
    // ↔ client 侧 base64 解码（兼容旧 CLI 流）+ 一次性私钥 open；坏 PEM → dump_begin 的 E_NAME 分支
    #[test]
    fn dump_envelope_roundtrip() {
        use base64::engine::general_purpose::STANDARD;
        use base64::read::DecoderReader;
        use base64::Engine as _;
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

        assert!(RsaPublicKey::from_public_key_pem("not a pem").is_err(), "坏 PEM → E_NAME 分支");
    }

    // —— db-dump 三段式（SPEC §10.4）：会话归属 / chunk 边界 / 状态机（临时目录，不碰 docker）——

    fn troot(name: &str) -> PathBuf {
        // 隔离纪律（TESTING.md 规则 1）：每次调用独立目录（pid+原子计数），固定名复用会互相 remove_dir_all
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "srctl-dump-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("_tmp")).unwrap();
        d
    }

    /// 手工落一份 ready 会话（绕开 dump_begin 的 docker 依赖），返回 (root, id, sealed 字节)。
    fn ready_session(name: &str, user: &str, sealed: &[u8]) -> (PathBuf, String) {
        let root = troot(name);
        let id = state::rand_hex(16);
        fs::write(dump_bin(&root, &id), sealed).unwrap();
        let meta = DumpMeta {
            user: user.into(),
            project: "demo".into(),
            db: "sr_demo".into(),
            created: 1,
        };
        fs::write(dump_meta_path(&root, &id), serde_json::to_string(&meta).unwrap()).unwrap();
        let st = DumpStatus { state: "ready".into(), total: sealed.len() as u64, error: None };
        fs::write(dump_status_path(&root, &id), serde_json::to_string(&st).unwrap()).unwrap();
        (root, id)
    }

    // 归属校验（chunk/status 共用 load_dump_meta_owned）：他人 → E_AUTH；
    // 非法 id / 不存在 → E_NOTFOUND（与 publish::load_meta_owned 同语义）
    #[test]
    fn dump_session_ownership() {
        let (root, id) = ready_session("own", "alice", b"0123456789");
        assert_eq!(dump_status(&root, "alice", &id).unwrap().state, "ready");
        assert_eq!(dump_status(&root, "bob", &id).unwrap_err().code, "E_AUTH");
        assert_eq!(dump_chunk(&root, "bob", &id, 0, 4).unwrap_err().code, "E_AUTH");
        // 归属拒绝不动会话（文件仍在，他人无法借报错探测）
        assert!(dump_bin(&root, &id).exists());
        for bad in ["", "..", "../../etc/passwd", &"x".repeat(31), &"z".repeat(32)] {
            assert_eq!(dump_status(&root, "alice", bad).unwrap_err().code, "E_NOTFOUND", "{bad:?}");
        }
        assert_eq!(dump_status(&root, "alice", &state::rand_hex(16)).unwrap_err().code, "E_NOTFOUND");
    }

    // chunk 切片边界：整段 / 末片 / 恰好 total / 越界 / len 0 与超 4MB / 未就绪与失败态
    #[test]
    fn dump_chunk_boundaries() {
        let sealed: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
        let (root, id) = ready_session("chunk", "alice", &sealed);
        let total = sealed.len() as u64;

        // 中段切片
        let (b, t, fin) = dump_chunk(&root, "alice", &id, 10, 50).unwrap();
        assert_eq!((b.as_slice(), t, fin), (&sealed[10..60], total, false));
        // 末片：请求越界长度被钳到 total-offset，且 finished=true
        let (b, _, fin) = dump_chunk(&root, "alice", &id, 200, 1 << 20).unwrap();
        assert_eq!((b.as_slice(), fin), (&sealed[200..], true));
        // 恰好 total：0 字节幂等收尾探测，finished=true
        let (b, _, fin) = dump_chunk(&root, "alice", &id, total, 64).unwrap();
        assert_eq!((b.len(), fin), (0, true));
        // offset 越界 → E_NAME；len 0 / 超 4MB → E_NAME
        assert_eq!(dump_chunk(&root, "alice", &id, total + 1, 4).unwrap_err().code, "E_NAME");
        assert_eq!(dump_chunk(&root, "alice", &id, 0, 0).unwrap_err().code, "E_NAME");
        assert_eq!(dump_chunk(&root, "alice", &id, 0, DUMP_CHUNK_MAX + 1).unwrap_err().code, "E_NAME");

        // 会话删除：三件套幂等清空，此后访问 → E_NOTFOUND
        dump_cleanup(&root, &id);
        assert!(!dump_bin(&root, &id).exists() && !dump_meta_path(&root, &id).exists());
        assert_eq!(dump_chunk(&root, "alice", &id, 0, 4).unwrap_err().code, "E_NOTFOUND");
        dump_cleanup(&root, &id); // 幂等
    }

    // 未就绪 / 失败态：chunk 拒 E_STATE；status 如实回传 error
    #[test]
    fn dump_not_ready_and_failed_states() {
        let root = troot("states");
        let id = state::rand_hex(16);
        let meta = DumpMeta { user: "alice".into(), project: "p".into(), db: "d".into(), created: 1 };
        fs::write(dump_meta_path(&root, &id), serde_json::to_string(&meta).unwrap()).unwrap();
        // 未写 status（线程极早期）→ 默认 running
        assert_eq!(dump_status(&root, "alice", &id).unwrap().state, "running");
        assert_eq!(dump_chunk(&root, "alice", &id, 0, 4).unwrap_err().code, "E_STATE");
        // failed 态带错误信息
        let st = DumpStatus { state: "failed".into(), total: 0, error: Some("pg_dump boom".into()) };
        fs::write(dump_status_path(&root, &id), serde_json::to_string(&st).unwrap()).unwrap();
        let e = dump_chunk(&root, "alice", &id, 0, 4).unwrap_err();
        assert_eq!((e.code, e.msg.contains("pg_dump boom")), ("E_STATE", true));
        assert_eq!(dump_status(&root, "alice", &id).unwrap().error.as_deref(), Some("pg_dump boom"));
    }

    // CapReader（dump 的 256MB E_QUOTA 判定）：帽内全读；超帽报 io 错且置 over
    #[test]
    fn cap_reader_limit() {
        let over = Arc::new(AtomicBool::new(false));
        let mut r = CapReader::new(std::io::Cursor::new(vec![7u8; 200]), 150, over.clone());
        let mut out = Vec::new();
        assert!(r.read_to_end(&mut out).is_err(), "200 > 150 应报错");
        assert!(over.load(Ordering::Relaxed), "超帽必须置标记（E_QUOTA 判定）");

        let over2 = Arc::new(AtomicBool::new(false));
        let mut r2 = CapReader::new(std::io::Cursor::new(vec![7u8; 100]), 100, over2.clone());
        let mut out2 = Vec::new();
        r2.read_to_end(&mut out2).unwrap();
        assert_eq!(out2.len(), 100, "恰好帽内放行");
        assert!(!over2.load(Ordering::Relaxed));
    }

    // 切片计划纯函数的补充边界：total=0（空信封不可能出现，但计划层须自洽）
    #[test]
    fn plan_chunk_zero_total() {
        assert_eq!(plan_chunk(0, 0, 1).unwrap(), (0, true));
        assert_eq!(plan_chunk(0, 1, 1).unwrap_err().code, "E_NAME");
        assert_eq!(plan_chunk(4 << 20, 0, 4 << 20).unwrap(), (4 << 20, true));
    }

    // —— §B restore 魔数探测：zstd-19 包裹与原生 -Fc 两条路 ——
    // 魔数纯函数与三路由/拼接帧解包的单测在 unpack.rs（与 import_inner 共用封装）；
    // 此处保留 dump 生成侧 ↔ restore 解包侧的一致性。

    /// -Fc 样式样本："PGDMP" 头 + 伪数据（真实 -Fc 由服务端 pg_dump -Z0 -Fc 产出）。
    fn fc_sample() -> Vec<u8> {
        let mut v = b"PGDMP".to_vec();
        v.extend((0..8192u32).map(|i| (i % 251) as u8));
        v
    }

    // dump 生成侧一致性：pg_dump -Z0 -Fc 的产物（PGDMP 头原生流）经 zstd-19 包裹后
    // 正是 restore 侧 zstd 路径的输入形态（生成/解包两端对同一压缩容器往返）
    #[test]
    fn dump_zstd19_roundtrip_with_sink() {
        let fc = fc_sample();
        // 生成侧管道等价物：zstd-19 包裹（dump_seal_to 中 Encoder 的读侧效果）
        let packed = zstd::encode_all(fc.as_slice(), DUMP_ZSTD_LEVEL).unwrap();
        // restore 侧：魔数探测 + 解压（SharedBuf 见下）
        let b = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(vec![])));
        let mut s = crate::unpack::MagicSink::new(b.clone());
        s.write_all(&packed).unwrap();
        s.flush().unwrap();
        drop(s);
        assert_eq!(b.0.borrow().clone(), fc);
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
}
