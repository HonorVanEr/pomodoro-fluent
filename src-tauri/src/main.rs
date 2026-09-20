// GUI subsystem：不要黑框。hook 侧是另一个 crate，那边不加这行。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 番茄钟（Rust / Tauri 版）。
//!
//! 目标：与 Electron 版**行为等价**，但底层换成 Rust + 系统 WebView2。
//! 分工：
//! - [`geometry`] —— 纯几何（迷你、贴边、拖拽限位），带单元测试
//! - [`window`]   —— 窗口读写与行为
//! - [`tray`]     —— 托盘图标染色 / 菜单 / 状态同步
//! - [`about`]    —— 版本信息、检查更新（唯一的联网点）、打开外链
//! - [`commands`] —— 渲染层 IPC 的落地点（薄）
//! - [`state`]    —— 共享状态
//! - `bridge.js`  —— 注入到渲染层的垫片，把 `window.pomodoro.*` 接到 invoke / event
//!
//! **`renderer/` 一行不改**是 M1 的硬要求：所有适配都在 `bridge.js` 和上面这些模块里。
//!
//! ⚠ 别在这里加任何"启动时联网 / 定时上报 / 后台检查更新"的东西 —— 应用唯一的
//! 网络出口是用户在设置里点「检查更新」（见 [`about`]）。

mod about;
mod commands;
mod geometry;
mod smoke;
mod state;
mod tray;
mod window;

use tauri::{PhysicalPosition, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::geometry::{FULL_H_DIP, FULL_MIN_H_DIP, FULL_MIN_W_DIP, FULL_W_DIP};
use crate::state::AppState;

/// 注入渲染层的垫片（`window.pomodoro.*` → Tauri invoke / event）。
///
/// 用 `include_str!` 而不是运行时读文件：打包后它就是 exe 里的一段常量，
/// 不会出现"找不到 bridge.js"的部署问题。
const BRIDGE_JS: &str = include_str!("bridge.js");

fn main() {
    // 自检模式（POMODORO_SMOKE=1）：必须在这里初始化，且要早于建窗口 ——
    // 渲染层一加载就会打启动期的那两个 invoke。见 src/smoke.rs。
    smoke::init();

    tauri::Builder::default()
        // 单实例必须**第一个**注册：重复启动时它会直接把进程结束掉，
        // 并回调这里把已有实例的窗口唤到前台 —— 等价 Electron 的 second-instance。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            window::reveal_main(app);
        }))
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::window_minimize_to_tray,
            commands::window_close,
            commands::window_toggle_pin,
            commands::window_drag_start,
            commands::window_drag_end,
            commands::mini_dock_reveal,
            commands::mini_dock_hide_request,
            commands::tray_update,
            commands::app_info,
            commands::app_check_update,
            commands::app_open_external,
            commands::gateway_get_state,
            commands::gateway_set_enabled,
            commands::pending_get_held,
            commands::hook_install,
        ])
        .setup(|app| {
            smoke::step("setup 进入");
            let handle = app.handle().clone();
            create_main_window(&handle)?;
            smoke::step("主窗口已建");
            tray::create(&handle)?;
            smoke::step("托盘已建");
            // 自检模式：起一个只看窗口标题的取证线程（见 smoke::watch_title）
            smoke::watch_title(&handle);
            smoke::step("setup 返回");
            Ok(())
        })
        .on_window_event(|win, event| {
            // 关窗 = 隐藏到托盘，不退出（沿用 Electron 版行为）。
            // 托盘的「退出」走 app.exit(0)，不经过这里，所以不会被拦下。
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = win.hide();
            }
        })
        // 注意：`Builder::run` **只接受 Context**，不接受闭包。
        // 想挂 RunEvent 回调必须先 `.build(context)` 拿到 `App`，再 `App::run(闭包)`。
        // （直接 `.run(|app, event| ...)` 会报 expected `Context`, found closure）
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|_app, event| match event {
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                // 常驻托盘：窗口全关也不退出。
                // 但托盘「退出」调的是 `app.exit(0)`，那时 `code` 是 `Some(0)`，要放行。
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            // M2: RunEvent::Exit => crate::gateway::stop(app),
            _ => {}
        });
}

/// 创建主窗口。
///
/// 刻意**不在 `tauri.conf.json` 里声明窗口**，而是代码里建：
/// 只有这样才拿得到 `initialization_script`（注入 bridge.js 的唯一入口），
/// 也才能把尺寸/位置/无边框这些一次性配齐，不用在两处维护同一份参数。
fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let mut builder = WebviewWindowBuilder::new(app, window::MAIN, WebviewUrl::App("index.html".into()))
        .title("番茄钟")
        // 构建器上的尺寸是**逻辑像素**（DIP），与 Electron 的 BrowserWindow 语义一致
        .inner_size(FULL_W_DIP as f64, FULL_H_DIP as f64)
        .min_inner_size(FULL_MIN_W_DIP as f64, FULL_MIN_H_DIP as f64)
        .resizable(true)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        // 先隐藏，摆好位置再显示 —— 否则会看到窗口先从屏幕左上方跳一下
        .visible(false)
        .initialization_script(BRIDGE_JS);

    // 自检模式下再挂一个诊断脚本，把页面侧的报错映射到窗口标题（见 smoke.rs）。
    // 正常运行时 POMODORO_SMOKE 没设，这段脚本不会进页面。
    if smoke::enabled() {
        builder = builder.initialization_script(smoke::DIAG_JS);
    }

    let win = builder.build()?;
    smoke::step("webview 构建完成");

    // 初始摆放：主屏工作区右下角（物理像素，见 geometry 里关于单位的说明）
    if let Some(b) = window::bounds(&win) {
        let m = window::metrics_for(&win);
        let wa = window::work_area_for(&win, b);
        let (x, y) = geometry::initial_position(wa, b.w, b.h, &m);
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
    smoke::step("位置已设");

    let _ = win.show();
    let _ = win.set_focus();
    smoke::step("已 show");
    Ok(())
}
