// —— util.mjs：纯工具层（$ / esc / fmtTime / busy / 随机名 / hex·b64·PEM 编解码 / showMsg / errText…）——
// 拆分自 dist/index.html 单文件（零逻辑改动；errText 的 ERR_CN 常量随其唯一使用方就近放置）。
export const $ = (id) => document.getElementById(id);
export const enc = new TextEncoder(), dec = new TextDecoder();

// —— 消息条（错误统一顶部：「错误码：中文解释（含下一步动作）」）与行内反馈 ——
const ERR_CN = {
  E_AUTH: '登录凭据校验未通过（常见于钥匙过期/被回收、更换设备，或操作了非本人项目）：重新用账号密码登录即可自动修复（CLI 同：showroom login 重登）',
  E_NAME: '命名不符合规则或使用了保留名；请按提示修正名称后重试',
  E_QUOTA: '超出平台限额（内存配额/导入体积/磁盘空间不足）；请调低内存配置、精简镜像，磁盘不足时可先删除旧镜像',
  E_NOTFOUND: '对象不存在（展项/会话）；请确认名称或重新发起',
  E_STATE: '当前状态不允许该操作；请检查前置条件（如确认标记、容器状态）后重试',
  E_NET: '网络或服务不可用；请稍后重试，或用 showroom doctor 排查',
};
export function showMsg(text, kind = 'err') {
  const el = $('msg');
  el.textContent = text;
  el.className = kind;
  if (kind === 'ok') setTimeout(() => { if (el.textContent === text) el.className = ''; }, 8000);
}
export const errText = (e) => {
  if (e && e.error) {
    const cn = ERR_CN[e.error] || '未知错误';
    return `错误码 ${e.error}：${cn}${e.msg ? `（${e.msg}）` : ''}`;
  }
  return String(e?.message || e);
};

// —— 界面规范助手：字段级内联错误（显示于字段旁并聚焦首错）与按钮「处理中…」 ——
export function fieldErr(id, msg) {
  const err = $(`${id}-err`);
  if (err) { err.textContent = msg || ''; if (msg && err.classList?.contains('hidden')) err.classList.remove('hidden'); }
  if (msg) { const el = $(id); el.focus?.(); }
}
export const clearErrs = (...ids) => ids.forEach((id) => fieldErr(id, ''));
/** 提交按钮请求中禁用 + 「处理中…」；完成后恢复原文字。busy(btn, true[, 文案]) / busy(btn, false)。 */
export function busy(btn, on, txt = '处理中…') {
  if (!btn) return;
  if (on) { btn.dataset.label = btn.textContent; btn.textContent = txt; btn.disabled = true; }
  else { if (btn.dataset.label !== undefined) btn.textContent = btn.dataset.label; btn.disabled = false; }
}

export const fmtTime = (unixSec) => {
  if (!unixSec) return '\u2014';
  try {
    // D1：展项表「更新」列带年份（YYYY/MM/DD HH:MM），跨年项目不再歧义
    return new Intl.DateTimeFormat('zh-CN', { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' }).format(new Date(unixSec * 1000));
  } catch { return '\u2014'; } // 非法输入（服务端形状为 unix 秒）防御：不因单行时间戳中断整表渲染
};

export const fmtGB = (n) => { // admin/status 磁盘字节（df -B1）→ 人读；非数字原样回显（前向兼容字符串单位）
  const v = Number(n);
  return Number.isFinite(v) ? `${(v / 1024 ** 3).toFixed(1)} GB` : String(n ?? '—');
};

export const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const STATUS_CN = { running: '运行中', exited: '已退出', paused: '已暂停', restarting: '重启中', created: '已创建', dead: '异常终止' }; // 管理页容器列表用（missing 不进 UI）
export const statusCn = (s) => STATUS_CN[s] || s || '未知';
export const statusCls = (s) => s === 'running' ? 'status-ok' : 'status-mid';

/** 复制到剪贴板（优先 Clipboard API，降级 execCommand；均失败提示手动复制）。 */
export async function copyText(text, what) {
  try {
    if (navigator.clipboard?.writeText) { await navigator.clipboard.writeText(text); return showMsg(`${what}已复制。`, 'ok'); }
  } catch { /* 降级到 execCommand */ }
  const ta = document.createElement('textarea');
  ta.value = text; document.body.appendChild(ta); ta.select();
  try { document.execCommand('copy'); showMsg(`${what}已复制。`, 'ok'); }
  catch { showMsg(`复制失败，请手动选中${what}复制。`); }
  ta.remove();
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export const fmtToolSize = (n) => {
  if (!Number.isFinite(n) || n < 0) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 ** 2).toFixed(1)} MB`;
};

export function Buffer_fromHex(hex) {
  const b = new Uint8Array(hex.length / 2);
  for (let i = 0; i < b.length; i++) b[i] = parseInt(hex.substr(i * 2, 2), 16);
  return b;
}
export function toHex(bytes) {
  let s = '';
  for (const b of bytes) s += b.toString(16).padStart(2, '0');
  return s;
}

export function nonceHex() {
  const b = new Uint8Array(16); crypto.getRandomValues(b);
  return toHex(b);
}

// —— §11 随机项目名：crypto.getRandomValues 拒绝采样生成 8 位 [a-zA-Z0-9]（降低字典遍历发现门槛） ——
export function randomProject() {
  const A = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
  let s = '';
  while (s.length < 8) {
    const b = new Uint8Array(32);
    crypto.getRandomValues(b);
    for (const v of b) { if (v < 248 && s.length < 8) s += A[v % 62]; } // 248=62×4：截断去模偏
  }
  return s;
}
// —— plan-16 §1.4 发卡/补发用强密码：crypto.getRandomValues 拒绝采样生成 16 位 [a-zA-Z0-9]
//    （62^16≈2^95 熵；纯字母数字便于人工抄录转交，与 CLI 侧 24 hex 同一强度档位） ——
export function randomPassword() {
  const A = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
  let s = '';
  while (s.length < 16) {
    const b = new Uint8Array(32);
    crypto.getRandomValues(b);
    for (const v of b) { if (v < 248 && s.length < 16) s += A[v % 62]; } // 248=62×4：截断去模偏
  }
  return s;
}

export function bytesToB64(bytes) {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}
/** DER → PEM（WebCrypto 只出 DER，需自行包 PEM 头尾）。 */
export function derToPem(der, label) {
  const b64 = bytesToB64(der).replace(/(.{64})/g, '$1\n').trim();
  return `-----BEGIN ${label}-----\n${b64}\n-----END ${label}-----\n`;
}
