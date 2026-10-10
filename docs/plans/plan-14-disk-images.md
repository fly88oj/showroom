# §14 实施方案：镜像删除 + 上传磁盘准入（v1.7）

依据 SPEC §14（573ce01）/§13.3/§10.2；行号锚定 HEAD=d5de35c。词汇按「owner→user 更名已完成」约定：
行文用**用户**；代码符号暂按 HEAD 写 `owner`/`own_check`，更名落地后同步替换（如 `user_check`），语义不变。
纪律：ponytail——复用现有 df 解析、manifest HTTP 客户端、registry-gc 函数体，不引新依赖、不加后台任务。

## 1. 接口定格

### GET /_api/space（签名；无 own_check，全员可看全局磁盘）
```json
响应 200: {"available_mb":<u64>, "reserve_mb":<u64>, "total_mb":<u64>}
```
- `available_mb`＝根 df avail **未扣预留**（MB，`bytes>>20` 向下取整，保守侧）；`reserve_mb`＝`DISK_RESERVE_GB*1024`；
  `total_mb`＝根 df size。控制台显示 `available_mb − reserve_mb`（「可用空间（已扣预留）」）。
- df 失败/畸形 → `500 {"error":"E_STATE","msg":"df /: ..."}`（fail-closed，同 §10.2 meminfo 口径）。

### POST /_api/image-rm（签名；body `{"image":"<repo>:<tag>"}`，如 `"alice/demo:v1"`）
```json
成功 200: {"freed_est_mb":<i64≥0>}
错误: E_NAME  形状非法（无冒号/repo 白名单外/tag 非法/digest 非法）
      E_AUTH  非本人命名空间且非 admin 钥匙（404 同体，零信息暴露）
      E_NOTFOUND manifest GET ≠200（镜像不存在）
      E_STATE  DELETE 失败 / GC 失败（部分成功语义见 §4）
```
- 归属：`state::repo_owned_by(repo, user)`（§13.1 同款）；admin 钥匙全量。

### E_QUOTA 消息模板（原文，import-begin/end 与 publish 复核共用同一纯函数产出）
```
磁盘空间不足：可用 {avail_mb} MB（已预留 {reserve_mb} MB）＜ 估算需求 {need_mb} MB（{sealed_mb} MB × 膨胀因子 {factor}）；请先删除旧镜像后重试
```
`need_mb = ceil(sealed_bytes×factor / 1MiB)`；avail_mb＝未扣预留的根可用。HTTP 429（serve.rs IntoResponse 既有映射）。

### CLI（showroom，用户侧；api.rs signed POST/GET 复用）
```
showroom space                       # 人读三行：可用/预留/总量（含已扣预留值）；--json 原样
showroom images [--json]             # §13.1 端点既有，本方案补 CLI 展示（如 §13 已带则跳过）
showroom rm-image <owner/project:tag># 删除+内联GC，打印「已删除 <image>，回收约 N MB」
```

## 2. 文件级改动清单

### showroomctl/src/state.rs
- `Policy` 增字段 `disk_reserve_gb: u64`（默认 20）、`upload_space_factor: u64`（默认 4）；`load` 增两行解析（照 `redis_dbs` 模式）；新增 `reserve_bytes()`→`disk_reserve_gb<<30`。
- 新增纯函数：
  ```rust
  pub fn disk_admission(sealed_bytes: u64, avail_bytes: u64, reserve_bytes: u64, factor: u64) -> Result<(), String>
  // (avail−reserve) 有符号差 < need → Err(§1 模板)；仿 mem_admission（state.rs:376）
  pub fn split_image_ref(image: &str) -> Option<(String, String)>          // "repo:tag" 一次性 split_once(':')，tag 交 check_tag
  pub fn valid_image_repo(repo: &str) -> bool                              // 每段 [a-z0-9][a-z0-9._-]*，'/' 分段，无 ".."、非空、≤2 段
  pub fn valid_digest(d: &str) -> bool                                     // "sha256:"+64 hex（DELETE 路径拼接前收口）
  pub fn sum_manifest_layers(body: &[u8]) -> Option<u64>                   // JSON：layers[].size 求和+config.size；无 layers（index）→ None
  ```
- `parse_df_bytes`（state.rs:822）**不改**：两列白名单解析与列名无关，`--output=size,avail` 时第二列即 avail（补 doc 注释+测试样例）。

### showroomctl/src/dockerops.rs
- 复用 `http_registry`/`header`/`MANIFEST_ACCEPT`/`manifest_digest`（dockerops.rs:309/347/358）。
- 新增：
  ```rust
  pub fn manifest_size(root,&repo,&tag) -> R<Option<u64>>   // GET manifest：404→Ok(None)；200→state::sum_manifest_layers；index 落 None→再 GET manifests[0] digest 取子 manifest 求和
  pub fn delete_manifest(root,&repo,&digest) -> R<()>       // DELETE /v2/{repo}/manifests/{digest}；202/204/200 收，否则 E_STATE 带 HTTP 码
  ```

### showroomctl/src/ops.rs
- 新增（serve 与潜在 CLI 共用内核）：
  ```rust
  pub fn space_snapshot(root:&Path) -> R<Value>             // 一次 dockerops::run("df",["-B1","--output=size,avail","/"]) → parse_df_bytes → json 三元组；失败 E_STATE
  fn disk_avail_bytes() -> R<u64>                           // 同上仅取 avail（image_rm 前后差用；与 space_snapshot 各自独立调用，不缓存）
  pub fn image_rm(root:&Path, repo:&str, tag:&str) -> R<i64> // §4 流程编排；返回 freed_est_mb
  ```
- `registry_gc`（ops.rs:419）**零改动**：`Cmd::RegistryGc` 的实现体就是它，image_rm 直接调用（无需从 main.rs 提取分支）。

### showroomctl/src/publish.rs（判定插入点）
- 新增 `fn disk_admit(root:&Path, sealed_bytes:u64) -> R<()>`：load policy → `disk_avail_bytes()` → `state::disk_admission(...)` → Err 映射 `ApiError::new("E_QUOTA", m)`。
- `import_begin`（publish.rs:395）：在 `if image_id.is_empty()` 块（:410-412）之后、`check_tag` 之前插 `disk_admit(root, total_bytes)?`。
- `import_end`（publish.rs:463）：在 `have != m.total` 校验（:468-470）之后、调 `import_from_file` 之前插 `disk_admit(root, m.total)?`（收 begin→end 期间空间被并发上传挤占的窗口）。
- `publish`（publish.rs:117-123）：把 `image_exists` 调用点替换为 `manifest_size`——`None → 既有 E_NOTFOUND 文案不变`；`Some(sz) → disk_admit(root, sz)?`（快通道复核，一次 HTTP 兼得存在性与体积）。

### showroomctl/src/serve.rs
- `dispatch` match 增两臂（置于 `/_api/images` 臂 serve.rs:233-239 之后）：
  ```rust
  ("GET", "/_api/space", _) => { let v = blk(move || ops::space_snapshot(&s.root)).await?; Ok(Json(v)) }
  ("POST", "/_api/image-rm", Collected::Mem(b)) => {
      let req: ImageRmReq = jbody(b)?;
      let (repo, tag) = image_guard(s, &req.image, &owner, &kid)?;   // 先守卫后进内核
      let freed = blk(move || ops::image_rm(&s.root, &repo, &tag)).await?;
      Ok(Json(json!({"freed_est_mb": freed})))
  }
  ```
- 新增 `ImageRmReq{image:String}` 与守卫（仿 redis_db_guard serve.rs:622）：
  ```rust
  fn image_guard(s:&S, image:&str, owner:&str, kid:&str) -> R<(String,String)>
  // split_image_ref→None: E_NAME；repo_owned_by || key_is_admin: 放行；否则 E_AUTH（404 同体）
  ```

### showroom/src/*（用户 CLI）
- `main.rs` Cmd 增 `Space`、`Images{json}`（若 §13 未带）、`RmImage{image:String}`；api.rs 走既有 signed GET/POST；rm-image 前打印一行待删镜像名（无交互 confirm，CLI 显式操作）。

### showroom-web/dist/index.html + smoke-wizard.mjs（提交版基础上；见 §7 冲突规避）
- 上传区空间条、镜像列表删除、E_QUOTA 引导（§5）。

### 文档同步（实现代理随代码一并提交）
- SPEC §5 policy.env 样例补 `DISK_RESERVE_GB=20 UPLOAD_SPACE_FACTOR=4`（§14.4 已定值，仅补样例行）；docs/operations.md 补 image-rm/space 两行。

## 3. 空间计算口径

- **df 源＝根分区 `/` 一次调用**。论证：镜像落 `/var/lib/docker`（docker load/pull 的未压缩层，root fs），registry 落 `/srv/showroom/_middleware/registry`（@showroom 子卷），`_tmp` 落 @showroom——三者同属一个 btrfs 文件系统（@ 与 @showroom 为同物理分区兄弟子卷），**btrfs 对任一路径的 df 返回整文件系统统计**，故 `df /` 即三者并集的真值；admin_status（ops.rs:640-651）实测 root/showroom 两行同数可佐证。取根避免双次子进程且天然覆盖 docker 层（这是子卷口径会漏掉的大头）。
- **取整**：字节→MB 一律 `>>20` 向下取整（与 mem_admission 的 kB→MB 同为保守侧）；`need_mb` 用 `(sealed*factor + (1<<20) - 1) >> 20` 向上取整（判定侧保守）。
- **判定式**（§14.2 原文）：`sealed × UPLOAD_SPACE_FACTOR ≤ avail − DISK_RESERVE_GB`；差额有符号运算防下溢（仿 mem_admission）。
- 插入点汇总：import-begin＝publish.rs:412 后；import-end＝publish.rs:470 后；publish 复核＝publish.rs:123 的 image_exists 替换处。

## 4. image-rm 内联 GC 流程（ops::image_rm）

```
1. avail0 = disk_avail_bytes()?
2. digest = dockerops::manifest_digest(root, repo, tag)?          # GET；非200 → manifest_digest 已 E_STATE → 映射 E_NOTFOUND（改：code!=404 才 E_STATE）
3. dockerops::delete_manifest(root, repo, digest)?                # digest 先经 valid_digest
4. ops::registry_gc(root)?                                        # 复用 Cmd::RegistryGc 实现体：停 sr-registry → registry:2
                                                                  # garbage-collect --delete-untagged → 重启（秒级；live-restore 保运行中容器）
5. freed_est_mb = ((disk_avail_bytes()? − avail0) >> 20).max(0)   # 前后 df 差；btrfs 偶发回吐为负时钳 0（如实不虚报）
```
- **失败路径语义（如实报部分成功）**：步骤 4 失败但 3 已成 → 返回 `Err(E_STATE, "manifest 已删但 registry-gc 失败：<原始错误>；空间暂未回收，每周 registry-gc timer 会补收，或稍后重试")`——错误通道承载部分成功信息（响应型只有 error/msg，无第三态，不虚构 freed 值）。步骤 2/3 失败零副作用。
- **blob 共享安全**：`--delete-untagged` 只清无引用 blob，多 tag 共享层天然保留（§14.3）。
- **并发面**：与每周 registry-gc timer 撞车时第二个 `docker stop sr-registry` 失败 → E_STATE 如实报；与 registry 快通道 push 并发时 push 短暂 E_NET，CLI `--via auto` 自回落 import 通道（既有行为）。均罕见且自愈，不加锁（ponytail，与 alloc_port 同判例）。

## 5. 前端改动（提交版 index.html）

- **空间条**：镜像分片上传 fieldset（index.html:214-227）内、`<progress id="up-prog">` 之上插
  `<div id="space-note" class="note">可用空间（已扣预留）：…</div>`。刷新时机：进入展项标签页时、每次 `import-end` 成功后、每次 image-rm 成功后（三处各一行 `refreshSpace()`：GET /_api/space → 显示 `(available_mb-reserve_mb)`，GB 一位小数）。拉取失败静默显示「空间信息不可用」，不阻断上传表单。
- **镜像列表删除**：挂在 §13 落地的镜像选择 UI（部署 ComboBox 同源 `/_api/images` 数据的渲染容器，建议 id=`img-list`）每行尾加「删除」按钮；confirm 文案原文：`确定删除镜像 <repo:tag> 吗？正在使用该镜像的已发布项目不受影响（容器继续运行），但回滚/重建将失效。` 成功后 alert「已回收约 N MB」并刷新列表+空间条。
- **E_QUOTA 引导**：`apiFail`（既有错误出口）增分支：`e.error==='E_QUOTA' && msg 含 '磁盘空间不足'` → 顶部提示追加「可到部署区删除旧镜像后重试」，并刷新空间条。

## 6. 测试清单（每新增纯函数一条最小用例）

1. `state::disk_admission`：`(avail−reserve)` 恰等于 need 放行；差 1MB 拒且 msg 含 avail/reserve/need/factor 四数与「请先删除旧镜像」；`factor=0` 需求 0 恒过；avail<reserve 拒。
2. `state::Policy` 新键：`DISK_RESERVE_GB=5 UPLOAD_SPACE_FACTOR=2` 解析命中；缺省 20/4；非法值回默认。
3. `state::parse_df_bytes` 补样例：`--output=size,avail` 输出（表头+末行两列）解析为 (size, avail)。
4. `state::split_image_ref`+`valid_image_repo`：`alice/demo:v1` 过；`a b/x:v`、`../x:v`、`bare`（无冒号）、`A/b:v`（大写）拒；`alice-x/y` 不误吞（沿用 §13 测试法）。
5. `state::valid_digest`：`sha256:`+64hex 过；短/非 hex/别的算法头拒。
6. `state::sum_manifest_layers`：v2 manifest 样例（3 层+config）求和；index（无 layers）→ None；畸形 JSON → None。
7. `serve::image_guard`：本人 repo 放行；他人 repo/未命名空间 → E_AUTH；admin 钥匙放行他人；无冒号 → E_NAME（仿 redis_db_guard_boundaries 测试体）。
8. `publish::import_begin` 增益：写 policy `DISK_RESERVE_GB` 使 total_bytes×factor 超限 → E_QUOTA 且 msg 含膨胀因子（temp root 打桩 df 不可行——`disk_admit` 的 df 调用抽为参数注入或仅测 state 层，e2e 覆盖）。

## 7. 执行步骤序列（实现代理）

1. `state.rs`：Policy 两字段+五纯函数+单测 1-6 → `cargo test -p showroomctl state::`。
2. `dockerops.rs`：`manifest_size`/`delete_manifest` → `cargo clippy -p showroomctl`。
3. `ops.rs`：`space_snapshot`/`disk_avail_bytes`/`image_rm`（含 manifest_digest 404→E_NOTFOUND 映射微调）。
4. `publish.rs` 三插入点 + `disk_admit`；`serve.rs` 两端点臂+`image_guard`+单测 7 → `cargo test -p showroomctl && cargo clippy -- -D warnings`。
5. `showroom` CLI 三命令 → `cargo build --release` 后本机 `SHOWROOM_ROOT=/srv/showroom target/debug/showroom space` 实测。
6. 前端：index.html+smoke-wizard（§5）→ `node showroom-web/smoke-wizard.mjs`。
7. 文档：SPEC §5 样例两键、operations.md、cli-reference.md 三命令。
8. e2e 实测（§8 实测项）后按仓库规范原子提交。
- **§13 in-flight 冲突**：HEAD 已含 `/_api/images` 端点（d5de35c），前端 ComboBox/镜像列表 UI 在并行工作树未提交——**步骤 6 必须在 §13 前端落地后 rebase 进行**，删除按钮挂其渲染容器（若 §13 UI 尚未合入则仅做空间条+E_QUOTA 引导，删除按钮留待 §14b 增量）；owner→user 更名落地时本方案全部符号/文案一次 sed 级替换（§2 已标注）。serve/publish/ops 改动与 §13 无文件级交叠（其改动集中在 index.html/smoke-wizard），先合后合皆可。

## 8. 验收标准

- [ ] `cargo test -p showroomctl` 全绿（含 §6 新增 7+ 用例）；`cargo clippy -- -D warnings` 零告警。
- [ ] `showroom space` 返回三数，与 `df -B1 --output=size,avail /` 手算一致（MB 向下取整）。
- [ ] **实测：上传大镜像被 E_QUOTA 拒**——调小 `DISK_RESERVE_GB`（如 9999）后 `showroom push`/控制台上传在 import-begin 即 429，msg 含 可用/预留/估算需求/膨胀因子；恢复 policy 后可传。
- [ ] **实测：删除后 df 立即回升**——`showroom rm-image <own/demo:v1>` 秒级返回 freed_est_mb>0，且 `df /` avail 增量与返回值同量级；`/_api/images` 列表不再含该 tag。
- [ ] **实测：一键重传成功**——E_QUOTA 拒传 → 删除旧镜像 → 不改任何配置原 tar 立即重传成功（闭环「上传不够→删除→立即可传」）。
- [ ] 删除他人命名空间镜像 → 404 `{"error":"E_AUTH"}`（与未登记钥匙逐字节同体）。
- [ ] 正在运行项目的镜像被删：容器不停（live-restore），页面仍可访问；msg/confirm 已告知回滚失效语义。
- [ ] blob 共享：两 tag 共享基础层，删其一后另一 tag 仍可 `docker pull 127.0.0.1:5000/...` 成功。
- [ ] 控制台：上传区常显「可用空间（已扣预留）：X GB」；E_QUOTA 错误含删除引导；删除 confirm 文案与 §5 逐字一致。
- [ ] 快通道复核：磁盘告急时 `showroom publish`（镜像已在 registry）在容器创建前被 E_QUOTA 拒，无半开容器（判定在 image_exists 之后、DB 供给之前）。
