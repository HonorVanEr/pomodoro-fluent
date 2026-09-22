//! 主线程停滞看门狗。
//!
//! # 为什么需要它
//!
//! Rust 版出现过「界面完全点不动 —— 关闭 / 最小化按钮无反应、托盘右键也不弹
//! 菜单 —— 但倒计时照常在走」的卡死，而且无法稳定复现。
//!
//! 倒计时还在走说明 **WebView2 渲染进程活着**（JS 跑在渲染进程里），所以问题只
//! 可能出在**主线程不再泵消息**上：Tauri 的同步命令、窗口事件、托盘回调全部跑在
//! 主线程（`setup` 里实测 `ThreadId(1)`）。而主线程一旦停在某个原生调用里，
//! 从外部完全看不出停在哪一步 —— 这是这次排查最大的盲区。
//!
//! 这里补上这个盲区：
//!
//! 1. 后台线程每 [`PING_MS`] 往主线程投一个任务并**等回执**（`run_on_main_thread`
//!    只保证投进队列，不保证被执行，所以必须用回执判定），超过 [`STALL_MS`]
//!    没回执即判定停滞。
//! 2. [`timed`] 把「主线程当前正在执行的原生调用」登记到一个静态量里；停滞时
//!    连同它一起写日志，下次再卡就能**直接点名是哪一个调用**。
//!
//! # 开销与落盘
//!
//! 日志只在**检测到停滞**时才写，路径 `<userData>/watchdog.log`，正常使用下
//! 一个字节都不写。停滞后不再刷屏（连续停滞只记前 [`MAX_REPORTS_PER_STALL`] 次）。
//! 每次停滞还同时打一行 stderr，debug 构建（有控制台）能直接看到。
//!
//! 设 `POMODORO_UI_WATCHDOG=0` 可整体关掉（排查看门狗自身的问题时用）。

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;

/// 每轮 ping 的间隔（ms）
const PING_MS: u64 = 500;
/// 超过这个时长没回执就判定主线程停滞（ms）
const STALL_MS: u64 = 3_000;
/// 一次连续停滞最多记这么多条，免得长期卡死把日志刷满
const MAX_REPORTS_PER_STALL: u8 = 3;
/// 日志体积上限（字节），超了直接重来 —— 不做轮转，这条日志只在出故障时才长
const LOG_MAX_BYTES: u64 = 64 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// 主线程当前正在执行的原生调用。
/// `&'static str` 字面量：登记时不分配，停滞时也不用担心分配失败。
static CURRENT: Mutex<Option<&'static str>> = Mutex::new(None);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn current() -> MutexGuard<'static, Option<&'static str>> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// 在 `main()` 里尽早调：只决定开关，不起线程。
pub fn init() {
    // 默认开；只有显式给 `0` 才关
    let on = std::env::var_os("POMODORO_UI_WATCHDOG").map_or(true, |v| v != "0");
    ENABLED.store(on, Ordering::Relaxed);
}

/// 在 `setup` 里调：起看门狗线程。
pub fn start(app: &AppHandle) {
    if !enabled() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let path = log_path();
        let mut reports_in_this_stall: u8 = 0;
        loop {
            std::thread::sleep(Duration::from_millis(PING_MS));
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let t0 = Instant::now();
            if app
                .run_on_main_thread(move || {
                    let _ = tx.send(());
                })
                .is_err()
            {
                // 事件循环已经停了（应用正在退出）—— 收工
                return;
            }
            if rx.recv_timeout(Duration::from_millis(STALL_MS)).is_ok() {
                if reports_in_this_stall > 0 {
                    let line = format!("{}\t主线程恢复（本轮累计停滞 {reports_in_this_stall} 次上报）", epoch_ms());
                    eprintln!("[watchdog] 主线程已恢复");
                    append(&path, &line);
                }
                reports_in_this_stall = 0;
                continue;
            }

            let stalled_ms = t0.elapsed().as_millis();
            let label = current().unwrap_or("(未登记 —— 不在 timed 包住的调用里)");
            if reports_in_this_stall < MAX_REPORTS_PER_STALL {
                eprintln!("[watchdog] 主线程停滞 {stalled_ms}ms，卡在: {label}");
                append(&path, &format!("{}\t停滞 {stalled_ms}ms\t{label}", epoch_ms()));
            }
            reports_in_this_stall = reports_in_this_stall.saturating_add(1);
        }
    });
}

/// 观测一段**主线程上的**原生调用：进入时登记标签、返回时清掉。
///
/// 看门狗判定停滞时会把登记着的标签写进日志 —— 那就是卡住处。
/// 只包可疑调用（跨进程的 shell 调用、等外部进程的原生接口），别包 16ms 级热路径。
///
/// ⚠ 只允许在主线程上调：`CURRENT` 表达的是「主线程正在做什么」，
/// 从别的工作线程登记会把它带偏。
pub fn timed<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        // 关掉看门狗时零开销（不碰锁）
        return f();
    }
    *current() = Some(label);
    let out = f();
    *current() = None;
    out
}

fn log_path() -> std::path::PathBuf {
    pomodoro_core::user_data_dir().join("watchdog.log")
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn append(path: &Path, line: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
        let _ = std::fs::File::create(path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}
