import { describe, expect, it } from "vitest";
import { flushLen, sealedTotal } from "@/lib/envelope";

// 信封总长不变量（SPEC §2）：WRAP + 明文 + ceil(明文/SEG)×(NONCE+TAG)
const SEG = 4 * 1024 * 1024, WRAP = 384, NONCE = 12, TAG = 16;

describe("信封长度数学（与 wasm 无关的纯不变量）", () => {
  it("sealedTotal 基本与边界", () => {
    expect(sealedTotal(0)).toBe(WRAP);
    expect(sealedTotal(1)).toBe(WRAP + 1 + NONCE + TAG);
    expect(sealedTotal(SEG)).toBe(WRAP + SEG + NONCE + TAG);
    expect(sealedTotal(SEG + 1)).toBe(WRAP + SEG + 1 + 2 * (NONCE + TAG));
    expect(sealedTotal(7 * SEG + 123)).toBe(WRAP + 7 * SEG + 123 + 8 * (NONCE + TAG));
  });
  it("flushLen 与 sealedTotal 自洽（尾段=末段明文+tag；整除无尾段）", () => {
    expect(flushLen(SEG)).toBe(0);
    expect(flushLen(2 * SEG)).toBe(0);
    expect(flushLen(SEG + 5)).toBe(5 + NONCE + TAG);
    expect(flushLen(123)).toBe(123 + NONCE + TAG);
  });
});
