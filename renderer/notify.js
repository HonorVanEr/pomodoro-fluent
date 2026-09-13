'use strict';

// ---------------------------------------------------------------------------
// 通知 / 交互弹窗页
//
// kind:
//   notification  纯通知，进度条走完自动消失
//   ask           提问：多问题选项（单选/多选）+ 自定义输入，弹窗内直接作答
//   permission    权限：允许 / 始终允许 / 拒绝，可填备注
//   custom        自定义按钮（旧 confirm 路径）
//
// 完整 payload 不在 URL 里，按 id 从主进程取（工具输入可能很长）。
// 渲染完成后实测内容高度回传主进程重设窗口，避免长内容被裁切。
// ---------------------------------------------------------------------------

const params = new URLSearchParams(window.location.search);
const POPUP_ID = params.get('id') || '';
const api = window.pomodoro;

const KIND_LABELS = {
  ask: '提问',
  permission: '权限',
  notification: '',
  custom: '',
};
const SOURCE_LABELS = {
  'zcode': 'ZCode',
  'claude-code': 'Claude Code',
  'opencode': 'OpenCode',
  'cursor': 'Cursor',
  'manual': '手动',
};
const TIMER_FLAVORS = new Set(['work', 'break', 'longBreak', 'idle', 'custom']);

const ICONS = {
  work: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 2v6M12 2C6 2 3 6 3 9c0 2 1.5 3 3 3 2 0 3-1 3-3 0-2-1-4-2.2-5C8 4.6 9.5 4 12 4"/>
      <path d="M12 2c6 0 9 4 9 7 0 2-1.5 3-3 3-2 0-3-1-3-3 0-2 1-4 2.2-5C16 4.6 14.5 4 12 4"/>
      <path d="M12 4v2"/><path d="M12 8v1"/>
      <path d="M12 22c-4 0-6-2-6-4 0-2 2-3 6-3s6 1 6 3c0 2-2 4-6 4z"/>
    </svg>`,
  break: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 2a10 10 0 1 0 10 10"/><path d="M12 6a6 6 0 1 1-6 6"/><path d="M12 12l4-2"/>
    </svg>`,
  longBreak: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M3 12a9 9 0 1 0 18 0"/><path d="M12 3v3M12 12l3-3"/><path d="M6.5 6.5l1.5 1.5"/>
    </svg>`,
  idle: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <circle cx="12" cy="13" r="8"/><path d="M12 9v4l3 2"/><path d="M9 2h6"/>
    </svg>`,
  agent: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <rect x="4" y="8" width="16" height="11" rx="3"/><path d="M12 8V5"/><circle cx="12" cy="3.5" r="1.5"/>
      <path d="M9 13.5h.01M15 13.5h.01"/><path d="M9.5 16.5h5"/><path d="M2 12v4M22 12v4"/>
    </svg>`,
  ask: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M21 12a8 8 0 1 1-3.2-6.4"/>
      <path d="M17 3.5V8h-4.5"/>
      <path d="M9.6 9.4a2.6 2.6 0 1 1 3.2 2.6c-.6.2-.8.7-.8 1.3v.4"/>
      <path d="M12 17h.01"/>
    </svg>`,
  permission: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 3l7 3v5.5c0 4.2-2.8 7.6-7 9.5-4.2-1.9-7-5.3-7-9.5V6l7-3z"/>
      <path d="M9.2 12.2l2 2 3.6-3.8"/>
    </svg>`,
};

const el = (id) => document.getElementById(id);
const state = {
  kind: 'notification',
  flavor: 'agent',
  interactive: false,
  questions: [],
  selection: new Map(), // qid -> Set(oid)
  timeoutMs: 0,
  submitted: false,
};

function autoResize() {
  const card = el('notify-card');
  const width = state.kind === 'ask' ? 460 : (state.kind === 'permission' ? 432 : 400);
  card.style.height = 'auto';
  const h = Math.ceil(card.getBoundingClientRect().height) + 2;
  card.style.height = '';
  if (api && api.resizeNotify) api.resizeNotify(width, h);
}

function respond(result) {
  if (state.submitted) return;
  state.submitted = true;
  if (api && api.respondInteraction) {
    api.respondInteraction({ id: POPUP_ID, ...result });
  }
  // 主进程在 resolve 成功时会主动关窗；这里补一刀兜底
  // （纯通知带按钮时网关没在等结果，主进程不会关）
  setTimeout(() => dismiss(), 250);
}

function dismiss() {
  if (api && api.closeNotify) api.closeNotify(POPUP_ID);
}

// ---------------- 渲染：ask ----------------
function renderAsk(payload) {
  const form = el('notifyForm');
  form.hidden = false;
  state.questions = Array.isArray(payload.questions) ? payload.questions : [];

  state.questions.forEach((q, qi) => {
    const block = document.createElement('div');
    block.className = 'q-block';
    block.dataset.qid = q.id;

    const head = document.createElement('div');
    head.className = 'q-head';
    if (q.header) {
      const chip = document.createElement('span');
      chip.className = 'q-chip';
      chip.textContent = q.header;
      head.appendChild(chip);
    }
    const text = document.createElement('span');
    text.className = 'q-text';
    text.textContent = q.question;
    head.appendChild(text);
    block.appendChild(head);

    if (q.options && q.options.length) {
      const opts = document.createElement('div');
      opts.className = 'q-options';
      q.options.forEach((o, oi) => {
        const btn = document.createElement('button');
        btn.className = 'q-option';
        btn.dataset.qid = q.id;
        btn.dataset.oid = o.id;
        btn.dataset.idx = String(oi + 1);
        const idx = document.createElement('span');
        idx.className = 'q-option-idx';
        idx.textContent = String(oi + 1);
        const label = document.createElement('span');
        label.className = 'q-option-label';
        label.textContent = o.label;
        btn.appendChild(idx);
        btn.appendChild(label);
        if (o.description) {
          const desc = document.createElement('span');
          desc.className = 'q-option-desc';
          desc.textContent = o.description;
          btn.appendChild(desc);
        }
        btn.addEventListener('click', (e) => {
          e.stopPropagation();
          selectOption(q, o.id);
        });
        opts.appendChild(btn);
      });
      block.appendChild(opts);
    }

    if (q.custom) {
      const input = document.createElement('input');
      input.type = 'text';
      input.className = 'q-custom';
      input.placeholder = '或输入自定义回答…';
      input.dataset.qid = q.id;
      input.addEventListener('input', () => {
        // 自定义回答优先：一旦输入就清掉该题的选项选择
        if (input.value.trim()) state.selection.delete(q.id);
        syncOptionUI();
      });
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); submitAsk(); }
      });
      block.appendChild(input);
    }

    form.appendChild(block);
    void qi;
  });

  const actions = [
    { id: 'submit', label: '提交回答', style: 'primary' },
    { id: 'cancel', label: '取消', style: 'default' },
  ];
  renderActions(actions, (id) => {
    if (id === 'submit') submitAsk();
    else respond({ action: 'cancel', answers: {}, text: '' });
  });
}

function selectOption(q, oid) {
  const cur = state.selection.get(q.id) || new Set();
  if (q.multiSelect) {
    if (cur.has(oid)) cur.delete(oid); else cur.add(oid);
  } else {
    cur.clear();
    cur.add(oid);
  }
  state.selection.set(q.id, cur);
  syncOptionUI();
  // 单题单选：选中即提交，少点一次
  if (state.questions.length === 1 && !q.multiSelect && !q.custom) {
    setTimeout(() => submitAsk(), 180);
  }
}

function syncOptionUI() {
  document.querySelectorAll('.q-option').forEach((btn) => {
    const sel = state.selection.get(btn.dataset.qid);
    btn.classList.toggle('selected', !!(sel && sel.has(btn.dataset.oid)));
  });
}

function collectAnswers() {
  const answers = {};
  state.questions.forEach((q) => {
    const custom = document.querySelector(`.q-custom[data-qid="${cssEsc(q.id)}"]`);
    const typed = custom ? custom.value.trim() : '';
    if (typed) { answers[q.id] = [typed]; return; }
    const sel = state.selection.get(q.id);
    if (sel && sel.size) {
      answers[q.id] = q.options.filter((o) => sel.has(o.id)).map((o) => o.label);
    }
  });
  return answers;
}

function cssEsc(s) {
  return String(s).replace(/["\\]/g, '\\$&');
}

function submitAsk() {
  const answers = collectAnswers();
  const unanswered = state.questions.filter((q) => !(answers[q.id] && answers[q.id].length));
  if (unanswered.length) {
    const first = document.querySelector(`.q-block[data-qid="${cssEsc(unanswered[0].id)}"]`);
    if (first) {
      first.classList.remove('shake');
      void first.offsetWidth;
      first.classList.add('shake');
      const input = first.querySelector('.q-custom');
      if (input) input.focus();
    }
    return;
  }
  respond({ action: 'submit', answers, text: '' });
}

// ---------------- 渲染：permission ----------------
function renderPermission(payload) {
  const form = el('notifyForm');
  const inputCfg = payload.input || {};
  if (inputCfg.enabled !== false) {
    form.hidden = false;
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'q-custom';
    input.id = 'noteInput';
    input.placeholder = inputCfg.placeholder || '补充说明 / 拒绝理由（可选）';
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); decide('allow'); }
    });
    form.appendChild(input);
  }

  const p = payload.permission || {};
  const actions = [{ id: 'allow', label: '允许一次', style: 'primary' }];
  if (p.canAlways !== false) {
    actions.push({ id: 'allow-always', label: '始终允许', style: 'default' });
  }
  actions.push({ id: 'deny', label: '拒绝', style: 'danger' });
  renderActions(actions, decide);
}

function decide(action) {
  const note = document.getElementById('noteInput');
  respond({ action, answers: {}, text: note ? note.value.trim() : '' });
}

// ---------------- 通用：按钮行 ----------------
function renderActions(actions, onPick) {
  const wrap = el('notifyActions');
  wrap.innerHTML = '';
  actions.forEach((a) => {
    const btn = document.createElement('button');
    btn.className = `notify-action ${a.style === 'primary' || a.style === 'danger' ? a.style : 'default'}`;
    btn.textContent = a.label;
    btn.dataset.action = a.id;
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      onPick(a.id);
    });
    wrap.appendChild(btn);
  });
  wrap.hidden = actions.length === 0;
}

// ---------------- 主流程 ----------------
function render(payload) {
  const p = payload || {};
  const card = el('notify-card');
  state.kind = p.kind || 'notification';
  state.flavor = p.flavor || 'agent';
  state.interactive = !!p.interactive;
  state.timeoutMs = Number(p.timeoutMs) > 0 ? Number(p.timeoutMs) : 0;

  card.dataset.kind = state.kind;
  card.dataset.flavor = state.flavor;

  el('notifyTitle').textContent = p.title || '时间到';
  el('notifyMessage').textContent = p.message || '';
  el('notifySub').textContent = p.sub || '';

  const detail = el('notifyDetail');
  if (p.detail) {
    detail.textContent = p.detail;
    detail.hidden = false;
  } else {
    detail.hidden = true;
  }

  // 徽标：agent 类事件才显示「提问 / 权限 / 来源」，定时器通知不显示
  const kindBadge = el('notifyKind');
  const srcBadge = el('notifySource');
  const isAgent = !TIMER_FLAVORS.has(state.flavor);
  const kindText = KIND_LABELS[state.kind] || '';
  if (isAgent && kindText) {
    kindBadge.textContent = kindText;
    kindBadge.hidden = false;
  } else {
    kindBadge.hidden = true;
  }
  const srcText = SOURCE_LABELS[p.source] || (p.source && isAgent ? String(p.source) : '');
  if (srcText) {
    srcBadge.textContent = srcText;
    srcBadge.hidden = false;
  } else {
    srcBadge.hidden = true;
  }

  el('notifyIcon').innerHTML = ICONS[state.flavor] || ICONS.agent;

  // 进度条：交互窗为决策剩余时间；通知为 5s 自动关闭
  const durationMs = state.interactive
    ? Math.min(Math.max(state.timeoutMs, 5000), 600000)
    : 5000;
  el('notifyProgress').style.animationDuration = `${durationMs}ms`;

  if (state.kind === 'ask') renderAsk(p);
  else if (state.kind === 'permission') renderPermission(p);
  else if (p.actions && p.actions.length) {
    // notification / custom 带自定义按钮
    renderActions(p.actions, (id) => respond({ action: id, answers: {}, text: '' }));
  }

  // 走完倒计时自行关闭（网关侧会同时按 timeout 兜底）
  setTimeout(() => { if (!state.submitted) dismiss(); }, durationMs);

  // 渲染完成后把真实高度回传给主进程
  requestAnimationFrame(() => autoResize());
}

function bindGlobal() {
  const closeBtn = el('notifyClose');
  closeBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    dismiss();
  });

  const card = el('notify-card');
  card.addEventListener('click', (e) => {
    if (state.interactive) return; // 交互窗点击空白不关闭，避免误触丢答案
    if (e.target.closest('.notify-close')) return;
    dismiss();
  });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (state.kind === 'permission') decide('deny');
      else if (state.kind === 'ask') respond({ action: 'cancel', answers: {}, text: '' });
      else dismiss();
      return;
    }
    if (e.key === 'Enter') {
      const tag = (e.target && e.target.tagName || '').toLowerCase();
      if (tag === 'input' || tag === 'textarea') return; // 输入框自己处理
      e.preventDefault();
      if (state.kind === 'permission') decide('allow');
      else if (state.kind === 'ask') submitAsk();
      else {
        const btn = document.querySelector('.notify-action.primary') || document.querySelector('.notify-action');
        if (btn) btn.click();
        else dismiss();
      }
      return;
    }
    // 数字键快速选项（提问）
    if (state.kind === 'ask' && /^[1-9]$/.test(e.key)) {
      const q = state.questions.find((qq) => !(state.selection.get(qq.id) || new Set()).size);
      const target = q || state.questions[0];
      if (!target || !target.options) return;
      const opt = target.options[Number(e.key) - 1];
      if (opt) { e.preventDefault(); selectOption(target, opt.id); }
    }
  });
}

document.addEventListener('DOMContentLoaded', () => {
  bindGlobal();
  if (!api || !api.getNotifyPayload) {
    render({ kind: 'notification', flavor: 'agent', title: '时间到' });
    return;
  }
  api.getNotifyPayload(POPUP_ID).then((payload) => {
    render(payload || { kind: 'notification', flavor: 'agent', title: '时间到' });
  }).catch(() => {
    render({ kind: 'notification', flavor: 'agent', title: '时间到' });
  });
});
