'use strict';

// ---------------------------------------------------------------------------
// Agent 网关：本地 HTTP 服务
// 供 ZCode / Claude Code hooks / OpenCode 插件 / 任意脚本与番茄钟联动。
//
//   GET  /health         存活探测（免鉴权，只暴露端口是否活着）
//   GET  /api/status     定时器状态 + 本专注期 agent 活动计数
//   POST /api/notify     主动弹一条通知 {title, message, sub, type}
//   POST /api/event      agent 事件上报 {kind, ...}（计数 + 策略弹窗）
//   POST /api/interaction 交互弹窗（长轮询等用户在弹窗内决策）
//                        kind=ask        提问：多问题选项 + 多选 + 自定义输入
//                        kind=permission 权限：允许 / 始终允许 / 拒绝
//                        kind=notification 纯通知（无按钮，立即返回）
//                        kind=custom     自定义按钮（兼容旧 /api/confirm）
//   POST /api/confirm    兼容旧接口：等价于 kind=custom
//   POST /api/timer      远程控制 {command: toggle|reset|skip}
//
// 交互返回值：
//   { ok, kind, action, decidedBy: user|timeout|dismissed, answers, text }
//   - ask:        action = submit|cancel，answers = { [questionId]: [optionLabel, ...] }
//   - permission: action = allow|allow-always|deny，text = 用户备注
//   - 超时：      action 落回该 kind 的安全默认值（permission=deny，ask=cancel）
//   - 被顶掉/关闭：action = null（调用方应回退到终端原生询问，不替用户做决定）
//
// 安全：仅绑定 127.0.0.1；除 /health 外全部要求 Bearer token；
// 校验 Host 头只允许 127.0.0.1/localhost（防 DNS rebinding）。
// 发现文件 userData/gateway.json（port/token/pid）供外部 CLI 定位。
// ---------------------------------------------------------------------------

const http = require('http');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const GATEWAY_VERSION = 2;
const DEFAULT_PORT = 5277;
const PORT_ATTEMPTS = 20;          // 端口被占时依次 +1 重试
const BODY_LIMIT = 256 * 1024;     // 请求体上限（提问/工具输入可能较长）
const DEFAULT_CONFIRM_TIMEOUT_MS = 5 * 60 * 1000;
const MAX_CONFIRM_TIMEOUT_MS = 10 * 60 * 1000;
const BREAK_SUGGEST_COOLDOWN_MS = 10 * 60 * 1000; // 「休息建议」弹窗冷却
const BREAK_SUGGEST_TIMEOUT_MS = 2 * 60 * 1000;

// 弹窗文案长度上限（超长截断；结构化内容不再走 URL query，可放宽）
const CAPS = { title: 120, message: 360, sub: 160, detail: 2000 };

function capText(str, n) {
  if (typeof str !== 'string') return '';
  return str.length > n ? str.slice(0, n - 1) + '…' : str;
}

function clampTimeout(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n) || n <= 0) return DEFAULT_CONFIRM_TIMEOUT_MS;
  return Math.min(MAX_CONFIRM_TIMEOUT_MS, Math.max(5000, Math.round(n)));
}

const SOURCE_LABELS = {
  'zcode': 'ZCode',
  'claude-code': 'Claude Code',
  'opencode': 'OpenCode',
  'cursor': 'Cursor',
  'manual': '手动',
};
function sourceLabel(s) {
  return SOURCE_LABELS[s] || (s ? String(s) : '');
}

// 四种交互形态；ask / permission 可在弹窗内直接作答
const KINDS = {
  ask: { title: 'Agent 提问', defaultAction: 'cancel' },
  permission: { title: '需要权限确认', defaultAction: 'deny' },
  notification: { title: '番茄钟', defaultAction: null },
  custom: { title: '需要确认', defaultAction: null },
};

function isValidKind(k) {
  return Object.prototype.hasOwnProperty.call(KINDS, k);
}

// ---------------------------------------------------------------------------
// 归一化：把各家协议转换后的请求收窄成弹窗能直接渲染的结构
// ---------------------------------------------------------------------------
function normalizeQuestions(list) {
  if (!Array.isArray(list)) return [];
  const out = [];
  list.slice(0, 4).forEach((q, i) => {
    if (!q || typeof q !== 'object') return;
    const text = capText(q.question || q.text || q.title || '', 200);
    if (!text) return;
    const options = (Array.isArray(q.options) ? q.options : [])
      .slice(0, 6)
      .map((o, j) => {
        if (typeof o === 'string') return { id: `o${j}`, label: capText(o, 60), description: '' };
        if (!o || typeof o !== 'object') return null;
        const label = capText(o.label || o.text || o.value || o.name || '', 60);
        if (!label) return null;
        return {
          id: typeof o.id === 'string' && o.id ? o.id : `o${j}`,
          label,
          description: capText(o.description || o.hint || '', 160),
        };
      })
      .filter(Boolean);
    out.push({
      id: typeof q.id === 'string' && q.id ? q.id : `q${i}`,
      question: text,
      header: capText(q.header || '', 24),
      multiSelect: !!(q.multiSelect || q.multiple),
      // 没有选项时必须给输入框，否则用户无处作答
      custom: typeof q.custom === 'boolean' ? q.custom : options.length === 0,
      options,
    });
  });
  return out;
}

function normalizePermission(p, o) {
  const pp = p && typeof p === 'object' ? p : {};
  const suggestions = (Array.isArray(pp.suggestions) ? pp.suggestions : [])
    .slice(0, 4)
    .map((s, i) => {
      if (typeof s === 'string') return { id: `s${i}`, label: capText(s, 60), toolName: '', ruleContent: capText(s, 200) };
      if (!s || typeof s !== 'object') return null;
      const label = capText(s.label || s.ruleContent || s.rule || s.pattern || '', 60);
      if (!label) return null;
      return {
        id: typeof s.id === 'string' && s.id ? s.id : `s${i}`,
        label,
        toolName: capText(s.toolName || '', 60),
        ruleContent: capText(s.ruleContent || s.rule || s.pattern || '', 200),
      };
    })
    .filter(Boolean);
  return {
    tool: capText(pp.tool || o.toolName || o.tool || '', 60),
    rule: capText(pp.rule || '', 200),
    suggestions,
    canAlways: pp.canAlways !== false,
  };
}

function normalizeInput(i, kind) {
  const ii = i && typeof i === 'object' ? i : {};
  // 权限弹窗默认给出备注输入（拒绝理由/补充说明）；提问默认关闭
  const enabled = typeof ii.enabled === 'boolean' ? ii.enabled : kind === 'permission';
  return {
    enabled: !!enabled,
    label: capText(ii.label || '', 40),
    placeholder: capText(ii.placeholder || '', 80),
    required: !!ii.required,
  };
}

function normalizeActions(actions) {
  if (!Array.isArray(actions)) return [];
  return actions
    .filter((a) => a && typeof a.id === 'string' && typeof a.label === 'string')
    .slice(0, 4)
    .map((a) => ({
      id: a.id,
      label: capText(a.label, 24),
      style: a.style === 'primary' || a.style === 'danger' ? a.style : 'default',
    }));
}

function normalizeInteraction(input) {
  const o = input || {};
  const kind = isValidKind(o.kind) ? o.kind : 'custom';
  const conf = KINDS[kind];
  const actions = normalizeActions(o.actions);
  // 视觉风味：ask / permission / agent / work / break / longBreak / idle
  let flavor = typeof o.flavor === 'string' && o.flavor ? o.flavor : '';
  if (!flavor) {
    if (kind === 'ask' || kind === 'permission') flavor = kind;
    else flavor = typeof o.type === 'string' && o.type ? o.type : 'agent';
  }
  const norm = {
    kind,
    flavor: capText(flavor, 24),
    source: capText(o.source || '', 32),
    title: capText(o.title || conf.title, CAPS.title),
    message: capText(o.message || '', CAPS.message),
    sub: capText(o.sub || '', CAPS.sub),
    detail: capText(o.detail || '', CAPS.detail),
    timeoutMs: clampTimeout(o.timeoutMs),
    defaultAction: typeof o.defaultAction === 'string' && o.defaultAction
      ? o.defaultAction
      : conf.defaultAction,
    actions: actions.length ? actions : (kind === 'custom' ? [{ id: 'ok', label: '知道了' }] : []),
    input: normalizeInput(o.input, kind),
  };
  if (kind === 'ask') norm.questions = normalizeQuestions(o.questions);
  if (kind === 'permission') norm.permission = normalizePermission(o.permission, o);
  return norm;
}

function createGateway(deps) {
  // deps:
  //   showPopup(payload)      弹窗（main.showNotify，payload.kind 决定交互形态）
  //   sendTimerCommand(cmd)   转发定时器命令到渲染进程（toggle/reset/skip）
  //   onActivity(activity)    活动计数变化 → 推给主窗口展示
  //   getTimerState()         主进程缓存的定时器状态
  //   getUserDataPath()       Electron userData 目录
  //   log(...args)            日志
  const { showPopup, sendTimerCommand, onActivity, getTimerState, getUserDataPath, log } = deps;

  let server = null;
  let port = null;
  let token = null;
  let running = false;

  // 本专注期活动计数：phase 切回 work 时清零
  const activity = { toolCalls: 0, interruptions: 0, stops: 0, sessions: 0, since: null };
  let prevPhase = null;
  let lastBreakSuggestAt = 0;

  // id -> { resolve, timer, kind, defaultAction }
  const pendingInteractions = new Map();

  // ---- 发现文件（CLI 靠它找到端口与 token） ----
  function discoveryFile() {
    return path.join(getUserDataPath(), 'gateway.json');
  }
  function writeDiscovery() {
    try {
      fs.mkdirSync(getUserDataPath(), { recursive: true });
      fs.writeFileSync(discoveryFile(), JSON.stringify({
        port, token, pid: process.pid,
        version: GATEWAY_VERSION, startedAt: new Date().toISOString(),
      }, null, 2));
    } catch (e) {
      log('[gateway] 写发现文件失败:', e.message);
    }
  }
  function removeDiscovery() {
    try { fs.rmSync(discoveryFile(), { force: true }); } catch (e) { /* ignore */ }
  }

  // ---- 活动计数 ----
  function emitActivity() {
    onActivity({ ...activity });
  }

  // 定时器阶段变化观察：进入 work 视为新专注期开始，计数清零
  function observeTimerState(state) {
    const phase = state && state.phase;
    if (phase && phase !== prevPhase) {
      if (phase === 'work') {
        activity.toolCalls = 0;
        activity.interruptions = 0;
        activity.stops = 0;
        activity.sessions = 0;
        activity.since = Date.now();
        emitActivity();
      }
      prevPhase = phase;
    }
  }

  // ---- 交互弹窗（长轮询） ----
  // 单窗口策略：任何新弹窗都会顶掉旧交互窗，旧请求按 dismissed 返回
  // （action=null → 调用方回退终端原生询问，绝不替用户做决定）
  function dismissPending(reason) {
    for (const id of [...pendingInteractions.keys()]) {
      resolveInteraction(id, { action: null, answers: {}, text: '' }, reason || 'dismissed');
    }
  }

  function resolveInteraction(id, result, decidedBy) {
    const p = pendingInteractions.get(id);
    if (!p) return false;
    pendingInteractions.delete(id);
    clearTimeout(p.timer);
    const r = result || {};
    p.resolve({
      ok: true,
      kind: p.kind,
      action: r.action == null ? null : r.action,
      answers: r.answers || {},
      text: r.text || '',
      decidedBy: decidedBy || 'user',
    });
    return true;
  }

  // 兼容旧调用：只回一个 action
  function resolveConfirm(id, action, decidedBy) {
    return resolveInteraction(id, { action: action == null ? null : action, answers: {}, text: '' }, decidedBy);
  }

  /**
   * 发起一次交互。
   * - notification（且无按钮）：不等待，弹完立即返回
   * - ask / permission / custom：挂起等待用户在弹窗内决策，超时落回安全默认值
   */
  function requestInteraction(opts) {
    const o = normalizeInteraction(opts);

    if (o.kind === 'notification' && !o.actions.length) {
      notifyPopup(o);
      return Promise.resolve({ ok: true, kind: o.kind, action: 'shown', answers: {}, text: '', decidedBy: 'shown' });
    }

    // ask 没有任何可作答控件时降级为通知，避免弹出无法提交的窗口
    if (o.kind === 'ask' && !o.questions.length) {
      o.kind = 'notification';
      o.flavor = 'ask';
      notifyPopup(o);
      return Promise.resolve({ ok: true, kind: 'notification', action: 'shown', answers: {}, text: '', decidedBy: 'shown' });
    }

    const id = crypto.randomUUID();
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        resolveInteraction(id, { action: o.defaultAction, answers: {}, text: '' }, 'timeout');
      }, o.timeoutMs);
      pendingInteractions.set(id, { resolve, timer, kind: o.kind, defaultAction: o.defaultAction });
      // 新弹窗替换旧交互
      for (const other of [...pendingInteractions.keys()]) {
        if (other !== id) resolveInteraction(other, { action: null, answers: {}, text: '' }, 'dismissed');
      }
      showPopup({ id, interactive: true, ...o });
    });
  }

  // 旧接口：自定义按钮确认
  function requestConfirm(opts) {
    return requestInteraction({ ...(opts || {}), kind: 'custom' });
  }

  // ---- 通知弹窗（不等待） ----
  function notifyPopup(payload) {
    const o = normalizeInteraction({ kind: 'notification', ...(payload || {}) });
    dismissPending('dismissed');
    showPopup({ id: null, interactive: false, ...o });
    return { ok: true };
  }

  // ---- agent 事件 → 计数 + 策略弹窗 ----
  const EVENT_KINDS = new Set([
    'notification', 'ask', 'permission', 'stop', 'subagent-stop',
    'tool-before', 'tool-after',
    'session-start', 'session-end', 'prompt',
  ]);

  function handleEvent(ev) {
    const e = ev || {};
    if (!EVENT_KINDS.has(e.kind)) {
      return { ok: false, error: `unknown kind: ${e.kind}` };
    }
    let triggered = null;
    switch (e.kind) {
      case 'tool-after':
        activity.toolCalls += 1;
        break;

      case 'notification':
      case 'ask':
      case 'permission': {
        // 权限请求 / 提问 / 等待用户输入：计入打断 + 弹通知
        activity.interruptions += 1;
        const src = sourceLabel(e.source);
        notifyPopup({
          kind: e.kind === 'notification' ? 'notification' : e.kind,
          title: capText(e.title || (e.kind === 'permission' ? 'Agent 需要权限确认' : 'Agent 需要你的确认'), CAPS.title),
          message: capText(e.message || '', CAPS.message),
          sub: src,
          source: e.source,
        });
        triggered = 'notify';
        break;
      }

      case 'stop':
        // 主 agent 回合结束（任务完成/空闲）
        activity.stops += 1;
        if (maybeSuggestBreak()) triggered = 'break-suggest';
        break;

      case 'session-start':
      case 'session-end':
        activity.sessions += 1;
        break;

      default:
        // tool-before / subagent-stop / prompt：暂不计数不弹窗
        break;
    }
    emitActivity();
    return { ok: true, triggered, activity: { ...activity } };
  }

  // Agent 空闲且仍在专注时段 → 弹「休息建议」，一键跳到休息
  function maybeSuggestBreak() {
    // 有未决交互时不打扰：别把正在等待决策的权限/提问窗顶掉
    if (pendingInteractions.size > 0) return false;
    const t = getTimerState();
    if (!t || t.phase !== 'work' || !t.running) return false;
    if (Date.now() - lastBreakSuggestAt < BREAK_SUGGEST_COOLDOWN_MS) return false;
    lastBreakSuggestAt = Date.now();
    requestConfirm({
      title: 'Agent 空闲了',
      message: '这轮任务跑完了，要趁机休息一下吗？',
      sub: '选择「休息一下」将提前结束本段专注进入休息',
      actions: [
        { id: 'break', label: '休息一下', style: 'primary' },
        { id: 'keep', label: '继续专注' },
        { id: 'ignore', label: '忽略' },
      ],
      defaultAction: 'ignore',
      timeoutMs: BREAK_SUGGEST_TIMEOUT_MS,
    }).then(({ action }) => {
      if (action === 'break') sendTimerCommand('skip');
    });
    return true;
  }

  // ---- HTTP 基础设施 ----
  function readBody(req) {
    return new Promise((resolve, reject) => {
      const chunks = [];
      let size = 0;
      req.on('data', (c) => {
        size += c.length;
        if (size > BODY_LIMIT) {
          reject(new Error('body too large'));
          req.destroy();
          return;
        }
        chunks.push(c);
      });
      req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
      req.on('error', reject);
    });
  }

  function send(res, code, obj) {
    const body = JSON.stringify(obj);
    res.writeHead(code, {
      'Content-Type': 'application/json; charset=utf-8',
      'Cache-Control': 'no-store',
    });
    res.end(body);
  }

  function authorized(req) {
    const m = /^Bearer\s+(.+)$/i.exec(String(req.headers['authorization'] || ''));
    if (!m || !token) return false;
    const got = Buffer.from(m[1]);
    const want = Buffer.from(token);
    return got.length === want.length && crypto.timingSafeEqual(got, want);
  }

  function hostOk(req) {
    const host = String(req.headers.host || '');
    return /^(127\.0\.0\.1|localhost)(:\d+)?$/i.test(host) || /^\[::1\](:\d+)?$/i.test(host);
  }

  async function handleRequest(req, res) {
    const u = new URL(req.url, 'http://127.0.0.1');
    const p = u.pathname;

    try {
      if (p === '/health' && req.method === 'GET') {
        return send(res, 200, { ok: true, app: 'pomodoro-fluent', gateway: GATEWAY_VERSION });
      }
      if (!hostOk(req)) return send(res, 403, { ok: false, error: 'forbidden host' });
      if (!authorized(req)) return send(res, 401, { ok: false, error: 'unauthorized' });

      if (req.method === 'GET' && p === '/api/status') {
        observeTimerState(getTimerState());
        const t = getTimerState() || {};
        return send(res, 200, {
          ok: true,
          timer: {
            phase: t.phase || 'work',
            running: !!t.running,
            remainMs: t.remainMs || 0,
            totalMs: t.totalMs || 0,
            completedFocus: t.completedFocus || 0,
            roundInCycle: t.roundInCycle || 1,
            rounds: t.rounds || 4,
          },
          activity: { ...activity },
          pending: pendingInteractions.size,
          gateway: { version: GATEWAY_VERSION, port },
        });
      }

      if (req.method === 'POST' && (
        p === '/api/notify' || p === '/api/event' || p === '/api/confirm' ||
        p === '/api/interaction' || p === '/api/timer'
      )) {
        const raw = await readBody(req);
        let body = {};
        try { body = raw ? JSON.parse(raw) : {}; } catch (e) {
          return send(res, 400, { ok: false, error: 'invalid json' });
        }
        if (p === '/api/notify') return send(res, 200, notifyPopup(body));
        if (p === '/api/event') return send(res, 200, handleEvent(body));
        if (p === '/api/timer') {
          const cmd = body.command;
          if (cmd !== 'toggle' && cmd !== 'reset' && cmd !== 'skip') {
            return send(res, 400, { ok: false, error: 'command must be toggle|reset|skip' });
          }
          sendTimerCommand(cmd);
          return send(res, 200, { ok: true });
        }
        // /api/interaction（长轮询等用户决策）；/api/confirm 为旧别名
        const result = await requestInteraction(p === '/api/confirm' ? { ...body, kind: 'custom' } : body);
        return send(res, 200, { ok: true, ...result });
      }

      return send(res, 404, { ok: false, error: 'not found' });
    } catch (e) {
      return send(res, 500, { ok: false, error: e.message });
    }
  }

  function listenOnce(srv, p) {
    return new Promise((resolve, reject) => {
      srv.once('error', reject);
      srv.listen(p, '127.0.0.1', () => {
        srv.removeListener('error', reject);
        resolve();
      });
    });
  }

  async function start() {
    if (running) return true;
    token = crypto.randomBytes(24).toString('hex');
    const srv = http.createServer(handleRequest);
    const base = Number(process.env.POMODORO_GATEWAY_PORT) || DEFAULT_PORT;
    let chosen = null;
    for (let off = 0; off < PORT_ATTEMPTS; off++) {
      const p = base + off;
      try {
        await listenOnce(srv, p);
        chosen = p;
        break;
      } catch (e) {
        if (e.code !== 'EADDRINUSE') throw e;
      }
    }
    if (chosen == null) throw new Error(`端口 ${base}~${base + PORT_ATTEMPTS - 1} 均被占用`);
    server = srv;
    port = chosen;
    running = true;
    writeDiscovery();
    log(`[gateway] Agent 网关已启动: http://127.0.0.1:${port}`);
    return true;
  }

  function stop() {
    if (server) {
      try { server.close(); } catch (e) { /* ignore */ }
      server = null;
    }
    dismissPending('dismissed');
    running = false;
    port = null;
    token = null;
    removeDiscovery();
    log('[gateway] Agent 网关已停止');
  }

  // ---- 网关自检（POMODORO_GATEWAY_SMOKE=1 时跑一遍全链路） ----
  function selfRequest(method, apiPath, body) {
    return new Promise((resolve, reject) => {
      const data = body ? JSON.stringify(body) : null;
      const req = http.request({
        host: '127.0.0.1', port, method, path: apiPath,
        headers: {
          Authorization: `Bearer ${token}`,
          ...(data ? { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) } : {}),
        },
        timeout: 12000,
      }, (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          try { resolve({ status: res.statusCode, body: JSON.parse(Buffer.concat(chunks).toString('utf8')) }); }
          catch (e) { reject(e); }
        });
      });
      req.on('error', reject);
      req.on('timeout', () => { req.destroy(); reject(new Error('timeout')); });
      if (data) req.write(data);
      req.end();
    });
  }

  async function smoke() {
    const results = [];
    const check = async (name, fn) => {
      try { await fn(); results.push(`PASS ${name}`); }
      catch (e) { results.push(`FAIL ${name}: ${e.message}`); }
    };
    await check('health', async () => {
      const r = await selfRequest('GET', '/health');
      if (r.status !== 200 || !r.body.ok) throw new Error(`status=${r.status}`);
    });
    await check('status', async () => {
      const r = await selfRequest('GET', '/api/status');
      if (r.status !== 200 || !r.body.ok) throw new Error(`status=${r.status}`);
    });
    await check('event(notification)', async () => {
      const r = await selfRequest('POST', '/api/event', { kind: 'notification', message: '[smoke] 权限确认测试', source: 'manual' });
      if (!r.body.ok) throw new Error(JSON.stringify(r.body));
    });
    await check('event(tool-after)', async () => {
      const r = await selfRequest('POST', '/api/event', { kind: 'tool-after', tool: 'smoke' });
      if (!r.body.ok || r.body.activity.toolCalls < 1) throw new Error(JSON.stringify(r.body));
    });
    await check('interaction(permission, timeout)', async () => {
      const r = await selfRequest('POST', '/api/interaction', {
        kind: 'permission', source: 'manual', title: '[smoke] 权限测试',
        message: '5 秒后自动超时', permission: { tool: 'Bash', rule: 'npm test', canAlways: true },
        timeoutMs: 5000,
      });
      if (r.body.decidedBy !== 'timeout' || r.body.action !== 'deny') throw new Error(JSON.stringify(r.body));
    });
    await check('interaction(ask, timeout)', async () => {
      const r = await selfRequest('POST', '/api/interaction', {
        kind: 'ask', source: 'manual', title: '[smoke] 提问测试',
        questions: [{ question: '继续吗？', header: '确认', multiSelect: false, options: [{ label: '继续' }, { label: '停下' }] }],
        timeoutMs: 5000,
      });
      if (r.body.decidedBy !== 'timeout' || r.body.action !== 'cancel') throw new Error(JSON.stringify(r.body));
    });
    await check('confirm(timeout)', async () => {
      const r = await selfRequest('POST', '/api/confirm', {
        title: '[smoke] 确认测试', message: '5 秒后自动超时', timeoutMs: 5000,
        defaultAction: 'deny', actions: [{ id: 'allow', label: '允许' }, { id: 'deny', label: '拒绝' }],
      });
      if (r.body.decidedBy !== 'timeout') throw new Error(JSON.stringify(r.body));
    });
    log('[gateway-smoke]\n  ' + results.join('\n  '));
    return results;
  }

  return {
    start, stop,
    isRunning: () => running,
    getPort: () => port,
    observeTimerState,
    resolveInteraction,
    resolveConfirm,
    dismissPending,
    requestInteraction,
    requestConfirm,
    handleEvent,
    notifyPopup,
    getActivity: () => ({ ...activity }),
    smoke,
  };
}

module.exports = { createGateway, GATEWAY_VERSION };
