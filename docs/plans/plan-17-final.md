# §17 控制台 IA 定稿（三轮评审合并实施稿）

> 来源标注：**[D]**=daqian 第一轮 · **[M]**=makesi 独立稿 · **[Z]**=dazhi 本轮裁决/新增。
> 素材：plan-17-ia.md、plan-17-ia-makesi.md、现状 dist/index.html（2811 行）、SPEC §3/§9.1/§13/§14、smoke-wizard.mjs（id 依赖盘点）。
> 范围：`showroom-web/dist/index.html` DOM/JS 重排 + `showroomctl` whoami 加一字段 + SPEC §17 增补；其余后端零改动。

## 0. 两案分歧与裁决

| # | 分歧 | daqian | makesi | 定稿裁决 |
|---|---|---|---|---|
| 1 | 导航模型 | 五任务标签 | 无标签三区+抽屉 | **保留标签**[Z]，采纳 M 的「落地即答案」原则。理由：数据/帮助体量大，单纵页会退化为现状长页；role=tablist 键盘模型已实现且被 smoke 断言，抽屉焦点管理重写回归风险高。M 的「卡片永远可见」翻译为：默认标签=我的展项，零点击见 URL。 |
| 2 | 状态粒度 | 7 后端态+2 瞬态=9 显示态 | 4 态（缺发布中） | **7 显示态**[Z]（§4 表）：服务器真值 5 态 + 本地在途 2 态；后端原始 status 保留进 tooltip[D]。 |
| 3 | 参数单源 | localStorage `sr.conf.*` | 项目元数据（需新写端点） | **分层单源**[Z]：mem 以 `GET /_api/projects` 的 conf 为权威（任务点名 API 已有）；cport/tag 用 `sr.conf.*` 记忆补（projects 响应无 cport）。M 的设置抽屉写元数据否决——需新端点，破坏零后端边界。 |
| 4 | PEM（F） | 设置对话框收纳 | 新公开端点 server-pubkey | **whoami 响应加 `platform_pem` 字段**[Z]：服务端读 `state/platform_rsa.pub` 约 3 行；RSA 公钥本就公开、经签名端点下发零信息暴露；缓存写现有 `sr.pempub` 键。M 的未签名新端点否决——`/_api/*` 一律签名（SPEC §3）是防枚举基线，不开第二个未签名族。设置对话框保留手工粘贴兜底[D]。 |
| 5 | 刷新按钮 | 卡片头 ⟳ 兜底 | 「X 秒前」可点链接 | **合并**[Z]：卡片副题「已加载 N 项 · HH:MM:SS」整段可点重取（=⟳+相对时间）；显式「刷新/载入」按钮全删。 |

其余共识直接采纳：三入口收敛单表单双镜像源（§5）、自动加载（§3）、删除确认说清后果（§8-14）、登录后焦点引导[M]。

## 1. 新页面 DOM 结构（id 层级草图）

```
body
├─ #msg                          [保留] 全局消息条（role=alert）
├─ #login                        [保留] 登录卡；lg-*/wz-* 全部原样不动
├─ <template id="app-tpl">       [保留] 登录后惰性克隆（零信息暴露不变）
│  ├─ header（单行 ≤56px）
│  │  ├─ #hd-user #hd-srv        [保留] 账号 / 服务器
│  │  ├─ #hd-role                [新增] 角色徽标 发布者/管理员/?（whoami 未回）
│  │  ├─ #btn-new-pub            [新增] 唯一常驻 CTA「+ 发布」→ activateTab('pub')
│  │  ├─ #btn-settings #btn-pwchange #btn-logout   [新增/保留/保留]
│  │  └─ nav#tabs                [保留容器] role=tablist
│  │     ├─ #tabbtn-proj → #tab-proj   [保留] 我的展项（默认落地）
│  │     ├─ #tabbtn-pub  → #tab-pub    [新增] 发布（单表单）
│  │     ├─ #tabbtn-img  → #tab-img    [新增] 镜像库
│  │     ├─ #tabbtn-db   → #tab-db     [保留 id] 数据（DB+Redis 合并）
│  │     ├─ #tabbtn-help → #tab-help   [保留]
│  │     └─ #tabbtn-admin→ #tab-admin  [保留] whoami admin 才渲染
│  ├─ #tab-proj：#proj-tbl[保留]（列：名称·状态·对外URL·标签·更新·操作·▾）
│  │    行操作序[D]：打开·更新·日志·启动/停止·更多▾(回滚/环境变量/数据/删除)
│  │    行展开抽屉（tr 后插行，data-drawer="<project>"，无全局 id）：
│  │      内嵌 #st-logs #st-logs-live #st-logs-target #st-logs-fb [迁移保留]
│  ├─ #tab-pub：#pub-form[保留]（见 §5 字段）；#dp-bar 及 dp-* [保留]
│  ├─ #tab-img：#img-list[迁移保留] + #space-note[迁移保留] + 仅上传入口 + 断点续传区（#btn-resume #up-id #up-ids #up-prog #up-status [迁移保留]）
│  ├─ #tab-db：db-* [保留] 全部；rd-* [迁移保留]（Redis 浏览自状态页迁入）
│  ├─ #tab-help：[保留] + 专家路径折叠（wz-*）[保留]
│  └─ 对话框：#dlg-env #dlg-pw [保留]；#dlg-settings [新增]
│       （#hd-pempub[迁移保留]、#set-base[新增 对外访问域名]、改密/忘记设备入口聚合）
├─ <template id="admin-tpl">     [保留] #tab-admin + ad-*/hs-*/dlg-* 全部不动；
│    事件流区（#ev-state #st-events [迁移保留]）并入管理页[M→Z 收敛：docker events 原始流降为管理员工具]
└─ #app                          [保留] 克隆挂载点
```

### id 改名/增删对照表（JS 与 smoke 兼容的唯一权威清单）

| 处置 | id | 去向 |
|---|---|---|
| 删除 | `tab-status` `st-tbl` `st-auto` `st-fb` `st-refresh` | 能力并入 #tab-proj（列表+抽屉）与管理页（事件流） |
| 删除 | `dep-form` `dep-project` `dep-tag` `dep-mem` `dep-cport` `dep-submit` `dep-fb` `dep-project-err` `dep-tag-err` | 三入口收敛（§5）；smoke-wizard §12 改写为镜像源 radio 路径 |
| 删除 | `btn-refresh` `dep-images-refresh` `btn-db-fill` `space-refresh` | 自动加载（§3）取代 |
| 迁移保留 | `hd-pempub`（header→设置对话框，localStorage 键 `sr.pempub` 不变）；`st-logs*`（→行抽屉）；`dep-image` `dl-images` `dep-images-fb` `img-list`（→发布表单镜像库源/镜像库页）；`rd-*`（→数据页）；`ev-state` `st-events`（→管理页） | id 不变，仅换父容器——JS 取元素方式（$() 全文档）不受影响 |
| 新增 | `tab-pub` `tabbtn-pub` `tab-img` `tabbtn-img` `hd-role` `btn-new-pub` `btn-settings` `dlg-settings` `set-base` `set-base-err` `pub-src-tar` `pub-src-registry` `pub-src-file-box` `pub-src-reg-box` `pub-pref-note` | |
| 保留 | 其余全部（`lg-*` `wz-*` `pw-*` `pub-project/tag/mem/cport/file/shuffle/url` `dp-*` `up-*` `db-*` `ad-*` `hs-*` `dlg-env` `dlg-pw` `dl-projects` `dl-mem` `msg` …） | 零改名 |

> 同 PR 必须同步改 `showroom-web/smoke-wizard.mjs`：§12 部署表单用例→radio 路径；st-* 健康表用例→行抽屉；新增「按钮零残留/单表单/状态词表」静态断言。

## 2. 六个用户问题 → 解法落点

| # | 问题 | 解法（节） | 来源 |
|---|---|---|---|
| A | 内网地址暴露（p.url=http://内网IP/…） | §6 URL 策略：主链接一律 `externalUrl()`，服务端 url 仅 tooltip | [D 算法+M 复制拼基址] |
| B | 内存双填（pub-mem 与 dep-mem 两处） | §5 发布表单=唯一参数处；dep-* 删除；预填自 projects+sr.conf | [D+Z 分层单源] |
| C | 「未部署」语义不明 | §4 状态机：missing→**待部署**（可点「部署」拉起）；「未部署」一词全站禁用 | [M 的 image-only 可操作化+D 的改名，取「待部署」：动词开头暗示下一步] |
| D | 手动加载（载入项目清单/刷新镜像清单/获取表清单前置） | §3 自动加载表；「载入/刷新」按钮 DOM 归零 | [D 表+M 链接化合并] |
| E | 层级混乱（四标签+fieldset 嵌套+对话框叠对话框） | §1 三级封顶（顶栏→标签→卡片）；卡片内禁嵌 fieldset；§7 视觉 token | [D 规范+M 反 fieldset] |
| F | PEM 占顶（header 常驻 textarea） | whoami 附带 platform_pem 自动取+缓存；PEM 迁设置对话框，缺 PEM 时内联引导 | [Z 裁决+D 收纳] |

## 3. 数据自动加载清单表

登录即取四路 `Promise.allSettled` 并发（projects/images/space/whoami）[D]；进页即订阅事件流[M]。

| 数据源 | 端点 | 触发时机 | 并发 | 失败表现 | 缓存 / 失效 |
|---|---|---|---|---|---|
| projects | GET /_api/projects | 登录即取；切展项/发布/数据页且 TTL 过期；publish/start/stop/rm/rollback 成功后；事件流 sr-* 2s 去抖 | 登录 4 路并发 | 表格内联「加载失败 · 重试」+3s 自动重试 1 次+副题手点 | 内存+TTL 30s；写操作/事件强制失效 |
| images | GET /_api/images | 登录即取；切发布/镜像库页 TTL 过期；whoami admin 后重取（管理员视图标注） | 同上 | 下拉区内联错误+副题手点 | TTL 300s；import-end/image-rm/仅上传后失效 |
| space | GET /_api/space | 登录即取；切镜像库/发布页；import-end/image-rm 后 | 同上 | 静默「空间信息不可用」，30s 后再试 | TTL 60s |
| whoami（+platform_pem） | GET /_api/whoami | 登录即取 | 同上 | 徽标「?」60s 周期重试；PEM 回落 sr.pempub | 会话级 |
| redis-dbs | GET /_api/redis-dbs | 数据页 Redis 区首次展开（惰性） | 单路 | 内联错误+重试 | 会话级；发布/删除项目后失效 |
| events 流 | GET /_api/events | 展项页可见时订阅；`document.hidden`/离页退订（服务端收尸子进程） | 流式 | 断线 5s 自动重连（保留现状） | — |
| logs 流 | GET /_api/logs-stream | 行抽屉展开时订阅，收起/换行即断 | 流式/单路 | 「连接断开 5 秒重连」（保留现状） | — |
| admin users/status/host | 各 admin 端点 | 切管理页 | 3 路并发 | 各卡片内联+副题手点 | 切页重取 |

数据库页项目下拉、发布页项目 datalist 直接消费已加载 `state.projects`——独立加载动作归零[D]。

## 4. 状态机表（展项 7 态全集）

| 状态 | UI 表现 | 可执行动作 | 数据来源字段 |
|---|---|---|---|
| `running` 运行中 | 绿● 运行中 | 打开·更新·日志·停止·更多▾（回滚/环境变量/数据/删除） | `p.status==='running'` |
| `stopped` 已停止 | 黄● 已停止（tooltip=后端原词 exited/paused/created） | 启动·更新·日志·更多▾ | `p.status∈{exited,paused,created}` |
| `rebuilding` 重建中 | 黄● 重建中（呼吸动画；reduced-motion 时静止） | 日志（其余禁用） | `p.status==='restarting'`，或更新发布成功后 60s 滚动窗口[D] |
| `failed` 异常 | 红● 异常（tooltip 后端原词 dead） | 日志·更新（重新发布）·删除 | `p.status==='dead'`；或本地发布失败终态标记 |
| `waiting-image` 待部署 | 灰● 待部署（tooltip「仅镜像无容器：从发布页选择镜像即可拉起」） | 部署（→发布页预填项目名）·删除；日志/停止禁用 | `p.status==='missing'`（conf 在、容器无） |
| `uploading` 上传中 | 行内标记「上传中 x%」（dp-bar 为主显示） | 等待；失败后 dp-retry 断点续传 | 本地 `deploy.phase==='upload'` 作用于该 project 行[Z] |
| `publishing` 发布中 | 黄● 发布中…（行按钮禁用） | 等待 | 本地 publish 在途[D] |

规则：显示层只出上表 7 词；「未部署」「missing」等原文不进 UI[D]；状态标识=●+文字双通道（非仅颜色，色盲可辨）[M→Z 纳入验收]。

## 5. 参数单源与发布单表单（问题 B/C 收口）

- **发布表单=全站唯一参数输入处**（两案共识）：字段仅 5——`pub-project`[保留+换一个]、镜像源 radio（`pub-src-tar` 本地文件 / `pub-src-registry` 镜像库下拉 `dep-image`）、`pub-tag`、`pub-mem`、`pub-cport`。`dep-mem/dep-cport` 删除。
- 选本地 tar → 压缩+分片上传+publish（现一键部署）；选镜像库 → ①②标 skip 直接 publish（现第三入口）；「仅上传」保留为提交旁次要按钮（选 tar 时可用）[D 结构+M 单表单意图]。
- **预填算法**[Z]：`pub-project` 命中 `state.projects` 行 → `pub-mem=行.mem`（服务端 conf 权威，GET /_api/projects 已含）；`localStorage['sr.conf.'+project]` 补 `cport/tag`（响应无 cport 字段）；字段下注「沿用上次发布参数（MM-DD），可修改」（`#pub-pref-note`）；无记忆默认 256m/8080。publish 成功写 `sr.conf.*={tag,cport,ts}`[D]。换设备/清缓存回落默认，不影响正确性（publish 参数自足）。
- 展项行「更新」→ 跳发布页预填同一套[D]；选镜像库镜像仍走 onImagePick 自动带出项目/tag（保留现状逻辑）。

## 6. URL 展示策略（问题 A 收口）

- `externalUrl(p) = base + '/' + p + '/'`。base 解析顺序[D]：① 设置项 `sr.publicBase`（#set-base，如 `http://47.107.155.180:10100`）→ ② `location.origin` 为公网 host 则用之（判定：host ∉ {localhost, 127.*, ::1, 10.*, 172.16–31.*, 192.168.*}）→ ③ 兜底 `location.origin` + 列表上方一次性提示条「正在用内网地址展示 URL，可在设置中配置公网域名」。M 的「首登预填 location.origin」否决——LAN 首登会把内网地址写死进存储。
- 生效范围：展项列表 URL 主列、dp 完成大字 URL、复制按钮、start/rollback 成功提示——全站一律 `externalUrl()`[D]。
- 服务端 `p.url`（根因：`SHOWROOM_BASE_URL=host.base_url()="http://"+LAN_HOST`，middleware.rs:186）**永不作主链接**，仅 URL 单元格 tooltip「服务器登记地址：…」[D]。不改服务端。

## 7. 视觉 token 与规范（问题 E 收口）[D 全盘采纳]

- 间距刻度 4/8/12/16/24/32：卡片内边距 16、卡片距 16、字段行距 12、label-控件 4、节距 24。
- 字号：页面题 18/600；卡片题 15/600；正文/控件 14；label/辅助 13；注释/时间/代码/URL 12（mono+tabular-nums）；大字 URL 20/700 唯一例外。
- 对齐：表单 label 上控件下、grid 底对齐；表格文本左、时间/数字右；行内操作固定顺序行尾。
- 色彩语义：绿=可用、黄=过渡/停止、红=故障/破坏性、灰=不存在；破坏性按钮 `.danger`+confirm。
- 层级封顶：顶栏→标签→卡片 三级；卡片=标题+状态副题（已加载 N 项 · HH:MM:SS，可点重取）+内容；卡片内禁嵌 fieldset。

## 8. 分步实施序列（给执行代理）

| 步 | 内容 | 验证 | 预估行数 |
|---|---|---|---|
| S1 | 助手层：`externalUrl()`/publicBase 解析、TTL store、登录 4 路 allSettled；#dlg-settings（hd-pempub 迁入+set-base+改密/忘记设备聚合）；serve.rs whoami 加 platform_pem（读 platform_rsa.pub）+SPEC §3 行 | `node showroom-web/smoke-wizard.mjs`（新增 whoami pem 断言）；grep header 无 hd-pempub | JS+DOM ~160 / rs ~6 |
| S2 | 标签重组：五标签激活表、tab-status 拆除（st-logs→抽屉、事件流→管理页）、rd-* 迁数据页；smoke §tab 用例改 | smoke 全绿；grep `tab-status`=0 | ~80 |
| S3 | 发布单表单：镜像源 radio、dep-form 删除并入、预填（projects.mem+sr.conf）、pub-pref-note；smoke §12 改写 | grep「内存需求」label=1、`dep-form`=0 | ~170 |
| S4 | 展项页：行操作序+更多▾菜单+行抽屉（日志/env 表/数据深链/上一版 tag+回滚）、事件流可见性钩子、st-auto 移除 | J3 断言：打开→见实时日志 ≤2 点击 | ~220 |
| S5 | 状态机换表：STATUS 7 态渲染+tooltip 后端原词+本地在途行标记 | grep「未部署」=0；7 词表断言 | ~60 |
| S6 | 自动加载+按钮归零：§3 表接线、副题「已加载 N · HH:MM:SS」可点、btn-db-fill/btn-refresh 等删除 | grep「载入项目清单」「刷新镜像清单」=0 | ~110 |
| S7 | 视觉 token：CSS 变量化间距/字号/对齐；●+文字双通道；focus-visible/reduced-motion 复核 | smoke 静态断言扩充 | ~90（CSS） |
| S8 | 文档同步：SPEC §17 增补（§9.1/§13.1 控制台侧描述由本节取代，CLI 三命令不变）、README 控制台截图文案、smoke 全量回归 | `node smoke.mjs && node smoke-wizard.mjs` 全绿 | ~70 |

总改动 ≈ 960 行（含 smoke 与文档）。S1–S8 严格顺序提交，每步独立可回退。

## 9. 验收清单（用户视角断言）

1. 钥匙自动登录后 3 秒内零点击：展项列表完整可见（中文状态/可点对外 URL/更新时间），无「未加载」占位。
2. 设置公网域名后，全站主链接与复制内容不含 `127.0.0.1`/`192.168.*`；内网地址仅存于 tooltip。
3. DOM 检索「载入项目清单」「刷新镜像清单」为零；数据库页项目下拉、发布页镜像下拉零点击即含全量数据。
4. 「内存需求」「监听端口」label 全站各恰好出现 1 次；`dep-form` 不存在。
5. 状态文案集合={运行中,已停止,重建中,异常,待部署,上传中,发布中}；「未部署」零出现；「待部署」tooltip 说明镜像在库可拉起。
6. 默认任何页面不见 PEM；未配置且 whoami 未带回 PEM 时选 tar 发布 → 内联引导「去设置」→ 对话框打开且焦点在 PEM 字段；whoami 带回后永不需手工粘贴。
7. 某项目上次以 mem=512m/cport=9000 发布成功后，点该行「更新」→ 发布页项目/内存/端口预填一致并注「沿用上次发布参数」。
8. 断网打开：各卡片独立内联「加载失败+重试」，3 秒自动重试一次；恢复后免刷新页面自动补齐。
9. 顶栏单行 ≤56px：账号、角色徽标、+发布 CTA、设置、退出；全站无卡片内嵌 fieldset。
10. J1 首发（自动登录起，除选文件/填表外）≤4 次点击拿到完整 URL；J3 从打开页面到见实时日志 ≤2 次点击。
11. 展项页停留期间在别处 stop 容器：≤5 秒行状态自动变「已停止」（事件流+2s 去抖，无手动刷新）。
12. 任意两卡片标题字号/内边距一致；间距全落 4/8/12/16/24/32 刻度；时间列右对齐且等宽数字。
13. 状态标识非仅颜色（●+文字），单色打印仍可区分七态。
14. 删除展项 confirm 文案含「数据库将一并删除，如需保留请先在数据页备份下载 .dump」[M 诚实版]。
15. 非管理员（whoami admin=false）DOM 无管理入口残留；管理标签渲染/拆除随权限即时。
16. 登录成功焦点落于「+ 发布」CTA（首用户）或首行「打开」（已有展项）；空账号空态卡引导「+ 发布新 Demo」。

## 10. 不破坏约束

- **id 兼容**：§1 对照表为唯一增删清单；表外 id 一律不动；smoke-wizard.mjs 同 PR 同步。
- **aria/规范**：tablist/tab/tabpanel+aria-selected+方向键；focus-visible 不移除；reduced-motion；状态双通道；表单 label 关联；confirm 全破坏性操作。
- **零框架/零构建/零外部资源**：原生 JS+wasm 垫片原样复用；不改签名/上传/下载/续传/OPAQUE 逻辑，仅搬 DOM 与调用时机。
- **零信息暴露**：登录前仅 #login；管理壳仍走 template 惰性克隆；whoami 加字段不新增未签名端点。
- **SPEC 修订点**（随 S1/S8 落）：§3 whoami 行加 `platform_pem`；新增 §17 取代 §9.1「四区」与 §13.1 控制台侧「三入口并存」的 UI 描述（§13.1 的 API `/_api/images` 与 CLI 三命令不变）。
