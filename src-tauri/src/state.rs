//! 应用共享状态。
//!
//! 分几块，各自一把锁：
//! - [`WindowState`] —— 迷你 / 贴边 / 拖拽。改它的人多（命令、托盘、拖拽线程），
//!   所以逻辑一律写成「取锁 → 算 → 放锁 → 再动窗口」，别在持锁时调窗口 API。
//! - [`TimerState`] —— 渲染层上报的完整计时快照（托盘倒计时、网关 `/api/status` 用）。
//! - [`TraySync`] —— 托盘上一次同步过的值，用来去重（图标染色、菜单重建都有成本）。
//! - [`Activity`] / [`GatewayRuntime`] —— 本专注期的 agent 活动计数与休息建议冷却。
//! - [`crate::interaction::Store`] —— 挂起中的交互（含「暂时收起」的那批）。
//! - [`crate::popup::PopupStore`] —— 弹窗 id → payload，供弹窗页按 id 取回。
//!
//! **加锁顺序**：`interactions` → `tray`（`refresh_entry_points` 会先读前者再改后者）。
//! 反向没有路径，所以不会死锁 —— 但也别去创造一条。

use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::geometry::{Edge, Rect};
use crate::interaction::Store as InteractionStore;
use crate::popup::PopupStore;

/// `panic = "abort"` 下不会有锁中毒，但 debug/测试环境下有。中毒了也要把值取回来 ——
/// 为了一个已经 panic 的临界区把整个窗口行为拖死不值当。
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 迷你模式拖拽中的一次会话。
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    /// 按下时光标相对窗口左上角的偏移（拖拽期间保持不变）
    pub offset: (i32, i32),
    /// 按下时的窗口尺寸
    pub size: (i32, i32),
    /// 本次拖拽的基准屏工作区（黏滞：只有光标真的深入另一屏才换）
    pub wa: Rect,
}

/// 窗口形态状态。
///
/// `Copy`：字段全是 Copy 类型，[`crate::window`] 靠它实现"取锁→拷贝→放锁"的读取快照
/// （见那边的 `win_state`）。**加字段时注意别引入非 Copy 类型** ——
/// 会让所有窗口行为的读法都得改回持锁。
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowState {
    /// 进入迷你模式前的完整 bounds（退出时按它还原尺寸）
    pub full: Option<Rect>,
    /// 是否处于迷你悬浮模式
    pub mini: bool,
    /// 贴边吸附在哪条边（None = 没贴边）
    pub dock: Option<Edge>,
    /// 贴边是否处于「收起」态（只露一条细进度条）
    pub dock_hidden: bool,
    /// 吸附瞬间所在屏的工作区快照 —— 之后滑出/收回的几何都以它为准，
    /// 免得窗口在边上来回移动时反复重新匹配屏幕
    pub dock_wa: Option<Rect>,
    /// 拖拽会话（None = 没在拖）
    pub drag: Option<Drag>,
    /// 延时收回的令牌。每次「安排」或「取消」都自增；睡眠线程醒来发现令牌变了就放弃。
    /// 用令牌取代 Electron 的 clearTimeout —— 跨线程取消定时器没有更好的写法。
    pub hide_token: u64,
}

/// 渲染层每次 render 都推一份完整计时快照。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerState {
    pub phase: String,
    pub running: bool,
    pub remain_ms: f64,
    pub total_ms: f64,
    pub completed_focus: f64,
    pub round_in_cycle: f64,
    pub rounds: f64,
}

impl Default for TimerState {
    fn default() -> Self {
        // `round_in_cycle` / `rounds` 的缺省值必须是 1 / 4：Electron 的 `/api/status`
        // 用的是 `t.roundInCycle || 1` / `t.rounds || 4`，也就是"缓存为空时"的兜底值。
        // 写 0 会让**刚启动、渲染层还没上报**那一刻的 `/api/status` 与 Electron 对不上。
        Self {
            phase: "work".to_string(),
            running: false,
            remain_ms: 0.0,
            total_ms: 0.0,
            completed_focus: 0.0,
            round_in_cycle: 1.0,
            rounds: 4.0,
        }
    }
}

/// `tray:update` 的入参。字段一律 `Option`：渲染层偶尔只推局部，
/// 缺失的字段要保留上一次的值（照搬 main.js 的合并语义）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayPatch {
    pub phase: Option<String>,
    pub running: Option<bool>,
    pub time_left_text: Option<String>,
    pub remain_ms: Option<f64>,
    pub total_ms: Option<f64>,
    pub completed_focus: Option<f64>,
    pub round_in_cycle: Option<f64>,
    pub rounds: Option<f64>,
}

impl TimerState {
    /// 合并一次上报（只覆盖真正带来的字段）。
    pub fn merge(&mut self, p: &TrayPatch) {
        if let Some(v) = p.remain_ms {
            self.remain_ms = v;
        }
        if let Some(v) = p.total_ms {
            self.total_ms = v;
        }
        if let Some(v) = p.completed_focus {
            self.completed_focus = v;
        }
        if let Some(v) = p.round_in_cycle {
            self.round_in_cycle = v;
        }
        if let Some(v) = p.rounds {
            self.rounds = v;
        }
        self.running = p.running.unwrap_or(false);
        if let Some(v) = &p.phase {
            self.phase = v.clone();
        }
    }
}

/// 托盘上一次实际下发的值 —— 只在变化时调原生接口。
#[derive(Debug, Default)]
pub struct TraySync {
    pub phase: Option<String>,
    pub running: Option<bool>,
    pub time_text: Option<String>,
    /// 待处理的确认条数（含正在弹的那个）——变化时要重建菜单 + 改 tooltip。
    /// 「暂时收起」的入口就在菜单最上面，不跟着变就会留一个点不动的死条目。
    pub pending: Option<usize>,
}

/// 本专注期的 agent 活动计数。
///
/// 与 `gateway.js` 的 `activity` 同形：阶段切回 `work` 时清零，
/// 主窗口把它渲染成 `🤖 工具 N · 打断 M`。
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub tool_calls: u32,
    /// 打断 = 提问 + 权限确认次数
    pub interruptions: u32,
    pub stops: u32,
    pub sessions: u32,
    /// 本期起点（ms 时间戳）。从未进入过专注期时是 null —— 渲染层不读，但契约里得有。
    pub since: Option<i64>,
}

/// 网关的纯数据部分（监听句柄在 [`crate::gateway`] 自己的静态量里，
/// 因为 `tiny_http::Server` 既不 `Debug` 也不 `Default`，塞进来会污染整个 `AppState`）。
#[derive(Debug, Default)]
pub struct GatewayRuntime {
    pub activity: Activity,
    /// 上一次观察到的定时器阶段 —— 用来识别「切回 work ⇒ 新专注期开始，计数清零」
    pub prev_phase: Option<String>,
    /// 上次弹「休息建议」的时间戳（冷却 10 分钟）
    pub last_break_suggest_at: i64,
}

/// 全部共享状态。
#[derive(Debug, Default)]
pub struct AppState {
    pub win: Mutex<WindowState>,
    pub timer: Mutex<TimerState>,
    pub tray: Mutex<TraySync>,
    pub gateway: Mutex<GatewayRuntime>,
    pub interactions: Mutex<InteractionStore>,
    pub popup: Mutex<PopupStore>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(json: serde_json::Value) -> TrayPatch {
        serde_json::from_value(json).expect("TrayPatch 反序列化失败")
    }

    #[test]
    fn merge_overwrites_only_present_fields() {
        let mut s = TimerState {
            phase: "work".into(),
            running: true,
            remain_ms: 1000.0,
            total_ms: 1500.0,
            completed_focus: 2.0,
            round_in_cycle: 2.0,
            rounds: 4.0,
        };
        // 只带了 phase / running / timeLeftText：其余数值必须原样保留
        s.merge(&patch(serde_json::json!({ "phase": "break", "running": false, "timeLeftText": "05:00" })));
        assert_eq!(s.phase, "break");
        assert!(!s.running);
        assert_eq!(s.remain_ms, 1000.0, "缺失的 remainMs 不该被清零");
        assert_eq!(s.total_ms, 1500.0);
        assert_eq!(s.completed_focus, 2.0);
        assert_eq!(s.round_in_cycle, 2.0);
        assert_eq!(s.rounds, 4.0);
    }

    #[test]
    fn merge_accepts_full_snapshot() {
        let mut s = TimerState::default();
        s.merge(&patch(serde_json::json!({
            "running": true, "phase": "longBreak", "timeLeftText": "14:59",
            "remainMs": 899000, "totalMs": 900000,
            "completedFocus": 3, "roundInCycle": 4, "rounds": 4
        })));
        assert_eq!(s.phase, "longBreak");
        assert_eq!(s.remain_ms, 899_000.0);
        assert_eq!(s.total_ms, 900_000.0);
        assert_eq!(s.completed_focus, 3.0);
        assert_eq!(s.round_in_cycle, 4.0);
        assert_eq!(s.rounds, 4.0);
    }

    #[test]
    fn merge_tolerates_empty_payload() {
        let mut s = TimerState::default();
        s.merge(&patch(serde_json::json!({})));
        assert_eq!(s.phase, "work");
        assert!(!s.running);
    }

    #[test]
    fn lock_recovers_from_poisoning() {
        let m = Mutex::new(1u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
            panic!("故意中毒");
        }));
        assert_eq!(*lock(&m), 1, "中毒后仍应取回原值");
    }
}
