# Agent 集成指南（ZCode / Claude Code / VS Code Copilot / Trae / Cursor / OpenCode / Codex / Qwen Code）

番茄钟运行时会在本地启动一个 **Agent 网关**（默认 `http://127.0.0.1:5277`，仅绑定本机回环地址）。
agent 要提问、要权限、或只是通知你一声时，番茄钟弹出 Fluent 风格桌面弹窗——**提问和权限可以直接在弹窗里作答**，不用来回切终端。

> 本文对应当前版本 **v1.1.6**。核心行为：**提问与权限都在弹窗里处理，`PreToolUse` 一概不做工具审批**
> （审批只走 `PermissionRequest`；没有该事件的宿主交给宿主自己）——详见下面的宿主支持表。

```
ZCode hook ────────────┐                                    ┌─ 提问弹窗（选项/多选/自定义回答）
Claude Code hook ──────┤                                    ├─ 权限弹窗（允许 / 始终允许 / 拒绝）
VS Code Copilot hook ──┼─HTTP→ 127.0.0.1:5277 网关 ─→ 弹窗 ─┤   （含上下文：任务 / 子 agent / 工具）
Trae hook ─────────────┤      (token 鉴权)      ↖ 长轮询   ├─ 通知弹窗（看完即走）
Cursor hook ───────────┤                        用户决策    └─ 专注期活动计数 / 远程控制
Qwen Code hook ────────┤
OpenCode 插件 ─────────┤
Codex hooks ───────────┘
```

各宿主支持到什么程度：

| 宿主 | 提问（弹窗作答） | 权限（弹窗审批） | 通知 / 计数 | 配置位置 |
|---|---|---|---|---|
| ZCode | ✅ `AskUserQuestion` | ✅ `PermissionRequest` | ✅ | `~/.zcode/cli/config.json` |
| Claude Code | ✅ `AskUserQuestion` | ✅ `PermissionRequest` | ✅ | `~/.claude/settings.json` |
| VS Code Copilot | ✅ `vscode/askQuestions` | ❌ **无权限事件** → 交回 VS Code 自己的审批设置 | ✅ `Stop` | `~/.copilot/hooks/*.json` 或 `.github/hooks/*.json` |
| Trae | ✅ `AskUserQuestion` | ❌ **无权限事件** → 交回 Trae 自己的审批设置 | ✅ `Notification` / `Stop` | `%userprofile%/.trae-cn/hooks.json` 或 `.trae/hooks.json` |
| Cursor | ✅ `preToolUse` + `updated_input` | ✅ `beforeShellExecution` / `preToolUse` / `beforeMCPExecution` | ✅ | `~/.cursor/hooks.json` 或 `.cursor/hooks.json` |
| OpenCode | ✅ `question.asked` | ✅ `permission.ask` | ✅ | 插件 + `opencode.json` |
| Codex CLI | ❌ 无提问回调 | ✅ `PermissionRequest`（Codex 的独立权限事件，**只在它本来就要问时触发**） | ✅ 12 个事件 | `~/.codex/hooks.json` 或 `~/.codex/config.toml` 的内联 `[hooks]` |
| Qwen Code | ✅ `AskUserQuestion` | ✅ `PermissionRequest` | ✅ | `~/.qwen/settings.json` |

> **`PreToolUse` 不做工具审批（2026-09-18 起）**：所有宿主的 `PreToolUse` 只用来接管**提问**
> （`AskUserQuestion` / `askQuestions` / `askQuestion`）和上报活动（`tool-before`），普通工具调用
> 一律静默、不回决策。审批统一走 `PermissionRequest`；**VS Code / Trae 没有这个事件**，所以它们的
> 命令执行、删改文件等操作**完全按宿主自己的审批设置走**，番茄钟不介入。
> 想调这两家的放行策略，请改宿主自己的设置（VS Code 的 `chat.tools.*`、Trae 的 `AI.toolcall.v2.*`）。

> 任何「Claude Code 兼容格式」的宿主（iFlow、Trae、CodeBuddy、Copilot CLI 等）都能直接用，把 `pomodoro-hook.js` 当成 hook command 填进去即可。

## 三种弹窗

| kind | 触发场景 | 弹窗里能做什么 | 返回给 agent |
|---|---|---|---|
| `ask` | `AskUserQuestion` / OpenCode `question.asked` | 选选项（单选即时提交、多选确认后提交）、填自定义回答、`Enter` 提交 `Esc` 取消 | `answers` |
| `permission` | `PermissionRequest`（Claude / ZCode / Qwen / Codex）/ Cursor 的 `beforeShellExecution`·`beforeMCPExecution`·`preToolUse` / OpenCode `permission.ask` | 允许一次 / 始终允许 / 拒绝，可填备注（会作为拒绝理由回传） | `allow` / `allow-always` / `deny` |
| `notification` | `Notification`、`session.error` 等 | 只展示，几秒后自动消失 | — |

**兜底策略（重要）**：

- 超时未决策 → 网关回该 kind 的安全默认值（`permission`=**deny**、`ask`=**cancel**），
  但 hook CLI 只认 `decidedBy: "user"`，所以这层默认值**不会被当成你的决定用掉**，
  实际表现是「不输出决策」→ agent 回退到**终端原生询问**，绝不替你放行；
- 你手动关掉弹窗（右上角 ×）→ 空决策，同样回退终端原生询问；
- 弹窗被新弹窗顶掉 → 空决策，同样回退终端；
- 番茄钟没启动 → hook 静默退出，绝不阻断 agent。

无论关窗还是等超时，agent 都不会卡死：拿不到决策它就去问终端。

## 两种「临时关闭」：交给终端 / 暂时收起

弹窗上有两条不同的退路，别混：

| 操作 | 语义 | agent 那边发生什么 |
|---|---|---|
| 右上角 **×** | **交给终端**：这次不在弹窗里答了 | 拿到空决策 → 回退**宿主原生询问**（终端里再问你一次） |
| 底部 **暂时收起，稍后处理** | **挂起**：这次交互还活着，只是把窗口收起来 | HTTP 长轮询**继续挂着**，agent 保持等待 |

收起之后怎么找回来（三个入口，随便哪个都行）：

1. **主窗口的确认提示条**——计时环下方会出现一条「权限 · Bash」样式的胶囊，点它就把那条重新弹出来；
   收起多条时显示「N 条确认待处理」，点一次弹一条（最早收起的先回来）。
2. **托盘左键单击**——有待处理确认时单击托盘图标 = 唤回最早收起的那条（没有待处理时单击不做任何事）。
3. **托盘右键 →「待处理的确认（N）」**——收起多条时菜单里会多一个「选择要处理的…」子菜单，
   托盘 tooltip 也会显示待处理条数。

> 之所以做成三个入口：收起后弹窗是关闭状态，只留托盘一条路时，托盘图标一旦被 Win11
> 折叠进溢出区就"找不着了"（等于丢了这次确认）。

挂起**不影响兜底计时**——收起不等于有人管了，到点照样按上面的兜底策略收尾。

## 三层超时：谁先到点，谁决定结局

等待链路是三层嵌套的，**顺序不能乱**：

| 层 | 默认值 | 到点后果 |
|---|---|---|
| 番茄钟兜底（网关） | `POMODORO_TIMEOUT_S` = **3600s** | 回空决策 → agent 回退终端原生询问（后果确定） |
| hook 等网关（HTTP） | 兜底 + 300s = **3900s** | 请求异常退出（不该发生，只是保险） |
| **宿主 hook timeout** | **4200s**（装 hook 时自动写进配置） | 宿主**直接杀掉 hook 进程**，空决策那句根本发不出去 → 宿主按**自己的审批设置**走 |

**为什么宿主那层必须最大**：宿主掐掉 hook 时，hook 没机会回「未决策」，
于是宿主按自己的审批设置处理 —— 你要是把某个工具设成了免确认，就等于**静默放行**。
所以三层必须满足 `宿主 > hook 等网关 > 番茄钟兜底`，让番茄钟永远先到点。

> 手写 hook 配置时最容易漏这个：VS Code 的 `timeout` 默认只有 **30 秒**，
> 30 秒后宿主掐掉 hook，弹窗白弹。装 hook 时会自动写 `4200`；
> 手动改过配置的话记得对齐。

## 弹窗上的上下文：哪个 agent、哪个任务、在动哪个工具

每个 hook 都是独立进程，所以 hook CLI 会把当前会话的上下文**按会话累积**在 `%TEMP%/pomodoro-hook-cache/session-*.json`（TTL 12 小时），随每次弹窗一起下发：

| 弹窗上显示 | 来源 | 示例 |
|---|---|---|
| 来源徽标 | `--source` / 自动识别 | `ZCode` / `Claude Code` / `VS Code Copilot` / `Trae` / `Cursor` / `Codex` / `Qwen Code` / `OpenCode` |
| 子 agent 徽标 | `agent_type`（Claude Code）/ `agent`（OpenCode）/ `SessionStart` 的 agent 信息 | `implementation-agent` |
| 任务行（最多两行） | `UserPromptSubmit` 的 `prompt` | 把 agent 网关的弹窗改成可交互的 |
| 工具 | 当前请求的 `tool_name`；通知类事件退化用「最近一次工具」 | `Bash` |
| 工具详情（等宽单行） | 从 `tool_input` 里挑代表性字段（`command` / `file_path` / `pattern`…） | `npm test` |
| 项目 | hook 入参 `cwd` 的目录名（OpenCode 用 `directory`） | `pomodoro-fluent` |
| 会话 | `session_id` 尾 6 位，避免刷一长串 uuid | `a1b2c3` |

- **通知类事件（`Notification` / `Stop` / `session.error`）也会带上下文**，所以「休息建议」和异常通知同样能看出是哪个任务在跑。
- 拿不到的字段直接不显示，不会出现空标签。
- 想核对 hook 到底跟踪到了什么，跑：

  ```bash
  node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" sessions
  # [{"session":"a1b2c3","source":"zcode","project":"pomodoro-fluent",
  #   "task":"把 agent 网关的弹窗改成可交互的","lastTool":"Bash",
  #   "lastToolDetail":"npm test","agentType":""}]
  ```

- 也可以在 HTTP 请求里直接传 `context`（字段同上），弹窗会照原样渲染：

  ```bash
  curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
    -d '{"kind":"permission","context":{"agent":"ci","project":"pomodoro-fluent","task":"发布 v1.0.2","tool":"Bash","toolDetail":"npm run pack","session":"9f8e7d"},"permission":{"tool":"Bash"}}' \
    http://127.0.0.1:5277/api/interaction
  ```

## 开启与关闭

两种方式，改的是同一份状态：

- **标题栏右上角的网关图标**（🔌 插头）——一键开关，绿点 = 运行中，鼠标悬停显示 `127.0.0.1:端口`，点一下即停用/启用；
- 设置抽屉（主窗口齿轮）→「Agent 集成」→ 启用 Agent 网关。

默认开启，状态与端口持久化在 `userData/config.json`。停用后本地端口立即释放，
hook 侧的上报会静默跳过（不会阻断 agent 工作）；想恢复弹窗再点一次即可，无需重启应用。

番茄钟启动时会自动把脚本安装到固定路径（与仓库/安装位置解耦）：

```
%APPDATA%\番茄钟\hook\pomodoro-hook.js
%APPDATA%\番茄钟\hook\opencode\pomodoro-opencode.ts
```

> 路径跟的是应用的 userData（打包版是 `%APPDATA%\番茄钟`）；找不到时会回退读
> `%APPDATA%\pomodoro-fluent\`（开发版/旧目录名）。`gateway.json` 同理，两处都会找。
> 实际用的是哪一份，看安装面板给出的路径最准。

## 一键接入（推荐）

**方式一：设置面板点「一键安装」**（推荐，不用碰终端）

设置抽屉 → 选好 agent → 点 **一键安装**。应用会直接把配置写好，并在面板上告诉你写了哪些文件。
旁边「清理旧条目」默认勾选，等价于给命令加 `--clean`（清掉指向 hook 脚本**其它副本**的旧条目，
避免同一次工具调用跑两遍）。

- 安装成功：列出写入的配置文件 + 注意事项（比如 Trae 要选「本地自动运行」）→ 重启对应 agent 生效；
- 安装失败：面板直接给出**原因**（写盘失败 / 权限 / 路径被占用）和**等价的命令行**，
  点「复制命令」就能自己到终端执行，不用回来找；
- 装完但本机没有 `node`：会明确警告 —— 配置装上了没错，但 hook 是运行时用
  `node "<脚本>"` 拉起的，缺了它 agent 那边不会弹窗。

实现上，主进程用 **Electron 自带的 Node**（`ELECTRON_RUN_AS_NODE=1`）执行 hook CLI，
所以本机没装 node 也能把配置写进去；配置里写的仍是 `node "..."`，与手动安装完全一致。

**方式二：复制命令自己执行**

点「复制安装命令」粘到终端。等价命令：

```bash
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent zcode
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent claude
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent vscode
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent trae
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent cursor
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent opencode
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent codex
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent qwen
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" install --agent all
```

- 会**自动合并**进对应配置文件，原文件备份为 `*.pomodoro.bak`；
- 已存在相同条目不会重复写入；
- 加 `--print` 只打印将要写入的内容，不动文件；
- 加 `--clean` 会清掉指向番茄钟 hook 其它副本的旧条目（避免同一个事件弹两次窗）；
- 加 `--with-notify`（仅 Codex）才会额外改写 `~/.codex/config.toml` 的 `notify` —— 默认不动它，
  因为那个键可能已被你指向别的工具（例如 codex-computer-use）。

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
      ],
      "PostToolUseFailure": [
        { "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source zcode" }] }
      ]
    }
  }
}
```

- `PreToolUse` 的 matcher 只收 `AskUserQuestion` —— 2026-09-18 起不做工具审批，普通工具调用靠
  `PostToolUse` / `PostToolUseFailure` 上报活动，不弹窗、不回决策。

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
    "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code", "timeout": 4200 }] }],
    "PreToolUse": [{ "matcher": "AskUserQuestion|askQuestions|askQuestion", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code", "timeout": 4200 }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source claude-code" }] }]
  }
}
```

- `PreToolUse` 的 matcher 只匹配提问工具（带 `timeout: 4200`，因为它要等你点弹窗）；
  普通工具调用不弹窗、不回决策，活动上报走 `PostToolUse`（2026-09-18 起不做工具审批）。
- `UserPromptSubmit` 是唯一携带「用户这次让 agent 干什么」的事件，弹窗的**任务行**就来自它
  （`payload.prompt`）—— 漏装这一条，任务名永远是空的。

权限决策输出（Claude Code 的 `PermissionRequest` 用 `decision` 对象）：

```json
{ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": { "behavior": "allow", "message": "用户通过番茄钟弹窗允许" } } }
```

> 提问（AskUserQuestion）说明：Claude Code 只在 `-p` 非交互模式官方支持 `defer` 回传答案。
> 交互模式下本 hook 默认仍走 `updatedInput.answers` 注入；若你的版本不认，原生提问 UI 会照常弹出，不会卡死。
> 想改用「deny + 把答案写进原因」的社区方案，设 `POMODORO_ASK_MODE=deny`。

## VS Code Copilot（当前稳定版 1.137，Agent hooks Preview）

> 版本基线：workspace hooks 更早就有了，**agent-scoped hooks 是 1.111（2026-03）** 加的，
> 当前稳定版是 **1.137（2026-09-09）**。下面这些字段名/事件名都是按官方
> [Agent hooks](https://code.visualstudio.com/docs/agent-customization/hooks) 与
> [Hooks reference](https://code.visualstudio.com/docs/agents/reference/hooks-reference) 对齐的。

好消息：**VS Code 的 hooks 与 Claude Code 同格式**（PascalCase 事件名 + 同形 `hookSpecificOutput`），
所以适配层基本复用，不需要额外插件。

- 配置位置：用户级 `~/.copilot/hooks/*.json`（`install --agent vscode` 会写这里），
  工作区级 `.github/hooks/*.json`。加载范围可用 `chat.hookFilesLocations` 调；
  如果 `~/.copilot/hooks` 没被加载，在设置里加一条 `"~/.copilot/hooks": true`。
- 事件集只有 8 个：`SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` /
  `PreCompact` / `SubagentStart` / `SubagentStop` / `Stop`。
  ⚠️ **没有 `PermissionRequest`，也没有 `Notification`** —— 提问只能挂 `PreToolUse`，
  "任务完成通知"只能挂 `Stop`。
  ⚠️ 也正因为没有权限事件，**番茄钟不做任何工具审批**：VS Code 里的命令执行、删改文件
  完全按 VS Code 自己的审批设置走（`chat.tools.*` / `chat.permissions.default`）。
- ⚠️ **VS Code 会忽略 matcher**（官方原话："Currently, VS Code ignores matcher values"），
  所有 hook 在每次工具调用时都会跑。PreToolUse 里非提问工具的调用只会静默上报活动，
  不会弹审批窗。
- ⚠️ **工具名和 Claude Code 完全不同**：VS Code 官方是 `run_in_terminal` / `create_file` /
  `replace_string_in_file` 这类**下划线**命名，工具入参也是 **camelCase**（`tool_input.filePath`），
  而 Claude Code 是 `Write` / `Bash` + `snake_case`（`tool_input.file_path`）。

返回值与语义（`hookSpecificOutput`）：

| 场景 | 返回 | 说明 |
|---|---|---|
| 提问你点了选项 | `permissionDecision: "deny"` + `permissionDecisionReason` / `additionalContext` | 答案写给模型，模型据此继续 |
| 提问超时 / 关窗 | 不返回决策 | 交回 VS Code 原生 QuickPick |
| 普通工具调用 | 不返回决策 | 只静默上报活动，不弹审批窗（2026-09-18 起审批已移除） |
| `Stop` | 什么都不返回 | **绝不能返回 `decision: "block"`**，那会阻止 agent 收尾（VS Code 的 `stop_hook_active` 就是防这个自循环的） |

**提问弹窗（`vscode/askQuestions`）**：与 Claude Code 不同，VS Code 的提问工具弹的是 QuickPick，
**答案不在入参里**，所以 `updatedInput` 改不动用户选择（它只换问题本身）。这里默认走
**`deny` + 把答案写进 `permissionDecisionReason` 与 `additionalContext`**——后者才是"给模型看"的字段，
模型据此继续。想换回注入式可以设 `POMODORO_ASK_MODE=answers`（但 VS Code 下不生效）。

**没有「始终允许」这回事**：VS Code 走不到审批通道（没有 `PermissionRequest`，`PreToolUse`
只处理提问），所以番茄钟的「允许 / 始终允许 / 拒绝」在这家**根本不会出现**。文件与命令的
放行策略请直接在 VS Code 设置里配（`chat.tools.global.autoApprove`、`chat.permissions.default`、
`chat.tools.terminal.autoApprove` 等），番茄钟一概不介入、也不会替它 auto-approve。

**超时要留够**：VS Code 的 hook 默认 `timeout` 只有 **30 秒**（单位是秒），而**提问**要等你点
弹窗 —— 所以 `install --agent vscode` 会把 `PreToolUse` 的 `timeout` 显式设成 `4200`。
手写配置时别漏了这个字段，否则 30 秒后宿主直接掐掉 hook，提问弹窗白弹（用户的选择传不回去）。
其余只做上报的事件用默认 `30` 秒就够。

**已在 Claude Code 配过的机器**：VS Code 默认也会读 `~/.claude/settings.json`，
同一次工具调用会被跑两遍（等于两套 hooks 叠加，多起一个 hook 进程）。两边都装了番茄钟时
**建议二选一**：要么只用 Claude Code 那份，要么在 VS Code 设置里关掉：

```jsonc
"chat.hookFilesLocations": { "~/.claude/settings.json": false }
```

**其它必知**：组织可能用企业策略禁用 hooks（需找管理员）；退出码 `2` = 阻断并把
stderr 给模型看，所以 CLI 恒以 0 退出；`Developer: Show Agent Debug Logs` 能看到
hook 是否执行、以及 `Load Hooks` 日志里各 hook 是从哪个文件加载的。

```json
{
  "version": 1,
  "hooks": {
    "PreToolUse": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 4200 }],
    "PostToolUse": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }],
    "SessionStart": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }],
    "UserPromptSubmit": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }],
    "SubagentStart": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }],
    "SubagentStop": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }],
    "PreCompact": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }],
    "Stop": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source vscode", "timeout": 30 }]
  }
}
```

## Trae（TraeCode，字节）

Trae 的 hook 体系基本照 Claude Code 那套做的，**配置是同样的嵌套结构**，所以适配很直接；
但事件集只有 6 个，和 VS Code 一样**没有 `PermissionRequest`** —— 因此**番茄钟不做工具审批**，
Trae 里的命令执行、删改文件按 Trae 自己的设置走（`AI.toolcall.v2.*`）：

| 事件 | 能做什么 | 我们的用法 |
|---|---|---|
| `SessionStart` | 注入上下文（`additionalContext`） | 记会话开始 |
| `UserPromptSubmit` | `decision: block` 拦截 / 附上下文 | 记任务提示词 |
| `PreToolUse` | **`permissionDecision` + `updatedInput`** | **只用来接提问**（`AskUserQuestion`）；工具审批不用，交给 Trae 自己的设置 |
| `PostToolUse` | `decision: block` 校验结果 | 工具计数 |
| `Stop` | `decision: block` 阻止收尾（可配 `loop_limit`） | 只上报，**不阻断** |
| `Notification` | 异步通知，**不改变流程** | 弹通知窗（含 `permission_prompt` / `idle_prompt`） |

- 配置位置：全局 `%userprofile%/.trae-cn/hooks.json`（`install --agent trae` 会写这里），
  或界面里 设置 > Hooks 创建；项目级 `$PROJECT/.trae/hooks.json`。
- `timeout` 单位是**秒**、默认 30 → `PreToolUse` 显式设 4200，否则等你点提问弹窗的工夫它就被掐了。
- **`matcher` 在 Trae 上是真生效的**（仅限 `PreToolUse` / `PostToolUse` / `Notification`）。
  2026-09-18 起不做工具审批，PreToolUse 的 matcher 已收窄为 `AskUserQuestion`（只处理提问），
  活动上报交给 `PostToolUse`。
- Trae 的工具名：`Read` `Write` `Edit` `Glob` `Grep` `LS` **`RunCommand`** `WebSearch`
  `WebFetch` `AskUserQuestion` `Skill` `mcp__<server>__<tool>`。
- 输入字段是 **snake_case**：`session_id` `cwd` `hook_event_name` `workspace_roots`
  `tool_use_id` `tool_name` `llm_tool_name` `tool_input`；Stop 多一个 `stop_hook_active`，
  Notification 多 `notification_type` / `message`。上下文里的项目名走 `workspace_roots[0]`。
- 提问未决策（超时 / 关窗）时不返回决策，交回 Trae 原生流程；
  普通工具调用一律静默上报，不弹审批窗（2026-09-18 起审批已移除）。
- ⚠️ **必须选「本地自动运行」**：Trae 创建 Hook 时会让你在「沙箱运行」和「本地自动运行」
  之间选。沙箱会限制系统权限，hook 很可能连不上本机 `127.0.0.1:5277` 的番茄钟网关；
  连不上时 CLI 是**静默跳过**的（不阻断 agent），表现就是"配了但没弹窗"。
- ⚠️ **Trae 会合并 Claude Code 的 hook 配置**（官方原话："若同时启用 Claude Code Hook 和
  TraeCode Hook，TraeCode 会读取所有已启用的 Hook 配置并合并执行"）。所以
  `~/.claude/settings.json` 里也有番茄钟 hook 的话，同一次工具调用会跑两遍 →
  用 `install --agent trae --clean` 收敛，或二选一。
- 看日志：Trae 的「运行日志 > 查看日志」里有 **Agent Hooks** 记录（退出后清空）。

```json
{
  "version": 1,
  "hooks": {
    "PreToolUse": [{
      "matcher": "AskUserQuestion",
      "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source trae", "timeout": 4200 }]
    }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source trae", "timeout": 30 }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source trae", "timeout": 30 }] }],
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source trae", "timeout": 30 }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source trae", "timeout": 30 }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source trae", "timeout": 30 }] }]
  }
}
```

> 提问答案的通道：Trae 的提问工具叫 `AskUserQuestion`（和 Claude Code 同名），
> 所以默认按 `updatedInput.answers` 注入。若你的 Trae 版本不认（原生提问 UI 照旧弹出），
> 设 `POMODORO_ASK_MODE=deny` 切到「拒绝 + 答案写进原因」的通道 —— 两条路都不会卡死 agent。

## Cursor

配置在 `~/.cursor/hooks.json`（用户级）或 `.cursor/hooks.json`（项目级，可提交进仓库）。事件名是 camelCase，响应字段是 **snake_case**，和 Claude Code 不一样，所以 hook CLI 里有独立适配：

| Cursor 事件 | 行为 | 弹窗返回 |
|---|---|---|
| `beforeShellExecution` | 拦截 shell 命令，弹权限窗（标题「允许执行命令？」，正文是命令） | `{permission: allow\|deny\|ask, user_message, agent_message}` |
| `preToolUse` | 提问工具 → 弹提问窗并 `updated_input.answers` 注入；其他工具 → 弹权限窗 | 同上，提问时带 `updated_input` |
| `beforeMCPExecution` | MCP 工具调用审批 | 同上 |
| `beforeReadFile` | 读敏感文件审批（Cursor 只认 `permission`，不接受消息字段） | `{permission}` |
| `beforeSubmitPrompt` | 记录任务提示词（用于弹窗上下文） | `{continue: true}` |
| `afterFileEdit` / `afterShellExecution` / `afterMCPExecution` / `postToolUse` | 工具调用计数 | 不回决策 |
| `afterAgentResponse` / `afterAgentThought` | 记录最后回复 / 思考（上下文用） | 不回决策 |
| `stop` | 回合结束 → 计数 + 可能弹「休息建议」 | `{}`（**不返回 `followup_message`**，避免把 agent 拖进循环） |
| `sessionStart` / `sessionEnd` / `subagentStart` / `subagentStop` / `preCompact` | 会话与子 agent 计数 | 不回决策 |

> **Cursor 是例外，仍然有权限弹窗。** 上面那条「`PreToolUse` 不做审批」说的是 Claude 风格的
> PascalCase `PreToolUse` 事件；Cursor 用的是自己的 camelCase 事件，且**原生就有专门的三条
> 审批通道**（`beforeShellExecution` / `beforeMCPExecution` / `preToolUse`），所以 Cursor 下的
> 「允许 / 始终允许 / 拒绝」照常工作。

超时或被关窗时返回 `permission: "ask"`，交回 Cursor 原生确认（Cursor 原生支持 ask，比硬拒更友好）；只有你**明确点了拒绝**才会 deny。

两个细节（避免踩坑）：

- `preToolUse` 上的 `ask` **官方接受但不强制**——Cursor 只在 `beforeShellExecution` / `beforeMCPExecution`
  上完整兑现 `allow / deny / ask` 三态。所以 `preToolUse` 上的 `ask` 等价于「不做决策」，
  会落回 Cursor 自己的审批流程；结果仍然是**不会静默放行**，只是提示语由 Cursor 出。
- 「始终允许」：Cursor 的 `preToolUse` 不能回写规则，所以改记进番茄钟本地规则缓存
  （`%TEMP%\pomodoro-hook-cache\always-allow.json`，默认 30 天），下次同工具同命令直接放行、
  不再弹窗。关掉：`POMODORO_LOCAL_ALWAYS_ALLOW=0`。
  另外 `POMODORO_PERMISSION=0` 关闭接管时返回 `{}`（不做决策），**不会**返回 `permission: "allow"`——
  以前那样写等于替用户强制放行，属于越权。

```json
{
  "version": 1,
  "hooks": {
    "beforeShellExecution": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor", "timeout": 4200 }],
    "preToolUse": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor", "timeout": 4200 }],
    "beforeMCPExecution": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor", "timeout": 4200 }],
    "beforeSubmitPrompt": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor" }],
    "afterFileEdit": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor" }],
    "afterShellExecution": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor" }],
    "afterAgentResponse": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor" }],
    "stop": [{ "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source cursor" }]
  }
}
```

## Codex CLI

Codex（`codex-cli` 0.154+）有完整的 hooks 体系，**12 个事件**：

```
PreToolUse  PermissionRequest  PostToolUse
SessionStart  SessionEnd  UserPromptSubmit
SubagentStart  SubagentStop  Stop  Interrupt
PreCompact  PostCompact
```

配置写 `~/.codex/hooks.json`（用户级；项目级 `.codex/hooks.json` 只在项目被信任后才加载）：

```json
{
  "hooks": {
    "PermissionRequest": [
      { "hooks": [{ "type": "command", "command": "node \"...\\pomodoro-hook.js\" --source codex", "timeout": 4200 }] }
    ],
    "PreToolUse": [
      { "matcher": "Bash|apply_patch|Edit|Write|mcp__.*",
        "hooks": [{ "type": "command", "command": "node \"...\\pomodoro-hook.js\" --source codex", "timeout": 30 }] }
    ]
  }
}
```

`install --agent codex` 会把这 12 个事件全部写好。

### 三个必须知道的差异

**1）审批只在 `PermissionRequest` 上做，不在 `PreToolUse` 上做。**

Codex 的 `PreToolUse` 只强制执行 `permissionDecision: "deny"`（而且必须带非空
`permissionDecisionReason`）；`"allow"` 和 `"ask"` 都只是**被解析、不生效**：

```
PreToolUse hook returned unsupported permissionDecision:allow
PreToolUse hook returned unsupported permissionDecision:ask
```

所以在 `PreToolUse` 上弹窗是错的 —— 你点「允许」根本传不回去，Codex 会照自己的审批
流程走，而那条流程又会触发 `PermissionRequest` → **弹两次窗**。番茄钟对 Codex 的
`PreToolUse` 只上报活动、绝不回决策。

> 顺便说明：2026-09-18 起这条已经是**全宿主**的约定 —— 没有哪个宿主在 `PreToolUse` 上被
> 番茄钟做审批。Codex 只是当年第一个暴露这个问题的宿主。

好在 Codex 的 `PermissionRequest` **只在「Codex 本来就要问用户」时才触发**（不需要审批的
调用不跑）—— Codex 自己已经判断过要不要问了，不需要额外配置。

**2）`updatedPermissions` 在 Codex 上会让整条答复失败。**

```
PermissionRequest hook returned unsupported updatedPermissions
PermissionRequest hook returned unsupported updatedInput
```

`updatedInput` / `updatedPermissions` / `interrupt` 都留给未来版本，当前遇到就是
**fail closed**。所以给 Codex 的答复里只放 `behavior` + `message`；
「始终允许」靠番茄钟本地规则落盘（`%APPDATA%/pomodoro-fluent/hook-cache/always-allow.json`）。

**3）matcher 是真生效的正则，但不是所有事件都支持。**

`PreToolUse` / `PostToolUse` / `PermissionRequest` / `SessionStart` / `SessionEnd` /
`SubagentStart` / `SubagentStop` / `PreCompact` / `PostCompact` 支持 `matcher`；
**`UserPromptSubmit` 与 `Stop` 不支持（写了会被忽略）**，所以那两个事件不写 matcher。

工具名统一为 `Bash`（含 unified exec）、`apply_patch`（也可用 `Edit` / `Write` 匹配）、
`mcp__server__tool`、以及其它本地函数工具名（如 `update_plan`、`spawn_agent`）。

### 其它提醒

- `[features] hooks = false` 会关掉全部 hooks；安装时会检测并告警。
- 同一层里 `hooks.json` 与 `config.toml` 的内联 `[hooks]` **同时存在会两条都加载并告警**，
  建议二选一（`install` 只写 `hooks.json`）。
- `install --agent codex` **默认不改写 `config.toml` 的 `notify`**（那可能已被你指向
  codex-computer-use 之类的工具）。需要老的回合结束回调时加 `--with-notify`。
- Codex 的 `Stop` / `SubagentStop` 支持 `decision: "block"` 让 agent 继续跑；
  番茄钟**绝不**返回它（那会把 agent 拖进自动续跑）。

## Qwen Code

Qwen Code 是 Claude Code 的兼容分支，hook 配置在 `~/.qwen/settings.json`，格式与 Claude Code 一致（`install --agent qwen` 直接写好）：

```json
{
  "hooks": {
    "Notification": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen" }] }],
    "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen", "timeout": 4200 }] }],
    "PreToolUse": [{ "matcher": "AskUserQuestion|askQuestions|askQuestion", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen", "timeout": 4200 }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen" }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen" }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "node \"%APPDATA%\\番茄钟\\hook\\pomodoro-hook.js\" --source qwen" }] }]
  }
}
```

与 Claude Code 完全同形：提问走 `updatedInput.answers`，审批走 `PermissionRequest` 的
`decision.behavior`，`PreToolUse` 只用来接提问。

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
| `source` | 来源徽标：`zcode` / `claude-code` / `vscode` / `trae` / `cursor` / `opencode` / `codex` / `qwen` / `manual` |
| `context` | 上下文：`{agent, agentType, agentId, session, project, task, tool, toolDetail}`，弹窗按空隐藏 |
| `title` `message` `sub` `detail` | 标题 / 正文 / 附注 / 等宽详情（工具输入，可折叠滚动） |
| `questions[]` | `ask` 用：`{id, question, header, multiSelect, custom, options:[{id,label,description}]}`，最多 4 题 × 6 选项 |
| `permission` | `permission` 用：`{tool, rule, suggestions[], canAlways}` |
| `input` | 文本输入：`{enabled, label, placeholder, required}`（权限弹窗默认开） |
| `actions[]` | `custom` / `notification` 用：`{id, label, style: primary\|danger\|default}` |
| `timeoutMs` | 兜底等待上限（5s ~ 2h，默认 **1h**）。到点回安全默认值，见下 |
| `defaultAction` | 超时兜底值，默认 `permission=deny`、`ask=cancel`、`custom=null`。**hook CLI 只认 `decidedBy:"user"`，不会拿它当你的决定用**，所以默认值只对直连 API 的调用方有意义 |

返回：`{ok, kind, action, answers, text, decidedBy}`，`decidedBy` 为 `user` / `timeout` / `dismissed` / `shown`。

弹窗上的「暂时收起」**不会**让这个请求提前返回 —— 它只是把窗口收起来，长轮询继续挂着，
直到用户从托盘唤回作答、或到达兜底上限。详见「两种「临时关闭」」一节。

CLI 直连模式（等价于上面的通用能力）：

```bash
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" ask --question "继续吗？" --option 继续 --option 停下 --task "重构缓存层" --agent zcode
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" permission --tool Bash --detail "npm test" --task "修登录超时" --agent claude-code
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" notify --title "构建完成" --message "可以回来验收了"
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" status
node "%APPDATA%\番茄钟\hook\pomodoro-hook.js" sessions
```

（`ask` / `permission` 加 `--task` `--agent` `--agent-type` `--project` 可以模拟上下文，方便调弹窗样式。）

## 事件类型（POST /api/event，只计数/通知，不等用户）

| kind | 含义 | 网关行为 |
|---|---|---|
| `permission` | 权限请求（无交互降级） | 打断计数 + 弹通知 |
| `ask` | 提问（无交互降级） | 打断计数 + 弹通知 |
| `notification` | 等待用户输入 | 打断计数 + 弹通知 |
| `stop` | 主 agent 回合结束 | 计数；专注中且无未决交互则弹「休息建议」 |
| `subagent-start` / `subagent-stop` | 子 agent 起止 | 仅记上下文（子 agent 名称） |
| `tool-after` | 工具调用完成 | 工具调用计数 |
| `tool-before` / `prompt` / `pre-compact` | 工具调用前 / 用户提交提示词 / 上下文压缩前 | 预留（只记上下文） |
| `session-start` / `session-end` | 会话开始/结束 | 会话计数 |

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `POMODORO_SOURCE` | 自动 | `zcode` / `claude-code` / `opencode` / `vscode` / `cursor` / `codex` / `qwen`（install 生成的命令会自动带 `--source`） |
| `POMODORO_ASK` | 1 | 是否接管 `AskUserQuestion` / `askQuestions` |
| `POMODORO_PERMISSION` | 1 | 是否接管权限请求 |
| `POMODORO_LOCAL_ALWAYS_ALLOW` | 1 | 0 关闭本地「始终允许」规则缓存（Cursor / Codex 不支持规则回写，靠它落地） |
| `POMODORO_ALWAYS_ALLOW` | 1 | 0 隐藏「始终允许」按钮 |
| `POMODORO_PERMISSION_DEST` | projectSettings | 「始终允许」写进哪份配置 |
| `POMODORO_TIMEOUT_S` | 3600 | 番茄钟兜底等待秒数（上限 7200）。宿主的 hook timeout 必须比它大——装了 hook 会自动写 4200s |
| `POMODORO_ASK_MODE` | answers（VS Code 为 deny） | `deny` = 用「拒绝 + 答案写进原因」的方式回传提问答案 |
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
- 超时/关闭一律落回**拒绝或不决策**，绝不替宿主放行。注意「不决策」：
  提问超时就交回宿主原生 UI，普通工具调用本就静默上报 —— 结果都由宿主自己的
  审批设置决定，CLI 不会替它说 allow；

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
