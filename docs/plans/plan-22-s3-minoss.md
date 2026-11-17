# plan-22：S3 兼容对象存储中间件（minoss）

> 状态：已实现（2026-11-17）。SPEC 落点：§5（布局/cred）、§6（S3_* 契约与供给）、
> §10.5（浏览端点）、§15（中间件集合）。本文记录设计裁决与依据，行为以 SPEC 为准。

## 0. 需求与裁决（2026-11-17 用户拍板）

- 平台增加 S3 兼容对象存储，供部署的容器应用使用；实现采用自维护 MinIO 分叉
  [fly88oj/minoss](https://github.com/fly88oj/minoss)（AGPLv3；release 镜像
  `ghcr.io/fly88oj/minoss`，**镜像内置 mc**，基座 ubi-micro 含 sh）。
- ①镜像**钉具体 RELEASE tag**（本批钉 `RELEASE.2026-11-17T14-06-43Z`，多架构 ghcr 发布）；
- ②**每项目独立桶 + 独立密钥**（对齐 PostgreSQL 每项目库/角色模型）；
- ③**不映射任何 host 端口**（S3 9000 与 Console 9001 均 仅 showroom-net 内网）；
- ④控制台 Data 页本期一并做 S3 浏览（对齐 Redis 浏览的只读形态：浏览/预览/下载）。

## 1. 中间件容器（setup.rs）

- `sr-minoss` = `ghcr.io/fly88oj/minoss:<RELEASE tag>`，`--network showroom-net
  --restart unless-stopped -e MINIO_ROOT_USER/MINIO_ROOT_PASSWORD -v
  <root>/_middleware/minoss:/data`，cmd `server /data --console-address ":9001"`
  （entrypoint 自动前缀 minio）；**无 -p**。
- root 凭据：用户名固定 `showroom`，密钥 `rand_hex(16)` 首跑生成；存
  `state/middleware.cred` 的 `minio:<user>:<secret>` 行（该文件改**行感知**多行
  `name:value`——postgres/minio 各一行，两分支合并一次落盘互不覆写；旧单行文件
  按行解析自然兼容）。
- 数据目录经 `expected_dirs` 的 MW_NAMES 循环自动纳入；容器编排（enable/disable/
  status/两段式 impact/drift）全部经既有 MW_NAMES 泛化路径，仅名单扩为 4 元素。

## 2. 每项目供给（s3.rs；publish 在 DB 供给后调用）

- 桶 `sr-<project 小写>`（S3 桶名必须小写；项目名允许大写 → 发布归一化阶段做
  **小写重名跨项目检查**，撞名 E_NAME 零副作用拒绝）。
- 访问键 `sr + sha256(project)[..16]`：确定性（rm 可重算拆除）、大小写项目不撞、
  纯字母数字（mc 访问键字符集安全）。密钥 `rand_hex(16)` **每次发布轮换**（对齐
  DB 密码模型；env 文件持久化，reconcile/autoheal 重建复用不轮换）。
- 桶级 IAM 策略：`s3:*` 限 `arn:aws:s3:::<桶>` 与 `<桶>/*` 两层；策略名=桶名。
- 执行：`docker exec -i sr-minoss sh -c <脚本>`（镜像内置 mc）。**凭据恒走 stdin 行**
  （root 两行 + 新密钥一行，脚本内 read 后进 MC_HOST 环境变量）；argv 只含派生名
  与常量脚本——与 psql SQL 走 stdin 同款纪律，宿主 ps 命令行零凭据。
- 幂等：桶 `--ignore-existing`；`user add` 重复执行=密钥轮换；policy create 已存在
  容忍（attach 失败以真实原因浮出）。
- 生命周期：`db-reset` 沿用 env 中的 S3 密钥重灌（「凭据保持不变」语义同 DB；未注入
  过则生成新密钥=升级路径）；`rm` 删桶（`mc rb --force` 含对象）+用户+策略，尽力幂等
  （对齐 DROP DATABASE）；stop/start/reconcile/autoheal 无 S3 动作。

## 3. env 注入（state.rs）

minoss 启用且已供给时注入 4 键（位置在 REDIS_* 之后）：

```
S3_ENDPOINT_URL=http://sr-minoss:9000
S3_ACCESS_KEY_ID=<访问键>   S3_SECRET_ACCESS_KEY=<密钥>   S3_BUCKET=<桶>
```

禁用 → 整组不注入（语义同 REDIS_*）；`${VAR}` 插值与 uenv 覆写天然可用。
`mw_dependents("minoss")` 以 env 含 `S3_ENDPOINT_URL=http://sr-minoss:9000` 判依赖
（disable 门槛）；mw_disable/mw_enable/action_touches_mw 均按既有泛化路径收编。

## 4. 浏览端点与控制台（serve.rs / Data.tsx）

- `GET /_api/s3-objects|s3-get|s3-download`：`user_check` 项目归属门禁（主人或
  admin）；后端**用项目自身凭据**访问（桶级隔离的实证路径）。限制：列表单次 500 条
  （`mc ls --json` 非递归一层，目录行下钻）、预览 64KB（UTF-8 校验，二进制给标记）、
  下载 8MB（base64 JSON，超出 E_STATE 提示容器内直取）。
  前缀/对象键白名单 `^[A-Za-z0-9._/-]+$`（脚本拼接与路径歧义双防）。
- 控制台：Data 页第三 tab「S3 对象」——前缀输入/上一级/目录下钻/文件预览/下载，
  只读（不做上传删除）；ENV_DESC 增 4 键、S3 密钥与 DB_PASSWORD 同款打码点显、
  Help「中间件连接」补 S3 行与 SDK 用法（region us-east-1 + path-style）、Admin
  中间件面板增 minoss 开关。

## 5. 边界与非目标

- 不映射公网/主机端口；Console 9001 仅内网（运维需主机内上下文时走容器网络）。
- 上传/删除/桶策略编辑不在控制台范围（应用用 S3_* 凭据直连；CLI 后续按需立项）。
- 服务间依赖/启动顺序不做（对齐 plan-19 v1 边界：DNS 自愈+应用重连）。
- 大对象（>8MB）不经控制台下载；mc 镜像版本随 release 钉死。
