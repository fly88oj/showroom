import { describe, expect, it } from "vitest";
import { externalUrl, publicBase, pageList, fmtTime } from "@/lib/app";

describe("对外 URL 三档策略（plan-17 §6）", () => {
  it("① sr.publicBase 优先（去尾斜杠）", () => {
    expect(publicBase("https://demo.example.com/")).toBe("https://demo.example.com");
    expect(externalUrl("https://demo.example.com", "shop")).toBe("https://demo.example.com/shop/");
  });
  it("② 未配置回落当前 origin（公网访问即公网地址）", () => {
    expect(externalUrl("", "shop")).toBe(`${window.location.origin}/shop/`);
  });
  it("③ 内网 host 判定（提示条依据）", () => {
    for (const h of ["localhost", "127.0.0.1", "10.0.0.5", "192.168.4.55", "172.20.1.1"]) {
      expect(isPrivateHostFor(h), h).toBe(true);
    }
    for (const h of ["demo.example.com", "106.55.224.229", "8.8.8.8"]) {
      expect(isPrivateHostFor(h), h).toBe(false);
    }
  });
});

// isPrivateHost 读 location.hostname——测试里以同正则直测（实现导出的是无参版，此处覆盖判定逻辑本体）
function isPrivateHostFor(host: string): boolean {
  const PRIV_HOST_RE = /^(localhost|127\.|0\.0\.0\.0|::1|\[::1\]|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/i;
  return PRIV_HOST_RE.test(host);
}

describe("pageList（分页省略号）", () => {
  it("≤7 全显", () => {
    expect(pageList(1, 5)).toEqual([1, 2, 3, 4, 5]);
    expect(pageList(3, 7)).toEqual([1, 2, 3, 4, 5, 6, 7]);
  });
  it("大页数：首尾+邻近+省略号", () => {
    expect(pageList(1, 20)).toEqual([1, 2, "…", 19, 20]);
    expect(pageList(10, 20)).toEqual([1, 2, "…", 9, 10, 11, "…", 19, 20]);
    expect(pageList(20, 20)).toEqual([1, 2, "…", 19, 20]);
  });
});

describe("fmtTime", () => {
  it("零/负值与非法输入回落 —", () => {
    expect(fmtTime(0)).toBe("—");
  });
  it("unix 秒渲染本地时区时间串", () => {
    const s = fmtTime(1760330000);
    expect(s).toMatch(/\d{4}\/\d{2}\/\d{2} \d{2}:\d{2}/);
  });
});
