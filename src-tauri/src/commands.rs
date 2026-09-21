//! Tauri 命令 —— 渲染层 `window.pomodoro.*` 的落地点。
//!
//! 这一层刻意"薄"：只做参数校验 + 转调，逻辑都在 [`crate::window`] / [`crate::tray`] /
//! [`crate::about`] / [`crate::gateway`] / [`crate::popup`] / [`crate::interaction`] 里。
//!
//! # 命令名与 `preload.js` 的对应关系
//!
//! Electron 版用 `ipcMain.on('window:close')` 这种带冒号的频道名；Tauri 的命令名
//! 只能是合法的 Rust 标识符，所以统一改成下划线，映射写在 `bridge.js` 里。
//! **改命令名要同步改 `bridge.js`**，否则渲染层按旧名字调用会静默失败
//! （bridge 会吞掉 invoke 的 Promise 拒绝，只打一条 console 警告）。
//!
//! # 参数必须包成对象
//!
//! Electron 的 `ipcRenderer.send('notify:close', id)` 传的是裸值；Tauri 的 `invoke`
//! 只接受**对象**。所以 bridge.js 里一律包一层 `{ id: ... }` / `{ payload: ... }`
//! —— 少包一层就是 `invalid args`，而且只进 console。
//!
//! # 「关闭弹窗」为什么要拿 `WebviewWindow`
//!
//! 弹窗页发出的回应只能关掉**它自己那扇窗**。用「当前弹窗」这个全局指针会有 bug：
//! 旧弹窗的自动关闭定时器晚触发时，会把已经换成新弹窗的那扇关掉。
//! Tauri 命令可以直接注入 `WebviewWindow`（就是发起调用的那个），正合用。
//!
//! # 收尾语义（别合并）
//!
//! - `notify_close`（弹窗右上角 ×）= **交给终端** → `dismissed`，`action=null`
//! - `interaction_hold`（底部「暂时收起」）= **挂起** → 只置 `state='held'`，
//!   **不 resolve、不停兜底表**
//! - `interaction_respond` = 用户真的作答了 → `decidedBy='user'`

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::about;
use crate::state::{lock, AppState, TrayPatch};
use crate::{gateway, interaction, popup, window};

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
// 通知 / 交互弹窗
// ---------------------------------------------------------------------------

/// 渲染层主动弹一条通知（阶段结束提醒等）。
///
/// 与网关那条路走的是同一个 [`popup::show`]，所以「新弹窗顶掉旧交互窗」的
/// 单窗口策略对两边一致。
#[tauri::command]
pub fn notify_show(app: AppHandle, payload: Value) {
    if let Err(e) = popup::show(&app, payload) {
        eprintln!("[popup] 渲染层请求弹窗失败: {e}");
    }
}

/// 弹窗页按 id 取回完整 payload。
///
/// ⚠ 这是**会被 await 的** invoke。它能不能回来，等于「弹窗窗口 → bridge.js →
/// 渲染层 → Rust」这条链路通不通 —— 自检里专门盯着 `popup::payload_fetch_count()`。
#[tauri::command]
pub fn notify_payload(app: AppHandle, id: String) -> Value {
    popup::get_payload(&app, &id).unwrap_or(Value::Null)
}

/// 弹窗页实测内容高度后回传 → 夹取尺寸、靠右上角、首次显示。
#[tauri::command]
pub fn notify_resize(app: AppHandle, win: WebviewWindow, width: f64, height: f64) {
    popup::resize(&app, &win, width, height);
}

/// 弹窗右上角 ×（`closeNotify`）＝**交给终端**。
///
/// 把该交互按 `dismissed` 收尾（`action = null` → 调用方回退宿主原生询问，
/// 不替用户做决定）。注意这与「暂时收起」是两种不同的收尾，见 `interaction_hold`。
#[tauri::command]
pub fn notify_close(app: AppHandle, win: WebviewWindow, id: Option<String>) {
    match id.as_deref() {
        Some(id) if !id.is_empty() => {
            interaction::resolve(&app, id, interaction::Decision::dismissed());
        }
        // 没带 id：把当前所有挂起的都按 dismissed 收走（等价 Electron 的 dismissPending）
        _ => interaction::dismiss_all(&app, true),
    }
    // 按请求来源窗口精确关闭：被顶掉的旧弹窗的自动关闭定时器晚触发时，
    // 不能误关当前正在展示的新弹窗。
    let _ = win.destroy();
    popup::forget(&app, win.label());
    interaction::refresh_entry_points(&app);
}

/// 交互弹窗（ask / permission / custom）用户在弹窗内决策 →
/// 网关 resolve → 长轮询的那个 hook 请求拿到结果。
#[tauri::command]
pub fn interaction_respond(app: AppHandle, win: WebviewWindow, payload: Value) {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let answers = payload.get("answers").cloned().unwrap_or_else(|| json!({}));

    let resolved = match (&id.is_empty(), action.clone()) {
        (false, Some(action)) => {
            interaction::resolve(&app, &id, interaction::Decision::user(action, answers, text))
        }
        // action 为 null（例如纯通知的兜底路径）不算用户决定
        _ => false,
    };

    // 非网关弹窗（阶段结束提醒等）也带按钮：点「进入下一阶段」直接开跑下一阶段
    if !resolved && action.as_deref() == Some("start-next") {
        window::send_command(&app, "start-next");
    }

    // 由弹窗页发出的回应一律收起该弹窗：网关那边可能已经 resolve（主进程关窗），
    // 本地弹窗（没有挂起交互）也得关，否则只能等自动关闭定时器。
    let _ = win.destroy();
    popup::forget(&app, win.label());
    interaction::refresh_entry_points(&app);
}

/// 「暂时收起」：只收起窗口，**不 resolve** —— 网关那边 HTTP 请求继续挂着，
/// 等用户从托盘 / 主界面提示条重新唤回再作答。
///
/// ⚠ 这里**绝不能**顺手调 `notify_close` 那套收尾，否则就退化成「交给终端」了。
/// 关窗由这里做（渲染层的 `hold()` 刻意不调 `dismiss()`）。
#[tauri::command]
pub fn interaction_hold(app: AppHandle, win: WebviewWindow, id: Option<String>) {
    if let Some(id) = id.as_deref().filter(|s| !s.is_empty()) {
        interaction::hold(&app, id);
    }
    let _ = win.destroy();
    popup::forget(&app, win.label());
    interaction::refresh_entry_points(&app);
}

/// 从托盘 / 主界面提示条唤回收起的确认（不带 id / 空 id 时取最早收起的那条）。
///
/// 与托盘的两个入口走**同一个** [`interaction::reopen_held`] —— 逻辑只有一份，
/// 不会出现"某个入口忘了关旧窗 / 忘了刷新列表"。
#[tauri::command]
pub fn interaction_reopen(app: AppHandle, id: Option<String>) {
    let wanted = id.as_deref().filter(|s| !s.is_empty());
    interaction::reopen_held(&app, wanted);
}

/// 旧接口兼容：只有 action 的确认弹窗。
#[tauri::command]
pub fn respond_confirm(app: AppHandle, win: WebviewWindow, id: String, action: String) {
    if id.is_empty() || action.is_empty() {
        return;
    }
    let resolved = interaction::resolve(
        &app,
        &id,
        interaction::Decision::user(action, json!({}), String::new()),
    );
    if resolved {
        let _ = win.destroy();
        popup::forget(&app, win.label());
    }
}

// ---------------------------------------------------------------------------
// Agent 网关
// ---------------------------------------------------------------------------

/// 网关状态：回推开关 / 端口 / hook 路径 / 活动计数。
///
/// 渲染层是乐观翻转（先改 UI 再等回推），所以这里必须在**状态真的变了之后**再推，
/// 否则会看到"点了没反应"或者"显示运行中但其实是假的"。
#[tauri::command]
pub fn gateway_get_state(app: AppHandle) {
    gateway::push_gateway_state(&app);
}

/// 网关开关。持久化在 `<userData>/config.json` 的 `gatewayEnabled`。
#[tauri::command]
pub fn gateway_set_enabled(app: AppHandle, enabled: bool) {
    // 参数名不能带下划线前缀：tauri 宏会把参数名转成 camelCase，
    // `_enabled` 转出来的 JS 键名不是 `enabled`，渲染层传不过来。
    crate::config::save_patch(json!({ "gatewayEnabled": enabled }));
    if enabled {
        if let Err(e) = gateway::start(&app) {
            eprintln!("[gateway] 启动失败: {e}");
        }
    } else {
        gateway::stop(&app);
    }
    gateway::push_gateway_state(&app);
}

/// 待处理的确认列表（**只含「暂时收起」的那批**）——主窗口提示条用它渲染入口。
#[tauri::command]
pub fn pending_get_held(app: AppHandle) {
    if crate::smoke::enabled() {
        crate::smoke::on_pending_get_held(&app);
    }
    window::emit(&app, "state:pending-held", interaction::held_chip_items(&app));
}

/// 一键安装 hook：M3 才接。明确回失败 + 原因，别让用户以为"装上了"。
#[tauri::command]
pub fn hook_install(agent: String, clean: bool) -> Value {
    // 参数名不能带下划线前缀（tauri 宏的参数名会转 camelCase，键名对不上）。
    let _ = clean;
    json!({
        "ok": false,
        "agent": agent,
        "message": "Rust 版还没接上「一键安装 hook」（计划在 M3）。\
                    要现在就配，可以在设置抽屉里点「复制配置」手动粘贴，\
                    或继续用 Electron 版安装。",
        // 字段与 Electron 版 `hook:install` 的返回保持一致（渲染层目前没读 files，
        // 但 preload 的注释里承诺了这四个，别让两边形状漂移）
        "files": [],
        "command": "",
        "log": "",
    })
}
