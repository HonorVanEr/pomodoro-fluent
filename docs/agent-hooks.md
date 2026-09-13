# Agent 集成指南（ZCode / Claude Code / OpenCode × 番茄钟）

番茄钟运行时会在本地启动一个 **Agent 网关**（默认 `http://127.0.0.1:5277`，仅绑定本机回环地址）。
agent 要提问、要权限、或只是通知你一声时，番茄钟弹出 Fluent 风格桌面弹窗——**提问和权限可以直接在弹窗里作答**，不用来回切终端。

```
ZCode hook ────────┐                                    ┌─ 提问弹窗（选项/多选/自定义回答）
Claude Code hook ──┼─HTTP→ 127.0.0.1:5277 网关 ─→ 弹窗 ─┼─ 权限弹窗（允许 / 始终允许 / 拒绝）
OpenCode 插件 ─────┤      (token 鉴权)      ↖ 长轮询   ├─ 通知弹窗（看完即走）
curl / 任意脚本 ───┘                        用户决策    └─ 专注期活动计数 / 远程控制
```

## 三种弹窗

| kind | 触发场景 | 弹窗里能做什么 | 返回给 agent |
|---|---|---|---|
| `ask` | `AskUserQuestion` / OpenCode `question.asked` | 选选项（单选即时提交、多选确认后提交）、填自定义回答、`Enter` 提交 `Esc` 取消 | `answers` |
| `permission` | `PermissionRequest` / OpenCode `permission.ask` | 允许一次 / 始终允许 / 拒绝，可填备注（会作为拒绝理由回传） | `allow` / `allow-always` / `deny` |
| `notification` | `Notification`、`session.error` 等 | 只展示，几秒后自动消失 | — |

**兜底策略（重要）**：

- 超时未决策 → `permission` 落回 **deny**，`ask` 落回 **cancel**（安全侧，绝不替你放行）；
- 你手动关掉弹窗、或弹窗被新弹窗顶掉 → 返回空决策，agent 回退到**终端原生询问**，不会静默通过；
- 番茄钟没启动 → hook 静默退出，绝不阻断 agent。

## 开启与关闭

设置抽屉（主窗口齿轮）→「Agent 集成」→ 启用 Agent 网关。默认开启，状态与端口持久化在 `userData/config.json`。

番茄钟启动时会自动把脚本安装到固定路径（与仓库/安装位置解耦）：

```
%APPDATA%\番茄钟\hook\pomodoro-hook.js
%APPDATA%\番茄钟\hook\opencode\pomodoro-opencode.ts
```

## 一键接入（推荐）

在设置抽屉里选好 agent，点「复制安装命令」，粘到终端执行即可。等价命令：

```bash
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent zcode
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent claude
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent opencode
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent all
```

- 会**自动合并**进对应配置文件，原文件备份为 `*.pomodoro.bak`；
- 已存在相同条目不会重复写入；
- 加 `--print` 只打印将要写入的内容，不动文件；
- 加 `--clean` 会清掉指向番茄钟 hook 其它副本的旧条目（避免同一个事件弹两次窗）。

改动需**重启 agent 会话**后生效（三端都是在会话启动时快照 hook 配置）。

## ZCode

配置在 `~/.zcode/cli/config.json`，两个坑：必须 `hooks.enabled: true`，事件挂在 `hooks.events` 下（项目级 `.zcode/config.json` 当前版本不执行）。

```json
{
  "hooks": {
    "enabled": true,
    "timeoutMs": 600000,
    "events": {
      "PermissionRequest": [
        { "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source zcode", "timeoutMs": 600000 }] }
      ],
      "PreToolUse": [
        { "matcher": "AskUserQuestion", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source zcode", "timeoutMs": 600000 }] }
      ],
      "Stop": [
        { "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source zcode" }] }
      ],
      "PostToolUse": [
        { "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source zcode" }] }
      ]
    }
  }
}
```

- ZCode 的 `AskUserQuestion` 会**同时触发 `PreToolUse` 和 `PermissionRequest`**（两个独立进程）。hook 用 `%TEMP%` 下的短期缓存（按 `tool_use_id`，90 秒）去重，**同一个问题只弹一次窗**，两个事件复用同一次决策。
- 答案通过 `updatedInput.answers` 注入，原生提问 UI 不再出现：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": { "behavior": "allow", "updatedInput": { "answers": { "用哪种方案？": "B 方案" } } }
  }
}
```

- 「始终允许」会带 `updatedPermissions`（`addRules`），默认写到 `projectSettings`，可用 `POMODORO_PERMISSION_DEST` 改（`userSettings` / `localSettings` / `session`）。

## Claude Code

配置在 `~/.claude/settings.json`：

```json
{
  "hooks": {
    "Notification": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }],
    "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code", "timeout": 600 }] }],
    "PreToolUse": [{ "matcher": "AskUserQuestion", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code", "timeout": 600 }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }]
  }
}
```

权限决策输出（Claude Code 的 `PermissionRequest` 用 `decision` 对象）：

```json
{ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": { "behavior": "allow", "message": "用户通过番茄钟弹窗允许" } } }
```

> 提问（AskUserQuestion）说明：Claude Code 只在 `-p` 非交互模式官方支持 `defer` 回传答案。
> 交互模式下本 hook 默认仍走 `updatedInput.answers` 注入；若你的版本不认，原生提问 UI 会照常弹出，不会卡死。
> 想改用「deny + 把答案写进原因」的社区方案，设 `POMODORO_ASK_MODE=deny`。

## OpenCode

OpenCode 走插件。执行 `install --agent opencode` 会：

1. 把插件复制到 `~/.config/opencode/plugins/pomodoro-opencode.ts`；
2. 在 `~/.config/opencode/opencode.json` 里加上 `"plugin": ["file://..."]`。

插件做三件事：

| OpenCode 钩子 | 行为 |
|---|---|
| `permission.ask` | 调番茄钟弹权限窗 → 回写 `output.status`（allow/deny；没决策就保持 ask 交给 TUI） |
| `question.asked` | 调番茄钟弹提问窗 → `POST /question/{id}/reply` 回传 `answers`（string[][]）；取消则 reject |
| `session.idle` / `session.error` | 上报 `stop` / `notification` |

插件本身不含业务逻辑，全部转发给 `pomodoro-hook.js`：

```
opencode-permission   # stdin {permission} → stdout {status}
opencode-question     # stdin {questions, sessionID, requestID, serverUrl} → stdout {answers} / {reject}
opencode-event        # stdin {event, properties} → 计数/通知
```

回传答案优先用插件拿到的 SDK client；拿不到时退回 HTTP，依次尝试
`POST /session/{sessionID}/question/reply` 与 `POST /question/{requestID}/reply`。
服务端地址可用 `OPENCODE_SERVER_URL`（或 `OPENCODE_PORT`）指定，默认 `http://127.0.0.1:4096`。

## HTTP API（curl / 任意脚本）

发现文件 `%APPDATA%\番茄钟\gateway.json` 含 `port` 与 `token`。除 `/health` 外均需 `Authorization: Bearer <token>`。

```bash
# 提问弹窗（长轮询，返回用户选的答案）
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"kind":"ask","source":"manual","title":"选个方案","questions":[{"id":"q0","question":"用哪种？","header":"方案","multiSelect":false,"custom":true,"options":[{"id":"o0","label":"A 方案","description":"稳"},{"id":"o1","label":"B 方案"}]}],"timeoutMs":120000}' \
  http://127.0.0.1:5277/api/interaction
# → {"ok":true,"kind":"ask","action":"submit","answers":{"q0":["A 方案"]},"text":"","decidedBy":"user"}

# 权限弹窗
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"kind":"permission","source":"manual","title":"允许 Bash？","detail":"command: npm test","permission":{"tool":"Bash","rule":"npm test","canAlways":true},"timeoutMs":120000}' \
  http://127.0.0.1:5277/api/interaction
# → {"ok":true,"kind":"permission","action":"allow-always","text":"","decidedBy":"user"}

# 纯通知（立即返回，不等用户）
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"kind":"notification","title":"构建完成","message":"可以回来验收了"}' \
  http://127.0.0.1:5277/api/interaction

# 自定义按钮（旧 /api/confirm 等价）
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"kind":"custom","title":"部署到测试环境？","actions":[{"id":"ok","label":"部署","style":"primary"},{"id":"no","label":"取消","style":"danger"}],"timeoutMs":120000}' \
  http://127.0.0.1:5277/api/interaction
```

`POST /api/interaction` 字段：

| 字段 | 说明 |
|---|---|
| `kind` | `ask` / `permission` / `notification` / `custom` |
| `source` | 来源徽标：`zcode` / `claude-code` / `opencode` / `manual` |
| `title` `message` `sub` `detail` | 标题 / 正文 / 附注 / 等宽详情（工具输入，可折叠滚动） |
| `questions[]` | `ask` 用：`{id, question, header, multiSelect, custom, options:[{id,label,description}]}`，最多 4 题 × 6 选项 |
| `permission` | `permission` 用：`{tool, rule, suggestions[], canAlways}` |
| `input` | 文本输入：`{enabled, label, placeholder, required}`（权限弹窗默认开） |
| `actions[]` | `custom` / `notification` 用：`{id, label, style: primary\|danger\|default}` |
| `timeoutMs` | 等待上限（5s ~ 10min，默认 5min） |
| `defaultAction` | 超时兜底值，默认 `permission=deny`、`ask=cancel`、`custom=null` |

返回：`{ok, kind, action, answers, text, decidedBy}`，`decidedBy` 为 `user` / `timeout` / `dismissed` / `shown`。

CLI 直连模式（等价于上面的通用能力）：

```bash
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" ask --question "继续吗？" --option 继续 --option 停下
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" permission --tool Bash --detail "npm test"
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" notify --title "构建完成" --message "可以回来验收了"
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" status
```

## 事件类型（POST /api/event，只计数/通知，不等用户）

| kind | 含义 | 网关行为 |
|---|---|---|
| `permission` | 权限请求（无交互降级） | 打断计数 + 弹通知 |
| `ask` | 提问（无交互降级） | 打断计数 + 弹通知 |
| `notification` | 等待用户输入 | 打断计数 + 弹通知 |
| `stop` | 主 agent 回合结束 | 计数；专注中且无未决交互则弹「休息建议」 |
| `subagent-stop` | 子 agent 结束 | 仅计数 |
| `tool-after` | 工具调用完成 | 工具调用计数 |
| `tool-before` / `prompt` | 工具调用前 / 用户提交提示词 | 预留 |
| `session-start` / `session-end` | 会话开始/结束 | 会话计数 |

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `POMODORO_SOURCE` | 自动 | `zcode` / `claude-code` / `opencode`（install 生成的命令会自动带 `--source`） |
| `POMODORO_ASK` | 1 | 是否接管 `AskUserQuestion` |
| `POMODORO_PERMISSION` | 1 | 是否接管 `PermissionRequest` |
| `POMODORO_CONFIRM_PRETOOL` | 0 | 1 = 普通 PreToolUse 也弹双向确认（建议只对 `Bash` 这类高风险工具开） |
| `POMODORO_ALWAYS_ALLOW` | 1 | 0 隐藏「始终允许」按钮 |
| `POMODORO_PERMISSION_DEST` | projectSettings | 「始终允许」写进哪份配置 |
| `POMODORO_TIMEOUT_S` | 240 | 弹窗等待秒数（上限 590；hook 配置的 timeout 要 ≥ 它） |
| `POMODORO_ASK_MODE` | answers | `deny` = 用「拒绝 + 答案写进原因」的方式回传提问答案 |
| `POMODORO_GATEWAY_FILE` | 自动发现 | 指定 gateway.json 路径 |
| `POMODORO_PORT` / `POMODORO_TOKEN` | 自动发现 | 直接指定网关端口与 token |

## 与番茄工作法的联动

- **专注期活动统计**：工作阶段主窗口显示 `🤖 工具 N · 打断 M`；专注结束弹窗汇总本番茄内 agent 的工具调用与打断次数。
- **休息建议**：agent 跑完任务（`stop`）而你还在专注时段，弹窗建议趁机休息，一键提前进入休息（10 分钟冷却；有未决交互时不打扰）。
- **打断质量**：打断数 = 提问 + 权限次数。现在打断可以在弹窗内直接处理，代价从「切窗口」降到「点一下」。

## 安全说明

- 网关仅绑定 `127.0.0.1`，局域网不可达；
- 除 `/health` 外全部要求随机 token（每次启动重新生成，存于本机 `gateway.json`）；
- 校验 `Host` 头，仅接受 `127.0.0.1` / `localhost`，防 DNS rebinding；
- 浏览器页面因无 CORS 头也无法读取响应；
- 超时/关闭一律落回**拒绝或不决策**，永不自动放行。

## 自测

```bash
# 纯 Node 跑通全链路：弹窗模拟 + ZCode/Claude Code 协议 + OpenCode 子命令
node scripts/smoke-interaction.js

# 应用内自检（需 Electron）
POMODORO_GATEWAY_SMOKE=1 npm start

# 依次弹一遍 ask / permission / notification，并把渲染层实测尺寸打到日志
POMODORO_POPUP_DEMO=1 npm start
```

## Roadmap

- **MCP server**（二期）：agent 主动调用番茄钟——查状态、开专注、任务认领番茄；
- **专注标签**：SessionStart 时把 agent 任务绑定到当前番茄，结束弹窗展示产出；
- **批量决策记忆**：同类型权限连续放行后自动收敛为规则建议。
