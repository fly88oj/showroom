// —— app.mjs：入口（会话生命周期 + 登录窗编排 + 初始化）——
// 依赖单向：util ← state ← api ← auth ← img ← db ← proj ← admin ← shell ← app（无循环依赖）。
// 顶层语句按原单文件执行顺序排列：登录区事件 → online/visibility → A3 定时器锚点 → 初始化。
import { $, showMsg, fieldErr, clearErrs, busy } from './util.mjs';
import { state, NAME_RE } from './state.mjs';
import { api } from './api.mjs';
import { passwordLogin, ensureSeed, startHeartbeat, heartbeatTimer } from './auth.mjs';
import { ensureImages, refreshSpace, apiFail } from './img.mjs';
import { refreshProjects, evLive, startEventsStream } from './proj.mjs';
import { setRdDbsLoaded } from './db.mjs';
import { buildApp, startAutoRefresh, syncAdmin, appBuilt, autoRefreshTimer } from './shell.mjs';
// ———— showroom-web 控制台（SPEC §9.1）：原生 JS + showroom-common wasm 垫片 ————
// 零框架 / 零构建 / 零外部资源；登录门内只有登录 UI，管理壳放在 <template> 里惰性克隆。


async function onLoginSubmit(ev) {
  ev.preventDefault();
  clearErrs('lg-user', 'lg-pass');
  const user = $('lg-user').value.trim();
  if (!NAME_RE.test(user)) return fieldErr('lg-user', `账号「${user}」不合法（须匹配 ^[a-z0-9][a-z0-9-]{0,30}$），请修改后重试`);
  const pass = $('lg-pass').value;
  if (!pass) return fieldErr('lg-pass', '请输入密码；忘记密码请联系管理员重置');
  busy($('lg-submit'), true, '登录中…');
  try {
    await passwordLogin(user, pass);
    $('lg-pass').value = ''; // 用后即清，不留在 DOM
    $('lg-fb').textContent = '';
    enterApp();
    if (state.boundAdmin) showMsg('管理员账号：本设备已继承管理员权限（顶部「管理」标签可用）。', 'ok');
  } catch (e) {
    $('lg-fb').textContent = '';
    if (e && e.error === 'E_PW_LOCAL') {
      // 客户端无法也不应区分「密码错」与「未设密码」（防枚举的一致代价，§3）
      fieldErr('lg-pass', '密码错误，或该账号尚未设置密码（请联系管理员重置）');
    } else if (e && e.error === 'E_AUTH') {
      fieldErr('lg-pass', '登录被服务端拒绝（可能为限流或服务暂不可用），请稍后重试；忘记密码请联系管理员重置');
    } else {
      // 单通道：行内提示已含下一步动作，不再另发顶部条（与同函数其他分支一致）
      fieldErr('lg-pass', `登录失败（${e && e.msg ? e.msg : '网络异常或服务不可用'}），请稍后重试`);
    }
  } finally { busy($('lg-submit'), false); }
}

// —— 登录门引导：有 seed+user → 签名 health 自测直达；E_AUTH 回落密码表单（§2.2「找回/换设备」） ——
async function bootSelfTest() {
  try {
    await api('GET', '/_api/health');
    enterApp();
  } catch (e) {
    if (e.error === 'E_AUTH') {
      $('lg-fb').textContent = '本设备登录状态已失效：请输入账号密码重新登录即可。';
      $('lg-pass').focus?.();
    } else {
      apiFail('登录（连接探测）', e);
    }
  }
}

// B5/B6：原设备级密钥清除入口（登录窗与设置弹窗两处按钮及处理链）与原登录窗
// 「CLI/机器用户折叠登录」入口已整体删除——控制台登录=账号+密码一种方式，
// 状态机只有两态：登录态=控制台 / 登出或钥匙失效=登录窗（第三态入口不再保留）


// —— 登录区事件 ——
$('lg-form').addEventListener('submit', onLoginSubmit);
$('lg-user').addEventListener('input', () => fieldErr('lg-user', ''));
$('lg-pass').addEventListener('input', () => fieldErr('lg-pass', ''));
// 「免密码进入」入口已删（两态铁律：登录态=控制台 / 登出或钥匙失效=登录窗，无第三态）

function enterApp() {
  state.loggedIn = true;
  // 视觉QA P0-2：进入管理壳即清空顶部条（登录失败等残留错误不驻留五页顶部）
  $('msg').textContent = '';
  $('msg').className = '';
  $('login').classList.add('hidden');
  // A2 根因修复：appBuilt 在 buildApp 内「模板克隆完成即置位」，wire* 各自 try/catch 隔离——
  // 任何单点异常不再击穿后续初始化（登录误报失败/数据页按钮全死/心跳不启动/换号重复克隆 DOM 均由此消除）
  if (!appBuilt) {
    buildApp();
    // 密钥生命周期：60s 心跳 → whoami touch 续期；E_AUTH=钥匙过期/吊销 → 自动跳回登录
    try { startHeartbeat(); } catch (e) { console.warn('[showroom] startHeartbeat 启动失败（会话续期不可用）', e); }
    try { startAutoRefresh(); } catch (e) { console.warn('[showroom] startAutoRefresh 启动失败', e); }
  }
  // 第2轮xiaozhi P2：换号刷新——#hd-user 仅在 buildApp（首次进入）设置，logout 后密码登录新账号会残留旧账号名；
  // 每次进入都按 state.user 重写；数据页 Redis 作用域同步复位（rdDbsLoaded 置否 + 清旧选项 → ensureRedisDbs 按新账号重取）
  $('hd-user').textContent = state.user;
  setRdDbsLoaded(false);
  $('rd-db').innerHTML = ''; // 换号复位：清旧账号 DB 选项（R4 断言暴露的真残留）
  $('app').classList.remove('hidden');
  // plan-17 §3：登录即 4 路并发（projects/images/space/whoami），任一路失败不阻断其余（allSettled）
  let focusGuided = false;
  Promise.allSettled([refreshProjects(true), ensureImages(), refreshSpace(true), syncAdmin()]).then(() => {
    // 验收 16：登录焦点引导——已有项目→URL 列首链接；空账号→「发布」标签按钮（一次性；R5：顶栏 CTA 已删）
    if (!state.loggedIn || focusGuided) return;
    focusGuided = true;
    const firstOpen = $('proj-tbl')?.querySelector?.('tbody a');
    if (state.projects.length && firstOpen?.focus) firstOpen.focus();
    else $('tabbtn-pub')?.focus?.();
  });
  evLive.want = true; startEventsStream(); // 进页即订阅事件流（项目页为默认落地页；验收 11 ≤5s 行状态收敛）
}

// 断网恢复钩子（§3/验收 8）：连网恢复即重订阅事件流 + 强制重取项目列表（免刷新自动补齐）
if (typeof window !== 'undefined') window.addEventListener('online', () => {
  if (!state.loggedIn) return;
  evLive.want = true; startEventsStream();
  refreshProjects(true); refreshSpace(true);
});
// 事件流可见性钩子（§3）：页签隐藏退订（服务端收尸子进程）、回开项目页自动重订
document.addEventListener?.('visibilitychange', () => {
  if (!state.loggedIn) return;
  if (document.hidden) { evLive.want = false; if (evLive.ac) { evLive.ac.abort(); evLive.ac = null; } const st = $('ev-state'); if (st) st.textContent = '页面隐藏，暂停订阅'; }
  else if (!$('tab-proj')?.classList?.contains('hidden')) { evLive.want = true; startEventsStream(); }
});

// A3 回归锚点：模块级 let 不上 window——以 getter 暴露两个会话定时器，供无头浏览器断言「心跳/自动刷新已启动」
if (typeof window !== 'undefined') {
  Object.defineProperty(window, 'heartbeatTimer', { configurable: true, get: () => heartbeatTimer });
  Object.defineProperty(window, 'autoRefreshTimer', { configurable: true, get: () => autoRefreshTimer });
}

// —— 初始化（plan-16 §3）：无感生成钥匙；有「钥匙+账号」→ 签名自测直达（免密码）；
//    自测失败回落密码表单（E_AUTH=钥匙被删/回收 → 密码重绑新钥匙=「找回/换设备」） ——
state.user = localStorage.getItem('sr.user') || '';
ensureSeed();
$('lg-user').value = state.user;
if (state.seed && NAME_RE.test(state.user)) bootSelfTest();
else if (state.user) $('lg-pass').focus?.();
else $('lg-user').focus?.();
