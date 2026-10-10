#!/usr/bin/env node
// showroom-web 向导冒烟（SPEC §10.1/§10.2/§10.3；无浏览器依赖，node ≥ 18）：
//   把 dist/index.html 的 <script type="module"> 挂到 mock DOM/localStorage/fetch 上执行，
//   驱动登录向导状态机走完全部分支：
//   1) 全新浏览器（无种子无账号）→ 三步向导 → E_AUTH 公钥展示与重检循环
//      → 模拟管理员登记成功 → 进入管理壳（展项列表请求渲染）
//   2) 同浏览器再访（账号+种子匹配）→ 直达第 3 步并自动进入
//   3) 种子在、账号换 → 走完整向导（第 2 步默认「使用本机已有密钥」，指纹与公钥一致）
//   4) 「导入密钥种子」分支：非法 seed 拒绝；合法 seed 进入第 3 步
//   5) 静态断言：「内存上限」零残留 / apiRaw 唯一 / §10.3 三端点已接入
import { readFileSync, writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

let failed = 0;
const ok = (cond, msg) => { console.log(`${cond ? 'ok  -' : 'FAIL-'} ${msg}`); if (!cond) failed++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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
    id, value: '', textContent: '', innerHTML: '', checked: false, disabled: false,
    dataset: {}, style: {}, listeners: {}, classList: new Cls(),
    addEventListener(ev, fn) { (this.listeners[ev] ||= []).push(fn); },
    removeEventListener() {}, append() {}, focus() {}, select() {}, remove() {},
    closest() { return null; },
    querySelector() { return makeEl(id + ':q'); },
    querySelectorAll() { return []; },
    content: { cloneNode: () => ({}) },
  };
  el.click = () => fire(el, 'click', { preventDefault() {} });
  return el;
}
const fire = (el, ev, arg) => { for (const fn of el.listeners[ev] || []) fn(arg || { preventDefault() {}, target: el, key: '' }); };
const $s = (id) => { if (!els.has(id)) els.set(id, makeEl(id)); return els.get(id); };
const click = (id) => fire($s(id), 'click', { preventDefault() {} });
const typeIn = (id) => fire($s(id), 'keydown', { key: 'Enter', preventDefault() {} });
const hidden = (id) => $s(id).classList.contains('hidden');
const shown = (id) => !hidden(id);
globalThis.document = {
  getElementById: $s,
  createElement: (tag) => makeEl(`dyn-${tag}-${els.size}`),
  body: { appendChild() {} },
  querySelector: () => null,
  addEventListener() {},
};
globalThis.location = { origin: 'http://showroom.test' };

// —— localStorage 垫片（跨场景保留 = 同一浏览器） ——
const store = new Map();
globalThis.localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: (k) => store.delete(k),
};

// —— fetch 垫片：wasm 服务真实字节；/_api 按 mode 返回（unregistered → 404 E_AUTH） ——
const wasmBytes = readFileSync(new URL('./dist/showroom_bg.wasm', import.meta.url));
const resp = (status, obj) => ({ ok: status < 400, status, text: async () => JSON.stringify(obj) });
const PROJECTS = { projects: [{ project: 'demo', slot: 'stable', owner: 'alice', tag: 'v1', url: '/demo/', status: 'running', port: 8100, mem: '256m', cpu: 0, updated: '2026-10-10T12:00:00' }] };
let mode = 'unregistered'; // unregistered | ok
globalThis.fetch = async (url) => {
  const u = String(url);
  if (u.includes('showroom_bg.wasm')) return { ok: true, status: 200, text: async () => '', arrayBuffer: async () => wasmBytes.buffer.slice(wasmBytes.byteOffset, wasmBytes.byteOffset + wasmBytes.byteLength) };
  if (u.includes('/_api/health')) return mode === 'ok' ? resp(200, { ok: true }) : resp(404, { error: 'E_AUTH' });
  if (u.includes('/_api/projects')) return resp(200, PROJECTS);
  if (u.includes('/_api/redis-dbs')) return resp(200, { dbs: [0, 1, 2, 3] });
  if (u.includes('/_api/redis-keys')) return resp(200, { keys: ['sr_demo_stable', 'sr_demo_next'] });
  if (u.includes('/_api/redis-get')) return resp(200, { type: 'string', ttl: -1, value: 'x'.repeat(5000) });
  throw new Error(`smoke: 未预期的请求 ${u}`);
};

// —— 模块提取 + 带查询串重载（每个场景一个全新模块实例） ——
const html = readFileSync(new URL('./dist/index.html', import.meta.url), 'utf8');
const script = html.match(/<script type="module">([\s\S]*?)<\/script>/)[1];
const file = join(mkdtempSync(join(tmpdir(), 'sr-wiz-')), 'wiz.mjs');
writeFileSync(file, script);
const load = (t) => import(pathToFileURL(file).href + `?t=${t}`);

// —— 独立 wasm 实例：由 seed 推导期望公钥串/指纹（交叉验证页面计算） ——
const exW = (await WebAssembly.instantiate(wasmBytes, {})).instance.exports;
const wmem = () => new Uint8Array(exW.memory.buffer);
function pubOf(hexSeed) {
  const p = exW.sr_alloc(32), o = exW.sr_alloc(128);
  for (let i = 0; i < 32; i++) wmem()[p + i] = parseInt(hexSeed.substr(i * 2, 2), 16);
  const r = exW.sr_pub_string(p, o, 128);
  const m = wmem(); let e = o; while (e < o + r && m[e] !== 0) e++;
  const s = new TextDecoder().decode(m.subarray(o, e));
  exW.sr_free(p, 32); exW.sr_free(o, 128);
  return s;
}

// ═══ 场景 1：全新浏览器 → 三步向导 → E_AUTH 循环 → 登记成功进入管理壳 ═══
mode = 'unregistered';
els.clear();
await load(1);
await sleep(40);
ok(shown('wz-p1') && hidden('wz-p2') && hidden('wz-p3'), '全新访问：仅第 1 步可见');
ok($s('wz-s1').classList.contains('on') && !$s('wz-s2').classList.contains('on'), '步骤条指示 1/3');
ok(shown('login') && !els.has('hd-owner'), '登录前：只有登录向导，管理壳未构建（零管理信息）');
$s('wz-owner').value = 'alice';
typeIn('wz-owner'); // 回车 = 下一步
ok(shown('wz-p2') && hidden('wz-p1'), '回车提交：进入第 2 步「选择密钥」');
ok(hidden('wz-l-use'), '无存量密钥：「使用本机已有密钥」不出现');
ok($s('wz-r-new').checked && !$s('wz-r-import').checked && hidden('wz-import-box'), '无存量：默认「生成新密钥」，导入粘贴框隐藏');
ok($s('wz-ownerEcho').textContent === 'alice', '第 2 步回显账号');
click('wz-next2');
const seed1 = store.get('sr.seed');
ok(/^[0-9a-f]{64}$/.test(seed1 || ''), `生成 seed 写入 localStorage（${(seed1 || '').slice(0, 8)}…）`);
ok(store.get('sr.owner') === 'alice', '账号与种子一起记忆（sr.owner）');
ok(shown('wz-p3'), '进入第 3 步「验证」');
await sleep(60);
ok(shown('wz-pubbox'), 'E_AUTH：展示公钥串区块');
ok($s('wz-pub').textContent === pubOf(seed1), '公钥串与 wasm 独立推导一致');
ok(/^ed25519:[A-Za-z0-9+/]{43}$/.test($s('wz-pub').textContent), '公钥串格式 ed25519:<b64-43>');
ok(shown('login'), '未登记：停留在登录向导');
click('wz-recheck');
await sleep(40);
ok(shown('wz-pubbox') && shown('login'), '重新检测：仍未登记 → 公钥展示循环');
mode = 'ok'; // 模拟管理员完成 owner-add 登记
click('wz-recheck');
await sleep(80);
ok(hidden('login') && shown('app') && els.has('hd-owner'), '登记后重检：自动进入管理壳');
ok($s('hd-owner').textContent === 'alice', '管理壳回显账号');
ok(($s('proj-fb').textContent || '').includes('已加载 1 个展项'), '展项列表已请求并渲染');

// —— 登录后 Redis 浏览（§10.3 三端点 + 交互） ——
$s('rd-db').value = '3'; $s('rd-q').value = 'sr_';
click('rd-refresh');
await sleep(60);
ok(($s('rd-db').innerHTML || '').includes('<option value="0">'), 'redis-dbs：填充 DB 下拉');
ok(($s('rd-keys').innerHTML || '').includes('sr_demo_stable'), 'redis-keys：key 列表渲染（前缀过滤请求）');
ok($s('rd-count').textContent === '共 2 个 key（可能截断）', '「共 N 个 key（可能截断）」提示');
$s('rd-keys').value = 'sr_demo_stable';
fire($s('rd-keys'), 'change');
await sleep(60);
const rmeta = $s('rd-meta').textContent || '';
ok(rmeta.includes('类型 string') && rmeta.includes('永久（-1）'), 'redis-get：类型 + 生存时间（-1=永久）');
ok(rmeta.includes('值已截断'), '值 ≥4KB：截断提示');
ok($s('rd-val').textContent.length === 5000, '值以等宽 pre 原样展示');

// ═══ 场景 2：同浏览器再访（账号+种子匹配）→ 直达第 3 步自动进入 ═══
els.clear();
await load(2);
await sleep(80);
ok(hidden('wz-p1') && hidden('wz-p2') && hidden('login') && shown('app'), '再访匹配账号：跳过 1/2 步直达验证并自动进入');
ok(store.get('sr.seed') === seed1 && store.get('sr.owner') === 'alice', '种子与账号未被改动');

// ═══ 场景 3：种子在、账号缺（换了账号走完整向导；第 2 步默认「使用已有」） ═══
mode = 'ok';
store.delete('sr.owner');
els.clear();
await load(3);
await sleep(40);
ok(shown('wz-p1') && hidden('wz-p3'), '无记忆账号：从第 1 步走完整向导');
$s('wz-owner').value = 'bob';
click('wz-next1');
ok(shown('wz-l-use') && $s('wz-r-use').checked, '有存量种子：默认「使用本机已有密钥」');
ok($s('wz-fp').textContent === pubOf(seed1).replace('ed25519:', '').slice(0, 8), '指纹 = 公钥载荷前 8 位');
ok($s('wz-new-text').textContent.includes('重新生成'), '次级单选为「重新生成新密钥」');
click('wz-back2');
ok(shown('wz-p1'), '第 2 步「上一步」可回退');
click('wz-next1');
click('wz-next2'); // 维持默认「使用已有」
await sleep(80);
ok(store.get('sr.seed') === seed1 && store.get('sr.owner') === 'bob', '沿用已有种子，记忆新账号');
ok(hidden('login') && shown('app'), '验证通过进入管理壳');

// ═══ 场景 4：「导入密钥种子」分支 ═══
mode = 'ok';
store.clear();
els.clear();
await load(4);
await sleep(40);
$s('wz-owner').value = 'carl';
click('wz-next1');
fire($s('wz-r-import'), 'change'); // 选导入 → 出现粘贴框
ok(shown('wz-import-box') && $s('wz-r-import').checked, '选「导入」：粘贴框出现');
$s('wz-seed').value = 'not-hex!';
click('wz-next2');
ok(shown('wz-p2') && ($s('msg').textContent || '').includes('64 位十六进制'), '非法 seed：拒绝并停在第 2 步');
$s('wz-seed').value = seed1;
click('wz-next2');
await sleep(80);
ok(store.get('sr.seed') === seed1 && store.get('sr.owner') === 'carl', '合法 seed 导入并记忆');
ok(hidden('login') && shown('app'), '导入后验证通过进入管理壳');

// ═══ 场景 5：静态断言（§10.2 更名 / §10.3 端点 / 无重复声明） ═══
ok(!html.includes('内存上限'), '§10.2：「内存上限」零残留');
ok((html.match(/内存需求/g) || []).length >= 3, '§10.2：表单/帮助/错误文案均用「内存需求」');
ok((html.match(/async function apiRaw/g) || []).length === 1, 'apiRaw 声明唯一（无重复）');
for (const ep of ['redis-dbs', 'redis-keys', 'redis-get']) ok(html.includes(ep), `§10.3：${ep} 已接入`);
ok(!/lg-keygen|lg-importkey|lg-login|renderLoginState|doLogin\b/.test(html), '旧登录区控件/函数无残留');

console.log(failed === 0 ? '\nWIZARD SMOKE PASS' : `\nWIZARD SMOKE FAIL (${failed})`);
process.exit(failed === 0 ? 0 : 1);
