# 安全策略（Security Policy）

## 支持版本

本项目处于 v1 定稿窗口，仅对仓库默认分支的最新版本提供安全修复。

## 威胁模型摘要

Showroom 面向的部署形态是：控制面流量可能经过**不可信的明文链路**（公网 frp 隧道、不启用 TLS 的反向代理）。因此安全不依赖传输层，而是全部实现在应用层：

- **认证 = SR-SIG1 签名**。`/_api/*` 全部端点（含 health）要求 Ed25519 请求签名：canonical 串覆盖 owner、时间戳、nonce、方法、路径与请求体 SHA-256，±300 秒窗口 + per-owner nonce 查重防重放。没有令牌、没有会话 cookie——私钥永远不离开发布者机器。
- **机密性 = 加密信封**。镜像导入与数据库恢复的上行、数据库备份的下行，都封装为「RSA-3072-OAEP-SHA256 包裹的一次性 AES-256-GCM 密钥 + 分段流加密」信封；下行（备份）用客户端一次性 RSA 公钥封装，平台侧无法解密自己的备份。签名覆盖密文哈希（Encrypt-then-MAC）。
- **授权 = 管理员钥匙分层**。发布者钥匙（owner key）只能操作归属自己的展项；管理员钥匙（admin key，`tokens/<owner>/<kid>.admin` 标记）才能发卡、吊销、查主机状态。管理员接口与普通接口一样返回 `404 E_AUTH`，与未登记不可区分。
- **零信息暴露**。未认证或验签失败的任何请求——无论路径是否存在——统一返回 `404 + {"error":"E_AUTH"}`；根路径与未匹配路径一律 404。攻击者无法用它枚举项目、版本或端点。公网控制台在登录成功前不加载任何管理资源。

完整规范（canonical 串格式、信封字节布局、威胁与缓解表）见 [docs/security.md](docs/security.md)。

## 漏洞报告

**请不要为安全漏洞开公开 Issue。**

请使用 GitHub 的私有漏洞报告（Private Vulnerability Reporting）：

1. 进入本仓库页面，选择 **Security** 标签；
2. 点击 **Report a vulnerability**；
3. 填写复现步骤与影响评估。

或使用 [GitHub 安全公告](https://docs.github.com/zh/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities) 提交。我们会在确认后尽快回复，修复发布前不公开细节。

### 报告内容建议

- 受影响组件（CLI / 控制面 / 控制台 / 部署脚本）与版本（`showroom --version`）；
- 复现步骤（可脱敏，请勿附真实密钥、内网地址）；
- 你认为的影响面（认证绕过 / 明文泄露 / 越权 / 拒绝服务）。

## 密钥泄露处置

### 发布者钥匙泄露

泄露的钥匙在过期前一直有效，必须吊销：

- **管理员动作**（控制台「管理」标签，或主机 CLI）：

  ```sh
  # 只删泄露的那把钥匙（kid 见 /_api/admin/owners 清单）
  showroomctl owner-key-rm <OWNER> <KID>
  # 或经签名 API：POST /_api/admin/key-rm {"owner":"<OWNER>","kid":"<KID>"}

  # 整个账号立即失能（SR-SIG1 与 registry 登录全失效，已发布展项不动）
  showroomctl owner-revoke <OWNER>
  # 或经签名 API：POST /_api/admin/owner-revoke {"owner":"<OWNER>"}
  ```

- **发布者动作**：`showroom keygen --force` 生成新钥匙，把新公钥串交管理员重新登记（`showroomctl owner-add <OWNER> --pubkey <公钥串>` 或控制台发卡对话框）。

### 管理员钥匙泄露

管理员钥匙 = 平台最高权限（可发卡、可吊销他人、可看主机状态），视同平台沦陷：

1. 立即用另一把管理员钥匙吊销泄露钥匙（`owner-key-rm` / `key-rm`，方法同上）；
2. 若**唯一**管理员钥匙泄露，只能在主机上执行 `showroomctl owner-key-rm <OWNER> <KID>`；
3. 评估该钥匙有效期内管理面被调用的痕迹（journalctl 中 `E_AUTH`/admin 相关记录）；
4. 视情况轮换平台 RSA 对：吊销后删除 `state/platform_rsa{,.pub}` 并 `showroomctl init-keys` 重新生成——**这会使所有发布者本地的 `platform_rsa.pub` 失效**，需重新分发（仅影响 import 加密通道，不影响签名认证）。

### 平台 RSA 私钥泄露

平台私钥（`/srv/showroom/state/platform_rsa`）用于解密上行信封。泄露意味着历史与未来的上行信封在攻击者同时拿到密文的前提下可被解密。处置：轮换（同上第 4 步）并评估 `_tmp` 残留。数据库备份是客户端公钥封装的端到端加密，不受平台私钥轮换影响。

## 安全边界（不在承诺范围内）

- 展项页面 `/{project}/*` 是**公开**的——发布者不应把机密放进 Demo 展项本身；
- 展项容器之间通过共享 docker 网络可达 `sr-postgres` / `sr-redis`（各自凭据隔离，但非网络隔离）；
- 控制台登录密钥存于浏览器 localStorage（P1 试验档，页面有明确提示），共享浏览器不适合存放管理员钥匙。
