# showroom-console

Showroom 控制台前端（React + shadcn/ui，Vite 构建；密码学层为 wasm32 垫片）。

```sh
npm install
npm run dev            # 本地开发（无后端时停在登录窗）
npm run build          # 产物 dist/（base=/_console/，随提交入库）
npx tsc --noEmit -p tsconfig.app.json   # 类型检查
```

线上验收：`SR_BASE=… SR_USER=… SR_PASS=… node e2e-live.mjs`（playwright 需另行安装，
路径可用 SR_PLAYWRIGHT_REQUIRE / SR_CHROME 覆盖）。
