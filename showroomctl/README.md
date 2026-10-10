# showroomctl

Showroom 主机侧唯一管理工具：HTTP 控制面（`serve`）+ 全部运维子命令。契约见仓库根 `SPEC.md`。

## 结构（深模块，对外接缝 = 子命令 / HTTP API）

- `state.rs` — `/srv/showroom/state` 读写 + 纯函数（名字 §4、policy、conf/env 序列化 §5 §6、端口/redisdb 分配、caddy 片段）
- `dockerops.rs` — 全部外部世界调用：docker / `docker exec psql` / caddy subprocess；registry v2 用裸 TCP HTTP/1.0 客户端（127.0.0.1:5000，无第三方 HTTP 依赖）
- `publish.rs` — publish / import 内核（SPEC §7 全流程；serve 与 CLI 子命令共用）
- `ops.rs` — 生命周期命令（stop/start/rm/rollback/reconcile/autoheal/gc*/user*/init-keys）
- `serve.rs` — axum 控制面，127.0.0.1:8090

## 子命令

| 命令 | 用途 |
|---|---|
| `serve` | HTTP 控制面（127.0.0.1:8090） |
| `user-add <name> --pubkey <ed25519:…> [--admin] [--replace-key]` | 登记公钥（kid=sha256 前 8 hex）+ 生成 registry 凭据（bcrypt cost 10 入 `state/registry.htpasswd`，明文 0600 入 `state/users/<name>/reg.cred`）；`--admin` 落 `tokens/<user>/<kid>.admin` 空标记=管理员钥匙（SPEC §12.2，首个管理员引导） |
| `key-admin <user> <kid> --admin true\|false` | 授/收管理员权限（幂等；`/_api/admin/key-admin` 同一内核） |
| `user-key-rm <user> <kid>` / `user-revoke <user>` | 删公钥（admin 标记随钥匙清）/ 整体吊销（展项不动） |
| `admin users` / `admin status` | 管理员视图（本机调试用，幂等）：账号/钥匙（kid+admin+登记时间）/项目数；服务/磁盘/内存/容器/timers 面板（`GET /_api/admin/*` 同源，均支持 `--json`） |
| `publish <user> <project> <slot> <tag> [--mem --cpu --container-port]` | 管理员直发通道（与 `/_api/publish` 同一内核） |
| `stop` / `start` / `rm` / `rollback` / `ps` / `logs` / `env` | 槽位生命周期与查看（均带 `--json`） |
| `reconcile` / `autoheal` / `gc-images` / `gc-registry` / `registry-gc` | systemd timer 运维（见 deploy/*.timer）；gc-images 末尾顺带清 `_tmp` 下 >24h 的 `*.bin`/`*.meta`（分片导入陈旧会话与孤儿） |
| `snapshot hourly|daily` | btrfs 只读快照 `/@showroom` → `/@showroom-snaps/<h-|d->YYYYMMDD-HHMM`，同前缀按名排序保留最新 hourly 24 / daily 7 个（SPEC §8.2） |
| `import <user> <project> <slot> <tag>`（stdin=信封） | 内部镜像导入通道（`/_api/import` 复用）；大文件分片走 `/_api/import-begin|-chunk|-status|-end`（SPEC §8.1） |
| `import-check <image_id>` | 本机 docker 是否已有该镜像 |
| `init-keys` | 生成平台 RSA 对（已存在不覆盖；serve 启动缺失自动生成） |

## serve 部署要点

systemd 单元已存在（`deploy/showroom-api.service`，root 运行，勿改）。部署脚本
`deploy/install.sh`（ssh 公钥免密 + scp；验证 unsigned `/_api/health` 得 404 即鉴权生效）。
注：`deploy/install.sh` 的历史提交中含过 SSH 密码，如需可 rotate。

**零信息暴露（SPEC §3）**：`/_api` 下所有端点（含 health/projects）一律要求 SR-SIG1 签名；
未认证或验签失败的任何请求——无论路径是否存在、方法是否支持——统一返回
`404 + {"error":"E_AUTH"}`（无 msg，细节只进 journal）。本机健康检查用进程级
`systemctl is-active showroom-api`，不要依赖 HTTP health。

手工/主机侧前置（bootstrap，一次性）：

1. Caddyfile 需包含：`import /srv/showroom/state/routes/*.caddy`（showroomctl 生成展项路由），
   `/_api/*` 反代 127.0.0.1:8090，`/v2` 反代 registry（外部推拉通道，SPEC §4）。
2. docker 网络 `showroom` 存在，`sr-postgres` / `sr-redis` / `sr-registry` 接入该网络
   （容器名即 DNS 名，注入给展项容器的 DB_HOST/REDIS_HOST 依赖它）。registry 数据卷
   bind 到 `/srv/showroom/_middleware/registry`，配置需 `delete.enabled: true`（gc-registry 用）
   并以 `state/registry.htpasswd` 做 basic auth。
3. PostgreSQL >= 13（publish 用 `DROP DATABASE … WITH (FORCE)` 幂等重建）。
4. root 执行 `docker login 127.0.0.1:5000`（import 后 `docker push` 用；凭据来自
   `state/users/<user>/reg.cred`）。
5. 首次 `showroomctl init-keys`（serve 启动缺失也会自动生成），再 `user-add` 登记发布者。

环境变量（均可省略）：`SHOWROOM_ROOT`（默认 `/srv/showroom`）、`SHOWROOM_BASE_URL`
（`url` 字段对外前缀，默认 `http://127.0.0.1`）。docker 网络名（`showroom-net`）与
Caddyfile 路径（`/etc/caddy/Caddyfile`）为常量。

已知天花板（ponytail 注释同步在代码里）：滚动更新因同槽位唯一 host 端口先停旧再起新（秒级中断）；
gc-registry 标签按字典序近似新旧（registry:2 无时间戳 API）；端口分配无并发锁（本机唯一管理通道）；
MAX_SLOTS 按全平台 slot 总数计。

Redis 浏览（§10.3/§13.2）：`/_api/redis-dbs|keys|get` 按签名用户隔离——普通钥匙仅能访问
自己名下项目分到的逻辑库（`redis-dbs` 只列自己的；admin 钥匙全量并标 `admin_view:true`）。
诚实边界：这是**界面级/协作级隔离**（防误看防误操作）——容器内应用持有原始连接仍可 SELECT
任意库，非安全边界；应用级强隔离需独实例或 Redis ACL，成本过高不做。

Redis 逻辑库池（§13.3）：分配单位维持每项目一库（conf `redisdb`，客户端 URL 零配合）；
池大小 policy.env `REDIS_DBS=` 可调，默认 256（`Conf.redisdb` 为 u8，库号至多 0-255；
需 sr-redis 启动参数 `--databases` ≥ 该值）。与 `MAX_SLOTS`（默认 30）的一致性由运维保证
（30≤256）；池耗尽发布返回 `E_QUOTA`（消息含 已用/总量），存量已分配库号不受调整影响。
