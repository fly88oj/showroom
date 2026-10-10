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
