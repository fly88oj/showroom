# showroomctl

Showroom 主机侧唯一管理工具：HTTP 控制面（`serve`）+ 全部运维子命令。契约见仓库根 `SPEC.md`。

## 结构（深模块，对外接缝 = 子命令 / HTTP API）

- `state.rs` — `/srv/showroom/state` 读写 + 纯函数（名字 §4、policy、conf/env 序列化 §5 §6、端口/redisdb 分配、caddy 片段）
- `dockerops.rs` — 全部外部世界调用：docker / `docker exec psql` / caddy subprocess；registry v2 用裸 TCP HTTP/1.0 客户端（127.0.0.1:5000，无第三方 HTTP 依赖）
- `publish.rs` — publish / import 内核（SPEC §7 全流程；serve 与 CLI 子命令共用）
- `ops.rs` — 生命周期命令（stop/start/rm/rollback/reconcile/autoheal/gc*/owner*/init-keys）
- `serve.rs` — axum 控制面，127.0.0.1:8090

## 子命令

| 命令 | 用途 |
|---|---|
| `serve` | HTTP 控制面（127.0.0.1:8090） |
| `owner-add <name> --pubkey <ed25519:…> [--replace-key]` | 登记公钥（kid=sha256 前 8 hex）+ 生成 registry 凭据（bcrypt cost 10 入 `state/registry.htpasswd`，明文 0600 入 `state/owners/<name>/reg.cred`） |
| `owner-key-rm <owner> <kid>` / `owner-revoke <owner>` | 删公钥 / 整体吊销（展项不动） |
| `publish <owner> <project> <slot> <tag> [--mem --cpu --container-port]` | 管理员直发通道（与 `/_api/publish` 同一内核） |
| `stop` / `start` / `rm` / `rollback` / `ps` / `logs` / `env` | 槽位生命周期与查看（均带 `--json`） |
| `reconcile` / `autoheal` / `gc-images` / `gc-registry` / `registry-gc` | systemd timer 运维（见 deploy/*.timer）；gc-images 末尾顺带清 `_tmp` 下 >24h 的 `*.bin`/`*.meta`（分片导入陈旧会话与孤儿） |
| `snapshot hourly|daily` | btrfs 只读快照 `/@showroom` → `/@showroom-snaps/<h-|d->YYYYMMDD-HHMM`，同前缀按名排序保留最新 hourly 24 / daily 7 个（SPEC §8.2） |
| `import <owner> <project> <slot> <tag>`（stdin=信封） | 内部镜像导入通道（`/_api/import` 复用）；大文件分片走 `/_api/import-begin|-chunk|-status|-end`（SPEC §8.1） |
| `import-check <image_id>` | 本机 docker 是否已有该镜像 |
| `init-keys` | 生成平台 RSA 对（已存在不覆盖；serve 启动缺失自动生成） |

## serve 部署要点

systemd 单元已存在（`deploy/showroom-api.service`，root 运行，勿改）。

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
   `state/owners/<owner>/reg.cred`）。
5. 首次 `showroomctl init-keys`（serve 启动缺失也会自动生成），再 `owner-add` 登记发布者。

环境变量（均可省略）：`SHOWROOM_ROOT`（默认 `/srv/showroom`）、`SHOWROOM_BASE_URL`
（`url` 字段对外前缀，默认 `http://127.0.0.1`）、`SHOWROOM_DOCKER_NET`（默认 `showroom`）、
`SHOWROOM_CADDY_FILE`（默认 `/etc/caddy/Caddyfile`）。

已知天花板（ponytail 注释同步在代码里）：滚动更新因同槽位唯一 host 端口先停旧再起新（秒级中断）；
gc-registry 标签按字典序近似新旧（registry:2 无时间戳 API）；端口分配无并发锁（本机唯一管理通道）；
MAX_SLOTS 按全平台 slot 总数计。
