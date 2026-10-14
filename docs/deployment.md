# Showroom 部署指南（三步自举）

目标：在一台 Linux 主机上部署完整的 Showroom 控制面。完成后你将拥有：Caddy 双面入口（内网面 :80 / 公网面 :8081）、控制面 API、共享 PostgreSQL/Redis/registry、systemd timers 自愈、首个管理员账号。

> 本文所有 `your-host`、`<USER>`、`<PROJECT>`、`<…>` 均为占位符，替换为你的实际值。协议与安全背景见 [security.md](security.md)；日常运维见 [operations.md](operations.md)。
> 手工全步骤（原理参考/排障用，setup 已自动化）见[附录 A](#附录-a-手工全步骤原理参考)；调优参数全集见[附录 B](#附录-b-调优参数表)。

## 0. 前置检查表

| 项 | 要求 | 原因 |
|---|---|---|
| OS | Linux（systemd；包管理 pacman 的发行版可直接装 Caddy，其余见附录 A） | 服务与 timer 单元 |
| Docker | ≥ 20（含 `docker exec`） | 展项与中间件容器 |
| 权限 | root | 写 /etc、/usr/local/bin、systemd 单元 |
| 文件系统 | btrfs | 快照（`snapshot hourly/daily`）依赖子卷 |
| 端口 | 80 对外（site A 内网面）；8081 为公网面（site B）；8082 仅本机（serve 内隧道网关 → 8081）；8100–8199 仅本机 | 展项端口不直接暴露 |

**唯一保留的手工步**：btrfs 子卷创建与挂载（setup 的 BtrfsSubvol Action 只覆盖「父目录即 btrfs」的顺手场景，子卷与 fstab 属磁盘布局，不自动化）：

```sh
# 假设 btrfs 池挂在 /mnt/btrfs（按实际路径替换）；快照父卷 /@showroom-snaps 由 setup 顺手创建
sudo btrfs subvolume create /mnt/btrfs/@showroom
sudo mkdir -p /srv/showroom
echo '/dev/disk/by-uuid/<你的-btrfs-UUID> /srv/showroom btrfs subvol=@showroom,defaults 0 0' | sudo tee -a /etc/fstab
sudo mount /srv/showroom
```

## 1. 取二进制

GitHub Releases 下载单文件 `showroomctl`（或源码构建 `cargo build --release` 后取 `target/release/showroomctl`）：

```sh
sudo install -m755 showroomctl /usr/local/bin/showroomctl
```

## 2. `showroomctl setup --apply`（幂等收敛）

```sh
sudo showroomctl setup            # diff 模式：只列出将做的变更与当前偏差
sudo showroomctl setup --apply    # 幂等执行；二次运行期望 `plan: 0 actions needed (clean)`
```

首跑缺 `state/host.env` 时按探测值生成并打印（`--lan-host`/`--public-host` 可覆写）。**限额缺省随实测硬件推导**（内存×15%/磁盘×3% 等，见附录 B；host.env 首行注释记录推导依据，audit 可溯；用户显式写入的值永远优先）。host.env 键表（整文件 0600——含 frp 凭据）：

| 键 | 取值 | 默认 | 含义 |
|---|---|---|---|
| `MIDDLEWARE` | 空格集合 ⊆ postgres/redis/registry | `postgres redis registry` | 中间件集合（禁用组 env 不再注入） |
| `CADDY` | yes/no | yes | 安装(pacman)+渲染 Caddyfile |
| `LAN_HOST` | 地址 | 首跑 `hostname -I` 首地址 | site A（内网面）地址；BASE_URL 由此派生 |
| `PUBLIC_HOST` | 地址/空 | 空 | site B（公网面）地址；空=纯内网主机 |
| `TUNING` | sysctl/none | sysctl | 内核调优（sysctl.d 全表见附录 B） |
| `DEDUP` | weekly/none | weekly | 周日 05:00 duperemove timer |
| `FRP` | none/chmlfrp/native | none | 隧道形态（并入 serve 常驻，见 §3 frp） |
| `FRP_TOKEN` / `FRP_TUNNEL` | 字符串 | 空 | chmlfrp：usertoken 与隧道 ID（多隧道=逗号列表，**每 ID 独立会话**——跨节点双入口即此形态；历史键 FRP_TUNNEL2 解析时自动折叠） |
| `FRP_TOML` | 文件路径 | 空 | native：指向 0600 的 frpc TOML 文件（自建 frps/其它中转商；路径非凭据可回显，凭据在该文件里） |
| `SERVICES_MAX`（policy.env） | 数字 | 4 | 每项目服务数上限（含主服务，plan-19 多容器） | native：指向 0600 的 frpc TOML 文件（自建 frps/其它中转商；路径非凭据可回显，凭据在该文件里） |

setup 做的事：目录/policy.env（缺键补默认、已有键保留）/host.env 落盘、docker 网络 `showroom-net`、所选中间件容器（pg 12 参数/redis `--databases=policy.REDIS_DBS`/registry 全旗标，规格见附录 B）、Caddy 安装+Caddyfile 渲染（registry ∈ 集合才有 `/v2`）、systemd 单元（api+7 timers+dedup）安装启用、sysctl/journald(400M+30d)/watchdog(10s) 调优、`showroom-precompress` 预压缩脚本与首跑执行。现网 redis 实例 `--databases` 低于 policy 时**不重建**（数据安全优先），只报黄条引导走 §3 扩容流。

## 3. 首个管理员账号 + 交付

在主机上发首把管理员账号（密码为主路径，SPEC §16）：

```sh
sudo showroomctl user-add <USER> --gen-password --admin
# 一次性返回两把密码：登录初始密码（OPAQUE 保护）与 registry 密码——分开发放，均仅显示一次
```

CI / 机器用户与日常登录一致：用账号密码走 `showroom login`（B7 统一账号密码认证，无公钥预登记路径）。

把三样东西交给发布者：**登录初始密码**（控制台一步登录 / CLI `showroom login` 用）、**registry 密码**（快通道 push 备用；`login` 会自动拉取）、`/srv/showroom/state/platform_rsa.pub`（发布者存为 `~/.showroom/platform_rsa.pub`，import 加密通道用）。平台公钥无需手工分发文件：控制台登录后自动配置（浏览器端 whoami 自动取并缓存），CLI 侧在控制台帮助页「工具下载」下载后放入 `~/.showroom/` 即可。

## 4. 验证

```sh
systemctl is-active showroom-api          # 期望 active（本机健康检查用进程级，不用 HTTP）
# 零信息暴露自检：未签名请求 /_api/health 必须得 404（得 200/403 说明鉴权没接上）
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8090/_api/health   # → 404
```

发布者机器上走一遍 `login → doctor → deploy`（见 [README](../README.md) 快速开始），成功后 timers 已由 setup 启用，无需再手工 `enable --now`。

## 5. 日常变更

| 场景 | 命令 |
|---|---|
| 增/减中间件 | `showroomctl middleware enable redis` / `showroomctl middleware disable redis [--force]`（有项目引用时拒绝并列出；bind 数据卷保留） |
| 期望/实际对照 | `showroomctl middleware status`（含 redis 实例库数 vs policy 漂移） |
| Redis 逻辑库扩容 | `showroomctl middleware set redis databases=256`（先见提示文案，加 `--confirm` 才停删重建实例：AOF 持久、约 3 秒、验证通过才写 policy） |
| 改主机形状后收敛 | 编辑 `state/host.env`（或 POST `/_api/admin/host` 两段式）→ `showroomctl setup --apply` |
| 就地升级 | 上传新二进制到 `/tmp/showroomctl` → `showroomctl upgrade`（探针→备份→原子替换→restart 探活→失败回滚；同步 user CLI 与 `_bootstrap/`，不碰 `_console`） |
| 管理面板 | `GET /_api/admin/host`：期望状态+漂移+redis 池水位+建议；`POST /_api/admin/host`：无 confirm 返回变更清单与影响面（removed_middleware/affected_projects），`confirm=true` 才落盘+apply |

### frp 隧道接入

**ChmlFrp（自举）**：编辑 `state/host.env` 置 `FRP=chmlfrp`、`FRP_TOKEN=<usertoken>`、`FRP_TUNNEL=<隧道ID>`（多隧道逗号分隔）→ `showroomctl setup --apply` → `systemctl restart showroom-api`。隧道**内嵌在 showroom-api.service 里**（D22）：serve 启动即拉起隧道模块——经 getcfg（`cf-v2.uapis.cn/cfg`）拉面板 ini，全部代理 local 恒改指网关 127.0.0.1:8082 → Caddy site B :8081。**热配置（2026-10-14）**：控制台「管理 → 宿主配置 → 公网隧道」可直接改形态/Token/隧道 ID——两段式确认后落盘 host.env 并 **≤2s 内热重建会话，无需重启 serve**（日志「配置已生效（N 会话）」；实时状态徽章就在编辑区旁）。setup --apply 与手工编辑 host.env 走同一条热重载路径。面板 ini 本身的改动（节点侧）仍在会话重建时重拉。无独立 frpc 单元、无二进制下载（旧 chmlfrp.service 由 setup 观察到即移除）。日志：`journalctl -u showroom-api -f | grep tunnel`；状态：`/_api/admin/host` 的 `frp.tunnel` 字段（connected/connecting/error/stale，数据源 `state/tunnel.status`）。

**自建 frp / 其它中转商（FRP=native）**：在公网服务器跑 frps（占位配置），Showroom 主机不再另跑 frpc——把客户端配置写进独立 0600 文件（如 `/etc/showroom/frpc-native.toml`，`install -m 600`），`state/host.env` 置 `FRP=native` + `FRP_TOML=/etc/showroom/frpc-native.toml`（**路径而非内联 TOML**——host.env 是单行 kv，多行值存不下来）→ `setup --apply` → `restart showroom-api`。文件在**会话重建时**重读（致命错误后的自动重试 ≤30s，或服务重启）——稳妥起见改完文件后 restart：

```toml
# FRP_TOML 指向文件的内容（完整 frpc TOML；local 无须手改——隧道模块恒改指内部网关）
serverAddr = "<FRP-SERVER>"
serverPort = 7000
auth.token = "<FRP-TOKEN>"

[[proxies]]
name = "showroom-http"
type = "tcp"
localPort = 8081          # 任意值均可（被网关改写覆盖）
remotePort = 28080         # 公网访问端口
```

隧道起来后公网面即 `http://<FRP-SERVER>:28080`；同时把该地址写入 `state/host.env` 的 `PUBLIC_HOST=` 再 `setup --apply`（渲染 site B——PUBLIC_HOST 非空是开关，地址仅作元数据展示；frp 流量按 :8081 端口命中，不按 Host 匹配）。发布者 CLI 配置公网通道（`login` 只写 server_lan，public 需补一条）：

```sh
showroom login <USER> --server-lan http://showroom-lan.your-host
showroom config --server-public http://<FRP-SERVER>:28080
```

**注意**：frp 仅转发 TCP；管理流量的认证与机密性由 SR-SIG1/信封/OPAQUE 在应用层保证，不依赖 frp 的加密。网关（:8082）按连接首批 6B 混合探测：SRTP 帧头→传输信封解封（SPEC §2.1）；其余（含全部现有流量）→ 字节级明文直通。**隧道必须是 tcp 型**（chmlfrp 隧道与 native 示例均满足；http 型按 Host 路由会解析载荷，不适用）。**CLI 两级传输已实装**：公网通道自动探测——服务端支持即全程协议层密封（stderr 提示「srtp: …已启用」），不可用自动回退明文（原逻辑，零配置）；LAN 恒明文。**浏览器侧（WS 载波）同已实装**（SPEC §2.2）：登录后控制台 API 流量经 `/_srtp` WebSocket 协议层密封，密钥随登录经 OPAQUE 绑定下发（篡改即 fail-closed），旧服务端/WS 失败自动回退明文 fetch。

---

## 附录 A：手工全步骤（原理参考）

> 以下为 setup 出现前的手工部署原文，**原理参考/排障用，setup 已自动化**（含 §A.2 的 PG 五参数——实配已扩至 12 参数，见附录 B）。

### A.1 目录与初始策略

```sh
sudo mkdir -p /srv/showroom/state/{tokens,users,projects,routes} \
             /srv/showroom/_middleware/{postgres,redis,registry} \
             /srv/showroom/_console \
             /srv/showroom/_tmp
sudo chmod 700 /srv/showroom/state /srv/showroom/state/tokens /srv/showroom/state/users
```

`state/policy.env`（setup 的 `render_policy` 对缺失键补这些默认、已有键一律保留）：

```ini
MAX_SLOTS=30
DEFAULT_MEM=256m
MAX_MEM=1g
IMPORT_MAX_GB=8
ALLOW_LAN_PORT=0
RESERVE_MEM_MB=1536
REDIS_DBS=256
DISK_RESERVE_GB=20
UPLOAD_SPACE_FACTOR=4
```

### A.2 中间件容器

```sh
sudo docker network create showroom-net

# PostgreSQL（凭据为占位符；展项库/用户由 publish 时自动创建）
sudo docker run -d --name sr-postgres --network showroom-net --restart unless-stopped \
  -e POSTGRES_PASSWORD='<PG-超级用户密码>' \
  -v /srv/showroom/_middleware/postgres:/var/lib/postgresql/data \
  postgres:16-alpine \
  -c shared_buffers=256MB -c effective_cache_size=768MB -c work_mem=16MB \
  -c max_connections=200 -c autovacuum_naptime=30s

# Redis（逻辑库按项目分配；--databases 与 policy REDIS_DBS 一致）
sudo docker run -d --name sr-redis --network showroom-net --restart unless-stopped \
  -v /srv/showroom/_middleware/redis:/data \
  redis:7-alpine --appendonly yes --databases 256 --maxmemory 512mb --maxmemory-policy allkeys-lru

# registry（先做 user-add 生成 htpasswd，再启动本容器；或先 touch 空文件占位）
sudo touch /srv/showroom/state/registry.htpasswd
sudo docker run -d --name sr-registry --network showroom-net --restart unless-stopped \
  -p 127.0.0.1:5000:5000 \
  -e REGISTRY_STORAGE_DELETE_ENABLED=true \
  -e REGISTRY_AUTH=htpasswd \
  -e REGISTRY_AUTH_HTPASSWD_REALM=ShowroomRegistry \
  -e REGISTRY_AUTH_HTPASSWD_PATH=/etc/docker/registry/htpasswd \
  -v /srv/showroom/_middleware/registry:/var/lib/registry \
  -v /srv/showroom/state/registry.htpasswd:/etc/docker/registry/htpasswd:ro \
  registry:2
```

要点（均为平台硬约定）：docker 网络名必须是 `showroom-net`（常量）；容器名 `sr-postgres` / `sr-redis` / `sr-registry` 即网络内 DNS 名（注入展项的 `DB_HOST` / `REDIS_HOST` 依赖它们）；registry 必须 `delete.enabled: true` 且以 `state/registry.htpasswd` 做 basic auth；registry 仅发布到 `127.0.0.1:5000`，外部推拉一律经 Caddy `/v2`。

### A.3 二进制与 systemd 单元

```sh
git clone <本仓库> && cd showroom
cargo build --release
sudo install -m755 target/release/showroomctl target/release/showroom /usr/local/bin/
sudo install -m644 deploy/showroom-*.service deploy/showroom-*.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now showroom-api
# timers（setup 已自动；手工路径）：
sudo systemctl enable --now showroom-reconcile.timer showroom-autoheal.timer \
  showroom-gc-images.timer showroom-gc-registry.timer showroom-registry-gc.timer \
  showroom-snapshot-hourly.timer showroom-snapshot-daily.timer
```

> 环境变量：`SHOWROOM_ROOT`（默认 `/srv/showroom`）、`SHOWROOM_BASE_URL`（publish 返回的 `url` 字段前缀；setup 渲染 api 单元时由 host.env 的 `LAN_HOST` 注入，无需 drop-in）。docker 网络名（`showroom-net`）与 Caddyfile 路径（`/etc/caddy/Caddyfile`）为常量。

### A.4 Caddyfile（完整样例）

`/etc/caddy/Caddyfile`（setup 的 `render_caddyfile` 按同一骨架渲染——registry ∈ 集合才有 `/v2` handle；`PUBLIC_HOST` 非空才渲染 site B）：

```caddyfile
# ————— 公共片段：两个站点共用 —————
(showroom-app) {
	# 控制面：全部端点要求 SR-SIG1 签名（零信息暴露由应用层保证）
	handle /_api/* {
		reverse_proxy 127.0.0.1:8090
	}
	# WASM 控制台（登录成功前不加载任何管理资源）
	handle_path /_console/* {
		root * /srv/showroom/_console
		file_server {
			precompressed zstd gzip
		}
	}
	# 展项路由（showroomctl 按项目生成到 state/routes/<project>.caddy 并 reload）
	import /srv/showroom/state/routes/*.caddy
	# 根路径与未匹配路径一律 404（零信息暴露：不重定向、不暴露站点信息）
	# （R8 裁决：_bootstrap/ 目录仅作 /_api/tool 工具源——CLI/Skill/公钥登录后控制台内
	#   下载，无静态匿名分发面，不进任何站点）
}

# ————— site A：内网面 —————
http://showroom-lan.your-host {
	import showroom-app
	# /v2：registry 推拉快通道（basic auth 由 registry 自身校验）
	handle /v2/* {
		reverse_proxy 127.0.0.1:5000
	}
}

# ————— site B：公网面（frp 等公网隧道流量命中的站点）—————
# 公网发布走加密 import 通道，不放 /v2。
http://:8081（respond 404 兜底） {
	import showroom-app
}
```

控制台静态资源与预压缩：控制台 wasm 产物必须与 `showroom-common` 垫片源码同提交更新（贡献纪律，见 [CONTRIBUTING.md](../CONTRIBUTING.md)）；`_console/` 更新后重跑 `/usr/local/bin/showroom-precompress`（setup 已安装并首跑；zstd+gzip 双格式预压缩——代理链路 Accept-Encoding 常被收敛为 gzip）。

## 附录 B：调优参数表

### B.1 PostgreSQL（12 个 `-c` 参数；前三个随首跑硬件推导，其余固定）

| 参数 | 首跑推导 / 固定值 | 说明 |
|---|---|---|
| `shared_buffers` | clamp(总内存×12%, 64MB, 1GB) | 约为专供内存的 1/4 封顶 1G |
| `effective_cache_size` | 总内存×35% | 优化器成本估计 |
| `work_mem` | 16MB（固定） | 每连接每算子；展项多且并发高时勿调太大 |
| `max_connections` | clamp(30+核数×10, 40, 120) | 展项数 × 各自连接池上限，留余量 |
| `autovacuum_naptime` | 30s（固定） | Demo 库写多读少，收紧自动清理间隔 |
| `max_wal_size` / `min_wal_size` | 2GB / 256MB（固定） | WAL 检查点平滑 |
| `checkpoint_completion_target` | 0.9（固定） | 检查点摊平 |
| `random_page_cost` | 1.1（固定） | SSD 友好 |
| `effective_io_concurrency` | 200（固定） | SSD 并发读 |
| `wal_compression` | on（固定） | WAL 压缩 |
| `maintenance_work_mem` | 128MB（固定） | vacuum/建索引内存 |

### B.2 内核（`/etc/sysctl.d/99-showroom.conf`，setup 渲染；`sysctl --system` 生效）

```ini
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 4096
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 87380 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216
net.ipv4.ip_local_port_range = 10240 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fastopen = 3
net.ipv4.tcp_mtu_probing = 1
net.ipv4.tcp_slow_start_after_idle = 0
vm.swappiness = 10
vm.vfs_cache_pressure = 50
fs.file-max = 131072
fs.inotify.max_user_instances = 512
fs.inotify.max_user_watches = 1048576
```

journald drop-in（`/etc/systemd/journald.conf.d/showroom.conf`）：`SystemMaxUse=400M`、`MaxRetentionSec=30d`；system.conf.d watchdog：`RuntimeWatchdogSec=10s`。

### B.3 policy.env 键表（业务限额；与 host.env 分层，不合并）

**分层**：host.env=基础设施形状（装什么、开什么开关，setup 幂等收敛项）；policy.env=业务限额（管理员日常调节项）。两类键的变更主体与风险等级不同——限额已由 §13.3/§14.4 落 policy，合并会造成双写漂移；分层让 setup diff 不混入限额噪声。

| 键 | 默认 | 说明 |
|---|---|---|
| `MAX_SLOTS` | 30 | 项目数上限（端口池 8100–8199 共 100 个，两者取小为实际容量） |
| `DEFAULT_MEM` / `MAX_MEM` | 256m / 1g | 容器内存缺省与上限 |
| `IMPORT_MAX_GB` | 8 | import 加密通道单包上限（GB） |
| `ALLOW_LAN_PORT` | 0 | 展项端口是否绑 LAN |
| `RESERVE_MEM_MB` | 1536（首跑 clamp(总内存×15%, 1024, 6144)） | 内存准入预留 |
| `REDIS_DBS` | 256 | Redis 逻辑库池（扩容走 §5 一条命令） |
| `DISK_RESERVE_GB` | 20（首跑 clamp(总磁盘×3%, 10, 60)） | 磁盘准入预留 |
| `UPLOAD_SPACE_FACTOR` | 4 | 上传空间估算膨胀因子 |

### B.4 磁盘

- `_middleware/postgres` 与 `_middleware/registry` 是增长大头；registry 有 tag 保留策略（每 repo 字典序降序保留前 5）+ 周期 `registry-gc`，见 [operations.md](operations.md)；
- 快照保留策略固定为 hourly×24 / daily×7，子卷为 CoW，增量占用通常远小于全量。

---

## 常见部署问题

| 症状 | 原因 / 处置 |
|---|---|
| `curl /_api/health` 返回 200/403 | 鉴权没接上（不可能由配置导致——检查是否真的连到了 showroomctl serve） |
| publish 报 `E_NET`（registry 不可达） | 展项镜像不在本机 registry：确认发布者 push 成功、registry 容器健康、`docker login 127.0.0.1:5000` 已在主机 root 下执行过 |
| 展项容器起不来、`DB_HOST` 解析失败 | 中间件容器没接 `showroom-net`；网络名是常量不可改 |
| `snapshot` 报错 | `/srv/showroom` 不是 btrfs 子卷，或父卷无空间创建 `@showroom-snaps` |
| CLI 公网通道 `E_AUTH` 但内网正常 | 公网面 Caddy 站点未反代 `/_api/*`，或中间代理改写了路径/方法（签名覆盖 path_with_query） |
| **公网页面空白 / 一律 404（frp 隧道）** | site B 是 `:8081` **纯端口匹配**：frp 流量 Host 是公网 IP，若 Caddyfile 仍按 Host 匹配会全部漏掉。确认 site B 为 `http://:8081`（重跑 `setup --apply`）、隧道本地目标 = 8081、`PUBLIC_HOST` 非空 |
| setup 黄条「redis 实例 databases=16 < policy」 | 手工部署遗留实例：`showroomctl middleware set redis databases=256 --confirm` 收编（§5 扩容流） |
| 隧道 `error: 面板拒绝` / 连不上节点 | `journalctl -u showroom-api -f \| grep tunnel` 看隧道日志；核 host.env 的 FRP_TOKEN/FRP_TUNNEL 与面板节点状态（旧 chmlfrp.service 若还挂着先 `systemctl disable --now chmlfrp` 再跑一次 `setup --apply` 收敛）。若 getcfg 报证书类错误（cf-v2.uapis.cn 证书与其面板历史配置不合——chmlfrp 官方魔改 frpc 即为此跳过校验），本实现不放宽校验：改走 `FRP=native` + FRP_TOML（从面板手抄节点地址/端口/隧道参数）或等其证书修复 |
