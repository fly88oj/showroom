// —— auth.ts：OPAQUE 客户端（登录/改密）+ 密钥无感生成 + 60s 心跳（移植自 dist/js/auth.mjs）——
import { Buffer_fromHex, bytesToB64, dec, enc, nonceHex, toHex } from "@/lib/bytes";
import { api, checkResp, signHeaderPtr } from "@/lib/api";
import { ex, mem, put, shimCStr, unput, OPAQUE_SHIM, REG_SHIM } from "@/lib/wasm";
import { state } from "@/lib/state";

function loadSeed() {
  const hex = localStorage.getItem("sr.seed");
  state.seed = /^[0-9a-fA-F]{64}$/.test(hex || "") ? hex : null;
  state.pub = "";
  if (state.seed) {
    const [sp, spl] = put(Buffer_fromHex(state.seed));
    state.pub = shimCStr((p, c) => (ex.sr_pub_string as (a: number, b: number, c: number) => number)(sp, p, c), 128);
    unput(sp, spl);
  }
}

// 密钥无感管理（plan-16 §3）：无 sr.seed 即自动生成（登录前也一样），用户全程不可见。
export function ensureSeed() {
  loadSeed();
  if (state.seed) return;
  const sp = (ex.sr_alloc as (n: number) => number)(32);
  try {
    if ((ex.sr_generate as (p: number) => number)(sp) !== 0) {
      throw { error: "E_NET", msg: "本机登录组件初始化失败，请刷新重试" };
    }
    localStorage.setItem("sr.seed", toHex(mem().slice(sp, sp + 32)));
  } finally {
    unput(sp, 32);
  }
  loadSeed();
}

// —— OPAQUE 客户端适配层（plan-16 §2.2 报文序列 / §6.1 垫片导出）——
// 真分支 ABI：b64 串进出；输出 NUL 结尾、容量不足 -(所需) ≤ -40；finish 一次性消费会话；密码错误 = -7。
// mock 兜底层仅在垫片探测失败时降级（正常永不触发）。

const MOCK_FP = (s: string) => {
  let h = 0x811c9dc5;
  for (const b of enc.encode(s)) { h ^= b; h = Math.imul(h, 0x01000193) >>> 0; }
  return h.toString(16).padStart(8, "0");
};
const MOCK_B64 = (o: unknown) => bytesToB64(enc.encode(JSON.stringify(o)));
function MOCK_PARSE(b64: string): Record<string, unknown> | null {
  try { return JSON.parse(dec.decode(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)))); } catch { return null; }
}
let mockSidSeq = 0;
const MOCK_SESS = new Map<number, { user: string; r: string }>();

export function opaqueStart(user: string, pass: string): { sid: number; ke1: string } {
  if (OPAQUE_SHIM()) {
    const [pp, pl] = put(enc.encode(pass)), [up, ul] = put(enc.encode(user));
    let sid: number;
    try {
      sid = (ex.sr_login_start as (a: number, b: number, c: number, d: number) => number)(pp, pl, up, ul);
    } finally { unput(pp, pl); unput(up, ul); }
    if (sid <= 0) throw { error: "E_NET", msg: `login_start 失败（${sid}；-6=熵未注入）` };
    const ke1 = shimCStr((p, c) => (ex.sr_login_ke1 as (a: number, b: number, c: number) => number)(sid, p, c), 2048);
    return { sid, ke1 };
  }
  const sid = ++mockSidSeq;
  MOCK_SESS.set(sid, { user, r: MOCK_FP(`showroom-opaque-mock|${user}|${pass}`) });
  return { sid, ke1: MOCK_B64({ v: 1, m: "ke1", u: user, n: nonceHex() }) };
}

export function opaqueFinish(sid: number, pass: string, ke2: string): string {
  if (OPAQUE_SHIM()) {
    const [pp, pl] = put(enc.encode(pass)), [kp, kl] = put(enc.encode(ke2));
    const cap = 2048, out = (ex.sr_alloc as (n: number) => number)(cap);
    const r = (ex.sr_login_finish as (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => number)(sid, pp, pl, kp, kl, out, cap);
    unput(pp, pl); unput(kp, kl);
    const ke3 = r > 0 ? (require_readCStr())(out, r) : null;
    unput(out, cap);
    if (r === -7) throw { error: "E_PW_LOCAL", msg: "" };
    if (!ke3) throw { error: "E_NET", msg: `login_finish 失败（${r}）` };
    return ke3;
  }
  const st = MOCK_SESS.get(sid), j = MOCK_PARSE(ke2);
  if (!st || !j || j.m !== "ke2" || j.mac !== MOCK_FP(`mac|${j.n}|${st.r}`)) throw { error: "E_PW_LOCAL", msg: "" };
  return MOCK_B64({ v: 1, m: "ke3", mac: j.mac });
}

// readCStr 延迟取用（wasm.ts 的 mem 视图需在调用后重取；直接 import readCStr 亦可，此处保持同构）
function require_readCStr() {
  return (p: number, written: number) => {
    const m = mem();
    let e = p;
    while (e < p + written && m[e] !== 0) e++;
    return dec.decode(m.subarray(p, e));
  };
}

export function opaqueEnd(sid: number) { MOCK_SESS.delete(sid); }

// —— OPAQUE 注册适配（改密码：m1 → m2 → record）——
export function opaqueRegStart(user: string, pass: string): { sid: number; m1: string } {
  if (REG_SHIM()) {
    const [pp, pl] = put(enc.encode(pass)), [up, ul] = put(enc.encode(user));
    let sid: number;
    try {
      sid = (ex.sr_reg_start as (a: number, b: number, c: number, d: number) => number)(pp, pl, up, ul);
    } finally { unput(pp, pl); unput(up, ul); }
    if (sid <= 0) throw { error: "E_NET", msg: `reg_start 失败（${sid}；-6=熵未注入）` };
    const m1 = shimCStr((p, c) => (ex.sr_reg_msg as (a: number, b: number, c: number) => number)(sid, p, c), 2048);
    return { sid, m1 };
  }
  const sid = ++mockSidSeq;
  MOCK_SESS.set(sid, { user, r: MOCK_FP(`showroom-opaque-mock|${user}|${pass}`) });
  return { sid, m1: MOCK_B64({ v: 1, m: "m1", u: user, n: nonceHex() }) };
}

export function opaqueRegFinish(sid: number, pass: string, m2: string): string {
  if (REG_SHIM()) {
    const [pp, pl] = put(enc.encode(pass)), [mp, ml] = put(enc.encode(m2));
    const cap = 4096, out = (ex.sr_alloc as (n: number) => number)(cap);
    const r = (ex.sr_reg_finish as (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => number)(sid, pp, pl, mp, ml, out, cap);
    unput(pp, pl); unput(mp, ml);
    const record = r > 0 ? require_readCStr()(out, r) : null;
    unput(out, cap);
    if (!record) throw { error: "E_NET", msg: `reg_finish 失败（${r}）` };
    return record;
  }
  const st = MOCK_SESS.get(sid), j = MOCK_PARSE(m2);
  if (!st || !j || j.m !== "m2") throw { error: "E_NET", msg: "" };
  return MOCK_B64({ v: 1, m: "record", u: st.user, mac: MOCK_FP(`rec|${j.n}|${st.r}`) });
}

export function opaqueRegEnd(sid: number) { MOCK_SESS.delete(sid); }

// —— 一步登录（plan-16 §2.2/§3）：login-begin → 本地 finish → login-finish → key-bind → 进入 ——
export async function postAuthJson(path: string, body: unknown) {
  const resp = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return checkResp(resp);
}

/** key-bind：候选钥匙签名（服务端按 body 内公钥验签而非已登记钥匙）+ 一次性 bind_token。 */
export async function apiBind(user: string, pubkey: string, bindToken: string) {
  const body = enc.encode(JSON.stringify({ user, pubkey }));
  const [bp, bl] = put(body);
  const auth = signHeaderPtr("POST", "/_api/auth/key-bind", bp, bl);
  unput(bp, bl);
  const resp = await fetch("/_api/auth/key-bind", {
    method: "POST",
    headers: { Authorization: auth, "X-Sr-Bind": bindToken, "Content-Type": "application/json" },
    body,
  });
  return checkResp(resp);
}

/** 密码登录全程：密钥生成/绑定无感；密码只在本地参与 OPAQUE 盲化，永不明文上线。 */
export async function passwordLogin(user: string, pass: string) {
  ensureSeed();
  state.user = user;
  state.boundAdmin = false;
  if (!user) throw { error: "E_STATE", msg: "账号为空，请重新输入" };
  const st = opaqueStart(user, pass);
  let r: { ke2?: string };
  try {
    r = (await postAuthJson("/_api/auth/login-begin", { user, ke1: st.ke1 })) as { ke2?: string };
  } catch (e) {
    opaqueEnd(st.sid);
    throw e;
  }
  let ke3: string;
  try {
    ke3 = opaqueFinish(st.sid, pass, r.ke2 as string);
  } finally {
    opaqueEnd(st.sid);
  }
  const r2 = (await postAuthJson("/_api/auth/login-finish", { user, ke3 })) as { bind_token: string };
  const kb = (await apiBind(user, state.pub, r2.bind_token)) as { kid?: string; admin?: boolean };
  state.kid = kb.kid || "";
  state.boundAdmin = !!kb.admin;
  localStorage.setItem("sr.user", user);
}

export let heartbeatTimer: ReturnType<typeof setInterval> | null = null;

export function stopHeartbeat() {
  if (heartbeatTimer) clearInterval(heartbeatTimer);
  heartbeatTimer = null;
}

/** E_AUTH 时回调（踢回登录窗）；60s whoami touch 续期。 */
export function startHeartbeat(onAuthLost: () => void) {
  if (heartbeatTimer) clearInterval(heartbeatTimer);
  heartbeatTimer = setInterval(async () => {
    try {
      await api("GET", "/_api/whoami");
    } catch {
      if (heartbeatTimer) clearInterval(heartbeatTimer);
      heartbeatTimer = null;
      localStorage.removeItem("sr.seed");
      localStorage.removeItem("sr.user");
      onAuthLost();
    }
  }, 60000);
}
