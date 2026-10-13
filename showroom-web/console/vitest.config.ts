import { resolve } from "node:path";
import { defineConfig } from "vitest/config";

// 纯函数层单测（lib/*）：不引 vite/react 插件，避免与主构建的 rolldown-vite 纠缠
export default defineConfig({
  resolve: {
    alias: { "@": resolve(import.meta.dirname, "./src") },
  },
  test: {
    environment: "happy-dom", // localStorage/window.location 需要
    include: ["src/**/*.test.ts"],
  },
});
