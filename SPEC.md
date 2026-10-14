# Showroom Protocol Spec v1（crate 间契约 — 改动需同步三处实现）

## 术语
user=发布者账号；project=展项（全局唯一）；tag=镜像 tag。
（slot=版本槽位——**已废弃**：v1.4 全链路移除（§11），模型恒为项目级单路径。）

## 1. 认证：SR-SIG1（所有 /_api 写操作与签名读操作）
```
Authorization: SR-SIG1 <user>:<unix_ts>:<nonce_hex16>:<sig_b64url>
canonical = "SR-SIG1\n<user>\n<ts>\n<nonce>\n<METHOD>\n<path_with_query>\n<sha256_hex(body)>"
sig = Ed25519_sign(user_priv, canonical.utf8)
```
服务端校验：user 已登记、|now−ts|≤300s、nonce（per-user LRU，重启清零可接受）未见、验签通过。
body 为空时 sha256_hex("")。

## 2. 加密信封 v1（import 通道）
```
k   = 32B random                       # AES-256-GCM 密钥，一次性
wrap= RSA-3072-OAEP-SHA256(platform_pub, k)   # 定长 384 字节
body= wrap || aes256gcm(k, zstd(docker_save_tar))
     # AES-GCM 流式分段：每段 [12B nonce][密文||16B tag]，段大小 4MiB 明文
```
完整性：SR-SIG1 签名已覆盖 sha256(body)（EtM）。
密钥文件：用户 `~/.showroom/id_ed25519`（PEM/PKCS8 或裸 32B seed+pub，实现自定但 CLI 与服务端经 common 互认）；平台对 `/srv/showroom/state/platform_rsa{.pub}`——公钥经 whoami `platform_pem` 自动下发（§12.2）/控制台工具下载页，控制台见 §17。

### 2.1 传输信封 SRTP v1（隧道内全流密封；服务端网关 + CLI 客户端已实装）

隧道（frp）是载荷无关的透明字节管道，端点可自行分层。SRTP = 6B 帧头 + TLS 式自定界记录：

```
帧头 = "SRTP" 0x01 <flags>        # flags bit0：0=上行（含 384B wrap）/ 1=下行（无 wrap）
记录 = [12B nonce][4B 大端密文长][密文||16B tag]     # 每条即时可解（逐记录随机 nonce）
上行 = 帧头(0) || 384B wrap || 记录流；记录明文 = 应用帧 [8B 大端长度][载荷]
下行 = 帧头(1) || 记录流（记录明文 = 上游响应原样字节）
```

- 设计依据（两条真机实弹教训）：①§2 定长段（4MiB 全段+EOF 尾段）不适合流式请求——
  尾段只在流结束时可解；②frp 中继无半关闭语义（用户侧 EOF 即拆整流）。故记录自定界、
  请求完结由应用帧界驱动，均不依赖 TCP 流尾。
- 两级处理（客户端裁决）：CLI 公网通道自动探测——协议层可用即全程密封（中转只见
  高熵字节：URL/账号/JSON 体全覆盖）；不可用（非 SRTP 应答/RST，如旧服务端）粘性降级
  为原 reqwest 明文逻辑（本进程内），LAN 通道恒走二级。零配置零感知。
- 混合模式（兼容铁律）：网关按连接首批 6B 探测——非 SRTP 头一律字节级明文直通。
  原有 Docker Push（公网加密 import 通道）与匿名公开访问零变化（import 通道在一级下
  为应用信封外再套传输密封，双重独立）。
- 实现位：`showroom-common/src/srtp.rs`（TWriter/TReader + unwrap_key，格式单源）、
  `showroomctl/src/tunnel/gateway.rs`（FrameFwd 帧转发）、`showroom/src/srtp_transport.rs`
  （CLI 一级 + 粘性降级）。浏览器侧（WS 载波）为后续项。

## 3. HTTP API（server=showroomctl serve，127.0.0.1:8090，经 Caddy /_api/* 代理）
**零信息暴露原则：/_api/* 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；未认证或验签失败的任何请求（无论路径是否存在、方法是否支持）统一返回 404 + `{"error":"E_AUTH"}`——不泄露路径存在性、版本、项目名等任何管理侧信息。**

| 方法路径 | 请求 | 响应 |
|---|---|---|
| GET /_api/health | - | `{"ok":true}` |
| GET /_api/projects | - | `{"projects":[{project,user,tag,url,status,port,mem,cpu,updated}]}` |
| POST /_api/publish | `{project,tag,mem?,cpu?,container_port?}` | `{ok,url,port}` |
| POST /_api/stop\|start\|rm\|rollback | `{project}` | `{ok,...}` |
| GET /_api/logs | `?project=&tail=100` | `{"logs":"..."}` |
| GET /_api/env | `?project=` | `{"env":{…}}`（发布者查看连接信息的入口） |
| POST /_api/import | query `?project=&tag=`，body=信封 | `{ok,digest}` |
| POST /_api/import-check | `{image_id}` | `{present:bool}` |

错误统一：HTTP 4xx/5xx + `{"error":"E_CODE","msg":"..."}`；错误码 `E_AUTH/E_NAME/E_QUOTA/E_NOTFOUND/E_STATE/E_CONFIRM/E_NET`（msg 不得包含未登记公钥串以外的内部信息；验签失败与路径不存在不可区分）。
user 归属校验：签名 user 必须等于 `<project>.conf` 中归属者（磁盘键名 owner=，见 §5；publish 时 project 归属首个创建者）。`GET /_api/projects` 恒只回签名用户本人项目（R4 裁决——跨用户项目名/账号名不对普通登录者枚举）；管理员跨用户全量走 `GET /_api/admin/projects`（§12.2）。

**静态面（Caddy）**：`/{project}/*` 项目页面公开；`_bootstrap/` 目录仅作 `/_api/tool` 工具源——CLI/Skill/公钥登录后控制台内下载（R8 裁决：无静态匿名分发面，`/_bootstrap/*` 不进任何站点）；`/_console` 仅暴露最小登录壳（登录成功前不加载任何管理资源）。

## 4. 命名与保留
project：`^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$`（§11 放宽，大写放开），project 全局唯一，保留：`v2 _api _bootstrap _console`。
镜像：`127.0.0.1:5000/<user>/<project>:<tag>`（外部经 :80 /v2）。
容器：`sr-<project>`；DB：`sr_<project 的 - → _>`（legacy 双名兼容见 §9）；项目端口 127.0.0.1:8100–8199（ports.reg 登记）。

## 5. 主机状态布局（/srv/showroom，btrfs @showroom）
```
state/policy.env        MAX_SLOTS=30 DEFAULT_MEM=256m MAX_MEM=1g IMPORT_MAX_GB=8 ALLOW_LAN_PORT=0
                        # MAX_SLOTS：键名沿用存量文件，§11 起语义=项目数上限（MAX_PROJECTS）
                        # RESERVE_MEM_MB=1536：内存准入预留 MB（§10.2，缺省 1536，可不写）
                        # DISK_RESERVE_GB=20 UPLOAD_SPACE_FACTOR=4：磁盘准入预留 GB 与上传膨胀因子（§14.4，缺省 20/4，可不写）
                        # REDIS_DBS=256：Redis 逻辑库池（§13.3）；BOUND_KEYS_MAX=5：密码绑定钥匙 keep-N（§16.1）
state/ports.reg         每行 <port> <project>（§11 去 slot 列，两列）
state/tokens/<user>/<kid>.pub    （ed25519 公钥，可多把）
state/users/<user>/reg.cred     registry basic_auth user:hash（bcrypt）
state/projects/<project>.conf   key=value: owner tag port cport mem cpu redisdb updated（磁盘键名沿用 owner=）
state/projects/<project>.env    见下方环境变量契约
state/projects/<project>.last2  上一 tag（老项目回滚兜底；现行=versions.json 发布历史）
_middleware/{postgres,redis,registry}/  数据卷(bind)
_tmp/                   import 暂存（即用即删）
```
URL 规则：恒 `/{project}/`（§11 起项目级单路径；路由文件 `routes/<project>.caddy`）。

## 6. 发布者环境变量契约（publish 时自动生成注入，值全部具体化，无占位符遗留；§11 起共 12 键）
容器启动 `--env-file` 注入，应用侧按 12-factor 读环境即可；`showroom env` / `GET /_api/env` 可随时查看这些值（连接地址一目了然）。§15.2 自适应：禁用 redis 中间件时 `REDIS_*` 组（4 键）不注入，其余 8 键照常：
```
DB_HOST=sr-postgres          DB_PORT=5432
DB_NAME=sr_<project 的 - → _>  DB_USER=<同 DB_NAME>
DB_PASSWORD=<随机生成>
DATABASE_URL=postgresql://<user>:<password>@sr-postgres:5432/<dbname>
REDIS_HOST=sr-redis          REDIS_PORT=6379     REDIS_DB=<按项目分配，池 REDIS_DBS 默认 256>
REDIS_URL=redis://sr-redis:6379/<n>
SHOWROOM_PROJECT=<project>   SHOWROOM_BASE_PATH=/<project>/
```
发布文档/控制台展示的「如何配置」= 上述键列表 + `showroom env` 输出示例；若发布者镜像内配置文件需要替换，推荐模式为镜像入口脚本里 `envsubst < config.tpl > config`（模板占位符 `${DB_PASSWORD}` 等由容器启动时自动替换，平台只负责注入真实值）。

## 7. 行为约定
- publish：校验名/配额→registry 镜像存在→分配端口→供给 DB/Redis→起容器（滚动：`-new` 后缀起新、健康后删旧改名）→写路由 `reload caddy`→更新 last2。健康=TCP 127.0.0.1:port 可连或 60s 超时放行。
- rollback：按发布历史 versions.json（append-only；`{project, version?}`，缺省=上一版本）以该版镜像重跑 publish 内核；版本号继续向前分配不消耗；last2 仅老项目（无历史文件）兜底。
- reconcile：遍历 `state/projects/<project>.conf`，容器缺失/路由缺失则按 conf 重建（幂等）。
- import：验签→解密到 _tmp（IMPORT_MAX_GB 帽）→`docker load`→`docker tag`→push 127.0.0.1:5000→删临时。
- gc-images/gc-registry/registry-gc/snapshot/autoheal：见设计文档 §6，由 systemd timer 调 `showroomctl <sub>`。disksentinel（磁盘哨兵）：**待实现**——当前无实现，磁盘水位以 `/_api/space` 与 admin/status 面板为准。

## 8. 分片导入 / 快照 / 控制台垫片
### 8.1 分片导入（大文件断点续传；控制台用，CLI 单发不变）
```
POST /_api/import-begin   {"project","tag","image_id","total_bytes"}  → {"upload_id"}   [签名]
POST /_api/import-chunk?upload=<id>&seq=<n>   body=信封原始字节切片（顺序追加）             [签名，每片独立验签]
GET  /_api/import-status?upload=<id>          → {"received":[seq...],"total"}                [签名]
POST /_api/import-end?upload=<id>             → 拼接后走单发 import 同一管道（解密→load→tag→push→manifest），删会话 [签名]
```
- 服务端会话：_tmp/<upload_id>.bin（追加）+ .meta（user/project/tag/total/seq 列表，JSON）；seq 必须连续且不重复；total 累计不得超 IMPORT_MAX_GB；end 时字节总数必须等于 total_bytes。
- 完整性：每片独立 SR-SIG1（哈希覆盖该片）；顺序与完整性由服务端 seq/total 强制——整包完整性=片完整性∪顺序完备。
- 陈旧会话（>24h）由 gc-images 顺带清理。
### 8.2 btrfs 快照（showroomctl snapshot hourly|daily）
- `btrfs subvolume snapshot -r /@showroom /@showroom-snaps/<h-|d->YYYYMMDD-HHMM`（/ @showroom-snaps 父子卷惰性创建）；hourly 保留 24 个、daily 保留 7 个（按名排序裁剪最旧）；失败如实报错不 panic。
### 8.3 WASM 控制台（showroom-web）
- Caddy 两站点新增 `handle_path /_console/* { root * /srv/showroom/_console; file_server }`（静态壳不含任何实例数据，登录前不暴露管理信息）。
- 密码学层：showroom-common 增加 `crate-type=cdylib` + `#[no_mangle] extern "C"` 的 wasm32 垫片（keygen/sign_header/seal 会话式 push/finish，指针+长度进出），**不用 wasm-bindgen/wasm-pack**（零新工具链，rustup target 即可）；JS 侧直接 instantiate 调用。
- 登录=账号+密码一步（§16，OPAQUE 已落）：页面自动生成/复用本机密钥（localStorage 持久）；密码登录自动绑定本机钥匙；CI/机器用户同样走账号密码 `login`。
- 功能对齐 CLI 只读+发布：列表/发布/停止/启动/删除/回滚/env/logs；镜像上传走 8.1 分片（File.slice 流式 seal，8MB/片，进度条+断点续传）。

## 9. 数据库维护：发布者自助管理 Schema 与数据
**原则**：user 只能碰自己 project 的库 `sr_<p>`；全端点签名；下行数据（dump）端到端加密（客户端一次性 RSA 公钥上行，信封同 §2 布局）；publish 改为**首建不重建**（数据保留），显式重置走 db-reset。
**库名归一化**：db_ident = `sr_` + project 中 `-`→`_`（psql -d、CREATE、env 的 DB_NAME/DB_USER/DATABASE_URL 一律用它）；v1.4 前的存量库（含旧槽位名 `sr_<p>_<s>` 与带连字符名）在供给/访问时检测旧名存在则沿用，直到 db-reset 重建为新名。
| 端点 | 请求 | 行为 |
|---|---|---|
| POST /_api/db-sql | `{project,sql,confirm}` | `docker exec -i sr-postgres psql -d sr_<p> -v ON_ERROR_STOP=1` 执行（stdin 传 sql，禁 argv）；confirm!=true 且语句含 DROP/TRUNCATE/DELETE/ALTER 时拒（**E_CONFIRM**，谓词服务端单源 `sql_needs_confirm`）；返回 `{out, csv}`——单条 SELECT 用 `psql --csv` 执行（out=csv 同文；控制台按 csv 渲染结果表格/多结果 Tab，多语句按分号状态机拆分逐条发送、确认后从断点续发）|
| GET /_api/db-tables | `?project=` | 列表查询（\dt 等价 SQL）→ `{"tables":[...]}`（控制台 ComboBox 数据源）|
| POST /_api/db-preview | `{project,table,limit≤200}` | `SELECT * LIMIT n` → `{columns,rows}` |
| POST /_api/db-dump-begin | `{project,client_pub_pem}` | 备份三段式起点（chunk/status 见 §10.4） |
| POST /_api/db-restore | query `?project=&confirm=` body=信封（客户端用平台公钥 seal 的 -Fc dump） | 验签→解密到 _tmp→`pg_restore --clean --if-exists`→删临时；走 import 同管道与大帽 |
| POST /_api/db-reset | `{project,confirm}` | DROP DATABASE WITH (FORCE)+重建+重发凭据（env 文件密码不变） |
CLI 对应：`showroom db sql|tables|preview|dump|restore|reset`。公网全可用（签名+加密），无需内网。
### 9.1 控制台
UI 与信息架构由 §17 定义（五标签：我的项目/发布/镜像库/数据/帮助，帮助页与管理页各带二级面板；登录前仅渲染登录窗，零管理信息）。本节以下各端点（db-*）的语义不变。
### 9.2 行为细则（对 §7）
- 根路径 `/` 与未匹配路径：一律 404，不再 302（零信息暴露）。
- publish 供给 DB：库不存在才建（CREATE DATABASE 不存在时）；不再幂等重建。

## 10. 登录 / 内存准入 / Redis 查看
### 10.1 登录
控制台一步登录（账号+密码，§16；密钥生成/绑定全程无感）。无向导、无「钥匙登录」折叠入口——CI/机器用户同样用账号密码走 `showroom login`。
### 10.2 内存需求与准入（原「内存上限」更名「内存需求」）
- UI/CLI 文案统一为「内存需求」；语义不变（容器 --memory 硬限）。
- publish 增加准入判断：读 /proc/meminfo 的 MemAvailable，policy 新增 RESERVE_MEM_MB（默认 1536）；`MemAvailable - RESERVE < 需求内存` → E_QUOTA，msg 给出「可用 X MB（已预留 Y MB）＜ 需求 Z MB」。policy.env 样例同步。
### 10.3 Redis 内容查看（排障用，跨 user 不隔离，注明）
- GET /_api/redis-dbs → {dbs:[0..15]}（静态）
- GET /_api/redis-keys?db=N&q=前缀&limit≤200 → {keys:[...]}（redis-cli --scan --pattern，db=N -n）
- GET /_api/redis-get?db=N&key=K → {type,ttl,value}（redis-cli TYPE/PTTL/按类型 GET/LRANGE/SMEMBERS/HGETALL/ZRANGE）
- 控制台：状态监控页新增「Redis 浏览」区（或独立第五标签）：DB 下拉+key 前缀过滤+点击查看值（等宽展示，值截断 4KB）。
### 10.4 实时下行与低内存大流量传输（ndjson 流 + 分片 dump）
方向性决策：控制台上行全是短请求，无需双向通道——不引 WebSocket，统一用 fetch 流式消费的 ndjson 流（SSE 风格，逐行 JSON）；大体积下行（db-dump）不再整包进内存，改三段式分片。旧 `POST /_api/db-dump`（base64 单发）**直接移除不保留兼容**（未发布给外部），CLI 同步切换。
- **GET /_api/logs-stream ?project&tail（默认 100，上限 10000）** `[签名+user_check]`
  `docker logs -f --tail <n> <容器>`（不加 --timestamps；stdout+stderr 双流合并桥接，逐行封装）→ 响应 `Content-Type: application/x-ndjson`，每行一个 JSON：`{"t":<unix_ms>,"l":"<日志行>"}`（l 经 JSON 转义；t=服务端收到时刻）。客户端断开（连接关闭→响应流 drop→桥接发送失败）即 kill docker logs 子进程并收尸，绝不留孤儿；容器 EOF（被删除）则流正常结束。
- **GET /_api/events** `[签名；全局事件流无 user_check，控制台按容器名 sr- 前缀自行过滤展示]`
  `docker events --filter event=start --filter event=stop --filter event=die --filter event=health_status --format {{json .}}`（事件量小不做容器过滤），逐行解析 JSON 提取字段后封装为 `{"t":<unix_ms>,"e":"<Action>","actor":"<容器名>"}`（t=timeNano/1e6 回落 time*1000；actor=Actor.Attributes.name 回落 Actor.ID；解析失败的行静默丢弃）；同为 x-ndjson 流+断开清理。**心跳**：每 30s 推一行 `{"hb":true}` 防代理空闲断连。
- **db-dump 三段式**（替代 §9 表中 db-dump 行；§9 内存 4× 问题：pg_dump 全量收进 Vec 再 base64 String）：
  1. `POST /_api/db-dump-begin {project,client_pub_pem}` → `{dump_id}` `[签名+user_check]`：校验 PEM（E_NAME）→ 定库 → 立即返回；后台线程执行 `pg_dump -Fc` 直连 `seal(客户端公钥)` **流式**写 `_tmp/<id>.bin`（信封必须一次成包才能解，故生成整体单次、生成过程流式，内存恒定 ~4MiB；明文 >256MB → E_QUOTA，杀子进程）；会话记录于 `_tmp/<id>.meta`（user/project/db），进度于 `_tmp/<id>.status`（running|ready|failed，failed 附 error，成功附 total=信封字节数）。
  2. `GET /_api/db-dump-chunk ?dump=&offset=&len=` `[签名；meta 校验 user]`：ready 门（否则 E_STATE）→ 按 offset/len 切片返回 `.bin` 原始字节（`application/octet-stream` + `X-Sr-Total` 头）；len ≤4MB（E_NAME 越界同拒），offset 越过 total → E_NAME，尾片自动钳到 total-offset；**offset+len ≥ total（读完最后一片）即删会话三件套**——多客户端并发拉同一 dump 允许（只读切片），后到者可能 E_NOTFOUND 需重走 begin；24h gc 兜底。
  3. `GET /_api/db-dump-status ?dump=` `[签名；meta 校验 user]` → `{"ready":bool,"total":n,"error":...}`（轮询用）。
- CLI `showroom db dump` 同步三段式：begin → 1s 轮询 status 至 ready（1h 上限，failed 即时报错）→ 4MB chunk 循环落本地临时信封（每片≤4MB，内存恒定）→ 一次性私钥解密流式写目标文件/stdout。
- 陈旧清理：`_tmp` 下 `.status` 并入 gc-images 的 24h 清理（原有 .bin/.meta 不变）。

## 11. 无槽位（slot）模型
**动机**：简化概念（Demo 定向展示）；多版本并存=发布者自建多个相近名称项目；项目名默认随机提升被字典遍历发现的门槛。
- 项目名规则放宽：`^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$`（保留名不变）；**发布入口默认预填随机 `[a-zA-Z0-9]{8}`**，发布者不改直接用。
- 全链路去 slot：API 请求体/查询参数不再含 slot；URL 恒 `/<project>/`；容器名 `sr-<project>`；DB 名 `sr_<project 的 - → _>`（legacy 双名兼容沿用 §9 归一化思路：老 `sr_<p>_<slot>` 检测沿用/迁移）；Redis 逻辑库按项目分配；env 注入去掉 SHOWROOM_SLOT，SHOWROOM_BASE_PATH=/<project>/。
- 状态布局：`state/projects/<project>.conf|.env|.last2`（原 `<project>/<slot>.conf` 迁移：取该目录下唯一 conf 上提为项目级；容器 docker rename sr-<p>-<s> → sr-<p>；routes 文件 <project>.caddy 覆盖写）。端口注册表行去 slot 列。
- 回滚/滚动/首建保留：全部挂在项目级。
- CLI：publish <project> <tag>；ps/logs/env/db*/rollback/stop/start/rm 全部去 slot 实参（旧调用直接报错提示新用法）。
- 控制台：发布表单去槽位字段；项目输入预填随机 8 位（带「换一个」按钮）；URL 提示恒 /<project>/。
- 兼容：`/-api` 无外部存量消费者，不做参数兼容层；主机存量数据（hello/skill-demo）按上述规则现场迁移。

## 12. 快捷部署 + 管理员体系
### 12.1 上传发布一体
- CLI：`showroom deploy [项目] <tag> [--mem --cpu --container-port]` = push（压缩上传）+ publish --wait + 打印 URL 的原子命令（任一步失败即停，报错含已完成的步骤）。
- 控制台：发布表单「发布」按钮——选 tar → 前端压缩 → 分片上传 → 自动 publish → 展示 URL；分步状态条（压缩中/上传 x%/发布中/完成）。
### 12.2 管理员（账号级，R4 裁决；原文「标记在 Key 上」模型已反转）
- state：`users/<user>/admin`（空文件存在=管理员账号）为**唯一真相**；旧钥匙级 `tokens/<user>/<kid>.admin` 与 `users/<user>/admin.pending` 仅作存量迁移输入（serve 启动一次性迁移清除）。
- 引导：主机 CLI `showroomctl user-add <名> --gen-password --admin` 建立首个管理员（`--admin` 即刻落账号级标记；统一账号密码认证，B7）；此后管理员可远程授权。
- 身份接口：`GET /_api/whoami`（签名）→ `{"user","kid","admin":bool,"platform_pem":str}`——admin **按账号判定**（`users/<u>/admin` 存在即真），控制台据此显示/隐藏「管理」标签；`platform_pem` 为平台 RSA 公钥 PEM（读 `state/platform_rsa.pub`，缺文件=空串；公钥本就公开，经签名端点下发零信息暴露），控制台自动缓存到本地（`sr.pempub`）免手工粘贴。
- 管理接口（全部签名+管理员账号校验，非管理员=404 E_AUTH 与未登记不可区分；公网可用）：
  - `GET /_api/admin/users` → 账号（admin 账号级）/钥匙(kid,登记时间)/项目数/has_password（§16 未设密码徽标）
  - `POST /_api/admin/user-add {user,admin?,password|gen_password:true}` → 一次性返回 `{login_password,registry_password}` 两把密码分开发放（§16 §1.3；统一账号密码认证 B7：必须设初始密码，无公钥粘贴路径）
  - `POST /_api/admin/passwd-set {user,password?|gen:true}` → `{password}` 一次性（§16 重置/补发；不动既有钥匙）
  - `POST /_api/admin/user-revoke {user}`、`POST /_api/admin/key-rm {user,kid}`
  - `POST /_api/admin/user-admin {user,admin}`（账号级授权/收权；旧钥匙级端点已移除）
  - `GET /_api/admin/status` → 服务状态(systemd is-active 四服务)、磁盘(根+@showroom)、内存(MemAvailable)、容器列表(名/状态)、timers 下次触发
  - `GET /_api/admin/projects` → 跨用户全量项目（每行附 owner=user；§3 `/_api/projects` 只回本人的管理员侧补充）
- 控制台「管理」标签（仅管理员账号可见）五面板（§17.1）：账号（账号/设备表、发卡恒设密码、账号行「设为/取消管理员」）、全部项目、平台状态、宿主配置、实时事件。
- 安全语义：管理员收授按账号（`user-admin`，对整账号全部钥匙生效）；删钥匙不影响管理员身份；whoami 按账号判定。

## 13. 镜像库 / Redis 浏览隔离
### 13.1 发布与镜像源
UI：发布页单表单双镜像源（tar 上传→压缩→上传→发布；镜像库源选已有镜像直接发布）；「上传入库」在镜像库页（与项目无关，镜像名取自 tar）——见 §17.4。
端点：`GET /_api/images`（签名）→ `{"images":["<user>/<project>:<tag>",...]}`（registry catalog+tags 合并；按签名用户 user 前缀过滤，管理员不过滤）。CLI `showroom deploy|push|publish` 三命令不变。
### 13.2 Redis 浏览按用户隔离（低成本实现，不动 Redis 本体）
- 隔离单位=Redis 逻辑库：`state` 已按项目分配 redisdb（池 REDIS_DBS，§13.3）。`/_api/redis-dbs|keys|get` 增加校验：请求 db ∈ 当前用户名下项目的 redisdb 集合；管理员放开全量。控制台 DB 下拉**显示项目名而非库号**（`/_api/redis-dbs` 附 entries=db→project 映射；管理员全池，未分配库显示占位）。
- 诚实边界（文档注明）：容器内应用持有原始连接仍可 SELECT 任意库——这是**界面级/协作级隔离**（防误看防误操作），非安全边界；应用级强隔离需独实例或 Redis ACL+rewrite，成本过高不做。

### 13.3 Redis 逻辑库容量（databases 16–256）
- 逻辑库数量=实例配置：sr-redis 启动参数加 `--databases 256`（自管容器，逻辑库仅命名空间无内存代价）。
- 分配池：state 的 redisdb 分配从硬编码 0-15 改为 policy `REDIS_DBS`（默认 256）；池耗尽返回 E_QUOTA（消息含 已用/总量）；policy 与 MAX_SLOTS 一致性由文档说明（默认 30≤256）。
- 分配单位维持**每项目一库**（客户端 URL 零配合、项目间不串键）；用户级隔离只作用于浏览视图（§13.2）。
- 存量兼容：已分配的 0..N 库号不变；容器重建保数据卷。

## 14. 镜像删除与上传磁盘准入
### 14.1 空间口径（显示与判定统一）
- 新端点 `GET /_api/space`（签名）→ `{"available_mb","reserve_mb","total_mb"}`：根分区可用 − policy `DISK_RESERVE_GB`（默认 20；覆盖系统/中间件/应用运行/快照增长）。
- 控制台上传区常显「可用空间：X GB」。
### 14.2 上传准入（加密导入通道，import-begin 处）
- 判定式：`sealed_size × UPLOAD_SPACE_FACTOR（默认 4）≤ available − reserve`。因子覆盖：解密临时件+解压膨胀（docker load 落 /var/lib/docker 的未压缩层，典型 2–4×）+ registry 压缩副本。
- 不足→ E_QUOTA，消息含 可用/预留/估算需求，提示「请先删除旧镜像」。
- registry 快通道（内网 docker 直推）无 begin 钩子：在 publish 时做同式复核（拉取展开发生在部署时刻）——文档注明不对称性。
### 14.3 镜像删除
- `POST /_api/image-rm {image}`（签名）：归属校验（自己的命名空间 `<user>/` 前缀，admin 全量）→ registry DELETE manifest（digest 查询后删）→ **内联触发 registry-gc**（停-收-启，秒级；live-restore 保护运行中容器）→ 返回 `{"freed_est_mb"}`。
- 删除即释放空间（不等每周 GC），闭环「上传不够→删除→立即可传」。
- blob 共享安全：多 tag 共享层由 registry-gc --delete-untagged 天然保留仍被引用者。
- 控制台：镜像列表（部署 ComboBox 同源数据）每行删除按钮（confirm，显示镜像名）；上传失败 E_QUOTA 时错误文案引导到删除。
### 14.4 policy 新增
`DISK_RESERVE_GB=20`、`UPLOAD_SPACE_FACTOR=4`（均可调）。

## 15. 主机自举（setup）与中间件编排
### 15.1 声明式期望状态
- `state/host.env`（setup 生成、可改、幂等应用）：
  ```
  MIDDLEWARE="postgres registry redis"     # 集合：按需增减（如无 redis 主机）
  CADDY=yes  LAN_HOST=<内网地址>  PUBLIC_HOST=  TUNING=sysctl  DEDUP=weekly
  FRP=none  FRP_TOKEN=  FRP_TUNNEL=  FRP_TOML=   # 隧道（整文件 0600；见下行注）
  ```
  > 隧道并入 serve（D22）：`FRP=chmlfrp` 走 getcfg 拉取面板 ini（FRP_TOKEN/FRP_TUNNEL；
  > 多隧道=FRP_TUNNEL 逗号列表——**每 ID 独立会话**（实测：getcfg 多 ID 合并调用仅支持
  > 同节点，跨节点双入口必须分会话；历史键 FRP_TUNNEL2 解析时折叠）；`FRP=native` 时
  > FRP_TOML=指向完整 frpc TOML 的**文件路径**（自建 frps/其它中转商；host.env 是单行
  > kv，内联多行 TOML 存活不了落盘往返——TOML 连同凭据住独立 0600 文件如
  > /etc/showroom/frpc-native.toml，**会话重建时**重读：frpc 内层重连不重读配置）。
  > **热重载**：FRP 四键任一变化（控制台两段式/setup --apply/手工编辑，均落盘 host.env）
  → serve 内隧道模块 ≤2s 检出签名变化 → 中止旧会话任务、按新源重建，无需重启。
  > 不再有独立 frpc 单元与二进制下载。
  > 勘误（plan-15 落地）：REDIS_DBS/DISK_RESERVE_GB/UPLOAD_SPACE_FACTOR/RESERVE_MEM_MB
  > 属业务限额，归 `state/policy.env`（§13.3/§14.4 已落），不在 host.env——两类键的变更主体
  > 与风险等级不同（限额=管理员日常调节；设施=setup 幂等收敛），分层不合并以免双写漂移。
  > 键分层论证与全集见 docs/plans/plan-15-bootstrap.md §1。首跑限额缺省随实测硬件推导
  > （hw_defaults），用户显式值永远优先。
- `showroomctl setup [--apply]`：无 --apply=diff 模式（列出将做的变更与当前偏差）；--apply 幂等执行：目录/btrfs 子卷/policy/env 合成、docker 网络、所选中间件容器（缺则建、多则停删——**有项目引用的中间件禁用需 --force 并列受影响项目**）、caddy 安装(pacman)+Caddyfile 渲染（按中间件集合生成路由）、systemd 单元（api/timers/dedup）安装启用、sysctl/journald/watchdog 调优、预压缩脚本。全新主机流程=下载二进制→`showroomctl setup --apply`→`user-add --admin`。
- `showroomctl upgrade`：上传新二进制后的就地升级（替换单元/重渲染配置，保数据）。
### 15.2 中间件子命令（CLI+admin 端点同源）
- `showroomctl middleware status|enable <名>|disable <名> [--force]|set <名> <k>=<v>`
- admin 端点：GET /_api/admin/host（期望状态+实际偏差+建议）、POST /_api/admin/host（改 host.env 后自动 setup --apply 的 diff→apply 两段式：先返回变更清单，confirm=true 执行）
- env 注入自适应：禁用的中间件对应变量组（REDIS_*）不再注入，publish 照常；ps/env 标注「未启用」。
### 15.3 Redis 逻辑库自动扩容
- admin/status 与 /_api/admin/host 常驻检查：`已用库数 + 头 room` vs `REDIS_DBS`；余量 <20% 时状态面板黄条提示「建议扩容」。
- 扩容流程（databases 变更需重启实例）：管理员 `showroomctl middleware set redis databases=256`（或 Web 表单；取值 16–256——Conf.redisdb 为 u8，单实例至多 256 库）→ showroom 提示「需重启 Redis（约 3 秒，数据 AOF 持久，运行中应用的连接将自动重连）」→ confirm → 停旧容器→新容器带 `--databases N` 起来→自动验证 ping+库数→回报结果。**不静默重启**——用户明确要求提示管理员确认。
- 项目数逼近池上限时 publish 的 redisdb 分配 E_QUOTA 消息直接给扩容命令建议。
### 15.4 分发
- release 二进制经 GitHub Releases（CI 产物）+ 主机 `_bootstrap`；docs/deployment.md 改写为 setup 单命令流程（旧手工步骤降级为附录/原理说明）。

## 16. 认证（密码为主 / 密钥无感）
目标体验：控制台一步登录（账号+密码），密钥生成/绑定/签名全程无感；管理员发账号时设初始密码；
CLI `showroom login` 一次后全部子命令免密码（gh/ssh 模型）；CI/机器用户同样走账号密码 login。

### 16.1 登录协议：OPAQUE（RFC 9380，opaque-ke 3.x，Ristretto255 suite）
- **选型**：OPAQUE——密码经盲化 OPRF 永不上线；服务端只存注册记录（信封），无离线可验证哈希；
  每次 KE 全新临时份额天然防重放。非 PAKE 的挑战/签名方案（H(pw,挑战) 上网线即可被离线字典）
  与 HKDF(pw) 对称凭证（可重放 bearer）均被否决。实现收口 showroom-common（两端同 crate 同
  Suite，无互操作缝隙；argon2id 硬化由 rwd() 在盲化前外部承担——参数 m=19456KiB/t=2/p=1（OWASP），
  盐=b"showroom-opaque-v1"+user，native/wasm 固定向量测试防漂移）。
- **存储**（§5 增补）：`state/opaque.key`（服务端 OPRF key，b64 0600，serve 启动缺失自动生成）；
  `state/users/<u>/pass.opaque`（注册记录 b64 0600；user-revoke 连 users 目录整删）；
  `state/tokens/<u>/<kid>.bound`（空文件=密码绑定钥匙，受 keep-N GC）；policy.env `BOUND_KEYS_MAX`
  （默认 5）。**两把密码分开发放**：登录密码（OPAQUE 保护）与 registry 密码（bcrypt，随 docker
  push 以 basic-auth 走线，不可与登录密码合并——零暴露前提与轮换独立性论证见 plan-16 §1.3）。
- **报文序列**（C=浏览器或 CLI，均走 common 同一客户端）：
```
C→S POST /_api/auth/login-begin  {"user":u,"ke1":b64}          # 未签名
S→C 200 {"ke2":b64}   # 未知用户/未设密码 → crate 原生 dummy 假记录路径，仍 200（防枚举）；
                      # 密码错误：客户端本地 finish 即失败，不产生第三请求（无线上差异）
C→S POST /_api/auth/login-finish {"user":u,"ke3":b64}          # ke3=客户端 MAC
S→C 200 {"bind_token":hex}   # 60s 有效、单次使用（内存）
C→S POST /_api/auth/key-bind {"user":u,"pubkey":串}
     Authorization: SR-SIG1 …  # 用候选钥匙签名、服务端按 body 内公钥验签（非已登记钥匙）
     X-Sr-Bind: <bind_token>
S→C 200 {"kid","admin"}   # 写 <kid>.pub+.bound；admin=账号级结论回传（users/<u>/admin 唯一
                          # 真相，R4）；GC 旧 .bound 钥匙
                          #（按 .pub mtime 保最近 BOUND_KEYS_MAX 把）；
                          # 幂等（同钥匙重绑=ok）
# 改密码（登录态。控制台要求输入当前密码并先本地校验——复用 login-begin+本地 finish、
#   不调 login-finish 即不产生登录态；校验通过才走下列两步。旧密码不上线，签名即会话证明）：
C→S POST /_api/auth/pw-begin  {"user":u,"m1":b64}    [SR-SIG1 常规]
S→C 200 {"m2":b64}                                  # 在途注册态内存保存，60s
C→S POST /_api/auth/pw-commit {"user":u,"record":b64} [SR-SIG1 常规]  # 原子替换记录
# 下线（CLI logout 恒为 / 控制台登出）：
C→S POST /_api/key-selfrm {"kid":k} [SR-SIG1]   # 仅允许删除请求签名所用 kid 自己
# 登录后信任通道下发 registry 凭据（CLI login 免手工 config）：
C→S GET  /_api/reg-cred [SR-SIG1] → {"user","registry_password"}  # 暴露面=已绑定钥匙=账号主人
```
- **零信息与防滥用**：/_api/auth/* 是唯一应答未签名请求的端点族；畸形/限流/ke3 拒绝/族内未知
  路径一律 404 `{"error":"E_AUTH"}`（与未知路径逐字节一致）；login-begin 按 user（10 次/10 分）
  +全局（120 次/10 分）双令牌桶（内存，超限同 404）；服务重启丢在途登录态，重试即可。
- **CLI**：`showroom login [账号] [--server-lan URL] [--ephemeral]`（rpassword 交互读密码，不回显
  不进 argv/env/ps）→ 上述序列 → 默认持久化（seed 落 key_path 0600、config 写 user/server_lan/
  registry 凭据）；`--ephemeral` 不落任何盘（服务端 keep-N GC 收尾）。`showroom logout`
  清本地并恒为 key-selfrm 远端注销。keygen 保留为隐藏调试命令（生成/重置本机签名密钥；统一账号
  密码认证 B7 后不再预登记公钥）。doctor：未登录/自测 E_AUTH → 提示 showroom login。

### 16.2 迁移与边界
- 存量 users 无 pass.opaque → 密码登录不可用（login-begin 走 UnknownUser 同形态）；控制台文案
  指路联系管理员（passwd-set 补发）；已有 localStorage seed / CLI 存量钥匙用户零变化。
- **风险边界（如实声明）**：明文 HTTP 首登无信任锚——PAKE 只保证密码不泄露，主动 MITM 可中继
  login-finish 并在 key-bind 登记自己的公钥接管账号（缓解：公网站点 B 经 Caddy TLS；LAN 不可信
  时用 CLI 钥匙路径）；UnknownUser 假记录在响应长度/时序上有统计残差（枚举成本从一次请求升到
  时序分析）；限流按 user 计（代理后取不到可信 IP），全局桶+rwd 计算成本抬高单价。

## 17. 控制台信息架构（plan-17 定稿）
控制台为 Vite+React+TS+shadcn/ui 工程（`showroom-web/console/`，懒加载分包、`base=/_console/`、产物 dist/ 入库），wasm 垫片装载与密码学约定不变（零 wasm-bindgen）；未登录仅渲染登录窗（零管理信息暴露）。
### 17.1 导航与层级
- 顶栏单行（≤56px sticky）：账号徽标（管理员加「管」角标，无角色徽标——R4 去冗余）+ 用户菜单（设置 / 修改密码… / 退出登录——改密入口归菜单，设置弹窗只剩对外域名+PEM 两字段）（无常驻 CTA，发布入口即「发布」标签）。
- 五标签（role=tablist 方向键）：**我的项目**（默认落地）/**发布**/**镜像库**/**数据**（DB+Redis 合并）/**帮助**（二级菜单五面板：账号/CLI 使用/镜像规范/工具下载/错误码）；「管理」标签仅 whoami admin=true 渲染，内部同为五面板（账号/全部项目/平台状态/宿主配置/实时事件；docker events 原始流降为管理员工具）。
- 三级封顶：顶栏→标签→卡片；卡片=标题+副题（纯状态文本，自动刷新兜底，无「已加载 N 项」可点重取入口）+内容；卡片内禁嵌 fieldset。
### 17.2 数据自动加载（显式刷新按钮全站归零）
登录即 4 路并发 `Promise.allSettled`（projects/images/space/whoami）；进页即订阅事件流（项目页可见时；`document.hidden`/离页退订）。
- projects：TTL 30s；publish/start/stop/rm/rollback 成功后与事件流 sr-* 2s 去抖强制失效；失败=表格内联「加载失败 · 原因 + 重试」+3s 自动重试 1 次，此后每 10s 慢速兜底重试直至恢复（验收 8 恢复后免刷新自动补齐；images/space 同机制）。
- images：TTL 300s；import-end/image-rm 后失效；whoami admin 后重取（无「管理员视图」徽章——身份由顶栏「管」角标表达）。
- space：TTL 60s；切镜像库页强制校准；失败静默「空间信息不可用」。
- whoami（+`platform_pem`，§12.2）：失败=顶部 warn 条（「当前管理功能保持不变」）+60s 周期重试；网络失败不收回已渲染的管理功能。
- redis-dbs：数据页 Redis 区首次展开惰性；下拉显示项目名而非库号（entries=db→project 映射；管理员全池，未分配库占位）；行抽屉日志流展开即订阅、收起/换行即断。
- 数据库页项目下拉、发布页项目/镜像下拉直读已加载 `state.projects`/images——独立加载动作归零。
### 17.3 项目 7 态状态机（显示层只出 7 词；「未部署」「missing」原词不进 UI）
| 状态 | 表现 | 可执行动作 | 来源 |
|---|---|---|---|
| 运行中 | 绿● | 更新·停止·更多▾(回滚/环境变量/数据/删除)·日志▾ | `status=running` |
| 已停止 | 黄●（tooltip=后端原词 exited/paused/created） | 启动·更新·更多▾·日志▾ | exited/paused/created |
| 重建中 | 黄●呼吸（reduced-motion 静止） | 日志▾ | restarting，或发布成功后 60s 滚动窗口 |
| 异常 | 红●（tooltip=dead） | 更新·删除·日志▾ | dead，或本地发布失败终态 |
| 待发布 | 灰●（tooltip「仅镜像无容器：从发布页选择镜像即可拉起」） | 发布（→发布页预填）·删除 | missing（conf 在、容器无） |
| 上传中 | 行内「上传中 x%」（dp 状态条为主显示） | 等待；失败断点续传 | 本地 deploy 在途 |
| 发布中 | 黄●发布中… | 等待 | 本地 publish 在途 |
状态标识=●+文字双通道（非仅颜色）；无「打开」按钮——URL 列即外链；行操作固定序「发布（更新）/停止(启动)/更多▾」行尾+日志列「▾」。
### 17.4 发布单表单与参数单源
- 发布页单表单：项目（可输可选，留空自动随机 8 位 crypto）+ 镜像源（**上传镜像 tar** / **镜像库**下拉）+ 版本（服务端自增分配，只读预览「将发布为 vN」）+ 部署参数折叠区（内存需求/容器端口；选已有项目自动回填，改动琥珀高亮）。
- 镜像库源选中即带出 tag 与项目，①压缩②上传跳过直接 publish；tar 源走完整四步（压缩→上传→发布→完成状态条，失败可断点续传重试）。
- 「上传入库」在镜像库页（与项目无关——镜像名取自 tar）；发布页恒完整发布。
- 预填：项目命中已有行 → 内存=服务端 conf（GET /_api/projects 权威）、标签/端口=本机 `sr.conf.*` 记忆（publish 成功写 `{tag,cport,ts}`）+ 注记「沿用上次发布参数（MM-DD），可修改」；行「更新」跳发布页预填同一套。
### 17.5 URL 展示策略
主链接一律 `externalUrl()=base+/<project>/`：base ① 设置 `sr.publicBase`（设置对话框「对外访问域名」）→ ② `location.origin`（host 非私网）→ ③ 兜底 origin+列表「正在用内网地址展示 URL」提示条。服务端登记 `p.url` 永不作主链接，仅 tooltip。
### 17.6 设置对话框（#dlg-settings）
对外访问域名（校验 http(s)、去尾斜杠、清空回退）+ 平台公钥 PEM（whoami `platform_pem` 自动取+缓存 `sr.pempub`，手工粘贴兜底；未配置时发布内联引导「去设置」）+ 修改密码/忘记此设备入口聚合。
### 17.7 关键交互契约
- 删除项目 confirm 必含「数据库将一并删除，如需保留请先在数据页备份下载 .dump」。
- 登录成功焦点引导：已有项目→URL 列首链接；空账号→「发布」标签按钮；空态行实测文案「暂无项目——到「发布」页发布第一个项目。」。
- 视觉 token：间距 4/8/12/16/24/32；页面题 18、卡片题 14/600（与正文/控件同级）、正文 14、辅助 13、注释/时间 12（mono+tabular-nums，时间列右对齐）；绿=可用/黄=过渡/红=故障/灰=不存在。
