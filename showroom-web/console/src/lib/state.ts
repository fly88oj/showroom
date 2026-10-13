// —— state.ts：状态单例 + 全站常量 + TTL 数据缓存（移植自 dist/js/state.mjs，去 DOM 化）——
// 与 .mjs 版逐字对齐（除 $() DOM 引用）；PEM 事实源仍在本层。
export const SEG = 4 * 1024 * 1024;        // AES-GCM 段明文 4MiB
export const WRAP = 384;                    // RSA-3072 wrap 头
export const NONCE = 12, TAG = 16;
export const CHUNK = 8 * 1024 * 1024;      // 上传分片粒度（§8.1）
export const DUMP_CHUNK = 4 * 1024 * 1024; // 备份下载分片粒度（§10.4，服务端上限 4MB）
export const NAME_RE = /^[a-z0-9][a-z0-9-]{0,30}$/;        // 账号（user）
export const PROJ_RE = /^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$/;  // 项目（§11 放宽：允许大写）
export const RESERVED = new Set(["v2", "_api", "_bootstrap", "_console"]);

// §6 环境变量 → 中文用途（帮助页与环境变量弹窗共用）
export const ENV_DESC: Record<string, string> = {
  DB_HOST: "PostgreSQL 数据库主机名（平台内置）",
  DB_PORT: "PostgreSQL 端口",
  DB_NAME: "本项目数据库名",
  DB_USER: "本项目数据库用户名",
  DB_PASSWORD: "本项目数据库密码（发布时随机生成）",
  DATABASE_URL: "完整 PostgreSQL 连接串",
  REDIS_HOST: "Redis 主机名（平台内置）",
  REDIS_PORT: "Redis 端口",
  REDIS_DB: "分配的 Redis 逻辑库编号（每项目一库，由平台统一分配）",
  REDIS_URL: "完整 Redis 连接串",
  SHOWROOM_PROJECT: "项目名",
  SHOWROOM_BASE_PATH: "对外子路径前缀（恒为 /<项目名>/；镜像内应据此适配或使用相对 URL）",
};

// —— 数据 TTL 缓存（plan-17 §3）：内存+TTL 单源；写操作/事件强制失效（force 绕过）——
const DATA_TTL: Record<string, number> = { projects: 30000, images: 300000, space: 60000 };
const dataAt: Record<string, number> = { projects: 0, images: 0, space: 0 };
export const dataFresh = (k: string) => Date.now() - dataAt[k] < DATA_TTL[k];
export const dataMark = (k: string) => {
  dataAt[k] = Date.now();
  retryCounts[k] = 0;
};
export const dataInvalidate = (...ks: string[]) => ks.forEach((k) => { dataAt[k] = 0; });

// 失败 3 秒自动重试 1 次；此后每 10s 慢速兜底重试（≤7 次）直至恢复
const retryCounts: Record<string, number> = {};
export function retryOnce(key: string, fn: () => void, delay = 3000) {
  const n = retryCounts[key] || 0;
  if (n >= 7) return;
  retryCounts[key] = n + 1;
  setTimeout(() => {
    if (state.loggedIn) fn();
  }, n === 0 ? delay : 10000);
}

// —— 密钥状态（localStorage：sr.seed hex64 / sr.user / sr.pempub）——
export const state = {
  seed: null as string | null,
  pub: "",
  user: "",
  loggedIn: false,
  kid: "",
  admin: false,
  boundAdmin: false,
  pem: typeof localStorage !== "undefined" ? localStorage.getItem("sr.pempub") || "" : "",
};
// 注：项目列表事实源在 React 层（app.tsx），单例只承载会话事实（seed/user/pem/admin）。

/** 水合 PEM（写 state.pem 并缓存 sr.pempub）；守卫（不覆盖已有配置）由调用方保持。 */
export function hydratePem(pem: string) {
  state.pem = pem;
  localStorage.setItem("sr.pempub", pem);
}

export function checkProj(project: string): string | null {
  if (!PROJ_RE.test(project) || RESERVED.has(project)) {
    return project === ""
      ? "请先填写项目名"
      : "项目名仅限字母、数字与连字符（且不得使用系统保留名），请修改后重试";
  }
  return null;
}

export const platformPem = () => state.pem.trim();

/** PEM 就绪检查（plan-17 验收 6）：未配置 → 由调用方渲染内联引导到「设置」。 */
export function pemReady(): boolean {
  return platformPem().startsWith("-----BEGIN PUBLIC KEY-----");
}
