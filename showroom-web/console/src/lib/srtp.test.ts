// —— srtp.test.ts：HTTP/1.1 序列化/解析三形状（与 CLI srtp_transport 测试同构）——
import { describe, expect, it } from "vitest";
import { enc } from "@/lib/bytes";
import { httpSerialize, parseHttpResponse } from "@/lib/srtp";

function u(s: string): Uint8Array {
  return enc.encode(s);
}

describe("httpSerialize", () => {
  it("含请求行/Host/Connection/CL/自定义头与体", () => {
    const body = u("body");
    const req = httpSerialize("POST", "/_api/x?a=1", [["Authorization", "SR-SIG1 u"]], body);
    const text = new TextDecoder().decode(req);
    expect(text.startsWith("POST /_api/x?a=1 HTTP/1.1\r\n")).toBe(true);
    expect(text).toContain("Connection: close\r\n");
    expect(text).toContain("Content-Length: 4\r\n");
    expect(text).toContain("Authorization: SR-SIG1 u\r\n");
    expect(text.endsWith("\r\n\r\nbody")).toBe(true);
  });
});

describe("parseHttpResponse", () => {
  it("Content-Length 定长体", () => {
    const r = parseHttpResponse(u("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"));
    expect(r.status).toBe(200);
    expect(new TextDecoder().decode(r.body)).toBe("hello");
  });

  it("chunked 去块", () => {
    const r = parseHttpResponse(u("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2\r\nlo\r\n0\r\n\r\n"));
    expect(new TextDecoder().decode(r.body)).toBe("hello");
  });

  it("EOF 体（无 CL）与非 2xx 状态", () => {
    const r = parseHttpResponse(u('HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n{"error":"E_AUTH"}'));
    expect(r.status).toBe(404);
    expect(new TextDecoder().decode(r.body)).toContain("E_AUTH");
  });
});
