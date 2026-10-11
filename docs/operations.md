# Showroom 运维手册

面向已部署主机（见 [deployment.md](deployment.md)）的日常运维：管理员操作速查、服务与 timer 清单、快照与垃圾回收、磁盘空间与镜像清理、数据库维护、故障排查与安全事件响应。命令中的 `your-host` / `<USER>` / `<PROJECT>` / `<KID>` 为占位符。

## 1. 管理员操作速查

| 场景 | 主机 CLI | 等价远程 API / 控制台 |
|---|---|---|
| 发新账号（初始密码 + registry 密码两把分开发放，均仅显示一次） | `showroomctl user-add <USER> --gen-password [--admin]` | `POST /_api/admin/user-add`；控制台「管理 → 发放新账号」 |
| 登记机器用户公钥（CI 专家路径，不设密码） | `showroomctl user-add <USER> --pubkey 'ed25519:<公钥串>'` | 同上（发卡对话框折叠「专家路径」） |
| 补发/重置登录密码（一次性返回，原密码即刻失效，**不动既有钥匙**） | `showroomctl user-passwd <USER> --gen-password` | `POST /_api/admin/passwd-set`；控制台账号表「补发密码」 |
| 删除泄露的某把钥匙 | `showroomctl user-key-rm <USER> <KID>` | `POST /_api/admin/key-rm`；控制台账号表快捷动作 |
| 收/授管理员权限（标记在钥匙上） | `showroomctl key-admin <USER> <KID> --admin false\|true` | `POST /_api/admin/key-admin` |
| 吊销整账号（签名 + registry 全失效，展项不动） | `showroomctl user-revoke <USER>` | `POST /_api/admin/user-revoke` |
| Redis 逻辑库扩容（需重启实例，约 3 秒，AOF 持久；**不静默重启**） | `showroomctl middleware set redis databases=512 --confirm` | `GET /_api/admin/host` 黄条 → 控制台「扩容」确认流 |
| 主机形状变更（中间件增减/开关） | 编辑 `state/host.env` → `showroomctl setup --apply` | `POST /_api/admin/host` 两段式：无 confirm 返回变更清单与影响面（removed_middleware/affected_projects），`confirm=true` 才执行 |
| 主机状态面板（服务/磁盘/内存/容器/timers/Redis 池水位） | `showroomctl admin status` | `GET /_api/admin/status`；控制台「管理 → 状态」30s 自动刷新 |
| 账号/钥匙清单（kid/admin/登记时间/项目数/未设密码徽标） | `showroomctl admin users` | `GET /_api/admin/users` |

发卡要点（plan-16 §1.3）：登录密码（OPAQUE 保护）与 registry 密码（bcrypt）**两把密码分开发放**——前者登录控制台/CLI，后者随 docker push 以 basic-auth 走线，不可合并。`--admin` 纯密码发卡时管理员意图经 `admin.pending` 由该账号**首把登录绑定钥匙**继承（一次性）。

## 2. 服务与 timer 清单

### 常驻服务

| 服务 | 作用 | 备注 |
|---|---|---|
| `showroom-api.service` | 控制面 HTTP（127.0.0.1:8090） | root 运行，`Restart=always`（3s）；本机健康检查用 `systemctl is-active`，**不要**依赖 HTTP health（未签名恒 404） |
| `caddy` | 静态面 + 反代（site A :80 内网面；site B :8080 公网面） | 展项路由变更由 showroomctl 自动 `caddy reload` |
| `docker` / `containerd` | 容器运行时 | 展项容器 `--restart unless-stopped` |
| `chmlfrp.service`（可选） | frpc 隧道（FRP=chmlfrp 时 setup 启用） | 转发公网流量到 127.0.0.1:8080（site B） |

### systemd timers（部署单元见 `deploy/*.timer`，setup 自动启用）

| timer | 周期 | 动作 |
|---|---|---|
| `showroom-reconcile.timer` | 每小时（开机 2min 后起算） | 遍历 `state/projects/*.conf`，缺失容器/路由幂等重建；顺带完成 v1.4 旧双层目录布局的幂等迁移 |
| `showroom-autoheal.timer` | 每 5 分钟 | 容器缺失重建、非预期退出拉起、unhealthy 重启；`stopped=1` 标记的项目跳过 |
| `showroom-gc-images.timer` | 每日 03:30 | `docker image prune -a` + 构建缓存 + 悬空卷 + `_tmp` 下 >24h 的陈旧会话 |
| `showroom-gc-registry.timer` | 每日 04:00 | registry 每 repo 字典序降序保留前 5 个 tag，其余 DELETE |
| `showroom-registry-gc.timer` | 每周日 04:30 | `registry:2 garbage-collect --delete-untagged`（停-扫-启） |
| `showroom-snapshot-hourly.timer` | 每小时 | btrfs 只读快照，保留 24 份 |
| `showroom-snapshot-daily.timer` | 每日 00:05 | btrfs 只读快照，保留 7 份 |
| `showroom-dedup.timer`（DEDUP=weekly 时） | 每周日 05:00 | duperemove 去重 |

```sh
systemctl list-timers 'showroom-*'          # 下次触发时间（也包含在 /_api/admin/status）
journalctl -u showroom-api -f               # 控制面日志（E_AUTH/登录限流细节只进这里）
```

控制台「管理 → 状态」面板同源展示：`docker/containerd/caddy/showroom-api` 四服务状态、根分区与 `/srv/showroom` 磁盘、MemAvailable、容器列表、timers 下次触发（30s 自动刷新）。

## 3. 快照与垃圾回收

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

发布者侧闭环见下一节：`showroom rm-image` 删除自己的镜像并**立即**回收空间，不必等每周 GC、也不必找管理员。

## 4. 磁盘空间管理与镜像清理

**空间口径统一**（SPEC §14.1）：`GET /_api/space` → `{"available_mb","reserve_mb","total_mb"}`，可用 = 根分区可用 − policy `DISK_RESERVE_GB`（默认 20G，覆盖系统/中间件/应用运行/快照增长）。控制台上传区常显「可用空间（已扣预留）：X GB」；CLI `showroom space` 同三数。

**上传准入**（加密 import 通道，`import-begin` 处判定）：

```
密封体积 × UPLOAD_SPACE_FACTOR（默认 4）≤ 可用 − 预留
```

膨胀因子覆盖：解密临时件 + 解压膨胀（docker load 落 `/var/lib/docker` 的未压缩层，典型 2–4×）+ registry 压缩副本。不足 → `E_QUOTA`（消息含 可用/预留/估算需求，提示先删旧镜像）。registry 快通道（内网 docker 直推）无 begin 钩子，在 publish 时做同式复核（拉取展开发生在部署时刻）——两条路径判定时点不对称，属已知设计。

**镜像删除**（SPEC §14.3，删除即释放）：

```sh
# 发布者删自己的镜像（<user>/ 前缀归属校验；admin 钥匙全量）
showroom rm-image <USER>/<PROJECT>:<TAG>
# → 已删除 alice/demo:v1，回收约 512 MB
```

服务端流程：registry DELETE manifest（digest 查询后删）→ **内联触发 registry-gc**（停-收-启，秒级；live-restore 保护运行中容器）→ 返回 `freed_est_mb`。闭环「上传不够 → 删除 → 立即可传」。多 tag 共享层由 `--delete-untagged` 天然保留仍被引用者。控制台镜像列表（与部署 ComboBox 同源）每行删除按钮（confirm）。

**policy 调节**（`/srv/showroom/state/policy.env`，每次判定即读）：

| 键 | 默认 | 说明 |
|---|---|---|
| `DISK_RESERVE_GB` | 20（首跑随实测磁盘推导） | 磁盘准入预留 |
| `UPLOAD_SPACE_FACTOR` | 4 | 上传空间估算膨胀因子 |

## 5. 数据库维护

每个展项独立供给 PostgreSQL 库（`sr_<PROJECT>`，`-`→`_`）与 Redis 逻辑库，凭据由 publish 自动注入容器（`showroom env` 可查）。**维护入口在发布者手里**（主机侧不经手明文）：CLI `showroom db sql|tables|preview|dump|restore|reset` 或控制台「数据库」标签（表清单 → 预览表格 → SQL 编辑器 → 加密备份下载/恢复上传），全部经签名 API，公网可用。

| 操作 | 命令 / 入口 | 备注 |
|---|---|---|
| 执行 SQL | `showroom db sql <PROJECT> [--file F] [--confirm]` | 含 `DROP/TRUNCATE/DELETE` 必须 `--confirm`；直达 psql stdin，禁 argv |
| 列表/预览 | `showroom db tables <PROJECT>` / `showroom db preview <PROJECT> <TABLE> [--limit 50]` | 服务端上限 200 行 |
| 加密备份 | `showroom db dump <PROJECT> -o FILE` | 端到端加密（一次性 RSA 公钥上行），三段式分片下载，产物 zstd-19 包裹的 `pg_dump -Fc`；明文 >256MB → E_QUOTA |
| 恢复 | `showroom db restore <PROJECT> <FILE> --confirm` | `pg_restore --clean --if-exists`（先清目标库） |
| 重置 | `showroom db reset <PROJECT> --confirm` | DROP+重建空库，凭据不变；publish 是**首建不重建**，清库走这里 |

主机侧相关行为：发布者库属业务数据，平台自身的状态备份见 §6（btrfs 快照）；`db reset`/`restore` 不需要主机介入。

## 6. 备份与恢复（主机视角）

平台自身的状态都在 `/srv/showroom`（btrfs 快照兜底）；发布者的**业务数据**备份走签名 API 端到端加密（见 §5），主机侧不经手明文。

| 对象 | 方式 |
|---|---|
| 全量状态（conf/env/密钥/路由） | btrfs 快照（hourly/daily）+ 必要时异地同步快照子卷 |
| 某项目的数据库 | 发布者：`showroom db dump <PROJECT> -o backup.bin.zst`（端到端加密，主机无法解密） |
| 灾难重建 | 新主机按 [deployment.md](deployment.md) 重部署；恢复 `/srv/showroom/state`（tokens/users/projects）与 `_middleware/` 数据卷，`showroomctl reconcile` 重建全部容器与路由 |
| 单项目误删 | `state/projects/<PROJECT>.*` 删了即彻底删除；如有快照可从 `@showroom-snaps` 拷回 conf 后 `reconcile` |

> `state/` 含全部公钥、OPAQUE 注册记录与 registry 凭据——快照异地存储需按密钥材料对待（加密传输与保存）。

## 7. 容量调整

### 扩内存（改准入预留 / 上限）

编辑 `/srv/showroom/state/policy.env` 后立即生效（每次 publish 读取，无需重启服务）：

```sh
sudo vi /srv/showroom/state/policy.env
#   DEFAULT_MEM=256m   → 新发布缺省内存需求
#   MAX_MEM=1g         → 单项目上限（publish --mem 不得超过）
#   RESERVE_MEM_MB=1536 → 内存准入预留：MemAvailable − 预留 < 需求 → E_QUOTA
```

已发布项目改内存：`showroom publish <PROJECT> <TAG> --mem 512m`（滚动更新，conf 与容器随之更新）。

### Redis 逻辑库池

- 池大小 = policy `REDIS_DBS`（默认 256），实例启动参数 `--databases` 与之一致；每项目一库；
- 池水位常驻检查：余量 <20% 时状态面板黄条提示「建议扩容」；publish 分配到池耗尽返回 `E_QUOTA`，消息直接附扩容命令；
- 扩容（databases 变更需重启实例，走确认流）：`showroomctl middleware set redis databases=512` 先见提示（约 3 秒、AOF 持久、运行中应用连接自动重连），加 `--confirm` 才停旧起新并验证 ping+库数。**不静默重启**。

### 其他

- 项目数上限：`MAX_SLOTS`（语义 = 全平台项目数；端口池 8100–8199 是硬上界 100）；
- import 大小帽：`IMPORT_MAX_GB`；
- 密码绑定钥匙回收上限：`BOUND_KEYS_MAX`（默认 5，按 `.pub` mtime 保最近 N 把 `.bound` 钥匙；无 `.bound` 的 CI 公钥不受 GC）。

## 8. 故障排查表

| 症状 | 诊断命令 | 动作 |
|---|---|---|
| 展项 502 / 打不开 | `curl -s http://127.0.0.1:8100/`（conf 里登记的端口）；`docker ps -a \| grep sr-<PROJECT>` | 容器挂了等 autoheal（≤5min）或手动 `showroomctl start <PROJECT>`；端口连不通看 `showroomctl logs <PROJECT>` |
| **公网页面空白/404（frp 隧道）** | `curl -s -H 'Host: x' http://127.0.0.1:8080/_console/` | site B 是 `:8080` **纯端口匹配**——frp 流量 Host 是公网 IP，按 Host 匹配会全部漏掉（实弹教训）。确认：① Caddyfile site B 为 `http://:8080`（`showroomctl setup --apply` 重渲染）；② 隧道本地目标指向 127.0.0.1:8080；③ `PUBLIC_HOST` 非空（否则不渲染 site B） |
| **登录失败（CLI/控制台）** | `journalctl -u showroom-api \| grep -i 'login\|rate'` | 密码错与未设密码同文案（防枚举）。若提示「服务端拒绝（E_AUTH：限流或请求被拒）」：login-begin 双令牌桶超限（按账号 10 次/10 分 + 全局 120 次/10 分），等 10 分钟；服务重启会丢在途登录态与限流计数，重试即可 |
| 展项漂移（容器/路由被误删） | `systemctl start showroom-reconcile.service` | reconcile 幂等重建；确认 timer 在跑 |
| publish 报 `E_QUOTA`（内存） | `grep MemAvailable /proc/meminfo` | 对比 `RESERVE_MEM_MB`；扩内存或调低需求 `--mem` |
| publish/上传报 `E_QUOTA`（磁盘） | `showroom space`（可用已扣预留） | 膨胀因子×密封体积 超出 可用−预留：`showroom rm-image` 删旧镜像（立即释放）或调 `DISK_RESERVE_GB`/`UPLOAD_SPACE_FACTOR` |
| publish 报 `E_QUOTA`（no free port） | `cat /srv/showroom/state/ports.reg \| wc -l` | 端口池 8100–8199 满：`showroom rm` 清理旧项目 |
| publish 报 `E_QUOTA`（redisdb 池耗尽） | `showroomctl middleware status` | 消息附扩容命令：`showroomctl middleware set redis databases=N --confirm`（§7 确认流） |
| publish 报 `E_NET` | `docker pull 127.0.0.1:5000/<USER>/<PROJECT>:<TAG>` | 镜像不在本机 registry：发布者重新 push；检查 registry 容器与 htpasswd |
| import 卡住 / `_tmp` 涨 | `ls -lh /srv/showroom/_tmp/` | 分片会话 >24h 由 gc-images 清；急用手动删（确认无进行中上传） |
| CLI 全部命令 404 E_AUTH | `journalctl -u showroom-api \| grep E_AUTH` | 公钥未登记 / 时钟偏差 >300s（NTP）/ Caddy 改写了 path；journal 里有具体原因。钥匙被删/GC 后重新 `showroom login` 即重绑 |
| 磁盘告急 | `df -h / /srv/showroom`；`docker system df` | 见 §3 回收顺序 + §4 镜像删除 |
| registry push 401 | `cat /srv/showroom/state/users/<USER>/reg.cred`（0600） | 凭据过期/吊销：重新发卡或补发密码；CLI 侧重新 `showroom login` 自动拉新凭据 |
| 快照失败 | `systemctl status showroom-snapshot-hourly.service` | 子卷满 / 非只读父卷；报错原文在 unit 输出 |
| 控制台打不开 | `ls /srv/showroom/_console/` | 静态资源未部署（见 deployment §5 升级路径：`showroomctl upgrade` 同步 `_bootstrap/` 但不碰 `_console`） |
| setup 黄条「redis 实例 databases=16 < policy」 | `showroomctl middleware status` | 手工部署遗留实例：`showroomctl middleware set redis databases=256 --confirm` 收编（§7 扩容流） |

## 9. 安全事件响应（吊销流程）

详细策略见 [SECURITY.md](../SECURITY.md)。速查：

```sh
# 1. 删除泄露的那把钥匙（kid 从清单查：showroomctl admin users）
sudo showroomctl user-key-rm <USER> <KID>

# 2. 疑似盗号：补发登录密码（原密码即刻失效，已绑定钥匙不受影响）
sudo showroomctl user-passwd <USER> --gen-password

# 3. 整账号失能（签名 + registry 登录全失效，展项不动）
sudo showroomctl user-revoke <USER>

# 4. 管理员权限收/授
sudo showroomctl key-admin <USER> <KID> --admin false|true

# 5. 重新发卡（登录初始密码与 registry 密码两把分开发放，均仅显示一次）
sudo showroomctl user-add <USER> --gen-password

# 管理员钥匙远程操作等价 API（任何管理员钥匙签名，公网可用）：
#   POST /_api/admin/key-rm | user-revoke | key-admin | user-add | passwd-set ；GET /_api/admin/users
```

事后检查：`journalctl -u showroom-api --since "<时间>" | grep -E 'E_AUTH|admin|login'` 评估泄露窗口内的管理面与登录调用痕迹。

## 10. 版本升级

```sh
# 首选：就地升级（探针→备份→原子替换→restart 探活→失败回滚；同步 user CLI 与 _bootstrap/，不碰 _console）
scp target/release/showroomctl <USER>@your-host:/tmp/showroomctl
sudo showroomctl upgrade                # 默认取 /tmp/showroomctl；--bin <路径> 可指定

# 源码构建路径
git pull && cargo nextest run --workspace && cargo build --release
sudo install -m755 target/release/showroomctl target/release/showroom /usr/local/bin/
sudo systemctl restart showroom-api
sudo showroomctl reconcile          # 旧布局迁移（如有）幂等完成
```

升级前建议手动 `showroomctl snapshot daily` 留一份回滚点。控制台静态资源（`_console/`）升级独立于二进制：替换文件后重跑 `/usr/local/bin/showroom-precompress`。
