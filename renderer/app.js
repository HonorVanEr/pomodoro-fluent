'use strict';

// ============================================================
// 番茄钟核心逻辑
// ============================================================

const PomodoroApp = (() => {
  // ---- 状态 ----
  const state = {
    phase: 'work',            // work | break | longBreak
    running: false,
    totalMs: 25 * 60 * 1000,  // 当前阶段总时长
    remainMs: 25 * 60 * 1000, // 剩余时长
    timerId: null,
    lastTick: 0,

    // 会话
    completedFocus: 0,        // 累计完成专注数
    roundInCycle: 1,          // 当前周期内第几轮（1~rounds）
    autoNext: false,

    // 设置
    durations: { work: 25, break: 5, longBreak: 15 },
    rounds: 4,
  };

  // 本专注期 agent 活动（主进程经 state:agent-activity 推送，不持久化）
  let agentActivity = { toolCalls: 0, interruptions: 0, stops: 0, sessions: 0 };
  let gatewayState = { enabled: false, port: null, hookPath: null };

  // ---- DOM 引用 ----
  const $ = (id) => document.getElementById(id);
  const dom = {
    app: $('app'),
    timeDisplay: $('timeDisplay'),
    phaseLabel: $('phaseLabel'),
    roundLabel: $('roundLabel'),
    ringProgress: $('ringProgress'),
    ringCenter: $('ringCenter'),
    roundDots: $('roundDots'),
    phaseTabs: $('phaseTabs'),
    btnStart: $('btnStart'),
    btnReset: $('btnReset'),
    btnSkip: $('btnSkip'),
    btnPin: $('btnPin'),
    btnMin: $('btnMin'),
    btnClose: $('btnClose'),
    btnSettings: $('btnSettings'),
    controlsHint: $('controlsHint'),
    iconPlay: $('iconPlay'),
    iconPause: $('iconPause'),
    iconResume: $('iconResume'),
    settingsDrawer: $('settingsDrawer'),
    drawerBackdrop: $('drawerBackdrop'),
    drawerClose: $('drawerClose'),
    drawerSave: $('drawerSave'),
    sWork: $('sWork'),
    sBreak: $('sBreak'),
    sLong: $('sLong'),
    sRounds: $('sRounds'),
    sAuto: $('sAuto'),
    sGateway: $('sGateway'),
    agentMeta: $('agentMeta'),
    agentSelect: $('agentSelect'),
    btnCopyHook: $('btnCopyHook'),
    btnCopyInstall: $('btnCopyInstall'),
    agentActivity: $('agentActivity'),
  };

  const RING_CIRCUM = 615.75; // 2π·98

  // ---- 持久化 ----
  const STORE_KEY = 'pomodoro-settings-v1';
  function loadSettings() {
    try {
      const raw = localStorage.getItem(STORE_KEY);
      if (raw) {
        const s = JSON.parse(raw);
        if (s.durations) state.durations = { ...state.durations, ...s.durations };
        if (s.rounds) state.rounds = s.rounds;
        if (typeof s.autoNext === 'boolean') state.autoNext = s.autoNext;
        // 恢复未完成计时
        if (s.phase && typeof s.remainMs === 'number' && s.remainMs > 0 && !s.running) {
          state.phase = s.phase;
          state.totalMs = s.totalMs;
          state.remainMs = s.remainMs;
          state.completedFocus = s.completedFocus || 0;
          state.roundInCycle = s.roundInCycle || 1;
        }
      }
    } catch (e) { /* ignore */ }
    state.totalMs = state.durations[state.phase] * 60 * 1000;
    if (state.remainMs > state.totalMs) state.remainMs = state.totalMs;
  }
  function saveSettings() {
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify({
        durations: state.durations,
        rounds: state.rounds,
        autoNext: state.autoNext,
        phase: state.phase,
        totalMs: state.totalMs,
        remainMs: state.remainMs,
        running: state.running,
        completedFocus: state.completedFocus,
        roundInCycle: state.roundInCycle,
      }));
    } catch (e) { /* ignore */ }
  }

  // ---- 格式化 ----
  function fmt(ms) {
    const total = Math.max(0, Math.ceil(ms / 1000));
    const m = Math.floor(total / 60);
    const s = total % 60;
    return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }

  // ---- 阶段元数据 ----
  const PHASE_META = {
    work:      { label: '专注时间', hint: '保持专注，屏蔽干扰' },
    break:     { label: '短休息',   hint: '起来活动一下，喝口水' },
    longBreak: { label: '长休息',   hint: '辛苦了，好好放松一下' },
  };

  // ---- 渲染（按需更新：仅真正变化的 DOM 才写入，避免高频全量重排/重绘） ----
  const shown = {
    timeText: null,    // 已渲染的时间文本
    ringOffset: null,  // 已渲染的进度环偏移
    phase: null,       // 已应用的阶段样式
    labelKey: null,    // 轮次标签状态指纹
    dotsKey: null,     // 会话圆点状态指纹
    controlsKey: null, // 主按钮/提示状态指纹
    dockP: null,       // 贴边进度条比例
  };

  function render(opts = {}) {
    // 时间文本
    const timeText = fmt(state.remainMs);
    if (timeText !== shown.timeText) {
      shown.timeText = timeText;
      dom.timeDisplay.textContent = timeText;
    }

    // 环进度（instant：重置/切换阶段时跳过过渡，避免进度环动画倒转一圈）
    const progress = state.totalMs > 0 ? state.remainMs / state.totalMs : 0;
    const offset = RING_CIRCUM * (1 - progress);
    if (offset !== shown.ringOffset || opts.instant) {
      shown.ringOffset = offset;
      setRingOffset(offset, opts.instant);
    }

    // 贴边进度条（细条模式下唯一的时间指示）
    const dockP = Math.round(progress * 1000) / 1000;
    if (dockP !== shown.dockP) {
      shown.dockP = dockP;
      document.body.style.setProperty('--dock-p', String(dockP));
    }

    // 阶段样式与标签
    if (state.phase !== shown.phase) {
      shown.phase = state.phase;
      // 注意：用 classList 切换，不能整体覆盖 className（会抹掉 mini 等模式类）
      document.body.classList.remove('phase-work', 'phase-break', 'phase-longBreak');
      document.body.classList.add(`phase-${state.phase}`);
      dom.phaseLabel.textContent = PHASE_META[state.phase].label;
      updateAgentActivityUI();
      // 同步阶段 Tab 高亮（跳过/托盘/自动切换阶段时不经过 Tab 点击）
      dom.phaseTabs.querySelectorAll('.phase-tab').forEach((t) => {
        t.classList.toggle('active', t.dataset.phase === state.phase);
      });
    }

    // 轮次标签
    const labelKey = `${state.phase}|${state.completedFocus}|${state.roundInCycle}|${state.rounds}`;
    if (labelKey !== shown.labelKey) {
      shown.labelKey = labelKey;
      if (state.phase === 'work') {
        dom.roundLabel.textContent = `第 ${state.completedFocus + 1} 轮 · 本轮 ${state.roundInCycle}/${state.rounds}`;
      } else if (state.phase === 'break') {
        dom.roundLabel.textContent = `休息中 · 已完成 ${state.completedFocus} 个专注`;
      } else {
        dom.roundLabel.textContent = `长休息 · 已完成 ${state.completedFocus} 个专注`;
      }
    }

    // 会话圆点
    const dotsKey = `${state.rounds}|${state.roundInCycle}|${state.phase}`;
    if (dotsKey !== shown.dotsKey) {
      shown.dotsKey = dotsKey;
      renderDots();
    }

    // 主按钮图标与提示
    const paused = state.remainMs < state.totalMs && state.remainMs > 0;
    const controlsKey = `${state.running}|${paused}|${state.phase}`;
    if (controlsKey !== shown.controlsKey) {
      shown.controlsKey = controlsKey;
      if (state.running) {
        dom.btnStart.classList.add('running');
        dom.iconPlay.style.display = 'none';
        dom.iconPause.style.display = 'block';
        dom.iconResume.style.display = 'none';
        dom.controlsHint.textContent = PHASE_META[state.phase].hint;
      } else {
        dom.btnStart.classList.remove('running');
        dom.iconPlay.style.display = paused ? 'none' : 'block';
        dom.iconPause.style.display = 'none';
        dom.iconResume.style.display = paused ? 'block' : 'none';
        dom.controlsHint.textContent = paused ? '已暂停 · 点击继续' : '点击开始';
      }
    }

    // 同步托盘（主进程侧做了去重，此调用本身很廉价）
    syncTray();
  }

  function setRingOffset(offset, instant) {
    if (instant) {
      dom.ringProgress.style.transition = 'none';
      dom.ringProgress.style.strokeDashoffset = offset;
      void dom.ringProgress.getBoundingClientRect(); // 强制回流使跳变立即生效
      dom.ringProgress.style.transition = '';
    } else {
      dom.ringProgress.style.strokeDashoffset = offset;
    }
  }

  function renderDots() {
    const count = state.rounds;
    let html = '';
    for (let i = 1; i <= count; i++) {
      const done = i < state.roundInCycle && state.phase !== 'work' ? 'done'
        : (i < state.roundInCycle ? 'done' : '');
      const current = (i === state.roundInCycle && state.phase === 'work') ? 'current' : '';
      html += `<span class="round-dot ${done} ${current}"></span>`;
    }
    dom.roundDots.innerHTML = html;
  }

  function syncTray() {
    const api = window.pomodoro;
    if (!api) return;
    api.updateTray({
      running: state.running,
      phase: state.phase,
      timeLeftText: fmt(state.remainMs),
      // 完整状态供主进程缓存（agent 网关 /api/status 查询）
      remainMs: state.remainMs,
      totalMs: state.totalMs,
      completedFocus: state.completedFocus,
      roundInCycle: state.roundInCycle,
      rounds: state.rounds,
    });
  }

  // ---- 计时 ----
  function start() {
    if (state.running) return;
    state.running = true;
    state.lastTick = Date.now();
    // 显示为秒级，1s 一次即可；进度环配合 1s 线性过渡保持平滑
    state.timerId = setInterval(tick, 1000);
    render();
  }

  function pause() {
    if (!state.running) return;
    state.running = false;
    clearInterval(state.timerId);
    state.timerId = null;
    render();
  }

  function toggle() {
    state.running ? pause() : start();
  }

  function tick() {
    const now = Date.now();
    const elapsed = now - state.lastTick;
    state.lastTick = now;
    state.remainMs = Math.max(0, state.remainMs - elapsed);

    if (state.remainMs <= 0) {
      state.remainMs = 0;
      render();
      completePhase();
      return;
    }
    render();
  }

  // ---- 阶段完成 ----
  function completePhase() {
    stopTimer();

    const was = state.phase;
    if (was === 'work') {
      state.completedFocus += 1;
      const stats = agentStatsSuffix();
      // 决定进入短休还是长休
      if (state.completedFocus % state.rounds === 0) {
        setPhase('longBreak');
        notify('work', '专注完成！', '干得漂亮！进入长休息', `已完成 ${state.completedFocus} 个番茄${stats}`);
      } else {
        setPhase('break');
        notify('work', '专注完成！', '太棒了，休息一下再继续', `已完成 ${state.completedFocus} 个番茄${stats}`);
      }
      // 自动进入下一阶段
      if (state.autoNext) {
        setTimeout(() => { start(); }, 600);
      }
    } else {
      // 休息结束 → 回到专注
      setPhase('work');
      notify('break', '休息结束', '准备好开始下一轮专注了吗？', `即将开始第 ${state.completedFocus + 1} 轮`);
      if (state.autoNext) {
        setTimeout(() => { start(); }, 600);
      }
    }
  }

  function stopTimer() {
    state.running = false;
    if (state.timerId) { clearInterval(state.timerId); state.timerId = null; }
  }

  // ---- 切换阶段 ----
  function setPhase(phase, opts = {}) {
    state.phase = phase;
    state.totalMs = state.durations[phase] * 60 * 1000;
    state.remainMs = state.totalMs;
    if (opts.resetRound) state.roundInCycle = 1;
    stopTimer();
    render({ instant: true });
  }

  // ---- 通知 ----
  function notify(type, title, message, sub) {
    const api = window.pomodoro;
    if (!api) return;
    api.showNotify({ type, title, message, sub });
  }

  // ---- Agent 活动（hook 上报）----
  function updateAgentActivityUI() {
    const a = agentActivity;
    const show = state.phase === 'work' && (a.toolCalls > 0 || a.interruptions > 0);
    dom.agentActivity.hidden = !show;
    if (show) {
      dom.agentActivity.textContent = `🤖 工具 ${a.toolCalls} · 打断 ${a.interruptions}`;
    }
  }

  // 专注结束通知的统计后缀（无活动时为空串）
  function agentStatsSuffix() {
    const a = agentActivity;
    if (a.toolCalls <= 0 && a.interruptions <= 0) return '';
    const parts = [];
    if (a.toolCalls > 0) parts.push(`🤖 工具×${a.toolCalls}`);
    if (a.interruptions > 0) parts.push(`打断×${a.interruptions}`);
    return ` · ${parts.join(' ')}`;
  }

  // 各家 agent 的 hook 配置片段（复制给用户粘贴/手动编辑）
  function buildHookSnippet(agent, hookPath, pluginPath) {
    // 统一带 --source，弹窗徽标才不会认错宿主
    const cmd = (src) => `node "${hookPath}" --source ${src}`;
    if (agent === 'zcode') {
      return JSON.stringify({
        hooks: {
          enabled: true,
          timeoutMs: 600000,
          events: {
            PermissionRequest: [{ matcher: '*', hooks: [{ type: 'command', command: cmd('zcode'), timeoutMs: 600000 }] }],
            // AskUserQuestion：ZCode 会同时触发 PreToolUse 与 PermissionRequest，两个都接上
            PreToolUse: [{ matcher: 'AskUserQuestion', hooks: [{ type: 'command', command: cmd('zcode'), timeoutMs: 600000 }] }],
            Stop: [{ hooks: [{ type: 'command', command: cmd('zcode') }] }],
            PostToolUse: [{ matcher: '*', hooks: [{ type: 'command', command: cmd('zcode') }] }],
            PostToolUseFailure: [{ matcher: '*', hooks: [{ type: 'command', command: cmd('zcode') }] }],
          },
        },
      }, null, 2);
    }
    if (agent === 'opencode') {
      return JSON.stringify({
        plugin: [`file://${(pluginPath || '').replace(/\\/g, '/')}`],
      }, null, 2);
    }
    if (agent === 'vscode') {
      // VS Code Copilot Agent hooks：与 Claude Code 同格式，用户级放 ~/.copilot/hooks/*.json。
      // 只有 8 个事件（无 PermissionRequest / Notification），审批走 PreToolUse；
      // VS Code 会忽略 matcher，只拦高风险工具的判断在 CLI 里做。
      // timeout 单位是秒、默认只有 30 → 要等弹窗就必须显式调大。
      return JSON.stringify({
        version: 1,
        hooks: {
          PreToolUse: [{ type: 'command', command: cmd('vscode'), timeout: 600 }],
          PostToolUse: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
          SessionStart: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
          UserPromptSubmit: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
          SubagentStart: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
          SubagentStop: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
          PreCompact: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
          Stop: [{ type: 'command', command: cmd('vscode'), timeout: 30 }],
        },
      }, null, 2);
    }
    if (agent === 'cursor') {
      // Cursor：~/.cursor/hooks.json（用户级）或 .cursor/hooks.json（项目级）
      return JSON.stringify({
        version: 1,
        hooks: {
          beforeShellExecution: [{ command: cmd('cursor'), timeout: 600 }],
          preToolUse: [{ command: cmd('cursor'), timeout: 600 }],
          beforeMCPExecution: [{ command: cmd('cursor'), timeout: 600 }],
          beforeSubmitPrompt: [{ command: cmd('cursor') }],
          afterFileEdit: [{ command: cmd('cursor') }],
          afterShellExecution: [{ command: cmd('cursor') }],
          afterAgentResponse: [{ command: cmd('cursor') }],
          stop: [{ command: cmd('cursor') }],
        },
      }, null, 2);
    }
    if (agent === 'codex') {
      // Codex 只有 notify：回合结束回调一次（无权限决策通道）
      return [
        '# ~/.codex/config.toml',
        `notify = ["node", "${(hookPath || '').replace(/\\/g, '\\\\')}", "codex-notify"]`,
      ].join('\n');
    }
    if (agent === 'qwen') {
      return JSON.stringify({
        hooks: {
          Notification: [{ hooks: [{ type: 'command', command: cmd('qwen') }] }],
          PreToolUse: [{ matcher: 'AskUserQuestion|askQuestions', hooks: [{ type: 'command', command: cmd('qwen'), timeout: 600 }] }],
          Stop: [{ hooks: [{ type: 'command', command: cmd('qwen') }] }],
          PostToolUse: [{ matcher: '*', hooks: [{ type: 'command', command: cmd('qwen') }] }],
        },
      }, null, 2);
    }
    return JSON.stringify({
      hooks: {
        Notification: [{ hooks: [{ type: 'command', command: cmd('claude-code') }] }],
        PermissionRequest: [{ matcher: '*', hooks: [{ type: 'command', command: cmd('claude-code'), timeout: 600 }] }],
        PreToolUse: [{ matcher: 'AskUserQuestion', hooks: [{ type: 'command', command: cmd('claude-code'), timeout: 600 }] }],
        Stop: [{ hooks: [{ type: 'command', command: cmd('claude-code') }] }],
        SubagentStop: [{ hooks: [{ type: 'command', command: cmd('claude-code') }] }],
        PostToolUse: [{ matcher: '*', hooks: [{ type: 'command', command: cmd('claude-code') }] }],
      },
    }, null, 2);
  }

  // 一键安装命令：交给 hook CLI 自己写配置（会先备份原文件）
  function buildInstallCommand(agent, hookPath) {
    return `node "${hookPath}" install --agent ${agent}`;
  }

  // ---- 事件绑定 ----
  function bindEvents() {
    // 阶段切换（未运行时）
    dom.phaseTabs.querySelectorAll('.phase-tab').forEach((tab) => {
      tab.addEventListener('click', () => {
        const p = tab.dataset.phase;
        if (p === state.phase) return;
        // 点击阶段切换：若当前正在运行先暂停
        if (state.running) pause();
        setPhase(p, { resetRound: false });
        dom.phaseTabs.querySelectorAll('.phase-tab').forEach((t) => t.classList.toggle('active', t === tab));
      });
    });

    // 开始/暂停
    dom.btnStart.addEventListener('click', toggle);

    // 重置：当前阶段计时归零，轮次也回到第 1 轮 · 本轮 1/N
    dom.btnReset.addEventListener('click', () => {
      stopTimer();
      state.remainMs = state.totalMs;
      state.roundInCycle = 1;
      state.completedFocus = 0;
      saveSettings();
      render({ instant: true });
    });

    // 跳过
    dom.btnSkip.addEventListener('click', () => {
      stopTimer();
      // 跳过当前阶段，直接完成（setPhase 内部已渲染）
      const was = state.phase;
      if (was === 'work') {
        state.completedFocus += 1;
        if (state.completedFocus % state.rounds === 0) setPhase('longBreak');
        else setPhase('break');
      } else {
        setPhase('work');
      }
    });

    // 固定悬浮：收缩为倒计时小方窗（再次点击小窗展开）
    dom.btnPin.addEventListener('click', () => {
      window.pomodoro.togglePin();
    });

    // 迷你模式：点击圆环中心展开回完整窗口（刚拖拽完不触发，避免拖动误展开）
    dom.ringCenter.addEventListener('click', () => {
      if (Date.now() - lastMiniDragEnd < 300) return;
      if (document.body.classList.contains('mini')) {
        window.pomodoro.togglePin();
      }
    });

    // 迷你模式拖拽：不用原生 drag 区（会吞掉 hover/鼠标事件），
    // 按住非按钮处超过阈值距离后，交给主进程跟随光标移动窗口
    let lastMiniDragEnd = 0;
    dom.app.addEventListener('pointerdown', (e) => {
      if (!document.body.classList.contains('mini')) return;
      if (e.button !== 0 || e.target.closest('button')) return;
      const startX = e.screenX;
      const startY = e.screenY;
      let dragging = false;
      const onMove = (ev) => {
        if (dragging) return;
        if (Math.abs(ev.screenX - startX) + Math.abs(ev.screenY - startY) > 4) {
          dragging = true;
          try { dom.app.setPointerCapture(e.pointerId); } catch (err) { /* ignore */ }
          window.pomodoro.dragStart();
        }
      };
      const onUp = () => {
        document.removeEventListener('pointermove', onMove);
        document.removeEventListener('pointerup', onUp);
        document.removeEventListener('pointercancel', onUp);
        if (dragging) {
          window.pomodoro.dragEnd();
          lastMiniDragEnd = Date.now();
        }
      };
      document.addEventListener('pointermove', onMove);
      document.addEventListener('pointerup', onUp);
      document.addEventListener('pointercancel', onUp);
    });

    // 最小化到托盘
    dom.btnMin.addEventListener('click', () => {
      window.pomodoro.minimizeToTray();
    });

    // 关闭
    dom.btnClose.addEventListener('click', () => {
      window.pomodoro.closeWindow();
    });

    // 双击标题栏打开设置
    document.querySelector('.titlebar-drag').addEventListener('dblclick', () => {
      openSettings();
    });

    // 齿轮按钮打开设置
    dom.btnSettings.addEventListener('click', () => {
      openSettings();
    });

    // 设置抽屉
    dom.drawerBackdrop.addEventListener('click', closeSettings);
    dom.drawerClose.addEventListener('click', closeSettings);
    dom.drawerSave.addEventListener('click', () => {
      const work = clampNum(dom.sWork.value, 1, 120, 25);
      const brk = clampNum(dom.sBreak.value, 1, 60, 5);
      const lng = clampNum(dom.sLong.value, 1, 120, 15);
      const rounds = clampNum(dom.sRounds.value, 1, 8, 4);
      state.durations = { work, break: brk, longBreak: lng };
      state.rounds = rounds;
      state.autoNext = dom.sAuto.checked;
      // 若未运行，则刷新当前阶段时长
      if (!state.running) {
        state.totalMs = state.durations[state.phase] * 60 * 1000;
        state.remainMs = state.totalMs;
      }
      saveSettings();
      render({ instant: true });
      closeSettings();
    });

    // 托盘命令
    window.pomodoro.onTrayCommand((cmd) => {
      if (cmd === 'toggle') toggle();
      else if (cmd === 'reset') { stopTimer(); state.remainMs = state.totalMs; render({ instant: true }); }
      else if (cmd === 'skip') {
        stopTimer();
        const was = state.phase;
        if (was === 'work') {
          state.completedFocus += 1;
          if (state.completedFocus % state.rounds === 0) setPhase('longBreak');
          else setPhase('break');
        } else {
          setPhase('work');
        }
      }
    });

    // 固定悬浮状态反馈：val=true 表示处于迷你悬浮模式
    window.pomodoro.onPinChanged((val) => {
      dom.btnPin.classList.toggle('is-active', val);
      document.body.classList.toggle('mini', val);
      dom.ringCenter.title = val ? '点击展开完整窗口' : '';
      if (val) closeSettings();
    });

    // 贴边隐藏：主进程收起/滑出后同步 body 类；
    // 鼠标移入细条 → 请求滑出；移开窗口 → 请求延时收回
    let dockState = { hidden: false, edge: null };
    window.pomodoro.onDockChanged((s) => {
      dockState = s || { hidden: false, edge: null };
      const body = document.body;
      body.classList.toggle('dock-hidden', !!dockState.hidden);
      body.classList.remove('dock-left', 'dock-right', 'dock-top', 'dock-bottom');
      if (dockState.hidden && dockState.edge) {
        body.classList.add(`dock-${dockState.edge}`);
      }
    });
    const root = document.documentElement;
    root.addEventListener('pointerenter', () => {
      if (dockState.hidden) window.pomodoro.dockReveal();
    });
    root.addEventListener('pointerleave', () => {
      if (!dockState.hidden && dockState.edge) window.pomodoro.dockHide();
    });

    // Agent 网关：状态回推 + 开关 + 复制 hook 配置
    window.pomodoro.onGatewayState((gs) => {
      gatewayState = gs || {};
      dom.sGateway.checked = !!gatewayState.enabled;
      dom.agentMeta.textContent = gatewayState.enabled
        ? `端口 ${gatewayState.port} · 运行中`
        : '已停用';
      if (gatewayState.activity) {
        agentActivity = gatewayState.activity;
        updateAgentActivityUI();
      }
    });
    dom.sGateway.addEventListener('change', () => {
      window.pomodoro.setGatewayEnabled(dom.sGateway.checked);
    });
    const flash = (btn, text) => {
      const old = btn.dataset.label || btn.textContent;
      btn.dataset.label = old;
      btn.textContent = text;
      setTimeout(() => { btn.textContent = btn.dataset.label; }, 1500);
    };

    const currentAgent = () => (dom.agentSelect ? dom.agentSelect.value : 'claude');

    dom.btnCopyHook.addEventListener('click', () => {
      if (!gatewayState.hookPath) return;
      const agent = currentAgent();
      window.pomodoro.copyText(buildHookSnippet(agent, gatewayState.hookPath, gatewayState.pluginPath));
      flash(dom.btnCopyHook, '已复制 ✓');
    });

    dom.btnCopyInstall.addEventListener('click', () => {
      if (!gatewayState.hookPath) return;
      window.pomodoro.copyText(buildInstallCommand(currentAgent(), gatewayState.hookPath));
      flash(dom.btnCopyInstall, '已复制 ✓');
    });
    window.pomodoro.onAgentActivity((a) => {
      if (a) {
        agentActivity = a;
        updateAgentActivityUI();
      }
    });
    window.pomodoro.requestGatewayState();

    // 快捷键：空格 开始/暂停，R 重置（输入框聚焦或按键重复时不触发）
    window.addEventListener('keydown', (e) => {
      if (e.repeat) return;
      const tag = document.activeElement && document.activeElement.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA') return;
      if (e.code === 'Space') { e.preventDefault(); toggle(); }
      if (e.code === 'KeyR') { stopTimer(); state.remainMs = state.totalMs; render({ instant: true }); }
    });
  }

  function clampNum(v, min, max, def) {
    const n = parseInt(v, 10);
    if (isNaN(n)) return def;
    return Math.min(max, Math.max(min, n));
  }

  function openSettings() {
    dom.sWork.value = state.durations.work;
    dom.sBreak.value = state.durations.break;
    dom.sLong.value = state.durations.longBreak;
    dom.sRounds.value = state.rounds;
    dom.sAuto.checked = state.autoNext;
    dom.settingsDrawer.classList.add('open');
  }
  function closeSettings() {
    dom.settingsDrawer.classList.remove('open');
  }

  // ---- 初始化 ----
  function init() {
    loadSettings();
    bindEvents();
    render();
    // 若有暂停中的计时，恢复
    const saved = localStorage.getItem(STORE_KEY);
    if (saved) {
      try {
        const s = JSON.parse(saved);
        if (s.running) start();
      } catch (e) { /* ignore */ }
    }
  }

  return { init };
})();

// 启动
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', PomodoroApp.init);
} else {
  PomodoroApp.init();
}
