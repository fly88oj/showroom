#!/usr/bin/env node
// showroom-web 冒烟（无浏览器依赖；node ≥ 18）：
//   1) sr_entropy → sr_generate → sr_pub_string：前缀/格式断言
//   2) sr_sign_header：产出头字段断言 + node:crypto 独立 Ed25519 验签（SPEC §1 canonical 逐字节兼容）
//   3) sr_seal_begin(_seed)/push/flush/end：
//      - 信封总长不变量 384 + n + ceil(n/4MiB)*(12+16)（SPEC §2）
//      - node 独立解封（RSA-OAEP-SHA256 unwrap + 分段 AES-256-GCM）恢复原文
//      - 同 seed 重放逐字节一致（§8.1 断点续传前提）
//   3b) SPEC §9 下行：sr_keygen_rsa → sr_rsa_{pub,priv}_pem 非空；
//       seal（wasm 一次性对 / node 平台对）→ sr_open_begin/push/flush/end 流式解封往返
//       （奇数分块推送 + 跨段明文 + 空明文 + 截断/坏 PEM/坏会话错误码）
//   3c) zstd 压缩会话（上传压缩统一）：sr_zstd_begin(19)/push/end——每次 push 一个独立
//       完整 zstd 帧，多段输出拼接后由系统 zstd CLI 解压比对（node 无内置 zstd）；
//       同分片重放逐字节一致（断点续传前提）；空输入/坏 level/坏会话/容量探测错误码
//   4) 错误码路径：容量不足/坏指针/坏会话
import { readFileSync, writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import {
  createHash, createDecipheriv, createPrivateKey, createPublicKey,
  generateKeyPairSync, privateDecrypt, constants as rc, verify as edVerify,
} from 'node:crypto';

const wasmUrl = new URL('./console/public/showroom_bg.wasm', import.meta.url);
const ex = (await WebAssembly.instantiate(readFileSync(wasmUrl), {})).instance.exports;
const enc = new TextEncoder(), dec = new TextDecoder();
// wasm memory 可能随内部分配增长而换代：任何 wasm 调用之后都要重取 view
const mem = () => new Uint8Array(ex.memory.buffer);

const alloc = (n) => { const p = ex.sr_alloc(n); if (!p) throw new Error(`sr_alloc(${n}) 失败（内存 ${ex.memory.buffer.byteLength / 65536} 页）`); return p; };
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
const user = 'alice', method = 'POST', path = '/_api/publish', nonce = '0123456789abcdef', ts = 1000000n;
const body = Buffer.from('{"project":"demo"}');
const [op, ol] = put(enc.encode(user)), [mp, ml] = put(enc.encode(method)),
      [pp2, pl] = put(enc.encode(path)), [np, nl] = put(enc.encode(nonce)),
      [bp, bl] = put(body);
const hp = alloc(512);
const r2 = ex.sr_sign_header(sp, op, ol, mp, ml, pp2, pl, ts, np, nl, bp, bl, hp, 512);
ok(r2 > 0, `sr_sign_header → ${r2}`);
const auth = readCStr(hp, r2);
const parts = auth.replace('SR-SIG1 ', '').split(':');
ok(auth.startsWith('SR-SIG1 ') && parts.length === 4 && parts[0] === 'alice' && parts[1] === '1000000' && parts[2] === nonce,
  '头四字段 user/ts/nonce');
const sig = Buffer.from(parts[3], 'base64url');
ok(sig.length === 64, '签名 64B（b64url 无 padding）');
// node 侧以独立实现验证：PKCS8(Ed25519) = 固定前缀 || 32B seed
const DER_PKCS8_ED25519 = Buffer.from('302e020100300506032b657004220420', 'hex');
const priv = createPrivateKey({ key: Buffer.concat([DER_PKCS8_ED25519, seed]), format: 'der', type: 'pkcs8' });
const canonical = `SR-SIG1\n${user}\n1000000\n${nonce}\n${method}\n${path}\n${createHash('sha256').update(body).digest('hex')}`;
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

// —— 3b) SPEC §9 下行解封：一次性 RSA 对 + open 会话 ——
// 容量不足自动扩容重试（-(所需) ≥ 40 与错误码 |e| ≤ 7 可区分）
function shimCStr(fn, cap0) {
  let cap = cap0, p = alloc(cap);
  let r = fn(p, cap);
  while (r < 0 && r <= -40) { ex.sr_free(p, cap); cap = -r; p = alloc(cap); r = fn(p, cap); }
  const s = r > 0 ? readCStr(p, r) : null;
  ex.sr_free(p, cap);
  return [r, s];
}
const kid = ex.sr_keygen_rsa();
ok(kid > 0, `sr_keygen_rsa → 会话 ${kid}`);
const [rpub, wasmPubPem] = shimCStr((p, c) => ex.sr_rsa_pub_pem(kid, p, c), 1024);
ok(rpub > 0 && wasmPubPem.startsWith('-----BEGIN PUBLIC KEY-----'), 'sr_rsa_pub_pem → SPKI PEM 非空');
const [rpriv, wasmPrivPem] = shimCStr((p, c) => ex.sr_rsa_priv_pem(kid, p, c), 4096);
ok(rpriv > 0 && wasmPrivPem.startsWith('-----BEGIN PRIVATE KEY-----'), 'sr_rsa_priv_pem → PKCS#8 PEM 非空');

// open 流式解封：奇数分块推送（跨段边界 carry）+ len=0 flush
function openAll(privPemStr, envelope, chunk) {
  const [pp, pl] = put(Buffer.from(privPemStr, 'utf8'));
  const sid = ex.sr_open_begin(pp, pl);
  ex.sr_free(pp, pl);
  if (!(sid > 0)) { ok(false, `sr_open_begin → ${sid}`); return null; }
  ok(sid > 0, `sr_open_begin → 会话 ${sid}`);
  const push = (bytes) => {
    const [ip, il] = bytes.length ? put(bytes) : [0, 0]; // 空输入=流结束信号，免分配
    let cap = bytes.length + SEG + 64, ob = alloc(cap);
    let w = ex.sr_open_push(sid, ip, il, ob, cap);
    if (w < 0 && w <= -40) { ex.sr_free(ob, cap); cap = -w; ob = alloc(cap); w = ex.sr_open_push(sid, ip, il, ob, cap); }
    const out = w > 0 ? Buffer.from(mem().subarray(ob, ob + w)) : null;
    ex.sr_free(ip, il); ex.sr_free(ob, cap);
    if (w < 0) throw new Error(`sr_open_push → ${w}`);
    return out;
  };
  const outs = [];
  for (let o = 0; o < envelope.length; o += chunk) {
    const part = push(envelope.subarray(o, Math.min(o + chunk, envelope.length)));
    if (part) outs.push(part);
  }
  const tail = push(new Uint8Array(0)); // len=0 = 流结束
  if (tail) outs.push(tail);
  ok(ex.sr_open_end(sid) === 0, 'sr_open_end → 0');
  return Buffer.concat(outs);
}
function sealOneShot(pubPemBuf, input) { // 随机会话整段封包（信封全量返回）
  const [pemp, peml] = put(pubPemBuf);
  const sid = ex.sr_seal_begin(pemp, peml);
  ex.sr_free(pemp, peml);
  if (!(sid > 0)) throw new Error(`sr_seal_begin → ${sid}`);
  const parts = [];
  const CHUNK2 = 5 * 1024 * 1024 + 999; // 奇数块，与 4MiB 段边界不对齐
  for (let o = 0; o < input.length; o += CHUNK2) {
    const [ip, il] = put(input.subarray(o, Math.min(o + CHUNK2, input.length)));
    const cap = WRAP + NON + TAG + il + (Math.ceil(il / SEG) + 2) * (NON + TAG);
    const ob = alloc(cap);
    const w = ex.sr_seal_push(sid, ip, il, ob, cap);
    if (w > 0) parts.push(Buffer.from(mem().subarray(ob, ob + w)));
    ex.sr_free(ip, il); ex.sr_free(ob, cap);
    if (w < 0) throw new Error(`seal push → ${w}`);
  }
  { const cap = WRAP + NON + TAG + SEG + 64, ob = alloc(cap);
    const w = ex.sr_seal_push(sid, 0, 0, ob, cap);
    if (w > 0) parts.push(Buffer.from(mem().subarray(ob, ob + w)));
    ex.sr_free(ob, cap);
    if (w < 0) throw new Error(`seal flush → ${w}`); }
  ex.sr_seal_end(sid);
  return Buffer.concat(parts);
}

// b1. wasm 一次性对：seal → open 往返（跨段 + 尾部零头，1MB+123 奇数分块喂 open）
{
  const n2 = SEG + 7777;
  const plain2 = Buffer.alloc(n2, 0x5a); plain2[0] = 9; plain2[n2 - 1] = 8;
  const env2 = sealOneShot(Buffer.from(wasmPubPem, 'utf8'), plain2);
  ok(env2.length === WRAP + n2 + Math.ceil(n2 / SEG) * (NON + TAG), `wasm 对信封总长 ${env2.length}`);
  ok(openAll(wasmPrivPem, env2, 1024 * 1024 + 123).equals(plain2), 'wasm 对 seal→open 往返（奇数分块）');
}
// b2. node 平台对（PKCS#8 私钥 PEM，即 WebCrypto exportKey 同格式）：open 解封 §3 已产出的信封
{
  const nodePrivPem = privateKey.export({ type: 'pkcs8', format: 'pem' }).toString();
  ok(openAll(nodePrivPem, sealed, 4096 + 7).equals(plain), 'node 对 seal→open 往返（跨 3 段信封）');
}
// b3. 空明文信封（仅 384B wrap 头）
{
  const env0 = sealOneShot(Buffer.from(wasmPubPem, 'utf8'), Buffer.alloc(0));
  ok(env0.length === WRAP, `空明文信封 = ${env0.length}B`);
  ok(openAll(wasmPrivPem, env0, 4096).length === 0, '空明文解封 → 0B');
}
// b4. 错误码：截断信封 / flush 后再 push / 坏 PEM / 坏会话
{
  const n3 = 1234;
  const env3 = sealOneShot(Buffer.from(wasmPubPem, 'utf8'), Buffer.alloc(n3, 7));
  const [pp3, pl3] = put(Buffer.from(wasmPrivPem, 'utf8'));
  const sid3 = ex.sr_open_begin(pp3, pl3);
  ex.sr_free(pp3, pl3);
  const [ip3, il3] = put(env3.subarray(0, env3.length - 5)); // 截去尾段 5 字节
  const ob3 = alloc(SEG + 64);
  ok(ex.sr_open_push(sid3, ip3, il3, ob3, SEG + 64) === 0, '截断信封 push：唯一段<满段，暂存不出明文（0）');
  ok(ex.sr_open_push(sid3, 0, 0, ob3, SEG + 64) === -7, '截断尾段 flush → -7');
  ex.sr_free(ip3, il3); ex.sr_free(ob3, SEG + 64);
  ex.sr_open_end(sid3);
  // flush 后再 push → -2（API 误用）
  const env4 = sealOneShot(Buffer.from(wasmPubPem, 'utf8'), Buffer.from('x'));
  { const [pp4, pl4] = put(Buffer.from(wasmPrivPem, 'utf8'));
    const s4 = ex.sr_open_begin(pp4, pl4); ex.sr_free(pp4, pl4);
    const [ip4, il4] = put(env4); const ob4 = alloc(SEG + 64);
    ex.sr_open_push(s4, ip4, il4, ob4, SEG + 64);
    ex.sr_open_push(s4, 0, 0, ob4, SEG + 64); // flush
    ok(ex.sr_open_push(s4, ip4, il4, ob4, SEG + 64) === -2, 'flush 后再 push → -2');
    ex.sr_free(ip4, il4); ex.sr_free(ob4, SEG + 64);
    ex.sr_open_end(s4);
  }
  { const [gp, gl] = put(Buffer.from('not a pem'));
    ok(ex.sr_open_begin(gp, gl) === -3, '坏私钥 PEM → -3'); ex.sr_free(gp, gl); }
  ok(ex.sr_open_push(9999, 0, 0, 0, 0) === -5, 'open 不存在的会话 → -5');
  ok(ex.sr_open_end(9999) === -5, 'open end 不存在的会话 → -5');
}

// —— 3c) zstd 压缩会话：独立帧 push 拼接 → 系统 zstd CLI 解压比对 + 确定性重放 + 错误码 ——
{
  const ZCHUNK = 8 * 1024 * 1024; // 控制台上传分片粒度（compressToBlob 同粒度）
  const mk = (n, mod) => { const b = Buffer.alloc(n); for (let i = 0; i < n; i++) b[i] = (i * 31 + 7) % mod; return b; };
  // > 8MB 一片 + 恰好 8MB 一片 + 尾部零头 + 单字节（跨帧边界形态全覆盖）
  const parts = [mk(ZCHUNK + 12345, 251), mk(ZCHUNK, 199), mk(4097, 17), Buffer.from([0x00])];
  const run = () => {
    const sid = ex.sr_zstd_begin(19);
    ok(sid > 0, `sr_zstd_begin(19) → 会话 ${sid}`);
    const frames = [];
    for (const p of parts) {
      const [ip, il] = put(p);
      const cap = p.length + (p.length >> 3) + 4096; // ≥ 垫片上界 len+len/256+64
      const ob = alloc(cap);
      const w = ex.sr_zstd_push(sid, ip, il, ob, cap);
      if (w < 0) { ex.sr_free(ip, il); ex.sr_free(ob, cap); throw new Error(`zstd push → ${w}`); }
      frames.push(Buffer.from(mem().subarray(ob, ob + w)));
      ex.sr_free(ip, il); ex.sr_free(ob, cap);
    }
    // 空输入合法但不产帧（返回 0）
    const ob0 = alloc(64);
    ok(ex.sr_zstd_push(sid, 0, 0, ob0, 64) === 0, '空输入 push → 0（不产空帧）');
    ex.sr_free(ob0, 64);
    ok(ex.sr_zstd_end(sid) === 0, 'sr_zstd_end → 0');
    return frames;
  };
  const raw = Buffer.concat(parts);
  const f1 = run(), f2 = run();
  ok(f1.length === parts.length, `每段产一个独立帧（${f1.length} 帧）`);
  const z1 = Buffer.concat(f1), z2 = Buffer.concat(f2);
  ok(z1.equals(z2), '同分片重放逐字节一致（断点续传重放前提）');
  ok(z1.subarray(0, 4).equals(Buffer.from('28b52ffd', 'hex')), '输出首 4 字节 = zstd 魔数 28 B5 2F FD');
  ok(z1.length < raw.length, `压缩生效：${raw.length} → ${z1.length} 字节`);
  // node 无内置 zstd：产物落临时文件，由系统 zstd CLI 解压回读比对（多帧拼接流整体解码）
  const dir = mkdtempSync(join(tmpdir(), 'sr-zstd-smoke-'));
  const zf = join(dir, 'multi.zst'), of = join(dir, 'out.bin');
  writeFileSync(zf, z1);
  execFileSync('zstd', ['-d', '-qf', '-o', of, zf]);
  ok(readFileSync(of).equals(raw), `系统 zstd CLI 解压拼接帧流恢复原文（${raw.length} 字节）`);
  // 错误码：坏 level / 坏会话 / 容量探测（负值 ≤ -40 与 |e| ≤ 7 区分）
  ok(ex.sr_zstd_begin(0) === -2 && ex.sr_zstd_begin(23) === -2, 'level 越界（0/23）→ -2');
  ok(ex.sr_zstd_push(9999, 0, 0, 0, 0) === -5, '不存在的会话 → -5');
  ok(ex.sr_zstd_end(9999) === -5, 'zstd end 不存在的会话 → -5');
  {
    const sid = ex.sr_zstd_begin(19);
    const [ip, il] = put(Buffer.from('0123456789abcdef'));
    const ob = alloc(8); // 容量不足
    const r = ex.sr_zstd_push(sid, ip, il, ob, 8);
    ok(r <= -40, `容量不足 → 负所需长度（${r}，下限 40 与错误码区分）`);
    const cap = 4096, ob2 = alloc(cap);
    ok(ex.sr_zstd_push(sid, ip, il, ob2, cap) > 0, '扩容后原样重试成功（会话状态未消耗）');
    ex.sr_free(ip, il); ex.sr_free(ob, 8); ex.sr_free(ob2, cap);
    ex.sr_zstd_end(sid);
  }
}

// —— 4) 错误码 ——
ok(ex.sr_seal_push(9999, 0, 0, 0, 0) === -5, '不存在的会话 → -5');
{ const [p, l] = put(pem.subarray(0, pem.length - 30)); ok(ex.sr_seal_begin(p, l) === -3, '截断 PEM → -3'); ex.sr_free(p, l); }
{ const small = alloc(4); ok(ex.sr_pub_string(sp, small, 4) < 0, '容量不足 → 负所需长度（-52）'); ex.sr_free(small, 4); }
ok(ex.sr_pub_string(sp, 0, 128) === -2, '空输出指针（容量足）→ -2');
ex.sr_free(sp, 32);

console.log(failed === 0 ? '\nSMOKE PASS' : `\nSMOKE FAIL (${failed})`);
process.exit(failed === 0 ? 0 : 1);
