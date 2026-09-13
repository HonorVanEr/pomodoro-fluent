'use strict';

// ---------------------------------------------------------------------------
// 交互链路自测（纯 Node，不需要 Electron）
//
//   1. 用桩依赖起网关，模拟「用户在弹窗里点按钮」
//   2. 直接打 HTTP 验证 /api/interaction 的 ask / permission / notification
//   3. 真跑一次 bin/pomodoro-hook.js，验证 ZCode/Claude Code 的
//      PermissionRequest / AskUserQuestion 与 OpenCode 子命令的 stdout 协议
//
// 用法：node scripts/smoke-interaction.js
// ---------------------------------------------------------------------------

const path = require('path');
const fs = require('fs');
const os = require('os');
const { spawn } = require('child_process');
const { createGateway } = require('../gateway');

const ROOT = path.join(__dirname, '..');
const HOOK = path.join(ROOT, 'bin', 'pomodoro-hook.js');
const USER_DATA = fs.mkdtempSync(path.join(os.tmpdir(), 'pomodoro-smoke-'));

let lastPopup = null;
const popups = [];
// 弹窗脚本：按 kind 指定用户会点什么
let plan = { permission: 'allow', ask: 'submit', askOption: 0, text: '' };

const gateway = createGateway({
  showPopup: (payload) => {
    lastPopup = payload;
    popups.push(payload);
    const id = payload.id;
    if (payload.title === '[timeout-probe]') return; // 该用例故意不回应，验证超时兜底
    if (!id) return; // 纯通知，无需回应
    if (payload.kind === 'permission') {
      setTimeout(() => gateway.resolveInteraction(id, { action: plan.permission, answers: {}, text: plan.text }, 'user'), 30);
    } else if (payload.kind === 'ask') {
      if (plan.ask === 'cancel') {
        setTimeout(() => gateway.resolveInteraction(id, { action: 'cancel', answers: {}, text: '' }, 'user'), 30);
      } else {
        const answers = {};
        payload.questions.forEach((q, i) => {
          const opt = q.options[plan.askOption];
          answers[q.id] = opt ? [opt.label] : ['自定义回答'];
        });
        setTimeout(() => gateway.resolveInteraction(id, { action: 'submit', answers, text: '' }, 'user'), 30);
      }
    } else {
      const first = payload.actions[0];
      setTimeout(() => gateway.resolveInteraction(id, { action: first ? first.id : 'ok', answers: {}, text: '' }, 'user'), 30);
    }
  },
  sendTimerCommand: () => {},
  onActivity: () => {},
  getTimerState: () => ({ phase: 'work', running: true, remainMs: 600000, totalMs: 1500000 }),
  getUserDataPath: () => USER_DATA,
  log: () => {},
});

// ---- 小工具 ----
let passed = 0;
let failed = 0;
function ok(name, cond, extra) {
  if (cond) { passed++; console.log(`  PASS ${name}`); }
  else { failed++; console.log(`  FAIL ${name}${extra ? `\n       ${JSON.stringify(extra)}` : ''}`); }
}

function token() {
  return JSON.parse(fs.readFileSync(path.join(USER_DATA, 'gateway.json'), 'utf8')).token;
}

function post(apiPath, body, timeoutMs = 15000) {
  return new Promise((resolve, reject) => {
    const http = require('http');
    const data = JSON.stringify(body || {});
    const req = http.request({
      host: '127.0.0.1', port: gateway.getPort(), method: 'POST', path: apiPath,
      headers: {
        Authorization: `Bearer ${token()}`,
        'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data),
      },
      timeout: timeoutMs,
    }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); }
        catch (e) { reject(e); }
      });
    });
    req.on('error', reject);
    req.on('timeout', () => { req.destroy(); reject(new Error('timeout')); });
    req.write(data);
    req.end();
  });
}

function getStatus() {
  return new Promise((resolve, reject) => {
    const http = require('http');
    http.get({
      host: '127.0.0.1', port: gateway.getPort(), path: '/api/status',
      headers: { Authorization: `Bearer ${token()}` },
    }, (r2) => {
      const chunks = [];
      r2.on('data', (c) => chunks.push(c));
      r2.on('end', () => resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))));
    }).on('error', reject);
  });
}

function runHook(stdinPayload, extraArgs = [], env = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [HOOK, ...extraArgs], {
      cwd: ROOT,
      env: {
        ...process.env,
        POMODORO_PORT: String(gateway.getPort()),
        POMODORO_TOKEN: token(),
        POMODORO_TIMEOUT_S: '20',
        ...env,
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let out = '';
    let err = '';
    child.stdout.on('data', (c) => { out += c; });
    child.stderr.on('data', (c) => { err += c; });
    child.on('error', reject);
    child.on('close', () => resolve({ out, err }));
    child.stdin.end(JSON.stringify(stdinPayload));
  });
}

function parseOut(out) {
  try { return JSON.parse(out); } catch (e) { return null; }
}

// ---- 用例 ----
async function run() {
  await gateway.start();
  const port = gateway.getPort();
  // 先打一次 status：让网关完成「进入 work 阶段」的状态观察（计数会在那时清零）
  await getStatus();

  console.log('\n[1] 弹窗脚本：模拟用户点按钮');

  plan = { permission: 'allow-always', ask: 'submit', askOption: 0, text: '备注：放行' };
  let r = await post('/api/interaction', {
    kind: 'permission', source: 'zcode', title: '允许 Bash？',
    detail: 'command: npm test', permission: { tool: 'Bash', rule: 'npm test', canAlways: true },
  });
  ok('permission → 用户在弹窗点「始终允许」', r.action === 'allow-always' && r.decidedBy === 'user', r);
  ok('permission 备注回传', r.text === '备注：放行', r);

  r = await post('/api/interaction', {
    kind: 'ask', source: 'claude-code', title: 'Agent 提问',
    questions: [{
      id: 'q0', question: '用哪种方案？', header: '方案', multiSelect: false,
      options: [{ id: 'o0', label: 'A 方案' }, { id: 'o1', label: 'B 方案' }], custom: true,
    }],
  });
  ok('ask → 用户在弹窗选了选项', r.action === 'submit' && r.answers.q0 && r.answers.q0[0] === 'A 方案', r);

  plan = { permission: 'deny', ask: 'cancel' };
  r = await post('/api/interaction', { kind: 'permission', permission: { tool: 'Write', canAlways: false } });
  ok('permission → 用户拒绝', r.action === 'deny', r);
  r = await post('/api/interaction', {
    kind: 'ask', questions: [{ id: 'q0', question: '继续吗？', options: [{ id: 'o0', label: '继续' }] }],
  });
  ok('ask → 用户取消', r.action === 'cancel', r);

  const before = popups.length;
  r = await post('/api/interaction', { kind: 'notification', title: '纯通知', message: '不等待用户' });
  ok('notification → 立即返回不阻塞', r.action === 'shown' && popups.length === before + 1, r);

  r = await post('/api/event', { kind: 'permission', message: '事件通道降级通知', source: 'opencode' });
  ok('event(permission) → 降级为通知且计数打断', r.ok === true, r);
  r = await post('/api/event', { kind: 'ask', message: '提问降级', source: 'zcode' });
  ok('event(ask) → 降级为通知', r.ok === true, r);

  r = await post('/api/confirm', {
    title: '旧接口', actions: [{ id: 'yes', label: '好' }, { id: 'no', label: '不' }],
  });
  ok('旧 /api/confirm 仍可用', r.action === 'yes', r);

  // 超时兜底：桩不回应，5 秒后应落回 deny
  const timeoutProbe = await post('/api/interaction', {
    kind: 'permission', title: '[timeout-probe]', permission: { tool: 'Bash' }, timeoutMs: 5000,
  });
  ok('permission 超时 → 落回 deny（安全侧）',
    timeoutProbe.decidedBy === 'timeout' && timeoutProbe.action === 'deny', timeoutProbe);

  const askTimeout = await post('/api/interaction', {
    kind: 'ask', title: '[timeout-probe]',
    questions: [{ id: 'q0', question: '继续吗？', options: [{ id: 'o0', label: '继续' }] }],
    timeoutMs: 5000,
  });
  ok('ask 超时 → 落回 cancel（不替用户作答）',
    askTimeout.decidedBy === 'timeout' && askTimeout.action === 'cancel', askTimeout);

  console.log('\n[2] ZCode / Claude Code hook 协议（真跑 CLI）');

  plan = { permission: 'allow', ask: 'submit', askOption: 1, text: '' };
  let res = await runHook({
    hook_event_name: 'PermissionRequest',
    tool_name: 'Bash',
    tool_input: { command: 'npm test', description: 'run tests' },
    tool_use_id: 'tool-1',
    permission_suggestions: [{ toolName: 'Bash', ruleContent: 'npm test' }],
  }, ['--source', 'zcode']);
  let out = parseOut(res.out);
  ok('PermissionRequest 允许 → decision.behavior=allow',
    out && out.hookSpecificOutput && out.hookSpecificOutput.decision &&
    out.hookSpecificOutput.decision.behavior === 'allow', res);

  plan = { permission: 'allow-always' };
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    tool_name: 'Bash',
    tool_input: { command: 'npm run build' },
    tool_use_id: 'tool-2',
  }, ['--source', 'zcode']);
  out = parseOut(res.out);
  const ups = out && out.hookSpecificOutput && out.hookSpecificOutput.decision && out.hookSpecificOutput.decision.updatedPermissions;
  ok('PermissionRequest 始终允许 → 带 updatedPermissions(addRules)',
    Array.isArray(ups) && ups[0].type === 'addRules' && ups[0].rules[0].toolName === 'Bash', res);

  plan = { permission: 'deny', text: '别跑这个' };
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    tool_name: 'Write',
    tool_input: { file_path: 'a.txt' },
    tool_use_id: 'tool-3',
  }, ['--source', 'claude-code']);
  out = parseOut(res.out);
  ok('PermissionRequest 拒绝 → deny 且带上用户备注',
    out && out.hookSpecificOutput.decision.behavior === 'deny' &&
    /别跑这个/.test(out.hookSpecificOutput.decision.message || ''), res);

  plan = { ask: 'submit', askOption: 1 };
  res = await runHook({
    hook_event_name: 'PreToolUse',
    tool_name: 'AskUserQuestion',
    tool_input: {
      questions: [{
        question: '用哪种方案？', header: '方案', multiSelect: false,
        options: [{ label: 'A 方案', description: '稳' }, { label: 'B 方案', description: '快' }],
      }],
    },
    tool_use_id: 'tool-ask-1',
  }, ['--source', 'zcode']);
  out = parseOut(res.out);
  ok('PreToolUse AskUserQuestion → allow + updatedInput.answers 注入',
    out && out.hookSpecificOutput.permissionDecision === 'allow' &&
    out.hookSpecificOutput.updatedInput.answers['用哪种方案？'] === 'B 方案', res);

  // 同一提问第二次触发（ZCode 会再发 PermissionRequest）：复用缓存，不再弹窗
  const popupsBefore = popups.length;
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    tool_name: 'AskUserQuestion',
    tool_input: {
      questions: [{
        question: '用哪种方案？', header: '方案', multiSelect: false,
        options: [{ label: 'A 方案' }, { label: 'B 方案' }],
      }],
    },
    tool_use_id: 'tool-ask-1',
  }, ['--source', 'zcode']);
  out = parseOut(res.out);
  ok('同一提问双触发 → 复用决策、不重复弹窗',
    popups.length === popupsBefore && out && out.hookSpecificOutput.decision.behavior === 'allow', res);

  plan = { ask: 'cancel' };
  res = await runHook({
    hook_event_name: 'PreToolUse',
    tool_name: 'AskUserQuestion',
    tool_input: { questions: [{ question: '继续吗？', options: [{ label: '继续' }] }] },
    tool_use_id: 'tool-ask-2',
  }, ['--source', 'zcode']);
  out = parseOut(res.out);
  ok('提问被取消 → deny，不静默放行',
    out && out.hookSpecificOutput.permissionDecision === 'deny', res);

  res = await runHook({
    hook_event_name: 'Notification',
    message: 'Claude 需要你的确认',
  }, ['--source', 'claude-code']);
  ok('Notification → 静默上报（stdout 为空）', res.out.trim() === '', res);

  console.log('\n[3] OpenCode 子命令');

  plan = { permission: 'allow' };
  res = await runHook({ permission: { id: 'p1', type: 'bash', patterns: ['npm test'], metadata: { command: 'npm test' } } }, ['opencode-permission']);
  out = parseOut(res.out);
  ok('openode-permission → {status:"allow"}', out && out.status === 'allow', res);

  plan = { ask: 'submit', askOption: 0 };
  res = await runHook({
    questions: [{ question: '继续吗？', header: '确认', options: [{ label: '继续' }, { label: '停下' }] }],
    sessionID: 'ses_1', requestID: 'que_1',
    // 故意不给 serverUrl：本用例只验证答案格式
  }, ['opencode-question']);
  out = parseOut(res.out);
  ok('opencode-question → answers 为 string[][]',
    out && Array.isArray(out.answers) && out.answers[0][0] === '继续', res);

  plan = { ask: 'cancel' };
  res = await runHook({
    questions: [{ question: '继续吗？', options: [{ label: '继续' }] }],
    sessionID: 'ses_2', requestID: 'que_2',
  }, ['opencode-question']);
  out = parseOut(res.out);
  ok('opencode-question 取消 → reject', out && out.reject === true, res);

  console.log('\n[4] 收尾');
  const status = await getStatus();
  ok('无挂起交互泄漏', status.pending === 0, status);
  ok('打断计数已累计', status.activity.interruptions > 0, status.activity);

  gateway.stop();
  console.log(`\n结果：${passed} passed, ${failed} failed  (端口 ${port})\n`);
  process.exit(failed ? 1 : 0);
}

run().catch((e) => {
  console.error(e);
  process.exit(1);
});
