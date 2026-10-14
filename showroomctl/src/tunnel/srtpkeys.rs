//! srtpkeys：per-user SRTP 包裹密钥与 srtp_id 注册表（plan-18 §3.3）。
//!
//! 登录（OPAQUE 成功）→ `issue`：load-or-gen 该用户的 RSA-3072 包裹密钥
//! （`state/users/<u>/srtp_rsa`，0600）+ 新鲜 8B srtp_id，随 login-finish 下发
//! （key_mac 绑定，见 opaque::srtp_key_mac）。网关/serve `lookup`：id → 私钥。
//! id 只是路由句柄：泄露 id 不授予任何能力（内层仍 SR-SIG1 验签），无需急切吊销；
//! 每用户保留最近 8 个 id（多设备），登录时 GC。
//! 内存缓存 + `state/srtp_ids.json` 持久化（进程重启后 lookup 惰性重建）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use showroom_common::rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use showroom_common::rsa::RsaPrivateKey;

use crate::state::{self, ApiError, R};

type KeyMap = HashMap<[u8; 8], (Arc<RsaPrivateKey>, Arc<str>)>;

fn cache() -> &'static RwLock<KeyMap> {
    static C: OnceLock<RwLock<KeyMap>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(HashMap::new()))
}

fn ids_path(root: &Path) -> std::path::PathBuf {
    root.join("state/srtp_ids.json")
}

fn user_key(root: &Path, user: &str) -> std::path::PathBuf {
    root.join(format!("state/users/{user}/srtp_rsa"))
}

/// load-or-gen 该用户的包裹密钥；返回 (私钥, 公钥 PEM)。
fn load_or_gen(root: &Path, user: &str) -> R<(RsaPrivateKey, String)> {
    let priv_p = user_key(root, user);
    if let Ok(pem) = std::fs::read_to_string(&priv_p) {
        if let Ok(k) = RsaPrivateKey::from_pkcs8_pem(&pem) {
            let pub_pem = k
                .to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .map_err(|e| ApiError::state(format!("encode srtp pub: {e}")))?
                .as_str()
                .to_owned();
            return Ok((k, pub_pem));
        }
    }
    // 首登惰性生成（一次性 ~0.5s）
    let mut rng = rand::rngs::OsRng;
    let k = RsaPrivateKey::new(&mut rng, 3072)
        .map_err(|e| ApiError::state(format!("srtp keygen: {e}")))?;
    let dir = priv_p.parent().unwrap_or(root);
    std::fs::create_dir_all(dir)?;
    let pem = k
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| ApiError::state(format!("encode srtp priv: {e}")))?;
    state::write_private(&priv_p, pem.as_str())?;
    let pub_pem = k
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| ApiError::state(format!("encode srtp pub: {e}")))?
        .as_str()
        .to_owned();
    std::fs::write(priv_p.with_extension("pub"), pub_pem.as_bytes())?;
    Ok((k, pub_pem))
}

/// 读取/初始化 id 映射（id_hex → user；value 空数组=同名多次登录记时间序，用 JSON map+序表）。
/// 简化持久化形状：{"ids": {"<id_hex>": "<user>"}, "order": {"<user>": ["<id_hex>", ...]}}
fn load_map(root: &Path) -> serde_json::Value {
    std::fs::read_to_string(ids_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"ids": {}, "order": {}}))
}

fn save_map(root: &Path, v: &serde_json::Value) {
    let _ = std::fs::write(ids_path(root), serde_json::to_string_pretty(v).unwrap_or_default());
}

/// 登录签发：fresh id + 该用户密钥；内存登记 + 落盘（每用户保最近 8 id）。
pub fn issue(root: &Path, user: &str) -> R<([u8; 8], String)> {
    let (k, pub_pem) = load_or_gen(root, user)?;
    let mut id = [0u8; 8];
    use rand::RngCore as _;
    rand::rngs::OsRng.fill_bytes(&mut id);
    let k = Arc::new(k);
    let priv_pem: Arc<str> = Arc::from(
        k.to_pkcs8_pem(showroom_common::rsa::pkcs8::LineEnding::LF)
            .map_err(|e| ApiError::state(format!("encode srtp priv: {e}")))?
            .as_str(),
    );
    cache().write().unwrap().insert(id, (k, priv_pem));

    let mut m = load_map(root);
    let id_hex = hex::encode(id);
    m["ids"][&id_hex] = serde_json::json!(user);
    let order = m["order"][user].as_array().cloned().unwrap_or_default();
    let mut order = order;
    order.push(serde_json::json!(id_hex));
    while order.len() > 8 {
        let old = order.remove(0).as_str().unwrap_or_default().to_string();
        if !old.is_empty() {
            m["ids"].as_object_mut().unwrap().remove(&old);
        }
    }
    m["order"][user] = serde_json::json!(order);
    save_map(root, &m);
    Ok((id, pub_pem))
}

/// id → 私钥（网关/serve 用；miss 时从磁盘重建缓存）。
/// id → 私钥 PEM（网关每连接解 wrap 用；缓存免重复编码）。
pub fn lookup_pem(root: &Path, id: &[u8; 8]) -> Option<Arc<str>> {
    lookup_pair(root, id).map(|(_, pem)| pem)
}

fn lookup_pair(root: &Path, id: &[u8; 8]) -> Option<(Arc<RsaPrivateKey>, Arc<str>)> {
    if let Some(v) = cache().read().unwrap().get(id) {
        return Some(v.clone());
    }
    // 重建：srtp_ids.json → user → 读私钥文件
    let m = load_map(root);
    let user = m["ids"].get(hex::encode(id))?.as_str()?;
    let pem = std::fs::read_to_string(user_key(root, user)).ok()?;
    let k: Arc<RsaPrivateKey> = Arc::new(RsaPrivateKey::from_pkcs8_pem(&pem).ok()?);
    let pair = (k.clone(), Arc::from(pem.as_str()));
    cache().write().unwrap().insert(*id, pair.clone());
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_lookup_and_per_user_gc() {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("sr-srtpkeys-{}-{n}.d", std::process::id()));
        std::fs::create_dir_all(root.join("state/users/alice")).unwrap();
        let (id1, pub1) = issue(&root, "alice").unwrap();
        // 同用户二次签发：同 pub（load-or-gen），不同 id
        let (id2, pub2) = issue(&root, "alice").unwrap();
        assert_ne!(id1, id2);
        assert_eq!(pub1, pub2, "per-user 密钥跨登录稳定（轮换=重置路径）");
        // lookup 命中 + 重建路径（清内存缓存后仍可查）
        assert!(lookup_pem(&root, &id1).is_some());
        cache().write().unwrap().clear();
        assert!(lookup_pem(&root, &id2).is_some());
        assert!(lookup_pem(&root, &[9u8; 8]).is_none());
        // GC：9 个 id 后最老被清（磁盘 ids 移除 → 重建后查不到）
        for _ in 0..7 {
            let _ = issue(&root, "alice").unwrap();
        }
        let m = load_map(&root);
        assert!(m["ids"].get(hex::encode(id1)).is_none(), "最老 id 被 GC");
        cache().write().unwrap().clear();
        assert!(lookup_pem(&root, &id1).is_none(), "GC 后不可再查");
        let _ = std::fs::remove_dir_all(&root);
    }
}
