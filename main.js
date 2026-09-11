'use strict';

const { app, BrowserWindow, Tray, Menu, nativeImage, ipcMain, screen, shell, Notification } = require('electron');
const path = require('path');
const fs = require('fs');
const { execFile } = require('child_process');

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
// 创建通知弹窗（到时间弹出）
// ---------------------------------------------------------------------------
function showNotify(payload) {
  const data = payload || {};
  const title = data.title || '时间到';
  const message = data.message || '';
  const type = data.type || 'work';

  // 如果已经有通知窗口，先关掉旧的
  if (notifyWindow && !notifyWindow.isDestroyed()) {
    notifyWindow.close();
    notifyWindow = null;
  }

  const nw = new BrowserWindow({
    width: 400,
    height: 170,
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

  // 放在屏幕右上角
  const { workArea } = screen.getPrimaryDisplay();
  const b = nw.getBounds();
  nw.setPosition(workArea.x + workArea.width - b.width - 16, workArea.y + 16);

  // 通过 query 传数据（简单可靠）
  nw.loadFile(path.join(__dirname, 'renderer', 'notify.html'), {
    query: { title, message, type },
  });

  nw.once('ready-to-show', () => {
    nw.show();
    setTimeout(() => applyAcrylicToWindow(nw, '28,30,40', 0.5, 14), 120);
  });

  nw.on('closed', () => {
    notifyWindow = null;
  });
}

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
      click: () => {
        if (mainWindow) mainWindow.webContents.send('tray:command', 'toggle');
      },
    },
    {
      label: '重置',
      click: () => {
        if (mainWindow) mainWindow.webContents.send('tray:command', 'reset');
      },
    },
    {
      label: '跳到下一阶段',
      click: () => {
        if (mainWindow) mainWindow.webContents.send('tray:command', 'skip');
      },
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

ipcMain.on('tray:update', (_e, state) => {
  if (!tray) return;
  const s = state || {};
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

ipcMain.on('notify:close', () => {
  if (notifyWindow && !notifyWindow.isDestroyed()) {
    notifyWindow.close();
  }
});

// ---------------------------------------------------------------------------
// 应用生命周期
// ---------------------------------------------------------------------------
app.whenReady().then(() => {
  // 保证图标资源存在
  ensureAssets();

  createMainWindow();
  createTray();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createMainWindow();
    else if (mainWindow) mainWindow.show();
  });

  if (process.env.POMODORO_SMOKE === '1') {
    // 冒烟测试模式：加载完成后自动退出
    setTimeout(() => { isQuitting = true; app.exit(0); }, 8000);
  }
});

app.on('window-all-closed', () => {
  // 常驻托盘，不退出（macOS 外也一样）
});

app.on('before-quit', () => {
  isQuitting = true;
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
