# Rust / Tauri 迁移方案（草案）

> 目标：把 `pomodoro-fluent` 的运行时分发体积从 **90 MB 安装包** 降到 **个位数 MB**，同时
> 保住全部既有行为约定（见第 4 节）。
> 本文是决策文档，第 3 节列出需要拍板的选项；确认后再进入实施。

---

## 1. 现状体检（实测，2026-09-20）

| 指标 | 实测值 |
|---|---|
| NSIS 安装包 | **90 MB**（`dist/Pomodoro-Fluent-Setup-1.1.6.exe`） |
| 解包后目录 | **289 MB**（`dist/win-unpacked`） |
| 其中 `番茄钟.exe`（Electron 主程序） | **234 MB** |
| `LICENSES.chromium.html` | 19.5 MB |
| `resources.pak` + `icudtl.dat` + `*.pak` + DLL | ~32 MB |
| **`resources/app.asar`（我们自己的全部代码）** | **316 KB** |
| `node_modules`（仅构建期，不随包发布） | 463 MB |
| 仓库受版本控制文件 | 33 个，4.5 MB |

**结论：99.9% 的体积是 Chromium 运行时，业务代码只占 0.1%。**
所以问题不是「代码写胖了」，而是「选错了运行时」。任何在 Electron 上做的瘦身（裁 locale、去 pak）
天花板都在 70 MB 以上——Electron 主程序本体无法拆。

### 迁移后体积预期

| | 现在（Electron v1.1.6） | Tauri v2（**M0 已实测**） |
|---|---|---|
| 主程序 exe | 234 MB | **2.66 MB** |
| hook CLI | 76 KB Node 脚本（**依赖宿主机装 Node**） | **107 KB 静态 exe（零依赖）** |
| NSIS 安装包 | 89 MB | **1.04 MB** |
| 宿主进程常驻内存 | — | 25 MB（WebView2 子进程另计） |

⇒ 安装包 **-98.9%**；两个 exe 合计 2.77 MB —— 比 Electron 版白带的
`LICENSES.chromium.html`（19.5 MB）还小一个数量级。

> 诚实说明：**内存和启动速度的收益远小于体积收益**。WebView2 同样是 Chromium，只是从「每个
> 应用自带一份」变成「系统共用一份」。真正的大头收益是磁盘与分发。

---

## 2. 推荐架构

**Tauri v2**（当前稳定版：`tauri` crate `2.11.5` / `tauri-cli` `2.11.4`，2026-07 发布）。

**单 Cargo workspace + 两个 bin，共享一个 core crate：**

```
pomodoro-fluent-rust/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ core/                   # 共享逻辑：配置、网关、协议适配、hook 安装
│  └─ gui/                    # 主程序（windows_subsystem = "windows"）
│     └─ src/main.rs          # → 替代 main.js
├─ src/bin/pomodoro-hook.rs   # hook CLI（**console subsystem**，必须能写 stdout JSON）
├─ renderer/                  # 原样保留的 HTML/CSS/JS
├─ src-tauri/tauri.conf.json  # 替代 package.json 的 build 段
└─ docs/
```

### 为什么必须是两个 exe

`windows_subsystem = "windows"` 的 GUI 程序被宿主以管道方式拉起时，stdout 句柄通常是可用的，
但**不是所有宿主/终端组合都保证**（部分场景要靠 console 附加）。hook 的输出是**决策 JSON**，
写不出去等于弹窗白等 1 小时——这个风险不能赌。

⇒ `pomodoro-hook.exe` 编译为 **console subsystem**（保证 stdout 可靠），
`番茄钟.exe` 编译为 **windows subsystem**（不起黑框）。
两者 `use pomodoro_core::*` 共享网关发现、协议适配、超时逻辑，只各写一层 IO 壳。

### 体积优化要点

- `[profile.release]`: `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`,
  `strip = true`
- hook 侧 HTTP 用 `ureq`（或直接 `TcpStream` 手写，避免拉入 tokio）；GUI 侧才用异步栈
- NSIS 安装模式：**不捆绑 WebView2**（Win11 自带；本机实测已装 152 / 153），
  可选 `minimumWebview2Version` 做版本校验而非静默失败

---

## 3. 需要拍板的选项

### D1 · hook CLI 是否一并 Rust 化

| | A. 一并 Rust 化（推荐） | B. 暂留 Node 脚本 |
|---|---|---|
| 宿主要求 | **不再需要 Node** | 宿主环境必须有 Node 在 PATH |
| 每次工具调用延迟 | ~2 ms | 40–80 ms（每次 PreToolUse/PostToolUse 都跑） |
| 工作量 | 重写 1708 行 + 迁移用户配置 | 把 .js 塞进 Rust 二进制、运行时释放到 userData |
| 风险 | 适配层每宿主一条分支，需与 `docs/agent-hooks.md` 逐条对表 | 保留既有 Node 依赖与启动开销 |

> 这是本方案**最容易被低估的收益**：hook 是每次工具调用都跑的热路径，且现在隐含要求用户的
> 机器装了 Node。`--clean` 迁移逻辑现成，可复用。

### D2 · 渲染层怎么处理

| | A. 原样保留 + `bridge.js` 垫片（推荐） | B. 借机重写 |
|---|---|---|
| 改动量 | `renderer/*` 几乎零改动：垫片把 `window.pomodoro.xxx()` 映射到 Tauri `invoke/emit` | 重写 3432 行 |
| 风险 | 需重新验证 CSP 与 IPC 通道 | 回归风险集中在 UI 手感（贴边、迷你模式、玻璃质感） |

`renderer/` 是纯净的 HTML/CSS/JS：**没有打包器、没有 import、CSP 是 `default-src 'self'`**，
天然适配 Tauri 的 `frontendDist`（连 dev server 都不需要，整个前端链路零 npm）。

### D3 · 网关（`gateway.js` 772 行）

建议 Rust 重写（`axum` 或 `tiny_http`），因为 hook CLI 与网关的 **API 契约是本项目最硬的接口**
（`/api/interaction` 长轮询、token 鉴权、held 状态机）。两个进程都 Rust 化后可以共享同一份
serde 类型定义，契约不会两边写歪。备选：保留 Node 子进程——但那就把 Node 依赖又带回来了。

### D4 · 版本与分支策略

- 现在：worktree `pomodoro-fluent-rust` / 分支 `tauri-rewrite`（已建好，基于 `main@5f1bcd1`）
- **两版并存 + 统一版本号**：同一个仓库里同时留两套实现，一次打包出两份安装包，
  挂在同一条 release（tag `vX.Y.Z`，**不带前缀**）。版本号只有一个事实源：`package.json`，
  由 `scripts/pack-release.mjs` 同步进 `tauri.conf.json`。详见 `docs/dual-release.md`。
- **不把 Rust 版号单独抬到 2.0.0**：版本号属于整个仓库，两版一起走。
  真要换大版本号（功能对齐时）只改 `package.json` 一处。
- hook 配置迁移：老用户的 `~/.claude/settings.json` 里是 `node ".../pomodoro-hook.js"`，
  新 `install --clean` 必须能识别并替换（现有逻辑保留，只改 `hookCmd()` 一处即可——
  实测全文只有第 1182 行一处拼命令）

---

## 4. 必须平移的既有行为（验收清单）

这些是本项目踩过坑换来的约定，迁移时**逐条当作验收项**，不是可选项：

1. **三层超时嵌套**：宿主 hook timeout 4200s > hook 等网关 3900s > 番茄钟兜底 3600s。
   宿主那层必须最大。（`DEFAULT_CONFIRM_TIMEOUT_MS` / `POMODORO_TIMEOUT_S` 语义保持）
2. **两种「临时关闭」语义不能合并**：右上角 × = 交给终端（dismissed）；
   底部「暂时收起」= 挂起（`state='held'`，**不 resolve、不停兜底定时器**）。
3. **唤回三入口**：主窗提示条 `#heldChip`、**托盘左键单击**（无 held 时什么都不做）、
   托盘右键子菜单。至少一个入口在主界面里。
4. **兜底值不算用户决定**：hook CLI 只认 `decidedBy === 'user'`。
5. **交互窗不套 `MAX_NOTIFY_MS`**；悬停暂停只对非交互窗生效。
6. **只有一处主动网络请求**：用户点「检查更新」才发 GitHub release API。启动/定时都不查。
7. **hook 未运行时静默退出**，绝不阻断 agent；只上报事件，不落盘。
8. **`install<Host>()` 的四处同步**：CLI、app.js 的「复制配置」、`docs/agent-hooks.md`、
   `README.md`。
9. PreToolUse 仍不做审批（只处理提问）；审批只走 `PermissionRequest`。

---

## 5. 风险清单

| 风险 | 影响 | 对策 |
|---|---|---|
| WebView2 缺失/过旧 | 装完打不开 | Win11 自带；安装器加 `minimumWebview2Version` 校验并给出提示 |
| 多屏 / DPI 定位差异（`screen` API 用了 15 处） | 弹窗跑到错误显示器、贴边位置偏 | 单独做一轮多屏 + 125%/150% 缩放验证 |
| 托盘单击 / 双击语义 | 左键唤回 held 失效 | 明确排除双击冲突；Win11 溢出区行为属 OS，文档里写明引导用户固定图标 |
| CSP + Tauri IPC 通道 | 渲染层调不通主进程 | 垫片先行，M0 阶段就用真实 `notify.html` 验证 |
| 无热更新 | 用户看不到改动 | 沿用现状：发布即重装；README 标注 |
| 安装目录/快捷方式变化 | 用户找不到新版本 | 安装器保持可自选目录 + 开始菜单「番茄钟」同名 |
| 未签名 | SmartScreen 警告 | 现状同样未签名，不回归即可 |

---

## 6. 分阶段里程碑

| 阶段 | 内容 | 产出验证 |
|---|---|---|
| **M0** | workspace 骨架 + 空窗口 + 托盘图标 + `tauri build` 出 NSIS | 安装包 < 8 MB，托盘能点 |
| **M1** | 主窗 UI（renderer 原样）+ 计时逻辑 + 配置持久化 + 迷你/贴边/acrylic | 计时可用，`renderer/*` 未被改动 |
| **M2** | 网关 Rust 化 + 弹窗（ask / permission）+ 三层超时 + held 状态机 | 三个唤回入口全通 |
| **M3** | hook CLI Rust 化 + `install` 子命令 + 四处文档同步 | 各宿主真实 CLI 跑通 |
| **M4** | `scripts/smoke-interaction.js` 迁移为 Rust 集成测试 | 冒烟用例全绿 |
| **M5** | 打包 + 冒烟 + 发新版（与 Electron 版同号、同一条 release） | 安装包体积实测对比 |

M0 结束时就能拿到真实体积数字——建议**先做完 M0 再决定要不要一路推到底**。

---

## 7. 前置条件（已在本机核实）

| 项 | 状态 |
|---|---|
| Rust | ✅ 1.96.0，默认 host `x86_64-pc-windows-msvc` |
| MSVC 链接器 | ✅ VS 2022 + Windows SDK 10.0.26100 |
| WebView2 运行时 | ✅ 已装 152.0.4191.66 / 153.0.4234.32 |
| `cargo-tauri` CLI | ✅ 已装（`cargo install tauri-cli --version "^2"`，实测 2.11.6） |
| Node | 仅 Electron 版与打包脚本需要；**Rust 版不随包发布 Node** |

---

## 8. 明确不做的事

- 不做后台自动更新 / 静默下载（沿用「用户点了才检查」）
- 不加任何遥测、不上报、不打通外部服务
- 不恢复任何「工作记录 / 报告」能力
- 不引入 npm 前端工具链（渲染层无打包器，继续维持零构建）

---

## 9. M0 实测结果（2026-09-20 已完成）

| 检查项 | 结果 |
|---|---|
| `cargo build --release` | ✅ 3m48s（tauri 2.11.6 / tao 0.35.3 / tray-icon 0.24.2 / webview2-com 0.38.2） |
| `cargo tauri build` → NSIS | ✅ `target/release/bundle/nsis/番茄钟_1.1.6_x64-setup.exe`（M0 当时是 2.0.0，后统一到 `package.json` 的版本号） |
| **安装包体积** | **1,065,276 B（1.04 MB）** —— Electron 版 89 MB，**-98.9%** |
| GUI exe | 2,791,424 B（2.66 MB） |
| hook exe | 110,080 B（107 KB） |
| GUI 冒烟 | ✅ 启动后 7s 仍存活，宿主进程 25 MB，WebView2 子进程正常拉起，托盘图标出现 |
| hook stdout | ✅ `pomodoro-hook.exe` → `{"continue":true}`，exit 0（验证了双 exe 的必要性） |

### 踩坑：GitHub 被墙导致 NSIS 打包失败

首次 `cargo tauri build` 报 `Error failed to bundle project: timeout: global` —— bundler 需要从
GitHub releases 下载 NSIS 工具链与 `nsis_tauri_utils.dll`，而本机对 GitHub 的连接被重置
（`curl` 直接 `Recv failure: Connection was reset`）。

**解法（用 Tauri 自带的正规开关，别去手工解压布局）**：

```bash
TAURI_BUNDLER_TOOLS_GITHUB_MIRROR=https://ghfast.top cargo tauri build
```

该环境变量真实存在于 `cargo-tauri.exe` 中（`grep -a -o -E "TAURI_[A-Z_]{3,50}" cargo-tauri.exe`
能查出来），它把下载地址拼成 `https://ghfast.top/https://github.com/...`。
实测 `ghfast.top` / `gh-proxy.com` / `ghproxy.net` 可用；`gh.llkk.cc` / `ghproxy.cc` 不通。

工具链落在 `%LOCALAPPDATA%\tauri\NSIS\`。若装坏了下错版本，直接删掉该目录再重跑即可
（删的时候要用 MSYS 风格路径 `/c/Users/...`，不要用 `$LOCALAPPDATA`，否则会被 safe-delete
拦截并 fail-closed）。

### M0 的边界

M0 只验证「能跑、能打包、体积达标」。窗口仍是 `src-tauri/m0/index.html` 占位页，
`frontendDist` 也还指着 `m0`；M1 才切到 `../renderer` 并接上 `bridge.js`。
`renderer/*` 至今一行未改。

---

## 10. M1 实测结果（2026-09-20）

目标：把**真实的 `renderer/`** 接上 Tauri。硬约束 —— `renderer/` 一行不改。

### 做了什么

| 项 | 做法 |
|---|---|
| 前端来源 | `tauri.conf.json` 的 `frontendDist` 切到 `../renderer`（`m0` 占位页作废） |
| 渲染层适配 | `src-tauri/src/bridge.js`，用 `initialization_script` **注入**，把 `window.pomodoro.*` 接到 Tauri invoke / event。**它是唯一适配层**，渲染层零改动 |
| IPC 通道 | `withGlobalTauri: true` + `bridge.js` 内部**双通道**：优先 `window.__TAURI__.core.invoke`，退化到 `window.ipc.postMessage` |
| 窗口行为 | `geometry` / `state` / `window` 三个模块：主窗、迷你模式、贴边隐藏、拖拽限位。纯几何部分带单元测试 |
| 托盘 | `tray.rs`：图标染色、菜单重建、状态去重（等价 Electron 的 `syncTray`） |
| 关于页 | `about.rs`：版本号、检查更新（**唯一联网点**，用户点了才发）、打开外链 |
| 单实例 | `tauri-plugin-single-instance`，回调 = `reveal_main`（等价 Electron 的 `second-instance`） |
| 版本同步 | `scripts/pack-release.mjs` 改为写根 `Cargo.toml` 的 `[workspace.package] version`，并**断言**各成员 crate 不得硬编码 `version = "..."` |

### 验收结果

| 检查项 | 结果 |
|---|---|
| 编译 | ✅ `cargo build` 无警告，exe 14,553,088 B（14.5 MB，debug） |
| 单元测试 | ✅ **40 passed / 0 failed** |
| bridge 对齐 | ✅ `scripts/check-bridge-parity.mjs`：`preload.js` 暴露 30 个方法 → `bridge.js` 30 个；`bridge.js` 调用的 15 个命令 → `commands.rs` 全部有定义 |
| **`renderer/` 未改动** | ✅ `git status renderer/` 为空，且与主仓库 6 个文件**逐字节一致** |
| IPC 链路实测 | ✅ 真起窗口，渲染层启动期的两个 invoke（`tray_update` + `pending_get_held`）都到达 Rust |

### 补的两个工具（M1 新增）

- **`scripts/check-bridge-parity.mjs`** —— 纯静态检查，不启应用。它抓的是 M1 最容易犯的错：
  `bridge.js` 漏实现一个方法。渲染层里一个 `TypeError` 就会让整个界面死掉，而**进程照活、托盘照在**，
  纯靠肉眼和"窗口起来了"完全看不出来。**实测有效**：它当场抓出 `requestHeldPending` 漏了
  （渲染层 `app.js` 启动时**无条件**调用它，漏了就是启动即崩）。
- **`scripts/smoke-tauri.mjs`** —— 真起 GUI 的冒烟，判定依据是 Rust 侧 `[smoke]` 三行 stderr
  （见 `src-tauri/src/smoke.rs`，`POMODORO_SMOKE=1` 才启用）。

  **为什么不用 CDP 进 webview**（试过，已放弃）：① WebView2 的 Browser 进程按用户数据目录共享，
  被强杀的实例会留下孤儿 `msedgewebview2.exe`，新实例复用它 → 自定义协议整个不响应，而**窗口照画**；
  ② 就算调试端口起来了，实测 t+3s 能看到页面、t+6s 起 DevTools 的 HTTP 端点整个不再响应，
  断言还没跑完端点就死了。

### ⚠ 踩坑（已解决）：冒烟"随机挂死"的真凶是残留实例，不是 WebView2

**这段值得完整留着**，因为症状极具误导性，浪费了一整轮排查。

**最初症状**：`WebviewWindowBuilder::build()` 看起来会**永久挂住** —— 埋点 `step: setup 进入`
打了、`step: 主窗口已建` 永远不打，窗口不出现，进程一直活着。通过率从会话开始的 100%
一路掉到 ~10%，"同一份二进制、零代码改动却越来越差" → 一度误判为**本机 WebView2 运行时退化**。

**逐个排除的假设**（每个都单独做过对照，全部无效）：

| 假设 | 结果 |
|---|---|
| WebView2 用户数据目录（`WEBVIEW2_USER_DATA_FOLDER` 指新目录） | 无关；且 Tauri 用自己的目录，这个环境变量实际不被采纳 |
| 残留孤儿 `msedgewebview2.exe` | 无关 —— 本应用相关进程数为 0 |
| 继承的 Chromium 环境变量（`CHROME_CRASHPAD_PIPE_NAME` / 代理 / `ELECTRON_RUN_AS_NODE`） | 略好但不解决；`env -i` 完全干净环境仍失败 |
| `tauri-plugin-single-instance` | 摘掉后仍失败（5/8） |
| 建窗口放 `setup()` vs `RunEvent::Ready` | 两处都失败，Ready 更差（1/10） |
| `--disable-gpu` / 禁用 crash reporter | 无关 |
| 窗口透明（`.transparent(true)` → WebView2 Composition Controller） | 关掉透明后仍失败（2/8） |
| 磁盘 / 句柄 / profile 损坏 | 磁盘 31 GB 空闲；profile 仅 70 项 |

**真凶**：残留的**应用实例**（以及它带起来的 WebView2 浏览器进程）。一旦有一个实例卡住没退，
它会同时占住两样东西，于是后续每一次启动都必然失败，而且**失败长相和最初成因完全不同**：

1. **单实例锁**被占 → 新实例在 `setup()` **之前**就被 `tauri-plugin-single-instance` 结束掉，
   exit 0、一行 stderr 都不多打。指纹：`[smoke] 自检模式已启用` 有、`step: setup 进入` 没有。
2. **WebView2 用户数据目录**被那个实例的浏览器进程占着 → 就算绕过了单实例锁，
   新实例的 `CreateCoreWebView2EnvironmentWithOptions` 也会**堵在那里**（就是那个"build() 挂死"）。

⇒ 这是**自我延续的陷阱**：初次偶发一次挂起之后，后面全部是它的回声。

**为什么最初没发现**：`taskkill //F //IM pomodoro.exe //T` **从 Git Bash 里发经常不生效**
（命令静默失败），被 `2>/dev/null` 吞掉了；`kill -9` 也依赖 bash 的 job pid。
**必须用 PowerShell 的 `Stop-Process -Id <pid> -Force`**，并且要同时确认
`Win32_Process` 里 `pomodoro.exe` 数为 0。

**修复**：`scripts/smoke-tauri.mjs` 加了**前置检查** —— 开跑前先用 `tasklist` 确认没有实例在跑，
有就直接报错退出（`exit 2`），不产出误导性的"失败"。想自动清掉可设 `TAURI_SMOKE_KILL_EXISTING=1`。

**验证结果**：

| 轮次 | 结果 |
|---|---|
| 干净状态第 1 轮（`TAURI_SMOKE_ATTEMPTS=1`，10 次） | ✅ **10 / 10** |
| 干净状态第 2 轮（12 次） | ✅ **12 / 12** |

**教训（写给未来的自己）**：GUI 冒烟测试的**第一步永远是确认进程表干净**。
"测试脚本自己造成的假阴性"比"应用缺陷"更常见，而且更贵 —— 这次为它花的时间
远超写 M1 本身。另外：**同一个二进制在零代码改动下表现变差，几乎一定不是应用代码的问题**。


