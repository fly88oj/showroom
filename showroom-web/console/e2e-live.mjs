#!/usr/bin/env node
// —— e2e-live.mjs：对真实服务跑控制台全页 e2e（登录限流 70s+，双账号间已留 78s）——
// 用法：SR_BASE=http://<host>/_console/ SR_USER=skilltest SR_PASS=… SR_ADMIN=qavis SR_ADMIN_PASS=… node e2e-live.mjs
// 账号由部署方提供（勿入库真实凭据）；截图输出到 /tmp/live/。
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
const require = createRequire("/home/jeffrey/works/personal/share-state/node_modules/"); // playwright 复用本机安装
const { chromium } = require("playwright");

const BASE = process.env.SR_BASE || "http://192.168.4.55/_console/";
const USER = process.env.SR_USER, PASS = process.env.SR_PASS;
const ADMIN = process.env.SR_ADMIN, ADMIN_PASS = process.env.SR_ADMIN_PASS;
if (!USER || !PASS) { console.error("需要 SR_USER/SR_PASS（可选 SR_ADMIN/SR_ADMIN_PASS）"); process.exit(2); }
mkdirSync("/tmp/live", { recursive: true });

const errors = [];
let n = 0;
const shot = async (page, name) => {
  await page.screenshot({ path: `/tmp/live/${String(++n).padStart(2, "0")}-${name}.png`, fullPage: true });
  console.log(`✓ ${n}-${name}`);
};
const ok = (c, w) => { if (!c) errors.push("[assert] " + w); };

const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", args: ["--no-sandbox"] });
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on("pageerror", (e) => errors.push("[pageerror] " + e.message.slice(0, 200)));
const nav = (t) => page.locator("nav").first().getByText(t, { exact: true });

async function login(u, p) {
  await page.waitForSelector("#lg-user", { timeout: 20000 });
  await page.locator("#lg-user").fill(u);
  await page.locator("#lg-pass").fill(p);
  await page.getByRole("button", { name: "登录" }).click();
  await page.waitForSelector("nav", { timeout: 20000 });
  await page.waitForTimeout(2500);
}
async function logout() {
  await page.getByRole("button", { name: new RegExp(page.__user) }).click();
  await page.getByRole("menuitem", { name: "退出登录" }).click();
  await page.waitForSelector("#lg-user", { timeout: 10000 });
}

await page.goto(BASE, { waitUntil: "networkidle" });
await page.waitForSelector("#lg-user", { timeout: 20000 });
await shot(page, "login");

page.__user = USER;
await login(USER, PASS);
ok(await page.getByText("项目列表").isVisible(), "项目列表");
await shot(page, "projects");
for (const t of ["发布", "镜像库", "数据", "帮助"]) {
  await nav(t).click(); await page.waitForTimeout(700); await shot(page, t);
}
await logout();
await shot(page, "logout");

if (ADMIN && ADMIN_PASS) {
  console.log("…等待 78s（登录限流窗口）");
  await page.waitForTimeout(78000);
  page.__user = ADMIN;
  await login(ADMIN, ADMIN_PASS);
  ok(await nav("管理").isVisible(), "管理员可见管理页");
  await nav("管理").click(); await page.waitForTimeout(1500);
  await shot(page, "admin-users");
  for (const sub of ["全部项目", "平台状态", "宿主配置", "实时事件"]) {
    await page.getByRole("tab", { name: sub }).click(); await page.waitForTimeout(1200);
    await shot(page, "admin-" + sub);
  }
  await logout();
}

console.log(errors.length ? `✗ ${errors.length} 问题：\n${errors.join("\n")}` : "✓ 全部通过");
await browser.close();
process.exit(errors.length ? 1 : 0);
