# 双版本发布流程（Electron 线 + Rust 线）

同一个仓库、同一份 `renderer/`，同时维护两条发布线：**Electron 线**（现有实现，继续修）
和 **Rust / Tauri 线**（新实现，从 2.0.0 起）。打包时一次产出两份安装包。

---

## 1. 两条线的约定（照这个表来，别临场发挥）

| | Electron 线 | Rust 线 |
|---|---|---|
| 版本号来源 | `package.json` | `src-tauri/tauri.conf.json` |
| 版本号区间 | **1.x**（继续修 bug） | **2.x** 起 |
| tag 命名 | `v1.1.7`（**无前缀**，沿用历史） | **`rust-v2.0.0`** |
| 安装包文件名 | `Pomodoro-Fluent-Setup-1.1.7.exe` | `Pomodoro-Fluent-Rust-Setup-2.0.0.exe` |
| 身份标识 | `com.pomodoro.fluent` / 产品名 `番茄钟` | **完全相同** |
| 数据目录 | `%APPDATA%\pomodoro-fluent` | **完全相同** |
| 版本号是否互相同步 | **否** —— 各自独立演进，脚本不做任何同步 | |

### ⚠️ tag 前缀是硬约定

两个版本的应用内「检查更新」都靠 **tag 前缀**区分版本线：

- Electron 版只认**不带** `rust-` 前缀的 tag；
- Rust 版只认**带** `rust-` 前缀的 tag。

**如果 Rust 线发了一个不带前缀的 tag**（比如直接打 `v2.0.0`），Electron 1.x 用户会收到
「有新版本 2.0.0」的提示，点进去下载到的是 Rust 版安装包 —— 装完才发现自己换了套实现。
反之亦然。所以：**Rust 线的 tag 一律写成 `rust-vX.Y.Z`。**

（为什么不用 `/releases/latest`：它的语义是「最近发布的非预发布 release」，两条线版本号
各自演进时它必然串线。`main.js` 已改成拉 `/releases?per_page=30` 再按前缀过滤。）

---

## 2. 打包

```bash
npm run pack:all      # 两份都打
npm run pack          # 只打 Electron 线（原有命令，语义没变）
npm run pack:rust     # 只打 Rust 线
```

产物分两条线收集，方便分别发 release：

```
release/
├─ electron/
│   ├─ Pomodoro-Fluent-Setup-1.1.7.exe
│   ├─ Pomodoro-Fluent-Setup-1.1.7.exe.blockmap
│   └─ latest.yml
└─ rust/
    └─ Pomodoro-Fluent-Rust-Setup-2.0.0.exe
```

> **Rust 线打包需要 GitHub 镜像。** 本机对 GitHub 的连接会被重置，而 tauri bundler 的
> NSIS 工具链只从 GitHub releases 取，不设镜像会直接报
> `Error failed to bundle project: timeout: global`。脚本默认用 `https://ghfast.top`，
> 要换镜像设环境变量 `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR` 即可。
> 工具链缓存在 `%LOCALAPPDATA%\tauri\NSIS\`，装坏了下错版本就删掉该目录重跑。

> **在 worktree 里跑要先 `npm install`。** worktree 只签出受版本控制的文件，
> `node_modules/`（gitignore 里）不会跟着过去，所以 `pack:all` 的 Electron 那一半会失败。
> Rust 那一半不依赖 node_modules，`npm run pack:rust` 可以直接在 worktree 里跑。

---

## 3. 发布（两条线各发一个 release）

```bash
# Electron 线
gh release create v1.1.7 --target <mergeCommit> --title v1.1.7 release/electron/*

# Rust 线（前缀不能省）
gh release create rust-v2.0.0 --target <mergeCommit> --title "Rust 版 2.0.0" release/rust/*
```

推代码仍然走 PR（`main` 上的 ruleset 禁直推），细节见发版 skill。

---

## 4. 两个版本在用户机器上的关系

按当前决策，两版**共用同一身份与同一份数据目录**：

- 安装第二个会顶掉第一个的快捷方式 / 安装项 ⇒ **别指望两个并排装**，
  切换版本时先卸载另一个。好处是**配置自动继承**，切换无痛。
- 两版写同一份 `config.json` ⇒
  **配置结构必须保持向后兼容：只加字段，不改已有字段的语义、不删字段。**
  否则新版本写过的配置会让旧版本读不懂（反之亦然）。
- `gateway.json` 同理：**同时只让一个版本在跑**，否则网关以最后启动的为准。
- hook 配置：两个版本的 `install --clean` 都会清掉指向另一版本的条目，
  ⇒ **同一台机器上只让一个版本管 hook**。

> 如果以后改主意要让两版**并排共存**，需要做的事：Rust 线换一个 `identifier`
> （如 `com.pomodoro.fluent.rust`）、换 productName / 快捷方式名、数据目录也要分开。
> 这三处一起改才有效，只改一个仍会互相覆盖。

---

## 5. 加了新宿主适配后要同步的地方（原「四处」现在是六处）

两条线并存后，宿主适配逻辑有了两份实现，漏同步的代价翻倍：

1. Electron 线 `bin/pomodoro-hook.js`
2. Rust 线 `crates/hook`（M3 才动）
3. `main.js` 里写出的 hook 配置
4. `renderer/app.js` 的 `buildHookSnippet`（**两版共用这一份**）
5. `docs/agent-hooks.md`
6. `README.md` 的支持表

`renderer/` 是两版共用的，改 UI 两边同时生效；但 **hook 的协议适配和写入的 JSON 是两套代码**，
是这套双版本方案里最容易出错的地方。

---

## 6. 当前状态（2026-09-20）

| | 状态 |
|---|---|
| Electron 线 | 完整可用，v1.1.6；本次只改了「检查更新」的版本线过滤 |
| Rust 线 | **M0 骨架**：空窗 + 托盘 + 能打出 1.04 MB 安装包；功能未迁移 |
| 打包脚本 | ✅ `scripts/pack-release.mjs` 已可用 |
| 发布文档 | ✅ 本文 |

Rust 线的功能对齐进度见 `docs/rust-migration-plan.md` 的 M0–M5 里程碑。
