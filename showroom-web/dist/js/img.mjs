// —— img.mjs：镜像库清单/删除 + 空间条 + 加密信封分片上传（seal/zstd 压缩/断点续传）——
// apiFail 随其磁盘 E_QUOTA 分支的依赖 refreshSpace 就近放置（全部模块均可 import 本层）。
// upBusy 归 proj.mjs（deploy 流私有旗标）；onResumeUpload 入口亦在 proj.mjs。
import { $, enc, esc, showMsg, errText, Buffer_fromHex, setComboOptions } from './util.mjs';
import { SEG, WRAP, NONCE, TAG, CHUNK, dataFresh, dataMark, dataInvalidate, retryOnce, platformPem } from './state.mjs';
import { ex, mem, alloc, put, unput, api, signHeaderPtr, checkResp } from './api.mjs';
export const apiFail = (what, e) => {
  // §14.2/§14.3：磁盘空间不足的 E_QUOTA（msg 含「磁盘空间不足」）→ 顶部条追加删除引导并刷新空间条（内存类 E_QUOTA 不追加）
  if (e && e.error === 'E_QUOTA' && (e.msg || '').includes('磁盘空间不足')) {
    if ($('space-note')) refreshSpace(true);
    return showMsg(`${what} 失败 — ${errText(e)}；可到镜像库删除旧镜像后重试`); // 第2轮xiaozhi P3：what 与「失败」间补空格（CJK/西文混排间距）
  }
  showMsg(`${what} 失败 — ${errText(e)}`);
};

// —— §13.1 镜像清单（ComboBox 数据源）：进入发布区惰性加载；force=true 强制重取 ——
// B13：imagesLoaded 旗标删除——TTL 缓存（dataMark/dataFresh）已是单源（dataAt>0 即曾成功加载）
export async function ensureImages(force = false) {
  if (!force && dataFresh('images')) return; // plan-17 §3：TTL 300s
  $('dep-images-fb').textContent = '正在加载镜像清单…'; // 镜像库下拉（发布页）反馈
  try {
    const j = await api('GET', '/_api/images');
    const images = (j.images || []).map(String);
    setComboOptions($('dep-image'), images);
    renderImageList(images); // §14.3：清单容器（删除按钮挂此）与 ComboBox 同源同刷
    dataMark('images');
    // B4 徽章已随 R4 用户裁决移除（管理员身份由顶栏「管」角标表达，同一信息只出现一次）
    $('img-sub').textContent = ''; // 成功不写「已加载 N 项」计数（清单即数据；自动刷新兜底，无手动重取入口）
    $('dep-images-fb').textContent = '';
  } catch (e) {
    // B5：失败态写进数据容器（tbody 占位；同一信息只出现一次）
    const tb = $('img-list')?.querySelector?.('tbody');
    if (tb) tb.innerHTML = '<tr><td colspan="2" class="note">加载失败（将自动重试）</td></tr>';
    $('dep-images-fb').textContent = '镜像清单加载失败，可切到镜像库页查看';
    retryOnce('images', () => ensureImages(true)); // §3：3s 自动重试 1 次
  }
}
/** §14.3 镜像清单渲染：发布 ComboBox 同源数据；每行尾「删除」按钮（POST /_api/image-rm，内联 GC 秒级回收）。 */
function renderImageList(images) {
  const tb = $('img-list').querySelector('tbody');
  tb.innerHTML = images.length
    ? images.map((s) => `<tr><td class="mono wrap">${esc(s)}</td><td><button class="act danger" data-act="image-rm" data-img="${esc(s)}">删除</button></td></tr>`).join('')
    : '<tr><td colspan="2" class="note">（暂无镜像；到「发布」页上传后此处可管理）</td></tr>';
}

/** §14.3 删除镜像：confirm 写明镜像名与回滚失效语义 → image-rm → alert 回收量 → 刷新清单+空间条。 */
export async function onImageRm(ev) {
  const btn = ev.target.closest('button[data-act="image-rm"]');
  if (!btn || btn.disabled) return;
  const image = btn.dataset.img;
  if (!confirm(`确定删除镜像 ${image} 吗？正在使用该镜像的已发布项目不受影响（容器继续运行），但回滚/重建将失效。`)) return;
  btn.disabled = true;
  try {
    const j = await api('POST', '/_api/image-rm', { image });
    showMsg(`已回收约 ${Number(j.freed_est_mb ?? 0)} MB`, 'ok'); // 原 alert 改顶部条（统一反馈通道）
    dataInvalidate('images', 'space'); // plan-17 §3：写操作强制失效
    ensureImages(true); // 清单重取：被删 tag 即刻消失
    refreshSpace(true);     // 空间数字同步刷新
  } catch (e) { apiFail(`删除镜像 ${image}`, e); }
  btn.disabled = false;
}

// —— §14.1 空间条：GET /_api/space → 「可用空间：X GB」（GB 一位小数）；失败静默降级 ——
// 第1轮UI审查P1：空间条双落点——镜像库页 #space-note 与发布页 #pub-space-note（tar 上传发生在发布页，须就地可见）
export async function refreshSpace(force = false) {
  if (!force && dataFresh('space')) return; // plan-17 §3：TTL 60s
  let txt;
  try {
    const j = await api('GET', '/_api/space');
    const freeMb = Number(j.available_mb) - Number(j.reserve_mb); // 显示口径＝可用−预留（SPEC §14.1）
    if (!Number.isFinite(freeMb)) throw new Error('bad space payload');
    txt = `可用空间：${(freeMb / 1024).toFixed(1)} GB`;
    dataMark('space');
  } catch { txt = '空间信息不可用'; } // 静默降级：不上顶部条、不阻断上传表单
  for (const id of ['space-note', 'pub-space-note']) { const el = $(id); if (el) el.textContent = txt; }
}

// —— §8.1 分片上传 + §2 信封（保留进度/重试/续传）——
export const sealedTotal = (n) => WRAP + n + Math.ceil(n / SEG) * (NONCE + TAG); // 信封总长（用于 import-begin total_bytes）

export function upProgress(sent, total) {
  $('up-prog').max = total; $('up-prog').value = sent;
  const pct = total ? ((sent / total) * 100).toFixed(1) : 0;
  $('up-status').textContent = `已上传 ${sent} / ${total} 字节（${pct}%）`;
}
export function latestUpId() {
  const ids = Object.keys(localStorage).filter((k) => k.startsWith('sr.up.')).map((k) => k.slice(6));
  return ids.length ? ids[ids.length - 1] : null; // localStorage 键序=记录序，最后即最近一次中断
}
export function refreshResumeUi() {
  // R5b：无中断会话→整个续传区不显示（按钮不可点=错误不存在）；有→仅按钮，编号自动取最近
  const fs = document.getElementById('fs-resume');
  if (fs) fs.classList.toggle('hidden', !latestUpId());
}

export function sealBegin(pem, seedBytes) {
  const [pemp, peml] = put(enc.encode(pem));
  const [ssp, ssl] = put(seedBytes);
  const sid = ex.sr_seal_begin_seed(pemp, peml, ssp);
  unput(pemp, peml); unput(ssp, ssl);
  if (sid <= 0) throw { error: 'E_NET', msg: `加密会话创建失败（${sid}）：请检查平台公钥 PEM` };
  return sid;
}

export function pushSlice(sid, bytes) { // 返回 [written, outPtr, outCap]（调用方负责 sr_free）
  const [ip, il] = put(bytes);
  const cap = WRAP + NONCE + TAG + bytes.length + (Math.ceil(bytes.length / SEG) + 2) * (NONCE + TAG) + 64;
  const ob = alloc(cap);
  const w = ex.sr_seal_push(sid, ip, il, ob, cap);
  unput(ip, il);
  if (w < 0) { unput(ob, cap); throw { error: 'E_NET', msg: `本地加密失败（${w}）` }; }
  return [w, ob, cap];
}
export function pushFlush(sid) { // 流结束（len=0）：写出尾段；返回 [written, outPtr, outCap]
  const cap = WRAP + SEG + NONCE + TAG + 64;
  const ob = alloc(cap);
  const w = ex.sr_seal_push(sid, 0, 0, ob, cap);
  if (w < 0) { unput(ob, cap); throw { error: 'E_NET', msg: `加密收尾失败（${w}）` }; }
  return [w, ob, cap];
}

async function postChunk(uploadId, seq, ptr, len, attempt = 1) {
  const pathQ = `/_api/import-chunk?upload=${encodeURIComponent(uploadId)}&seq=${seq}`;
  // 签名必须覆盖上传的确切字节：body 直读 wasm 内存 [ptr, ptr+len)
  const auth = signHeaderPtr('POST', pathQ, ptr, len);
  const body = len ? new Uint8Array(mem().buffer, ptr, len).slice() : new Uint8Array(0); // 拷贝脱离 wasm 内存（防后续增长失联）
  try {
    const resp = await fetch(pathQ, { method: 'POST', headers: { Authorization: auth, 'Content-Type': 'application/octet-stream' }, body });
    return await checkResp(resp);
  } catch (e) {
    if (attempt < 3 && (!e.error || e.error === 'E_NET')) { // 网络类失败重试（服务端 seq 连续性保证不双计）
      await new Promise((r) => setTimeout(r, 1000 * attempt));
      return postChunk(uploadId, seq, ptr, len, attempt + 1);
    }
    throw e;
  }
}

// —— 上传压缩（全链路 zstd 统一）：先全量压缩成 Blob（两遍法第一遍），再从压缩 Blob 分片 seal ——
export const ZSTD_LEVEL = 19;
const isZstdHead = (b) => b && b.length >= 4 && b[0] === 0x28 && b[1] === 0xb5 && b[2] === 0x2f && b[3] === 0xfd;
function zPush(sid, bytes) { // 返回 [written, outPtr, outCap]（独立完整 zstd 帧；调用方负责 sr_free）
  const [ip, il] = put(bytes);
  const cap = bytes.length + (bytes.length >> 3) + 4096; // ≥ 垫片输出上界 len+len/256+64 的安全余量
  const ob = alloc(cap);
  const w = ex.sr_zstd_push(sid, ip, il, ob, cap);
  unput(ip, il);
  if (w < 0) { unput(ob, cap); throw { error: 'E_NET', msg: `本地压缩失败（${w}）` }; }
  return [w, ob, cap];
}
/** 全量压缩为 Blob（两遍法第一遍）：逐 8MB 片压成独立完整 zstd 帧（服务端流式解码跨帧续解）。
 *  压缩产物确定性（同输入 + 同 level ⇒ 逐字节相同）是断点续传重放的前提；每片输出立即
 *  拷贝脱离 wasm 内存后交给 Blob（浏览器 Blob 由磁盘句柄支撑，不占 JS 堆——大文件压缩
 *  阶段内存曲线仍平坦）。 */
async function compressToBlob(file, note) {
  const sid = ex.sr_zstd_begin(ZSTD_LEVEL);
  if (sid <= 0) throw { error: 'E_NET', msg: `压缩会话创建失败（${sid}）` };
  const parts = [];
  try {
    for (let o = 0; o < file.size; o += CHUNK) {
      const end = Math.min(o + CHUNK, file.size);
      const bytes = new Uint8Array(await file.slice(o, end).arrayBuffer());
      const [w, ob, cap] = zPush(sid, bytes);
      if (w > 0) parts.push(new Uint8Array(mem().subarray(ob, ob + w)));
      unput(ob, cap);
      if (note) note(end, file.size);
    }
  } finally { ex.sr_zstd_end(sid); }
  return new Blob(parts);
}

/** 上传载荷统一入口：文件头已是 zstd 魔数 → 原样（避免二次压缩）；否则压缩后上传。
 *  服务端按魔数自适应解包，两种形态都正确。返回 { blob, zstd }。 */
export async function uploadPayload(file, note) {
  const head = new Uint8Array(await file.slice(0, 4).arrayBuffer());
  if (isZstdHead(head)) return { blob: file, zstd: true };
  return { blob: await compressToBlob(file, note), zstd: false };
}

/** 从 startSeq 继续上传（fresh 与 resume 共用；已 push 过的分片由调用方先行重放）。
 *  blob 为压缩产物 Blob（或已是 zstd 的原文件——Blob/File 分片读法一致）；note 进度回调（缺省走 upProgress）。 */
export async function uploadRest(uploadId, sid, blob, startSeq, sent0, total, note) {
  const slices = Math.ceil(blob.size / CHUNK) || 1;
  let sent = sent0;
  for (let i = startSeq; i < slices; i++) {
    const part = blob.slice(i * CHUNK, Math.min((i + 1) * CHUNK, blob.size));
    const bytes = new Uint8Array(await part.arrayBuffer());
    const [w, ob, cap] = pushSlice(sid, bytes);
    try { await postChunk(uploadId, i, ob, w); } finally { unput(ob, cap); }
    sent += w; (note || upProgress)(sent, total);
  }
  { // flush（len=0 尾段）——也作为一个分片上传（0 字节片合法，保证 seq 与 push 一一对应）
    const [w, ob, cap] = pushFlush(sid);
    try { await postChunk(uploadId, slices, ob, w); } finally { unput(ob, cap); }
    sent += w; (note || upProgress)(sent, total);
  }
  if (sent !== total) throw { error: 'E_STATE', msg: `信封总长校验失败：本地 ${sent} ≠ 预计 ${total}（不上传收尾，请重试）` };
  ex.sr_seal_end(sid);
  await api('POST', `/_api/import-end?upload=${encodeURIComponent(uploadId)}`, null);
  localStorage.removeItem('sr.up.' + uploadId);
  refreshResumeUi();
  dataInvalidate('space'); refreshSpace(true); // §14.1：import-end 成功后刷新空间条
}

/** 断点续传核心（手动「恢复上传」与发布重试共用，§8.1）：查已收分片 → 重新压缩重建产物
 *  （压缩确定性 ⇒ 信封逐字节一致）→ 重放已收分片推进加密会话 → 补传剩余 → import-end 收尾。
 *  C19：续传文件由调用方传入（发布重试传 deploy.file；手动恢复传发布表单当前选择）。
 *  opts：compNote 压缩进度 / onRec(rec) 已收片数提示 / upNote 上传进度。返回 { pre, zsize }。 */
export async function resumeUploadCore(id, file, opts = {}) {
  const saved = JSON.parse(localStorage.getItem('sr.up.' + id) || 'null');
  if (!saved) throw { error: 'E_STATE', msg: '本地无该会话记录（浏览器数据被清则无法续传，请重新发起）' };
  if (!file || file.size !== saved.size) throw { error: 'E_STATE', msg: `续传需在发布表单选择同一文件（${saved.name}，${saved.size} 字节）` };
  const pem = platformPem();
  if (!pem.startsWith('-----BEGIN PUBLIC KEY-----')) throw { error: 'E_STATE', msg: '尚未完成安全配置：到「设置」完成' };
  const st = await api('GET', `/_api/import-status?upload=${encodeURIComponent(id)}`);
  const rec = (st.received || []).length;
  opts.onRec?.(rec);
  // 重建压缩产物（确定性 ⇒ 与原会话逐字节相同）；旧会话记录（无 zsize）按原样直传兼容
  const { blob: zb, zstd: pre } = await uploadPayload(file, opts.compNote);
  if (saved.zsize !== undefined && saved.zsize !== zb.size) {
    throw { error: 'E_STATE', msg: `压缩产物长度与上次不符（${zb.size} ≠ ${saved.zsize}），请确认选择的是同一文件` };
  }
  const total = sealedTotal(zb.size);
  const slices = Math.ceil(zb.size / CHUNK) || 1;
  const sid = sealBegin(pem, Buffer_fromHex(saved.seed));
  let sent = 0;
  for (let i = 0; i < Math.min(rec, slices); i++) { // 重放已收分片（同 seed+同产物 ⇒ 密文一致，只推进会话状态不再上传）
    const part = zb.slice(i * CHUNK, Math.min((i + 1) * CHUNK, zb.size));
    const bytes = new Uint8Array(await part.arrayBuffer());
    const [w, ob, cap] = pushSlice(sid, bytes);
    sent += w; unput(ob, cap);
  }
  (opts.upNote || upProgress)(sent, total);
  const flushBytes = (zb.size % SEG) ? (zb.size % SEG) + NONCE + TAG : 0; // 尾段输出量（重放计数公式）
  if (rec < slices) { // 还有数据片未收：从断点补传（uploadRest 内部完成收尾与会话清理）
    await uploadRest(id, sid, zb, rec, sent, total, opts.upNote);
    return { pre, zsize: zb.size };
  }
  if (rec === slices) { // 数据片齐，仅缺收尾片
    const [w, ob, cap] = pushFlush(sid);
    try { await postChunk(id, slices, ob, w); } finally { unput(ob, cap); }
    sent += w;
  } else sent += flushBytes; // rec === slices+1：收尾片也已收到，只补计数
  if (sent !== total) throw { error: 'E_STATE', msg: `重放长度不符（${sent}/${total}），请重新发起` };
  ex.sr_seal_end(sid);
  await api('POST', `/_api/import-end?upload=${encodeURIComponent(id)}`, null);
  localStorage.removeItem('sr.up.' + id);
  refreshResumeUi();
  dataInvalidate('space'); refreshSpace(true); // §14.1：import-end 成功后刷新空间条
  return { pre, zsize: zb.size };
}
