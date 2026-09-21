//! 通知 / 交互弹窗窗口 —— 对应 Electron 版 `main.js` 的 `showNotify` /
//! `notify:payload` / `notify:resize` / `notify:close` 那几段。
//!
//! # 加载的是**真实** `renderer/notify.html`
//!
//! 不重写一份 HTML，而是把 `notify.html?id=<id>` 交给 webview，并注入同一份
//! `bridge.js`。于是弹窗页与主界面走同一条适配层，`renderer/` 依然一行未改。
//!
//! payload 不走 URL（提问的工具输入可能上千字），而是按 id 存在
//! [`PopupStore`] 里，弹窗页用 `getNotifyPayload(id)` 取回 —— 与 Electron 版一致。
//!
//! # 单窗口策略
//!
//! 任何时刻只有一个弹窗。新弹窗会顶掉旧的：**旧的交互窗先按 `dismissed` 收尾**
//! （`action = null`，调用方回退终端原生询问），否则 hook 那边的长轮询只能干等到
//! 超时才拿到兜底值。
//!
//! ⚠ 别给窗口用固定 label：关窗是异步的，`destroy()` 之后旧 label 可能还被占着，
//! 紧接着 `build()` 会报「label 已存在」。所以 label 带上 id，唯一即无冲突。
//!
//! # 单位
//!
//! 弹窗这一层**全部用逻辑像素（DIP）**：`notify.js` 量的是 CSS px，报回来直接就是
//! DIP；而 Electron 的 `setSize` / `screen.getDisplayMatching` 也都是 DIP。
//! 别在这里混进物理像素 —— 125% 缩放下会偏出去几十像素。
//! （主窗口的几何是另一套，见 [`crate::geometry`]，那边按物理像素算，有它的理由。）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::payload;
use crate::state::{lock, AppState};

/// 弹窗页取回 payload 的次数（自检用，见 [`payload_fetch_count`]）
static PAYLOAD_FETCHES: AtomicU64 = AtomicU64::new(0);

/// 弹窗尺寸下限（与 `main.js` 的 `NOTIFY_MIN` 一致）
const MIN_W: f64 = 320.0;
const MIN_H: f64 = 130.0;
/// 弹窗尺寸上限（`NOTIFY_MAX`）
const MAX_W: f64 = 560.0;
const MAX_H: f64 = 660.0;

/// 靠工作区右上角的留白（`placeTopRight` 里写死的 16）
const MARGIN: f64 = 16.0;

/// 等弹窗页实测高度上报的兜底时间；超时就先按初始尺寸显示（`main.js` 的 600ms）
const SHOW_FALLBACK_MS: u64 = 600;

/// 当前弹窗的标识。
///
/// `interactive` 是这里唯一的**行为**字段（顶掉旧窗时要不要按 `dismissed` 收尾）。
/// 刻意不缓存 `kind` 之类只用于展示的信息 —— 想显示"正在弹什么"就直接读
/// `payloads` 里那份 payload，别让两个副本有机会漂移。
#[derive(Clone, Debug)]
struct Current {
    id: String,
    interactive: bool,
    label: String,
    window: WebviewWindow,
}

/// id → 完整 payload（弹窗页按 id 取回）+ 当前展示中的那个。
///
/// 不 `#[derive(Default)]` 就有 `Debug`；下面手写是因为 `WebviewWindow` 有 Debug，
/// 而 `Current` 已 derive，所以直接派生即可。
#[derive(Default, Debug)]
pub struct PopupStore {
    payloads: HashMap<String, Value>,
    current: Option<Current>,
}

// ---------------------------------------------------------------------------
// 尺寸 / 定位（逻辑像素）
// ---------------------------------------------------------------------------

/// 弹窗应该出现在哪块屏上。
///
/// Electron 用的是「新窗口默认落点所在屏」（`getDisplayMatching(nw.getBounds())`），
/// 在 Windows 上通常就是主屏。这里改成**主窗口所在屏**——用户正在看哪儿就弹哪儿，
/// 多屏下比"总是主屏"更符合预期。主窗口不在了就退回首屏。
fn target_work_area(app: &AppHandle, win: &WebviewWindow) -> (crate::geometry::Rect, f64) {
    if let Some(main) = crate::window::main_window(app) {
        if let Ok(Some(m)) = main.current_monitor() {
            let wa = m.work_area();
            return (
                crate::geometry::Rect::new(
                    wa.position.x,
                    wa.position.y,
                    wa.size.width as i32,
                    wa.size.height as i32,
                ),
                m.scale_factor(),
            );
        }
    }
    if let Ok(Some(m)) = win.current_monitor() {
        let wa = m.work_area();
        return (
            crate::geometry::Rect::new(
                wa.position.x,
                wa.position.y,
                wa.size.width as i32,
                wa.size.height as i32,
            ),
            m.scale_factor(),
        );
    }
    (crate::geometry::Rect::new(0, 0, 1280, 720), 1.0)
}

/// 靠工作区右上角摆放（物理像素下算完再设，因为 `set_position` 收的是逻辑坐标）。
fn place_top_right(win: &WebviewWindow, wa_physical: crate::geometry::Rect, scale: f64) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let Ok(size) = win.outer_size() else { return };
    // 窗口自己的物理尺寸换算成逻辑尺寸，才能和工作区的逻辑坐标对齐
    let w_logical = size.width as f64 / scale;
    let x = (wa_physical.x as f64 + wa_physical.w as f64) / scale - w_logical - MARGIN;
    let y = wa_physical.y as f64 / scale + MARGIN;
    let _ = win.set_position(LogicalPosition::new(x, y));
}

/// JS 的 `Math.max(lo, Math.min(hi, v))` —— 注意 `lo` 可能大于 `hi`
///（工作区极窄时），这时要落 `lo` 而不是 panic。Rust 的 `f64::clamp` 会 panic。
fn clamp_num(v: f64, lo: f64, hi: f64) -> f64 {
    if !v.is_finite() {
        return lo;
    }
    v.round().max(lo).min(hi.max(lo))
}

// ---------------------------------------------------------------------------
// 展示
// ---------------------------------------------------------------------------

/// 弹一条通知 / 交互窗。
///
/// `payload` 由调用方（网关或渲染层）给出；这里只补 `id` / `kind` / `interactive` /
/// `flavor` 四个派生字段，**不重新归一化** —— 与 Electron 的 `showNotify` 一致。
pub fn show(app: &AppHandle, payload: Value) -> Result<(), String> {
    let mut obj = match payload {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };

    // id：调用方没给就现生成（渲染层自己弹阶段提醒时不给 id）
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(crate::interaction::new_id);

    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .filter(|k| payload::kind_spec(k).is_some())
        .unwrap_or("notification")
        .to_string();
    let interactive = payload::is_interactive(&kind);
    // 视觉风味：ask / permission 用自身 kind；其余沿用调用方给的 type
    let flavor = obj
        .get("flavor")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if kind == "ask" || kind == "permission" {
                kind.clone()
            } else {
                obj.get("type")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("agent")
                    .to_string()
            }
        });

    obj.insert("id".into(), json!(id));
    obj.insert("kind".into(), json!(kind));
    obj.insert("interactive".into(), json!(interactive));
    obj.insert("flavor".into(), json!(flavor));
    let data = Value::Object(obj);

    // 先处理旧窗：正在等决策的先按 dismissed 收尾（单窗口策略）
    dismiss_previous(app, &id);

    // 初始尺寸按 kind 给（`popupSizeFor`）——刻意不夹取，与 Electron 一致；
    // 真正的尺寸由弹窗页实测后回传（`resize`）
    let (w, h) = payload::popup_size(&kind);

    let label = format!("notify-{id}");
    let url = format!("notify.html?id={id}");
    let builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(PathBuf::from(url)))
        .title("番茄钟提醒")
        .inner_size(w, h)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        // 先隐藏：等弹窗页实测完高度再显示，避免用户看到一次尺寸跳变
        .visible(false)
        .initialization_script(crate::BRIDGE_JS);

    let win = builder.build().map_err(|e| format!("创建弹窗窗口失败: {e}"))?;

    {
        let st = app.state::<AppState>();
        let mut store = lock(&st.popup);
        store.payloads.insert(id.clone(), data);
        store.current = Some(Current {
            id: id.clone(),
            interactive,
            label: label.clone(),
            window: win.clone(),
        });
    }

    // 兜底显示：弹窗页若 600ms 内没上报尺寸（脚本报错 / 页面没加载），
    // 也要让用户看到东西，而不是"弹窗没出现"。
    let handle = app.clone();
    let fallback_label = label.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(SHOW_FALLBACK_MS));
        let Some(w) = handle.get_webview_window(&fallback_label) else {
            return;
        };
        if w.is_visible().unwrap_or(false) {
            return;
        }
        let (wa, scale) = target_work_area(&handle, &w);
        place_top_right(&w, wa, scale);
        let _ = w.show();
    });

    Ok(())
}

/// 顶掉当前弹窗：交互窗先按 `dismissed` 收尾，再关窗。
///
/// `new_id` 是**即将弹出来的那一份**的 id。若旧窗和它同 id（就是「暂时收起」
/// 后又被唤回），那就**只关窗、绝不 resolve** —— 否则用户点了"唤回"，拿到的却是
/// `dismissed`（等于"算了，交回终端"），而他自己什么都没决定。
fn dismiss_previous(app: &AppHandle, new_id: &str) {
    let previous = {
        let st = app.state::<AppState>();
        let store = lock(&st.popup);
        store.current.clone()
    };
    let Some(prev) = previous else { return };
    if prev.interactive && prev.id != new_id {
        // action=null → 调用方回退宿主原生询问，绝不替用户做决定
        crate::interaction::resolve(app, &prev.id, crate::interaction::Decision::dismissed());
    }
    let _ = prev.window.destroy();
    forget(app, &prev.label);
}

/// 关掉某个 id 的弹窗（如果还在），并清掉它的 payload。
///
/// 「唤回」用得到：正常路径下渲染层收起时会自己关窗，但**不能指望调用方**——
/// 同 id 的窗口还在的话，重弹时 `notify-<id>` 这个 label 会被判"已存在"而建不出来。
pub fn close_by_id(app: &AppHandle, id: &str) {
    let target = {
        let st = app.state::<AppState>();
        let store = lock(&st.popup);
        store
            .current
            .as_ref()
            .filter(|c| c.id == id)
            .map(|c| (c.window.clone(), c.label.clone()))
    };
    if let Some((win, label)) = target {
        let _ = win.destroy();
        forget(app, &label);
    }
}

/// 弹窗页实测内容尺寸后回传 → 夹取、重设尺寸、靠右上角、首次显示。
///
/// ⚠ 交互窗**不套** `MAX_NOTIFY_MS` 那类上限：兜底可能长达 1 小时，
/// 若这里 10 分钟就自动关窗，`closeNotify` 会把交互按 dismissed 提前收尾，
/// 网关的兜底就白设了。（上限统一由 `payload::clamp_timeout_ms` 决定。）
pub fn resize(app: &AppHandle, win: &WebviewWindow, width: f64, height: f64) {
    let (wa, scale) = target_work_area(app, win);
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let wa_w = wa.w as f64 / scale;
    let wa_h = wa.h as f64 / scale;

    let w = clamp_num(width, MIN_W, MAX_W.min(wa_w - 24.0));
    let h = clamp_num(height, MIN_H, MAX_H.min(wa_h - 32.0));

    let _ = win.set_size(LogicalSize::new(w, h));
    place_top_right(win, wa, scale);
    if !win.is_visible().unwrap_or(false) {
        let _ = win.show();
    }
}

/// 弹窗页按 id 取回完整 payload。
pub fn get_payload(app: &AppHandle, id: &str) -> Option<Value> {
    PAYLOAD_FETCHES.fetch_add(1, Ordering::Relaxed);
    let st = app.state::<AppState>();
    let store = lock(&st.popup);
    store.payloads.get(id).cloned()
}

/// 弹窗页一共取回过几次 payload。
///
/// 自检用：它是一个**能证明整条链路通了**的计数 —— 只有「弹窗窗口建起来 →
/// bridge.js 注入 → 渲染层发起一次 await 的 invoke → Rust 回包 → Promise resolve」
/// 全都成立，这个数才会涨。窗口建起来但页面/桥接有问题时它是 0。
pub fn payload_fetch_count() -> u64 {
    PAYLOAD_FETCHES.load(Ordering::Relaxed)
}

/// 窗口真的没了之后的清理：删掉它的 payload，当前指针置空。
///
/// 两个调用点：`destroy()` 之后（主动），以及 `WindowEvent::Destroyed`（被动，
/// 比如用户在任务栏里关掉）。**两处都要幂等** —— 别把 `payloads` 清成空的。
pub fn forget(app: &AppHandle, label: &str) {
    let st = app.state::<AppState>();
    let mut store = lock(&st.popup);
    let matched = store
        .current
        .as_ref()
        .filter(|c| c.label == label)
        .map(|c| c.id.clone());
    if let Some(id) = matched {
        store.payloads.remove(&id);
        store.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_matches_js_semantics() {
        // 常规夹取
        assert_eq!(clamp_num(460.0, 320.0, 560.0), 460.0);
        assert_eq!(clamp_num(100.0, 320.0, 560.0), 320.0);
        assert_eq!(clamp_num(9999.0, 320.0, 560.0), 560.0);
        // js: max(lo, min(hi, v)) —— hi < lo 时落 lo，不是 panic
        assert_eq!(clamp_num(400.0, 320.0, 100.0), 320.0);
        // 非数字落 lo
        assert_eq!(clamp_num(f64::NAN, 320.0, 560.0), 320.0);
        // 取整
        assert_eq!(clamp_num(400.4, 320.0, 560.0), 400.0);
    }

    #[test]
    fn store_default_is_empty() {
        let s = PopupStore::default();
        assert!(s.payloads.is_empty());
        assert!(s.current.is_none());
    }
}
