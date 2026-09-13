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
//   POMODORO_SOURCE            来源标记：zcode | claude-code | opencode
//   POMODORO_ASK               1/0，AskUserQuestion 是否接管（默认 1）
//   POMODORO_PERMISSION        1/0，PermissionRequest 是否接管（默认 1）
//   POMODORO_CONFIRM_PRETOOL   1 开启普通 PreToolUse 双向确认（默认 0）
//   POMODORO_ALWAYS_ALLOW      0 隐藏「始终允许」按钮（默认 1）
//   POMODORO_PERMISSION_DEST   始终允许写哪里（默认 projectSettings）
//   POMODORO_TIMEOUT_S         弹窗等待秒数（默认 240，上限 590）
//   POMODORO_ASK_MODE          answers | deny（默认 answers）
//   POMODORO_HOOK_PATH         OpenCode 插件定位本脚本用
// ---------------------------------------------------------------------------

const http = require('http');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');

const HOOK_VERSION = '2.0.0';

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
  const cwd = payload.cwd || payload.working_directory;
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
      custom: q && typeof q.custom === 'boolean' ? q.custom : options.length === 0,
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
function timeoutMs() {
  const s = Math.min(Number(env.POMODORO_TIMEOUT_S) || 240, 590);
  return Math.max(5, Math.round(s)) * 1000;
}

async function postInteraction(gw, body, ms) {
  return request(gw.port, gw.token, 'POST', '/api/interaction', body, ms + 20000);
}

async function postEvent(gw, body) {
  return request(gw.port, gw.token, 'POST', '/api/event', body, 8000);
}

// ---------------------------------------------------------------------------
// 协议识别
// ---------------------------------------------------------------------------
const ASK_TOOL_RE = /^(AskUserQuestion|AskUser|ask_user_question|question)$/i;

function isAskTool(payload) {
  const tool = String(payload.tool_name || '');
  if (ASK_TOOL_RE.test(tool)) return true;
  const ti = payload.tool_input;
  return !!(ti && Array.isArray(ti.questions) && ti.questions.length && !ti.command && !ti.file_path);
}

function detectSource(payload) {
  if (env.POMODORO_SOURCE) return String(env.POMODORO_SOURCE);
  if (payload && payload.source) return String(payload.source);
  if (env.ZCODE_PLUGIN_ROOT || env.ZCODE_CLI_HOME) return 'zcode';
  try {
    if (fs.existsSync(path.join(os.homedir(), '.zcode'))) return 'zcode';
  } catch (e) { /* ignore */ }
  return 'claude-code';
}

function detectProtocol(payload) {
  if (payload && typeof payload.hook_event_name === 'string') return 'ancli'; // Claude Code / ZCode 同族
  if (payload && payload.permission && typeof payload.permission === 'object') return 'opencode-permission';
  if (payload && Array.isArray(payload.questions)) return 'opencode-question';
  if (payload && typeof payload.event === 'string') return 'opencode-event';
  return 'ancli';
}

// ---------------------------------------------------------------------------
// Claude Code / ZCode（事件名一致，输出同形）
// ---------------------------------------------------------------------------
async function handleAsk(payload, source, gw, context) {
  const toolInput = payload.tool_input || {};
  const questions = questionsFromToolInput(toolInput);
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
    if (String(env.POMODORO_ASK_MODE || 'answers').toLowerCase() === 'deny') {
      // 备用通道：部分宿主不认 updatedInput.answers，改成 deny + 把答案塞进原因
      return {
        hookSpecificOutput: {
          hookEventName: 'PreToolUse',
          permissionDecision: 'deny',
          permissionDecisionReason: `用户在番茄钟弹窗中的回答：\n${formatAnswerMap(answers)}`,
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
  const ms = timeoutMs();
  const result = await postInteraction(gw, {
    kind: 'permission',
    source,
    title: `允许 ${tool}？`,
    message: payload.message || '',
    detail: summarizeToolInput(toolInput),
    permission: {
      tool,
      rule: ruleContentFor(tool, toolInput),
      suggestions: payload.permission_suggestions || [],
      canAlways: flag('POMODORO_ALWAYS_ALLOW', true) !== false,
    },
    context: { ...(context || {}), tool, toolDetail: collapse(ruleContentFor(tool, toolInput), 160) },
    timeoutMs: ms,
  }, ms);

  if (!result) return null;
  const decided = result.decidedBy === 'user';
  const event = payload.hook_event_name;

  if (decided && result.action === 'allow') {
    const out = { behavior: 'allow', message: result.text || '用户通过番茄钟弹窗允许' };
    if (event === 'PermissionRequest') return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: out } };
    return { hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'allow', permissionDecisionReason: out.message } };
  }

  if (decided && result.action === 'allow-always') {
    const updatedPermissions = buildPermissionUpdates(tool, toolInput, payload.permission_suggestions);
    if (event === 'PermissionRequest') {
      return {
        hookSpecificOutput: {
          hookEventName: 'PermissionRequest',
          decision: { behavior: 'allow', message: '用户选择始终允许', updatedPermissions },
        },
      };
    }
    return {
      hookSpecificOutput: {
        hookEventName: 'PreToolUse',
        permissionDecision: 'allow',
        permissionDecisionReason: '用户选择始终允许',
        updatedPermissions,
      },
    };
  }

  if (decided && result.action === 'deny') {
    const message = result.text ? `用户拒绝：${result.text}` : '用户通过番茄钟弹窗拒绝';
    if (event === 'PermissionRequest') return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'deny', message } } };
    return { hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: message } };
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

  // 普通 PreToolUse 双向确认（可选开启，默认关）
  if (event === 'PreToolUse' && flag('POMODORO_CONFIRM_PRETOOL', false) === true) {
    const out = await handlePermission(payload, source, gw, context);
    if (out) process.stdout.write(JSON.stringify(out));
    return;
  }

  // 其余事件：只上报计数/通知
  const bodyByEvent = {
    Notification: { kind: 'notification', message: payload.message || '', source },
    Stop: { kind: 'stop', source },
    SubagentStop: { kind: 'subagent-stop', source },
    PostToolUse: { kind: 'tool-after', tool: payload.tool_name || '', source },
    PreToolUse: { kind: 'tool-before', tool: payload.tool_name || '', source },
    SessionStart: { kind: 'session-start', source },
    SessionEnd: { kind: 'session-end', source },
    UserPromptSubmit: { kind: 'prompt', source },
  };
  const body = bodyByEvent[event];
  if (!body) return; // 未识别的事件：静默忽略
  if (event === 'Notification' && !body.message) body.message = 'Agent 需要你的确认';
  // 通知类也带上上下文：弹窗能显示是哪个任务、在动哪个工具
  if (event === 'Notification' || event === 'Stop') body.context = context;
  await postEvent(gw, body);
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
  } catch (e) { /* ignore */ }
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(obj, null, 2));
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

function installClaude(print) {
  const file = path.join(os.homedir(), '.claude', 'settings.json');
  const cfg = readJson(file);
  cfg.hooks = cfg.hooks || {};
  const h = hookCommandShell('claude-code');
  const withTimeout = (e) => ({ ...e, timeout: 600 });
  pushHook(cfg.hooks, 'Notification', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'PermissionRequest', { matcher: '*', hooks: [withTimeout({ ...h })] });
  pushHook(cfg.hooks, 'PreToolUse', { matcher: 'AskUserQuestion', hooks: [withTimeout({ ...h })] });
  pushHook(cfg.hooks, 'Stop', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'SubagentStop', { hooks: [{ ...h }] });
  pushHook(cfg.hooks, 'PostToolUse', { matcher: '*', hooks: [{ ...h }] });
  writeJson(file, cfg, print);
}

function installZcode(print) {
  const file = path.join(os.homedir(), '.zcode', 'cli', 'config.json');
  const cfg = readJson(file);
  cfg.hooks = cfg.hooks || {};
  cfg.hooks.enabled = true;                       // 必须显式开启
  cfg.hooks.timeoutMs = cfg.hooks.timeoutMs || 600000;
  const events = cfg.hooks.events || (cfg.hooks.events = {});
  const h = hookCommandShell('zcode');
  const long = (e) => ({ ...e, timeoutMs: 600000 });
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
    process.stderr.write(`[pomodoro-hook] 找不到插件源文件: ${pluginSrc}\n`);
    return;
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

function runInstall(args) {
  const print = args.includes('--print');
  CLEAN_STALE = args.includes('--clean');
  const idx = args.indexOf('--agent');
  const agent = (idx >= 0 && args[idx + 1]) || 'all';
  const targets = agent === 'all' ? ['zcode', 'claude', 'opencode'] : [agent];
  targets.forEach((t) => {
    if (t === 'claude') installClaude(print);
    else if (t === 'zcode') installZcode(print);
    else if (t === 'opencode') installOpencode(print);
    else process.stderr.write(`[pomodoro-hook] 未知 agent: ${t}\n`);
  });
  if (!print) process.stdout.write('改动需重启对应 agent 会话后生效。\n');
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------
function usage() {
  process.stdout.write([
    `番茄钟 Agent Hook CLI v${HOOK_VERSION}`,
    '用法:',
    '  pomodoro-hook.js                       # hook 模式：stdin JSON（自动识别 ZCode/Claude Code/OpenCode）',
    '  pomodoro-hook.js opencode-permission   # OpenCode 插件：权限请求 → stdout {status}',
    '  pomodoro-hook.js opencode-question     # OpenCode 插件：提问 → stdout {answers}',
    '  pomodoro-hook.js opencode-event        # OpenCode 插件：会话事件上报',
    '  pomodoro-hook.js ask --question "Q" --option A --option B [--task "任务" --agent zcode]',
    '  pomodoro-hook.js permission --tool Bash --detail "npm test" [--task "任务"]',
    '  pomodoro-hook.js notify --title T --message M [--sub S] [--type agent]',
    '  pomodoro-hook.js status',
    '  pomodoro-hook.js sessions               # 查看 hook 跟踪到的会话（任务/最近工具/项目）',
    '  pomodoro-hook.js install --agent zcode|claude|opencode|all [--print] [--clean]',
    '',
    '  --print  只打印将要写入的配置，不动文件',
    '  --clean  同时清掉指向番茄钟 hook 其它副本的旧条目',
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
