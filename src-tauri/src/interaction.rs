//! 待处理交互的状态机 —— 对应 Electron 版 `gateway.js` 里 `pendingInteractions` 那一块。
//!
//! # 它要解决的问题
//!
//! 一次交互弹窗（ask / permission / custom）的 HTTP 请求要**挂在那儿**等用户点按钮，
//! 可能挂 1 小时。这期间用户还可能：
//! - 「暂时收起」→ 窗口关掉，但**这次交互没结束**，agent 继续等你从托盘唤回；
//! - 被下一条通知 / 新弹窗顶掉 → 旧请求按 `dismissed` 收尾（调用方回退终端原生询问）；
//! - 什么都不做 → 到兜底上限，落回该 kind 的安全默认值。
//!
//! # 线程模型
//!
//! 每个挂起的交互在 `HashMap` 里有一条记录，带一根 [`mpsc::Sender`]。
//! 发起请求的 HTTP 线程拿 [`Receiver`] 去 [`wait`]，于是：
//! - 用户作答 / 被顶掉 → [`resolve`] 往通道发一条 → HTTP 线程醒来，原样回响应；
//! - 超时 → `recv_timeout` 自己到点，**不需要额外的定时器线程**。
//!
//! 「暂时收起」因此天然满足「不停表」：它只改 `state`，通道没人发东西，
//! `recv_timeout` 继续数着。若改成给 hold 单独起个定时器，那条约定就会漂。
//!
//! # 谁说了算
//!
//! 超时那一刻和用户点击可能只差几毫秒。裁决权交给「**谁先从表里删掉那条**」：
//! [`resolve`] 与超时收尾都在同一把锁里 `remove`，删到的那一方决定结果。
//! 这样不存在「用户点了允许，结果回了 deny」这类最不能接受的错。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::payload;
use crate::state::{lock, AppState};

/// 一次决策的结果。`decided_by` 只有四个合法值，与文档一致：
/// `user` / `timeout` / `dismissed` / `shown`。
///
/// ⚠ `timeout` 与 `dismissed` **都不是用户决定** —— hook CLI 只认 `user`。
/// 别为了让调用方"好处理"把兜底值伪装成 `user`。
#[derive(Debug, Clone)]
pub struct Decision {
    pub action: Option<String>,
    pub answers: Value,
    pub text: String,
    pub decided_by: &'static str,
}

impl Decision {
    /// 用户在弹窗内作答
    pub fn user(action: impl Into<String>, answers: Value, text: String) -> Self {
        Self {
            action: Some(action.into()),
            answers,
            text,
            decided_by: "user",
        }
    }

    /// 「交给终端」/ 被顶掉：`action = null`，调用方回退宿主原生询问
    pub fn dismissed() -> Self {
        Self {
            action: None,
            answers: json!({}),
            text: String::new(),
            decided_by: "dismissed",
        }
    }

    /// 到兜底上限：落该 kind 的安全默认值
    pub fn timeout(default_action: Option<String>) -> Self {
        Self {
            action: default_action,
            answers: json!({}),
            text: String::new(),
            decided_by: "timeout",
        }
    }

    /// 纯通知：弹完即返回
    pub fn shown() -> Self {
        Self {
            action: Some("shown".into()),
            answers: json!({}),
            text: String::new(),
            decided_by: "shown",
        }
    }

    pub fn to_json(&self, kind: &str) -> Value {
        json!({
            "ok": true,
            "kind": kind,
            "action": match &self.action { Some(a) => json!(a), None => Value::Null },
            "answers": self.answers,
            "text": self.text,
            "decidedBy": self.decided_by,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// 窗口正在等用户决策
    Active,
    /// 用户「暂时收起」——窗口收起来了，但这次交互仍在等（**没停表**）
    Held,
}

impl PendingState {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingState::Active => "active",
            PendingState::Held => "held",
        }
    }
}

pub struct Pending {
    pub kind: String,
    pub default_action: Option<String>,
    pub state: PendingState,
    pub held_at: i64,
    pub expires_at: i64,
    /// 归一化后的完整 payload —— 唤回时原样重弹，不用重新构造
    pub payload: Value,
    tx: Sender<Decision>,
}

/// 全部挂起的交互。挂在 [`AppState`] 上，命令 / 托盘 / 网关三边都要读。
#[derive(Default)]
pub struct Store {
    pending: HashMap<String, Pending>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("pending", &self.pending.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// 依赖 AppHandle 的读写（内部自己取锁，调用方不要持锁进来）
// ---------------------------------------------------------------------------

/// 当前毫秒时间戳（与 JS `Date.now()` 同量纲，便于和文档里的时间对照）
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 随机 id（12 字节十六进制）。
///
/// 它只是本地 map 的键 + 塞进弹窗 URL 的 query，**不是安全凭据**；
/// 但要保证同一秒内多次请求不撞 —— 随机比自增计数器更省心。
pub fn new_id() -> String {
    let mut buf = [0u8; 12];
    match getrandom::fill(&mut buf) {
        Ok(()) => {}
        Err(_) => {
            // 系统 CSPRNG 都拿不到就退化成时间 + 地址，唯一性够用（不承担安全职责）
            let t = now_ms() as u128;
            buf[..8].copy_from_slice(&t.to_le_bytes());
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 注册一次等待决策的交互，返回 [`Receiver`]（发起方拿它去 [`wait`]）。
///
/// **副作用**：会把当前所有 `active` 的交互按 `dismissed` 收尾（单窗口策略），
/// 但**放过 `held` 的** —— 用户点名要稍后处理，不该被一条新通知替他丢掉。
/// 弹窗由调用方在本函数返回后自己弹（网关侧的 `show` 回调）。
pub fn register(app: &AppHandle, id: &str, payload: Value, timeout_ms: u64) -> Receiver<Decision> {
    let (tx, rx) = mpsc::channel();
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("custom")
        .to_string();
    let default_action = payload::default_action_of(&payload);

    // 先把要顶掉的收集出来，出锁之后再逐个 resolve（resolve 自己要取锁）
    let victims: Vec<String> = {
        let st = app.state::<AppState>();
        let mut store = lock(&st.interactions);
        let victims = store
            .pending
            .iter()
            .filter(|(k, p)| k.as_str() != id && p.state == PendingState::Active)
            .map(|(k, _)| k.clone())
            .collect();
        store.pending.insert(
            id.to_string(),
            Pending {
                kind,
                default_action,
                state: PendingState::Active,
                held_at: 0,
                expires_at: now_ms() + timeout_ms as i64,
                payload,
                tx,
            },
        );
        victims
    };
    for victim in victims {
        resolve(app, &victim, Decision::dismissed());
    }
    rx
}

/// 等一次决策。挂起期间「暂时收起」不会影响它 —— 通道里没人发东西。
pub fn wait(app: &AppHandle, id: &str, rx: Receiver<Decision>, timeout_ms: u64) -> Decision {
    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(d) => d,
        Err(RecvTimeoutError::Timeout) => match take(app, id) {
            // 抢到了：按该 kind 的安全默认值收尾
            Some(p) => Decision::timeout(p.default_action),
            // 没抢到 → 说明有人（用户点击 / dismiss_all）已经在临界区里删掉并发了结果。
            // 那次 send 早于本次 remove 的失败返回，所以这里必然收得到。
            None => rx.recv_timeout(Duration::from_millis(2000)).unwrap_or_else(|_| {
                eprintln!("[interaction] 超时收尾时既没抢到条目也没收到决策，按 timeout 处理: {id}");
                Decision::timeout(None)
            }),
        },
        Err(RecvTimeoutError::Disconnected) => {
            // 不该发生：Sender 随条目一起被删，删的人必然先发了结果
            eprintln!("[interaction] 决策通道意外断开: {id}");
            Decision::timeout(None)
        }
    }
}

/// 从表里删掉一条（返回它）。删除即裁决 —— 见模块头。
fn take(app: &AppHandle, id: &str) -> Option<Pending> {
    let st = app.state::<AppState>();
    let mut store = lock(&st.interactions);
    store.pending.remove(id)
}

/// 落定一次决策。返回 `false` 表示这条已经不存在了（多半已被超时收走）。
pub fn resolve(app: &AppHandle, id: &str, decision: Decision) -> bool {
    // 取出 + 发送 必须在同一个临界区：否则超时线程会在「已删除但还没发」的窗口里
    // 走进 `take` 返回 None 的分支，然后收不到任何东西。
    let taken = {
        let st = app.state::<AppState>();
        let mut store = lock(&st.interactions);
        store.pending.remove(id).map(|p| {
            // 收件方可能已经走了（超时线程退出），send 失败不是错误
            let _ = p.tx.send(decision);
            (p.state, p.kind)
        })
    };
    match taken {
        Some((state, kind)) => {
            // 收起的那条被收走时要刷新入口，否则托盘会留一个点不动的死条目
            if state == PendingState::Held {
                refresh_entry_points(app);
            }
            let _ = kind;
            true
        }
        None => false,
    }
}

/// 把当前所有 `active` 的交互按 `dismissed` 收尾（`keep_held` 时放过收起的那批）。
pub fn dismiss_all(app: &AppHandle, keep_held: bool) {
    let victims: Vec<String> = {
        let st = app.state::<AppState>();
        let store = lock(&st.interactions);
        store
            .pending
            .iter()
            .filter(|(_, p)| !(keep_held && p.state == PendingState::Held))
            .map(|(k, _)| k.clone())
            .collect()
    };
    for id in victims {
        resolve(app, &id, Decision::dismissed());
    }
}

/// 「暂时收起」：只改状态 + 让调用方关窗，**不 resolve、不停表**。
///
/// ⚠ 这与右上角 ×（`dismissed`，交给终端）是两种不同的收尾，**不能合并**。
pub fn hold(app: &AppHandle, id: &str) -> Option<Value> {
    let st = app.state::<AppState>();
    let mut store = lock(&st.interactions);
    let p = store.pending.get_mut(id)?;
    if p.state == PendingState::Held {
        return None;
    }
    p.state = PendingState::Held;
    p.held_at = now_ms();
    let summary = summary_of(id, p);
    drop(store);
    refresh_entry_points(app);
    Some(summary)
}

/// 唤回：把存档的 payload 交回调用方重弹。已被兜底收走的不复活。
pub fn reopen(app: &AppHandle, id: &str) -> Option<Value> {
    let payload = {
        let st = app.state::<AppState>();
        let mut store = lock(&st.interactions);
        let p = store.pending.get_mut(id)?;
        if p.state != PendingState::Held {
            return None;
        }
        p.state = PendingState::Active;
        p.held_at = 0;
        p.payload.clone()
    };
    refresh_entry_points(app);
    Some(payload)
}

/// 托盘菜单标签：`权限 · Bash` 这种。工具名优先取 context，退回 permission.tool。
fn summary_of(id: &str, p: &Pending) -> Value {
    let pl = &p.payload;
    let ctx_tool = pl
        .get("context")
        .and_then(|c| c.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let perm_tool = pl
        .get("permission")
        .and_then(|c| c.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "id": id,
        "kind": p.kind,
        "state": p.state.as_str(),
        "heldAt": p.held_at,
        "expiresAt": p.expires_at,
        "title": pl.get("title").and_then(Value::as_str).unwrap_or(""),
        "message": pl.get("message").and_then(Value::as_str).unwrap_or(""),
        "source": pl.get("source").and_then(Value::as_str).unwrap_or(""),
        "tool": if ctx_tool.is_empty() { perm_tool } else { ctx_tool },
    })
}

/// 待处理列表（含正在弹的那个），供托盘 / 主窗口展示入口。
pub fn list(app: &AppHandle) -> Vec<Value> {
    let st = app.state::<AppState>();
    let store = lock(&st.interactions);
    let mut out: Vec<Value> = store
        .pending
        .iter()
        .map(|(id, p)| summary_of(id, p))
        .collect();
    // HashMap 无序 → 固定按「收起时间」排，保证托盘菜单与提示条顺序稳定、
    // 「点击唤回最早收起的一条」有意义
    out.sort_by_key(|v| v.get("heldAt").and_then(Value::as_i64).unwrap_or(0));
    out
}

/// 只看「暂时收起」的那批。
pub fn list_held(app: &AppHandle) -> Vec<Value> {
    list(app)
        .into_iter()
        .filter(|v| v.get("state").and_then(Value::as_str) == Some("held"))
        .collect()
}

pub fn count(app: &AppHandle) -> usize {
    let st = app.state::<AppState>();
    // 先把值取出来再返回：`lock(...)` 的临时 guard 若挂在尾表达式上，
    // 会先于 `st` 析构 → 借用检查报"st 活得不够久"。
    let n = lock(&st.interactions).pending.len();
    n
}

/// 最早挂起的那条交互的 id —— 自检脚本用来模拟「用户点了一下」。
///
/// 按 `heldAt` 排序：`active` 的 `heldAt` 是 0，所以优先拿到正在弹的那个。
pub fn first_pending_id(app: &AppHandle) -> Option<String> {
    let st = app.state::<AppState>();
    let store = lock(&st.interactions);
    store
        .pending
        .iter()
        .min_by_key(|(_, p)| p.held_at)
        .map(|(id, _)| id.clone())
}

/// 收起集变化后刷新**全部**唤回入口：主窗口提示条 + 托盘菜单/tooltip。
///
/// 三个入口必须一起刷 —— 曾经的教训是只做了托盘右键子菜单，用户报「收起后
/// 没地方重新打开」（其实是 Win11 把托盘图标折进了溢出区）。**至少一个入口
/// 必须在主界面里。**
pub fn refresh_entry_points(app: &AppHandle) {
    let held = list_held(app);
    crate::window::emit(app, "state:pending-held", held.clone());
    crate::tray::refresh_pending(app, held.len());
}

/// 主窗口提示条只用来展示「有几条、分别是什么」——和托盘的菜单标签同一套文案。
pub fn held_chip_items(app: &AppHandle) -> Value {
    Value::Array(
        list_held(app)
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.get("id").cloned().unwrap_or(Value::Null),
                    "kind": p.get("kind").cloned().unwrap_or(Value::Null),
                    "label": menu_label(&p),
                    "title": p.get("title").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

/// 唤回收起的一条并重新弹出来（`id = None` 时取最早收起的那条）。
///
/// 三个入口（主界面提示条 / 托盘左键单击 / 托盘右键子菜单）都走这里 ——
/// 逻辑只有一份，不会出现「其中某个入口忘了刷新列表」。
pub fn reopen_held(app: &AppHandle, id: Option<&str>) -> bool {
    let target = match id {
        Some(i) if !i.is_empty() => i.to_string(),
        _ => match list_held(app)
            .first()
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
        {
            Some(s) => s.to_string(),
            None => return false,
        },
    };
    // 旧窗口可能还开着（正常路径下渲染层收起时会自己关掉，但唤回不该依赖调用方）。
    // 同 id 的窗口必须先消失，否则 `popup::show` 建 `notify-<id>` 这个 label 会失败。
    crate::popup::close_by_id(app, &target);
    // 可能已经在收起期间被兜底收走了，拿不到就不再弹
    let reopened = match reopen(app, &target) {
        Some(data) => match crate::popup::show(app, data) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[interaction] 唤回收起的确认失败: {e}");
                false
            }
        },
        None => false,
    };
    refresh_entry_points(app);
    reopened
}

/// 托盘菜单 / 提示条共用的短标签：`权限 · Bash`、`提问 · 选个方案`。
///
/// 入参可能是两种形状，**两个都要能出 `权限 · Bash`**：
/// - [`summary_of`] 产出的列表项（已把工具名摊平到顶层 `tool`）；
/// - 原始归一化 payload（只有 `context.tool` / `permission.tool`）。
///
/// 所以这里**自己走完整的兜底链**，不假设调用方替我们摊平过 —— 否则哪天有人拿
/// 原始 payload 调一次，标签就会退化成 `权限 · 允许 Bash？`（曾经就是这样）。
pub fn menu_label(p: &Value) -> String {
    let kind_text = match p.get("kind").and_then(Value::as_str).unwrap_or("") {
        "permission" => "权限",
        "ask" => "提问",
        _ => "确认",
    };
    // 工具名优先取 context，退回 permission.tool（直连 API 的调用方常常只传后者）
    let direct = p.get("tool").and_then(Value::as_str).unwrap_or("");
    let ctx_tool = p
        .get("context")
        .and_then(|c| c.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let perm_tool = p
        .get("permission")
        .and_then(|c| c.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool = if !direct.is_empty() {
        direct
    } else if !ctx_tool.is_empty() {
        ctx_tool
    } else {
        perm_tool
    };
    let what = if !tool.is_empty() {
        tool
    } else {
        ["title", "message"]
            .iter()
            .map(|k| p.get(*k).and_then(Value::as_str).unwrap_or(""))
            .find(|s| !s.is_empty())
            .unwrap_or("")
    };
    // 空白折成单空格，否则多行的 title 会把托盘菜单撑歪
    let what: String = what.split_whitespace().collect::<Vec<_>>().join(" ");
    if what.is_empty() {
        return kind_text.to_string();
    }
    let short: String = what.chars().take(35).collect();
    if what.chars().count() > 36 {
        format!("{kind_text} · {short}…")
    } else {
        format!("{kind_text} · {short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::normalize_interaction;

    fn perm_payload(tool: &str) -> Value {
        let mut p = normalize_interaction(&json!({
            "kind": "permission",
            "permission": { "tool": tool },
            "title": "允许 Bash？"
        }));
        p.as_object_mut()
            .unwrap()
            .insert("id".into(), json!("test-id"));
        p
    }

    #[test]
    fn decision_json_shape_matches_docs() {
        let d = Decision::user("allow-always", json!({}), "备注".into());
        let v = d.to_json("permission");
        assert_eq!(v["ok"], true);
        assert_eq!(v["kind"], "permission");
        assert_eq!(v["action"], "allow-always");
        assert_eq!(v["decidedBy"], "user");
        assert_eq!(v["text"], "备注");

        // 被顶掉 / 交给终端：action 必须是 null，调用方据此回退终端原生询问
        let v = Decision::dismissed().to_json("ask");
        assert_eq!(v["action"], Value::Null);
        assert_eq!(v["decidedBy"], "dismissed");
        assert_eq!(v["answers"], json!({}));

        // 超时落安全默认值
        let v = Decision::timeout(Some("deny".into())).to_json("permission");
        assert_eq!(v["action"], "deny");
        assert_eq!(v["decidedBy"], "timeout");
    }

    #[test]
    fn ids_are_unique_and_url_safe() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 24);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn menu_label_prefers_tool_then_title() {
        let p = perm_payload("Bash");
        assert_eq!(menu_label(&p), "权限 · Bash");
        // 没有 tool 就退到标题
        let p = normalize_interaction(&json!({ "kind": "ask", "title": "选个方案" }));
        assert_eq!(menu_label(&p), "提问 · 选个方案");
        // 都没有就只剩类别。
        // ⚠ 这里只能喂**没有归一化过**的裸对象：归一化会给每个 kind 填上默认标题
        //（custom → "需要确认"），那样标签就变成 `确认 · 需要确认` 了 —— 那是对的行为。
        let p = json!({ "kind": "custom" });
        assert_eq!(menu_label(&p), "确认");
    }

    #[test]
    fn menu_label_collapses_whitespace_and_truncates() {
        let p = normalize_interaction(&json!({ "kind": "ask", "title": "多行\n标题\t带空白" }));
        assert_eq!(menu_label(&p), "提问 · 多行 标题 带空白");
        let long = "很长的标题".repeat(20);
        let p = normalize_interaction(&json!({ "kind": "ask", "title": long }));
        let label = menu_label(&p);
        assert!(label.ends_with('…'));
        // 上界 = 类别（最多 2 字）+ " · "（3）+ 正文（最多 35 + 省略号 1）
        assert!(label.chars().count() <= 2 + 3 + 36, "实际 {} 字", label.chars().count());
    }

    #[test]
    fn menu_label_reads_tool_from_raw_payload_too() {
        // 原始归一化 payload 没有顶层 tool，只有 context.tool / permission.tool。
        // 托盘列表项（summary）才有摊平后的顶层 tool —— 两种形状都得能用。
        let p = normalize_interaction(&json!({
            "kind": "permission",
            "permission": { "tool": "Bash" },
            "title": "允许 Bash？"
        }));
        assert_eq!(menu_label(&p), "权限 · Bash", "没有顶层 tool 时要从 permission.tool 兜底");

        // context.tool 优先于 permission.tool
        let p = normalize_interaction(&json!({
            "kind": "permission",
            "context": { "tool": "Edit" },
            "permission": { "tool": "Bash" }
        }));
        assert_eq!(menu_label(&p), "权限 · Edit");
    }

    #[test]
    fn summary_tool_falls_back_to_permission_tool() {
        let p = perm_payload("Bash");
        let s = summary_of("id1", &Pending {
            kind: "permission".into(),
            default_action: Some("deny".into()),
            state: PendingState::Active,
            held_at: 0,
            expires_at: 0,
            payload: p,
            tx: mpsc::channel().0,
        });
        assert_eq!(s["tool"], "Bash");
        assert_eq!(s["state"], "active");
        assert_eq!(s["kind"], "permission");
    }
}
