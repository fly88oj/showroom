// —— db.mjs：数据页四件套（表清单/预览/SQL/备份恢复重置）+ Redis 浏览（库号/键列表/值查看）——
import { $, enc, esc, showMsg, busy, sleep, derToPem, setComboOptions } from './util.mjs';
import { checkProj, SEG, DUMP_CHUNK, platformPem, pemReady, retryOnce, myProjectNames } from './state.mjs';
import { ex, mem, alloc, put, unput, shimCStr, api, apiDo, respErr } from './api.mjs';
import { apiFail, uploadPayload, sealBegin, pushSlice, pushFlush } from './img.mjs';
// ================ 标签页 2：数据库（SPEC §9） ================
// C17：openSettings 归 shell（对话框编排层）——经 wireDbTab 注入（恢复流程「去设置」引导用），避免 db→shell 反向依赖
let openSettings = () => {};
export function wireDbTab(openSettingsFn) {
  openSettings = openSettingsFn;
  // Redis 浏览（§10.3；S2 自状态页迁入数据页）
  // A1 根因清理：#rd-refresh 按钮已随 c3b0558 删除（30s 自动刷新取代），此处死引用曾致
  // wireDbTab 抛 TypeError → buildApp/enterApp 雪崩（登录误报失败等 4 个 P0）；查询入口=回车/选库
  $('rd-q').addEventListener('keydown', (ev) => { if (ev.key === 'Enter') { ev.preventDefault(); refreshRedisKeys(); } });
  $('rd-db').addEventListener('change', () => refreshRedisKeys()); // C2：选库即自动加载键列表
  $('rd-keys').addEventListener('change', onRedisKeyPick);
  $('db-project').addEventListener('change', () => { if ($('db-project').value.trim()) onDbTables(); }); // 选定项目即自动载表清单（R4 用户裁决：非业务操作最小化）
  $('btn-db-preview').addEventListener('click', onDbPreview);
  $('btn-db-sql').addEventListener('click', onDbSql);
  $('btn-db-dump').addEventListener('click', onDbDump);
  $('btn-db-restore').addEventListener('click', onDbRestore);
  $('btn-db-reset').addEventListener('click', onDbReset);
}
function dbTarget() {
  const project = $('db-project').value.trim();
  const bad = checkProj(project);
  if (bad) { showMsg(bad); return null; }
  return { project };
}
export function fillDbTargets() {
  setComboOptions($('db-project'), myProjectNames()); // C16：集合提取 state.mjs（与项目下拉候选同源）
}

const dbOut = (text) => { $('db-out').textContent = text; };

async function onDbTables() {
  const t = dbTarget();
  if (!t) return;
  $('db-tbl-fb').textContent = '正在获取表清单…';
  try {
    const j = await api('GET', `/_api/db-tables?project=${encodeURIComponent(t.project)}`);
    const tables = j.tables || [];
    setComboOptions($('db-table'), tables);
    $('db-table').placeholder = tables.length ? '选择或输入表名…' : '该项目暂无表'; // 加载结果如实反映（原静态「选定项目后自动载入」在 0 表时说谎）
    $('db-tbl-fb').textContent = ''; // 成功静默（R8 F1：「正在获取…」悬挂=状态说谎）；db-out 保留原文输出
    dbOut(j.out || `\\dt：共 ${tables.length} 张表`);
  } catch (e) { $('db-tbl-fb').textContent = ''; apiFail('表清单获取', e); }
}

async function onDbPreview() {
  const t = dbTarget();
  if (!t) return;
  const table = $('db-table').value.trim();
  if (!table) return showMsg('请先填写或选择表名');
  const limit = Math.min(parseInt($('db-limit').value, 10) || 100, 200);
  $('db-tbl-fb').textContent = '正在预览…';
  busy($('btn-db-preview'), true, '预览中…'); // B3：请求期间按钮就地「预览中…」并禁用
  try {
    const j = await api('POST', '/_api/db-preview', { project: t.project, table, limit });
    const cols = j.columns || [], rows = j.rows || [];
    $('db-preview-wrap').classList.remove('hidden');
    $('db-preview-tbl').innerHTML =
      `<thead><tr>${cols.map((c) => `<th>${esc(c)}</th>`).join('')}</tr></thead><tbody>` +
      (rows.length ? rows.map((r) => `<tr>${r.map((v) => `<td class="wrap mono">${esc(v ?? 'NULL')}</td>`).join('')}</tr>`).join('')
        : `<tr><td colspan="${cols.length || 1}" class="note">（无数据）</td></tr>`) + '</tbody>';
    $('db-tbl-fb').textContent = `表 ${table}：${rows.length} 行（上限 ${limit}）`;
  } catch (e) {
    $('db-tbl-fb').textContent = '';
    $('db-preview-wrap').classList.add('hidden'); // B5：预览失败清旧表（不残留上一次的数据误导）
    $('db-preview-tbl').innerHTML = '';
    apiFail('表预览', e);
  } finally { busy($('btn-db-preview'), false); }
}

const DANGEROUS_SQL = /\b(drop|truncate|delete)\b/i;
async function onDbSql() {
  const t = dbTarget();
  if (!t) return;
  const sql = $('db-sql').value;
  if (!sql.trim()) return showMsg('请输入要执行的 SQL 语句');
  let confirmFlag = false;
  if (DANGEROUS_SQL.test(sql)) {
    if (!confirm('检测到 DROP / TRUNCATE / DELETE 等危险语句，确认要在数据库上执行吗？')) return;
    confirmFlag = true;
  }
  busy($('btn-db-sql'), true, '执行中…');
  dbOut('正在执行…');
  try {
    const j = await api('POST', '/_api/db-sql', { project: t.project, sql, confirm: confirmFlag });
    dbOut(j.out ?? '（无输出）');
    $('db-fb').textContent = '执行完成';
  } catch (e) { $('db-fb').textContent = ''; dbOut('执行失败（原文见顶部提示）'); apiFail('执行 SQL', e); } // B5：失败态写进数据容器
  busy($('btn-db-sql'), false);
}

// —— 备份下载：一次性 RSA-3072 对 → 公钥上行 → wasm 解信封 → 下载 ——
/** 生成一次性 RSA-3072 密钥对（优先 WebCrypto；非安全上下文（如局域网 HTTP）回落 wasm 垫片）。 */
async function oneTimeRsaPemPair() {
  if (crypto.subtle) {
    try {
      const kp = await crypto.subtle.generateKey(
        { name: 'RSA-OAEP', modulusLength: 3072, publicExponent: [1, 0, 1], hash: 'SHA-256' },
        true, ['encrypt', 'decrypt']); // extractable：需导出 PEM 交 wasm 解封
      const spki = new Uint8Array(await crypto.subtle.exportKey('spki', kp.publicKey));
      const pkcs8 = new Uint8Array(await crypto.subtle.exportKey('pkcs8', kp.privateKey));
      return { pub: derToPem(spki, 'PUBLIC KEY'), priv: derToPem(pkcs8, 'PRIVATE KEY') };
    } catch { /* 落到 wasm 垫片路径 */ }
  }
  const sid = ex.sr_keygen_rsa();
  if (sid <= 0) throw { error: 'E_NET', msg: `下载准备失败（${sid}）` };
  return {
    pub: shimCStr((p, c) => ex.sr_rsa_pub_pem(sid, p, c), 1024),    // SPKI
    priv: shimCStr((p, c) => ex.sr_rsa_priv_pem(sid, p, c), 4096),  // PKCS#8
  };
}

/** 信封流式解封会话（§2）：sr_open_begin + push（分片可任意边界，内部按段缓冲）+ flush + end。
 *  返回 {push(bytes)→明文分段|null, end()}；每段输出立即拷贝脱离 wasm 内存。 */
function openSession(privPem) {
  const [pp, pl] = put(enc.encode(privPem));
  const sid = ex.sr_open_begin(pp, pl);
  unput(pp, pl);
  if (sid <= 0) throw { error: 'E_NET', msg: `解封会话创建失败（${sid}）：私钥 PEM 无效` };
  return {
    push(bytes) {
      const [ip, il] = bytes.length ? put(bytes) : [0, 0];
      let cap = bytes.length + SEG + 64, ob = alloc(cap);
      let w = ex.sr_open_push(sid, ip, il, ob, cap);
      if (w < 0 && w <= -40) { unput(ob, cap); cap = -w; ob = alloc(cap); w = ex.sr_open_push(sid, ip, il, ob, cap); }
      const out = w > 0 ? new Uint8Array(mem().subarray(ob, ob + w)) : null; // 立即拷贝脱离 wasm 内存
      unput(ip, il); unput(ob, cap);
      if (w < 0) throw { error: 'E_NET', msg: `解封失败（${w}）：数据损坏或私钥不匹配` };
      return out;
    },
    end() { ex.sr_open_end(sid); },
  };
}

// —— 备份下载（§10.4 三段式）：begin → status 轮询 → 4MB chunk 边下边解密 → Blob 下载 ——
let dumpBusy = false;
function dbDumpProgress(cur, total, txt) {
  $('db-prog').max = total || 1;
  $('db-prog').value = Math.min(cur, total || 1);
  $('db-prog-txt').textContent = txt;
}

async function onDbDump() {
  const t = dbTarget();
  if (!t) return;
  if (dumpBusy) return showMsg('已有备份任务进行中，请等待完成');
  dumpBusy = true;
  busy($('btn-db-dump'), true, '备份中…');
  dbDumpProgress(0, 1, '正在准备下载…');
  try {
    const pair = await oneTimeRsaPemPair();
    dbDumpProgress(0, 1, '已发起备份（服务端后台 pg_dump 加密导出）…');
    // ① begin：仅公钥上行，立即返回 dump_id
    const j = await api('POST', '/_api/db-dump-begin', { project: t.project, client_pub_pem: pair.pub });
    const dumpId = j.dump_id;
    // ② 轮询 status：1s 间隔，5 分钟上限（failed 即时报错）
    const t0 = Date.now();
    let total = 0, srvOut = '';
    for (;;) {
      const st = await api('GET', `/_api/db-dump-status?dump=${encodeURIComponent(dumpId)}`);
      if (st.error) throw { error: 'E_STATE', msg: `服务端备份失败：${st.error}` };
      if (st.ready) { total = Number(st.total) || 0; srvOut = st.out || ''; break; }
      if (Date.now() - t0 > 5 * 60 * 1000) throw { error: 'E_STATE', msg: '备份超时（5 分钟未就绪），请稍后重试' };
      dbDumpProgress(0, 1, `服务端正在导出（已等待 ${Math.round((Date.now() - t0) / 1000)} 秒）…`);
      await sleep(1000);
    }
    // ③ chunk 循环：分片字节一到即推入解封会话（内存占用与分片大小成正比，明文分段累积为 Blob 源）
    const opener = openSession(pair.priv);
    const parts = [];
    let offset = 0;
    try {
      while (offset < total) {
        const len = Math.min(DUMP_CHUNK, total - offset);
        const resp = await apiDo('GET', `/_api/db-dump-chunk?dump=${encodeURIComponent(dumpId)}&offset=${offset}&len=${len}`, null);
        if (!resp.ok) throw await respErr(resp);
        const hTotal = Number(resp.headers?.get?.('X-Sr-Total'));
        if (Number.isFinite(hTotal) && hTotal !== total) throw { error: 'E_STATE', msg: `下载总长不一致（响应头 ${hTotal} ≠ 状态 ${total}）` };
        const buf = new Uint8Array(await resp.arrayBuffer());
        if (!buf.length || offset + buf.length > total) throw { error: 'E_STATE', msg: `下载数据长度异常（offset=${offset}，收到 ${buf.length} / 总 ${total}）` };
        const out = opener.push(buf);
        if (out) parts.push(out);
        offset += buf.length;
        dbDumpProgress(offset, total, `下载中：${offset} / ${total} 字节`);
      }
      const tail = opener.push(new Uint8Array(0)); // len=0 = 流结束：解出尾段
      if (tail) parts.push(tail);
    } finally { opener.end(); }
    const plain = parts.reduce((n, p) => n + p.length, 0);
    const blob = new Blob(parts, { type: 'application/octet-stream' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `${t.project}.dump`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 60000);
    // C10/F8：完成双通道分工——进度条只报文件名；技术明细归 db-out（服务端原文优先，无则本地核算）
    dbDumpProgress(total, total, `完成：已下载 ${t.project}.dump`);
    dbOut(srvOut || `pg_dump -Fc 三段式下载完成：信封 ${total} 字节（${Math.max(1, Math.ceil(total / DUMP_CHUNK))} 片 × ≤4MB）→ 本地解密 ${plain} 字节。`);
  } catch (e) {
    $('db-fb').textContent = '';
    dbDumpProgress(0, 1, '备份失败或中断（可重新发起）');
    apiFail('备份下载', e);
  } finally { dumpBusy = false; busy($('btn-db-dump'), false); }
}

async function onDbRestore() {
  const t = dbTarget();
  if (!t) return;
  const file = $('db-restore-file').files[0];
  if (!file) return showMsg('请先选择要恢复的 .dump 文件（「备份下载」的产物）');
  if (!pemReady()) { openSettings(true); return showMsg('尚未完成安全配置：到「设置」完成（已为你打开设置）'); }
  const pem = platformPem();
  if (!confirm(`确认将 ${file.name}（${file.size} 字节）恢复到 ${t.project}？当前数据将被替换。`)) return;
  busy($('btn-db-restore'), true, '恢复中…');
  $('db-fb').textContent = '正在压缩并加密文件…';
  try {
    // 魔数分支：已是压缩格式（备份下载产物即服务端压缩包裹的 -Fc）直接上传；
    // 原生 -Fc / 其他未压缩形态先压缩——服务端按魔数自适应解包，两态都正确。
    const { blob: payload, zstd: pre } = await uploadPayload(file, (done, raw) => {
      $('db-fb').textContent = `压缩中 ${done} / ${raw} 字节…`;
    });
    const sid = sealBegin(pem, crypto.getRandomValues(new Uint8Array(32)));
    const parts = [];
    const CHUNK2 = 4 * 1024 * 1024;
    for (let o = 0; o < payload.size; o += CHUNK2) {
      const bytes = new Uint8Array(await payload.slice(o, Math.min(o + CHUNK2, payload.size)).arrayBuffer());
      const [w, ob, cap] = pushSlice(sid, bytes);
      parts.push(new Uint8Array(mem().subarray(ob, ob + w))); // 立即拷贝脱离 wasm 内存
      unput(ob, cap);
    }
    { const [w, ob, cap] = pushFlush(sid); parts.push(new Uint8Array(mem().subarray(ob, ob + w))); unput(ob, cap); }
    ex.sr_seal_end(sid);
    const envelope = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    { let off = 0; for (const p of parts) { envelope.set(p, off); off += p.length; } }
    $('db-fb').textContent = '正在上传恢复（服务端解密并 pg_restore）…';
    const j = await api('POST',
      `/_api/db-restore?project=${encodeURIComponent(t.project)}&confirm=true`, envelope);
    $('db-fb').textContent = '恢复完成';
    dbOut(j.out || `pg_restore --clean --if-exists 完成：信封 ${envelope.length} 字节（${pre ? `文件无需压缩（${payload.size} 字节），直接上传` : `压缩后 ${payload.size} / 原文件 ${file.size} 字节`}）。`);
  } catch (e) { $('db-fb').textContent = ''; apiFail('恢复上传', e); }
  busy($('btn-db-restore'), false);
}

async function onDbReset() {
  const t = dbTarget();
  if (!t) return;
  if (!confirm(`确认重置 ${t.project} 的数据库？全部表与数据将被删除并重建（连接凭据不变），不可恢复。`)) return;
  busy($('btn-db-reset'), true, '重置中…');
  dbOut('正在重置…');
  try {
    const j = await api('POST', '/_api/db-reset', { project: t.project, confirm: true });
    $('db-fb').textContent = '重置完成';
    dbOut(j.out || '数据库已删除并重建，凭据保持不变。');
  } catch (e) { $('db-fb').textContent = ''; dbOut('执行失败（原文见顶部提示）'); apiFail('重置数据库', e); } // B5：失败态写进数据容器
  busy($('btn-db-reset'), false);
}

// —— Redis 浏览（SPEC §13.2/13.3：DB 下拉仅列自己的（admin 全量）；错误一律走顶部条）——
// B13：rdDbsLoaded 旗标删除——事实源 = #rd-db 有无 option（空列表/换号清空 innerHTML 即未加载态，
// 下次进入自动重取；成功填充 option 即已加载）。
const rdHasDbs = () => ($('rd-db').innerHTML || '').includes('<option');

export async function ensureRedisDbs() {
  if (rdHasDbs()) return;
  try {
    const j = await api('GET', '/_api/redis-dbs'); // 服务端已按用户过滤（§13.2：请求 db ∈ 名下项目集合）
    const entries = (j.entries ?? []).filter((e) => Number.isInteger(e.db));
    if (!entries.length) {
      $('rd-db').innerHTML = '';
      $('rd-count').textContent = '您名下暂无可浏览的项目（发布项目后自动出现）';
      return;
    }
    // R8 用户裁决：下拉显示项目名而非库号（value 仍为库号供查询）；管理员未分配库显示占位
    $('rd-db').innerHTML = entries.map((e) =>
      `<option value="${e.db}">${esc(e.project ?? '未分配')}</option>`).join('');
  } catch (e) { apiFail('Redis 项目列表获取', e); }
}

async function refreshRedisKeys() {
  await ensureRedisDbs();
  if (!rdHasDbs()) return;
  const db = $('rd-db').value;
  const q = $('rd-q').value.trim();
  $('rd-keys').innerHTML = '<option value="">加载中…</option>';
  // B5：换库/重查即清旧值（值面板不残留上一轮选中键的内容）
  $('rd-meta').textContent = '（点击左侧键查看类型 / 剩余生存时间 / 值）';
  $('rd-val').textContent = '（未选择键）';
  try {
    const j = await api('GET', `/_api/redis-keys?db=${encodeURIComponent(db)}&q=${encodeURIComponent(q)}&limit=200`);
    const keys = j.keys || [];
    $('rd-keys').innerHTML = keys.length
      ? keys.map((k) => `<option value="${esc(k)}">${esc(k)}</option>`).join('')
      : '<option value="">（无匹配键）</option>';
    $('rd-count').textContent = `共 ${keys.length} 个键`; // 「可能截断」只在上方静态说明保留一次
  } catch (e) {
    // B5：失败态写进数据容器（键列表占位；顶部条保留原文）
    $('rd-keys').innerHTML = '<option value="">（加载失败，将自动重试）</option>';
    $('rd-count').textContent = '';
    apiFail('Redis 键列表获取', e);
    retryOnce('redis-keys', () => refreshRedisKeys());
  }
}

async function onRedisKeyPick() {
  const key = $('rd-keys').value;
  if (!key) return;
  const db = $('rd-db').value;
  $('rd-meta').textContent = `${key}：读取中…`;
  try {
    const j = await api('GET', `/_api/redis-get?db=${encodeURIComponent(db)}&key=${encodeURIComponent(key)}`);
    const ttl = Number(j.ttl_ms ?? j.ttl); // 服务端字段 ttl_ms（PTTL 毫秒；-1=永久）
    const ttlCn = Number.isFinite(ttl) ? (ttl === -1 ? '永久（-1）' : `${ttl} ms`) : '未知';
    let v = j.value;
    if (Array.isArray(v)) v = v.join('\n');
    else if (v != null && typeof v !== 'string') v = JSON.stringify(v, null, 2);
    else v = v ?? '';
    const cut = v.length >= 4096; // 服务端按 4KB 截断：达到阈值即提示
    $('rd-meta').textContent = `${key} · 类型 ${j.type ?? '未知'} · 剩余生存 ${ttlCn}${cut ? ' · 值已截断（≥4KB）' : ''}`;
    $('rd-val').textContent = v === '' ? '（空值）' : v;
  } catch (e) { $('rd-meta').textContent = ''; apiFail('Redis 值读取', e); }
}
