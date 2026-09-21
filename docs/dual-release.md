# 双版本发布流程（Electron 版 + Rust 版，统一版本号）

同一个仓库、同一份 `renderer/`，同时维护两套底层实现：**Electron 版**（现有实现，功能完整）
和 **Rust / Tauri 版**（新实现，功能仍在迁移）。打包时一次产出两份安装包。

**版本号是统一的** —— 一个 tag、一个 release，两份安装包挂在同一条 release 里。

---

## 1. 约定一览（照这个表来，别临场发挥）

| | Electron 版 | Rust / Tauri 版 |
|---|---|---|
| 版本号来源 | `package.json` 的 `version`（**唯一事实源**） | 打包时由脚本从 `package.json` 同步进 `src-tauri/tauri.conf.json` |
| 版本号 | **完全相同**（共用同一个号） | **完全相同** |
| tag | **共用一个：`vX.Y.Z`**（不带前缀） | 同左 |
| 安装包文件名 | `Pomodoro-Fluent-Setup-X.Y.Z.exe` | `Pomodoro-Fluent-Rust-Setup-X.Y.Z.exe` |
| 身份标识 | `com.pomodoro.fluent` / 产品名 `番茄钟` | **完全相同** |
| 数据目录 | `%APPDATA%\pomodoro-fluent` | **完全相同** |

### 为什么统一版本号（这版方案曾是两条独立版本线，已否掉）

两条独立版本线（Electron `v1.x` + Rust `rust-v2.x`）会逼出两个额外代价：

1. **应用内「检查更新」必须改成按 tag 前缀过滤**。`main.js` 原来用
   `/releases/latest` + 裸 semver 比较；两条线各自演进时，Rust 线一发 `v2.0.0`，
   Electron `1.x` 用户就会收到「有新版本 2.0.0」，点进去下到的是另一种实现的安装包。
   要修就得动 `main.js`，而且每次发版都得记住前缀约定。
2. **两个 release 各自更新，用户看不懂自己在用哪条线的哪个版本。**

统一版本号之后这两条自然消失：`/releases/latest` 语义重新正确，
**`main.js` 的检查更新逻辑一行都不用改**（与 `main` 分支完全一致）。

代价是**两版必须一起升版本号**。所以约定：

> **Rust 版功能没对齐之前，不要为了「标记 Rust 是个大版本」而把号跳到 2.0.0。**
> 版本号是整个仓库的，不是某一条实现线的。真正功能对齐、值得换大版本号时，
> 改 `package.json` 一处，两份安装包一起变 2.0.0。

---

## 2. 版本号怎么升（单一事实源）

```
package.json 的 version  ──[ pack 脚本自动同步 ]──▶  src-tauri/tauri.conf.json 的 version
```

```bash
# 升版本 = 只手改 package.json 一处
#   "version": "1.1.6"  →  "version": "1.1.7"
# 然后跑一次打包（或者随便一次 pack），脚本会把号带到 tauri.conf.json：
node scripts/pack-release.mjs --only rust      # 会在开头打印「版本号同步: ... 1.1.6 → 1.1.7」
```

- **别手工去改 `src-tauri/tauri.conf.json` 的版本号** —— 它由脚本覆盖，手改会被冲掉。
- 版本号提交沿用既有流程：`feat: ...` + `chore: bump version to X.Y.Z`。
  与单版本时代唯一的区别是 **bump 提交现在会带两个文件**（`package.json` 手改 +
  `tauri.conf.json` 由脚本同步），别再按「bump 只改 package.json」的老习惯检查。

---

## 3. 打包

```bash
npm run pack:all      # 两份都打，产物统一收进 release/
npm run pack          # 只打 Electron 版（原有命令，语义没变）
npm run pack:rust     # 只打 Rust 版
```

产物（**不分两条线，就在一个目录里**，因为要挂到同一条 release）：

```
release/
├─ Pomodoro-Fluent-Setup-1.1.7.exe            ← Electron 版
├─ Pomodoro-Fluent-Setup-1.1.7.exe.blockmap
├─ latest.yml
└─ Pomodoro-Fluent-Rust-Setup-1.1.7.exe       ← Rust / Tauri 版
```

> **Rust 侧打包需要 GitHub 镜像。** 本机对 GitHub 的连接会被重置，而 tauri bundler 的
> NSIS 工具链只从 GitHub releases 取，不设镜像会直接报
> `Error failed to bundle project: timeout: global`。脚本默认用 `https://ghfast.top`，
> 要换镜像设环境变量 `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR` 即可。
> 工具链缓存在 `%LOCALAPPDATA%\tauri\NSIS\`，装坏了下错版本就删掉该目录重跑
> （删的时候用 MSYS 路径 `/c/Users/...`，别用 `$LOCALAPPDATA`，会被 safe-delete 拦下）。

> **在 worktree 里跑要先 `npm install`。** worktree 只签出受版本控制的文件，
> `node_modules/`（gitignore 里）不会跟着过去，所以 `pack:all` 的 Electron 那一半会失败。
> Rust 那一半不依赖 node_modules，`npm run pack:rust` 可以直接在 worktree 里跑。

---

## 4. 发布（一条命令）

```bash
gh release create v1.1.7 --target <mergeCommit> --title v1.1.7 release/*
```

**一份 release 里有两个安装包，所以 release notes 必须写清哪个是哪个** ——
用户是照着文件名点下载的，点错了会装上一个功能还没迁移完的应用：

```markdown
- `Pomodoro-Fluent-Setup-1.1.7.exe` —— **Electron 版**（现行实现，功能完整）
- `Pomodoro-Fluent-Rust-Setup-1.1.7.exe` —— **Rust 版**（安装包体积 ~1 MB，功能仍在迁移中）
```

推代码仍然走 PR（`main` 上有 ruleset 禁直推），细节见发版 skill。

> **Rust 版不要一上来就随 every release 对外发。** M0/M1 阶段它是个占位应用，
> 对外发等于把半成品丢给用户。日常发版只发 Electron 时，把
> `Pomodoro-Fluent-Rust-Setup-*.exe` 从 `release/` 里删掉再 `gh release create` 即可
> （脚本在 `pack:all` 之后会提示这一点）。

---

## 5. 两个版本在用户机器上的关系

按当前决策，两版**共用同一身份与同一份数据目录**：

- 安装第二个会顶掉第一个的快捷方式 / 安装项 ⇒ **别指望两个并排装**，
  切换版本时先卸载另一个。好处是**配置自动继承**，切换无痛。
- 两版写同一份 `config.json` ⇒
  **配置结构必须保持向后兼容：只加字段，不改已有字段的语义、不删字段。**
  否则新版本写过的配置会让旧版本读不懂（反之亦然）。
- `gateway.json` 同理：**同时只让一个版本在跑**，否则网关以最后启动的为准。
- hook 配置：两个版本的 `install --clean` 都会清掉指向另一版本的条目，
  ⇒ **同一台机器上只让一个版本管 hook**。

> 如果以后改主意要让两版**并排共存**，需要做的事：Rust 版换一个 `identifier`
> （如 `com.pomodoro.fluent.rust`）、换 productName / 快捷方式名、数据目录也要分开。
> 这三处一起改才有效，只改一个仍会互相覆盖。

---

## 6. 加了新宿主适配后要同步的地方（原「四处」现在是六处）

两套实现并存后，宿主适配逻辑有了两份，漏同步的代价翻倍：

1. Electron 版 `bin/pomodoro-hook.js`
2. Rust 版 `crates/hook`（**M3 起已就位**）
3. `main.js` 里写出的 hook 配置
4. `renderer/app.js` 的 `buildHookSnippet`（**两版共用这一份**）
5. `docs/agent-hooks.md`
6. `README.md` 的支持表

`renderer/` 是两版共用的，改 UI 两边同时生效；但 **hook 的协议适配和写入的 JSON 是两套代码**，
是这套双版本方案里最容易出错的地方。

改完别只跑一边的测试 —— 跑 **`bash scripts/check-hook-parity.sh`**（M3 新增，M4 扩到 77 项）：
它真起 Rust 版 GUI，把同一批输入同时喂给 `bin/pomodoro-hook.js` 与 `pomodoro-hook.exe`，
逐字比对 stdout（只归一化「本来就该不同」的 hook 路径与 `since` 时间格式）。
宿主适配漏同步、事件名/matcher/timeout 写歪，都会在这里现形。
M4 补上了**决策输出**那一半：§3b 用「预置缓存作答」验 `ask` 的已作答路径，
§3c 用假网关（`scripts/fake-gateway.mjs`）验 permission / cursor / codex / opencode 的已作答路径
—— 之前这两块只在「无人作答」的退化路径上比过。

---

## 7. 当前状态（2026-09-21）

| | 状态 |
|---|---|
| Electron 版 | 完整可用，**v1.2.0**；双版本改造中**功能未改**（检查更新逻辑与 `main` 完全一致）。唯一例外：`renderer/app.js` 的 hook 命令拼装新增了对 `.exe` 的分支（对 Electron **零行为变化**） |
| Rust 版 | **M5 完成（已发布）**：主窗 UI + 计时 + 迷你/贴边 + 托盘 + 网关 + 弹窗 + held 状态机 + hook CLI 与 `install` 子命令全部 Rust 化；交互链路按覆盖审计补齐（自检 21 项 + 双版本差分 77 项）；**安装包已随 `v1.2.0` 对外发布** |
| Node 依赖 | Electron 版需要；**Rust 版已完全不依赖**（hook 是原生 exe，打包也不发 Node） |
| 迁移进度 | **M0–M5 全部完成**。`scripts/smoke-interaction.js` **保留在 Electron 仓储** —— M4 实测后决定不照字面重写，改为覆盖审计 + 补缺口（见 `docs/rust-migration-plan.md` 第 13 节） |
| 版本号 | 统一：`package.json` / `src-tauri/tauri.conf.json` / `Cargo.toml [workspace.package]` / `Cargo.lock` 都是 `1.2.0` |
| 打包脚本 | ✅ `scripts/pack-release.mjs`（`pack:all` / `pack:rust`）—— 会**先构建 hook exe** 再 `cargo tauri build`，末尾**自动核对安装包内容**（`scripts/check-installer-contents.mjs`，见下「打包时必须记得的一件事」） |
| 安装包体积 | Electron **94,168,782 B（89.8 MB）** ／ Rust **1,363,113 B（1.30 MB）** |
| 已发布的 release | [`v1.2.0`](https://github.com/HonorVanEr/pomodoro-fluent/releases/tag/v1.2.0) —— 一条 release 挂两份安装包（tag 指向 `c04f3fb`） |
| 发布文档 | ✅ 本文 + `docs/rust-migration-plan.md` §9–14（各阶段实测与踩坑） |

### ⚠ 打包时必须记得的一件事

NSIS **只装 GUI 那一个 exe**。hook CLI（`pomodoro-hook.exe`）与 OpenCode 插件是
**sidecar**，靠 `src-tauri/tauri.conf.json` 的 `bundle.resources` 打进 `$INSTDIR`：

```json
"resources": {
  "../target/release/pomodoro-hook.exe": "pomodoro-hook.exe",
  "../bin/opencode/pomodoro-opencode.ts": "opencode/pomodoro-opencode.ts"
}
```

由于 `cargo tauri build` **只构建 GUI 那一个 bin**，打 Rust 包前必须先
`cargo build --release -p pomodoro-hook`（`pack-release.mjs` 已经替你做了）。
漏掉的后果不是「打包失败」而是**装上去的 hook exe 是上一版的** —— 更隐蔽，详见
`docs/rust-migration-plan.md` §14。

**已经有自动闸门**：`pack-release.mjs` 在 `cargo tauri build` 之后会跑
`scripts/check-installer-contents.mjs`。它读 `bundle.resources`，逐条到 Tauri 生成的
`target/release/nsis/x64/installer.nsi` 里核对「是否真有一条 `File` 指令把它拷进 `$INSTDIR`」，
并打印源文件的大小/mtime（顺带能看出是不是本次构建的产物）。少一项就**直接让打包失败**，
不需要人去记。单独跑、或对别的 `.nsi` 排查：

```bash
node scripts/check-installer-contents.mjs
node scripts/check-installer-contents.mjs --nsi <别的 installer.nsi>
```

要手工核对时：

```bash
grep -n -E '^\s*(File|CreateDirectory)' target/release/nsis/x64/installer.nsi
```

Rust 版的功能对齐进度见 `docs/rust-migration-plan.md` 的 M0–M5 里程碑，
第 9–12 节是各阶段的实测结果与踩坑记录。
