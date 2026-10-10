# Showroom Protocol Spec v1（crate 间契约 — 改动需同步三处实现）

## 术语
owner=发布者账号；project=展项（全局唯一）；slot=版本槽位（stable/next/pr42…）；tag=镜像 tag。

## 1. 认证：SR-SIG1（所有 /_api 写操作与签名读操作）
```
Authorization: SR-SIG1 <owner>:<unix_ts>:<nonce_hex16>:<sig_b64url>
canonical = "SR-SIG1\n<owner>\n<ts>\n<nonce>\n<METHOD>\n<path_with_query>\n<sha256_hex(body)>"
sig = Ed25519_sign(user_priv, canonical.utf8)
```
服务端校验：owner 已登记、|now−ts|≤300s、nonce（per-owner LRU，重启清零可接受）未见、验签通过。
body 为空时 sha256_hex("")。

## 2. 加密信封 v1（import 通道）
```
k   = 32B random                       # AES-256-GCM 密钥，一次性
wrap= RSA-3072-OAEP-SHA256(platform_pub, k)   # 定长 384 字节
body= wrap || aes256gcm(k, zstd(docker_save_tar))
     # AES-GCM 流式分段：每段 [12B nonce][密文||16B tag]，段大小 4MiB 明文
```
完整性：SR-SIG1 签名已覆盖 sha256(body)（EtM）。
密钥文件：用户 `~/.showroom/id_ed25519`（PEM/PKCS8 或裸 32B seed+pub，实现自定但 CLI 与服务端经 common 互认）；平台对 `/srv/showroom/state/platform_rsa{.pub}`。

## 3. HTTP API（server=showroomctl serve，127.0.0.1:8090，经 Caddy /_api/* 代理）
**零信息暴露原则：/_api/* 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；未认证或验签失败的任何请求（无论路径是否存在、方法是否支持）统一返回 404 + `{"error":"E_AUTH"}`——不泄露路径存在性、版本、项目名等任何管理侧信息。**

| 方法路径 | 请求 | 响应 |
|---|---|---|
| GET /_api/health | - | `{"ok":true}` |
| GET /_api/projects | - | `{"projects":[{project,slot,owner,tag,url,status,port,mem,cpu,updated}]}` |
| POST /_api/publish | `{project,slot,tag,mem?,cpu?,container_port?}` | `{ok,url,port}` |
| POST /_api/stop\|start\|rm\|rollback | `{project,slot}` | `{ok,...}` |
| GET /_api/logs | `?project=&slot=&tail=100` | `{"logs":"..."}` |
| GET /_api/env | `?project=&slot=` | `{"env":{…}}`（发布者查看连接信息的入口） |
| POST /_api/import | query `?project=&slot=&tag=`，body=信封 | `{ok,digest}` |
| POST /_api/import-check | `{image_id}` | `{present:bool}` |

错误统一：HTTP 4xx/5xx + `{"error":"E_CODE","msg":"..."}`；错误码 `E_AUTH/E_NAME/E_QUOTA/E_NOTFOUND/E_STATE/E_NET`（msg 不得包含未登记公钥串以外的内部信息；验签失败与路径不存在不可区分）。
owner 归属校验：签名 owner 必须等于 slot.conf 中 owner（publish 时 project 归属首个创建者）。

**静态面（Caddy）**：`/{project}[/slot]/*` 展项页面公开；`/_bootstrap/*`（CLI/Skill 分发）**仅 site A（内网面）提供，site B（公网面）返回 404**；未来的 `/_console` 公网面仅暴露最小登录壳（登录成功前不加载任何管理资源）。

## 4. 命名与保留
project/slot：`^[a-z0-9][a-z0-9-]{0,30}$`，project 全局唯一，保留：`v2 _api _bootstrap _console`。
镜像：`127.0.0.1:5000/<owner>/<project>:<tag>`（外部经 :80 /v2）。
容器：`sr-<project>-<slot>`；DB：`sr_<project>_<slot>`；槽位端口 127.0.0.1:8100–8199（ports.reg 登记）。

## 5. 主机状态布局（/srv/showroom，btrfs @showroom）
```
state/policy.env        MAX_SLOTS=30 DEFAULT_MEM=256m MAX_MEM=1g IMPORT_MAX_GB=8 ALLOW_LAN_PORT=0
                        # RESERVE_MEM_MB=1536：内存准入预留 MB（§10.2，缺省 1536，可不写）
state/ports.reg         每行 <port> <project> <slot>
state/tokens/<owner>/<kid>.pub    （ed25519 公钥，可多把）
state/owners/<owner>/reg.cred     registry basic_auth user:hash（bcrypt）
state/projects/<project>/<slot>.conf   key=value: owner tag port cport mem cpu redisdb updated
state/projects/<project>/<slot>.env    见下方环境变量契约
state/projects/<project>/<slot>.last2  上一 tag
_middleware/{postgres,redis,registry}/  数据卷(bind)
_tmp/                   import 暂存（即用即删）
```
URL 规则：`/{project}/`（stable 槽位特例：路由写 `<project>-stable` 文件但路径不带 slot）与 `/{project}/{slot}/`。

## 6. 发布者环境变量契约（publish 时自动生成注入，值全部具体化，无占位符遗留）
容器启动 `--env-file` 注入，应用侧按 12-factor 读环境即可；`showroom env` / `GET /_api/env` 可随时查看这些值（连接地址一目了然）：
```
DB_HOST=sr-postgres          DB_PORT=5432
DB_NAME=sr_<project>_<slot>  DB_USER=sr_<project>_<slot>
DB_PASSWORD=<随机生成>
DATABASE_URL=postgresql://<user>:<password>@sr-postgres:5432/<dbname>
REDIS_HOST=sr-redis          REDIS_PORT=6379     REDIS_DB=<0-15 分配>
REDIS_URL=redis://sr-redis:6379/<n>
SHOWROOM_PROJECT=<project>   SHOWROOM_SLOT=<slot>  SHOWROOM_BASE_PATH=/<project>[/<slot>]/
```
发布文档/控制台展示的「如何配置」= 上述键列表 + `showroom env` 输出示例；若发布者镜像内配置文件需要替换，推荐模式为镜像入口脚本里 `envsubst < config.tpl > config`（模板占位符 `${DB_PASSWORD}` 等由容器启动时自动替换，平台只负责注入真实值）。

## 7. 行为约定
- publish：校验名/配额→registry 镜像存在→分配端口→供给 DB/Redis→起容器（滚动：`-new` 后缀起新、健康后删旧改名）→写路由 `reload caddy`→更新 last2。健康=TCP 127.0.0.1:port 可连或 60s 超时放行。
- rollback：读 last2，按其 tag 重跑 publish 内核，旧 tag 写回 last2。
- reconcile：遍历 slot.conf，容器缺失/路由缺失则按 conf 重建（幂等）。
- import：验签→解密到 _tmp（IMPORT_MAX_GB 帽）→`docker load`→`docker tag`→push 127.0.0.1:5000→删临时。
- gc-images/gc-registry/registry-gc/snapshot/autoheal/disksentinel：见设计文档 §6，由 systemd timer 调 `showroomctl <sub>`。

## 8. 選产补篇（v1.1）：分片导入 / 快照 / 控制台
### 8.1 分片导入（大文件断点续传；控制台用，CLI 单发不变）
```
POST /_api/import-begin   {"project","slot","tag","image_id","total_bytes"}  → {"upload_id"}   [签名]
POST /_api/import-chunk?upload=<id>&seq=<n>   body=信封原始字节切片（顺序追加）             [签名，每片独立验签]
GET  /_api/import-status?upload=<id>          → {"received":[seq...],"total"}                [签名]
POST /_api/import-end?upload=<id>             → 拼接后走单发 import 同一管道（解密→load→tag→push→manifest），删会话 [签名]
```
- 服务端会话：_tmp/<upload_id>.bin（追加）+ .meta（owner/project/slot/tag/total/seq 列表，JSON）；seq 必须连续且不重复；total 累计不得超 IMPORT_MAX_GB；end 时字节总数必须等于 total_bytes。
- 完整性：每片独立 SR-SIG1（哈希覆盖该片）；顺序与完整性由服务端 seq/total 强制——整包完整性=片完整性∪顺序完备。
- 陈旧会话（>24h）由 gc-images 顺带清理。
### 8.2 btrfs 快照（showroomctl snapshot hourly|daily）
- `btrfs subvolume snapshot -r /@showroom /@showroom-snaps/<h-|d->YYYYMMDD-HHMM`（/ @showroom-snaps 父子卷惰性创建）；hourly 保留 24 个、daily 保留 7 个（按名排序裁剪最旧）；失败如实报错不 panic。
### 8.3 WASM 控制台（showroom-web）
- Caddy 两站点新增 `handle_path /_console/* { root * /srv/showroom/_console; file_server }`（静态壳不含任何实例数据，登录前不暴露管理信息）。
- 密码学层：showroom-common 增加 `crate-type=cdylib` + `#[no_mangle] extern "C"` 的 wasm32 垫片（keygen/sign_header/seal 会话式 push/finish，指针+长度进出），**不用 wasm-bindgen/wasm-pack**（零新工具链，rustup target 即可）；JS 侧直接 instantiate 调用。
- 登录=密钥即身份（P1 档）：首访生成/导入 Ed25519（存 localStorage，页面明确告知属试验性存储），公钥串展示给管理员登记；OPAQUE 属后续。
- 功能对齐 CLI 只读+发布：列表/发布/停止/启动/删除/回滚/env/logs；镜像上传走 8.1 分片（File.slice 流式 seal，8MB/片，进度条+断点续传）。

## 9. 数据库维护（v1.2）：发布者自助管理 Schema 与数据
**原则**：owner 只能碰自己 project×slot 的库 `sr_<p>_<s>`；全端点签名；下行数据（dump）端到端加密（客户端一次性 RSA 公钥上行，信封同 §2 布局）；publish 改为**首建不重建**（数据保留），显式重置走 db-reset。
**库名归一化**：db_ident = `sr_` + project/slot 中 `-`→`_`（psql -d、CREATE、env 的 DB_NAME/DB_USER/DATABASE_URL 一律用它）；v1.2 前带连字符的存量库在供给/访问时检测旧名存在则沿用，直到 db-reset 重建为新名。
| 端点 | 请求 | 行为 |
|---|---|---|
| POST /_api/db-sql | `{project,slot,sql,confirm}` | `docker exec -i sr-postgres psql -d sr_<p>_<s> -v ON_ERROR_STOP=1` 执行（stdin 传 sql，禁 argv）；confirm!=true 且语句含 DROP/TRUNCATE/DELETE 时拒（E_STATE 提示确认）；返回 `{out}`（psql 文本输出）|
| GET /_api/db-tables | `?project=&slot=` | 列表查询（\dt 等价 SQL）→ `{"tables":[...]}`（控制台 ComboBox 数据源）|
| POST /_api/db-preview | `{project,slot,table,limit≤200}` | `SELECT * LIMIT n` → `{columns,rows}` |
| POST /_api/db-dump-begin | `{project,slot,client_pub_pem}` | v1.3 起改为三段式（begin/chunk/status，见 §10.4）；本行旧单发 base64 形式已移除 |
| POST /_api/db-restore | query `?project=&slot=&confirm=` body=信封（客户端用平台公钥 seal 的 -Fc dump） | 验签→解密到 _tmp→`pg_restore --clean --if-exists`→删临时；走 import 同管道与大帽 |
| POST /_api/db-reset | `{project,slot,confirm}` | DROP DATABASE WITH (FORCE)+重建+重发凭据（env 文件密码不变） |
CLI 对应：`showroom db sql|tables|preview|dump|restore|reset`。公网全可用（签名+加密），无需内网。
### 9.1 控制台（登录后四区，全中文，免装 CLI）
- **展项**：列表（不含槽位端口等内部细节）+ 发布（项目=ComboBox(已有)+可新建、槽位=ComboBox(stable/next/pr42/自定义)、tag=Input(必填)、内存=ComboBox(128m/256m/512m/1g)、Demo 监听端口=数字 Input 默认 8080、镜像 tar 上传=文件选择）。
- **数据库**：表清单 ComboBox→预览表格；SQL 编辑器（唯一自由文本，多行）；备份下载（自动生成一次性密钥对）；恢复上传（confirm 二次确认）；重置（confirm）。
- **状态**：展项健康列表 + 日志查看（自动刷新开关）。
- **帮助**：镜像规范/环境变量清单（§6）/CLI 对照/常见错误表。登录前仅登录界面（零管理信息）。
### 9.2 行为修订（对 §7）
- 根路径 `/` 与未匹配路径：一律 404，不再 302（零信息暴露）。
- publish 供给 DB：库不存在才建（CREATE DATABASE 不存在时）；不再幂等重建。

## 10. v1.3 修订：登录向导 / 内存准入 / Redis 查看
### 10.1 登录改为单一向导（线性三步，替代自由表单）
步骤1「你是谁」：输入账号名（唯一输入）。步骤2「密钥」：自动判断——浏览器已有本账号密钥→默认「使用本机已有密钥」；否则二选一按钮「新设备生成密钥」（推荐）/「从其他设备导入密钥种子」。步骤3「验证」：自动签名自测；未登记→展示公钥串+「已发给管理员，点此重试」循环；通过→进入管理界面。不再并排出现生成/导入/账号三组控件。
### 10.2 内存需求与准入（原「内存上限」更名「内存需求」）
- UI/CLI 文案统一为「内存需求」；语义不变（容器 --memory 硬限）。
- publish 增加准入判断：读 /proc/meminfo 的 MemAvailable，policy 新增 RESERVE_MEM_MB（默认 1536）；`MemAvailable - RESERVE < 需求内存` → E_QUOTA，msg 给出「可用 X MB（已预留 Y MB）＜ 需求 Z MB」。policy.env 样例同步。
### 10.3 Redis 内容查看（排障用，跨 owner 不隔离，注明）
- GET /_api/redis-dbs → {dbs:[0..15]}（静态）
- GET /_api/redis-keys?db=N&q=前缀&limit≤200 → {keys:[...]}（redis-cli --scan --pattern，db=N -n）
- GET /_api/redis-get?db=N&key=K → {type,ttl,value}（redis-cli TYPE/PTTL/按类型 GET/LRANGE/SMEMBERS/HGETALL/ZRANGE）
- 控制台：状态监控页新增「Redis 浏览」区（或独立第五标签）：DB 下拉+key 前缀过滤+点击查看值（等宽展示，值截断 4KB）。
### 10.4 实时下行与低内存大流量传输（ndjson 流 + 分片 dump）
方向性决策：控制台上行全是短请求，无需双向通道——不引 WebSocket，统一用 fetch 流式消费的 ndjson 流（SSE 风格，逐行 JSON）；大体积下行（db-dump）不再整包进内存，改三段式分片。旧 `POST /_api/db-dump`（base64 单发）**直接移除不保留兼容**（未发布给外部），CLI 同步切换。
- **GET /_api/logs-stream ?project&slot&tail（默认 100，上限 10000）** `[签名+own_check]`
  `docker logs -f --tail <n> <容器>`（不加 --timestamps；stdout+stderr 双流合并桥接，逐行封装）→ 响应 `Content-Type: application/x-ndjson`，每行一个 JSON：`{"t":<unix_ms>,"l":"<日志行>"}`（l 经 JSON 转义；t=服务端收到时刻）。客户端断开（连接关闭→响应流 drop→桥接发送失败）即 kill docker logs 子进程并收尸，绝不留孤儿；容器 EOF（被删除）则流正常结束。
- **GET /_api/events** `[签名；全局事件流无 own_check，控制台按容器名 sr- 前缀自行过滤展示]`
  `docker events --filter event=start --filter event=stop --filter event=die --filter event=health_status --format {{json .}}`（事件量小不做容器过滤），逐行解析 JSON 提取字段后封装为 `{"t":<unix_ms>,"e":"<Action>","actor":"<容器名>"}`（t=timeNano/1e6 回落 time*1000；actor=Actor.Attributes.name 回落 Actor.ID；解析失败的行静默丢弃）；同为 x-ndjson 流+断开清理。**心跳**：每 30s 推一行 `{"hb":true}` 防代理空闲断连。
- **db-dump 三段式**（替代 §9 表中 db-dump 行；§9 内存 4× 问题：pg_dump 全量收进 Vec 再 base64 String）：
  1. `POST /_api/db-dump-begin {project,slot,client_pub_pem}` → `{dump_id}` `[签名+own_check]`：校验 PEM（E_NAME）→ 定库 → 立即返回；后台线程执行 `pg_dump -Fc` 直连 `seal(客户端公钥)` **流式**写 `_tmp/<id>.bin`（信封必须一次成包才能解，故生成整体单次、生成过程流式，内存恒定 ~4MiB；明文 >256MB → E_QUOTA，杀子进程）；会话记录于 `_tmp/<id>.meta`（owner/project/slot/db），进度于 `_tmp/<id>.status`（running|ready|failed，failed 附 error，成功附 total=信封字节数）。
  2. `GET /_api/db-dump-chunk ?dump=&offset=&len=` `[签名；meta 校验 owner]`：ready 门（否则 E_STATE）→ 按 offset/len 切片返回 `.bin` 原始字节（`application/octet-stream` + `X-Sr-Total` 头）；len ≤4MB（E_NAME 越界同拒），offset 越过 total → E_NAME，尾片自动钳到 total-offset；**offset+len ≥ total（读完最后一片）即删会话三件套**——多客户端并发拉同一 dump 允许（只读切片），后到者可能 E_NOTFOUND 需重走 begin；24h gc 兜底。
  3. `GET /_api/db-dump-status ?dump=` `[签名；meta 校验 owner]` → `{"ready":bool,"total":n,"error":...}`（轮询用）。
- CLI `showroom db dump` 同步三段式：begin → 1s 轮询 status 至 ready（1h 上限，failed 即时报错）→ 4MB chunk 循环落本地临时信封（每片≤4MB，内存恒定）→ 一次性私钥解密流式写目标文件/stdout。
- 陈旧清理：`_tmp` 下 `.status` 并入 gc-images 的 24h 清理（原有 .bin/.meta 不变）。

## 11. v1.4 修订：移除槽位（slot）模型
**动机**：简化概念（Demo 定向展示）；多版本并存=发布者自建多个相近名称项目；项目名默认随机提升被字典遍历发现的门槛。
- 项目名规则放宽：`^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$`（保留名不变）；**发布入口默认预填随机 `[a-zA-Z0-9]{8}`**，发布者不改直接用。
- 全链路去 slot：API 请求体/查询参数不再含 slot；URL 恒 `/<project>/`；容器名 `sr-<project>`；DB 名 `sr_<project 的 - → _>`（legacy 双名兼容沿用 §9 归一化思路：老 `sr_<p>_<slot>` 检测沿用/迁移）；Redis 逻辑库按项目分配；env 注入去掉 SHOWROOM_SLOT，SHOWROOM_BASE_PATH=/<project>/。
- 状态布局：`state/projects/<project>.conf|.env|.last2`（原 `<project>/<slot>.conf` 迁移：取该目录下唯一 conf 上提为项目级；容器 docker rename sr-<p>-<s> → sr-<p>；routes 文件 <project>.caddy 覆盖写）。端口注册表行去 slot 列。
- 回滚/滚动/首建保留：全部挂在项目级。
- CLI：publish <project> <tag>；ps/logs/env/db*/rollback/stop/start/rm 全部去 slot 实参（旧调用直接报错提示新用法）。
- 控制台：发布表单去槽位字段；项目输入预填随机 8 位（带「换一个」按钮）；URL 提示恒 /<project>/。
- 兼容：`/-api` 无外部存量消费者，不做参数兼容层；主机存量数据（hello/skill-demo）按上述规则现场迁移。

## 12. v1.5：一键部署 + 管理员体系
### 12.1 上传发布一体
- CLI：`showroom deploy [项目] <tag> [--mem --cpu --container-port]` = push（压缩上传）+ publish --wait + 打印 URL 的原子命令（任一步失败即停，报错含已完成的步骤）。
- 控制台：发布表单「一键部署」按钮——选 tar → 前端压缩 → 分片上传 → 自动 publish → 展示 URL；分步状态条（压缩中/上传 x%/发布中/完成）。
### 12.2 管理员（标记在 Key 上）
- state：`tokens/<owner>/<kid>.pub` 旁增加 `tokens/<owner>/<kid>.admin`（空文件存在=管理员钥匙）。
- 引导：主机 CLI `showroomctl owner-add <名> --pubkey <…> --admin` 首个管理员；此后管理员可远程授权。
- 身份接口：`GET /_api/whoami`（签名）→ `{"owner","kid","admin":bool}`——控制台据此显示/隐藏「管理」标签。
- 管理接口（全部签名+管理员钥匙校验，非管理员=404 E_AUTH 与未登记不可区分；公网可用）：
  - `GET /_api/admin/owners` → 账号/钥匙(kid,admin,登记时间)/项目数
  - `POST /_api/admin/owner-add {owner,pubkey,admin?}` → 返回 registry 密码（远程发卡）
  - `POST /_api/admin/owner-revoke {owner}`、`POST /_api/admin/key-rm {owner,kid}`
  - `POST /_api/admin/key-admin {owner,kid,admin}`（授权/收权）
  - `GET /_api/admin/status` → 服务状态(systemd is-active 四服务)、磁盘(根+@showroom)、内存(MemAvailable)、容器列表(名/状态)、timers 下次触发
- 控制台「管理」标签（仅 admin 钥匙可见）：账号与钥匙表格（发卡对话框含公钥粘贴+是否管理员勾选）、状态面板（自动刷新 30s）、快捷动作（收权/删钥匙，confirm）。
- 安全语义：管理员钥匙遗失=平台最高权限泄露，owner-key-rm 单独吊销；whoami 用当前请求的 kid 判定。

## 13. v1.6：部署入口并存 + 镜像选择 + Redis 浏览隔离
### 13.1 发布入口三存（一键不取代分步）
- 「一键部署」：tar→压缩→上传→立即发布（快捷路径）。
- 「仅上传」：只入 registry，不入库部署（发布者择时部署）。
- 「部署」：独立表单——**镜像 ComboBox**（datalist 搜索）列当前可选镜像，默认仅当前用户的（admin 可看全部）；选中后填内存需求/端口→发布（旧版本单独部署即从此入口）。
- 新端点 `GET /_api/images`（签名）→ `{"images":["<owner>/<project>:<tag>",...]}`：registry catalog+tags 合并；按签名用户 owner 前缀过滤；admin 钥匙不过滤。
### 13.2 Redis 浏览按用户隔离（低成本实现，不动 Redis 本体）
- 隔离单位=Redis 逻辑库：`state` 已按项目分配 redisdb（0-15）。`/_api/redis-dbs|keys|get` 增加校验：请求 db ∈ 当前用户名下项目的 redisdb 集合；admin 钥匙放开全量。控制台 DB 下拉仅列自己的（admin 全量，标注「管理员视图」）。
- 诚实边界（文档注明）：容器内应用持有原始连接仍可 SELECT 任意库——这是**界面级/协作级隔离**（防误看防误操作），非安全边界；应用级强隔离需独实例或 Redis ACL+rewrite，成本过高不做。

### 13.3 Redis 逻辑库容量（修订 §13.2 的隐含 16 上限）
- 逻辑库数量=实例配置：sr-redis 启动参数加 `--databases 256`（自管容器，逻辑库仅命名空间无内存代价）。
- 分配池：state 的 redisdb 分配从硬编码 0-15 改为 policy `REDIS_DBS`（默认 256）；池耗尽返回 E_QUOTA（消息含 已用/总量）；policy 与 MAX_SLOTS 一致性由文档说明（默认 30≤256）。
- 分配单位维持**每项目一库**（客户端 URL 零配合、项目间不串键）；用户级隔离只作用于浏览视图（§13.2）。
- 存量兼容：已分配的 0..N 库号不变；容器重建保数据卷。

## 14. v1.7：镜像删除 + 上传磁盘准入
### 14.1 空间口径（显示与判定统一）
- 新端点 `GET /_api/space`（签名）→ `{"available_mb","reserve_mb","total_mb"}`：根分区可用 − policy `DISK_RESERVE_GB`（默认 20；覆盖系统/中间件/应用运行/快照增长）。
- 控制台上传区常显「可用空间（已扣预留）：X GB」。
### 14.2 上传准入（加密导入通道，import-begin 处）
- 判定式：`sealed_size × UPLOAD_SPACE_FACTOR（默认 4）≤ available − reserve`。因子覆盖：解密临时件+解压膨胀（docker load 落 /var/lib/docker 的未压缩层，典型 2–4×）+ registry 压缩副本。
- 不足→ E_QUOTA，消息含 可用/预留/估算需求，提示「请先删除旧镜像」。
- registry 快通道（内网 docker 直推）无 begin 钩子：在 publish 时做同式复核（拉取展开发生在部署时刻）——文档注明不对称性。
### 14.3 镜像删除
- `POST /_api/image-rm {image}`（签名）：归属校验（自己的命名空间 `<owner>/` 前缀，admin 全量）→ registry DELETE manifest（digest 查询后删）→ **内联触发 registry-gc**（停-收-启，秒级；live-restore 保护运行中容器）→ 返回 `{"freed_est_mb"}`。
- 删除即释放空间（不等每周 GC），闭环「上传不够→删除→立即可传」。
- blob 共享安全：多 tag 共享层由 registry-gc --delete-untagged 天然保留仍被引用者。
- 控制台：镜像列表（部署 ComboBox 同源数据）每行删除按钮（confirm，显示镜像名）；上传失败 E_QUOTA 时错误文案引导到删除。
### 14.4 policy 新增
`DISK_RESERVE_GB=20`、`UPLOAD_SPACE_FACTOR=4`（均可调）。

## 15. v1.8：主机自举（setup）与中间件编排
### 15.1 声明式期望状态
- `state/host.env`（setup 生成、可改、幂等应用）：
  ```
  MIDDLEWARE="postgres registry redis"     # 集合：按需增减（如无 redis 主机）
  REDIS_DBS=256  DISK_RESERVE_GB=20  UPLOAD_SPACE_FACTOR=4  RESERVE_MEM_MB=1536
  CADDY=yes  TUNING=sysctl  DEDUP=weekly
  ```
- `showroomctl setup [--apply]`：无 --apply=diff 模式（列出将做的变更与当前偏差）；--apply 幂等执行：目录/btrfs 子卷/policy/env 合成、docker 网络、所选中间件容器（缺则建、多则停删——**有项目引用的中间件禁用需 --force 并列受影响项目**）、caddy 安装(pacman)+Caddyfile 渲染（按中间件集合生成路由）、systemd 单元（api/timers/dedup）安装启用、sysctl/journald/watchdog 调优、预压缩脚本。全新主机流程=下载二进制→`showroomctl setup --apply`→`owner-add --admin`。
- `showroomctl upgrade`：上传新二进制后的就地升级（替换单元/重渲染配置，保数据）。
### 15.2 中间件子命令（CLI+admin 端点同源）
- `showroomctl middleware status|enable <名>|disable <名> [--force]|set <名> <k>=<v>`
- admin 端点：GET /_api/admin/host（期望状态+实际偏差+建议）、POST /_api/admin/host（改 host.env 后自动 setup --apply 的 diff→apply 两段式：先返回变更清单，confirm=true 执行）
- env 注入自适应：禁用的中间件对应变量组（REDIS_*）不再注入，publish 照常；ps/env 标注「未启用」。
### 15.3 Redis 逻辑库自动扩容
- admin/status 与 /_api/admin/host 常驻检查：`已用库数 + 头 room` vs `REDIS_DBS`；余量 <20% 时状态面板黄条提示「建议扩容」。
- 扩容流程（databases 变更需重启实例）：管理员 `showroomctl middleware set redis databases=512`（或 Web 表单）→ showroom 提示「需重启 Redis（约 3 秒，数据 AOF 持久，运行中应用的连接将自动重连）」→ confirm → 停旧容器→新容器带 `--databases N` 起来→自动验证 ping+库数→回报结果。**不静默重启**——用户明确要求提示管理员确认。
- 项目数逼近池上限时 publish 的 redisdb 分配 E_QUOTA 消息直接给扩容命令建议。
### 15.4 分发
- release 二进制经 GitHub Releases（CI 产物）+ 主机 `_bootstrap`；docs/deployment.md 改写为 setup 单命令流程（旧手工步骤降级为附录/原理说明）。
