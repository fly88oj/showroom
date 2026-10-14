# frpc-rust

使用 Tokio 实现的原生 Rust frp 客户端。客户端直接实现 frp 消息协议和数据转发，不依赖 Go 运行时，也不会启动或包装官方 frpc。

本项目按“常用生产功能完整”的范围开发，以官方 **frp v0.70.0 的 v1 协议**为兼容基线。已经实现下表功能，并通过本地协议对端测试。**真实官方 frps 联调尚待完成**：开发环境的 Windows Defender 隔离了官方发布的 frps/frpc 程序。仓库提供独立的官方联调测试与 Linux CI 任务，不能把本地对端测试理解为已经通过官方互操作验证。

## 功能范围

| 功能 | 实现内容 |
| --- | --- |
| 代理 | TCP、UDP、HTTP、HTTPS、STCP、SUDP、TCPMUX/HTTP CONNECT |
| 访问者 | STCP、SUDP；独立的用户命名空间与访问签名 |
| 服务端连接 | TCP、QUIC、WebSocket、WSS；yamux 多路复用；工作连接池 |
| TLS | 默认启用；自定义 CA、SNI、客户端证书、frp 自定义首字节 |
| 认证 | Token、Token 文件源、OIDC Client Credentials、OIDC 文件源；心跳/工作连接附加认证 |
| 数据流 | AES-128-CFB、Snappy 帧压缩、双向转发、半关闭、IPv4/IPv6、PROXY protocol v1/v2 |
| 代理配置 | 域名、子域名、HTTP 路由、BasicAuth、请求/响应头、Host 改写、分组负载均衡、元数据 |
| 可用性 | 带随机抖动的指数退避重连、登录失败退出策略、注册失败重试、TCP/HTTP 健康检查 |
| 带宽控制 | 每个代理聚合限速，支持客户端或服务端限速 |
| 管理 | Web 页面、状态/健康/Prometheus 接口、重载、停止、管理认证 |
| 配置 | TOML、JSON、YAML；`{{ .Envs.NAME }}`；相对路径 includes；严格字段校验 |
| 日志与退出 | 结构化日志、按天滚动、保留文件数量、Ctrl+C、Linux SIGTERM、任务取消和资源回收 |
| 插件 | `socks5`、`http_proxy`、`static_file`、`unix_domain_socket`（Unix）、`tls2raw`、`http2http`、`http2https`、`https2http`、`https2https` |

HTTP/HTTPS 域名路由、HTTP BasicAuth、头改写、TCPMUX 和分组负载均衡通过官方 `NewProxy` 消息交由 frps 执行，客户端负责本地流量转发。HTTP 转换插件支持流式请求、HTTP/1.1 Upgrade 和 HTTPS 入站 HTTP/2。SOCKS5 插件支持 CONNECT 与用户名密码认证。

以下官方能力**不在当前实现范围**，相关配置会报错：XTCP/P2P、VirtualNet、KCP、wireProtocol v2、NTLM 上游代理、自定义 DNS、旧 INI 配置、任意 Go template/端口范围模板、Visitor 插件及转发回退、Store 动态增删持久化、官方管理 API 的全部兼容接口。WebServer 的 TLS 配置也未实现；可通过反向代理提供 HTTPS。

## 构建与启动

需要 Rust 工具链。项目声明 Rust 1.85 / edition 2024，开发环境实际验证版本为 **Rust 1.94.1**；更低版本的工具链尚未做独立验证。

```powershell
cargo build --release --locked
cargo run -- -c frpc.toml verify
cargo run -- -c frpc.toml
```

Windows 发布程序位于 `target/release/frpc-rust.exe`，Linux/macOS 位于 `target/release/frpc-rust`。

仓库根目录的 `frpc.toml` 是本机开发示例：将 `127.0.0.1:8080` 映射到 frps 的 `18080` 端口。使用经过允许的官方 frps 运行配套配置，并自行启动本地 HTTP 服务：

```powershell
frps -c examples/frps.toml
python -m http.server 8080 --bind 127.0.0.1
```

启动客户端后访问 `http://127.0.0.1:18080`；管理页面在 `http://127.0.0.1:7400`。示例中的 `local-development-only` 是公开的本地测试占位值，部署时应替换。

如果服务端宿主机的 `7000` 已被其他应用占用，可使用 [Docker Compose 部署配置](deploy/frps/README.md)，将宿主机 `7001` 映射到容器内 frps 的 `7000`，客户端对应设置 `serverPort = 7001`。

## 常用配置

```toml
serverAddr = "frps.example.com"
serverPort = 7000
user = "office"
loginFailExit = false
auth.token = "{{ .Envs.FRP_TOKEN }}"

transport.tls.enable = true
transport.tls.trustedCaFile = "certs/ca.pem"
transport.tls.serverName = "frps.example.com"
transport.tcpMux = true
transport.poolCount = 4

webServer.addr = "127.0.0.1"
webServer.port = 7400

[[proxies]]
name = "ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = 6000
transport.useEncryption = true
transport.useCompression = true
```

完整的七类代理和访问者示例见 [examples/proxies.toml](examples/proxies.toml)，OIDC 示例见 [examples/oidc.toml](examples/oidc.toml)。

认证文件源格式为：

```toml
auth.method = "token"
auth.tokenSource.type = "file"
auth.tokenSource.file.path = "secrets/frp-token.txt"
```

`token` 与 `tokenSource` 互斥。Token 文件在新会话创建时读取，OIDC 文件源在每次需要认证时读取。OIDC Client Credentials 缓存 access token，并在过期前刷新。缺失环境变量会使校验失败；配置文件中的环境变量值按原文替换，需要与目标 TOML/JSON/YAML 字符串语法相容。

`includes = ["conf.d/*.toml"]` 相对主配置所在目录展开，包含文件只提供 proxies/visitors，不递归包含其他文件。证书、Token 和插件文件路径相对声明它们的配置文件。`start = ["ssh"]` 选择部分代理/访问者，单项 `enabled = false` 可以禁用项目；重复名称和 `start` 中不存在的名称都会被拒绝。

## 传输与运维行为

- `transport.protocol` 可设置为 `tcp`、`quic`、`websocket`、`wss`。QUIC 的 `serverPort` 对应 frps 的 `quicBindPort`，ALPN 为 `frp`。QUIC 自带流复用。
- `transport.proxyURL` 支持 HTTP CONNECT 和 SOCKS5 上游代理，可带 URL 编码的用户名密码。QUIC 不使用此设置。
- 未配置 `trustedCaFile` 时，TLS 行为与官方 frpc 一致：接受 frps 自动生成的自签名证书。配置 CA 后会校验证书链和服务端名称。HTTPS 转换插件的 HTTPS 后端同样按官方行为接受自签名证书。
- 默认每 30 秒发应用心跳，90 秒无响应重连。`heartbeatInterval = -1` 时，本实现仍用 `tcpMuxKeepaliveInterval` 周期发送控制心跳，以保证 Rust yamux 空闲连接也能及时发现故障。
- 重连等待从约 1 秒指数增长到约 30 秒，附带随机抖动；成功维持 30 秒后重置退避。首次登录失败是否退出由 `loginFailExit` 控制。
- UDP 按远端地址维护独立本地 socket，空闲 60 秒后回收；每个隧道最多 4096 个会话。消息队列有界，拥塞时可能丢弃 UDP 数据包。`udpPacketSize` 默认 1500，当前 v1 帧下允许 1～6500 字节，必须与 frps 配置一致。
- `bandwidthLimit = "10MB"` 使用 1024 进制字节/秒；客户端模式下，一个代理的所有连接及两个方向共享限额。
- TCP 健康检查通过连接本地服务判断；HTTP 检查仅把 HTTP 200 视为成功。失败达到阈值后注销代理，恢复后重新注册。
- 工作连接受会话生命周期管理；重连和退出会关闭活动连接。普通 TCP 转发支持半关闭，缓冲区在写端关闭前排空。
- 日志可配置 `log.to = "logs/frpc.log"`、`log.level = "info"`、`log.maxDays = 7`。滚动文件按天生成，最多保留指定数量的文件；日志路径相对进程工作目录。

## 管理接口

```powershell
cargo run -- -c frpc.toml status
cargo run -- -c frpc.toml reload
cargo run -- -c frpc.toml stop
cargo run -- version
```

| 接口 | 方法 | 说明 |
| --- | --- | --- |
| `/` | GET | 管理页面，每 3 秒刷新 |
| `/api/status` | GET | 会话、代理、访问者、连接数和流量计数 |
| `/healthz` | GET | 已登录返回 200，离线返回 503 |
| `/metrics` | GET | Prometheus 格式的连接和流量指标 |
| `/api/reload` | POST | 从原配置路径重载 |
| `/api/stop` | POST | 停止客户端 |

**重载会重建整个会话并中断当前连接。** 配置解析、文件资源或参数校验失败时，当前会话保持运行。`webServer` 和 `log` 修改需要重启。重载接口成功表示已接受配置，代理最终是否注册成功应通过状态接口确认。代理流量指标为应用数据字节，按会话重新计数。

非回环地址的管理监听必须配置 `webServer.user` 与 `webServer.password`。全部接口共用 BasicAuth，并拒绝浏览器跨源请求；仅提供项目所列接口，不宣称完全替代官方管理 API。

Linux 服务模板见 [examples/frpc-rust.service](examples/frpc-rust.service)。部署前创建 `frpc` 系统用户、放置程序和配置；模板默认输出日志到 journal，`ExecReload` 依赖启用的管理接口。

## TLS 连接排错

`received corrupt message of type InvalidContentType` 表示 TLS 层收到了非 TLS 格式的数据，例如目标端口返回了明文 `HTTP/1.1 400`。客户端日志会保留完整错误链，并显示目标地址、传输协议和配置检查提示。

先核对 `serverAddr:serverPort` 是否确实对应 frps 的控制入口 `bindPort`。如果通过 Docker 发布端口，应填写宿主机映射到容器 `bindPort` 的端口；管理页面的 `webServer.port`、HTTP 业务入口和其他应用的端口不能代替该入口。HTTP 反向代理场景还需要核对 TCP 透传或 WebSocket/WSS 转发方式；frp WebSocket 路径为 `/~!frp`，正常升级响应为 HTTP 101。

旧版 frps 或控制端口与 HTTPS 业务端口复用时，可能需要 `transport.tls.disableCustomTLSFirstByte = false`。这个选项只处理 frp TLS 流量的分流，无法将普通 HTTP 服务变成 frps 控制服务。TLS 握手失败时，客户端不会自动关闭 TLS 或重试明文连接。

## 验证

```powershell
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

本地测试覆盖协议帧、独立 Node crypto AES-CFB 向量、认证、分片消息、长度边界、加密/压缩组合、半关闭、任务清理、TLS/CA/SNI、yamux、WebSocket/WSS、QUIC、并发 TCP、UDP 会话隔离、STCP/SUDP 访问者、重连、健康检查、重载保护、管理认证、OIDC 和主要插件。TLS 插件测试包含 HTTP/2 协商。

官方互操作测试默认标记为 ignored，需提供一个可运行的官方 frps：

```powershell
$env:FRPS_BIN = "C:/tools/frp/frps.exe"
cargo test --locked --test interop -- --ignored --nocapture
```

```sh
FRPS_BIN=/opt/frp/frps cargo test --locked --test interop -- --ignored --nocapture
```

该测试在临时目录创建配置，仅使用本机回环地址，对 TCP/WebSocket/WSS/QUIC 分别运行七类代理和四种加密压缩组合。测试进程退出时会关闭子进程。CI 的互操作任务从固定的官方 v0.70.0 源码提交构建 frps 后执行；**本次开发尚未运行远端 CI**。

## 代码结构

| 文件 | 职责 |
| --- | --- |
| `src/config.rs` | 类型化配置、模板、includes、校验 |
| `src/wire.rs` | frp v1 帧与 UDP 消息 |
| `src/auth.rs` | Token/OIDC 认证 |
| `src/stream.rs` | 加密、压缩、限速、流量计数与任务生命周期 |
| `src/transport.rs` | TCP/TLS/QUIC/WebSocket、yamux、上游代理 |
| `src/service.rs` | 登录、控制消息、心跳、重连、注册、健康检查和重载 |
| `src/proxy.rs` | TCP/UDP 转发、访问者与 PROXY protocol |
| `src/plugin.rs` | 本地协议/服务插件 |
| `src/admin.rs` | 管理接口与指标 |

协议参照和授权信息见 [NOTICE](NOTICE)。本项目使用 Apache-2.0 许可证。
