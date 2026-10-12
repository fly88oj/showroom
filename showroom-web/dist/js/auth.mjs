// —— auth.mjs：OPAQUE 客户端（登录/改密序列）+ 密钥无感生成（ensureSeed）+ passwordLogin + 60s 心跳 ——
// 登录窗 UI 编排（onLoginSubmit/enterApp/bootSelfTest）见 app.mjs（会话生命周期需触达全部页签）。
import { enc, dec, bytesToB64, Buffer_fromHex, toHex, nonceHex } from './util.mjs';
import { state } from './state.mjs';
import { ex, mem, alloc, put, unput, readCStr, shimCStr, OPAQUE_SHIM, REG_SHIM, api, signHeaderPtr, checkResp } from './api.mjs';
function loadSeed() {
  const hex = localStorage.getItem('sr.seed');
  state.seed = /^[0-9a-fA-F]{64}$/.test(hex || '') ? hex : null;
  state.pub = '';
  if (state.seed) {
    const [sp, spl] = put(Buffer_fromHex(state.seed));
    state.pub = shimCStr((p, c) => ex.sr_pub_string(sp, p, c), 128);
    unput(sp, spl);
  }
}
// 密钥指纹 = 公钥串载荷（b64）前 8 位（服务端/CLI 侧展示用；B6 后控制台不再展示）

// —— 密钥无感管理（plan-16 §3）：无 sr.seed 即自动生成（登录前也一样），用户全程不可见 ——
export function ensureSeed() {
  loadSeed();
  if (state.seed) return;
  const sp = alloc(32);
  try {
    if (ex.sr_generate(sp) !== 0) throw { error: 'E_NET', msg: '本机登录组件初始化失败，请刷新重试' };
    localStorage.setItem('sr.seed', toHex(mem().slice(sp, sp + 32)));
  } finally { unput(sp, 32); }
  loadSeed();
}

// —— OPAQUE 客户端适配层（plan-16 §2.2 报文序列 / §6.1 垫片导出）——
// 垫片导出已由服务端侧落入 showroom-common/src/wasm_shim.rs（真实 ABI，下方 real 分支已按其
// 接线）：sr_rwd / sr_login_start(pw,user)→sid / sr_login_ke1(sid)→b64 / sr_login_finish(sid,pw,ke2)→ke3。
// wasm 垫片已含 OPAQUE 导出（sr_login_start 等），OPAQUE_SHIM 探测自动走真分支；
// mock 兜底层仅在探测失败时降级（如 wasm 加载失败），正常永不触发。
// 真分支 ABI 要点（与垫片源码一致）：b64 串进出；输出 NUL 结尾、容量不足 -(所需) ≤ -40；
// finish 一次性消费会话（不做容量探测重试，首给足容量）；密码错误/协议失败 = -7。

const MOCK_FP = (s) => { let h = 0x811c9dc5; for (const b of enc.encode(s)) { h ^= b; h = Math.imul(h, 0x01000193) >>> 0; } return h.toString(16).padStart(8, '0'); };
const MOCK_B64 = (o) => bytesToB64(enc.encode(JSON.stringify(o)));
function MOCK_PARSE(b64) { try { return JSON.parse(dec.decode(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)))); } catch { return null; } }
let mockSidSeq = 0;
const MOCK_SESS = new Map();
/** ① login_start：产出 ke1（真垫片=盲化 OPRF 份额 + 临时份额；mock=形状兼容假报文）。返回 {sid, ke1}。 */
export function opaqueStart(user, pass) {
  if (OPAQUE_SHIM) {
    const [pp, pl] = put(enc.encode(pass)), [up, ul] = put(enc.encode(user));
    let sid;
    try { sid = ex.sr_login_start(pp, pl, up, ul); } // rwd 硬化在垫片内部完成
    finally { unput(pp, pl); unput(up, ul); }
    if (sid <= 0) throw { error: 'E_NET', msg: `login_start 失败（${sid}；-6=熵未注入）` };
    const ke1 = shimCStr((p, c) => ex.sr_login_ke1(sid, p, c), 2048); // 幂等可重复取
    return { sid, ke1 };
  }
  const sid = ++mockSidSeq;
  MOCK_SESS.set(sid, { user, r: MOCK_FP(`showroom-opaque-mock|${user}|${pass}`) });
  return { sid, ke1: MOCK_B64({ v: 1, m: 'ke1', u: user, n: nonceHex() }) };
}
/** ② login_finish：以 ke2 在本地完成密钥交换；密码错误在此抛 E_PW_LOCAL（不产生第三请求）。 */
export function opaqueFinish(sid, pass, ke2) {
  if (OPAQUE_SHIM) {
    const [pp, pl] = put(enc.encode(pass)), [kp, kl] = put(enc.encode(ke2));
    const cap = 2048, out = alloc(cap); // finish 一次性消费会话：首给足容量，不做探测重试
    const r = ex.sr_login_finish(sid, pp, pl, kp, kl, out, cap);
    unput(pp, pl); unput(kp, kl);
    const ke3 = r > 0 ? readCStr(out, r) : null;
    unput(out, cap);
    if (r === -7) throw { error: 'E_PW_LOCAL', msg: '' }; // 密码错误/协议失败：本地判定
    if (!ke3) throw { error: 'E_NET', msg: `login_finish 失败（${r}）` };
    return ke3;
  }
  const st = MOCK_SESS.get(sid), j = MOCK_PARSE(ke2);
  if (!st || !j || j.m !== 'ke2' || j.mac !== MOCK_FP(`mac|${j.n}|${st.r}`)) throw { error: 'E_PW_LOCAL', msg: '' };
  return MOCK_B64({ v: 1, m: 'ke3', mac: j.mac });
}
export function opaqueEnd(sid) { MOCK_SESS.delete(sid); } // 真垫片会话在 finish 时一次性自弃；此处仅清 mock 映射

// —— OPAQUE 注册适配（改密码，plan-16 §2.2 pw 三件序列：m1 → m2 → record）——
// 真垫片导出 sr_reg_start/sr_reg_msg/sr_reg_finish（b64 串进出、finish 一次性消费会话、
// 容量不足 -(所需) ≤ -40）；OPAQUE_SHIM 未就绪时同走「形状兼容 mock」（注册对 m2 不做
// 密码验证——错密码语义只属登录序列，此处任何合法形状 m2 都产出 record）。

/** ① reg_start：新密码 + user → m1（真垫片=盲化 OPRF 注册首条；mock=形状兼容假报文）。返回 {sid, m1}。 */
export function opaqueRegStart(user, pass) {
  if (REG_SHIM) {
    const [pp, pl] = put(enc.encode(pass)), [up, ul] = put(enc.encode(user));
    let sid;
    try { sid = ex.sr_reg_start(pp, pl, up, ul); }
    finally { unput(pp, pl); unput(up, ul); }
    if (sid <= 0) throw { error: 'E_NET', msg: `reg_start 失败（${sid}；-6=熵未注入）` };
    const m1 = shimCStr((p, c) => ex.sr_reg_msg(sid, p, c), 2048); // 幂等可重复取
    return { sid, m1 };
  }
  const sid = ++mockSidSeq;
  MOCK_SESS.set(sid, { user, r: MOCK_FP(`showroom-opaque-mock|${user}|${pass}`) });
  return { sid, m1: MOCK_B64({ v: 1, m: 'm1', u: user, n: nonceHex() }) };
}
/** ② reg_finish：喂 m2 在本地完成注册 → record（b64，pw-commit 上行）。成功后会话一次性自弃。 */
export function opaqueRegFinish(sid, pass, m2) {
  if (REG_SHIM) {
    const [pp, pl] = put(enc.encode(pass)), [mp, ml] = put(enc.encode(m2));
    const cap = 4096, out = alloc(cap); // finish 一次性消费会话：首给足容量，不做探测重试
    const r = ex.sr_reg_finish(sid, pp, pl, mp, ml, out, cap);
    unput(pp, pl); unput(mp, ml);
    const record = r > 0 ? readCStr(out, r) : null;
    unput(out, cap);
    if (!record) throw { error: 'E_NET', msg: `reg_finish 失败（${r}）` };
    return record;
  }
  const st = MOCK_SESS.get(sid), j = MOCK_PARSE(m2);
  if (!st || !j || j.m !== 'm2') throw { error: 'E_NET', msg: '' };
  return MOCK_B64({ v: 1, m: 'record', u: st.user, mac: MOCK_FP(`rec|${j.n}|${st.r}`) });
}
export function opaqueRegEnd(sid) { MOCK_SESS.delete(sid); } // 真垫片会话在 finish 时一次性自弃；此处仅清 mock 映射

// —— 一步登录（plan-16 §2.2/§3）：login-begin → 本地 finish → login-finish → key-bind → 进入 ——
export async function postAuthJson(path, body) { // /_api/auth/*：唯一应答未签名请求的端点族（§2.3）
  const resp = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  return checkResp(resp);
}
/** key-bind：候选钥匙签名（服务端按 body 内公钥验签而非已登记钥匙）+ 一次性 bind_token。 */
export async function apiBind(user, pubkey, bindToken) {
  const body = enc.encode(JSON.stringify({ user, pubkey }));
  const [bp, bl] = put(body);
  const auth = signHeaderPtr('POST', '/_api/auth/key-bind', bp, bl);
  unput(bp, bl);
  const resp = await fetch('/_api/auth/key-bind', {
    method: 'POST',
    headers: { Authorization: auth, 'X-Sr-Bind': bindToken, 'Content-Type': 'application/json' },
    body,
  });
  return checkResp(resp);
}

/** 密码登录全程：密钥生成/绑定无感；密码只在本地参与 OPAQUE 盲化，永不明文上线。 */
export async function passwordLogin(user, pass) {
  ensureSeed();               // 无感：无本地钥匙即刻生成（key-bind 的候选钥匙）
  state.user = user;
  state.boundAdmin = false;
  if (!user) throw { error: 'E_STATE', msg: '账号为空，请重新输入' };
  const st = opaqueStart(user, pass);                                    // ① ke1（未签名）
  let r;
  try { r = await postAuthJson('/_api/auth/login-begin', { user: user, ke1: st.ke1 }); }
  catch (e) { opaqueEnd(st.sid); throw e; }
  let ke3;
  try { ke3 = opaqueFinish(st.sid, pass, r.ke2); }                            // ② 本地 finish（错密码即败）
  finally { opaqueEnd(st.sid); }
  const r2 = await postAuthJson('/_api/auth/login-finish', { user: user, ke3: ke3 }); // ③ ke3 → bind_token（60s 单次）
  const kb = await apiBind(user, state.pub, r2.bind_token);                // ④ 绑定（幂等：同钥匙重绑=ok）
  state.kid = kb.kid || '';
  state.boundAdmin = !!kb.admin; // §2.2：账号持 admin 钥匙 → 密码绑定的新钥匙继承 admin（显式提示）
  localStorage.setItem('sr.user', user); // 账号一并记忆：下次自测直达（免密码）
}

export let heartbeatTimer = null;

export function startHeartbeat() {
  clearInterval(heartbeatTimer);
  heartbeatTimer = setInterval(async () => {
    try {
      await api('GET', '/_api/whoami', null);
      // 心跳成功=钥匙有效，静默（不更新 UI）
    } catch (e) {
      // E_AUTH=钥匙过期/吊销 → 停心跳+清本地+跳登录
      clearInterval(heartbeatTimer);
      localStorage.removeItem('sr.seed');
      localStorage.removeItem('sr.user');
      location.reload(); // 最简跳回登录（刷新后无 seed 走密码表单）
    }
  }, 60000); // 60s
}
