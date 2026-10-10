# 调研：WASM 直接操作 DOM/自绘渲染——能否让控制台内部信息对外不可见

> 结论先行，每条附来源。基线：现状 dist/index.html 121KB（结构+端点+流程全内联可读）+ showroom_bg.wasm 961KB（gz 287KB，仅密码学）。

## 一、总结论（TL;DR）

1. **DOM 框架化（Yew/Leptos/Dioxus）只能隐藏"逻辑"，隐藏不了"渲染结果"**：三者最终都把组件物化为真实可读 DOM（浏览器渲染必需），view-source 只剩空挂载点，但 DevTools 实时 DOM 全可读，爬虫执行 JS 后照样拿到渲染后 HTML——[Yew 官方（VDOM diff→DOM patch）](https://yew.rs)、[Leptos Book（细粒度信号直接 set_attribute 真实节点）](https://book.leptos.dev)、[三框架对比](https://reintech.io/blog/leptos-vs-yew-vs-dioxus-rust-frontend-framework)、[Google 官方：索引用渲染后 HTML](https://developers.google.com/search/docs/crawling-indexing/javascript/javascript-seo-basics)。
2. **Canvas 全自绘（egui/Makepad/Flutter-CanvasKit）能把 DOM 藏成单个 `<canvas>`，但代价是可访问性灾难 + 1–3MB 体积 + 自担文本/IME/调试**：且旗舰实践 Flutter 为了可访问性又会把语义树**重建回 DOM**（隐藏的 flt-semantics 节点）——信息绕一圈回到 DOM——[eframe：web 无 AccessKit 后端](https://github.com/emilk/egui/blob/master/crates/eframe/CHANGELOG.md)、[Makepad 无障碍公开 issue](https://github.com/makepad/makepad/issues/196)、[Flutter semantics DOM 讨论](https://news.ycombinator.com/item?id=23924292)。
3. **"藏进 wasm"≠保密，只是混淆**：wasm2wat 一条命令把 wasm 转成可 grep 的文本，字符串/常量原样可见——[Yahoo：WASM 从不为藏密而设计](https://www.yahooinc.com/theforum/wasm-never-built-for-secrets-extracting-c2/)、[wabt 官方工具](https://webassembly.github.io/wabt/demo/)、[GitHub 讨论](https://github.com/WebAssembly/wabt/issues/1293)。
4. **Showroom 真正该防的都不是渲染层问题**：SPEC §4 零信息暴露（未认证一律 404 E_AUTH，不泄露路径存在性）已使「JS 里的端点路径」失去情报价值；登录后数据本就该给登录者看，机密靠 SR-SIG1 签名 + RSA 信封 E2E（docs/security.md）。**建议维持现状架构**，唯一值得做的是 SPEC 已定的「登录成功前不加载管理资源」式代码分片，与换渲染技术无关。

## 二、Rust→WASM 框架如何生成 DOM（调研点 1）

| 框架 | DOM 生成方式 | view-source 看到 | DevTools/爬虫看到 |
|---|---|---|---|
| Yew | 虚拟 DOM 双树 diff → 批量 DOM patch（React 模型） | 空挂载点 + 壳 | 完整可读实时 DOM |
| Dioxus | fiber 式优化 VDOM，组件粒度 diff | 同上 | 同上 |
| Leptos | 无 VDOM：信号直接 `set_attribute/set_property` 真实节点；SSR+hydration | CSR 同上；SSR 则首屏 HTML 已含结构 | 同上 |
| Sycamore | 细粒度响应、无 VDOM、web-sys 直建 DOM | 同上 | 同上 |

- 机制来源：[yew.rs](https://yew.rs)、[Leptos Book/leptos_reactive](https://book.leptos.dev)、[reintech 对比文](https://reintech.io/blog/leptos-vs-yew-vs-dioxus-rust-frontend-framework)、[js-framework-benchmark（含 Leptos/Dioxus/Sycamore/Yew）](https://krausest.github.io/js-framework-benchmark/)。
- 体积税（gz，最小应用）：Leptos ~25KB（hydration ~35KB）为最小，Yew/Dioxus 更大——[reintech](https://reintech.io/blog/leptos-vs-yew-vs-dioxus-rust-frontend-framework)、[rustify.rs 2026 对比](https://rustify.rs/leptos-vs-dioxus/)。
- **关键不变式**：只要走 DOM 渲染，最终树必须展开成浏览器可渲染的 DOM——框架改变的只是「谁在什么时刻创建节点」，不是「节点是否可读」。CSR 只是让 view-source（原始 HTML）变干净，渲染后 DOM 对任何登录者/自动化工具完全开放。

## 三、Canvas/WebGPU 全自绘（调研点 2）

- **egui/eframe wasm**：全部绘制进单个 `<canvas>`，widget 无 DOM 存在。桌面端有 AccessKit，**web 端 AccessKit 无后端**，只有实验性 `web_screen_reader`（Web Speech API 朗读，非语义树）——[eframe CHANGELOG](https://github.com/emilk/egui/blob/master/crates/eframe/CHANGELOG.md)、[eframe docs.rs](https://docs.rs/crate/eframe/latest)、[AccessKit](https://github.com/AccessKit/accesskit)。
- **iced**：web 双路线——iced_wgpu（WebGPU→canvas）与 iced_web（走 DOM）；wasm canvas 路线仍有坑（如 canvas 文本 wasm 下静默不渲染 #3199）——[iced_wgpu docs](https://docs.iced.rs/iced_wgpu/)、[issue #3199](https://github.com/iced-rs/iced/issues/3199)。
- **Makepad**：wasm/WebGL 单 canvas，WebGPU 演进中；无障碍是官方承认的开放问题（自述"DOM 框架的优势"）、GPU 运行时增大包体——[makepad.rs](https://makepad.rs)、[issue #196](https://github.com/makepad/makepad/issues/196)。
- **Flutter web（CanvasKit）**：DOM≈单 canvas + 巨型 wasm/js（~1.5–2MB 级）；检测到辅助技术时生成**隐藏 flt-semantics DOM 树**镜像 widget 树供读屏器——为可访问性把结构重新暴露进 DOM——[HN: Flutter Web: A Fractal of Bad Design](https://news.ycombinator.com/item?id=23924292)。
- 通用代价：无文本选择/IME 键盘/浏览器查找、SEO 归零、DevTools 无法审查、测试自动化退化为截图比对（同 HN 讨论及 [devm.io](https://devm.io)）。

## 四、Showroom 的真实敏感面（调研点 3）

- **登录前**：纯静态壳、零数据（SPEC §42：公网面仅暴露最小登录壳）。现状 index.html 里可读的是：`/_api/*` 端点清单、流程逻辑、命名规则、错误码语义——但服务端对未认证请求**无论路径是否存在一律 404 E_AUTH**（SPEC §4/§26，docs/security.md §5 枚举防护），拿到路径清单也换不来任何服务端信息。属"用法说明书"级泄露，非凭证。
- **登录后**：数据本来就该给登录者看；任何渲染技术（DOM 或 canvas）都要把明文送进该用户的浏览器。真正防线已在：SR-SIG1 全端点签名 + dump 走一次性 RSA 信封 E2E（wasm_shim.rs 即此职责）。
- **若仍想降低登录前面情报量**：唯一低成本有效项是**登录成功后再加载含端点/管理流程的 JS**（SPEC §42 已规划，代码分片即可）；把 JS 逻辑编进 wasm 只是抬高阅读门槛（wasm2wat 可逆，见结论 3），不改变威胁模型。

## 五、三档对比矩阵与建议（调研点 4）

| 维度 | 现状：单 HTML+JS + wasm 密码学 | 框架化（Leptos/Yew/Dioxus） | Canvas 自绘（egui/Makepad/Flutter） |
|---|---|---|---|
| 信息可见性 | view-source 全可读（结构+端点+流程）；wasm 仅藏密码学 | view-source 只剩挂载点；DevTools/爬虫仍全可读；逻辑进 wasm 但可逆向 | DOM 只剩 `<canvas>`；但 wasm 可逆向，a11y 桥（Flutter）会把语义树写回 DOM |
| 体积（gz） | 121KB html + 287KB wasm | +~25–150KB 框架税 | ~1–3MB |
| 可访问性 | 原生 DOM，最好 | 原生 DOM，好 | 灾难（web 无 AccessKit 后端/issue #196/靠隐藏 DOM 补丁） |
| 工程成本 | 已存在，零增量 | UI 全重写 + trunk/wasm-pack 链 + 现 C-ABI shim 需适配 | 全重写 + 自担文本/IME/调试/测试 |

**建议**：① 维持现状；② 唯一立项项 = 登录后加载管理 JS 的代码分片（安全收益真实、成本小）；③ 框架化仅当出于工程可维护性考量才评估，与保密无关；④ canvas 自绘明确否决。
