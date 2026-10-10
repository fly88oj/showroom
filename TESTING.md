# TESTING.md —— 测试纪律与运行指南

本仓库的测试基线：**138 个单元测试，`cargo test --workspace` 约 1.6s、`cargo nextest run` 约 1.1s（32 核开发机，2026-10 实测），无 flaky**。任何改动若让测试变慢、变 flaky 或破坏下列规则，视为回归。

提速与隔离改造的完整背景（审计发现、数字归因、取舍）见本文 §3-§4 与提交 `test: 并行化重构+隔离审计+TESTING.md 纪律`。

## 1. 新测试五规则（写任何测试前读一遍）

1. **独立临时目录**：每个测试的落盘路径必须经由所在文件的 `troot(name)` / `tmp(name)` helper（pid + 原子计数保证每次调用独立），禁止手写 `env::temp_dir().join("固定名")`。教训：`showroom/src/config.rs` 曾用共享父目录 + 固定名，并行测试互相 `remove_dir_all` 造成 flaky（bf542ea 修复）；本次已把全部 helper 统一为计数器化写法（9 个文件），复用该模式即可。目录在 `/tmp` 下由系统清理回收；测试失败时故意不清理（保留现场供排查），成功路径的清理是尽力而为不是义务。
2. **禁全局状态**：禁止 `env::set_var` / `env::remove_var`（进程内所有并行测试共享 env，libtest 与 nextest 都无法隔离）；禁止跨测试共享的可变静态（`static mut`、无锁的全局 `RefCell`）。只读的进程级缓存可以用 `OnceLock`（如 `showroom-common` 测试里的 `platform()` 平台钥匙）。需要"状态"的就显式构造：`serve.rs` 测试的 `AppState` 每测试自建一份。
3. **禁固定端口**：不 bind 端口。HTTP 层测试直调 handler（`api_inner`，见 `serve.rs::tests::call`），不 起 socket；端口只作为配置数据出现在字符串断言里。
4. **显式入参**：被测函数需要的路径/策略/时间一律参数传入，不读真实 HOME、不读仓库外文件。需要 policy 的用 `troot` + 显式写 `state/policy.env`（参考 `publish.rs::tests::neutral_disk_policy`——测试自定中性配额，不依赖宿主机磁盘余量）。例外（只读且结论与宿主机无关，已逐一审定）：`publish.rs::meminfo_read_wiring` 读 `/proc/meminfo`、`import_begin_disk_quota_gate` 跑真实 `df`（reserve 设成天文数字保证必拒）。
5. **超时预算**：单测试目标 < 2s。确需等待真实时间的（如 mtime 秒级精度），用最小必要 sleep 并在慢测试清单（§5）登记原因。禁止网络往返、docker/journalctl/systemctl 真实调用进单测——这些属于 `tests/` 下的手工 e2e（fixtures 在 `tests/hello/`）。

## 2. 运行指南

日常（推荐，已含隔离与提速收益）：

```bash
cargo nextest run --workspace          # 首选：每测试独立进程 + 全二进制并行 + 慢测试排行
cargo test --workspace                 # 备选：零依赖路径（CI 同款），三二进制串行、二进制内并行
cargo test -p showroomctl state::      # 单模块；nextest 同样支持过滤表达式
cargo clippy --workspace --all-targets -- -D warnings   # 必须零告警
```

**并行上限**：libtest 与 nextest 默认并发 = 逻辑核数。本仓库测试是单线程 CPU 密集型（argon2/RSA/AES），满核即恰好饱和、无超订阅问题，**默认值就是推荐值**。需要压上限的场景（共享 CI runner、发热敏感笔记本、与构建并行跑）：

```bash
cargo nextest run --workspace -j 8          # nextest：作业级上限
RUST_TEST_THREADS=8 cargo test --workspace  # libtest：线程级上限
```

内存注记：argon2 rwd 参数 m=19456（19MiB/次），32 并发峰值 ~600MB，属安全范围；再上调并发前先估此项。

**flaky 排查**：`cargo nextest run --workspace --retries 2`；单测试复跑 `cargo nextest run -E 'test(名字)'`。

## 3. 提速配置（根 Cargo.toml，两层结构）

| 层 | 配置 | 治什么 | 实测 |
|---|---|---|---|
| 1 | `[profile.test] opt-level = 1` | AEAD/CTR/GHASH 等**泛型**加密代码在调用方单态化，-O0 测试目标把热循环一起拖下水——package 级覆盖救不了 | 8MiB aes-gcm：0.61s → 0.013s（46×） |
| 2 | `[profile.dev.package.*] opt-level = 3`（rsa、num-bigint-dig、curve25519-dalek、opaque-ke、argon2、aes、aes-gcm、ctr、ghash、polyval、sha2、zstd） | 依赖自身的**非泛型**热函数：RSA-3072 素性检测、argon2 填充循环、ristretto255 群运算 | 见下行总账 |

效果（同机 before → after，2026-10，32 核 Ryzen AI MAX+ 395）：

- `showroom-common` 测试二进制：**~8-17s（波动主因 RSA keygen 随机性）→ 0.2-0.8s**
- `cargo test --workspace` 全量：**15-38s → ~1.6s**
- `cargo nextest run --workspace`：**→ ~1.1s**

维护纪律：新增密码学依赖进 `[dependencies]` 时，评估是否加入第 2 层清单（判据：其热路径是否非泛型、测试是否大量经过）；动 `[profile.*]` 前先跑 `cargo nextest run` 留下基线数字。

## 4. 隔离审计结论（2026-10，bf542ea 后全量复查）

- 固定 /tmp 路径：除 §1.1 所述 helper 计数器化外，无测试写仓库外固定路径。
- 共享 env：无 `env::set_var`/`remove_var`。
- 固定端口：无测试 bind socket；serve 层测试直调 `api_inner`。
- 真实 docker/journalctl：无（生产代码 `dockerops.rs` 有，测试不触；涉及容器语义的路径均以"校验先于 docker 调用"设计，测试只到校验层）。
- 跨测试共享状态文件：无；每测试独立 root，`AppState` 每测试自建。
- 生产代码改动：**零**。本次全部改动在 Cargo.toml、测试 helper 与本文档。

## 5. 慢测试清单（>0.4s 者登记，新增慢测试须写明原因）

| 测试 | 时长 | 原因 |
|---|---|---|
| `showroomctl ops::tests::key_bind_semantics` | ~1.1s | 含 `sleep(1100ms)`：`file_mtime` 秒级精度，需保证第 6 把钥匙 mtime 严格最新以确定 GC 淘汰集合（断言只锁定数量与新钥匙保留，不锁具体淘汰者，见测试内注释） |
| `showroom-common tests::platform_pem_roundtrip` | 0.5-0.8s | RSA-3072 keygen 素性搜索天然随机（`num-bigint-dig` 已 -O3，此为硬件下限） |
| `showroomctl serve::tests::*`（登录族） | 0.3-0.6s | OPAQUE 全流程多轮 + 每测试进程独立 server_setup（nextest 进程模型下不可共享） |
| `showroom-common tests::envelope_roundtrip` | ~0.5s | 跨段（0 / 4MiB / 8MiB+1234B）全量 seal+open，数据量即耗时（AES 已 -O3 + 测试目标 -O1） |

## 6. CI 对应（`.github/workflows/ci.yml`）

- `build-and-test` job：`cargo test --workspace`——与 §2 备选路径同款；§3 的 profile 随 Cargo.toml 走，CI 无需任何额外配置即享受提速（代价：密码学依赖首次构建多花 ~1-2min，`Swatinem/rust-cache` 缓存后摊平）。
- 并发上限：GitHub ubuntu-latest runner 核数少，libtest 默认并发自适应核数，无需干预。
- clippy 门禁在贡献流程（CONTRIBUTING.md）：`cargo clippy --workspace --all-targets -- -D warnings` 必须零告警。
- nextest **未**进 CI：三 crate 二进制本就内建并行，CI 上 nextest 的增量收益（跨二进制重叠）小于安装成本；本地开发建议安装（`cargo install cargo-nextest` 一次性编译成本，换来进程级隔离、重试与慢测试排行）。

## 7. e2e（仓库根 `tests/`）

`tests/hello/` 是手工 e2e 展项 fixture（busybox httpd Dockerfile），不属于 cargo 测试目标（workspace 成员外的目录 cargo 忽略）；跑法见 `docs/operations.md`。Web 控制台的冒烟在 `showroom-web/smoke*.mjs`，由 CI 的 `web-smoke` job 执行。
