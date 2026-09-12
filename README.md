# 🍅 番茄钟 · Pomodoro Fluent

一个符合 **Windows 11 Fluent Design** 设计规范的番茄钟桌面应用，基于 Electron 构建。

![番茄钟](docs/screenshot.png)

![License](https://img.shields.io/badge/license-MIT-green) ![Platform](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-blue) ![Electron](https://img.shields.io/badge/Electron-44-47848F)

## ✨ 功能特性

- **⏱️ 番茄钟计时**：专注 25 分钟 / 短休 5 分钟 / 长休 15 分钟，支持自定义时长
- **🔄 自动循环**：专注 → 短休（4 轮后长休）→ 专注，可开启自动进入下一阶段
- **🔔 精美弹窗提醒**：阶段结束时弹出 Win11 风格毛玻璃通知，带动画与进度条
- **📌 迷你悬浮窗**：点击图钉收成 176×64 紧凑小窗，只剩时间与阶段文字；悬停时按钮从右侧浮出；置顶显示、自动撤下任务栏图标
- **🧲 贴边隐藏**：迷你小窗拖到屏幕上/下/左/右边缘自动吸附，收起成 6px 进度细条；悬停滑出完整小窗，移开自动收回；多显示器下不串屏
- **🖥️ 后台运行**：关闭窗口最小化到系统托盘，后台持续计时
- **🤖 Agent 集成**：本地 Agent 网关 + hook CLI，Claude Code / OpenCode 需要确认、权限审批或任务完成时弹窗提醒，支持弹窗上直接「允许/拒绝」；统计专注期 agent 工具调用与打断次数，agent 空闲时建议休息（详见 [Agent 集成指南](docs/agent-hooks.md)）
- **🎨 智能配色**：界面配色随阶段变化（专注红 / 短休绿 / 长休蓝）
- **🖱️ 无边框玻璃窗口**：Fluent 圆角卡片，可自由拖动

## 🚀 快速开始

### 环境要求
- Node.js 18+（含 npm）
- Windows 10/11

### 安装与运行

```bash
# 1. 安装 Electron
npm install

# 2. 启动应用
npm start
```

> 💡 若 Electron 二进制下载缓慢，可配置镜像：
> ```bash
> set ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/
> npm install
> ```

## 🎯 使用说明

| 操作 | 效果 |
|---|---|
| 点击 **▶ 按钮** | 开始 / 暂停计时 |
| 点击 **⏹ 重置** | 重置当前阶段计时，**轮次同时回到第 1 轮 · 本轮 1/N** |
| 点击 **⏭ 跳过** | 跳过当前阶段 |
| 点击 **Tab**（专注/短休/长休） | 手动切换阶段 |
| 点击 **📌 图钉** | 收成迷你悬浮小窗（置顶）；悬停小窗浮出 重置/开始·暂停/跳过 按钮；点时间展开回完整窗口 |
| 拖动迷你小窗**到屏幕边缘** | 贴边吸附，收起成进度细条；悬停细条滑出，拖离边缘取消吸附 |
| 点击 **⚙ 齿轮** | 打开设置（时长 / 自动循环 / Agent 集成） |
| 点击 **— 最小化** | 最小化到系统托盘 |
| 点击 **✕ 关闭** | 隐藏到托盘，后台继续计时 |
| **空格键** | 快速开始/暂停 |
| **R 键** | 快速重置（含轮次） |
| **双击托盘图标** | 显示主窗口（迷你/贴边态会先展开完整窗口） |
| 设置抽屉 → **复制 Hook 配置** | 一键生成 Claude Code hooks 配置片段（含本机 hook 脚本路径） |

### 托盘菜单
右键托盘图标可：显示主窗口 / 开始-暂停 / 重置 / 跳到下一阶段 / 退出。

### 后台运行
应用关闭窗口后不会退出，而是隐藏在系统托盘（任务栏右侧的 🍅 图标），计时继续，到点照常弹窗提醒。**退出请在托盘右键菜单选择「退出」。**

## 🤖 Agent 集成（Claude Code / OpenCode）

应用运行时会在本地启动一个 **Agent 网关**（默认 `http://127.0.0.1:5277`，仅绑定本机回环地址 + 随机 token 鉴权），让 AI 编程工具与番茄钟联动：

- **需要确认 / 权限审批 / 任务完成时弹窗提醒** —— 人不在终端前也能看到
- **弹窗远程批准**：开启 PreToolUse 双向确认后，可在弹窗上直接点「允许 / 拒绝」，决策回传给 Claude Code（超时安全回退，不会误放行）
- **专注期活动统计**：主窗口显示 `🤖 工具 N · 打断 M`，专注结束弹窗汇总本期 agent 产出
- **休息建议**：agent 跑完任务而你仍在专注时段，弹窗建议趁机休息，一键跳到休息

**Claude Code 三步接入**：应用保持运行 → 打开设置抽屉点击「复制 Hook 配置」→ 把片段粘贴进 `~/.claude/settings.json` 的 `hooks` 字段。应用会自动把 hook 脚本安装到 `%APPDATA%\番茄钟\hook\pomodoro-hook.js`，配置一次即可。

任意脚本也能直接调用（token 见 `%APPDATA%\番茄钟\gateway.json`）：

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"title":"构建完成","message":"可以回来验收了"}' \
  http://127.0.0.1:5277/api/notify
```

OpenCode 插件接入、远程允许/拒绝、HTTP API 全量说明见 **[Agent 集成指南](docs/agent-hooks.md)**。

## 🛠️ 技术实现

- **主进程**（`main.js`）：窗口管理、系统托盘、通知弹窗、单实例锁
- **Agent 网关**（`gateway.js`）：仅绑定 `127.0.0.1` 回环 + 每次启动随机 token + Host 头校验（防 DNS rebinding）；双向确认经长轮询回传决策；hook CLI（`bin/pomodoro-hook.js`）零依赖，应用未运行时静默退出，绝不阻断 agent
- **迷你悬浮 & 贴边隐藏**：手动光标跟随拖拽（原生 drag 区会吞掉 `:hover`）；贴边收起时窗口带透明留白绕开 Windows 约 32×39 的最小窗口限制，仅靠屏幕边缘的 6px 绘制进度条，透明区域完全穿透（可见性与点击均不受影响）
- **渲染进程**（`renderer/`）：Win11 风格 UI + 番茄钟逻辑
- **安全桥接**（`preload.js`）：contextBridge 隔离

### 视觉设计
- **毛玻璃效果**：`backdrop-filter: blur` + 半透明背景（CSS 兜底）
- **系统级 Acrylic 模糊**：通过 Win32 `SetWindowCompositionAttribute`（ACCENT_ENABLE_ACRYLICBLURBEHIND）实现真正的背景模糊（`main.js` 中 `ENABLE_ACRYLIC` 开关，部分 Win10 版本上开启会有卡顿/发白问题，默认关闭，由近实心玻璃卡片兜底）；打包后脚本自动从 asar 解包到用户数据目录执行
- **Fluent 圆角**：主窗口 12px、按钮 7-9px、弹窗 14px；透明窗口只画内容圆角、不画外溢阴影，四角干净
- **自适应配色**：CSS 变量随阶段切换
- **平滑动画**：环形进度、弹性按钮、弹窗入场

## 📦 打包为独立应用

```bash
# 安装打包工具（需要网络，国内可用 npmmirror 加速）
npm i -D electron-builder --registry=https://registry.npmmirror.com

# 打包为便携目录（快速验证，输出 dist/win-unpacked）
npm run pack:dir

# 打包 Windows 安装包（NSIS，输出 dist/Pomodoro-Fluent-Setup-1.0.1.exe）
npm run pack
```

打包产物位于 `dist/` 目录：
- `Pomodoro-Fluent-Setup-1.0.1.exe` —— 安装程序（含桌面/开始菜单快捷方式，可选安装目录；纯英文产物名，GitHub Release 附件名不支持中文）
- `win-unpacked/番茄钟.exe` —— 免安装便携版（直接运行）

> 打包需要联网下载 NSIS 等工具，若速度慢可设置镜像：
> ```bash
> set ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/
> set ELECTRON_BUILDER_BINARIES_MIRROR=https://npmmirror.com/mirrors/electron-builder-binaries/
> npm run pack
> ```

## 📁 项目结构

```
pomodoro-fluent/
├── main.js              # Electron 主进程
├── preload.js           # 安全桥接层
├── gateway.js           # Agent 网关（本地 HTTP，hooks 对接）
├── bin/
│   └── pomodoro-hook.js # Agent hook CLI（Claude Code / OpenCode / curl）
├── package.json
├── apply-acrylic.ps1    # Acrylic 毛玻璃（DWM API，PowerShell）
├── assets/              # 图标资源（自动生成）
│   ├── icon.png         # 应用/窗口图标
│   └── tray.png         # 托盘图标模板
├── scripts/             # 打包辅助脚本（afterPack）
├── docs/                # 截图与文档（agent-hooks.md）
└── renderer/            # 界面
    ├── index.html       # 主窗口
    ├── styles.css       # 主界面样式（Win11 Fluent）
    ├── app.js           # 番茄钟逻辑
    ├── notify.html      # 通知弹窗（支持 agent 确认模式）
    ├── notify.css       # 弹窗样式
    └── notify.js        # 弹窗逻辑
```

## 📄 许可证

[MIT](LICENSE)
