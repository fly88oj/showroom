# Showroom 架构

本文描述组件职责、关键数据流与设计决策。协议细节见 [security.md](security.md)，内部契约原文见仓库根 [SPEC.md](../SPEC.md)（协议演进史）。

## 1. 组件与职责

```text
┌─────────────────────────── your-host（Linux + Docker + btrfs）──────────────────────────┐
│                                                                                          │
│   Caddy（静态面 + 反代，setup 渲染）                                                       │
│   ├─ site A（:80，内网面 LAN_HOST）                                                        │
│   │   ├─ /_api/*        → showroomctl serve（127.0.0.1:8090）                             │
│   │   ├─ /v2            → sr-registry（127.0.0.1:5000，basic auth，registry ∈ 集合才有）  │
│   │   ├─ /_bootstrap/*  → CLI/Skill 分发（仅内网面）                                       │
│   │   └─ /<project>/*   → 展项容器（127.0.0.1:8100–8199，逐项目端口）                      │
│   ├─ site B（:8080 纯端口匹配，公网面——frp 等隧道流量命中的站点；无 /v2 与 /_bootstrap）   │
│   │   公网隧道（frpc → 127.0.0.1:8080）──┐                                                │
│   │                                       ▼                                               │
│   │   └─ /_api/* /_console/* /<project>/*（与 site A 同 import showroom-app 片段）        │
│   └─ /_console/*        → /srv/showroom/_console（WASM 控制台静态资源）                    │
│                                                                                          │
│   showroomctl serve（控制面）── docker CLI/exec ──▶ 展项容器 · sr-postgres · sr-redis     │
│        │  状态读写                                                                        │
│        ▼                                                                                 │
│   /srv/showroom（btrfs 子卷 @showroom）                                                  │
│   ├─ state/（policy.env · host.env · ports.reg · tokens/ · users/ · projects/ · routes/）│
│   ├─ _middleware/{postgres,redis,registry}/（中间件数据卷）                              │
│   └─ _tmp/（import/dump 暂存，即用即删 + 24h gc）                                        │
│                                                                                          │
│   systemd timers → showroomctl reconcile / autoheal / gc-images / gc-registry /          │
│                    registry-gc / snapshot hourly|daily                                   │
└──────────────────────────────────────────────────────────────────────────────────────────┘
        ▲ SR-SIG1 签名 + 加密信封（明文链路安全）；OPAQUE 登录（密码零上线）
        │
   ┌────┴────────────────────────────┐
   │ showroom（发布者 CLI）           │ showroom-web（WASM 控制台，浏览器内跑同一套密码学）
   │ docker save|zstd|seal → 上传    │ File.slice 流式 seal → 分片上传
   └─────────────────────────────────┘
```

| crate | 产物 | 职责 |
|---|---|---|
| `showroom-common` | rlib + **wasm32 cdylib** | SPEC §1/§2/§16 的契约实现：SR-SIG1 签名/验签、加密信封 seal/open、OPAQUE 登录双端包装（Suite/argon2id rwd）、命名校验、一次性 RSA 密钥对。原生侧被 ctl/cli 复用；wasm 垫片（C ABI，不用 wasm-bindgen）给浏览器控制台——**CLI 与 Web 走同一 crate 同一 Suite，无互操作缝隙**。密码学依赖（ed25519-dalek / rsa / aes-gcm / opaque-ke / argon2）锁定在本 crate 并重导出，避免下游类型分裂 |
| `showroom` | 发布者 CLI（`showroom`） | 登录/登出（`login`/`logout`，密码一次密钥常驻）、配置管理（`~/.showroom/`）、push 双通道、publish/deploy、查询与生命周期命令、db 维护子命令、镜像与磁盘（images/space/rm-image）、doctor 体检。除 `push` 需要 docker CLI 外零运行时依赖 |
| `showroomctl` | 主机工具（`showroomctl`） | HTTP 控制面（`serve`，仅监听 127.0.0.1:8090）+ 全部运维子命令：user 管理（含密码发放）、发布内核、生命周期、reconcile/autoheal/gc、快照、导入、主机自举（`setup`/`middleware`/`upgrade`）。状态存 `/srv/showroom`，全部外部世界调用集中在 `dockerops`（docker CLI / psql / caddy / 裸 TCP registry 客户端） |
| `showroom-web` | 静态资源（`dist/`） | 控制台单页：`index.html` + `showroom_bg.wasm`（由 showroom-common cdylib 构建产物改名）。一步登录（账号+密码，OPAQUE）、展项/数据库/状态/帮助四区、管理标签（admin 钥匙可见：账号/钥匙/主机编排/Redis 扩容）。登录页仅账号+密码；钥匙登录（CLI/机器用户）收进帮助页折叠（专家路径）。产物入库（部署通道依赖静态文件），与垫片源码同一提交内重建 |

内部深模块（showroomctl）：`state`（状态读写+纯函数：命名/policy/conf/端口/redisdb 分配/caddy 片段）、`dockerops`（外部世界）、`publish`（publish/import 内核，serve 与 CLI 子命令共用）、`ops`（生命周期、user/key 管理、key-bind 继承与 GC）、`serve`（axum 统一 fallback 分发、`/_api/auth/*` 免签族与限流）、`dbmaint`（db-sql/dump 三段式）、`streams`（ndjson 实时流）、`setup`（声明式期望状态引擎与 Action 体系）、`middleware`（host.env 契约与编排/扩容状态机）、`unpack`（信封解包）。

## 2. 数据流

### 2.0 认证流（OPAQUE 登录，SPEC §16）

密码一次、密钥常驻（gh/ssh 模型）；浏览器与 CLI 走 showroom-common 同一客户端：

```text
C→S POST /_api/auth/login-begin  {"user":u,"ke1":b64}            # 未签名（唯一免签家族）
S→C 200 {"ke2":b64}     # 未知用户/未设密码 → 原生 dummy 假记录路径，仍 200（防枚举）
C  本地 finish：密码错误在此即失败（无第三请求，无线上差异）
C→S POST /_api/auth/login-finish {"user":u,"ke3":b64}            # ke3=客户端 MAC
S→C 200 {"bind_token":hex}      # 60s 有效、单次使用（内存）
C→S POST /_api/auth/key-bind {"user":u,"pubkey":串}
     Authorization: SR-SIG1 …   # 用候选钥匙签名（服务端按 body 内公钥验签）
     X-Sr-Bind: <bind_token>
S→C 200 {"kid","admin"}         # 写 <kid>.pub+.bound；账号已有任一 admin 钥匙或
                                # admin.pending → 新钥匙继承 admin；GC 旧 .bound 钥匙
C→S GET /_api/reg-cred [SR-SIG1] → registry 凭据（login 免手工 config）
```

- 密码经盲化 OPRF **永不上线**；服务端只存注册记录信封（`state/users/<u>/pass.opaque`），无离线可验证哈希；
- `/_api/auth/*` 是唯一应答未签名请求的端点族；畸形/限流/族内未知路径一律 404 `{"error":"E_AUTH"}`；login-begin 双令牌桶（账号 10 次/10 分 + 全局 120 次/10 分）；
- 改密码（控制台）：先输入**当前密码本地校验**（复用 login-begin+本地 finish，不调 login-finish、不产生登录态），通过后才走 `pw-begin`/`pw-commit` 两段（SR-SIG1 常规签名，旧密码不上线——签名即会话证明）；自助下线 `/_api/key-selfrm` 仅能删当前签名所用 kid。

### 2.1 发布双通道（push）与三入口（SPEC §13.1）

CLI 自动探测内网可达性：优先 lan，探不通回落 public。控制台无 lan 概念，恒走公网加密通道。**发布入口三存（一键不取代分步）**：

```text
入口 1「一键部署」：tar → 压缩 → 分片上传 → 立即 publish → 展示 URL
        CLI：showroom deploy（push auto + publish --wait + URL 横幅，任一步失败即停）
        控制台：状态条（压缩中/上传 x%/发布中/完成），断点续传
入口 2「仅上传」：只入 registry 不部署（发布者择时部署）
入口 3「部署」：独立表单——镜像 ComboBox（datalist，GET /_api/images 按签名用户过滤；
        admin 全量）选中已有镜像 → 填内存需求/端口 → publish（旧版本单独部署从此入口）
```

```text
通道 A：registry 快通道（lan 可达时）
  docker tag demo:v1 <your-host>/<user>/demo:v1
  docker login <your-host>（basic auth ← user 的 registry 凭据）
  docker push ──▶ Caddy /v2 ──▶ sr-registry(127.0.0.1:5000)
  showroom publish demo v1 ──▶ POST /_api/publish（SR-SIG1）

通道 B：import 加密通道（公网/lan 不可达；控制台同通道）
  docker save demo:v1 │ zstd -19 │ seal(平台RSA公钥)   # CLI：流式管道，临时文件
                     │（控制台：File.slice 8MB/片，wasm 会话式 seal，断点续传）
  POST /_api/import-begin {project,tag,image_id,total_bytes} → {upload_id}
  POST /_api/import-chunk?upload=<id>&seq=<n>  body=信封字节切片   # 每片独立 SR-SIG1
  GET  /_api/import-status?upload=<id>          # 断点续传查询 received/total
  POST /_api/import-end?upload=<id>             # 拼接 → 走单发 import 同一管道
                                                #   解密→docker load→docker tag
                                                #   →push 127.0.0.1:5000→删 _tmp
  （CLI 单发：POST /_api/import?project=&tag=，body=完整信封）
```

publish 内核（三条入口汇合后）：校验名/配额 → registry 镜像存在 → 内存准入（MemAvailable − RESERVE_MEM_MB ≥ 需求）→ 磁盘准入复核（registry 快通道无 begin 钩子，拉取展开发生在部署时刻，§14.2）→ 分配 host 端口（8100–8199）→ 供给 DB/Redis（**首建不重建**，数据保留）→ 起容器（滚动：`-new` 后缀起新、健康后删旧改名；健康 = TCP 可连或 60s 超时放行）→ 写路由片段 + `caddy reload` → 更新 last2。

磁盘准入在两条上传路径上的位置：加密 import 通道在 `import-begin` 判定 `密封体积 × UPLOAD_SPACE_FACTOR（默认 4）≤ 可用 − DISK_RESERVE_GB 预留（默认 20G）`（因子覆盖解密临时件+解压膨胀+registry 压缩副本）；registry 直推快通道在 publish 时同式复核。不足 → `E_QUOTA`，消息含 可用/预留/估算需求，提示先删旧镜像（`showroom rm-image` 删除即内联 registry-gc 立即释放，闭环「上传不够→删除→立即可传」）。`GET /_api/space` 返回同一口径三数（可用已扣预留）。

### 2.2 备份三段式（db dump/restore）

大体积下行不整包进内存（服务端内存恒定 ~4MiB）：

```text
下行（dump，端到端加密——平台只见一次性公钥）：
  客户端：进程内生成一次性 RSA-3072 对
  POST /_api/db-dump-begin {project, client_pub_pem} → {dump_id}
        服务端：后台线程 pg_dump -Fc 直连 seal(客户端公钥) 流式写 _tmp/<id>.bin
        （明文 >256MB → E_QUOTA；进度 _tmp/<id>.status：running|ready|failed）
  GET /_api/db-dump-status ?dump=<id>        # 1s 轮询至 ready（CLI 上限 1h）
  GET /_api/db-dump-chunk ?dump=&offset=&len=   # ≤4MB/片，octet-stream + X-Sr-Total
        # 读完最后一片即删会话三件套；24h gc 兜底
  客户端：临时信封落盘 → 一次性私钥解密流式写目标（zstd-19 包裹的 pg_dump -Fc）

上行（restore）：
  客户端：备份文件（zstd-19 包裹或原生 -Fc，服务端魔数自适应）│ seal(平台公钥)
  POST /_api/db-restore ?project=&confirm=true body=信封
        服务端：验签→解密到 _tmp→pg_restore --clean --if-exists→删临时
```

### 2.3 实时流（ndjson，非 WebSocket）

- `GET /_api/logs-stream ?project&tail`（默认 100，上限 10000）：`docker logs -f` 桥接为 `application/x-ndjson`，每行 `{"t":<unix_ms>,"l":"<日志行>"}`；客户端断开即 kill 子进程收尸，不留孤儿；
- `GET /_api/events`：`docker events`（start/stop/die/health_status）封装为 `{"t","e","actor"}`；每 30s 一行 `{"hb":true}` 心跳防代理空闲断连。

### 2.4 自愈循环

```text
systemd timer（每 1h）→ showroomctl reconcile：遍历 projects/*.conf，容器/路由缺失按 conf 幂等重建
systemd timer（每 5min）→ showroomctl autoheal：容器缺失重建 / 非预期退出拉起 / unhealthy 重启
                                            （conf 标记 stopped=1 的跳过）
docker --restart unless-stopped：进程级兜底
```

### 2.5 主机状态布局

```text
/srv/showroom                      # btrfs 子卷 @showroom；SHOWROOM_ROOT 可覆盖
├─ state/
│  ├─ policy.env                   # 业务限额：MAX_SLOTS（键名沿用，语义=项目数上限）DEFAULT_MEM
│  │                               #   MAX_MEM IMPORT_MAX_GB ALLOW_LAN_PORT RESERVE_MEM_MB
│  │                               #   REDIS_DBS DISK_RESERVE_GB UPLOAD_SPACE_FACTOR BOUND_KEYS_MAX
│  ├─ host.env                     # 设施形状（setup 生成/幂等应用，0600）：MIDDLEWARE CADDY
│  │                               #   LAN_HOST PUBLIC_HOST TUNING DEDUP FRP FRP_TOKEN FRP_TUNNEL
│  ├─ ports.reg                    # 每行 <port> <project>
│  ├─ opaque.key                   # 服务端 OPRF key（b64 0600；serve 启动缺失自动生成）
│  ├─ platform_rsa{,.pub}          # 平台 RSA-3072 对（解上行信封）
│  ├─ registry.htpasswd            # registry basic auth（bcrypt）
│  ├─ tokens/<user>/<kid>.pub     # Ed25519 公钥（可多把）
│  ├─ tokens/<user>/<kid>.admin   # 空文件 = 管理员钥匙（标记在钥匙上）
│  ├─ tokens/<user>/<kid>.bound   # 空文件 = 密码绑定钥匙（受 keep-N GC；CI 公钥无此标记不受 GC）
│  ├─ users/<user>/pass.opaque    # OPAQUE 注册记录（b64 0600）
│  ├─ users/<user>/admin.pending  # 空文件 = 纯密码发卡的管理员意图（首把绑定钥匙继承后清除）
│  ├─ users/<user>/reg.cred      # registry basic_auth（0600 明文）
│  ├─ projects/<project>.conf      # key=value：owner tag port cport mem cpu redisdb updated
│  ├─ projects/<project>.env       # 注入容器的连接信息（§4 契约）
│  ├─ projects/<project>.last2     # 上一 tag（回滚目标）
│  └─ routes/<project>.caddy       # 展项路由片段（主 Caddyfile import）
├─ _middleware/{postgres,redis,registry}/   # 中间件容器数据卷（bind）
├─ _bootstrap/                     # CLI/Skill 分发（仅内网面）
├─ _console/                       # WASM 控制台静态资源
└─ _tmp/                           # import/dump 暂存（即用即删 + 24h gc）
```

`/<project>/`（host 端口 127.0.0.1:8100–8199 反代）→ 展项容器；容器名 `sr-<project>`；库名 `sr_<project 的 - → _>`。

**host.env 与 policy.env 分层**（SPEC §15.1 勘误）：限额（REDIS_DBS/DISK_RESERVE_GB/UPLOAD_SPACE_FACTOR/RESERVE_MEM_MB）归 policy.env——管理员日常调节项；设施形状（装什么中间件、开什么开关）归 host.env——setup 幂等收敛项。两类键变更主体与风险等级不同，分层不合并以免双写漂移；首跑限额缺省随实测硬件推导（hw_defaults），用户显式值永远优先。

### 2.6 主机自举（setup 与 Action 体系，SPEC §15）

`showroomctl setup` 是**声明式期望状态引擎**：读 host.env（首跑缺省时按探测值生成）→ 观测主机实际状态 → 产出 Action 列表 → diff 模式只打印，`--apply` 逐个执行。幂等收敛：二次运行期望 `plan: 0 actions needed (clean)`。

```text
期望状态（host.env + policy.env）
   │ 观察（目录/文件/容器/单元/包/二进制 实况快照 Obs）
   ▼
diff = 期望 − 实际 → Action 列表（有序、可命名，幂等判定为纯函数逐臂比对）
   CreateDir / WriteFile（Caddyfile·policy·单元；registry.htpasswd 为 touch 语义绝不覆盖活账本）
   BtrfsSubvol（顺手场景）/ NetworkCreate（showroom-net）
   ContainerUp（pg 12 参数·redis --databases=policy·registry 全旗标）/ ContainerRm
   UnitInstall / UnitEnable（api·7 timers·dedup·chmlfrp）/ UnitDisableRm
   PackageInstall（caddy）/ Cmd（sysctl·journald·watchdog·caddy validate/reload）
   FrpcFetch（下载后 file 校验 ELF，非 ELF 报错引导手工放置）
   ── setup --apply 执行；middleware enable/disable → 仅相关 Action 局部 apply
```

配套：`showroomctl middleware status|enable|disable|set` 与 Web 管理面板的 `GET/POST /_api/admin/host` 同源——POST 为**两段式**（无 confirm 返回变更清单与影响面，`confirm=true` 才落盘并自动 apply）；Redis 逻辑库扩容（databases 变更需重启实例）走独立确认流，不静默重启。`showroomctl upgrade` 就地升级二进制（探针→备份→原子替换→restart 探活→失败回滚）。现网 redis 实例 `--databases` 低于 policy 时不重建（数据安全优先），只报黄条引导走扩容流。

## 3. 关键设计决策

| # | 决策 | 理由（决策记录） | 代价 / 已知天花板 |
|---|---|---|---|
| D1 | **明文链路上做应用层加密，不把 TLS 当前提**（SR-SIG1 + 信封） | 目标环境常只有明文 HTTP 可用（公网隧道转发、无证书的内网面）；认证与机密性必须不依赖部署者的 TLS 配置。信封 EtM：签名盖密文哈希 | 展项页面本身仍明文公开（不缓解，见威胁表 #2）；有 TLS 的部署可获得额外一层 |
| D2 | **不用 WebSocket，统一 ndjson 流（SSE 风格，fetch 流式消费）** | 控制台上行全是短请求，无需双向通道；少一条长连接状态机与升级路径（SPEC §10.4 方向性决策） | 事件流靠 30s 心跳防代理空闲断连 |
| D3 | **移除槽位（slot）模型**（v1.4） | 简化概念——Demo 定向展示不需要 stable/next/pr42 并存；多版本并存 = 发布者自建多个相近名称项目；项目名默认随机 8 位提升字典遍历门槛（SPEC §11） | 同项目多环境需多个项目名承载；存量多槽位数据由 reconcile 幂等迁移（最新 updated 者上提） |
| D4 | **加密信封 = RSA-OAEP 包裹 + AES-GCM 分段流式**，而非整包 RSA/TLS 风格握手 | 镜像 tar 可达数 GB，必须流式（内存恒定 ~4MiB）；一次性对称密钥 + 定长 384B 头，无握手状态 | 一次成包才能解（生成整体单次、过程流式）；dump 下行因此也要三段式拉取 |
| D5 | **db-dump 旧单发 base64 直接移除，不做兼容** | 未发布给外部、无存量消费者；整包进内存有 4× 放大问题（SPEC §10.4） | — |
| D6 | **WASM 垫片不用 wasm-bindgen/wasm-pack** | 零新工具链：`rustup target add wasm32-unknown-unknown` 即可构建；JS 侧直接 instantiate 调 C ABI（指针+长度进出） | ABI 手工维护（容量不足返回负数所需字节的约定） |
| D7 | **零信息暴露：未认证一律 404 E_AUTH** | 管理面不给枚举素材：路径存在性、项目名、版本全部不可探测（SPEC §3） | 404 同时是"没这路径"与"没权限"，排障要靠服务端 journal；本机健康检查用 `systemctl is-active` 而非 HTTP health |
| D8 | **管理员标记在钥匙上（kid.admin）而非账号上** | 一人可同时持普通与管理员钥匙；`whoami` 按当前请求命中的 kid 判定，吊销粒度细到单钥匙（SPEC §12.2） | 管理员钥匙遗失 = 平台最高权限泄露；纯密码发卡的管理员意图经 `admin.pending` 由**首把登录绑定钥匙继承**（一次性标记，消费即清；此后账号内既有 admin 钥匙持续让新钥匙继承——密码即账号级最高凭证） |
| D9 | **publish 首建不重建数据库**（v1.2 起） | 发布者重新 publish 不应意外清库；显式重置走 `db reset`（DROP+重建+凭据不变） | 迁移 schema 是发布者自己的责任（db sql/restore 工具齐备） |
| D10 | **nonce 查重服务重启清零** | 窗口仅 ±300s，重启后旧 nonce 早已过期；避免持久化（SPEC §1 明示可接受） | — |
| D11 | **registry v2 客户端用裸 TCP HTTP/1.0 手写** | 控制面对 localhost 单一 registry，引第三方 async HTTP 栈不划算（ctl 内部约定） | 仅覆盖本机 registry 场景 |
| D12 | **统一 zstd 压缩（CLI 与控制台上传、dump）** | 压缩在加密之前，先压缩后加密不泄露熵；zstd-19 高压缩；服务端魔数自适应兼容原生 -Fc | — |
| D13 | **滚动更新 = `-new` 后缀起新再删旧改名** | 同项目唯一 host 端口约束下的简单滚动 | 秒级中断（已知天花板，代码内 ponytail 注释同步） |
| D14 | **`/_api/projects` 对任意登记发布者全量可见** | 团队内部可见性是 Demo 平台的合理默认 | 非强多租户隔离；强隔离需求在反代层收紧 |
| D15 | **git 历史定稿窗口允许重塑** | v1 未定稿前保持重塑自由（详见 CONTRIBUTING「历史重塑政策」） | 定稿后禁止 force-push；重塑需公告并评估凭据轮换 |
| D16 | **认证以密码为主、OPAQUE 落地（v1.9），公钥登记降级为专家路径** | 目标体验是一步登录（账号+密码）与 CLI `login` 一次免密（gh/ssh 模型），密钥生成/绑定全程无感。选 OPAQUE（RFC 9380）而非其他密码方案：密码经盲化 OPRF **永不上线**、服务端只存注册信封**无离线可验证哈希**、每次 KE 全新临时份额天然防重放——非 PAKE 的挑战/签名方案（H(pw,挑战) 上网线即可离线字典）与 HKDF(pw) 对称凭证（可重放 bearer）均被否决。argon2id 由 rwd() 在盲化前硬化；两端同 crate 同 Suite 无互操作缝隙 | 明文 HTTP 首登无信任锚——主动 MITM 可中继 login-finish 并登记自己的公钥（缓解：公网站点 B 经 Caddy TLS、LAN 不可信时走 CLI 钥匙路径）；UnknownUser 假记录存在时序统计残差；两把密码（登录/registry）必须分开发放，不能合并 |
| D17 | **为什么 OPAQUE 之外仍保留 SR-SIG1 钥匙签名做日常请求** | OPAQUE 解决「密码不泄露」的**登录引导**问题；登录后的每请求认证用已绑定的 Ed25519 钥匙签名（无密码参与、可离线验签、多设备多钥匙独立吊销）。两者是引导期与会话期的组合，不是替代关系 | 密码只出现在登录三步内；日常流量对密码零依赖 |
| D18 | **site B（公网面）用 :8080 纯端口匹配，不做 Host 匹配** | frp 隧道转发来的请求 Host 是公网 IP/隧道域名，按 `PUBLIC_HOST` 做 Host 匹配会漏掉全部 frp 流量（断电重启后实弹复现）；纯端口匹配保证隧道流量恒命中。frpc 转发目标恒 127.0.0.1:8080，PUBLIC_HOST 降为元数据（是否渲染 site B 的开关 + 展示用） | :8080 与 site A 的 :80 是两个监听，公网直连 80 端口不会命中公网面 |
| D19 | **host.env（设施形状）与 policy.env（业务限额）分层不合并** | 两类键的变更主体与风险等级不同：限额=管理员日常调节，设施=setup 幂等收敛；合并会造成双写漂移、setup diff 混入限额噪声（SPEC §15.1 勘误） | 改限额不触发 setup；改形状必须走 setup --apply |
| D20 | **Redis 逻辑库池 256（`--databases 256`），每项目一库；浏览视图按用户隔离** | 逻辑库仅命名空间无内存代价；每项目一库使客户端 URL 零配合、项目间不串键；用户隔离只作用于浏览视图（请求 db ∈ 本人项目 redisdb 集合，admin 全量）——诚实边界：容器内应用持原始连接仍可 SELECT 任意库，这是界面级/协作级隔离非安全边界 | 强隔离需独实例或 Redis ACL+rewrite，成本过高不做；池耗尽 E_QUOTA 附扩容命令建议 |
| D21 | **镜像删除内联触发 registry-gc（停-收-启，秒级）** | 删除即释放空间（不等每周 GC），闭环「上传 E_QUOTA→删除→立即可传」；live-restore 保护运行中容器；共享 blob 由 --delete-untagged 天然保留仍被引用者 | 每次删除有一次秒级 registry 重启窗口 |

## 4. 环境变量契约（publish 自动注入）

容器以 `--env-file` 注入以下变量，值全部具体化；应用按 12-factor 读环境即可，`showroom env` 随时查看。**中间件自适应**（§15.2）：禁用的中间件对应变量组（如 `REDIS_*`）不再注入，publish 照常，`ps`/`env` 标注「未启用」：

```
DB_HOST=sr-postgres          DB_PORT=5432
DB_NAME=sr_<project>         DB_USER=sr_<project>   （`-`→`_`）
DB_PASSWORD=<随机生成>
DATABASE_URL=postgresql://<user>:<password>@sr-postgres:5432/<dbname>
REDIS_HOST=sr-redis          REDIS_PORT=6379        REDIS_DB=<按项目分配，池 REDIS_DBS 默认 256>
REDIS_URL=redis://sr-redis:6379/<n>
SHOWROOM_PROJECT=<project>   SHOWROOM_BASE_PATH=/<project>/
```

镜像内配置文件需替换时，推荐入口脚本 `envsubst < config.tpl > config`（模板占位符 `${DB_PASSWORD}` 等在容器启动时被真实值替换，平台只注入不解析）。
