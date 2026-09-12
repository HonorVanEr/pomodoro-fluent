'use strict';

// 解析 loadFile query 参数（window.location.search）
const params = new URLSearchParams(window.location.search);
const title = params.get('title') || '时间到';
const message = params.get('message') || '';
const type = params.get('type') || 'work';
const sub = params.get('sub') || '';
const mode = params.get('mode') === 'confirm' ? 'confirm' : 'notify';
const confirmId = params.get('confirmId') || '';
const timeoutMs = Number(params.get('timeoutMs')) || 0;

function parseActions() {
  try {
    const list = JSON.parse(params.get('actions') || '[]');
    if (Array.isArray(list)) return list;
  } catch (e) { /* ignore */ }
  return [];
}

document.addEventListener('DOMContentLoaded', () => {
  const card = document.getElementById('notify-card');
  card.dataset.type = type;
  if (mode === 'confirm') card.dataset.mode = 'confirm';

  document.getElementById('notifyTitle').textContent = title;
  document.getElementById('notifyMessage').textContent = message;
  document.getElementById('notifySub').textContent = sub;

  // 图标
  const icons = {
    work: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 2v6M12 2C6 2 3 6 3 9c0 2 1.5 3 3 3 2 0 3-1 3-3 0-2-1-4-2.2-5C8 4.6 9.5 4 12 4"/>
      <path d="M12 2c6 0 9 4 9 7 0 2-1.5 3-3 3-2 0-3-1-3-3 0-2 1-4 2.2-5C16 4.6 14.5 4 12 4"/>
      <path d="M12 4v2"/>
      <path d="M12 8v1"/>
      <path d="M12 22c-4 0-6-2-6-4 0-2 2-3 6-3s6 1 6 3c0 2-2 4-6 4z"/>
    </svg>`,
    break: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 2a10 10 0 1 0 10 10"/>
      <path d="M12 6a6 6 0 1 1-6 6"/>
      <path d="M12 12l4-2"/>
    </svg>`,
    longBreak: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M3 12a9 9 0 1 0 18 0"/>
      <path d="M12 3v3M12 12l3-3"/>
      <path d="M6.5 6.5l1.5 1.5"/>
    </svg>`,
    idle: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <circle cx="12" cy="13" r="8"/>
      <path d="M12 9v4l3 2"/>
      <path d="M9 2h6"/>
    </svg>`,
    agent: `<svg viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <rect x="4" y="8" width="16" height="11" rx="3"/>
      <path d="M12 8V5"/>
      <circle cx="12" cy="3.5" r="1.5"/>
      <path d="M9 13.5h.01M15 13.5h.01"/>
      <path d="M9.5 16.5h5"/>
      <path d="M2 12v4M22 12v4"/>
    </svg>`,
  };
  document.getElementById('notifyIcon').innerHTML = icons[type] || icons.work;

  // 进度条：notify 模式为 5s 自动关闭倒计时；confirm 模式为决策剩余时间
  const progress = document.getElementById('notifyProgress');
  const durationMs = mode === 'confirm' ? Math.min(Math.max(timeoutMs, 5000), 600000) : 5000;
  progress.style.animationDuration = `${durationMs}ms`;

  // 确认模式：渲染按钮，不自动关闭、点卡片不关闭
  const close = document.getElementById('notifyClose');
  if (mode === 'confirm') {
    const wrap = document.getElementById('notifyActions');
    const actions = parseActions();
    actions.forEach((a) => {
      const btn = document.createElement('button');
      btn.className = `notify-action ${a.style === 'primary' || a.style === 'danger' ? a.style : 'default'}`;
      btn.textContent = a.label || a.id;
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        if (window.pomodoro && window.pomodoro.respondConfirm) {
          window.pomodoro.respondConfirm(confirmId, a.id);
        }
      });
      wrap.appendChild(btn);
    });
    wrap.hidden = actions.length === 0;
    // 倒计时归零自行关闭（网关侧会同时按 timeout 兜底）
    setTimeout(() => {
      if (window.pomodoro && window.pomodoro.closeNotify) window.pomodoro.closeNotify();
    }, durationMs);
  } else {
    // 进度条结束后自动关闭
    setTimeout(() => {
      if (window.pomodoro && window.pomodoro.closeNotify) window.pomodoro.closeNotify();
    }, 5000);
  }

  // 关闭按钮
  close.addEventListener('click', () => {
    if (window.pomodoro && window.pomodoro.closeNotify) {
      window.pomodoro.closeNotify();
    }
  });

  // 点击卡片主体关闭（确认模式下卡片点击不关闭，避免误触）
  card.addEventListener('click', (e) => {
    if (mode === 'confirm') return;
    if (e.target.closest('.notify-close')) return;
    if (window.pomodoro && window.pomodoro.closeNotify) {
      window.pomodoro.closeNotify();
    }
  });
});
