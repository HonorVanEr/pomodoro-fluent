//! Claude Code / ZCode / VS Code Copilot / Qwen Code 同族的适配层。
//!
//! 对应 `pomodoro-hook.js` 的 `handleAsk` / `handlePermission` / `runAncliMode`。
//!
//! 这四家共用 Claude Code 的 hook 协议（`hook_event_name` + `hookSpecificOutput`），
//! 差异只体现在「有哪些事件」和「输出里哪些字段生效」上 —— 后者靠
//! [`crate::proto::is_permission_event_only`] 与 `POMODORO_ASK_MODE` 兜。

use serde_json::{json, Value};

use crate::cache::{cache_key_for, cache_read, cache_write, local_rule_add, local_rule_match};
use crate::http::{request, Gateway, HttpError};
use crate::proto::{
    build_answer_map, build_permission_updates, format_answer_map, is_ask_tool,
    is_permission_event_only, questions_from_tool_input, rule_content_for, summarize_tool_input,
    timeout_ms, tool_input_questions, EVENT_TIMEOUT_MS, WAIT_HTTP_MARGIN_MS,
};
use crate::session::{build_context, remember_context, with_extra};
use crate::util::{get, pick, pick_str};

/// 一个模式的返回值：`Ok(Some(json))` 表示要往 stdout 打这段决策；
/// `Ok(None)` 表示**什么都不输出** —— 宿主会退回到自己的原生询问流程。
pub type HookResult = Result<Option<Value>, HttpError>;

/// `postInteraction(gw, body, ms)`
pub fn post_interaction(gw: &Gateway, body: &Value, ms: u64) -> Result<Value, HttpError> {
    request(
        gw.port,
        &gw.token,
        "POST",
        "/api/interaction",
        Some(body),
        ms + WAIT_HTTP_MARGIN_MS,
    )
}

/// `postEvent(gw, body)`
pub fn post_event(gw: &Gateway, body: &Value) -> Result<Value, HttpError> {
    request(
        gw.port,
        &gw.token,
        "POST",
        "/api/event",
        Some(body),
        EVENT_TIMEOUT_MS,
    )
}

/// `runAncliMode(gw, payload)`
pub fn run(gw: &Gateway, payload: &Value) -> HookResult {
    let source = crate::proto::detect_source(payload);
    let event = pick_str(payload, &["hook_event_name"]);
    // 累积会话上下文（任务 / 子 agent / 最近工具 / 项目），供弹窗展示
    let session = remember_context(payload, &event, &source);
    let context = build_context(payload, &source, session.as_ref());

    // 提问（AskUserQuestion）：PreToolUse 与 PermissionRequest 都可能触发
    if is_ask_tool(payload) && pomodoro_core::env_flag("POMODORO_ASK", true) {
        return handle_ask(payload, &source, gw, &context);
    }

    // 权限请求
    if event == "PermissionRequest" && pomodoro_core::env_flag("POMODORO_PERMISSION", true) {
        return handle_permission(payload, &source, gw, &context);
    }

    // PreToolUse：2026-09-18 起只处理提问（上面那个分支）与活动上报，
    // **不再拦截/审批普通工具调用** —— 审批统一交给有 PermissionRequest 的宿主。

    let tool = pick_str(payload, &["tool_name"]);
    let mut body = match event.as_str() {
        "Notification" => json!({ "kind": "notification", "message": pick_str(payload, &["message"]), "source": source }),
        "Stop" => json!({ "kind": "stop", "source": source }),
        "SubagentStop" => json!({ "kind": "subagent-stop", "source": source }),
        "SubagentStart" => json!({ "kind": "subagent-start", "source": source }),
        "PostToolUse" => json!({ "kind": "tool-after", "tool": tool, "source": source }),
        "PreToolUse" => json!({ "kind": "tool-before", "tool": tool, "source": source }),
        "SessionStart" => json!({ "kind": "session-start", "source": source }),
        "SessionEnd" => json!({ "kind": "session-end", "source": source }),
        "PreCompact" => json!({ "kind": "pre-compact", "source": source }),
        "PostCompact" => json!({ "kind": "post-compact", "source": source }),
        "Interrupt" => json!({ "kind": "interrupt", "source": source }),
        "UserPromptSubmit" => json!({ "kind": "prompt", "source": source }),
        // 未识别的事件：静默忽略（绝不猜测，猜错会往网关灌垃圾事件）
        _ => return Ok(None),
    };

    if event == "Notification" && pick_str(&body, &["message"]).is_empty() {
        body["message"] = json!("Agent 需要你的确认");
    }
    // 通知类也带上上下文：弹窗能显示是哪个任务、在动哪个工具。
    // ⚠ UserPromptSubmit 必须带：任务名（`context.task`）就来自这里，
    // 漏了它弹窗上「在做什么」永远是空的。
    if matches!(event.as_str(), "Notification" | "Stop" | "UserPromptSubmit") {
        body["context"] = context;
    }

    // ⚠ Stop 只上报，**绝不**输出 `decision:"block"` —— 那会阻止 agent 收尾，
    // 甚至把它拖进自动续跑的循环（VS Code 的 `stop_hook_active` 就是防这个的）
    post_event(gw, &body)?;
    Ok(None)
}

/// `handleAsk(payload, source, gw, context)`
fn handle_ask(payload: &Value, source: &str, gw: &Gateway, context: &Value) -> HookResult {
    let tool_input = pick(payload, &["tool_input", "toolInput"])
        .cloned()
        .unwrap_or_else(|| json!({}));
    let questions = questions_from_tool_input(&tool_input_questions(payload));
    if questions.is_empty() {
        return Ok(None);
    }
    let key = cache_key_for(payload);

    // 同一提问被 PreToolUse + PermissionRequest 双触发时复用首次决策
    let mut result = cache_read(&key);
    if result.is_none() {
        let ms = timeout_ms();
        let question_text = if questions.len() == 1 {
            pick_str(&questions[0], &["question"])
        } else {
            format!("{} 个问题等待回答", questions.len())
        };
        let body = json!({
            "kind": "ask",
            "source": source,
            "title": "Agent 提问",
            "message": question_text,
            "questions": questions,
            "context": with_extra(
                context.clone(),
                &[
                    ("tool", json!(first_or(&pick_str(payload, &["tool_name"]), "AskUserQuestion"))),
                    // 提问弹窗刻意不显示「在动哪个工具」——答案与工具详情无关，
                    // 显示了反而让人分不清这是提问还是审批
                    ("toolDetail", json!("")),
                ],
            ),
            "timeoutMs": ms,
        });
        let got = post_interaction(gw, &body, ms)?;
        cache_write(&key, &got);
        result = Some(got);
    }
    // JS: `if (!result) return null` —— 缓存里读到 null 也走这条
    let Some(result) = result.filter(crate::util::is_truthy) else {
        return Ok(None);
    };

    let answers = build_answer_map(&questions, get(&result, "answers").unwrap_or(&Value::Null));
    let decided = pick_str(&result, &["decidedBy"]) == "user";
    let event = pick_str(payload, &["hook_event_name"]);
    let action = pick_str(&result, &["action"]);

    // ---- 用户作答：注入 answers 让原生 UI 不再弹出 ----
    if decided && action == "submit" {
        let mut updated_input = match &tool_input {
            Value::Object(o) => o.clone(),
            _ => serde_json::Map::new(),
        };
        updated_input.insert("answers".into(), answers.clone());
        let updated_input = Value::Object(updated_input);

        // VS Code 的提问工具（`vscode/askQuestions`）弹的是 QuickPick，答案不在入参里、
        // 改 updatedInput 只换了问题本身，改不动用户选择 → 默认走 deny + 把答案写进原因。
        // 其它宿主默认走 updatedInput.answers（Claude Code / ZCode），可用
        // POMODORO_ASK_MODE 覆盖。
        let ask_mode = pomodoro_core::env_str("POMODORO_ASK_MODE")
            .unwrap_or_else(|| if source == "vscode" { "deny".into() } else { "answers".into() })
            .to_ascii_lowercase();
        if ask_mode == "deny" {
            let reason = format!("用户在番茄钟弹窗中的回答：\n{}", format_answer_map(&answers));
            return Ok(Some(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                    // VS Code 里 additionalContext 才是「给模型看」的字段，reason 只展示给用户
                    "additionalContext": reason,
                }
            })));
        }
        if event == "PermissionRequest" {
            return Ok(Some(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": { "behavior": "allow", "updatedInput": updated_input },
                }
            })));
        }
        return Ok(Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": "用户在番茄钟弹窗内作答",
                "updatedInput": updated_input,
            }
        })));
    }

    // ---- 用户显式取消 / 拒绝 ----
    if decided && (action == "cancel" || action == "deny") {
        let text = pick_str(&result, &["text"]);
        let reason = if !text.is_empty() {
            format!("用户取消：{text}")
        } else {
            "用户取消了这次提问（番茄钟弹窗）".to_string()
        };
        if event == "PermissionRequest" {
            return Ok(Some(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": { "behavior": "deny", "message": reason },
                }
            })));
        }
        return Ok(Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })));
    }

    // 超时 / 被顶掉：不输出决策，交回终端原生 UI
    Ok(None)
}

/// `handlePermission(payload, source, gw, context)`
fn handle_permission(payload: &Value, source: &str, gw: &Gateway, context: &Value) -> HookResult {
    let tool = first_or(&pick_str(payload, &["tool_name"]), "工具");
    let tool_input = pick(payload, &["tool_input"])
        .cloned()
        .unwrap_or_else(|| json!({}));
    let rule = rule_content_for(&tool, &tool_input);

    // 宿主不认 updatedPermissions 时，靠本地规则实现「始终允许」
    if local_rule_match(source, &tool, &rule) {
        return Ok(Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow", "message": "命中本地「始终允许」规则" },
            }
        })));
    }

    let ms = timeout_ms();
    let suggestions = crate::util::arr(payload, "permission_suggestions").to_vec();
    let body = json!({
        "kind": "permission",
        "source": source,
        "title": format!("允许 {tool}？"),
        "message": pick_str(payload, &["message"]),
        "detail": summarize_tool_input(&tool_input),
        "permission": {
            "tool": tool,
            "rule": rule,
            "suggestions": suggestions,
            "canAlways": pomodoro_core::env_flag("POMODORO_ALWAYS_ALLOW", true),
        },
        "context": with_extra(
            context.clone(),
            &[
                ("tool", json!(tool)),
                ("toolDetail", json!(pomodoro_core::collapse(&rule, 160))),
            ],
        ),
        "timeoutMs": ms,
    });
    let result = post_interaction(gw, &body, ms)?;

    // 未收到决策：不输出，交回宿主原生审批
    if !crate::util::is_truthy(&result) {
        return Ok(None);
    }
    let decided = pick_str(&result, &["decidedBy"]) == "user";
    let action = pick_str(&result, &["action"]);
    let text = pick_str(&result, &["text"]);
    if !decided {
        return Ok(None);
    }

    if action == "allow" {
        let message = if text.is_empty() {
            "用户通过番茄钟弹窗允许".to_string()
        } else {
            text
        };
        return Ok(Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow", "message": message },
            }
        })));
    }

    if action == "allow-always" {
        let updated_permissions = build_permission_updates(&tool, &tool_input, &suggestions);
        // 兜底：宿主不认 updatedPermissions 时也生效
        local_rule_add(source, &tool, &rule);
        let mut decision = json!({
            "behavior": "allow",
            "message": "用户选择始终允许（已记入番茄钟本地规则）",
        });
        // ⚠ Codex 遇到不支持的字段会 **fail closed**（整条答复作废），
        // 绝不能把 updatedPermissions 塞给它 —— 那会让用户点了「始终允许」却什么都没发生
        if !is_permission_event_only(source) {
            decision["updatedPermissions"] = json!(updated_permissions);
        }
        return Ok(Some(json!({
            "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
        })));
    }

    if action == "deny" {
        let message = if text.is_empty() {
            "用户通过番茄钟弹窗拒绝".to_string()
        } else {
            format!("用户拒绝：{text}")
        };
        return Ok(Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "deny", "message": message },
            }
        })));
    }

    Ok(None)
}

/// `pick_str` 的空值兜底（JS 的 `a || b`）。
fn first_or(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}
