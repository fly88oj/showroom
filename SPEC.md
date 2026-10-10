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
| POST /_api/db-dump | `{project,slot,client_pub_pem}` | pg_dump -Fc → 用 client_pub 按 §2 信封加密 → `{payload_b64}`（>256MB 拒 E_QUOTA）|
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
