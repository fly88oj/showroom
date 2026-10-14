# Showroom 安全设计

本文是安全面的**规范文档**：OPAQUE 登录协议、会话认证 SR-SIG1 全文、加密信封字节布局、管理员体系与零信息暴露语义，以及威胁与缓解对照表。部署与应急视角见根目录 [SECURITY.md](../SECURITY.md)。

Showroom 的安全前提：控制面流量可能经过不可信的明文链路（公网隧道、无 TLS 反代），因此不依赖传输层——认证、完整性、机密性全部实现在应用层。

---

## 1. OPAQUE 登录协议（密码引导，SPEC §16）

### 1.1 选型与原理

登录采用 **OPAQUE**（RFC 9380，opaque-ke 3.x，Ristretto255 suite）——增强型 PAKE：

- **密码零上线**：密码先经 argon2id 硬化为 rwd，再经盲化 OPRF 与服务端交互——**任何一步都不把密码或其可验证材料送上网络**；
- **服务端无离线可验证哈希**：服务端只存注册记录信封（`state/users/<u>/pass.opaque`，b64 0600）；拖库后无法离线字典攻击；
- **天然防重放**：每次密钥交换使用全新临时份额，无长期对称凭证可窃取重放。

被否决的替代方案：非 PAKE 的挑战/签名方案（`H(pw, 挑战)` 上网线即可被离线字典）；`HKDF(pw)` 派生对称凭证（可重放的 bearer token）。

实现收口 `showroom-common`（两端同 crate 同 Suite，CLI 与 Web 控制台无互操作缝隙）。argon2id 硬化由 `rwd()` 在盲化前外部承担——参数 **m=19456KiB / t=2 / p=1**（OWASP），盐 = `b"showroom-opaque-v1" + user`；native/wasm 两侧用固定向量测试防漂移。

服务端 OPRF key 存 `state/opaque.key`（b64 0600；serve 启动缺失自动生成）。

### 1.2 报文序列

```
C→S POST /_api/auth/login-begin  {"user":u,"ke1":b64}          # 未签名（唯一免签家族）
S→C 200 {"ke2":b64}   # 未知用户/未设密码 → crate 原生 dummy 假记录路径，仍 200（防枚举）
                      # 密码错误：客户端本地 finish 即失败，不产生第三请求（无线上差异）
C→S POST /_api/auth/login-finish {"user":u,"ke3":b64}          # ke3=客户端 MAC
S→C 200 {"bind_token":hex}   # 60s 有效、单次使用（内存，重启即失效）
C→S POST /_api/auth/key-bind {"user":u,"pubkey":串}
     Authorization: SR-SIG1 …  # 用候选钥匙签名、服务端按 body 内公钥验签（非已登记钥匙）
     X-Sr-Bind: <bind_token>
S→C 200 {"kid","admin"}   # 写 <kid>.pub+.bound；继承语义见 §4.2；GC 旧 .bound 钥匙；幂等
# 改密码（登录态。控制台要求输入当前密码并先本地校验——复用 login-begin+本地 finish、
#   不调 login-finish 即不产生登录态；校验通过才走下列两步。旧密码不上线，签名即会话证明）：
C→S POST /_api/auth/pw-begin  {"user":u,"m1":b64}    [SR-SIG1 常规]
S→C 200 {"m2":b64}                                  # 在途注册态内存保存，60s
C→S POST /_api/auth/pw-commit {"user":u,"record":b64} [SR-SIG1 常规]  # 原子替换记录
# 下线（CLI logout 恒为 / 控制台登出）：
C→S POST /_api/key-selfrm {"kid":k} [SR-SIG1]   # 仅允许删除请求签名所用 kid 自己
# 登录后信任通道下发 registry 凭据（CLI login 免手工 config）：
C→S GET  /_api/reg-cred [SR-SIG1] → {"user","registry_password"}  # 暴露面=已绑定钥匙=账号主人
```

登录成功后，日常全部请求回到 **SR-SIG1 钥匙签名**（§2）——OPAQUE 只负责引导期（密码换钥匙），会话期无密码参与。

### 1.3 认证流安全分析

| 性质 | 实现 |
|---|---|
| 密码零上线 | 盲化 OPRF；rwd（argon2id）在盲化前把每次在线尝试成本抬到 ~19MiB 内存 + t=2 迭代 |
| 防枚举（账号存在性） | 未知用户/未设密码走 dummy 假记录路径，与真实用户响应同构（仍 200） |
| 防在线字典 | 密码错误在客户端本地 finish 即失败，**不产生第三请求**——服务端视角无差别，限流只认 login-begin |
| bind_token 一次性 | 60s 有效、单次使用、纯内存；key-bind 必须同时持有 token 与候选私钥 |
| 防滥用（限流） | login-begin 双令牌桶：按 user 10 次/10 分 + 全局 120 次/10 分（内存）；超限与畸形/族内未知路径一律 404 `{"error":"E_AUTH"}`（与未知路径逐字节一致） |
| 钥匙生命周期 | 密码绑定钥匙（`.bound` 标记）keep-N GC：按 `.pub` mtime 保最近 `BOUND_KEYS_MAX`（默认 5）把；无 `.bound` 的 CI 公钥不受 GC（人工生命周期） |

**风险边界（如实声明）**：

1. **明文 HTTP 首登无信任锚**：PAKE 只保证密码不泄露，主动 MITM 可中继 login-finish 并在 key-bind 登记自己的公钥接管账号。缓解：公网站点 B 经 Caddy 上 TLS；B7 统一账号密码认证后无免密码的预登记公钥旁路（CI/机器用户同样走 `login`），首登须在可信或加密链路上进行。
2. **UnknownUser 假记录的统计残差**：响应长度/时序上存在残差，枚举成本从一次请求升到时序分析。
3. **限流按 user 计**：代理后取不到可信 IP，全局桶 + rwd 计算成本抬高单价。
4. 服务重启丢在途登录态与限流计数（内存）——用户重试即可，攻击者窗口也被重置（接受）。

### 1.4 两把密码分开发放

登录密码（OPAQUE 保护）与 registry 密码（bcrypt，随 `docker push` 以 basic-auth **走线**）不可合并：合并意味着 registry 侧每次推镜像都暴露登录密码的材料，破坏「密码零上线」前提；且二者轮换周期与泄露面独立（registry 密码可能进 CI secret，登录密码不应）。发卡时两把密码一次性分别返回，各自独立补发。

---

## 2. SR-SIG1 请求签名（会话认证）

### 2.1 头格式

```
Authorization: SR-SIG1 <user>:<unix_ts>:<nonce_hex16>:<sig_b64url>
```

| 字段 | 含义 |
|---|---|
| `user` | 发布者账号名 |
| `unix_ts` | 请求时刻的 Unix 秒级时间戳 |
| `nonce_hex16` | 16 个 hex 字符（8 随机字节），每次请求唯一 |
| `sig_b64url` | Ed25519 签名，base64url **无 padding** |

### 2.2 canonical 串

```
canonical = "SR-SIG1\n<user>\n<ts>\n<nonce>\n<METHOD>\n<path_with_query>\n<sha256_hex(body)>"

sig = Ed25519_sign(user_priv, canonical.utf8)
```

- `METHOD` 大写 HTTP 方法；`path_with_query` 为含查询串的完整路径（如 `/_api/logs?project=demo&tail=100`）；
- `body` 为空时取 `sha256_hex("")`；
- 服务端校验四要素：user 已登记、`|now − ts| ≤ 300s`、nonce 未见（per-user 查重，服务重启清零可接受）、验签通过。

### 2.3 防重放与防篡改

- **时间窗口 ±300 秒**：签名即刻过期，客户端与服务端时钟需大致同步（NTP）；
- **nonce 查重**：同一 user 的 nonce 一次有效，窗口内截获重放同一请求会被拒；
- **方法/路径/请求体全覆盖**：URL 改写、参数篡改、body 替换都会破坏签名。

### 2.4 密钥体系

- 发布者密钥：Ed25519，`~/.showroom/id_ed25519`（裸 32B seed，权限 0600；`showroom login` 自动生成绑定）；公钥串格式 `ed25519:<base64(pubkey)>`；
- 一个 user 可登记多把公钥（`state/tokens/<user>/<kid>.pub`），`kid` = 公钥 sha256 前 8 hex；
- 验签时对 user 名下**全部**公钥尝试匹配，命中哪把即以哪把的 `kid` 记录身份（admin 按账号级 `users/<u>/admin` 判定，见 §4）；
- `showroom keygen`（隐藏命令）仅用于排查签名问题或强制换钥（生成后仍需 `showroom login` 绑定登记）；CI/机器用户与日常登录一致走账号密码 `showroom login`（绑定钥匙受 keep-N GC）。

---

## 3. 加密信封 v1（大载荷传输）

import（镜像上行）、db-restore（恢复上行）、db-dump（备份下行）共用同一信封格式：

```
k   = 32B random                       # AES-256-GCM 密钥，一次性
wrap= RSA-3072-OAEP-SHA256(peer_pub, k)   # 定长 384 字节
body= wrap || aes256gcm(k, zstd(payload))
     # AES-GCM 流式分段：每段 [12B nonce][密文||16B tag]，段大小 4MiB 明文
```

### 3.1 字节布局

```text
偏移 0            384                 384+14 ...
┌──────────────────┬──────────────────┬─────────────────────┬─ ─ ─ ─ ─ ┐
│ RSA-OAEP 包裹的 k │ 段1 nonce(12B)   │ 段1 密文‖tag(16B)    │ 段2 …    │
└──────────────────┴──────────────────┴─────────────────────┴─ ─ ─ ─ ─ ┘
   定长 384B            └────────── 每段：12B nonce + 密文 + 16B tag ──────┘
```

总长度不变量：`384 + n + ceil(n / 4MiB) × (12 + 16)`（n 为压缩后明文字节数）。

### 3.2 密钥协商方向

| 通道 | 封装公钥 | 持私钥方 |
|---|---|---|
| 上行（import / db-restore） | 平台 RSA-3072 公钥（`~/.showroom/platform_rsa.pub`，对应服务端 `state/platform_rsa{.pub}`） | 平台 |
| 下行（db-dump） | 客户端**一次性** RSA-3072 公钥（每次 dump 进程内生成，不落盘，只上行 PEM） | 发布者 |

因此备份是端到端加密：平台在生成信封后自身也无法解密。

### 3.3 完整性

SR-SIG1 签名已覆盖 `sha256(body)`（整个信封的哈希）——Encrypt-then-MAC，先验签后解密，密文被篡改在验签阶段即失败。分片上传（`import-begin/-chunk/-end`）时每片独立签名，哈希覆盖该片字节；顺序与总量完备性由服务端 seq/total 强制。

### 3.4 压缩

信封内载荷统一为 zstd 压缩流（CLI 与控制台上传均为 zstd-19 高压缩；服务端解包对 zstd/gzip 魔数自适应）。压缩在**加密之前**——先压缩后加密，不泄露熵信息。

---

## 4. 管理员体系

### 4.1 标记与能力分层（账号级，R4 裁决）

- 标记在**账号**上：`state/users/<user>/admin`（空文件存在即管理员账号）是**唯一真相**；旧钥匙级 `tokens/<user>/<kid>.admin` 与 `users/<user>/admin.pending` 仅作存量迁移输入（serve 启动时一次性迁移清除）；
- 引导：首个管理员由主机 CLI `showroomctl user-add <名> --gen-password --admin` 建立（`--admin` 即刻落账号级标记；统一账号密码认证，B7）；此后管理员可经签名 API 远程发卡与授权；

| 能力 | 发布者账号 | 管理员账号 |
|---|---|---|
| 操作自己归属的项目（发布/停止/删除/回滚/日志/env/db 维护） | ✓ | ✓ |
| `GET /_api/projects` 项目列表 | 仅本人项目 | 仅本人项目（跨用户全量走 `GET /_api/admin/projects`，每行附 owner） |
| 镜像列表 `/_api/images` | 仅自己命名空间 | 全量 |
| Redis 浏览 | 仅自己项目的逻辑库（协作级隔离） | 全量（下拉显示项目名，未分配库占位） |
| 发卡 / 重置密码 / 吊销 / 收权 / 主机状态与编排面板 | ✗ | ✓ |
| 管理员直发任意项目（`showroomctl publish <user> …`，仅主机 CLI） | — | 仅主机 |

- 授权/收权：`showroomctl user-admin <USER> --admin true|false` / `POST /_api/admin/user-admin {user,admin}`（账号级，对该账号**全部钥匙**生效；旧钥匙级端点已移除）；
- 管理员账号凭据泄露 = 平台最高权限泄露，须按 [SECURITY.md](../SECURITY.md) 处置流程吊销；删单把钥匙（`key-rm`/`key-selfrm`）**不影响**账号的管理员身份。

### 4.2 whoami 与登录绑定的 admin 回传

`key-bind`（密码登录绑定新钥匙）与 `GET /_api/whoami` 回传的 `admin` 均为**账号级结论**（读 `users/<u>/admin`，不缓存）：管理员账号绑定的任何新钥匙登录即具管理权限，控制台据此显隐「管理」标签。吊销粒度：`user-key-rm` 删单把钥匙；`user-revoke` 连 `users` 目录整删（pass.opaque、registry 凭据一并失效）。

---

## 5. 零信息暴露语义

**规则**：`/_api` 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；未认证或验签失败的任何请求——无论路径是否存在、方法是否支持——统一返回 `404 + {"error":"E_AUTH"}`，无 msg，细节只进服务端 journal。**唯一例外**是 `/_api/auth/*` 免签家族（§1.2 的 login-begin/login-finish/key-bind）：畸形、限流、ke3 拒绝、族内未知路径同样一律 404 `{"error":"E_AUTH"}`，与未知路径逐字节一致。

由此推出：

- **路径存在性不泄露**：探测 `/_api/health` 与探测 `/_api/不存在的` 得到同一响应；
- **版本与项目名不泄露**：`GET /` 与未匹配路径一律 404，不做 302 跳转；
- **错误码语义最小化**：业务错误 `E_NAME/E_QUOTA/E_NOTFOUND/E_STATE/E_NET` 的 msg 不得包含内部信息；`E_AUTH` 恒无 msg；
- **公网控制台**登录成功前不加载任何管理资源，只有登录壳；CLI/Skill/平台公钥经 `/_api/tool/*`（签名）登录后控制台内下载——`_bootstrap/` 目录仅为工具源，**无静态匿名分发面**（R8 裁决）。

注意：`/_api/projects` 只返回本人项目（管理员经 `/_api/admin/projects` 取跨用户全量）——跨用户项目名/账号名不对普通登录者枚举。Redis 浏览的按用户隔离（请求 db ∈ 本人项目 redisdb 集合）是**界面级/协作级**隔离（防误看防误操作），非安全边界——容器内应用持有原始连接仍可 SELECT 任意库。

---

## 6. 威胁与缓解表

| # | 威胁 | 缓解 |
|---|---|---|
| 1 | 公网链路窃听管理请求 | SR-SIG1 只暴露 user 名、时间戳与签名；请求体含机密的端点（import/restore/dump）整体走信封加密 |
| 2 | 窃听展项流量 | 不缓解——展项页面公开，机密不应进 Demo |
| 3 | 重放合法请求 | ±300s 窗口 + per-user nonce 查重；OPAQUE 每次 KE 全新临时份额 |
| 4 | 伪造/篡改请求 | Ed25519 覆盖方法/路径/查询/请求体哈希；信封为 EtM（签名盖密文） |
| 5 | 离线暴力破解 | Ed25519 / RSA-3072-OAEP-SHA256 / AES-256-GCM 均无已知实际攻击；nonce 不重复保证 GCM 每段密钥流唯一（信封一次性）；**密码侧**：OPAQUE 服务端只存注册信封，无离线可验证哈希；rwd argon2id（19MiB/t=2）抬高每次在线尝试成本 |
| 6 | 枚举端点/项目名/版本/账号 | 零信息暴露（§5）：未认证一律 404 E_AUTH；项目名默认随机 8 位；登录防枚举：未知用户 dummy 记录同构响应（残差见 §1.3） |
| 7 | 越权操作他人展项 | user 归属校验（user_check）：签名 user ≠ 项目 user → E_NOTFOUND；镜像删除 `<user>/` 前缀归属校验 |
| 8 | 发布者钥匙泄露 | 多钥匙 + 单钥匙吊销（`key-rm`/`key-selfrm`），不影响同 user 其他钥匙；keep-N GC 自动收尾遗忘设备 |
| 9 | 管理员账号凭据泄露 | 分层授权面小；可用另一管理员账号远程吊销/收权；吊销即失效（无令牌缓存）；删单把钥匙不影响账号级判定，须 `user-admin --admin false` 或 `user-revoke` |
| 10 | 平台私钥泄露（历史信封解密） | 轮换平台 RSA 对即可切断；下行备份客户端公钥封装，不受影响 |
| 11 | registry 凭据泄露 | bcrypt（cost 10）入 htpasswd；`user-revoke` 同步使 registry 登录失效；registry 仅经 Caddy `/v2` 暴露（内网面）；与登录密码独立发放、独立轮换（§1.4） |
| 12 | 恶意镜像攻击平台 | 镜像以受限资源跑（--memory/--cpus、host 端口仅 127.0.0.1）；但展项容器对共享网络内的 sr-postgres/sr-redis 可达——发布者凭证隔离，非网络隔离（已知边界） |
| 13 | SQL 注入 / 命令注入 | db-sql 直达 psql stdin（禁 argv）；redis-keys/get 白名单字符校验先于 docker 调用；展项凭据为独立随机密码 |
| 14 | 大载荷拒绝服务 | JSON body 1MB 帽、import IMPORT_MAX_GB（默认 8GB）帽 + 磁盘准入（膨胀因子×密封体积 ≤ 可用−预留）、db-dump 明文 256MB 帽、分片 seq/total 强制、内存准入（RESERVE_MEM_MB 预留） |
| 15 | 服务端会话残留 | `_tmp` 即用即删 + 24h gc 兜底；dump 会话读完最后一片即删 |
| 16 | 控制台 localStorage 种子窃取 | P1 试验档，页面明示；不适用于共享浏览器。登录已走 OPAQUE（密码永存 localStorage 之外）；种子即 Ed25519 钥匙，`logout` 恒为远程注销 |
| 17 | 时钟偏差攻击（拖回 ts） | 窗口绝对值判定；nonce 与 ts 绑定在 canonical 内，篡改即验签失败 |
| 18 | 明文 HTTP 首登 MITM（接管账号） | 已知边界（§1.3 风险 1）：PAKE 不提供信任锚，主动 MITM 可中继 login-finish 并登记自己的公钥；缓解：公网面经 Caddy TLS、LAN 不可信走 CLI 钥匙路径；bind_token 60s 单次缩小窗口 |
| 19 | 登录端点在线滥用 | login-begin 双令牌桶（user 10 次/10 分 + 全局 120 次/10 分）；超限 404 与未知路径逐字节一致；rwd 计算成本抬高单价 |

---

## 7. 密钥与凭据清单

| 密钥 | 位置 | 用途 | 轮换方式 |
|---|---|---|---|
| 发布者 Ed25519 私钥 | `~/.showroom/id_ed25519`（0600） | SR-SIG1 签名 | 重新 `showroom login` 落新钥匙自动重绑（CI/机器用户同款） |
| 发布者公钥 | 服务端 `state/tokens/<user>/<kid>.pub` | 验签 | `key-rm` 删旧 / `user-add` 加新；`.bound` 钥匙 keep-N GC |
| 服务端 OPRF key | `state/opaque.key`（0600） | OPAQUE 登录 | 删除后重启 serve 自动重生成（**会使全部登录密码失效**，需全员补发——仅灾备用） |
| OPAQUE 注册记录 | `state/users/<u>/pass.opaque`（0600） | 校验登录密码（无离线可验证哈希） | `user-passwd` / `passwd-set` 补发（不动既有钥匙） |
| bind_token | 服务端内存（60s 单次） | 登录后绑定新钥匙的一次性凭证 | 天然一次性 |
| 平台 RSA-3072 | 服务端 `state/platform_rsa{,.pub}`；公钥分发到 `~/.showroom/platform_rsa.pub` | 解开上行信封 | 删文件 + `init-keys` + 重新分发公钥 |
| 一次性 RSA-3072 | dump 时客户端进程内 | 解开下行备份 | 每次 dump 天然轮换 |
| registry 凭据 | `state/users/<user>/reg.cred`（0600 明文）+ htpasswd（bcrypt） | `/v2` 推拉镜像 | `user-revoke` + 重新 `user-add`；发布者重 `login` 自动拉新 |
| 展项 DB 凭据 | `state/projects/<project>.env`（0600） | 注入容器 | `db reset` 重发（密码不变，库重建） |
