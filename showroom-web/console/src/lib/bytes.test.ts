import { describe, expect, it } from "vitest";
import { Buffer_fromHex, toHex, bytesToB64, derToPem, randAlnum, nonceHex } from "@/lib/bytes";

describe("hex 往返", () => {
  it("fromHex→toHex 无损", () => {
    expect(toHex(Buffer_fromHex("00ff10a5"))).toBe("00ff10a5");
  });
  it("空串往返", () => {
    expect(toHex(Buffer_fromHex(""))).toBe("");
  });
  it("大写 hex 归一为小写", () => {
    expect(toHex(Buffer_fromHex("AB"))).toBe("ab");
  });
});

describe("base64/PEM", () => {
  it("bytesToB64 基本", () => {
    expect(bytesToB64(new Uint8Array([104, 105]))).toBe("aGk="); // "hi"
  });
  it("derToPem 包头尾并 64 列折行", () => {
    const der = new Uint8Array(100).fill(65); // b64(100B)=136 字符 → 64+64+8 三行
    const pem = derToPem(der, "PUBLIC KEY");
    expect(pem.startsWith("-----BEGIN PUBLIC KEY-----")).toBe(true);
    expect(pem.includes("\n-----END PUBLIC KEY-----")).toBe(true);
    const lines = pem.split("\n");
    const dataLines = lines.slice(1, lines.indexOf("-----END PUBLIC KEY-----"));
    expect(dataLines.map((l: string) => l.length)).toEqual([64, 64, 8]);
    expect(dataLines.join("")).toHaveLength(136);
  });
});

describe("随机串", () => {
  it("randAlnum 长度与字符集", () => {
    const s = randAlnum(64);
    expect(s).toHaveLength(64);
    expect(s).toMatch(/^[A-Za-z0-9]+$/);
  });
  it("两次生成不同（CSPRNG）", () => {
    expect(randAlnum(32)).not.toBe(randAlnum(32));
  });
  it("nonceHex 32 位 hex", () => {
    expect(nonceHex()).toMatch(/^[0-9a-f]{32}$/);
  });
});
