import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      globals: globals.browser,
    },
    rules: {
      'react-refresh/only-export-components': 'off',
      // _ 前缀 = 刻意未用的参数（回调 API 形状对齐调用方，如 publishDone(_name)）
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_' }],
      // React Compiler 系 lint（react-hooks v7 起随 recommended 默认 error）：本控制台为
      // 无框架 fetch-on-mount 架构——列表加载的 setStates 恒在 await 续体（非同步级联），
      // 弹窗/表单重置为刻意的同步模式；真问题以代码修复（purity/immutability），此规则关闭。
      'react-hooks/set-state-in-effect': 'off',
    },
  },
])
