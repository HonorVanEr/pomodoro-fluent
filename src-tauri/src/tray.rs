//! 系统托盘：按阶段染色的图标、tooltip、菜单。
//!
//! 对应 Electron 版 `main.js` 的 `makeTrayIcon` / `trayIconForPhase` /
//! `createTray` / `rebuildTrayMenu` / `tray:update` 那几段。
//!
//! 染色是"亮度当混合权重"的简易着色（不是 HSV 旋转），算法逐行照搬 `main.js`，
//! 所以颜色与原版一致 —— 改的时候两边一起改。
//!
//! M1 的菜单还没有「待处理的确认」那一节；那段属于 M2（要接上网关才有数据），
//! 位置固定在菜单最上面（[`build_menu`] 里留了注释）。

use std::sync::OnceLock;

use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::state::{lock, AppState, TrayPatch};
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
    // M2：这一节要插在菜单最上面 ——
    //   待处理的确认（N）           → reopenHeld(None)
    //   选择要处理的…（N > 1 时）    → 子菜单，每项 reopenHeld(id)
    //   分隔线
    // 收起后的弹窗只能从托盘 / 主界面提示条唤回，所以它必须排在「显示主窗口」前面。
    let show = MenuItem::with_id(app, MENU_SHOW, "显示主窗口", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let toggle = MenuItem::with_id(app, MENU_TOGGLE, if running { "暂停" } else { "开始" }, true, None::<&str>)?;
    let reset = MenuItem::with_id(app, MENU_RESET, "重置", true, None::<&str>)?;
    let skip = MenuItem::with_id(app, MENU_SKIP, "跳到下一阶段", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    Menu::with_items(app, &[&show, &sep1, &toggle, &reset, &skip, &sep2, &quit])
}

/// 重建并下推菜单。只在运行状态变化 / 待处理数量变化时调 —— 构建开销不小。
pub fn rebuild_menu(app: &AppHandle, running: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(menu) = build_menu(app, running) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id.as_ref() {
        MENU_SHOW => window::reveal_main(app),
        // 菜单文案是"开始/暂停"，但转发给渲染层的命令与 Electron 版一样是 toggle ——
        // 由渲染层按自己的真实状态决定动作，避免两边状态不一致时点反。
        MENU_TOGGLE => window::send_command(app, "toggle"),
        MENU_RESET => window::send_command(app, "reset"),
        MENU_SKIP => window::send_command(app, "skip"),
        // app.exit 会直接退出，不会触发窗口的 CloseRequested（也就不会被"隐藏到托盘"拦下）
        MENU_QUIT => app.exit(0),
        _ => {}
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
        // 左键单击这里**故意留空**：Electron 版用它唤回「暂时收起」的确认
        // （收起后那是最自然的找法），M2 接上网关后补上 —— 且只在真有
        // 待处理确认时才动作。**别把单击改成"切窗口"**：Windows 上双击会先触发
        // 一次 click，两边都切窗口就会来回抵消。
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

/// 处理渲染层的 `tray:update`：缓存完整计时状态，并按需刷新图标 / tooltip / 菜单。
///
/// 去重很重要：渲染层每秒 render 一次都会调上来，而 `set_icon` / `set_menu`
/// 都是实打实的原生调用。
pub fn sync_from_patch(app: &AppHandle, patch: &TrayPatch) {
    // 缓存完整状态（托盘没就绪时也要缓存 —— M2 的网关 /api/status 读它）
    if patch.phase.is_some() {
        let st = app.state::<AppState>();
        // M2: crate::gateway::observe_timer_state(app, &snapshot);
        lock(&st.timer).merge(patch);
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
            let _ = tray.set_icon(Some(icon));
        }
        sync.phase = Some(phase);
    }
    // tooltip：仅时间文本变化时更新
    if sync.time_text.as_deref() != Some(time_text.as_str()) {
        let text = if time_text.is_empty() {
            "番茄钟".to_string()
        } else {
            format!("番茄钟 {time_text}")
        };
        let _ = tray.set_tooltip(Some(text));
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

/// 调整 tooltip 上的"待处理确认"后缀。
///
/// M2 接上网关后调用（待处理数量变化时刷新菜单 + tooltip）。M1 没有待处理数据，
/// 先留在这里，免得 M2 还要回来改 tooltip 的拼法。
#[allow(dead_code)]
pub fn set_pending_tooltip(app: &AppHandle, count: usize) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let base = {
        let st = app.state::<AppState>();
        let sync = lock(&st.tray);
        match &sync.time_text {
            Some(t) if !t.is_empty() => format!("番茄钟 {t}"),
            _ => "番茄钟".to_string(),
        }
    };
    let _ = tray.set_tooltip(Some(if count > 0 {
        format!("{base} · {count} 条确认待处理")
    } else {
        base
    }));
}
