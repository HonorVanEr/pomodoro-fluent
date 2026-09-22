//! 主窗口行为：定位、关窗隐藏、迷你悬浮、贴边隐藏与唤回。
//!
//! 对应 Electron 版 `main.js` 里 `createMainWindow` / `setPinMini` / `drag-*` /
//! `dock-*` / `revealMainWindow` 那几段。**几何计算全在 [`crate::geometry`]**，
//! 这里只负责取窗口状态、调窗口 API、推事件。
//!
//! 并发约定：一律「取锁 → 拷贝出需要的值 → 放锁 → 调窗口 API」，
//! 绝不持锁调用长得可能阻塞的原生接口（会跟拖拽线程互相卡）。
//! 读取走 [`win_state`]（快照）、修改走 [`edit_win`]（闭包），
//! **不要在别处手写 `lock(&state(app).win)`** —— 见 [`win_state`] 的说明。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::geometry::{self, Edge, Metrics, Rect, DOCK_HIDE_PAD, DOCK_REVEAL_PAD};
use crate::state::{lock, AppState, Drag, WindowState};
use crate::watchdog;

/// 主窗口 label
pub const MAIN: &str = "main";

/// 迷你模式拖拽的轮询间隔（ms），与 Electron 版一致
const DRAG_POLL_MS: u64 = 16;
/// 单次拖拽的硬上限（ms）。正常拖拽是秒级，这个值只是防「按键状态本身也不可信」
/// 时线程永久泄漏（例如输入桌面被切走、物理按键卡死）。
const DRAG_MAX_MS: u64 = 120_000;
/// 「延时收回」重试间隔（ms）
const DOCK_HIDE_RETRY_MS: u64 = 300;
/// 「延时收回」最多重试几次。
///
/// **必须有上限**：光标位置取不到时 [`cursor`] 会退化成 `NaN`，而
/// [`geometry::cursor_in_rect_padded`] 把 `NaN` 当作「还在窗口上」（这是刻意的，
/// 免得拿不到坐标就误收起）。两者叠加就成了**每 300ms 起一条新线程的死循环**，
/// 这是本文件里唯一的无界循环。
const DOCK_HIDE_MAX_RETRY: u32 = 10;
/// 抢前台失败后闪任务栏的延迟（ms）
const FOREGROUND_RETRY_MS: u64 = 300;

/// 拖拽线程的世代号：每次 start_drag 自增，旧线程醒来发现世代变了就退出。
/// 不能只靠 `drag == None` 判断 —— 用户可能「停止后立刻重新按下」，
/// 旧线程醒来会看到新会话还在，于是两个线程同时驱动窗口，窗口开始抖。
static DRAG_GENERATION: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// 共享状态读取
// ---------------------------------------------------------------------------

/// 窗口状态的**快照**（取锁 → 拷贝 → 立刻放锁）。
///
/// 为什么不能写成 `lock(&state(app).win)`：
/// `tauri::State` 的 `Deref::deref(&self) -> &T` 借用的是 `State` **这个临时值**，
/// 临时值在语句结束就被释放，守卫却还想活着 → `E0716: temporary value dropped
/// while borrowed`。所以必须先把 `State` 绑到局部变量。
/// 本函数把这个绑定收在一处，调用方就只会拿到一个 `Copy` 的快照值。
///
/// 快照而非守卫也是刻意的：调用方拿到值就会接着调窗口 API，
/// 而持锁过原生调用会和 16ms 的拖拽线程互相卡。
pub fn win_state(app: &AppHandle) -> WindowState {
    let st = app.state::<AppState>();
    // 先落到局部变量再返回：写成尾表达式 `*lock(&st.win)` 的话，
    // 守卫那个临时值会在 `st` 之后析构 → `E0597: st does not live long enough`
    // （守卫的 Drop 要跑，编译器不允许它活过 `st`）。
    let snapshot = *lock(&st.win);
    snapshot
}

/// 改窗口状态：闭包在**持锁期间**执行，返回值原样带出。
///
/// 闭包体里只做内存改动，**不要调窗口 API / 推事件** —— 会持锁过原生调用。
pub fn edit_win<R>(app: &AppHandle, f: impl FnOnce(&mut WindowState) -> R) -> R {
    let st = app.state::<AppState>();
    let mut w = lock(&st.win);
    f(&mut w)
}

// ---------------------------------------------------------------------------
// 基础取值
// ---------------------------------------------------------------------------

pub fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(MAIN)
}

/// 窗口所在屏的缩放比 → 本次几何计算要用的物理像素尺寸。
pub fn metrics_for(win: &WebviewWindow) -> Metrics {
    Metrics::for_scale(win.scale_factor().unwrap_or(1.0))
}

/// 窗口当前的物理 bounds。
pub fn bounds(win: &WebviewWindow) -> Option<Rect> {
    let p = win.outer_position().ok()?;
    let s = win.outer_size().ok()?;
    Some(Rect::new(p.x, p.y, s.width as i32, s.height as i32))
}

fn monitor_to_rect(m: &tauri::Monitor) -> Rect {
    let wa = m.work_area();
    Rect::new(
        wa.position.x,
        wa.position.y,
        wa.size.width as i32,
        wa.size.height as i32,
    )
}

/// 所有屏的工作区（物理像素）。
///
/// **保证非空**：中途拔屏 / 驱动异常时 `available_monitors()` 可能是空的，
/// 而调用方（限位、吸附判定）一旦拿到空集合就会退回「不限制」，
/// 窗口会被拖到看不见的地方。
pub fn work_areas(win: &WebviewWindow) -> Vec<Rect> {
    let mut v: Vec<Rect> = win
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(monitor_to_rect)
        .collect();
    if v.is_empty() {
        if let Ok(Some(m)) = win.primary_monitor() {
            v.push(monitor_to_rect(&m));
        }
    }
    if v.is_empty() {
        if let Some(b) = bounds(win) {
            v.push(Rect::new(0, 0, b.right().max(1280), b.bottom().max(720)));
        } else {
            v.push(Rect::new(0, 0, 1280, 720));
        }
    }
    v
}

/// 与 `rect` 交集最大的那块屏的工作区（等价 Electron 的 `getDisplayMatching`）。
pub fn work_area_for(win: &WebviewWindow, rect: Rect) -> Rect {
    let areas = work_areas(win);
    geometry::pick_work_area(rect, &areas).unwrap_or(rect)
}

/// 光标位置（物理像素，桌面全局坐标）。
///
/// 取不到时返回 `NaN` —— [`geometry::cursor_in_rect_padded`] 会把 `NaN` 当作
/// 「还在窗口上」，宁可少收一次也不要在拿不到坐标时误收起。
fn cursor(win: &WebviewWindow) -> (f64, f64) {
    win.cursor_position()
        .map(|p| (p.x, p.y))
        .unwrap_or((f64::NAN, f64::NAN))
}

fn cursor_i32(win: &WebviewWindow) -> (i32, i32) {
    let (x, y) = cursor(win);
    (
        if x.is_finite() { x.round() as i32 } else { 0 },
        if y.is_finite() { y.round() as i32 } else { 0 },
    )
}

/// 应用一组 bounds。
///
/// **先尺寸后位置**：贴着屏幕边缘缩窗口时 Windows 可能顺手把窗口挪一下，
/// 后设位置才能盖过这个自动调整（顺序反了会看到窗口"跳一下"）。
pub fn apply_rect(win: &WebviewWindow, r: Rect) {
    let _ = win.set_size(PhysicalSize::new(r.w.max(1) as u32, r.h.max(1) as u32));
    let _ = win.set_position(PhysicalPosition::new(r.x, r.y));
}

/// 向主窗口推一个事件（`win.emit` 只发给这个 webview，不广播）。
pub fn emit(app: &AppHandle, event: &str, payload: impl serde::Serialize + Clone) {
    if let Some(win) = main_window(app) {
        let _ = win.emit(event, payload);
    }
}

// ---------------------------------------------------------------------------
// 显隐 / 唤回
// ---------------------------------------------------------------------------

/// 最小化到托盘（标题栏最小化按钮）
pub fn hide_to_tray(app: &AppHandle) {
    if let Some(win) = main_window(app) {
        let _ = win.hide();
    }
}

/// 关窗 = 隐藏到托盘（由 `CloseRequested` 里 prevent_close 后再调它）
pub fn close_window(app: &AppHandle) {
    if let Some(win) = main_window(app) {
        let _ = win.close();
    }
}

/// 从托盘唤回主窗口。
///
/// 迷你/贴边状态下窗口本来就"可见"（细条），`show()` 没效果 ——
/// 必须先退出迷你模式展开成完整窗口，并推一次状态，
/// 否则渲染层残留的 `mini` / `dock-hidden` 类会把完整窗口画成透明卡片。
pub fn reveal_main(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    if win_state(app).mini {
        set_pin_mini(app, false);
        emit_mini_state(app);
    }
    if !win.is_visible().unwrap_or(false) {
        let _ = win.show();
    }
    // Windows 前台锁可能拒绝后台进程的 SetForegroundWindow：
    // 先把窗口顶到 topmost 层强制盖过其他应用，片刻后再放回来。
    let _ = win.set_always_on_top(true);
    let _ = win.set_focus();

    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(FOREGROUND_RETRY_MS));
        let Some(win) = main_window(&handle) else { return };
        // 期间用户可能又进了迷你模式 —— 那时 topmost 是它的正常状态，别关掉
        if !win_state(&handle).mini {
            let _ = win.set_always_on_top(false);
        }
        if !win.is_focused().unwrap_or(true) {
            // 实在抢不到前台就闪一下任务栏提示
            let _ = win.request_user_attention(Some(tauri::UserAttentionType::Informational));
        }
    });
}

/// 托盘双击：显示 / 隐藏切换。
pub fn toggle_main(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    if win_state(app).mini {
        // 迷你小窗不该被双击托盘"藏起来"，直接展开主窗口
        reveal_main(app);
    } else if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 把计时器命令转发给渲染层（托盘菜单 / M2 的网关 / 弹窗按钮共用）。
pub fn send_command(app: &AppHandle, cmd: &str) {
    emit(app, "tray:command", cmd);
}

// ---------------------------------------------------------------------------
// 迷你悬浮模式
// ---------------------------------------------------------------------------

/// 进出迷你模式。`on = true` 收缩成只剩倒计时的迷你小窗。
pub fn set_pin_mini(app: &AppHandle, on: bool) {
    let Some(win) = main_window(app) else { return };
    // 状态没变就直接返回（重复点图钉不该反复改窗口属性）
    let changed = edit_win(app, |w| {
        if w.mini == on {
            return false;
        }
        w.mini = on;
        w.drag = None;
        // 进出迷你模式都重置贴边状态，并取消未决的「延时收回」
        w.hide_token = w.hide_token.wrapping_add(1);
        w.dock = None;
        w.dock_hidden = false;
        w.dock_wa = None;
        true
    });
    if !changed {
        return;
    }
    // 让旧拖拽线程立刻退出（它是轮询的，最多再跑一帧）
    DRAG_GENERATION.fetch_add(1, Ordering::SeqCst);

    let m = metrics_for(&win);
    if on {
        let Some(b) = bounds(&win) else { return };
        edit_win(app, |w| w.full = Some(b));
        let target = geometry::mini_from_full(b, &m);
        // 顺序照搬 Electron：先解除尺寸下限（否则缩不到 176 宽），再禁用手动缩放
        let _ = win.set_min_size(None::<PhysicalSize<u32>>);
        let _ = win.set_resizable(false);
        watchdog::timed("apply_rect(进入迷你)", || apply_rect(&win, target));
        let _ = win.set_always_on_top(true);
        // 迷你悬浮视同隐藏窗口：撤下任务栏按钮，只保留小窗与托盘。
        //
        // ⚠ `set_skip_taskbar` 是这里唯一**跨进程**的窗口调用：tao 的实现是
        // `CoCreateInstance(TaskbarList)` + `DeleteTab/AddTab`，也就是同步 COM 到
        // explorer。explorer 忙或正在重启时这条调用会长时间不返回 —— 而它跑在主
        // 线程上，一停就是「界面全死、倒计时照走」。给它登记看门狗标签，真要卡住
        // 日志会直接点名它。
        watchdog::timed("set_skip_taskbar(true)", || {
            let _ = win.set_skip_taskbar(true);
        });
    } else {
        let cur = bounds(&win).unwrap_or(Rect::new(0, 0, m.full_w, m.full_h));
        let saved = win_state(app).full;
        let full = saved.unwrap_or(Rect::new(0, 0, m.full_w, m.full_h));
        let wa = work_area_for(&win, cur);
        let target = geometry::mini_to_full(
            cur,
            full.w.max(m.full_min_w),
            full.h.max(m.full_min_h),
            wa,
        );
        apply_rect(&win, target);
        let _ = win.set_min_size(Some(PhysicalSize::new(
            m.full_min_w as u32,
            m.full_min_h as u32,
        )));
        let _ = win.set_resizable(true);
        let _ = win.set_always_on_top(false);
        watchdog::timed("set_skip_taskbar(false)", || {
            let _ = win.set_skip_taskbar(false);
        });
    }
}

/// 图钉：在迷你 / 完整之间切换，并立刻把状态推给渲染层。
///
/// 两步必须在同一个函数里成对出现 —— 只切窗口不推状态，渲染层残留的 `mini` 类
/// 会把完整窗口画成一张透明卡片。
pub fn toggle_pin(app: &AppHandle) {
    let on = !win_state(app).mini;
    set_pin_mini(app, on);
    emit_mini_state(app);
}

/// 迷你/贴边状态变化后必须同步给渲染层。
pub fn emit_mini_state(app: &AppHandle) {
    emit(app, "state:pin-changed", win_state(app).mini);
    send_dock_state(app);
}

/// 推贴边状态。`edge` 用 `null` 表示没贴边（渲染层按它切 body 类名）。
pub fn send_dock_state(app: &AppHandle) {
    let s = win_state(app);
    emit(
        app,
        "state:dock-changed",
        json!({ "hidden": s.dock_hidden, "edge": s.dock.map(Edge::as_str) }),
    );
}

// ---------------------------------------------------------------------------
// 迷你模式手动拖拽
//
// 原生 `-webkit-app-region: drag` 会吞掉 `:hover` 与鼠标事件（悬停浮出按钮就失效了），
// 所以迷你模式放弃原生拖拽区，改由渲染层 pointerdown 打头，这里轮询光标位置跟随。
// ---------------------------------------------------------------------------

/// 开始跟随光标拖拽。渲染层已经判定「按住并移动超过阈值」，这里只负责跟。
pub fn start_drag(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    if !win_state(app).mini {
        return;
    }
    let Some(b) = bounds(&win) else { return };
    let (cx, cy) = cursor_i32(&win);
    // 基准屏黏滞：起拖时按窗口所在屏定基准
    let wa = work_area_for(&win, b);
    edit_win(app, |w| {
        w.drag = Some(Drag {
            offset: (cx - b.x, cy - b.y),
            size: (b.w, b.h),
            wa,
        });
    });
    let generation = DRAG_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    let handle = app.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(DRAG_POLL_MS));
            // 这一轮拖拽已经结束（或被新的一次取代）→ 线程退出
            if DRAG_GENERATION.load(Ordering::SeqCst) != generation {
                break;
            }
            // 物理左键已经松开 → 本轮拖拽结束。
            //
            // ⚠ 这条兜底是必需的，别删。拖拽的结束信号本来只有渲染层的 `pointerup`，
            // 而 `pointerup` 在几个真实场景下会**丢**：安全桌面 / UAC 提示接管输入、
            // 锁屏或远程桌面切走输入桌面、触摸笔的 pointercancel 没送达、窗口焦点被
            // 别的进程抢走。一旦丢了，这一轮就永远收不到 `end_drag` —— 窗口会以 16ms
            // 的周期一直粘着光标跑，表现就是「主界面所有按钮都点不动、托盘右键也不弹
            // 菜单」（窗口总能追到鼠标那儿去），同时倒计时照常走，看着像整个应用死了。
            // 直接问系统要按键状态，不依赖渲染层。
            if !left_button_down() || started.elapsed() > Duration::from_millis(DRAG_MAX_MS) {
                // 收尾照旧走主线程：`end_drag` 里有窗口 API 与共享状态，
                // 不能在这个轮询线程上直接跑。
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || end_drag(&h));
                break;
            }
            let Some(mut drag) = win_state(&handle).drag else { break };
            let Some(win) = main_window(&handle) else { break };
            let (cx, cy) = cursor_i32(&win);

            // 只有光标真的深入另一块屏才换基准屏，否则接缝处会逐帧翻转限位屏幕
            let areas = work_areas(&win);
            if let Some(here) = geometry::work_area_at_point(cx, cy, &areas) {
                if here != drag.wa {
                    let m = metrics_for(&win);
                    if geometry::beyond_work_area((cx, cy), drag.wa, m.drag_switch_margin) {
                        drag.wa = here;
                        edit_win(&handle, |w| {
                            if let Some(d) = w.drag.as_mut() {
                                d.wa = here;
                            }
                        });
                    }
                }
            }

            let (nx, ny) = geometry::drag_position((cx, cy), drag.offset, drag.size, drag.wa);
            let _ = win.set_position(PhysicalPosition::new(nx, ny));
        }
    });
}

/// `GetAsyncKeyState` 的返回值 → 「此刻是否按着」。
///
/// 只认**最高位**（`0x8000` ＝ 当前按下）。最低位是"本线程上次调用之后按过没有"，
/// 与拖拽无关 —— 认错这一位就会把"曾经按过"当成"还按着"，兜底等于没做。
///
/// 抽成纯函数是为了能单测这个位运算：写反了就恒假 → 拖拽一开始就被判定"已松手"，
/// 拖不动；或恒真 → 兜底永不触发。
#[cfg(windows)]
fn is_down_from_async_state(raw: i16) -> bool {
    (raw as u16) & 0x8000 != 0
}

#[cfg(windows)]
fn left_button_down() -> bool {
    /// `VK_LBUTTON` 恒为 `0x01`（Win32 稳定 ABI）。直接写字面量可以避开绑定 crate
    /// 之间 `VIRTUAL_KEY` 是 newtype 还是裸 `u16` 的类型漂移。
    const VK_LBUTTON: i32 = 0x01;
    is_down_from_async_state(unsafe { GetAsyncKeyState(VK_LBUTTON) })
}

/// 非 Windows 平台没有这个兜底（本项目只发 Windows 版）。
/// 返回 `true` ＝ "当作还按着"，也就是把兜底关掉，行为退回改动前。
#[cfg(not(windows))]
fn left_button_down() -> bool {
    true
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(vkey: i32) -> i16;
}

/// 结束拖拽线程（不动贴边状态）。
pub fn stop_drag(app: &AppHandle) {
    DRAG_GENERATION.fetch_add(1, Ordering::SeqCst);
    edit_win(app, |w| w.drag = None);
}

/// 松手：判定吸附。命中就收起成细条，否则恢复成普通迷你小窗。
pub fn end_drag(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    // stop_drag 会清掉 drag，先取出本次拖拽的基准屏
    let base_wa = win_state(app).drag.map(|d| d.wa);
    stop_drag(app);
    if !win_state(app).mini {
        return;
    }
    let Some(b) = bounds(&win) else { return };
    let wa = base_wa.unwrap_or_else(|| work_area_for(&win, b));
    let m = metrics_for(&win);

    if let Some(edge) = geometry::detect_edge(b, wa, &m) {
        edit_win(app, |w| {
            w.hide_token = w.hide_token.wrapping_add(1);
            w.dock = Some(edge);
            w.dock_hidden = true;
            // 快照：后续滑出/收回的几何不再重新匹配屏幕
            w.dock_wa = Some(wa);
        });
        apply_dock_geometry(app);
    } else {
        // 离边缘较远：取消吸附，恢复普通迷你小窗（从细条拖离边缘时也要恢复尺寸）
        edit_win(app, |w| {
            w.hide_token = w.hide_token.wrapping_add(1);
            w.dock = None;
            w.dock_hidden = false;
            w.dock_wa = None;
        });
        apply_rect(&win, geometry::mini_resize_into(b, wa, &m));
        send_dock_state(app);
    }
}

// ---------------------------------------------------------------------------
// 贴边隐藏
// ---------------------------------------------------------------------------

/// 按当前贴边状态摆放窗口（收起 = 细条，展开 = 迷你小窗）。
pub fn apply_dock_geometry(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    let s = win_state(app);
    if !s.mini {
        return;
    }
    let Some(edge) = s.dock else { return };
    let Some(b) = bounds(&win) else { return };
    // 优先用吸附瞬间快照的工作区；快照失效（拔掉副屏等）才按窗口当前位置重新匹配
    let wa = s.dock_wa.unwrap_or_else(|| work_area_for(&win, b));
    let m = metrics_for(&win);
    let target = geometry::dock_geometry(edge, b, wa, s.dock_hidden, &m);
    // 贴边滑出/收回要改窗口尺寸 → 会走到 WebView2 的 put_Bounds。
    // 登记标签，卡住时能区分是「移动」还是「改尺寸」这一步。
    watchdog::timed("apply_rect(贴边)", || apply_rect(&win, target));
    send_dock_state(app);
}

/// 滑出细条（渲染层 `pointerenter`）。
pub fn reveal_from_dock(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    let s = win_state(app);
    if !s.mini || s.dock.is_none() || !s.dock_hidden {
        return;
    }
    // 细条只有几个像素，渲染层的 pointerenter 可能是误报：
    // 校验光标确实落在细条附近才滑出，否则忽略
    let Some(b) = bounds(&win) else { return };
    if !geometry::cursor_in_rect_padded(cursor(&win), b, DOCK_REVEAL_PAD) {
        return;
    }
    edit_win(app, |w| {
        w.hide_token = w.hide_token.wrapping_add(1);
        w.dock_hidden = false;
    });
    apply_dock_geometry(app);
}

/// 延时收回成细条（渲染层 `pointerleave`）。
pub fn schedule_dock_hide(app: &AppHandle, delay_ms: u64) {
    let token = edit_win(app, |w| {
        w.hide_token = w.hide_token.wrapping_add(1);
        w.hide_token
    });
    spawn_dock_hide(app.clone(), delay_ms, token, DOCK_HIDE_MAX_RETRY);
}

/// [`schedule_dock_hide`] 的执行体。拆出来只为把「重试额度」变成参数 ——
/// 重试**必须有上限**，理由见 [`DOCK_HIDE_MAX_RETRY`]。
fn spawn_dock_hide(handle: AppHandle, delay_ms: u64, token: u64, retries_left: u32) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(delay_ms));
        {
            let s = win_state(&handle);
            // 期间被取消 / 重排了，或者状态已经不需要收回
            if s.hide_token != token || !s.mini || s.dock.is_none() || s.dock_hidden {
                return;
            }
        }
        let Some(win) = main_window(&handle) else { return };
        let Some(b) = bounds(&win) else { return };
        // 收回前校验光标真实位置：窗口展开/收起瞬间 DOM 的 pointerleave 可能误报
        // （命中测试变化），光标其实还在窗口上。这时延后重试，避免"弹出又立刻收回"
        // 的抖动，也避免收回后光标恰在细条上引发的循环。
        if retries_left > 0 && geometry::cursor_in_rect_padded(cursor(&win), b, DOCK_HIDE_PAD) {
            spawn_dock_hide(handle.clone(), DOCK_HIDE_RETRY_MS, token, retries_left - 1);
            return;
        }
        // 额度用完还认为「光标在窗口上」→ 照常收回。
        // 收回是**自纠正**的：光标真在细条上会立刻再 pointerenter 滑出来（细条本来
        // 就是为悬停设计的），所以这里宁可收回来，也绝不能无上限地每 300ms 起一条
        // 新线程。
        // 重试期间可能又被取消 / 又滑出了，落地前再确认一次
        let proceed = edit_win(&handle, |w| {
            if w.hide_token != token || !w.mini || w.dock.is_none() || w.dock_hidden {
                return false;
            }
            w.dock_hidden = true;
            true
        });
        if !proceed {
            return;
        }
        apply_dock_geometry(&handle);
    });
}

#[cfg(all(test, windows))]
mod button_tests {
    use super::is_down_from_async_state;

    #[test]
    fn async_state_only_reads_the_high_bit() {
        // 最高位（0x8000）= 此刻按下；最低位 = 本线程"上次调用之后按过没有"，与拖拽无关。
        assert!(!is_down_from_async_state(0), "全 0 = 没按");
        // 只有最低位：曾被记录过但**已经松开** —— 认错这一位会让兜底失去意义
        assert!(!is_down_from_async_state(1), "只看最低位 → 必须判为没按");
        assert!(!is_down_from_async_state(i16::MAX), "0x7FFF 最高位是 0");
        // 最高位为 1 的两种形态：0x8000（刚按下）与 0x8001（按下 + 用过）
        assert!(is_down_from_async_state(i16::MIN), "0x8000 = 按着");
        assert!(is_down_from_async_state(-32767), "0x8001 = 按着");
    }
}
