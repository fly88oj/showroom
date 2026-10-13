// —— api.ts：SR-SIG1 签名请求（移植自 dist/js/api.mjs，协议路径零改动）——
import { Buffer_fromHex, enc, nonceHex } from "@/lib/bytes";
import { state } from "@/lib/state";
import { ex, mem, put, unput, shimCStr } from "@/lib/wasm";

export interface ApiError { error: string; msg: string }

/** 对 wasm 内存中 [ptr, ptr+len) 的 body 签名（分片上传零拷贝路径）。 */
export function signHeaderPtr(method: string, pathQ: string, bodyPtr: number, bodyLen: number): string {
  const nonce = nonceHex();
  const ts = BigInt(Math.floor(Date.now() / 1000));
  const [sp, spl] = put(Buffer_fromHex(state.seed as string));
  const [op, ol] = put(enc.encode(state.user));
  const [mp, ml] = put(enc.encode(method));
  const [pp, pl] = put(enc.encode(pathQ));
  const [np, nl] = put(enc.encode(nonce));
  const auth = shimCStr((out, cap) => ex.sr_sign_header(sp, op, ol, mp, ml, pp, pl, ts, np, nl, bodyPtr, bodyLen, out, cap), 640);
  for (const [p, l] of [[sp, spl], [op, ol], [mp, ml], [pp, pl], [np, nl]] as [number, number][]) unput(p, l);
  return auth;
}

/** 统一 API 调用：json 传对象 → JSON body；Uint8Array → 原始 body；null → 空 body。signal 供流式 GET 中止。 */
export async function apiDo(method: string, pathQ: string, body: unknown, signal?: AbortSignal): Promise<Response> {
  if (!state.seed || !state.user) throw { error: "E_STATE", msg: "尚未登录" };
  let bodyBytes: Uint8Array<ArrayBufferLike> = new Uint8Array(0), isJson = false;
  if (body && typeof body === "object" && !(body instanceof Uint8Array)) {
    bodyBytes = enc.encode(JSON.stringify(body));
    isJson = true;
  } else if (body instanceof Uint8Array) {
    bodyBytes = body;
  }
  const [bp, bl] = put(bodyBytes);
  const auth = signHeaderPtr(method, pathQ, bp, bl);
  unput(bp, bl);
  return fetch(pathQ, {
    method,
    headers: { Authorization: auth, ...(isJson ? { "Content-Type": "application/json" } : {}) },
    body: method === "GET" || method === "HEAD" ? undefined : (bodyBytes as BodyInit),
    signal,
  });
}

export async function api(method: string, pathQ: string, body: unknown = null): Promise<Record<string, unknown>> {
  return checkResp(await apiDo(method, pathQ, body));
}

export async function checkResp(resp: Response): Promise<Record<string, unknown>> {
  const text = await resp.text();
  let j: Record<string, unknown> | null = null;
  try { j = JSON.parse(text); } catch { /* 非 JSON */ }
  if (resp.ok) return (j ?? {}) as Record<string, unknown>;
  throw {
    error: (j?.error as string) || "E_NET",
    msg: (j?.msg as string) || `HTTP ${resp.status}`,
  };
}

/** 非 JSON 响应（octet-stream 分片等）的错误解析：统一 {error,msg} 抛出。 */
export async function respErr(resp: Response): Promise<ApiError> {
  let j: Record<string, unknown> | null = null;
  try { j = JSON.parse(await resp.text()); } catch { /* 非 JSON */ }
  return {
    error: (j?.error as string) || "E_NET",
    msg: (j?.msg as string) || `HTTP ${resp.status}`,
  };
}

/** §10.4 ndjson 流式消费：行缓冲跨 chunk 拼行、逐行 JSON 回调（非法行静默丢弃）。 */
export async function consumeNdjson(url: string, onLine: (j: Record<string, unknown>) => void, signal?: AbortSignal): Promise<void> {
  const resp = await apiDo("GET", url, null, signal);
  if (!resp.ok) throw await respErr(resp);
  const reader = resp.body!.getReader();
  const td = new TextDecoder();
  let buf = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += td.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (line.trim()) {
        try { onLine(JSON.parse(line)); } catch { /* 非法行丢弃 */ }
      }
    }
  }
  buf += td.decode();
  if (buf.trim()) {
    try { onLine(JSON.parse(buf)); } catch { /* 同上 */ }
  }
}

export { mem };
