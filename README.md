# Showroom

**多租户 Demo 展示平台**：把一个 Docker 镜像变成一条 URL。内网、公网均可部署，一套平台供整个团队共享 PostgreSQL / Redis，发布者用 Web 控制台或 CLI 自助发布、更新、回滚 Demo 展项（project）——控制台账号+密码一步登录，CLI `showroom login` 一次后免密码。

```sh
showroom deploy v1          # 推镜像 + 发布 + 打印 URL，一步到位
```

## 适用场景

- 团队需要长期挂着一批演示环境（售前 Demo、验收环境、内部工具展览），但不想每人一台虚拟机；
- 发布者只会打 Docker 镜像，不想碰 K8s YAML、TLS 证书、数据库运维；
- 链路不可信——发布流量要走公网中转，但不想在应用层之外依赖传输层加密（明文 HTTP 上的端到端加密传输）。

## 核心特性

| 特性 | 说明 |
|---|---|
| OPAQUE 密码登录 | 控制台一步登录（账号+密码）、CLI `showroom login` 一次后全命令免密码（gh/ssh 模型）；密码经 OPRF 盲化**永不上线**（RFC 9380 / Ristretto255），登录自动绑定本机钥匙并配好 registry 凭据；密钥生成/登记全程无感 |
| 安全模型 | 所有管理请求经 **SR-SIG1**（Ed25519 请求签名）认证；镜像与数据库备份以**加密信封**（RSA-3072-OAEP 包裹 AES-256-GCM 密钥）在明文链路上端到端加密；管理员钥匙分层（admin key）。详见 [docs/security.md](docs/security.md) |
| 零信息暴露 | 未认证请求对 `/_api/*` 一律返回 `404 {"error":"E_AUTH"}`——不泄露路径存在性、项目名、版本等任何管理侧信息 |
| 一键发布 | `showroom deploy`（push + publish --wait + URL 横幅一步到位）与控制台发布页**单表单双镜像源**（本地 tar 文件 / 镜像库下拉选已有镜像发旧版本；「仅上传」为次要按钮） |
| 自愈 | systemd timer 驱动 `reconcile`（状态比对、幂等重建）与 `autoheal`（退出拉起、unhealthy 重启）；容器 `--restart unless-stopped` 兜底 |
| 多通道发布 | 内网走 registry 直推（快通道），公网走加密 import 通道（大文件分片断点续传）；CLI 自动探测、失败自动回落 |
| 磁盘准入与镜像管理 | 上传前按 `膨胀因子×密封体积 ≤ 可用−预留` 准入（`showroom space` 三数可见）；`showroom rm-image` 删除即内联 GC **立即**回收空间，闭环「上传不够→删除→立即可传」 |
| 共享数据库维护 | 每个展项独立供给 PostgreSQL 库与 Redis 逻辑库，凭据自动注入容器；发布者经 CLI/控制台自助执行 SQL、预览表、加密备份与恢复（zstd-19 压缩、端到端加密） |
| 主机自举 | `showroomctl setup --apply` 一条命令幂等收敛目录/中间件/Caddy/systemd/调优（声明式 host.env）；`middleware` 子命令与 Web 主机编排面板支持两段式变更与 Redis 扩容确认流 |
| WASM 控制台 | 纯静态单页控制台（Rust 编译 wasm32，无 wasm-bindgen 依赖），浏览器本地完成 OPAQUE 登录、签名与加解密，密钥不出本机；CLI 与 Web 走同一套密码学（showroom-common） |

## 架构一览

```text
                        ┌────────────────────────── your-host ──────────────────────────┐
 浏览器（控制台） ──┐   │  ┌───────────────────── Caddy ────────────────────────────────┐ │
 CLI ──────────────┼──▶│  │ site A（:80，内网面 LAN_HOST）                              │ │
                   │   │  │   /_api/*      → showroomctl serve (127.0.0.1:8090)        │ │
                   │   │  │   /v2          → registry (127.0.0.1:5000，推拉快通道)     │ │
                   │   │  │   /<project>/* → 展项容器 (127.0.0.1:8100–8199)           │ │
                   │   │  │   /_console/*  → 静态 WASM 控制台                          │ │
                   │   │  │ site B（:8080 纯端口匹配，公网面）                          │ │
 访客（展项页面）──┘   │  │   公网隧道（frp→127.0.0.1:8080）命中的站点；无 /v2 与 /_bootstrap │ │
                       │  └───────────────────────────────────────────────────────────┘ │
                       │     ┌─ showroomctl ─┐   ┌─ sr-postgres ─┬─ sr-redis ─┐        │
                       │     │ 状态+控制面    ├──▶│ 共享中间件（docker 网络 showroom-net）│
                       │     └───────────────┘   └───────────────┴────────────┘        │
                       │        /srv/showroom（btrfs 子卷：状态、快照、_tmp）            │
                       └───────────────────────────────────────────────────────────────┘
```

组件与数据流的完整说明见 [docs/architecture.md](docs/architecture.md)。

## 快速开始

### 第一步：部署主机（管理员，一次性）

在一台 Linux + Docker + btrfs 的机器上：下载二进制 → `showroomctl setup --apply`（幂等收敛目录/中间件/Caddy/systemd/调优）→ `showroomctl user-add <名> --gen-password --admin`（发首把管理员账号，返回一次性初始密码）。

完整三步自举见 **[docs/deployment.md](docs/deployment.md)**。

### 第二步：登录（发布者）

```sh
showroom login <你的账号> --server-lan http://<your-host>
# 输入密码（不回显）→ 密钥自动生成并绑定本机 → registry 凭据自动写入 config
showroom doctor                                  # 体检：密钥/配置/服务端/签名/docker
```

密码经 OPAQUE 盲化永不上线；一次登录后全部子命令免密码。CI / 机器用户走专家路径：`showroom keygen` 生成密钥、公钥串交管理员登记（见 [docs/cli-reference.md](docs/cli-reference.md)）。

### 第三步：发布

```sh
docker build -t demo:v1 .
showroom deploy demo v1                          # push + publish --wait + 打印 URL
# ══════════════════════════════
#   部署成功
#   http://<your-host>/demo/
# ══════════════════════════════
```

不想装 CLI？打开 `http://<your-host>/_console/` 用账号+密码登录：登录即见展项列表（自动加载、中文状态、可点对外 URL），「发布」页单表单双镜像源一键发布，展项行内看实时日志（行抽屉）、管数据库、浏览 Redis——显式刷新按钮零残留，数据全部自动加载：

<!-- 控制台截图位：docs/assets/console-overview.png（待补；§17 五标签布局：我的展项/发布/镜像库/数据/帮助） -->

命令逐条说明（含 `login`/`db` 全家、退出码）见 **[docs/cli-reference.md](docs/cli-reference.md)**。

## 文档索引

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](docs/architecture.md) | 组件职责、认证/发布/备份数据流、关键设计决策记录 |
| [docs/deployment.md](docs/deployment.md) | 三步自举：取二进制、setup --apply、发管理员账号；frp 接入、性能调优 |
| [docs/operations.md](docs/operations.md) | 日常运维：管理员速查、服务与 timer 清单、磁盘与镜像清理、数据库维护、故障排查、安全事件响应 |
| [docs/security.md](docs/security.md) | OPAQUE 登录协议、SR-SIG1 规范全文、加密信封字节布局、管理员体系、威胁与缓解表 |
| [docs/cli-reference.md](docs/cli-reference.md) | `showroom` 全部命令、示例与退出码 |
| [SPEC.md](SPEC.md) | crate 间内部契约（协议演进历史，深度阅读用） |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献流程与提交纪律 |
| [TESTING.md](TESTING.md) | 测试纪律：并行化、隔离规则、慢测试清单 |
| [SECURITY.md](SECURITY.md) | 漏洞报告方式与密钥泄露处置 |

## 开发

```sh
cargo nextest run --workspace    # 首选测试入口（每测试独立进程；备选 cargo test --workspace，CI 同款）
cargo clippy --workspace --all-targets -- -D warnings   # 必须零告警
cargo build --release         # 发布者 CLI（showroom）与主机工具（showroomctl）
rustup target add wasm32-unknown-unknown
cargo build -p showroom-common --target wasm32-unknown-unknown --release   # 控制台 wasm
cd showroom-web && node smoke.mjs && node smoke-wizard.mjs                 # 控制台冒烟（node ≥ 18，需系统 zstd）
```

测试基线与五规则（独立临时目录/禁全局状态/禁固定端口/显式入参/超时预算）见 **[TESTING.md](TESTING.md)**：138 个单元测试约 1.6s，无 flaky。

工作区包含四个模块：`showroom-common`（SR-SIG1、加密信封与 OPAQUE 的契约实现）、`showroom`（发布者 CLI）、`showroomctl`（主机控制面与运维）、`showroom-web`（WASM 控制台静态资源）。提交流程与规范见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

双许可（dual license），任选其一：

- [LICENSE-MIT](LICENSE-MIT)（MIT）
- [LICENSE-APACHE](LICENSE-APACHE)（Apache-2.0）

除非明确声明，你提交到本仓库的贡献将按同样的双许可授权。
