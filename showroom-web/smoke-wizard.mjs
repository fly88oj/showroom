#!/usr/bin/env node
// showroom-web 向导冒烟（SPEC §10.1/§10.2/§10.3/§10.4/§11；无浏览器依赖，node ≥ 18）：
//   把 dist/index.html 的 <script type="module"> 挂到 mock DOM/localStorage/fetch 上执行，
//   驱动登录向导状态机走完全部分支：
//   1) 全新浏览器（无种子无账号）→ 三步向导 → E_AUTH 公钥展示与重检循环
//      → 模拟管理员登记成功 → 进入管理壳（展项列表请求渲染）
//   2) 同浏览器再访（账号+种子匹配）→ 直达第 3 步并自动进入
//   3) 种子在、账号换 → 走完整向导（第 2 步默认「使用本机已有密钥」，指纹与公钥一致）
//   4) 「导入密钥种子」分支：非法 seed 拒绝；合法 seed 进入第 3 步
//   5) §11 去版本字段：发布表单随机 8 位项目名 +「换一个」+ URL 恒 /<project>/
//   6) §10.4 流式消费：日志实时流（ndjson 行缓冲/1000 行帽/时间戳/开关停流）、
//      平台事件流（sr- 过滤/心跳忽略/20 条滚动/2s 去抖刷新/断线 5s 重连）、
//      三段式备份下载（begin → status 两轮轮询 → 两个 4MB chunk → wasm 流式解密 → Blob 落地）
//   7) 静态断言：「内存上限」「槽位/slot」零残留 / §10.3 与 §10.4 端点已接入 / 旧单发 db-dump 已移除
import { readFileSync, writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

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
const tabBtn = (name) => ({ dataset: { tab: name }, closest: () => ({ dataset: { tab: name } }) });
const clickTab = (name) => fire($s('tabs'), 'click', { target: tabBtn(name) });
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

// —— Blob 下载垫片：捕获 createObjectURL 的 Blob 以便校验解密产物 ——
const blobs = [];
globalThis.URL.createObjectURL = (b) => { blobs.push(b); return `blob:mock-${blobs.length}`; };
globalThis.URL.revokeObjectURL = () => {};

// —— wasm 实例（页面用 mock fetch 喂同一份字节；此处独立实例做交叉验证与服务端信封模拟） ——
const wasmBytes = readFileSync(new URL('./dist/showroom_bg.wasm', import.meta.url));
const enc2 = new TextEncoder(), dec2 = new TextDecoder();
const exW = (await WebAssembly.instantiate(wasmBytes, {})).instance.exports;
const wmem = () => new Uint8Array(exW.memory.buffer);
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

// —— fetch 垫片：wasm 服务真实字节；/_api 按 mode 返回（unregistered → 404 E_AUTH） ——
const resp = (status, obj) => ({ ok: status < 400, status, text: async () => JSON.stringify(obj) });
const PROJECTS = { projects: [{ project: 'demo', owner: 'alice', tag: 'v1', url: '/demo/', status: 'running', port: 8100, mem: '256m', cpu: 0, updated: '2026-10-10T12:00:00' }] };
let mode = 'unregistered'; // unregistered | ok
// 观测计数 / 捕获（场景 6 断言用）
let projectsN = 0, eventsN = 0, logsN = 0, statusN = 0;
let lastLogsUrl = '', lastLogsAuth = '', lastEventsAuth = '', beginBody = null;
const chunkCalls = [];
let dumpEnv = null; // { env: 信封字节 }
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
  if (u.includes('showroom_bg.wasm')) return { ok: true, status: 200, text: async () => '', arrayBuffer: async () => wasmBytes.buffer.slice(wasmBytes.byteOffset, wasmBytes.byteOffset + wasmBytes.byteLength) };
  if (u.includes('/_api/health')) return mode === 'ok' ? resp(200, { ok: true }) : resp(404, { error: 'E_AUTH' });
  if (u.includes('/_api/projects')) { projectsN++; return resp(200, PROJECTS); }
  if (u.includes('/_api/redis-dbs')) return resp(200, { dbs: [0, 1, 2, 3] });
  if (u.includes('/_api/redis-keys')) return resp(200, { keys: ['sr_demo'] });
  if (u.includes('/_api/redis-get')) return resp(200, { type: 'string', ttl: -1, value: 'x'.repeat(5000) });
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
    statusN++; // 第一轮 running，第二轮 ready（轮询两轮路径）
    return statusN === 1 ? resp(200, { ready: false, total: 0, error: null })
                         : resp(200, { ready: true, total: dumpEnv.env.length, error: null });
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
  throw new Error(`smoke: 未预期的请求 ${u}`);
};

// —— 模块提取 + 带查询串重载（每个场景一个全新的模块实例） ——
const html = readFileSync(new URL('./dist/index.html', import.meta.url), 'utf8');
const script = html.match(/<script type="module">([\s\S]*?)<\/script>/)[1];
const file = join(mkdtempSync(join(tmpdir(), 'sr-wiz-')), 'wiz.mjs');
writeFileSync(file, script);
const load = (t) => import(pathToFileURL(file).href + `?t=${t}`);

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
ok(($s('rd-keys').innerHTML || '').includes('sr_demo'), 'redis-keys：key 列表渲染（前缀过滤请求）');
ok($s('rd-count').textContent === '共 1 个 key（可能截断）', '「共 N 个 key（可能截断）」提示');
$s('rd-keys').value = 'sr_demo';
fire($s('rd-keys'), 'change');
await sleep(60);
const rmeta = $s('rd-meta').textContent || '';
ok(rmeta.includes('类型 string') && rmeta.includes('永久（-1）'), 'redis-get：类型 + 生存时间（-1=永久）');
ok(rmeta.includes('值已截断'), '值 ≥4KB：截断提示');
ok($s('rd-val').textContent.length === 5000, '值以等宽 pre 原样展示');

// —— §11 发布表单：随机项目名 + 「换一个」 + URL 恒 /<project>/ ——
const rp1 = $s('pub-project').value;
ok(/^[a-zA-Z0-9]{8}$/.test(rp1), `发布表单预填随机 8 位项目名（${rp1}）`);
click('pub-shuffle');
const rp2 = $s('pub-project').value;
ok(/^[a-zA-Z0-9]{8}$/.test(rp2) && rp2 !== rp1, `「换一个」重新随机（${rp1} → ${rp2}）`);
ok(($s('pub-url').textContent || '') === `/${rp2}/`, `URL 提示恒 /<项目名>/（当前 /${rp2}/）`);
$s('pub-project').value = 'demo'; // 手输（用户可清空手输的路径）
fire($s('pub-project'), 'input');
ok($s('pub-url').textContent === '/demo/', 'URL 提示随手输联动');
ok(!els.has('pub-slot') || $s('pub-slot') === undefined, '发布表单无版本维度控件');

// ═══ 场景 6a：§10.4 平台事件流（进入状态页自动订阅） ═══
const projBase = projectsN;
clickTab('status');
await sleep(80);
ok(eventsN === 1, '进入状态页：自动订阅 /_api/events');
ok(/^SR-SIG1 /.test(lastEventsAuth), '事件流为签名 GET（SR-SIG1，空 body 哈希）');
ok(projectsN === projBase + 1, '进入状态页：健康列表刷新一次');
ok(($s('ev-state').textContent || '').includes('5 秒后自动重连'), '事件流关闭（done）→ 显示 5 秒重连');
const evTxt = $s('st-events').textContent || '';
ok(evTxt.includes('sr-demo') && evTxt.includes('启动') && evTxt.includes('转为健康'), '事件渲染：时间+动作+容器（含健康检查翻译）');
ok(!evTxt.includes('other-box'), '非 sr- 前缀容器被过滤');
ok(!evTxt.includes('hb') && !evTxt.includes('true'), '心跳行被忽略');
ok(/^\d{2}:\d{2}:\d{2}/.test(evTxt), '事件行带时间戳（HH:MM:SS）');
await sleep(2100);
ok(projectsN === projBase + 2, '事件到达 → 2 秒去抖自动刷新健康列表');
ok(await waitFor(() => eventsN === 2, 4500), '断线 5 秒后自动重连（第二次订阅）');
await sleep(80);
ok(($s('st-events').textContent || '').includes('sr-demo2') && ($s('st-events').textContent || '').includes('停止'), '重连后新事件继续滚动（最近 20 条）');
clickTab('proj');
ok(($s('ev-state').textContent || '').includes('未连接'), '离开状态页：事件流停止');

// ═══ 场景 6b：§10.4 日志实时流（ndjson 行缓冲 + 1000 行帽 + 开关停流） ═══
clickTab('status');
$s('st-logs-live').checked = true; // 与真实浏览器 HTML 默认 checked 对齐
fire($s('st-logs-live'), 'change');
fire($s('st-tbl'), 'click', { target: { closest: () => ({ dataset: { logs: '', p: 'demo' } }) } });
await waitFor(() => logsN === 1, 3000);
ok(lastLogsUrl.includes('/_api/logs-stream?project=demo&tail=100'), `日志流请求仅按项目（${lastLogsUrl}）`);
ok(!lastLogsUrl.includes('slot'), '日志流请求无版本参数');
ok(/^SR-SIG1 /.test(lastLogsAuth), '日志流为签名 GET（SR-SIG1）');
await waitFor(() => ($s('st-logs').textContent || '').includes('log-1002'), 5000);
const logTxt = $s('st-logs').textContent || '';
const logRows = logTxt.split('\n');
ok(logRows.length === 1000, `1000 行环形帽（收 1003 行，保留 ${logRows.length}）`);
ok(logTxt.includes('log-0003') && !logTxt.includes('log-0002'), '超帽丢弃头部（log-0000..0002 被裁）');
ok(/^\d{2}:\d{2}:\d{2} log-/m.test(logTxt), '每行带格式化时间戳前缀');
$s('st-logs-live').checked = false;
fire($s('st-logs-live'), 'change');
ok(($s('st-logs-fb').textContent || '').includes('已停止实时跟随'), '开关关闭 → 停流提示');
ok(logsN === 1, '停流后不再发起日志请求（AbortController）');

// ═══ 场景 6c：§10.4 三段式备份下载（begin → 轮询两轮 → 两个 chunk → 解密落地） ═══
blobs.length = 0;
$s('db-project').value = 'demo';
click('btn-db-dump');
ok(await waitFor(() => ($s('db-fb').textContent || '').includes('备份已下载'), 15000), '备份完成并提示下载');
ok(beginBody && beginBody.project === 'demo' && !('slot' in beginBody), 'db-dump-begin：仅项目 + 一次性公钥');
ok(/^-----BEGIN PUBLIC KEY-----/.test(beginBody?.client_pub_pem || ''), '一次性 RSA 公钥 PEM 已上行');
ok(statusN === 2, `status 轮询两轮后 ready（实际 ${statusN}）`);
ok(chunkCalls.length === 2, `4MB 分片循环两个 chunk（实际 ${chunkCalls.length}）`);
ok(chunkCalls[0]?.off === 0 && chunkCalls[0]?.len === 4 * 1024 * 1024, '第一片 offset=0 len=4MiB');
ok(chunkCalls[1]?.off === 4 * 1024 * 1024 && chunkCalls[1]?.len === dumpEnv.env.length - 4 * 1024 * 1024, '第二片 offset=4MiB len=尾片');
ok($s('db-prog').value === $s('db-prog').max && Number($s('db-prog').max) === dumpEnv.env.length, '进度条按 offset/total 收满');
ok(blobs.length === 1, '产物经 Blob 下载（createObjectURL 一次）');
{
  const got = new Uint8Array(await blobs[0].arrayBuffer());
  let same = got.length === dumpEnv.plain.length
    && got[0] === dumpEnv.plain[0] && got[got.length - 1] === dumpEnv.plain[dumpEnv.plain.length - 1];
  for (let i = 0; same && i < got.length; i += 4096) if (got[i] !== dumpEnv.plain[i]) same = false;
  ok(same, `分片信封经 wasm 流式解密还原明文（${got.length} 字节，抽样+首尾比对）`);
}
ok(($s('db-out').textContent || '').includes('三段式下载完成'), 'dbOut 记录三段式结果');

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

// ═══ 场景 5：静态断言（§10.2 更名 / §10.3+§10.4 端点 / §11 零残留） ═══
ok(!html.includes('内存上限'), '§10.2：「内存上限」零残留');
ok((html.match(/内存需求/g) || []).length >= 3, '§10.2：表单/帮助/错误文案均用「内存需求」');
ok(!/槽位|slot/i.test(html), '§11：全 HTML「槽位/slot」零残留');
ok(!html.includes('SHOWROOM_SLOT'), '§11：环境变量清单去掉 SHOWROOM_SLOT');
ok(html.includes('^[a-zA-Z0-9][a-zA-Z0-9-]{0,30}$'), '§11：项目名规则已放宽（允许大写字母）');
ok(!html.includes('apiRaw'), '旧单发备份路径 apiRaw 已移除');
ok(!html.includes("'/_api/db-dump'"), '旧单发 POST /_api/db-dump 无残留');
for (const ep of ['redis-dbs', 'redis-keys', 'redis-get']) ok(html.includes(ep), `§10.3：${ep} 已接入`);
for (const ep of ['/_api/logs-stream', '/_api/events', '/_api/db-dump-begin', '/_api/db-dump-status', '/_api/db-dump-chunk']) ok(html.includes(ep), `§10.4：${ep} 已接入`);
ok(!/lg-keygen|lg-importkey|lg-login|renderLoginState|doLogin\b/.test(html), '旧登录区控件/函数无残留');

console.log(failed === 0 ? '\nWIZARD SMOKE PASS' : `\nWIZARD SMOKE FAIL (${failed})`);
process.exit(failed === 0 ? 0 : 1);
