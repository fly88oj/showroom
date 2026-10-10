# Research：showroom_bg.wasm 混淆/反逆向方案调研

> 状态：结论性调研（无代码变更）。对象：`showroom-web/dist/showroom_bg.wasm`（938KB，release `lto="thin"+strip=true`，wasm32-unknown-unknown，SR-SIG1 签名 + AES-GCM/RSA-OAEP 信封 + zstd 压缩垫片）。
> 环境：本机 wasm-tools 1.261.0（cargo install）、binaryen 121（release 预编译）、wasm-snip 0.4.0，实测 2026-10-11。

## 1. 基线实测：当前产物还剩什么可读信息

段清单（wasm-tools/自写 parser）：仅 type/function/table/memory/global/export/elem/code/data——**无任何 custom 段**（`name`/`producers` 已被 `strip=true` 移除），`wasm-tools strip` 实测为字节级 no-op。

- 导出 29 个：`memory`、19 个 `sr_*`（JS 契约）+ `__getrandom_custom` + 9 个 `rust_zstd_wasm_shim_*`（zstd-sys 自带）。866 个函数全部匿名（`(func (;23;)`）。
- wat 可读性：`wasm-tools print` 得线性栈机码，`sr_sign_header`（func 23）约 633 行 wat；无局部变量名（仅 `$0/$1` 编号）、无类型名。
- 数据段（59,423 B）全文件 ≥10 字符可打印串 325 个，其中**有信息量的**：
  - `"SR-SIG1 "` 头前缀常量（协议字面量，删了功能就坏）；
  - 44 条 panic Location 路径：`…/.cargo/registry/src/…/rsa-0.9.10/src/key.rs`、`aes-0.8.4`、`curve25519-dalek-4.1.3`、`zstd-sys-2.1.1+zstd.1.5.7` 等完整依赖版本指纹；`showroom-common/src/wasm_shim.rs` 自身路径；
  - 12 组 panic/assert 消息（`called \`Option::unwrap\`…` 等 std 标准串）。
- 相邻 `dist/index.html`（121KB 明文 JS）本就含 `"SR-SIG1"` 与全部 `sr_*` 调用点及参数名——wasm 导出名即使改掉，调用方一屏之内全部交代。

## 2. 工具链实测与评估

| 方案 | 实测结果 | 评估 |
|---|---|---|
| 现状（lto+strip） | 基线 | 已达"无符号名"状态 |
| `wasm-opt -O4 --strip-dwarf --strip-producers --strip-target-features --all-features` | 960,938→893,911B（-7%）；可打印串 325→274；registry 路径 44→44；SR-SIG1 仍在 | 纯体积优化，**安全零增益**（无段可删） |
| `wasm-snip --snip-rust-panicking-code --snip-rust-fmt-code` | -2.7%，registry 路径 44→44 | 按函数名定位目标——对已 strip 的产物**找不到目标**，工具停留 wasm-gc 时代 |
| `wasm-tools strip` | 字节相同 | no-op |
| 导出名混淆（`sr_sign_header`→`a23`） | — | 浅混淆：明文 index.html 同源暴露映射；改两端契约换 0 安全增益，**不值** |
| `panic="abort"` / `-Zbuild-std panic_immediate_abort` | 未实测（需 nightly/破坏契约） | 唯一能真正去掉 panic 路径串的杠杆，但 wasm_shim.rs 靠 `catch_unwind` 返回 -1 错误码（shim 注释已自知），abort 会炸掉导出面契约。依赖版本指纹属公开信息（Cargo.lock 同仓库开源），泄露无害 |

## 3. 商业/重型方案成熟度（网络调研）

- **VMProtect：不支持 wasm**（仅 Win PE）。商业 Web 保护的头部产品 Jscrambler/Digital.ai 只做 JS/HTML5，不接 wasm 字节码。
- 学术/开源：Wobfuscator（NDSS'22）方向相反（把 JS 编进 wasm 以躲检测）；WASMixer（2024，自称"首个通用 wasm 二进制混淆器"）、WasmX（GitHub 早期项目，控制流扁平化）均论文/原型级，无生产维护承诺；Tigress 是 C 源码级，不适用 Rust。**结论：wasm 混淆无成熟商用/生产级开源方案**，上原型 = 自担构建管线与正确性风险。

## 4. 诚实评估：门槛与威胁模型

- wasm 结构规整、指令带类型，wabt `wasm-decompile`/Ghidra 均可出伪 C；社区共识是**逆向 wasm 不比逆向 minified JS 难**（甚至更顺）。混淆能抬高的只是"时间成本常数"，对有决心者近似无效。
- 威胁模型消解：**本仓库 MIT/Apache-2.0 全开源**，SR-SIG1 canonical 串、AES-GCM/RSA-OAEP 信封构造、wasm_shim.rs 全部源码就在 GitHub。逆向 wasm 能"偷"到的逻辑，攻击者 `git clone` 一秒钟合法拿到更多；密钥不在 wasm 里（用户 seed 运行时经 `sr_entropy` 注入、平台公钥是公开值），wasm 内**无任何秘密可提取**。逆向的边际收益 ≈ 0。

## 5. 结论：性价比排序（含"什么都不做"）

1. **什么都不做（推荐）**：基线已无符号名/无调试段；逻辑开源、无密钥，混淆无资产可保护；省下构建复杂度与工具链风险。
2. （仅当未来加入闭源逻辑时）`wasm-opt -O4 --all-features --strip-*`：-7% 体积顺带收益，勿以"安全"为由。
3. 依赖版本指纹消除：`panic=abort`+重设计错误码契约（破坏 catch_unwind，需重构 shim）——收益仅是抹掉本就公开的 Cargo.lock 信息，不做。
4. 导出名/自定义段清理：已等效完成或为 no-op，无动作空间。
5. 学术混淆器（WASMixer/WasmX）与商业方案：不成熟/不存在，明确不采用。

## 附：本机实测命令与输出片段

```
$ wasm-tools print showroom_bg.wasm | grep -c "^  (func)"   → 866
$ strings showroom_bg.wasm | grep -oE 'registry/src/…/[a-z-]+-[0-9.]+' | sort -u | wc -l → 19 crates
  （rsa-0.9.10 / aes-0.8.4 / curve25519-dalek-4.1.3 / zstd-sys-2.1.1+zstd.1.5.7 …）
$ wasm-opt t.wasm --all-features -O4 --strip-dwarf --strip-producers … -o t-o4.wasm
  960938 → 893911B；registry 路径 44→44 条不变；`SR-SIG1` 仍在
$ wasm-snip --snip-rust-panicking-code --snip-rust-fmt-code → 934854B，registry 44 条不变
$ wasm-tools strip t.wasm -o t2.wasm && cmp → identical（无 custom 段可删）
```
