import { beforeEach, describe, expect, it } from "vitest";
import { checkProj, dataFresh, dataInvalidate, dataMark, pemReady, platformPem, hydratePem, state, ENV_DESC } from "@/lib/state";

describe("checkProj（§11 命名规则）", () => {
  it("合法名通过", () => {
    expect(checkProj("demo")).toBeNull();
    expect(checkProj("Demo-1")).toBeNull(); // 大写放开
    expect(checkProj("a".repeat(31))).toBeNull();
  });
  it("非法名拒绝", () => {
    expect(checkProj("my_app")).toMatch(/仅限字母/);
    expect(checkProj("-x")).toMatch(/仅限字母/);
    expect(checkProj("a".repeat(32))).toMatch(/仅限字母/);
  });
  it("保留名拒绝", () => {
    for (const n of ["v2", "_api", "_console", "_bootstrap"]) expect(checkProj(n)).toMatch(/保留名/);
  });
  it("空名给独立文案", () => {
    expect(checkProj("")).toMatch(/请先填写/);
  });
});

describe("TTL 缓存单源", () => {
  beforeEach(() => dataInvalidate("projects", "space", "images"));
  it("mark 后 fresh、invalidate 后失效", () => {
    expect(dataFresh("projects")).toBe(false);
    dataMark("projects");
    expect(dataFresh("projects")).toBe(true);
    dataInvalidate("projects");
    expect(dataFresh("projects")).toBe(false);
  });
});

describe("PEM 事实源", () => {
  beforeEach(() => { state.pem = ""; localStorage.removeItem("sr.pempub"); });
  it("未配置不 ready", () => {
    expect(pemReady()).toBe(false);
  });
  it("合法头 ready；hydrate 持久化", () => {
    hydratePem("-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----\n");
    expect(pemReady()).toBe(true);
    expect(platformPem().startsWith("-----BEGIN PUBLIC KEY-----")).toBe(true);
    expect(localStorage.getItem("sr.pempub")).toContain("BEGIN PUBLIC KEY");
  });
});

it("ENV_DESC 覆盖 §6 十二键（事实源）", () => {
  expect(Object.keys(ENV_DESC).length).toBeGreaterThanOrEqual(12);
  for (const k of ["DATABASE_URL", "DB_HOST", "DB_PORT", "DB_NAME", "DB_USER", "DB_PASSWORD", "REDIS_HOST", "REDIS_PORT", "REDIS_DB", "REDIS_URL", "SHOWROOM_PROJECT", "SHOWROOM_BASE_PATH"]) {
    expect(ENV_DESC[k], `缺 ${k}`).toBeTruthy();
  }
});
