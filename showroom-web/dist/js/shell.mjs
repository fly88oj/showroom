// —— shell.mjs：管理壳（buildApp/wireTabs/顶栏与设置弹窗接线/修改密码）+ logout + whoami(syncAdmin) + 30s 自动刷新 ——
// 会话编排（enterApp/登录窗提交/心跳接线）见 app.mjs；adminBuilt/adTimer 写入经 admin.mjs 访问器。
import { $, esc, showMsg, fieldErr, clearErrs, busy, randomProject, fmtToolSize } from './util.mjs';
import { state, ENV_DESC, openSettings } from './state.mjs';
import { api, apiDo, respErr } from './api.mjs';
import { opaqueStart, opaqueFinish, opaqueEnd, opaqueRegStart, opaqueRegFinish, opaqueRegEnd, postAuthJson } from './auth.mjs';
import { ensureImages, imagesLoaded, apiFail, refreshSpace } from './img.mjs';
import { drawerRow, wireProjTab, activateTab, updatePubUrl, logsLive, logsRender, refreshProjects, renderProjects, closeDrawer, stopEventsStream } from './proj.mjs';
import { wireDbTab } from './db.mjs';
import { adminBuilt, setAdminBuilt, stopAdTimer, buildAdminTab, teardownAdmin, loadAdminUsers, refreshAdminStatus } from './admin.mjs';
import { evLive } from './proj.mjs'; // R7：事件流指示器（仅 admin 模板克隆后补写用）
// —— 帮助页·工具下载（/_api/tool/*，签名 GET → Blob → a.download）——
// 三件：CLI 二进制（Linux x86_64）/ Agent Skill（SKILL.md）/ 平台公钥（PEM）。
// 大小不预取（简化）：首次下载从 Content-Length / blob.size 回填大小列。

async function onToolDl(ev) {
  const btn = ev.target.closest('button[data-tool-dl]');
  if (!btn) return;
  const name = btn.dataset.toolDl;
  busy(btn, true, '下载中…');
  try {
    const resp = await apiDo('GET', `/_api/tool/${encodeURIComponent(name)}`, null);
    if (!resp.ok) throw await respErr(resp);
    const blob = await resp.blob();
    const sizeCell = document.getElementById(`tool-size-${name}`);
    if (sizeCell) sizeCell.textContent = fmtToolSize(blob.size); // 首次响应回填大小（不预取）
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = name;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 60000);
    showMsg(`已下载 ${name}（${fmtToolSize(blob.size)}）`, 'ok');
  } catch (e) {
    apiFail(`下载 ${name}`, e);
  } finally { busy(btn, false); }
}
function wireHelpTab() {
  document.getElementById('tool-tbl')?.addEventListener('click', onToolDl);
}

// —— 登录门 ⇄ 管理壳切换 ——
export let appBuilt = false, stTimer = null;

// ═══ 全局自动刷新（30s 周期）：按当前可见标签只刷新对应数据源，零手动操作 ═══
export let autoRefreshTimer = null;
export function startAutoRefresh() {
  clearInterval(autoRefreshTimer);
  autoRefreshTimer = setInterval(async () => {
    if (!state.loggedIn) return;
    const tab = document.querySelector('#tabs button[aria-selected="true"]')?.dataset?.tab;
    try {
      switch (tab) {
        case 'proj': await refreshProjects(true); break;
        case 'img': await ensureImages(true); await refreshSpace(); break;
        case 'db': await refreshSpace(); break;
        case 'admin': await loadAdminUsers(); await refreshAdminStatus(); break;
      }
    } catch (e) { /* 静默重试下轮 */ }
  }, 30000);
}

export function logout() {
  state.admin = false; state.boundAdmin = false; // P3：登出复位 admin 状态，防换账号短暂继承
  // 第2轮xiaozhi P3：顶部条消息随会话收尾清空（错误提示不跨会话驻留到下一次登录）
  $('msg').textContent = '';
  $('msg').className = '';
  // 密钥生命周期：登出=清本机密钥+远程吊销（不留僵尸钥匙）
  // key-selfrm fire-and-forget：不阻塞 UI，失败靠 TTL 自然过期兜底
  // （B5 修正：真实路由为 /_api/key-selfrm 且要求 kid=签名所用钥匙——原 /_api/auth/key-selfrm 为错误路径，一直静默失败）
  if (state.seed && state.user && state.kid) {
    api('POST', '/_api/key-selfrm', { kid: state.kid }).catch(() => {});
  }
  localStorage.removeItem('sr.seed');
  localStorage.removeItem('sr.user');
  localStorage.removeItem('sr.pempub');
  state.loggedIn = false;
  clearTimeout(whoamiTimer);
  if (adminBuilt) { // 管理页定时器与发卡/补发/扩容对话框随登录态收尾
    stopAdTimer();
    if ($('dlg-user-add').open) $('dlg-user-add').close();
    if ($('dlg-passwd-set')?.open) $('dlg-passwd-set').close(); // plan-16 §1.5 补发对话框同收尾
    if ($('dlg-redis-x')?.open) $('dlg-redis-x').close(); // §15 扩容对话框同收尾
  }
  if ($('dlg-pw')?.open) $('dlg-pw').close(); // 改密对话框随登录态收尾
  closeDrawer();                    // 行抽屉随登录态收起（内部停日志流）
  logsLive.lines = [];
  logsRender();
  stopEventsStream();               // 平台事件流同上
  $('app').classList.add('hidden');
  $('login').classList.remove('hidden');
  // B2 登出语义：上方已清本机钥匙（localStorage）并远程吊销（key-selfrm）——无密可免，
  // 登录卡只引导密码重登；提示如实说明钥匙已回收
  $('lg-pass').value = '';
  $('lg-fb').textContent = '已退出登录。请输入账号密码重新登录。';
}

// —— 修改密码（plan-16 §2.2 收口）：当前密码本地校验（复用 OPAQUE 登录序列 login-begin + 本地
//    finish，不调 login-finish 即不产生登录态）→ pw-begin {user,m1}（SR-SIG1）→ 本地 reg finish →
//    pw-commit {user,record}（SR-SIG1，服务端原子替换 pass.opaque，不动既有钥匙）→ 提示重新登录 ——
function openPwDlg() {
  $('pw-cur').value = ''; $('pw-new').value = ''; $('pw-new2').value = '';
  clearErrs('pw-cur', 'pw-new', 'pw-new2');
  $('pw-fb').textContent = '';
  $('dlg-pw').showModal();
  $('pw-cur').focus();
}
async function onPwChange() {
  clearErrs('pw-cur', 'pw-new', 'pw-new2');
  const cur = $('pw-cur').value, nw = $('pw-new').value, nw2 = $('pw-new2').value;
  if (!cur) return fieldErr('pw-cur', '请输入当前密码；忘记密码请联系管理员重置');
  if (!nw) return fieldErr('pw-new', '请输入新密码');
  if (nw !== nw2) return fieldErr('pw-new2', '两次输入的新密码不一致，请重新输入');
  if (nw === cur) return fieldErr('pw-new', '新密码与当前密码相同，请换一个');
  busy($('pw-submit'), true, '修改中…');
  try {
    // ① 当前密码校验：完整 login-begin + 本地 finish（错密码本地即败），不产生登录态
    const st = opaqueStart(state.user, cur);
    let ke2;
    try { ke2 = (await postAuthJson('/_api/auth/login-begin', { user: state.user, ke1: st.ke1 })).ke2; }
    catch (e) { opaqueEnd(st.sid); throw e; }
    try { opaqueFinish(st.sid, cur, ke2); }
    catch (e) { if (e && e.error === 'E_PW_LOCAL') throw { error: 'E_PW_CUR', msg: '' }; throw e; }
    finally { opaqueEnd(st.sid); }
    // ② pw 三件序列（服务端 §16：签名即会话证明，旧密码不再上线；60s 在途注册态一次性消费）
    const rs = opaqueRegStart(state.user, nw); // ②-1 m1
    let m2;
    try { m2 = (await api('POST', '/_api/auth/pw-begin', { user: state.user, m1: rs.m1 })).m2; }
    catch (e) { opaqueRegEnd(rs.sid); throw e; }
    let record;
    try { record = opaqueRegFinish(rs.sid, nw, m2); } // ②-2 本地 reg finish → record
    finally { opaqueRegEnd(rs.sid); }
    await api('POST', '/_api/auth/pw-commit', { user: state.user, record }); // ②-3 原子替换
    $('pw-cur').value = ''; $('pw-new').value = ''; $('pw-new2').value = ''; // 用后即清，不留在 DOM
    if ($('dlg-pw').open) $('dlg-pw').close();
    showMsg('密码修改成功：请使用新密码重新登录。', 'ok');
    logout(); // 收口：结束当前会话，引导新密码重登（登出即回收本机钥匙，B2 同语义）
    $('lg-fb').textContent = '密码已修改：请输入新密码重新登录。';
  } catch (e) {
    $('pw-fb').textContent = '';
    if (e && e.error === 'E_PW_CUR') fieldErr('pw-cur', '当前密码不正确（或该账号密码已被重置），请重输');
    else apiFail('修改密码', e);
  } finally { busy($('pw-submit'), false); }
}

export function buildApp() {
  // 视觉QA P0-2：建壳即清空顶部条（首帧不带旧会话错误横幅）
  $('msg').textContent = '';
  $('msg').className = '';
  $('app').append($('app-tpl').content.cloneNode(true));
  appBuilt = true; // A2：模板克隆完成立即置位——此后任何单点异常都不会触发换号/重进时的重复克隆

  $('hd-user').textContent = state.user;
  $('hd-pempub').value = localStorage.getItem('sr.pempub') || '';
  $('hd-pempub').addEventListener('input', () => localStorage.setItem('sr.pempub', $('hd-pempub').value));
  // 设置（plan-17 S1）：PEM 收纳 + 对外访问域名 + 改密/忘记设备聚合入口
  $('btn-settings').addEventListener('click', () => openSettings(false));
  $('dlg-settings-close').addEventListener('click', () => $('dlg-settings').close());
  $('set-base').addEventListener('input', onSetBaseInput);
  $('btn-go-settings').addEventListener('click', () => openSettings(true));
  $('btn-base-note-settings').addEventListener('click', () => openSettings(false));
  refreshAdminBadge(); // 顶栏账号徽标的管理员角标（密码登录继承的 admin 此刻已可知）
  $('btn-logout').addEventListener('click', logout);
  // 修改密码（plan-16 §2.2）：设置对话框聚合入口 + 对话框（密码框 autocomplete、字段内联错误、busy）
  $('btn-pwchange').addEventListener('click', openPwDlg);
  $('pw-close').addEventListener('click', () => $('dlg-pw').close());
  $('pw-submit').addEventListener('click', onPwChange);
  for (const id of ['pw-cur', 'pw-new', 'pw-new2']) $(id).addEventListener('input', () => fieldErr(id, ''));
  // 帮助页：环境变量清单表 + 工具下载（/_api/tool/*）
  $('help-env-tbl').querySelector('tbody').innerHTML = Object.entries(ENV_DESC)
    .map(([k, d]) => `<tr><td class="mono">${esc(k)}</td><td class="wrap">${esc(d)}</td></tr>`).join('');
  wireHelpTab();
  $('pub-project').value = randomProject(); // 页面加载（建壳）时预填一次随机名（留空提交时自动生成兜底，无需手动重随机入口）
  updatePubUrl();
  // 行抽屉（S2）：先挂入文档（hidden）再接线——getElementById 只寻址文档内元素
  $('proj-tbl').querySelector('tbody')?.append?.(drawerRow);
  $('drawer-close').addEventListener('click', () => closeDrawer());
  logsLive.want = true; // R4 用户裁决：抽屉展开即恒实时跟随（断线 5 秒自动重连），无开关 UI
  // A2：三个 wire* 各自 try/catch——单页绑定异常只降级对应页签（console.warn 记录页签名），不再雪崩全局
  for (const [name, fn] of [['wireTabs', wireTabs], ['wireProjTab', wireProjTab], ['wireDbTab', wireDbTab]]) {
    try { fn(); } catch (e) { console.warn(`[showroom] ${name} 事件绑定失败（对应页签功能可能不可用）`, e); }
  }
}

/** 对外访问域名（plan-17 §6）：合法 http(s) 地址即存 sr.publicBase（去尾斜杠）并重渲 URL 列；清空回退 origin。 */
function onSetBaseInput() {
  fieldErr('set-base', '');
  const v = $('set-base').value.trim().replace(/\/+$/, '');
  if (!v) { localStorage.removeItem('sr.publicBase'); renderProjects(); return; }
  try {
    const u = new URL(v);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') throw new Error('scheme');
    localStorage.setItem('sr.publicBase', v);
    renderProjects();
  } catch {
    fieldErr('set-base', '对外访问域名须为 http(s):// 开头的完整地址（如 http://106.55.224.229:10100），请修正后重试');
  }
}

/** 顶栏账号徽标的管理员角标（纯呈现）：boundAdmin=密码登录继承；admin=whoami 结论。
 *  F11：数据页 Redis 静态句的「管理员可见全部逻辑库」半句同钩子显隐（state.admin||boundAdmin 才拼入）。 */
function refreshAdminBadge() {
  const m = $('hd-admin');
  if (m) m.classList.toggle('hidden', !(state.admin || state.boundAdmin));
  const rc = $('rd-admin-clause');
  if (rc) rc.classList.toggle('hidden', !(state.admin || state.boundAdmin));
}

function wireTabs() {
  $('tabs').addEventListener('click', (ev) => {
    const btn = ev.target.closest('button[data-tab]');
    if (btn) activateTab(btn.dataset.tab);
  });
  // 方向键在标签间移动焦点并切换（tablist 键盘约定；首尾循环）
  $('tabs').addEventListener('keydown', (ev) => {
    if (ev.key !== 'ArrowRight' && ev.key !== 'ArrowLeft') return;
    const btns = $('tabs').querySelectorAll('button[data-tab]');
    if (!btns.length) return;
    const cur = ev.target.closest?.('button[data-tab]');
    const idx = Math.max(0, Array.from(btns).findIndex((b) => b.dataset.tab === cur?.dataset.tab));
    const next = (idx + (ev.key === 'ArrowRight' ? 1 : -1) + btns.length) % btns.length;
    ev.preventDefault();
    btns[next].focus?.();
    activateTab(btns[next].dataset.tab);
  });
}

let whoamiTimer = null;
export async function syncAdmin() {
  let isAdmin = false;
  try {
    const j = await api('GET', '/_api/whoami');
    state.kid = j.kid || '';
    // plan-17 S1：platform_pem 自动取+缓存（写现有 sr.pempub 键；不覆盖手工已配置值）
    if (j.platform_pem && !$('hd-pempub').value.trim()) {
      $('hd-pempub').value = j.platform_pem;
      localStorage.setItem('sr.pempub', j.platform_pem);
    }
    isAdmin = !!j.admin;
    clearTimeout(whoamiTimer);
  } catch (e) {
    showMsg('身份查询暂时失败（网络异常或服务不可用），将在约 60 秒后自动重试；当前管理功能保持不变。', 'warn');
    clearTimeout(whoamiTimer);
    whoamiTimer = setTimeout(syncAdmin, 60000); // P2-1 修复：重试定时器必须在 return 之前设置
    return; // P2：whoami 网络失败 ≠ 权限收回，保留现有状态等待重试（R5 C10：return 后原有不可达死代码已删）
  }
  state.admin = isAdmin; // §13.1/§13.2：管理员可见全量（镜像清单/Redis 库；身份由顶栏「管」角标表达）
  // 第2轮xiaozhi P3：帮助页 CLI 对照表的 user-add/user-passwd 为管理员命令——按 whoami 结论显隐（非 admin 默认隐藏，防误导）
  for (const el of document.querySelectorAll('#app .admin-only')) el.classList.toggle('hidden', !isAdmin);
  refreshAdminBadge(); // whoami 落定后同步顶栏角标（纯呈现；管理员与否只看「管」角标，无第三徽标——R4 用户裁决去冗余）
  if (isAdmin && imagesLoaded) ensureImages(true); // whoami 晚于首次清单加载时重取（admin 全量视角）
  if (isAdmin && !adminBuilt) {
    buildAdminTab(); setAdminBuilt(true);
    // R7：enterApp 的 startEventsStream 先于管理模板克隆执行，「已连接」写入落空且流已
    // 建立不会重写（对 ac 非空早退）——模板进文档后按流实况补一次指示器（第四轮终扫阻塞项）
    const evst = $('ev-state'); if (evst && evLive.ac) evst.textContent = '已连接';
  }
  else if (!isAdmin && adminBuilt) { // 权限被收回：拆除管理 DOM 并回到普通标签
    teardownAdmin(); setAdminBuilt(false);
    if (state.loggedIn) activateTab('proj');
  }
}
