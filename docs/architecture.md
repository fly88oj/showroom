# Showroom 架构

本文描述组件职责、关键数据流与设计决策。协议细节见 [security.md](security.md)，内部契约原文见仓库根 [SPEC.md](../SPEC.md)（协议演进史）。

## 1. 组件与职责

```text
┌─────────────────────────── your-host（Linux + Docker + btrfs）──────────────────────────┐
│                                                                                          │
│   Caddy（静态面 + 反代）                                                                  │
│   ├─ /_api/*        → showroomctl serve（127.0.0.1:8090）                                │
│   ├─ /v2            → sr-registry（127.0.0.1:5000，basic auth）                          │
│   ├─ /<project>/*   → 展项容器（127.0.0.1:8100–8199，逐项目端口）                        │
│   └─ /_console/*    → /srv/showroom/_console（WASM 控制台静态资源）                      │
│                                                                                          │
│   showroomctl serve（控制面）── docker CLI/exec ──▶ 展项容器 · sr-postgres · sr-redis     │
│        │  状态读写                                                                        │
│        ▼                                                                                 │
│   /srv/showroom（btrfs 子卷 @showroom）                                                  │
│   ├─ state/（policy.env · ports.reg · tokens/ · owners/ · projects/ · routes/）          │
│   ├─ _middleware/{postgres,redis,registry}/（中间件数据卷）                              │
│   └─ _tmp/（import/dump 暂存，即用即删 + 24h gc）                                        │
│                                                                                          │
│   systemd timers → showroomctl reconcile / autoheal / gc-images / gc-registry /          │
│                    registry-gc / snapshot hourly|daily                                   │
└──────────────────────────────────────────────────────────────────────────────────────────┘
        ▲ SR-SIG1 签名 + 加密信封（明文链路安全）
        │
   ┌────┴────────────────────────────┐
   │ showroom（发布者 CLI）           │ showroom-web（WASM 控制台，浏览器内跑同一套密码学）
   │ docker save|zstd|seal → 上传    │ File.slice 流式 seal → 分片上传
   └─────────────────────────────────┘
```

| crate | 产物 | 职责 |
|---|---|---|
| `showroom-common` | rlib + **wasm32 cdylib** | SPEC §1/§2 的契约实现：SR-SIG1 签名/验签、加密信封 seal/open、命名校验、一次性 RSA 密钥对。原生侧被 ctl/cli 复用；wasm 垫片（C ABI，不用 wasm-bindgen）给浏览器控制台。密码学依赖（ed25519-dalek / rsa / aes-gcm）锁定在本 crate 并重导出，避免下游类型分裂 |
| `showroom` | 发布者 CLI（`showroom`） | 密钥与配置管理（`~/.showroom/`）、push 双通道、publish/deploy、查询与生命周期命令、db 维护子命令、doctor 体检。除 `push` 需要 docker CLI 外零运行时依赖 |
| `showroomctl` | 主机工具（`showroomctl`） | HTTP 控制面（`serve`，仅监听 127.0.0.1:8090）+ 全部运维子命令（owner 管理、发布内核、生命周期、reconcile/autoheal/gc、快照、导入）。状态存 `/srv/showroom`，全部外部世界调用集中在 `dockerops`（docker CLI / psql / caddy / 裸 TCP registry 客户端） |
| `showroom-web` | 静态资源（`dist/`） | 控制台单页：`index.html` + `showroom_bg.wasm`（由 showroom-common cdylib 构建产物改名）。登录向导、展项/数据库/状态/帮助四区、管理标签（admin 钥匙可见）。产物入库（部署通道依赖静态文件），与垫片源码同一提交内重建 |

内部深模块（showroomctl）：`state`（状态读写+纯函数：命名/policy/conf/端口/redisdb 分配/caddy 片段）、`dockerops`（外部世界）、`publish`（publish/import 内核，serve 与 CLI 子命令共用）、`ops`（生命周期与运维）、`serve`（axum 统一 fallback 分发）、`dbmaint`（db-sql/dump 三段式）、`streams`（ndjson 实时流）。

## 2. 数据流

### 2.1 发布双通道（push）

CLI 自动探测内网可达性：优先 lan，探不通回落 public。控制台无 lan 概念，恒走公网加密通道。

```text
通道 A：registry 快通道（lan 可达时）
  docker tag demo:v1 <your-host>/<owner>/demo:v1
  docker login <your-host>（basic auth ← owner 的 registry 凭据）
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

publish 内核（两条通道汇合后）：校验名/配额 → registry 镜像存在 → 内存准入（MemAvailable − RESERVE_MEM_MB ≥ 需求）→ 分配 host 端口（8100–8199）→ 供给 DB/Redis（**首建不重建**，数据保留）→ 起容器（滚动：`-new` 后缀起新、健康后删旧改名；健康 = TCP 可连或 60s 超时放行）→ 写路由片段 + `caddy reload` → 更新 last2。

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
│  ├─ policy.env                   # MAX_SLOTS DEFAULT_MEM MAX_MEM IMPORT_MAX_GB ALLOW_LAN_PORT RESERVE_MEM_MB
│  ├─ ports.reg                    # 每行 <port> <project>
│  ├─ tokens/<owner>/<kid>.pub     # Ed25519 公钥（可多把）；同目录 <kid>.admin 空文件 = 管理员钥匙
│  ├─ owners/<owner>/reg.cred      # registry basic_auth（0600 明文）
│  ├─ projects/<project>.conf      # key=value：owner tag port cport mem cpu redisdb updated
│  ├─ projects/<project>.env       # 注入容器的连接信息（§6 契约）
│  ├─ projects/<project>.last2     # 上一 tag（回滚目标）
│  └─ routes/<project>.caddy       # 展项路由片段（主 Caddyfile import）
├─ _middleware/{postgres,redis,registry}/   # 中间件容器数据卷（bind）
├─ _console/                       # WASM 控制台静态资源
└─ _tmp/                           # import/dump 暂存（即用即删 + 24h gc）
```

`/<project>/`（host 端口 127.0.0.1:8100–8199 反代）→ 展项容器；容器名 `sr-<project>`；库名 `sr_<project 的 - → _>`。

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
| D8 | **管理员标记在钥匙上（kid.admin）而非账号上** | 一人可同时持普通与管理员钥匙；`whoami` 按当前请求命中的 kid 判定，吊销粒度细到单钥匙（SPEC §12.2） | 管理员钥匙遗失 = 平台最高权限泄露 |
| D9 | **publish 首建不重建数据库**（v1.2 起） | 发布者重新 publish 不应意外清库；显式重置走 `db reset`（DROP+重建+凭据不变） | 迁移 schema 是发布者自己的责任（db sql/restore 工具齐备） |
| D10 | **nonce 查重服务重启清零** | 窗口仅 ±300s，重启后旧 nonce 早已过期；避免持久化（SPEC §1 明示可接受） | — |
| D11 | **registry v2 客户端用裸 TCP HTTP/1.0 手写** | 控制面对 localhost 单一 registry，引第三方 async HTTP 栈不划算（ctl 内部约定） | 仅覆盖本机 registry 场景 |
| D12 | **统一 zstd 压缩（CLI 与控制台上传、dump）** | 压缩在加密之前，先压缩后加密不泄露熵；zstd-19 高压缩；服务端魔数自适应兼容原生 -Fc | — |
| D13 | **滚动更新 = `-new` 后缀起新再删旧改名** | 同项目唯一 host 端口约束下的简单滚动 | 秒级中断（已知天花板，代码内 ponytail 注释同步） |
| D14 | **`/_api/projects` 对任意登记发布者全量可见** | 团队内部可见性是 Demo 平台的合理默认 | 非强多租户隔离；强隔离需求在反代层收紧 |
| D15 | **git 历史定稿窗口允许重塑** | v1 未定稿前保持重塑自由（详见 CONTRIBUTING「历史重塑政策」） | 定稿后禁止 force-push；重塑需公告并评估凭据轮换 |

## 4. 环境变量契约（publish 自动注入）

容器以 `--env-file` 注入以下变量，值全部具体化；应用按 12-factor 读环境即可，`showroom env` 随时查看：

```
DB_HOST=sr-postgres          DB_PORT=5432
DB_NAME=sr_<project>         DB_USER=sr_<project>   （`-`→`_`）
DB_PASSWORD=<随机生成>
DATABASE_URL=postgresql://<user>:<password>@sr-postgres:5432/<dbname>
REDIS_HOST=sr-redis          REDIS_PORT=6379        REDIS_DB=<0-15 分配>
REDIS_URL=redis://sr-redis:6379/<n>
SHOWROOM_PROJECT=<project>   SHOWROOM_BASE_PATH=/<project>/
```

镜像内配置文件需替换时，推荐入口脚本 `envsubst < config.tpl > config`（模板占位符 `${DB_PASSWORD}` 等在容器启动时被真实值替换，平台只注入不解析）。
