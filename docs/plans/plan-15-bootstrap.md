# Plan 15：主机自举（setup）与中间件编排 — 实施方案（SPEC §15）

> 状态：待实施。词汇约定：owner→user 更名已落地，本文一律用 user（现符号 `owner_add/own_check` 等按更名后 `user_add/user_check` 书写）。
> 基准：HEAD d5de35c。§13.3（policy REDIS_DBS）已实现；§14 仅 spec 提交（image-rm/space 端点未落）。

## 0. 主机现状事实（提炼自 deploy/ 与 docs/deployment.md，SSH 不通的兜底基线）

- 根：`/srv/showroom`（btrfs `@showroom` 子卷，fstab 挂载）；目录 `state/{tokens,owners,projects,routes}`、`_middleware/{postgres,redis,registry}`、`_console`、`_tmp`、`_bootstrap`；`state/registry.htpasswd`。
- 容器（手工 docker run，无代码编排）：`sr-postgres`(postgres:16-alpine + 5 个 `-c` 调优参数 + 超级用户密码 env)、`sr-redis`(redis:7-alpine，appendonly+maxmemory 512mb+allkeys-lru，**现网未带 `--databases`**=默认 16)、`sr-registry`(registry:2，127.0.0.1:5000、htpasswd、delete.enabled)。网络 `showroom-net`。
- 单元：`showroom-api.service`（root、`SHOWROOM_BASE_URL` env、Restart=always、WorkingDirectory=/srv/showroom）+ 7 个 timer + dedup timer（`/usr/local/bin/showroom-dedup` 主机脚本，仓库内无源）。
- Caddy：`/etc/caddy/Caddyfile`（常量），snippet `showroom-app` + site A（/v2、/_bootstrap 仅此面）+ site B（公网面，frp 时用）；`deploy/showroom-api.service` 与 `deploy/install.sh` 仍含真实 IP（开源卫生遗留）。
- 调优：`/etc/sysctl.d/99-showroom.conf`（BBR/somaxconn 4096/syn_backlog 8192/port_range/file-max）；journald/watchdog 无既有配置（本方案定义）。

## 1. host.env 契约定格

文件：`state/host.env`（setup 首跑生成；KEY=VALUE，`state::parse_kv` 解析）。

| 键 | 类型 | 默认 | 作用域 |
|---|---|---|---|
| `MIDDLEWARE` | 空格集合 ⊆{postgres,redis,registry} | `postgres registry redis` | 主机：中间件集合 |
| `CADDY` | yes/no | yes | 主机：安装(pacman)+渲染 Caddyfile |
| `LAN_HOST` | str | 首跑 `hostname -I` 首地址 | site A 站点地址（Caddyfile 渲染） |
| `PUBLIC_HOST` | str 可空 | `""` | site B 站点地址；空=纯内网主机，不渲染 site B |
| `TUNING` | none/sysctl | sysctl | 内核调优（sysctl.d 文件 + `sysctl --system`） |
| `DEDUP` | none/weekly | weekly | 周日 05:00 duperemive timer |

解析规则：坏值（未知中间件名/非 yes-no）→ `E_NAME` 且指名键；未知键 → warning 不失败（前向兼容）。**不含任何秘密**：pg 超级用户密码由 setup 生成 32hex 存 `state/middleware.cred`（0600，同 `_internal.cred` 风格），`BASE_URL` 派生自 `LAN_HOST` 不单设。

**与 policy.env 的关系：分层，不合并**。host.env=基础设施形状（装什么、开什么开关）；policy.env=业务限额（MAX_SLOTS/DEFAULT_MEM/MAX_MEM/IMPORT_MAX_GB/ALLOW_LAN_PORT/RESERVE_MEM_MB/REDIS_DBS/〔§14:DISK_RESERVE_GB/UPLOAD_SPACE_FACTOR〕）。论证：两类键的变更主体与风险等级不同——限额是管理员日常调节项（§13.3/§14.4 已落 policy，合并会造成双写漂移），设施是 setup 幂等收敛项；分层让 setup diff 不混入限额噪声。**setup 负责 policy 缺省生成**：`render_policy` 对缺失键补默认值、已有键一律保留。对 §15.1 样例的偏离（REDIS_DBS 等四键从 host.env 移除）按此论证执行，落地时在 SPEC §15.1 加一行勘误注。

## 2. 模块设计（新文件 setup.rs / middleware.rs，均挂 showroomctl）

### 2.1 middleware.rs —— host.env 与中间件集合

```rust
pub struct HostEnv { pub middleware: BTreeSet<String>, pub caddy: bool,
                     pub lan_host: String, pub public_host: String,
                     pub tuning: bool /*TUNING=sysctl*/, pub dedup: bool /*DEDUP=weekly*/ }
impl HostEnv {
    pub fn parse(text: &str) -> R<Self>;                    // 纯函数（规则见 §1）
    pub fn to_env(&self) -> String;                         // 纯函数（键序固定，幂等写盘前提）
    pub fn load(root: &Path) -> R<Self>;                    // 缺文件 → default_for(first_lan_ip())
    pub fn save(&self, root: &Path) -> R<()>;
    pub fn base_url(&self) -> String;                       // http://<LAN_HOST>
}
pub fn validate_mw(name: &str) -> R<()>;                    // ∈ {postgres,redis,registry} 否则 E_NAME
/// 引用该中间件的项目清单（disable 门槛与 --force 列表）：postgres/redis=env 注入对应变量组的项目
/// （redis：conf.redisdb 已分配即算，当前=全部项目）；registry=全部项目（镜像拉取依赖）。
pub fn mw_dependents(root: &Path, mw: &str) -> Vec<String>; // 纯读 all_confs
pub fn middleware_status(root: &Path) -> R<Value>;          // 期望(host.env)/实际(container_status+inspect args)
pub fn middleware_enable(root: &Path, name: &str) -> R<Report>;   // host.env 加成员 → setup::apply(仅该 mw 相关 Action)
pub fn middleware_disable(root: &Path, name: &str, force: bool) -> R<Value>; // 门槛见 §3
pub fn middleware_set(root: &Path, name: &str, k: &str, v: &str, confirm: bool) -> R<Value>; // 仅 redis.databases，其余 E_NAME
pub fn redis_expand(root: &Path, new_dbs: u16, confirm: bool) -> R<Value>;   // §5 状态机
pub fn redis_drift(root: &Path) -> Option<Value>;           // inspect 实例 databases vs policy → 黄条数据
```

### 2.2 setup.rs —— 声明式期望状态

```rust
/// 现状观察值（plan 的唯一世界输入；observe 收集，单测喂 fake）。
pub struct Obs { dirs: BTreeSet<PathBuf>, files: BTreeMap<PathBuf, String>,
                 net: bool, containers: BTreeSet<String>, redis_databases: Option<u16>,
                 pkgs: BTreeSet<String>, units_installed: BTreeSet<String>,
                 units_enabled: BTreeSet<String> }
pub fn observe(root: &Path, host: &HostEnv) -> Obs;         // fs + docker/network/systemctl/pacman 探测

pub enum Action {
    CreateDir    { path: PathBuf, mode: u32 },              // needed: !obs.dirs.contains
    WriteFile    { path: PathBuf, contents: String, mode: u32 }, // needed: 内容不等（或缺）
    BtrfsSubvol  { path: PathBuf },                         // needed: 父目录 btrfs 且 path 不存在；btrfs subvolume show 幂等
    NetworkCreate{ name: String },                          // needed: !obs.net；docker network inspect
    ContainerUp  { spec: ContainerSpec },                   // needed: !obs.containers.contains(name)；参数漂移不重建→warning
    ContainerRm  { name: String },                          // needed: obs.containers.contains；docker rm -f -v 排除 bind 卷
    PackageInstall{ pkg: &'static str },                    // needed: !obs.pkgs.contains；pacman -Qi 已装则跳
    UnitInstall  { name: String, contents: String },        // needed: /etc/systemd/system/<name> 内容不等
    UnitEnable   { name: String, now: bool },               // needed: !obs.units_enabled.contains
    UnitDisableRm{ name: String },                          // disable --now + rm（DEDUP=none / CADDY=no 收敛用）
    Cmd          { what: String, argv: Vec<String> },       // daemon-reload / sysctl --system / caddy validate+reload / 预压缩
}
pub fn action_needed(a: &Action, obs: &Obs) -> bool;        // 幂等判定（每类一臂，纯函数，单测直测）
pub fn plan_setup(host: &HostEnv, obs: &Obs) -> Vec<Action>; // 纯函数（含 redis 漂移 → 不产 Action，产 Warning）
pub struct Report { pub items: Vec<(Action, bool, String)>, pub changed: usize,
                    pub warnings: Vec<String> }             // (action, ok, detail)
pub fn apply(root: &Path, actions: &[Action], dry: bool) -> R<Report>; // dry=只列 needed；逐项执行，单项失败记录继续（容器/单元类失败即中止余项）
pub fn render_caddyfile(mw: &BTreeSet<String>, lan: &str, public: &str) -> String; // 纯函数，快照测试
pub fn render_policy(existing: Option<&str>) -> String;      // 纯函数：缺键补默认、已有保留（§1 键表）
pub fn render_unit_api(host: &HostEnv) -> String;            // deploy/showroom-api.service 模板，Environment=SHOWROOM_BASE_URL 注入 base_url()
pub fn diff_json(host: &HostEnv, actions: &[Action], warnings: &[String]) -> Value;
```

**容器规格常量表**（提炼自 docs/deployment.md §2/§8.2，`ContainerSpec` 常量；`{root}`=SHOWROOM_ROOT）：

```
postgres: docker run -d --name sr-postgres --network showroom-net --restart unless-stopped
  -e POSTGRES_PASSWORD=<state/middleware.cred 首次生成 32hex>
  -v {root}/_middleware/postgres:/var/lib/postgresql/data
  postgres:16-alpine
  -c shared_buffers=256MB -c effective_cache_size=768MB -c work_mem=16MB
  -c max_connections=200 -c autovacuum_naptime=30s
redis:    docker run -d --name sr-redis --network showroom-net --restart unless-stopped
  -v {root}/_middleware/redis:/data
  redis:7-alpine --appendonly yes --databases <policy.REDIS_DBS(默认256)>
  --maxmemory 512mb --maxmemory-policy allkeys-lru
registry: docker run -d --name sr-registry --network showroom-net --restart unless-stopped
  -p 127.0.0.1:5000:5000 -e REGISTRY_STORAGE_DELETE_ENABLED=true
  -e REGISTRY_AUTH=htpasswd -e REGISTRY_AUTH_HTPASSWD_REALM=ShowroomRegistry
  -e REGISTRY_AUTH_HTPASSWD_PATH=/etc/docker/registry/htpasswd
  -v {root}/_middleware/registry:/var/lib/registry
  -v {root}/state/registry.htpasswd:/etc/docker/registry/htpasswd:ro
  registry:2
```

其余渲染产物（Action 生成物，全部 WriteFile+Cmd）：
- `state/policy.env` ← render_policy（既有文件喂 existing）。
- `state/host.env` ← 首跑 HostEnv::to_env。
- Caddyfile ← render_caddyfile（registry ∈ 集合才有 site A 的 `/v2` handle；`/_bootstrap`、`/_console`、routes import 恒在）+ `caddy validate` 后 `caddy reload`。CADDY=no：不产 caddy 相关 Action。
- 单元：`include_str!("../../deploy/showroom-*.service|timer")` 内嵌 + render_unit_api（BASE_URL 注入、清洗真实 IP 遗留）；api UnitEnable(now=true)；timers（reconcile/autoheal/gc-images/gc-registry/registry-gc/snapshot-hourly/snapshot-daily 恒装）；DEDUP=weekly → dedup timer + `WriteFile /usr/local/bin/showroom-dedup`（内嵌 duperemive 增量脚本常量：hashfile 落 `{root}/state/dedup.hash`，`duperemove -d --hashfile` 增量模式，Nice=19 由单元保证）。
- 调优：TUNING=sysctl → WriteFile `/etc/sysctl.d/99-showroom.conf`（§0 六行）+ `sysctl --system`；journald drop-in `SystemMaxUse=2G`；system.conf.d `RuntimeWatchdogSec=10s`。
- 预压缩脚本：`{root}/_console/precompress.sh`（zstd -19 循环，deploy 文档 §5 同义）+ 首跑对存在文件执行一次。
- registry 前置：`touch {root}/state/registry.htpasswd`。

## 3. CLI 命令定格（main.rs 新增）

| 命令 | 参数 | 语义 |
|---|---|---|
| `setup` | `[--apply] [--lan-host H] [--public-host H]` | 无 --apply=diff；--apply 幂等执行。首跑交互：缺 host.env 时用探测值生成并打印（LAN_HOST 可覆写） |
| `upgrade` | `[--bin <路径>]`（默认 /tmp/showroomctl） | §6 |
| `middleware status` | — | 期望/实际/漂移表 |
| `middleware enable` | `<postgres\|redis\|registry>` | 集合加成员 → 局部 apply |
| `middleware disable` | `<名> [--force]` | mw_dependents 非空且无 --force → E_STATE 拒绝并列项目 |
| `middleware set` | `redis databases=N [--confirm]` | §5 扩容流 |

diff 人读样例（`--json` 同构展开）：

```
$ showroomctl setup
host.env: will create /srv/showroom/state/host.env (6 keys)
plan: 21 actions, 21 needed, 0 clean
  + dir      /srv/showroom/state/tokens
  + file     /srv/showroom/state/policy.env (8 keys, 0 kept)
  + network  showroom-net
  + ctr      sr-postgres (postgres:16-alpine)
  + ctr      sr-redis (redis:7-alpine --databases 256)
  + pkg      caddy
  + unit     showroom-api.service (render, enable --now)
  + unit     showroom-dedup.timer (enable)
  + sysctl   /etc/sysctl.d/99-showroom.conf (6 lines)
  ...
warnings:
  redis 实例 databases=16 < policy REDIS_DBS=256 → `showroomctl middleware set redis databases=256`
run `showroomctl setup --apply` to apply
```

`--json`：`{"host_env":{…}, "actions":[{"kind":"ContainerUp","target":"sr-redis","needed":true,"detail":"redis:7-alpine --databases 256"}…], "warnings":[…]}`；apply 后 Report 同构加 `"ok":bool,"result":"…"`. 二次运行 `setup --apply` 期望输出 `plan: 0 actions needed (clean)`。

## 4. admin 端点定格（serve.rs，admin_guard 复用，非管理员=404 E_AUTH 同体现状）

`GET /_api/admin/host`：

```json
{ "host_env": {"MIDDLEWARE":["postgres","redis","registry"],"CADDY":true,"LAN_HOST":"…","PUBLIC_HOST":"","TUNING":"sysctl","DEDUP":"weekly"},
  "policy": {"MAX_SLOTS":30,"REDIS_DBS":256,"RESERVE_MEM_MB":1536},
  "drift": [
    {"item":"sr-redis","expected":"--databases 256","actual":"--databases 16","severity":"warn",
     "advice":"showroomctl middleware set redis databases=256"},
    {"item":"caddy","expected":"installed","actual":"missing","severity":"error","advice":"showroomctl setup --apply"}],
  "redis_pool": {"used":27,"total":256,"headroom":229,"suggest_expand":false},
  "suggestions": ["…"] }
```

- drift 来源：middleware_status + redis_drift + 单元/Caddyfile 存在性（observe 复用）；`severity: error`（缺失/停摆）/`warn`（漂移）。
- `redis_pool`：used=已分配库数（conf 计数）、total=policy.redis_pool_cap()；**余量 <20% → `suggest_expand:true` + suggestions 首条黄条**：「Redis 逻辑库余量不足（剩 X/Y）：建议扩容 `middleware set redis databases=<Z>`」（§15.3，admin/status 响应同源追加 `redis_pool` 字段）。
- `POST /_api/admin/host`（两段式）：body `{"host_env":{"MIDDLEWARE":"postgres registry"} , "confirm":false|true}`——先过 schema 校验（E_NAME）；**无 confirm**：写盘前预演，返回 `{"actions":[同 diff_json], "impact":{"removed_middleware":["redis"],"affected_projects":["demo1","demo2"]}, "confirm_required":true}`；**confirm=true**：HostEnv 落盘 → apply → `{"ok":true,"report":{…}}`。redis 扩容走第二形态 body `{"redis_databases":512,"confirm":…}`（进入 §5 状态机，confirm=false 返回 `prompt` 文案原文 + `confirm_required:true`）。

## 5. Redis 逻辑库扩容确认流（状态机）

`middleware set redis databases=N [--confirm]`：

1. **校验**（纯）：N 为整数且 `16 ≤ N ≤ 256`（上界=Conf.redisdb u8 承载，超界 E_NAME 注明类型约束）；`N > max(已分配 db)+1` 否则 E_NAME「会截断在用库」；`N == 当前实例值` → no-op 报告。
2. **提示**（无 --confirm 只打印不动容器，文案原文）：
   ```
   Redis 逻辑库池将 256 → <N>：需重启 Redis 实例（约 3 秒）。
   - 数据已开 AOF 持久化（appendonly yes），重启不丢已确认写入；
   - 使用 REDIS_* 的应用连接会闪断一次并自动重连（客户端重试期间个别请求可能报错）；
   - 数据卷 _middleware/redis 为 bind 挂载，容器重建不触碰数据。
   确认执行请加 --confirm。
   ```
3. **confirm → 执行**：`docker stop sr-redis` → `docker rm sr-redis` → 按 §2.2 redis 规格带 `--databases N` 起新（同名同卷同网络）。
4. **验证**（30s 上限）：`docker exec sr-redis redis-cli ping` == PONG 且 `redis-cli CONFIG GET databases` 数值 == N。
5. **收尾**：验证通过才写 `policy.env REDIS_DBS=N` + host 侧无其它改动 → 报告 `{"ok":true,"databases":N,"restart_secs":…}`。
6. **失败回滚语义**：步骤 3/4 任一失败 → `docker rm -f` 新容器 → 按**旧 N** 重启同规格容器 → 若旧容器也起不来 → E_STATE 如实报「数据卷未动，手工恢复命令：<docker run 原文>」；policy 恒在验证成功后才写，故任何失败路径下 policy 与实例状态不自相矛盾（最坏=实例旧参数+policy 旧值，即原状）。

## 6. upgrade 语义（二进制自替换安全顺序）

`showroomctl upgrade [--bin <path>]`（root；默认取 /tmp/showroomctl，不存在 E_NOTFOUND）：

1. 新二进制先落 `/usr/local/bin/showroomctl.new`（0755；`<bin> --help` 退出码 0 作最小可用性探针，失败拒替）。
2. 备份旧：`cp -a /usr/local/bin/showroomctl /usr/local/bin/showroomctl.bak-<YYYYmmdd-HHMM>`（保留最近 3 份，更旧自删）。
3. `rename(showroomctl.new → showroomctl)` 原子替换（同文件系统 rename 不受运行中进程 ETXTBSY 影响；禁止 cp 覆盖式写入）。
4. `systemctl restart showroom-api`；3s 后 `is-active` 探活。
5. 内联收敛：对新二进制跑 setup 语义的重渲染（单元/Caddyfile 模板若有变，diff 后 apply）。
6. **失败回滚**：restart 后非 active → `mv bak 回 showroomctl` + restart + 再探活 → 仍失败 E_STATE 报手工路径；两份二进制共存磁盘供手工切换。
7. 附带：/tmp/showroom（user CLI）存在则同步替换 + 刷新 `{root}/_bootstrap/`；**不碰 `_console`**（wasm 产物随发布流程单独更新）。

## 7. docs/deployment.md 改写大纲（三步自举为主，手工降附录）

1. **三步自举**（新 §1）：0 前置检查表（Linux/systemd/Docker≥20/root/btrfs 子卷两条命令+fstab 挂载——唯一保留的手工步，BtrfsSubvol Action 仅覆盖父目录即 btrfs 的顺手场景）；1 取二进制（GitHub Releases 或 cargo build --release，单文件 showroomctl）；2 `showroomctl setup --apply`（含 host.env 首跑说明与 LAN/PUBLIC_HOST 含义表）；3 `showroomctl user-add <USER> --pubkey … --admin` + 交付两样东西（registry 密码、platform_rsa.pub）。
2. **验证**（沿用现 §6：is-active + 未签名 404 自检）。
3. **日常变更**：middleware enable/disable/扩容一条命令表；upgrade；frp 接入（现 §7 保留）。
4. **附录 A 手工全步骤**：现 §1–§3、§5 原文降级（标注「原理参考/排障用，setup 已自动化」）。
5. **附录 B 调优参数表**：pg 六参数、sysctl 六行、policy 键表（§1 键位+分层说明）。
同步：`deploy/install.sh` 头部注明「已被 setup 取代，保留作单元分发参考」；deploy/showroom-api.service 清洗真实 IP（渲染注入）。

## 8. 测试清单（cargo test；纯函数优先，subprocess 仅 observe/apply 薄层）

- **host.env 解析**：parse 缺省/全键/坏值 E_NAME/未知键 warning；to_env↔parse 往返；MIDDLEWARE 空格集合与顺序无关。
- **plan_setup（纯）**：空 Obs（全新主机）→ 21 类 Action 全量序列快照（断言含每类至少 1 个：CreateDir/WriteFile/NetworkCreate/ContainerUp×3/PackageInstall/UnitInstall/UnitEnable/SysctlWrite(WriteFile)/Cmd/BtrfsSubvol）；满 Obs → 空 plan；MIDDLEWARE 去 redis → 无 sr-redis Action 且无 REDIS 相关；CADDY=no → 无 caddy Action；DEDUP=none → 无 dedup 单元且产 UnitDisableRm。
- **action_needed 每类**：CreateDir 存在/缺；WriteFile 同内容/异内容；ContainerUp 容器在/缺；UnitInstall 内容同/异；UnitEnable 已启用/未；NetworkCreate/PackageInstall/BtrfsSubvol/ContainerRm 各正反一例。
- **中间件依赖**：mw_dependents——有项目 conf 时 disable redis 无 --force → E_STATE 且 msg 列全部项目名；--force 放行；registry disable 提示影响文案。
- **render_caddyfile 快照**：三组合——{pg,redis,registry}/{pg,redis}/{pg,registry,redis 缺}：registry 缺 → site A 无 `/v2` 块；PUBLIC_HOST 空 → 无 site B；三组合完整文本快照比对。
- **render_policy**：空→全默认键；已有 MAX_SLOTS=5 → 保留 5 其余补齐；§14 键预留位。
- **redis_expand 校验纯函数**：N≤maxused+1 拒；N>256 拒（msg 含 u8 约束）；N==当前 → no-op。
- **serve 层**：POST /_api/admin/host 无 confirm 不落盘（host.env 字节不变）且返回 impact；非管理员 404 E_AUTH 与未登记逐字节一致（复用现测法）。

## 9. 执行步骤序列、冲突点、验收

**步骤序列**（每步一提交，cargo test 绿后进下一步）：
1. `state.rs`：HostEnv/parse/to_env/render_policy 纯函数+单测（零 subprocess）。
2. `setup.rs`：Action/Obs/action_needed/plan_setup/apply/三渲染函数+内嵌单元与脚本常量（deploy 模板 IP 清洗并入）。
3. `middleware.rs`：status/enable/disable/set/dependents/redis_drift。
4. `main.rs` CLI 接线 + `serve.rs` GET/POST /_api/admin/host（admin_guard+blk 复用模式）。
5. publish/env 自适应：`env_file_contents` 增 mw 集合参数（禁用组不注入），`GET /_api/env`、ps 响应附 `middleware` 标注「未启用」；admin/status 追加 redis_pool。
6. `upgrade` 子命令（§6）。
7. docs/deployment.md 改写（§7 大纲）+ SPEC §15.1 勘误注（键分层）。

**与 in-flight 的交叠/冲突点**：
- **§14 未实现**：DISK_RESERVE_GB/UPLOAD_SPACE_FACTOR 先以注释键占位于 render_policy，值语义随 §14 实现启用——两个 plan 都不得单独删改对方键集。
- **§13.3 半落**：policy REDIS_DBS=256 已实现但现网 redis 实例仍 --databases 16（手工部署遗留）——setup 对已存在容器**不重建**（数据安全优先），只报黄条引导走 §5 扩容流收编；这是首次 setup diff 必现的一条 warning，验收预期内。
- **owner→user 更名**：若更名分支未合入，步骤 5 的 `user_add` 等符号以更名后为准，rebase 时仅改符号不改语义。
- **redisdb u8 上限**：扩容 N≤256 被类型钉死；>256 需先改 Conf.redisdb 为 u16+迁移，本文档明确出范围（msg 如实提示）。
- **showroom-dedup 脚本**：主机现网脚本收编为常量时以实机版本为准对齐（步骤 2 实施时从主机取一次）。

**验收 checklist**：
- [ ] `cargo test` 全绿（§8 清单全过）。
- [ ] 空目录 `SHOWROOM_ROOT=/tmp/sr-fresh showroomctl setup`（dry）→ 列全量 Action；`--apply` 后**再跑 diff=空**（`plan: 0 needed`，幂等核心）。
- [ ] 禁用 redis 且存在项目 conf → 拒绝并列出项目名；`--force` 后 host.env 收敛、后续 publish 产物 env 无 REDIS_*、`/_api/env` 标注「未启用」。
- [ ] 扩容流实测：无 --confirm 只见提示、容器 status 不变；`--confirm` 后容器重建、ping=PONG、CONFIG GET databases=N、policy 同步、报告 ok；同 N 重跑=no-op。
- [ ] GET /_api/admin/host：漂移含 redis 黄条与建议命令；余量<20% 造池（REDIS_DBS=30、29 项目）触发 suggest_expand。
- [ ] upgrade：探针失败拒替；正常路径替换后 is-active=active、未签名 /_api/health 仍 404；模拟 restart 失败自动回滚旧二进制。
- [ ] docs/deployment.md 三步流程在干净 VM 走通至 user-add（或注明留待实机验收项）。
