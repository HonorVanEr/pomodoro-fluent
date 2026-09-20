// tauri-build 的构建脚本。这里是 `tauri.conf.json` 的解析入口 ——
// 所以本文件头的注释就是那份配置的"注释区"。
//
// ⚠ `tauri.conf.json` 是**严格 JSON，写不了注释**。
//   别在里面加 `//` 行，也别用 `"// 键名": "说明"` 这种变通写法 ——
//   `AppConfig` 带着 `deny_unknown_fields`，后者会被当成未知字段直接**构建失败**：
//       unknown field `// windows`, expected one of `windows`, `security`, ...
//   真想要注释得改成 `tauri.conf.json5`，但那要给 tauri-build / tauri / tauri-macros
//   同时开 `config-json5` 特性（默认没开），不值当。
//
// 那份配置里有四条是"改了就坏"的约定，记在这儿：
//
// 1. `build.frontendDist = "../renderer"` —— 与 Electron 版**共用同一份渲染层**，
//    一行不改。Tauri 打包时把这个目录整个塞进 exe。
//
// 2. `app.withGlobalTauri = true` —— `src/bridge.js` 依赖 `window.__TAURI__`
//    （`core.invoke` / `event.listen`）。关掉它 bridge 第一段就报错退出，
//    渲染层随即满屏 "undefined 不是函数"。
//
// 3. `app.windows = []` —— **刻意留空**。主窗口在 `src/main.rs` 的
//    `create_main_window()` 里用 `WebviewWindowBuilder` 建，因为只有那条路径能挂
//    `initialization_script()` 注入 bridge.js。**别把窗口挪回这里** ——
//    挪回来就没地方注入垫片了，而且会同时存在两个窗口。
//
// 4. `app.security.csp` 里的 `connect-src ipc: http://ipc.localhost` 是必须的，
//    否则渲染层的 `invoke` 会被 CSP 拦掉。其余保持与 Electron 版一致：
//    只允许自身资源（`default-src 'self'`），不引任何外部 CDN。
//
// `version` 由 `scripts/pack-release.mjs` 从 `package.json` 同步过来，别手工改
// （改了下次打包会被冲掉）。

fn main() {
    tauri_build::build()
}
