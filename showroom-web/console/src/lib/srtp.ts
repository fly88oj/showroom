// —— srtp.ts：浏览器侧 SRTP 一级通道（plan-18：WS 载波 + 两级自动回退）——
// 激活时序：登录族恒走明文（OPAQUE 为敌意信道设计）；login-finish 下发
// (srtp_id, per-user pub, key_mac)，客户端以 AKE 会话密钥验签后建立 WS——
// 此后 api() 全走一级。key_mac 失败=fail-closed（不降级：降级正是 MITM 所愿）。
// WS 失败/旧服务端 → 页面会话内粘性二级（原 fetch 路径，零改动）。
import { Buffer_fromHex, enc, toHex } from "@/lib/bytes";
import { ex, mem, put, unput } from "@/lib/wasm";

const SUB_PROTOCOL = "showroom-srtp-v1";
const CHALLENGE_TIMEOUT_MS = 3000;
/** 单请求超时（GUI 实测教训：一次响应丢失即永久挂起整条顺序队列——必须有超时出口） */
const REQUEST_TIMEOUT_MS = 30000;

export interface SrtpCreds { id: Uint8Array; pubPem: string }

let creds: SrtpCreds | null = null;
let tier1 = false;          // 一级已验证可用（粘性）
let tier2Sticky = false;    // 一级不可用（页面会话内粘性）
let ws: WebSocket | null = null;
let wsReady: Promise<WebSocket> | null = null;

export function srtpActive(): boolean { return tier1; }

/** 登录成功后注入凭据（key_mac 已验——见 verifyKeyMac）并预备通道。 */
export function setCreds(id: Uint8Array, pubPem: string) {
  creds = { id, pubPem };
}

/** AKE 会话密钥取回（32B；垫片会话槽，finish 后可读）。 */
export function loginSessionKey(sid: number): Uint8Array {
  const fn = ex.sr_login_session_key as (a: number, b: number, c: number) => number;
  let cap = 32, out = (ex.sr_alloc as (n: number) => number)(cap);
  let r = fn(sid, out, cap);
  if (r < 0 && r <= -40) { unput(out, cap); cap = -r; out = (ex.sr_alloc as (n: number) => number)(cap); r = fn(sid, out, cap); }
  if (r <= 0) { unput(out, cap); throw { error: "E_NET", msg: `会话密钥取回失败（${r}）` }; }
  const k = new Uint8Array(mem().subarray(out, out + r));
  unput(out, cap);
  return k;
}

/** 验证服务端下发的 key_mac（失败=链路篡改，fail-closed）。 */
export function verifyKeyMac(sk: Uint8Array, idHex: string, pubPem: string, macHex: string): Uint8Array {
  const id = Buffer_fromHex(idHex);
  if (id.length !== 8) throw { error: "E_NET", msg: "srtp_id 形状非法" };
  const msg = new Uint8Array(11 + 8 + pubPem.length);
  msg.set(enc.encode("srtp-key-v1"), 0);
  msg.set(id, 11);
  msg.set(enc.encode(pubPem), 19);
  const [kp, kl] = put(sk), [dp, dl] = put(msg);
  const out = (ex.sr_alloc as (n: number) => number)(32);
  const r = (ex.sr_hmac_sha256 as (a: number, b: number, c: number, d: number, e: number, f: number) => number)(
    kp, kl, dp, dl, out, 32,
  );
  unput(kp, kl); unput(dp, dl);
  if (r !== 32) { unput(out, 32); throw { error: "E_NET", msg: `HMAC 计算失败（${r}）` }; }
  const want = toHex(new Uint8Array(mem().subarray(out, out + 32)));
  unput(out, 32);
  if (want !== macHex) {
    throw { error: "E_NET", msg: "检测到传输链路篡改（SRTP 密钥下发校验失败），已中止。请检查网络或联系管理员" };
  }
  return id;
}

// —— wasm 会话薄封装（容量协商 ≤-40 协议）——

let wasmSid = 0;

/** begin 返回 sid；wrap 经 out 缓冲取回。垫片 ABI：返回 sid>0，wrap 写入 out。 */
function srtpBeginFull(): { sid: number; wrap: Uint8Array } {
  const [pp, pl] = put(enc.encode(creds!.pubPem));
  let cap = 384, out = (ex.sr_alloc as (n: number) => number)(cap);
  let sid = (ex.sr_srtp_begin as (a: number, b: number, c: number, d: number) => number)(pp, pl, out, cap);
  if (sid < 0 && sid <= -40) { unput(out, cap); cap = -sid; out = (ex.sr_alloc as (n: number) => number)(cap); sid = (ex.sr_srtp_begin as (a: number, b: number, c: number, d: number) => number)(pp, pl, out, cap); }
  unput(pp, pl);
  if (sid <= 0) { unput(out, cap); throw { error: "E_NET", msg: `SRTP 会话创建失败（${sid}）` }; }
  const wrap = new Uint8Array(mem().subarray(out, out + 384));
  unput(out, cap);
  return { sid, wrap };
}

function srtpSeal(sid: number, plain: Uint8Array): Uint8Array {
  const [ip, il] = put(plain);
  let cap = plain.length + 64, out = (ex.sr_alloc as (n: number) => number)(cap);
  let r = (ex.sr_srtp_seal as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, out, cap);
  if (r < 0 && r <= -40) { unput(out, cap); cap = -r; out = (ex.sr_alloc as (n: number) => number)(cap); r = (ex.sr_srtp_seal as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, out, cap); }
  unput(ip, il);
  if (r < 0) { unput(out, cap); throw { error: "E_NET", msg: `SRTP 封装失败（${r}）` }; }
  const wire = new Uint8Array(mem().subarray(out, out + r));
  unput(out, cap);
  return wire;
}

function srtpOpen(sid: number, wire: Uint8Array): Uint8Array {
  const [ip, il] = put(wire);
  let cap = wire.length + 64, out = (ex.sr_alloc as (n: number) => number)(cap);
  let r = (ex.sr_srtp_open as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, out, cap);
  if (r < 0 && r <= -40) { unput(out, cap); cap = -r; out = (ex.sr_alloc as (n: number) => number)(cap); r = (ex.sr_srtp_open as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, out, cap); }
  unput(ip, il);
  if (r < 0) { unput(out, cap); throw { error: "E_NET", msg: `SRTP 解封失败（${r}）` }; }
  const plain = new Uint8Array(mem().subarray(out, out + r));
  unput(out, cap);
  return plain;
}

// —— WS 通道 ——

interface Pending { resolve: (b: Uint8Array) => void; reject: (e: unknown) => void }

let inflight: Pending | null = null;   // 顺序一比一（与 serve 会话模型对齐）
let downBuf: Uint8Array[] = [];
let downLen = 0;

async function connect(): Promise<WebSocket> {
  if (ws && ws.readyState === WebSocket.OPEN) return ws;
  if (wsReady) return wsReady;
  wsReady = (async () => {
    if (!creds) throw new Error("no creds");
    const { sid, wrap } = srtpBeginFull();
    wasmSid = sid;
    const proto = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/_srtp`;
    const sock = new WebSocket(proto, SUB_PROTOCOL);
    sock.binaryType = "arraybuffer";
    const opened = await new Promise<void>((res, rej) => {
      const t = setTimeout(() => rej(new Error("ws timeout")), CHALLENGE_TIMEOUT_MS);
      sock.onopen = () => { clearTimeout(t); res(); };
      sock.onerror = () => { clearTimeout(t); rej(new Error("ws error")); };
      sock.onclose = () => { clearTimeout(t); rej(new Error("ws closed")); };
    });
    opened;
    // msg0：id + wrap
    const m0 = new Uint8Array(8 + wrap.length);
    m0.set(creds.id, 0);
    m0.set(wrap, 8);
    sock.send(m0);
    sock.onmessage = (ev) => {
      const b = new Uint8Array(ev.data as ArrayBuffer);
      downBuf.push(b);
      downLen += b.length;
      // 尝试完整响应判定：累积明文 → HTTP 响应按 Content-Length 判完
      tryFlush();
    };
    sock.onclose = () => {
      const p = inflight;
      inflight = null;
      ws = null;
      wsReady = null;
      p?.reject({ error: "E_NET", msg: "SRTP 通道已断开" });
    };
    sock.onerror = () => { /* onclose 兜底收束 */ };
    ws = sock;
    return sock;
  })();
  try {
    return await wsReady;
  } catch (e) {
    wsReady = null;
    throw e;
  }
}

function tryFlush() {
  if (!inflight || !wasmSid || downBuf.length === 0) return;
  const wire = downBuf.length === 1 ? downBuf[0] : (() => {
    const all = new Uint8Array(downLen);
    let o = 0;
    for (const b of downBuf) { all.set(b, o); o += b.length; }
    return all;
  })();
  downBuf = [];
  downLen = 0;
  let plain: Uint8Array;
  try {
    plain = srtpOpen(wasmSid, wire);
  } catch (e) {
    inflight?.reject(e);
    inflight = null;
    return;
  }
  // HTTP 响应完结判定：Content-Length 满足或连接级单响应（serve 每 WS 消息=一完整响应）
  inflight.resolve(plain);
  inflight = null;
}

// —— 请求队列（顺序一比一，与 serve 会话模型对齐）——
// 登录后四路并发首拉（projects/images/space/whoami）曾以单 inflight 槽竞态错位
// （A 的响应被 B 的 promise 吃掉、A 永挂——真机实弹）。串行化后每响应必达其主。
let queueTail: Promise<unknown> = Promise.resolve();

function enqueue<T>(job: () => Promise<T>): Promise<T> {
  const p = queueTail.then(job, job);
  queueTail = p.catch(() => { /* 前序失败不堵队 */ });
  return p;
}

/** 一级请求：HTTP/1.1 字节（调用方构造，含 SR-SIG1 头）→ 明文响应字节。 */
export function srtpRequest(httpBytes: Uint8Array): Promise<Uint8Array> {
  return enqueue(() => srtpRequestOne(httpBytes));
}

async function srtpRequestOne(httpBytes: Uint8Array): Promise<Uint8Array> {
  const sock = await connect();
  return new Promise<Uint8Array>((resolve, reject) => {
    // 超时出口：到点未收到响应 → 拒绝并清理 inflight（调用方走 srtpTry 的降级路径）
    const timer = setTimeout(() => {
      if (inflight) {
        inflight.reject({ error: "E_NET", msg: "SRTP 通道响应超时" });
        inflight = null;
      }
    }, REQUEST_TIMEOUT_MS);
    const onDone = () => clearTimeout(timer);
    const innerResolve = resolve;
    const innerReject = reject;
    resolve = (b) => { onDone(); innerResolve(b); };
    reject = (e) => { onDone(); innerReject(e); };
    inflight = { resolve, reject };
    // 应用帧 [8B 长度][HTTP] → 记录（64KiB 分片）
    const framed = new Uint8Array(8 + httpBytes.length);
    new DataView(framed.buffer).setBigUint64(0, BigInt(httpBytes.length));
    framed.set(httpBytes, 8);
    for (let o = 0; o < framed.length; o += 65536) {
      const piece = srtpSeal(wasmSid, framed.subarray(o, Math.min(o + 65536, framed.length)));
      sock.send(piece.slice().buffer as ArrayBuffer);
    }
  });
}

/** 一级尝试（None=不可用→调用方二级）：失败粘性二级。 */
export async function srtpTry(httpBytes: Uint8Array): Promise<Uint8Array | null> {
  if (tier2Sticky || !creds) return null;
  try {
    const resp = await srtpRequest(httpBytes);
    tier1 = true;
    return resp;
  } catch {
    tier2Sticky = true;
    tier1 = false;
    try { ws?.close(); } catch { /* */ }
    ws = null;
    wsReady = null;
    inflight = null;
    downBuf = [];
    downLen = 0;
    return null;
  }
}

export function srtpTeardown() {
  queueTail = Promise.resolve();
  try { if (wasmSid) (ex.sr_srtp_end as (s: number) => number)(wasmSid); } catch { /* */ }
  wasmSid = 0;
  try { ws?.close(); } catch { /* */ }
  ws = null;
  wsReady = null;
  creds = null;
  tier1 = false;
  tier2Sticky = false;
}

// —— HTTP/1.1 序列化/解析（与 CLI srtp_transport 同构；vitest 覆盖三形状）——

export function httpSerialize(method: string, pathQ: string, headers: [string, string][], body: Uint8Array): Uint8Array {
  const host = location.host;
  const head = [
    `${method} ${pathQ} HTTP/1.1`,
    `Host: ${host}`,
    "Connection: close",
    `Content-Length: ${body.length}`,
    ...headers.map(([k, v]) => `${k}: ${v}`),
    "",
    "",
  ].join("\r\n");
  const out = new Uint8Array(enc.encode(head).length + body.length);
  out.set(enc.encode(head), 0);
  out.set(body, enc.encode(head).length);
  return out;
}

export function parseHttpResponse(raw: Uint8Array): { status: number; body: Uint8Array } {
  const dec = new TextDecoder();
  const text = dec.decode(raw);
  const headEnd = text.indexOf("\r\n\r\n");
  if (headEnd < 0) throw { error: "E_NET", msg: "HTTP 响应头不完整" };
  const head = text.slice(0, headEnd);
  const lines = head.split("\r\n");
  const status = parseInt((lines[0].split(" ")[1] || "0"), 10) || 0;
  let cl: number | null = null;
  let chunked = false;
  for (const l of lines.slice(1)) {
    const i = l.indexOf(":");
    if (i < 0) continue;
    const k = l.slice(0, i).trim().toLowerCase();
    const v = l.slice(i + 1).trim();
    if (k === "content-length") cl = parseInt(v, 10);
    if (k === "transfer-encoding" && v.toLowerCase().includes("chunked")) chunked = true;
  }
  const headBytes = enc.encode(head).length + 4;
  let body = raw.subarray(headBytes);
  if (chunked) body = dechunk(body);
  else if (cl != null) body = body.subarray(0, cl);
  return { status, body };
}

function dechunk(b: Uint8Array): Uint8Array {
  const parts: Uint8Array[] = [];
  let o = 0;
  const dec = new TextDecoder();
  while (o < b.length) {
    const le = b.findIndex((v, i) => i >= o && v === 13 && b[i + 1] === 10);
    if (le < 0) break;
    const size = parseInt(dec.decode(b.subarray(o, le)).split(";")[0].trim(), 16);
    o = le + 2;
    if (!size) break;
    parts.push(b.subarray(o, o + size));
    o += size + 2;
  }
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let x = 0;
  for (const p of parts) { out.set(p, x); x += p.length; }
  return out;
}
