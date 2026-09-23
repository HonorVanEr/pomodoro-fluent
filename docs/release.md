# 发布流程（Rust / Tauri 版）

本仓库**只维护 Rust / Tauri 一套实现**。

> 旧的 Electron 实现已冻结在 **`electron-archive`** 分支（含完整源码与当时的
> `main.js` / `preload.js` / `gateway.js` / `bin/pomodoro-hook.js`）。
> 那个分支**不再维护**、不再发版，只在需要考古 Electron 行为时查阅。

---

## 1. 版本号：只有一个事实源

```
package.json 的 version  ──[ pack 脚本自动同步 ]──▶  src-tauri/tauri.conf.json 的 version
                                                  └▶  Cargo.toml [workspace.package] 的 version
```

```bash
# 升版本 = 只手改 package.json 一处
#   "version": "1.2.0"  →  "version": "1.2.1"
node scripts/pack-release.mjs     # 开头会打印「版本号同步: ... 1.2.0 → 1.2.1」
```

- **别手工改 `src-tauri/tauri.conf.json` 与根 `Cargo.toml` 的版本号** —— 会被脚本覆盖。
- **别在成员 crate 里写死 `version`**（`src-tauri/Cargo.toml`、`crates/*/Cargo.toml`）：
  一律 `version.workspace = true`。写死会盖掉 workspace 的值，而且脚本同步不到它
  （踩过：hook exe 的版本资源一直停在旧值）。打包脚本里有闸门会直接报错。
- 版本号提交沿用既有流程：`feat: ...` + `chore: bump version to X.Y.Z`。
  bump 提交会带**三个文件**（`package.json` 手改 + 另两个由脚本同步）。

---

## 2. 打包

```bash
npm run pack          # 等价 node scripts/pack-release.mjs
```

产物收进 `release/`：

```
release/
└─ Pomodoro-Fluent-Rust-Setup-X.Y.Z.exe
```

> **需要 GitHub 镜像。** 本机对 GitHub 的连接会被重置，而 tauri bundler 的 NSIS 工具链
> 只从 GitHub releases 取，不设镜像会直接报 `Error failed to bundle project: timeout: global`。
> 脚本默认用 `https://ghfast.top`；要换镜像设 `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR` 即可。
> 工具链缓存在 `%LOCALAPPDATA%\tauri\NSIS\`，装坏了下错版本就删掉该目录重跑
> （删的时候用 MSYS 路径 `/c/Users/...`，别用 `$LOCALAPPDATA`，会被 safe-delete 拦下）。

> **打包脚本会自动做三件事**，顺序不能乱：
> ① 同步版本号；② **先 `cargo build --release -p pomodoro-hook`**；③ `cargo tauri build`
> + 核对安装包内容 + 按版本号过滤产物。

---

## 3. 发布

```bash
gh release create vX.Y.Z --target <mergeCommit> --title vX.Y.Z release/*
```

- **tag 是 `vX.Y.Z`，不带任何前缀**（别加 `rust-`）：应用内「检查更新」用 GitHub 的
  `/releases/latest`，带前缀的 tag 会被它跳过。
- 推代码走 PR —— `main` 上有 ruleset 禁直推。

---

## 4. ⚠ 打包时必须记得的一件事：sidecar 靠 `bundle.resources` 进包

NSIS **只装 GUI 那一个 exe**。hook CLI（`pomodoro-hook.exe`）与 OpenCode 插件是
**sidecar**，靠 `src-tauri/tauri.conf.json` 的 `bundle.resources` 打进 `$INSTDIR`：

```json
"resources": {
  "../target/release/pomodoro-hook.exe": "pomodoro-hook.exe",
  "../bin/opencode/pomodoro-opencode.ts": "opencode/pomodoro-opencode.ts"
}
```

map 方向是 **`{源: 目标}`**。运行时**从 exe 同级目录读**的文件（hook exe、OpenCode 插件）
都必须在这里登记，否则**装完就没有** —— 而开发目录里它们天然同级，永远看不出问题。

由于 `cargo tauri build` **只构建 GUI 那一个 bin**，打包前必须先
`cargo build --release -p pomodoro-hook`（脚本已替你做了）。漏掉的后果不是「打包失败」，
而是**装上去的 hook exe 是上一版的** —— 更隐蔽。

**已经有自动闸门**：`pack-release.mjs` 在 `cargo tauri build` 之后会自动跑
`scripts/check-installer-contents.mjs`。它读 `bundle.resources`，逐条到 Tauri 生成的
`target/release/nsis/x64/installer.nsi` 里核对「是否真有一条 `File` 指令把它拷进 `$INSTDIR`」，
并打印源文件的大小 / mtime（顺带能看出是不是本次构建的产物）。少一项就**直接让打包失败**。

```bash
node scripts/check-installer-contents.mjs              # 单独跑
node scripts/check-installer-contents.mjs --nsi <别的 installer.nsi>
grep -n -E '^\s*(File|CreateDirectory)' target/release/nsis/x64/installer.nsi   # 手工核对
```

> **收集产物必须按版本号过滤。** `target/release/bundle/nsis/` 里会留着上一次打包的旧安装包，
> 全部照抄的话**旧包会顶掉新包，而且日志看着像成功**（实测踩过）。脚本已按 `version` 过滤并
> 打印被忽略的旧产物。

---

## 5. 改了 hook 适配要同步的地方（四处）

加一个新宿主、或改已有宿主的事件 / matcher / timeout，必须**同步改这四处**：

1. `crates/hook`（Rust 版 hook CLI，唯一的 CLI 实现）
2. `renderer/app.js` 的 `buildHookSnippet`（界面上给用户复制的配置片段）
3. `docs/agent-hooks.md`
4. `README.md` 的支持表

漏第 2 处的症状最隐蔽：自动安装是对的，用户手动复制粘贴的片段是错的。

---

## 6. 发布前自检

```bash
cargo test --workspace                 # 单元测试
node scripts/check-bridge-parity.mjs   # bridge.js ↔ commands 静态比对（渲染层契约）
node scripts/smoke-tauri.mjs           # GUI 启动握手冒烟
node scripts/smoke-gateway.mjs         # 网关自检（真起应用打自己的 HTTP 接口）
npm run pack                           # 打包（含安装包内容闸门）
```

> 任何**起 GUI 的脚本都必须隔离 `POMODORO_USER_DATA`** —— GUI 启动会写
> `<userData>/hook/` 与 `gateway.json`，不隔离会把真实 `%APPDATA%` 写脏。

---

## 7. 用户机器上的约定

- 身份与数据目录：`com.pomodoro.fluent` / 产品名 `番茄钟` /
  `%APPDATA%\pomodoro-fluent`（兜底 `%APPDATA%\番茄钟`）。
- **hook 只该让一个应用实例管**：`install --clean` 会清掉指向别的 hook 的条目。
- **配置结构必须向后兼容：只加字段，不改已有字段语义、不删字段。**
  装过旧版的用户带着旧 `config.json` 升级，读不懂就白屏。
- 老用户的配置里可能写着 `node ".../pomodoro-hook.js"`（Electron 时代的写法）。
  `crates/hook` 的 `is_pomodoro_entry` 用**前缀匹配 `pomodoro-hook`** 认它，
  换成 `pomodoro-hook.exe` 时不会留下孤儿条目 —— 改那段代码时别把前缀匹配收紧成 `.exe`。
- 应用**不主动联网**：唯一例外是「关于 → 检查更新」用户点了才向 GitHub 发一次请求。
