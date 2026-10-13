// —— mock.ts：Showroom 控制台 shadcn Demo 的全部假数据（形状对齐 _api 真实响应）——

export type ProjStatus =
  | "running"
  | "exited"
  | "restarting"
  | "dead"
  | "missing";

export interface Project {
  project: string;
  status: ProjStatus;
  version: number;
  url: string;
  updated: string; // 人类可读
  updatedTs: number;
  mem: number;
  cport: number;
}

export interface ProjVersion {
  version: number;
  image: string;
  time: string;
  current?: boolean;
  gone?: boolean; // 镜像已删除 → 灰置不可选
}

export interface ImageItem {
  name: string; // alice/demo-shop:v7
  size: string;
  ts: number;
}

export interface DeviceRow {
  user: string;
  device: string;
  loginAt: string;
  kid: string;
}

export interface AdminProject {
  project: string;
  owner: string;
  status: ProjStatus | "publishing";
  updated: string;
}

export const DEMO_USERS = {
  alice: { name: "alice", admin: false },
  boss: { name: "boss", admin: true },
} as const;

export const PROJECTS: Project[] = [
  { project: "demo-shop", status: "running", version: 7, url: "/demo-shop/", updated: "2 小时前", updatedTs: Date.now() - 2 * 3600e3, mem: 512, cport: 8080 },
  { project: "k8s-lab", status: "restarting", version: 12, url: "/k8s-lab/", updated: "26 分钟前", updatedTs: Date.now() - 26 * 60e3, mem: 1024, cport: 8080 },
  { project: "sales-bi", status: "exited", version: 3, url: "/sales-bi/", updated: "昨天 18:42", updatedTs: Date.now() - 26 * 3600e3, mem: 256, cport: 3000 },
  { project: "hello-world", status: "dead", version: 2, url: "/hello-world/", updated: "3 天前", updatedTs: Date.now() - 72 * 3600e3, mem: 256, cport: 8080 },
  { project: "legacy-crm", status: "missing", version: 0, url: "/legacy-crm/", updated: "上周", updatedTs: Date.now() - 7 * 86400e3, mem: 256, cport: 8080 },
];

export const VERSIONS: Record<string, ProjVersion[]> = {
  "demo-shop": [
    { version: 7, image: "sr.local/alice/demo-shop:v7", time: "2026-10-13 16:20", current: true },
    { version: 6, image: "sr.local/alice/demo-shop:v6", time: "2026-10-11 10:03" },
    { version: 5, image: "sr.local/alice/demo-shop:v5", time: "2026-10-08 19:47" },
    { version: 4, image: "sr.local/alice/demo-shop:v4", time: "2026-09-30 09:12", gone: true },
  ],
  "k8s-lab": [
    { version: 12, image: "sr.local/alice/k8s-lab:v12", time: "2026-10-13 17:55", current: true },
    { version: 11, image: "sr.local/alice/k8s-lab:v11", time: "2026-10-12 21:30" },
  ],
  "sales-bi": [
    { version: 3, image: "sr.local/alice/sales-bi:v3", time: "2026-10-12 18:42", current: true },
    { version: 2, image: "sr.local/alice/sales-bi:v2", time: "2026-10-05 14:20" },
    { version: 1, image: "sr.local/alice/sales-bi:v1", time: "2026-09-28 11:00" },
  ],
  "hello-world": [
    { version: 2, image: "sr.local/alice/hello-world:v2", time: "2026-10-10 09:31", current: true },
    { version: 1, image: "sr.local/alice/hello-world:v1", time: "2026-10-09 15:44" },
  ],
};

export const ENV_VARS: { key: string; value: string; desc: string }[] = [
  { key: "SHOWROOM_BASE_PATH", value: "/demo-shop", desc: "子路径前缀：项目恒以 /<project>/ 对外发布，页面内链接需按此适配" },
  { key: "SR_DB_URL", value: "postgres://sr_demo_shop:***@sr-postgres:5432/sr_demo_shop", desc: "项目专属 PostgreSQL 库连接串（凭据自动注入）" },
  { key: "SR_REDIS_URL", value: "redis://sr-redis:6379/7", desc: "项目专属 Redis 逻辑库" },
  { key: "SR_DB_USER", value: "sr_demo_shop", desc: "数据库用户名" },
  { key: "SR_DB_PASS", value: "（已注入，此处不回显）", desc: "数据库密码" },
];

export const IMAGES: ImageItem[] = [
  { name: "sr.local/alice/demo-shop:v7", size: "412 MB", ts: Date.now() - 2 * 3600e3 },
  { name: "sr.local/alice/demo-shop:v6", size: "398 MB", ts: Date.now() - 46 * 3600e3 },
  { name: "sr.local/alice/k8s-lab:v12", size: "1.2 GB", ts: Date.now() - 26 * 60e3 },
  { name: "sr.local/alice/sales-bi:v3", size: "256 MB", ts: Date.now() - 26 * 3600e3 },
];

// —— 数据页（预览分页用：表行数做实到可翻页量级） ——
const pad = (n: number) => String(n).padStart(2, "0");
function ts(day: number, h: number, m: number) {
  return `2026-09-${pad(day)} ${pad(h)}:${pad(m)}:${pad((day * 7 + m) % 60)}`;
}
function genUsers(n: number): string[][] {
  const surnames = ["张", "李", "王", "赵", "钱", "孙", "周", "吴", "郑", "冯"];
  return Array.from({ length: n }, (_, i) => {
    const id = i + 1;
    return [String(id), `${surnames[i % 10]}${["三", "四", "五", "六", "七"][i % 5]}${id}`, `user${id}@demo.cn`, ts(1 + (i % 28), 8 + (i % 12), i % 60)];
  });
}
function genOrders(n: number): string[][] {
  const statuses = ["paid", "pending", "shipped", "cancelled"];
  return Array.from({ length: n }, (_, i) => {
    const id = 1001 + i;
    const amount = 39 + ((i * 53) % 1900);
    return [String(id), String(1 + (i % 5)), `¥${amount.toLocaleString("en-US")}.00`, statuses[i % 4], ts(1 + (i % 28), 9 + (i % 10), (i * 3) % 60)];
  });
}
function genProducts(n: number): string[][] {
  const titles = ["机械键盘 87 键", "无线鼠标", "4K 显示器 27\"", "USB-C 扩展坞", "降噪耳机", "升降桌架"];
  return Array.from({ length: n }, (_, i) => [
    `SKU-${String(i + 1).padStart(4, "0")}`, `${titles[i % 6]} ${["标准版", "Pro", "Lite"][i % 3]}`,
    `¥${(99 + ((i * 137) % 2200)).toLocaleString("en-US")}`, String(10 + ((i * 29) % 400)),
  ]);
}
export const DB_TABLES: Record<string, { cols: string[]; rows: string[][] }> = {
  users: { cols: ["id", "name", "email", "created_at"], rows: genUsers(237) },
  orders: { cols: ["id", "user_id", "amount", "status", "created_at"], rows: genOrders(126) },
  products: { cols: ["sku", "title", "price", "stock"], rows: genProducts(28) },
};

export const REDIS_KEYS: { key: string; type: string; ttl: string; value: string }[] = [
  { key: "sr_demo_shop:session:9f2c1a", type: "string", ttl: "1800s", value: "{\"uid\":1,\"exp\":1760360000}" },
  { key: "sr_demo_shop:session:71bd0e", type: "string", ttl: "1800s", value: "{\"uid\":3,\"exp\":1760360120}" },
  { key: "sr_demo_shop:cart:uid:1", type: "hash", ttl: "-1", value: "{\"SKU-0001\":1,\"SKU-0002\":2}" },
  { key: "sr_demo_shop:ratelimit:login", type: "string", ttl: "42s", value: "3" },
  { key: "sr_demo_shop:cache:home", type: "string", ttl: "300s", value: "<!doctype html>…（4KB 截断）" },
];

// —— 管理端 ——
export const ADMIN_DEVICES: DeviceRow[] = [
  { user: "alice", device: "MacBook Pro · Chrome 141", loginAt: "2026-10-13 17:02", kid: "k-a1b2" },
  { user: "alice", device: "Arch Linux · showroom CLI", loginAt: "2026-10-12 09:40", kid: "k-c3d4" },
  { user: "bob", device: "Windows 11 · Edge", loginAt: "2026-10-11 15:22", kid: "k-e5f6" },
  { user: "boss", device: "MacBook Air · Safari", loginAt: "2026-10-13 10:11", kid: "k-g7h8" },
];

export const ADMIN_PROJECTS: AdminProject[] = [
  { project: "demo-shop", owner: "alice", status: "running", updated: "2 小时前" },
  { project: "k8s-lab", owner: "alice", status: "restarting", updated: "26 分钟前" },
  { project: "sales-bi", owner: "alice", status: "exited", updated: "昨天 18:42" },
  { project: "hello-world", owner: "alice", status: "dead", updated: "3 天前" },
  { project: "legacy-crm", owner: "alice", status: "missing", updated: "上周" },
  { project: "visitor-portal", owner: "bob", status: "running", updated: "5 小时前" },
  { project: "qa-signup", owner: "bob", status: "running", updated: "昨天 21:03" },
  { project: "docs-site", owner: "boss", status: "running", updated: "6 天前" },
];

export const HOST_KV: { key: string; value: string }[] = [
  { key: "LAN_HOST", value: "192.168.4.55:80" },
  { key: "WAN_FACE", value: "on" },
  { key: "WAN_PORT", value: "8080" },
  { key: "FRP_TUNNEL", value: "chmlfrp:tcp-bj-01" },
  { key: "REGISTRY_GC_DAYS", value: "14" },
  { key: "SNAPSHOT_HOUR", value: "*/15 * * * *" },
];

export const POLICY = { REDIS_DBS: 48, DISK_RESERVE_GB: 40, RESERVE_MEM_MB: 2048, UPLOAD_SPACE_FACTOR: 2.5 };

export const POOL = { used: 45, max: 48 };

export function buildPlatformStatus(): string {
  const d = new Date();
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  const avail = (860 + Math.floor(Math.random() * 6)).toFixed(0);
  const events = 35 + Math.floor(Math.random() * 8);
  return `平台版本   showroom v1.4.2（2026-10-12 构建）
中间件     sr-postgres 17.2 ✓   sr-redis 7.4 ✓   registry 2.8 ✓
项目容器   8 运行 / 1 停止 / 1 重建中   端口池 8100-8199（已用 11）
磁盘 /srv  总 1.8T · 快照 216G · 镜像 486G · 可用 ${avail}G（预留 40G）
自愈       reconcile 上轮 ${hm} ✓ 幂等   autoheal 上轮 ${hm} ✓ 无动作
事件流     已连接（最近 1 小时 ${events} 条）`;
}

// 事件流模板（demo：定时追加）
export const EVENT_POOL: { t: string; line: string }[] = [
  { t: "reconcile", line: '{"ev":"reconcile","project":"demo-shop","state":"in-sync","ms":412}' },
  { t: "autoheal", line: '{"ev":"autoheal","project":"k8s-lab","action":"restart","reason":"unhealthy(2/3)"}' },
  { t: "publish", line: '{"ev":"publish","user":"alice","project":"k8s-lab","v":12,"image":"sr.local/alice/k8s-lab:v12"}' },
  { t: "gc", line: '{"ev":"registry-gc","freed":"2.1GB","blobs":134}' },
  { t: "snapshot", line: '{"ev":"snapshot","kind":"hourly","size":"216G","ms":3811}' },
  { t: "reconcile", line: '{"ev":"reconcile","project":"qa-signup","state":"recreated","ms":1903}' },
];

export const LOG_POOL = [
  '[2026-10-13T17:58:01Z] listening on :8080 (base /k8s-lab/)',
  '[2026-10-13T17:58:02Z] GET /k8s-lab/api/cart 200 12ms',
  '[2026-10-13T17:58:04Z] GET /k8s-lab/api/cart 200 9ms',
  '[2026-10-13T17:58:07Z] healthcheck ok (1/1)',
  '[2026-10-13T17:58:09Z] POST /k8s-lab/api/checkout 500 87ms',
  '[2026-10-13T17:58:09Z] error: db ping timeout (context deadline exceeded)',
  '[2026-10-13T17:58:12Z] retried checkout → 200 46ms',
  '[2026-10-13T17:58:15Z] healthcheck ok (1/1)',
];

export const HELP_ENV_TABLE: { key: string; desc: string }[] = [
  { key: "SHOWROOM_BASE_PATH", desc: "子路径前缀（恒为 /<project>/），页面内链接据此适配" },
  { key: "SR_DB_URL", desc: "项目专属 PostgreSQL 连接串" },
  { key: "SR_REDIS_URL", desc: "项目专属 Redis 逻辑库" },
  { key: "SR_DB_USER / SR_DB_PASS", desc: "数据库凭据（自动注入）" },
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

export const CLI_TABLE: { web: string; cli: string; adminOnly?: boolean }[] = [
  { web: "账号密码登录（自动绑定本机）", cli: "showroom login <账号>" },
  { web: "退出登录", cli: "showroom logout" },
  { web: "修改密码（登录态自助）", cli: "控制台「设置 → 修改密码」入口（CLI 暂无）" },
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

export const TOOLS_TABLE: { file: string; desc: string }[] = [
  { file: "showroom", desc: "CLI 二进制（Linux x86_64）：下载后 chmod +x showroom 放入 PATH（如 ~/.local/bin）。" },
  { file: "showroom-publisher.md", desc: "Agent Skill（SKILL.md）：AI 助手的 Showroom 发布代理技能文件。" },
  { file: "platform_rsa.pub", desc: "平台公钥（PEM）：镜像上传与数据库恢复的加密信封公钥。" },
];

// —— 工具 ——
export function fmtClock(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export function randPw(len = 16): string {
  const cs = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
  let s = "";
  const buf = new Uint32Array(len);
  crypto.getRandomValues(buf);
  for (let i = 0; i < len; i++) s += cs[buf[i] % cs.length];
  return s;
}
