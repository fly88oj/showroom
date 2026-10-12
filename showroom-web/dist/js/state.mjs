// —— state.mjs：状态单例 + 全站常量（SEG/WRAP/NAME_RE/ENV_DESC…）+ TTL 数据缓存/兜底重试 + PEM/设置配置访问 ——
// platformPem/pemReady/openSettings 就近于 state 层：img/db（下层页签）与 shell 均消费，避免反向依赖。
import { $, fieldErr } from './util.mjs';
// —— 常量（与 showroom-common 信封常量一致，用于前端进度/总长核算）——
export const SEG = 4 * 1024 * 1024;      // AES-GCM 段明文 4MiB
export const WRAP = 384;                  // RSA-3072 wrap 头
export const NONCE = 12, TAG = 16;
export const CHUNK = 8 * 1024 * 1024;    // 上传分片粒度（§8.1）
export const DUMP_CHUNK = 4 * 1024 * 1024; // 备份下载分片粒度（§10.4，服务端上限 4MB）
export const NAME_RE = /^[a-z0-9][a-z0-9-]{0,30}$/;        // 账号（user）
export const PROJ_RE = /^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$/;  // 项目（§11 放宽：允许大写）
export const RESERVED = new Set(['v2', '_api', '_bootstrap', '_console']);

// §6 环境变量 → 中文用途（帮助页与环境变量模态共用）
export const ENV_DESC = {
  DB_HOST: 'PostgreSQL 数据库主机名（平台内置）',
  DB_PORT: 'PostgreSQL 端口',
  DB_NAME: '本项目数据库名',
  DB_USER: '数据库用户名',
  DB_PASSWORD: '数据库密码（发布时随机生成）',
  DATABASE_URL: '完整 PostgreSQL 连接串',
  REDIS_HOST: 'Redis 主机名（平台内置）',
  REDIS_PORT: 'Redis 端口',
  REDIS_DB: '分配的 Redis 逻辑库编号（每项目一库，由平台统一分配）',
  REDIS_URL: '完整 Redis 连接串',
  SHOWROOM_PROJECT: '项目名',
  SHOWROOM_BASE_PATH: '对外子路径前缀（恒为 /<项目名>/；镜像内应据此适配或使用相对 URL）',
};

// —— 数据 TTL 缓存（plan-17 §3）：内存+TTL 单源；写操作/事件强制失效（force 绕过） ——
const DATA_TTL = { projects: 30000, images: 300000, space: 60000 };
const dataAt = { projects: 0, images: 0, space: 0 };
export const dataFresh = (k) => Date.now() - dataAt[k] < DATA_TTL[k];
export const dataMark = (k) => { dataAt[k] = Date.now(); retryCounts[k] = 0; }; // 成功即复位兜底重试计数
export const dataInvalidate = (...ks) => ks.forEach((k) => { dataAt[k] = 0; });
// plan-17 §3/验收 8：失败 3 秒自动重试 1 次；此后每 10s 慢速兜底重试（≤7 次）直至恢复——
// 恢复联网/服务后免刷新自动补齐（online 钩子与事件流重连之外的第三重保障）
const retryCounts = {};
export function retryOnce(key, fn, delay = 3000) {
  const n = retryCounts[key] || 0;
  if (n >= 7) return;
  retryCounts[key] = n + 1;
  setTimeout(() => { if (state.loggedIn) fn(); }, n === 0 ? delay : 10000);
}

// —— 密钥状态（localStorage：sr.seed hex64 / sr.user / sr.pempub）——
export const state = { seed: null, pub: '', user: '', loggedIn: false, projects: [], kid: '', admin: false, boundAdmin: false };

// —— 发布（可选先走 §8.1 分片上传）——
export function checkProj(project) {
  if (!PROJ_RE.test(project) || RESERVED.has(project)) return `项目「${project}」不合法（须匹配 ^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$，保留名：${[...RESERVED].join(' ')}）`;
  return null;
}

export const platformPem = () => $('hd-pempub').value.trim();

/** PEM 就绪检查（plan-17 验收 6）：未配置 → 内联引导到「设置」完成安全配置（F12 统一文案）。
 *  whoami 已自动带回 platform_pem 时本检查恒过，永不需手工粘贴。 */
export function pemReady() {
  if (platformPem().startsWith('-----BEGIN PUBLIC KEY-----')) return true;
  $('pub-pem-hint')?.classList.remove('hidden');
  return false;
}

/** 打开设置对话框（plan-17 S1）：focusPem=true 时焦点落 PEM 字段（内联引导跳转）。 */
export function openSettings(focusPem = false) {
  $('set-base').value = localStorage.getItem('sr.publicBase') || '';
  fieldErr('set-base', '');
  $('pub-pem-hint')?.classList.add('hidden');
  $('dlg-settings').showModal();
  (focusPem ? $('hd-pempub') : $('set-base'))?.focus();
}
