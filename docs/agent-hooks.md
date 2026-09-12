# Agent 集成指南（Claude Code / OpenCode × 番茄钟）

番茄钟运行时会在本地启动一个 **Agent 网关**（默认 `http://127.0.0.1:5277`，仅绑定本机回环地址），供 Claude Code、OpenCode 等支持 hook 的 agent 工具调用。agent 需要你确认、权限审批或任务完成时，番茄钟会弹出 Fluent 风格的桌面弹窗——人不在终端前也能看到（甚至远程批准）。

```
Claude Code hook ──┐                                 ┌─ 通知弹窗（需要确认/任务完成）
(bin/pomodoro-hook.js)│                                 ├─ 确认弹窗（允许/拒绝，双向）
OpenCode 插件 ──────┼─HTTP→ 127.0.0.1:5277 网关 ──→ ├─ 专注期活动计数（工具/打断）
curl / 任意脚本 ────┘   (token 鉴权)                   └─ 远程控制（开始/重置/跳过）
```

## 开启与关闭

设置抽屉（主窗口齿轮）→「Agent 集成」→ 启用 Agent 网关。默认开启，状态与端口持久化在 `userData/config.json`。

番茄钟启动时会自动把 hook 脚本安装到固定路径（与仓库/安装位置解耦）：

```
%APPDATA%\番茄钟\hook\pomodoro-hook.js
```

## Claude Code 接入

编辑 `~/.claude/settings.json`（用户级）或项目 `.claude/settings.json`，加入 hooks。**路径用你机器上的实际 hook 路径**：

```json
{
  "hooks": {
    "Notification": [
      { "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\"" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\"" }] }
    ],
    "SubagentStop": [
      { "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\"" }] }
    ],
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\"" }] }
    ]
  }
}
```

主窗口设置抽屉里的「复制 Hook 配置」按钮会生成这段 JSON（已带本机路径）。

各事件的效果：

| hook 事件 | 效果 |
|---|---|
| `Notification` | 弹「Agent 需要你的确认」通知；计入本专注期打断数 |
| `Stop` | agent 回合结束；若仍在专注时段弹「休息建议」（一键跳休息，10 分钟冷却） |
| `SubagentStop` | 子 agent 结束（仅计数，不弹窗） |
| `PostToolUse` | 工具调用计数（主窗口显示 `🤖 工具 N · 打断 M`） |

### 远程允许/拒绝（双向确认，可选）

设置环境变量 `POMODORO_CONFIRM_PRETOOL=1` 并把 `PreToolUse` 也接上（注意给足超时，Claude Code hook 默认 60s 会提前掐掉长轮询）：

```json
"PreToolUse": [
  {
    "matcher": "Bash",
    "hooks": [{
      "type": "command",
      "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\"",
      "timeout": 360,
      "env": { "POMODORO_CONFIRM_PRETOOL": "1" }
    }]
  }
]
```

开启后每次触发匹配的工具调用，番茄钟会弹带「允许 / 拒绝」按钮的确认窗，你的选择会以 `permissionDecision` 返回给 Claude Code——在弹窗上点「允许」等同于在终端按了允许。超时或未决策则回退 `ask`（交回终端原生询问，不会误放行）。等待时长可用 `POMODORO_CONFIRM_TIMEOUT_S` 调整（默认 240 秒）。

> 建议只对高风险工具（如 `Bash`）开双向确认，其余交给 `Notification` 通知即可，避免弹窗轰炸。

## OpenCode 接入

OpenCode 通过插件调用网关。在 `~/.config/opencode/plugin/pomodoro.js`（全局）或项目 `.opencode/plugin/pomodoro.js` 新建：

```js
// 番茄钟联动插件：读 gateway.json 拿端口与 token，把工具事件上报给番茄钟
import { readFileSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

function gateway() {
  try {
    const file = join(homedir(), "AppData", "Roaming", "番茄钟", "gateway.json")
    return JSON.parse(readFileSync(file, "utf8"))
  } catch { return null }
}

async function report(kind, extra = {}) {
  const gw = gateway()
  if (!gw) return
  try {
    await fetch(`http://127.0.0.1:${gw.port}/api/event`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${gw.token}`,
      },
      body: JSON.stringify({ kind, source: "opencode", ...extra }),
    })
  } catch { /* 番茄钟未启动时静默 */ }
}

export const PomodoroPlugin = async ({ bus }) => {
  bus.on("tool.execute.after", async ({ tool }) => report("tool-after", { tool: tool ?? "" }))
  // permission.ask 在部分版本尚未触发，触发时即为打断计数
  bus.on("permission.ask", async () => report("notification", { message: "OpenCode 等待你的确认" }))
}
```

（事件名以 [OpenCode 插件文档](https://opencode.ai/docs/plugins/) 为准，不同版本可能调整。）

## HTTP API（curl / 任意脚本）

发现文件 `%APPDATA%\番茄钟\gateway.json` 含 `port` 与 `token`。除 `/health` 外均需 `Authorization: Bearer <token>`。

```bash
# 存活探测
curl http://127.0.0.1:5277/health

# 定时器状态 + 本专注期活动
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:5277/api/status

# 弹一条通知
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"title":"构建完成","message":"可以回来验收了"}' \
  http://127.0.0.1:5277/api/notify

# 双向确认（长轮询，返回 {"ok":true,"action":"break","decidedBy":"user"}）
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"title":"部署到测试环境？","actions":[{"id":"ok","label":"部署","style":"primary"},{"id":"no","label":"取消","style":"danger"}],"timeoutMs":120000}' \
  http://127.0.0.1:5277/api/confirm

# 远程控制：toggle / reset / skip
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"command":"skip"}' http://127.0.0.1:5277/api/timer
```

CLI 直连模式（等价于上面的 notify/status）：

```bash
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" notify --title "构建完成" --message "可以回来验收了"
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" status
```

## 事件类型（POST /api/event）

| kind | 含义 | 网关行为 |
|---|---|---|
| `notification` | 权限请求 / 等待用户输入 | 打断计数 + 弹通知 |
| `stop` | 主 agent 回合结束 | 计数；专注中则弹「休息建议」 |
| `subagent-stop` | 子 agent 结束 | 仅计数 |
| `tool-after` | 工具调用完成 | 工具调用计数 |
| `tool-before` / `prompt` | 工具调用前 / 用户提交提示词 | 预留 |
| `session-start` / `session-end` | 会话开始/结束 | 会话计数 |

## 与番茄工作法的联动

- **专注期活动统计**：工作阶段主窗口显示 `🤖 工具 N · 打断 M`；专注结束弹窗汇总本番茄内 agent 的工具调用与打断次数。
- **休息建议**：agent 跑完任务（`stop`）而你还在专注时段，弹窗建议趁机休息，一键提前进入休息。
- **打断质量**：打断数 = 权限确认次数。打断越多说明预授权越少——试试更完整的权限预案（如 Claude Code 的 plan 模式、允许清单），把人从审批循环里解放出来，专注质量会明显提升。

## 安全说明

- 网关仅绑定 `127.0.0.1`，局域网不可达；
- 除 `/health` 外全部要求随机 token（每次启动重新生成，存于本机 `gateway.json`）；
- 校验 `Host` 头，仅接受 `127.0.0.1` / `localhost`，防 DNS rebinding；
- 浏览器页面因无 CORS 头也无法读取响应。

## Roadmap

- **MCP server**（二期）：agent 主动调用番茄钟——查状态、开专注、任务认领番茄；
- **专注标签**：SessionStart 时把 agent 任务绑定到当前番茄，结束弹窗展示产出；
- **任务番茄账**：任务列表按番茄计费（经典番茄工作法估算）。
