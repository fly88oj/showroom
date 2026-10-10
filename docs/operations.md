# Showroom 运维手册

面向已部署主机（见 [deployment.md](deployment.md)）的日常运维：服务与 timer 清单、快照与垃圾回收、容量调整、备份恢复、故障排查与安全事件响应。命令中的 `your-host` / `<USER>` / `<PROJECT>` 为占位符。

## 1. 服务与 timer 清单

### 常驻服务

| 服务 | 作用 | 备注 |
|---|---|---|
| `showroom-api.service` | 控制面 HTTP（127.0.0.1:8090） | root 运行，`Restart=always`（3s）；本机健康检查用 `systemctl is-active`，**不要**依赖 HTTP health（未签名恒 404） |
| `caddy` | 静态面 + 反代 | 展项路由变更由 showroomctl 自动 `caddy reload` |
| `docker` / `containerd` | 容器运行时 | 展项容器 `--restart unless-stopped` |

### systemd timers（部署单元见 `deploy/*.timer`）

| timer | 周期 | 动作 |
|---|---|---|
| `showroom-reconcile.timer` | 每小时（开机 2min 后起算） | 遍历 `state/projects/*.conf`，缺失容器/路由幂等重建；顺带执行 v1.4 槽位布局迁移 |
| `showroom-autoheal.timer` | 每 5 分钟 | 容器缺失重建、非预期退出拉起、unhealthy 重启；`stopped=1` 标记的项目跳过 |
| `showroom-gc-images.timer` | 每日 03:30 | `docker image prune -a` + 构建缓存 + 悬空卷 + `_tmp` 下 >24h 的陈旧会话 |
| `showroom-gc-registry.timer` | 每日 04:00 | registry 每 repo 字典序降序保留前 5 个 tag，其余 DELETE |
| `showroom-registry-gc.timer` | 每周日 04:30 | `registry:2 garbage-collect --delete-untagged`（停-扫-启） |
| `showroom-snapshot-hourly.timer` | 每小时 | btrfs 只读快照，保留 24 份 |
| `showroom-snapshot-daily.timer` | 每日 00:05 | btrfs 只读快照，保留 7 份 |

```sh
systemctl list-timers 'showroom-*'          # 下次触发时间（也包含在 /_api/admin/status）
journalctl -u showroom-api -f               # 控制面日志（E_AUTH 细节只进这里）
```

控制台「管理 → 状态」面板同源展示：`docker/containerd/caddy/showroom-api` 四服务状态、根分区与 `/srv/showroom` 磁盘、MemAvailable、容器列表、timers 下次触发（30s 自动刷新）。

## 2. 快照与垃圾回收

### 快照（btrfs）

```sh
showroomctl snapshot hourly    # 手动触发一次（timer 平时自动跑）
sudo btrfs subvolume list /srv/showroom          # 查看 @showroom-snaps 下的快照
```

- 位置：`/@showroom-snaps/<h-|d->YYYYMMDD-HHMM`（只读快照；父子卷惰性创建）；
- 保留：hourly 24 份 / daily 7 份（按名排序裁最旧）；失败如实报错不 panic。

### 回收

一切 GC 都可手动重跑（幂等）：`showroomctl gc-images` / `gc-registry` / `registry-gc`。磁盘告急时的处置顺序：

```sh
showroomctl gc-registry       # 先删多余 tag（软删除）
showroomctl registry-gc       # 再做 registry 离线 GC 回收 blob（停-扫-启 registry）
showroomctl gc-images         # docker 侧：镜像/构建缓存/悬空卷 + _tmp 陈旧会话
docker system df              # 观察回收效果
```

## 3. 容量调整

### 扩内存（改准入预留 / 上限）

编辑 `/srv/showroom/state/policy.env` 后立即生效（每次 publish 读取，无需重启服务）：

```sh
sudo vi /srv/showroom/state/policy.env
#   DEFAULT_MEM=256m   → 新发布缺省内存需求
#   MAX_MEM=1g         → 单项目上限（publish --mem 不得超过）
#   RESERVE_MEM_MB=1536 → 内存准入预留：MemAvailable − 预留 < 需求 → E_QUOTA
```

已发布项目改内存：`showroom publish <PROJECT> <TAG> --mem 512m`（滚动更新，conf 与容器随之更新）。

### 其他

- 项目数上限：`MAX_SLOTS`（语义 = 全平台项目数；端口池 8100–8199 是硬上界 100）；
- import 大小帽：`IMPORT_MAX_GB`；
- Redis 逻辑库：0–15 按项目分配，满了需清理旧项目（`showroom rm`）。

## 4. 备份与恢复（主机视角）

平台自身的状态都在 `/srv/showroom`（btrfs 快照兜底）；发布者的**业务数据**备份走签名 API 端到端加密（见 [cli-reference.md](cli-reference.md) `db dump/restore`），主机侧不经手明文。

| 对象 | 方式 |
|---|---|
| 全量状态（conf/env/密钥/路由） | btrfs 快照（hourly/daily）+ 必要时异地同步快照子卷 |
| 某项目的数据库 | 发布者：`showroom db dump <PROJECT> -o backup.bin.zst`（端到端加密，主机无法解密） |
| 灾难重建 | 新主机按 [deployment.md](deployment.md) 重部署；恢复 `/srv/showroom/state`（tokens/users/projects）与 `_middleware/` 数据卷，`showroomctl reconcile` 重建全部容器与路由 |
| 单项目误删 | `state/projects/<PROJECT>.*` 删了即彻底删除；如有快照可从 `@showroom-snaps` 拷回 conf 后 `reconcile` |

> `state/` 含全部公钥与 registry 凭据——快照异地存储需按密钥材料对待（加密传输与保存）。

## 5. 故障排查表

| 症状 | 诊断命令 | 动作 |
|---|---|---|
| 展项 502 / 打不开 | `curl -s http://127.0.0.1:8100/`（conf 里登记的端口）；`docker ps -a \| grep sr-<PROJECT>` | 容器挂了等 autoheal（≤5min）或手动 `showroomctl start <PROJECT>`；端口连不通看 `showroomctl logs <PROJECT>` |
| 展项漂移（容器/路由被误删） | `systemctl start showroom-reconcile.service` | reconcile 幂等重建；确认 timer 在跑 |
| publish 报 `E_QUOTA`（内存） | `grep MemAvailable /proc/meminfo` | 对比 `RESERVE_MEM_MB`；扩内存或调低需求 `--mem` |
| publish 报 `E_QUOTA`（no free port） | `cat /srv/showroom/state/ports.reg \| wc -l` | 端口池 8100–8199 满：`showroom rm` 清理旧项目 |
| publish 报 `E_NET` | `docker pull 127.0.0.1:5000/<USER>/<PROJECT>:<TAG>` | 镜像不在本机 registry：发布者重新 push；检查 registry 容器与 htpasswd |
| import 卡住 / `_tmp` 涨 | `ls -lh /srv/showroom/_tmp/` | 分片会话 >24h 由 gc-images 清；急用手动删（确认无进行中上传） |
| CLI 全部命令 404 E_AUTH | `journalctl -u showroom-api \| grep E_AUTH` | 公钥未登记 / 时钟偏差 >300s（NTP）/ Caddy 改写了 path；journal 里有具体原因 |
| 磁盘告急 | `df -h / /srv/showroom`；`docker system df` | 见 §2 回收顺序 |
| registry push 401 | `cat /srv/showroom/state/users/<USER>/reg.cred`（0600） | 凭据过期/吊销：重新 user-add 发卡 |
| 快照失败 | `systemctl status showroom-snapshot-hourly.service` | 子卷满 / 非只读父卷；报错原文在 unit 输出 |
| 控制台打不开 | `ls /srv/showroom/_console/` | 静态资源未部署（见 deployment §5） |

## 6. 安全事件响应（吊销流程）

详细策略见 [SECURITY.md](../SECURITY.md)。速查：

```sh
# 1. 删除泄露的那把钥匙（kid 从清单查：showroomctl admin users）
sudo showroomctl user-key-rm <USER> <KID>

# 2. 整账号失能（签名 + registry 登录全失效，展项不动）
sudo showroomctl user-revoke <USER>

# 3. 管理员权限收/授
sudo showroomctl key-admin <USER> <KID> --admin false|true

# 4. 重新发卡（返回一次性展示的 registry 密码）
sudo showroomctl user-add <USER> --pubkey 'ed25519:<新公钥串>'

# 管理员钥匙远程操作等价 API（任何管理员钥匙签名，公网可用）：
#   POST /_api/admin/key-rm | user-revoke | key-admin | user-add ；GET /_api/admin/users
```

事后检查：`journalctl -u showroom-api --since "<时间>" | grep -E 'E_AUTH|admin'` 评估泄露窗口内的管理面调用痕迹。

## 7. 版本升级

```sh
git pull && cargo test --workspace && cargo build --release
sudo install -m755 target/release/showroomctl target/release/showroom /usr/local/bin/
sudo systemctl restart showroom-api
sudo showroomctl reconcile          # 新版布局迁移（如 v1.4 槽位上提）幂等完成
```

升级前建议手动 `showroomctl snapshot daily` 留一份回滚点。
