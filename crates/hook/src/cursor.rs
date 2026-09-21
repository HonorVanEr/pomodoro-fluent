//! Cursor 适配层。
//!
//! 对应 `pomodoro-hook.js` 的 `writeCursor` / `cursorEventAlias` / `cursorToolOf` /
//! `cursorDetailOf` / `runCursorMode`。
//!
//! Cursor 与别家最大的不同：
//!  * 配置是 `~/.cursor/hooks.json`，事件名是 **camelCase**（`preToolUse`），
//!    而输出字段是 **snake_case**（`updated_input` / `agent_message`）；
//!  * **没有独立的 PermissionRequest 事件** → 由 `beforeShellExecution` /
//!    `preToolUse` / `beforeMCPExecution` / `beforeReadFile` 四条接管审批；
//!  * 它原生支持 `permission: "ask"`，所以「没拿到决策」时可以体面地交回去，
//!    不用像 VS Code 那样硬拒。

use serde_json::{json, Value};

use crate::ancli::{post_event, post_interaction, HookResult};
use crate::cache::{local_rule_add, local_rule_match};
use crate::http::Gateway;
use crate::proto::{
    build_answer_map, is_ask_tool, questions_from_tool_input, summarize_tool_input, timeout_ms,
    tool_input_questions,
};
use crate::session::{build_context, remember_context, with_extra};
use crate::util::{is_truthy, pick, pick_str};

/// Cursor 事件名 → 复用同一套上下文记录时用的「标准（PascalCase）事件名」。
///
/// ⚠ `afterAgentResponse` 映射到 `Stop` 是**只用它留最后一段回复**，
/// 不当作回合结束（真正的回合结束是 `stop`）。搞混会让「任务完成」通知发两次。
fn event_alias(event: &str) -> &'static str {
    match event {
        "beforeShellExecution" | "beforeMCPExecution" | "beforeReadFile" | "preToolUse" => {
            "PreToolUse"
        }
        "afterShellExecution" | "afterMCPExecution" | "afterFileEdit" | "postToolUse"
        | "afterAgentThought" => "PostToolUse",
        "postToolUseFailure" => "PostToolUseFailure",
        "beforeSubmitPrompt" => "UserPromptSubmit",
        "sessionStart" => "SessionStart",
        "sessionEnd" => "SessionEnd",
        "subagentStart" => "SubagentStart",
        "subagentStop" => "SubagentStop",
        "stop" => "Stop",
        "preCompact" => "PreCompact",
        "afterAgentResponse" => "Stop",
        _ => "",
    }
}

fn tool_of(payload: &Value, event: &str) -> String {
    match event {
        "beforeShellExecution" => "Shell".to_string(),
        "beforeReadFile" => "Read".to_string(),
        _ => {
            let t = pick_str(payload, &["tool_name", "tool", "name"]);
            if t.is_empty() {
                "tool".to_string()
            } else {
                t
            }
        }
    }
}

fn detail_of(payload: &Value, event: &str) -> String {
    if event == "beforeShellExecution" {
        return pomodoro_core::collapse(&pick_str(payload, &["command"]), 160);
    }
    if event == "beforeReadFile" {
        return pomodoro_core::collapse(&pick_str(payload, &["file_path"]), 160);
    }
    // ⚠ 照抄 JS 的 `payload.tool_input || payload.tool_input === '' ? payload.tool_input
    // : payload.toolInput` —— 注意它其实是 `(a || (a === '')) ? a : b`（`?:` 优先级低于 `||`）。
    // 也就是说「tool_input 存在且（truthy 或恰好是空串）」才用它，否则用 toolInput。
    let ti = match payload.get("tool_input") {
        Some(v) if is_truthy(v) => Some(v),
        Some(Value::String(s)) if s.is_empty() => Some(payload.get("tool_input").unwrap()),
        _ => payload.get("toolInput"),
    };
    match ti {
        Some(Value::String(s)) => pomodoro_core::collapse(s, 160),
        Some(v) => pomodoro_core::collapse(&summarize_tool_input(v), 160),
        None => String::new(),
    }
}

/// 会话里记的 tool_input：`payload.tool_input || (command ? {command} : (file_path ? {file_path} : {}))`
fn session_tool_input(payload: &Value) -> Value {
    if let Some(v) = pick(payload, &["tool_input"]) {
        return v.clone();
    }
    if let Some(c) = pick(payload, &["command"]) {
        return json!({ "command": c });
    }
    if let Some(f) = pick(payload, &["file_path"]) {
        return json!({ "file_path": f });
    }
    json!({})
}

/// `runCursorMode(gw, payload)` —— 恒有输出（最少是一个 `{}`）。
pub fn run(gw: &Gateway, payload: &Value) -> HookResult {
    let event = pick_str(payload, &["hook_event_name"]);
    let roots = crate::util::arr(payload, "workspace_roots");
    let cwd = pick(payload, &["cwd"])
        .map(crate::util::as_str_lossy)
        .filter(|s| !s.is_empty())
        .or_else(|| roots.first().map(crate::util::as_str_lossy))
        .unwrap_or_default();
    let tool_input = session_tool_input(payload);

    // 归一成通用形状，复用上下文记录
    let session_payload = json!({
        "session_id": pick(payload, &["conversation_id", "generation_id"])
            .map(crate::util::as_str_lossy)
            .unwrap_or_default(),
        "cwd": cwd,
        "agent_type": pick(payload, &["subagent_type", "subagent"])
            .map(crate::util::as_str_lossy)
            .unwrap_or_default(),
        "prompt": pick_str(payload, &["prompt"]),
        "tool_name": tool_of(payload, &event),
        "tool_input": tool_input,
        "last_assistant_message": pick_str(payload, &["text"]),
    });
    let session = remember_context(&session_payload, event_alias(&event), "cursor");

    let base_ctx = build_context(
        &json!({ "session_id": pick_str(payload, &["conversation_id"]), "cwd": cwd }),
        "cursor",
        session.as_ref(),
    );
    let tool = tool_of(payload, &event);
    let detail = detail_of(payload, &event);
    let ctx = with_extra(
        base_ctx,
        &[("tool", json!(tool)), ("toolDetail", json!(detail))],
    );

    // ---- 可拦截事件：问答 / 权限 ----
    let blocking = matches!(
        event.as_str(),
        "preToolUse" | "beforeMCPExecution" | "beforeShellExecution" | "beforeReadFile"
    );
    if blocking {
        let ms = timeout_ms();

        // 提问：Cursor 的 preToolUse 支持 updated_input，注入答案后放行
        if event == "preToolUse"
            && is_ask_tool(payload)
            && pomodoro_core::env_flag("POMODORO_ASK", true)
        {
            let questions = questions_from_tool_input(&tool_input_questions(payload));
            if !questions.is_empty() {
                let question_text = if questions.len() == 1 {
                    pick_str(&questions[0], &["question"])
                } else {
                    format!("{} 个问题等待回答", questions.len())
                };
                let r = post_interaction(
                    gw,
                    &json!({
                        "kind": "ask",
                        "source": "cursor",
                        "title": "Agent 提问",
                        "message": question_text,
                        "questions": questions,
                        "context": with_extra(ctx.clone(), &[("toolDetail", json!(""))]),
                        "timeoutMs": ms,
                    }),
                    ms,
                )?;
                let decided = pick_str(&r, &["decidedBy"]) == "user";
                let action = pick_str(&r, &["action"]);
                let text = pick_str(&r, &["text"]);
                if decided && action == "submit" {
                    let answers = build_answer_map(
                        &questions,
                        crate::util::get(&r, "answers").unwrap_or(&Value::Null),
                    );
                    let mut updated = match &tool_input {
                        Value::Object(o) => o.clone(),
                        _ => serde_json::Map::new(),
                    };
                    updated.insert("answers".into(), answers);
                    return Ok(Some(
                        json!({ "permission": "allow", "updated_input": Value::Object(updated) }),
                    ));
                }
                if decided && (action == "cancel" || action == "deny") {
                    return Ok(Some(json!({
                        "permission": "deny",
                        "user_message": if text.is_empty() { "已在番茄钟弹窗中取消".to_string() } else { text.clone() },
                        "agent_message": if text.is_empty() { "用户取消了这次提问".to_string() } else { text },
                    })));
                }
                return Ok(Some(json!({
                    "permission": "ask",
                    "user_message": "番茄钟未获得回答，交回 Cursor 原生提问",
                })));
            }
        }

        // 权限：默认接管（Cursor 没有单独的 PermissionRequest 事件）
        if pomodoro_core::env_flag("POMODORO_PERMISSION", true) {
            // 本地「始终允许」规则：Cursor 的 preToolUse 不回写规则，只能自己记
            if local_rule_match("cursor", &tool, &detail) {
                return Ok(Some(if event == "beforeReadFile" {
                    json!({ "permission": "allow" })
                } else {
                    json!({ "permission": "allow", "agent_message": "命中番茄钟「始终允许」规则" })
                }));
            }
            let is_shell = event == "beforeShellExecution";
            let r = post_interaction(
                gw,
                &json!({
                    "kind": "permission",
                    "source": "cursor",
                    "title": if is_shell { "允许执行命令？".to_string() } else { format!("{tool} 需要授权") },
                    "message": if is_shell { pick_str(payload, &["command"]) } else { String::new() },
                    "detail": if is_shell { format!("cwd: {cwd}") } else { summarize_tool_input(&tool_input) },
                    "permission": { "tool": tool, "rule": detail, "suggestions": [], "canAlways": true },
                    "context": ctx,
                    "timeoutMs": ms,
                }),
                ms,
            )?;
            if pick_str(&r, &["decidedBy"]) == "user" {
                let action = pick_str(&r, &["action"]);
                let text = pick_str(&r, &["text"]);
                if action == "allow" || action == "allow-always" {
                    if action == "allow-always" {
                        local_rule_add("cursor", &tool, &detail);
                    }
                    // beforeReadFile 只认 permission，多带字段会被判为非法输出
                    return Ok(Some(if event == "beforeReadFile" {
                        json!({ "permission": "allow" })
                    } else {
                        json!({
                            "permission": "allow",
                            "agent_message": if text.is_empty() { "用户在番茄钟弹窗中允许".to_string() } else { text },
                        })
                    }));
                }
                return Ok(Some(if event == "beforeReadFile" {
                    json!({ "permission": "deny" })
                } else {
                    json!({
                        "permission": "deny",
                        "user_message": if text.is_empty() { "已在番茄钟弹窗中拒绝".to_string() } else { text.clone() },
                        "agent_message": if text.is_empty() { "用户通过番茄钟弹窗拒绝".to_string() } else { format!("用户拒绝：{text}") },
                    })
                }));
            }
            // 超时 / 被关闭：交回 Cursor 原生确认（**不是**硬拒 —— Cursor 原生支持 ask）
            return Ok(Some(json!({
                "permission": "ask",
                "user_message": "番茄钟未获得决策，交回 Cursor 原生确认",
            })));
        }

        // 不接管权限：不回任何决定，交回 Cursor 原生审批流程。
        // （以前这里回 `permission:allow`，等于替用户强制放行，属于越权）
        return Ok(Some(json!({})));
    }

    // ---- 非拦截事件：只计数 / 通知，不回任何决定 ----
    if event == "beforeSubmitPrompt" {
        // 记录任务后放行，不阻断用户提示词
        return Ok(Some(json!({ "continue": true })));
    }
    if event == "stop" {
        post_event(gw, &json!({ "kind": "stop", "source": "cursor", "context": ctx }))?;
        // 不要 followup_message，别把 agent 拖进循环
        return Ok(Some(json!({})));
    }

    let kind = match event.as_str() {
        "sessionStart" => "session-start",
        "sessionEnd" => "session-end",
        "subagentStart" => "session-start",
        "subagentStop" => "subagent-stop",
        "preCompact" => "notification",
        "postToolUse" | "postToolUseFailure" | "afterShellExecution" | "afterMCPExecution"
        | "afterFileEdit" | "afterAgentThought" => "tool-after",
        _ => "",
    };
    if !kind.is_empty() {
        let mut body = json!({
            "kind": kind,
            "source": "cursor",
            "tool": tool,
            "message": if kind == "notification" { event } else { String::new() },
        });
        // JS 里这里是 `context: cond ? ctx : undefined` —— undefined 会被
        // JSON.stringify **整个丢掉**，所以这里也必须"不加这个键"，不能写成 null
        if kind == "notification" || kind == "stop" {
            body["context"] = ctx;
        }
        post_event(gw, &body)?;
        return Ok(Some(json!({})));
    }
    Ok(Some(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_aliases_cover_cursor_casing() {
        assert_eq!(event_alias("preToolUse"), "PreToolUse");
        assert_eq!(event_alias("beforeReadFile"), "PreToolUse");
        assert_eq!(event_alias("afterAgentThought"), "PostToolUse");
        assert_eq!(event_alias("stop"), "Stop");
        // afterAgentResponse 只用来留最后一段回复，映射成 Stop 但不当回合结束
        assert_eq!(event_alias("afterAgentResponse"), "Stop");
        // 没映射的事件返回空串（remember_context 会跳过 switch）
        assert_eq!(event_alias("workspaceOpen"), "");
    }

    #[test]
    fn tool_names_are_synthesized_for_non_tool_events() {
        assert_eq!(tool_of(&json!({}), "beforeShellExecution"), "Shell");
        assert_eq!(tool_of(&json!({}), "beforeReadFile"), "Read");
        // 其它事件从 payload 里取，取不到落 "tool"（不是空串 —— 空串会让弹窗标题变空）
        assert_eq!(tool_of(&json!({}), "preToolUse"), "tool");
        assert_eq!(tool_of(&json!({ "tool_name": "Edit" }), "preToolUse"), "Edit");
    }

    #[test]
    fn detail_falls_back_through_tool_input_shapes() {
        // beforeShellExecution 只看 command
        assert_eq!(detail_of(&json!({ "command": "npm test" }), "beforeShellExecution"), "npm test");
        // beforeReadFile 只看 file_path
        assert_eq!(detail_of(&json!({ "file_path": "a/b.ts" }), "beforeReadFile"), "a/b.ts");
        // tool_input 是字符串时直接用（不拼 JSON）
        assert_eq!(detail_of(&json!({ "tool_input": "raw text" }), "preToolUse"), "raw text");
        // tool_input 是对象 → summarizeToolInput
        assert_eq!(
            detail_of(&json!({ "tool_input": { "a": "1" } }), "preToolUse"),
            "a: 1"
        );
        // tool_input 恰好是空串 → 仍然用 tool_input（JS 那句 `|| a === ''`）
        assert_eq!(detail_of(&json!({ "tool_input": "", "toolInput": { "b": "2" } }), "preToolUse"), "");
        // tool_input 缺失 → 退回 toolInput
        assert_eq!(
            detail_of(&json!({ "toolInput": { "b": "2" } }), "preToolUse"),
            "b: 2"
        );
        // 多行会被折平
        assert_eq!(detail_of(&json!({ "command": "a\nb" }), "beforeShellExecution"), "a b");
    }

    #[test]
    fn session_tool_input_prefers_command_then_file_path() {
        assert_eq!(session_tool_input(&json!({ "tool_input": { "x": 1 } }))["x"], 1);
        assert_eq!(session_tool_input(&json!({ "command": "ls" }))["command"], "ls");
        assert_eq!(session_tool_input(&json!({ "file_path": "a.ts" }))["file_path"], "a.ts");
        assert!(session_tool_input(&json!({})).as_object().unwrap().is_empty());
    }
}
