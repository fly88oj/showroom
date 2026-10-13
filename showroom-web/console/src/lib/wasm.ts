// —— wasm.ts：wasm 垫片装载（移植自 dist/js/api.mjs，协议路径零改动）——
// wasm 部署于 _console 根（index.html 同级）：探测路径不再需要 ../ 上移。
import { dec } from "@/lib/bytes";

interface WasmExports {
  memory: WebAssembly.Memory;
  sr_alloc(n: number): number;
  sr_free(p: number, l: number): number;
  sr_entropy(p: number, l: number): number;
  sr_generate(out: number): number;
  sr_pub_string(seed: number, out: number, cap: number): number;
  sr_sign_header(
    seed: number, user: number, ul: number, method: number, ml: number,
    path: number, pl: number, ts: bigint, nonce: number, nl: number,
    body: number, bl: number, out: number, cap: number,
  ): number;
  sr_login_start(pp: number, pl: number, up: number, ul: number): number;
  sr_login_ke1(sid: number, out: number, cap: number): number;
  sr_login_finish(sid: number, pp: number, pl: number, kp: number, kl: number, out: number, cap: number): number;
  sr_reg_start(pp: number, pl: number, up: number, ul: number): number;
  sr_reg_msg(sid: number, out: number, cap: number): number;
  sr_reg_finish(sid: number, pp: number, pl: number, mp: number, ml: number, out: number, cap: number): number;
  [k: string]: unknown; // 其余导出（seal/open/zstd 等）按需使用
}

export let ex: WasmExports = null as unknown as WasmExports;

async function loadWasm(): Promise<WebAssembly.Exports> {
  const paths = ["showroom_bg.wasm", "/_console/showroom_bg.wasm"];
  let lastErr: unknown = null;
  for (const p of paths) {
    try {
      return (await WebAssembly.instantiateStreaming(fetch(p), {})).instance.exports;
    } catch (e) {
      lastErr = e;
      try {
        return (await WebAssembly.instantiate(await (await fetch(p)).arrayBuffer(), {})).instance.exports;
      } catch (e2) {
        lastErr = e2;
      }
    }
  }
  throw lastErr;
}

/** 装载垫片并注入熵（app 启动调用一次；crypto.getRandomValues 供 wasm32 无 OS 熵源）。 */
export async function initWasm(): Promise<void> {
  ex = (await loadWasm()) as unknown as WasmExports;
  const b = new Uint8Array(64);
  crypto.getRandomValues(b);
  const p = ex.sr_alloc(64);
  mem().set(b, p);
  if (ex.sr_entropy(p, 64) !== 0) throw new Error("sr_entropy 失败");
  ex.sr_free(p, 64);
  // 垫片导出齐备断言（坏 wasm 早败，调用方无需降级分支）
  for (const fn of ["sr_login_start", "sr_login_ke1", "sr_login_finish", "sr_reg_start", "sr_reg_msg", "sr_reg_finish"]) {
    if (typeof ex[fn] !== "function") throw new Error(`垫片缺少导出 ${fn}`);
  }
}

// wasm 内存可能随分配增长而换代：任何 wasm 调用后都重取 view
export const mem = () => new Uint8Array(ex.memory.buffer);
export const alloc = (n: number) => {
  const p = ex.sr_alloc(n);
  if (!p) throw new Error(`sr_alloc(${n}) 失败`);
  return p;
};
export const put = (b: Uint8Array): [number, number] => {
  if (!b.length) return [0, 0]; // 空缓冲：空指针+0 长度（ABI 合法空切片）
  const p = alloc(b.length);
  mem().set(b, p);
  return [p, b.length];
};
export const unput = (p: number, l: number) => { ex.sr_free(p, l); };
export const readCStr = (p: number, written: number) => {
  const m = mem();
  let e = p;
  while (e < p + written && m[e] !== 0) e++;
  return dec.decode(m.subarray(p, e));
};

/** 垫片「写字符串」类调用的容量自适应封装：容量不足（返回 -(所需) ≤ -40）时扩容重试。 */
export function shimCStr(call: (p: number, cap: number) => number, cap0: number): string {
  let cap = cap0, p = alloc(cap), r = call(p, cap);
  while (r < 0 && r <= -40) {
    unput(p, cap);
    cap = -r;
    p = alloc(cap);
    r = call(p, cap);
  }
  const s = r > 0 ? readCStr(p, r) : null;
  unput(p, cap);
  if (!s) throw { error: "E_NET", msg: `垫片调用失败（${r}）` };
  return s;
}


