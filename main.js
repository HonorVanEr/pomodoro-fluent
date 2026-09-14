'use strict';

const { app, BrowserWindow, Tray, Menu, nativeImage, ipcMain, screen, shell, Notification, clipboard } = require('electron');
const path = require('path');
const fs = require('fs');
const os = require('os');
const crypto = require('crypto');
const { execFile } = require('child_process');
const { createGateway } = require('./gateway');

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------
let mainWindow = null;      // 主窗口
let notifyWindow = null;    // 通知弹窗
let tray = null;            // 系统托盘
let trayIconBase = null;    // 基础托盘图标（nativeImage，用于变体生成）
let isQuitting = false;     // 是否真正退出（否则关窗=隐藏到托盘）
let isMiniMode = false;     // 迷你悬浮模式（仅剩倒计时的小窗）
let lastFullBounds = null;  // 进入迷你模式前的完整窗口尺寸/位置
let miniDock = null;        // 贴边吸附的边：left | right | top | bottom（null=不贴边）
let miniDockHidden = false; // 贴边收起状态（只露一条细进度条）
let dockHideTimer = null;
let miniDockWA = null;      // 吸附瞬间所在屏的工作区快照（滑出/收回几何以此为准，多屏稳定）
let miniDragWA = null;      // 拖拽期间黏滞的基准屏工作区（防止接缝处基准逐帧翻转）

// 窗口尺寸约定
const FULL_SIZE = { width: 360, height: 560 }; // 完整模式默认尺寸
const FULL_MIN = { width: 300, height: 460 };  // 完整模式最小尺寸
const MINI_W = 176;                             // 迷你模式宽
const MINI_H = 64;                              // 迷你模式高（静置只放时间+文字，悬停文字换成按钮）
const DOCK_TAB = 6;                             // 贴边隐藏时可见细条厚度（由 CSS 绘制）
const DOCK_LEN = 76;                            // 贴边隐藏时细条的长边（px）
// Windows 会把窗口最小尺寸钳到约 32×39（setMinimumSize(0,0) 也绕不开）。
// 隐藏态窗口按下面尺寸开（不小于下限），多出的部分是透明留白：
// CSS 只在靠屏幕边缘的 6px 里画进度条，透明区域鼠标穿透，视觉上就是 6px 细条。
const DOCK_PAD_W = 34;
const DOCK_PAD_H = 40;
const DOCK_SNAP_DIST = 64;                      // 拖拽松手时距屏幕边缘多近触发吸附
const DOCK_SNAP_INSET = 8;                      // 越过屏幕边缘多深则不再吸附（双屏接缝场景）

const ICON_PATH = path.join(__dirname, 'assets', 'tray.png');    // 托盘染色模板
const APP_ICON_PATH = path.join(__dirname, 'assets', 'icon.png'); // 窗口/任务栏图标

// ---------------------------------------------------------------------------
// 单实例锁：避免重复启动
// ---------------------------------------------------------------------------
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on('second-instance', () => {
    revealMainWindow();
  });
}

// ---------------------------------------------------------------------------
// 工具：生成不同颜色的托盘图标（按阶段变色）
// ---------------------------------------------------------------------------
function makeTrayIcon(r, g, b) {
  if (!trayIconBase) {
    try {
      trayIconBase = nativeImage.createFromPath(ICON_PATH);
    } catch (e) {
      return null;
    }
  }
  if (trayIconBase.isEmpty()) return null;
  // 对图标按颜色做色调映射：先取原始 PNG 的像素做着色
  // 简单方案：用一张单色模板图（白色），再染色
  try {
    const raw = trayIconBase.toBitmap();
    const w = trayIconBase.getSize().width;
    const h = trayIconBase.getSize().height;
    // 对 RGBA 像素：将纯白像素替换为目标色，保留 alpha
    const tinted = Buffer.from(raw);
    for (let i = 0; i < tinted.length; i += 4) {
      const a = tinted[i + 3];
      if (a > 0) {
        // 亮度作为染色比例
        const lum = (tinted[i] + tinted[i + 1] + tinted[i + 2]) / 3 / 255;
        tinted[i] = Math.round(r * lum + tinted[i] * (1 - lum));
        tinted[i + 1] = Math.round(g * lum + tinted[i + 1] * (1 - lum));
        tinted[i + 2] = Math.round(b * lum + tinted[i + 2] * (1 - lum));
      }
    }
    return nativeImage.createFromBitmap(tinted, { width: w, height: h });
  } catch (e) {
    return trayIconBase;
  }
}

// 各阶段对应的托盘图标颜色
const TRAY_PHASE_COLORS = {
  work: [255, 107, 129],     // 珊瑚粉
  break: [56, 189, 178],     // 青绿
  longBreak: [96, 165, 250], // 蓝
  idle: [203, 213, 225],     // 灰
};
const trayIconCache = new Map(); // 阶段 -> 已染色的 nativeImage（染色有成本，缓存复用）

function trayIconForPhase(phase) {
  if (!trayIconCache.has(phase)) {
    const [r, g, b] = TRAY_PHASE_COLORS[phase] || TRAY_PHASE_COLORS.idle;
    trayIconCache.set(phase, makeTrayIcon(r, g, b) || nativeImage.createFromPath(ICON_PATH));
  }
  return trayIconCache.get(phase);
}

// ---------------------------------------------------------------------------
// 应用 Acrylic 毛玻璃（Win32 DWM API，Win10 1803+ / Win11 均支持）
// 通过 PowerShell 调用 SetWindowCompositionAttribute 设置 ACCENT_ENABLE_ACRYLICBLURBEHIND
// 失败时静默降级（不影响窗口正常显示）
// ---------------------------------------------------------------------------
const ACRYLIC_SCRIPT = path.join(__dirname, 'apply-acrylic.ps1');
// Win10 的 DWM Acrylic 不跟随窗口圆角裁剪（SetWindowRgn 只裁窗口内容，
// 毛玻璃仍画满整个矩形，CSS 圆角外会露出方角）。因此 Win10 上改用
// 近不透明圆角卡片；以后跑在 Win11（系统默认给无边框窗口加圆角）时
// 可改回 true 恢复毛玻璃效果。
const ENABLE_ACRYLIC = false;
// 实际传给 PowerShell 执行的脚本路径：
//  - 开发模式：直接使用源码目录里的 apply-acrylic.ps1
//  - 打包模式（asar 内）：解包到 userData 可写目录（asar 是只读的，PowerShell 也无法直接执行 asar 内路径）
let acrylicRuntimeScript = null;
let acrylicScriptReady = false;
let acrylicCheckPromise = null;

// 异步确认脚本存在、可执行且为 UTF-8 BOM（避免 PowerShell 解析 here-string 出错）
function ensureAcrylicScript() {
  if (!acrylicCheckPromise) {
    acrylicCheckPromise = (async () => {
      try {
        const srcBuf = fs.readFileSync(ACRYLIC_SCRIPT);
        let targetPath = ACRYLIC_SCRIPT;
        // 打包后 __dirname 在 app.asar 内：解包到 userData
        if (__dirname.includes('app.asar') || !fs.existsSync(path.dirname(targetPath))) {
          const userData = app.getPath('userData');
          try { fs.mkdirSync(userData, { recursive: true }); } catch (e) { /* ignore */ }
          targetPath = path.join(userData, 'apply-acrylic.ps1');
        }
        // 若无 BOM，补上（PowerShell 5.1 需要 BOM 正确解析 UTF-8）
        let buf = srcBuf;
        if (!(buf[0] === 0xEF && buf[1] === 0xBB && buf[2] === 0xBF)) {
          buf = Buffer.concat([Buffer.from([0xEF, 0xBB, 0xBF]), srcBuf]);
        }
        // 内容不同或目标不存在才写，避免每次都写盘
        const needWrite = !fs.existsSync(targetPath) ||
          !buf.equals(fs.readFileSync(targetPath));
        if (needWrite) fs.writeFileSync(targetPath, buf);
        acrylicRuntimeScript = targetPath;
        return true;
      } catch (e) {
        return false;
      }
    })();
  }
  return acrylicCheckPromise;
}

function applyAcrylicToWindow(win, tint, tintOpacity, cornerRadius) {
  if (!ENABLE_ACRYLIC) return;
  if (!win || win.isDestroyed()) return;
  ensureAcrylicScript().then((ok) => {
    if (!ok) return;
    try {
      const hwndBuf = win.getNativeWindowHandle();
      if (!hwndBuf || hwndBuf.length < 8) return;
      const hwnd = hwndBuf.readBigUInt64LE(0).toString();
      const args = [
        '-NoProfile', '-ExecutionPolicy', 'Bypass',
        '-File', acrylicRuntimeScript,
        '-Hwnd', hwnd,
      ];
      if (tint) args.push('-Tint', tint);
      if (typeof tintOpacity === 'number') args.push('-TintOpacity', String(tintOpacity));
      if (typeof cornerRadius === 'number' && cornerRadius > 0) args.push('-CornerRadius', String(cornerRadius));
      execFile('powershell', args, { timeout: 15000, windowsHide: true }, () => {
        // 结果不阻塞，静默处理
      });
    } catch (e) {
      // 忽略：无毛玻璃也能正常使用
    }
  });
}

// ---------------------------------------------------------------------------
// 创建主窗口（透明无边框，Win11 Fluent 风格）
// ---------------------------------------------------------------------------
function createMainWindow() {
  const win = new BrowserWindow({
    width: FULL_SIZE.width,
    height: FULL_SIZE.height,
    minWidth: FULL_MIN.width,
    minHeight: FULL_MIN.height,
    frame: false,
    transparent: true,
    backgroundColor: '#00000000',
    resizable: true,
    show: false,
    skipTaskbar: false,
    icon: APP_ICON_PATH,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
      // 隐藏到托盘后仍保持秒级 tick，托盘倒计时/菜单继续更新
      backgroundThrottling: false,
    },
  });

  // 默认在屏幕右下角附近出现
  const { workArea } = screen.getPrimaryDisplay();
  const x = workArea.x + workArea.width - win.getBounds().width - 24;
  const y = workArea.y + workArea.height - win.getBounds().height - 24;
  win.setPosition(x, y);

  win.loadFile(path.join(__dirname, 'renderer', 'index.html'));

  win.once('ready-to-show', () => {
    win.show();
    // 应用 Acrylic 毛玻璃 + 圆角裁剪（延迟一点等窗口完全显示）
    setTimeout(() => applyAcrylicToWindow(win, '30,32,42', 0.42, 12), 150);
  });

  // 窗口尺寸变化后重设圆角裁剪区域（SetWindowRgn 不随窗口缩放自动更新）
  let cornerDebounce = null;
  win.on('resize', () => {
    if (cornerDebounce) clearTimeout(cornerDebounce);
    cornerDebounce = setTimeout(() => {
      if (!win.isDestroyed()) applyAcrylicToWindow(win, '30,32,42', 0.42, 12);
    }, 250);
  });

  // 关闭行为：隐藏到托盘（除非真的退出）
  win.on('close', (e) => {
    if (!isQuitting) {
      e.preventDefault();
      win.hide();
    }
  });

  // 拖拽由渲染层 -webkit-app-region: drag 处理；这里补充窗口移动
  mainWindow = win;

  return win;
}

// ---------------------------------------------------------------------------
// 创建通知 / 交互弹窗（到时间弹出、agent 提问、权限审批）
//
// 三种形态（payload.kind）：
//   notification  纯通知，几秒后自动消失
//   ask           提问：可在弹窗内选选项 / 填自定义回答
//   permission    权限：允许 / 始终允许 / 拒绝
//   custom        自定义按钮（旧 confirm 路径、休息建议）
//
// 完整 payload 存进 popupPayloads，由弹窗页按 id 通过 IPC 取回
// （不再塞 URL query——提问的工具输入可能很长）。尺寸由弹窗页实测后
// 通过 notify:resize 回传，主进程按右上角锚定重设，避免内容被裁切。
// ---------------------------------------------------------------------------
const INTERACTIVE_KINDS = new Set(['ask', 'permission', 'custom']);
const popupPayloads = new Map(); // id -> payload
const NOTIFY_MIN = { w: 320, h: 130 };
const NOTIFY_MAX = { w: 560, h: 660 };

function popupSizeFor(kind) {
  if (kind === 'ask') return { width: 460, height: 260 };
  if (kind === 'permission') return { width: 432, height: 240 };
  return { width: 400, height: 176 };
}

function clampNum(n, lo, hi) {
  const v = Number(n);
  if (!Number.isFinite(v)) return lo;
  return Math.max(lo, Math.min(hi, Math.round(v)));
}

function placeTopRight(win) {
  const b = win.getBounds();
  const { workArea } = screen.getDisplayMatching(b);
  win.setPosition(workArea.x + workArea.width - b.width - 16, workArea.y + 16);
}

function showNotify(payload) {
  const data = payload || {};
  const id = data.id || crypto.randomUUID();
  const kind = INTERACTIVE_KINDS.has(data.kind) ? data.kind : 'notification';
  // ask / permission / custom 一律等待用户在弹窗内决策（网关侧同样只在
  // 这三类上做长轮询），notification 才是看完即走
  const interactive = INTERACTIVE_KINDS.has(kind);

  // 如果已经有通知窗口，先关掉旧的
  if (notifyWindow && !notifyWindow.isDestroyed()) {
    // 旧窗如果是在等用户决策的交互窗（权限/提问），先按 dismissed 收尾：
    // 否则窗口被顶掉后，hook 那边的长轮询只能干等到超时才拿到兜底值
    let prev = null;
    for (const p of popupPayloads.values()) {
      if (p && p.interactive) { prev = p; break; }
    }
    if (prev && gateway) {
      gateway.resolveInteraction(prev.id, { action: null, answers: {}, text: '' }, 'dismissed');
    }
    notifyWindow.close();
    notifyWindow = null;
  }

  const size = popupSizeFor(kind);
  const nw = new BrowserWindow({
    width: size.width,
    height: size.height,
    frame: false,
    transparent: true,
    backgroundColor: '#00000000',
    resizable: false,
    alwaysOnTop: true,
    skipTaskbar: true,
    show: false,
    hasShadow: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
    },
  });

  notifyWindow = nw;
  // 视觉风味：ask / permission 用自身 kind；其余沿用旧 type（work / break / agent…）
  const flavor = data.flavor || (kind === 'ask' || kind === 'permission' ? kind : (data.type || 'agent'));
  popupPayloads.set(id, { ...data, id, kind, interactive, flavor });

  placeTopRight(nw);
  nw.loadFile(path.join(__dirname, 'renderer', 'notify.html'), { query: { id } });

  // 演示/排障模式：把弹窗页的 console 与加载失败打到主进程日志
  if (process.env.POMODORO_POPUP_DEMO) {
    nw.webContents.on('console-message', (_e, level, message, line, sourceId) => {
      console.log(`[popup:${level}] ${message} (${sourceId}:${line})`);
    });
    nw.webContents.on('did-fail-load', (_e, code, desc) => {
      console.error('[popup] did-fail-load', code, desc);
    });
  }

  // 等弹窗页实测高度后 resize 再显示；兜底：600ms 内没上报就直接显示
  const showFallback = setTimeout(() => {
    if (!nw.isDestroyed() && !nw.isVisible()) {
      placeTopRight(nw);
      nw.show();
    }
  }, 600);

  nw.once('ready-to-show', () => {
    setTimeout(() => applyAcrylicToWindow(nw, '28,30,40', 0.5, 14), 120);
  });

  nw.on('closed', () => {
    clearTimeout(showFallback);
    popupPayloads.delete(id);
    notifyWindow = null;
  });
}

// 弹窗页取回自己的完整 payload
ipcMain.handle('notify:payload', (_e, id) => popupPayloads.get(id) || null);

// 弹窗页实测内容高度后回传，主进程重设尺寸并靠右上角显示
ipcMain.on('notify:resize', (e, size) => {
  const win = BrowserWindow.fromWebContents(e.sender);
  if (!win || win.isDestroyed()) return;
  const s = size || {};
  const { workArea } = screen.getDisplayMatching(win.getBounds());
  const w = clampNum(s.width, NOTIFY_MIN.w, Math.min(NOTIFY_MAX.w, workArea.width - 24));
  const h = clampNum(s.height, NOTIFY_MIN.h, Math.min(NOTIFY_MAX.h, workArea.height - 32));
  win.setSize(w, h);
  placeTopRight(win);
  if (process.env.POMODORO_POPUP_DEMO) console.log(`[popup] resize → ${w}x${h}`);
  if (!win.isVisible()) win.show();
});

// ---------------------------------------------------------------------------
// 系统托盘
// ---------------------------------------------------------------------------
function createTray() {
  let icon = trayIconForPhase('work');
  if (!icon || icon.isEmpty()) icon = nativeImage.createFromPath(ICON_PATH);
  tray = new Tray(icon);
  tray.setToolTip('番茄钟');
  rebuildTrayMenu();

  // 双击托盘显示主窗口
  tray.on('double-click', () => {
    toggleMainWindow();
  });
}

function rebuildTrayMenu(state) {
  const s = state || {};
  const running = !!s.running;
  const menu = Menu.buildFromTemplate([
    {
      label: '显示主窗口',
      click: () => revealMainWindow(),
    },
    { type: 'separator' },
    {
      label: running ? '暂停' : '开始',
      click: () => sendRendererCommand('toggle'),
    },
    {
      label: '重置',
      click: () => sendRendererCommand('reset'),
    },
    {
      label: '跳到下一阶段',
      click: () => sendRendererCommand('skip'),
    },
    { type: 'separator' },
    {
      label: '退出',
      click: () => {
        isQuitting = true;
        app.quit();
      },
    },
  ]);
  if (tray) tray.setContextMenu(menu);
}

// 从托盘唤回主窗口：迷你/贴边状态下窗口本来就"可见"（细条），
// show() 无效果——需先退出迷你模式展开成完整窗口
function revealMainWindow() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  if (isMiniMode) {
    setPinMini(mainWindow, false);
    emitMiniState(); // 关键：解除渲染层的 mini/贴边类，否则展开的是透明窗口
  }
  if (!mainWindow.isVisible()) mainWindow.show();
  // Windows 前台锁可能拒绝后台进程的 SetForegroundWindow：
  // 先把窗口顶到 topmost 层强制盖过其他应用，片刻后再放回来
  mainWindow.setAlwaysOnTop(true, 'screen-saver');
  mainWindow.moveTop();
  mainWindow.focus();
  setTimeout(() => {
    if (mainWindow && !mainWindow.isDestroyed() && !isMiniMode) {
      mainWindow.setAlwaysOnTop(false, 'screen-saver');
    }
    if (mainWindow && !mainWindow.isDestroyed() && !mainWindow.isFocused()) {
      mainWindow.flashFrame(true); // 实在抢不到前台就闪任务栏提示
    }
  }, 300);
}

function toggleMainWindow() {
  if (!mainWindow) return;
  if (isMiniMode) {
    // 迷你小窗不该被双击托盘"藏起来"，直接展开主窗口
    revealMainWindow();
  } else if (mainWindow.isVisible()) {
    mainWindow.hide();
  } else {
    mainWindow.show();
    mainWindow.focus();
  }
}

// 把定时器命令转发给渲染进程（托盘菜单 / Agent 网关 / 弹窗按钮共用）
function sendRendererCommand(cmd) {
  if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send('tray:command', cmd);
}

// ---------------------------------------------------------------------------
// IPC：渲染进程 → 主进程
// ---------------------------------------------------------------------------
ipcMain.on('window:minimize-to-tray', () => {
  if (mainWindow) mainWindow.hide();
});

ipcMain.on('window:close', () => {
  if (mainWindow) mainWindow.close();
});

// ---------------------------------------------------------------------------
// 迷你悬浮模式：置顶 + 收缩成只剩倒计时圆环的圆角方块（类似输入法悬浮窗）
// 进入：以原窗口右上角为锚点收缩（小窗出现在标题栏图钉下方）
// 退出：以小窗右上角为锚点展开回完整尺寸，并限制在工作区内
// ---------------------------------------------------------------------------
function setPinMini(win, on) {
  if (isMiniMode === on) return;
  isMiniMode = on;
  stopMiniDrag();
  // 进出迷你模式都重置贴边状态
  if (dockHideTimer) { clearTimeout(dockHideTimer); dockHideTimer = null; }
  miniDock = null;
  miniDockHidden = false;
  miniDockWA = null;
  if (on) {
    lastFullBounds = win.getBounds();
    const b = lastFullBounds;
    win.setMinimumSize(0, 0);
    win.setResizable(false);
    win.setBounds({ x: b.x + b.width - MINI_W, y: b.y, width: MINI_W, height: MINI_H });
    win.setAlwaysOnTop(true, 'floating');
    // 迷你悬浮视同隐藏窗口：撤下任务栏按钮，只保留小窗与托盘
    win.setSkipTaskbar(true);
  } else {
    const cur = win.getBounds();
    const full = lastFullBounds || { width: FULL_SIZE.width, height: FULL_SIZE.height };
    const { workArea } = screen.getDisplayMatching(cur);
    let x = cur.x + cur.width - full.width;
    let y = cur.y;
    x = Math.max(workArea.x, Math.min(x, workArea.x + workArea.width - full.width));
    y = Math.max(workArea.y, Math.min(y, workArea.y + workArea.height - full.height));
    win.setBounds({ x, y, width: full.width, height: full.height });
    win.setMinimumSize(FULL_MIN.width, FULL_MIN.height);
    win.setResizable(true);
    win.setAlwaysOnTop(false, 'floating');
    win.setSkipTaskbar(false);
  }
}

// 迷你/贴边状态变化后必须同步给渲染层，
// 否则页面残留 dock-hidden 类会把完整窗口画成透明卡片+一条细条
function emitMiniState() {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send('state:pin-changed', isMiniMode);
    sendDockState();
  }
}

ipcMain.on('window:toggle-pin', () => {
  if (!mainWindow) return;
  setPinMini(mainWindow, !isMiniMode);
  emitMiniState();
});

// ---------------------------------------------------------------------------
// 迷你模式手动拖拽：原生 -webkit-app-region: drag 会吞掉 :hover 和鼠标事件
// （悬停浮出按钮失效），所以迷你模式放弃原生拖拽区，改为渲染层 pointerdown
// 后由这里轮询光标位置、让窗口跟随光标移动
// ---------------------------------------------------------------------------
let miniDragTimer = null;
let miniDragOffset = null;

function stopMiniDrag() {
  if (miniDragTimer) { clearInterval(miniDragTimer); miniDragTimer = null; }
  miniDragOffset = null;
  miniDragWA = null;
}

ipcMain.on('window:drag-start', () => {
  if (!mainWindow || mainWindow.isDestroyed() || !isMiniMode) return;
  stopMiniDrag();
  const cursor = screen.getCursorScreenPoint();
  const [wx, wy] = mainWindow.getPosition();
  const b = mainWindow.getBounds();
  // 基准屏黏滞：起拖时按窗口所在屏定基准。之后只有光标深入另一屏 >40px 才
  // 切换基准，避免接缝处每帧翻转限位屏幕导致窗口突然跳到另一块屏
  miniDragWA = screen.getDisplayMatching(b).workArea;
  miniDragOffset = { dx: cursor.x - wx, dy: cursor.y - wy, w: b.width, h: b.height };
  miniDragTimer = setInterval(() => {
    if (!mainWindow || mainWindow.isDestroyed()) return stopMiniDrag();
    const o = miniDragOffset;
    const c = screen.getCursorScreenPoint();
    let wa = miniDragWA;
    const cur = screen.getDisplayMatching({ x: c.x, y: c.y, width: 1, height: 1 });
    if (cur.id !== wa.id) {
      const beyond = c.x < wa.x - 40 || c.x > wa.x + wa.width + 40
        || c.y < wa.y - 40 || c.y > wa.y + wa.height + 40;
      if (beyond) { wa = cur.workArea; miniDragWA = wa; }
    }
    let nx = Math.round(c.x - o.dx);
    let ny = Math.round(c.y - o.dy);
    nx = Math.max(wa.x, Math.min(nx, wa.x + wa.width - o.w));
    ny = Math.max(wa.y, Math.min(ny, wa.y + wa.height - o.h));
    mainWindow.setPosition(nx, ny);
  }, 16);
});

// ---------------------------------------------------------------------------
// 贴边隐藏：拖拽松手时靠近屏幕边缘 → 吸附并收起成细进度条；
// 鼠标移入细条 → 滑出完整小窗；鼠标移开 → 延时收回
// ---------------------------------------------------------------------------
function sendDockState() {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send('state:dock-changed', { hidden: miniDockHidden, edge: miniDock });
  }
}

// 按当前贴边状态摆放窗口（收起=细条，展开=迷你小窗，两种都沿边对齐、保留贴边位置）
function applyMiniDockGeometry() {
  if (!mainWindow || mainWindow.isDestroyed() || !isMiniMode || !miniDock) return;
  const b = mainWindow.getBounds();
  // 优先用吸附瞬间快照的工作区；快照失效（拔掉副屏等）才按窗口当前位置重新匹配
  const wa = miniDockWA || screen.getDisplayMatching(b).workArea;
  // 沿边方向的尺寸：收起态用短细条，展开态用完整小窗（各自限位于工作区内）
  const y = Math.max(wa.y, Math.min(b.y, wa.y + wa.height - (miniDockHidden ? DOCK_LEN : MINI_H)));
  const x = Math.max(wa.x, Math.min(b.x, wa.x + wa.width - (miniDockHidden ? DOCK_LEN : MINI_W)));
  const flush = {
    left:   { x: wa.x, y, width: MINI_W, height: MINI_H },
    right:  { x: wa.x + wa.width - MINI_W, y, width: MINI_W, height: MINI_H },
    top:    { x, y: wa.y, width: MINI_W, height: MINI_H },
    bottom: { x, y: wa.y + wa.height - MINI_H, width: MINI_W, height: MINI_H },
  }[miniDock];
  const tab = {
    left:   { x: wa.x, y, width: DOCK_PAD_W, height: DOCK_LEN },
    right:  { x: wa.x + wa.width - DOCK_PAD_W, y, width: DOCK_PAD_W, height: DOCK_LEN },
    top:    { x, y: wa.y, width: DOCK_LEN, height: DOCK_PAD_H },
    bottom: { x, y: wa.y + wa.height - DOCK_PAD_H, width: DOCK_LEN, height: DOCK_PAD_H },
  }[miniDock];
  mainWindow.setBounds(miniDockHidden ? tab : flush);
  sendDockState();
}

ipcMain.on('window:drag-end', () => {
  // stopMiniDrag 会清 miniDragWA，先取出本次拖拽的基准屏
  const wa = miniDragWA || (() => {
    const bb = mainWindow && !mainWindow.isDestroyed()
      ? mainWindow.getBounds() : { x: 0, y: 0, width: 1, height: 1 };
    return screen.getDisplayMatching(bb).workArea;
  })();
  stopMiniDrag();
  if (!mainWindow || mainWindow.isDestroyed() || !isMiniMode) return;
  const b = mainWindow.getBounds();
  // 窗口在整个拖拽期间都被限位在基准屏内，直接对该屏判边缘距离最稳。
  // 越过边缘超过 DOCK_SNAP_INSET 不算贴边（双屏接缝处避免吸到另一块屏上）
  const inRange = (d) => d >= -DOCK_SNAP_INSET && d <= DOCK_SNAP_DIST;
  let edge = null;
  if (inRange(wa.x - b.x)) edge = 'left';
  else if (inRange(b.x + b.width - (wa.x + wa.width))) edge = 'right';
  else if (inRange(wa.y - b.y)) edge = 'top';
  else if (inRange(b.y + b.height - (wa.y + wa.height))) edge = 'bottom';
  if (dockHideTimer) { clearTimeout(dockHideTimer); dockHideTimer = null; }
  if (edge) {
    miniDock = edge;
    miniDockHidden = true;
    miniDockWA = wa; // 快照：后续滑出/收回几何不再重新匹配屏幕
    applyMiniDockGeometry();
  } else {
    // 离边缘较远：取消吸附，恢复普通迷你小窗（从细条拖离边缘时也要恢复尺寸）。
    // 细条比小窗窄/短，恢复尺寸后重新限位于基准屏内，避免越出屏幕
    miniDock = null;
    miniDockHidden = false;
    miniDockWA = null;
    const x = Math.max(wa.x, Math.min(b.x, wa.x + wa.width - MINI_W));
    const y = Math.max(wa.y, Math.min(b.y, wa.y + wa.height - MINI_H));
    mainWindow.setBounds({ x, y, width: MINI_W, height: MINI_H });
    sendDockState();
  }
});

ipcMain.on('mini:dock-reveal', () => {
  if (!isMiniMode || !miniDock || !miniDockHidden) return;
  if (!mainWindow || mainWindow.isDestroyed()) return;
  // 细条只有几个像素，渲染层的 pointerenter 可能是误报：
  // 校验光标确实落在细条附近才滑出，否则忽略
  const b = mainWindow.getBounds();
  const c = screen.getCursorScreenPoint();
  const pad = 8; // DPI 取整与边缘余量
  if (c.x < b.x - pad || c.x > b.x + b.width + pad
    || c.y < b.y - pad || c.y > b.y + b.height + pad) return;
  if (dockHideTimer) { clearTimeout(dockHideTimer); dockHideTimer = null; }
  miniDockHidden = false;
  applyMiniDockGeometry();
});

// 延时收回。收回前校验光标真实位置：窗口展开/收起瞬间 DOM 的 pointerleave
// 可能误报（命中测试变化），若光标实际仍在窗口上则延后重试，
// 避免"弹出又立刻收回"的抖动，也避免收回后光标恰在细条上引发的循环。
function scheduleDockHide(delay) {
  if (dockHideTimer) clearTimeout(dockHideTimer);
  dockHideTimer = setTimeout(() => {
    dockHideTimer = null;
    if (!isMiniMode || !miniDock || miniDockHidden) return;
    if (!mainWindow || mainWindow.isDestroyed()) return;
    const b = mainWindow.getBounds();
    const c = screen.getCursorScreenPoint();
    const pad = 2; // DPI 取整余量
    if (c.x >= b.x - pad && c.x <= b.x + b.width + pad
      && c.y >= b.y - pad && c.y <= b.y + b.height + pad) {
      scheduleDockHide(300);
      return;
    }
    miniDockHidden = true;
    applyMiniDockGeometry();
  }, delay);
}

ipcMain.on('mini:dock-hide-request', () => {
  if (!isMiniMode || !miniDock || miniDockHidden) return;
  scheduleDockHide(350);
});

ipcMain.on('notify:show', (_e, payload) => {
  showNotify(payload);
});

// 托盘当前已同步的状态（用于去重，避免重复的原生调用）
let traySync = { phase: null, running: null, timeText: null };

// 渲染进程上报的完整定时器状态快照（agent 网关 /api/status 使用）
let lastTimerState = {
  phase: 'work', running: false, remainMs: 0, totalMs: 0,
  completedFocus: 0, roundInCycle: 1, rounds: 4,
};

ipcMain.on('tray:update', (_e, state) => {
  const s = state || {};
  // 缓存完整状态（托盘未就绪时也要缓存，供网关查询）
  if (s.phase) {
    lastTimerState = {
      ...lastTimerState,
      phase: s.phase,
      running: !!s.running,
      remainMs: typeof s.remainMs === 'number' ? s.remainMs : lastTimerState.remainMs,
      totalMs: typeof s.totalMs === 'number' ? s.totalMs : lastTimerState.totalMs,
      completedFocus: typeof s.completedFocus === 'number' ? s.completedFocus : lastTimerState.completedFocus,
      roundInCycle: typeof s.roundInCycle === 'number' ? s.roundInCycle : lastTimerState.roundInCycle,
      rounds: typeof s.rounds === 'number' ? s.rounds : lastTimerState.rounds,
    };
    if (gateway) gateway.observeTimerState(lastTimerState);
  }
  if (!tray) return;
  const phase = s.phase || 'idle';
  const running = !!s.running;
  const timeText = s.timeLeftText || '';

  // 图标颜色：仅阶段变化时更新（图标已按颜色缓存）
  if (phase !== traySync.phase) {
    const icon = trayIconForPhase(phase);
    if (icon && !icon.isEmpty()) tray.setImage(icon);
    traySync.phase = phase;
  }

  // tooltip：仅时间文本变化时更新
  if (timeText !== traySync.timeText) {
    tray.setToolTip(`番茄钟 ${timeText}`.trim());
    traySync.timeText = timeText;
  }

  // 托盘菜单：仅运行状态变化时重建（Menu.buildFromTemplate 开销较大）
  if (running !== traySync.running) {
    rebuildTrayMenu({ running });
    traySync.running = running;
  }
});

ipcMain.on('notify:close', (e, id) => {
  // 用户手动关闭弹窗：只把该弹窗对应的交互按 dismissed 兜底返回
  // （action=null → 调用方回退终端原生询问，不替用户做决定）
  if (gateway) {
    if (id) gateway.resolveInteraction(id, { action: null, answers: {}, text: '' }, 'dismissed');
    else gateway.dismissPending('dismissed');
  }
  // 按请求来源窗口精确关闭：被顶掉的旧弹窗的自动关闭定时器晚触发时，
  // 不能误关当前正在展示的新弹窗
  const win = BrowserWindow.fromWebContents(e.sender);
  if (win && !win.isDestroyed()) win.close();
});

// ---------------------------------------------------------------------------
// Agent 网关：本地 HTTP 服务（Claude Code / OpenCode hooks 对接）
// 开关持久化在 userData/config.json，主进程启动即读（不依赖渲染进程）
// ---------------------------------------------------------------------------
let gateway = null;

function configPath() {
  return path.join(app.getPath('userData'), 'config.json');
}
function loadConfig() {
  try { return JSON.parse(fs.readFileSync(configPath(), 'utf8')); } catch (e) { return {}; }
}
function saveConfig(patch) {
  const merged = { ...loadConfig(), ...patch };
  try {
    fs.mkdirSync(app.getPath('userData'), { recursive: true });
    fs.writeFileSync(configPath(), JSON.stringify(merged, null, 2));
  } catch (e) { /* ignore */ }
  return merged;
}

// hook CLI 安装到固定路径：userData/hook/pomodoro-hook.js，
// hook 配置引用该路径即可与仓库/安装位置解耦
function hookScriptPath() {
  return path.join(app.getPath('userData'), 'hook', 'pomodoro-hook.js');
}
function opencodePluginPath() {
  return path.join(app.getPath('userData'), 'hook', 'opencode', 'pomodoro-opencode.ts');
}
// 复制单个文件：内容一致就跳过（避免每次启动都写盘）
function copyOnce(src, dst) {
  const buf = fs.readFileSync(src);
  fs.mkdirSync(path.dirname(dst), { recursive: true });
  if (!fs.existsSync(dst) || !buf.equals(fs.readFileSync(dst))) {
    fs.writeFileSync(dst, buf);
  }
  return dst;
}
function installHookScript() {
  try {
    const dst = copyOnce(path.join(__dirname, 'bin', 'pomodoro-hook.js'), hookScriptPath());
    // OpenCode 插件一起带上：CLI 的 install --agent opencode 会用到
    try {
      copyOnce(path.join(__dirname, 'bin', 'opencode', 'pomodoro-opencode.ts'), opencodePluginPath());
    } catch (e) { /* 插件缺失不影响主流程 */ }
    return dst;
  } catch (e) {
    console.error('[gateway] 安装 hook 脚本失败:', e.message);
    return null;
  }
}

// ---------------------------------------------------------------------------
// 一键安装 hook（设置面板点按钮就写配置，不用去终端粘命令）
//
// 执行方式：用 Electron 自带的 Node 跑 CLI（ELECTRON_RUN_AS_NODE=1），
//   这样本机即使没装 node 也能把配置写进去。
//   但 hook 是 *运行时* 由 agent 用 `node "<脚本>"` 拉起的，所以本机没 node
//   时配置能装上、执行会失败 —— 下面会探一次并如实告知，不假装成功。
// 失败时把等价的命令行一并回给渲染层，让用户自己复制执行（退路）。
// ---------------------------------------------------------------------------
const HOOK_AGENTS = ['zcode', 'claude', 'vscode', 'trae', 'cursor', 'opencode', 'codex', 'qwen', 'all'];

function manualInstallCommand(agent, clean) {
  return `node "${hookScriptPath()}" install --agent ${agent}${clean ? ' --clean' : ''}`;
}

function runHookCli(args, timeoutMs = 20000) {
  return new Promise((resolve) => {
    execFile(process.execPath, args, {
      timeout: timeoutMs,
      windowsHide: true,
      env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
      maxBuffer: 4 * 1024 * 1024,
    }, (err, stdout, stderr) => {
      // err.code 是数字=退出码；是字符串=压根没起来（ENOENT 之类）
      const exitCode = err ? (typeof err.code === 'number' ? err.code : -1) : 0;
      const spawnErr = err && typeof err.code !== 'number' ? String(err.message || err) : '';
      resolve({ exitCode, spawnErr, stdout: String(stdout || ''), stderr: String(stderr || '') });
    });
  });
}

// hook 运行时要靠 `node` 在 PATH 里；装了才敢说"能用"
function probeNode() {
  return new Promise((resolve) => {
    // 不用 shell：Windows 上 libuv 自己会按 PATHEXT 找到 node.exe，
    // 而 shell:true + args 在 Node 22 上会报 DEP0190
    execFile('node', ['--version'], { timeout: 5000, windowsHide: true },
      (err, stdout) => resolve(err ? '' : String(stdout || '').trim()));
  });
}

function extractWrittenFiles(out) {
  const files = [];
  let m;
  const re = /已写入 ([^\r\n（]+)/g;
  while ((m = re.exec(out))) files.push(m[1].trim());
  const re2 = /已安装插件 ([^\r\n]+)/g;
  while ((m = re2.exec(out))) files.push(m[1].trim());
  return Array.from(new Set(files));
}

// stdout 里除了「写了哪个文件」之外的说明性文字（沙箱/双跑之类的注意事项），
// 单独抽出来显示在面板上，别让它埋在折叠的日志里被忽略
function extractNotes(out) {
  return String(out || '')
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter((s) => s
      && !/^已写入 /.test(s)
      && !/^已安装插件 /.test(s)
      && !/^改动需重启/.test(s))
    .join('\n')
    .trim();
}

ipcMain.handle('hook:install', (_e, payload) => runHookInstall(payload));

// 抽成函数，便于冒烟测试直接调用（见文末 POMODORO_SMOKE_INSTALL）
async function runHookInstall(payload) {
  const p = payload || {};
  const agent = HOOK_AGENTS.includes(String(p && p.agent)) ? String(p.agent) : 'claude';
  const clean = !!(p && p.clean);
  const command = manualInstallCommand(agent, clean);

  const script = installHookScript();
  if (!script) {
    return {
      ok: false, agent, command, files: [],
      message: '无法释放 hook 脚本：写用户目录失败（检查磁盘权限/空间）',
    };
  }

  const args = [script, 'install', '--agent', agent];
  if (clean) args.push('--clean');
  const res = await runHookCli(args);

  const files = extractWrittenFiles(res.stdout);
  const log = [res.stdout, res.stderr].filter(Boolean).join('\n').trim();
  const ok = res.exitCode === 0 && !res.spawnErr;

  if (!ok) {
    const firstErr = String(res.stderr || '').split(/\r?\n/).map((s) => s.trim()).filter(Boolean)[0] || '';
    return {
      ok: false, agent, command, files, log,
      message: res.spawnErr
        ? `安装进程没能启动：${res.spawnErr}`
        : `安装失败（退出码 ${res.exitCode}）${firstErr ? '：' + firstErr : ''}`,
    };
  }

  const nodeVersion = await probeNode();
  return {
    ok: true, agent, command, files, log,
    notes: extractNotes(res.stdout),
    nodeVersion,
    // 配置装上了，但运行时缺 node → 必须提示，不然用户只会看到"配了没反应"
    nodeMissing: !nodeVersion,
    message: files.length
      ? `已写入 ${files.length} 处配置：\n${files.join('\n')}`
      : '安装完成（未解析到写入路径，展开日志查看详情）',
  };
}

function pushGatewayState() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  mainWindow.webContents.send('state:gateway', {
    enabled: !!(gateway && gateway.isRunning()),
    port: gateway ? gateway.getPort() : null,
    hookPath: hookScriptPath(),
    pluginPath: opencodePluginPath(),
    activity: gateway ? gateway.getActivity() : null,
  });
}

function startGateway() {
  if (gateway) return;
  gateway = createGateway({
    showPopup: showNotify,
    sendTimerCommand: sendRendererCommand,
    onActivity: (a) => {
      if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send('state:agent-activity', a);
    },
    getTimerState: () => lastTimerState,
    getUserDataPath: () => app.getPath('userData'),
    log: (...args) => console.log(...args),
  });
  gateway.start().then(() => {
    pushGatewayState();
    if (process.env.POMODORO_GATEWAY_SMOKE === '1') gateway.smoke();
  }).catch((err) => {
    console.error('[gateway] 启动失败:', err.message);
    gateway = null;
    pushGatewayState();
  });
}

function stopGateway() {
  if (gateway) {
    gateway.stop();
    gateway = null;
  }
  pushGatewayState();
}

ipcMain.on('gateway:set-enabled', (_e, enabled) => {
  saveConfig({ gatewayEnabled: !!enabled });
  if (enabled) startGateway();
  else stopGateway();
});

ipcMain.on('gateway:get-state', () => {
  pushGatewayState();
});

// 交互弹窗（ask / permission / custom）用户在弹窗内决策 →
// 网关 resolve → 长轮询的 hook 请求拿到结果 → 关闭该弹窗（按来源窗口定位）
ipcMain.on('interaction:respond', (e, payload) => {
  const p = payload || {};
  const resolved = gateway
    ? gateway.resolveInteraction(p.id, { action: p.action, answers: p.answers || {}, text: p.text || '' }, 'user')
    : false;
  // 非网关弹窗（阶段结束提醒等）也带按钮：点「进入下一阶段」直接开跑下一阶段
  if (!resolved && p.action === 'start-next') sendRendererCommand('start-next');
  // 由弹窗页发出的回应一律收起该弹窗：网关那边可能已经 resolve（主进程关），
  // 本地弹窗（没有挂起交互）也得关，否则只能等自动关闭
  const win = BrowserWindow.fromWebContents(e.sender);
  if (win && !win.isDestroyed()) win.close();
});

// 旧接口兼容：只有 action 的确认
ipcMain.on('confirm:respond', (e, payload) => {
  const p = payload || {};
  const resolved = gateway ? gateway.resolveConfirm(p.id, p.action, 'user') : false;
  if (resolved) {
    const win = BrowserWindow.fromWebContents(e.sender);
    if (win && !win.isDestroyed()) win.close();
  }
});

// 渲染进程请求写剪贴板（复制 hook 配置片段）
ipcMain.on('clipboard:write', (_e, text) => {
  try { clipboard.writeText(String(text || '')); } catch (e) { /* ignore */ }
});

// ---------------------------------------------------------------------------
// 应用生命周期
// ---------------------------------------------------------------------------
app.whenReady().then(() => {
  // 保证图标资源存在
  ensureAssets();

  createMainWindow();
  createTray();

  // Agent 网关（hooks 对接）：安装 hook 脚本 + 按配置启动
  installHookScript();
  if (loadConfig().gatewayEnabled !== false) startGateway();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createMainWindow();
    else if (mainWindow) mainWindow.show();
  });

  // 弹窗演示：POMODORO_POPUP_DEMO=1 依次弹一遍 ask / permission / notification
  if (process.env.POMODORO_POPUP_DEMO) {
    const demo = [
      { kind: 'ask', source: 'zcode', title: '用哪种方案实现？', message: 'ZCode 想确认重构方向',
        context: { agent: 'zcode', session: 'a1b2c3', project: 'pomodoro-fluent', task: '把 agent 网关的弹窗改成可交互的', tool: 'AskUserQuestion' },
        questions: [
          { id: 'q0', question: '选一种缓存策略：', header: '缓存', multiSelect: false, custom: true,
            options: [{ id: 'o0', label: 'LRU', description: '最近最少使用，内存可控' }, { id: 'o1', label: 'TTL', description: '按时间过期，实现简单' }] },
          { id: 'q1', question: '需要覆盖哪些端？（可多选）', header: '范围', multiSelect: true, custom: false,
            options: [{ id: 'o0', label: 'Web' }, { id: 'o1', label: '桌面端' }, { id: 'o2', label: '移动端' }] },
        ], timeoutMs: 60000 },
      { kind: 'permission', source: 'claude-code', title: '允许 Bash？', message: 'agent 请求执行该工具',
        detail: 'command: npm run build -- --watch\ndescription: 构建并监听变更',
        context: { agent: 'claude-code', agentType: 'implementation-agent', session: 'd4e5f6', project: 'pomodoro-fluent', task: '修复登录超时后重试逻辑', tool: 'Bash', toolDetail: 'npm run build -- --watch' },
        permission: { tool: 'Bash', rule: 'npm run build', canAlways: true }, timeoutMs: 60000 },
      { kind: 'notification', source: 'opencode', title: '任务跑完了', message: '12 个文件已更新，测试全绿', sub: '',
        context: { agent: 'opencode', session: '778899', project: 'pomodoro-fluent', task: '重构缓存层并补齐单测', tool: 'task' } },
    ];
    demo.forEach((d, i) => setTimeout(() => showNotify(d), 400 + i * 3500));
  }

  if (process.env.POMODORO_SMOKE === '1') {
    // 冒烟测试模式：加载完成后自动退出
    let smokeMs = 8000;
    // POMODORO_SMOKE_INSTALL=1 时，把「一键安装」从渲染层按钮点到落盘整个链路跑一遍。
    // 关键：HOME 全程指向临时目录，所以只验证链路是否通，绝不碰本机真实 agent 配置。
    if (process.env.POMODORO_SMOKE_INSTALL === '1') {
      smokeMs = 30000;
      const tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'pomodoro-smoke-home-'));
      const saved = { USERPROFILE: process.env.USERPROFILE, HOME: process.env.HOME, XDG_CONFIG_HOME: process.env.XDG_CONFIG_HOME };
      const setHome = (h) => {
        process.env.USERPROFILE = h;
        process.env.HOME = h;
        process.env.XDG_CONFIG_HOME = path.join(h, '.config');
      };
      setHome(tmpHome);

      // 等页面加载完，再在渲染层里点按钮，读回结果面板的文本
      const waitLoaded = () => new Promise((resolve) => {
        if (!mainWindow || mainWindow.isDestroyed()) return resolve(false);
        if (!mainWindow.webContents.isLoading()) return resolve(true);
        mainWindow.webContents.once('did-finish-load', () => resolve(true));
      });
      const clickInstall = () => mainWindow.webContents.executeJavaScript(`(async () => {
        const btn = document.getElementById('btnInstallHook');
        const box = document.getElementById('installResult');
        const sel = document.getElementById('agentSelect');
        if (!btn || !box || !sel) return 'missing-dom';
        if (typeof window.pomodoro.installHook !== 'function') return 'missing-bridge';
        sel.value = 'trae';
        sel.dispatchEvent(new Event('change'));
        btn.click();
        const t0 = Date.now();
        while (Date.now() - t0 < 20000) {
          if (!box.hidden && box.textContent) break;
          await new Promise((r) => setTimeout(r, 120));
        }
        return (box.hidden ? 'hidden|' : 'shown|') + box.textContent.replace(/\\s+/g, ' ').slice(0, 300)
          + ' [cmd=' + (/install --agent trae/.test(box.textContent) ? 'yes' : 'no') + ']';
      })()`, true);

      (async () => {
        try {
          if (!await waitLoaded()) { console.log('[smoke] 页面未加载，跳过一键安装验证'); return; }
          console.log('[smoke] 渲染层一键安装（成功路径）→ ' + await clickInstall());
          // 失败路径：把 HOME 指到「父路径是文件」的位置，写入必然失败
          const blocker = path.join(tmpHome, 'blocker');
          fs.writeFileSync(blocker, 'x');
          setHome(path.join(blocker, 'sub'));
          console.log('[smoke] 渲染层一键安装（失败路径）→ ' + await clickInstall());
        } catch (e) {
          console.log('[smoke] 一键安装冒烟异常:', e && e.message);
        } finally {
          Object.entries(saved).forEach(([k, v]) => {
            if (v === undefined) delete process.env[k]; else process.env[k] = v;
          });
          try { fs.rmSync(tmpHome, { recursive: true, force: true }); } catch (e) { /* ignore */ }
          isQuitting = true;
          app.exit(0);
        }
      })();
    }
    // 安全兜底：万一上面的流程卡住，也保证进程能退出
    setTimeout(() => { isQuitting = true; app.exit(0); }, smokeMs);
  }
});

app.on('window-all-closed', () => {
  // 常驻托盘，不退出（macOS 外也一样）
});

app.on('before-quit', () => {
  isQuitting = true;
  stopGateway();
});

// ---------------------------------------------------------------------------
// 资源自检：确保图标存在（从 package 生成占位 PNG）
// 1) tray.png  —— 白色单色 + 透明背景的番茄模板，供 makeTrayIcon 染色使用
// 2) icon.png  —— 彩色红番茄图标，用于窗口/任务栏
// ---------------------------------------------------------------------------
function ensureAssets() {
  const dir = path.join(__dirname, 'assets');
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });

  const p = path.join(dir, 'tray.png');
  if (!fs.existsSync(p)) {
    writePng(p, 32, 32, (x, y) => {
      const [v, a] = tomatoShape(x + 0.5, y + 0.5, 32);
      return [v, v, v, a];
    });
  }

  const ip = path.join(dir, 'icon.png');
  if (!fs.existsSync(ip)) {
    writePng(ip, 256, 256, (x, y) => {
      // 彩色番茄：红渐变主体 + 绿叶子 + 高光
      const s = 256;
      const px = x + 0.5, py = y + 0.5;
      const [v, a] = tomatoShape(px, py, s);
      if (a <= 0) return [0, 0, 0, 0];
      // 红渐变（从左上亮红到右下深红）
      const t = (px / s + py / s) / 2;
      const r = Math.round(255 - 40 * t);
      const g = Math.round(120 - 55 * t);
      const b = Math.round(130 - 60 * t);
      // 高光区提亮
      const cx = s / 2, cy = s * 0.58, bodyR = s * 0.36;
      const hiCx = cx - bodyR * 0.3, hiCy = cy - bodyR * 0.35, hiR = bodyR * 0.32;
      const dHi = Math.sqrt((px - hiCx) ** 2 + (py - hiCy) ** 2);
      let fr = r, fg = g, fb = b;
      if (dHi < hiR) { fr = 255; fg = Math.min(255, g + 60); fb = Math.min(255, b + 60); }
      // 叶子为绿色
      const leafCy = cy - bodyR + 1;
      const dLeafC = Math.sqrt((px - cx) ** 2 + (py - leafCy) ** 2);
      let isLeaf = dLeafC < s * 0.07;
      for (let i = 0; i < 5; i++) {
        const ang = (i * 72 - 90) * Math.PI / 180;
        const along = (px - cx) * Math.cos(ang) + (py - leafCy) * Math.sin(ang);
        const perp = -(px - cx) * Math.sin(ang) + (py - leafCy) * Math.cos(ang);
        if (along >= -s * 0.03 && along <= s * 0.24) {
          const halfW = s * 0.045 + along * 0.08;
          if (Math.abs(perp) < halfW) isLeaf = true;
        }
      }
      if (isLeaf) { fr = 96; fg = 190; fb = 120; }
      return [fr, fg, fb, a];
    });
  }
}

// 番茄造型：白色主体 + 顶部五角星叶片 + 高光（返回 [白度, alpha]）
function tomatoShape(px, py, s) {
  const cx = s / 2, cy = s * 0.58;
  const bodyR = s * 0.36;

  // 主体（下方略扁）
  const dBody = Math.sqrt((px - cx) ** 2 + (py - cy) ** 2);
  let bodyA = Math.max(0, Math.min(1, bodyR + 0.5 - dBody));
  const squash = 1.08;
  if (py > cy) {
    const dBody2 = Math.sqrt(((px - cx) ** 2) + ((py - cy) ** 2) / (squash * squash));
    bodyA = Math.max(0, Math.min(1, bodyR * squash + 0.5 - dBody2));
  }

  // 顶部五角星叶片
  const leafCy = cy - bodyR + 1;
  const leafCx = cx;
  let leafA = 0;
  for (let i = 0; i < 5; i++) {
    const ang = (i * 72 - 90) * Math.PI / 180;
    const along = (px - leafCx) * Math.cos(ang) + (py - leafCy) * Math.sin(ang);
    const perp = -(px - leafCx) * Math.sin(ang) + (py - leafCy) * Math.cos(ang);
    if (along >= -s * 0.03 && along <= s * 0.24) {
      const halfW = s * 0.045 + along * 0.08;
      if (Math.abs(perp) < halfW) leafA = Math.max(leafA, 0.9);
    }
  }
  const dLeafC = Math.sqrt((px - leafCx) ** 2 + (py - leafCy) ** 2);
  if (dLeafC < s * 0.07) leafA = Math.max(leafA, 0.9);

  // 高光（左上）
  const hiCx = cx - bodyR * 0.3, hiCy = cy - bodyR * 0.35, hiR = bodyR * 0.32;
  const dHi = Math.sqrt((px - hiCx) ** 2 + (py - hiCy) ** 2);
  const bodyV = dHi < hiR ? 255 : 230;

  const alpha = Math.min(1, Math.max(bodyA, leafA));
  if (alpha <= 0) return [0, 0];
  return [Math.max(bodyV, 255), Math.round(alpha * 255)];
}

// 极简 PNG 编码器（无第三方依赖），用于生成托盘图标
function writePng(file, w, h, colorAt) {
  const zlib = require('zlib');

  function crc32(buf) {
    let table = crc32.table;
    if (!table) {
      table = crc32.table = new Int32Array(256);
      for (let n = 0; n < 256; n++) {
        let c = n;
        for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
        table[n] = c;
      }
    }
    let c = 0xffffffff;
    for (let i = 0; i < buf.length; i++) c = table[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  }

  function chunk(type, data) {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length, 0);
    const typeBuf = Buffer.from(type, 'ascii');
    const body = Buffer.concat([typeBuf, data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(body), 0);
    return Buffer.concat([len, body, crc]);
  }

  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;  // bit depth
  ihdr[9] = 6;  // color type RGBA
  ihdr[10] = 0; // compression
  ihdr[11] = 0; // filter
  ihdr[12] = 0; // interlace

  const raw = Buffer.alloc(h * (1 + w * 4));
  for (let y = 0; y < h; y++) {
    raw[y * (1 + w * 4)] = 0; // filter none
    for (let x = 0; x < w; x++) {
      const [r, g, b, a] = colorAt(x, y);
      const o = y * (1 + w * 4) + 1 + x * 4;
      raw[o] = r; raw[o + 1] = g; raw[o + 2] = b; raw[o + 3] = a;
    }
  }
  const idat = zlib.deflateSync(raw);

  const png = Buffer.concat([
    sig,
    chunk('IHDR', ihdr),
    chunk('IDAT', idat),
    chunk('IEND', Buffer.alloc(0)),
  ]);
  fs.writeFileSync(file, png);
}
