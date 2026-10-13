// —— envelope.ts：端到端加密信封与 zstd 压缩（wasm 垫片收口层）——
// 上一轮评审（design#3/#4）：seal/open/zstd 的 wasm 内存手动协议（put/unput/容量扩容）
// 只在本模块出现，调用方（upload/Data）只见 Blob/Uint8Array 深接口。
import { enc } from "@/lib/bytes";
import { alloc, ex, mem, put, shimCStr, unput } from "@/lib/wasm";
import { CHUNK, NONCE, TAG, WRAP, SEG } from "@/lib/state";

export const sealedTotal = (n: number) => WRAP + n + Math.ceil(n / SEG) * (NONCE + TAG);
/** 尾段输出量（重放计数用；SEG 整除时无尾段明文 → 0）。 */
export const flushLen = (plainBytes: number) => (plainBytes % SEG) ? (plainBytes % SEG) + NONCE + TAG : 0;
const ZSTD_LEVEL = 19;

/** seal 会话：begin（PEM+种子）→ pushSlice/pushFlush → end。 */
export function sealBegin(pem: string, seedBytes: Uint8Array): number {
  const [pemp, peml] = put(enc.encode(pem));
  const [ssp, ssl] = put(seedBytes);
  const sid = (ex.sr_seal_begin_seed as (a: number, b: number, c: number) => number)(pemp, peml, ssp);
  unput(pemp, peml);
  unput(ssp, ssl);
  if (sid <= 0) throw { error: "E_NET", msg: `加密会话创建失败（${sid}）：请检查平台公钥 PEM` };
  return sid;
}

export function pushSlice(sid: number, bytes: Uint8Array): [number, number, number] {
  const [ip, il] = put(bytes);
  const cap = WRAP + NONCE + TAG + bytes.length + (Math.ceil(bytes.length / SEG) + 2) * (NONCE + TAG) + 64;
  const ob = alloc(cap);
  const w = (ex.sr_seal_push as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, ob, cap);
  unput(ip, il);
  if (w < 0) { unput(ob, cap); throw { error: "E_NET", msg: `本地加密失败（${w}）` }; }
  return [w, ob, cap];
}

export function pushFlush(sid: number): [number, number, number] {
  const cap = WRAP + SEG + NONCE + TAG + 64;
  const ob = alloc(cap);
  const w = (ex.sr_seal_push as (a: number, b: number, c: number, d: number, e: number) => number)(sid, 0, 0, ob, cap);
  if (w < 0) { unput(ob, cap); throw { error: "E_NET", msg: `本地加密收尾失败（${w}）` }; }
  return [w, ob, cap];
}

export function sealEnd(sid: number) {
  (ex.sr_seal_end as (s: number) => number)(sid);
}

/** seal 会话深接口（与 openSession 对称）：push/flush 返回已拷贝密文，调用方零 wasm 义务。 */
export function sealSession(pem: string, seedBytes: Uint8Array) {
  const sid = sealBegin(pem, seedBytes);
  return {
    /** 推入明文分片 → 密文段（可能 null：未跨段界）。 */
    push(bytes: Uint8Array): Uint8Array | null {
      const [w, ob, cap] = pushSlice(sid, bytes);
      const out = w > 0 ? new Uint8Array(mem().subarray(ob, ob + w)) : null;
      unput(ob, cap);
      return out;
    },
    /** 流结束尾段（可能 null）。 */
    flush(): Uint8Array | null {
      const [w, ob, cap] = pushFlush(sid);
      const out = w > 0 ? new Uint8Array(mem().subarray(ob, ob + w)) : null;
      unput(ob, cap);
      return out;
    },
    /** 会话始终收尾（异常路径也须调用——内部 sr_seal_end 幂等）。 */
    end() { sealEnd(sid); },
    /** 原始句柄（分片签名上传的零拷贝路径专用：postChunk 直读 wasm 内存）。 */
    raw: sid,
  };
}

/** 信封流式解封会话（§2）：分片任意边界，内部按段缓冲；每段输出立即拷贝脱离 wasm 内存。 */
export function openSession(privPem: string) {
  const [pp, pl] = put(enc.encode(privPem));
  const sid = (ex.sr_open_begin as (p: number, l: number) => number)(pp, pl);
  unput(pp, pl);
  if (sid <= 0) throw { error: "E_NET", msg: `解封会话创建失败（${sid}）：私钥 PEM 无效` };
  return {
    push(bytes: Uint8Array): Uint8Array | null {
      const [ip, il] = bytes.length ? put(bytes) : [0, 0];
      let cap = bytes.length + SEG + 64, ob = alloc(cap);
      let w = (ex.sr_open_push as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, ob, cap);
      if (w < 0 && w <= -40) { unput(ob, cap); cap = -w; ob = alloc(cap); w = (ex.sr_open_push as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, ob, cap); }
      const o = w > 0 ? new Uint8Array(mem().subarray(ob, ob + w)) : null;
      unput(ip, il);
      unput(ob, cap);
      if (w < 0) throw { error: "E_NET", msg: `解封失败（${w}）：数据损坏或私钥不匹配` };
      return o;
    },
    end() { (ex.sr_open_end as (s: number) => number)(sid); },
  };
}

/** 把任意 Blob 密封为完整信封字节（db-restore 上行用；不联网；异常也收尾防会话泄漏）。 */
export async function sealToEnvelope(pem: string, payload: Blob): Promise<Uint8Array> {
  const sess = sealSession(pem, crypto.getRandomValues(new Uint8Array(32)));
  const parts: Uint8Array[] = [];
  try {
    for (let o = 0; o < payload.size; o += SEG) {
      const bytes = new Uint8Array(await payload.slice(o, Math.min(o + SEG, payload.size)).arrayBuffer());
      const out = sess.push(bytes);
      if (out) parts.push(out);
    }
    const tail = sess.flush();
    if (tail) parts.push(tail);
  } finally {
    sess.end();
  }
  const envelope = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let off = 0;
  for (const p of parts) { envelope.set(p, off); off += p.length; }
  return envelope;
}

// —— zstd 全量压缩（两遍法第一遍）：逐 CHUNK 片独立完整帧；压缩确定性（同输入+同 level ⇒ 逐字节
//    相同）是断点续传重放的前提；每片输出立即拷贝脱离 wasm 内存（Blob 由磁盘句柄支撑）。 ——

const isZstdHead = (b: Uint8Array) => b && b.length >= 4 && b[0] === 0x28 && b[1] === 0xb5 && b[2] === 0x2f && b[3] === 0xfd;

function zPush(sid: number, bytes: Uint8Array): [number, number, number] {
  const [ip, il] = put(bytes);
  const cap = bytes.length + (bytes.length >> 3) + 4096;
  const ob = alloc(cap);
  const w = (ex.sr_zstd_push as (a: number, b: number, c: number, d: number, e: number) => number)(sid, ip, il, ob, cap);
  unput(ip, il);
  if (w < 0) { unput(ob, cap); throw { error: "E_NET", msg: `本地压缩失败（${w}）` }; }
  return [w, ob, cap];
}

/** 上传载荷统一入口：文件头已是 zstd 魔数 → 原样（避免二次压缩）；否则压缩后上传。 */
export async function uploadPayload(file: File, note?: (done: number, raw: number) => void): Promise<{ blob: Blob; zstd: boolean }> {
  const head = new Uint8Array(await file.slice(0, 4).arrayBuffer());
  if (isZstdHead(head)) return { blob: file, zstd: true };
  const sid = (ex.sr_zstd_begin as (l: number) => number)(ZSTD_LEVEL);
  if (sid <= 0) throw { error: "E_NET", msg: `压缩会话创建失败（${sid}）` };
  const parts: BlobPart[] = [];
  try {
    for (let o = 0; o < file.size; o += CHUNK) {
      const end = Math.min(o + CHUNK, file.size);
      const bytes = new Uint8Array(await file.slice(o, end).arrayBuffer());
      const [w, ob, cap] = zPush(sid, bytes);
      if (w > 0) parts.push(new Uint8Array(mem().subarray(ob, ob + w)));
      unput(ob, cap);
      if (note) note(end, file.size);
    }
  } finally {
    (ex.sr_zstd_end as (s: number) => number)(sid);
  }
  return { blob: new Blob(parts), zstd: false };
}


/** 一次性 RSA-3072 密钥对（备份下载用）：WebCrypto 优先；非安全上下文（局域网 HTTP）回落 wasm 垫片。 */
import { derToPem } from "@/lib/bytes";

export async function oneTimeRsaPemPair(): Promise<{ pub: string; priv: string }> {
  if (crypto.subtle) {
    try {
      const kp = await crypto.subtle.generateKey(
        { name: "RSA-OAEP", modulusLength: 3072, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" } as Algorithm & RsaHashedKeyGenParams,
        true, ["encrypt", "decrypt"],
      );
      const spki = new Uint8Array(await crypto.subtle.exportKey("spki", kp.publicKey));
      const pkcs8 = new Uint8Array(await crypto.subtle.exportKey("pkcs8", kp.privateKey));
      return { pub: derToPem(spki, "PUBLIC KEY"), priv: derToPem(pkcs8, "PRIVATE KEY") };
    } catch { /* 落到 wasm 垫片路径 */ }
  }
  const sid = (ex.sr_keygen_rsa as () => number)();
  if (sid <= 0) throw { error: "E_NET", msg: `下载准备失败（${sid}）` };
  return {
    pub: shimCStr((p, c) => (ex.sr_rsa_pub_pem as (a: number, b: number, c: number) => number)(sid, p, c), 1024),
    priv: shimCStr((p, c) => (ex.sr_rsa_priv_pem as (a: number, b: number, c: number) => number)(sid, p, c), 4096),
  };
}
