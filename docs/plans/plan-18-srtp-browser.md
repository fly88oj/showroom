# Plan 18：SRTP 浏览器侧（WS 载波）— 设计方案（SPEC §2.1 的 WS 载波扩展）

> 状态：设计定稿（用户裁决两级处理已落 CLI 侧，本文为浏览器侧实现蓝本）。
> 基准：HEAD a3277f1。依赖既有：common `TWriter/TReader/unwrap_key`、网关 `FrameFwd`、
> CLI `srtp_transport`（TCP 载波参照实现）。

## 0. 载波选型（为什么是 WebSocket）

| 候选 | 结论 | 原因 |
|---|---|---|
| 裸 TCP + SRTP | ✗ | 浏览器无 TCP socket API |
| **WebSocket** | ✓ | 唯一能穿 frp tcp 隧道的浏览器原生双向二进制通道；握手是普通 HTTP（Caddy reverse_proxy 原生支持升级），中转零配合 |
| WebTransport | ✗ | 需 HTTP/3/QUIC 入口；chmlfrp 是 tcp 型中转，H3 终止在中转=要求中转配合（违背零配合前提） |
| WebRTC DataChannel | ✗ | 需 STUN/TURN 信令设施，为一个密封通道引入整套 P2P 设备，不成比例 |

## 1. 总体形态

```
浏览器 wasm console ── WS(二进制) ──► 公网入口 ──► [frps 中转：只见密文] ──► frpc
  ──► 网关 :8082（明文 HTTP 直通）──► Caddy :8081 ──► serve :8090 `GET /_srtp`（WS 升级）
        └─ serve 内 SRTP-WS 会话：解记录 → 帧界 → HTTP/1.1 字节 → 走原 Router 分发
          （SR-SIG1 验签/限流/零信息暴露全部原样）→ 响应原样字节 → 记录密封回发
```

关键分层决策：**SRTP-WS 终止在 serve**（axum 内置 ws），不在网关。网关对 WS 握手的
前 6 字节（`GET /_…`）命中明文直通 → Caddy → serve，一行不改。内层协议与 CLI 的
TCP 载波**逐层同构**（记录层 + 应用帧 + HTTP/1.1 字节），三处复用同一套格式定义。

## 2. 线格式（SPEC §2.2 候选文）

载波信封按载体而定，**记录层及以上完全共享**：

```
TCP 载波（CLI，v1.1 起）："SRTP"0x01 flags ‖ 8B srtp_id ‖ 384B wrap ‖ 记录流
WS  载波（本文）        ：msg[0]  = 8B srtp_id ‖ 384B wrap
                         msg[n]  = 记录字节（0..N 条记录拼接；WS 消息边界即分片边界）
记录 = [12B nonce][4B 大端密文长][密文‖16B tag]            （TWriter/TReader，不变）
上行记录明文 = 应用帧 [8B 大端长度][HTTP/1.1 请求字节]      （FrameFwd，不变）
下行记录明文 = 上游响应原样字节（读到上游 EOF 为一响应）     （不变）
```

- 无 6B 帧头：其唯一职责是网关混合探测（明文直通 vs SRTP），WS 连接天然专用于 SRTP。
- 服务端**每完成一帧即分发**（请求完结由帧界驱动——中继无半关闭语义的教训同样适用），
  帧内记录可分多条 WS 消息到达（上传 8MiB 分片流式）。
- 帧上限：**16MiB**（覆盖 1MiB JSON 体上限与 8MiB import/dump 切片 + 余量；网关 TCP
  载波的 1GiB 上限保留给 CLI 大流）。超限 → 会话关闭。

## 3. serve 端点（showroomctl）

### 3.1 路由与零信息暴露

- `Router::new().route("/_srtp", get(srtp_ws)).fallback(api)`——真实路由置于统一
  fallback 之前；`/_srtp` 进入 serve 的 **shared showroom-app 片段**（site A/B 同有；
  LAN 上存在但控制台不会启用——能力判定见 §5）。
- 非 Upgrade 的 GET、`Sec-WebSocket-Protocol != showroom-srtp-v1` → **404 空体**
  （与不存在路径不可区分，纪律不倒退）。端点匿名可达性与现状等价：内层请求仍需
  SR-SIG1 验签，密封会话本身不授予任何能力。
- Cargo：`axum` 加 `ws` feature（内置，零新依赖）。

### 3.2 会话核心（可测形状）

```rust
// 会话状态机从 WebSocket 里抽出来，泛型于消息通道——单测用 futures channel 直测：
async fn srtp_ws_session<I, O>(input: I, output: O, app: Router, keys: &PlatformKeys)
where I: Stream<Item = Vec<u8>>, O: Sink<Vec<u8>>
{
    // ① msg0(384B wrap) → unwrap_key → k；坏 wrap → 关闭
    // ② 循环：TReader.push(msg) → FrameFwd.feed → 完整帧 = 一个 HTTP/1.1 请求
    //    （请求行+头+Content-Length 体；解析失败 → 会话关闭，不回显细节）
    // ③ 构造 http::Request<Body>（原样 method/uri/headers/body）→ app.clone().call(req)
    //    ——Router 即 Service：SR-SIG1/nonce/限流/E_AUTH 404 全部走原路径，零新面
    // ④ 响应收集（cap 16MiB）→ 序列化 HTTP/1.1 字节（status+headers+body，无 chunked：
    //    明确 Content-Length）→ TWriter.record(s) → output
    // ⑤ 上游响应结束即一响应完成；继续等下一帧（一连接多请求，顺序一比一）
}
// WS 胶水层：WebSocketUpgrade.protocols(["showroom-srtp-v1"]).on_upgrade(...)
```

- **并发模型**：单会话内请求严格顺序（WS 消息天然有序，帧界定界）；控制台侧队列
  一深度（§5）。会话上限：全局 64 并发（超限握手后即关），单连接记录速不设限
  （令牌在带宽）。
- 服务端空闲心跳：axum ws `Ping` 每 20s，两轮无 `Pong` 判死清理（浏览器自动回 Pong）。
- 日志口径：`srtp-ws 会话建立`（与网关的 `SRTP 会话建立` 区分，排障可 grep）。

### 3.3 密钥编排：动态下发、按用户隔离（OPAQUE 绑定）——替代静态公钥文件

**问题重述（先厘清密码学事实）**：公钥是公开物，被「窃取」无害；要防的是两件事——
①传输途中的**替换**（主动 MITM 换自己的公钥 → 解密再转封 = 全程窃听）；
②服务端私钥失窃的**波及面**。静态文件分发（初稿方案）对两者皆无防御。

**核心洞察：本系统已有服务端身份验证——OPAQUE 登录**。登录仅对真实服务端可完成
（MITM 无 OPRF 密钥不可能代答），且 opaque-ke 两侧各得同一 `session_key`（AKE 输出，
依赖面已核实）。因此：

```
login-finish 响应（客户端此刻已确证对端=真实服务端）附带：
  srtp_id      : 8B 随机句柄（本次登录新鲜；中转只见此不可逆句柄，用户名不露）
  srtp_wrap_pub: 该用户的 SRTP 包裹公钥（PEM）
  key_mac      : HMAC-SHA256(opaque_session_key, "srtp-key-v1" ‖ srtp_id ‖ srtp_wrap_pub)
```

- **防替换（①）**：客户端以本地 OPAQUE session_key 验 key_mac——MITM 改任何一个字段
  即验签失败 → **fail-closed**（向用户报告「检测到传输链路篡改」并禁用该会话）。
  这同时给了此前不具备的**主动篡改检测能力**。旧 (pub, mac) 重放无效（MAC 绑定本次
  登录的新鲜 session_key）。
- **按用户隔离（②）**：包裹密钥 per-user（`state/users/<u>/srtp_rsa{,.pub}`，RSA-3072，
  首次登录惰性生成、0600）。单用户私钥失窃只暴露该用户流量；密码重置/吊销即轮换该用户
  密钥（不动他人）。诚实边界：单机部署下全部私钥同居一磁盘——隔离的价值是**定向轮换
  与波及面收敛**，不是抗整站磁盘失窃；抗「录制流量 + 事后密钥失窃」的前向保密见 §3.5。
- **动态性**：密钥随每次登录下发（内存持有，不落客户端盘）——轮换即时生效，无静态
  文件的缓存/过期问题；原 `srtp.pub` 静态资产方案作废（匿名面反而收窄）。
- **多设备**：同用户多设备各自持有 srtp_id；`state/srtp_ids.json` 维护 id→user 映射
  （登录签发、登出/吊销作废、幂等 GC）。网关与 serve 共享该映射（内存缓存 + 落盘）。

**前置格式变更（v1.1）**：上行 wrap 前加 8B srtp_id（见 §2）——网关/serve 先查
id→用户私钥，再解 wrap。平台级 RSA 密钥保留给 §2 上传信封（import 通道），不动。

### 3.5 前向保密（明确缓办，记录取舍）

per-user 静态密钥不能防「录制流量 + 事后私钥失窃」的回放解密。彻底解法 = 每次登录
下发**临时** X25519 密钥（ECIES 取代 RSA-OAEP wrap，x25519-dalek + HKDF，会话结束
即焚）。缓办理由：新增一条密码学路径（依赖+实现+审计面），而其防御的对象（事后磁盘
失窃）在本机部署的现实威胁排序中低于 MITM 替换（§3.3 已解）；格式已版本化（SRTP
v1.1），后续升级为 v1.2 临时密钥时载波与帧层零改动。

## 4. wasm 垫片新增（showroom-common/wasm_shim，C-ABI）

会话双工（同一 k 同时服务上行封装与下行解封，浏览器会话语义）：

```text
sr_srtp_begin(pub_pem, out_wrap, wrap_cap) -> sid   // 生成 k、写 384B wrap；会话持有 k
sr_srtp_seal(sid, in, in_len, out, out_cap) -> n     // 明文 chunk → 记录字节（每 64KiB 一记录，多次调用共构一帧；JS 拼帧头 8B）
sr_srtp_open(sid, in, in_len, out, out_cap) -> n     // 下行记录字节 → 明文（任意分片）
sr_srtp_end(sid)                                     // 会话收尾
```

- 原生实现 = `TWriter::record` / `TReader::push` 的会话表包装（与 sr_seal/sr_open
  的既有会话表同构：槽位只增、容量协商 ≤ -40 协议）。
- `srtp.pub` 解析复用既有 PEM 路径；k 不出会话（与 §2 信封同安全域）。

## 5. 控制台侧（console/src/lib/srtp.ts + api.ts 接线）

### 5.1 两级与能力判定（与 CLI 同哲学：尝试 + 降级，零配置）

- **能力探测与激活时序**：`/_api/auth/*` 登录族**恒走 Tier 2**（OPAQUE 本就是为
  敌意信道设计的零知识交换，无需密封；且包裹公钥恰由 login-finish 下发——先有蛋）。
  login-finish 验 key_mac 成功后建立 WS 通道；此后全部 api() 走 Tier 1。
  WS 建立失败（404/超时/旧服务端）→ 页面会话内粘性 Tier 2（与 CLI 对齐）。
  key_mac 验证失败 → fail-closed 报「检测到传输链路篡改」并中止（不降级——降级
  正是 MITM 所愿）。
- 判定不依赖 Caddy 头注入等服务器配置面；LAN 入口旧服务端自动回落，双端版本任意组合。
- 匿名访客不建连（登录页纯静态+登录提交才首个 api 调用——懒连接天然覆盖）。

### 5.2 通道与请求队列

```ts
class SrtpChannel {
  // 单 WS 连接；请求队列深度 1（与 serve 顺序一比一模型对齐）
  // request(httpBytes: Uint8Array): Promise<Uint8Array>
  //   → 8B 帧头 + wasm sr_srtp_seal 分记录发送
  //   → 收集下行消息 → sr_srtp_open → 帧解帧 → resolve（响应字节）
  // 每请求整体超时 30s（JSON）/ 300s（分片上传/下载），与现有 fetch 超时档一致
}
```

- `httpSerialize/parseHttp`：从 CLI `srtp_transport` 移植的 TS 版（~120 行，
  vitest 移植 CLI 的形状测试：Content-Length/chunked 去块/EOF 体）。
- **失败重试语义**：WS 中途断开时在途请求**重签后**走 Tier 2 重放（新 nonce+时间戳
  ——服务端 nonce 查重使同签重放必被拒；重签是幂等重试的正确形态。import 分片
  本就有分片级重试语义）。
- 上传（envelope 8MiB 切片）：帧内按 64KiB 记录流式 seal——内存有界；下载
  （dump 4MiB 切片）对称。`envelope.ts`/`upload.ts` 零改动（信封仍在其位，
  SRTP 之外再一层，双重独立=与 CLI push 同型）。

### 5.3 api.ts 接线

- `api()` 入口：Tier 1 时构造与现有完全相同的 SR-SIG1 头与 body → serialize →
  channel.request → parse → 交付 JSON；Tier 2 → 现有 fetch 原样。
- 上传/下载的 `postChunk`/分片 GET 同样过通道（signed_get_raw 等价物）。

## 6. 威胁模型与不变量（如实声明）

| 面 | SRTP-WS 后 | 说明 |
|---|---|---|
| frps 中转看 API 流量 | 看不到（高熵记录） | 本设计主目标 |
| 浏览器→中转被动窃听 | 看不到 | 同上 |
| 浏览器→中转**主动** MITM（密钥替换） | **可检测并阻断** | OPAQUE 绑定 key_mac，fail-closed（§3.3）；但 JS/wasm 本体替换仍防不住——根治=入口 HTTPS（独立事项） |
| 服务端认证/防重放 | 不变 | SR-SIG1+nonce 在记录内层原样验 |
| 设备/密钥模型 | 不变 | 密封管道内仍是逐请求签名（前轮设计结论：SRTP≠身份） |
| 明文直通流量（匿名访客/Docker Push import） | 不变 | 网关混合探测原样 |

## 7. 实施清单与工作量

| 步骤 | 内容 | 量级 |
|---|---|---|
| S0 | 密钥编排（§3.3）：login-finish 附 (srtp_id, per-user pub, key_mac)；opaque 两侧 session_key 透出；srtp_ids.json 映射 + 网关/serve 内存缓存；v1.1 前置 8B id；CLI 同步（登录后内存持有） | ~250 行 |
| S1 | common：wasm_shim 四导出 + 原生会话包装 + 单测（native 侧 TWriter/TReader 组合） | ~150 行 |
| S2 | ctl：axum ws feature、`/_srtp` 路由 + 会话状态机（泛型通道）+ WS 真socket 单测（dev-dep tokio-tungstenite） | ~300 行 |
| S3 | Caddyfile 渲染：showroom-app 片段加 `/_srtp` 反代（setup 快照测试同步） | ~10 行 |
| S4 | console：srtp.ts（通道/序列化/解析/降级）+ api/upload 接线 + vitest（移植形状测试） | ~400 行 |
| S5 | SPEC §2.2（WS 载波）+ §2.1 补一句「载波信封按载体」+ architecture D22 + deployment | 文档 |
| S6 | 真机 E2E：公网登录全流走 WS-SRTP（journal `srtp-ws 会话建立`/DevTools WS 帧）、LAN 回 Tier 2、上传/下载切片、断连重签重放 | 验收 |

顺序 S1→S6 串行（S2/S4 可并行）；总量约 900 行（含测试），一次批可完成。

## 8. 明确不做（本轮裁决记录）

- 不做 WebTransport/WebRTC（§0）。
- 不在网关实现 WS 终止（serve 一处终止，协议复用最大化）。
- 不做入口 HTTPS/证书（独立事项，另行立项）。
- 不做多请求并发流水线（顺序一比一够用；如未来需要，帧序号化再议）。
- 控制台不渲染通道层级 UI（stderr/journal 可见性即可——控制台零新 UI 面，两态纪律）。
