import { afterEach, describe, expect, it, vi } from "vitest";
import { checkResp, respErr } from "@/lib/api";

function jsonResponse(status: number, body: unknown) {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}
function textResponse(status: number, body: string) {
  return new Response(body, { status });
}

describe("checkResp（统一 JSON/错误解析）", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("2xx JSON 返回对象", async () => {
    const j = await checkResp(jsonResponse(200, { ok: true, projects: [] }));
    expect(j).toEqual({ ok: true, projects: [] });
  });
  it("2xx 非 JSON 返回空对象", async () => {
    const j = await checkResp(textResponse(200, "plain"));
    expect(j).toEqual({});
  });
  it("错误响应带 error/msg 原样抛出", async () => {
    await expect(checkResp(jsonResponse(409, { error: "E_CONFIRM", msg: "需确认" })))
      .rejects.toEqual({ error: "E_CONFIRM", msg: "需确认" });
  });
  it("错误非 JSON 回落 HTTP 状态码", async () => {
    await expect(checkResp(textResponse(502, "<html>bad gateway</html>")))
      .rejects.toEqual({ error: "E_NET", msg: "HTTP 502" });
  });
});

describe("respErr（octet-stream 等非 JSON 响应）", () => {
  it("解析错误对象", async () => {
    const e = await respErr(jsonResponse(413, { error: "E_QUOTA", msg: "磁盘空间不足" }));
    expect(e).toEqual({ error: "E_QUOTA", msg: "磁盘空间不足" });
  });
  it("非 JSON 回落状态码", async () => {
    const e = await respErr(textResponse(500, "oops"));
    expect(e).toEqual({ error: "E_NET", msg: "HTTP 500" });
  });
});
