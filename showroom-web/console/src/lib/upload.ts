// —— upload.ts：分片上传编排（import-begin → 逐片签名 POST → import-end；断点续传重放）——
// 信封/压缩原语在 lib/envelope.ts（本模块只做传输与续传协议）。
import { toHex, Buffer_fromHex } from "@/lib/bytes";
import { api, checkResp, signHeaderPtr } from "@/lib/api";
import { mem, unput } from "@/lib/wasm";
import { dataInvalidate, pemReady, platformPem } from "@/lib/state";
import {
  pushSlice, pushFlush, sealBegin, sealEnd, sealedTotal, uploadPayload,
} from "@/lib/envelope";

export { sealedTotal, uploadPayload };

async function postChunk(uploadId: string, seq: number, ptr: number, len: number, attempt = 1): Promise<Record<string, unknown>> {
  const pathQ = `/_api/import-chunk?upload=${encodeURIComponent(uploadId)}&seq=${seq}`;
  // 签名必须覆盖上传的确切字节：body 直读 wasm 内存 [ptr, ptr+len)
  const auth = signHeaderPtr("POST", pathQ, ptr, len);
  const body = len ? new Uint8Array(mem().buffer, ptr, len).slice() : new Uint8Array(0);
  try {
    const resp = await fetch(pathQ, {
      method: "POST",
      headers: { Authorization: auth, "Content-Type": "application/octet-stream" },
      body: body as BodyInit,
    });
    return await checkResp(resp);
  } catch (e) {
    if (attempt < 3 && (!e || !(e as { error?: string }).error || (e as { error?: string }).error === "E_NET")) {
      await new Promise((r) => setTimeout(r, 1000 * attempt));
      return postChunk(uploadId, seq, ptr, len, attempt + 1);
    }
    throw e;
  }
}

export interface UpRecord { seed: string; project: string; tag: string; size: number; zsize: number; name: string }

export function saveUpSession(uploadId: string, rec: UpRecord) {
  localStorage.setItem("sr.up." + uploadId, JSON.stringify(rec));
}
export function loadUpSession(id: string): UpRecord | null {
  try { return JSON.parse(localStorage.getItem("sr.up." + id) || "null"); } catch { return null; }
}
export function latestUpId(): string | null {
  const ids = Object.keys(localStorage).filter((k) => k.startsWith("sr.up.")).map((k) => k.slice(6));
  return ids.length ? ids[ids.length - 1] : null;
}

/** 从 startSeq 继续上传；note(sent,total) 进度回调。完成即 import-end + 会话清理 + 空间失效。 */
export async function uploadRest(
  uploadId: string, sid: number, blob: Blob, startSeq: number,
  sent0: number, total: number, note?: (sent: number, total: number) => void,
  onSpaceInvalidate?: () => void,
): Promise<void> {
  const CH = 8 * 1024 * 1024;
  const slices = Math.ceil(blob.size / CH) || 1;
  let sent = sent0;
  for (let i = startSeq; i < slices; i++) {
    const part = blob.slice(i * CH, Math.min((i + 1) * CH, blob.size));
    const bytes = new Uint8Array(await part.arrayBuffer());
    const [w, ob, cap] = pushSlice(sid, bytes);
    try { await postChunk(uploadId, i, ob, w); } finally { unput(ob, cap); }
    sent += w;
    note?.(sent, total);
  }
  { // flush（len=0 尾段）也作为一个分片（seq 与 push 一一对应）
    const [w, ob, cap] = pushFlush(sid);
    try { await postChunk(uploadId, slices, ob, w); } finally { unput(ob, cap); }
    sent += w;
    note?.(sent, total);
  }
  if (sent !== total) throw { error: "E_STATE", msg: `信封总长校验失败：本地 ${sent} ≠ 预计 ${total}（不上传收尾，请重试）` };
  sealEnd(sid);
  await api("POST", `/_api/import-end?upload=${encodeURIComponent(uploadId)}`, null);
  localStorage.removeItem("sr.up." + uploadId);
  dataInvalidate("space");
  onSpaceInvalidate?.();
}

/** 全新上传序列（发布②段 / 镜像库入库共用）：begin（tag 服务端分配）→ 登记会话 → 逐片 → 收尾。 */
export async function freshUpload(
  project: string, file: File, zb: Blob,
  note?: (sent: number, total: number) => void,
  onSpaceInvalidate?: () => void,
): Promise<{ uploadId: string; tag: string }> {
  if (!pemReady()) throw { error: "E_STATE", msg: "尚未完成安全配置：到「设置」完成" };
  const pem = platformPem();
  const total = sealedTotal(zb.size);
  const sessSeed = new Uint8Array(32);
  crypto.getRandomValues(sessSeed);
  let uploadId: string, tag: string;
  try {
    const j = (await api("POST", "/_api/import-begin", { project, image_id: file.name, total_bytes: total })) as { upload_id: string; tag?: string };
    uploadId = String(j.upload_id);
    tag = String(j.tag || "");
  } catch (e) {
    const err = e as { error?: string; msg?: string };
    if (err.error === "E_QUOTA") err.msg += `（信封总长 ${total} 字节）`;
    throw e;
  }
  saveUpSession(uploadId, { seed: toHex(sessSeed), project, tag, size: file.size, zsize: zb.size, name: file.name });
  const sid = sealBegin(pem, sessSeed);
  await uploadRest(uploadId, sid, zb, 0, 0, total, note, onSpaceInvalidate);
  return { uploadId, tag };
}

/** 断点续传核心：查已收分片 → 重新压缩重建产物（确定性 ⇒ 一致）→ 重放已收 → 补传 → import-end。 */
export async function resumeUploadCore(
  id: string, file: File,
  opts: { compNote?: (d: number, r: number) => void; onRec?: (n: number) => void; upNote?: (s: number, t: number) => void; onSpaceInvalidate?: () => void } = {},
): Promise<{ zstd: boolean; zsize: number }> {
  const saved = loadUpSession(id);
  if (!saved) throw { error: "E_STATE", msg: "本地无该会话记录（浏览器数据被清则无法续传，请重新发起）" };
  if (!file || file.size !== saved.size) {
    throw { error: "E_STATE", msg: `续传需选择同一文件（${saved.name}，${saved.size} 字节）` };
  }
  if (!pemReady()) throw { error: "E_STATE", msg: "尚未完成安全配置：到「设置」完成" };
  const pem = platformPem();
  const st = (await api("GET", `/_api/import-status?upload=${encodeURIComponent(id)}`)) as { received?: unknown[] };
  const rec = (st.received || []).length;
  opts.onRec?.(rec);
  const { blob: zb, zstd: pre } = await uploadPayload(file, opts.compNote);
  if (saved.zsize !== undefined && saved.zsize !== zb.size) {
    throw { error: "E_STATE", msg: `压缩产物长度与上次不符（${zb.size} ≠ ${saved.zsize}），请确认选择的是同一文件` };
  }
  const total = sealedTotal(zb.size);
  const CH = 8 * 1024 * 1024;
  const slices = Math.ceil(zb.size / CH) || 1;
  const sid = sealBegin(pem, Buffer_fromHex(saved.seed));
  let sent = 0;
  for (let i = 0; i < Math.min(rec, slices); i++) { // 重放已收分片（同 seed+同产物 ⇒ 密文一致，只推进会话状态）
    const part = zb.slice(i * CH, Math.min((i + 1) * CH, zb.size));
    const bytes = new Uint8Array(await part.arrayBuffer());
    const [w, ob, cap] = pushSlice(sid, bytes);
    sent += w;
    unput(ob, cap);
  }
  opts.upNote?.(sent, total);
  const flushBytes = (zb.size % (4 * 1024 * 1024)) ? (zb.size % (4 * 1024 * 1024)) + 12 + 16 : 0;
  if (rec < slices) {
    await uploadRest(id, sid, zb, rec, sent, total, opts.upNote, opts.onSpaceInvalidate);
    return { zstd: pre, zsize: zb.size };
  }
  if (rec === slices) { // 数据片齐，仅缺收尾片
    const [w, ob, cap] = pushFlush(sid);
    try { await postChunk(id, slices, ob, w); } finally { unput(ob, cap); }
    sent += w;
  } else {
    sent += flushBytes; // 收尾片也已收到，只补计数
  }
  if (sent !== total) throw { error: "E_STATE", msg: `重放长度不符（${sent}/${total}），请重新发起` };
  sealEnd(sid);
  await api("POST", `/_api/import-end?upload=${encodeURIComponent(id)}`, null);
  localStorage.removeItem("sr.up." + id);
  dataInvalidate("space");
  opts.onSpaceInvalidate?.();
  return { zstd: pre, zsize: zb.size };
}
