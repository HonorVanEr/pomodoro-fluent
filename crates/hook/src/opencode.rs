//! OpenCode 适配层。
//!
//! 对应 `pomodoro-hook.js` 的 `openCodeContext` / `runOpenCodePermission` /
//! `runOpenCodeQuestion` / `replyQuestion` / `rejectQuestion` /
//! `finishOpenCodeQuestion` / `runOpenCodeEvent`。
//!
//! OpenCode 走的是**插件**而不是 hook 配置：`bin/opencode/pomodoro-opencode.ts`
//! 监听 `permission.ask` / `question.asked` 等事件，再把 JSON 通过 stdin 交给本 CLI，
//! 拿 stdout 的 JSON 回写 `output.status`。所以这一族的输出形状与别家完全不同
//! （`{status}` / `{answers}` / `{reject}` / `{ok}`），**不是** `hookSpecificOutput`。
//!
//! 提问的答案还有一条**直连**通道：插件会把 OpenCode 服务端地址（`serverUrl`）
//! 一起带过来，本 CLI 可以直接 POST 回去，省掉插件那一跳。

use serde_json::{json, Value};

use crate::ancli::{post_event, post_interaction, HookResult};
use crate::http::{encode_uri_component, status_request, Gateway};
use crate::proto::{summarize_tool_input, timeout_ms};
use crate::session::with_extra;
use crate::util::{as_str_lossy, is_truthy, pick, pick_str, short_session};

/// OpenCode 回传答案时试的两条路由，各自的超时。
const REPLY_TIMEOUT_MS: u64 = 8_000;

/// `openCodeContext(payload, extra)` —— 上下文由插件随请求带上
/// （`sessionID` / `directory` / `sessionTitle`），CLI 侧不用猜。
pub fn context(payload: &Value, extra: &[(&str, Value)]) -> Value {
    let dir = pick_str(payload, &["directory"]);
    let project = {
        let explicit = pick_str(payload, &["project"]);
        if !explicit.is_empty() {
            explicit
        } else if !dir.is_empty() {
            crate::util::basename(&dir)
        } else {
            String::new()
        }
    };
    let base = json!({
        "agent": "opencode",
        "agentType": pick_str(payload, &["agent", "agentType"]),
        "agentId": "",
        "session": short_session(
            &pick(payload, &["sessionID", "sessionId"])
                .map(as_str_lossy)
                .unwrap_or_default()
        ),
        "project": project,
        "task": pomodoro_core::collapse(&pick_str(payload, &["sessionTitle", "title"]), 160),
        "tool": "",
        "toolDetail": "",
    });
    with_extra(base, extra)
}

/// `runOpenCodePermission(gw, payload)` → `{status}` / `{status, message}`
pub fn run_permission(gw: &Gateway, payload: &Value) -> HookResult {
    // 插件传的是 `{ permission: {...}, ...info }`，也兼容直接把 permission 摊平的情况
    let p = match pick(payload, &["permission"]) {
        Some(v) if is_truthy(v) => v.clone(),
        _ => payload.clone(),
    };
    let perm_type = {
        let t = pick_str(&p, &["type", "permission"]);
        if t.is_empty() {
            "tool".to_string()
        } else {
            t
        }
    };
    // `p.patterns || (p.pattern ? [p.pattern] : [])` —— 注意 patterns 可能**不是数组**，
    // 那样 `patterns[0]` 取到的是字符串首字符（照抄 JS 的行为）
    let patterns = match p.get("patterns") {
        Some(v) if is_truthy(v) => Some(v.clone()),
        _ => pick(&p, &["pattern"]).map(|v| json!([v])),
    };
    let meta = pick(&p, &["metadata"]).cloned().unwrap_or_else(|| json!({}));

    let message = match &patterns {
        Some(Value::Array(a)) => a
            .iter()
            .map(as_str_lossy)
            .collect::<Vec<_>>()
            .join("  "),
        Some(v) => as_str_lossy(v),
        None => String::new(),
    };
    let rule = patterns
        .as_ref()
        .and_then(|v| match v {
            Value::Array(a) => a.first().cloned(),
            Value::String(s) => s.chars().next().map(|c| json!(c.to_string())),
            _ => None,
        })
        .map(|v| as_str_lossy(&v))
        .unwrap_or_default();

    let ms = timeout_ms();
    let title = {
        let t = pick_str(&p, &["title"]);
        if t.is_empty() {
            format!("允许 {perm_type}？")
        } else {
            t
        }
    };
    let result = post_interaction(
        gw,
        &json!({
            "kind": "permission",
            "source": "opencode",
            "title": title,
            "message": message,
            "detail": summarize_tool_input(&meta),
            "permission": {
                "tool": perm_type,
                "rule": rule,
                "suggestions": [],
                // OpenCode 没有「始终允许」的落点，别显示一个点了没用的按钮
                "canAlways": false,
            },
            "context": context(payload, &[
                ("tool", json!(perm_type)),
                ("toolDetail", json!(pomodoro_core::collapse(&rule, 160))),
            ]),
            "timeoutMs": ms,
        }),
        ms,
    )?;

    // 番茄钟没运行 / 用户没决策 → 交回 OpenCode 原生询问
    if !is_truthy(&result) || pick_str(&result, &["decidedBy"]) != "user" {
        return Ok(Some(json!({ "status": "ask" })));
    }
    let text = pick_str(&result, &["text"]);
    if pick_str(&result, &["action"]) == "allow" {
        return Ok(Some(json!({ "status": "allow", "message": text })));
    }
    Ok(Some(json!({ "status": "deny", "message": text })))
}

/// `runOpenCodeQuestion(gw, payload)` → `{answers, text}` 或 `{reject: true}`
pub fn run_question(gw: &Gateway, payload: &Value) -> HookResult {
    let raw = crate::util::arr(payload, "questions");
    let mut questions = Vec::new();
    for (i, q) in raw.iter().take(4).enumerate() {
        let options: Vec<Value> = crate::util::arr(q, "options")
            .iter()
            .take(6)
            .enumerate()
            .map(|(j, o)| match o {
                Value::String(s) => json!({ "id": format!("o{j}"), "label": s, "description": "" }),
                _ => json!({
                    "id": format!("o{j}"),
                    "label": pick_str(o, &["label", "text"]),
                    "description": pick_str(o, &["description"]),
                }),
            })
            .filter(|o| !pick_str(o, &["label"]).is_empty())
            .collect();
        let question_text = {
            let t = pick_str(q, &["question", "text"]);
            if t.is_empty() {
                format!("问题 {}", i + 1)
            } else {
                t
            }
        };
        let custom = match crate::util::get(q, "custom").and_then(Value::as_bool) {
            Some(b) => b,
            None => options.is_empty(),
        };
        questions.push(json!({
            "id": format!("q{i}"),
            "question": question_text,
            "header": pick_str(q, &["header"]),
            "multiSelect": crate::util::pick_bool(q, &["multiple", "multiSelect"]),
            "custom": custom,
            "options": options,
        }));
    }
    if questions.is_empty() {
        return Ok(Some(json!({ "reject": true })));
    }

    let ms = timeout_ms();
    let message = if questions.len() == 1 {
        pick_str(&questions[0], &["question"])
    } else {
        format!("{} 个问题等待回答", questions.len())
    };
    let result = post_interaction(
        gw,
        &json!({
            "kind": "ask",
            "source": "opencode",
            "title": "Agent 提问",
            "message": message,
            "questions": questions,
            "context": context(payload, &[("tool", json!("question"))]),
            "timeoutMs": ms,
        }),
        ms,
    )?;

    if !is_truthy(&result)
        || pick_str(&result, &["decidedBy"]) != "user"
        || pick_str(&result, &["action"]) != "submit"
    {
        return Ok(Some(json!({ "reject": true })));
    }

    let answers_holder = pick(&result, &["answers"]).cloned().unwrap_or_else(|| json!({}));
    let answers: Vec<Value> = questions
        .iter()
        .map(|q| {
            let v = pick(&answers_holder, &[pick_str(q, &["id"]).as_str()]);
            match v {
                Some(Value::Array(a)) => Value::Array(a.clone()),
                Some(other) => json!([other.clone()]),
                None => json!([]),
            }
        })
        .collect();

    Ok(Some(json!({
        "answers": answers,
        "text": pick_str(&result, &["text"]),
    })))
}

/// `runOpenCodeEvent(gw, payload)`
pub fn run_event(gw: &Gateway, payload: &Value) -> HookResult {
    let evt = pick_str(payload, &["event", "type"]);
    // `p.properties || p.data || p`
    let props = pick(payload, &["properties", "data"])
        .cloned()
        .unwrap_or_else(|| payload.clone());
    let kind = match evt.as_str() {
        "session.idle" => "stop",
        "session.error" => "notification",
        "session.created" => "session-start",
        "session.deleted" => "session-end",
        _ => "",
    };
    let kind = if !kind.is_empty() {
        kind.to_string()
    } else {
        pick_str(payload, &["kind"])
    };
    if kind.is_empty() {
        return Ok(None);
    }

    let message = if kind == "notification" {
        let m = pick_str(&props, &["error", "message"]);
        if m.is_empty() {
            "OpenCode 会话异常".to_string()
        } else {
            m
        }
    } else {
        String::new()
    };
    let tool = if kind == "notification" && pick(&props, &["error"]).is_some() {
        "error"
    } else {
        ""
    };

    post_event(
        gw,
        &json!({
            "kind": kind,
            "source": "opencode",
            "message": message,
            "context": context(&props, &[("tool", json!(tool))]),
        }),
    )?;
    Ok(None)
}

/// `finishOpenCodeQuestion(payload, out)` —— 把答案送回 OpenCode。
///
/// 先试 `session/<id>/question/reply`，再试 `question/<id>/reply`（新旧两种路由都
/// 试一遍，OpenCode 改过路由）。全都失败就把结果原样打给插件，由它用 SDK 回传。
pub fn finish_question(payload: &Value, out: &Value) -> HookResult {
    let server_url = pick_str(payload, &["serverUrl"]);
    let session_id = pick_str(payload, &["sessionID"]);
    let request_id = pick_str(payload, &["requestID"]);

    let delivered = if is_truthy(out) && crate::util::get(out, "reject").map(is_truthy).unwrap_or(false) {
        reject_question(&server_url, &session_id, &request_id)
    } else {
        let answers = pick(out, &["answers"]).cloned().unwrap_or_else(|| json!([]));
        reply_question(&server_url, &session_id, &request_id, &answers)
    };

    Ok(Some(if delivered { json!({ "ok": true }) } else { out.clone() }))
}

fn reply_question(server_url: &str, session_id: &str, request_id: &str, answers: &Value) -> bool {
    if server_url.is_empty() || request_id.is_empty() {
        return false;
    }
    let base = server_url.trim_end_matches('/');
    let tries = [
        (
            format!(
                "{base}/session/{}/question/reply",
                encode_uri_component(session_id)
            ),
            json!({ "requestID": request_id, "answers": answers }),
        ),
        (
            format!("{base}/question/{}/reply", encode_uri_component(request_id)),
            json!({ "answers": answers }),
        ),
    ];
    for (url, body) in tries {
        if let Ok(status) = status_request(&url, None, &body, REPLY_TIMEOUT_MS) {
            if status < 400 {
                return true;
            }
        }
    }
    false
}

fn reject_question(server_url: &str, session_id: &str, request_id: &str) -> bool {
    if server_url.is_empty() || request_id.is_empty() {
        return false;
    }
    let base = server_url.trim_end_matches('/');
    let tries = [
        (
            format!(
                "{base}/session/{}/question/reject",
                encode_uri_component(session_id)
            ),
            json!({ "requestID": request_id }),
        ),
        (
            format!("{base}/question/{}/reject", encode_uri_component(request_id)),
            json!({}),
        ),
    ];
    for (url, body) in tries {
        if let Ok(status) = status_request(&url, None, &body, REPLY_TIMEOUT_MS) {
            if status < 400 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_uses_plugin_supplied_fields() {
        let p = json!({
            "sessionID": "abcdefghij",
            "directory": "C:\\ws\\proj",
            "sessionTitle": "重构弹窗",
            "agent": "build",
        });
        let ctx = context(&p, &[("tool", json!("bash"))]);
        assert_eq!(ctx["agent"], "opencode");
        assert_eq!(ctx["agentType"], "build");
        assert_eq!(ctx["session"], "efghij");
        assert_eq!(ctx["project"], "proj");
        assert_eq!(ctx["task"], "重构弹窗");
        assert_eq!(ctx["tool"], "bash");
    }

    #[test]
    fn context_project_prefers_explicit_then_dir() {
        let ctx = context(&json!({ "project": "显式", "directory": "C:\\a\\b" }), &[]);
        assert_eq!(ctx["project"], "显式");
        let ctx = context(&json!({ "directory": "C:\\a\\b" }), &[]);
        assert_eq!(ctx["project"], "b");
        let ctx = context(&json!({}), &[]);
        assert_eq!(ctx["project"], "");
    }

    #[test]
    fn question_shape_defaults_and_caps() {
        // 单题无选项 → custom 打开；question 缺失 → 用「问题 N」兜底
        let p = json!({ "questions": [{ "text": "继续吗" }, { "question": "q2", "options": ["A"] }] });
        assert_eq!(crate::util::arr(&p, "questions").len(), 2);
        let q0_text = {
            let t = pick_str(&crate::util::arr(&p, "questions")[0], &["question", "text"]);
            if t.is_empty() { "问题 1".to_string() } else { t }
        };
        assert_eq!(q0_text, "继续吗");
        let q1 = &crate::util::arr(&p, "questions")[1];
        assert_eq!(pick_str(q1, &["question", "text"]), "q2");
        // custom 默认 = 没有选项
        assert!(crate::util::arr(q1, "options").len() == 1);
    }

    #[test]
    fn empty_questions_reject_immediately() {
        // 这条分支不发网络请求，直接 reject
        let out = json!({ "reject": true });
        assert_eq!(out["reject"], true);
        assert!(crate::util::arr(&json!({}), "questions").is_empty());
    }
}
