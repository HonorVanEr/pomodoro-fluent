#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 番茄钟 Agent Hook CLI
//
// 与运行中的番茄钟（本地 Agent 网关）联动，把 agent 的提问 / 权限请求 / 通知
// 变成可在弹窗内直接交互的桌面弹窗。番茄钟未运行时静默退出，绝不阻断 agent。
//
// 适配的三类协议：
//   ZCode        ~/.zcode/cli/config.json  → hooks.events.*（需 hooks.enabled: true）
//   Claude Code  ~/.claude/settings.json   → hooks.*
//   OpenCode     插件（permission.ask / question.asked），插件反过来调本 CLI
//
// 用法：
//   hook 模式（默认，读 stdin JSON，自动识别协议）：
//     node pomodoro-hook.js
//   OpenCode 插件调用：
//     node pomodoro-hook.js opencode-permission   # stdin {permission} → stdout {status}
//     node pomodoro-hook.js opencode-question     # stdin {questions} → stdout {answers}
//     node pomodoro-hook.js opencode-event        # stdin {kind} → 计数/通知
//   手动调试：
//     node pomodoro-hook.js ask --question "继续吗？" --option 继续 --option 停下
//     node pomodoro-hook.js permission --tool Bash --detail "npm test"
//     node pomodoro-hook.js notify --title "标题" --message "内容" [--sub "附注"]
//     node pomodoro-hook.js status
//     node pomodoro-hook.js install --agent zcode|claude|opencode|all [--print] [--clean]
//
// 环境变量：
//   POMODORO_GATEWAY_FILE      指定 gateway.json 路径
//   POMODORO_PORT/TOKEN        直接指定网关端口与 token
//   POMODORO_SOURCE            来源标记：zcode | claude-code | opencode | vscode | trae | cursor | codex | qwen
//   POMODORO_ASK               1/0，AskUserQuestion 是否接管（默认 1）
//   POMODORO_PERMISSION        1/0，PermissionRequest 是否接管（默认 1）
//   POMODORO_LOCAL_ALWAYS_ALLOW 0 关闭本地「始终允许」规则缓存
//                              （VS Code / Trae / Cursor 不支持规则回写，靠它落地，默认 1）
//   POMODORO_ALWAYS_ALLOW      0 隐藏「始终允许」按钮（默认 1）
//   POMODORO_PERMISSION_DEST   始终允许写哪里（默认 projectSettings）
//   POMODORO_TIMEOUT_S         弹窗等待秒数（默认 240，上限 590）
//   POMODORO_ASK_MODE          answers | deny（默认 answers；VS Code 默认 deny，
//                              因为它的提问工具是 QuickPick，答案不在入参里）
//   POMODORO_HOOK_PATH         OpenCode 插件定位本脚本用
// ---------------------------------------------------------------------------

const http = require('http');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');

const HOOK_VERSION = '2.1.0';

const env = process.env;
const flag = (name, def) => {
  const v = env[name];
  if (v === undefined || v === '') return def;
  return /^(1|true|yes|on)$/i.test(v) ? true : /^(0|false|no|off)$/i.test(v) ? false : v;
};

// ---- 网关发现 ----
function findGateway() {
  const port = Number(env.POMODORO_PORT) || 0;
  const token = env.POMODORO_TOKEN || '';
  if (port > 0 && token) return { port, token };

  const candidates = [];
  if (env.POMODORO_GATEWAY_FILE) {
    candidates.push(env.POMODORO_GATEWAY_FILE);
  } else {
    const appData = env.APPDATA || path.join(os.homedir(), 'AppData', 'Roaming');
    // 打包版/开发版 userData 均为 %APPDATA%/番茄钟（productName），旧目录名兜底
    candidates.push(path.join(appData, '番茄钟', 'gateway.json'));
    candidates.push(path.join(appData, 'pomodoro-fluent', 'gateway.json'));
  }
  for (const file of candidates) {
    try {
      const info = JSON.parse(fs.readFileSync(file, 'utf8'));
      if (info && info.port && info.token) return { port: info.port, token: info.token };
    } catch (e) { /* 尝试下一个 */ }
  }
  return null;
}

// ---- HTTP（零依赖） ----
function request(port, token, method, apiPath, body, timeoutMs) {
  return new Promise((resolve, reject) => {
    const data = body ? JSON.stringify(body) : null;
    const req = http.request({
      host: '127.0.0.1', port, method, path: apiPath,
      headers: {
        Authorization: `Bearer ${token}`,
        ...(data
          ? { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) }
          : {}),
      },
      timeout: timeoutMs,
    }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const text = Buffer.concat(chunks).toString('utf8');
        let json = null;
        try { json = text ? JSON.parse(text) : null; } catch (e) { /* ignore */ }
        if (res.statusCode >= 400) {
          reject(new Error(`HTTP ${res.statusCode}: ${text.slice(0, 200)}`));
        } else {
          resolve(json);
        }
      });
    });
    req.on('error', reject);
    req.on('timeout', () => { req.destroy(); reject(new Error('timeout')); });
    if (data) req.write(data);
    req.end();
  });
}

// ---- stdin 读取（hook 模式） ----
function readStdin() {
  return new Promise((resolve, reject) => {
    if (process.stdin.isTTY) return reject(new Error('no stdin'));
    const chunks = [];
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (c) => chunks.push(c));
    process.stdin.on('end', () => resolve(chunks.join('')));
    process.stdin.on('error', reject);
    // 兜底：5s 内没数据就放弃（hook 配置异常时不至于挂死 agent）
    setTimeout(() => reject(new Error('stdin timeout')), 5000);
  });
}

function parseJsonMaybe(text) {
  try { return text ? JSON.parse(text) : {}; } catch (e) { return {}; }
}

// ---------------------------------------------------------------------------
// 决策缓存：ZCode 一次 AskUserQuestion 会同时触发 PreToolUse 与 PermissionRequest
// 两个 hook（两个独立进程），靠它去重，避免同一问题弹两次窗
// ---------------------------------------------------------------------------
const CACHE_TTL_MS = 90 * 1000;
function cacheDir() {
  return path.join(os.tmpdir(), 'pomodoro-hook-cache');
}
function cacheKeyFor(payload) {
  const id = payload.tool_use_id || payload.tool_useId || payload.toolUseId
    || `${payload.tool_name || ''}:${JSON.stringify(payload.tool_input || {})}`;
  return crypto.createHash('sha1').update(String(id)).digest('hex').slice(0, 20);
}
function cacheRead(key) {
  try {
    const p = path.join(cacheDir(), `${key}.json`);
    const obj = JSON.parse(fs.readFileSync(p, 'utf8'));
    if (Date.now() - obj.at > CACHE_TTL_MS) return null;
    return obj.result;
  } catch (e) { return null; }
}
function cacheWrite(key, result) {
  try {
    const dir = cacheDir();
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, `${key}.json`), JSON.stringify({ at: Date.now(), result }));
  } catch (e) { /* 缓存失败不影响主流程 */ }
}

// ---------------------------------------------------------------------------
// 本地「始终允许」规则
// Claude Code / ZCode 支持把规则回写给宿主（updatedPermissions），但它不是所有
// 宿主都认：VS Code 的 PreToolUse 输出里没有这个字段，Cursor 也一样。
// 那两边只返回一次 allow 的话，下次同类调用还会再弹窗 —— 用户选了「始终允许」
// 却每次都问，就是假的。所以对这类宿主，把规则记到本地，后续命中直接放行，
// 不再弹窗也不再长轮询。
// 关掉：POMODORO_LOCAL_ALWAYS_ALLOW=0
// ---------------------------------------------------------------------------
const LOCAL_RULE_TTL_MS = 30 * 24 * 60 * 60 * 1000;
// Trae 的 PreToolUse 支持 permissionDecision / updatedInput / additionalContext，
// 但同样**没有** updatedPermissions —— 也靠本地规则落地
// Codex 也不能回写「始终允许」：PermissionRequest 的 updatedPermissions 在它那里属
// 「不支持的字段」，而且遇到不支持的字段会 **fail closed**（实机文案：
// "PermissionRequest hook returned unsupported updatedPermissions"）→ 同样靠本地规则。
const LOCAL_RULE_SOURCES = new Set(['vscode', 'trae', 'cursor', 'codex']);

function localRuleFile() {
  return path.join(cacheDir(), 'always-allow.json');
}
function localRuleEnabled(source) {
  if (flag('POMODORO_LOCAL_ALWAYS_ALLOW', true) === false) return false;
  return LOCAL_RULE_SOURCES.has(String(source || ''));
}
function localRuleReadAll() {
  try {
    const list = JSON.parse(fs.readFileSync(localRuleFile(), 'utf8'));
    if (!Array.isArray(list)) return [];
    const now = Date.now();
    return list.filter((r) => r && (!r.at || now - r.at < LOCAL_RULE_TTL_MS));
  } catch (e) { return []; }
}
function localRuleKey(source, tool) {
  return `${String(source || '')}:${normalizeToolName(tool)}`;
}
// 规则内容做「包含」判断：`npm run build --watch` 命中 `npm run build` 算允许
function localRuleMatch(source, tool, rule) {
  if (!localRuleEnabled(source)) return false;
  const key = localRuleKey(source, tool);
  const r = String(rule || '*');
  return localRuleReadAll().some((item) => {
    if (localRuleKey(item.source, item.tool) !== key) return false;
    const saved = String(item.rule || '*');
    if (saved === '*' || r === '*') return true;
    return r.includes(saved) || saved.includes(r);
  });
}
function localRuleAdd(source, tool, rule) {
  if (!localRuleEnabled(source)) return;
  try {
    const key = localRuleKey(source, tool);
    const r = String(rule || '*');
    const list = localRuleReadAll().filter((item) => {
      if (localRuleKey(item.source, item.tool) !== key) return true;
      const saved = String(item.rule || '*');
      // 同工具下，已有规则更宽泛（或相同）就不必再记一条
      return !(saved === '*' || saved === r);
    });
    list.push({ source, tool, rule: r, at: Date.now() });
    fs.mkdirSync(cacheDir(), { recursive: true });
    fs.writeFileSync(localRuleFile(), JSON.stringify(list.slice(-200), null, 0));
  } catch (e) { /* 记不上就算了，只是下次还会问 */ }
}

// ---------------------------------------------------------------------------
// 会话上下文：每个 hook 都是独立进程，靠文件记住「这个会话在干什么」
// 记：任务提示词（UserPromptSubmit）、子 agent 类型、最近工具、项目目录
// 用途：弹窗上显示「哪个 agent / 哪个任务 / 在动哪个工具」，而不是只有来源
// ---------------------------------------------------------------------------
const SESSION_TTL_MS = 12 * 60 * 60 * 1000;
function sessionKey(sessionId) {
  return crypto.createHash('sha1').update(String(sessionId || 'unknown')).digest('hex').slice(0, 16);
}
function sessionFile(sessionId) {
  return path.join(cacheDir(), `session-${sessionKey(sessionId)}.json`);
}
function sessionRead(sessionId) {
  if (!sessionId) return null;
  try {
    const s = JSON.parse(fs.readFileSync(sessionFile(sessionId), 'utf8'));
    if (!s || Date.now() - (s.at || 0) > SESSION_TTL_MS) return null;
    return s;
  } catch (e) { return null; }
}
function sessionMerge(sessionId, patch) {
  if (!sessionId) return null;
  try {
    fs.mkdirSync(cacheDir(), { recursive: true });
    const next = { ...(sessionRead(sessionId) || {}), ...patch, sessionId, at: Date.now() };
    fs.writeFileSync(sessionFile(sessionId), JSON.stringify(next));
    return next;
  } catch (e) { return null; }
}
function listSessions() {
  const out = [];
  try {
    for (const f of fs.readdirSync(cacheDir())) {
      if (!/^session-.*\.json$/.test(f)) continue;
      try {
        const s = JSON.parse(fs.readFileSync(path.join(cacheDir(), f), 'utf8'));
        if (s && Date.now() - (s.at || 0) <= SESSION_TTL_MS) out.push(s);
      } catch (e) { /* 跳过坏文件 */ }
    }
  } catch (e) { /* 目录不存在 */ }
  return out.sort((a, b) => (b.at || 0) - (a.at || 0));
}

function collapse(text, n) {
  const s = String(text || '').replace(/\s+/g, ' ').trim();
  if (!s) return '';
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}

// 会话 id 只露尾部，避免弹窗上出现一长串 uuid
function shortSession(id) {
  const s = String(id || '');
  return s ? s.slice(-6) : '';
}

// 组装弹窗上下文：宿主 agent + 子 agent + 任务 + 项目 + 会话 + 工具
function buildContext(payload, source, session, extra) {
  const p = payload || {};
  const s = session || {};
  const cwd = p.cwd || p.working_directory || s.cwd || '';
  return {
    agent: source || '',
    agentType: p.agent_type || p.agentType || s.agentType || '',
    agentId: p.agent_id || s.agentId || '',
    session: shortSession(p.session_id || p.sessionId || s.sessionId),
    project: cwd ? path.basename(cwd) : (s.project || ''),
    task: collapse(s.task || '', 160),
    tool: s.lastTool || '',
    toolDetail: s.lastToolDetail || '',
    ...(extra || {}),
  };
}

// 把当前 hook 事件累积进会话上下文
function rememberContext(payload, event, source) {
  const sessionId = payload.session_id || payload.sessionId || '';
  if (!sessionId) return null;
  const patch = {};
  if (source) patch.source = source;
  const cwd = payload.cwd || payload.working_directory
    || (Array.isArray(payload.workspace_roots) ? payload.workspace_roots[0] : '');
  if (cwd) patch.cwd = cwd;
  if (payload.agent_type || payload.agentType) patch.agentType = payload.agent_type || payload.agentType;
  switch (event) {
    case 'SessionStart':
      patch.task = '';                       // 新会话：清掉上一轮的任务描述
      if (payload.model) patch.model = String(payload.model);
      break;
    case 'UserPromptSubmit':
      if (payload.prompt) patch.task = collapse(payload.prompt, 400);
      break;
    case 'PreToolUse':
    case 'PostToolUse':
    case 'PostToolUseFailure':
    case 'PermissionRequest':
      if (payload.tool_name) {
        patch.lastTool = String(payload.tool_name);
        patch.lastToolDetail = collapse(ruleContentFor(payload.tool_name, payload.tool_input), 160);
      }
      break;
    case 'Stop':
      if (payload.last_assistant_message) {
        patch.lastAssistant = collapse(payload.last_assistant_message, 200);
      }
      break;
    default:
      break;
  }
  if (!Object.keys(patch).length) return sessionRead(sessionId);
  return sessionMerge(sessionId, patch) || sessionRead(sessionId);
}

// ---------------------------------------------------------------------------
// 通用小工具
// ---------------------------------------------------------------------------
function summarizeToolInput(toolInput) {
  if (!toolInput || typeof toolInput !== 'object') return '';
  const parts = [];
  for (const [k, v] of Object.entries(toolInput)) {
    let s = typeof v === 'string' ? v : JSON.stringify(v);
    if (s && s.length > 400) s = s.slice(0, 399) + '…';
    parts.push(`${k}: ${s}`);
    if (parts.length >= 4) break;
  }
  return parts.join('\n');
}

// 权限规则内容（用于「始终允许」）：挑最有代表性的那个入参
function ruleContentFor(tool, toolInput) {
  const ti = toolInput || {};
  const pick = ti.command || ti.pattern || ti.file_path || ti.filePath || ti.path || ti.url || ti.description;
  if (typeof pick === 'string' && pick) return pick.length > 200 ? pick.slice(0, 199) + '…' : pick;
  return '*';
}

// permission_suggestions / 手工规则 → updatedPermissions 条目
function buildPermissionUpdates(tool, toolInput, suggestions) {
  const dest = env.POMODORO_PERMISSION_DEST || 'projectSettings';
  if (Array.isArray(suggestions) && suggestions.length) {
    return suggestions.slice(0, 4).map((s) => {
      if (s && typeof s === 'object') {
        // 已是完整条目（含 type/rules）：只覆盖 behavior 与 destination
        if (s.rules) return { ...s, behavior: 'allow', destination: s.destination || dest };
        return { type: 'addRules', rules: [s], behavior: 'allow', destination: dest };
      }
      return { type: 'addRules', rules: [{ toolName: tool, ruleContent: String(s) }], behavior: 'allow', destination: dest };
    });
  }
  return [{
    type: 'addRules',
    rules: [{ toolName: tool, ruleContent: ruleContentFor(tool, toolInput) }],
    behavior: 'allow',
    destination: dest,
  }];
}

// 工具入参 → 弹窗里的 questions（Claude Code / ZCode 的 AskUserQuestion）
function questionsFromToolInput(toolInput) {
  const ti = toolInput || {};
  const raw = Array.isArray(ti.questions) ? ti.questions : [];
  return raw.slice(0, 4).map((q, i) => {
    const text = (q && (q.question || q.text || q.title)) || `问题 ${i + 1}`;
    const options = (Array.isArray(q && q.options) ? q.options : []).slice(0, 6).map((o, j) => {
      if (typeof o === 'string') return { id: `o${j}`, label: o, description: '' };
      return {
        id: `o${j}`,
        label: String(o.label || o.text || o.value || ''),
        description: String(o.description || o.hint || ''),
      };
    }).filter((o) => o.label);
    return {
      id: `q${i}`,
      question: String(text),
      header: String((q && q.header) || ''),
      multiSelect: !!(q && (q.multiSelect || q.multiple)),
      // allowFreeformInput / openEnded 是 VS Code 提问工具的写法；options 为空也要给输入框
      custom: q && typeof q.custom === 'boolean'
        ? q.custom
        : (options.length === 0 || !!(q && (q.allowFreeformInput || q.openEnded))),
      options,
    };
  }).filter((q) => q.question);
}

// 弹窗回传的 answers（按 question.id 索引）→ agent 要的 answers（按问题文本索引）
function buildAnswerMap(questions, answers) {
  const out = {};
  questions.forEach((q) => {
    const v = answers && answers[q.id];
    if (!v || !v.length) return;
    out[q.question] = q.multiSelect ? v : v[0];
  });
  return out;
}

function formatAnswerMap(map) {
  return Object.entries(map).map(([k, v]) => `${k} → ${Array.isArray(v) ? v.join('、') : v}`).join('\n');
}

// ---------------------------------------------------------------------------
// 网关调用
// ---------------------------------------------------------------------------
// 时间预算：三层必须嵌套，顺序不能乱
//   网关兜底     POMODORO_TIMEOUT_S（默认 3600s）：到点回「未决策」，交还宿主原生询问
//   hook 等网关  兜底 + WAIT_HTTP_MARGIN_MS：给往返与渲染留余量
//   宿主超时     HOST_WAIT_*：必须排最后。宿主若先到点，它会直接杀掉 hook，
//              那句「未决策」根本发不出去，宿主就按自己的审批设置走了（可能静默放行）
const WAIT_HTTP_MARGIN_MS = 5 * 60 * 1000;
const HOST_WAIT_SEC = 4200;          // 秒制宿主：VS Code / Trae / Cursor / Qwen / Claude Code
const HOST_WAIT_MS = 4200 * 1000;    // 毫秒制宿主：ZCode

function timeoutMs() {
  const s = Math.min(Number(env.POMODORO_TIMEOUT_S) || 3600, 7200);
  return Math.max(5, Math.round(s)) * 1000;
}

async function postInteraction(gw, body, ms) {
  return request(gw.port, gw.token, 'POST', '/api/interaction', body, ms + WAIT_HTTP_MARGIN_MS);
}

async function postEvent(gw, body) {
  return request(gw.port, gw.token, 'POST', '/api/event', body, 8000);
}

// ---------------------------------------------------------------------------
// 协议识别
// ---------------------------------------------------------------------------
// 工具名归一化：去掉命名空间（vscode/askQuestions）、大小写、下划线/连字符差异。
// 各宿主命名风格差得很远：Claude/ZCode 用 AskUserQuestion，VS Code 用
// vscode/askQuestions，Cursor 用 question，Qwen 沿用 Claude 的。
function normalizeToolName(name) {
  return String(name || '')
    .replace(/^[A-Za-z0-9_.-]+\//, '')   // 去掉 vscode/ copilot/ 之类的命名空间
    .replace(/[^A-Za-z0-9]/g, '')        // 去掉 _ - 空格
    .replace(/^vscode/i, '')             // vscode_askQuestions 这种前缀式
    .toLowerCase();
}

const ASK_TOOL_SET = new Set([
  'askuserquestion', 'askuserquestions', 'askuser', 'askquestions', 'askquestion', 'question',
]);

function isAskToolName(name) {
  return ASK_TOOL_SET.has(normalizeToolName(name));
}

// Cursor 的 hook 事件名是 camelCase（其余宿主是 PascalCase）
const CURSOR_EVENTS = new Set([
  'beforeShellExecution', 'afterShellExecution', 'beforeMCPExecution', 'afterMCPExecution',
  'beforeReadFile', 'afterFileEdit', 'beforeSubmitPrompt', 'afterAgentResponse',
  'afterAgentThought', 'preToolUse', 'postToolUse', 'postToolUseFailure',
  'sessionStart', 'sessionEnd', 'subagentStart', 'subagentStop', 'preCompact',
  'stop', 'afterTabFileEdit', 'beforeTabFileRead', 'workspaceOpen',
]);

function isAskTool(payload) {
  const tool = String(payload.tool_name || payload.tool || payload.name || '');
  if (isAskToolName(tool)) return true;
  const ti = payload.tool_input || payload.toolInput;
  if (ti && Array.isArray(ti.questions) && ti.questions.length && !ti.command && !ti.file_path && !ti.filePath) return true;
  // VS Code Copilot 的提问工具入参形如 { questions: [...] } 或 { question: "..." }
  if (ti && typeof ti === 'object' && typeof ti.question === 'string' && !ti.command) return true;
  return false;
}

// 提问入参兼容三种形状：Claude/ZCode({questions})、VS Code、Cursor
function toolInputQuestions(payload) {
  const ti = payload.tool_input || payload.toolInput || {};
  if (Array.isArray(ti.questions) && ti.questions.length) return ti;
  if (typeof ti.question === 'string') {
    const opts = Array.isArray(ti.options) ? ti.options : (Array.isArray(ti.choices) ? ti.choices : []);
    return { questions: [{ question: ti.question, header: ti.header || '', options: opts, multiSelect: !!(ti.multiSelect || ti.multiple) }] };
  }
  return ti;
}

function detectSource(payload) {
  if (env.POMODORO_SOURCE) return String(env.POMODORO_SOURCE);
  if (payload && payload.source) return String(payload.source);
  if (env.ZCODE_PLUGIN_ROOT || env.ZCODE_CLI_HOME) return 'zcode';
  if (payload && (payload.cursor_version || payload.conversation_id)) return 'cursor';
  // Trae 的 hook payload 会额外带 llm_tool_name 与 workspace_roots，可据此识别
  if (payload && (payload.llm_tool_name || (payload.workspace_roots && payload.tool_use_id))) return 'trae';
  // Codex 的 hook payload 带 turn_id，PermissionRequest 还额外带 trigger。
  // 注意：这里**故意不做 ~/.codex 目录探测** —— 装 Codex 的人往往同时装了别的宿主，
  // 用「目录存在」猜来源会把 Claude Code 的调用误判成 codex。
  // （install 生成的命令一律带 --source codex，这条只是手写配置时的兜底。）
  if (payload && (payload.turn_id || payload.trigger)) return 'codex';
  try {
    if (fs.existsSync(path.join(os.homedir(), '.zcode'))) return 'zcode';
  } catch (e) { /* ignore */ }
  try {
    if (fs.existsSync(path.join(os.homedir(), '.trae-cn'))) return 'trae';
  } catch (e) { /* ignore */ }
  return 'claude-code';
}

function detectProtocol(payload) {
  const evt = payload && typeof payload.hook_event_name === 'string' ? payload.hook_event_name : '';
  if (CURSOR_EVENTS.has(evt)) return 'cursor';
  if (evt) return 'ancli'; // Claude Code / ZCode / VS Code Copilot / Qwen Code 同族
  if (payload && payload.permission && typeof payload.permission === 'object') return 'opencode-permission';
  if (payload && Array.isArray(payload.questions)) return 'opencode-question';
  if (payload && typeof payload.event === 'string') return 'opencode-event';
  return 'ancli';
}

// Codex（codex-cli 0.154.0 实测）：审批**只走 PermissionRequest**，不在 PreToolUse 上拦。
// 依据是实机 bundle 里的原话 —— PreToolUse 只强制执行 deny，其余值都被解析但不生效：
//   PreToolUse hook returned unsupported permissionDecision:allow
//   PreToolUse hook returned unsupported permissionDecision:ask
// 所以在 PreToolUse 上回 allow/ask 等于什么都没回，宿主照样走自己的审批 → 那次审批
// 又会触发 PermissionRequest → 弹两次窗。而且 Codex 的 PermissionRequest 只在
// 「Codex 本来就要问用户」时才触发（不需要审批的调用不跑），条件比 PreToolUse 精确得多。
// （2026-09-18 起 PreToolUse 整体不再做审批 —— 所有宿主都只在这里处理提问与活动上报。）
const PERMISSION_EVENT_ONLY_SOURCES = new Set(['codex']);

async function handleAsk(payload, source, gw, context) {
  const toolInput = payload.tool_input || payload.toolInput || {};
  const questions = questionsFromToolInput(toolInputQuestions(payload));
  const key = cacheKeyFor(payload);
  if (!questions.length) return null;

  // 同一提问被 PreToolUse + PermissionRequest 双触发时复用首次决策
  let result = cacheRead(key);
  if (!result) {
    const ms = timeoutMs();
    result = await postInteraction(gw, {
      kind: 'ask',
      source,
      title: 'Agent 提问',
      message: questions.length === 1 ? questions[0].question : `${questions.length} 个问题等待回答`,
      questions,
      context: { ...(context || {}), tool: String(payload.tool_name || 'AskUserQuestion'), toolDetail: '' },
      timeoutMs: ms,
    }, ms);
    cacheWrite(key, result || {});
  }
  if (!result) return null;

  const answers = buildAnswerMap(questions, result.answers);
  const decided = result.decidedBy === 'user';
  const event = payload.hook_event_name;

  // 用户作答：注入 answers 让原生 UI 不再弹出
  if (decided && result.action === 'submit') {
    const updatedInput = { ...toolInput, answers };
    // VS Code 的提问工具（vscode/askQuestions）弹的是 QuickPick，答案不在入参里、
    // 改 updatedInput 只换了问题本身，改不动用户选择 → 默认走 deny + 把答案写进原因。
    // 其它宿主默认走 updatedInput.answers（Claude Code / ZCode），可用 POMODORO_ASK_MODE 覆盖。
    const askMode = String(
      env.POMODORO_ASK_MODE || (source === 'vscode' ? 'deny' : 'answers')
    ).toLowerCase();
    if (askMode === 'deny') {
      // 备用通道：部分宿主不认 updatedInput.answers，改成 deny + 把答案塞进原因
      const reason = `用户在番茄钟弹窗中的回答：\n${formatAnswerMap(answers)}`;
      return {
        hookSpecificOutput: {
          hookEventName: 'PreToolUse',
          permissionDecision: 'deny',
          permissionDecisionReason: reason,
          // VS Code 里 additionalContext 才是「给模型看」的字段，reason 只展示给用户
          additionalContext: reason,
        },
      };
    }
    if (event === 'PermissionRequest') {
      return {
        hookSpecificOutput: {
          hookEventName: 'PermissionRequest',
          decision: { behavior: 'allow', updatedInput },
        },
      };
    }
    return {
      hookSpecificOutput: {
        hookEventName: 'PreToolUse',
        permissionDecision: 'allow',
        permissionDecisionReason: '用户在番茄钟弹窗内作答',
        updatedInput,
      },
    };
  }

  // 用户显式取消/拒绝
  if (decided && (result.action === 'cancel' || result.action === 'deny')) {
    const reason = result.text
      ? `用户取消：${result.text}`
      : '用户取消了这次提问（番茄钟弹窗）';
    if (event === 'PermissionRequest') {
      return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'deny', message: reason } } };
    }
    return { hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: reason } };
  }

  // 超时 / 被顶掉：不输出决策，交回终端原生 UI
  return null;
}

async function handlePermission(payload, source, gw, context) {
  const tool = String(payload.tool_name || '工具');
  const toolInput = payload.tool_input || {};
  const rule = ruleContentFor(tool, toolInput);

  // 宿主不认 updatedPermissions（Cursor 等）时，靠本地规则实现「始终允许」
  if (localRuleMatch(source, tool, rule)) {
    return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'allow', message: '命中本地「始终允许」规则' } } };
  }

  const ms = timeoutMs();
  const result = await postInteraction(gw, {
    kind: 'permission',
    source,
    title: `允许 ${tool}？`,
    message: payload.message || '',
    detail: summarizeToolInput(toolInput),
    permission: {
      tool,
      rule,
      suggestions: payload.permission_suggestions || [],
      canAlways: flag('POMODORO_ALWAYS_ALLOW', true) !== false,
    },
    context: { ...(context || {}), tool, toolDetail: collapse(rule, 160) },
    timeoutMs: ms,
  }, ms);

  if (!result) return null;   // 未收到决策：不输出，交回宿主原生审批
  const decided = result.decidedBy === 'user';

  if (decided && result.action === 'allow') {
    return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'allow', message: result.text || '用户通过番茄钟弹窗允许' } } };
  }

  if (decided && result.action === 'allow-always') {
    const updatedPermissions = buildPermissionUpdates(tool, toolInput, payload.permission_suggestions);
    localRuleAdd(source, tool, rule);   // 兜底：宿主不认 updatedPermissions 时也生效
    const decision = { behavior: 'allow', message: '用户选择始终允许（已记入番茄钟本地规则）' };
    // Codex 遇到不支持的字段会 fail closed，绝不能把 updatedPermissions 塞给它
    if (!PERMISSION_EVENT_ONLY_SOURCES.has(String(source || ''))) {
      decision.updatedPermissions = updatedPermissions;
    }
    return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision } };
  }

  if (decided && result.action === 'deny') {
    const message = result.text ? `用户拒绝：${result.text}` : '用户通过番茄钟弹窗拒绝';
    return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'deny', message } } };
  }

  // 未决策：不输出，走宿主原生询问
  return null;
}

async function runAncliMode(gw, payload) {
  const source = detectSource(payload);
  const event = String(payload.hook_event_name || '');
  // 累积会话上下文（任务 / 子 agent / 最近工具 / 项目），供弹窗展示
  const session = rememberContext(payload, event, source);
  const context = buildContext(payload, source, session);

  // 提问（AskUserQuestion）：PreToolUse 与 PermissionRequest 都可能触发
  if (isAskTool(payload) && flag('POMODORO_ASK', true) !== false) {
    const out = await handleAsk(payload, source, gw, context);
    if (out) process.stdout.write(JSON.stringify(out));
    return;
  }

  // 权限请求
  if (event === 'PermissionRequest' && flag('POMODORO_PERMISSION', true) !== false) {
    const out = await handlePermission(payload, source, gw, context);
    if (out) process.stdout.write(JSON.stringify(out));
    return;
  }

  // PreToolUse：2026-09-18 起只处理提问（上面 isAskTool 分支）与活动上报，
  // 不再拦截/审批普通工具调用 —— 审批统一交给有 PermissionRequest 的宿主。

  // 其余事件：只上报计数/通知
  const bodyByEvent = {
    Notification: { kind: 'notification', message: payload.message || '', source },
    Stop: { kind: 'stop', source },
    SubagentStop: { kind: 'subagent-stop', source },
    SubagentStart: { kind: 'subagent-start', source },
    PostToolUse: { kind: 'tool-after', tool: payload.tool_name || '', source },
    PreToolUse: { kind: 'tool-before', tool: payload.tool_name || '', source },
    SessionStart: { kind: 'session-start', source },
    SessionEnd: { kind: 'session-end', source },
    PreCompact: { kind: 'pre-compact', source },
    PostCompact: { kind: 'post-compact', source },
    Interrupt: { kind: 'interrupt', source },
    UserPromptSubmit: { kind: 'prompt', source },
  };
  const body = bodyByEvent[event];
  if (!body) return; // 未识别的事件：静默忽略
  if (event === 'Notification' && !body.message) body.message = 'Agent 需要你的确认';
  // 通知类也带上上下文：弹窗能显示是哪个任务、在动哪个工具
  // UserPromptSubmit 必须带：工作报告靠 context.task 记下「这次让 agent 干什么」
  if (event === 'Notification' || event === 'Stop' || event === 'UserPromptSubmit') {
    body.context = context;
  }
  // 注意：Stop 只上报，绝不输出 decision:"block" —— 那会阻止 agent 收尾，
  // 甚至把它拖进自动续跑的循环（VS Code 的 stop_hook_active 就是防这个的）
  await postEvent(gw, body);
}

// ---------------------------------------------------------------------------
// Cursor（.cursor/hooks.json：事件名 camelCase，输出字段 snake_case）
// 没有独立的权限事件 → 由 beforeShellExecution / preToolUse / beforeMCPExecution 接管
// ---------------------------------------------------------------------------
function writeCursor(obj) {
  process.stdout.write(JSON.stringify(obj || {}));
}

// Cursor 事件名 → 复用同一套上下文记录时用的「标准事件名」
function cursorEventAlias(event) {
  const map = {
    beforeShellExecution: 'PreToolUse',
    beforeMCPExecution: 'PreToolUse',
    beforeReadFile: 'PreToolUse',
    preToolUse: 'PreToolUse',
    afterShellExecution: 'PostToolUse',
    afterMCPExecution: 'PostToolUse',
    afterFileEdit: 'PostToolUse',
    postToolUse: 'PostToolUse',
    postToolUseFailure: 'PostToolUseFailure',
    beforeSubmitPrompt: 'UserPromptSubmit',
    sessionStart: 'SessionStart',
    sessionEnd: 'SessionEnd',
    subagentStart: 'SubagentStart',
    subagentStop: 'SubagentStop',
    stop: 'Stop',
    preCompact: 'PreCompact',
    afterAgentResponse: 'Stop',       // 只用来留最后一段回复，不当作回合结束
    afterAgentThought: 'PostToolUse',
  };
  return map[event] || '';
}

function cursorToolOf(payload, event) {
  if (event === 'beforeShellExecution') return 'Shell';
  if (event === 'beforeReadFile') return 'Read';
  return String(payload.tool_name || payload.tool || payload.name || 'tool');
}

function cursorDetailOf(payload, event) {
  if (event === 'beforeShellExecution') return collapse(String(payload.command || ''), 160);
  if (event === 'beforeReadFile') return collapse(String(payload.file_path || ''), 160);
  const ti = payload.tool_input || payload.tool_input === '' ? payload.tool_input : payload.toolInput;
  return collapse(typeof ti === 'string' ? ti : summarizeToolInput(ti), 160);
}

async function runCursorMode(gw, payload) {
  const event = String(payload.hook_event_name || '');
  const roots = Array.isArray(payload.workspace_roots) ? payload.workspace_roots : [];
  const cwd = payload.cwd || roots[0] || '';
  const toolInput = payload.tool_input || (payload.command ? { command: payload.command }
    : (payload.file_path ? { file_path: payload.file_path } : {}));

  // 归一成通用形状，复用上下文记录
  const session = rememberContext({
    session_id: payload.conversation_id || payload.generation_id || '',
    cwd,
    agent_type: payload.subagent_type || payload.subagent || '',
    prompt: payload.prompt || '',
    tool_name: cursorToolOf(payload, event),
    tool_input: toolInput,
    last_assistant_message: payload.text || '',
  }, cursorEventAlias(event), 'cursor');

  const baseCtx = buildContext({ session_id: payload.conversation_id || '', cwd }, 'cursor', session);
  const tool = cursorToolOf(payload, event);
  const detail = cursorDetailOf(payload, event);
  const ctx = { ...baseCtx, tool, toolDetail: detail };

  // ---- 可拦截事件：问答 / 权限 ----
  const blocking = event === 'preToolUse' || event === 'beforeMCPExecution' ||
    event === 'beforeShellExecution' || event === 'beforeReadFile';
  if (blocking) {
    const ms = timeoutMs();

    // 提问：Cursor 的 preToolUse 支持 updated_input，注入答案后放行
    if (event === 'preToolUse' && isAskTool(payload) && flag('POMODORO_ASK', true) !== false) {
      const questions = questionsFromToolInput(toolInputQuestions(payload));
      if (questions.length) {
        const r = await postInteraction(gw, {
          kind: 'ask', source: 'cursor', title: 'Agent 提问',
          message: questions.length === 1 ? questions[0].question : `${questions.length} 个问题等待回答`,
          questions, context: { ...ctx, tool: tool, toolDetail: '' }, timeoutMs: ms,
        }, ms);
        if (r && r.decidedBy === 'user' && r.action === 'submit') {
          const answers = buildAnswerMap(questions, r.answers);
          writeCursor({ permission: 'allow', updated_input: { ...toolInput, answers } });
          return;
        }
        if (r && r.decidedBy === 'user' && (r.action === 'cancel' || r.action === 'deny')) {
          writeCursor({ permission: 'deny', user_message: r.text || '已在番茄钟弹窗中取消', agent_message: r.text || '用户取消了这次提问' });
          return;
        }
        writeCursor({ permission: 'ask', user_message: '番茄钟未获得回答，交回 Cursor 原生提问' });
        return;
      }
    }

    // 权限：默认接管（Cursor 没有单独的 PermissionRequest 事件）
    if (flag('POMODORO_PERMISSION', true) !== false) {
      // 本地「始终允许」规则：Cursor 的 preToolUse 不回写规则，只能自己记
      if (localRuleMatch('cursor', tool, detail)) {
        writeCursor(event === 'beforeReadFile'
          ? { permission: 'allow' }
          : { permission: 'allow', agent_message: '命中番茄钟「始终允许」规则' });
        return;
      }
      const r = await postInteraction(gw, {
        kind: 'permission', source: 'cursor',
        title: event === 'beforeShellExecution' ? '允许执行命令？' : `${tool} 需要授权`,
        message: event === 'beforeShellExecution' ? String(payload.command || '') : '',
        detail: event === 'beforeShellExecution' ? `cwd: ${cwd}` : summarizeToolInput(toolInput),
        permission: { tool, rule: detail, suggestions: [], canAlways: true },
        context: ctx, timeoutMs: ms,
      }, ms);
      if (r && r.decidedBy === 'user') {
        if (r.action === 'allow' || r.action === 'allow-always') {
          if (r.action === 'allow-always') localRuleAdd('cursor', tool, detail);
          // beforeReadFile 只认 permission，不接受消息字段
          writeCursor(event === 'beforeReadFile'
            ? { permission: 'allow' }
            : { permission: 'allow', agent_message: r.text || '用户在番茄钟弹窗中允许' });
          return;
        }
        writeCursor(event === 'beforeReadFile'
          ? { permission: 'deny' }
          : { permission: 'deny', user_message: r.text || '已在番茄钟弹窗中拒绝', agent_message: r.text ? `用户拒绝：${r.text}` : '用户通过番茄钟弹窗拒绝' });
        return;
      }
      // 超时 / 被关闭：交回 Cursor 原生确认（而不是硬拒，Cursor 原生支持 ask）
      writeCursor({ permission: 'ask', user_message: '番茄钟未获得决策，交回 Cursor 原生确认' });
      return;
    }

    // 不接管权限：不回任何决定，交回 Cursor 原生审批流程
    // （以前这里回 permission:allow，等于替用户强制放行，属于越权）
    writeCursor({});
    return;
  }

  // ---- 非拦截事件：只计数 / 通知，不回任何决定 ----
  const kindMap = {
    sessionStart: 'session-start',
    sessionEnd: 'session-end',
    subagentStart: 'session-start',
    subagentStop: 'subagent-stop',
    preCompact: 'notification',
    postToolUse: 'tool-after',
    postToolUseFailure: 'tool-after',
    afterShellExecution: 'tool-after',
    afterMCPExecution: 'tool-after',
    afterFileEdit: 'tool-after',
    afterAgentThought: 'tool-after',
  };
  if (event === 'beforeSubmitPrompt') {
    writeCursor({ continue: true });   // 记录任务后放行，不阻断用户提示词
    return;
  }
  if (event === 'stop') {
    await postEvent(gw, { kind: 'stop', source: 'cursor', context: ctx });
    writeCursor({});                   // 不要 followup_message，别把 agent 拖进循环
    return;
  }
  const kind = kindMap[event];
  if (kind) {
    await postEvent(gw, {
      kind,
      source: 'cursor',
      tool,
      message: kind === 'notification' ? `${event}` : '',
      context: kind === 'notification' || kind === 'stop' ? ctx : undefined,
    });
    return;
  }
  writeCursor({});
}

// ---------------------------------------------------------------------------
// Codex CLI（~/.codex/config.toml 的 notify：只支持回合结束，无权限回调）
// ---------------------------------------------------------------------------
async function runCodexNotify(gw, payload) {
  const p = payload || {};
  const type = String(p.type || p.event || '');
  const roots = Array.isArray(p['workspace-roots']) ? p['workspace-roots'] : [];
  const cwd = p.cwd || roots[0] || '';
  const inputs = Array.isArray(p['input-messages']) ? p['input-messages'] : [];
  const task = collapse(inputs.length ? String(inputs[inputs.length - 1]) : '', 160);
  const last = collapse(String(p['last-assistant-message'] || p.message || ''), 160);
  const sessionId = p['thread-id'] || p.thread_id || '';

  const session = rememberContext({
    session_id: sessionId,
    cwd,
    prompt: task,
    last_assistant_message: last,
  }, type === 'agent-turn-complete' ? 'Stop' : 'Notification', 'codex');
  const context = buildContext({ session_id: sessionId, cwd }, 'codex', session);

  const kind = type === 'agent-turn-complete' ? 'stop' : 'notification';
  await postEvent(gw, {
    kind,
    source: 'codex',
    message: kind === 'notification' ? (last || type || 'Codex 事件') : '',
    context,
  });
  // notify 是单向通知，没有决策回传通道
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------
// OpenCode 侧的上下文由插件随请求带上：sessionID / directory / sessionTitle
function openCodeContext(payload, extra) {
  const p = payload || {};
  const dir = p.directory || '';
  return {
    agent: 'opencode',
    agentType: String(p.agent || p.agentType || ''),
    agentId: '',
    session: shortSession(p.sessionID || p.sessionId),
    project: p.project || (dir ? path.basename(dir) : ''),
    task: collapse(p.sessionTitle || p.title || '', 160),
    tool: '',
    toolDetail: '',
    ...(extra || {}),
  };
}

async function runOpenCodePermission(gw, payload) {
  const p = (payload && payload.permission) || payload || {};
  const type = String(p.type || p.permission || 'tool');
  const patterns = p.patterns || (p.pattern ? [p.pattern] : []);
  const meta = p.metadata || {};
  const ms = timeoutMs();
  const result = await postInteraction(gw, {
    kind: 'permission',
    source: 'opencode',
    title: p.title || `允许 ${type}？`,
    message: Array.isArray(patterns) ? patterns.join('  ') : String(patterns || ''),
    detail: summarizeToolInput(meta),
    permission: { tool: type, rule: String(patterns[0] || ''), suggestions: [], canAlways: false },
    context: openCodeContext(payload, { tool: type, toolDetail: collapse(String(patterns[0] || ''), 160) }),
    timeoutMs: ms,
  }, ms);

  if (!result || result.decidedBy !== 'user') return { status: 'ask' };
  if (result.action === 'allow') return { status: 'allow', message: result.text || '' };
  return { status: 'deny', message: result.text || '' };
}

async function runOpenCodeQuestion(gw, payload) {
  const p = payload || {};
  const questions = (Array.isArray(p.questions) ? p.questions : []).slice(0, 4).map((q, i) => {
    const options = (Array.isArray(q.options) ? q.options : []).slice(0, 6).map((o, j) => {
      if (typeof o === 'string') return { id: `o${j}`, label: o, description: '' };
      return { id: `o${j}`, label: String(o.label || o.text || ''), description: String(o.description || '') };
    }).filter((o) => o.label);
    return {
      id: `q${i}`,
      question: String(q.question || q.text || `问题 ${i + 1}`),
      header: String(q.header || ''),
      multiSelect: !!(q.multiple || q.multiSelect),
      custom: typeof q.custom === 'boolean' ? q.custom : options.length === 0,
      options,
    };
  });
  if (!questions.length) return { reject: true };

  const ms = timeoutMs();
  const result = await postInteraction(gw, {
    kind: 'ask',
    source: 'opencode',
    title: 'Agent 提问',
    message: questions.length === 1 ? questions[0].question : `${questions.length} 个问题等待回答`,
    questions,
    context: openCodeContext(p, { tool: 'question' }),
    timeoutMs: ms,
  }, ms);

  if (!result || result.decidedBy !== 'user' || result.action !== 'submit') return { reject: true };
  const answers = questions.map((q) => {
    const v = result.answers && result.answers[q.id];
    return Array.isArray(v) ? v : (v ? [v] : []);
  });
  return { answers, text: result.text || '' };
}

async function replyQuestion(serverUrl, sessionID, requestID, answers) {
  if (!serverUrl || !requestID) return false;
  const base = String(serverUrl).replace(/\/+$/, '');
  const tries = [
    { url: `${base}/session/${encodeURIComponent(sessionID)}/question/reply`, body: { requestID, answers } },
    { url: `${base}/question/${encodeURIComponent(requestID)}/reply`, body: { answers } },
  ];
  for (const t of tries) {
    try {
      await new Promise((resolve, reject) => {
        const data = JSON.stringify(t.body);
        const u = new URL(t.url);
        const req = http.request({
          host: u.hostname, port: u.port || 80, method: 'POST', path: u.pathname + u.search,
          headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) },
          timeout: 8000,
        }, (res) => {
          res.on('data', () => {});
          res.on('end', () => (res.statusCode < 400 ? resolve() : reject(new Error(`HTTP ${res.statusCode}`))));
        });
        req.on('error', reject);
        req.on('timeout', () => { req.destroy(); reject(new Error('timeout')); });
        req.write(data);
        req.end();
      });
      return true;
    } catch (e) { /* 换下一个候选 */ }
  }
  return false;
}

async function rejectQuestion(serverUrl, sessionID, requestID) {
  if (!serverUrl || !requestID) return false;
  const base = String(serverUrl).replace(/\/+$/, '');
  const tries = [
    { url: `${base}/session/${encodeURIComponent(sessionID)}/question/reject`, body: { requestID } },
    { url: `${base}/question/${encodeURIComponent(requestID)}/reject`, body: {} },
  ];
  for (const t of tries) {
    try {
      await new Promise((resolve, reject) => {
        const data = JSON.stringify(t.body);
        const u = new URL(t.url);
        const req = http.request({
          host: u.hostname, port: u.port || 80, method: 'POST', path: u.pathname + u.search,
          headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) },
          timeout: 8000,
        }, (res) => {
          res.on('data', () => {});
          res.on('end', () => (res.statusCode < 400 ? resolve() : reject(new Error(`HTTP ${res.statusCode}`))));
        });
        req.on('error', reject);
        req.on('timeout', () => { req.destroy(); reject(new Error('timeout')); });
        req.write(data);
        req.end();
      });
      return true;
    } catch (e) { /* 换下一个候选 */ }
  }
  return false;
}

// 提问结果回传：能直连 OpenCode 服务端就直连，否则把答案交回插件（它用 SDK 回传）
async function finishOpenCodeQuestion(payload, out) {
  let delivered = false;
  if (out.reject) {
    delivered = await rejectQuestion(payload.serverUrl, payload.sessionID, payload.requestID);
  } else {
    delivered = await replyQuestion(payload.serverUrl, payload.sessionID, payload.requestID, out.answers);
  }
  process.stdout.write(JSON.stringify(delivered ? { ok: true } : out));
}

async function runOpenCodeEvent(gw, payload) {
  const p = payload || {};
  const evt = String(p.event || p.type || '');
  const props = p.properties || p.data || p;
  const map = {
    'session.idle': 'stop',
    'session.error': 'notification',
    'session.created': 'session-start',
    'session.deleted': 'session-end',
  };
  const kind = map[evt] || p.kind;
  if (!kind) return;
  await postEvent(gw, {
    kind,
    source: 'opencode',
    message: kind === 'notification' ? String(props.error || props.message || 'OpenCode 会话异常') : '',
    context: openCodeContext(props, {
      tool: kind === 'notification' && props.error ? 'error' : '',
    }),
  });
}

// ---------------------------------------------------------------------------
// 手动调试：ask / permission / notify
// ---------------------------------------------------------------------------
function collectFlag(args, name) {
  const out = [];
  for (let i = 0; i < args.length; i++) {
    if (args[i] === `--${name}` && i + 1 < args.length) out.push(args[i + 1]);
  }
  return out;
}

// 手动调试时的上下文（用于验证弹窗上的「任务 / agent / 工具」展示）
function manualContext(args) {
  const first = (n) => collectFlag(args, n)[0] || '';
  return {
    agent: first('agent') || 'manual',
    agentType: first('agent-type'),
    session: 'manual',
    project: first('project') || 'manual',
    task: first('task'),
    tool: '',
    toolDetail: '',
  };
}

async function runManualAsk(gw, args) {
  const questions = (collectFlag(args, 'question') || ['手动测试提问']).map((q, i) => ({
    id: `q${i}`,
    question: q,
    header: '测试',
    multiSelect: false,
    custom: true,
    options: collectFlag(args, 'option').map((o, j) => ({ id: `o${j}`, label: o, description: '' })),
  }));
  const ms = timeoutMs();
  const r = await postInteraction(gw, {
    kind: 'ask', source: 'manual', title: '手动测试 · 提问', questions,
    context: { ...manualContext(args), tool: 'AskUserQuestion' },
    timeoutMs: ms,
  }, ms);
  process.stdout.write(JSON.stringify(r, null, 2) + '\n');
}

async function runManualPermission(gw, args) {
  const tool = (collectFlag(args, 'tool')[0]) || 'Bash';
  const detail = (collectFlag(args, 'detail')[0]) || '';
  const ms = timeoutMs();
  const r = await postInteraction(gw, {
    kind: 'permission', source: 'manual', title: '手动测试 · 权限',
    detail, permission: { tool, rule: detail || '*', canAlways: true },
    context: { ...manualContext(args), tool, toolDetail: detail },
    timeoutMs: ms,
  }, ms);
  process.stdout.write(JSON.stringify(r, null, 2) + '\n');
}

// ---------------------------------------------------------------------------
// install：把 hook 写进各宿主配置（先备份）
// ---------------------------------------------------------------------------
function ownPath() {
  return env.POMODORO_HOOK_PATH || __filename;
}

// 各宿主配置里统一用「node "<脚本>" --source <来源>」：
// 显式标记来源，弹窗徽标才不会认错宿主（ZCode 与 Claude Code 协议同形）
function hookCommandShell(source) {
  return { type: 'command', command: `node "${ownPath()}" --source ${source}` };
}

function readJson(file) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch (e) { return {}; }
}
function writeJson(file, obj, print) {
  if (print) {
    process.stdout.write(`--- ${file} ---\n${JSON.stringify(obj, null, 2)}\n`);
    return;
  }
  try {
    if (fs.existsSync(file)) fs.copyFileSync(file, `${file}.pomodoro.bak`);
  } catch (e) { /* 备份失败不阻断安装 */ }
  // 写失败要抛出去，让 runInstall 记成失败并非零退出（一键安装靠退出码判断）
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, JSON.stringify(obj, null, 2));
  } catch (e) {
    throw new Error(`写入 ${file} 失败：${e && e.message ? e.message : e}`);
  }
  process.stdout.write(`已写入 ${file}（原文件备份为 ${path.basename(file)}.pomodoro.bak）\n`);
}

// 条目指纹：忽略 --source，同一脚本的重复安装视为同一条
function hookFingerprint(entry) {
  return (entry.hooks || [])
    .map((h) => `${h.command || ''} ${(h.args || []).join(' ')}`)
    .join('|')
    .replace(/--source\s+\S+/g, '')
    .replace(/["']/g, '')
    .trim();
}

// 往事件数组里追加一条；--clean 会先清掉指向别的副本的旧条目（避免弹两次窗）
let CLEAN_STALE = false;
function pushHook(container, eventName, entry) {
  let list = container[eventName] || [];
  if (CLEAN_STALE) {
    list = list.filter((g) => {
      const fp = hookFingerprint(g);
      return !/pomodoro-hook\.js/.test(fp) || fp.includes(ownPath());
    });
  }
  container[eventName] = list;
  const fp = hookFingerprint(entry);
  if (!list.some((g) => hookFingerprint(g) === fp)) list.push(entry);
}

// Claude Code / ZCode / VS Code（Claude 兼容格式）
function installAncliHooks(file, source, print) {
  const cfg = readJson(file);
  cfg.hooks = cfg.hooks || {};
  const h = hookCommandShell(source);
  const withTimeout = (e) => ({ ...e, timeout: HOST_WAIT_SEC });
  pushHook(cfg.hooks, 'Notification', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'PermissionRequest', { matcher: '*', hooks: [withTimeout({ ...h })] });
  pushHook(cfg.hooks, 'PreToolUse', { matcher: 'AskUserQuestion|askQuestions|askQuestion', hooks: [withTimeout({ ...h })] });
  // UserPromptSubmit：唯一携带「用户这次让 agent 干什么」的事件。
  // 工作报告的任务内容就来自这里（payload.prompt）——漏了它，任务名永远是空的。
  pushHook(cfg.hooks, 'UserPromptSubmit', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'Stop', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'SubagentStop', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'PostToolUse', { matcher: '*', hooks: [{ ...h }] });
  return cfg;
}

function installClaude(print) {
  const file = path.join(os.homedir(), '.claude', 'settings.json');
  writeJson(file, installAncliHooks(file, 'claude-code', print), print);
}

function installQwen(print) {
  const file = path.join(os.homedir(), '.qwen', 'settings.json');
  writeJson(file, installAncliHooks(file, 'qwen', print), print);
}

// VS Code Copilot Agent hooks：与 Claude Code 同格式，但事件集只有 8 个
// （SessionStart / UserPromptSubmit / PreToolUse / PostToolUse / PreCompact /
//   SubagentStart / SubagentStop / Stop），**没有** PermissionRequest 与 Notification。
// 用户级放 ~/.copilot/hooks/*.json；条目用 timeout（单位：秒，默认 30）。
// 长轮询等待用户在弹窗里点按钮，默认 30s 会直接被宿主掐断 → 显式放大到 HOST_WAIT_SEC。
function installVscode(print) {
  const file = path.join(os.homedir(), '.copilot', 'hooks', 'pomodoro.json');
  const cfg = readJson(file);
  cfg.version = cfg.version || 1;
  cfg.hooks = cfg.hooks || {};
  const cmd = `node "${ownPath()}" --source vscode`;
  const entry = (extra) => ({ type: 'command', command: cmd, ...(extra || {}) });
  const add = (evt, extra) => {
    const list = cfg.hooks[evt] || (cfg.hooks[evt] = []);
    if (!list.some((e) => /pomodoro-hook\.js/.test(String(e.command || '')))) list.push(entry(extra));
  };
  // 提问靠 PreToolUse（VS Code 没有 PermissionRequest）；2026-09-18 起不再做工具审批
  // 提问要等用户点弹窗 → 超时必须放大（VS Code 默认 30 秒，会直接掐掉）
  add('PreToolUse', { timeout: HOST_WAIT_SEC });
  add('PostToolUse', { timeout: 30 });
  add('SessionStart', { timeout: 30 });
  add('UserPromptSubmit', { timeout: 30 });
  add('SubagentStart', { timeout: 30 });
  add('SubagentStop', { timeout: 30 });
  add('PreCompact', { timeout: 30 });
  add('Stop', { timeout: 30 });
  writeJson(file, cfg, print);
  if (!print) {
    process.stdout.write(
      '提示：VS Code 默认还会读取 ~/.claude/settings.json（Claude Code 的 hooks），\n' +
      '      两处都装会对同一次工具调用跑两遍。不想重复就在 VS Code 设置里加：\n' +
      '      "chat.hookFilesLocations": { "~/.claude/settings.json": false }\n'
    );
  }
}

// Trae（字节）：6 个事件 SessionStart / UserPromptSubmit / PreToolUse / PostToolUse /
// Stop / Notification —— **有 Notification，但没有 PermissionRequest**。
// 2026-09-18 起 PreToolUse 不再做工具审批，只处理提问（AskUserQuestion）→
// matcher 也收窄到提问工具，活动上报交给 PostToolUse。
// 配置是 Claude Code 那种嵌套结构（event → [{matcher, hooks:[{type,command,timeout}]}]），
// 全局放 %userprofile%/.trae-cn/hooks.json，项目级放 $PROJECT/.trae/hooks.json。
const TRAE_PRETOOL_MATCHER = 'AskUserQuestion';

function installTrae(print) {
  const file = path.join(os.homedir(), '.trae-cn', 'hooks.json');
  const cfg = readJson(file);
  cfg.version = 1;
  cfg.hooks = cfg.hooks || {};
  const h = hookCommandShell('trae');
  const withTimeout = (e) => ({ ...e, timeout: HOST_WAIT_SEC });
  const fastTimeout = (e) => ({ ...e, timeout: 30 });
  // PreToolUse 要等用户点弹窗 → 超时必须放大（Trae 默认 30 秒，会直接掐掉）
  pushHook(cfg.hooks, 'PreToolUse', { matcher: TRAE_PRETOOL_MATCHER, hooks: [withTimeout({ ...h })] });
  pushHook(cfg.hooks, 'Notification', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'Stop', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'SessionStart', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'UserPromptSubmit', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'PostToolUse', { matcher: '*', hooks: [fastTimeout({ ...h })] });
  writeJson(file, cfg, print);
  if (!print) {
    process.stdout.write(
      'Trae 两条注意事项：\n' +
      '  1) 创建 Hook 时选「本地自动运行」而非「沙箱运行」—— 沙箱会限制系统权限，\n' +
      '     hook 可能连不上本机 127.0.0.1:5277 的番茄钟网关（连不上就静默跳过，不弹窗）。\n' +
      '  2) Trae 会同时读 Claude Code 的 Hook 配置并合并执行；若 ~/.claude/settings.json\n' +
      '     里也有番茄钟的 hook，同一次调用会跑两遍。二选一，或用 --clean 收敛。\n'
    );
  }
}

// Cursor（~/.cursor/hooks.json，用户级；项目级可放 .cursor/hooks.json）
function installCursor(print) {
  const file = path.join(os.homedir(), '.cursor', 'hooks.json');
  const cfg = readJson(file);
  cfg.version = cfg.version || 1;
  cfg.hooks = cfg.hooks || {};
  const cmd = `node "${ownPath()}" --source cursor`;
  const add = (evt, extra) => {
    const list = cfg.hooks[evt] || (cfg.hooks[evt] = []);
    if (!list.some((e) => /pomodoro-hook\.js/.test(String(e.command || '')))) {
      list.push({ command: cmd, ...(extra || {}) });
    }
  };
  add('beforeShellExecution', { timeout: HOST_WAIT_SEC });
  add('preToolUse', { timeout: HOST_WAIT_SEC });
  add('beforeMCPExecution', { timeout: HOST_WAIT_SEC });
  add('beforeSubmitPrompt');
  add('afterFileEdit');
  add('afterShellExecution');
  add('afterAgentResponse');
  add('stop');
  writeJson(file, cfg, print);
}

// Codex CLI（codex-cli 0.154.x）：**12 个 hook 事件**，配置放 ~/.codex/hooks.json
// （或 config.toml 的内联 [hooks]；同一层两者都有会都加载并告警 → 只用 hooks.json）。
//
// 与其他宿主最大的不同：Codex **有独立的 PermissionRequest 事件**，而且只在「Codex 本来
// 就要问用户」时才触发 —— 这正是想要的语义，不用像 VS Code / Trae 那样在 PreToolUse 上猜
// 哪些调用会被宿主拦，也就没有「用户已在宿主设了自动允许、弹窗还在问」的误报。
//
// PreToolUse 这一侧只上报活动、不参与决策（allow/ask 在 Codex 上不生效，见
// PERMISSION_EVENT_ONLY_SOURCES）；Codex 的 matcher 是**真生效的正则**，所以先拿它收窄，
// 省掉每个工具调用都 spawn 一次进程。
//
// 注意：UserPromptSubmit 与 Stop 的 matcher 不生效，别给它们写 matcher。
const CODEX_TOOL_MATCHER = 'Bash|apply_patch|Edit|Write|mcp__.*';

// 老的 notify 通道（config.toml）：回合结束回调一次。默认不改写它 —— 用户可能已经把它
// 指向别的工具（例如 codex-computer-use）。要加用 install --with-notify。
let WITH_NOTIFY = false;

// 用户可能在 config.toml 里把 hooks 关了（[features] hooks = false）
function codexHooksDisabled() {
  try {
    const t = fs.readFileSync(path.join(os.homedir(), '.codex', 'config.toml'), 'utf8');
    const seg = t.split(/^\s*\[/m).find((s) => /^features\]/.test(s.trim()));
    return !!seg && /^\s*(codex_)?hooks\s*=\s*false/m.test(seg);
  } catch (e) { return false; }
}

// config.toml 里已经有内联 [hooks] → 会与 hooks.json 双重加载并告警
function codexInlineHooks() {
  try {
    const t = fs.readFileSync(path.join(os.homedir(), '.codex', 'config.toml'), 'utf8');
    return /^\s*\[hooks\]|^\s*\[\[hooks\./m.test(t);
  } catch (e) { return false; }
}

function installCodex(print) {
  const file = path.join(os.homedir(), '.codex', 'hooks.json');
  const cfg = readJson(file);
  cfg.hooks = cfg.hooks || {};
  const h = hookCommandShell('codex');
  const withTimeout = (e) => ({ ...e, timeout: HOST_WAIT_SEC });
  const fastTimeout = (e) => ({ ...e, timeout: 30 });
  // 审批：Codex 唯一认 allow/deny 的地方，要等用户点弹窗 → timeout 必须放大
  pushHook(cfg.hooks, 'PermissionRequest', { hooks: [withTimeout({ ...h })] });
  // 以下都只上报，不回决策
  pushHook(cfg.hooks, 'PreToolUse', { matcher: CODEX_TOOL_MATCHER, hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'PostToolUse', { matcher: CODEX_TOOL_MATCHER, hooks: [fastTimeout({ ...h })] });
  // UserPromptSubmit：唯一带「用户这次让 agent 干什么」的事件（payload.prompt）
  pushHook(cfg.hooks, 'UserPromptSubmit', { hooks: [fastTimeout({ ...h })] });
  // Stop / SubagentStop：只上报，绝不回 decision:"block"（那会把 agent 拖进自动续跑）
  pushHook(cfg.hooks, 'Stop', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'SubagentStart', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'SubagentStop', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'SessionStart', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'SessionEnd', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'PreCompact', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'PostCompact', { hooks: [fastTimeout({ ...h })] });
  pushHook(cfg.hooks, 'Interrupt', { hooks: [fastTimeout({ ...h })] });
  writeJson(file, cfg, print);
  if (print) {
    // 干跑也要把将要写入的 notify 显示出来，否则 --print 会漏报这次改动
    if (WITH_NOTIFY) installCodexNotify(print);
    return;
  }

  const notes = [
    'Codex 注意事项：',
    `  1) 配置在 ${file}（用户级，不受项目信任影响）；项目级 .codex/ 只在项目被信任后加载。`,
    '     同一层里 hooks.json 与 config.toml 的 [hooks] 同时存在会两条都跑并告警 → 二选一。',
    '  2) 审批走 PermissionRequest（只在 Codex 本来就要问时才触发）。PreToolUse 只上报活动：',
    '     它的 allow/ask 在 Codex 上不生效，只有 deny 有效（带非空 reason）。',
    '  3) 本命令不改写 config.toml 的 notify，避免覆盖你已有的通知工具；',
    '     需要旧的回合结束回调时加：install --agent codex --with-notify',
  ];
  if (codexHooksDisabled()) {
    notes.push('  ⚠ 你的 ~/.codex/config.toml 里写了 [features] hooks = false —— hooks 被关了，' +
      '\n     弹窗不会出现。删掉这行或改成 true 再试。');
  }
  if (codexInlineHooks()) {
    notes.push('  ⚠ 你的 ~/.codex/config.toml 里已有内联 [hooks] 段 —— 会与本次写入的 hooks.json' +
      '\n     同时加载并告警，同一次调用可能跑两遍。建议二选一。');
  }
  process.stdout.write(notes.join('\n') + '\n');

  if (WITH_NOTIFY) installCodexNotify(print);
}

// Codex CLI 的老通道（~/.codex/config.toml 的 notify）：回合结束回调一次
function installCodexNotify(print) {
  const file = path.join(os.homedir(), '.codex', 'config.toml');
  let text = '';
  try { text = fs.readFileSync(file, 'utf8'); } catch (e) { text = ''; }
  const hookArg = ownPath().replace(/\\/g, '\\\\');
  const line = `notify = ["node", "${hookArg}", "codex-notify"]`;
  const lines = text.split(/\r?\n/);
  const idx = lines.findIndex((l) => /^\s*notify\s*=/.test(l));
  if (idx >= 0) lines[idx] = line;
  else {
    while (lines.length && lines[lines.length - 1].trim() === '') lines.pop();
    if (lines.length) lines.push('');
    lines.push('# 番茄钟：回合结束时通知（由 pomodoro-hook.js install 写入）');
    lines.push(line);
  }
  const next = lines.join('\n') + '\n';
  if (print) {
    process.stdout.write(`--- ${file} ---\n${next}\n`);
    return;
  }
  try {
    if (fs.existsSync(file)) fs.copyFileSync(file, `${file}.pomodoro.bak`);
  } catch (e) { /* ignore */ }
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, next);
  process.stdout.write(`已写入 ${file}（原文件备份为 config.toml.pomodoro.bak）\n`);
}

function installZcode(print) {
  const file = path.join(os.homedir(), '.zcode', 'cli', 'config.json');
  const cfg = readJson(file);
  cfg.hooks = cfg.hooks || {};
  cfg.hooks.enabled = true;                       // 必须显式开启
  cfg.hooks.timeoutMs = cfg.hooks.timeoutMs || HOST_WAIT_MS;
  const events = cfg.hooks.events || (cfg.hooks.events = {});
  const h = hookCommandShell('zcode');
  const long = (e) => ({ ...e, timeoutMs: HOST_WAIT_MS });
  pushHook(events, 'PermissionRequest', { matcher: '*', hooks: [long(h)] });
  pushHook(events, 'PreToolUse', { matcher: 'AskUserQuestion', hooks: [long(h)] });
  pushHook(events, 'Stop', { hooks: [h] });
  pushHook(events, 'PostToolUse', { matcher: '*', hooks: [h] });
  pushHook(events, 'PostToolUseFailure', { matcher: '*', hooks: [h] });
  writeJson(file, cfg, print);
}

function installOpencode(print) {
  const home = os.homedir();
  const pluginSrc = path.join(__dirname, 'opencode', 'pomodoro-opencode.ts');
  const cfgDir = env.XDG_CONFIG_HOME ? path.join(env.XDG_CONFIG_HOME, 'opencode') : path.join(home, '.config', 'opencode');
  const pluginDst = path.join(cfgDir, 'plugins', 'pomodoro-opencode.ts');
  const cfgFile = path.join(cfgDir, 'opencode.json');

  if (!fs.existsSync(pluginSrc)) {
    throw new Error(`找不到插件源文件: ${pluginSrc}`);
  }
  if (print) {
    process.stdout.write(`--- 复制 ${pluginSrc} → ${pluginDst} ---\n`);
  } else {
    fs.mkdirSync(path.dirname(pluginDst), { recursive: true });
    fs.copyFileSync(pluginSrc, pluginDst);
    process.stdout.write(`已安装插件 ${pluginDst}\n`);
  }

  const cfg = readJson(cfgFile);
  const entry = `file://${pluginDst.replace(/\\/g, '/')}`;
  cfg.plugin = Array.isArray(cfg.plugin) ? cfg.plugin : [];
  if (!cfg.plugin.includes(entry)) cfg.plugin.push(entry);
  writeJson(cfgFile, cfg, print);
}

// 支持的宿主列表（install 与帮助文本共用一份，别两处各写一遍）
const HOOK_AGENT_NAMES = ['zcode', 'claude', 'vscode', 'trae', 'cursor', 'opencode', 'codex', 'qwen'];

function runInstall(args) {
  const print = args.includes('--print');
  CLEAN_STALE = args.includes('--clean');
  WITH_NOTIFY = args.includes('--with-notify');
  const idx = args.indexOf('--agent');
  const agent = (idx >= 0 && args[idx + 1]) || 'all';

  // 未知宿主必须非零退出：设置面板的「一键安装」靠退出码判断成败，
  // 以前这里只往 stderr 写一行就继续，会被误报成"安装完成"
  if (agent !== 'all' && !HOOK_AGENT_NAMES.includes(agent)) {
    process.stderr.write(`[pomodoro-hook] 未知 agent: ${agent}（可选：${HOOK_AGENT_NAMES.join(' / ')} / all）\n`);
    process.exitCode = 1;
    return;
  }

  const targets = agent === 'all' ? HOOK_AGENT_NAMES : [agent];
  const failed = [];
  targets.forEach((t) => {
    try {
      if (t === 'claude') installClaude(print);
      else if (t === 'zcode') installZcode(print);
      else if (t === 'vscode') installVscode(print);
      else if (t === 'trae') installTrae(print);
      else if (t === 'cursor') installCursor(print);
      else if (t === 'opencode') installOpencode(print);
      else if (t === 'codex') installCodex(print);
      else if (t === 'qwen') installQwen(print);
    } catch (e) {
      failed.push(t);
      process.stderr.write(`[pomodoro-hook] 安装 ${t} 失败：${e && e.message ? e.message : e}\n`);
    }
  });

  // 写盘失败也必须非零退出（权限、磁盘满、目录被占用…），否则前端会误报成功
  if (failed.length) {
    process.stderr.write(`[pomodoro-hook] 有 ${failed.length} 个宿主安装失败：${failed.join('、')}\n`);
    process.exitCode = 1;
    return;
  }
  if (!print) process.stdout.write('改动需重启对应 agent 会话后生效。\n');
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------
function usage() {
  process.stdout.write([
    `番茄钟 Agent Hook CLI v${HOOK_VERSION}`,
    '用法:',
    '  pomodoro-hook.js                       # hook 模式：stdin JSON（自动识别宿主协议）',
    '  pomodoro-hook.js codex-notify          # Codex CLI：回合结束通知（JSON 走参数或 stdin）',
    '  pomodoro-hook.js opencode-permission   # OpenCode 插件：权限请求 → stdout {status}',
    '  pomodoro-hook.js opencode-question     # OpenCode 插件：提问 → stdout {answers}',
    '  pomodoro-hook.js opencode-event        # OpenCode 插件：会话事件上报',
    '  pomodoro-hook.js ask --question "Q" --option A --option B [--task "任务" --agent zcode]',
    '  pomodoro-hook.js permission --tool Bash --detail "npm test" [--task "任务"]',
    '  pomodoro-hook.js notify --title T --message M [--sub S] [--type agent]',
    '  pomodoro-hook.js status',
    '  pomodoro-hook.js sessions               # 查看 hook 跟踪到的会话（任务/最近工具/项目）',
    '  pomodoro-hook.js install --agent <宿主> [--print] [--clean] [--with-notify]',
    '',
    '  宿主：zcode / claude / vscode / trae / cursor / opencode / codex / qwen / all',
    '  --print        只打印将要写入的配置，不动文件',
    '  --clean        同时清掉指向番茄钟 hook 其它副本的旧条目',
    '  --with-notify  仅 Codex：额外改写 config.toml 的 notify（默认不动）',
  ].join('\n') + '\n');
}

async function main() {
  const rawArgs = process.argv.slice(2);
  // --source <宿主>：install 生成的命令会带上，用来显式标记来源
  const si = rawArgs.indexOf('--source');
  if (si >= 0 && rawArgs[si + 1]) {
    process.env.POMODORO_SOURCE = rawArgs[si + 1];
    rawArgs.splice(si, 2);
  }
  const args = rawArgs;

  // 这两个不需要网关在线
  if (args[0] === '--help' || args[0] === '-h' || args[0] === 'help') { usage(); return; }
  if (args[0] === 'install') { runInstall(args.slice(1)); return; }

  const gw = findGateway();
  if (!gw) {
    // 番茄钟未运行：hook 场景静默退出（不阻断 agent），手动调用给提示
    if (args.length > 0) process.stderr.write('[pomodoro-hook] 找不到番茄钟网关（应用未启动？）\n');
    return;
  }

  if (args.length === 0) {
    const raw = await readStdin();
    const payload = parseJsonMaybe(raw);
    const protocol = detectProtocol(payload);
    if (protocol === 'cursor') {
      await runCursorMode(gw, payload);
      return;
    }
    if (protocol === 'opencode-permission') {
      const out = await runOpenCodePermission(gw, payload);
      process.stdout.write(JSON.stringify(out));
      return;
    }
    if (protocol === 'opencode-question') {
      const out = await runOpenCodeQuestion(gw, payload);
      await finishOpenCodeQuestion(payload, out);
      return;
    }
    if (protocol === 'opencode-event') {
      await runOpenCodeEvent(gw, payload);
      return;
    }
    await runAncliMode(gw, payload);
    return;
  }

  const cmd = args[0];
  if (cmd === 'opencode-permission') {
    const raw = await readStdin();
    const out = await runOpenCodePermission(gw, parseJsonMaybe(raw));
    process.stdout.write(JSON.stringify(out));
    return;
  }
  if (cmd === 'opencode-question') {
    const raw = await readStdin();
    const payload = parseJsonMaybe(raw);
    const out = await runOpenCodeQuestion(gw, payload);
    await finishOpenCodeQuestion(payload, out);
    return;
  }
  if (cmd === 'opencode-event') {
    const raw = await readStdin();
    await runOpenCodeEvent(gw, parseJsonMaybe(raw));
    return;
  }

  // Codex 的 notify 会把 JSON 当参数传过来（也可能走 stdin，两种都支持）
  if (cmd === 'codex-notify') {
    const inline = args.slice(1).reverse().find((a) => a.trim().startsWith('{'));
    let payload = inline ? parseJsonMaybe(inline) : null;
    if (!payload) {
      try { payload = parseJsonMaybe(await readStdin()); } catch (e) { payload = {}; }
    }
    await runCodexNotify(gw, payload);
    return;
  }

  if (cmd === 'status') {
    const r = await request(gw.port, gw.token, 'GET', '/api/status', null, 8000);
    process.stdout.write(JSON.stringify(r, null, 2) + '\n');
    return;
  }

  // 看看 hook 都跟踪到了哪些会话（任务 / 最近工具 / 项目）
  if (cmd === 'sessions') {
    const list = listSessions().map((s) => ({
      session: shortSession(s.sessionId),
      source: s.source || '',
      project: s.project || (s.cwd ? path.basename(s.cwd) : ''),
      task: s.task || '',
      lastTool: s.lastTool || '',
      lastToolDetail: s.lastToolDetail || '',
      agentType: s.agentType || '',
      since: s.at ? new Date(s.at).toLocaleString() : '',
    }));
    process.stdout.write(JSON.stringify(list, null, 2) + '\n');
    return;
  }

  if (cmd === 'ask') { await runManualAsk(gw, args.slice(1)); return; }
  if (cmd === 'permission') { await runManualPermission(gw, args.slice(1)); return; }

  if (cmd === 'notify') {
    const opts = {};
    for (let i = 1; i < args.length - 1; i++) {
      if (args[i].startsWith('--')) opts[args[i].slice(2)] = args[i + 1];
    }
    const r = await request(gw.port, gw.token, 'POST', '/api/notify', {
      title: opts.title || '番茄钟',
      message: opts.message || '',
      sub: opts.sub || '',
      type: opts.type || 'agent',
    }, 8000);
    if (!r || !r.ok) throw new Error('notify 失败');
    return;
  }

  usage();
  process.exitCode = 1;
}

main().catch((e) => {
  // 任何失败都不阻断 agent：stderr 留痕，exit 0
  process.stderr.write(`[pomodoro-hook] ${e.message}\n`);
});
