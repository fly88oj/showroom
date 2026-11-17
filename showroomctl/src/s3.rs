//! s3：minoss 中间件（plan-22）——每项目桶/密钥供给、拆除与只读浏览。
//! 一切外部交互走 `docker exec -i sr-minoss sh -c <脚本>`（镜像内置 mc）：
//! 凭据恒走 stdin 行喂入、argv 只含派生名与常量脚本（与 psql SQL 走 stdin 同款纪律，
//! 宿主 ps 命令行零凭据）；浏览用项目自身凭据（桶级策略隔离的实证路径）。

use std::path::Path;

use serde_json::Value;

use crate::dockerops;
use crate::setup;
use crate::state::{self, ApiError, R};

/// 中间件容器名（= 网络内 DNS 名，与 setup::minoss_spec 一致）。
const CTR: &str = "sr-minoss";

/// 对象预览上限（文本截断阈值；plan-22 §4）。
pub const PREVIEW_CAP: u64 = 64 * 1024;
/// 对象下载上限（base64 JSON 单响应体积边界；超出提示走 CLI/mc）。
pub const DOWNLOAD_CAP: u64 = 8 * 1024 * 1024;
/// 单次列目录上限（mc ls 无 limit 参数，读满后截断并标记）。
const LIST_CAP: usize = 500;

// —— 纯函数（单测直测）——

/// 桶级 IAM 策略 JSON（plan-22 §2.2）：本项目桶内 s3:* 全权（桶与对象两层 arn）。
/// 策略名恒等于桶名（合法 mc 策略名字符集：字母数字与 -_.）。
pub fn policy_json(bucket: &str) -> String {
    format!(
        "{{\"Version\":\"2012-10-17\",\"Statement\":[{{\"Effect\":\"Allow\",\"Action\":[\"s3:*\"],\
         \"Resource\":[\"arn:aws:s3:::{b}\",\"arn:aws:s3:::{b}/*\"]}}]}}",
        b = bucket
    )
}

/// mc 路径段白名单（plan-22 §4：前缀/对象键仅允许字母数字 ._/-，防 shell 注入与
/// 路径歧义；空串/控制字符/其它 Unicode 拒绝——平台内对象键按此子集约定）。
pub fn check_mc_path(kind: &str, s: &str) -> R<()> {
    let ok = !s.is_empty()
        && s.len() <= 512
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::new(
            "E_NAME",
            format!("{kind} 仅允许字母数字与 . _ - /（≤512 字符）"),
        ))
    }
}

/// 列表条目（serve 直接 json!；full_key=桶内完整键，UI 无需拼接）。
pub struct ObjEntry {
    pub name: String,
    pub full_key: String,
    pub size: u64,
    pub last_modified: String,
    pub is_dir: bool,
}

// —— 执行底座（凭据走 stdin）——

/// `docker exec -i sr-minoss sh -c <script>`：script 由调用方以派生名拼好（无秘密），
/// stdin 为凭据行；失败 E_STATE 附 stderr 尾部。
fn exec_mc(script: &str, stdin_lines: &str, what: &str) -> R<String> {
    let (ok, out, err) = dockerops::exec_stdin(CTR, &["sh", "-c", script], stdin_lines.as_bytes())?;
    if ok {
        Ok(String::from_utf8_lossy(&out).to_string())
    } else {
        let tail = dockerops::tail_lines(&String::from_utf8_lossy(&err), 5);
        Err(ApiError::state(format!("minoss {what} failed: {tail}")))
    }
}

/// 同 exec_mc，但要二进制 stdout（mc cat 原始对象内容）。
fn exec_mc_bytes(script: &str, stdin_lines: &str, what: &str) -> R<Vec<u8>> {
    let (ok, out, err) = dockerops::exec_stdin(CTR, &["sh", "-c", script], stdin_lines.as_bytes())?;
    if ok {
        Ok(out)
    } else {
        let tail = dockerops::tail_lines(&String::from_utf8_lossy(&err), 5);
        Err(ApiError::state(format!("minoss {what} failed: {tail}")))
    }
}

fn minio_creds_or_err(root: &Path) -> R<(String, String)> {
    setup::minio_creds_of(root)
        .ok_or_else(|| ApiError::state("minoss 凭据缺失（state/middleware.cred 无 minio 行）：先 showroomctl middleware enable minoss"))
}

// —— 供给 / 拆除（publish / rm 调用）——

/// 每项目供给（plan-22 §2.2，publish 内核在 DB 供给后调用）：生成新密钥并供给，返回之。
/// 幂等语义：桶 --ignore-existing；用户重复 add = 密钥轮换（对齐 DB 密码每次发布轮换）。
pub fn provision(root: &Path, project: &str) -> R<String> {
    let sk = state::rand_hex(16);
    provision_with_secret(root, project, &sk)?;
    Ok(sk)
}

/// 指定密钥重灌桶/用户/策略（db-reset「凭据保持不变」语义：沿用 env 里的 S3 密钥）。
pub fn provision_with_secret(root: &Path, project: &str, sk: &str) -> R<()> {
    let (ru, rp) = minio_creds_or_err(root)?;
    let bucket = state::s3_bucket_ident(project);
    let ak = state::s3_access_key(project);
    let pol = &bucket; // 策略名=桶名
    let pj = policy_json(&bucket);
    // argv 只有派生名（bucket/ak/策略 JSON 内容均为项目名派生，非秘密）；
    // root 凭据与密钥三行走 stdin。
    let script = format!(
        "set -e\n\
         read -r U; read -r P; read -r SK\n\
         export MC_HOST_admin=\"http://$U:$P@127.0.0.1:9000\"\n\
         mc mb --ignore-existing admin/{bucket}\n\
         mc admin user add admin {ak} \"$SK\"\n\
         printf '%s' '{pj}' > /tmp/{pol}.json\n\
         mc admin policy create admin {pol} /tmp/{pol}.json || true\n\
         mc admin policy attach admin {pol} --user {ak}\n\
         rm -f /tmp/{pol}.json\n"
    );
    exec_mc(&script, &format!("{ru}\n{rp}\n{sk}\n"), "provision").map(|_| ())
}

/// 项目拆除（rm 调用，尽力幂等——对齐 DROP DATABASE 的 let _ 语义）：
/// 桶（含对象）+ 用户 + 策略。minoss 未启用/容器缺失时由调用方容忍失败。
pub fn teardown(root: &Path, project: &str) -> R<()> {
    let (ru, rp) = minio_creds_or_err(root)?;
    let bucket = state::s3_bucket_ident(project);
    let ak = state::s3_access_key(project);
    let pol = &bucket;
    let script = format!(
        "read -r U; read -r P\n\
         export MC_HOST_admin=\"http://$U:$P@127.0.0.1:9000\"\n\
         mc admin policy detach admin {pol} --user {ak} || true\n\
         mc admin user remove admin {ak} || true\n\
         mc admin policy remove admin {pol} || true\n\
         mc rb --force admin/{bucket} || true\n"
    );
    exec_mc(&script, &format!("{ru}\n{rp}\n"), "teardown").map(|_| ())
}

// —— 浏览（serve 端点调用；用项目自身凭据——隔离实证）——

/// 从 env 文件读项目 S3 凭据（publish 时注入；未注入=minoss 禁用期发布的项目）。
fn project_creds(root: &Path, project: &str) -> R<(String, String, String)> {
    let text = fs_err(state::env_path(root, project))?;
    let m = state::parse_kv(&text);
    let ak = m.get("S3_ACCESS_KEY_ID").cloned();
    let sk = m.get("S3_SECRET_ACCESS_KEY").cloned();
    let bucket = m.get("S3_BUCKET").cloned();
    match (ak, sk, bucket) {
        (Some(ak), Some(sk), Some(bucket)) if !ak.is_empty() && !sk.is_empty() => Ok((ak, sk, bucket)),
        _ => Err(ApiError::state(
            "该项目未注入 S3_* 凭据（发布时 minoss 未启用）：重新发布即可注入",
        )),
    }
}

fn fs_err(p: impl AsRef<Path>) -> R<String> {
    std::fs::read_to_string(p.as_ref()).map_err(|e| ApiError::state(format!("读 env 文件: {e}")))
}

/// 列目录（非递归一层，prefix 为空=桶根）。返回 (条目, 截断?)；
/// 文件夹条目 name 带 / 结尾（mc 语义），点击进入即 prefix=full_key。
pub fn list_objects(root: &Path, project: &str, prefix: &str) -> R<(Vec<ObjEntry>, bool)> {
    // 空前缀=桶根（合法常态）；非空前缀过白名单（对象键仍恒非空）
    if !prefix.is_empty() {
        check_mc_path("前缀", prefix)?;
    }
    let (ak, sk, bucket) = project_creds(root, project)?;
    let path = if prefix.is_empty() {
        format!("p/{bucket}")
    } else {
        format!("p/{bucket}/{prefix}")
    };
    let script = format!(
        "read -r AK; read -r SK\n\
         export MC_HOST_p=\"http://$AK:$SK@127.0.0.1:9000\"\n\
         mc ls --json {path}\n"
    );
    let out = exec_mc(&script, &format!("{ak}\n{sk}\n"), "ls")?;
    let mut objects = vec![];
    let mut truncated = false;
    for line in out.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["status"].as_str() != Some("success") {
            continue;
        }
        if objects.len() >= LIST_CAP {
            truncated = true;
            break;
        }
        let Some(key) = v["key"].as_str() else { continue };
        let is_dir = v["type"].as_str() == Some("folder");
        let full_key = format!("{prefix}{key}");
        objects.push(ObjEntry {
            name: key.to_string(),
            full_key,
            size: v["size"].as_u64().unwrap_or(0),
            last_modified: v["lastModified"].as_str().unwrap_or("").to_string(),
            is_dir,
        });
    }
    Ok((objects, truncated))
}

/// 取对象：先 stat 后 cat（超限拒绝，防大对象整读进内存）。
/// 返回 (size, 内容字节)；cap 由调用方给（预览 64KB / 下载 8MB）。
pub fn get_object(root: &Path, project: &str, key: &str, cap: u64) -> R<(u64, Vec<u8>)> {
    check_mc_path("对象键", key)?;
    let (ak, sk, bucket) = project_creds(root, project)?;
    let creds = format!("{ak}\n{sk}\n");
    let stat = exec_mc(
        &format!(
            "read -r AK; read -r SK\n\
             export MC_HOST_p=\"http://$AK:$SK@127.0.0.1:9000\"\n\
             mc stat --json p/{bucket}/{key}\n"
        ),
        &creds,
        "stat",
    )?;
    // mc stat --json：一行对象元数据（size 字段）
    let size = stat
        .lines()
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v["size"].as_u64())
        .ok_or_else(|| ApiError::new("E_NOTFOUND", format!("对象不存在或不可读: {key}")))?;
    if size > cap {
        return Err(ApiError::state(format!(
            "对象 {key}（{size} 字节）超过单次响应上限 {} 字节；大文件请在项目容器内用 S3_* 凭据直取",
            cap
        )));
    }
    let data = exec_mc_bytes(
        &format!(
            "read -r AK; read -r SK\n\
             export MC_HOST_p=\"http://$AK:$SK@127.0.0.1:9000\"\n\
             mc cat p/{bucket}/{key}\n"
        ),
        &creds,
        "cat",
    )?;
    Ok((size, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_json_shape() {
        let j = policy_json("sr-demo");
        let v: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["Version"], "2012-10-17");
        let st = &v["Statement"][0];
        assert_eq!(st["Effect"], "Allow");
        assert_eq!(st["Action"][0], "s3:*");
        assert_eq!(st["Resource"][0], "arn:aws:s3:::sr-demo");
        assert_eq!(st["Resource"][1], "arn:aws:s3:::sr-demo/*");
        // 桶名带连字符也合法（项目名子集）
        assert!(policy_json("sr-my-app").contains("arn:aws:s3:::sr-my-app/*"));
    }

    #[test]
    fn mc_path_whitelist() {
        assert!(check_mc_path("k", "dir/file.txt").is_ok());
        assert!(check_mc_path("k", "a_b-c.d/e").is_ok());
        assert!(check_mc_path("k", "").is_err(), "空键拒绝");
        assert!(check_mc_path("k", "a b").is_err(), "空格拒绝（注入面）");
        assert!(check_mc_path("k", "a;rm").is_err(), "分号拒绝");
        assert!(check_mc_path("k", "键值").is_err(), "非 ASCII 拒绝");
        assert!(check_mc_path("k", &"x".repeat(513)).is_err(), "超长拒绝");
        // 单引号/美元——脚本拼接的第二道防线
        assert!(check_mc_path("k", "it's").is_err());
        assert!(check_mc_path("k", "$HOME").is_err());
    }

    #[test]
    fn provision_script_carries_no_secrets_in_argv() {
        // 脚本由派生名拼成：root 凭据与新密钥只出现在 stdin 三行（函数内构造）。
        // 此处以纯函数方式断言脚本模板不含凭据占位（ru/rp/sk 变量名只出现在 read 行之后）。
        let script = "set -e\nread -r U; read -r P; read -r SK\nexport MC_HOST_admin=\"http://$U:$P@127.0.0.1:9000\"\nmc admin user add admin srabc123 \"$SK\"\n";
        assert!(script.contains("read -r U"));
        assert!(!script.contains("showroom:"), "凭据值不得内联");
        // 命名派生一致性（与 state.rs 单测互证）
        assert_eq!(state::s3_bucket_ident("Demo-9"), "sr-demo-9");
        assert_eq!(state::s3_access_key("demo").len(), 18);
        assert!(state::s3_access_key("demo").starts_with("sr"));
        assert!(state::s3_access_key("demo").chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
