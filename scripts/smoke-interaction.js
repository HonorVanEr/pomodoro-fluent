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
// 置 true 时桩「装死」不回应，用来验证 hook 侧拿不到决策时的兜底（超时/关窗）
let holdInteraction = false;

const gateway = createGateway({
  showPopup: (payload) => {
    lastPopup = payload;
    popups.push(payload);
    const id = payload.id;
    if (payload.title === '[timeout-probe]') return; // 该用例故意不回应，验证超时兜底
    if (holdInteraction) return;                     // 同上，走 hook CLI 的真实超时路径
    // 「暂时收起」用例自己控制答复时机（先 hold、再 reopen、最后作答）；
    // [after-hold] 也必须放行 —— 否则会被下面 30ms 的自动应答抢先 resolve，
    // 用例在 `await holdReq` 之后就找不到它了（竞态，实测会 flaky）。
    if (/^\[(hold|after-hold)/.test(String(payload.title || ''))) return;
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

// 等一个条件成立（轮询式，避免依赖固定 sleep）
function waitFor(cond, ms = 2000) {
  const t0 = Date.now();
  return new Promise((resolve) => {
    const tick = () => {
      if (cond() || Date.now() - t0 > ms) resolve();
      else setTimeout(tick, 20);
    };
    tick();
  });
}

// 取 pending 里标题匹配的那条
function pendingByTitle(title) {
  return gateway.listPending().find((p) => p.title === title) || null;
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
    child.on('close', (code) => resolve({ out, err, code }));
    child.stdin.on('error', () => {}); // 子进程先退出时不至于把父进程带崩
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

  console.log('\n[1.5] 「暂时收起」（hold）与唤回');

  // 收起：只收窗口，不结束交互——HTTP 请求继续挂着
  const holdReq = post('/api/interaction', {
    kind: 'permission', title: '[hold-probe]', message: '允许 npm test？',
    permission: { tool: 'Bash', rule: 'npm test', canAlways: true },
    timeoutMs: 60000,
  });
  await waitFor(() => !!pendingByTitle('[hold-probe]'));
  const hp = pendingByTitle('[hold-probe]');
  ok('交互已挂起在 pending 列表（state=active）', !!hp && hp.state === 'active', hp);

  const held = hp ? gateway.holdInteraction(hp.id) : null;
  ok('hold → 标记 held 并返回摘要', !!held && held.state === 'held' && held.kind === 'permission', held);
  ok('hold → 同一 id 仍在 pending（没被 resolve）',
    !!pendingByTitle('[hold-probe]') && pendingByTitle('[hold-probe]').state === 'held',
    gateway.listPending());
  ok('hold → 摘要带上了弹窗标题与工具',
    !!held && held.title === '[hold-probe]' && held.tool === 'Bash', held);
  ok('重复 hold 幂等（第二次返回 null）', hp && gateway.holdInteraction(hp.id) === null);

  // 收起期间来新交互：held 的不能被顶掉
  const afterReq = post('/api/interaction', {
    kind: 'permission', title: '[after-hold]', permission: { tool: 'Write', canAlways: false },
    timeoutMs: 60000,
  });
  await waitFor(() => !!pendingByTitle('[after-hold]'));
  ok('新弹窗不顶掉已收起的交互',
    !!pendingByTitle('[hold-probe]') && pendingByTitle('[hold-probe]').state === 'held',
    gateway.listPending());

  // 唤回：拿回原 payload，状态回到 active
  const revived = hp ? gateway.reopenInteraction(hp.id) : null;
  ok('reopen → 拿回原 payload 且状态回 active',
    !!revived && revived.id === hp.id && revived.title === '[hold-probe]'
      && !!pendingByTitle('[hold-probe]') && pendingByTitle('[hold-probe]').state === 'active',
    revived);
  ok('reopen 不存在的 id → null', gateway.reopenInteraction('no-such-id') === null);
  ok('reopen 已经 active 的 → null（不重复弹）',
    hp && gateway.reopenInteraction(hp.id) === null);

  // 收尾：两条都答掉，别留挂起交互（后面有用例检查泄漏）
  gateway.resolveInteraction(hp.id, { action: 'allow', answers: {}, text: '' }, 'user');
  const h1 = await holdReq;
  ok('唤回答复 → decidedBy=user 且真拿到决策',
    h1.decidedBy === 'user' && h1.action === 'allow', h1);

  const ap = pendingByTitle('[after-hold]');
  gateway.resolveInteraction(ap.id, { action: 'deny', answers: {}, text: '' }, 'user');
  const h2 = await afterReq;
  ok('被顶掉场景下的交互仍可正常作答', h2.decidedBy === 'user' && h2.action === 'deny', h2);
  ok('收尾后 pending 已清空', gateway.listPending().length === 0, gateway.listPending());

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

  console.log('\n[2.5] 上下文跟踪（哪个任务 / 哪个 agent / 在动哪个工具）');

  // 网关侧：context 原样透传到弹窗
  plan = { ask: 'cancel' };
  await post('/api/interaction', {
    kind: 'ask', source: 'manual', title: '上下文透传测试',
    questions: [{ id: 'q0', question: '继续？', options: [{ id: 'o0', label: '继续' }] }],
    context: { agent: 'zcode', project: 'pomodoro-fluent', task: '重构网关', tool: 'Bash', session: 'abc123' },
  });
  ok('HTTP context → 弹窗 payload 完整保留',
    lastPopup && lastPopup.context && lastPopup.context.project === 'pomodoro-fluent' &&
    lastPopup.context.task === '重构网关' && lastPopup.context.tool === 'Bash' && lastPopup.context.session === 'abc123',
    lastPopup && lastPopup.context);

  // CLI 侧：先记任务（UserPromptSubmit），再触发权限弹窗，弹窗应带上任务/项目/工具
  const sess = `smoke-ctx-${process.pid}`;
  plan = { permission: 'allow' };
  await runHook({
    hook_event_name: 'UserPromptSubmit',
    session_id: sess,
    prompt: '把番茄钟的 agent 弹窗改成可交互的',
    cwd: path.join(os.tmpdir(), 'demo-project'),
  }, ['--source', 'zcode']);
  await runHook({
    hook_event_name: 'PermissionRequest',
    session_id: sess,
    tool_name: 'Bash',
    tool_input: { command: 'npm test' },
    tool_use_id: `ctx-${process.pid}`,
  }, ['--source', 'zcode']);
  const c = (lastPopup && lastPopup.context) || {};
  ok('CLI 跟踪任务提示词 → 弹窗显示任务',
    c.task === '把番茄钟的 agent 弹窗改成可交互的', c);
  ok('CLI 跟踪项目目录 → 显示项目名', c.project === 'demo-project', c);
  ok('CLI 跟踪工具与命令 → 显示工具与详情',
    c.tool === 'Bash' && c.toolDetail === 'npm test', c);
  ok('CLI 记录会话尾号', typeof c.session === 'string' && c.session.length === 6, c);
  ok('CLI 记录宿主 agent 名称', c.agent === 'zcode', c);

  res = await runHook({}, ['sessions']);
  ok('sessions 子命令列出跟踪到的会话',
    res.out.includes(sess.slice(-6)) && res.out.includes('npm test') && res.out.includes('demo-project'),
    res.out.slice(0, 240));

  console.log('\n[2.7] VS Code Copilot / Cursor / Codex');

  // VS Code：与 Claude Code 同格式，但只有 8 个事件（没有 PermissionRequest /
  // Notification），提问挂在 PreToolUse。2026-09-18 起不再做工具审批 ——
  // 普通工具调用（含 run_in_terminal）一律静默上报、不回决策。
  let popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-vscode-${process.pid}`,
    cwd: path.join(os.tmpdir(), 'vscode-proj'),
    tool_name: 'run_in_terminal',            // 下划线命名，不是 runInTerminal
    tool_input: { command: 'npm run build', cwd: '/tmp' },
    timestamp: new Date().toISOString(),
  }, ['--source', 'vscode']);
  out = parseOut(res.out);
  ok('VS Code PreToolUse（run_in_terminal）→ 不弹窗、不回决策（审批已移除）',
    popups.length === popupCount && (!out || !out.hookSpecificOutput), res);

  // 普通工具（读文件）同样不弹窗、不回决策，只静默上报活动
  popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-vscode-${process.pid}`,
    tool_name: 'read_file',
    tool_input: { filePath: 'src/index.ts' },   // VS Code 入参是 camelCase
  }, ['--source', 'vscode']);
  out = parseOut(res.out);
  ok('VS Code 普通工具（read_file）→ 不弹窗、不回决策',
    popups.length === popupCount && (!out || !out.hookSpecificOutput), res);

  // 提问：VS Code 的 askQuestions 弹的是 QuickPick，答案不在入参里，
  // 改 updatedInput 改不动用户选择 → 走 deny + 把答案写进原因/上下文
  plan = { ask: 'submit', askOption: 0 };
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-vscode-${process.pid}`,
    tool_name: 'vscode/askQuestions',
    tool_input: {
      questions: [{
        header: '缓存', question: '用哪种缓存策略？', multiSelect: false,
        options: [{ label: 'LRU', description: '内存可控' }, { label: 'TTL', description: '实现简单' }],
      }],
    },
    tool_use_id: `vscode-ask-${process.pid}`,
  }, ['--source', 'vscode']);
  out = parseOut(res.out);
  ok('VS Code 提问（vscode/askQuestions）→ deny + 答案进 reason/additionalContext',
    out && out.hookSpecificOutput.permissionDecision === 'deny'
      && /LRU/.test(out.hookSpecificOutput.permissionDecisionReason || '')
      && /LRU/.test(out.hookSpecificOutput.additionalContext || ''), res);
  ok('VS Code 提问弹窗带出选项与来源',
    lastPopup && lastPopup.questions && lastPopup.questions[0].options[0].label === 'LRU'
      && lastPopup.context && lastPopup.context.agent === 'vscode',
    lastPopup && lastPopup.context);

  // Stop 只上报，绝不回 decision:block（那会阻止 agent 收尾甚至把 agent 拖进自循环）
  res = await runHook({
    hook_event_name: 'Stop',
    session_id: `smoke-vscode-${process.pid}`,
    stop_hook_active: false,
  }, ['--source', 'vscode']);
  ok('VS Code Stop → 静默上报，不返回 decision:block', res.out.trim() === '', res);

  // 新增事件要能被网关接受（写完就上报，不能 400）
  r = await post('/api/event', { kind: 'pre-compact', source: 'vscode' });
  ok('网关接受 pre-compact 事件', r && r.ok === true, r);
  r = await post('/api/event', { kind: 'subagent-start', source: 'vscode' });
  ok('网关接受 subagent-start 事件', r && r.ok === true, r);

  console.log('\n[2.8] Trae');

  // Trae 的 hook 是 Claude Code 那种嵌套格式，输入 snake_case；
  // 但只有 6 个事件（有 Notification、无 PermissionRequest）→ 提问挂 PreToolUse。
  // 2026-09-18 起不再做工具审批：普通工具调用（RunCommand）一律静默上报。
  popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-trae-${process.pid}`,
    cwd: path.join(os.tmpdir(), 'trae-proj'),
    workspace_roots: [path.join(os.tmpdir(), 'trae-proj')],
    tool_use_id: `trae-run-${process.pid}`,
    tool_name: 'RunCommand',
    llm_tool_name: 'RunCommand',
    tool_input: { command: 'npm run build' },
  }, ['--source', 'trae']);
  out = parseOut(res.out);
  ok('Trae PreToolUse（RunCommand）→ 不弹窗、不回决策（审批已移除）',
    popups.length === popupCount && (!out || !out.hookSpecificOutput), res);

  // 普通工具（Read）不弹窗、不回决策
  popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-trae-${process.pid}`,
    tool_name: 'Read',
    tool_input: { file_path: 'src/index.ts' },
  }, ['--source', 'trae']);
  out = parseOut(res.out);
  ok('Trae 普通工具（Read）→ 不弹窗、不回决策',
    popups.length === popupCount && (!out || !out.hookSpecificOutput), res);

  // 提问：Trae 的工具名与 Claude Code 同名（AskUserQuestion）→ 默认走 updatedInput.answers
  plan = { ask: 'submit', askOption: 0 };
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-trae-${process.pid}`,
    cwd: path.join(os.tmpdir(), 'trae-proj'),
    workspace_roots: [path.join(os.tmpdir(), 'trae-proj')],
    tool_use_id: `trae-ask-${process.pid}`,
    tool_name: 'AskUserQuestion',
    tool_input: {
      questions: [{
        question: '用哪种缓存策略？', header: '缓存',
        options: [{ label: 'LRU', description: '内存可控' }, { label: 'TTL', description: '实现简单' }],
      }],
    },
  }, ['--source', 'trae']);
  out = parseOut(res.out);
  ok('Trae 提问 → updatedInput.answers 注入（与 Claude Code 同名同通道）',
    out && out.hookSpecificOutput && out.hookSpecificOutput.permissionDecision === 'allow'
      && out.hookSpecificOutput.updatedInput
      && /LRU/.test(JSON.stringify(out.hookSpecificOutput.updatedInput.answers || {})), res);
  ok('Trae 上下文 → 来源标为 trae 且项目名取自 workspace_roots',
    lastPopup && lastPopup.context && lastPopup.context.agent === 'trae'
      && lastPopup.context.project === 'trae-proj', lastPopup && lastPopup.context);

  // Notification 是 Trae 的异步通知（含 permission_prompt / idle_prompt）→ 只上报
  res = await runHook({
    hook_event_name: 'Notification',
    session_id: `smoke-trae-${process.pid}`,
    notification_type: 'idle_prompt',
    message: '智能体已完成任务',
  }, ['--source', 'trae']);
  ok('Trae Notification → 静默上报（stdout 为空）', res.out.trim() === '', res);

  // Stop 只上报：Trae 的 Stop 支持 decision:block 阻止收尾，但我们绝不用
  res = await runHook({
    hook_event_name: 'Stop',
    session_id: `smoke-trae-${process.pid}`,
    stop_hook_active: false,
    loop_count: 0,
    last_assistant_message: '已完成重构',
  }, ['--source', 'trae']);
  ok('Trae Stop → 静默上报，不返回 decision:block', res.out.trim() === '', res);

  // 来源自动识别：不带 --source，靠 payload 里的 llm_tool_name / workspace_roots 认出来
  plan = { ask: 'submit', askOption: 0 };
  res = await runHook({
    hook_event_name: 'PreToolUse',
    session_id: `smoke-trae-auto-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'trae-auto')],
    tool_use_id: `trae-auto-${process.pid}`,
    tool_name: 'AskUserQuestion',
    llm_tool_name: 'AskUserQuestion',
    tool_input: {
      questions: [{
        question: '自动识别来源？', header: '来源',
        options: [{ label: '是', description: 'trae' }, { label: '否', description: '不是' }],
      }],
    },
  });
  out = parseOut(res.out);
  ok('Trae 来源自动识别（llm_tool_name/workspace_roots）',
    lastPopup && lastPopup.context && lastPopup.context.agent === 'trae', lastPopup && lastPopup.context);

  // Cursor：beforeShellExecution 拦截命令
  plan = { permission: 'deny', text: '这条命令先不动' };
  res = await runHook({
    hook_event_name: 'beforeShellExecution',
    conversation_id: `smoke-cursor-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'cursor-proj')],
    command: 'rm -rf build',
    cwd: path.join(os.tmpdir(), 'cursor-proj'),
  }, ['--source', 'cursor']);
  out = parseOut(res.out);
  ok('Cursor beforeShellExecution 拒绝 → permission=deny',
    out && out.permission === 'deny' && /先不动/.test(out.user_message || ''), res);

  plan = { permission: 'allow' };
  res = await runHook({
    hook_event_name: 'beforeShellExecution',
    conversation_id: `smoke-cursor-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'cursor-proj')],
    command: 'npm test',
    cwd: path.join(os.tmpdir(), 'cursor-proj'),
  }, ['--source', 'cursor']);
  out = parseOut(res.out);
  ok('Cursor beforeShellExecution 允许 → permission=allow', out && out.permission === 'allow', res);

  // Cursor：afterFileEdit 只计数，不回决策；stop 触发「休息建议」判定
  res = await runHook({
    hook_event_name: 'afterFileEdit',
    conversation_id: `smoke-cursor-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'cursor-proj')],
    file_path: path.join(os.tmpdir(), 'cursor-proj', 'src/index.ts'),
  }, ['--source', 'cursor']);
  ok('Cursor afterFileEdit → 静默（stdout 为空）', res.out.trim() === '', res);

  res = await runHook({
    hook_event_name: 'beforeSubmitPrompt',
    conversation_id: `smoke-cursor-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'cursor-proj')],
    prompt: '把库存扣减改成幂等',
  }, ['--source', 'cursor']);
  out = parseOut(res.out);
  ok('Cursor beforeSubmitPrompt → {continue:true}', out && out.continue === true, res);

  res = await runHook({
    hook_event_name: 'stop',
    conversation_id: `smoke-cursor-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'cursor-proj')],
    status: 'completed',
    loop_count: 0,
  }, ['--source', 'cursor']);
  ok('Cursor stop → 返回 {} 且不触发 followup', res.out.trim() === '{}', res);

  // Cursor 提问：preToolUse + question 工具 → updated_input 注入答案
  plan = { ask: 'submit', askOption: 0 };
  res = await runHook({
    hook_event_name: 'preToolUse',
    conversation_id: `smoke-cursor-${process.pid}`,
    workspace_roots: [path.join(os.tmpdir(), 'cursor-proj')],
    tool_name: 'askQuestion',
    tool_input: { question: '用哪个方案？', options: [{ label: '方案 A' }, { label: '方案 B' }] },
  }, ['--source', 'cursor']);
  out = parseOut(res.out);
  ok('Cursor preToolUse 提问 → permission=allow + updated_input.answers',
    out && out.permission === 'allow' && out.updated_input && out.updated_input.answers, res);

  // Codex：notify 只在回合结束时回调
  res = await runHook({
    type: 'agent-turn-complete',
    'thread-id': `smoke-codex-${process.pid}`,
    cwd: path.join(os.tmpdir(), 'codex-proj'),
    'input-messages': ['修一下登录超时'],
    'last-assistant-message': '已修复并补了测试',
  }, ['codex-notify']);
  ok('Codex notify(agent-turn-complete) → 静默上报', res.out.trim() === '', res);

  console.log('\n[2.95] Codex（审批只走 PermissionRequest）');

  // Codex 的 PreToolUse 只强制执行 permissionDecision:"deny"，allow / ask 都是「被解析
  // 但不生效」（实机 bundle 原话：unsupported permissionDecision:allow / :ask）。所以在
  // PreToolUse 上弹窗是错的：用户点「允许」传不回去，宿主走自己的审批 → PermissionRequest
  // → 又弹一次。这里断言：即便工具是 Bash，也绝不在此弹窗、不回决策。
  const codexBase = {
    session_id: `smoke-codex-${process.pid}`,
    turn_id: `turn-${process.pid}`,
    model: 'gpt-5.3-codex',
    permission_mode: 'default',
  };
  popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PreToolUse',
    ...codexBase,
    tool_name: 'Bash',
    tool_input: { command: 'rm -rf dist' },
    tool_use_id: 'call_pre',
  }, ['--source', 'codex']);
  out = parseOut(res.out);
  ok('Codex PreToolUse → 不弹窗、不回决策（allow/ask 在 Codex 上不生效）',
    popups.length === popupCount && (!out || !out.hookSpecificOutput), res);

  // 审批正解：PermissionRequest —— 只在「Codex 本来就要问」时才触发
  plan = { permission: 'allow', text: '' };
  popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    ...codexBase,
    trigger: 'untrusted_command',
    tool_name: 'Bash',
    tool_input: { command: `npm run codex-${process.pid}` },
    tool_use_id: 'call_1',
  }, ['--source', 'codex']);
  out = parseOut(res.out);
  ok('Codex PermissionRequest 允许 → hookSpecificOutput.decision.behavior=allow',
    popups.length === popupCount + 1 && out && out.hookSpecificOutput
      && out.hookSpecificOutput.decision
      && out.hookSpecificOutput.decision.behavior === 'allow', res);
  ok('Codex 答复里没有 updatedPermissions / updatedInput / interrupt（不支持字段会 fail closed）',
    out && out.hookSpecificOutput && out.hookSpecificOutput.decision
      && !('updatedPermissions' in out.hookSpecificOutput.decision)
      && !('updatedInput' in out.hookSpecificOutput.decision)
      && !('interrupt' in out.hookSpecificOutput.decision), out && out.hookSpecificOutput.decision);
  ok('Codex 上下文 → 来源标为 codex',
    lastPopup && lastPopup.context && lastPopup.context.agent === 'codex', lastPopup && lastPopup.context);

  // 拒绝：behavior=deny + 带原因（Codex 的 deny 会把 message 显示给用户）
  plan = { permission: 'deny', text: '这条命令先不动' };
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    ...codexBase,
    tool_name: 'Bash',
    tool_input: { command: `npm run codex-${process.pid}-deny` },
    tool_use_id: 'call_2',
  }, ['--source', 'codex']);
  out = parseOut(res.out);
  ok('Codex PermissionRequest 拒绝 → behavior=deny + 带上拒绝原因',
    out && out.hookSpecificOutput && out.hookSpecificOutput.decision
      && out.hookSpecificOutput.decision.behavior === 'deny'
      && /这条命令先不动/.test(out.hookSpecificOutput.decision.message || ''), res);

  // 「始终允许」：Codex 落不了盘规则 → 靠番茄钟本地规则兜住；答复里同样不能带 updatedPermissions
  const codexRule = `npm run codex-always-${process.pid}`;
  plan = { permission: 'allow-always', text: '' };
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    ...codexBase,
    tool_name: 'Bash',
    tool_input: { command: codexRule },
    tool_use_id: 'call_3',
  }, ['--source', 'codex']);
  out = parseOut(res.out);
  ok('Codex「始终允许」→ 仍不带 updatedPermissions（靠本地规则落地）',
    out && out.hookSpecificOutput && out.hookSpecificOutput.decision
      && out.hookSpecificOutput.decision.behavior === 'allow'
      && !('updatedPermissions' in out.hookSpecificOutput.decision), res);

  popupCount = popups.length;
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    ...codexBase,
    tool_name: 'Bash',
    tool_input: { command: codexRule },
    tool_use_id: 'call_4',
  }, ['--source', 'codex']);
  out = parseOut(res.out);
  ok('Codex 本地「始终允许」规则生效 → 第二次不再弹窗',
    popups.length === popupCount && out && out.hookSpecificOutput
      && out.hookSpecificOutput.decision.behavior === 'allow', res);

  // 来源自动识别：不带 --source 时靠 payload 的 turn_id 认出 Codex
  plan = { permission: 'allow', text: '' };
  res = await runHook({
    hook_event_name: 'PermissionRequest',
    session_id: `smoke-codex-auto-${process.pid}`,
    turn_id: `turn-auto-${process.pid}`,
    tool_name: 'Bash',
    tool_input: { command: `npm run codex-auto-${process.pid}` },
    tool_use_id: 'call_5',
  });
  ok('不带 --source 也能靠 turn_id 认出 Codex',
    !!lastPopup && lastPopup.source === 'codex', lastPopup && lastPopup.source);

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

  console.log('\n[4] 一键安装（设置面板按钮走的同一条 CLI 路径）');

  // 把 HOME 重定向到临时目录，验证 install 真能写出配置，且不碰本机真实配置
  const fakeHome = fs.mkdtempSync(path.join(os.tmpdir(), 'pomodoro-home-'));
  const homeEnv = { USERPROFILE: fakeHome, HOME: fakeHome, XDG_CONFIG_HOME: path.join(fakeHome, '.config') };

  res = await runHook({}, ['install', '--agent', 'trae'], homeEnv);
  const traeFile = path.join(fakeHome, '.trae-cn', 'hooks.json');
  let traeCfg = null;
  try { traeCfg = JSON.parse(fs.readFileSync(traeFile, 'utf8')); } catch (e) { /* 断言会失败 */ }
  ok('install --agent trae 写出 ~/.trae-cn/hooks.json（退出码 0）',
    res.out.includes('已写入') && traeCfg && traeCfg.version === 1, res);
  ok('install 写出的 PreToolUse 带 matcher（只匹配提问工具）+ timeout=4200（秒）',
    traeCfg && traeCfg.hooks && traeCfg.hooks.PreToolUse
      && /AskUserQuestion/.test(traeCfg.hooks.PreToolUse[0].matcher || '')
      && traeCfg.hooks.PreToolUse[0].hooks[0].timeout === 4200, traeCfg && traeCfg.hooks);

  // VS Code 那边要写 timeout（不是 timeoutSec），且覆盖 8 个事件
  res = await runHook({}, ['install', '--agent', 'vscode'], homeEnv);
  const vsFile = path.join(fakeHome, '.copilot', 'hooks', 'pomodoro.json');
  let vsCfg = null;
  try { vsCfg = JSON.parse(fs.readFileSync(vsFile, 'utf8')); } catch (e) { /* 断言会失败 */ }
  ok('install --agent vscode 写出 ~/.copilot/hooks/pomodoro.json',
    vsCfg && vsCfg.hooks && Object.keys(vsCfg.hooks).length === 8, vsCfg && vsCfg.hooks);
  ok('VS Code 配置用 timeout（不是 timeoutSec）且 PreToolUse=4200',
    vsCfg && vsCfg.hooks.PreToolUse[0].timeout === 4200
      && !('timeoutSec' in vsCfg.hooks.PreToolUse[0]), vsCfg && vsCfg.hooks.PreToolUse[0]);

  // 重复安装要幂等：同一条目不重复追加
  await runHook({}, ['install', '--agent', 'trae'], homeEnv);
  const traeAgain = JSON.parse(fs.readFileSync(traeFile, 'utf8'));
  ok('重复安装幂等（PreToolUse 仍只有 1 条）',
    traeAgain.hooks.PreToolUse.length === 1, traeAgain.hooks.PreToolUse);

  // --clean 要清掉指向别的副本的旧条目（否则同一次调用会跑两遍）
  traeAgain.hooks.PreToolUse.push({
    matcher: 'RunCommand',
    hooks: [{ type: 'command', command: 'node "D:\\\\old\\\\pomodoro-hook.js" --source trae', timeout: 600 }],
  });
  fs.writeFileSync(traeFile, JSON.stringify(traeAgain, null, 2));
  await runHook({}, ['install', '--agent', 'trae', '--clean'], homeEnv);
  const traeCleaned = JSON.parse(fs.readFileSync(traeFile, 'utf8'));
  ok('--clean 清掉指向其它副本的旧条目（只剩当前安装路径）',
    traeCleaned.hooks.PreToolUse.length === 1
      && !/old/.test(JSON.stringify(traeCleaned.hooks.PreToolUse)), traeCleaned.hooks.PreToolUse);

  // Codex：hooks.json 覆盖 12 个事件。审批在 PermissionRequest（Codex 唯一认 allow/deny
  // 的地方）→ 必须等用户点弹窗；PreToolUse 只上报活动 → matcher 收窄、timeout 保持短。
  res = await runHook({}, ['install', '--agent', 'codex'], homeEnv);
  const codexFile = path.join(fakeHome, '.codex', 'hooks.json');
  let codexCfg = null;
  try { codexCfg = JSON.parse(fs.readFileSync(codexFile, 'utf8')); } catch (e) { /* 断言会失败 */ }
  ok('install --agent codex 写出 ~/.codex/hooks.json（12 个事件）',
    res.out.includes('已写入') && codexCfg && codexCfg.hooks
      && Object.keys(codexCfg.hooks).length === 12,
    codexCfg && codexCfg.hooks ? Object.keys(codexCfg.hooks) : codexCfg);
  ok('Codex PermissionRequest 用 timeout=4200（要等用户点弹窗）',
    codexCfg && codexCfg.hooks.PermissionRequest
      && codexCfg.hooks.PermissionRequest[0].hooks[0].timeout === 4200
      && /--source codex/.test(codexCfg.hooks.PermissionRequest[0].hooks[0].command || ''),
    codexCfg && codexCfg.hooks.PermissionRequest);
  ok('Codex PreToolUse 只上报 → matcher 收窄 + timeout=30（不阻塞等弹窗）',
    codexCfg && codexCfg.hooks.PreToolUse[0].matcher === 'Bash|apply_patch|Edit|Write|mcp__.*'
      && codexCfg.hooks.PreToolUse[0].hooks[0].timeout === 30, codexCfg && codexCfg.hooks.PreToolUse);
  ok('Codex 不给 UserPromptSubmit / Stop 写 matcher（这两个事件忽略 matcher）',
    codexCfg && codexCfg.hooks.UserPromptSubmit[0].matcher === undefined
      && codexCfg.hooks.Stop[0].matcher === undefined, codexCfg && codexCfg.hooks);

  // notify 是更老的通道，用户可能已经把它指向别的工具（如 codex-computer-use）
  // → 默认绝不能覆盖；只有显式 --with-notify 才动
  const codexCfgToml = path.join(fakeHome, '.codex', 'config.toml');
  fs.mkdirSync(path.dirname(codexCfgToml), { recursive: true });
  const notifyOrigin = 'notify = ["some-other-tool.exe", "turn-ended"]';
  fs.writeFileSync(codexCfgToml, `model = "gpt-5.3-codex"\n${notifyOrigin}\n`);
  await runHook({}, ['install', '--agent', 'codex'], homeEnv);
  ok('install --agent codex 不动 config.toml 的 notify（避免覆盖已有通知工具）',
    fs.readFileSync(codexCfgToml, 'utf8').includes(notifyOrigin),
    fs.readFileSync(codexCfgToml, 'utf8'));
  await runHook({}, ['install', '--agent', 'codex', '--with-notify'], homeEnv);
  ok('--with-notify 才改写 notify（并备份原文件）',
    /pomodoro-hook\.js/.test(fs.readFileSync(codexCfgToml, 'utf8'))
      && fs.existsSync(`${codexCfgToml}.pomodoro.bak`), fs.readFileSync(codexCfgToml, 'utf8'));

  // 用户把 hooks 关掉时（[features] hooks = false）必须明确告警，否则「配好了不弹窗」查不出来
  fs.writeFileSync(codexCfgToml, '[features]\nhooks = false\n');
  res = await runHook({}, ['install', '--agent', 'codex'], homeEnv);
  ok('config.toml 里 hooks 被关掉 → 安装时明确告警',
    /hooks 被关了/.test(res.out || ''), res.out);

  // 未知宿主必须非零退出 —— 前端「一键安装」靠退出码判断成败，不能误报成功
  res = await runHook({}, ['install', '--agent', 'no-such-agent'], homeEnv);
  ok('install 未知宿主 → 非零退出码（前端才能识别为失败）',
    res.code !== 0 && /未知 agent/.test(res.err || ''), { code: res.code, err: res.err });

  // 写盘失败也要非零退出：把目标做成"父路径是文件"，mkdir 必然失败
  const blocker = path.join(fakeHome, 'blocked');
  fs.writeFileSync(blocker, 'not a directory');
  const badHome = fs.mkdtempSync(path.join(os.tmpdir(), 'pomodoro-badhome-'));
  fs.mkdirSync(path.join(badHome, '.claude'), { recursive: true });
  fs.writeFileSync(path.join(badHome, '.claude', 'settings.json'), '{}');
  // 用只读文件系统不好造，这里改测「父路径被文件占位」：把 .trae-cn 建成文件
  fs.writeFileSync(path.join(badHome, '.trae-cn'), 'not a directory');
  res = await runHook({}, ['install', '--agent', 'trae'],
    { USERPROFILE: badHome, HOME: badHome, XDG_CONFIG_HOME: path.join(badHome, '.config') });
  ok('install 写盘失败 → 非零退出码 + 明确报错',
    res.code !== 0 && /失败/.test(res.err || ''), { code: res.code, err: res.err });
  void blocker;

  console.log('\n[5] 收尾');
  const status = await getStatus();
  ok('无挂起交互泄漏', status.pending === 0, status);
  ok('打断计数已累计', status.activity.interruptions > 0, status.activity);

  try { fs.rmSync(fakeHome, { recursive: true, force: true }); } catch (e) { /* ignore */ }
  try { fs.rmSync(badHome, { recursive: true, force: true }); } catch (e) { /* ignore */ }

  gateway.stop();
  console.log(`\n结果：${passed} passed, ${failed} failed  (端口 ${port})\n`);
  process.exit(failed ? 1 : 0);
}

run().catch((e) => {
  console.error(e);
  process.exit(1);
});
