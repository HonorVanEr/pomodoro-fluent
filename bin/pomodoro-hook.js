#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 番茄钟 Agent Hook CLI
//
// 与运行中的番茄钟（本地 Agent 网关）联动，把 agent 事件变成弹窗。
// 定位网关：读 %APPDATA%/番茄钟/gateway.json（番茄钟启动时写入，
// 含端口与 token）。番茄钟未运行时静默退出，绝不阻断 agent。
//
// 用法：
//   hook 模式（默认，读 stdin JSON，Claude Code 等 hook 协议）：
//     node pomodoro-hook.js
//   直连模式：
//     node pomodoro-hook.js notify --title "标题" --message "内容" [--sub "附注"]
//     node pomodoro-hook.js status
//
// 环境变量：
//   POMODORO_GATEWAY_FILE    指定 gateway.json 路径
//   POMODORO_PORT / POMODORO_TOKEN   直接指定网关端口与 token
//   POMODORO_SOURCE          事件来源标记（默认按参数推断 claude-code）
//   POMODORO_CONFIRM_PRETOOL=1  开启 PreToolUse 双向确认（弹窗允许/拒绝）
//   POMODORO_CONFIRM_TIMEOUT_S   双向确认等待秒数（默认 240）
// ---------------------------------------------------------------------------

const http = require('http');
const fs = require('fs');
const os = require('os');
const path = require('path');

const HOOK_VERSION = '1.0.0';

// ---- 网关发现 ----
function findGateway() {
  const port = Number(process.env.POMODORO_PORT) || 0;
  const token = process.env.POMODORO_TOKEN || '';
  if (port > 0 && token) return { port, token };

  const candidates = [];
  if (process.env.POMODORO_GATEWAY_FILE) {
    candidates.push(process.env.POMODORO_GATEWAY_FILE);
  } else {
    const appData = process.env.APPDATA || path.join(os.homedir(), 'AppData', 'Roaming');
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

// ---- 工具输入摘要（弹窗正文用） ----
function summarizeToolInput(toolInput) {
  if (!toolInput || typeof toolInput !== 'object') return '';
  const parts = [];
  for (const [k, v] of Object.entries(toolInput)) {
    let s = typeof v === 'string' ? v : JSON.stringify(v);
    if (s && s.length > 120) s = s.slice(0, 119) + '…';
    parts.push(`${k}: ${s}`);
    if (parts.length >= 3) break;
  }
  return parts.join('\n');
}

function pretoolDecision(action, decidedBy) {
  // 映射为 Claude Code PreToolUse 的 hookSpecificOutput；
  // 只有用户真实点击才算决策，超时/被顶掉一律回退 ask 交回终端原生前确认（最安全）
  const decided = decidedBy === 'user' && (action === 'allow' || action === 'deny');
  const decision = decided ? action : 'ask';
  const reason = decided
    ? `用户通过番茄钟弹窗选择：${decision === 'allow' ? '允许' : '拒绝'}`
    : '番茄钟弹窗未获决策，回退默认询问';
  return JSON.stringify({
    hookSpecificOutput: {
      hookEventName: 'PreToolUse',
      permissionDecision: decision,
      permissionDecisionReason: reason,
    },
  });
}

async function runHookMode(gw) {
  const raw = await readStdin();
  const payload = raw ? JSON.parse(raw) : {};
  const event = payload.hook_event_name || '';
  const source = process.env.POMODORO_SOURCE
    || (String(payload.source || '') === 'opencode' ? 'opencode' : 'claude-code');

  // PreToolUse：可选双向确认（长轮询等用户在弹窗上点允许/拒绝）
  if (event === 'PreToolUse' && process.env.POMODORO_CONFIRM_PRETOOL === '1') {
    const timeoutS = Math.min(Number(process.env.POMODORO_CONFIRM_TIMEOUT_S) || 240, 590);
    const tool = payload.tool_name || '工具';
    try {
      const r = await request(gw.port, gw.token, 'POST', '/api/confirm', {
        title: `允许 ${tool}？`,
        message: summarizeToolInput(payload.tool_input) || 'agent 请求执行该工具',
        sub: source,
        actions: [
          { id: 'allow', label: '允许', style: 'primary' },
          { id: 'deny', label: '拒绝', style: 'danger' },
        ],
        defaultAction: 'deny',
        timeoutMs: timeoutS * 1000,
      }, (timeoutS + 10) * 1000);
      process.stdout.write(pretoolDecision(r && r.action, r && r.decidedBy));
    } catch (e) {
      process.stderr.write(`[pomodoro-hook] confirm 失败: ${e.message}\n`);
    }
    return;
  }

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
  await request(gw.port, gw.token, 'POST', '/api/event', body, 8000);
}

function usage() {
  process.stderr.write([
    `番茄钟 Agent Hook CLI v${HOOK_VERSION}`,
    '用法:',
    '  pomodoro-hook.js                       # hook 模式：stdin JSON（Claude Code）',
    '  pomodoro-hook.js notify --title T --message M [--sub S] [--type agent]',
    '  pomodoro-hook.js status                # 查看番茄钟状态 JSON',
  ].join('\n') + '\n');
}

async function main() {
  const args = process.argv.slice(2);
  const gw = findGateway();
  if (!gw) {
    // 番茄钟未运行：hook 场景静默退出（不阻断 agent），手动调用给提示
    if (args.length > 0) process.stderr.write('[pomodoro-hook] 找不到番茄钟网关（应用未启动？）\n');
    return;
  }

  if (args.length === 0) {
    await runHookMode(gw);
    return;
  }

  const cmd = args[0];
  if (cmd === 'status') {
    const r = await request(gw.port, gw.token, 'GET', '/api/status', null, 8000);
    process.stdout.write(JSON.stringify(r, null, 2) + '\n');
    return;
  }

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
