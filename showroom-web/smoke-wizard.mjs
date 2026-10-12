#!/usr/bin/env node
// showroom-web 向导冒烟（SPEC §10.2/§10.3/§10.4/§11/§12/§13 + plan-16 认证重设计；无浏览器依赖，node ≥ 18）：
//   mock 全局（document/localStorage/fetch/WebCrypto…）安装完毕后原生 import dist/js/app.mjs
//   （零构建 ES Modules，相对 import 由 node 自动解析），驱动登录与控制台状态机走完全部分支：
//   1) plan-16 一步登录（账号+密码，密钥无感）：全新浏览器 → 无感自动生成 seed（零提示、
//      DOM 无 seed/公钥展示）→ 错密码本地 finish 即失败（两态文案+无第三请求）→ 正确密码
//      login-begin/finish → key-bind（SR-SIG1 候选钥匙签名 + X-Sr-Bind 一次性令牌）→ 进入管理壳；
//      全部上线请求体无密码明文（OPAQUE 形状级断言，mock 垫片下）
//   2) 同浏览器再访（钥匙+账号记忆）→ 签名自测直达（零 auth 请求）；§12.2 管理员态；
//      退出登录=清本机密钥+远程吊销（B2：无密可免，登录卡只引导密码重登）
//   3) E_AUTH 回落（钥匙被删/GC）→ 密码表单 + 失效提示 → 密码重绑（同钥匙幂等）→ 进入
//   4) 失败两态之二：服务端 404 E_AUTH（限流/拒绝文案，无后续请求）；未知用户 login-begin
//      仍 200 且与错密码同文案（防枚举）；换账号密码登录（沿用本机钥匙 + admin 继承显式提示）；
//      whoami 失败顶部条报错
//   5) 「忘记此设备」：confirm 取消不清除；确认 → key-selfrm {kid}（仅删自身）→ 本地清
//      sr.seed/sr.user → 无感层即刻重生成新候选钥匙 → 干净登录卡
//   6) 专家路径（钥匙登录折叠）：本机已有默认选中（指纹=公钥载荷前 8 位）/ 导入种子
//      （非法 seed 字段内联拒绝；合法导入）→ E_AUTH 公钥展示与重检循环 → 登记后进入
//   7) §11 去版本字段：发布表单随机 8 位项目名预填（留空提交自动生成兜底）+ URL 恒 /<project>/
//   8) §10.4 流式消费：日志实时流（ndjson 行缓冲/1000 行帽/时间戳/开关停流）、
//      平台事件流（sr- 过滤/心跳忽略/20 条滚动/2s 去抖刷新/断线 5 秒重连）、
//      三段式备份下载（begin → status 两轮轮询 → 两个 4MB chunk → wasm 流式解密 → Blob 落地）
//   9) 上传压缩统一（zstd）：发布表单选普通镜像 tar → 自动 zstd-19 压缩 → import-begin
//      的 total_bytes 按压缩后长度申报（与独立 wasm 实例同粒度重压缩逐字节对账）；
//      恢复上传魔数两态（已是 zstd 原样 / 原生 -Fc 压缩后），信封长度按对应载荷核算
//  10) §12.1 一键发布：状态条①压缩（x%）→②上传（已传/压缩后总长）→③发布→④完成（大字 URL）
//      四段流转；发布失败标红重试（项目留空自动生成回填）；上传失败断点续传重试
//  11) §12.2 管理员：whoami 两态（admin=false 管理 DOM 不存在 / true 第五标签渲染）、
//      账号与钥匙表、发卡对话框提交体校验（registry 密码仅显示一次）、行内管理动作
//      （key-admin/key-rm/user-revoke + 中文 confirm 文案）、平台状态面板、30s 自动刷新、
//      admin 接口 404 E_AUTH 顶部条报错、whoami 失败顶部条报错
//  11b) plan-16 §1.5 发卡收口：主路径=打开对话框即客户端生成并暂存登录初始密码（crypto 随机 16 位，
//      不展示）→ user-add {user,password}（无 pubkey）→ 结果两行一次性密码（登录初始+registry）；
//      公钥直登记收进折叠专家路径（{user,pubkey}，结果仅 registry 行+kid）；「同时授予管理员」
//      勾选带 admin:true；账号表 has_password 徽标（已设/未设）+ 行内「补发密码」
//      （confirm → passwd-set {user,password} → 新密码一次性展示对话框 → 徽标翻转）
//  11c) plan-16 §2.2 改密收口：顶部「修改密码」对话框（当前+新+确认；autocomplete 三件）→
//      字段校验（空/不一致/与当前相同）→ 当前密码校验复用 login-begin+本地 finish（错即内联拒绝，
//      不触达 pw-begin）→ pw-begin {user,m1}（SR-SIG1 签名）→ pw-commit {user,record} → 旧密码
//      失效/新密码可登录（toy 服务端记录替换）→ 成功提示重新登录并退出会话
//  12) §13 发布三入口并存：一键发布 / 仅上传（无 publish 调用，③④ skip，镜像已入库提示）/
//      部署表单（/_api/images 惰性加载+刷新+选择自动带出项目标签+发布）；
//      §13.2 Redis 浏览用户隔离文案 + 管理员视图标注（whoami 两态）
//  13) 静态断言：「内存上限」「槽位/slot」零残留 / 旧三步向导主路径零残留 /
//      §10.3-§13 + plan-16 auth 端点已接入 / 旧单发 db-dump 已移除 /
//      新增文案全中文（无英文混排）/ 界面规范（无内联事件/无 transition:all/无 outline:none/
//      占位符 … 结尾/tablist 语义/reduced-motion/overscroll/touch-action/tabular-nums/theme-color）
//  14) §14 磁盘空间与镜像删除：上传区空间条（/_api/space 常显「可用空间（已扣预留）」一位小数 /
//      手动刷新 / 拉取失败静默「空间信息不可用」/ import-end 成功后自动刷新）、
//      镜像清单每行删除（confirm 含镜像名与回滚失效语义 → image-rm 提交体 → 顶部条回收量 →
//      清单+空间条刷新；confirm 取消不提交；失败走顶部条）、
//      磁盘 E_QUOTA 顶部条追加删除引导并刷新空间条（内存类 E_QUOTA 不追加）
//  15) §15 主机编排：期望状态渲染（中间件开关勾选态 / host.env 其余键通用键值渲染
//      含 FRP 不特判 / 四参数当前值回填 / 漂移黄条 warn-err 两档）、两段式（无 confirm 仅预演
//      返回 actions+impact 人读行不落盘；confirm=true 才 POST 执行并渲染报告；禁用中间件
//      impact 含受影响项目 → 红色警示 + 二次 confirm 取消不提交）、Redis 池黄条两态
//      （常态数字行 / <20% 余量黄条+「扩容」按钮）、扩容对话框流（非法值字段内联拒绝 →
//      confirm:false 取服务端提示原文逐字节展示 → 「我已了解，执行扩容」→ 结果回报+自动重取）
import { readFileSync, writeFileSync, readdirSync, copyFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { generateKeyPairSync } from 'node:crypto';

let failed = 0;
const ok = (cond, msg) => { console.log(`${cond ? 'ok  -' : 'FAIL-'} ${msg}`); if (!cond) failed++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(cond, ms = 8000, step = 100) {
  const t0 = Date.now();
  while (!cond()) { if (Date.now() - t0 > ms) return false; await sleep(step); }
  return true;
}

// —— DOM 垫片：按 id 惰性建元素桩（记录监听器；属性/classList 可读写） ——
class Cls {
  constructor() { this.s = new Set(); }
  add(...c) { c.forEach((x) => this.s.add(x)); }
  remove(...c) { c.forEach((x) => this.s.delete(x)); }
  toggle(c, f) { if (f === undefined) f = !this.s.has(c); if (f) this.s.add(c); else this.s.delete(c); return f; }
  contains(c) { return this.s.has(c); }
}
const els = new Map();
function makeEl(id) {
  const el = {
    id, value: '', textContent: '', innerHTML: '', checked: false, disabled: false, open: false,
    dataset: {}, style: {}, listeners: {}, classList: new Cls(),
    addEventListener(ev, fn) { (this.listeners[ev] ||= []).push(fn); },
    removeEventListener() {}, append() {}, focus() {}, select() {}, remove() {},
    closest() { return null; },
    querySelector() { return $s(id + ':q'); }, // 注册进 els：tbody 等子节点渲染结果可断言
    querySelectorAll() { return []; },
    content: { cloneNode: () => ({}) },
    showModal() { this.open = true; },
    close() { this.open = false; },
  };
  el.click = () => fire(el, 'click', { preventDefault() {} });
  return el;
}
const fire = (el, ev, arg) => { for (const fn of el.listeners[ev] || []) fn(arg || { preventDefault() {}, target: el, key: '' }); };
const $s = (id) => { if (!els.has(id)) els.set(id, makeEl(id)); return els.get(id); };
const click = (id) => fire($s(id), 'click', { preventDefault() {} });
const hidden = (id) => $s(id).classList.contains('hidden');
const shown = (id) => !hidden(id);
const tabBtn = (name) => ({ dataset: { tab: name }, classList: new Cls(), closest: () => ({ dataset: { tab: name } }) });
const clickTab = (name) => fire($s('tabs'), 'click', { target: tabBtn(name) });
/** R5 E1：管理页二级导航按钮桩与点击（与主 tab 同法）。 */
const adpBtn = (name) => ({ dataset: { adp: name }, classList: new Cls(), closest: () => ({ dataset: { adp: name } }) });
const clickAdp = (name) => fire($s('admin-subnav'), 'click', { target: adpBtn(name) });
/** tabs 方向键支持：querySelectorAll 桩返回按钮序列（与页面 #tabs 初始顺序一致）。 */
const armTabs = () => { $s('tabs').querySelectorAll = () => ['proj', 'pub', 'img', 'db', 'help'].map(tabBtn); }; // plan-17 §1 五标签
/** 与真实登录 HTML 的初始 class 对齐（惰性元素桩无 class 信息；断言「初始不可见」前先打桩）。
 *  B5/B6 后登录卡无隐藏按钮，此桩已空（保留调用点便于将来再引入初始 hidden 元素）。 */
const armLoginInit = () => {};
/** 模拟浏览器渲染 HTML：给关键按钮桩设置初始文案（busy 恢复断言按真实文字比对）。 */
const BTN_TEXT = {
  'lg-submit': '登录',
  'pub-submit': '发布', 'btn-upload-only': '仅上传', 'ad-na-submit': '发放', // plan-17 §5：单表单（dep-submit 删除）
  'hs-apply': '查看变更', 'hs-confirm': '确认执行', 'hs-rx-preview': '获取确认提示', 'hs-rx-run': '我已了解，执行扩容',
  'pw-submit': '确认修改',
};
const armBtnText = () => { for (const [id, t] of Object.entries(BTN_TEXT)) $s(id).textContent = t; };
globalThis.document = {
  getElementById: $s,
  createElement: (tag) => makeEl(`dyn-${tag}-${els.size}`),
  body: { appendChild() {} },
  querySelector: () => null,
  querySelectorAll: () => [], // 真实 DOM API 补齐（syncAdmin 的 #app .admin-only 显隐走此处；桩无树可查=空集）
  addEventListener() {},
  execCommand: () => true, // copyText 降级路径（node 无剪贴板）
};
globalThis.location = { origin: 'http://showroom.test' };

// —— localStorage 垫片（跨场景保留 = 同一浏览器） ——
const store = new Map();
globalThis.localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: (k) => store.delete(k),
};

// —— Blob 下载垫片：捕获 createObjectURL 的 Blob 以便校验解密产物 ——
const blobs = [];
globalThis.URL.createObjectURL = (b) => { blobs.push(b); return `blob:mock-${blobs.length}`; };
globalThis.URL.revokeObjectURL = () => {};

// —— wasm 实例（页面用 mock fetch 喂同一份字节；此处独立实例做交叉验证与服务端信封模拟） ——
const wasmBytes = readFileSync(new URL('./dist/showroom_bg.wasm', import.meta.url));
const enc2 = new TextEncoder(), dec2 = new TextDecoder();
const exW = (await WebAssembly.instantiate(wasmBytes, {})).instance.exports;
const wmem = () => new Uint8Array(exW.memory.buffer);

// —— OPAQUE 导出剥离钩子：本冒烟的 auth 服务端是 toy mock，只能与页面的 mock 垫片分支对话 ——
// 无论 dist/showroom_bg.wasm 是否已随新垫片重建（含 sr_login_*），页面加载路径在此一律剥掉
// OPAQUE 导出，确定性地驱动 mock 分支（报文序列形状 + 界面行为）；真 OPAQUE 往返由
// cargo test（common 固定向量 / serve 端到端）覆盖。上方 exW 用原始 instantiate，不受影响。
{
  const strip = (exports) => {
    const e = { ...exports };
    for (const k of Object.keys(e)) if (k === 'sr_rwd' || k.startsWith('sr_login') || k.startsWith('sr_reg')) delete e[k];
    return e;
  };
  const origInstantiate = WebAssembly.instantiate.bind(WebAssembly);
  WebAssembly.instantiate = async (bytes, imports) => {
    const r = await origInstantiate(bytes, imports);
    return { module: r.module, instance: { exports: strip(r.instance.exports) } };
  };
  const origStreaming = WebAssembly.instantiateStreaming?.bind(WebAssembly);
  if (origStreaming) {
    WebAssembly.instantiateStreaming = async (src, imports) => {
      const r = await origStreaming(src, imports);
      return { module: r.module, instance: { exports: strip(r.instance.exports) } };
    };
  }
}
function pubOf(hexSeed) {
  const p = exW.sr_alloc(32), o = exW.sr_alloc(128);
  for (let i = 0; i < 32; i++) wmem()[p + i] = parseInt(hexSeed.substr(i * 2, 2), 16);
  const r = exW.sr_pub_string(p, o, 128);
  const m = wmem(); let e = o; while (e < o + r && m[e] !== 0) e++;
  const s = dec2.decode(m.subarray(o, e));
  exW.sr_free(p, 32); exW.sr_free(o, 128);
  return s;
}
// 「服务端」侧信封：以客户端上行的一次性 RSA 公钥 seal 明文（§2 布局，镜像页面 pushSlice 数学）
const SEG = 4 * 1024 * 1024, WRAP = 384, NONCE = 12, TAG = 16;
function sealFor(pubPem, plain) {
  const putW = (b) => { const p = exW.sr_alloc(b.length); wmem().set(b, p); return [p, b.length]; };
  const [pp, pl] = putW(enc2.encode(pubPem));
  const [sp, sl] = putW(new Uint8Array(32)); // 会话 seed 固定即可（一次性信封）
  const sid = exW.sr_seal_begin_seed(pp, pl, sp);
  exW.sr_free(pp, pl); exW.sr_free(sp, sl);
  if (sid <= 0) throw new Error(`smoke: seal_begin 失败（${sid}）`);
  const parts = [];
  const push = (bytes) => {
    const [ip, il] = bytes.length ? putW(bytes) : [0, 0];
    const cap = bytes.length
      ? WRAP + NONCE + TAG + bytes.length + (Math.ceil(bytes.length / SEG) + 2) * (NONCE + TAG) + 64
      : WRAP + SEG + NONCE + TAG + 64; // flush 尾段最长 SEG（与页面 pushFlush 容量一致）
    const ob = exW.sr_alloc(cap);
    const w = exW.sr_seal_push(sid, ip, il, ob, cap);
    if (il) exW.sr_free(ip, il);
    if (w < 0) { exW.sr_free(ob, cap); throw new Error(`smoke: seal_push 失败（${w}）`); }
    const out = new Uint8Array(wmem().subarray(ob, ob + w));
    exW.sr_free(ob, cap);
    parts.push(out);
  };
  for (let o = 0; o < plain.length; o += SEG) push(plain.subarray(o, Math.min(o + SEG, plain.length)));
  push(new Uint8Array(0)); // flush 尾段
  exW.sr_seal_end(sid);
  const env = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let off = 0; for (const p of parts) { env.set(p, off); off += p.length; }
  return env;
}

// —— confirm 垫片：默认放行；文案全部记录（§12.2 管理动作中文 confirm 断言用） ——
const confirmMsgs = [];
let confirmRet = true;
globalThis.confirm = (m) => { confirmMsgs.push(String(m)); return confirmRet; };
// 原原生 alert() 已改 showMsg（IA 审计 39）：页面不再调用 alert，无需垫片

// —— 独立 zstd 压缩（与页面同 wasm 字节的另一实例）：对账页面 compressToBlob 的产物 ——
function zstdAll(chunks) {
  const sid = exW.sr_zstd_begin(19);
  if (!(sid > 0)) throw new Error(`smoke: sr_zstd_begin → ${sid}`);
  const parts = [];
  for (const c of chunks) {
    const p = exW.sr_alloc(c.length); wmem().set(c, p);
    const cap = c.length + (c.length >> 3) + 4096;
    const ob = exW.sr_alloc(cap);
    const w = exW.sr_zstd_push(sid, p, c.length, ob, cap);
    exW.sr_free(p, c.length);
    if (w < 0) { exW.sr_free(ob, cap); throw new Error(`smoke: sr_zstd_push → ${w}`); }
    if (w > 0) parts.push(new Uint8Array(wmem().subarray(ob, ob + w)));
    exW.sr_free(ob, cap);
  }
  exW.sr_zstd_end(sid);
  const all = new Uint8Array(parts.reduce((n, x) => n + x.length, 0));
  let o = 0; for (const x of parts) { all.set(x, o); o += x.length; }
  return all;
}
/** 信封总长公式（与页面 sealedTotal 一致，用于 total_bytes/信封长度对账）。 */
const sealedTotal = (n) => WRAP + n + Math.ceil(n / SEG) * (NONCE + TAG);

// —— fetch 垫片：wasm 服务真实字节；/_api 按 mode 返回（unregistered → 404 E_AUTH） ——
const resp = (status, obj) => ({ ok: status < 400, status, text: async () => JSON.stringify(obj) });
// plan-17 S1：whoami 附带平台公钥 PEM（服务端读 state/platform_rsa.pub 下发；假 PEM 仅作自动带回验证，密封前场景 7 会替换真钥）
const WHOAMI_PEM = '-----BEGIN PUBLIC KEY-----\nMIIBsmokeAutoPem\n-----END PUBLIC KEY-----\n';
const PROJECTS = { projects: [
  { project: 'demo', user: 'alice', tag: 'v1', url: '/demo/', status: 'running', port: 8100, mem: '256m', cpu: 0, updated: 1760107200 },
  { project: 'demo0', user: 'alice', tag: 'v0', url: '/demo0/', status: 'missing', port: 0, mem: '128m', cpu: 0, updated: 1760020800 }, // §4：仅镜像无容器 → 待发布
] };
let mode = 'unregistered'; // unregistered | ok（key-bind 成功 → ok）
// —— plan-16 §2.2 auth 端点族 mock（与页面 MOCK 垫片同一 toy 协议：ke2 内嵌验证因子，
//    错密码/未知用户在客户端本地 finish 即失败——行为契约与真 OPAQUE 一致：无第三请求） ——
const fnv = (s) => { let h = 0x811c9dc5; for (const b of enc2.encode(s)) { h ^= b; h = Math.imul(h, 0x01000193) >>> 0; } return h.toString(16).padStart(8, '0'); };
const mockR = (user, pass) => fnv(`showroom-opaque-mock|${user}|${pass}`);
const mB64 = (o) => Buffer.from(JSON.stringify(o)).toString('base64');
const mParse = (b64) => { try { return JSON.parse(Buffer.from(String(b64), 'base64').toString('utf8')); } catch { return null; } };
const PWDB = { alice: { pass: 'alice-pw', admin: false }, bob: { pass: 'bob-pw', admin: true } };
let authBeginN = 0, authFinishN = 0, authBindN = 0, selfrmN = 0, bindSeq = 0;
let authServerDown = false;              // true：login-begin 一律 404 E_AUTH（失败态二：限流/拒绝）
let pendingBind = null;                  // 服务端在途 bind_token（单次使用语义）
const authBeginBodies = [], authFinishBodies = [], keyBindBodies = [], keyBindAuths = [], keyBindToks = [];
const selfrmBodies = [];
const wireTexts = [];                    // 全部上线请求体（密码零明文断言用）
// plan-16 §2.2 改密三件序列（SR-SIG1 签名端点）+ §1.5 补发：观测与捕获
let pwBeginN = 0, pwCommitN = 0;
const pwBeginBodies = [], pwCommitBodies = [], pwBeginAuths = [];
let pwChangeNew = null;                  // toy 服务端无法从 record 提取密码：由场景侧告知（记录替换即生效）
// plan-16 §1.5 补发（passwd-set）：观测与捕获
const passwdSetBodies = [];
// 观测计数 / 捕获（场景 6 断言用）
let projectsN = 0, eventsN = 0, logsN = 0, statusN = 0;
let lastLogsUrl = '', lastLogsAuth = '', lastEventsAuth = '', beginBody = null;
const chunkCalls = [];
let dumpEnv = null; // { env: 信封字节 }
// 场景 7 捕获：分片导入 / 恢复上传
let importBeginBody = null, importEndN = 0, publishN = 0, uploadRec = null;
const importChunkLens = [];
const restoreLens = [];
// 场景 8/9 捕获（§12）：whoami 两态 / 管理接口 / 一键发布状态条快照 / 失败重试
let whoamiAdmin = false, whoamiFail = false, adminFail404 = false;
let adminUsersN = 0, adminStatusN = 0, importStatusN = 0, publishAttempts = 0;
let userAddBody = null, lastPublishBody = null, publishFailOnce = false, chunkFailN = 0;
const keyAdminBodies = [], keyRmBodies = [], revokeBodies = [], userAdminBodies = [];
let adminProjectsN = 0;
const projActs = []; // R5 E3：跨用户行操作（start/stop/rm 提交体）
const dpSnaps = [], compSnaps = [];
let dpBeginSnap = null, dpPublishSnap = null;
const upSessions = {}; // 模拟服务端分片会话：upload_id → { received: [seq…] }
let curUploadId = null;
// 场景 §13 捕获：镜像清单（/_api/images）惰性加载/刷新/管理员视图
let imagesN = 0, imagesAdminView = false; // imagesAdminView：模拟服务端按 admin 钥匙返回全量
const IMAGES_ALL = ['alice/demo:v1', 'alice/demo2:v2', 'bob/other:v1'];
const IMAGES_SELF = ['alice/demo:v1', 'alice/demo2:v2'];
// 场景 §14 捕获：磁盘空间（/_api/space）/ 镜像删除（/_api/image-rm）
let spaceN = 0, spaceFail = false;        // spaceFail：模拟 df 失败（500 E_STATE，fail-closed）
const imagesRemoved = new Set();          // 模拟服务端删除后清单收敛
const imageRmBodies = [];
let imageRmFail = null;                   // 一次性 image-rm 失败响应（{error,msg}）
let publishQuota = null;                  // 一次性 publish E_QUOTA（{msg}：磁盘/内存两态文案）
// 场景 §15 捕获：主机编排（/_api/admin/host GET 期望状态 / POST 两段式 + Redis 扩容流）
let hostGetN = 0;
const hostPostBodies = [];                // 全部 POST 体（断言 confirm 两态与形状）
const REDIS_PROMPT = [
  'Redis 逻辑库池将 256 → 256：需重启 Redis 实例（约 3 秒）。',
  '- 数据已开 AOF 持久化（appendonly yes），重启不丢已确认写入；',
  '- 使用 REDIS_* 的应用连接会闪断一次并自动重连（客户端重试期间个别请求可能报错）；',
  '- 数据卷 _middleware/redis 为 bind 挂载，容器重建不触碰数据。',
  '确认执行请加 --confirm。',
].join('\n');
const HOST_STATE = { // 方案 §4 契约形状 + 协调者补充的 FRP 键（前端通用键值渲染）
  host_env: { MIDDLEWARE: ['postgres', 'redis', 'registry'], CADDY: true, LAN_HOST: '10.0.0.8', PUBLIC_HOST: '', TUNING: 'sysctl', DEDUP: 'weekly', FRP: 'running' },
  policy: { MAX_SLOTS: 30, REDIS_DBS: 256, RESERVE_MEM_MB: 1536, DISK_RESERVE_GB: 20, UPLOAD_SPACE_FACTOR: 4 },
  drift: [
    { item: 'sr-redis', expected: '--databases 256', actual: '--databases 16', severity: 'warn', advice: 'showroomctl middleware set redis databases=256' },
  ],
  redis_pool: { used: 27, total: 256, headroom: 229, suggest_expand: false },
  suggestions: [],
};
let hostResp = HOST_STATE; // 可替换（池两态/扩容后收敛）
const ADM_USERS = { users: [ // R5 D1：admin 为账号级（users[].admin）；钥匙行无级（D2 垫片移除后 keys[].admin 不存在）
  { user: 'alice', projects: 2, has_password: true, admin: true, keys: [
    { kid: 'kid-a1', since: 1788228000 },
    { kid: 'kid-a2', since: 1788318000 } ] },
  { user: 'bob', projects: 0, has_password: false, admin: false, keys: [ { kid: 'kid-b1', since: 1790816400 } ] },
] };
// R5 E3：管理页「全部项目」面板数据源（跨用户全量 + owner）
const ADM_PROJECTS = { projects: [
  { project: 'demo', user: 'alice', tag: 'v1', url: '/demo/', status: 'running', port: 8100, mem: '256m', cpu: 0, updated: 1760107200 },
  { project: 'bobapp', user: 'bob', tag: 'v1', url: '/bobapp/', status: 'exited', port: 8101, mem: '128m', cpu: 0, updated: 1760110000 },
] };
const ADM_STATUS = {
  services: { 'showroom-caddy': 'active', 'showroom-registry': 'active', 'showroom-postgres': 'active', 'showroom-redis': 'inactive' },
  // 真实服务端形状（ops.rs admin_status）：disk（单数）+ df -B1 字节值 + mem.available_mb
  disk: { root: { total: 1073741824000, used: 3547525120 }, showroom: { total: 214748364800, used: 45097315666 } },
  mem: { available_mb: 4096 },
  containers: [{ name: 'sr-demo', status: 'running' }, { name: 'sr-hello', status: 'exited' }],
  timers: [{ timer: 'showroom-snapshot-hourly.timer', next: '2026-10-10 13:00:00' }],
};
// ndjson 流响应：逐行 read；close=true 发完即 done（模拟服务端收流），否则长连挂起
function streamResp(lines, opt = {}) {
  let i = 0, closed = false;
  return {
    ok: true, status: 200,
    text: async () => '',
    arrayBuffer: async () => new ArrayBuffer(0),
    body: {
      getReader() {
        return {
          async read() {
            if (i < lines.length) return { done: false, value: enc2.encode(lines[i++] + '\n') };
            if (opt.close && !closed) { closed = true; return { done: true }; }
            return new Promise(() => {}); // 长连：保持打开
          },
          cancel: async () => {},
        };
      },
    },
  };
}
const EVENTS1 = [
  '{"hb":true}',
  '{"t":1760000000000,"e":"start","actor":"sr-demo"}',
  '{"t":1760000001000,"e":"die","actor":"other-box"}',
  '{"t":1760000002000,"e":"health_status: healthy","actor":"sr-demo"}',
];
const EVENTS2 = [
  '{"hb":true}',
  '{"t":1760000009000,"e":"stop","actor":"sr-demo2"}',
];
const LOG_LINES = Array.from({ length: 1003 }, (_, i) => `{"t":1760000100000,"l":"log-${String(i).padStart(4, '0')}"}`);
globalThis.fetch = async (url, init = {}) => {
  const u = String(url);
  const auth = init?.headers?.Authorization || '';
  // 上线内容全量留档（密码零明文断言用；二进制体按 utf8 有损解码仅为子串扫描）
  const t = typeof init.body === 'string' ? init.body
    : (init.body instanceof Uint8Array ? dec2.decode(init.body) : '');
  if (t) wireTexts.push(`${u} ${t}`);
  if (u.includes('showroom_bg.wasm')) return { ok: true, status: 200, text: async () => '', arrayBuffer: async () => wasmBytes.buffer.slice(wasmBytes.byteOffset, wasmBytes.byteOffset + wasmBytes.byteLength) };
  // —— plan-16 §2.2：唯一应答未签名请求的端点族；畸形/限流一律 404 E_AUTH（与未知路径一致） ——
  if (u.includes('/_api/auth/login-begin')) {
    authBeginN++;
    const body = JSON.parse(t);
    authBeginBodies.push(body);
    if (authServerDown) return resp(404, { error: 'E_AUTH' });
    const rec = PWDB[body.user];
    const rCorrect = rec ? mockR(body.user, rec.pass) : fnv(`unknown|${body.user}`); // 未知用户走假记录路径（防枚举）
    const n = `n${authBeginN}`;
    return resp(200, { ke2: mB64({ v: 1, m: 'ke2', n, mac: fnv(`mac|${n}|${rCorrect}`) }) });
  }
  if (u.includes('/_api/auth/login-finish')) {
    authFinishN++;
    const body = JSON.parse(t);
    authFinishBodies.push(body);
    const j = mParse(body.ke3);
    if (!j || j.m !== 'ke3') return resp(404, { error: 'E_AUTH' });
    pendingBind = { user: body.user, token: `bindtok-${++bindSeq}` };
    return resp(200, { bind_token: pendingBind.token });
  }
  if (u.includes('/_api/auth/key-bind')) {
    authBindN++;
    const body = JSON.parse(t);
    const tok = init?.headers?.['X-Sr-Bind'] || '';
    keyBindBodies.push(body); keyBindAuths.push(auth); keyBindToks.push(tok);
    if (!pendingBind || pendingBind.token !== tok || pendingBind.user !== body.user) return resp(404, { error: 'E_AUTH' });
    pendingBind = null; // bind_token 单次使用
    mode = 'ok';        // 绑定成功 ⇒ 候选钥匙此后 health 自测通过
    return resp(200, { kid: `kid-bind-${authBindN}`, admin: !!(PWDB[body.user] && PWDB[body.user].admin) });
  }
  if (u.includes('/_api/key-selfrm')) { selfrmN++; selfrmBodies.push(JSON.parse(t)); return resp(200, { ok: true }); }
  // —— plan-16 §2.2 改密三件序列（SR-SIG1 常规签名；与页面的 mock 注册分支同一 toy 协议：m1/m2/record） ——
  if (u.includes('/_api/auth/pw-begin')) {
    pwBeginN++;
    const body = JSON.parse(t);
    pwBeginBodies.push(body); pwBeginAuths.push(auth);
    const j1 = mParse(body.m1);
    if (!j1 || j1.m !== 'm1' || j1.u !== body.user) return resp(500, { error: 'E_STATE', msg: 'pw-begin: bad m1' });
    return resp(200, { m2: mB64({ v: 1, m: 'm2', n: `p${pwBeginN}` }) });
  }
  if (u.includes('/_api/auth/pw-commit')) {
    pwCommitN++;
    const body = JSON.parse(t);
    pwCommitBodies.push(body);
    const jr = mParse(body.record);
    if (!jr || jr.m !== 'record') return resp(500, { error: 'E_STATE', msg: 'pw-commit: bad record' });
    if (pwChangeNew != null && PWDB[body.user]) PWDB[body.user].pass = pwChangeNew; // 记录替换即新密码生效（toy 协议）
    return resp(200, { ok: true });
  }
  if (u.includes('/_api/health')) return mode === 'ok' ? resp(200, { ok: true }) : resp(404, { error: 'E_AUTH' });
  if (/\/_api\/(start|stop|rollback|rm)$/.test(u)) { // S4：行动作（rm 走诚实 confirm 后直达）；R5 E3：跨用户操作捕获
    const act = u.split('/').pop();
    projActs.push({ act, body: JSON.parse(t || '{}') });
    return resp(200, { ok: true, ...((act === 'start' || act === 'rollback') ? { url: '/demo/' } : {}) });
  }
  if (u.includes('/_api/admin/projects')) { // R5 E3：全部项目面板（admin_guard 之后的全量 + owner）；置于 /_api/projects 之前
    adminProjectsN++;
    if (adminFail404) return resp(404, { error: 'E_AUTH' });
    return resp(200, ADM_PROJECTS);
  }
  if (u.includes('/_api/projects')) { projectsN++; return resp(200, PROJECTS); }
  if (u.includes('/_api/images')) { // §13.1：admin 钥匙全量（admin_view），普通仅本人前缀；§14.3 删除后清单收敛
    imagesN++;
    const src = imagesAdminView ? IMAGES_ALL : IMAGES_SELF;
    return resp(200, { images: src.filter((s) => !imagesRemoved.has(s)) });
  }
  if (u.includes('/_api/space')) { // §14.1：根分区三元组（MB；available 未扣预留，控制台显示 available-reserve）
    spaceN++;
    return spaceFail ? resp(500, { error: 'E_STATE', msg: 'df /: 模拟失败' }) : resp(200, { available_mb: 40960, reserve_mb: 20480, total_mb: 102400 });
  }
  if (u.includes('/_api/image-rm')) { // §14.3：删除+内联 GC → freed_est_mb；清单即刻不含该 tag
    const body = JSON.parse(dec2.decode(init.body));
    imageRmBodies.push(body);
    if (imageRmFail) { const f = imageRmFail; imageRmFail = null; return resp(f.error === 'E_QUOTA' ? 429 : 404, f); }
    imagesRemoved.add(body.image);
    return resp(200, { freed_est_mb: 512 });
  }
  if (u.includes('/_api/redis-dbs')) return resp(200, { dbs: [0, 1, 2, 3], admin_view: whoamiAdmin, entries: [
    { db: 0, project: 'demo' }, { db: 1, project: 'demo2' }, { db: 2, project: null }, { db: 3, project: null },
  ] });
  if (u.includes('/_api/redis-keys')) return resp(200, { keys: ['sr_demo'] });
  if (u.includes('/_api/redis-get')) return resp(200, { type: 'string', ttl_ms: -1, value: 'x'.repeat(5000) });
  // —— §10.4 流式端点 ——
  if (u.includes('/_api/logs-stream')) { logsN++; lastLogsUrl = u; lastLogsAuth = auth; return streamResp(LOG_LINES); }
  if (u.includes('/_api/events')) {
    eventsN++; lastEventsAuth = auth;
    return eventsN === 1 ? streamResp(EVENTS1, { close: true }) : streamResp(EVENTS2);
  }
  // —— §10.4 三段式 db-dump（服务端模拟） ——
  if (u.includes('/_api/db-dump-begin')) {
    beginBody = JSON.parse(dec2.decode(init.body));
    const plain = new Uint8Array(SEG + 12345); // >4MiB ⇒ 信封必跨两个分片
    for (let i = 0; i < plain.length; i++) plain[i] = (i * 31 + 7) & 0xff;
    dumpEnv = { env: sealFor(beginBody.client_pub_pem, plain), plain };
    return resp(200, { dump_id: 'dmp-smoke-1' });
  }
  if (u.includes('/_api/db-dump-status')) {
    statusN++; // 第一轮 running，第二轮 ready（轮询两轮路径）；F8：ready 附服务端原文 out（db-out 优先展示）
    return statusN === 1 ? resp(200, { ready: false, total: 0, error: null })
                         : resp(200, { ready: true, total: dumpEnv.env.length, error: null, out: 'pg_dump -Fc 导出完成（服务端原文）。' });
  }
  if (u.includes('/_api/db-dump-chunk')) {
    const m = u.match(/[?&]offset=(\d+)&len=(\d+)/) || [];
    const off = Number(m[1]), len = Number(m[2]);
    chunkCalls.push({ off, len });
    const b = dumpEnv.env.subarray(off, off + len);
    const c = new Uint8Array(b.length); c.set(b); // 独立拷贝
    return {
      ok: true, status: 200,
      text: async () => '',
      headers: { get: (k) => String(k).toLowerCase() === 'x-sr-total' ? String(dumpEnv.env.length) : null },
      arrayBuffer: async () => c.buffer,
    };
  }
  // —— 场景 7/8：分片导入（压缩后上传）、断点续传查询、发布 ——
  if (u.includes('/_api/import-begin')) {
    importBeginBody = JSON.parse(dec2.decode(init.body));
    dpBeginSnap = { txt: $s('dp-text').textContent || '' }; // import-begin 时状态条应处②上传段
    curUploadId = 'up-smoke-1';
    upSessions[curUploadId] = { received: [] }; // 新会话（旧会话已被 import-end 消费）
    return resp(200, { upload_id: curUploadId });
  }
  if (u.includes('/_api/import-chunk')) {
    if (chunkFailN > 0) { chunkFailN--; return resp(500, { error: 'E_NET', msg: '模拟上传网络失败' }); }
    importChunkLens.push(init.body?.length ?? 0);
    upSessions[curUploadId]?.received.push(Number(u.match(/seq=(\d+)/)[1]));
    dpSnaps.push({ txt: $s('dp-text').textContent || '', v: $s('dp-prog').value, m: $s('dp-prog').max, s2: $s('dp-s2').classList.contains('on') });
    if (!uploadRec) uploadRec = JSON.parse(store.get('sr.up.up-smoke-1') || 'null'); // 首片时抓本地会话记录
    return resp(200, { ok: true });
  }
  if (u.includes('/_api/import-status')) { // 断点续传：返回该会话已收分片序号
    importStatusN++;
    const sid = u.match(/upload=([^&]+)/)?.[1];
    return resp(200, { received: (upSessions[decodeURIComponent(sid || '')] || { received: [] }).received.slice(), total: 0 });
  }
  if (u.includes('/_api/import-end')) { importEndN++; delete upSessions[curUploadId]; return resp(200, { ok: true, digest: 'sha256:smoke' }); }
  if (u.includes('/_api/publish')) {
    publishAttempts++;
    lastPublishBody = JSON.parse(dec2.decode(init.body));
    dpPublishSnap = { txt: $s('dp-text').textContent || '' }; // publish 时状态条应处③发布中
    if (publishQuota) { const f = publishQuota; publishQuota = null; return resp(429, { error: 'E_QUOTA', msg: f.msg }); }
    if (publishFailOnce) { publishFailOnce = false; return resp(500, { error: 'E_NET', msg: '模拟发布失败' }); }
    publishN++;
    return resp(200, { ok: true, url: '/' + lastPublishBody.project + '/', port: 8101 });
  }
  // —— §12.2 身份与管理端点（非管理员/故障 → 404 E_AUTH 不可区分） ——
  if (u.includes('/_api/whoami')) {
    if (whoamiFail) return resp(404, { error: 'E_AUTH' });
    return resp(200, { user: 'alice', kid: 'kid-self', admin: whoamiAdmin, platform_pem: WHOAMI_PEM });
  }
  if (u.includes('/_api/admin/user-add')) { // plan-16 §1.5 + B7：password → login_password 一次性回显；无密码 → 400 E_NAME
    userAddBody = JSON.parse(dec2.decode(init.body));
    if (!userAddBody.password && !userAddBody.gen_password) return resp(400, { error: 'E_NAME', msg: '缺少初始密码：password 与 gen_password 至少其一' });
    return resp(200, {
      ok: true,
      ...(userAddBody.password ? { login_password: userAddBody.password } : {}),
      registry_password: 'reg-pw-smoke',
    });
  }
  if (u.includes('/_api/admin/passwd-set')) { // plan-16 §1.5 补发：{user,password} → 一次性返回；徽标随之翻转
    const body = JSON.parse(dec2.decode(init.body));
    passwdSetBodies.push(body);
    const row = ADM_USERS.users.find((x) => x.user === body.user);
    if (row) row.has_password = true;
    return resp(200, { ok: true, password: body.password || 'gen-pw-smoke' });
  }
  if (u.includes('/_api/admin/user-revoke')) { revokeBodies.push(JSON.parse(dec2.decode(init.body))); return resp(200, { ok: true }); }
  if (u.includes('/_api/admin/user-admin')) { userAdminBodies.push(JSON.parse(dec2.decode(init.body))); return resp(200, { ok: true }); } // R5 D1：账号级设/免管理员
  if (u.includes('/_api/admin/key-rm')) { keyRmBodies.push(JSON.parse(dec2.decode(init.body))); return resp(200, { ok: true }); }
  if (u.includes('/_api/admin/users')) { adminUsersN++; if (adminFail404) return resp(404, { error: 'E_AUTH' }); return resp(200, ADM_USERS); }
  if (u.includes('/_api/admin/status')) { adminStatusN++; if (adminFail404) return resp(404, { error: 'E_AUTH' }); return resp(200, ADM_STATUS); }
  if (u.includes('/_api/admin/host')) { // §15：GET 期望状态 / POST 两段式（host_env+policy）与扩容第二形态（redis_databases）
    if ((init?.method || 'GET') === 'POST') {
      const body = JSON.parse(dec2.decode(init.body));
      hostPostBodies.push(body);
      if (body.redis_databases !== undefined) { // 扩容流：无 confirm 返回提示原文；confirm=true 走状态机回报
        if (!body.confirm) return resp(200, { prompt: REDIS_PROMPT, confirm_required: true });
        hostResp = { ...HOST_STATE, redis_pool: { used: 27, total: body.redis_databases, headroom: body.redis_databases - 27, suggest_expand: false } };
        return resp(200, { ok: true, databases: body.redis_databases, restart_secs: 3 });
      }
      const keep = String(body.host_env?.MIDDLEWARE ?? '').split(/\s+/).filter(Boolean);
      const removed = ['postgres', 'redis', 'registry'].filter((m) => !keep.includes(m));
      const affected = removed.includes('redis') ? ['demo', 'demo2'] : []; // 禁 redis ⇒ 有项目引用
      if (!body.confirm) return resp(200, { // 写盘前预演：actions（needed 两态）+ impact + confirm_required
        actions: [
          { kind: 'ContainerRm', target: 'sr-redis', needed: true, detail: 'docker rm -f -v sr-redis（bind 卷除外）' },
          { kind: 'UnitDisableRm', target: 'showroom-dedup.timer', needed: true, detail: 'systemctl disable --now + rm' },
          { kind: 'ContainerUp', target: 'sr-postgres', needed: false, detail: 'postgres:16-alpine（已就绪，参数一致）' },
        ],
        impact: { removed_middleware: removed, affected_projects: affected },
        confirm_required: true,
      });
      hostResp = { // confirm=true：host_env/policy 按提交体收敛
        ...HOST_STATE,
        host_env: { ...HOST_STATE.host_env, MIDDLEWARE: keep },
        policy: { ...HOST_STATE.policy, ...(body.policy || {}) },
      };
      return resp(200, { ok: true, report: { changed: 2, warnings: [], actions: [
        { kind: 'ContainerRm', target: 'sr-redis', needed: true, ok: true, result: 'removed' },
        { kind: 'UnitDisableRm', target: 'showroom-dedup.timer', needed: true, ok: true, result: 'disabled' },
      ] } });
    }
    hostGetN++;
    return resp(200, hostResp);
  }
  if (u.includes('/_api/db-restore')) {
    restoreLens.push(init.body?.length ?? 0);
    return resp(200, { ok: true });
  }
  throw new Error(`smoke: 未预期的请求 ${u}`);
};

// —— 模块加载（每个场景一份全新 dist/js 拷贝 → node 原生 ES Modules 全新实例；mock 全局先行安装） ——
// 拆分适配：原「从 index.html 抽内联 <script> 写临时文件求值」改为原生 import dist/js/app.mjs。
// 查询串只刷新入口文件本身（静态依赖会命中缓存共享实例），故每场景把整份 js/ 拷贝到独立
// 临时目录再 import——与原「每场景全新模块实例」语义一致；wasm 经 mock fetch 按 URL 子串供给。
const html = [
  'index.html', 'css/app.css',
  ...readdirSync(new URL('./dist/js/', import.meta.url)).filter((f) => f.endsWith('.mjs')).sort().map((f) => 'js/' + f),
].map((f) => readFileSync(new URL('./dist/' + f, import.meta.url), 'utf8')).join('\n'); // 静态断言全量文本源（index → css → js，保持顺序敏感断言成立）
const load = (t) => {
  const dir = mkdtempSync(join(tmpdir(), `sr-wiz-${t}-`));
  for (const f of readdirSync(new URL('./dist/js/', import.meta.url))) {
    if (f.endsWith('.mjs')) copyFileSync(new URL('./dist/js/' + f, import.meta.url), join(dir, f));
  }
  return import(pathToFileURL(join(dir, 'app.mjs')).href + `?t=${t}`);
};

// ═══ 场景 1：全新浏览器 → 无感生成钥匙 → 密码登录（失败两态/成功绑定）→ 进入管理壳 ═══
mode = 'unregistered';
els.clear();
await load(1);
armTabs(); // tabs 方向键桩
armBtnText(); // 按钮初始文案（busy 恢复断言用）
armLoginInit(); // 登录卡初始 hidden 桩（对齐真实 HTML；现无隐藏按钮，空操作）
await sleep(40);
ok(shown('login') && !els.has('hd-user'), '登录前：只有登录卡（一步「账号+密码」），管理壳未构建（零管理信息）');
const seed1 = store.get('sr.seed');
ok(/^[0-9a-f]{64}$/.test(seed1 || ''), `无感断言：首访自动生成 seed 入 localStorage（${(seed1 || '').slice(0, 8)}…）`);
ok(($s('msg').textContent || '') === '', '无感断言：密钥生成零提示（顶部条静默）');
{
  const pub1 = pubOf(seed1);
  const leaked = [...els.values()].some((el) =>
    (el.textContent || '').includes(seed1) || (el.textContent || '').includes(pub1)
    || (el.innerHTML || '').includes(seed1) || (el.innerHTML || '').includes(pub1));
  ok(!leaked, '无感断言：任何 DOM 文本不含 seed 十六进制与公钥串');
}
ok(els.has('lg-user') && els.has('lg-pass'), '一步登录表单：账号 + 密码两个输入');
ok(!els.has('lg-reenter'), '无第三态入口：「免密码进入」按钮已删（两态铁律：登录态=控制台 / 未登录=密码表单）');
// —— 失败态一（本地密码错）：两态文案 + 无第三请求 ——
const ab1 = authBeginN, af1 = authFinishN, kb1 = keyBindBodies.length;
$s('lg-user').value = 'alice';
$s('lg-pass').value = 'wrong-pw';
wireTexts.length = 0;
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(80);
ok(authBeginN === ab1 + 1 && authBeginBodies.at(-1).user === 'alice'
  && typeof authBeginBodies.at(-1).ke1 === 'string' && authBeginBodies.at(-1).ke1.length > 16,
  'login-begin 提交体 {user, ke1}（未签名 POST，OPAQUE 报文序列①）');
ok(authFinishN === af1 && keyBindBodies.length === kb1, '失败态一（本地密码错）：客户端本地 finish 即失败，无 login-finish/key-bind 请求');
const perr1 = $s('lg-pass-err').textContent || '';
ok(perr1.includes('密码错误') && perr1.includes('尚未设置密码') && perr1.includes('请联系管理员'),
  '失败态一文案：「密码错误，或该账号尚未设置密码（请联系管理员…）」两态合一（防枚举一致代价）');
ok(shown('login') && !els.has('hd-user'), '失败后停留登录卡（管理壳未构建）');
ok(!$s('lg-submit').disabled && $s('lg-submit').textContent === '登录', '登录按钮恢复可用与原文');
// —— 成功路径：login-finish → key-bind（SR-SIG1 + X-Sr-Bind）→ 进入管理壳 ——
$s('lg-pass').value = 'alice-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
ok(await waitFor(() => keyBindBodies.length === kb1 + 1 && hidden('login'), 5000), '登录成功：login-finish → key-bind → 自动进入管理壳');
ok(authFinishN === af1 + 1 && authFinishBodies.at(-1).user === 'alice'
  && typeof authFinishBodies.at(-1).ke3 === 'string' && authFinishBodies.at(-1).ke3.length > 16,
  'login-finish 提交体 {user, ke3}（报文序列③）');
ok(keyBindBodies.at(-1).user === 'alice' && keyBindBodies.at(-1).pubkey === pubOf(seed1)
  && /^ed25519:/.test(keyBindBodies.at(-1).pubkey),
  'key-bind 提交体 {user, pubkey}（公钥=本机无感生成钥匙：登录成功即绑定）');
ok(/^SR-SIG1 /.test(keyBindAuths.at(-1)), 'key-bind 以候选钥匙 SR-SIG1 签名（服务端按 body 公钥验签）');
ok(keyBindToks.at(-1) === `bindtok-${bindSeq}` && keyBindToks.at(-1).startsWith('bindtok-'),
  'key-bind 携带 X-Sr-Bind 一次性令牌（login-finish 应答的 bind_token）');
ok(shown('app') && els.has('hd-user') && $s('hd-user').textContent === 'alice', '管理壳回显账号');
ok(store.get('sr.user') === 'alice' && store.get('sr.seed') === seed1, '账号记忆（sr.user）；seed 未变动');
ok(!wireTexts.some((t) => t.includes('alice-pw') || t.includes('wrong-pw')),
  'OPAQUE 断言：全部上线请求体（含 ke1/ke3 与签名体）均无密码明文（mock 垫片形状级验证）');
ok(!($s('msg').textContent || '').includes('继承管理员'), '非管理员账号：无 admin 继承提示');
await sleep(60); // 项目列表异步渲染
{
  const ptbl1 = $s('proj-tbl:q').innerHTML || '';
  ok(ptbl1.includes('>demo<') && ptbl1.includes('>demo0<'), '项目列表已请求并渲染（表格行渲染）');
}
ok(($s('proj-fb').textContent || '') === '', '成功不再写「已加载 N 项」计数副题（proj-fb 仅失败/操作反馈用）');
{
  const ptbl = $s('proj-tbl:q').innerHTML || '';
  ok(ptbl.includes('>●</span> 运行中') && ptbl.includes('>●</span> 待发布'), 'S5：●+文字双通道状态（运行中/待发布）');
  ok(!ptbl.includes('未部署') && !ptbl.includes('missing') && !ptbl.includes('待部署'), 'S5：「未部署」「missing」「待部署」零进 UI（§4 词表纪律；F1 统一「待发布」）');
  ok(ptbl.includes('仅镜像无容器：从发布页选择镜像即可拉起'), 'S5：待发布 tooltip 说明镜像在库可拉起');
  ok(ptbl.includes('data-act="deploy-row"') && />发布</.test(ptbl), 'S5/F1：待发布行可执行「发布」（→发布页预填）');
  ok(!/title="running"/.test(ptbl) && !/title="exited"/.test(ptbl) && !/title="missing"/.test(ptbl),
    'R5 C4：已知状态不再带英文 tooltip（原词仅未知回落分支保留）');
  ok(!ptbl.includes('>打开<') && !ptbl.includes('非本人项目'), 'R5 A2：「打开」按钮删除（URL 列即链接）；他人项目分支零残留');
}
await sleep(60); // whoami 异步返回（§12.2）
ok(($s('hd-pempub').value || '').startsWith('-----BEGIN PUBLIC KEY-----'), 'S1：whoami 附带 platform_pem 自动写入 PEM 字段（免手工粘贴）');
ok((store.get('sr.pempub') || '').startsWith('-----BEGIN PUBLIC KEY-----'), 'S1：platform_pem 已自动缓存到 sr.pempub（写现有键）');
ok(!els.has('tab-admin') && !els.has('ad-users-tbl') && !els.has('dlg-user-add') && !els.has('ad-st-auto') && !els.has('st-logs-live'),
  'whoami admin=false：管理标签与全部管理 DOM 不存在（第五标签不渲染）；自动刷新/实时跟随无开关 UI（R4 恒开）');

// —— 登录后 Redis 浏览（§10.3 三端点 + 交互；§13.2 普通用户视图） ——
$s('rd-db').value = '3'; $s('rd-q').value = 'sr_';
fire($s('rd-q'), 'keydown', { key: 'Enter', preventDefault() {} }); // A1 后：查询入口=回车（#rd-refresh 已随 c3b0558 删除）
await sleep(60);
ok(($s('rd-db').innerHTML || '').includes('<option value="0">demo</option>') && ($s('rd-db').innerHTML || '').includes('未分配'), 'redis-dbs：下拉显示项目名（R8 裁决；未分配库占位，value=库号）');
ok(($s('rd-keys').innerHTML || '').includes('sr_demo'), 'redis-keys：键列表渲染（前缀过滤请求）');
ok($s('rd-count').textContent === '共 1 个键', '「共 N 个键」提示（C2：Redis 文案统一「键」；「可能截断」只留在静态说明）');
ok(!els.has('rd-scope'), '§13.2 R4 裁决：管理员视图徽章已删（顶栏「管」角标唯一，同一信息只出现一次）');
ok(hidden('rd-admin-clause'), 'F11：非管理员不渲染「管理员可见全部逻辑库」半句（Redis 静态句按受众拆分）');
$s('rd-keys').value = 'sr_demo';
fire($s('rd-keys'), 'change');
await sleep(60);
const rmeta = $s('rd-meta').textContent || '';
ok(rmeta.includes('类型 string') && rmeta.includes('永久（-1）'), 'redis-get：类型 + 生存时间（ttl_ms -1=永久）');
ok(rmeta.includes('值已截断'), '值 ≥4KB：截断提示');
ok($s('rd-val').textContent.length === 5000, '值以等宽 pre 原样展示');

// —— §13.1 镜像清单（部署表单数据源）：进入发布区惰性加载 + 前缀过滤 ComboBox 数据 ——
const imgSrc = readFileSync(new URL('./dist/js/img.mjs', import.meta.url), 'utf8');
ok(imagesN === 1, '进入管理壳（发布区）：/_api/images 惰性加载一次');
ok(imgSrc.includes('setComboOptions($(\'dep-image\'), images)') || imgSrc.includes('setComboOptions($(",'), '镜像清单填充 ComboBox（R5b：img.mjs 调 setComboOptions 喂 dep-image）');
ok(!($s('img-list').innerHTML || '').includes('bob/other'), '普通用户：清单不含他人镜像（服务端按 user 过滤）');
ok(($s('img-sub').textContent || '') === '', 'S6：镜像库副题成功时保持空（「已加载 N 项」计数删除；清单以下拉 + 表格呈现）');
// S3：手动「刷新镜像清单」按钮随部署 fieldset 溶解而删除（plan-17 §0-5：显式刷新归零，TTL+写路径失效取代；
// 强制重取路径由 whoami admin 后 ensureImages(true) 与镜像删除后重取覆盖）

// —— §14.1/§14.3 空间条（上传区常显）+ 镜像清单删除（镜像库页）——
ok(spaceN >= 1, '进入发布区：/_api/space 已拉取（空间条常显）');
ok(($s('space-note').textContent || '') === '可用空间：20.0 GB',
  '空间条：显示 available−reserve 数值（40960−20480 MB → 20.0 GB；R5b 文案无机制自证）');
// S6：手动「刷新空间」按钮归零——镜像库页进入即强制校准（§14.1 进入本区自动刷新）
let spBase = spaceN;
spaceFail = true;
clickTab('img');
await sleep(40);
ok(($s('space-note').textContent || '') === '空间信息不可用', '空间拉取失败：静默显示「空间信息不可用」（不上顶部条、不阻断表单）');
spaceFail = false;
clickTab('img');
await sleep(40);
ok(spaceN > spBase && ($s('space-note').textContent || '').includes('20.0 GB'), '切镜像库页强制校准空间条（原「刷新空间」职责）');
clickTab('proj');
let ilist = $s('img-list:q').innerHTML || '';
ok(ilist.includes('alice/demo:v1') && ilist.includes('alice/demo2:v2') && ilist.includes('删除'),
  '镜像清单（img-list）：与部署 ComboBox 同源渲染 + 每行「删除」按钮');
const rmAct = (img) => fire($s('img-list'), 'click', { target: { closest: () => ({ dataset: { act: 'image-rm', img } }) } });
spBase = spaceN;
confirmMsgs.length = 0;
rmAct('alice/demo:v1');
await sleep(60);
ok(imageRmBodies.length === 1 && imageRmBodies[0].image === 'alice/demo:v1', '删除镜像：image-rm 提交体 {image:"<repo:tag>"}');
const rmConfirm = confirmMsgs.at(-1) || '';
ok(rmConfirm.includes('确定删除镜像 alice/demo:v1 吗') && rmConfirm.includes('不受影响') && rmConfirm.includes('回滚/重建将失效'),
  '删除镜像：confirm 文案含镜像名与影响说明（容器继续运行、回滚/重建失效）');
ok(($s('msg').textContent || '').includes('已回收约 512 MB'), '删除成功：顶部条「已回收约 N MB」（原 alert 改 showMsg 统一反馈通道）');
ilist = $s('img-list:q').innerHTML || '';
ok(!ilist.includes('alice/demo:v1') && ilist.includes('alice/demo2:v2'), '删除成功：清单刷新（该行消失、其余保留）');
ok(spaceN > spBase, '删除成功后：空间数字同步刷新');
confirmRet = false;
rmAct('alice/demo2:v2');
await sleep(40);
ok(imageRmBodies.length === 1 && !imagesRemoved.has('alice/demo2:v2'), 'confirm 取消：不提交 image-rm');
confirmRet = true;
imageRmFail = { error: 'E_NOTFOUND', msg: '镜像不存在（可能已被删除）' };
rmAct('alice/demo2:v2');
await sleep(60);
ok(imageRmBodies.length === 2 && ($s('msg').textContent || '').includes('E_NOTFOUND'), '删除失败：顶部条报错（含错误码）');
ok(($s('img-list:q').innerHTML || '').includes('alice/demo2:v2'), '删除失败：清单保留该镜像');

// —— §14.2 磁盘 E_QUOTA：顶部条追加删除引导并刷新空间条；内存类 E_QUOTA 不追加 ——
publishQuota = { msg: '磁盘空间不足：可用 10240 MB（已预留 20480 MB）＜ 估算需求 30720 MB（7680 MB × 膨胀因子 4）；请先删除旧镜像后重试' };
const pubProjKeep = $s('pub-project').value; // 保存当前（随机）项目名：单表单后 E_QUOTA 用例与 §11 共用 pub-project
$s('pub-project').value = 'demo5';
$s('pub-tag').value = 'v5';
spBase = spaceN;
fire($s('pub-form'), 'submit', { preventDefault() {} }); // plan-17 §5：单表单（原 dep-form）
await sleep(60);
let qmsg = $s('msg').textContent || '';
ok(qmsg.includes('E_QUOTA') && qmsg.includes('磁盘空间不足') && qmsg.includes('膨胀因子'),
  '磁盘 E_QUOTA：顶部条含服务端原文（可用/预留/估算需求/膨胀因子）');
ok(qmsg.includes('可到镜像库删除旧镜像后重试'), '磁盘 E_QUOTA：顶部条追加删除引导');
ok(spaceN > spBase, '磁盘 E_QUOTA：触发空间条刷新');
publishQuota = { msg: '可用内存不足：可用 512 MB（已预留 1536 MB）＜ 需求 2048 MB' };
fire($s('pub-form'), 'submit', { preventDefault() {} });
await sleep(60);
qmsg = $s('msg').textContent || '';
ok(qmsg.includes('E_QUOTA') && !qmsg.includes('可到镜像库删除旧镜像后重试'), '内存类 E_QUOTA：不追加磁盘删除引导');
$s('pub-project').value = pubProjKeep; // 还原（后续场景复用；含 §11 的随机名断言）

// —— tablist 方向键切换（role=tablist 键盘约定） ——
fire($s('tabs'), 'keydown', { key: 'ArrowRight', preventDefault() {}, target: tabBtn('proj') });
ok(!hidden('tab-pub') && hidden('tab-proj'), 'ArrowRight：proj → pub 切换（五标签）');
fire($s('tabs'), 'keydown', { key: 'ArrowLeft', preventDefault() {}, target: tabBtn('pub') });
ok(!hidden('tab-proj') && hidden('tab-pub'), 'ArrowLeft：pub → proj 切回（首尾循环）');

// —— §11 发布表单：随机项目名预填（「换一个」已删：建壳预填 + 留空提交自动生成兜底）+ URL 恒 /<project>/ ——
const rp1 = $s('pub-project').value;
ok(/^[a-zA-Z0-9]{8}$/.test(rp1), `发布表单预填随机 8 位项目名（${rp1}）`);
ok(!els.has('pub-shuffle'), '「换一个」按钮已删（预填 + 留空自动生成已全覆盖，无手动重随机入口）');
ok(($s('pub-url').textContent || '') === `/${rp1}/`, `URL 提示恒 /<项目名>/（当前 /${rp1}/）`);
$s('pub-project').value = 'demo'; // 手输（用户可清空手输的路径）
fire($s('pub-project'), 'input');
ok($s('pub-url').textContent === '/demo/', 'URL 提示随手输联动');
// —— S3 §5 参数单源预填：命中已有项目 → mem=服务端行值、tag/cport=sr.conf.* 记忆 + 注记 ——
store.set('sr.conf.demo', JSON.stringify({ tag: 'v9', cport: 9000, ts: Date.parse('2026-10-01T10:00:00') }));
$s('pub-project').value = 'demo';
fire($s('pub-project'), 'change');
ok($s('pub-mem').value === '256' && $s('pub-tag').value === 'v9' && $s('pub-cport').value === '9000',
  'S3 预填：mem=projects 行值（C4：MiB 裸数字展示）、tag/cport=sr.conf.* 记忆');
ok(($s('pub-pref-note').textContent || '').includes('沿用上次发布参数') && ($s('pub-pref-note').textContent || '').includes('10-01'),
  'S3 预填：注记「沿用上次发布参数（MM-DD）」');
$s('pub-project').value = 'nosuch';
fire($s('pub-project'), 'change');
ok(hidden('pub-pref-note'), 'S3 预填：未命中项目不显注记');
$s('pub-project').value = 'demo'; // 还原

// —— S4 行操作序 / 更多▾ / 更新预填 / 数据深链 / 删除诚实 confirm ——
{
  const rowHtml = $s('proj-tbl:q').innerHTML || '';
  const idx = (t) => rowHtml.indexOf(t);
  ok(!rowHtml.includes('>打开<') && idx('>发布<') >= 0
    && (idx('>停止<') > idx('>发布<') || idx('>启动<') > idx('>发布<')) && idx('更多') > idx('>发布<'),
    'S4：行操作固定顺序 发布·启动/停止·更多▾（R5 A2「打开」删除；R8 F2 与主动词统一）；日志经日志列 ▾ 单入口');
  ok(idx('回滚到上一版本') > idx('更多') && idx('>环境变量<') > idx('回滚') && idx('>数据<') > idx('>环境变量<') && idx('>删除<') > idx('>数据<'),
    'S4：更多▾菜单项序 回滚·环境变量·数据·删除');
  ok(rowHtml.includes('role="menu"') && rowHtml.includes('aria-haspopup'), 'S4：更多菜单 role=menu/aria-haspopup');
  ok(rowHtml.includes('data-act="logs"') && rowHtml.includes('aria-expanded="false"'), 'R5 C4：日志列 ▾ 按钮（aria-expanded 初态 false）');
}
fire($s('proj-tbl'), 'click', { target: { closest: () => ({ dataset: { act: 'update', p: 'demo' } }) } });
await sleep(20);
ok(!hidden('tab-pub') && hidden('tab-proj'), 'S4：行「更新」→ 跳发布页');
ok($s('pub-project').value === 'demo' && $s('pub-tag').value === 'v9' && String($s('pub-cport').value) === '9000' && $s('pub-mem').value === '256',
  'S4/验收 7：更新预填同一套参数（项目/内存/端口一致）');
ok(shown('pub-pref-note'), 'S4：更新预填注记可见（沿用上次发布参数）');
clickTab('proj');
fire($s('proj-tbl'), 'click', { target: { closest: () => ({ dataset: { act: 'dblink', p: 'demo' } }) } });
ok(!hidden('tab-db') && $s('db-project').value === 'demo', 'S4：数据深链 → 数据页并填项目（零请求）');
clickTab('proj');
confirmMsgs.length = 0;
fire($s('proj-tbl'), 'click', { target: { closest: () => ({ dataset: { act: 'rm', p: 'demo' } }) } });
await sleep(30);
{
  const rmC = confirmMsgs.at(-1) || '';
  ok(rmC.includes('demo') && rmC.includes('数据库将一并删除，如需保留请先在数据页备份下载 .dump'),
    'S4/验收 14：删除 confirm 诚实文案（数据库一并删除 + 数据页备份 .dump 指引）');
}

// —— S1 §6 对外 URL 策略：设置公网域名后主链接换 base；清空回退；非法域名内联报错 ——
$s('set-base').value = 'http://47.107.155.180:10100/';
fire($s('set-base'), 'input');
{
  const ptblBase = $s('proj-tbl:q').innerHTML || '';
  ok(ptblBase.includes('http://47.107.155.180:10100/demo/') && !ptblBase.includes('http://showroom.test/demo/'),
    'S1 §6：设置公网域名后展项主链接改用 base（尾斜杠去除）');
}
ok(hidden('proj-base-note'), 'S1 §6：配置 base 后「内网地址」提示条不出现');
$s('set-base').value = 'ftp://bad';
fire($s('set-base'), 'input');
ok(($s('set-base-err').textContent || '').includes('http(s)://') && store.get('sr.publicBase') === 'http://47.107.155.180:10100',
  'S1 §6：非法域名内联报错且保留上一个合法配置');
$s('set-base').value = '';
fire($s('set-base'), 'input');
ok(!store.has('sr.publicBase'), 'S1 §6：清空即回退当前访问地址（移除存储）');
{
  const ptblBack = $s('proj-tbl:q').innerHTML || '';
  ok(ptblBack.includes('http://showroom.test/demo/'), 'S1 §6：清空后主链接回退 origin');
}
ok(!els.has('pub-slot') || $s('pub-slot') === undefined, '发布表单无版本维度控件');

// ═══ 场景 6a：§10.4 平台事件流（S2：展项页可见即订阅，事件驱动列表自动刷新，离页退订） ═══
ok(await waitFor(() => eventsN >= 1, 5000), '登录落地展项页：自动订阅 /_api/events（零点击）');
ok(/^SR-SIG1 /.test(lastEventsAuth), '事件流为签名 GET（SR-SIG1，空 body 哈希）');
ok(await waitFor(() => projectsN >= 2, 5000), '事件到达 → 2 秒去抖自动刷新展项列表（验收 11：无手动刷新）');
ok(/5 秒后自动重连|已连接/.test($s('ev-state').textContent || ''), '事件流状态：流关闭（done）→ 5 秒重连或已重连');
const evTxt = $s('st-events').textContent || '';
ok(evTxt.includes('sr-demo') && evTxt.includes('启动') && evTxt.includes('转为健康'), '事件渲染：时间+动作+容器（含健康检查翻译）');
ok(!evTxt.includes('other-box'), '非 sr- 前缀容器被过滤');
ok(!evTxt.includes('hb') && !evTxt.includes('true'), '心跳行被忽略');
ok(/^\d{2}:\d{2}:\d{2}/.test(evTxt), '事件行带时间戳（HH:MM:SS）');
clickTab('pub');
ok(($s('ev-state').textContent || '').includes('未连接'), '离开展项页：事件流退订（服务端收尸子进程）');
clickTab('proj');
ok(await waitFor(() => eventsN >= 2, 5000), '回到展项页：自动重新订阅（第二次订阅）');
await sleep(80);
ok(($s('st-events').textContent || '').includes('sr-demo2') && ($s('st-events').textContent || '').includes('停止'), '重连后新事件继续滚动（最近 20 条）');

// ═══ 场景 6b：§10.4 日志实时流（S2：st-logs* 迁入行抽屉——行「日志」展开即恒跟随，R4 无开关 UI） ═══
fire($s('proj-tbl'), 'click', { target: { closest: () => ({ dataset: { act: 'logs', p: 'demo' } }) } });
await waitFor(() => logsN === 1, 3000);
ok($s('proj-tbl').dataset !== undefined, '行抽屉随「日志」展开（data-drawer 行插机制）');
ok(lastLogsUrl.includes('/_api/logs-stream?project=demo&tail=100'), `日志流请求仅按项目（${lastLogsUrl}）`);
ok(!lastLogsUrl.includes('slot'), '日志流请求无版本参数');
ok(/^SR-SIG1 /.test(lastLogsAuth), '日志流为签名 GET（SR-SIG1）');
await waitFor(() => ($s('st-logs').textContent || '').includes('log-1002'), 5000);
const logTxt = $s('st-logs').textContent || '';
const logRows = logTxt.split('\n');
ok(logRows.length === 1000, `1000 行环形帽（收 1003 行，保留 ${logRows.length}）`);
ok(logTxt.includes('log-0003') && !logTxt.includes('log-0002'), '超帽丢弃头部（log-0000..0002 被裁）');
ok(/^\d{2}:\d{2}:\d{2} log-/m.test(logTxt), '每行带格式化时间戳前缀');
ok(logsN === 1, '抽屉展开只发起一次日志流（恒跟随，无开关路径）');
click('drawer-close');
ok(!($s('st-logs-fb').textContent || '').includes('已收起抽屉') && !($s('st-logs-fb').textContent || '').includes('未跟随'),
  'S2：收起抽屉无「已收起/未跟随」类冗余回执（自动行为不播报；want 恒 true 无死分支）');
ok(logsN === 1, '收起抽屉 → 日志流随行断开（不再发起日志请求）');

// ═══ 场景 6c：§10.4 三段式备份下载（begin → 轮询两轮 → 两个 chunk → 解密落地） ═══
blobs.length = 0;
$s('db-project').value = 'demo';
click('btn-db-dump');
ok(await waitFor(() => ($s('db-prog-txt').textContent || '').includes('完成：已下载'), 15000), '备份完成：进度条文案播报（C10：db-fb 通道收敛删除）');
ok(beginBody && beginBody.project === 'demo' && !('slot' in beginBody), 'db-dump-begin：仅项目 + 一次性公钥');
ok(/^-----BEGIN PUBLIC KEY-----/.test(beginBody?.client_pub_pem || ''), '一次性 RSA 公钥 PEM 已上行');
ok(statusN === 2, `status 轮询两轮后 ready（实际 ${statusN}）`);
ok(chunkCalls.length === 2, `4MB 分片循环两个 chunk（实际 ${chunkCalls.length}）`);
ok(chunkCalls[0]?.off === 0 && chunkCalls[0]?.len === 4 * 1024 * 1024, '第一片 offset=0 len=4MiB');
ok(chunkCalls[1]?.off === 4 * 1024 * 1024 && chunkCalls[1]?.len === dumpEnv.env.length - 4 * 1024 * 1024, '第二片 offset=4MiB len=尾片');
ok($s('db-prog').value === $s('db-prog').max && Number($s('db-prog').max) === dumpEnv.env.length, '进度条按 offset/total 收满');
ok(($s('db-prog-txt').textContent || '') === '完成：已下载 demo.dump', 'F8：进度条完成文案只报文件名（信封字节/片数/解密字节数不再重复）');
ok(blobs.length === 1, '产物经 Blob 下载（createObjectURL 一次）');
{
  const got = new Uint8Array(await blobs[0].arrayBuffer());
  let same = got.length === dumpEnv.plain.length
    && got[0] === dumpEnv.plain[0] && got[got.length - 1] === dumpEnv.plain[dumpEnv.plain.length - 1];
  for (let i = 0; same && i < got.length; i += 4096) if (got[i] !== dumpEnv.plain[i]) same = false;
  ok(same, `分片信封经 wasm 流式解密还原明文（${got.length} 字节，抽样+首尾比对）`);
}
ok(($s('db-out').textContent || '') === 'pg_dump -Fc 导出完成（服务端原文）。', 'F8：db-out 保留服务端原文（j.out 优先；技术明细单通道归操作输出）');

// ═══ 场景 7：上传压缩统一（zstd）——发布表单压缩后上传 + 恢复上传魔数两态 ═══
{
  // 伪镜像 tar：9MB+123（跨 8MB 分片边界）周期性内容（可压缩）；独立实例按页面同粒度重压缩对账
  const CHUNK8 = 8 * 1024 * 1024;
  const raw = new Uint8Array(CHUNK8 + 123456);
  for (let i = 0; i < raw.length; i++) raw[i] = i % 251;
  const zref = zstdAll([raw.slice(0, CHUNK8), raw.slice(CHUNK8)]);
  ok(Buffer.from(zref.subarray(0, 4)).toString('hex') === '28b52ffd', 'compressToBlob 产物首 4 字节 = zstd 魔数（独立实例同产物）');
  ok(zref.length < raw.length, `压缩率 > 1（${raw.length} → ${zref.length} 字节）`);
  ok(zstdAll([raw.slice(0, CHUNK8), raw.slice(CHUNK8)]).every((b, i) => b === zref[i]), '压缩产物确定性（重算逐字节一致）');

  // S1 验收 6：未配置 PEM（模拟 whoami 未带回）选 tar 发布 → 内联「去设置」引导 → 设置对话框打开
  const fakeFile = (bytes, name) => ({
    name, size: bytes.length,
    slice: (a, b) => { const c = bytes.slice(a, Math.min(b, bytes.length)); return { arrayBuffer: async () => { compSnaps.push($s('dp-text').textContent || ''); return c.buffer; } }; },
  });
  $s('hd-pempub').value = ''; store.delete('sr.pempub'); // 模拟未配置（清掉 whoami 自动带回值）
  $s('pub-pem-hint').classList.add('hidden'); // 对齐真实 HTML 初始 hidden（惰性桩无 class 信息）
  $s('pub-project').value = 'demo9'; $s('pub-tag').value = 'v9';
  $s('pub-file').files = [fakeFile(new Uint8Array(1024), 'pem-guide.tar')];
  fire($s('pub-form'), 'submit', { preventDefault() {} });
  await sleep(40);
  ok(shown('pub-pem-hint'), 'S1 验收6：未配置 PEM 选 tar 发布 → 内联「去设置」引导出现（不发起上传）');
  ok(importBeginBody === null && publishN === 0, 'S1 验收6：引导路径未触达 import-begin/publish');
  click('btn-go-settings');
  ok($s('dlg-settings').open === true, 'S1 验收6：「去设置」→ 设置对话框打开');
  click('dlg-settings-close');
  ok($s('dlg-settings').open === false, 'S1 验收6：设置对话框可关闭');
  // 平台公钥（页面 seal 信封用）：node 生成 RSA-3072 SPKI PEM（引导测试后恢复真钥）
  const plat = generateKeyPairSync('rsa', { modulusLength: 3072 });
  $s('hd-pempub').value = plat.publicKey.export({ type: 'spki', format: 'pem' }).toString();

  // 7a. 一键发布（§12.1）：状态条四段流转——压缩 2 片 → 上传 2 chunk（数据片+收尾片）→ 发布 → 完成
  $s('pub-project').value = 'demo2';
  $s('pub-tag').value = 'v2';
  $s('pub-file').files = [fakeFile(raw, 'demo-img.tar')];
  dpSnaps.length = 0; compSnaps.length = 0; dpBeginSnap = null; dpPublishSnap = null;
  const spaceBase7a = spaceN;
  fire($s('pub-form'), 'submit', { preventDefault() {} });
  ok(await waitFor(() => publishN === 1, 30000), '一键发布：压缩 → 分片上传 → 发布全链路完成');
  ok($s('dp-s1').classList.contains('done') && $s('dp-s2').classList.contains('done')
    && $s('dp-s3').classList.contains('done') && $s('dp-s4').classList.contains('done'), '状态条①压缩②上传③发布④完成四段全部置为完成');
  ok(!$s('dp-bar').classList.contains('err') && hidden('dp-retry'), '成功路径：状态条无标红、无重试按钮');
  ok(shown('dp-done') && $s('dp-url').textContent === 'http://showroom.test/demo2/', '④完成段：大字 URL 展示（externalUrl → http://showroom.test/demo2/）');
  ok(compSnaps.some((t) => /① 压缩中.*%/.test(t)), `①压缩中（x%）进度文案出现（快照 ${compSnaps.filter((t) => t.includes('① 压缩中')).length} 条）`);
  ok(importBeginBody && importBeginBody.project === 'demo2' && importBeginBody.tag === 'v2', 'import-begin 携带项目与标签');
  ok(importBeginBody.total_bytes === sealedTotal(zref.length),
    `import-begin total_bytes 按压缩后长度申报（${importBeginBody.total_bytes} = 信封化 ${zref.length}）`);
  ok(uploadRec && uploadRec.zsize === zref.length && uploadRec.size === raw.length,
    '本地会话记录 zsize=压缩后 / size=原文件（续传重压缩对账用）');
  ok(importChunkLens.reduce((a, b) => a + b, 0) === importBeginBody.total_bytes, '分片字节总和 = 信封总长');
  ok(importEndN === 1 && !store.has('sr.up.up-smoke-1'), 'import-end 完成：服务端会话消费、本地记录清除');
  await sleep(40);
  ok(spaceN > spaceBase7a, 'import-end 成功后：空间条自动刷新（§14.1）');
  ok(dpSnaps.length === 2, `上传恰好 2 个 chunk 请求（1 数据片 + 1 收尾片，实际 ${dpSnaps.length}）`);
  ok(dpSnaps.every((s) => s.m === importBeginBody.total_bytes), '②上传段进度 max = 压缩后信封总长');
  ok(dpSnaps[0].v === 0 && dpSnaps[0].s2 === true, '首片请求时：②段进行中（on）、进度 0');
  ok(/已传 \d+ \/ 压缩后总长/.test(dpSnaps[1].txt) && dpSnaps[1].v > 0, '②上传段文案「已传 / 压缩后总长」且进度推进');
  ok(dpBeginSnap && dpBeginSnap.txt.includes('② 上传中'), 'import-begin 时已切入②上传段');
  ok(dpPublishSnap && dpPublishSnap.txt.includes('③ 发布中'), 'publish 请求时处于③发布中');
  ok(($s('dp-text').textContent || '').includes('④ 完成：发布成功'), '最终文案为④完成（发布成功；发布结果仅在状态条播报，顶部条不重复）');
  click('dp-copy');
  ok(($s('msg').textContent || '').includes('已复制'), '④完成段「复制地址」按钮可用（剪贴板降级路径）');

  // 7a-2. 项目留空自动生成回填 + 发布失败标红 → 重试仅重发发布
  publishFailOnce = true;
  $s('pub-project').value = '';
  $s('pub-file').files = [];
  fire($s('pub-form'), 'submit', { preventDefault() {} });
  ok(await waitFor(() => shown('dp-retry'), 8000), '发布失败：出现重试按钮');
  ok($s('dp-bar').classList.contains('err'), '发布失败：状态条标红');
  ok((($s('dp-text').textContent || '').includes('发布')) && ($s('dp-text').textContent || '').includes('失败'), '失败文案含「发布/失败」与错误说明');
  ok(/^[a-zA-Z0-9]{8}$/.test(lastPublishBody.project) && $s('pub-project').value === lastPublishBody.project,
    `项目名留空：自动生成随机 8 位并回填表单（${lastPublishBody.project}）`);
  ok(($s('pub-url').textContent || '') === `/${lastPublishBody.project}/`, 'URL 提示随自动生成名联动');
  ok($s('dp-s1').classList.contains('skip') && $s('dp-s2').classList.contains('skip'), '未选文件：①②段标记为跳过');
  click('dp-retry');
  ok(await waitFor(() => publishN === 2 && shown('dp-done'), 10000), '重试：仅重发发布并完成');
  ok($s('dp-url').textContent === `http://showroom.test/${lastPublishBody.project}/`, '完成 URL 与自动生成项目名一致（externalUrl）');

  // 7a-3. 上传失败 → 状态条标红提示断点续传 → 重试走 import-status 断点补传
  const small = new Uint8Array(300000);
  for (let i = 0; i < small.length; i++) small[i] = i % 251;
  chunkFailN = 3; // 首片三连败（页面内部重试 3 次后放弃）
  $s('pub-project').value = 'demo3';
  $s('pub-tag').value = 'v3';
  $s('pub-file').files = [fakeFile(small, 'small-img.tar')];
  const importStatusBase = importStatusN;
  fire($s('pub-form'), 'submit', { preventDefault() {} });
  ok(await waitFor(() => shown('dp-retry'), 20000), '上传失败：出现重试按钮（片级 3 次重试后放弃）');
  ok($s('dp-bar').classList.contains('err') && ($s('dp-text').textContent || '').includes('从断点继续'), '上传失败：标红并提示从断点继续（F9：机制词「保留已传分片」清除）');
ok(!($s('dp-text').textContent || '').includes('分片'), 'F9：上传失败文案无「分片」字样');
  ok([...store.keys()].some((k) => k.startsWith('sr.up.')), '上传会话已登记本地（upload_id 可续传）');
  ok(publishN === 2, '上传失败未触达发布');
  chunkFailN = 0;
  click('dp-retry');
  ok(await waitFor(() => publishN === 3 && shown('dp-done'), 30000), '重试：断点续传补传后自动发布完成');
  ok(importStatusN > importStatusBase, '重试经 import-status 查询已收分片（断点）');
  ok(![...store.keys()].some((k) => k.startsWith('sr.up.')), '续传成功后本地会话记录清除');
  ok($s('dp-url').textContent === 'http://showroom.test/demo3/', '重试后完成 URL 正确（externalUrl）');

  // 7a-4. §13.1 入口二「仅上传」：同一 tar 选择，只压缩上传入库——无 publish 调用、③④ skip
  $s('pub-project').value = 'demo4';
  $s('pub-tag').value = 'v4';
  $s('pub-file').files = [];
  fire($s('btn-upload-only'), 'click', { preventDefault() {} });
  ok(($s('pub-file-err').textContent || '').includes('仅上传'), '仅上传未选文件：字段旁内联错误（含下一步指引）');
  ok(importEndN === 2, '仅上传未选文件：未发起上传请求');
  const small2 = new Uint8Array(200000);
  for (let i = 0; i < small2.length; i++) small2[i] = i % 251;
  $s('pub-file').files = [fakeFile(small2, 'only-up.tar')];
  const pubBase = publishN, imgBase = imagesN;
  fire($s('btn-upload-only'), 'click', { preventDefault() {} });
  ok(await waitFor(() => importEndN === 3, 20000), '仅上传：压缩 → 分片上传 → import-end 完成');
  await sleep(80);
  ok(publishN === pubBase, '仅上传：全程无 publish 调用（不部署）');
  ok($s('dp-s1').classList.contains('done') && $s('dp-s2').classList.contains('done'), '仅上传：①压缩②上传两段完成');
  ok($s('dp-s3').classList.contains('skip') && $s('dp-s4').classList.contains('skip'), '仅上传：③发布④完成标记为跳过');
  ok(($s('dp-text').textContent || '').includes('镜像已入库'), '仅上传完成文案：「镜像已入库」');
  ok(($s('dp-text').textContent || '').includes('镜像库'), '仅上传完成文案：指引稍后到「发布」页选「镜像库」源');
  ok(hidden('dp-done'), '仅上传：不展示部署完成 URL 段');
  ok(imagesN > imgBase, '仅上传完成后自动刷新镜像清单（新镜像即刻可选）');
  ok(!$s('btn-upload-only').disabled && $s('btn-upload-only').textContent === '仅上传', '请求结束：按钮恢复原文与可用');

  // 7c. plan-17 §5 镜像库源：radio 切换 → ComboBox 选中镜像带出项目/标签 → ①②skip 直接发布
  fire($s('pub-src-registry'), 'change');
  ok(hidden('pub-src-file-box') && !hidden('pub-src-reg-box') && $s('btn-upload-only').disabled,
    '镜像库源：tar 框收起、镜像下拉出现、「仅上传」禁用');
  $s('dep-image').value = 'alice/demo:v1';
  fire($s('dep-image'), 'input');
  ok($s('pub-project').value === 'demo' && $s('pub-tag').value === 'v1', '选中镜像：项目与标签自动带出到发布表单（user/project:tag 解析）');
  $s('pub-mem').value = '512m'; $s('pub-cport').value = '9090';
  fire($s('pub-form'), 'submit', { preventDefault() {} });
  ok(await waitFor(() => $s('dp-s4').classList.contains('done'), 8000), '镜像库源：直接发布完成（③④ done）');
  ok($s('dp-s1').classList.contains('skip') && $s('dp-s2').classList.contains('skip'), '镜像库源：①压缩②上传自动跳过');
  ok(lastPublishBody.project === 'demo' && lastPublishBody.tag === 'v1'
    && lastPublishBody.mem === '512m' && lastPublishBody.container_port === 9090,
    '镜像库源提交体：镜像带出的项目/标签 + 内存需求 + 监听端口');
  ok(($s('dp-url').textContent || '').includes('/demo/'), '镜像库源：完成大字 URL 含访问地址');
  $s('pub-project').value = 'bad_name!';
  fire($s('pub-form'), 'submit', { preventDefault() {} });
  ok(($s('pub-project-err').textContent || '').includes('项目名仅限'), '发布表单：项目校验为字段内联错误（R5b：用户语言，无正则/保留名）');
  fire($s('pub-src-tar'), 'change');
  ok(!hidden('pub-src-file-box') && hidden('pub-src-reg-box') && !$s('btn-upload-only').disabled, '切回 tar 源：文件框恢复、仅上传可用');
  $s('pub-project').value = 'demo'; // 还原

  // 7b. 恢复上传魔数两态：已是 zstd（备份下载产物）原样；原生 -Fc 压缩后
  const plain = new Uint8Array(5 * 1024 * 1024 + 77); // > 4MiB：恢复信封跨 seal 段
  for (let i = 0; i < plain.length; i++) plain[i] = (i * 31 + 7) & 0xff;
  const dumpZ = zstdAll([plain]); // 服务端 dump 形态：整体单帧 zstd 包裹
  ok(dumpZ.length < plain.length, `已压缩态样本可压缩（${plain.length} → ${dumpZ.length}）`);
  $s('db-project').value = 'demo';
  $s('db-restore-file').files = [fakeFile(dumpZ, 'demo.dump')];
  click('btn-db-restore');
  ok(await waitFor(() => restoreLens.length === 1, 20000), '恢复上传（已压缩态）完成');
  ok(restoreLens[0] === sealedTotal(dumpZ.length), `已压缩态：信封按原样字节核算（${restoreLens[0]}）`);
  ok(($s('db-out').textContent || '').includes('无需压缩'), 'F9：已压缩态回显「无需压缩，直接上传」（原「已是 zstd 原样上传」）');

  // 未压缩态：原生 -Fc 形态（PGDMP 头，魔数非 zstd → 页面压缩后上传）
  const fcAll = new Uint8Array(plain.length);
  fcAll.set(plain); fcAll.set(enc2.encode('PGDMP'), 0);
  const zrefB = zstdAll([fcAll.slice(0, CHUNK8), fcAll.slice(CHUNK8)]); // 与页面同压缩粒度
  $s('db-restore-file').files = [fakeFile(fcAll, 'demo-fc.dump')];
  click('btn-db-restore');
  ok(await waitFor(() => restoreLens.length === 2, 20000), '恢复上传（未压缩态）完成');
  ok(restoreLens[1] === sealedTotal(zrefB.length), `未压缩态：信封按压缩后字节核算（${restoreLens[1]}）`);
  ok(restoreLens[1] < sealedTotal(fcAll.length), '未压缩态经压缩后上传（信封显著小于原样信封）');
  ok(($s('db-out').textContent || '').includes('压缩后'), '未压缩态回显「压缩后」');
}

// ═══ 场景 2：同浏览器再访（钥匙+账号记忆）→ 签名自测直达（免密码）；§12.2 管理员态 ═══
whoamiAdmin = true; // 本次登录的钥匙是管理员钥匙
imagesAdminView = true; // 服务端对 admin 钥匙返回全量镜像（§13.1）
const ab2 = authBeginN, kb2n = keyBindBodies.length;
els.clear();
await load(2);
armBtnText(); // 发卡按钮初始文案（busy 恢复断言用）
armLoginInit(); // 与真实登录 HTML 初始 hidden 对齐（B2 断言「免密码进入」不出现的前提）
await sleep(80);
ok(hidden('login') && shown('app'), '再访（钥匙+账号记忆）：签名自测通过自动进入（免密码）');
ok(authBeginN === ab2 && keyBindBodies.length === kb2n, '再访直达：零 auth 请求（不重走密码登录）');
ok(store.get('sr.seed') === seed1 && store.get('sr.user') === 'alice', '种子与账号未被改动');
await sleep(60); // whoami 异步返回
ok(els.has('ad-users-tbl') && !els.has('ad-st-auto'),
  'whoami admin=true：管理区 DOM 渲染（第五「管理」标签随 whoami 构建）；自动刷新恒开无开关（R4）');
ok(shown('rd-admin-clause'), 'F11：管理员渲染「管理员可见全部逻辑库」半句（whoami 结论拼入 Redis 静态句）');

// —— §13.1/§13.2 管理员全量视角：镜像清单（whoami 回退）+ Redis 库（admin_view 全量）——
ok(!els.has('img-scope') && !els.has('proj-scope'),
  'R4 裁决：镜像库/展项列表标题旁无「管理员视图」徽章（顶栏「管」角标唯一）');
ok(imgSrc.includes('setComboOptions($(' + String.fromCharCode(39) + 'dep-image' + String.fromCharCode(39) + '), images)') && imgSrc.includes('renderImageList(images);'),
  'admin 全量镜像（服务端按身份返回；前端 setComboOptions+renderImageList 原样呈现，不改数据）');

// —— 管理页（R5 E1：五面板二级导航——账号|全部项目|平台状态|宿主配置|实时事件）——
clickTab('admin');
await sleep(80);
ok(els.has('tab-admin'), '第五标签面板（tab-admin）已进入渲染');
ok(!hidden('adp-users') && hidden('adp-projects') && hidden('adp-status') && hidden('adp-host') && hidden('adp-events'),
  'E1：管理页默认「账号」面板（五面板互斥显示）');
ok(adminUsersN >= 1, '进入管理页（默认账号面板）：users 自动拉取');
const otbl = $s('ad-users-tbl:q').innerHTML || '';
ok(otbl.includes('alice') && otbl.includes('kid-a1') && otbl.includes('kid-b1'), 'F2：账号表：账号 + 设备（kid 标识）渲染');
ok(/data-act="userunadmin" data-user="alice"/.test(otbl) && /data-act="useradmin" data-user="bob"/.test(otbl),
  'D1：设/免管理员在账号行（alice=管理员徽标+取消 / bob=设为）');
ok(!otbl.includes('>普通<') && !/data-act="(un)?admin" data-kid=/.test(otbl),
  'D1：设备行「管理员/普通」列与设备级设免按钮零残留');
ok(otbl.includes('2026/09/01') && otbl.includes('项目数 2'), '登录时间与项目数展示');
ok(otbl.includes('吊销整个账号') && otbl.includes('移除此设备'), '行内管理动作按钮渲染（吊销账号/移除此设备）');
ok(otbl.includes('设备 2 台') && !otbl.includes('钥匙') && !otbl.includes('把</span>'), 'F2：设备词族收口（「设备 N 台」；钥匙 N 把/删除钥匙零残留）');
// E3：全部项目面板（跨用户表 + owner 列 + 行操作）
const admProjBase = adminProjectsN;
clickAdp('projects');
await sleep(80);
ok(adminProjectsN === admProjBase + 1 && hidden('adp-users') && !hidden('adp-projects'), 'E4：切「全部项目」面板即拉取 admin/projects（互斥显示）');
{
  const aptbl = $s('ad-projects-tbl:q').innerHTML || '';
  ok(aptbl.includes('>demo<') && aptbl.includes('>bobapp<') && aptbl.includes('>bob<'),
    'E3：跨用户项目表渲染（项目 + 所属账号列）');
  ok(/data-act="adstop" data-p="demo" data-owner="alice"/.test(aptbl) && /data-act="adstart" data-p="bobapp" data-owner="bob"/.test(aptbl),
    'E3：行操作按状态路由（运行中→停止 / 已退出→启动）+ 删除按钮');
  confirmMsgs.length = 0;
  projActs.length = 0;
  fire($s('ad-projects-tbl'), 'click', { target: { closest: () => ({ dataset: { act: 'adstop', p: 'demo', owner: 'alice' } }) } });
  await sleep(60);
  const stopAsk = confirmMsgs.at(-1) || '';
  ok(stopAsk.includes('alice') && stopAsk.includes('demo') && stopAsk.includes('停止'), 'E3：跨用户停止 confirm 写明账号与项目');
  ok(projActs.length === 1 && projActs[0].act === 'stop' && projActs[0].body.project === 'demo',
    'E3：行操作复用 /_api/{start,stop,rm} 端点（服务端按 owner 校验管理员放行）');
}
// E4：平台状态面板独立进入刷新
const adStBase = adminStatusN;
clickAdp('status');
await sleep(80);
ok(adminStatusN === adStBase + 1 && hidden('adp-projects') && !hidden('adp-status'), 'E4：切「平台状态」面板即拉取 status（互斥显示）');
const stHtml = $s('ad-status').innerHTML || '';
ok(stHtml.includes('showroom-caddy') && stHtml.includes('showroom-redis'), '平台状态：四服务状态灯渲染');
ok(stHtml.includes('运行中') && stHtml.includes('未运行'), '服务状态两态（active/inactive 中文）');
ok(stHtml.includes('3.3 GB') && stHtml.includes('42.0 GB') && stHtml.includes('4096'),
  '磁盘与内存数字展示（disk+df -B1 字节 → GB 人读「已用/共」+ MemAvailable）');
ok(stHtml.includes('sr-demo') && stHtml.includes('sr-hello') && stHtml.includes('已退出'), '容器列表渲染（状态中文化）');
ok(stHtml.includes('showroom-snapshot-hourly') && stHtml.includes('13:00'), 'timer 下次触发时间展示');
ok(html.includes('adTimer = setInterval(() => { refreshAdminStatus(); refreshHost(); }, 30000);')
  && html.includes("case 'admin': await loadAdminUsers(); await refreshAdminStatus(); break;"),
  'R4：状态面板与主机期望状态 30 秒自动刷新恒开（无开关 UI；#hs-refresh 手动刷新按钮已删，host 刷新并入定时器）');

// —— 发卡对话框（plan-16 §1.5 主路径）：账号 + 打开时自动生成并暂存的初始密码 → user-add {user,password} ——
click('ad-user-add');
ok($s('dlg-user-add').open === true, '发卡对话框打开（打开即客户端生成并暂存初始密码，不展示）');
click('ad-na-submit'); // 全空 → 账号名校验
ok(userAddBody === null && ($s('ad-na-user-err').textContent || '').includes('账号名'), '发卡校验：账号名不合法即内联拒绝（未发请求）');
$s('ad-na-user').value = 'newbie';
wireTexts.length = 0;
click('ad-na-submit'); // 密码已在打开对话框时自动生成：无需任何前置生成步骤
await sleep(60);
ok(userAddBody && userAddBody.user === 'newbie' && /^[a-zA-Z0-9]{16}$/.test(userAddBody.password || '')
  && !('pubkey' in userAddBody) && !('admin' in userAddBody),
  `发卡提交体（主路径）：{user, password=打开时自动生成 16 位（${userAddBody?.password?.slice(0, 4)}…）}（无 pubkey；未勾选管理员不带 admin）`);
const genPw1 = userAddBody.password;
ok(shown('ad-na-result') && $s('ad-na-login-pw').textContent === genPw1 && $s('ad-na-pw').textContent === 'reg-pw-smoke',
  '发卡结果两行：登录初始密码（一次性展示）+ registry 密码（服务端一次性返回）');
ok(!els.has('ad-na-gen') && !els.has('ad-na-gen-box') && !els.has('ad-na-gen-err'),
  '「自动生成强密码」按钮与生成展示区/前置校验已删（密码仅在发放结果区一次性展示）');
ok(!wireTexts.filter((t) => !t.includes('/_api/admin/user-add')).some((t) => t.includes(genPw1)),
  '发卡上行：初始密码仅出现在 user-add 管理员签名通道（不进 /_api/auth/* 等其他请求体）');
ok(!$s('ad-na-submit').disabled && $s('ad-na-submit').textContent === '发放', '发卡请求结束：按钮恢复原文与可用');
$s('ad-na-user').value = 'newbie2';
$s('ad-na-admin').checked = true;
click('ad-na-submit');
await sleep(60);
ok(userAddBody.admin === true && userAddBody.user === 'newbie2'
  && /^[a-zA-Z0-9]{16}$/.test(userAddBody.password || '') && userAddBody.password !== genPw1,
  '连续发放：勾选「同时授予管理员」→ admin:true；第二轮自动换新密码（旧密码不残留误发）');
// B7：原发卡对话框「专家路径：直接登记公钥」折叠（ad-na-pub/pubsubmit/kid-note）已整体删除
ok(!html.includes('id="ad-na-expert"') && !html.includes('id="ad-na-pub"') && !html.includes('登记公钥发放'),
  'B7：发卡对话框公钥直登记路径零残留（发卡恒设初始密码）');
click('ad-na-close');
ok($s('dlg-user-add').open === false, '发卡对话框关闭');

// —— 行内管理动作：中文 confirm 文案 + 提交体（user-admin / key-rm / user-revoke） ——
confirmMsgs.length = 0;
const act = (a, user, kid) => fire($s('ad-users-tbl'), 'click', { target: { closest: () => ({ dataset: { act: a, user, kid } }) } });
act('keyrm', 'bob', 'kid-b1');
await sleep(60);
ok(keyRmBodies.length === 1 && keyRmBodies[0].user === 'bob' && keyRmBodies[0].kid === 'kid-b1', '移除此设备：key-rm 提交体 {user,kid}');
{
  const krmAsk = confirmMsgs.at(-1) || '';
  ok(krmAsk.includes('移除') && krmAsk.includes('无法使用') && krmAsk.includes('重新登录'),
    'F2：移除设备 confirm 设备语言（立即无法使用 + 重新登录即可重新登记）');
  ok(!krmAsk.includes('钥匙') && !krmAsk.includes('绑定') && !krmAsk.includes('密钥') && !krmAsk.includes('删除'),
    'F2：移除设备 confirm 无钥匙/绑定/密钥/删除字样');
}
act('userunadmin', 'alice');
await sleep(60);
ok(userAdminBodies.some((b) => b.admin === false && b.user === 'alice' && !('kid' in b)),
  'R5 D1：取消管理员走账号级 user-admin（{user,admin:false}，无 kid；key-admin 垫片已移除）');
act('useradmin', 'bob');
await sleep(60);
ok(userAdminBodies.some((b) => b.admin === true && b.user === 'bob'),
  'R5 D1：设为管理员走账号级 user-admin（{user,admin:true}）');
act('revoke', 'bob');
await sleep(60);
ok(revokeBodies.length === 1 && revokeBodies[0].user === 'bob', '吊销整个账号：user-revoke 提交体 {user}');
confirmRet = false; // confirm 拒绝：动作不得提交
act('keyrm', 'bob', 'kid-b1');
await sleep(60);
ok(keyRmBodies.length === 1, 'confirm 取消：管理动作不提交');
confirmRet = true;

// —— 账号表 has_password 徽标（plan-16 §1.5）：未设密码灰徽标 + 行内「补发密码」→ 一次性新密码 ——
const tbPw = $s('ad-users-tbl:q').innerHTML || '';
ok(tbPw.indexOf('已设密码') >= 0 && tbPw.indexOf('未设密码') > tbPw.indexOf('已设密码'),
  'has_password 徽标两态：alice=已设密码 / bob=未设密码（按行对应）');
ok(/data-act="pwset" data-user="bob"/.test(tbPw) && !/data-act="pwset" data-user="alice"/.test(tbPw),
  '「补发密码」按钮仅出现在未设密码账号行（已设密码行无此按钮）');
confirmMsgs.length = 0;
act('pwset', 'bob');
await sleep(60);
const psConfirm = confirmMsgs.at(-1) || '';
ok(psConfirm.includes('bob') && psConfirm.includes('补发') && psConfirm.includes('失效') && !psConfirm.includes('仅显示一次'),
  '补发密码：confirm 中文文案含账号名与失效语义（「仅显示一次」只在对话框内保留一处）');
ok(psConfirm.includes('其他设备不受影响') && !psConfirm.includes('钥匙') && !psConfirm.includes('绑定'),
  'F2：补发密码 confirm 设备语言（其他设备不受影响；无钥匙/绑定词）');
ok(passwdSetBodies.length === 1 && passwdSetBodies[0].user === 'bob' && /^[a-zA-Z0-9]{16}$/.test(passwdSetBodies[0].password),
  '补发提交体：{user, password=<crypto 随机 16 位>}');
ok($s('dlg-passwd-set').open === true && ($s('ad-ps-user').textContent || '') === 'bob'
  && $s('ad-ps-pw').textContent === passwdSetBodies[0].password,
  '补发结果：新登录密码一次性展示（对话框醒目提示 + 回显服务端返回值）');
click('ad-ps-copy');
ok(($s('msg').textContent || '').includes('已复制'), '补发结果「复制密码」按钮可用');
await sleep(60); // loadAdminUsers 刷新（mock 侧 has_password 已翻转）
const tbPw2 = $s('ad-users-tbl:q').innerHTML || '';
ok(!tbPw2.includes('未设密码') && !/data-act="pwset"/.test(tbPw2),
  '补发成功后：账号表刷新，徽标翻转为「已设密码」（补发按钮消失）');
click('ad-ps-close');
ok($s('dlg-passwd-set').open === false, '补发对话框关闭');
confirmRet = false; // confirm 取消：不提交 passwd-set
act('pwset', 'bob');
await sleep(40);
ok(passwdSetBodies.length === 1, 'confirm 取消：补发不提交');
confirmRet = true;

// —— admin 接口 404 E_AUTH（权限被收回等异常）：顶部条报错 ——
adminFail404 = true;
clickTab('admin'); // 重进管理页 → 当前面板（status）按需刷新 → 404 走顶部条
await sleep(60);
ok(($s('msg').textContent || '').includes('E_AUTH'), 'admin 接口 404 E_AUTH：顶部条报错');
adminFail404 = false;

// —— §15.1/§15.2 主机编排（E4：宿主配置面板独立进入即拉取）——
// 桩对齐真实 HTML 初始 class（惰性元素桩无 class 信息；F3 keepEdits 依 hs-review 初始 hidden 判定预演不在架）
for (const id of ['hs-review', 'hs-report', 'hs-confirm']) $s(id).classList.add('hidden');
clickAdp('host');
await sleep(80);
ok(hostGetN >= 1, '切「宿主配置」面板：/_api/admin/host 自动拉取（期望状态）');
ok($s('hs-mw-postgres').checked && $s('hs-mw-redis').checked && $s('hs-mw-registry').checked,
  '中间件集合：三开关按 host_env.MIDDLEWARE 勾选');
ok(!els.has('hs-mw-redis-badge'), '中间件「已启用」徽标已删（与 checkbox 勾选态同源重复，勾选即真相）');
const hsKv = $s('hs-kv-tbl:q').innerHTML || '';
ok(hsKv.includes('FRP') && hsKv.includes('running'), 'host.env 其余键通用键值渲染（FRP 项原样显示，不特判）');
ok(hsKv.includes('CADDY') && hsKv.includes('LAN_HOST') && hsKv.includes('sysctl'), 'host.env 键值表含 CADDY/LAN_HOST/TUNING');
ok($s('hs-p-redisdbs').value === '256' && $s('hs-p-disk').value === '20'
  && $s('hs-p-mem').value === '1536' && $s('hs-p-factor').value === '4',
  '参数当前值回填（REDIS_DBS/DISK_RESERVE_GB/RESERVE_MEM_MB/UPLOAD_SPACE_FACTOR）');
const hsDrift = $s('hs-drift').innerHTML || '';
ok(hsDrift.includes('sr-redis') && hsDrift.includes('warnbar') && hsDrift.includes('--databases 16') && hsDrift.includes('建议'),
  '漂移清单黄条：条目 + 期望/实际 + 建议命令渲染');
ok(!hidden('hs-pool-ok') && hidden('hs-pool-warn'), '池状态一（余量充足）：常态数字行、无黄条');
ok(($s('hs-pool-ok').innerHTML || '').includes('27') && ($s('hs-pool-ok').innerHTML || '').includes('256')
  && ($s('hs-pool-ok').innerHTML || '').includes('229'), '池状态一：已用/共/余 数字展示（tabular-nums 容器）');

// 两段式 A：仅改参数（impact 无受影响项目）——预演不落盘、确认后才 POST confirm=true
$s('hs-p-mem').value = '2048';
hostPostBodies.length = 0;
confirmMsgs.length = 0;
click('hs-apply');
await sleep(60);
const hsPv = hostPostBodies.find((b) => b.confirm === false);
ok(hsPv && hsPv.host_env && hsPv.host_env.MIDDLEWARE === 'postgres redis registry',
  '查看变更：POST confirm=false（开关合成空格集合）');
ok(hsPv && hsPv.host_env.FRP === 'running' && hsPv.host_env.CADDY === 'yes', '查看变更：host_env 其余键全量透传（FRP 字符串透传 / CADDY bool→yes/no 字符串规范化）');
ok(hsPv && hsPv.policy && hsPv.policy.RESERVE_MEM_MB === 2048 && hsPv.policy.REDIS_DBS === 256,
  '查看变更：policy 全量回传（改值覆写、REDIS_DBS 原样——它只走扩容流）');
ok(!hostPostBodies.some((b) => b.confirm === true), '两段式：无 confirm 不执行（无 confirm=true 请求）');
ok(!hidden('hs-review') && hidden('hs-report') && !hidden('hs-confirm'), '预演后：变更清单与「确认执行」出现、报告隐藏');
const hsAtbl = $s('hs-actions-tbl:q').innerHTML || '';
ok(hsAtbl.includes('删容器') && hsAtbl.includes('sr-redis') && hsAtbl.includes('起容器') && hsAtbl.includes('sr-postgres'),
  'actions 人读行：kind 中文化 + 目标渲染（needed=false 行亦列出）');
ok(($s('hs-review-sum').textContent || '').includes('3 项动作') && ($s('hs-review-sum').textContent || '').includes('2 项需执行'),
  '预演摘要：N 项动作 / M 项需执行');
ok(!($s('hs-impact').innerHTML || '').includes('warnbar err'), '无受影响项目：不出现红色警示');
click('hs-confirm');
await sleep(100);
ok(confirmMsgs.length === 0, '无受影响项目：确认执行不弹二次 confirm');
const hsRunA = hostPostBodies.find((b) => b.confirm === true);
ok(hsRunA && hsRunA.policy && hsRunA.policy.RESERVE_MEM_MB === 2048, '确认执行：POST confirm=true（同一期望状态体）');
const hsRep = $s('hs-report').innerHTML || '';
ok(!hidden('hs-report') && hsRep.includes('执行报告') && hsRep.includes('完成 2 项'), '执行报告渲染（完成 N 项）');
ok(hsRep.includes('成功') && hsRep.includes('sr-redis'), '报告行含成功标记与目标');
ok(await waitFor(() => hostGetN >= 2, 3000), '执行后自动重取期望状态（面板收敛）');
ok(!$s('hs-confirm').disabled && $s('hs-confirm').textContent === '确认执行', '确认执行按钮恢复原文与可用');
ok(!($s('msg').textContent || '').includes('主机变更已执行') && ($s('hs-fb').textContent || '').includes('执行完成'),
  'F7：主机执行单通道（面板 fb「执行完成」唯一播报；顶部条不再重复）');

// R5 B4：预演后任一编辑控件 change → 预演清单即失效（收起 + 提示重新预演）
hostPostBodies.length = 0;
confirmMsgs.length = 0;
click('hs-apply'); // 先造一份在架预演
await sleep(60);
ok(!hidden('hs-review') && !hidden('hs-confirm'), 'B4 前置：预演清单与确认钮在架');
fire($s('hs-p-disk'), 'change');
ok(hidden('hs-review') && hidden('hs-confirm') && ($s('hs-fb').textContent || '').includes('配置已修改，请重新预演'),
  'B4：预演后编辑参数 → 清单/确认钮收起 + 提示「配置已修改，请重新预演」');
$s('hs-fb').textContent = ''; // 清掉上一条提示再验静默路径
fire($s('hs-mw-redis'), 'change'); // 收起态编辑静默（不误报）
ok(!($s('hs-fb').textContent || '').includes('配置已修改'), 'B4：未预演时编辑不弹提示（收起态静默）');

// 两段式 B：禁用 redis（impact 含受影响项目）→ 红色警示 + 二次 confirm
$s('hs-mw-redis').checked = false;
hostPostBodies.length = 0;
confirmMsgs.length = 0;
click('hs-apply');
await sleep(60);
const hsImp = $s('hs-impact').innerHTML || '';
ok(hsImp.includes('warnbar err') && hsImp.includes('demo') && hsImp.includes('demo2'),
  '禁用中间件且 impact 含受影响项目：红色警示渲染（列项目名）');
ok(hsImp.includes('redis'), 'impact 标注将禁用的中间件');
ok(hidden('hs-report'), '新一轮预演：上一轮报告收起');
confirmRet = false; // 二次 confirm 取消：不得提交
click('hs-confirm');
await sleep(60);
ok(!hostPostBodies.some((b) => b.confirm === true), '二次 confirm 取消：不执行（无 confirm=true 请求）');
const hsAsk = confirmMsgs.at(-1) || '';
ok(hsAsk.includes('demo') && hsAsk.includes('demo2') && hsAsk.includes('确认'), '二次 confirm 文案列受影响项目并请求确认');
confirmRet = true;
click('hs-confirm');
await sleep(100);
ok(hostPostBodies.some((b) => b.confirm === true && b.host_env.MIDDLEWARE === 'postgres registry'),
  '二次 confirm 通过：confirm=true 执行（redis 已从集合移除）');

// 池两态之二：余量 <20% → 黄条 + 「扩容」按钮
hostResp = { ...HOST_STATE, redis_pool: { used: 250, total: 256, headroom: 6, suggest_expand: true } };
clickAdp('host'); // E4：重进「宿主配置」面板即重取期望状态（手动刷新按钮已删）
await sleep(60);
ok(!hidden('hs-pool-warn') && hidden('hs-pool-ok'), '池状态二（<20% 余量）：黄条出现、常态行隐藏');
ok(($s('hs-pool-warn-txt').textContent || '').includes('余量不足') && ($s('hs-pool-warn-txt').textContent || '').includes('6/256'),
  '黄条文案：「剩 X/Y」建议扩容');
ok(!hidden('hs-pool-expand'), '黄条带「扩容」按钮');

// §15.3 扩容对话框流：非法值内联拒绝 → 提示原文逐字节 → 「我已了解，执行扩容」→ 结果回报
hostPostBodies.length = 0;
click('hs-pool-expand');
ok($s('dlg-redis-x').open === true, '扩容对话框打开');
ok(hidden('hs-rx-run') && hidden('hs-rx-prompt'), '对话框初始：提示原文与执行按钮隐藏');
$s('hs-rx-n').value = '8';
click('hs-rx-preview');
ok(hostPostBodies.length === 0 && ($s('hs-rx-n-err').textContent || '').includes('16–256'),
  '非法值（<16）：字段内联错误，不发请求');
$s('hs-rx-n').value = '512';
click('hs-rx-preview');
ok(hostPostBodies.length === 0 && ($s('hs-rx-n-err').textContent || '').includes('16–256'),
  '非法值（>256）：字段内联错误，不发请求');
$s('hs-rx-n').value = '256';
click('hs-rx-preview');
await sleep(60);
const hsRxPre = hostPostBodies.find((b) => b.redis_databases !== undefined && b.confirm === false);
ok(hsRxPre && hsRxPre.redis_databases === 256 && !('host_env' in hsRxPre),
  '扩容预演：POST {redis_databases,confirm:false}（第二形态，不带 host_env）');
ok(($s('hs-rx-prompt').textContent || '') === REDIS_PROMPT, '服务端确认提示原文逐字节展示');
ok(($s('hs-rx-prompt').textContent || '').includes('约 3 秒') && ($s('hs-rx-prompt').textContent || '').includes('AOF')
  && ($s('hs-rx-prompt').textContent || '').includes('自动重连'), '提示原文含：重启约 3 秒 / AOF 持久 / 自动重连');
ok(!hidden('hs-rx-run'), '提示展示后：「我已了解，执行扩容」按钮出现');
click('hs-rx-run');
await sleep(100);
const hsRxRun = hostPostBodies.find((b) => b.redis_databases !== undefined && b.confirm === true);
ok(hsRxRun && hsRxRun.redis_databases === 256, '执行扩容：POST {redis_databases,confirm:true}');
const hsRxFb = $s('hs-rx-fb').textContent || '';
ok(hsRxFb.includes('扩容完成') && hsRxFb.includes('256') && hsRxFb.includes('3 秒'), '结果回报：databases → N + 重启耗时');
ok(!($s('msg').textContent || '').includes('Redis 逻辑库池已扩容'),
  'F7：扩容单通道（面板 fb 唯一播报；顶部条「Redis 逻辑库池已扩容到 N」删除）');
ok(await waitFor(() => hostGetN >= 3, 3000), '扩容后自动重取期望状态（池数字收敛）');
ok(hidden('hs-rx-run'), '执行后：扩容按钮收起（防重复点击）');
ok(!$s('hs-rx-run').disabled && $s('hs-rx-run').textContent === '我已了解，执行扩容', '扩容按钮恢复原文与可用');
click('hs-rx-close');
ok($s('dlg-redis-x').open === false, '扩容对话框关闭');

// —— §13.2 Redis 浏览管理员视图（S2：Redis 已迁数据页；服务端 admin_view 标注路径） ——
clickTab('db');
await sleep(80);
ok(($s('rd-db').innerHTML || '').includes('<option value="3">未分配</option>'),
  'admin：Redis 下拉全量（dbs 0-3 含未分配；R8：显示项目名，全量视角由数据本身体现）');
clickTab('proj');
whoamiAdmin = false;

// —— F4：实时事件面板在管理页——admin 可见期同样订阅，离开 proj/admin 才退订 ——
const evF4a = eventsN;
clickTab('pub'); // 离开订阅面（发布页无事件面板）→ 退订
ok(($s('ev-state').textContent || '').includes('未连接'), 'F4：离开 proj/admin（发布页）→ 事件流退订');
clickTab('admin'); // 管理页（实时事件面板所在）→ 订阅
ok(await waitFor(() => eventsN > evF4a, 5000), 'F4：切管理页 → 事件流订阅（面板可见期真实时）');
await sleep(80);
const evF4b = eventsN;
clickTab('proj'); // proj ↔ admin 同属订阅面：切换不重复订阅
await sleep(80);
ok(eventsN === evF4b, 'F4：admin → proj 切换不重复订阅（两页同属订阅面）');

// —— 修改密码（plan-16 §2.2 pw 三件序列收口）：校验 → 当前密码本地校验 → pw-begin/commit → 重新登录 ——
click('btn-pwchange');
ok($s('dlg-pw').open === true, '顶部「修改密码」对话框打开（状态条区入口）');
click('pw-submit'); // 全空 → 当前密码字段内联拒绝
ok(pwBeginN === 0 && ($s('pw-cur-err').textContent || '').includes('当前密码'), '改密校验：当前密码为空即内联拒绝（未发请求）');
$s('pw-cur').value = 'alice-pw';
$s('pw-new').value = 'alice-pw-2'; $s('pw-new2').value = 'two-different';
click('pw-submit');
ok(pwBeginN === 0 && ($s('pw-new2-err').textContent || '').includes('不一致'), '改密校验：两次新密码不一致即内联拒绝');
$s('pw-new').value = 'alice-pw'; $s('pw-new2').value = 'alice-pw'; // 两次一致但与当前密码相同
click('pw-submit');
ok(pwBeginN === 0 && ($s('pw-new-err').textContent || '').includes('相同'), '改密校验：新密码与当前相同即内联拒绝');
$s('pw-cur').value = 'wrong-old'; $s('pw-new').value = 'alice-pw-2'; $s('pw-new2').value = 'alice-pw-2';
const abP = authBeginN;
wireTexts.length = 0;
click('pw-submit');
await sleep(80);
ok(authBeginN === abP + 1 && pwBeginN === 0 && pwCommitN === 0, '当前密码校验：复用 login-begin + 本地 finish（无登录态副作用、不触达 pw-begin）');
ok(($s('pw-cur-err').textContent || '').includes('当前密码不正确'), '当前密码错：字段内联错误（本地 finish 即败）');
ok(!wireTexts.some((t) => t.includes('wrong-old') || t.includes('alice-pw-2')), 'OPAQUE 断言：校验上行仅 ke1 报文，新旧密码零明文');
$s('pw-cur').value = 'alice-pw';
pwChangeNew = 'alice-pw-2'; // toy 服务端：pw-commit 记录替换后新密码生效
wireTexts.length = 0;
click('pw-submit');
ok(await waitFor(() => pwCommitN === 1 && hidden('app'), 5000), '改密成功：pw-commit 完成 → 退出当前会话（提示重新登录）');
ok(pwBeginBodies[0] && pwBeginBodies[0].user === 'alice' && typeof pwBeginBodies[0].m1 === 'string' && pwBeginBodies[0].m1.length > 16,
  'pw-begin 提交体 {user, m1}（报文序列①，b64 注册首条）');
ok(/^SR-SIG1 /.test(pwBeginAuths[0] || ''), 'pw-begin 为 SR-SIG1 常规签名 POST（与 login-begin 未签名相对）');
ok(pwCommitBodies[0] && pwCommitBodies[0].user === 'alice' && typeof pwCommitBodies[0].record === 'string' && pwCommitBodies[0].record.length > 16,
  'pw-commit 提交体 {user, record}（报文序列③：原子替换记录）');
ok(!wireTexts.some((t) => t.includes('alice-pw-2')), 'OPAQUE 断言：改密全程上行（m1/record/签名体）无新密码明文');
ok($s('dlg-pw').open === false, '改密成功：对话框关闭');
ok(($s('msg').textContent || '') === '' && ($s('lg-fb').textContent || '').includes('新密码'),
  '改密成功提示驻登录卡；顶部条随 logout 清空（第2轮xiaozhi P3：消息不跨会话驻留）');
ok(($s('lg-fb').textContent || '').includes('新密码'), '登录卡指引：输入账号密码重新登录（钥匙已清无免密码入口');
ok(($s('pw-cur').value || '') === '' && ($s('pw-new').value || '') === '' && ($s('pw-new2').value || '') === '',
  '改密完成：三个密码框已清空（不留在 DOM）');
// 旧密码失效 / 新密码可登录（toy 服务端记录已替换）
const kbP = keyBindBodies.length, afP = authFinishN;
$s('lg-user').value = 'alice'; $s('lg-pass').value = 'alice-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(80);
ok(authFinishN === afP && ($s('lg-pass-err').textContent || '').includes('密码错误') && !hidden('login'),
  '旧密码已失效：登录本地 finish 即失败（两态文案，无第三请求）');
$s('lg-pass').value = 'alice-pw-2';
fire($s('lg-form'), 'submit', { preventDefault() {} });
ok(await waitFor(() => keyBindBodies.length === kbP + 1 && hidden('login'), 5000), '新密码登录成功（pass 记录已替换，钥匙重绑幂等）');
pwChangeNew = null;
PWDB.alice.pass = 'alice-pw'; // 还原 toy 口令本（后续场景 3 仍按旧口令编排）

// —— 退出登录（8bf79ed 密钥生命周期 + B2 语义：登出=清本机密钥+远程吊销；钥匙已回收，无密可免） ——
click('btn-logout');
ok(shown('login') && hidden('app'), '退出登录：回到登录卡（管理壳隐藏）');
ok(!store.has('sr.seed') && !store.has('sr.user') && !store.has('sr.pempub'),
  '密钥生命周期（8bf79ed）：登出清本机 seed/账号记忆/PEM（不留僵尸钥匙）');
ok(!els.has('lg-reenter') && !els.has('lg-forget'), 'B2：钥匙已回收——「免密码进入」入口不存在（按钮已删，无密可免）；B5 后设备清除入口不存在');
ok(($s('lg-fb').textContent || '').includes('已退出登录') && ($s('lg-fb').textContent || '').includes('重新登录')
  && !($s('lg-fb').textContent || '').includes('钥匙'),
  'B2 退出提示：「已退出登录。请输入账号密码重新登录。」（R5 追加裁决：机制词删除）');

// ═══ 场景 3：E_AUTH 回落（钥匙被删/GC）→ 密码表单 → 同钥匙重绑（找回/换设备路径） ═══
mode = 'unregistered'; // 模拟服务端已删除/GC 本机钥匙
// 8bf79ed 后登出即清记忆（无「记忆账号」可续用）——手工铺垫场景 1 的原钥匙+账号，复现「设备有记忆但钥匙被删」
store.set('sr.seed', seed1); store.set('sr.user', 'alice');
els.clear();
await load(3);
armBtnText();
armLoginInit();
await sleep(80);
ok(shown('login') && !els.has('hd-user'), '钥匙失效：自测失败回落密码表单（不自动进入，管理壳未构建）');
ok(($s('lg-fb').textContent || '').includes('失效') && ($s('lg-fb').textContent || '').includes('重新登录'),
  '回落提示：凭据失效 → 输账号密码重绑新钥匙（找回/换设备）');
ok(!els.has('lg-reenter'), 'E_AUTH 回落：无「免密码进入」入口（按钮已删，直接密码重登）');
const kb3 = keyBindBodies.length;
$s('lg-pass').value = 'alice-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(120);
ok(keyBindBodies.length === kb3 + 1 && keyBindBodies.at(-1).pubkey === pubOf(seed1)
  && hidden('login') && shown('app'),
  '密码重登：同一把本地钥匙重新绑定（幂等重绑）并进入');

// ═══ 场景 4：失败两态之二（服务端 404）/ 未知用户防枚举 / 换账号（admin 继承提示）/ whoami 失败 ═══
mode = 'unregistered';
store.clear(); // 全新浏览器（新 seed，无账号记忆）
els.clear();
await load(4);
armBtnText();
await sleep(40);
const seed4 = store.get('sr.seed');
// —— 4a. 服务端拒绝（login-begin → 404 E_AUTH：限流/畸形一律不可区分） ——
authServerDown = true;
const af4 = authFinishN;
$s('lg-user').value = 'alice';
$s('lg-pass').value = 'alice-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(80);
const perr4 = $s('lg-pass-err').textContent || '';
ok(perr4.includes('服务端拒绝') && perr4.includes('稍后重试'), '失败态二（服务端 404 E_AUTH）：限流/拒绝文案 + 稍后重试');
ok(authFinishN === af4 && keyBindBodies.length === kb3 + 1, '服务端拒绝：不产生 login-finish/key-bind 请求');
authServerDown = false;
// —— 4b. 未知用户：login-begin 仍 200（假记录路径），本地 finish 失败 → 与错密码同文案 ——
const ab4 = authBeginN;
$s('lg-user').value = 'nobody';
$s('lg-pass').value = 'whatever-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(80);
ok(authBeginN === ab4 + 1, '未知用户：login-begin 仍 200（防枚举：与已注册用户不可区分）');
ok(authFinishN === af4 && ($s('lg-pass-err').textContent || '').includes('密码错误'),
  '未知用户：本地 finish 失败 → 与「密码错误」同一文案（客户端无法也不应区分）');
// —— 4c. 换账号密码登录（bob=管理员）：沿用本机钥匙绑定新账号 + admin 继承显式提示 ——
$s('lg-user').value = 'bob';
$s('lg-pass').value = 'bob-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(120);
ok(keyBindBodies.at(-1).user === 'bob' && keyBindBodies.at(-1).pubkey === pubOf(seed4),
  '换账号密码登录：用本机既有钥匙绑定新账号');
ok(store.get('sr.seed') === seed4 && store.get('sr.user') === 'bob', '沿用同一 seed，记忆新账号');
ok(hidden('login') && shown('app') && $s('hd-user').textContent === 'bob', '进入管理壳（回显新账号）');
ok(($s('msg').textContent || '').includes('已进入管理员账号'), 'admin 账号登录提示（R5：无机制词）');
// —— 4c-2. 换号顶栏刷新 + 退出清顶部条（第2轮xiaozhi P2/P3）：logout → 密码登录另一账号 → #hd-user/即时复位 ——
whoamiAdmin = true;
clickTab('db'); // bob 视角点亮数据页（换号前作旧值残留的对照）
await sleep(80);
ok(($s('rd-db').innerHTML || '').includes('<option value="0">'), '换号前置：bob 数据页 DB 下拉已加载（全量 0-3）');
clickTab('proj');
whoamiAdmin = false;
click('btn-logout');
ok(($s('msg').textContent || '') === '' && ($s('msg').className || '') === '',
  '退出登录：顶部条消息清空（错误提示不跨会话驻留）');
$s('lg-user').value = 'alice';
$s('lg-pass').value = 'alice-pw';
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(200);
ok(hidden('login') && shown('app') && $s('hd-user').textContent === 'alice',
  '换号重登：顶栏 #hd-user 即时显示新账号（不残留旧账号名 bob）');
ok(($s('rd-db').innerHTML || '') === '',
  '换号重登：数据页 Redis 作用域复位（DB 列表清空按新账号重取，不残留旧账号数据）');
// —— 4d. whoami 失败（404 E_AUTH）：顶部条报错、管理标签不渲染 ——
whoamiFail = true; // §12.2：whoami 404 E_AUTH → 顶部条报错、管理标签不渲染
click('btn-logout');
$s('lg-user').value = 'alice'; $s('lg-pass').value = 'alice-pw'; // B2 后无「免密码进入」——密码重登驱动 enterApp
fire($s('lg-form'), 'submit', { preventDefault() {} });
await sleep(200);
ok(hidden('login') && shown('app'), '密码重登成功（whoami 失败不影响本地进入）');
ok(($s('msg').textContent || '').includes('身份查询') || ($s('msg').textContent || '').includes('E_AUTH'),
  'whoami 失败（404 E_AUTH）：顶部条报错');
ok(!els.has('tab-admin') && !els.has('ad-users-tbl'), 'whoami 失败：管理标签不渲染');
whoamiFail = false;

// ═══ 场景 5/6（B5/B6 后已删除）：原「忘记此设备」与登录窗专家路径链路整体移除，
//     其行为断言由场景 7 的零残留静态断言取代 ═══

// ═══ 场景 7：静态断言（plan-16 登录重设计 / §10.2 更名 / §10.3+§10.4 端点 / §11 零残留 / §12+§13 接入 / 界面规范） ═══
// —— plan-16：一步登录 + 密钥无感 + OPAQUE 端点 + 专家折叠 ——
ok(!html.includes('wz-s1') && !html.includes('wz-s2') && !html.includes('wz-s3') && !html.includes('wz-p1') && !html.includes('wz-p2')
  && !html.includes('wz-next1') && !html.includes('wz-next2') && !html.includes('wz-back2'),
  'plan-16：旧三步向导主路径零残留（步骤条/分步面板/前后步按钮）');
ok(html.includes('id="lg-form"') && html.includes('id="lg-user"') && html.includes('id="lg-pass"') && html.includes('type="password"'),
  'plan-16：一步登录表单（账号 + 密码，type=password）');
ok(!html.includes('已生成新密钥并存入'), 'plan-16：密钥自动生成零提示文案（无感）');
for (const ep of ['/_api/auth/login-begin', '/_api/auth/login-finish', '/_api/auth/key-bind']) ok(html.includes(ep), `plan-16 §2.2：${ep} 已接线`);
ok(html.includes('X-Sr-Bind'), 'plan-16 §2.2：key-bind 携带 X-Sr-Bind 头');
ok(html.includes('/_api/key-selfrm'), 'plan-16 §2.2：key-selfrm（登出远程吊销本机钥匙）已接线');
ok(html.includes('E_PW_LOCAL'), 'plan-16：本地密码错错误态（E_PW_LOCAL，无第三请求）已接线');
ok(html.includes('密码错误，或该账号尚未设置密码（请联系管理员重置）'), 'plan-16 §3：失败文案两态合一（防枚举一致代价）');
ok(html.includes('忘记密码') && html.includes('联系管理员重置'), 'plan-16 §3：忘记密码 → 找管理员重置文案');
{
  const loginHtml = html.slice(html.indexOf('<section id="login">'), html.indexOf('<!-- ———— 管理壳')).replace(/<!--[\s\S]*?-->/g, '');
  ok(html.includes('<p class="note">忘记密码请联系管理员重置。</p>') && !loginHtml.includes('绑定') && !loginHtml.includes('密钥') && !loginHtml.includes('钥匙') && !loginHtml.includes('迁移'),
    'R5 追加裁决：登录窗说明段整删只留忘记密码一句（无常驻机制词）');
}
ok(!html.includes('忘记此设备') && !html.includes('id="lg-forget"') && !html.includes('btn-settings-forget'),
  'B5：「忘记此设备」整链路零残留（登录窗/设置弹窗/帮助页/CLI 对照表）');
ok(!html.includes('id="lg-expert"') && !html.includes('钥匙登录（CLI / 机器用户'),
  'B6：登录窗「钥匙登录（专家路径）」折叠零残留（控制台登录=账号+密码唯一方式）');
ok(!html.includes('lg-reenter'), '两态铁律：#lg-reenter「免密码进入」按钮与监听器零残留（登录窗无第三态残骸）');
{
  const helpLogin = html.slice(html.indexOf('登录与账号'), html.indexOf('CLI 使用'));
  ok(html.includes('账号与登录密码由管理员发放') && html.includes('忘记密码请联系管理员重置（控制台无法自助找回）'),
    'R5 追加裁决：帮助页登录节收敛为两句事实（账号来源 + 忘记密码）');
  ok(!helpLogin.includes('绑定') && !helpLogin.includes('钥匙') && !helpLogin.includes('迁移') && !helpLogin.includes('试验性存储'),
    'R5 追加裁决：帮助页登录节机制词清零（新设备/换浏览器/绑定句删除；CLI 文档段技术语境除外）');
}
ok(html.includes('CLI 使用') && html.includes('showroom login') && html.includes('showroomctl user-add &lt;名&gt; --gen-password')
  && !html.includes('--pubkey') && !html.includes('专家路径：公钥与机器用户'),
  'B7 帮助页：统一账号密码认证（CLI login + 管理员发卡指引；预登记公钥路径零残留）');
ok(html.includes('showroom keygen') && html.includes('调试用'),
  'B7 帮助页：keygen 保留但定位为调试（CLI 对照表行标注）');
// —— 工具下载（GET /_api/tool/*）：帮助页三行下载卡片 + 签名下载接线 ——
ok(html.includes('工具下载') && html.includes('id="tool-tbl"') && (html.match(/data-tool-dl=/g) || []).length === 3,
  '工具下载：帮助页三行下载卡片（CLI 二进制 / Agent Skill / 平台公钥）齐备');
ok(html.includes('<th>文件</th><th>说明</th><th>大小</th><th>操作</th>'),
  '工具下载：四列表头齐备（第 4 列「操作」不为空，第2轮xiaozhi P3）');
ok((html.match(/class="admin-only hidden"/g) || []).length === 2
  && html.includes("document.querySelectorAll('#app .admin-only')"),
  'CLI 对照表：user-add/user-passwd 两行 admin-only 默认隐藏 + syncAdmin 按 whoami 显隐（B7：登记公钥行删除）');
for (const t of ['showroom', 'showroom-publisher.md', 'platform_rsa.pub']) {
  ok(html.includes(`data-tool-dl="${t}"`) && html.includes(`id="tool-size-${t}"`), `工具下载：${t} 行（按钮 + 大小列）存在`);
}
ok(html.includes('/_api/tool/') && html.includes('wireHelpTab') && html.includes('fmtToolSize'),
  '工具下载：签名 GET /_api/tool/* → Blob 下载逻辑已接线（含大小回填）');
// —— plan-16 收口：发卡初始密码 / 补发 / 改密入口 ——
for (const ep of ['/_api/auth/pw-begin', '/_api/auth/pw-commit', '/_api/admin/passwd-set']) ok(html.includes(ep), `plan-16 §1.5/§2.2：${ep} 已接线`);
ok(html.includes('id="btn-pwchange"') && html.includes('id="dlg-pw"') && html.includes('修改密码'), '改密入口：设置对话框聚合按钮 + 对话框容器');
ok(!html.includes('id="tab-status"') && !html.includes('id="tabbtn-status"') && !html.includes('id="st-tbl"')
  && !html.includes('id="st-auto"') && !html.includes('id="st-refresh"') && !html.includes('id="st-fb"')
  && !html.includes('refreshStatus'),
  'S2：tab-status 拆除——健康表/手动刷新/10s 开关归零（能力并入展项列表+行抽屉+事件流去抖）');
ok(html.includes('id="help-subnav"') && (html.match(/class="hpanel/g) || []).length === 5,
  'R9：帮助页二级导航五面板（账号/CLI 使用/镜像规范/工具与命令/错误码）——与管理页同构，不混一页');
ok(html.includes('id="tabbtn-pub"') && html.includes('id="tabbtn-img"') && html.includes('id="tab-pub"') && html.includes('id="tab-img"')
  && html.includes('我的项目') && !html.includes('id="btn-new-pub"') && !html.includes('id="hd-role"') && !html.includes('id="lg-srv"') && !html.includes('id="hd-srv"'),
  'S2：五标签（我的项目/发布/镜像库/数据/帮助）；R5 A1 顶栏/登录窗服务器行与 A3「+ 发布」CTA 均已删除（「管」角标唯一，无第三徽标）');
ok(html.indexOf('id="st-events"') > html.indexOf('id="tab-admin"') && html.includes('实时事件'),
  'S2：docker events 原始流迁入管理页「实时事件」面板（R5 C9：h2 后缀删除）');
ok(html.indexOf('id="rd-db"') > html.indexOf('id="tab-db"') && html.indexOf('id="rd-db"') < html.indexOf('id="tab-help"'),
  'S2：Redis 浏览迁入数据页（rd-* 换父容器，id 不变）');
ok(html.includes('data-drawer') || html.includes("drawerRow.dataset.drawer"), 'S2：行抽屉 data-drawer 机制（无全局行 id）');
{
  const words = ['运行中', '已停止', '重建中', '异常', '待发布', '上传中', '发布中'];
  for (const w of words) ok(html.includes(w), `S5/验收 5：7 态词表含「${w}」`);
  ok(!html.includes('未部署') && !html.includes('待部署'), 'S5/验收 5/F1：「未部署」「待部署」全站零出现（统一「待发布/发布」词族）');
  ok(html.includes('st-breath') && html.includes('@keyframes st-pulse'), 'S5：重建中呼吸动画（reduced-motion 全局禁用即静止）');
  ok(html.includes('markPublishing') && html.includes('markPublishDone') && html.includes('markPublishFail'),
    'S5：本地在途标记（发布中/上传中/异常终态/60s 重建窗口）已接线');
}
ok(html.includes('<th>名称</th><th>状态</th><th>对外 URL</th><th>标签</th><th class="num">更新</th><th>操作</th><th>日志</th>'),
  'S4：列序 名称·状态·对外URL·标签·更新·操作·日志（R5 C4：「详情」列头改「日志」）');
ok(html.includes('visibilitychange') && html.includes('暂无项目——到「发布」页发布第一个项目'),
  'S4：事件流可见性钩子 + 空态卡引导「发布」页（R5 A3：顶栏 CTA 删除后空态改指发布 Tab）');
ok(html.includes('focusGuided') || html.includes('登录焦点引导'), 'S4/验收 16：登录焦点引导（首行打开 / +发布 CTA）');
{
  const appTop = html.slice(html.indexOf('<template id="app-tpl">'), html.indexOf('</header>'));
  ok(!appTop.includes('hd-pempub') && !appTop.includes('btn-pempub') && !appTop.includes('btn-pwchange'),
    'S1：顶栏不常驻 PEM/改密入口（聚合进设置对话框；验收 9 顶栏单行）');
  ok(html.includes('id="dlg-settings"') && html.includes('id="set-base"') && html.includes('id="set-base-err"')
    && html.indexOf('id="hd-pempub"') > html.indexOf('id="dlg-settings"'),
    'S1：设置对话框齐备（对外访问域名 + PEM 收纳 + 改密/忘记设备聚合；hd-pempub 迁入其中）');
  ok(html.includes('externalUrl') && html.includes('sr.publicBase') && html.includes('id="proj-base-note"'),
    'S1 §6：externalUrl 助手 / 公网域名设置 / 内网兜底提示条已接入');
  ok(html.includes('id="pub-pem-hint"') && html.includes('id="btn-go-settings"'),
    'S1 验收6：PEM 内联引导容器（去设置）已接入');
}
ok(html.includes('autocomplete="current-password"') && html.includes('autocomplete="new-password"'),
  '规范：改密密码框 autocomplete（current-password / new-password）');
ok((html.match(/type="password"/g) || []).length >= 4, '规范：密码输入均 type=password（登录 1 + 改密 3）');
for (const s of ['登录初始密码', '未设密码', '已设密码', '补发密码', '仅显示一次', '重新登录']) {
  ok(html.includes(s), `plan-16 收口文案全中文：${s}`);
}
ok(html.includes('id="ad-na-login-pw"') && html.includes('id="ad-ps-pw"'),
  '发卡/补发一次性密码展示容器齐备（结果两行 + 补发对话框；生成展示区已并入「打开即自动生成」）');
ok(!html.includes('id="ad-na-expert"') && !html.includes('登记公钥发放'), 'B7：发卡对话框无公钥直登记折叠（与场景 2 行为断言互补）');
for (const id of ['pw-cur', 'pw-new', 'pw-new2']) ok(html.includes(`for="${id}"`), `规范：改密控件 label for="${id}"`);
ok(!html.includes('op=change'), '改密路径与服务端对齐（pw-begin 无 op 查询参数——按 serve.rs/SPEC §16 实形状）');
ok(!html.includes('内存上限'), '§10.2：「内存上限」零残留');
ok((html.match(/内存需求/g) || []).length === 1, '§10.2/plan-17 验收 4：「内存需求」仅发布表单 label 一处');
ok(!/槽位|slot/i.test(html), '§11：全 HTML「槽位/slot」零残留');
ok(!html.includes('SHOWROOM_SLOT'), '§11：环境变量清单去掉 SHOWROOM_SLOT');
ok(html.includes('^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$'), '§11：项目名规则已放宽（允许大写字母）');
ok(!html.includes('apiRaw'), '旧单发备份路径 apiRaw 已移除');
ok(!html.includes("'/_api/db-dump'"), '旧单发 POST /_api/db-dump 无残留');
for (const ep of ['redis-dbs', 'redis-keys', 'redis-get']) ok(html.includes(ep), `§10.3：${ep} 已接入`);
for (const ep of ['/_api/logs-stream', '/_api/events', '/_api/db-dump-begin', '/_api/db-dump-status', '/_api/db-dump-chunk']) ok(html.includes(ep), `§10.4：${ep} 已接入`);
ok(!/lg-keygen|lg-importkey|renderLoginState|doLogin\b|gotoStep|enterStep3/.test(html), '旧登录区控件/向导状态机函数无残留');
// —— 上传压缩统一（zstd）：压缩助手 / wasm 会话接线 / 文案 ——
ok(html.includes('compressToBlob') && html.includes('uploadPayload'), '上传压缩助手 compressToBlob/uploadPayload 已接入');
ok(html.includes('sr_zstd_begin') && html.includes('sr_zstd_push') && html.includes('sr_zstd_end'), 'wasm zstd 压缩会话（sr_zstd_*）已接线');
ok((html.match(/压缩后上传/g) || []).length >= 1, '上传进度文案标注「压缩后上传」');
// —— §12.1 一键发布 ——
ok(html.includes('单表单双镜像源') && html.includes('id="pub-src-tar"') && html.includes('id="pub-src-registry"')
  && html.includes('id="pub-src-file-box"') && html.includes('id="pub-src-reg-box"') && html.includes('id="pub-pref-note"'),
  'plan-17 §5：发布单表单双镜像源（radio + 双源容器 + 预填注记）齐备');
ok(!html.includes('id="dep-form"') && !html.includes('id="dep-project"') && !html.includes('id="dep-tag"')
  && !html.includes('id="dep-mem"') && !html.includes('id="dep-cport"') && !html.includes('id="dep-submit"')
  && !html.includes('id="dep-fb"') && !html.includes('id="dep-project-err"') && !html.includes('id="dep-tag-err"'),
  'S3：dep-form 九件全零残留（能力并入发布表单镜像库源）');
ok((html.match(/内存需求/g) || []).length === 1 && (html.match(/项目端口/g) || []).length === 1,
  'S3/验收 4：「内存需求」「项目端口」label 全站各恰好 1 次（C4 更名 + R5 A5 展项→项目全局更名）');
for (const s of ['① 压缩', '② 上传', '③ 发布', '④ 完成']) ok(html.includes(s), `§12.1：状态条分段「${s}」`);
ok(html.includes('沿用上次发布参数'), '§5：预填注记文案');
ok(html.includes('压缩后总长') && html.includes('断点续传') && html.includes('复制地址'), '§12.1：②段文案「已传/压缩后总长」+ 断点续传 + 完成段复制按钮');
ok(html.includes('自动生成随机项目名') || html.includes('留空自动生成'), '§12.1：项目留空自动生成随机 8 位');
ok(html.includes('重试'), '§12.1：失败可重试按钮');
// —— §12.2 管理员 ——
ok(html.includes('/_api/whoami'), '§12.2：whoami 已接入');
for (const ep of ['/_api/admin/users', '/_api/admin/user-add', '/_api/admin/status', '/_api/admin/projects', "'key-rm'", "'user-revoke'", "'user-admin'"]) {
  ok(html.includes(ep), `§12.2：${ep} 已接线（user-revoke/key-rm/user-admin 经 ep 表驱动；R5 D2 key-admin 垫片移除）`);
}
ok(!html.includes("key-admin") && !html.includes('/_api/admin/key-admin'), 'R5 D2：key-admin 兼容垫片端点零残留（前后端同步移除）');
for (const s of ['发放新账号', '同时授予管理员', '仅显示一次', '吊销整个账号', '设为管理员', '取消管理员', '移除此设备', '平台状态', '管理']) {
  ok(html.includes(s), `§12.2 新增文案全中文：${s}`);
}
ok(html.includes('<h2>账号</h2>') && html.includes('<th>设备</th><th>登录时间</th><th>操作</th>')
  && html.includes('设备 ${keys.length} 台') && html.includes('（该账号暂无设备）'),
  'F2：管理页账号面板设备词族（h2「账号」/表头 设备·登录时间/设备 N 台/暂无设备）');
ok(!html.includes('账号与钥匙') && !html.includes('删除钥匙') && !html.includes('绑定时间') && !html.includes('钥匙 kid')
  && !html.includes('（该账号没有钥匙）') && !html.includes('钥匙 ${keys.length} 把'),
  'F2：钥匙词族零残留（账号与钥匙/删除钥匙/绑定时间/钥匙 kid/没有钥匙/钥匙 N 把）');
ok(!html.includes('该账号任意钥匙登录即具管理权限') && html.includes('该账号登录即具管理权限'),
  'F2：账号级权限表述设备语言化（「该账号登录即具管理权限」）');
// —— §13.1 三入口并存（一键不取代分步）+ §13.2/13.3 Redis 隔离 ——
ok(html.includes('id="pub-submit"') && html.includes('id="btn-upload-only"'),
  'plan-17 §5：发布主按钮 pub-submit + 次要按钮「仅上传」（三入口收敛为单表单）');
for (const s of ['仅上传', '镜像已入库', '镜像库（已入库镜像）']) ok(html.includes(s), `§13.1/§5：入口与镜像源文案「${s}」`);
ok(html.includes('/_api/images'), '§13.1：/_api/images 已接线（镜像 ComboBox 数据源）');
ok(html.includes('setComboOptions') && !html.includes('<datalist'), '§13.1：镜像 ComboBox（R5b 自绘下拉：同宽/token 样式/输入过滤）；原生 datalist 全站归零');
ok(html.includes('onImagePick'), '§5：镜像库源选中即带出项目/标签（onImagePick 保留）');
ok(html.includes('<h2>Redis 浏览</h2>') && !html.includes('仅显示您名下项目的库') && !html.includes('管理员视图'),
  'R5 C7：Redis 浏览标题括号删除（隔离语义由空态表达；「管理员视图」标注随 R4 已删）');
ok(!html.includes('非安全边界') && !html.includes('界面级'), 'R5 C7：底部「仅排障用途…非安全边界」句删除');
// —— §14 磁盘空间显示 + 镜像删除 ——
ok(html.includes('/_api/space') && html.includes('id="space-note"'),
  '§14.1：/_api/space 接线 + 空间条容器（手动刷新按钮按 plan-17 §0-5 归零）');
ok(html.includes('可用空间：') && !html.includes('已扣预留') && html.includes('空间信息不可用'), '§14.1：空间条文案（R5b：无机制自证 + 失败静默降级）');
ok(html.includes('id="img-list"') && html.includes('/_api/image-rm'), '§14.3：镜像清单渲染容器 img-list + image-rm 接线');
ok(html.includes('回滚/重建将失效') && html.includes('已回收约'), '§14.3：删除 confirm 影响文案 + 顶部条回收量文案');
ok(html.includes('可到镜像库删除旧镜像后重试'), '§14.2：磁盘 E_QUOTA 顶部条删除引导文案（部署区→镜像库更名）');
// —— §15 主机编排（期望状态 / 两段式 / Redis 扩容流） ——
ok(html.includes('/_api/admin/host'), '§15.2：/_api/admin/host 已接线（GET 期望状态 + POST 两段式）');
for (const s of ['主机编排', '中间件集合', '查看变更', '确认执行', '两段式', '漂移', 'Redis 逻辑库余量不足',
  '扩容', '获取确认提示', '我已了解，执行扩容', '执行报告', '红色警示']) {
  ok(html.includes(s), `§15 新增文案全中文：${s}`);
}
ok(html.includes('id="hs-mw-redis"') && html.includes('id="hs-mw-postgres"') && html.includes('id="hs-mw-registry"'),
  '§15：中间件三开关（postgres/redis/registry）');
ok(html.includes('id="hs-p-redisdbs"') && html.includes('id="hs-p-disk"') && html.includes('id="hs-p-mem"') && html.includes('id="hs-p-factor"'),
  '§15：四参数控件（REDIS_DBS 只读 + 三可编辑值）');
ok(html.includes('id="hs-pool-expand"') && html.includes('id="dlg-redis-x"') && html.includes('id="hs-apply"') && html.includes('id="hs-confirm"'),
  '§15：扩容按钮/扩容对话框/查看变更/确认执行容器齐备');
for (const id of ['hs-mw-postgres', 'hs-mw-redis', 'hs-mw-registry', 'hs-p-redisdbs', 'hs-p-disk', 'hs-p-mem', 'hs-p-factor', 'hs-rx-n']) {
  ok(html.includes(`for="${id}"`), `§15 规范：控件 label for="${id}"`);
}
ok(html.includes('warnbar') && html.includes('.warnbar.err'), '§15：黄条/红条样式（漂移、池余量、受影响项目警示）');
ok(!html.includes('id="btn-refresh"') && !html.includes('id="btn-db-fill"') && !html.includes('id="space-refresh"')
  && !html.includes('id="dep-images-refresh"'),
  'S6/验收 3：手动加载按钮 DOM 归零（刷新列表/载入项目清单/刷新空间/刷新镜像清单）');
ok(!html.includes('载入项目清单') && !html.includes('刷新镜像清单') && !html.includes('刷新列表'),
  'S6/验收 3：「载入项目清单」「刷新镜像清单」文案零残留');
ok(html.includes('class="sub"') && html.includes('retryOnce') && html.includes('retry-projects'),
  'S6/§3：副题纯状态容器（成功不写计数、失败显示自动重试）+ 失败内联「重试」+ 3s 自动重试一次');
ok(html.includes('60s 周期重试') || html.includes('60000'), 'S6/§3：whoami 失败 ? 徽标 60s 周期重试');
ok(html.includes('redis_databases') && html.includes('confirm_required'), '§15.3：扩容第二形态提交体 redis_databases 与 confirm_required 语义已接线');
// —— R5 全量复核：黑名单零残留 + 双入口删除/分层/账号级 admin/状态视觉 静态断言 ——
ok(!html.includes('展项'), 'R5 A5：「展项」全站清零（用户可见面与注释同清，统一「项目」）');
ok(!html.includes('一键发布') && !html.includes('部署区') && !html.includes('部署成功'), 'R5 C5：「一键发布/部署区/部署成功」零残留（统一「发布」）');
ok(!html.includes('服务器 http') && !html.includes('.srv') && !html.includes('id="lg-srv"') && !html.includes('id="hd-srv"'),
  'R5 A1：服务器地址展示（顶栏+登录窗）与 .srv 移动端样式零残留');
ok(!html.includes('复制 PEM') && !html.includes('id="pempub-copy"') && html.includes('登录后通常已自动配置'),
  'R5 C3：「复制 PEM」按钮删除（帮助页下载唯一入口）+ 两处自动配置措辞统一');
ok(!html.includes('镜像分片上传') && html.includes('<h3>断点续传</h3>') && !html.includes('上传机制说明') && !html.includes('镜像库说明'),
  'R5 C6：镜像库两折叠整删 + h3 更名「断点续传」');
ok(!html.includes('非本人项目') && !html.includes('not-mine'), 'R5：他人项目分支零残留（/_api/projects 只回本人）');
ok(!html.includes('高级说明'), 'R5 C5：发布页「高级说明」三条整删');
ok(!html.includes('机制说明') && !html.includes('服务器无法解密') && html.includes('<h2>备份与恢复</h2>') && html.includes('<h3>操作输出</h3>'),
  'R5 C7+R9：机制折叠与信任声明全删（安全语义按钮括注自足）+ 两处标题收敛');
ok(!html.includes('会先弹出确认框'), 'R5 C7：SQL 区危险语句预讲删除（运行时 confirm 已表达）');
ok(!html.includes('仅展示对外可见信息'), 'R5 A4：项目表底部说明段整删');
ok(html.includes('密码（非 docker push 用的镜像库密码）') && !html.includes('保管于浏览器 localStorage'),
  'R5 C1/C2：登录窗密码括号缩短 + localStorage 说明第二句删');
ok(!html.includes('（OPAQUE，密码不明文上线）') && !html.includes('新旧密码均只在本机参与 OPAQUE 盲化') && html.includes('只在本机参与盲化'),
  'R5 C3：改密按钮 title 与改密弹窗尾注「OPAQUE」协议名删除（帮助页技术文档保留）');
ok(html.includes('id="admin-subnav"') && html.includes('data-adp="users"') && html.includes('data-adp="projects"')
  && html.includes('data-adp="status"') && html.includes('data-adp="host"') && html.includes('data-adp="events"'),
  'R5 E1：管理页二级导航五面板（账号|全部项目|平台状态|宿主配置|实时事件）');
ok(html.includes('id="ad-projects-tbl"') && html.includes('所属账号') && html.includes('全部账号的项目（管理员）'),
  'R5 E3：全部项目面板（跨用户表 + 说明一句）');
ok(html.includes('管理员是账号属性：该账号登录即具管理权限'), 'R5 C9/D3+F2：账号面板底部长说明缩为一句（账号级权限表述）');
ok(html.includes('取消勾选即禁用，重新勾选即启用'), 'R5 C9：中间件说明缩为一句');
ok(html.includes('配置已修改，请重新预演'), 'R5 B4：主机预演失效提示接线');
ok(html.includes('执行失败（原文见顶部提示）') && html.includes('加载失败（将自动重试）') && html.includes('（加载失败，将自动重试）'),
  'R5 B5：失败态写进数据容器（dbOut 覆写/镜像清单·键列表·键值表·漂移占位）');
ok(html.includes('收起日志') && html.includes('▴') && html.includes('更多 ▴'), 'R5 B1/B2：抽屉 ▾/▴ 翻转 + 更多菜单箭头翻转接线');
ok(html.includes('a:hover:not(.act) { color: var(--fg-bright); text-decoration: underline; }') && html.includes('select:not(:disabled):hover'),
  'R5 B6：a:hover 下划线变色 + select hover 补齐');
// —— 第三轮 IA 复查（F3/F4/F5/F6/F7/F9/F11/F12）静态断言 ——
ok(!html.includes('已刷新') && !html.includes('加载期望状态中'),
  'F3：宿主面板成功静默（「已刷新/加载期望状态中」回执删除；失败才说话）');
ok(html.includes("if (name === 'proj' || name === 'admin') { evLive.want = true; startEventsStream(); }")
  && html.includes("const adminShown = !!$('tab-admin') && !$('tab-admin')?.classList?.contains('hidden');"),
  'F4：事件流订阅面=proj|admin（activateTab + 可见性钩子双接线）');
ok(html.includes('自动压缩并上传再发布') && html.includes('自动压缩 → 上传 → 发布')
  && !html.includes('自动压缩并分片上传') && !html.includes('分片上传 → 发布'),
  'F5：发布页「分片」两处清除（文件说明 + 发布说明步骤）');
ok(html.includes('自动生成下载专用密码') && !html.includes('一次性密钥') && !html.includes('备份内容用'),
  'F6+R9：按钮括注保留「下载专用密码」；信任声明句整删（词族零残留）');
ok(!html.includes('主机变更已执行') && !html.includes('逻辑库池已扩容到'),
  'F7：宿主执行/扩容单通道（顶部条播报删除，面板 fb 唯一）');
ok(html.includes('正在准备下载…') && html.includes('下载中：${offset}') && html.includes('压缩中 ${done} / ${raw} 字节…')
  && html.includes('文件无需压缩，直接上传') && html.includes('从断点继续。')
  && html.includes('本地加密失败') && html.includes('本地压缩失败') && html.includes("'尚未登录'"),
  'F9：进度/错误文案机制词清尾（RSA 密钥/分片/zstd 字样→人读词）');
ok(!html.includes('zstd-${ZSTD_LEVEL}') && !html.includes('加密分片失败') && !html.includes('压缩分片失败')
  && !html.includes('分片下载中') && !html.includes('保留已传分片') && !html.includes('未配置密钥或账号')
  && !html.includes('分片总长不一致') && !html.includes('分片长度异常') && !html.includes('分片上传中'),
  'F9：旧机制词组零残留（用户可见面）');
ok(html.includes('id="rd-admin-clause"') && html.includes('；管理员可见全部项目的键空间'),
  'F11：Redis 静态句管理员半句拆分（默认隐藏，refreshAdminBadge 按 admin||boundAdmin 拼入）');
ok((html.match(/尚未完成安全配置/g) || []).length === 4
  && !html.includes('请先在「设置」中配置平台公钥') && !html.includes('恢复需先配置平台公钥'),
  'F12：未配置 PEM 引导统一「尚未完成安全配置：到「设置」完成」（hint + proj/img/db 三处错误）');
ok(html.includes('每项目一库，由平台统一分配') && !html.includes('池上限见平台 policy'),
  'F12：ENV_DESC REDIS_DB 说明去 policy 机制词');
// —— 界面规范静态断言（B 部分逐条） ——
ok(!/<div[^>]+onclick|onclick=/.test(html), '规范：无内联事件（含 div onclick）');
ok(!/transition\s*:\s*all/.test(html), '规范：无 transition: all（过渡列明属性）');
ok(!/outline\s*:\s*none/.test(html), '规范：无 outline:none 裸奔（:focus-visible 可见焦点）');
ok(html.includes(':focus-visible'), '规范：:focus-visible 焦点样式存在');
ok(html.includes('role="tablist"') && html.includes('role="tab"') && html.includes('role="tabpanel"') && html.includes('aria-selected'),
  '规范：tablist/tab/tabpanel 语义 + aria-selected');
ok(html.includes('aria-live="polite"'), '规范：异步反馈区 aria-live="polite"');
ok(html.includes('prefers-reduced-motion'), '规范：prefers-reduced-motion 禁非必要过渡');
ok(html.includes('overscroll-behavior: contain'), '规范：滚动容器 overscroll-behavior: contain');
ok(html.includes('touch-action: manipulation'), '规范：touch-action: manipulation');
ok(html.includes('font-variant-numeric: tabular-nums'), '规范：表格数字列 tabular-nums');
ok(html.includes('name="theme-color"') && html.includes('color-scheme: dark'), '规范：theme-color + color-scheme: dark');
ok(html.includes('select option {'), '规范：select option 显式配色');
// —— plan-17 S7 视觉 token ——
ok(!html.includes('<fieldset') && !html.includes('<legend'), 'S7/验收 9：全站无卡片内嵌 fieldset（分组一律 .fs + h3）');
ok(html.includes('.fs {') && html.includes('--sp-1:') && html.includes('--sp-8:'),
  'S7/验收 12：间距刻度 token（4/8/12/16/24/32）+ 分组样式 .fs');
ok(html.includes('h2 { font-size: 15px') && html.includes('.field > span:not(.row):not(.q) { color: var(--fg-dim); font-size: 13px'),
  'S7/§7：字号刻度（卡片题 15/600、label/辅助 13、正文 14、注释 12）');
ok(html.includes('.num { text-align: right; font-variant-numeric: tabular-nums; }') && html.includes('<th class="num">更新</th>'),
  'S7/验收 12：时间列右对齐且等宽数字');
{
  const ph = [...html.matchAll(/placeholder="([^"]*)"/g)].map((x) => x[1]);
  ok(ph.length > 0 && ph.every((p) => p.endsWith('…')), `规范：占位符全部以 … 结尾（${ph.length} 处）`);
}
for (const id of ['lg-user', 'lg-pass', 'pub-project', 'dep-image', 'db-project', 'rd-db', 'ad-na-user']) {
  ok(html.includes(`for="${id}"`), `规范：表单控件 label for="${id}"`);
}
ok(html.includes('showModal'), '规范：dialog 用 showModal（Esc/焦点圈闭/inert）');
ok(!html.includes('id="pub-slot"') && !/>\s*(Deploy|Retry|Publish Now|Admin Panel)\s*</.test(html), '新增 UI 无英文按钮文案混排');

console.log(failed === 0 ? '\nWIZARD SMOKE PASS' : `\nWIZARD SMOKE FAIL (${failed})`);
process.exit(failed === 0 ? 0 : 1);
