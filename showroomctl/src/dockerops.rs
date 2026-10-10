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
