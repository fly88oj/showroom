// —— bytes.ts：编码/十六进制/Base64 工具（移植自 dist/js/util.mjs，逻辑零改动）——
export const enc = new TextEncoder();
export const dec = new TextDecoder();

export function Buffer_fromHex(hex: string): Uint8Array {
  const b = new Uint8Array(hex.length / 2);
  for (let i = 0; i < b.length; i++) b[i] = parseInt(hex.substr(i * 2, 2), 16);
  return b;
}

export function toHex(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

export function nonceHex(): string {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  return toHex(b);
}

/** 随机字母数字串：crypto.getRandomValues 拒绝采样（248=62×4 截断去模偏）。 */
export function randAlnum(len: number): string {
  const A = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
  let s = "";
  while (s.length < len) {
    const b = new Uint8Array(32);
    crypto.getRandomValues(b);
    for (const v of b) {
      if (v < 248 && s.length < len) s += A[v % 62];
    }
  }
  return s;
}

export function bytesToB64(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

/** DER → PEM（WebCrypto 只出 DER，需自行包 PEM 头尾）。 */
export function derToPem(der: Uint8Array, label: string): string {
  const b64 = bytesToB64(der).replace(/(.{64})/g, "$1\n").trim();
  return `-----BEGIN ${label}-----\n${b64}\n-----END ${label}-----\n`;
}
