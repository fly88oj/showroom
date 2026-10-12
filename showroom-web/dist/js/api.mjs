// —— api.mjs：wasm 垫片装载（ex/mem/alloc/put/shimCStr…）+ OPAQUE 垫片探测 + SR-SIG1 签名请求（api/apiDo/consumeNdjson）——
// wasm 保留在 _console 根：模块位于 js/ 子目录，相对探测路径由 ./ 上移为 ../（其余逐字未变）。
import { enc, dec, nonceHex, Buffer_fromHex } from './util.mjs';
import { state } from './state.mjs';
// —— wasm 加载 ——

export let ex = null;
async function loadWasm() {
  const paths = ['../showroom_bg.wasm', '/_console/showroom_bg.wasm'];
  let lastErr = null;
  for (const p of paths) {
    try { return (await WebAssembly.instantiateStreaming(fetch(p), {})).instance.exports; }
    catch (e) { lastErr = e; try { return (await WebAssembly.instantiate(await (await fetch(p)).arrayBuffer(), {})).instance.exports; } catch (e2) { lastErr = e2; } }
  }
  throw lastErr;
}
ex = await loadWasm();
// wasm 内存可能随分配增长而换代：任何 wasm 调用后都重取 view
export const mem = () => new Uint8Array(ex.memory.buffer);
export const alloc = (n) => { const p = ex.sr_alloc(n); if (!p) throw new Error(`sr_alloc(${n}) 失败`); return p; };
export const put = (b) => { if (!b.length) return [0, 0]; const p = alloc(b.length); mem().set(b, p); return [p, b.length]; }; // 空缓冲：空指针+0 长度（ABI 合法空切片）
export const unput = (p, l) => ex.sr_free(p, l);
export const readCStr = (p, written) => { const m = mem(); let e = p; while (e < p + written && m[e] !== 0) e++; return dec.decode(m.subarray(p, e)); };
/** 垫片「写字符串」类调用的容量自适应封装：容量不足（返回 -(所需) ≤ -40）时扩容重试。 */
export function shimCStr(call, cap0) {
  let cap = cap0, p = alloc(cap), r = call(p, cap);
  while (r < 0 && r <= -40) { unput(p, cap); cap = -r; p = alloc(cap); r = call(p, cap); }
  const s = r > 0 ? readCStr(p, r) : null;
  unput(p, cap);
  if (!s) throw { error: 'E_NET', msg: `垫片调用失败（${r}）` };
  return s;
}
// 熵注入：垫片在 wasm32 无 OS 熵源，随机性全部由 WebCrypto 提供
{
  const b = new Uint8Array(64); crypto.getRandomValues(b);
  const p = ex.sr_alloc(64); mem().set(b, p);
  if (ex.sr_entropy(p, 64) !== 0) throw new Error('sr_entropy 失败');
  ex.sr_free(p, 64);
}

export const OPAQUE_SHIM = typeof ex.sr_login_start === 'function' && typeof ex.sr_login_ke1 === 'function' && typeof ex.sr_login_finish === 'function';

export const REG_SHIM = typeof ex.sr_reg_start === 'function' && typeof ex.sr_reg_msg === 'function' && typeof ex.sr_reg_finish === 'function';

// —— SR-SIG1 签名请求（SPEC §1/§3：所有 /_api 一律签名）——

/** 对 wasm 内存中 [ptr,ptr+len) 的 body 签名（分片上传零拷贝路径）。 */
export function signHeaderPtr(method, pathQ, bodyPtr, bodyLen) {
  const nonce = nonceHex();
  const ts = BigInt(Math.floor(Date.now() / 1000));
  const [sp, spl] = put(Buffer_fromHex(state.seed));
  const [op, ol] = put(enc.encode(state.user)), [mp, ml] = put(enc.encode(method));
  const [pp, pl] = put(enc.encode(pathQ)), [np, nl] = put(enc.encode(nonce));
  const auth = shimCStr((out, cap) =>
    ex.sr_sign_header(sp, op, ol, mp, ml, pp, pl, ts, np, nl, bodyPtr, bodyLen, out, cap), 640);
  for (const [p, l] of [[sp, spl], [op, ol], [mp, ml], [pp, pl], [np, nl]]) unput(p, l);
  return auth;
}

/** 统一 API 调用：json 传对象 → JSON body；Uint8Array → 原始 body；null → 空 body。signal 供流式 GET 中止。 */
export async function apiDo(method, pathQ, body, signal) {
  if (!state.seed || !state.user) throw { error: 'E_STATE', msg: '未配置密钥或账号' };
  let bodyBytes = new Uint8Array(0), isJson = false;
  if (body && typeof body === 'object' && !(body instanceof Uint8Array)) {
    bodyBytes = enc.encode(JSON.stringify(body)); isJson = true;
  } else if (body instanceof Uint8Array) bodyBytes = body;
  const [bp, bl] = put(bodyBytes);
  const auth = signHeaderPtr(method, pathQ, bp, bl);
  unput(bp, bl);
  const resp = await fetch(pathQ, {
    method,
    headers: { Authorization: auth, ...(isJson ? { 'Content-Type': 'application/json' } : {}) },
    body: method === 'GET' || method === 'HEAD' ? undefined : bodyBytes,
    signal,
  });
  return resp;
}
export async function api(method, pathQ, body) {
  return checkResp(await apiDo(method, pathQ, body));
}

export async function checkResp(resp) {
  const text = await resp.text();
  let j = null;
  try { j = JSON.parse(text); } catch { /* 非 JSON */ }
  if (resp.ok) return j ?? {};
  throw { error: j?.error || 'E_NET', msg: j?.msg || `HTTP ${resp.status}` };
}

/** 非 JSON 响应（octet-stream 分片等）的错误解析：统一 {error,msg} 抛出。 */
export async function respErr(resp) {
  let j = null;
  try { j = JSON.parse(await resp.text()); } catch { /* 非 JSON */ }
  return { error: j?.error || 'E_NET', msg: j?.msg || `HTTP ${resp.status}` };
}

/** §10.4 ndjson 流式消费：流式 GET（空 body 签名=空串哈希，与普通签名 GET 相同）；
 *  行缓冲跨 chunk 拼行、逐行 JSON 回调（非法行静默丢弃）；reader 按消费节奏拉取（天然背压）；
 *  signal 中止 → fetch/reader 随之中断并抛 AbortError。 */
export async function consumeNdjson(url, onLine, signal) {
  const resp = await apiDo('GET', url, null, signal);
  if (!resp.ok) throw await respErr(resp);
  const reader = resp.body.getReader();
  const td = new TextDecoder();
  let buf = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += td.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (line.trim()) { try { onLine(JSON.parse(line)); } catch { /* 非法行丢弃 */ } }
    }
  }
  buf += td.decode();
  if (buf.trim()) { try { onLine(JSON.parse(buf)); } catch { /* 同上 */ } }
}
