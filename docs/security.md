# Showroom 安全设计

本文是安全面的**规范文档**：认证协议 SR-SIG1 全文、加密信封字节布局、管理员体系与零信息暴露语义，以及威胁与缓解对照表。部署与应急视角见根目录 [SECURITY.md](../SECURITY.md)。

Showroom 的安全前提：控制面流量可能经过不可信的明文链路（公网隧道、无 TLS 反代），因此不依赖传输层——认证、完整性、机密性全部实现在应用层。

---

## 1. SR-SIG1 请求签名

### 1.1 头格式

```
Authorization: SR-SIG1 <user>:<unix_ts>:<nonce_hex16>:<sig_b64url>
```

| 字段 | 含义 |
|---|---|
| `user` | 发布者账号名 |
| `unix_ts` | 请求时刻的 Unix 秒级时间戳 |
| `nonce_hex16` | 16 个 hex 字符（8 随机字节），每次请求唯一 |
| `sig_b64url` | Ed25519 签名，base64url **无 padding** |

### 1.2 canonical 串

```
canonical = "SR-SIG1\n<user>\n<ts>\n<nonce>\n<METHOD>\n<path_with_query>\n<sha256_hex(body)>"

sig = Ed25519_sign(user_priv, canonical.utf8)
```

- `METHOD` 大写 HTTP 方法；`path_with_query` 为含查询串的完整路径（如 `/_api/logs?project=demo&tail=100`）；
- `body` 为空时取 `sha256_hex("")`；
- 服务端校验四要素：user 已登记、`|now − ts| ≤ 300s`、nonce 未见（per-user 查重，服务重启清零可接受）、验签通过。

### 1.3 防重放与防篡改

- **时间窗口 ±300 秒**：签名即刻过期，客户端与服务端时钟需大致同步（NTP）；
- **nonce 查重**：同一 user 的 nonce 一次有效，窗口内截获重放同一请求会被拒；
- **方法/路径/请求体全覆盖**：URL 改写、参数篡改、body 替换都会破坏签名。

### 1.4 密钥体系

- 发布者密钥：Ed25519，`~/.showroom/id_ed25519`（裸 32B seed，权限 0600）；公钥串格式 `ed25519:<base64(pubkey)>`；
- 一个 user 可登记多把公钥（`state/tokens/<user>/<kid>.pub`），`kid` = 公钥 sha256 前 8 hex；
- 验签时对 user 名下**全部**公钥尝试匹配，命中哪把即以哪把的 `kid` 记录身份（`GET /_api/whoami` 据此判定 admin）。

---

## 2. 加密信封 v1（大载荷传输）

import（镜像上行）、db-restore（恢复上行）、db-dump（备份下行）共用同一信封格式：

```
k   = 32B random                       # AES-256-GCM 密钥，一次性
wrap= RSA-3072-OAEP-SHA256(peer_pub, k)   # 定长 384 字节
body= wrap || aes256gcm(k, zstd(payload))
     # AES-GCM 流式分段：每段 [12B nonce][密文||16B tag]，段大小 4MiB 明文
```

### 2.1 字节布局

```text
偏移 0            384                 384+14 ...
┌──────────────────┬──────────────────┬─────────────────────┬─ ─ ─ ─ ─ ┐
│ RSA-OAEP 包裹的 k │ 段1 nonce(12B)   │ 段1 密文‖tag(16B)    │ 段2 …    │
└──────────────────┴──────────────────┴─────────────────────┴─ ─ ─ ─ ─ ┘
   定长 384B            └────────── 每段：12B nonce + 密文 + 16B tag ──────┘
```

总长度不变量：`384 + n + ceil(n / 4MiB) × (12 + 16)`（n 为压缩后明文字节数）。

### 2.2 密钥协商方向

| 通道 | 封装公钥 | 持私钥方 |
|---|---|---|
| 上行（import / db-restore） | 平台 RSA-3072 公钥（`~/.showroom/platform_rsa.pub`，对应服务端 `state/platform_rsa{.pub}`） | 平台 |
| 下行（db-dump） | 客户端**一次性** RSA-3072 公钥（每次 dump 进程内生成，不落盘，只上行 PEM） | 发布者 |

因此备份是端到端加密：平台在生成信封后自身也无法解密。

### 2.3 完整性

SR-SIG1 签名已覆盖 `sha256(body)`（整个信封的哈希）——Encrypt-then-MAC，先验签后解密，密文被篡改在验签阶段即失败。分片上传（`import-begin/-chunk/-end`）时每片独立签名，哈希覆盖该片字节；顺序与总量完备性由服务端 seq/total 强制。

### 2.4 压缩

信封内载荷统一为 zstd 压缩流（CLI 与控制台上传均为 zstd-19 高压缩；服务端解包对 zstd/gzip 魔数自适应）。压缩在**加密之前**——先压缩后加密，不泄露熵信息。

---

## 3. 管理员体系

- 标记在**钥匙**上而非账号上：`state/tokens/<user>/<kid>.admin`（空文件存在即管理员钥匙）；
- 引导：首个管理员由主机 CLI `showroomctl user-add <名> --pubkey <…> --admin` 建立；此后管理员可经签名 API 远程发卡与授权；
- 能力分层：

| 能力 | 发布者钥匙 | 管理员钥匙 |
|---|---|---|
| 操作自己归属的展项（发布/停止/删除/回滚/日志/env/db 维护） | ✓ | ✓ |
| `GET /_api/projects` 全平台列表 | ✓（明文含 user 名） | ✓ |
| Redis 浏览（排障） | ✓ | ✓ |
| 发卡 / 吊销 / 收权 / 主机状态面板 | ✗ | ✓ |
| 管理员直发任意项目（`showroomctl publish <user> …`，仅主机 CLI） | — | 仅主机 |

- 管理员钥匙遗失 = 平台最高权限泄露，须按 [SECURITY.md](../SECURITY.md) 处置流程吊销。

---

## 4. 零信息暴露语义

**规则**：`/_api` 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；未认证或验签失败的任何请求——无论路径是否存在、方法是否支持——统一返回 `404 + {"error":"E_AUTH"}`，无 msg，细节只进服务端 journal。

由此推出：

- **路径存在性不泄露**：探测 `/​_api/health` 与探测 `/_api/不存在的` 得到同一响应；
- **版本与项目名不泄露**：`GET /` 与未匹配路径一律 404，不做 302 跳转；
- **错误码语义最小化**：业务错误 `E_NAME/E_QUOTA/E_NOTFOUND/E_STATE/E_NET` 的 msg 不得包含内部信息；`E_AUTH` 恒无 msg；
- **公网控制台**登录成功前不加载任何管理资源，只有登录壳；`/_bootstrap/*`（CLI/Skill 分发）仅内网面提供，公网面 404。

注意：`/_api/projects` 等已认证端点对**任意已登记发布者**返回全平台项目清单（含 user 名）——这是设计取舍（Demo 展示平台的团队内部可见性），不是漏洞；如需强隔离请自行在 Caddy 层收紧。

---

## 5. 威胁与缓解表

| # | 威胁 | 缓解 |
|---|---|---|
| 1 | 公网链路窃听管理请求 | SR-SIG1 只暴露 user 名、时间戳与签名；请求体含机密的端点（import/restore/dump）整体走信封加密 |
| 2 | 窃听展项流量 | 不缓解——展项页面公开，机密不应进 Demo |
| 3 | 重放合法请求 | ±300s 窗口 + per-user nonce 查重 |
| 4 | 伪造/篡改请求 | Ed25519 覆盖方法/路径/查询/请求体哈希；信封为 EtM（签名盖密文） |
| 5 | 离线暴力破解 | Ed25519 / RSA-3072-OAEP-SHA256 / AES-256-GCM 均无已知实际攻击；nonce 不重复保证 GCM 每段密钥流唯一（信封一次性） |
| 6 | 枚举端点/项目名/版本 | 零信息暴露（§4）：未认证一律 404 E_AUTH；项目名默认随机 8 位 |
| 7 | 越权操作他人展项 | user 归属校验（user_check）：签名 user ≠ 项目 user → E_NOTFOUND |
| 8 | 发布者钥匙泄露 | 多钥匙 + 单钥匙吊销（`key-rm`），不影响同 user 其他钥匙 |
| 9 | 管理员钥匙泄露 | 分层授权面小；可用另一管理员钥匙远程吊销；吊销即失效（无令牌缓存） |
| 10 | 平台私钥泄露（历史信封解密） | 轮换平台 RSA 对即可切断；下行备份客户端公钥封装，不受影响 |
| 11 | registry 凭据泄露 | bcrypt（cost 10）入 htpasswd；`user-revoke` 同步使 registry 登录失效；registry 仅经 Caddy `/v2` 暴露 |
| 12 | 恶意镜像攻击平台 | 镜像以受限资源跑（--memory/--cpus、host 端口仅 127.0.0.1）；但展项容器对共享网络内的 sr-postgres/sr-redis 可达——发布者凭证隔离，非网络隔离（已知边界） |
| 13 | SQL 注入 / 命令注入 | db-sql 直达 psql stdin（禁 argv）；redis-keys/get 白名单字符校验先于 docker 调用；展项凭据为独立随机密码 |
| 14 | 大载荷拒绝服务 | JSON body 1MB 帽、import IMPORT_MAX_GB（默认 8GB）帽、db-dump 明文 256MB 帽、分片 seq/total 强制、内存准入（RESERVE_MEM_MB 预留） |
| 15 | 服务端会话残留 | `_tmp` 即用即删 + 24h gc 兜底；dump 会话读完最后一片即删 |
| 16 | 控制台 localStorage 窃取 | P1 试验档，页面明示；不适用于共享浏览器；OPAQUE 类方案属后续演进 |
| 17 | 时钟偏差攻击（拖回 ts） | 窗口绝对值判定；nonce 与 ts 绑定在 canonical 内，篡改即验签失败 |

---

## 6. 密钥与凭据清单

| 密钥 | 位置 | 用途 | 轮换方式 |
|---|---|---|---|
| 发布者 Ed25519 私钥 | `~/.showroom/id_ed25519`（0600） | SR-SIG1 签名 | `keygen --force` + 重新登记 |
| 发布者公钥 | 服务端 `state/tokens/<user>/<kid>.pub` | 验签 | `key-rm` 删旧 / `user-add` 加新 |
| 平台 RSA-3072 | 服务端 `state/platform_rsa{,.pub}`；公钥分发到 `~/.showroom/platform_rsa.pub` | 解开上行信封 | 删文件 + `init-keys` + 重新分发公钥 |
| 一次性 RSA-3072 | dump 时客户端进程内 | 解开下行备份 | 每次 dump 天然轮换 |
| registry 凭据 | `state/users/<user>/reg.cred`（0600 明文）+ htpasswd（bcrypt） | `/v2` 推拉镜像 | `user-revoke` + 重新 `user-add` |
| 展项 DB 凭据 | `state/projects/<project>.env`（0600） | 注入容器 | `db reset` 重发（密码不变，库重建） |
