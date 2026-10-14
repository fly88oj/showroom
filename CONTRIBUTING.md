# 贡献与提交纪律（Showroom）

本仓库采用 **Conventional Commits 1.0.0** + 原子提交范围约束。开发代理与人类贡献者同等遵守。

## 提交消息格式

```
<type>(<scope>): <主题>

[正文：为什么做这个变更、权衡了什么，72 字符换行]

[脚注：BREAKING CHANGE: … / 关联任务: …]
```

### type（社区标准九类）

`feat`（新功能）· `fix`（缺陷修复）· `docs`（仅文档）· `refactor`（无行为变化的重构）· `test`（仅测试）· `perf`（性能）· `build`（构建/依赖）· `ci`（流水线）· `chore`（杂务）

### scope（本仓库固定词汇表）

`common`（showroom-common）· `cli`（showroom）· `ctl`（showroomctl）· `web`（showroom-web）· `spec`（SPEC.md 契约）· `deploy`（deploy/ 与主机单元）· `skill`（发布 Skill）· `repo`（仓库自身）

### 规则

1. **主题**：祈使句、≤50 字符（中文 ≤25 字），结尾不加句号；含破坏性变更加 `!`（如 `feat(api)!: …`）并在正文写迁移说明。
2. **正文写 why 而非 what**（diff 已经说明 what）；说明取舍与副作用。
3. **语言**：主题与正文用中文；type/scope/脚注关键字保持英文。

## 文件包含范围（原子提交）

1. **一个提交 = 一个逻辑变更**：实现代码与其对应测试**必须同一提交**（不允许"先合实现后补测试"）；纯重构不混入行为变化。
2. **契约随功能同笔**（2026-10-14 用户裁决，替代旧「契约先行拆两笔」）：改协议/行为约定时，SPEC/文档与实现在**同一提交**内变更——文档跟随功能走，不允许独立的 docs(spec) 先行笔；正文写明"实现 SPEC §N"。
3. **禁止入库**：
   - `target/`、`*.zst`（预压缩产物）、`dist/` 之外的生成物、临时脚本；
   - **任何密钥/密码/令牌/凭据**（提交前对 diff 执行敏感词扫描，见检查单）；
   - 调试残留（`eprintln!` 调试输出、注释掉的死代码）。
4. **web 改动检查**：`cd showroom-web/console && npx tsc --noEmit -p tsconfig.app.json && npm test && npm run build`（CI `web-smoke` 已卡同款四连）。
5. **例外**：`showroom-web/console/dist/`（Vite 构建产物，含 `showroom_bg.wasm`）允许入库（部署通道依赖静态产物），但**必须与 shim/前端源码同一提交内重新构建**（`cd showroom-web/console && npm run build`），正文注明"产物已重建"。wasm 从 `console/public/showroom_bg.wasm` 复制，随垫片源码同提交更新。
5. 提交粒度以"可独立 revert 且不破坏其余功能"为准绳。

## 提交前检查单（缺一不可）

```bash
cargo test --workspace          # 全绿（并行纪律与提速配置见 TESTING.md）
cargo clippy --workspace --all-targets -- -D warnings   # 零告警
cargo build --release           # 零 warning
git diff --cached | grep -inE 'password|secret|token|BEGIN .*PRIVATE' && echo "⛔ 敏感内容" || echo OK
```

Shell 脚本改动另跑 `bash -n`。新增/修改测试须遵守 [TESTING.md](TESTING.md) §1 五规则（独立临时目录、禁全局状态、禁固定端口、显式入参、超时预算）。

## 历史重塑政策

- **日常禁止改写已推送的历史**（不 rebase/不 force-push 已公开分支）。
- 唯一例外是**版本定稿窗口**：由维护者一次性 `git filter-repo` / 交互 rebase 重塑并 force-push，重塑后公告（含旧引用清除与凭据轮换评估）。当前仓库处于 v1 定稿窗口，直至宣布定稿。
- 备份性推送（每次改动后 push）与定稿重塑并行不悖：重塑只发生在定稿时刻。

## 测试纪律（并行与资源）

新测试必须：独立临时目录（禁全局状态/固定路径/固定端口）、显式入参、有超时预算；运行方式与并行上限见 `TESTING.md`。
