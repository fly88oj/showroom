# Plan 16：认证重设计（密码为主 / 密钥无感）— 实施方案（SPEC §16）

> 状态：已实施。基准：HEAD 8386b68。目标体验：打开控制台 → 页面自动生成临时密钥（无感）→ 用户只输
> 账号+密码登录 → 之后一切签名操作完全无感；管理员发账号时设初始密码。CLI 与 Web 认证模型一致
> （密码一次、密钥常驻，gh/ssh 模型），公钥登记降级为机器对机器专家路径。
> **偏离声明**：任务书 §1 要求服务端存 argon2id `pass.hash`；§2 论证后选 A=OPAQUE，服务端改存注册
> 记录（信封）而非裸哈希——argon2id 保留为客户端 rwd 硬化（参数同 §1），文件名 `pass.opaque`，位置
> `state/users/<u>/` 不变。裸 pass.hash 仅在放弃 OPAQUE 的退化方案中才存在。

## 0. 结论一览
- 登录协议选 **A=OPAQUE**（RFC 9380，opaque-ke 3.x，Ristretto255 suite，common 单点定义双端共用）；
  B（非 PAKE 挑战+签名+绑定因子）**不安全，明说只有 A**；C（HKDF 对称凭证）否决（§2.1）。
- 登录密码与 registry 密码**两把秘密分开发放**（§1.3）；registry 侧 bcrypt 不动（registry:2 仅支持 bcrypt）。
- 控制台一步登录（账号+密码）；密钥生成/绑定/签名全无感；keygen 公钥流程保留但降为 CI 专家路径。

## 1. 账号密码存储与发放
1.1 **引依赖评估**：`argon2 = "0.5"`（RustCrypto，纯 Rust 无 C 工具链，可交叉 wasm32；熵走 common 既有
    custom getrandom 桥）。参数 m=19456KiB / t=2 / p=1（OWASP），盐=b"showroom-opaque-v1"+user（RFC9380
    允许常数盐：盲化已提供保护，硬化只为抬高离线代价）。`bcrypt="0.15"` 保留但仅用于 registry.htpasswd。
1.2 **状态布局**（SPEC §5 增补）：`state/opaque.key`（服务端 OPRF key，b64 0600，serve 启动缺失自动生成，
    同 platform_rsa 模式）；`state/users/<u>/pass.opaque`（注册记录 b64 0600；user_revoke 连 users 目录
    一并删除，记录随之消失）；`state/tokens/<u>/<kid>.bound`（空文件=密码绑定钥匙，Web/CLI 同类）；
    policy.env 可选键 `BOUND_KEYS_MAX`（默认 5）。
1.3 **两把密码的取舍（不合并）**：① registry 密码随 docker push 以 basic-auth **明文走线**（site A 是
    http），若与登录密码合并，被 PAKE 保护的密码等同走线密码，零暴露前提崩塌；② 格式锁定（bcrypt vs
    OPAQUE 记录）无法共享校验器；③ 轮换独立。→ user-add/admin API 响应**两个一次性返回**：
    `login_password` + `registry_password`，面板并列各显示一次。
1.4 **CLI（showroomctl）**：`user-add <u> [--pubkey <串>] [--admin] [--password <pw>|--gen-password]`
    （pubkey 与 password 至少其一，全无→E_NAME 指引）；设密码=进程内跑 OPAQUE 注册写记录（明文初始
    密码只过管理员终端与主机进程，不上网线），输出仅此一次。新增 `user-passwd <u> [--password|--gen-password]`
    （重置/补设：替换记录，不动既有钥匙；疑似盗号走 user-revoke/key-rm）。
1.5 **admin API**：user-add body 增 `password`|`gen_password:true` → 响应 `{kid?, login_password,
    registry_password}`；新增 `POST /_api/admin/passwd-set {user, password?|gen:true}` → `{password}`；
    user_list 行增 `has_password`（面板「未设密码」徽标→一键补发）。

## 2. 登录协议（明文 HTTP 链路上密码零暴露）
2.1 **选型**：
- **A OPAQUE**（选）：密码经盲化 OPRQ 永不上线；服务端无离线可验证哈希（只泄露记录不泄露 OPRF key 时
  无法试密码）；每次 KE 全新临时份额天然防重放。`opaque-ke 3.x`：Rust 实现 RFC9380 含测试向量，依赖仅
  curve25519-dalek/hkdf/sha2/rand_core 0.6（与现栈兼容，wasm 熵桥复用）；**评估**：可用但维护团队小、
  无第三方审计 → 第 0 步 spike 门禁（编译 wasm32+与 native 往返），失败则本 plan 回炉。
- **B 挑战+临时密钥签名+密码绑定因子**（否决）：不用 PAKE 的「密码知识证明」只能是 H(pw,挑战) 类确定性
  值上网线；挑战在明文链路可见 → 被动窃听即可离线跑字典，主动 MITM 更可冒充服务器直接收集；且换设备
  凭密码重绑新钥匙要求服务端能验密码，而「密码只做本地加密种子」则任何人可给任意账号绑钥匙。**结论：
  B 无法做到安全，安全方案只有 A。**
- **C HKDF(pw) 当对称凭证**（否决）：密码等价物明文上线可重放（bearer），防重放与零暴露双失败；服务端
  可见派生值=免费离线字典材料。
2.2 **报文序列**（完整；C=浏览器或 CLI，均走 wasm/native 同一份 common 客户端）：
```
# 首次登录（新设备 / 本地钥匙失效）：
C→S POST /_api/auth/login-begin  {"user":u,"ke1":b64}          # 未签名；ke1=blind(argon2id(pw))+临时份额
S→C 200 {"ke2":b64}         # 未知用户/未设密码 → crate 原生 UnknownUser 假记录路径，仍 200（防枚举）
                             # 密码错误：客户端本地 finish 即失败，不产生第三请求（本地提示，无线上差异）
C→S POST /_api/auth/login-finish {"user":u,"ke3":b64}          # ke3=客户端 MAC；验证过 → 发一次性 bind_token
S→C 200 {"bind_token":hex}  # 60s 有效、单次使用（nonce 同表语义，内存）
C→S POST /_api/auth/key-bind {"user":u,"pubkey":串}
     Authorization: SR-SIG1 …  # 用候选钥匙签名、服务端按 body 内公钥验签（而非已登记钥匙）
     X-Sr-Bind: <bind_token>
S→C 200 {"kid","admin"}     # 写 tokens/<u>/<kid>.pub + .bound；账号已有任一 admin 钥匙 → 新钥匙继承
                             # admin（密码即账号级最高凭证，控制台显式提示）；GC 旧 .bound 钥匙（按 mtime
                             # 保留最近 BOUND_KEYS_MAX 把）；幂等（同钥匙重绑=ok）
# 此后一切照旧 SR-SIG1（§1），用户无感。页面/CLI 启动若已有本地 seed：先签名 /_api/health 自测，
# 通过直达工作区（免密码）；E_AUTH（钥匙被删/GC）→ 回落密码表单重绑新钥匙=「找回/换设备」。
# 改密码（登录态，旧密码免输——已绑定钥匙即会话证明）：
C→S POST /_api/auth/pw-begin  {"user":u,"m1":b64}   [SR-SIG1]
S→C 200 {"m2":b64}                                  # 注册在途态内存保存，60s
C→S POST /_api/auth/pw-commit {"user":u,"record":b64} [SR-SIG1]  # 客户端用新密码完成注册，服务端原子替换记录
# 自助下线（CLI logout --purge / 控制台「忘记此设备」远端部分）：
C→S POST /_api/key-selfrm {"kid":k} [SR-SIG1]       # 仅允许删除请求签名所用 kid 自己（.pub/.bound/.admin 一并清）
# 登录后信任通道下发 registry 凭据（CLI 免手工 config）：
C→S GET  /_api/reg-cred [SR-SIG1] → {"user","registry_password"}  # 读 reg.cred；暴露面=已绑定钥匙=账号主人
```
2.3 **零信息与防滥用**：/_api/auth/* 是唯一应答未签名请求的端点族；畸形/限流/ke3 拒绝一律 404
    `{"error":"E_AUTH"}`（与未知路径逐字节一致，复用现有测法）；login-begin 按 user（10 次/10 分）+全局
    （120 次/10 分）双令牌桶（内存，超限同 404）；服务重启丢在途登录态，重试即可。

## 3. 浏览器侧（showroom-web/dist/index.html）
- wasm 就绪即：无 sr.seed → 自动生成密钥入 localStorage（登录前也无感生成）；有 seed → 签名 health
  自测，通过直接进控制台；失败回落密码表单。
- 登录卡一步：账号+密码+提交（OPAQUE→bind→enterApp）。失败文案：「密码错误，或该账号尚未设置密码
  （请联系管理员）」——客户端无法也不应区分两者（防枚举的一致代价）。
- 「钥匙登录（CLI/机器用户）」收进 <details> 折叠：保留旧 2/3 步（用本机已有/导入种子/公钥串交管理员+
  重检循环）；公钥串不再出现在主路径。
- localStorage：sr.seed/sr.user 保留（试验性存储告示保留）；退出登录不删 seed；新增「忘记此设备」
  （清 seed+user，可选联动 key-selfrm）。改密码入口在帮助页（登录态）。

## 4. CLI 侧（showroom，密码为主与 Web 同模型）
- **`showroom login [账号] [--server-lan URL] [--ephemeral]`**：rpassword 交互读密码（不回显、不进
  argv/env/ps；rpassword="7" 仅 libc/win 依赖，备选手写 termios）；本机生成临时 Ed25519 → 走 §2.2 同一
  协议 login→key-bind → 默认**持久化**：seed 落 key_path（0600 现格式）、config.toml 写 user/server_lan、
  并 GET /_api/reg-cred 把 registry 凭据写入 config（**评估结论：并入 login 下发**——服务端 reg.cred 本就
  明文常驻，下发对象是已验签的账号主人钥匙，暴露面等同今日管理员转交；走线属性与每次 docker push 的
  basic-auth 相同，无新暴露类别；登录密码仍永不上线）。此后全部子命令照旧签名，密码只此一次
  （gh/ssh 模型）。**取舍论证**：真正会话级临时钥匙=每次调用都要密码，cron/CI/脚本化 deploy 即死——
  违背自动化目标，故默认常驻、`--ephemeral` 供选择（不落任何盘，进程退出即弃，服务端靠 keep-N GC 收尾，
  每次使用需重新登录，仅适合一次性/共享机器）。自动化场景的另一正解=下条专家路径。
- **`showroom logout [--purge]`**：本地清 config/钥匙；--purge 追加 key-selfrm 远端注销。
- **keygen 降级**：clap `hide=true`（--help 不列），保留机器对机器/CI 用（管理员 user-add --pubkey 登记，
  无 .bound 标记=不受 GC，人工生命周期）；控制台帮助页与 README 保留说明。
- **doctor 适配**：无 config/seed 或自测 E_AUTH → 提示 `showroom login <账号>`；已登录则现状检查不变。
- `showroom config --registry-*` 降级为手工覆盖辅助（login 已自动写）。

## 5. 管理面板联动（web admin 区）
- 发账号对话框：账号 + 初始密码（默认自动生成，可自输）+ 管理员勾选 + 折叠高级（公钥串，可选）；成功
  返回两块一次性展示：登录密码 / registry 密码（各配复制按钮）。
- 账号表每行「重置密码」（confirm→自动生成→仅显示一次；不动既有钥匙，文案注明）；
  `has_password=false` 行显示「未设密码」徽标+一键补发。

## 6. 文件级改动清单（签名级）+ 测试 + 迁移 + 验收
6.1 **common**：`Cargo.toml` +opaque-ke="3" +argon2="0.5"；新 `src/opaque.rs`：`pub struct Suite;`
    （Ristretto255+Argon2id KSF）、`rwd(pw,user)->[u8;32]`、客户端包装 `reg_start/reg_finish/
    login_start/login_finish`（状态对象）；`wasm_shim.rs` +`sr_rwd/sr_login_start/sr_login_finish/
    sr_reg_start/sr_reg_finish`（会话 push/finish 模式、指针+长度 ABI）。
6.2 **showroomctl**：`Cargo.toml` +opaque-ke +argon2；`state.rs` +`opaque_key_path/pass_opaque_path/
    bound_marker_path/bound_keys_gc(policy BOUND_KEYS_MAX)`；`ops.rs`：`user_add(..., init_pw: InitPw
    {None,Set,Gen})`、+`user_passwd_set`（进程内注册/替换）、+`key_bind(root,user,pubkey)->kid`（幂等+
    .bound+admin 继承+GC）、user_list +has_password、user_revoke 不变（users 目录整删）；`serve.rs`：
    dispatch 在 auth() 之前分支 /_api/auth/*（§2.2 全序列+限流+bind_token 表）、+`GET /_api/reg-cred`、
    +`POST /_api/key-selfrm`、+`POST /_api/admin/passwd-set`、user-add arm 同步；`main.rs`：user-add
    --password/--gen-password（pubkey 可选）+ user-passwd 子命令。
6.3 **showroom（CLI）**：`Cargo.toml` +rpassword；新 `src/auth.rs`（login/logout/purge 流程）；`main.rs`
    +Login/Logout 子命令、keygen hide=true、doctor 未登录提示；`config.rs` 登录写入（user/server_lan/
    registry）。
6.4 **web**：登录区一步化+钥匙折叠（§3）；admin 对话框/重置/徽标（§5）；ERR_CN 与帮助页文案；旧三步
    向导主路径移除。**SPEC.md**：新增 §16（协议/报文/存储/继承语义/风险边界），改 §8.3（OPAQUE 已落）、
    §10.1（三步→一步）、§12.2（user-add/passwd-set）。
6.5 **测试清单**（cargo test）：common——OPAQUE 注册→登录往返、错密码客户端 finish 失败、rwd 固定向量
    （防 native/wasm 漂移）；ops——user_add(Gen) 落记录且密码仅返一次、user_passwd_set 替换后旧钥匙仍
    可用、key_bind 幂等/GC 保 5 弃旧/有 admin 钥则继承、user_revoke 连记录删、user_list has_password、
    key-selfrm 仅删自身；serve(tokio)——login-begin 未知用户 200 且与已注册用户响应不可区分、ke3 拒绝/
    畸形/限流→404 E_AUTH 逐字节一致（复用现有测法）、bind_token 单次、key-bind 验签必须按 body 公钥。
6.6 **迁移**：存量 users 无 pass.opaque → 密码登录不可用（走 UnknownUser 同形态），控制台文案指路联系
    管理员（passwd-set 补发）；已有 localStorage seed 的控制台用户零变化（自测直达）；CLI 存量钥匙用户
    零变化；面板 has_password 徽标引导补发。
6.7 **验收 checklist**：
- [ ] cargo test 全绿（6.5）；spike：opaque-ke 编译 wasm32 与 native 往返+熵桥先行通过
- [ ] 新用户：面板发账号（两密码各显示一次）→ 控制台账号+密码登录 → 全程无密钥感知 → 关闭重访免密码
- [ ] 第二台设备密码登录自动绑新钥匙；第 6 把旧 .bound 钥匙被 GC（保 5）；CI 无标记公钥不受 GC
- [ ] 错密码本地提示（无第三请求）；未知用户 login-begin=200 与已注册不可区分；畸形 auth 请求=404 E_AUTH
- [ ] CLI：login 一次（含 registry 凭据自动落 config）→ 后续 deploy/ps 免密码；logout --purge 远端注销
- [ ] 面板发卡/重置/补发正常；admin 账号密码登录后管理标签出现（继承）且有提示
- [ ] user-passwd 重置后旧密码失效新密码可用、既有钥匙不受影响

## 7. 风险与最险一点
1. **最险一点：明文 HTTP 首登无信任锚**——PAKE 只保证密码本身不泄露；主动 MITM 可中继 login-finish 并
   在 key-bind 登记自己的公钥，整体接管账号（且众人会因「有 PAKE」误判链路安全）。缓解：公网站点 B 经
   Caddy 启 TLS；LAN 不可信时用 CLI 钥匙路径；控制台/README 如实声明边界。
2. opaque-ke 成熟度：维护团队小、无第三方审计；wasm 交叉或熵桥失败会卡死控制台登录。缓解：spike 第 0
   步门禁+钥匙折叠兜底；两端同 crate 同 Suite 定义（无互操作缝隙）。
3. admin 继承语义变化（账号持 admin 钥→密码绑定钥匙继承 admin）：比「admin 挂钥匙」语义宽，但密码本已
   是账号级凭证（可随意绑钥匙），无实质提权；SPEC 明示。
4. 限流按 user 计（代理后取不到可信 IP）：跨用户分布式低速字典仍有残面 → 全局桶+rwd 计算成本抬高单价。
5. UnknownUser 假记录在响应长度/时序上有统计残差（枚举成本从一次请求升到时序分析）；记录在案。
