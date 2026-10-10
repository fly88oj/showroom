# Showroom

**多租户 Demo 展示平台**：把一个 Docker 镜像变成一条 URL。内网、公网均可部署，一套平台供整个团队共享 PostgreSQL / Redis，发布者用 Web 控制台、CLI 或 Agent Skill 三种方式自助发布、更新、回滚 Demo 展项（project）。

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
| 安全模型 | 所有管理请求经 **SR-SIG1**（Ed25519 请求签名）认证；镜像与数据库备份以**加密信封**（RSA-3072-OAEP 包裹 AES-256-GCM 密钥）在明文链路上端到端加密；管理员钥匙分层（admin key）。详见 [docs/security.md](docs/security.md) |
| 零信息暴露 | 未认证请求对 `/_api/*` 一律返回 `404 {"error":"E_AUTH"}`——不泄露路径存在性、项目名、版本等任何管理侧信息 |
| 自愈 | systemd timer 驱动 `reconcile`（状态比对、幂等重建）与 `autoheal`（退出拉起、unhealthy 重启）；容器 `--restart unless-stopped` 兜底 |
| 多通道发布 | 内网走 registry 直推（快通道），公网走加密 import 通道（大文件分片断点续传）；CLI 自动探测、失败自动回落 |
| 共享数据库维护 | 每个展项独立供给 PostgreSQL 库与 Redis 逻辑库，凭据自动注入容器；发布者经 CLI/控制台自助执行 SQL、预览表、加密备份与恢复 |
| WASM 控制台 | 纯静态单页控制台（Rust 编译 wasm32，无 wasm-bindgen 依赖），浏览器本地完成签名与加解密，密钥不出本机 |

## 架构一览

```text
                       ┌────────────────────────── your-host ──────────────────────────┐
 浏览器（控制台）  ──┐  │  ┌───────────────────────────── Caddy（80/443，双面）────────┐ │
 CLI / Agent Skill ──┼──┼─▶│  /_api/*        → showroomctl serve (127.0.0.1:8090)     │ │
 访客（展项页面）  ──┘  │  │  /v2            → registry (127.0.0.1:5000，推拉镜像)    │ │
                       │  │  /<project>/*   → 展项容器 (127.0.0.1:8100–8199)         │ │
                       │  │  /_console/*    → 静态 WASM 控制台                        │ │
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

在一台 Linux + Docker + btrfs 的机器上部署控制面与中间件：目录与子卷、systemd 单元、Caddyfile、中间件容器、首把管理员钥匙引导（`showroomctl user-add <名> --pubkey <…> --admin`）。

完整步骤见 **[docs/deployment.md](docs/deployment.md)**。

### 第二步：生成密钥并登记（发布者）

```sh
showroom keygen                                  # 生成 Ed25519 密钥，打印公钥串
# 把公钥串发给管理员登记（管理员可用控制台「管理」标签远程发卡，返回 registry 密码）
showroom config --user <你的账号> \
    --server-lan http://<your-host> \
    --registry-user <你的账号> --registry-password <registry 密码>
showroom doctor                                  # 体检：密钥/配置/服务端/签名/docker
```

### 第三步：发布

```sh
docker build -t demo:v1 .
showroom deploy demo v1                          # push + publish --wait + 打印 URL
# ══════════════════════════════
#   部署成功
#   http://<your-host>/demo/
# ══════════════════════════════
```

不想装 CLI？打开 `http://<your-host>/_console/` 用浏览器密钥登录，同样能发布、看日志、管数据库：

<!-- 控制台截图位：docs/assets/console-overview.png（待补） -->

命令逐条说明（含 `db` 全家、退出码）见 **[docs/cli-reference.md](docs/cli-reference.md)**。

## 文档索引

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](docs/architecture.md) | 组件职责、发布/备份数据流、关键设计决策记录 |
| [docs/deployment.md](docs/deployment.md) | 从零部署：目录/子卷、systemd、Caddyfile、中间件、管理员引导、frp 接入、性能调优 |
| [docs/operations.md](docs/operations.md) | 日常运维：服务与 timer 清单、快照与 GC、扩内存、备份恢复、故障排查、安全事件响应 |
| [docs/security.md](docs/security.md) | SR-SIG1 规范全文、加密信封字节布局、管理员体系、威胁与缓解表 |
| [docs/cli-reference.md](docs/cli-reference.md) | `showroom` 全部命令、示例与退出码 |
| [SPEC.md](SPEC.md) | crate 间内部契约（协议演进历史，深度阅读用） |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献流程与提交纪律 |
| [SECURITY.md](SECURITY.md) | 漏洞报告方式与密钥泄露处置 |

## 开发

```sh
cargo test --workspace        # 全部单元测试
cargo build --release         # 发布者 CLI（showroom）与主机工具（showroomctl）
rustup target add wasm32-unknown-unknown
cargo build -p showroom-common --target wasm32-unknown-unknown --release   # 控制台 wasm
cd showroom-web && node smoke.mjs && node smoke-wizard.mjs                 # 控制台冒烟（node ≥ 18，需系统 zstd）
```

工作区包含四个模块：`showroom-common`（SR-SIG1 与信封的契约实现）、`showroom`（发布者 CLI）、`showroomctl`（主机控制面与运维）、`showroom-web`（WASM 控制台静态资源）。提交流程与规范见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

双许可（dual license），任选其一：

- [LICENSE-MIT](LICENSE-MIT)（MIT）
- [LICENSE-APACHE](LICENSE-APACHE)（Apache-2.0）

除非明确声明，你提交到本仓库的贡献将按同样的双许可授权。
