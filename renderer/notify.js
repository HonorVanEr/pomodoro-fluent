'use strict';

// 解析 loadFile query 参数（window.location.search）
const params = new URLSearchParams(window.location.search);
const title = params.get('title') || '时间到';
const message = params.get('message') || '';
const type = params.get('type') || 'work';
const sub = params.get('sub') || '';

document.addEventListener('DOMContentLoaded', () => {
  const card = document.getElementById('notify-card');
  card.dataset.type = type;

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
  };
  document.getElementById('notifyIcon').innerHTML = icons[type] || icons.work;

  // 关闭
  const close = document.getElementById('notifyClose');
  close.addEventListener('click', () => {
    // 通过 preload 通知主进程关闭
    if (window.pomodoro && window.pomodoro.closeNotify) {
      window.pomodoro.closeNotify();
    }
  });

  // 进度条结束后自动关闭
  setTimeout(() => {
    if (window.pomodoro && window.pomodoro.closeNotify) {
      window.pomodoro.closeNotify();
    }
  }, 5000);

  // 点击卡片主体关闭
  card.addEventListener('click', (e) => {
    if (e.target.closest('.notify-close')) return;
    if (window.pomodoro && window.pomodoro.closeNotify) {
      window.pomodoro.closeNotify();
    }
  });
});
