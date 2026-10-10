//! dockerops：全部外部世界调用的唯一出口——docker / psql(经 docker exec) / caddy 一律 subprocess，
//! registry v2 API 用裸 TCP 的 HTTP/1.0 极简客户端（127.0.0.1:5000，无第三方 HTTP 依赖）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::engine::Engine as _;

use crate::state::{self, ApiError, R};

pub const REGISTRY: &str = "127.0.0.1:5000"; // SPEC §4：镜像 127.0.0.1:5000/<owner>/<project>:<tag>
pub const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.index.v1+json";
pub const CADDY_FILE: &str = "/etc/caddy/Caddyfile";
pub const DOCKER_NET: &str = "showroom-net";

fn fail(what: &str, o: &Output) -> ApiError {
    ApiError::state(format!(
        "{what} failed: {}{}",
        String::from_utf8_lossy(&o.stdout).trim(),
        String::from_utf8_lossy(&o.stderr).trim()
    ))
}

/// 取文本尾部若干行（psql/pg_restore 报错裁剪进 ApiError.msg，防膨胀）。
pub fn tail_lines(s: &str, n: usize) -> String {
    let v: Vec<&str> = s.lines().collect();
    let start = v.len().saturating_sub(n);
    v[start..].join("\n")
}

/// 提前返回前杀掉并回收子进程（防僵尸；对已退出进程 kill/wait 只是 Err，忽略）。
pub fn kill_wait(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub fn run(cmd: &str, args: &[&str]) -> R<String> {
    let what = format!("{cmd} {}", args.join(" "));
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| ApiError::state(format!("spawn {cmd}: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(fail(&what, &out))
    }
}

/// 尽力执行：返回是否成功（用于“可能不存在”的清理动作）。
pub fn run_fail_ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn run_stdin(cmd: &str, args: &[&str], what: &str, stdin: &[u8]) -> R<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn {cmd}: {e}")))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin)
        .map_err(|e| ApiError::state(format!("{what} stdin: {e}")))?;
    let out = child
        .wait_with_output()
        .map_err(|e| ApiError::state(format!("{what} wait: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(fail(what, &out))
    }
}

pub fn docker(args: &[&str]) -> R<String> {
    run("docker", args)
}

/// psql 经 `docker exec -i sr-postgres psql`，SQL 走 stdin 免拼接转义；ON_ERROR_STOP=1 让失败非零退出。
pub fn psql(sql: &str) -> R<()> {
    run_stdin(
        "docker",
        &["exec", "-i", "sr-postgres", "psql", "-U", "postgres", "-v", "ON_ERROR_STOP=1"],
        "psql",
        sql.as_bytes(),
    )
    .map(|_| ())
}

// —— SPEC §9 数据库维护：定库 psql / pg_dump（pg_restore 的流式管道在 dbmaint）——

/// `docker exec -i sr-postgres <args>` 执行底座：stdin 全量送入（独立线程，防大 SQL 与大输出管道互锁），
/// stdout/stderr 全量收集（wait_with_output 内部并行排空），返回 (成功?, stdout, stderr)。
fn exec_pg(args: &[&str], stdin_data: &[u8]) -> R<(bool, Vec<u8>, Vec<u8>)> {
    let mut child = Command::new("docker")
        .args(["exec", "-i", "sr-postgres"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn docker exec: {e}")))?;
    let mut stdin = child.stdin.take().unwrap();
    let data = stdin_data.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data); // EPIPE（子进程早退）忽略，退出状态兜底
    });
    let out = child
        .wait_with_output()
        .map_err(|e| ApiError::state(format!("docker exec wait: {e}")))?;
    let _ = writer.join();
    Ok((out.status.success(), out.stdout, out.stderr))
}

/// 定库 psql：SQL 恒走 stdin（绝不进 argv）；extra 为固定旗标（-t -A -F …）。
/// 成功返回 stdout 全文（psql 文本格式）；失败 E_STATE 附 stderr 尾部几行。
pub fn psql_db(db: &str, extra: &[&str], sql: &[u8]) -> R<String> {
    let mut args: Vec<&str> = vec!["psql", "-U", "postgres", "-d", db, "-v", "ON_ERROR_STOP=1"];
    args.extend_from_slice(extra);
    let (ok, out, err) = exec_pg(&args, sql)?;
    if ok {
        Ok(String::from_utf8_lossy(&out).to_string())
    } else {
        let tail = tail_lines(&String::from_utf8_lossy(&err), 8);
        Err(ApiError::state(format!("psql -d {db} failed: {tail}")))
    }
}

/// 库是否存在：在 postgres 维护库上查 pg_database（db 名来自已校验名字的归一化，无注入面）。
pub fn db_exists(db: &str) -> R<bool> {
    let sql = format!("SELECT 1 FROM pg_database WHERE datname = '{db}'");
    let (ok, out, err) = exec_pg(
        &["psql", "-U", "postgres", "-d", "postgres", "-t", "-A", "-v", "ON_ERROR_STOP=1"],
        sql.as_bytes(),
    )?;
    if !ok {
        let tail = tail_lines(&String::from_utf8_lossy(&err), 8);
        return Err(ApiError::state(format!("psql pg_database: {tail}")));
    }
    Ok(String::from_utf8_lossy(&out).trim() == "1")
}

/// 容器状态（docker inspect .State.Status）；容器不存在 → None。
pub fn container_status(name: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// 健康检查状态（镜像无 HEALTHCHECK 时为 None）。
pub fn health_status(name: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["inspect", "-f", "{{if .State.Health}}{{.State.Health.Status}}{{end}}", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// docker logs（stdout+stderr 合并；容器的 stderr docker 会写到进程 stderr）。
pub fn logs(name: &str, tail: u32) -> R<String> {
    let out = Command::new("docker")
        .args(["logs", "--tail", &tail.to_string(), name])
        .output()
        .map_err(|e| ApiError::state(format!("docker logs: {e}")))?;
    if !out.status.success() {
        return Err(fail("docker logs", &out));
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

pub fn caddy_validate() -> R<()> {
    run("caddy", &["validate", "--config", CADDY_FILE, "--adapter", "caddyfile"]).map(|_| ())
}

pub fn caddy_reload() -> R<()> {
    run("caddy", &["reload", "--config", CADDY_FILE, "--adapter", "caddyfile"]).map(|_| ())
}

// —— Redis 浏览（SPEC §10.3）：redis-cli 恒走 docker exec sr-redis（ponytail：不引 redis 客户端库）——

/// redis-cli 单命令（TYPE/PTTL/GET/LRANGE/HGETALL/SMEMBERS/ZRANGE）：argv 传参（无 shell），
/// 返回 stdout 全文（非交互 redis-cli 为 raw 文本输出）。
pub fn redis_cli(db: u8, args: &[&str]) -> R<String> {
    let db_s = db.to_string();
    let mut argv: Vec<&str> = vec!["exec", "sr-redis", "redis-cli", "-n", &db_s];
    argv.extend_from_slice(args);
    docker(&argv)
}

/// `redis-cli -n <db> --scan --pattern <p>`：增量读行，收满 limit 个后多读到一行即杀子进程并标截断
/// （--scan 本无 limit 参数；杀掉 docker exec 客户端后容器内 redis-cli 写断管自退，不陪跑全量 scan）。
/// 返回 (前 limit 个 key, 是否截断)；EOF 自然结束则 truncated=false。
pub fn redis_scan(db: u8, pattern: &str, limit: usize) -> R<(Vec<String>, bool)> {
    use std::io::BufRead;
    let db_s = db.to_string();
    let mut child = Command::new("docker")
        .args(["exec", "sr-redis", "redis-cli", "-n", &db_s, "--scan", "--pattern", pattern])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::state(format!("spawn redis-cli --scan: {e}")))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut keys: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut line = String::new();
    {
        let mut reader = std::io::BufReader::new(&mut stdout);
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF：scan 自然结束
                Ok(_) => {
                    let l = line.trim();
                    if l.is_empty() {
                        continue;
                    }
                    if keys.len() >= limit {
                        truncated = true; // 已够 limit 又见一行 → 还有更多
                        break;
                    }
                    keys.push(l.to_string());
                }
                Err(e) => {
                    kill_wait(&mut child);
                    return Err(ApiError::state(format!("redis --scan read: {e}")));
                }
            }
        }
    }
    if truncated {
        kill_wait(&mut child);
        return Ok((keys, true));
    }
    let out = child
        .wait_with_output()
        .map_err(|e| ApiError::state(format!("redis --scan wait: {e}")))?;
    if !out.status.success() {
        return Err(fail("redis-cli --scan", &out));
    }
    Ok((keys, false))
}

// —— registry v2：HTTP/1.0 over TcpStream（HTTP/1.0 服务器不得用 chunked，读至 EOF 即 body）——

/// state/_internal.cred 的 user:pass；不存在则生成 `_internal` 账号并登记进 registry.htpasswd。
pub fn ensure_internal_cred(root: &Path) -> R<(String, String)> {
    let p = root.join("state/_internal.cred");
    if let Ok(s) = std::fs::read_to_string(&p) {
        if let Some((u, pw)) = s.trim().split_once(':') {
            if !u.is_empty() && !pw.is_empty() {
                return Ok((u.to_string(), pw.to_string()));
            }
        }
    }
    let pass = state::rand_hex(16);
    let hash = bcrypt::hash(&pass, 10).map_err(|e| ApiError::state(format!("bcrypt: {e}")))?;
    state::upsert_htpasswd(root, "_internal", &hash)?;
    state::write_private(&p, &format!("_internal:{pass}\n"))?;
    Ok(("_internal".into(), pass))
}

/// 返回 (status, 小写 headers, body)。
pub fn http_registry(root: &Path, method: &str, path: &str, accept: Option<&str>) -> R<(u16, Vec<(String, String)>, Vec<u8>)> {
    let (user, pass) = ensure_internal_cred(root)?;
    let auth = STANDARD.encode(format!("{user}:{pass}"));
    let mut req = format!(
        "{method} {path} HTTP/1.0\r\nHost: {REGISTRY}\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n"
    );
    if let Some(a) = accept {
        req.push_str(&format!("Accept: {a}\r\n"));
    }
    req.push_str("\r\n");

    let mut s = TcpStream::connect(REGISTRY)
        .map_err(|e| ApiError::new("E_NET", format!("registry {REGISTRY}: {e}")))?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;

    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| ApiError::state("registry: malformed http response"))?;
    let head = String::from_utf8_lossy(&buf[..sep]).to_string();
    let body = buf[sep + 4..].to_vec();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| ApiError::state(format!("registry: bad status line '{status_line}'")))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok((code, headers, body))
}

pub fn header<'a>(hdrs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    hdrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

/// GET manifest 判断镜像是否已在 registry（200 / 非 200）。
pub fn image_exists(root: &Path, repo: &str, tag: &str) -> R<bool> {
    let (code, _, _) = http_registry(root, "GET", &format!("/v2/{repo}/manifests/{tag}"), Some(MANIFEST_ACCEPT))?;
    Ok(code == 200)
}

/// push 后取 manifest digest（Docker-Content-Digest 头）。
pub fn manifest_digest(root: &Path, repo: &str, tag: &str) -> R<String> {
    let (code, hdrs, _) = http_registry(root, "GET", &format!("/v2/{repo}/manifests/{tag}"), Some(MANIFEST_ACCEPT))?;
    if code != 200 {
        return Err(ApiError::state(format!("registry manifest {repo}:{tag}: HTTP {code}")));
    }
    header(&hdrs, "docker-content-digest")
        .map(str::to_string)
        .ok_or_else(|| ApiError::state("registry response missing Docker-Content-Digest"))
}
