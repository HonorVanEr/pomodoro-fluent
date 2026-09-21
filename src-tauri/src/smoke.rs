//! 冒烟自检钩子 —— 只在 `POMODORO_SMOKE=1` 时生效，正常运行时完全不参与。
//!
//! # 为什么需要它
//!
//! M1 的核心风险是「渲染层 → bridge.js → invoke → Rust」这条链路在**真机上**通不通。
//! 而下面这些都不能证明链路是通的：
//! - 进程没崩、窗口起来了、托盘有图标 —— 渲染层的错误只进 webview 的 console，
//!   进程 stderr 里一个字都看不到（bridge 漏个方法就是一句 TypeError，界面全死但进程照活）；
//! - 打包能过、单元测试能过 —— 它们压根不碰 webview。
//!
//! 本来想用 WebView2 的 CDP 调试端口去 webview 里看（`--remote-debugging-port`），
//! 实测**不可靠**，放弃了：
//! 1. WebView2 的 Browser 进程是**按用户数据目录共享**的。被强杀的实例会残留
//!    `msedgewebview2.exe`，新实例复用它 —— 而它没带调试端口，于是端口时有时无；
//!    更阴的是 `/json/list` 会报出**上一次的**陈旧 target（`about:blank`），
//!    连上去 evaluate 一直在问另一个文档，看着像 "bridge 没注入"。
//! 2. 就算端口起来了，实测 t+3s 能看到应用页面，t+6s 起 DevTools 的 HTTP 端点
//!    就整个不再响应（清一色 TimeoutError）—— 没等断言完就废了。
//!
//! # 做法
//!
//! 渲染层启动时**必然**会打两个 invoke（不用点任何按钮）：
//! - `tray_update`       ← `syncTray()`，启动即调一次，之后每秒一次
//! - `pending_get_held`  ← `requestHeldPending()`，启动时无条件调一次
//!
//! 这里各记一行到 stderr，两个都到齐就打 `[smoke] handshake complete` 并退出。
//! `scripts/smoke-tauri.mjs` 读 stderr 就能判定链路是否打通 —— 确定性、无端口、无超时抖动。
//!
//! ⚠ 正常运行时零影响：`init()` 只做一次 `env::var_os`，没设变量就永远不会再有分支。

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager};

static ENABLED: AtomicBool = AtomicBool::new(false);
static SEEN_TRAY_UPDATE: AtomicBool = AtomicBool::new(false);
static SEEN_PENDING: AtomicBool = AtomicBool::new(false);

/// 是否处于自检模式（每个调用点都会问一次，成本是一次原子读）
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn init() {
    if std::env::var_os("POMODORO_SMOKE").is_some() {
        ENABLED.store(true, Ordering::Relaxed);
        eprintln!("[smoke] 自检模式已启用，等渲染层的两个启动期 invoke…");
    }
}

/// 自检模式下的启动步骤埋点：打印一行 `[smoke] step: <msg>`。
///
/// 用来区分「卡在某一步没往下走」和「走完了但渲染层没起来」——
/// 没有它的话，两种情况在 stderr 上长得一模一样（都只有 init 那一行）。
pub fn step(msg: &str) {
    if enabled() {
        eprintln!("[smoke] step: {msg}");
    }
}

/// 自检模式下的「窗口标题」观察线程 —— 失败现场取证用。
///
/// [`DIAG_JS`] 把页面侧的环境信息写进 `document.title`，wry 会把它同步成窗口标题。
/// 这条旁路**不依赖 IPC**（IPC 本身就是嫌疑对象），也不依赖 DevTools。
/// 每秒往 stderr 打一行，于是「没握上手」时我们能直接看到 `ipc=` / `pomodoro=` / `ready=`。
///
/// 只在自检模式启动；正常运行时这个线程根本不会创建。
pub fn watch_title(app: &AppHandle) {
    if !enabled() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        for i in 1..=12 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let title = app
                .get_webview_window(crate::window::MAIN)
                .and_then(|w| w.title().ok())
                .unwrap_or_else(|| "<取不到窗口>".into());
            eprintln!("[smoke] t+{i}s title={title}");
        }
    });
}

/// 两个握手点都到了就收工。
fn maybe_finish(app: &AppHandle) {
    if SEEN_TRAY_UPDATE.load(Ordering::Relaxed) && SEEN_PENDING.load(Ordering::Relaxed) {
        eprintln!("[smoke] handshake complete");
        // 直接退出：不要再走窗口 / 托盘的收尾流程，免得脚本等超时
        app.exit(0);
    }
}

/// `tray:update` 到达（渲染层 `syncTray()`）
pub fn on_tray_update(app: &AppHandle, phase: &str, time_text: &str, running: bool) {
    if SEEN_TRAY_UPDATE.swap(true, Ordering::Relaxed) {
        return; // 之后每秒一次，只记第一次
    }
    eprintln!("[smoke] invoke ok: tray_update phase={phase} text={time_text} running={running}");
    maybe_finish(app);
}

/// `pending:get-held` 到达（渲染层 `requestHeldPending()`）
pub fn on_pending_get_held(app: &AppHandle) {
    if SEEN_PENDING.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!("[smoke] invoke ok: pending_get_held");
    maybe_finish(app);
}

/// 自检模式下**额外**注入的诊断脚本：把页面侧的 console.error / console.warn /
/// 未捕获异常 / unhandledrejection 汇总写进 `document.title`。
///
/// 为什么需要它：如果 IPC 链路是断的，上面那两个握手点就永远到不了，
/// 而"为什么断"的答案只在 webview 的 console 里 —— 那是个黑盒：
///   · CDP 进不去（见文件头，DevTools 端点存活几秒就死）；
///   · `--enable-logging` 也捞不到渲染进程的 console；
///   · 走 IPC 上报更是循环依赖（IPC 本身就是嫌疑对象）。
/// 但 `document.title` 会被 wry 同步成**窗口标题**，而窗口标题用系统 API 就能读
/// （`(Get-Process pomodoro).MainWindowTitle`）—— 这是一条不依赖 IPC、也不依赖
/// DevTools 的旁路，专门用来查"IPC 为什么不通"这种问题。
///
/// 只在 `POMODORO_SMOKE=1` 时注入，正常运行时这段代码根本不会进到页面里。
pub const DIAG_JS: &str = r#"
(function () {
  var msgs = [];
  function envInfo() {
    return 'ipc=' + typeof window.ipc
      + ',ipcpm=' + typeof (window.ipc && window.ipc.postMessage)
      + ',tauri=' + typeof window.__TAURI__
      + ',invoke=' + typeof (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      + ',pomodoro=' + typeof window.pomodoro
      + ',ready=' + document.readyState;
  }
  function paint() {
    try { document.title = 'DIAG#2 ' + envInfo() + ' || ' + msgs.join(' || '); } catch (e) { /* ignore */ }
  }
  // 先无条件占个位：这样"窗口标题还是默认值"和"脚本没跑"能区分开
  paint();
  setInterval(paint, 1000);
  function push(kind, text) {
    if (msgs.length < 5) { msgs.push(kind + '=' + String(text).slice(0, 110)); paint(); }
  }
  ['error', 'warn'].forEach(function (k) {
    var orig = console[k] ? console[k].bind(console) : function () {};
    console[k] = function () {
      push(k, Array.prototype.slice.call(arguments).map(function (a) {
        return (a && a.message) ? a.message : String(a);
      }).join(' '));
      return orig.apply(null, arguments);
    };
  });
  window.addEventListener('error', function (e) { push('onerror', e.message); });
  window.addEventListener('unhandledrejection', function (e) {
    var r = e.reason;
    push('rej', (r && (r.message || r)) || 'unknown');
  });
})();
"#;
