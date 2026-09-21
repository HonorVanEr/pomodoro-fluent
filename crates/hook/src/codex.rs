//! Codex CLI 的 **老 notify 通道**（`~/.codex/config.toml` 的 `notify`）。
//!
//! 对应 `pomodoro-hook.js` 的 `runCodexNotify`。
//!
//! ⚠ 这条通道与 `hooks.json` 那套完全是两回事：`notify` 只在**回合结束**回调一次，
//! 单向、没有决策回传通道，payload 字段还全是 kebab-case
//! （`thread-id` / `input-messages` / `last-assistant-message`）。
//! 默认 `install` **不改写** config.toml 的 notify —— 用户可能已经把它指向别的工具
//! （例如 codex-computer-use）。要加得显式带 `--with-notify`。

use serde_json::{json, Value};

use crate::ancli::{post_event, HookResult};
use crate::http::Gateway;
use crate::session::{build_context, remember_context};
use crate::util::{arr, pick_str};

/// `runCodexNotify(gw, payload)` —— 单向通知，**不往 stdout 写任何东西**。
pub fn run_notify(gw: &Gateway, payload: &Value) -> HookResult {
    let p = payload;
    let event_type = pick_str(p, &["type", "event"]);
    let roots = arr(p, "workspace-roots");
    let cwd = {
        let direct = pick_str(p, &["cwd"]);
        if direct.is_empty() {
            roots.first().map(crate::util::as_str_lossy).unwrap_or_default()
        } else {
            direct
        }
    };
    let inputs = arr(p, "input-messages");
    let task = pomodoro_core::collapse(
        &inputs
            .last()
            .map(crate::util::as_str_lossy)
            .unwrap_or_default(),
        160,
    );
    let last = pomodoro_core::collapse(&pick_str(p, &["last-assistant-message", "message"]), 160);
    let session_id = pick_str(p, &["thread-id", "thread_id"]);

    let session_payload = json!({
        "session_id": session_id,
        "cwd": cwd,
        "prompt": task,
        "last_assistant_message": last,
    });
    let kind_event = if event_type == "agent-turn-complete" {
        "Stop"
    } else {
        "Notification"
    };
    let session = remember_context(&session_payload, kind_event, "codex");
    let context = build_context(
        &json!({ "session_id": session_id, "cwd": cwd }),
        "codex",
        session.as_ref(),
    );

    let kind = if event_type == "agent-turn-complete" {
        "stop"
    } else {
        "notification"
    };
    let message = if kind == "notification" {
        if !last.is_empty() {
            last
        } else if !event_type.is_empty() {
            event_type
        } else {
            "Codex 事件".to_string()
        }
    } else {
        String::new()
    };

    post_event(
        gw,
        &json!({ "kind": kind, "source": "codex", "message": message, "context": context }),
    )?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_drives_kind() {
        // agent-turn-complete → stop；其余一律 notification
        let kind_of = |t: &str| if t == "agent-turn-complete" { "stop" } else { "notification" };
        assert_eq!(kind_of("agent-turn-complete"), "stop");
        assert_eq!(kind_of(""), "notification");
        assert_eq!(kind_of("别的事件"), "notification");
    }

    #[test]
    fn kebab_case_payload_fields_are_read() {
        let p = json!({
            "type": "agent-turn-complete",
            "thread-id": "t-1",
            "workspace-roots": ["C:\\ws"],
            "input-messages": ["第一句", "最后一句"],
            "last-assistant-message": "干完了",
        });
        assert_eq!(pick_str(&p, &["type", "event"]), "agent-turn-complete");
        assert_eq!(pick_str(&p, &["thread-id", "thread_id"]), "t-1");
        assert_eq!(arr(&p, "workspace-roots").len(), 1);
        assert_eq!(arr(&p, "input-messages").len(), 2);
        assert_eq!(
            pick_str(&p, &["last-assistant-message", "message"]),
            "干完了"
        );
    }
}
