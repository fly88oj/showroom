// —— proj.mjs：项目列表/行操作/更多菜单/行抽屉/日志流/事件流 + 发布表单与发布 dp-*（deploy 流）——
// activateTab/wireTabs 的页签切换与各页签数据源同层（switchTab 别名保留）。
import { $, esc, fmtTime, showMsg, errText, fieldErr, clearErrs, busy, copyText, randomProject, toHex } from './util.mjs';
import { state, ENV_DESC, checkProj, platformPem, pemReady, dataFresh, dataMark, retryOnce } from './state.mjs';
import { api, consumeNdjson } from './api.mjs';
import { ensureImages, refreshSpace, onImageRm, apiFail, uploadPayload, resumeUploadCore, sealedTotal, sealBegin, uploadRest, refreshResumeUi, latestUpId, upProgress } from './img.mjs';
import { fillDbTargets, ensureRedisDbs } from './db.mjs';
import { adminBuilt, activateAdminPanel } from './admin.mjs';
// —— plan-17 §4：项目 7 态显示机（服务器真值 5 + 本地在途 2；显示层只出 7 词；C4：已知态不再带英文 tooltip，未知原词回落保留） ——
const PUB_REBUILD_MS = 60000; // 发布成功后「重建中」滚动窗口
const localState = { publishing: new Set(), failedUntil: new Map(), rebuiltUntil: new Map() };
function markPublishing(project) { localState.publishing.add(project); renderProjects(); }
function markPublishDone(project) {
  localState.publishing.delete(project);
  localState.failedUntil.delete(project);
  localState.rebuiltUntil.set(project, Date.now() + PUB_REBUILD_MS);
  setTimeout(() => { if (state.loggedIn) refreshProjects(true); }, PUB_REBUILD_MS + 1000); // 窗口结束回真实态
}
function markPublishFail(project) { localState.publishing.delete(project); localState.failedUntil.set(project, Date.now() + PUB_REBUILD_MS); }
function projState(p) {
  const raw = String(p.status || '');
  const q = p.project;
  const now = Date.now();
  if (localState.publishing.has(q))
    return { label: '发布中…', cls: 'st-wait', breath: false, tip: '发布请求进行中（本地在途）：行操作已禁用，请稍候', act: { update: 0, ss: null, more: 0 } };
  if (deploy.phase === 'upload' && deploy.project === q)
    return { label: '上传中', cls: 'st-wait', breath: false,
      tip: '上传中（本地在途）：完成状态条见发布页；失败可断点续传', act: { update: 0, ss: null, more: 0 } };
  if (raw === 'running')
    return { label: '运行中', cls: 'status-ok', breath: false, tip: null, act: { update: 1, ss: 'stop', more: 1 } };
  if (raw === 'exited' || raw === 'paused' || raw === 'created')
    return { label: '已停止', cls: 'status-mid', breath: false, tip: null, act: { update: 1, ss: 'start', more: 1 } };
  if (raw === 'restarting' || (localState.rebuiltUntil.get(q) || 0) > now)
    return { label: '重建中', cls: 'status-mid', breath: true, tip: '更新发布成功后的滚动窗口', act: { update: 0, ss: null, more: 0 } };
  if (raw === 'dead' || (localState.failedUntil.get(q) || 0) > now)
    return { label: '异常', cls: 'status-bad', breath: false, tip: '可看日志排查，或点「发新版本」重新发布', act: { update: 1, ss: null, more: 0, rmOnly: 1 } };
  if (raw === 'missing')
    return { label: '待发布', cls: 'st-wait', breath: false, tip: '仅镜像无容器：从发布页选择镜像即可拉起', act: { update: 0, ss: null, more: 0, deploy: 1 } };
  return { label: '已停止', cls: 'status-mid', breath: false, tip: raw, act: { update: 1, ss: 'start', more: 1 } }; // 未知原词回落已停止（原词进 tooltip，唯一保留处）
}
/** 行操作区按状态机渲染（§4 可执行动作表）；●+文字双通道（色盲可辨，单色打印可分）。 */
function rowStatusHtml(p) {
  const st = projState(p);
  return `<td class="${st.cls}"${st.tip ? ` title="${esc(st.tip)}"` : ''}><span class="${st.breath ? 'st-breath' : ''}">●</span> ${esc(st.label)}</td>`;
}
/** 行操作区（R5：A2「打开」按钮删除——URL 列即链接；他人项目分支随 /_api/projects 只回本人项目整链路移除）。 */
function rowActionsHtml(p) {
  const st = projState(p);
  const q = esc(p.project);
  const b = (act, label, extra = '') => `<button class="act" data-act="${act}" data-p="${q}"${extra}>${label}</button>`;
  if (st.act.deploy) // 待发布：发布（→发布页预填项目名）·删除；日志经日志列 ▾ 打开抽屉
    return `${b('deploy-row', '发布', ' title="到发布页预填该项目并选择镜像拉起"')}${b('rm', '删除')}`;
  let html = st.act.update ? b('update', '发新版本') : b('update', '发新版本', ' disabled');
  // B1：停止为危险操作——danger variant（红）+ 二次确认（见 onProjAction），与删除/重置同档警示
  if (st.act.ss) html += st.act.ss === 'stop'
    ? `<button class="act danger" data-act="stop" data-p="${q}">停止</button>`
    : b(st.act.ss, '启动');
  if (st.act.rmOnly) html += b('rm', '删除');
  if (st.act.more) html += `<span class="more-wrap">
          <button class="act" data-act="more" data-p="${q}" aria-haspopup="menu" aria-expanded="false">更多 ▾</button>
          <span class="menu hidden" role="menu" aria-label="更多操作">
            <button class="act" role="menuitem" data-act="rollback" data-p="${q}">回滚到上一版本</button>
            <button class="act" role="menuitem" data-act="env" data-p="${q}">环境变量</button>
            <button class="act" role="menuitem" data-act="dblink" data-p="${q}">数据</button>
            <button class="act danger" role="menuitem" data-act="rm" data-p="${q}">删除</button>
          </span>
        </span>`;
  return html;
}

// —— 对外 URL 展示策略（plan-17 §6）：全站主链接一律 externalUrl()；服务端 p.url 仅作 tooltip ——
// base 解析：① 设置 sr.publicBase（#set-base）→ ② location.origin（host 非私网）→ ③ 兜底 origin + 内网提示条
const PRIV_HOST_RE = /^(localhost|127\.|0\.0\.0\.0|::1|\[::1\]|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/i;
function publicBase() {
  const saved = (localStorage.getItem('sr.publicBase') || '').trim().replace(/\/+$/, '');
  return saved || location.origin; // ②/③：公网 origin 直接用；内网 origin 兜底（列表上方出提示条）
}
const usingLanOrigin = () => !localStorage.getItem('sr.publicBase')
  && location.protocol !== 'https:' && PRIV_HOST_RE.test(location.hostname);
const externalUrl = (p) => `${publicBase()}/${p.project}/`;

export const updatePubUrl = () => {
  const v = $('pub-project').value.trim();
  $('pub-url').textContent = `/${v || '<project>'}/`; // §11：访问地址恒 /<project>/
};

// —— 标签页导航（role=tablist/tab/tabpanel：aria-selected 同步 + 左右方向键切换） ——
export function activateTab(name) {
  const tabs = ['proj', 'pub', 'img', 'db', 'help']; // plan-17 §1 五标签（管理页动态追加）
  if (adminBuilt) tabs.push('admin');
  for (const b of $('tabs').querySelectorAll('button')) {
    const on = b.dataset.tab === name;
    b.classList.toggle('on', on);
    b.setAttribute?.('aria-selected', on ? 'true' : 'false');
  }
  for (const s of tabs) $(`tab-${s}`).classList.toggle('hidden', s !== name);
  if (name === 'proj') { refreshProjects(); refreshSpace(); } // TTL 内跳过（plan-17 §3）
  if (name === 'pub') { ensureImages(); refreshSpace(true); } // 发布页：镜像下拉数据源（TTL 300s）+ 空间条强制校准（第1轮UI审查P1：tar 上传准入信息在本页可见）
  if (name === 'img') { ensureImages(); refreshSpace(true); } // 镜像库：清单（TTL）+ 空间条强制校准（上传准入关键信息，§14.1 进入本区即刷新）
  if (name === 'proj' || name === 'admin') { evLive.want = true; startEventsStream(); } // F4：事件面板在管理页——proj/admin 可见期均实时订阅（§3）
  else stopEventsStream();                                  // 离开两页才退订（服务端收尸 docker events 子进程）
  if (name === 'db') { ensureRedisDbs(); fillDbTargets(); } // Redis 区首次展开惰性拉库号；下拉消费已加载 projects
  if (name === 'admin') activateAdminPanel(); // E1：管理页按二级导航当前面板按需刷新（默认「账号」）
}

const switchTab = activateTab;

// ================ 标签页 1：项目管理（§13.1 三入口并存） ================
export function wireProjTab() {
  // R4 用户裁决：副题=纯状态文本，自动刷新兜底，无手动重取入口
  $('proj-tbl').addEventListener('click', onProjAction);
  // B2：「更多 ▾」菜单收起语义补全——document 外点收起 + Esc 收起（点菜单所在 .more-wrap 内不收）
  document.addEventListener('click', (ev) => {
    if (moreOpen && !ev.target?.closest?.('.more-wrap')) closeMoreMenu();
  });
  document.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape' && moreOpen) closeMoreMenu();
  });
  $('img-list').addEventListener('click', onImageRm);                       // §14.3：镜像清单行删除（事件委托）
  $('pub-form').addEventListener('submit', onDeploy); // 单表单提交：tar→压缩+上传+发布 / 镜像库→直接发布（plan-17 §5）
  $('btn-upload-only').addEventListener('click', onUploadOnly); // 次要按钮「仅上传」：同一 tar，只入库不发布
  for (const r of ['tar', 'registry']) $(`pub-src-${r}`).addEventListener('change', () => pickPubSource(r));
  $('dep-image').addEventListener('input', onImagePick);
  for (const id of ['pub-project', 'pub-tag', 'pub-file']) $(id).addEventListener('input', () => fieldErr(id, ''));
  $('pub-project').addEventListener('change', onPubProjectChange); // 命中已有项目 → 沿用上次发布参数
  $('dlg-env-close').addEventListener('click', () => $('dlg-env').close());
  $('dp-retry').addEventListener('click', () => {
    if (deploy.busy) return;
    // 失败阶段决定重试入口：上传失败且有会话 → 断点续传；发布失败 → 仅重发；否则全新开始
    const mode = deploy.failAt === 'upload' && deploy.uploadId ? 'resume'
      : deploy.failAt === 'publish' ? 'publish'
      : 'fresh';
    for (const i of [1, 2, 3]) $(`dp-s${i}`).classList.remove('err');
    dpSet(4, null);
    $('dp-done').classList.add('hidden');
    deployRun(mode);
  });
  $('dp-copy').addEventListener('click', () => copyText($('dp-url').textContent, '访问地址'));
  $('btn-resume').addEventListener('click', onResumeUpload);
  $('pub-project').addEventListener('input', updatePubUrl);
  refreshResumeUi();
}

// —— 镜像库源：选中镜像 → 自动带出项目与标签到发布表单（形如 user/project:tag；可手改） ——
function onImagePick() {
  fieldErr('dep-image', '');
  const m = $('dep-image').value.trim().match(/^[^\/\s]+\/([^\/\s:]+):(\S+)$/);
  if (m) {
    $('pub-project').value = m[1];
    $('pub-tag').value = m[2];
    updatePubUrl();
    fieldErr('pub-project', ''); fieldErr('pub-tag', '');
    pubPrefill(m[1], { keepTag: true }); // 参数预填（镜像 tag 优先于本地记忆 tag）
  }
}
/** plan-17 §5：镜像源 radio（默认 tar）。「镜像库」源发布时 ①压缩②上传自动跳过。 */
const pubSrc = () => ($('pub-src-registry').checked ? 'registry' : 'tar');
function pickPubSource(v) {
  for (const r of ['tar', 'registry']) $(`pub-src-${r}`).checked = r === v;
  $('pub-src-file-box').classList.toggle('hidden', v !== 'tar');
  $('pub-src-reg-box').classList.toggle('hidden', v !== 'registry');
  $('btn-upload-only').disabled = v !== 'tar'; // 仅上传只对本地 tar 有意义
  if (v === 'tar') fieldErr('dep-image', '');
}
/** 命中已有项目名 → 沿用上次发布参数（mem=服务端 conf 行；tag/cport=本机 sr.conf.* 记忆）。 */
// C4：内存输入以 MiB 裸数字为准（如 256）；服务端 parse_mem 把裸数字按字节解释，故提交时补 m 后缀
const memToUi = (v) => String(v ?? '').trim().replace(/^(\d+)[mM]$/, '$1');       // 「256m」→「256」（展示）
const memFromUi = (v) => { const s = String(v ?? '').trim(); return /^\d+$/.test(s) ? `${s}m` : s; }; // 「256」→「256m」（提交）
function onPubProjectChange() {
  const v = $('pub-project').value.trim();
  if (state.projects.some((x) => x.project === v)) pubPrefill(v);
  else $('pub-pref-note')?.classList.add('hidden');
}
function pubPrefill(project, opts = {}) {
  const row = state.projects.find((x) => x.project === project);
  let conf = null;
  try { conf = JSON.parse(localStorage.getItem('sr.conf.' + project) || 'null'); } catch { /* 坏记录忽略 */ }
  let hit = false;
  if (row && row.mem) { $('pub-mem').value = memToUi(row.mem); hit = true; } // 服务端 conf 权威（GET /_api/projects）
  if (conf && conf.cport) { $('pub-cport').value = String(conf.cport); hit = true; } // 响应无 cport 字段 → 本地记忆补（字符串赋值）
  if (conf && conf.tag && !opts.keepTag) { $('pub-tag').value = conf.tag; hit = true; }
  const note = $('pub-pref-note');
  if (note) {
    if (hit) {
      const d = conf && conf.ts ? new Date(conf.ts) : null;
      const md = d && !Number.isNaN(d) ? `（${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}）` : '';
      note.textContent = `沿用上次发布参数${md}，可修改`;
      note.classList.remove('hidden');
    } else note.classList.add('hidden');
  }

}
export async function refreshProjects(force = false) {
  if (!force && dataFresh('projects')) return; // plan-17 §3：TTL 内跳过（内存单源）
  const tb = $('proj-tbl').querySelector('tbody');
  if (tb) tb.innerHTML = '<tr><td colspan="7" class="note">加载中…</td></tr>';
  try {
    const j = await api('GET', '/_api/projects');
    state.projects = j.projects || [];
    renderProjects();
    dataMark('projects');
    $('proj-fb').textContent = ''; // 成功不写「已加载 N 项」计数（表格即数据）；本区保留操作反馈用途（启动/停止结果等）
    fillDbTargets();
  } catch (e) {
    // §3 失败表现：表格内联「加载失败 + 重试」（顶部条不打扰）+ 3s 自动重试 1 次
    if (tb) tb.innerHTML = `<tr><td colspan="7" class="note">加载失败 · ${esc(errText(e))}　<button type="button" class="act" data-act="retry-projects">重试</button></td></tr>`;
    $('proj-fb').textContent = '加载失败（将自动重试）';
    retryOnce('projects', () => refreshProjects(true));
  }
}

export function renderProjects() {
  const tb = $('proj-tbl').querySelector('tbody');
  // B4 徽章已随 R4 用户裁决移除（管理员身份由顶栏「管」角标表达，同一信息只出现一次）
  if (!state.projects.length) { tb.innerHTML = '<tr><td colspan="7" class="note">暂无项目——到「发布」页发布第一个项目。</td></tr>'; }
  else {
    const acts = state.projects.map((p) => {
      const q = esc(p.project);
      return `<tr data-row="${q}">
      <td class="mono">${q}</td>
      ${rowStatusHtml(p)}
      <td class="wrap"><a href="${esc(externalUrl(p))}" target="_blank" rel="noopener">${esc(externalUrl(p))}</a></td>
      <td class="mono">${esc(p.tag)}</td>
      <td class="mono num">${esc(fmtTime(p.updated))}</td>
      <td>${rowActionsHtml(p)}</td>
      <td><button class="act" data-act="logs" data-p="${q}" title="展开日志抽屉" aria-expanded="false">▾</button></td>
    </tr>`;
    }).join('');
    tb.innerHTML = acts;
  }
  $('proj-base-note')?.classList.toggle('hidden', !usingLanOrigin()); // 兜底 origin 为内网才提示（配置 base 即收敛）
  // 行抽屉常驻：重渲后重新挂回（hidden 状态保持；展开时移到原项目行后不丢抽屉）
  const tbEl = $('proj-tbl')?.querySelector?.('tbody');
  if (tbEl && !tbEl.contains?.(drawerRow)) tbEl.append?.(drawerRow);
  if (drawerRow.dataset.drawer) {
    const r = $('proj-tbl')?.querySelector?.(`tr[data-row="${drawerRow.dataset.drawer}"]`);
    if (r && r.after) r.after(drawerRow);
  }
  syncLogsDrawerBtn(); // B1：重渲后恢复该行 ▴/aria-expanded（抽屉展开态不因重渲丢失）
  if (moreOpen && moreOpen.isConnected === false) moreOpen = null; // B2：重渲后旧「更多」按钮已脱离文档——指向即清
  // 项目下拉候选 = 已有项目
  $('dl-projects').innerHTML = [...new Set(state.projects.map((p) => p.project))]
    .map((p) => `<option value="${esc(p)}">`).join('');
}

const ACT_CN = { start: '启动', stop: '停止', rollback: '回滚', rm: '删除' };
let moreOpen = null; // 当前展开的「更多 ▾」菜单按钮（单例；翻页/重渲自然收起）
function closeMoreMenu() {
  if (!moreOpen) return;
  const wrap = moreOpen.closest ? moreOpen.closest('.more-wrap') : null;
  const menu = wrap && wrap.querySelector ? wrap.querySelector('.menu') : null;
  if (menu) menu.classList.add('hidden');
  moreOpen.textContent = '更多 ▾'; // B2：箭头随收起翻回 ▾
  moreOpen.setAttribute?.('aria-expanded', 'false');
  moreOpen = null;
}
function toggleMoreMenu(btn) {
  const wrap = btn.closest ? btn.closest('.more-wrap') : null;
  const menu = wrap && wrap.querySelector ? wrap.querySelector('.menu') : null;
  if (!menu) return;
  const wasOpen = moreOpen === btn;
  closeMoreMenu();
  if (!wasOpen) {
    menu.classList.remove('hidden');
    btn.textContent = '更多 ▴'; // B2：展开箭头翻 ▴
    btn.setAttribute?.('aria-expanded', 'true');
    moreOpen = btn;
  }
}

/** 行「更新」→ 发布页预填同一套（plan-17 §5/验收 7）。 */
function updateViaPub(p) {
  activateTab('pub');
  $('pub-project').value = p.project;
  updatePubUrl();
  pubPrefill(p.project, { fallbackTag: p.tag }); // 无本地记忆时回落当前 tag
  $('pub-tag').focus?.();
}

async function onProjAction(ev) {
  const btn = ev.target.closest('button[data-act]');
  if (!btn || btn.disabled) return;
  const { act, p: project } = btn.dataset;
  if (act === 'env') return showEnvDlg(project);
  if (act === 'logs') { closeMoreMenu(); return drawerRow.dataset.drawer === project ? closeDrawer() : openDrawer(project); } // ▾ 开关语义：再点同项目即收起（R4 用户反馈"收不起来"）
  if (act === 'more') return toggleMoreMenu(btn);
  if (act === 'retry-projects') { btn.disabled = true; return refreshProjects(true); } // 内联重试（§3）
  closeMoreMenu();
  if (act === 'update' || act === 'deploy-row') { // 待发布行「发布」=发布页预填项目名（§4）
    const row = state.projects.find((x) => x.project === project);
    if (row) updateViaPub(row);
    return;
  }
  if (act === 'dblink') { // 数据深链：数据页并填项目（复用已加载 projects，不另发请求）
    activateTab('db');
    const el = $('db-project'); if (el) el.value = project;
    return;
  }
  if (act === 'rm' && !confirm(`确认删除项目 ${project}？容器与路由即刻下线，数据库将一并删除，如需保留请先在数据页备份下载 .dump；删除后凭据一并清除，不可恢复。`)) return;
  // B1：停止与删除/重置同款二次确认（危险操作警示对齐：按钮红 + confirm 写明影响）
  if (act === 'stop' && !confirm(`确认停止项目 ${project}？容器即刻停止、对外不可访问（数据与凭据保留，可随时重新「启动」）。`)) return;
  // 第1轮UI审查P3：启停/回滚/删除请求期间按钮就地变「…中」并禁用（busy 状态），完成后恢复
  const ACT_BUSY_CN = { start: '启动中…', stop: '停止中…', rollback: '回滚中…', rm: '删除中…' }[act];
  if (ACT_BUSY_CN) busy(btn, true, ACT_BUSY_CN); else btn.disabled = true;
  try {
    await api('POST', `/_api/${act}`, { project });
    $('proj-fb').textContent = `已${ACT_CN[act]} ${project}`; // 访问地址表格 URL 列已有，不重复播报
    refreshProjects(true); // 写操作强制失效（plan-17 §3）
  } catch (e) { apiFail(`${ACT_CN[act]} ${project}`, e); }
  finally { if (ACT_BUSY_CN) busy(btn, false); else btn.disabled = false; }
}

// —— §12.1/§13.1 发布与仅上传（同一状态条）：①压缩 → ②分片上传 → ③发布 → ④完成 ——
const deploy = { busy: false, intent: 'deploy', file: null, blob: null, project: '', tag: '', uploadId: null, failAt: null };
const PHASE_CN = { compress: '压缩', upload: '上传', publish: '发布' };
function dpSet(i, cls) { // cls：'on' 进行中 / 'done' 完成 / 'err' 失败 / 'skip' 跳过
  const el = $(`dp-s${i}`);
  el.classList.remove('on', 'done', 'err', 'skip');
  if (cls) el.classList.add(cls);
}
const dpText = (t) => { $('dp-text').textContent = t; };
function dpReset() {
  $('dp-bar').classList.remove('hidden', 'err');
  for (const i of [1, 2, 3, 4]) dpSet(i, null);
  $('dp-done').classList.add('hidden');
  $('dp-retry').classList.add('hidden');
  $('dp-url').textContent = '';
  $('dp-url').removeAttribute?.('href');
  $('dp-prog').max = 1; $('dp-prog').value = 0;
  dpText('准备中…');
  deploy.failAt = null;
}
/** 部署主流程：mode = fresh 全新 / resume 断点续传（沿用 uploadId 会话）/ publish 仅重发发布。
 *  终点由 deploy.intent 决定：'deploy' 上传后自动发布（③④），'upload' 仅上传入库（③④ 标 skip）。 */
async function deployRun(mode) {
  deploy.busy = true; upBusy = true;
  $('dp-retry').classList.add('hidden');
  $('dp-bar').classList.remove('err');
  const { project, tag, file } = deploy;
  let phase = mode === 'publish' ? 'publish' : (mode === 'resume' ? 'upload' : 'compress');
  deploy.phase = phase; // plan-17 §4：本地「上传中」行标记数据源
  try {
    if (mode === 'fresh') { // ① 压缩（未选文件则①②一并跳过）
      if (file) {
        dpSet(1, 'on'); dpText('① 压缩中：读取文件…');
        $('dp-prog').max = file.size; $('dp-prog').value = 0;
        const { blob, zstd: pre } = await uploadPayload(file, (done, raw) => {
          dpText(`① 压缩中：${((done / raw) * 100).toFixed(1)}%（${done} / ${raw} 字节）`);
          $('dp-prog').max = raw; $('dp-prog').value = done;
        });
        deploy.blob = blob; deploy.pre = pre;
        dpSet(1, 'done');
        dpText(`压缩完成：${pre ? '文件无需压缩，直接上传' : `压缩后 ${blob.size} / 原 ${file.size} 字节`}`);
      } else {
        dpSet(1, 'skip'); dpSet(2, 'skip');
        dpText('未选镜像文件：跳过压缩与上传，直接发布（要求镜像已入镜像库）');
      }
    }
    if (file && mode !== 'publish') { // ② 上传（失败重试走断点续传，§8.1）
      phase = 'upload';
      deploy.phase = 'upload';
      renderProjects(); // 行内标记「上传中」（dp-bar 为主显示）
      dpSet(2, 'on');
      if (deploy.uploadId) {
        await resumeUploadCore(deploy.uploadId, {
          compNote: (done, raw) => { dpSet(1, 'on'); dpText(`① 重新压缩（断点续传重建产物）：${done} / ${raw} 字节…`); $('dp-prog').max = raw; $('dp-prog').value = done; },
          onRec: (rec) => dpText(`② 上传中：断点续传（服务端已收 ${rec} 片），重新压缩后补传…`),
          upNote: dpUploadNote,
        });
        dpSet(1, 'done');
      } else {
        await deployUpload(project, tag);
      }
      deploy.uploadId = null;
      dpSet(2, 'done');
      if (deploy.intent === 'upload') { // 入口二「仅上传」：到②即完成，不发布（③④ 跳过）
        dpSet(3, 'skip'); dpSet(4, 'skip');
        dpText('② 完成：镜像已入库。可稍后到本页选「镜像库」源发布。');
        deploy.failAt = null;
        ensureImages(true); // 清单立刻补上新镜像
        return;
      }
      dpText('上传完成：镜像已导入镜像库。');
    }
    phase = 'publish'; // ③ 发布（参数取自表单）
    deploy.phase = 'publish';
    markPublishing(project); // 行内标记「发布中…」
    dpSet(3, 'on');
    dpText('③ 发布中…');
    const body = { project, tag, container_port: parseInt($('pub-cport').value, 10) || 8080 };
    const memv = memFromUi($('pub-mem').value); // C4：裸数字按 MiB 提交（补 m 后缀，服务端字节语义不变）
    if (memv) body.mem = memv;
    await api('POST', '/_api/publish', body);
    try { localStorage.setItem('sr.conf.' + project, JSON.stringify({ tag, cport: parseInt($('pub-cport').value, 10) || 8080, ts: Date.now() })); } catch { /* 存储不可写忽略 */ } // plan-17 §5：下次发布/「更新」预填
    markPublishDone(project); // 「重建中」60s 滚动窗口（§4）
    dpSet(3, 'done'); dpSet(4, 'done'); // ④ 完成：大字 URL + 复制（发布结果以此处为唯一播报，不再另发顶部条）
    const eurl = externalUrl({ project }); // plan-17 §6：完成大字 URL 一律 externalUrl
    dpText('④ 完成：发布成功。');
    $('dp-url').textContent = eurl;
    $('dp-url').setAttribute?.('href', eurl);
    $('dp-done').classList.remove('hidden');
    refreshProjects(true);
    deploy.failAt = null;
  } catch (e) {
    deploy.failAt = phase;
    if (phase === 'publish') markPublishFail(project); // 本地「异常」终态标记（§4）
    $('dp-bar').classList.add('err');
    dpSet(phase === 'compress' ? 1 : phase === 'upload' ? 2 : 3, 'err');
    dpText(`「${PHASE_CN[phase]}」步骤失败：${errText(e)}${phase === 'upload' && deploy.uploadId ? '；点「重试」将从断点继续。' : '；可点「重试」再次执行。'}`);
    $('dp-retry').classList.remove('hidden');
    apiFail(`发布（${PHASE_CN[phase]}）`, e);
    if (phase === 'compress') deploy.blob = null;
  } finally { deploy.busy = false; upBusy = false; deploy.phase = null; renderProjects(); }
}

/** 校验并填充公共发布参数（项目留空自动生成回填；标签必填）；返回 null=校验未过。 */
function collectPubFields(requireFile) {
  let project = $('pub-project').value.trim();
  if (!project) { // §12.1：项目名为空 = 自动生成随机 8 位并回填表单
    project = randomProject();
    $('pub-project').value = project;
    updatePubUrl();
    showMsg(`项目名为空：已自动生成随机项目名 ${project} 并填入表单。`, 'ok');
  }
  const tag = $('pub-tag').value.trim();
  const badP = checkProj(project);
  if (badP) { fieldErr('pub-project', badP); return null; }
  if (!tag || /\s|\//.test(tag)) { fieldErr('pub-tag', `镜像标签「${tag}」不合法（非空、无空白/斜杠），请填写后再试`); return null; }
  const file = ($('pub-file').files || [])[0] || null;
  if (requireFile && !file) { fieldErr('pub-file', '「仅上传」需要选择镜像 tar 文件（不选文件的直发请用「发布」，已有镜像请改选「镜像库」源）'); return null; }
  return { project, tag, file };
}

/** 两入口共用：请求期间互斥禁用对方按钮（各自显示「发布中…/上传中…」）。 */
function pubBtnsBusy(on) {
  busy($('pub-submit'), on, '发布中…');
  busy($('btn-upload-only'), on, '上传中…');
}

/** 入口一「发布」：tar→压缩→上传→立即发布（快捷路径）。 */
async function onDeploy(ev) {
  ev.preventDefault();
  if (deploy.busy || upBusy) return showMsg('已有发布或上传进行中，请等待完成');
  clearErrs('pub-project', 'pub-tag', 'pub-file', 'dep-image');
  let file = null;
  if (pubSrc() === 'registry') { // 镜像库源：①②跳过直接发布；镜像必须已选（选中即带出项目/标签）
    const img = $('dep-image').value.trim();
    if (!/^[^\/\s]+\/[^\/\s:]+:\S+$/.test(img)) return fieldErr('dep-image', '请先在「镜像库」源选择镜像（格式 user/project:tag），或切换回「本地 tar 文件」源');
  } else {
    file = ($('pub-file').files || [])[0] || null; // files 恒存在（FileList）；防御stub/异常环境
  }
  const f = collectPubFields(false);
  if (!f) return;
  if (file && !pemReady()) return; // plan-17 验收 6：未配置 PEM → 内联「去设置」引导
  deploy.intent = 'deploy';
  deploy.project = f.project; deploy.tag = f.tag;
  deploy.file = file; // tar 源才携带文件；镜像库源恒 null（①②跳过）
  deploy.blob = null; deploy.uploadId = null;
  dpReset();
  pubBtnsBusy(true);
  await deployRun('fresh');
  pubBtnsBusy(false);
}

/** 次要按钮「仅上传」（§13.1）：同一 tar 选择，只压缩上传入库，不发布。 */
async function onUploadOnly() {
  if (deploy.busy || upBusy) return showMsg('已有发布或上传进行中，请等待完成');
  if (pubSrc() !== 'tar') return showMsg('「仅上传」用于本地 tar 文件：请先选回「本地 tar 文件」镜像源');
  clearErrs('pub-project', 'pub-tag', 'pub-file');
  const f = collectPubFields(true);
  if (!f) return;
  if (!pemReady()) return; // 仅上传同样走加密信封
  deploy.intent = 'upload';
  deploy.project = f.project; deploy.tag = f.tag;
  deploy.file = f.file;
  deploy.blob = null; deploy.uploadId = null;
  dpReset();
  pubBtnsBusy(true);
  await deployRun('fresh');
  pubBtnsBusy(false);
}

/** ② 上传段进度：已传 / 压缩后总长（同时镜像到镜像库「断点续传」区的进度条）。 */
function dpUploadNote(sent, total) {
  upProgress(sent, total);
  $('dp-prog').max = total; $('dp-prog').value = sent;
  dpText(`② 上传中：已传 ${sent} / 压缩后总长 ${total} 字节（${total ? ((sent / total) * 100).toFixed(1) : '0'}%）`);
}

/** 全新上传（发布②段）：import-begin → 本地会话登记 → 逐片加密上传 → 收尾。 */
async function deployUpload(project, tag) {
  const pem = platformPem();
  if (!pem.startsWith('-----BEGIN PUBLIC KEY-----')) throw { error: 'E_STATE', msg: '尚未完成安全配置：到「设置」完成' };
  const zb = deploy.blob, file = deploy.file;
  const total = sealedTotal(zb.size);
  dpUploadNote(0, total); // 状态条切入②上传段（已传 0 / 压缩后总长）
  const sessSeed = new Uint8Array(32); crypto.getRandomValues(sessSeed);
  let uploadId;
  try {
    const j = await api('POST', '/_api/import-begin', { project, tag, image_id: file.name, total_bytes: total });
    uploadId = j.upload_id;
  } catch (e) {
    if (e.error === 'E_QUOTA') e.msg += `（信封总长 ${total} 字节）`;
    throw e;
  }
  deploy.uploadId = uploadId; // 失败重试凭此断点续传
  localStorage.setItem('sr.up.' + uploadId, JSON.stringify({ seed: toHex(sessSeed), project, tag, size: file.size, zsize: zb.size, name: file.name }));
  refreshResumeUi();
  // 上传开始即自动记录（localStorage sr.up.<id>），恢复时自动取最近会话——用户无需知道编号
  const sid = sealBegin(pem, sessSeed);
  await uploadRest(uploadId, sid, zb, 0, 0, total, dpUploadNote);
}

// —— 环境变量模态（§6 键 + 当前值 + 中文说明）——
async function showEnvDlg(project) {
  const dlg = $('dlg-env');
  $('env-tbl').querySelector('tbody').innerHTML = '<tr><td colspan="3" class="note">加载中…</td></tr>';
  dlg.showModal();
  try {
    const j = await api('GET', `/_api/env?project=${encodeURIComponent(project)}`);
    const env = j.env || {};
    const keys = Object.keys(env).sort();
    $('env-tbl').querySelector('tbody').innerHTML = keys.length
      ? keys.map((k) => `<tr><td class="mono">${esc(k)}</td><td class="mono wrap">${esc(env[k])}</td><td class="wrap">${esc(ENV_DESC[k] || '')}</td></tr>`).join('')
      : '<tr><td colspan="3" class="note">（空）</td></tr>';
  } catch (e) {
    $('env-tbl').querySelector('tbody').innerHTML = `<tr><td colspan="3" class="status-bad">${esc(errText(e))}</td></tr>`;
  }
}

let upBusy = false;

async function onResumeUpload() {
  if (upBusy || deploy.busy) return showMsg('已有上传或发布进行中');
  const id = latestUpId();
  if (!id) return; // 按钮仅在存在中断会话时可见（R5b），此处为防御
  if (!pemReady()) return; // 续传重建信封同样需要 PEM
  upBusy = true;
  busy($('btn-resume'), true, '恢复中…'); // B3：请求期间按钮就地「恢复中…」并禁用
  try {
    const r = await resumeUploadCore(id, {
      onRec: (rec) => { $('up-status').textContent = `会话 ${id}：服务端已收 ${rec} 片，重建压缩产物…`; },
      compNote: (done, raw) => { $('up-status').textContent = `重新压缩中 ${done} / ${raw} 字节（恢复续传需先重建压缩产物）…`; },
    });
    // C5：完成引导并入 up-status 唯一落点（proj-fb 跨页写入删除）
    $('up-status').textContent = `恢复完成：镜像导入完成（${r.pre ? '原文无需压缩' : `压缩后 ${r.zsize} 字节`}）。到「发布」页选「镜像库」源即可发布。`;
  } catch (e) {
    apiFail('恢复上传', e);
    $('up-status').textContent = `恢复中断：${errText(e)}`;
  } finally { upBusy = false; busy($('btn-resume'), false); }
}

// ================ 标签页 3：状态监控 ================
// —— §10.4 日志实时流：ndjson 追加渲染 + 1000 行环形 + 断开 5 秒重连 ——
const LOGS_MAX = 1000;
export const logsLive = { ac: null, timer: null, project: null, lines: [], want: true }; // want 恒 true（跟随开关 UI 已删：抽屉展开即实时跟随）
const tsCn = (t) => Number.isFinite(t)
  ? new Date(t).toLocaleTimeString('zh-CN', { hour12: false }) : '--:--:--';
export function logsRender() {
  $('st-logs').textContent = logsLive.lines.length ? logsLive.lines.join('\n') : '（暂无日志）';
}
function logsAppend(t, l) {
  logsLive.lines.push(`${tsCn(t)} ${l}`); // 每行带服务端时间戳（unix ms → 本地 HH:MM:SS）
  if (logsLive.lines.length > LOGS_MAX) logsLive.lines.splice(0, logsLive.lines.length - LOGS_MAX); // 超帽丢头部
  logsRender();
}
function stopLogsStream() {
  clearTimeout(logsLive.timer); logsLive.timer = null;
  if (logsLive.ac) { logsLive.ac.abort(); logsLive.ac = null; } // 断流即停（回执由连接状态就地更新，无「已收起」类播报）
}
async function startLogsStream() {
  if (!logsLive.want || !logsLive.project || logsLive.ac) return;
  const project = logsLive.project;
  logsLive.ac = new AbortController();
  const ac = logsLive.ac;
  $('st-logs-fb').textContent = ''; // 成功态静默（R4 同一信息只出现一次；仅在断连/结束时说话）
  try {
    await consumeNdjson(`/_api/logs-stream?project=${encodeURIComponent(project)}&tail=100`, (j) => {
      if (j && typeof j.l === 'string') logsAppend(j.t, j.l);
    }, ac.signal);
    if (ac.signal.aborted) return;
    logsLive.ac = null;
    $('st-logs-fb').textContent = '流已结束（容器可能已被删除）'; // 容器 EOF 正常收尾：不再重连
  } catch (e) {
    if (ac.signal.aborted) return; // 主动停止不算断线
    logsLive.ac = null;
    $('st-logs-fb').textContent = `连接断开（${errText(e)}），5 秒后自动重连…`;
    clearTimeout(logsLive.timer);
    logsLive.timer = setTimeout(() => { if (logsLive.want && logsLive.project === project) startLogsStream(); }, 5000);
  }
}
function pickLogs(project) {
  logsLive.project = project;
  logsLive.lines = [];
  logsRender();
  stopLogsStream();
  startLogsStream(); // want 恒 true（跟随开关已删，无「未跟随」分支）
}

// —— §10.4 平台事件流：sr- 前缀过滤 + 最近 20 条滚动 + 心跳忽略 + 健康列表去抖刷新 ——
const EV_MAX = 20;
const EV_CN = { start: '启动', stop: '停止', die: '退出', destroy: '销毁',
  'health_status: healthy': '转为健康', 'health_status: unhealthy': '转为不健康', 'health_status: starting': '健康检查中' };
export const evLive = { ac: null, timer: null, want: false, rows: [], debounce: null };
function evRender() {
  const el = $('st-events'); // S2：st-events 迁入管理页（非管理员 DOM 不存在——静默跳过渲染）
  if (el) el.textContent = evLive.rows.length ? evLive.rows.join('\n') : '（暂无事件）';
}
function evPush(t, e, actor) {
  evLive.rows.push(`${tsCn(t)}  ${EV_CN[e] || e}  ${actor}`); // 时间 + 动作 + 容器名
  if (evLive.rows.length > EV_MAX) evLive.rows.splice(0, evLive.rows.length - EV_MAX);
  evRender();
  if (evLive.debounce) clearTimeout(evLive.debounce); // 事件到达 → 2 秒去抖自动刷新项目列表（验收 11）
  evLive.debounce = setTimeout(() => { evLive.debounce = null; if (state.loggedIn) refreshProjects(true); }, 2000);
}
export function stopEventsStream() {
  evLive.want = false;
  clearTimeout(evLive.timer); evLive.timer = null;
  if (evLive.debounce) { clearTimeout(evLive.debounce); evLive.debounce = null; }
  if (evLive.ac) { evLive.ac.abort(); evLive.ac = null; }
  const st = $('ev-state'); // S2：ev-state 迁入管理页（非管理员不存在）
  if (st) st.textContent = '未连接（项目页/管理页可见时自动订阅）';
}
export async function startEventsStream() {
  if (!evLive.want || evLive.ac || !state.loggedIn) return;
  evLive.ac = new AbortController();
  const ac = evLive.ac;
  const st = $('ev-state'); if (st) st.textContent = '已连接';
  try {
    await consumeNdjson('/_api/events', (j) => {
      // 仅展示 sr- 开头的项目容器；心跳（{"hb":true}）与缺字段行静默忽略
      if (j && typeof j.actor === 'string' && j.actor.startsWith('sr-')) evPush(j.t, j.e, j.actor);
    }, ac.signal);
    if (ac.signal.aborted) return;
    reconnectEvents(`流结束`);
  } catch (e) {
    if (ac.signal.aborted) return;
    reconnectEvents(`断开（${errText(e)}）`);
  }
}
function reconnectEvents(why) {
  evLive.ac = null;
  const st = $('ev-state'); if (st) st.textContent = `${why}，5 秒后自动重连…`;
  clearTimeout(evLive.timer);
  evLive.timer = setTimeout(() => { if (evLive.want && state.loggedIn) startEventsStream(); }, 5000);
}

// —— 行抽屉（plan-17 §1/S2）：st-logs* 迁入项目行后插行；一次一行，展开即订阅、收起/换行即断 ——
export const drawerRow = document.createElement('tr');
drawerRow.className = 'drawer-row';
drawerRow.hidden = true; // buildApp 即挂入项目表 tbody（保证 st-logs* 可 getElementById 接线），展开时移动到项目行后
drawerRow.dataset.drawer = ''; // 展开时=所属项目（无全局行 id；st-logs* 唯一 id 在抽屉内）
drawerRow.innerHTML = `<td colspan="7">
  <div class="row">
    <button type="button" id="drawer-close" class="act">收起</button>
    <span class="fb warn" id="st-logs-fb" aria-live="polite"></span>
  </div>
  <pre class="out mono" id="st-logs" aria-live="polite">（未选择项目）</pre>
</td>`;
/** B1：抽屉展开态同步到所属行「日志」按钮——▾/▴ 翻转 + title + aria-expanded；
 *  三处调用：openDrawer / closeDrawer / renderProjects（重渲后恢复）。 */
function syncLogsDrawerBtn() {
  const project = drawerRow.dataset.drawer;
  const btn = project ? $('proj-tbl')?.querySelector?.(`tr[data-row="${project}"] button[data-act="logs"]`) : null;
  if (!btn) return;
  const open = !drawerRow.hidden && !!project;
  btn.textContent = open ? '▴' : '▾';
  btn.setAttribute?.('title', open ? '收起日志' : '展开日志抽屉');
  btn.setAttribute?.('aria-expanded', open ? 'true' : 'false');
}
function openDrawer(project) {
  closeDrawer();
  const esc2 = (typeof CSS !== 'undefined' && CSS.escape) ? CSS.escape(project) : project; // 项目名恒 [a-zA-Z0-9-]，转义仅防御
  const row = $('proj-tbl').querySelector(`tr[data-row="${esc2}"]`);
  if (row && row.after) row.after(drawerRow); else $('proj-tbl').querySelector('tbody')?.append?.(drawerRow);
  drawerRow.hidden = false;
  drawerRow.dataset.drawer = project;
  syncLogsDrawerBtn(); // B1：展开 ▴ + title「收起日志」
  pickLogs(project);
}
export function closeDrawer() {
  drawerRow.hidden = true; // 收起=隐藏挂回 tbody（元素常驻文档，st-logs* 始终可寻址）
  syncLogsDrawerBtn(); // B1：收起 ▾（在 dataset.drawer 清空前读所属行按钮）
  drawerRow.dataset.drawer = '';
  if (logsLive.project) stopLogsStream(); // 收起即断（§3）
  logsLive.project = null;
}
