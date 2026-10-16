// —— auth.ts：OPAQUE 客户端（登录/改密）+ 密钥无感生成 + 60s 心跳（移植自 dist/js/auth.mjs）——
import { Buffer_fromHex, enc, toHex } from "@/lib/bytes";
import { api, checkResp, signHeaderPtr } from "@/lib/api";
import { ex, mem, put, readCStr, shimCStr, unput } from "@/lib/wasm";
import { state } from "@/lib/state";
import { loginSessionKey, setCreds, srtpTeardown, verifyKeyMac } from "@/lib/srtp";

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
// ABI：b64 串进出；输出 NUL 结尾、容量不足 -(所需) ≤ -40；finish 一次性消费会话（无 end 清理步）；密码错误 = -7。
// 登录/注册两对报文同构，参数化垫片导出收敛为一套实现。

type StartFn = (pp: number, pl: number, up: number, ul: number) => number;
type MsgFn = (sid: number, p: number, c: number) => number;
type FinishFn = (sid: number, pp: number, pl: number, rp: number, rl: number, out: number, cap: number) => number;

function shimStart(startFn: StartFn, msgFn: MsgFn, what: string, user: string, pass: string): { sid: number; msg: string } {
  const [pp, pl] = put(enc.encode(pass)), [up, ul] = put(enc.encode(user));
  let sid: number;
  try {
    sid = startFn(pp, pl, up, ul);
  } finally { unput(pp, pl); unput(up, ul); }
  if (sid <= 0) throw { error: "E_NET", msg: `${what} 失败（${sid}；-6=熵未注入）` };
  const msg = shimCStr((p, c) => msgFn(sid, p, c), 2048);
  return { sid, msg };
}

/** 返回输出串；失败抛错（err.code=垫片返回码，登录侧据此把 -7 译为「密码错误」）。 */
function shimFinish(finishFn: FinishFn, what: string, sid: number, pass: string, resp: string, cap: number): string {
  const [pp, pl] = put(enc.encode(pass)), [rp, rl] = put(enc.encode(resp));
  const out = (ex.sr_alloc as (n: number) => number)(cap);
  const r = finishFn(sid, pp, pl, rp, rl, out, cap);
  unput(pp, pl); unput(rp, rl);
  const msg = r > 0 ? readCStr(out, r) : null;
  unput(out, cap);
  if (!msg) throw { error: "E_NET", code: r, msg: `${what} 失败（${r}）` };
  return msg;
}

export function opaqueStart(user: string, pass: string): { sid: number; ke1: string } {
  const r = shimStart(ex.sr_login_start as StartFn, ex.sr_login_ke1 as MsgFn, "login_start", user, pass);
  return { sid: r.sid, ke1: r.msg };
}

export function opaqueFinish(sid: number, pass: string, ke2: string): string {
  try {
    return shimFinish(ex.sr_login_finish as FinishFn, "login_finish", sid, pass, ke2, 2048);
  } catch (e) {
    if ((e as { code?: number }).code === -7) throw { error: "E_PW_LOCAL", msg: "" };
    throw e;
  }
}

// —— OPAQUE 注册适配（改密码：m1 → m2 → record）——
export function opaqueRegStart(user: string, pass: string): { sid: number; m1: string } {
  const r = shimStart(ex.sr_reg_start as StartFn, ex.sr_reg_msg as MsgFn, "reg_start", user, pass);
  return { sid: r.sid, m1: r.msg };
}

export function opaqueRegFinish(sid: number, pass: string, m2: string): string {
  return shimFinish(ex.sr_reg_finish as FinishFn, "reg_finish", sid, pass, m2, 4096);
}

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
  const r = (await postAuthJson("/_api/auth/login-begin", { user, ke1: st.ke1 })) as { ke2?: string };
  // finish 一次性消费会话（垫片自弃），无需显式 end
  const ke3 = opaqueFinish(st.sid, pass, r.ke2 as string);
  const sessionKey = loginSessionKey(st.sid); // AKE 输出：验证 SRTP 密钥下发绑定（plan-18 §3.3）
  const r2 = (await postAuthJson("/_api/auth/login-finish", { user, ke3 })) as {
    bind_token: string;
    srtp_id?: string;
    srtp_pub?: string;
    key_mac?: string;
  };
  // SRTP 密钥下发：三项齐备 → 验签（失败=fail-closed 抛错，不降级）；旧服务端无字段=跳过
  srtpTeardown();
  if (r2.srtp_id && r2.srtp_pub && r2.key_mac) {
    const id = verifyKeyMac(sessionKey, r2.srtp_id, r2.srtp_pub, r2.key_mac);
    setCreds(id, r2.srtp_pub);
  }
  const kb = (await apiBind(user, state.pub, r2.bind_token)) as { kid?: string; admin?: boolean };
  state.kid = kb.kid || "";
  state.boundAdmin = !!kb.admin;
  localStorage.setItem("sr.user", user);
}

let heartbeatTimer: ReturnType<typeof setInterval> | null = null;

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
    } catch (e) {
      // 仅钥匙失效（E_AUTH）销毁本机会话；网络闪断留给 30s 轮询自愈
      if ((e as { error?: string })?.error !== "E_AUTH") return;
      if (heartbeatTimer) clearInterval(heartbeatTimer);
      heartbeatTimer = null;
      localStorage.removeItem("sr.seed");
      localStorage.removeItem("sr.user");
      onAuthLost();
    }
  }, 60000);
}
