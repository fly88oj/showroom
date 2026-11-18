//! redis：`showroom redis` 子命令（SPEC §10.3 浏览端点的 CLI 面，与控制台数据页 Redis tab 对齐）。
//! dbs=名下项目的逻辑库映射；keys=键列表（--q 前缀过滤）；get=查看键值（类型/ttl/内容）。
//! 浏览隔离同网页：db 须 ∈ 签名用户名下项目的 redisdb 集合（管理员全量）。

use std::io::Write;

use clap::Subcommand;
use serde_json::Value;

use crate::api::{self, Api, Which};
use crate::config::Config;

#[derive(Subcommand)]
pub enum RedisCmd {
    /// 列出可浏览的逻辑库（db → 项目映射；管理员可见全池）
    Dbs,
    /// 列出键（--q 前缀过滤，单次上限 200）
    Keys {
        db: u8,
        /// 前缀过滤（缺省 *）
        #[arg(long, default_value = "*")]
        q: String,
    },
    /// 查看键值（类型/ ttl / 内容；长值截断到 4KB，同网页）
    Get {
        db: u8,
        key: String,
    },
}

pub fn run(cfg: &Config, force: Option<Which>, cmd: &RedisCmd) -> Result<(), String> {
    match cmd {
        RedisCmd::Dbs => dbs(cfg, force),
        RedisCmd::Keys { db, q } => keys(cfg, force, *db, q),
        RedisCmd::Get { db, key } => get(cfg, force, *db, key),
    }
}

fn dbs(cfg: &Config, force: Option<Which>) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get("/_api/redis-dbs")?;
    if let Some(entries) = v["entries"].as_array() {
        let named: Vec<&Value> = entries.iter().filter(|e| e["project"].is_string()).collect();
        if named.is_empty() {
            println!("（您名下暂无可浏览的项目——发布项目后自动出现）");
        } else {
            println!("{:<5} {}", "DB", "PROJECT");
            for e in named {
                println!("{:<5} {}", e["db"].as_u64().unwrap_or(0), e["project"].as_str().unwrap_or("?"));
            }
        }
    }
    Ok(())
}

fn keys(cfg: &Config, force: Option<Which>, db: u8, q: &str) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get(&api::redis_keys_query(db, q))?;
    if let Some(keys) = v["keys"].as_array() {
        if keys.is_empty() {
            println!("（无匹配键）");
        } else {
            for k in keys {
                println!("{}", k.as_str().unwrap_or("?"));
            }
        }
    }
    if v["truncated"].as_bool() == Some(true) {
        eprintln!("（单次上限 200 已截断，可用 --q 前缀缩小范围）");
    }
    Ok(())
}

fn get(cfg: &Config, force: Option<Which>, db: u8, key: &str) -> Result<(), String> {
    let api = Api::connect(cfg, force)?;
    let v = api.signed_get(&api::redis_get_query(db, key))?;
    let t = v["type"].as_str().unwrap_or("?");
    let ttl = v["ttl_ms"].as_i64().or_else(|| v["ttl"].as_i64());
    let ttl_s = match ttl {
        Some(-1) => "永久（-1）".to_string(),
        Some(ms) => format!("{ms} ms"),
        None => "未知".to_string(),
    };
    println!("type: {t}");
    println!("ttl: {ttl_s}");
    let val = render_value(&v["value"]);
    print!("{val}");
    let _ = std::io::stdout().flush();
    Ok(())
}

/// 值渲染对齐网页 pickKey：数组 join 换行、对象 JSON 缩进、标量原样
fn render_value(v: &Value) -> String {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|x| match x.as_str() {
                Some(s) => s.to_string(),
                None => x.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => serde_json::to_string_pretty(v).unwrap_or_default(),
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
