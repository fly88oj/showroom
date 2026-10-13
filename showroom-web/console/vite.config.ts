import { resolve } from "node:path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

// https://vite.dev/config/
export default defineConfig({
  base: "/_console/", // 控制台恒挂子路径（Caddy /_console/* → 静态；勿改根路径）
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": resolve(import.meta.dirname, "./src"),
    },
  },
  // 分包策略：发布/数据/帮助/管理四页 React.lazy 按需加载（见 App.tsx）；
  // wasm 走 public/ 不进 bundle；本 rolldown-vite 版本 advancedChunks 类型未开放，暂不手分 vendor。
})
