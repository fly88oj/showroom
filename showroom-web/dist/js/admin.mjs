// —— admin.mjs：管理页五面板（账号/全部项目/平台状态/宿主配置/实时事件）+ 发卡补发 + §15 主机编排 ——
// adminBuilt/adTimer 为本模块私有可变旗标：shell（登出收尾/权限收回）经下方访问器读写。
import { $, esc, fmtTime, fmtGB, statusCn, statusCls, showMsg, fieldErr, clearErrs, busy, copyText, randomPassword } from './util.mjs';
import { NAME_RE } from './state.mjs';
import { api } from './api.mjs';
import { apiFail } from './img.mjs';
// ================ §12.2 管理员标签（whoami admin=true 才渲染；否则 DOM 不存在） ================
export let adminBuilt = false, adTimer = null;
let naGenPw = ''; // 发卡对话框已生成未发放的登录初始密码（发放成功即清空，防旧密码误发二次）
let adminPanel = 'users'; // E1：当前管理子面板（账号|全部项目|平台状态|宿主配置|实时事件）

// 拆分适配（跨模块可变旗标访问器；行为不变）：
export const setAdminBuilt = (v) => { adminBuilt = v; };
export const stopAdTimer = () => { clearInterval(adTimer); adTimer = null; };

export function buildAdminTab() {
  const btn = document.createElement('button');
  btn.dataset.tab = 'admin';
  btn.textContent = '管理';
  btn.setAttribute?.('role', 'tab');
  btn.setAttribute?.('id', 'tabbtn-admin');
  btn.setAttribute?.('aria-selected', 'false');
  btn.setAttribute?.('aria-controls', 'tab-admin');
  $('tabs').append(btn);                                    // 第五标签
  $('app').append($('admin-tpl').content.cloneNode(true));   // 管理区 DOM 仅此刻进入文档流
  wireAdminTab();
  activateAdminPanel('users'); // E1：默认「账号」面板
}
export function teardownAdmin() {
  clearInterval(adTimer); adTimer = null;
  adminPanel = 'users'; // 权限收回：下次重建回落默认面板
  for (const b of $('tabs').querySelectorAll('button[data-tab="admin"]')) b.remove();
  $('admin-root')?.remove?.();
}

// —— E1：管理页二级导航——五面板互斥显示；E4：独立进入时按需刷新（沿用现有拉取函数） ——
export function activateAdminPanel(name) {
  if (name) adminPanel = name;
  const panels = ['users', 'projects', 'status', 'host', 'events'];
  for (const p of panels) {
    const el = $(`adp-${p}`);
    if (el) el.classList.toggle('hidden', p !== adminPanel);
    const b = $(`adpbtn-${p}`);
    if (b) {
      b.classList.toggle('on', p === adminPanel);
      b.setAttribute?.('aria-selected', p === adminPanel ? 'true' : 'false');
    }
  }
  // 面板可见即取数（30s 定时与写操作失效另行兜底）
  if (adminPanel === 'users') loadAdminUsers();
  else if (adminPanel === 'projects') loadAdminProjects();
  else if (adminPanel === 'status') refreshAdminStatus();
  else if (adminPanel === 'host') refreshHost();
  // events：事件流订阅由项目页可见性/进页逻辑驱动（st-events 仅作展示），无需拉取
}

function wireAdminTab() {
  // 根因清理：#ad-refresh/#ad-st-refresh/#hs-refresh 手动刷新按钮已随自动刷新删除（30s 定时取代）；
  // R4 用户裁决：自动刷新恒开无开关，进管理页即启动状态面板定时器（平台状态 + 主机期望状态）
  if (!adTimer) adTimer = setInterval(() => { refreshAdminStatus(); refreshHost(); }, 30000);
  $('admin-subnav').addEventListener('click', (ev) => {
    const btn = ev.target.closest?.('button[data-adp]');
    if (btn) activateAdminPanel(btn.dataset.adp);
  });
  $('ad-user-add').addEventListener('click', () => { // 发卡对话框（打开即聚焦首字段；Esc/关闭焦点自动归还）
    $('ad-na-user').value = '';
    $('ad-na-admin').checked = false;
    naGenPw = randomPassword(); // 打开即客户端生成并暂存（不展示）；发放成功后在结果区一次性展示
    $('ad-na-result').classList.add('hidden');
    $('ad-na-login-pw').textContent = ''; $('ad-na-pw').textContent = '';
    clearErrs('ad-na-user');
    $('dlg-user-add').showModal();
    $('ad-na-user').focus();
  });
  $('ad-na-close').addEventListener('click', () => $('dlg-user-add').close());
  $('ad-na-submit').addEventListener('click', onUserAdd);
  $('ad-na-user').addEventListener('input', () => fieldErr('ad-na-user', ''));
  $('ad-na-copy-login').addEventListener('click', () => copyText($('ad-na-login-pw').textContent, '登录初始密码'));
  $('ad-na-copy').addEventListener('click', () => copyText($('ad-na-pw').textContent, '镜像库密码'));
  // 补发密码对话框（plan-16 §1.5）：只承载一次性密码展示，动作在账号表行内
  $('ad-ps-close').addEventListener('click', () => $('dlg-passwd-set').close());
  $('ad-ps-copy').addEventListener('click', () => copyText($('ad-ps-pw').textContent, '登录初始密码'));
  $('ad-users-tbl').addEventListener('click', onAdminKeyAction);
  $('ad-projects-tbl').addEventListener('click', onAdminProjectAction); // E3：跨用户项目行操作
  // §15 主机编排：期望状态随 30s 定时与进页自动刷新（手动刷新按钮已删）/ 两段式（预演→确认）/ Redis 扩容对话框
  $('hs-apply').addEventListener('click', onHostApply);
  $('hs-confirm').addEventListener('click', onHostConfirmRun);
  $('hs-pool-expand').addEventListener('click', openRedisX);
  $('hs-rx-close').addEventListener('click', () => $('dlg-redis-x').close());
  $('hs-rx-preview').addEventListener('click', onRedisXPreview);
  $('hs-rx-run').addEventListener('click', onRedisXRun);
  $('hs-rx-n').addEventListener('input', () => fieldErr('hs-rx-n', ''));
  // B4：预演后任一编辑控件 change → 预演清单即失效（收起 + 提示重新预演）
  for (const id of ['hs-mw-postgres', 'hs-mw-redis', 'hs-mw-registry', 'hs-p-disk', 'hs-p-mem', 'hs-p-factor']) {
    $(id).addEventListener('change', onHostEditedAfterPreview);
  }
}

/** B4：预演清单失效——编辑控件变更后收起 hs-review/确认钮并提示重新预演（未预演过则静默）。 */
function onHostEditedAfterPreview() {
  if ($('hs-review')?.classList?.contains('hidden') && $('hs-confirm')?.classList?.contains('hidden')) return;
  $('hs-review').classList.add('hidden');
  $('hs-confirm').classList.add('hidden');
  hostImpact = null; // 受影响项目缓存随预演一并失效
  $('hs-fb').textContent = '配置已修改，请重新预演。';
}

export async function loadAdminUsers() { // 账号表（账号行 + 设备行；D1：管理员为账号级——账号行徽标+设/免按钮；设备行无级）
  const tb = $('ad-users-tbl').querySelector('tbody');
  tb.innerHTML = '<tr><td colspan="3" class="note">加载中…</td></tr>';
  try {
    const j = await api('GET', '/_api/admin/users');
    const users = j.users || [];
    tb.innerHTML = users.length ? users.map((o) => {
      const keys = o.keys || [];
      // plan-16 §1.5 has_password 徽标：未设密码 → 灰徽标 + 行内「补发密码」（passwd-set）；已设 → 静态徽标
      const pwCell = o.has_password
        ? '<span class="badge">已设密码</span>'
        : `<span class="badge">未设密码</span> <button class="act" data-act="pwset" data-user="${esc(o.user)}">补发密码</button>`;
      // D1：管理员徽标 + 设/免按钮上收到账号行（POST /_api/admin/user-admin；设备行无级）
      const adminCell = o.admin
        ? `<span class="status-ok">管理员</span> <button class="act" data-act="userunadmin" data-user="${esc(o.user)}">取消管理员</button>`
        : `<button class="act" data-act="useradmin" data-user="${esc(o.user)}">设为管理员</button>`;
      return `<tr class="ad-user-row"><td colspan="3">
          <span class="mono">${esc(o.user)}</span>
          <span class="note">· 项目数 ${esc(String(o.projects ?? '—'))} · 设备 ${keys.length} 台</span>
          ${pwCell}
          ${adminCell}
          <button class="act danger" data-act="revoke" data-user="${esc(o.user)}" style="margin-left:.4rem">吊销整个账号</button>
        </td></tr>` + (keys.map((k) => `<tr>
          <td class="mono wrap">${esc(k.kid)}</td>
          <td class="mono">${esc(fmtTime(k.since))}</td>
          <td><button class="act danger" data-act="keyrm" data-user="${esc(o.user)}" data-kid="${esc(k.kid)}">移除此设备</button></td>
        </tr>`).join('') || '<tr><td colspan="3" class="note">（该账号暂无设备）</td></tr>');
    }).join('') : '<tr><td colspan="3" class="note">暂无账号</td></tr>';
    $('ad-fb').textContent = ''; // 成功不写计数回执（表格即数据）；本区保留操作反馈用途
  } catch (e) { tb.innerHTML = '<tr><td colspan="3" class="note">加载失败 · <button type="button" class="act" data-act="retry-users">重试</button></td></tr>'; apiFail('账号加载', e); } // D2：失败行内「重试」可点（非管理员 404 E_AUTH 也走顶部条）
}

// —— E3：全部项目面板（跨用户项目表；GET /_api/admin/projects 每行附 owner）——
export async function loadAdminProjects() {
  const tb = $('ad-projects-tbl').querySelector('tbody');
  tb.innerHTML = '<tr><td colspan="5" class="note">加载中…</td></tr>';
  try {
    const j = await api('GET', '/_api/admin/projects');
    const rows = j.projects || [];
    tb.innerHTML = rows.length ? rows.map((p) => {
      const q = esc(p.project), u = esc(p.user || '—');
      const st = String(p.status || '');
      // 行操作按状态复用项目页语义（服务端 user_check：管理员可操作任意账号的项目）
      const op = st === 'running'
        ? `<button class="act danger" data-act="adstop" data-p="${q}" data-owner="${u}">停止</button>`
        : st === 'missing' ? '' : `<button class="act" data-act="adstart" data-p="${q}" data-owner="${u}">启动</button>`;
      return `<tr>
        <td class="mono">${q}</td>
        <td class="mono">${u}</td>
        <td class="${statusCls(st)}"><span>●</span> ${esc(statusCn(st))}</td>
        <td class="mono num">${esc(fmtTime(p.updated))}</td>
        <td>${op}<button class="act danger" data-act="adrm" data-p="${q}" data-owner="${u}">删除</button></td>
      </tr>`;
    }).join('') : '<tr><td colspan="5" class="note">（暂无项目）</td></tr>';
    $('ad-proj-fb').textContent = '';
  } catch (e) {
    tb.innerHTML = '<tr><td colspan="5" class="note">加载失败 · <button type="button" class="act" data-act="retry-adprojects">重试</button></td></tr>';
    apiFail('全部项目加载', e);
  }
}

const AD_PROJ_ACT = { adstart: 'start', adstop: 'stop', adrm: 'rm' };
const AD_PROJ_CN = { adstart: '启动', adstop: '停止', adrm: '删除' };
/** E3：跨用户项目行操作——与项目页同一端点（/_api/{start,stop,rm}，服务端按 owner 校验管理员放行）。 */
async function onAdminProjectAction(ev) {
  const btn = ev.target.closest('button[data-act]');
  if (!btn || btn.disabled) return;
  const { act, p: project, owner } = btn.dataset;
  if (act === 'retry-adprojects') { btn.disabled = true; return loadAdminProjects(); }
  const ep = AD_PROJ_ACT[act];
  if (!ep) return;
  if (act === 'adstop' && !confirm(`确认停止账号 ${owner} 的项目 ${project}？容器即刻停止、对外不可访问（数据与凭据保留，可随时重新「启动」）。`)) return;
  if (act === 'adrm' && !confirm(`确认删除账号 ${owner} 的项目 ${project}？容器与路由即刻下线，数据库将一并删除；删除后凭据一并清除，不可恢复。`)) return;
  busy(btn, true, `${AD_PROJ_CN[act]}中…`);
  try {
    await api('POST', `/_api/${ep}`, { project });
    $('ad-proj-fb').textContent = `已${AD_PROJ_CN[act]} ${project}（${owner}）`;
    loadAdminProjects(); // 写操作后重取
  } catch (e) { apiFail(`${AD_PROJ_CN[act]} ${project}`, e); }
  finally { busy(btn, false); }
}

// 管理动作：全部中文 confirm 写明影响（§12.2；F2 设备词族——不出现钥匙/绑定/密钥）
const ADMIN_ACT = {
  useradmin: { ep: 'user-admin', cn: '设为管理员', body: (o) => ({ user: o, admin: true }),
    ask: (o) => `确认将账号 ${o} 设为管理员？该账号登录即具管理权限（可管理全部账号、项目与平台状态）。` },
  userunadmin: { ep: 'user-admin', cn: '取消管理员', body: (o) => ({ user: o, admin: false }),
    ask: (o) => `确认取消账号 ${o} 的管理员权限？该账号所有设备同时失去管理权限。` },
  keyrm: { ep: 'key-rm', cn: '移除此设备', body: (o, k) => ({ user: o, kid: k }),
    ask: (o, k) => `确认移除账号 ${o} 的此设备（${k}）？该设备立即无法使用，重新登录即可重新登记。` },
  revoke: { ep: 'user-revoke', cn: '吊销整个账号', body: (o) => ({ user: o }),
    ask: (o) => `确认吊销整个账号 ${o}？其名下全部设备与镜像库访问凭据将一并删除，该账号即刻无法登录。` },
};

async function onAdminKeyAction(ev) {
  const btn = ev.target.closest('button[data-act]');
  if (!btn || btn.disabled) return;
  const { act, user, kid } = btn.dataset;
  if (act === 'pwset') return onAdminPasswdSet(user, btn); // plan-16 §1.5 补发：单独流（confirm + 一次性密码展示）
  if (act === 'retry-users') { btn.disabled = true; return loadAdminUsers(); } // D2：加载失败行内重试
  const def = ADMIN_ACT[act];
  if (!def) return;
  if (!confirm(def.ask(user, kid))) return;
  btn.disabled = true;
  try {
    await api('POST', `/_api/admin/${def.ep}`, def.body(user, kid));
    $('ad-fb').textContent = `已执行「${def.cn}」${kid ? `（${user} · ${kid}）` : `（${user}）`}`;
    loadAdminUsers();
  } catch (e) { apiFail(def.cn, e); }
  btn.disabled = false;
}

/** 补发/重置登录密码（plan-16 §1.5）：confirm → passwd-set {user,password}（客户端 crypto 生成）→
 *  新密码一次性展示（对话框）；旧密码即刻失效，其他设备不受影响 → 表刷新（徽标翻转）。 */
async function onAdminPasswdSet(user, btn) {
  if (!confirm(`确认为账号 ${user} 补发密码？将生成新的登录初始密码，原密码即刻失效；其他设备不受影响。`)) return;
  const pw = randomPassword();
  busy(btn, true, '补发中…'); // B3：请求期间按钮就地「补发中…」并禁用
  try {
    const j = await api('POST', '/_api/admin/passwd-set', { user, password: pw });
    $('ad-ps-user').textContent = user;
    $('ad-ps-pw').textContent = j.password || pw; // 服务端一次性回显；异常缺失回落本地生成值
    $('dlg-passwd-set').showModal();
    $('ad-fb').textContent = `已补发密码（${user}）`;
    loadAdminUsers();
  } catch (e) { apiFail(`补发密码（${user}）`, e); }
  finally { busy(btn, false); }
}

/** 发卡提交（B7 统一账号密码认证）：user-add {user,password[,admin]} → 两行一次性密码结果（登录初始 / registry）+ 表刷新。 */
async function issueUser(body) {
  const j = await api('POST', '/_api/admin/user-add', body);
  $('ad-na-result').classList.remove('hidden');
  $('ad-na-login-row').classList.toggle('hidden', !j.login_password);
  $('ad-na-login-pw').textContent = j.login_password || '';
  $('ad-na-pw').textContent = j.registry_password || j.password || j.cred || '（响应未携带密码，请联系主机管理员）';
  $('ad-na-fb').textContent = `已发放账号 ${body.user}${body.admin ? '（含管理员）' : ''}`;
  loadAdminUsers();
}
async function onUserAdd() { // 主路径（plan-16 §1.5 + B7）：账号 + 打开对话框时自动生成的初始密码 → user-add {user,password}
  const user = $('ad-na-user').value.trim();
  if (!NAME_RE.test(user)) return fieldErr('ad-na-user', `账号名「${user}」不合法（须匹配 ^[a-z0-9][a-z0-9-]{0,30}$），请修改后重试`);
  const body = { user, password: naGenPw };
  if ($('ad-na-admin').checked) body.admin = true; // 「同时授予管理员」勾选才携带
  busy($('ad-na-submit'), true, '发放中…');
  try {
    await issueUser(body);
    naGenPw = randomPassword(); // 本轮密码已在结果区一次性展示；就地补生成，同一对话框连续发放不复用旧密码
  } catch (e) { apiFail('发放新账号', e); }
  busy($('ad-na-submit'), false);
}

const SVC_CN = { active: '运行中', inactive: '未运行', failed: '失败', activating: '启动中', deactivating: '停止中' };
const svcCn = (s) => SVC_CN[s] || s || '未知';
export async function refreshAdminStatus() { // 平台状态面板：服务状态灯 / 磁盘内存 / 容器 / timer 下次触发
  const el = $('ad-status');
  el.textContent = '加载中…';
  try {
    const j = await api('GET', '/_api/admin/status');
    const svcs = Object.entries(j.services || {});
    const disks = Object.entries(j.disk ?? j.disks ?? {}).map(([p, d]) =>
      `<tr><td class="mono">${esc(p)}</td><td class="mono">${esc(typeof d === 'object' && d ? `${fmtGB(d.used)} 已用 / 共 ${fmtGB(d.total)}` : String(d))}</td></tr>`).join('');
    const mem = j.mem?.available_mb ?? j.mem_available_mb;
    const ctrs = (j.containers || []).map((c) => (typeof c === 'string' ? { name: c, status: '' } : c));
    const timers = (j.timers || []).map((t) => (typeof t === 'string' ? { timer: t } : t));
    el.innerHTML = `
      <h3>服务状态（systemd is-active）</h3>
      <div class="row">${svcs.length ? svcs.map(([n, s]) =>
        `<span class="row" style="gap:.3rem;margin-right:1.2rem"><span class="${s === 'active' ? 'status-ok' : 'status-bad'}">●</span><span class="mono">${esc(n)}</span><span class="note">${esc(svcCn(s))}</span></span>`).join('')
        : '<span class="note">（无数据）</span>'}</div>
      <h3>资源</h3>
      <div class="tbl-wrap"><table><tbody>
        <tr><td>内存可用（MemAvailable）</td><td class="mono">${esc(mem != null ? `${mem} MB` : '—')}</td></tr>
        ${disks || '<tr><td class="note">（无磁盘数据）</td></tr>'}
      </tbody></table></div>
      <h3>容器</h3>
      <div class="tbl-wrap"><table>
        <thead><tr><th>名称</th><th>状态</th></tr></thead><tbody>${ctrs.length ? ctrs.map((c) =>
          `<tr><td class="mono">${esc(c.name ?? '')}</td><td class="${c.status === 'running' ? 'status-ok' : 'status-mid'}">${esc(statusCn(c.status))}</td></tr>`).join('')
          : '<tr><td colspan="2" class="note">（无容器）</td></tr>'}</tbody></table></div>
      <h3>定时器（下次触发）</h3>
      <div class="tbl-wrap"><table>
        <thead><tr><th>timer</th><th>下次触发</th></tr></thead><tbody>${timers.length ? timers.map((t) =>
          `<tr><td class="mono">${esc(t.timer ?? t.name ?? '')}</td><td class="mono">${esc(t.next ?? t.next_elapse ?? '—')}</td></tr>`).join('')
          : '<tr><td colspan="2" class="note">（无定时器）</td></tr>'}</tbody></table></div>`;
  } catch (e) { el.textContent = ''; apiFail('平台状态加载', e); } // 自动刷新：成功不播报（面板即状态），失败走顶部条
}

// ================ §15 主机编排（期望状态 / 两段式变更 / Redis 扩容流；仅管理员） ================
const MW_NAMES = ['postgres', 'redis', 'registry'];
// setup::Action kind → 人读动作名（未知 kind 原样回显，前向兼容）
const KIND_CN = {
  CreateDir: '建目录', WriteFile: '写文件', BtrfsSubvol: 'btrfs 子卷', NetworkCreate: '建网络',
  ContainerUp: '起容器', ContainerRm: '删容器', PackageInstall: '装软件包',
  UnitInstall: '装单元', UnitEnable: '启用单元', UnitDisableRm: '停用单元', Cmd: '执行命令',
};
const kindCn = (k) => KIND_CN[k] || k || '动作';
let hostState = null, hostImpact = null; // 最近一次 GET /_api/admin/host 与预演 impact
/** 拉取期望状态并渲染（进入管理页 / 30s 定时 / 执行后收敛核对共用）。
 *  F3：成功全程静默（面板即状态，不写 hs-fb 回执；失败才说话，与平台状态面板同款）；
 *  F3：自动刷新不得收起预演/确认——清单失效唯一路径 = 编辑控件 change（B4 onHostEditedAfterPreview）。 */
export async function refreshHost() {
  try {
    const j = await api('GET', '/_api/admin/host');
    hostState = j;
    // 预演在架时保留其 impact（确认执行的安全二次 confirm 依赖它；B4 编辑失效时会清空）
    const reviewOn = !$('hs-review')?.classList?.contains('hidden');
    if (!reviewOn) hostImpact = null;
    renderHostState(j, reviewOn);
  } catch (e) {
    // B5：失败态写进数据容器（键值表/漂移清单占位；30s 定时自动重试）
    const kvTb = $('hs-kv-tbl')?.querySelector?.('tbody');
    if (kvTb) kvTb.innerHTML = '<tr><td colspan="2" class="note">（加载失败，将自动重试）</td></tr>';
    $('hs-drift').innerHTML = '<p class="note m-0">（加载失败，将自动重试）</p>';
    $('hs-fb').textContent = '';
    apiFail('主机期望状态加载', e);
  }
}
/** 期望状态渲染：中间件开关勾选态、host.env 其余键通用键值（含 FRP，不特判）、参数当前值、漂移黄条、池两态。
 *  F3：keepEdits=true（预演在架时自动刷新）不回写编辑控件——控件此刻表达的是被预演的期望值，
 *  静默还原会让在架清单与确认执行体不一致；只更新只读展示（键值表/漂移/池）。 */
function renderHostState(j, keepEdits = false) {
  const he = j.host_env || {};
  const mw = new Set(Array.isArray(he.MIDDLEWARE) ? he.MIDDLEWARE : String(he.MIDDLEWARE ?? '').split(/\s+/).filter(Boolean));
  if (!keepEdits) { // 中间件集合：开关勾选态即启用态（「已启用」徽标已删——与勾选同源重复）
    for (const n of MW_NAMES) $(`hs-mw-${n}`).checked = mw.has(n);
  }
  const kv = Object.entries(he).filter(([k]) => k !== 'MIDDLEWARE');
  $('hs-kv-tbl').querySelector('tbody').innerHTML = kv.length ? kv.map(([k, v]) =>
    `<tr><td class="mono">${esc(k)}</td><td class="mono wrap">${esc(typeof v === 'object' && v !== null ? JSON.stringify(v) : String(v))}</td></tr>`).join('')
    : '<tr><td colspan="2" class="note">（无其他键）</td></tr>';
  if (!keepEdits) {
    const p = j.policy || {};
    $('hs-p-redisdbs').value = p.REDIS_DBS != null ? String(p.REDIS_DBS) : '';
    $('hs-p-disk').value = p.DISK_RESERVE_GB != null ? String(p.DISK_RESERVE_GB) : '';
    $('hs-p-mem').value = p.RESERVE_MEM_MB != null ? String(p.RESERVE_MEM_MB) : '';
    $('hs-p-factor').value = p.UPLOAD_SPACE_FACTOR != null ? String(p.UPLOAD_SPACE_FACTOR) : '';
  }
  const drift = j.drift || []; // 漂移清单：warn 黄条 / error 红条，附建议命令
  $('hs-drift').innerHTML = drift.length ? drift.map((d) => `<div class="warnbar${d.severity === 'error' ? ' err' : ''}">` +
    `${d.severity === 'error' ? '缺失/停摆' : '漂移'} · <span class="mono">${esc(d.item)}</span>：期望 <span class="mono">${esc(String(d.expected))}</span>，实际 <span class="mono">${esc(String(d.actual))}</span>` +
    `${d.advice ? ` —— 建议：<span class="mono">${esc(d.advice)}</span>` : ''}</div>`).join('')
    : '<p class="note" style="margin:0">（无漂移：期望状态与实际一致）</p>';
  const pool = j.redis_pool, total = Number(pool?.total) || 0;
  const expandOn = !!(pool && total > 0 && pool.suggest_expand);
  if (pool && total > 0 && !expandOn) {
    const used = Number(pool.used) || 0;
    const head = pool.headroom != null ? Number(pool.headroom) : Math.max(0, total - used);
    $('hs-pool-ok').innerHTML = `Redis 逻辑库池：已用 <span class="mono">${used}</span> / 共 <span class="mono">${total}</span>（余 <span class="mono">${head}</span>）。`;
  }
  $('hs-pool-warn').classList.toggle('hidden', !expandOn); // 池黄条两态：<20% 余量才提示扩容
  $('hs-pool-ok').classList.toggle('hidden', expandOn || !pool || total <= 0);
  $('hs-pool-expand').classList.toggle('hidden', !expandOn);
  if (expandOn) $('hs-pool-warn-txt').textContent =
    `Redis 逻辑库余量不足（剩 ${pool.headroom != null ? Number(pool.headroom) : Math.max(0, total - (Number(pool.used) || 0))}/${total}）：建议扩容`;
}

/** 组装 POST /_api/admin/host 提交体：host_env 全量回传（MIDDLEWARE 由开关合成空格集合，其余键含 FRP 原样透传）+ policy 全量回传（三个可编辑参数覆写；REDIS_DBS 只读不走此处）。 */
function hostPostBody(confirm) {
  const he = { ...(hostState?.host_env || {}) };
  delete he.FRP_TOKEN; // P1：GET 回来是打码值 <set>，回传会覆盖真实凭据——剔除不发
  he.MIDDLEWARE = MW_NAMES.filter((n) => $(`hs-mw-${n}`).checked).join(' ');
  // bool 类键：服务端 AdminHostEnvBody 期待字符串（CADDY=yes/no、TUNING=sysctl/none、DEDUP=weekly/none）
  // GET 回来的值可能已是 bool（服务端 to_json 序列化）——统一转回服务端期待的字符串
  const toYesNo = (v, y, n) => (v === true || v === y || v === 'yes' || v === 'true') ? y : n;
  he.CADDY = typeof he.CADDY === 'boolean' ? toYesNo(he.CADDY, 'yes', 'no') : String(he.CADDY ?? 'yes');
  he.TUNING = typeof he.TUNING === 'boolean' ? toYesNo(he.TUNING, 'sysctl', 'none') : String(he.TUNING ?? 'sysctl');
  he.DEDUP = typeof he.DEDUP === 'boolean' ? toYesNo(he.DEDUP, 'weekly', 'none') : String(he.DEDUP ?? 'weekly');
  const policy = { ...(hostState?.policy || {}) };
  for (const [id, key] of [['hs-p-disk', 'DISK_RESERVE_GB'], ['hs-p-mem', 'RESERVE_MEM_MB'], ['hs-p-factor', 'UPLOAD_SPACE_FACTOR']]) {
    const v = $(id).value.trim();
    if (v !== '' && Number.isFinite(Number(v))) policy[key] = Number(v); // 空值保留原值
  }
  return { host_env: he, policy, confirm };
}
/** 两段式第一步「查看变更」：confirm=false 写盘前预演，服务端返回 actions+impact（人读行）；不落盘。 */
async function onHostApply() {
  if (!hostState) return showMsg('期望状态尚未加载（自动加载失败或仍在进行），请稍后重试');
  busy($('hs-apply'), true, '预演中…');
  $('hs-fb').textContent = '正在生成变更清单（写盘前预演，未执行）…';
  try {
    const j = await api('POST', '/_api/admin/host', hostPostBody(false));
    hostImpact = j.impact || {};
    const actions = j.actions || [];
    const needed = actions.filter((a) => a.needed !== false);
    $('hs-review-sum').textContent = `预演：共 ${actions.length} 项动作，其中 ${needed.length} 项需执行；尚未写盘，点「确认执行」才会应用。`;
    $('hs-actions-tbl').querySelector('tbody').innerHTML = actions.length ? actions.map((a) =>
      `<tr${a.needed === false ? ' style="opacity:.55"' : ''}><td>${a.needed === false ? '＝' : '＋'} ${esc(kindCn(a.kind))}</td>` +
      `<td class="mono">${esc(a.target ?? '')}</td><td class="wrap">${esc(a.detail ?? '')}</td></tr>`).join('')
      : '<tr><td colspan="3" class="note">（无动作：期望状态已就绪）</td></tr>';
    const rm = hostImpact.removed_middleware || [], aff = hostImpact.affected_projects || [];
    $('hs-impact').innerHTML = (rm.length || aff.length) ? `
      ${rm.length ? `<p class="note" style="margin:.2rem 0 0">将禁用的中间件：<span class="mono">${rm.map(esc).join(' ')}</span>（后续发布不再注入对应变量组）</p>` : ''}
      ${aff.length ? `<div class="warnbar err" style="margin-top:.4rem">红色警示：此变更影响 ${aff.length} 个在用项目 —— ${aff.map((x) => `<span class="mono">${esc(x)}</span>`).join('、')}；这些项目的重启/回滚/新发布将失去该中间件支持，确认执行前需再次确认。</div>` : ''}`
      : '';
    $('hs-report').classList.add('hidden');
    $('hs-review').classList.remove('hidden');
    $('hs-confirm').classList.toggle('hidden', j.confirm_required === false);
    $('hs-fb').textContent = '已列出变更清单：请核对后点「确认执行」';
  } catch (e) { $('hs-fb').textContent = ''; apiFail('查看变更（预演）', e); }
  busy($('hs-apply'), false);
}
/** 两段式第二步「确认执行」：confirm=true 落盘并 apply；impact 含受影响项目时先经二次 confirm。 */
async function onHostConfirmRun() {
  if (!hostState) return;
  const aff = hostImpact?.affected_projects || [];
  if (aff.length && !confirm(`此变更将影响 ${aff.length} 个在用项目（${aff.join('、')}）：相关项目的重启/回滚/新发布将失去该中间件支持。确认继续执行？`)) return;
  busy($('hs-confirm'), true, '执行中…');
  try {
    const j = await api('POST', '/_api/admin/host', hostPostBody(true));
    await refreshHost(); // 重取期望状态：面板数字/漂移/池随之收敛
    const rep = j.report || {};
    const items = rep.actions || rep.items || [];
    const rows = items.map((a) => `<tr><td class="${a.ok === false ? 'status-bad' : 'status-ok'}">${a.ok === false ? '● 失败' : '● 成功'}</td>` +
      `<td>${esc(kindCn(a.kind))}</td><td class="mono">${esc(a.target ?? '')}</td><td class="wrap">${esc(a.result || a.detail || '')}</td></tr>`).join('');
    $('hs-report').innerHTML = `
      <h3 style="margin-top:.2rem">执行报告${j.ok === false ? '（有失败项）' : ''}</h3>
      <p class="note" style="margin:.2rem 0 0">完成 ${Number(rep.changed ?? items.length)} 项变更。</p>
      ${items.length ? `<div class="tbl-wrap" style="max-height:26vh"><table><thead><tr><th>结果</th><th>动作</th><th>目标</th><th>输出</th></tr></thead><tbody>${rows}</tbody></table></div>` : '<p class="note">（无动作）</p>'}
      ${(rep.warnings || []).map((w) => `<div class="warnbar">${esc(w)}</div>`).join('')}`;
    $('hs-report').classList.remove('hidden');
    $('hs-fb').textContent = j.ok === false ? '执行完成（含失败项，详见报告与警告）' : '执行完成'; // F7：面板 fb 单通道（顶部条不重复播报）
  } catch (e) { $('hs-fb').textContent = ''; apiFail('确认执行', e); }
  busy($('hs-confirm'), false);
}

// —— §15.3 Redis 扩容对话框：输入新值 → confirm=false 取服务端提示原文 → 「我已了解，执行扩容」→ 结果回报 ——
function openRedisX() {
  $('hs-rx-n').value = '';
  clearErrs('hs-rx-n');
  $('hs-rx-prompt').textContent = '';
  $('hs-rx-prompt').classList.add('hidden');
  $('hs-rx-run').classList.add('hidden');
  $('hs-rx-fb').textContent = '';
  $('dlg-redis-x').showModal();
  $('hs-rx-n').focus();
}
/** 读取并校验新 databases 值（16–256 整数；上界受库编号类型承载限制）；非法 → 字段内联错误并返回 null。 */
function redisXVal() {
  const n = Number($('hs-rx-n').value);
  if (!Number.isInteger(n) || n < 16 || n > 256) {
    fieldErr('hs-rx-n', 'databases 须为 16–256 的整数（上限受库编号类型承载限制，且需大于当前已分配的最大库编号），请修正后重试');
    return null;
  }
  return n;
}
async function onRedisXPreview() { // 不动容器：仅取回服务端确认提示原文
  const n = redisXVal();
  if (n == null) return;
  busy($('hs-rx-preview'), true, '获取中…');
  try {
    const j = await api('POST', '/_api/admin/host', { redis_databases: n, confirm: false });
    $('hs-rx-prompt').textContent = j.prompt || '（服务端未返回提示文案）';
    $('hs-rx-prompt').classList.remove('hidden');
    $('hs-rx-run').classList.remove('hidden');
    $('hs-rx-fb').textContent = '请阅读服务端提示原文后，再点「我已了解，执行扩容」。';
  } catch (e) { apiFail('获取扩容确认提示', e); }
  busy($('hs-rx-preview'), false);
}
async function onRedisXRun() { // confirm=true：停旧容器 → 新容器带 --databases N 起来 → 验证 → 回报
  const n = redisXVal();
  if (n == null) return;
  busy($('hs-rx-run'), true, '扩容中…');
  try {
    const j = await api('POST', '/_api/admin/host', { redis_databases: n, confirm: true });
    const secs = j.restart_secs != null ? `（实例重启约 ${j.restart_secs} 秒）` : '';
    $('hs-rx-fb').textContent = `扩容完成：databases → ${j.databases ?? n}${secs}，服务端已自动验证（PING + 库数）。`; // F7：面板 fb 单通道
    $('hs-rx-run').classList.add('hidden');
    refreshHost(); // 池数字与漂移黄条随之收敛（不阻塞对话框回报）
  } catch (e) { apiFail('执行扩容', e); }
  busy($('hs-rx-run'), false);
}
