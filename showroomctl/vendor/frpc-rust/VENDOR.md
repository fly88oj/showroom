# Vendored: frpc-rust

- 上游：https://github.com/L1yp/frpc-rust @ `e8920c6badfaf6ab249b2da46513d31652b65867`（2026-10 vendor）
- 许可：Apache-2.0（NOTICE 见本目录；声明为 frp v1 协议的第三方 Rust 实现，非 frp 官方发布）
- 用途：showroomctl 隧道模块（docs/architecture.md D22）——frp v1 客户端协议实现
  （tcp 传输 + yamux + TLS 首字节 + token 认证）。
- 本地修改（仅此一处）：`[profile.release]`（lto/strip）已移除——vendor 进 showroom
  workspace 后 profile 由仓库根统辖。其余源码逐字未动。
- 验证：其自带 interop 测试 ×官方 frps v0.70.0 实测——tcp/websocket 传输全 7 类代理
  × 4 加密压缩组合全绿；wss 传输变体失败（与本仓用途无关，chmlfrp/native 均走 tcp）。
  chmlfrp 真机节点联调见 docs/deployment.md 验收清单。
- 升级纪律：如需更新上游，替换本目录后必须重跑 interop（FRPS_BIN=官方 frps）+ 本仓
  tunnel 测试，并在本文件更新 rev 与验证记录。
- 已知剪裁机会（评审裁决本轮不做：LTO 下运行时零影响，纯构建面收益）：admin.rs（axum，
  仅 vendor 自带 main.rs 单点引用）、bin 目标（clap/tracing-subscriber）、quic/websocket
  传输（quinn/tokio-tungstenite）、visitor/plugin。若做，建议 cfg feature-gate 而非删源码
  （保「逐字未动」可升级纪律）；注意 plugin.rs 亦用 hyper/tower，gate 面比直觉大。
