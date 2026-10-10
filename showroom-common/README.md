# showroom-common

SPEC.md §1（SR-SIG1 签名认证）与 §2（加密信封 v1）的契约实现，showroomctl / showroom 共用。
密码学依赖以本 crate 锁定的版本为准：已 `pub use {ed25519_dalek, rsa}`，下游直接
`use showroom_common::{ed25519_dalek, rsa};`，勿另引版本（避免 `VerifyingKey` 等类型分裂）。

## 公共 API（与 src/lib.rs 同步维护）

```rust
pub const SIG_WINDOW_SECS: i64 = 300;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct OwnerKey(/* ed25519_dalek::SigningKey */);
impl OwnerKey {
    pub fn generate() -> Self;
    pub fn from_seed_file(path: &Path) -> Result<Self>;   // 裸 32B seed 文件
    pub fn save_seed(&self, path: &Path) -> Result<()>;    // 裸 32B seed，权限 0600
    pub fn pub_string(&self) -> String;                    // "ed25519:<base64(pubkey)>"
    pub fn from_pub_string(s: &str) -> Result<ed25519_dalek::VerifyingKey>;
}

pub fn build_canonical(owner: &str, ts: i64, nonce: &str, method: &str,
                       path_with_query: &str, body_sha256_hex: &str) -> String;
pub fn sign(owner: &str, key: &OwnerKey, method: &str, path_with_query: &str,
            body: &[u8], now_unix: i64, nonce: &str) -> String; // 完整 Authorization 值
pub struct Verified { pub owner: String, pub ts: i64, pub nonce: String }
pub fn verify(auth_value: &str, pubkeys: &[ed25519_dalek::VerifyingKey], method: &str,
              path_with_query: &str, body: &[u8], now_unix: i64)
    -> std::result::Result<Verified, AuthError>; // nonce 查重由调用方做
pub enum AuthError { Format, Stale, BadSignature } // Display 输出 E_AUTH 细节

pub struct PlatformKeys(/* rsa::RsaPrivateKey, RSA-3072 */);
impl PlatformKeys {
    pub fn generate() -> Self;
    pub fn save_pem(&self, dir: &Path) -> Result<PathBuf>; // dir/platform_rsa{.pub}，返回私钥路径（0600）
    pub fn load_pem(dir: &Path) -> Result<Self>;
    pub fn public_pem(&self) -> String;
}

// 信封 v1：[384B RSA-OAEP-SHA256 封装的 k] || 每段[12B nonce][密文+16B tag]，段=4MiB 明文
pub fn seal<W: Write>(platform_pub: &rsa::RsaPublicKey, plain: impl Read, out: W) -> std::io::Result<u64>; // 返回写出总字节
pub fn open(platform_priv: &rsa::RsaPrivateKey, sealed: impl Read, out: impl Write) -> std::io::Result<u64>; // 返回明文总字节
pub fn sha256_hex(data: &[u8]) -> String;
```

编码约定：`sign` 输出的签名为 base64url 无 padding；`pub_string` 为 base64 无 padding。
zstd 压缩不在本 crate——调用方在 `docker save` 管道自行压缩，再喂给 `seal`。

## 用法

```rust
use showroom_common::*; // 含重导出的 ed25519_dalek / rsa
use std::io::Cursor;

// —— 签名请求（CLI 端）——
let key = OwnerKey::from_seed_file(home.join(".showroom/id_ed25519").as_path())?;
let nonce = "0f1e2d3c4b5a6978"; // 8 随机字节 hex16，调用方生成
let auth = sign("alice", &key, "POST", "/_api/publish", body, now_unix, nonce);
// 请求头：Authorization: <auth>

// —— 验签（服务端）——
let v = verify(&auth, &[OwnerKey::from_pub_string(&pub_str)?], "POST", path_q, body, now)?;
// 之后用 v.nonce 做 per-owner 查重（LRU）

// —— 加密信封（import 通道）——
let pub_pem = std::fs::read_to_string("/srv/showroom/state/platform_rsa.pub")?;
let platform_pub = rsa::RsaPublicKey::from_public_key_pem(&pub_pem)?;
seal(&platform_pub, Cursor::new(zstd_tar_bytes), std::io::stdout())?; // 管道式写出
```
