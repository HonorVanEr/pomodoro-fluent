//! 手动调试子命令：`ask` / `permission` / `notify`。
//!
//! 对应 `pomodoro-hook.js` 的 `collectFlag` / `manualContext` / `runManualAsk` /
//! `runManualPermission`。
//!
//! 它们的存在意义是**不依赖任何 agent 就能验证弹窗**：
//! `pomodoro-hook.exe ask --question "继续吗？" --option 继续 --option 停下`
//! 就能看到提问窗长什么样、答案怎么回来。排"某个宿主的 hook 没生效"时，
//! 先用它确认番茄钟这一侧是好的，能省掉一半时间。

use serde_json::{json, Value};

use crate::ancli::{post_interaction, HookResult};
use crate::http::Gateway;
use crate::install::collect_flag;
use crate::proto::timeout_ms;
use crate::session::with_extra;

/// `collectFlag(args, name)[0] || ''`
fn first(args: &[String], name: &str) -> String {
    collect_flag(args, name).into_iter().next().unwrap_or_default()
}

/// `manualContext(args)` —— 手动调试时的上下文，用于验证弹窗上的
/// 「任务 / agent / 工具」展示。
fn manual_context(args: &[String]) -> Value {
    let project = first(args, "project");
    json!({
        "agent": if first(args, "agent").is_empty() { "manual".to_string() } else { first(args, "agent") },
        "agentType": first(args, "agent-type"),
        "session": "manual",
        "project": if project.is_empty() { "manual".to_string() } else { project },
        "task": first(args, "task"),
        "tool": "",
        "toolDetail": "",
    })
}

/// `runManualAsk(gw, args)`
pub fn run_ask(gw: &Gateway, args: &[String]) -> HookResult {
    let raw_questions = {
        let q = collect_flag(args, "question");
        if q.is_empty() {
            vec!["手动测试提问".to_string()]
        } else {
            q
        }
    };
    // ⚠ 选项列表是**所有题目共用**的（JS 在 map 里对每题重新算一遍，结果相同）
    let options: Vec<Value> = collect_flag(args, "option")
        .into_iter()
        .enumerate()
        .map(|(j, o)| json!({ "id": format!("o{j}"), "label": o, "description": "" }))
        .collect();

    let questions: Vec<Value> = raw_questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            json!({
                "id": format!("q{i}"),
                "question": q,
                "header": "测试",
                "multiSelect": false,
                // 手动提问永远给输入框：调试时最需要的就是"随便打点什么"
                "custom": true,
                "options": options,
            })
        })
        .collect();

    let ms = timeout_ms();
    let r = post_interaction(
        gw,
        &json!({
            "kind": "ask",
            "source": "manual",
            "title": "手动测试 · 提问",
            "questions": questions,
            "context": with_extra(manual_context(args), &[("tool", json!("AskUserQuestion"))]),
            "timeoutMs": ms,
        }),
        ms,
    )?;
    crate::print_pretty_line(&r);
    Ok(None)
}

/// `runManualPermission(gw, args)`
pub fn run_permission(gw: &Gateway, args: &[String]) -> HookResult {
    let tool = {
        let t = first(args, "tool");
        if t.is_empty() {
            "Bash".to_string()
        } else {
            t
        }
    };
    let detail = first(args, "detail");
    let ms = timeout_ms();
    // ⚠ 这里**不带 `message` 字段**（照抄 JS）——手动权限窗只显示工具与详情
    let r = post_interaction(
        gw,
        &json!({
            "kind": "permission",
            "source": "manual",
            "title": "手动测试 · 权限",
            "detail": detail,
            "permission": {
                "tool": tool,
                "rule": if detail.is_empty() { "*".to_string() } else { detail.clone() },
                "canAlways": true,
            },
            "context": with_extra(
                manual_context(args),
                &[("tool", json!(tool)), ("toolDetail", json!(detail))],
            ),
            "timeoutMs": ms,
        }),
        ms,
    )?;
    crate::print_pretty_line(&r);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn manual_context_defaults_to_manual() {
        let ctx = manual_context(&args(&[]));
        assert_eq!(ctx["agent"], "manual");
        assert_eq!(ctx["project"], "manual");
        assert_eq!(ctx["session"], "manual");
        assert_eq!(ctx["task"], "");
        let ctx = manual_context(&args(&["--agent", "zcode", "--task", "重构", "--project", "p"]));
        assert_eq!(ctx["agent"], "zcode");
        assert_eq!(ctx["task"], "重构");
        assert_eq!(ctx["project"], "p");
    }

    #[test]
    fn ask_options_are_shared_across_questions() {
        let options: Vec<Value> = collect_flag(&args(&["--option", "A", "--option", "B"]), "option")
            .into_iter()
            .enumerate()
            .map(|(j, o)| json!({ "id": format!("o{j}"), "label": o, "description": "" }))
            .collect();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["id"], "o0");
        assert_eq!(options[1]["label"], "B");
        // 没有 --question 时 JS 会给一个默认题目
        let qs = collect_flag(&args(&[]), "question");
        assert!(qs.is_empty());
    }

    #[test]
    fn tool_defaults_to_bash_and_rule_to_star() {
        let tool = {
            let t = first(&args(&[]), "tool");
            if t.is_empty() {
                "Bash".to_string()
            } else {
                t
            }
        };
        assert_eq!(tool, "Bash");
        let detail = first(&args(&[]), "detail");
        let rule = if detail.is_empty() { "*".to_string() } else { detail };
        assert_eq!(rule, "*");
    }
}
