# Plan 19：中间件连接呈现 + 多容器项目（服务编排）— 设计与实施

> 状态：设计定稿即实施。基准 HEAD d0e38f4。
> 两个用户需求：①UI 完全没有说明容器内应用如何连接中间件；②一个项目需要多个容器互相通信，目前不支持。

## A. 中间件连接呈现（纯 UI 面，后端契约已备）

数据源=既有 `GET /_api/env`（逐项目真实值，SPEC §6 十二键）与 `showroom env`。呈现三处：

1. **我的项目 → 行抽屉「连接信息」区**：调 `/_api/env` 渲染键值表（`DB_PASSWORD` 打码可点显）+
   `DATABASE_URL`/`REDIS_URL` 整行复制 + envsubst 模板示例 + 服务发现键（B 部分注入的
   `SHOWROOM_SVC_*` 一并展示）。
2. **发布页**：静态「发布后将自动注入」键清单（无值，含禁用组自适应说明），发布成功视图
   附首个项目即可去抽屉查看的引导。
3. **帮助页新面板「中间件连接」**：12 键语义表、连接串示例、envsubst 推荐模式、多容器服务
   发现说明（B 部分）。

## B. 多容器项目（服务编排 v1）

### B.1 模型（存量零迁移）

- Conf 增 `services: Vec<Svc>`（默认空=现状单容器，旧 conf 读入得空表，无迁移）：
  `Svc { name, image, cport, mem, cpu }`——name 校验同项目名规则；image 为**平台镜像库内
  引用**（无 `/` 前缀自动补 `<user>/`；`registry:2` 同库）。
- 容器命名：主服务保持 `sr-<project>`（公网端口+Caddy 路由不变）；附加服务
  `sr-<project>-<svc>`（**仅内网**，无 host 端口映射）。
- 互通：全部挂 `showroom-net`（Docker 内嵌 DNS 按容器名解析）；env 注入**服务发现键**：
  对每个服务（含主）注入 `SHOWROOM_SVC_<NAME 大写-_>_HOST=<容器名>` 与 `_PORT=<cport>`
  → 任意容器 12-factor 读 env 即可找到同伴。

### B.2 发布语义

- publish 携带 `services[]`（CLI `--svc name=image:tag, port=, mem=, cpu=` 可重复；
  UI 发布页「附加服务」重复行）。每个服务镜像必须已在平台镜像库（manifest 校验，
  缺失 E_NOTFOUND 指名）。
- 配额：`SERVICES_MAX`（policy.env 新键，默认 4，含主服务）；内存准入按**全体服务和**。
- 生命周期不变式：publish = 主服务滚动（既有逻辑）→ 附加服务「对齐」：新集合中创建/重建
  （配置变了才重建），不在集合中的旧附加服务删除；stop/start/rm/reconcile/autoheal 遍历
  全体服务容器；logs 增 `service=` 参数（缺省主服务）；ps 展示 `服务 N`。
- 版本/回滚：v1 版本历史仍只记主服务镜像（诚实边界：附加服务随发布固定，回滚不动它们）。

### B.3 实现落点

| 层 | 变更 |
|---|---|
| state.rs | Conf.services 字段（serde default）；`env_file_contents(..., services)` 注入发现键；Policy.services_max |
| publish.rs | services 校验/镜像解析/逐服务 run（内网无 -p）；对齐删除；redeploy 带服务 |
| ops.rs | stop/start/rm/logs(+service)/reconcile/autoheal 遍历；ps 输出服务数 |
| serve.rs | publish body 解析 services；logs?service= 透传 |
| CLI | publish --svc（可重复）；logs --svc；ps/env 不变 |
| 控制台 | 发布页附加服务重复行；抽屉连接信息区（A.1）；帮助面板（A.3） |
| 文档 | SPEC §6 发现键 + SERVICES_MAX；deployment/architecture 同批 |

### B.4 测试与验收

- 单测：services serde 往返/默认空；发现键生成；镜像引用解析；SERVICES_MAX 越限。
- 真机 E2E：发布 `e2e-multi`（主=hello 夹具 httpd + 附=`sidecar` busybox sleep）→
  `docker exec sidecar wget -qO- sr-e2e-multi:8080` 实证 DNS 互通 + exec env 见
  SHOWROOM_SVC_*；stop/rm 全体容器清；UI 抽屉连接信息呈现；CLI --svc 发布。

## C. 明确不做（v1 边界）

- 服务间依赖/启动顺序编排（Docker 无原生依赖；网络 DNS 自愈即可，健康等待不做）。
- 附加服务的公网端口/路由（一律内网；需要公网=拆两个项目）。
- per-service 版本历史/回滚、日志聚合（logs 按 service 查询单流）。
- compose 文件导入。
