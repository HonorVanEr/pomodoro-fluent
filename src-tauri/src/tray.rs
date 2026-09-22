//! 系统托盘：按阶段染色的图标、tooltip、菜单。
//!
//! 对应 Electron 版 `main.js` 的 `makeTrayIcon` / `trayIconForPhase` /
//! `createTray` / `rebuildTrayMenu` / `refreshPendingTray` / `tray:update` 那几段。
//!
//! 染色是"亮度当混合权重"的简易着色（不是 HSV 旋转），算法逐行照搬 `main.js`，
//! 所以颜色与原版一致 —— 改的时候两边一起改。
//!
//! # 三条唤回入口之一 / 之二
//!
//! 「暂时收起」的交互有三条唤回路径（① 主窗口提示条；② 托盘左键单击；③ 托盘右键
//! 子菜单）。这里负责 ② 和 ③，标签拼法必须与 `interaction::menu_label` 一致 ——
//! 那一条是主窗口提示条和托盘共用的唯一实现。
//!
//! ⚠ 左键单击**只在真有 held 时才动作**：Windows 上双击会先触发一次 click，
//! 若单击去切窗口、双击也切窗口，就会来回抵消。**别把单击改成"切窗口"。**

use std::sync::OnceLock;

use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::state::{lock, AppState, TrayPatch};
use crate::watchdog;
use crate::window;

/// 托盘 id（`app.tray_by_id` 用它取回来改图标/菜单）
pub const TRAY_ID: &str = "main";

/// 各阶段对应的托盘图标颜色（与 `main.js` 的 TRAY_PHASE_COLORS 一致）
const TRAY_PHASE_COLORS: [(&str, [u8; 3]); 4] = [
    ("work", [255, 107, 129]),     // 珊瑚粉
    ("break", [56, 189, 178]),     // 青绿
    ("longBreak", [96, 165, 250]), // 蓝
    ("idle", [203, 213, 225]),     // 灰
];

/// 菜单项 id
const MENU_SHOW: &str = "show";
const MENU_TOGGLE: &str = "toggle";
const MENU_RESET: &str = "reset";
const MENU_SKIP: &str = "skip";
const MENU_QUIT: &str = "quit";
/// 「待处理的确认（N）」—— 点击唤回最早收起的那条
const MENU_PENDING: &str = "pending";
/// 「选择要处理的…」子菜单
const MENU_PENDING_PICK: &str = "pending-pick";
/// 子菜单里每项的前缀 + 交互 id（`on_menu_event` 靠它反解出 id）
const PENDING_ITEM_PREFIX: &str = "pending:";

/// 当前「暂时收起」的交互列表（托盘和主窗口提示条共用同一份数据源）。
fn held(app: &AppHandle) -> Vec<serde_json::Value> {
    crate::interaction::list_held(app)
}

// ---------------------------------------------------------------------------
// 图标染色
// ---------------------------------------------------------------------------

/// 已染色的图标（阶段名 → 图）。染色有成本，缓存复用。
///
/// 内嵌 PNG 而不是运行时读文件：打包后资源都在 exe 里，
/// 少一个"找不到 assets 目录"的失败模式（Electron 版为此写了 ensureAssets 兜底）。
static TINTED: OnceLock<Vec<(&'static str, Image<'static>)>> = OnceLock::new();

fn build_tinted() -> Vec<(&'static str, Image<'static>)> {
    let Ok(base) = Image::from_bytes(include_bytes!("../../assets/tray.png")) else {
        // 图标解不出来就退化成"没有染色图标"，调用方会回退到默认窗口图标
        return Vec::new();
    };
    let (w, h) = (base.width(), base.height());
    let rgba = base.rgba();
    TRAY_PHASE_COLORS
        .iter()
        .map(|(name, rgb)| {
            let mut buf = rgba.to_vec();
            // 对 RGBA 像素：保留 alpha，用亮度决定染多少色。
            // 纯白（lum=1）完全变目标色，纯黑保持黑色 —— 与原版一致。
            for px in buf.chunks_exact_mut(4) {
                if px[3] > 0 {
                    let lum = (px[0] as f32 + px[1] as f32 + px[2] as f32) / 3.0 / 255.0;
                    for i in 0..3 {
                        let v = rgb[i] as f32 * lum + px[i] as f32 * (1.0 - lum);
                        px[i] = v.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
            (*name, Image::new_owned(buf, w, h))
        })
        .collect()
}

fn tinted() -> &'static [(&'static str, Image<'static>)] {
    TINTED.get_or_init(build_tinted)
}

/// 取某个阶段的托盘图标。未知阶段退回 `idle`（与原版 `|| TRAY_PHASE_COLORS.idle` 一致）。
pub fn icon_for_phase(phase: &str) -> Option<Image<'static>> {
    let list = tinted();
    list.iter()
        .find(|(n, _)| *n == phase)
        .or_else(|| list.iter().find(|(n, _)| *n == "idle"))
        .map(|(_, icon)| icon.clone())
}

// ---------------------------------------------------------------------------
// 菜单
// ---------------------------------------------------------------------------

fn build_menu(app: &AppHandle, running: bool) -> tauri::Result<Menu<Wry>> {
    let held = held(app);

    // 顶部：待处理的确认 —— 收起后的弹窗只能从托盘 / 主界面提示条唤回，
    // 所以它必须排在「显示主窗口」前面。
    let pending_top = if held.is_empty() {
        None
    } else {
        Some(MenuItem::with_id(
            app,
            MENU_PENDING,
            format!("待处理的确认（{}）", held.len()),
            true,
            None::<&str>,
        )?)
    };
    // N > 1 时给一个能挑的入口（只有一条时，点上面那项就够了）
    let pending_pick = if held.len() > 1 {
        let mut children: Vec<MenuItem<Wry>> = Vec::with_capacity(held.len());
        for p in &held {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
            children.push(MenuItem::with_id(
                app,
                format!("{PENDING_ITEM_PREFIX}{id}"),
                crate::interaction::menu_label(p),
                true,
                None::<&str>,
            )?);
        }
        let child_refs: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
            children.iter().map(|c| c as _).collect();
        Some(Submenu::with_id_and_items(
            app,
            MENU_PENDING_PICK,
            "选择要处理的…",
            true,
            &child_refs,
        )?)
    } else {
        None
    };
    let pending_sep = if held.is_empty() {
        None
    } else {
        Some(PredefinedMenuItem::separator(app)?)
    };

    let show = MenuItem::with_id(app, MENU_SHOW, "显示主窗口", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let toggle = MenuItem::with_id(app, MENU_TOGGLE, if running { "暂停" } else { "开始" }, true, None::<&str>)?;
    let reset = MenuItem::with_id(app, MENU_RESET, "重置", true, None::<&str>)?;
    let skip = MenuItem::with_id(app, MENU_SKIP, "跳到下一阶段", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;

    // 顺序即 Electron 版 `rebuildTrayMenu` 的模板顺序，一一对应。
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = Vec::new();
    if let Some(p) = &pending_top {
        items.push(p);
    }
    if let Some(s) = &pending_pick {
        items.push(s);
    }
    if let Some(s) = &pending_sep {
        items.push(s);
    }
    items.push(&show);
    items.push(&sep1);
    items.push(&toggle);
    items.push(&reset);
    items.push(&skip);
    items.push(&sep2);
    items.push(&quit);
    Menu::with_items(app, &items)
}

/// 重建并下推菜单。只在运行状态变化 / 待处理集合变化时调 —— 构建开销不小。
pub fn rebuild_menu(app: &AppHandle, running: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(menu) = build_menu(app, running) {
        // 菜单是 HMENU（进程内对象），但 `set_menu` 之后 tray-icon 还要改图标/提示，
        // 一并登记标签，别让托盘卡住时无从下手。
        let _ = watchdog::timed("tray.set_menu", || tray.set_menu(Some(menu)));
    }
}

/// 待处理集合变化 → 刷新托盘两个入口（菜单 + tooltip）。
///
/// 三个唤回入口的刷新统一由 `interaction::refresh_entry_points` 触发，
/// 这里只管托盘这一侧（主窗口提示条是那边 `window::emit` 的事）。
/// **不能在这里拿 `st.tray` 锁再调 `list_held`** —— 会与
/// `state.rs` 里记的加锁顺序（interactions → tray）打反。
pub fn refresh_pending(app: &AppHandle, count: usize) {
    let running = {
        let st = app.state::<AppState>();
        let sync = lock(&st.tray);
        sync.running.unwrap_or(false)
    };
    rebuild_menu(app, running);
    set_pending_tooltip(app, count);
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id.as_ref();
    match id {
        MENU_SHOW => window::reveal_main(app),
        // 菜单文案是"开始/暂停"，但转发给渲染层的命令与 Electron 版一样是 toggle ——
        // 由渲染层按自己的真实状态决定动作，避免两边状态不一致时点反。
        MENU_TOGGLE => window::send_command(app, "toggle"),
        MENU_RESET => window::send_command(app, "reset"),
        MENU_SKIP => window::send_command(app, "skip"),
        // 「待处理的确认（N）」：不指定 id → 唤回最早收起的那条
        MENU_PENDING => {
            crate::interaction::reopen_held(app, None);
        }
        // 「选择要处理的…」的标题项本身不做事
        MENU_PENDING_PICK => {}
        // app.exit 会直接退出，不会触发窗口的 CloseRequested（也就不会被"隐藏到托盘"拦下）
        MENU_QUIT => app.exit(0),
        other => {
            if let Some(id) = other.strip_prefix(PENDING_ITEM_PREFIX) {
                crate::interaction::reopen_held(app, Some(id));
            }
        }
    }
}

fn on_tray_event(tray: &TrayIcon, event: TrayIconEvent) {
    let app = tray.app_handle();
    match event {
        // 双击托盘：显示 / 隐藏主窗口
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => window::toggle_main(app),
        // 左键单击：**只在真有「暂时收起」的确认时才动作** —— 收起后这是最自然的
        // 找法。没有待处理时什么都不做（不动窗口），以免与双击的显示 / 隐藏互相打架
        // （Windows 上双击会先触发一次 click，两边都切窗口就会来回抵消）。
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: tauri::tray::MouseButtonState::Up,
            ..
        } => {
            if !held(app).is_empty() {
                crate::interaction::reopen_held(app, None);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 创建 / 状态同步
// ---------------------------------------------------------------------------

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app, false)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("番茄钟")
        .menu(&menu)
        // 左键不弹菜单，留给「单击唤回」
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event);
    if let Some(icon) = icon_for_phase("work").or_else(|| app.default_window_icon().cloned()) {
        builder = builder.icon(icon);
    }
    if let Err(e) = builder.build(app) {
        // 托盘建不起来不该拦住整个应用（窗口还能用），但要让日志里有痕迹
        eprintln!("[tray] 创建失败: {e}");
        return Err(e);
    }
    Ok(())
}

/// 拼 tooltip：`番茄钟 12:34 · 2 条确认待处理`
/// （与 Electron 版 `refreshPendingTray` 的拼法一致）
fn tooltip_text(time_text: Option<&str>, pending: usize) -> String {
    let base = match time_text {
        Some(t) if !t.is_empty() => format!("番茄钟 {t}"),
        _ => "番茄钟".to_string(),
    };
    if pending > 0 {
        format!("{base} · {pending} 条确认待处理")
    } else {
        base
    }
}

/// 处理渲染层的 `tray:update`：缓存完整计时状态，并按需刷新图标 / tooltip / 菜单。
///
/// 去重很重要：渲染层每秒 render 一次都会调上来，而 `set_icon` / `set_menu`
/// 都是实打实的原生调用。
pub fn sync_from_patch(app: &AppHandle, patch: &TrayPatch) {
    // 缓存完整状态（托盘没就绪时也要缓存 —— 网关 /api/status 读它）
    if patch.phase.is_some() {
        let st = app.state::<AppState>();
        lock(&st.timer).merge(patch);
        // 切回 work ⇒ 新专注期开始，网关的活动计数清零（并推给渲染层）
        if let Some(phase) = patch.phase.as_deref() {
            crate::gateway::observe_timer_state(app, phase);
        }
    }

    let phase = patch.phase.clone().unwrap_or_else(|| "idle".to_string());
    let running = patch.running.unwrap_or(false);
    let time_text = patch.time_left_text.clone().unwrap_or_default();

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let st = app.state::<AppState>();
    let mut sync = lock(&st.tray);

    // 图标颜色：仅阶段变化时更新
    if sync.phase.as_deref() != Some(phase.as_str()) {
        if let Some(icon) = icon_for_phase(&phase).or_else(|| app.default_window_icon().cloned()) {
            // `Shell_NotifyIcon` 是跨进程调用（到 explorer 的托盘窗口）。
            // 这条每秒都可能跑一次，是托盘侧唯一能長時間阻塞主线程的点。
            let _ = watchdog::timed("tray.set_icon", || tray.set_icon(Some(icon)));
        }
        sync.phase = Some(phase);
    }
    // tooltip：仅时间文本变化时更新。⚠ 必须把待处理后缀一起拼上 ——
    // 否则每秒一次的倒计时刷新会把「N 条确认待处理」冲掉，收起入口就"消失"了。
    if sync.time_text.as_deref() != Some(time_text.as_str()) {
        let pending = sync.pending.unwrap_or(0);
        let _ = watchdog::timed("tray.set_tooltip(sync)", || {
            tray.set_tooltip(Some(tooltip_text(Some(time_text.as_str()), pending)))
        });
        sync.time_text = Some(time_text);
    }
    // 菜单：仅运行状态变化时重建
    let running_changed = sync.running != Some(running);
    if running_changed {
        sync.running = Some(running);
    }
    drop(sync);
    if running_changed {
        rebuild_menu(app, running);
    }
}

/// 待处理集合变化时刷新 tooltip 后缀（菜单由 [`refresh_pending`] 一起重建）。
pub fn set_pending_tooltip(app: &AppHandle, count: usize) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let (time_text, changed) = {
        let st = app.state::<AppState>();
        let mut sync = lock(&st.tray);
        let changed = sync.pending != Some(count);
        sync.pending = Some(count);
        (sync.time_text.clone(), changed)
    };
    if !changed {
        return;
    }
    let _ = watchdog::timed("tray.set_tooltip(pending)", || {
        tray.set_tooltip(Some(tooltip_text(time_text.as_deref(), count)))
    });
}
