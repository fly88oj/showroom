# Showroom 部署指南（从零开始）

目标：在一台 Linux 主机（假设已有 Docker、root 权限、一块 btrfs 文件系统）上部署完整的 Showroom 控制面。完成后你将拥有：Caddy 双面入口、控制面 API、共享 PostgreSQL/Redis/registry、systemd timers 自愈、首把管理员钥匙。

> 本文所有 `your-host`、`<USER>`、`<PROJECT>`、`<…>` 均为占位符，替换为你的实际值。协议与安全背景见 [security.md](security.md)；日常运维见 [operations.md](operations.md)。

## 0. 前置要求

| 项 | 要求 | 原因 |
|---|---|---|
| OS | Linux（systemd） | 服务与 timer 单元 |
| Docker | ≥ 20（含 `docker exec`） | 展项与中间件容器 |
| 文件系统 | btrfs | 快照（`snapshot hourly/daily`）依赖子卷 |
| Caddy | 2.x | 静态面 + 反代 |
| PostgreSQL | ≥ 13 | `DROP DATABASE … WITH (FORCE)` |
| 端口 | 80/443 对外（Caddy）；8100–8199 仅本机 | 展项端口不直接暴露 |

## 1. 目录与 btrfs 子卷

```sh
# 假设 btrfs 池挂在 /mnt/btrfs（按实际路径替换）；快照父卷 /@showroom-snaps 会在首次快照时惰性创建
sudo btrfs subvolume create /mnt/btrfs/@showroom
sudo mkdir -p /srv/showroom
echo '/dev/disk/by-uuid/<你的-btrfs-UUID> /srv/showroom btrfs subvol=@showroom,defaults 0 0' | sudo tee -a /etc/fstab
sudo mount /srv/showroom

sudo mkdir -p /srv/showroom/state/{tokens,users,projects,routes} \
             /srv/showroom/_middleware/{postgres,redis,registry} \
             /srv/showroom/_console \
             /srv/showroom/_tmp
sudo chmod 700 /srv/showroom/state /srv/showroom/state/tokens /srv/showroom/state/users
```

写入初始策略（`/srv/showroom/state/policy.env`，键含义与默认值）：

```sh
sudo tee /srv/showroom/state/policy.env >/dev/null <<'EOF'
MAX_SLOTS=30
DEFAULT_MEM=256m
MAX_MEM=1g
IMPORT_MAX_GB=8
ALLOW_LAN_PORT=0
RESERVE_MEM_MB=1536
EOF
```

- `MAX_SLOTS`：项目数上限；`DEFAULT_MEM`/`MAX_MEM`：容器内存需求缺省值与上限；
- `IMPORT_MAX_GB`：import 加密通道单包上限（GB）；`RESERVE_MEM_MB`：内存准入预留（`MemAvailable − 预留 < 需求` 即拒绝）。

## 2. 中间件容器

```sh
sudo docker network create showroom-net

# PostgreSQL（凭据为占位符；展项库/用户由 publish 时自动创建）
sudo docker run -d --name sr-postgres --network showroom-net --restart unless-stopped \
  -e POSTGRES_PASSWORD='<PG-超级用户密码>' \
  -v /srv/showroom/_middleware/postgres:/var/lib/postgresql/data \
  postgres:16-alpine \
  -c shared_buffers=256MB -c effective_cache_size=768MB -c work_mem=16MB \
  -c max_connections=200 -c autovacuum_naptime=30s

# Redis（逻辑库 0–15 按项目分配）
sudo docker run -d --name sr-redis --network showroom-net --restart unless-stopped \
  -v /srv/showroom/_middleware/redis:/data \
  redis:7-alpine --appendonly yes --maxmemory 512mb --maxmemory-policy allkeys-lru

# registry（先做第 4 步 user-add 生成 htpasswd，再启动本容器；
# 或先 touch 空文件占位、user-add 后 restart）
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

要点（均为平台硬约定）：

- docker 网络名必须是 `showroom-net`（常量）；容器名 `sr-postgres` / `sr-redis` / `sr-registry` 即网络内 DNS 名，注入展项的 `DB_HOST` / `REDIS_HOST` 依赖它们；
- registry 必须 `delete.enabled: true`（tag 保留策略 `gc-registry` 用）且以 `state/registry.htpasswd` 做 basic auth；
- registry 仅发布到 `127.0.0.1:5000`；外部推拉一律经 Caddy `/v2`。

## 3. 构建与安装二进制 + systemd 单元

```sh
git clone <本仓库> && cd showroom
cargo build --release

sudo install -m755 target/release/showroomctl target/release/showroom /usr/local/bin/
sudo install -m644 deploy/showroom-*.service deploy/showroom-*.timer /etc/systemd/system/
```

`deploy/` 内单元即实际使用的单元（`showroom-api.service` 为 root 运行的控制面，其余为 7 个 timer）。按你的域名覆盖对外 URL（不改动仓库文件，用 drop-in）：

```sh
sudo mkdir -p /etc/systemd/system/showroom-api.service.d
sudo tee /etc/systemd/system/showroom-api.service.d/override.conf >/dev/null <<'EOF'
[Service]
Environment=SHOWROOM_BASE_URL=https://your-host
EOF
```

> 环境变量：`SHOWROOM_ROOT`（默认 `/srv/showroom`）、`SHOWROOM_BASE_URL`（publish 返回的 `url` 字段前缀，默认 `http://127.0.0.1`）。docker 网络名（`showroom-net`）与 Caddyfile 路径（`/etc/caddy/Caddyfile`）为常量。

启用（先只起 API，timers 在验证后再开）：

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now showroom-api
```

## 4. 首把管理员钥匙引导

在**发布者机器**上生成密钥并取回公钥串：

```sh
showroom keygen        # 打印：密钥路径 + 公钥串（ed25519:<base64>）
```

在主机上登记为首个管理员：

```sh
# 平台 RSA 对（serve 启动缺失也会自动生成；显式执行可提前拿到公钥分发给发布者）
sudo showroomctl init-keys

sudo showroomctl user-add <USER> --pubkey 'ed25519:<发布者公钥串>' --admin
# 输出 registry 凭据：<USER> 的 basic auth 密码（state/users/<USER>/reg.cred 明文留存）

sudo docker restart sr-registry     # 若启动 registry 时 htpasswd 还是空的
```

把两样东西交给发布者：registry 密码（快通道 push 用）、`/srv/showroom/state/platform_rsa.pub`（发布者存为 `~/.showroom/platform_rsa.pub`，import 加密通道用）。

## 5. Caddyfile（完整样例）

`/etc/caddy/Caddyfile`：

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
			precompressed zstd
		}
	}
	# 展项路由（showroomctl 按项目生成到 state/routes/<project>.caddy 并 reload）
	import /srv/showroom/state/routes/*.caddy
	# 根路径与未匹配路径一律 404（零信息暴露：不重定向、不暴露站点信息）
}

# ————— site A：内网面 —————
http://showroom-lan.your-host {
	import showroom-app
	# /v2：registry 推拉快通道（basic auth 由 registry 自身校验）
	handle /v2/* {
		reverse_proxy 127.0.0.1:5000
	}
	# /_bootstrap：CLI / Skill 分发（仅内网面提供，公网面 404）
	handle_path /_bootstrap/* {
		root * /srv/showroom/_bootstrap
		file_server
	}
}

# ————— site B：公网面 —————
# 经公网隧道（如 frp）转发到本机 80 端口时，公网流量命中的就是这里的站点。
# 公网发布走加密 import 通道，不放 /v2；/_bootstrap 返回 404。
http://showroom-public.your-host {
	import showroom-app
}
```

安装控制台静态资源（与仓库 `showroom-web/dist/` 同步，可选做预压缩）：

```sh
sudo cp showroom-web/dist/index.html showroom-web/dist/showroom_bg.wasm /srv/showroom/_console/
# 预压缩（Caddy file_server precompressed 直接下发 .zst，省 CPU）：
sudo bash -c 'cd /srv/showroom/_console && for f in index.html showroom_bg.wasm; do zstd -19 -k -q "$f"; done'
```

> 注意：控制台 wasm 产物必须与 `showroom-common` 垫片源码同提交更新（贡献纪律，见 [CONTRIBUTING.md](../CONTRIBUTING.md)）。更新 `_console/` 后重跑预压缩。

## 6. 验证

```sh
systemctl is-active showroom-api          # 期望 active（本机健康检查用进程级，不用 HTTP）
# 零信息暴露自检：未签名请求 /_api/health 必须得 404（得 200/403 说明鉴权没接上）
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8090/_api/health   # → 404
```

发布者机器上走一遍 `keygen → config → doctor → deploy`（见 [README](../README.md) 快速开始），成功后开全部 timers：

```sh
sudo systemctl enable --now showroom-reconcile.timer showroom-autoheal.timer \
  showroom-gc-images.timer showroom-gc-registry.timer showroom-registry-gc.timer \
  showroom-snapshot-hourly.timer showroom-snapshot-daily.timer
```

## 7. frp 隧道接入（主机无公网 IP 时）

在公网服务器跑 frps（占位配置），在 Showroom 主机跑 frpc 把 Caddy 暴露出去：

```toml
# /etc/frp/frpc.toml（Showroom 主机）
serverAddr = "<FRP-SERVER>"
serverPort = 7000
auth.token = "<FRP-TOKEN>"

[[proxies]]
name = "showroom-http"
type = "tcp"
remotePort = 28080         # 公网访问端口
# frp 自身隧道加密（与应用层加密叠加，可选）
transport.useEncryption = true
```

frpc 以 systemd 常驻后，公网面即 `http://<FRP-SERVER>:28080`。发布者 CLI 配置双通道：

```sh
showroom config --server-lan http://showroom-lan.your-host \
    --server-public http://<FRP-SERVER>:28080
```

Caddy 侧按第 5 步把公网流量对应到 site B（frps 转发到本机 80，`Host` 匹配公网域名）。**注意**：frp 仅转发 TCP；管理流量的认证与机密性由 SR-SIG1/信封在应用层保证，不依赖 frp 的加密。

## 8. 性能调优清单

### 8.1 内核（BBR + 连接队列，`/etc/sysctl.d/99-showroom.conf`）

```ini
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 8192
net.ipv4.ip_local_port_range = 10240 65535
fs.file-max = 131072
```

```sh
sudo sysctl --system
```

### 8.2 PostgreSQL（第 2 步命令中已内联，按内存调）

```ini
shared_buffers = 256MB        # 约为专供内存的 1/4
effective_cache_size = 768MB  # 约为专供内存的 3/4
work_mem = 16MB               # 每连接每算子；展项多且并发高时勿调太大
max_connections = 200         # 展项数 × 各自连接池上限，留余量
autovacuum_naptime = 30s      # Demo 库写多读少，收紧自动清理间隔
```

### 8.3 平台策略（`state/policy.env`）

- 小内存主机：调低 `DEFAULT_MEM`（如 `128m`）并保留 `RESERVE_MEM_MB`（默认 1536）防过卖；
- 展项规模：`MAX_SLOTS` 是全平台项目数上限；端口池固定 8100–8199（100 个），两者取小者为实际容量；
- 大镜像团队：`IMPORT_MAX_GB` 按需上调（同时确认 `_tmp` 所在子卷容量）。

### 8.4 控制台静态资源预压缩

见第 5 步：`file_server { precompressed zstd }` + 定期对 `_console/` 重跑 zstd 预压缩脚本，避免每次请求现场压缩。

### 8.5 磁盘

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
