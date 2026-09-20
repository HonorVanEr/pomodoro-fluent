//! Tauri 命令 —— 渲染层 `window.pomodoro.*` 的落地点。
//!
//! 这一层刻意"薄"：只做参数校验 + 转调，逻辑都在 [`crate::window`] / [`crate::tray`] /
//! [`crate::about`] 里。
//!
//! # 命令名与 `preload.js` 的对应关系
//!
//! Electron 版用 `ipcMain.on('window:close')` 这种带冒号的频道名；Tauri 的命令名
//! 只能是合法的 Rust 标识符，所以统一改成下划线，映射写在 `bridge.js` 里。
//! **改命令名要同步改 `bridge.js`**，否则渲染层按旧名字调用会静默失败
//! （bridge 会吞掉 invoke 的 Promise 拒绝，只打一条 console 警告）。
//!
//! # M2 的占位
//!
//! 网关 / 待处理确认 / 一键安装属于 M2。这里先给**诚实的空实现**：
//! - 网关状态一律回「已停用」（不是假装运行中）；
//! - 待处理确认一律回空列表；
//! - 一键安装明确回「本版本还没接上」，而不是假装成功。
//!
//! 这样渲染层不会卡在「检测中…」，也不会显示假的成功。

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::about;
use crate::state::{lock, AppState, TrayPatch};
use crate::window;

// ---------------------------------------------------------------------------
// 窗口控制
// ---------------------------------------------------------------------------

/// 标题栏最小化按钮 → 隐藏到托盘
#[tauri::command]
pub fn window_minimize_to_tray(app: AppHandle) {
    window::hide_to_tray(&app);
}

/// 标题栏关闭按钮 → 触发 `CloseRequested`，最终走"隐藏到托盘"（见 main.rs）
#[tauri::command]
pub fn window_close(app: AppHandle) {
    window::close_window(&app);
}

/// 图钉按钮：进出迷你悬浮模式
#[tauri::command]
pub fn window_toggle_pin(app: AppHandle) {
    window::toggle_pin(&app);
}

/// 迷你模式开始跟随光标拖拽
#[tauri::command]
pub fn window_drag_start(app: AppHandle) {
    window::start_drag(&app);
}

/// 松手：判定贴边吸附
#[tauri::command]
pub fn window_drag_end(app: AppHandle) {
    window::end_drag(&app);
}

/// 光标移入贴边细条 → 滑出
#[tauri::command]
pub fn mini_dock_reveal(app: AppHandle) {
    window::reveal_from_dock(&app);
}

/// 光标移开窗口 → 延时收回成细条（350ms，与 Electron 版一致）
#[tauri::command]
pub fn mini_dock_hide_request(app: AppHandle) {
    // 只有真的处于「贴边且展开」时才排程，其余情况直接忽略（省一个睡眠线程）
    {
        let st = app.state::<AppState>();
        let w = lock(&st.win);
        if !w.mini || w.dock.is_none() || w.dock_hidden {
            return;
        }
    }
    window::schedule_dock_hide(&app, 350);
}

// ---------------------------------------------------------------------------
// 托盘状态同步
// ---------------------------------------------------------------------------

/// 渲染层每次 render 上报的计时快照。**去重后**才刷新图标 / tooltip / 菜单。
#[tauri::command]
pub fn tray_update(app: AppHandle, patch: TrayPatch) {
    if crate::smoke::enabled() {
        crate::smoke::on_tray_update(
            &app,
            patch.phase.as_deref().unwrap_or(""),
            patch.time_left_text.as_deref().unwrap_or(""),
            patch.running.unwrap_or(false),
        );
    }
    crate::tray::sync_from_patch(&app, &patch);
}

// ---------------------------------------------------------------------------
// 关于 / 检查更新 / 外链
// ---------------------------------------------------------------------------

/// 版本与平台信息。渲染层只在首次打开设置抽屉时调一次。
#[tauri::command]
pub fn app_info(app: AppHandle) -> Value {
    about::app_info(&app)
}

/// 检查更新 —— **唯一会联网的命令**，只在用户点「检查更新」时被调用。
///
/// 必须 `async` + `spawn_blocking`：`minreq` 是同步阻塞的，最长会等 10s，
/// 直接在命令线程上跑会把 IPC 卡住（窗口拖拽也会跟着一顿一顿的）。
#[tauri::command]
pub async fn app_check_update(app: AppHandle) -> Value {
    let current = app.package_info().version.to_string();
    match tauri::async_runtime::spawn_blocking(move || about::check_update(&current)).await {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": format!("检查更新任务异常：{e}") }),
    }
}

/// 用系统浏览器打开外链（只放行 http/https）
#[tauri::command]
pub fn app_open_external(url: String) {
    if !about::is_allowed_external(&url) {
        eprintln!("[open-external] 拒绝非 http(s) 链接: {url}");
        return;
    }
    if !about::open_url(&url) {
        eprintln!("[open-external] ShellExecute 失败: {url}");
    }
}

// ---------------------------------------------------------------------------
// M2 占位：网关 / 待处理确认 / 一键安装
// ---------------------------------------------------------------------------

/// 网关状态：M1 恒为「已停用」。推一次让渲染层把「检测中…」换成确定文案。
#[tauri::command]
pub fn gateway_get_state(app: AppHandle) {
    push_gateway_disabled(&app);
}

/// 网关开关：M1 不做任何事，只把「已停用」再推一次 ——
/// 渲染层是乐观翻转（先改 UI 再等回推），推回来就会自动弹回关闭，
/// 用户看到的是"点了没反应"，而不是"显示运行中但其实是假的"。
#[tauri::command]
pub fn gateway_set_enabled(app: AppHandle, enabled: bool) {
    // 参数名不能带下划线前缀：tauri 宏会把参数名转成 camelCase，
    // `_enabled` 转出来的 JS 键名不是 `enabled`，渲染层传不过来。
    let _ = enabled;
    push_gateway_disabled(&app);
}

fn push_gateway_disabled(app: &AppHandle) {
    window::emit(
        app,
        "state:gateway",
        json!({ "enabled": false, "port": null, "hookPath": null, "pluginPath": null }),
    );
}

/// 待处理的确认：M1 没有可收起的东西，回空列表。
#[tauri::command]
pub fn pending_get_held(app: AppHandle) {
    if crate::smoke::enabled() {
        crate::smoke::on_pending_get_held(&app);
    }
    window::emit(&app, "state:pending-held", json!([]));
}

/// 一键安装 hook：M2 才接。明确回失败 + 原因，别让用户以为"装上了"。
#[tauri::command]
pub fn hook_install(agent: String, clean: bool) -> Value {
    // 参数名不能带下划线前缀（tauri 宏的参数名会转 camelCase，键名对不上）。
    let _ = clean;
    json!({
        "ok": false,
        "agent": agent,
        "message": "Rust 版还没接上「一键安装 hook」（计划在 M2）。\
                    要现在就配，可以在设置抽屉里点「复制配置」手动粘贴，\
                    或继续用 Electron 版安装。",
        // 字段与 Electron 版 `hook:install` 的返回保持一致（渲染层目前没读 files，
        // 但 preload 的注释里承诺了这四个，别让两边形状漂移）
        "files": [],
        "command": "",
        "log": "",
    })
}
