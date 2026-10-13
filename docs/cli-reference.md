# Showroom CLI 参考

发布者 CLI（`showroom`）的全部命令。除 `push` 需要 `docker` 命令行外零运行时依赖；所有管理请求经 SR-SIG1 签名（见 [security.md](security.md)）。主机侧工具 `showroomctl` 的命令面向管理员，见文末附录。

命令中 `<PROJECT>`、`<TAG>`、`<USER>`、`your-host` 为占位符。

## 0. 全局

```
showroom [--server lan|public] <子命令>
```

| 项 | 说明 |
|---|---|
| `--server lan\|public` | 强制服务端通道；缺省自动探测（优先 lan，探不通回落 public） |
| `--version` | 版本号 |
| `--help` | 帮助 |

本地文件（权限 0600）：

| 文件 | 内容 |
|---|---|
| `~/.showroom/config.toml` | user（账号）、双服务端地址、registry 凭据（`login` 自动写入） |
| `~/.showroom/id_ed25519` | 发布者 Ed25519 私钥（裸 32B seed；`login` 自动生成绑定） |
| `~/.showroom/platform_rsa.pub` | 平台 RSA 公钥（import 加密通道；控制台登录后自动取，或控制台帮助页「工具下载」下载） |

## 1. 登录与会话

### showroom login

```sh
showroom login [<账号>] [--server-lan URL] [--ephemeral]
```

登录一次，之后全部子命令免密码（gh/ssh 模型）。流程：交互读密码（不回显、不进 argv/env/ps）→ **OPAQUE** 认证（密码经盲化永不上线）→ 自动生成本机 Ed25519 钥匙并绑定账号 → 经登录态信任通道拉取 registry 凭据自动写入 config。

- `<账号>` 缺省用 config 里的 user；`--server-lan` 缺省用 config 的 server_lan（回落 server_public）；
- `--ephemeral`：临时会话，**不落任何盘**（进程退出即弃，服务端 keep-N GC 收尾）——一次性/共享机器用；
- 本机已有有效钥匙时自动跳过（提示「已是登录态」）；钥匙被服务端删除/GC 后重新 login 即视为找回/换设备，自动落新钥匙重绑；
- 密码错误与「账号未设密码」返回同一文案（防枚举；未设密码请联系管理员补发）。

```text
登录成功：账号 alice，kid 1a2b3c4d [ADMIN]
密钥: /home/alice/.showroom/id_ed25519
配置: /home/alice/.showroom/config.toml（server_lan / registry 凭据已自动写入）
```

### showroom logout

```sh
showroom logout
```

清本地登录态：删除本机钥匙、清除 config 身份字段（server 地址保留，下次 login 免输），并恒为远程注销本机这把钥匙（`/_api/key-selfrm`，仅删请求签名所用 kid 自己；服务端不可达时如实报错但本地仍清理）。

### showroom keygen（隐藏命令：调试用）

```sh
showroom keygen [--force]     # 生成/重置本机签名密钥；已存在拒绝覆盖（--force 强制）
```

**日常登录走 `showroom login`，不要用本命令**——keygen 仅用于排查签名问题或强制换钥（生成后仍需 `showroom login` 绑定登记）。统一账号密码认证（B7）后不再支持预登记公钥发卡，CI/机器用户与日常登录一致走账号密码 `login`。已从 `--help` 隐藏。

### showroom config

```sh
showroom config                                        # 无参数=打印当前配置
showroom config --user <USER> \
    --server-lan http://showroom-lan.your-host \
    --server-public http://showroom-public.your-host \
    --registry-user <USER> --registry-password '<registry 密码>'
```

仅更新给出的字段；`--server-lan ""` / `--server-public ""`（空串）清除该字段。地址须以 `http(s)://` 开头。registry 凭据供 push 快通道 `docker login` 使用（密码只走 stdin，不进 argv）。**日常无需手工 config**——`login` 已自动写好 user/server_lan/registry；仅多服务端地址或凭据轮换时用。

### showroom doctor

```sh
showroom doctor
```

体检并逐项打印：密钥可读（不可读 → 提示 `showroom login`）/ user 已配置 / server_lan 与 server_public 可达（签名探测）/ 签名自测（用不存在的项目名验证：返回 `E_NOTFOUND` = 验签通过且密钥已登记；`E_AUTH` = 未登记或时钟偏差——提示重新 login 重绑）/ registry 凭据有效（非关键项）/ docker 可用（仅 push 需要）。任一关键项失败 → 非零退出。

## 2. 推送与发布

### showroom push

```sh
showroom push <PROJECT> <TAG> [--via auto|registry|import]
```

把本地镜像推到服务端，不发布。

- `--via auto`（缺省）：lan 可达走 registry 快通道（`docker tag` + `docker login` + `docker push` 经 `/v2`），失败自动回落加密 import 通道；lan 不可达直接走 import；
- `--via registry`：强制快通道（需要 `server_lan` 与 registry 凭据）；
- `--via import`：强制加密通道：`docker save | zstd-19 | seal(平台公钥)` 流式成临时信封 → `POST /_api/import`；服务端先查 `import-check`（镜像 ID 已登记则跳过上传）。

import 通道需要 `~/.showroom/platform_rsa.pub`。仓库名规则：`<host>/<USER>/<project 小写>:<TAG>`（docker 仓库名强制小写，项目名大写自动转小写）。

### showroom publish

```sh
showroom publish [<PROJECT>] <TAG> [--mem 256m] [--cpu 0.5] [--container-port 8080] [--wait]
```

发布/更新项目。**单参数 = tag，项目名自动生成随机 8 位并醒目打印**；两参数 = `<PROJECT> <TAG>`。

- `--mem`：内存需求（docker `--memory` 硬限），缺省取平台 `DEFAULT_MEM`，上限 `MAX_MEM`；受内存准入约束（`MemAvailable − RESERVE_MEM_MB < 需求` → `E_QUOTA`）；
- `--cpu`：CPU 配额（核数，正数如 `0.5`、`2`）；
- `--container-port`：Demo 应用在容器内的监听端口；
- `--wait`：轮询至 `status=running`（2s 间隔，90s 超时）后打印完整 URL。

滚动更新语义：先起 `-new` 后缀新容器、健康（TCP 可连或 60s 放行）后删旧改名。数据库**首建不重建**——重发布保留数据，清库走 `db reset`。

### showroom deploy

```sh
showroom deploy [<PROJECT>] <TAG> [--mem] [--cpu] [--container-port]
```

一键部署（原子命令）= `push`（auto 通道）+ `publish --wait` + 打印最终 URL 横幅。任一步失败即停，报错标明已到第几步：

```text
deploy: 第 1 步(push) 失败：docker push 失败
```

## 3. 查看与生命周期

| 命令 | 说明 |
|---|---|
| `showroom ps [--json]` | 本人项目列表（PROJECT/STATUS/TAG/USER；跨用户全量为管理员控制台「全部项目」面板，走 `GET /_api/admin/projects`）；`--json` 机读 |
| `showroom images [--json]` | 列出自己的镜像 `<USER>/<PROJECT>:<TAG>`（管理员全量）；`--json` 机读 |
| `showroom space [--json]` | 服务端磁盘空间三数（可用/预留/总量；可用行含预留扣减明细，MB）；`--json` 机读 |
| `showroom rm-image <USER/PROJECT:TAG>` | 删除自己的镜像并内联 registry-gc 立即回收空间，打印「已删除 …，回收约 N MB」 |
| `showroom url <PROJECT>` | 打印项目 URL（`http://<server>/<PROJECT>/`），不开浏览器 |
| `showroom logs <PROJECT> [--tail 100] [-f]` | 查看容器日志；`-f` 跟随（2s 轮询追加式打印） |
| `showroom env <PROJECT>` | 打印注入容器的环境变量（DB/Redis 连接信息一目了然） |
| `showroom rollback <PROJECT>` | 回滚到上一 tag（`last2` 记录），旧 tag 写回 `last2` |
| `showroom stop <PROJECT>` | 停止（标记 stopped，autoheal 跳过） |
| `showroom start <PROJECT>` | 启动（容器缺失则按 conf 重建） |
| `showroom rm <PROJECT> --yes` | 彻底删除（容器/路由/DB/凭据/登记全清）；必须 `--yes` |

## 4. 数据库维护（db 全家）

user 只能操作自己项目的库 `sr_<PROJECT>`；全部签名、公网可用。危险语句均需 `--confirm` 二次确认。

| 命令 | 说明 |
|---|---|
| `showroom db sql <PROJECT> [--file F] [--confirm]` | 执行 SQL（缺省读 stdin；`--file` 读文件）。语句含 `DROP/TRUNCATE/DELETE` 时必须 `--confirm`，否则 `E_STATE` 拒绝。输出为 psql 文本 |
| `showroom db tables <PROJECT>` | 列出 public schema 的表 |
| `showroom db preview <PROJECT> <TABLE> [--limit 50]` | 预览表数据（制表符分隔，服务端上限 200 行） |
| `showroom db dump <PROJECT> [-o FILE \| --stdout]` | 加密备份：客户端生成一次性 RSA-3072 密钥对 → 服务端 `pg_dump` 直连加密流式写临时区 → 4MB 分片下载 → 本地解密落盘。产物为 zstd-19 包裹的 `pg_dump -Fc` 格式。明文 >256MB → `E_QUOTA`；就绪等待上限 1h |
| `showroom db restore <PROJECT> <FILE> --confirm` | 恢复：备份文件（zstd-19 包裹或原生 `-Fc` 均可）加密上传 → 服务端 `pg_restore --clean --if-exists`（先清目标库） |
| `showroom db reset <PROJECT> --confirm` | `DROP DATABASE WITH (FORCE)` + 重建空库，凭据不变 |

示例：

```sh
showroom db sql demo --confirm <<< 'DROP TABLE IF EXISTS sessions;'
showroom db tables demo
showroom db preview demo users --limit 10
showroom db dump demo -o demo-$(date +%F).bin.zst
showroom db restore demo demo-2026-10-10.bin.zst --confirm
```

## 5. Agent Skill

### showroom skill

```sh
showroom skill [install]     # install 为缺省动作，当前唯一动作（其他动作报「未知动作」）
```

把发布 Skill 安装到 `~/.agents/skills/showroom-publisher/`（内嵌于 CLI 的 SKILL.md 操作手册，重复执行覆盖更新）。安装后，编码代理在用户提到「发布 demo / 部署演示环境 / 回滚 demo」等时自动按该手册使用 `showroom` CLI，不手写 curl 或 docker 命令。手册内容：doctor 前置检查、镜像规范（8080 监听 / 子路径 / 环境变量注入）、发布标准流、日常操作、db 维护与硬规则。

## 6. 退出码与错误码

### 进程退出码

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 运行时错误（stderr 前缀 `错误:`；`deploy` 失败带步骤号） |
| 2 | 用法/参数错误（clap 解析失败，含缺失 `--yes`/`--confirm` 之外的参数问题） |

注：破坏性确认（`rm --yes`、`db restore --confirm`、`db reset --confirm`、`keygen` 覆盖）在业务层检查，缺失确认串 → 退出码 1，错误信息给出正确用法。

### 服务端错误码（出现在错误信息前缀）

| 码 | HTTP | 含义 |
|---|---|---|
| `E_AUTH` | 404 | 未认证/验签失败/密钥未登记/时钟偏差 >300s/非管理员访问管理端点/登录限流或被拒（与路径不存在不可区分） |
| `E_NAME` | 400 | 名字/参数非法（project/tag/mem/cpu/表名等） |
| `E_QUOTA` | 429 | 配额：项目数上限、端口池耗尽、Redis 逻辑库池耗尽、内存准入不足、磁盘准入不足（提示先删旧镜像）、import 超 IMPORT_MAX_GB、dump 超 256MB |
| `E_NOTFOUND` | 404 | 项目不存在或非本人归属 |
| `E_STATE` | 500 | 状态冲突（如 dump 未就绪、SQL 需确认、需扩容确认） |
| `E_NET` | 502 | 依赖不可达（registry / 数据库） |

登录失败提示「密码错误，或该账号尚未设置密码」且重试仍 404 时：大概率是 login-begin 限流（按账号 10 次/10 分 + 全局 120 次/10 分，纯内存令牌桶），等 10 分钟再试；服务端细节看 `journalctl -u showroom-api`。

## 附录：showroomctl（主机侧，管理员）

仅主机 root 使用（serve 由 systemd 托管）；全局 `--json` 切机器可读输出。

### 账号与钥匙

| 命令 | 说明 |
|---|---|
| `showroomctl user-add <USER> (--password '<初始密码>'\|--gen-password) [--admin]` | 发账号：设初始登录密码（必填其一）+ 生成 registry 凭据（bcrypt）。`--gen-password` 自动生成初始密码一次性返回；`--admin` 授管理员（账号级，即刻落 `users/<u>/admin` 生效——对整账号全部钥匙生效；旧 `admin.pending` 仅作存量迁移输入）。统一账号密码认证（B7）：预登记公钥路径已删除 |
| `showroomctl user-passwd <USER> [--password '<新密码>'\|--gen-password]` | 重置/补发登录密码（替换 `pass.opaque`，**不动既有钥匙**）；一次性返回新密码 |
| `showroomctl user-key-rm <USER> <KID>` | 删除某把公钥（admin 是账号级属性，删钥匙**不影响**该账号的管理员身份） |
| `showroomctl user-admin <USER> --admin true\|false` | 授/收管理员权限（账号级） |
| `showroomctl user-revoke <USER>` | 吊销账号（签名与 registry 登录全失效，项目不动；users 目录整删） |
| `showroomctl admin users\|status` | 管理员视图：账号与设备清单（含「未设密码」徽标）；服务/磁盘/内存/容器/timers 面板 |

### 主机自举与编排

| 命令 | 说明 |
|---|---|
| `showroomctl setup [--apply] [--lan-host <地址>] [--public-host <地址>]` | 主机自举：无 `--apply`=diff 模式（列变更与偏差）；`--apply` 幂等收敛目录/policy/中间件容器/Caddy/systemd/调优。首跑按探测值生成 `state/host.env`，限额缺省随实测硬件推导 |
| `showroomctl middleware status\|enable <名>\|disable <名> [--force]\|set <名> <k>=<v> [--confirm]` | 中间件编排：期望/实际/漂移对照；`set redis databases=N`（16–256）为 Redis 逻辑库扩容确认流（无 `--confirm` 只见提示不动容器——变更需重启实例） |
| `showroomctl upgrade [--bin <路径>]` | 就地升级二进制（探针→备份→原子替换→restart 探活→失败回滚；默认取 `/tmp/showroomctl`；同步 user CLI 与 `_bootstrap/`） |

### 服务与运维

| 命令 | 说明 |
|---|---|
| `showroomctl serve` | 启动控制面（127.0.0.1:8090；正常由 `showroom-api.service` 运行） |
| `showroomctl publish <USER> <PROJECT> <TAG> [--mem --cpu --container-port]` | 管理员直发（与 `/_api/publish` 同一内核） |
| `showroomctl stop\|start\|rm\|rollback <PROJECT>` | 项目生命周期（rm 全清；管理 API 侧同名端点另有归属校验） |
| `showroomctl ps\|logs <PROJECT>\|env <PROJECT>` | 项目查看（`--json` 机读） |
| `showroomctl reconcile\|autoheal` | 幂等重建 / 自愈（timer 平时自动跑） |
| `showroomctl gc-images\|gc-registry\|registry-gc` | 垃圾回收三件套 |
| `showroomctl snapshot hourly\|daily` | btrfs 快照（保留 24/7） |
| `showroomctl import <USER> <PROJECT> <TAG>` | 从 stdin 读加密信封导入镜像（`/_api/import` 同一内核） |
| `showroomctl import-check <IMAGE_ID>` | 本机是否已有该镜像 |
| `showroomctl init-keys` | 生成平台 RSA 对（已存在不覆盖；serve 启动缺失时也会自动生成） |

环境变量：`SHOWROOM_ROOT`（默认 `/srv/showroom`）、`SHOWROOM_BASE_URL`（publish 返回 URL 的前缀）。
