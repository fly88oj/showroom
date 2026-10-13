// —— help.ts：帮助页静态内容（CLI 对照/错误码/工具下载/注入环境变量清单）——
// 迁移自原版 dist/js 内嵌帮助文案；非 mock 数据。
export const CLI_TABLE: { web: string; cli: string; adminOnly?: boolean }[] = [
  { web: "账号密码登录（自动绑定本机）", cli: "showroom login <账号>" },
  { web: "退出登录", cli: "showroom logout" },
  { web: "修改密码（登录态自助）", cli: "控制台菜单「修改密码…」入口（CLI 暂无）" },
  { web: "生成/重置本机签名密钥（调试用）", cli: "showroom keygen" },
  { web: "发放新账号（初始密码 / 管理员）", cli: "showroomctl user-add <账号> --gen-password [--admin]", adminOnly: true },
  { web: "补发 / 重置密码（管理员）", cli: "showroomctl user-passwd <账号> --gen-password", adminOnly: true },
  { web: "项目列表", cli: "showroom ps" },
  { web: "发布（上传镜像 / 镜像库）/ 上传入库", cli: "showroom publish / push" },
  { web: "启动 / 停止 / 回滚 / 删除", cli: "showroom start / stop / rollback / rm" },
  { web: "日志 / 环境变量", cli: "showroom logs / showroom env" },
  { web: "SQL 执行 / 表清单 / 预览", cli: "showroom db sql / tables / preview" },
  { web: "备份下载 / 恢复上传 / 重置", cli: "showroom db dump / restore / reset" },
];

export const HELP_ENV_TABLE: { key: string; desc: string }[] = [
  { key: "SHOWROOM_BASE_PATH", desc: "子路径前缀（恒为 /<project>/），页面内链接据此适配" },
  { key: "SR_DB_URL", desc: "项目专属 PostgreSQL 连接串" },
  { key: "SR_REDIS_URL", desc: "项目专属 Redis 逻辑库" },
  { key: "SR_DB_USER / SR_DB_PASS", desc: "数据库凭据（自动注入）" },
];

export const TOOLS_TABLE: { file: string; desc: string }[] = [
  { file: "showroom", desc: "CLI 二进制（Linux x86_64）：下载后 chmod +x showroom 放入 PATH（如 ~/.local/bin）。" },
  { file: "showroom-publisher.md", desc: "Agent Skill（SKILL.md）：AI 助手的 Showroom 发布代理技能文件。" },
  { file: "platform_rsa.pub", desc: "平台公钥（PEM）：镜像上传与数据库恢复的加密信封公钥。" },
];

export const ERROR_TABLE: { code: string; desc: string }[] = [
  { code: "E_AUTH", desc: "登录凭据校验未通过（常见于本设备登录状态过期或被收回、更换设备）。重新用账号密码登录即可自动修复（CLI 同：showroom login 重登）；仍失败可用 showroom doctor 排查时钟偏差。" },
  { code: "E_NAME", desc: "命名不符合规则 ^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$ 或使用了保留名（v2/_api/_bootstrap/_console）。" },
  { code: "E_QUOTA", desc: "超出限额：内存配额 / 导入体积 / 磁盘空间（扣除预留）不足；请调低内存配置、精简镜像，磁盘不足时可在「镜像库」删除旧镜像后重试。" },
  { code: "E_NOTFOUND", desc: "项目或上传/下载会话不存在。" },
  { code: "E_STATE", desc: "当前状态不允许该操作（如危险 SQL 未确认、容器状态冲突）。" },
  { code: "E_NET", desc: "网络或服务不可用，稍后重试。" },
  { code: "502", desc: "项目服务异常（容器未就绪或已崩溃），请联系管理员。" },
];
