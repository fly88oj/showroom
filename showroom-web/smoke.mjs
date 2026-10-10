#!/usr/bin/env node
// showroom-web 冒烟（无浏览器依赖；node ≥ 18）：
//   1) sr_entropy → sr_generate → sr_pub_string：前缀/格式断言
//   2) sr_sign_header：产出头字段断言 + node:crypto 独立 Ed25519 验签（SPEC §1 canonical 逐字节兼容）
//   3) sr_seal_begin(_seed)/push/flush/end：
//      - 信封总长不变量 384 + n + ceil(n/4MiB)*(12+16)（SPEC §2）
//      - node 独立解封（RSA-OAEP-SHA256 unwrap + 分段 AES-256-GCM）恢复原文
//      - 同 seed 重放逐字节一致（§8.1 断点续传前提）
//   4) 错误码路径：容量不足/坏指针/坏会话
import { readFileSync } from 'node:fs';
import {
  createHash, createDecipheriv, createPrivateKey, createPublicKey,
  generateKeyPairSync, privateDecrypt, constants as rc, verify as edVerify,
} from 'node:crypto';

const wasmUrl = new URL('./dist/showroom_bg.wasm', import.meta.url);
const ex = (await WebAssembly.instantiate(readFileSync(wasmUrl), {})).instance.exports;
const enc = new TextEncoder(), dec = new TextDecoder();
// wasm memory 可能随内部分配增长而换代：任何 wasm 调用之后都要重取 view
const mem = () => new Uint8Array(ex.memory.buffer);

const alloc = (n) => { const p = ex.sr_alloc(n); if (!p) throw new Error('sr_alloc 失败'); return p; };
const put = (bytes) => { const p = alloc(bytes.length); mem().set(bytes, p); return [p, bytes.length]; };
const readCStr = (p, written) => { const m = mem(); let e = p; while (e < p + written && m[e] !== 0) e++; return dec.decode(m.subarray(p, e)); };
let failed = 0;
const ok = (cond, msg) => { console.log(`${cond ? 'ok  -' : 'FAIL-'} ${msg}`); if (!cond) failed++; };

// —— 熵注入（浏览器侧由 crypto.getRandomValues 承担）——
const ent = new Uint8Array(64); crypto.getRandomValues(ent);
{ const [p, l] = put(ent); ok(ex.sr_entropy(p, l) === 0, 'sr_entropy 注入 64B → 0'); ex.sr_free(p, l); }

// —— 1) keygen / pub ——
const sp = alloc(32);
ok(ex.sr_generate(sp) === 0, 'sr_generate → 0');
const seed = Buffer.from(mem().slice(sp, sp + 32));
ok(seed.length === 32 && !seed.equals(Buffer.alloc(32)), 'seed 32B 非全零');
const pp = alloc(128);
const r1 = ex.sr_pub_string(sp, pp, 128);
ok(r1 > 0, `sr_pub_string → ${r1}（含 NUL）`);
const pubStr = readCStr(pp, r1);
ok(/^ed25519:[A-Za-z0-9+/]{43}$/.test(pubStr), `公钥串格式 (${pubStr.slice(0, 16)}…)`);
ex.sr_free(pp, 128);

// —— 2) sign_header + node 独立验签 ——
const owner = 'alice', method = 'POST', path = '/_api/publish', nonce = '0123456789abcdef', ts = 1000000n;
const body = Buffer.from('{"project":"demo"}');
const [op, ol] = put(enc.encode(owner)), [mp, ml] = put(enc.encode(method)),
      [pp2, pl] = put(enc.encode(path)), [np, nl] = put(enc.encode(nonce)),
      [bp, bl] = put(body);
const hp = alloc(512);
const r2 = ex.sr_sign_header(sp, op, ol, mp, ml, pp2, pl, ts, np, nl, bp, bl, hp, 512);
ok(r2 > 0, `sr_sign_header → ${r2}`);
const auth = readCStr(hp, r2);
const parts = auth.replace('SR-SIG1 ', '').split(':');
ok(auth.startsWith('SR-SIG1 ') && parts.length === 4 && parts[0] === 'alice' && parts[1] === '1000000' && parts[2] === nonce,
  '头四字段 owner/ts/nonce');
const sig = Buffer.from(parts[3], 'base64url');
ok(sig.length === 64, '签名 64B（b64url 无 padding）');
// node 侧以独立实现验证：PKCS8(Ed25519) = 固定前缀 || 32B seed
const DER_PKCS8_ED25519 = Buffer.from('302e020100300506032b657004220420', 'hex');
const priv = createPrivateKey({ key: Buffer.concat([DER_PKCS8_ED25519, seed]), format: 'der', type: 'pkcs8' });
const canonical = `SR-SIG1\n${owner}\n1000000\n${nonce}\n${method}\n${path}\n${createHash('sha256').update(body).digest('hex')}`;
ok(edVerify(null, Buffer.from(canonical), createPublicKey(priv), sig), 'node Ed25519 验签通过（canonical 兼容）');
for (const [p, l] of [[op, ol], [mp, ml], [pp2, pl], [np, nl], [bp, bl]]) ex.sr_free(p, l);
ex.sr_free(hp, 512);

// —— 3) seal 会话 ——
const SEG = 4 * 1024 * 1024, WRAP = 384, NON = 12, TAG = 16;
const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 3072 });
const pem = Buffer.from(publicKey.export({ type: 'spki', format: 'pem' }));

function sealAll(seedBuf, input) {
  const [pemp, peml] = put(pem), [ssp, ssl] = put(seedBuf);
  const sid = ex.sr_seal_begin_seed(pemp, peml, ssp);
  ok(sid > 0, `sr_seal_begin_seed → 会话 ${sid}`);
  const parts = [];
  const CHUNK = 8 * 1024 * 1024; // 控制台上传分片粒度
  for (let o = 0; o < input.length; o += CHUNK) {
    const slice = input.subarray(o, Math.min(o + CHUNK, input.length));
    const [ip, il] = put(slice);
    const cap = WRAP + NON + TAG + slice.length + (Math.ceil(slice.length / SEG) + 2) * (NON + TAG); // 宽裕上界
    const ob = alloc(cap);
    const w = ex.sr_seal_push(sid, ip, il, ob, cap);
    ok(w >= 0, `push ${slice.length}B 明文 → ${w}B 密文`);
    if (w > 0) parts.push(Buffer.from(mem().subarray(ob, ob + w)));
    ex.sr_free(ip, il); ex.sr_free(ob, cap);
  }
  const cap2 = WRAP + NON + TAG + SEG; // flush 尾段宽裕上界
  const ob2 = alloc(cap2);
  const w2 = ex.sr_seal_push(sid, 0, 0, ob2, cap2); // len=0 = 流结束
  ok(w2 >= 0, `flush → ${w2}B`);
  if (w2 > 0) parts.push(Buffer.from(mem().subarray(ob2, ob2 + w2)));
  ex.sr_free(ob2, cap2);
  ok(ex.sr_seal_end(sid) === 0, 'sr_seal_end → 0');
  for (const [p, l] of [[pemp, peml], [ssp, ssl]]) ex.sr_free(p, l);
  return Buffer.concat(parts);
}

const n = SEG * 2 + 1234; // 跨段 + 尾部零头
const plain = Buffer.alloc(n, 0xab); plain[0] = 1; plain[n - 1] = 2;
const sessSeed = Buffer.from('00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff', 'hex');
ok(sessSeed.length === 32, '会话 seed 32B');
const sealed = sealAll(sessSeed, plain);
ok(sealed.length === WRAP + n + Math.ceil(n / SEG) * (NON + TAG),
  `信封总长不变量 384+${n}+${Math.ceil(n / SEG)}*28 = ${sealed.length}`);
ok(sealed2equals(), '同 seed 重放逐字节一致（断点续传前提）');
function sealed2equals() { return sealed.equals(sealAll(sessSeed, plain)); }

// node 独立解封：unwrap k → 逐段 AES-256-GCM
const k = privateDecrypt({ key: privateKey, padding: rc.RSA_PKCS1_OAEP_PADDING, oaepHash: 'sha256' }, sealed.subarray(0, WRAP));
ok(k.length === 32, 'RSA-OAEP-SHA256 unwrap → 32B 会话密钥');
let off = WRAP; const outs = [];
while (off < sealed.length) {
  const iv = sealed.subarray(off, off + NON); off += NON;
  const rest = sealed.length - off;
  const segLen = rest >= SEG + TAG ? SEG + TAG : rest; // 除末段外均为满段
  const ct = sealed.subarray(off, off + segLen); off += segLen;
  const d = createDecipheriv('aes-256-gcm', k, iv);
  d.setAuthTag(ct.subarray(segLen - TAG));
  outs.push(d.update(ct.subarray(0, segLen - TAG)), d.final());
}
ok(Buffer.concat(outs).equals(plain), 'node 解封恢复原文（信封逐字节兼容）');

// 随机会话路径（熵池）+ 小输入
{
  const [pemp, peml] = put(pem);
  const sid = ex.sr_seal_begin(pemp, peml);
  ok(sid > 0, `sr_seal_begin（随机）→ 会话 ${sid}`);
  const small = Buffer.from('showroom-smoke-30-bytes-abcdefghijkl');
  const [ip, il] = put(small);
  const ob = alloc(WRAP + small.length + NON + TAG + 64);
  const w = ex.sr_seal_push(sid, ip, il, ob, WRAP + small.length + NON + TAG + 64);
  const w2 = ex.sr_seal_push(sid, 0, 0, ob, 64); // 尾段写出
  const total = w + w2;
  ok(total === WRAP + small.length + NON + TAG, `小输入单段总长 ${total}`);
  ex.sr_seal_end(sid);
  for (const [p, l] of [[pemp, peml], [ip, il]]) ex.sr_free(p, l);
  ex.sr_free(ob, WRAP + small.length + NON + TAG + 64);
}

// —— 4) 错误码 ——
ok(ex.sr_seal_push(9999, 0, 0, 0, 0) === -5, '不存在的会话 → -5');
{ const [p, l] = put(pem.subarray(0, l0(pem) - 30)); ok(ex.sr_seal_begin(p, l) === -3, '截断 PEM → -3'); ex.sr_free(p, l); }
function l0(b) { return b.length; }
{ const small = alloc(4); ok(ex.sr_pub_string(sp, small, 4) < 0, '容量不足 → 负所需长度（-52）'); ex.sr_free(small, 4); }
ok(ex.sr_pub_string(sp, 0, 128) === -2, '空输出指针（容量足）→ -2');
ex.sr_free(sp, 32);

console.log(failed === 0 ? '\nSMOKE PASS' : `\nSMOKE FAIL (${failed})`);
process.exit(failed === 0 ? 0 : 1);
