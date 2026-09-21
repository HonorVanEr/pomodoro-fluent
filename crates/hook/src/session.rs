//! 会话上下文：每个 hook 都是**独立进程**，靠临时文件记住「这个会话在干什么」。
//!
//! 对应 `pomodoro-hook.js` 的 `session*` / `collapse` / `shortSession` /
//! `buildContext` / `rememberContext`。
//!
//! 记四样东西：任务提示词（`UserPromptSubmit` 里的 `prompt`）、子 agent 类型、
//! 最近动过的工具、项目目录。用途只有一个 —— 弹窗上显示
//! 「哪个 agent / 哪个任务 / 在动哪个工具」，而不是光一个来源徽标。

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::cache::{cache_dir, now_ms};
use crate::util::{basename, is_truthy, map, pick, pick_str, sha1_hex_prefix, short_session};

/// 会话记录有效期：12 小时（一个工作日之内）。
pub const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1000;

fn session_file(session_id: &str) -> PathBuf {
    // JS: `String(sessionId || 'unknown')` —— 空串落成 `unknown`。
    // 实际上 read/merge 都会先挡掉空 id，这条只是照抄，不留分叉的口子。
    let key_input = if session_id.is_empty() { "unknown" } else { session_id };
    let key = sha1_hex_prefix(key_input, 16);
    cache_dir().join(format!("session-{key}.json"))
}

/// `sessionRead(sessionId)`。空 id / 过期 / 坏文件 → `None`。
pub fn read(session_id: &str) -> Option<Value> {
    if session_id.is_empty() {
        return None;
    }
    let text = std::fs::read_to_string(session_file(session_id)).ok()?;
    let s: Value = serde_json::from_str(&text).ok()?;
    if !is_truthy(&s) {
        return None;
    }
    // JS: `Date.now() - (s.at || 0) > TTL`
    let at = crate::util::get(&s, "at").and_then(Value::as_i64).unwrap_or(0);
    if now_ms() - at > SESSION_TTL_MS {
        return None;
    }
    Some(s)
}

/// `sessionMerge(sessionId, patch)`：浅合并 + 强制覆盖 `sessionId` / `at`。
pub fn merge(session_id: &str, patch: Map<String, Value>) -> Option<Value> {
    if session_id.is_empty() {
        return None;
    }
    let dir = cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    // `{ ...(read || {}), ...patch, sessionId, at }`：
    // 开 preserve_order 的 Map 是 IndexMap，`insert` 命中已有键时**原地改值、保持位置**，
    // 与 JS 对象展开的键序行为一致（这点很重要：会话文件会被两版交替读写）。
    let mut next = match read(session_id) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    for (k, v) in patch {
        next.insert(k, v);
    }
    next.insert("sessionId".into(), json!(session_id));
    next.insert("at".into(), json!(now_ms()));

    let value = Value::Object(next);
    if std::fs::write(session_file(session_id), value.to_string()).is_err() {
        return None;
    }
    Some(value)
}

/// `listSessions()`：全部未过期的会话，按 `at` 倒序。
pub fn list() -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("session-") && name.ends_with(".json")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(s) = serde_json::from_str::<Value>(&text) else {
            continue; // 坏文件跳过
        };
        let at = crate::util::get(&s, "at").and_then(Value::as_i64).unwrap_or(0);
        if now_ms() - at <= SESSION_TTL_MS {
            out.push(s);
        }
    }
    out.sort_by_key(|s| {
        std::cmp::Reverse(crate::util::get(s, "at").and_then(Value::as_i64).unwrap_or(0))
    });
    out
}

// ---------------------------------------------------------------------------
// 上下文组装
// ---------------------------------------------------------------------------

/// `buildContext(payload, source, session, extra)`
///
/// 字段名是 camelCase 且**会直接进弹窗**（网关的 `payload::normalize_context`
/// 按这 8 个字段取值），所以键名一个字都不能改。
pub fn build_context(payload: &Value, source: &str, session: Option<&Value>) -> Value {
    let empty = Value::Null;
    let s = session.unwrap_or(&empty);
    let p = payload;

    let cwd = pick(p, &["cwd", "working_directory"])
        .map(crate::util::as_str_lossy)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            pick(s, &["cwd"])
                .map(crate::util::as_str_lossy)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();

    let project = if cwd.is_empty() {
        pick_str(s, &["project"])
    } else {
        basename(&cwd)
    };

    let session_short = short_session(&pick_str(p, &["session_id", "sessionId"]))
        .to_string();
    let session_short = if session_short.is_empty() {
        short_session(&pick_str(s, &["sessionId"]))
    } else {
        session_short
    };

    let mut ctx = map();
    ctx.insert("agent".into(), json!(source));
    ctx.insert(
        "agentType".into(),
        json!(first_non_empty(&[
            pick_str(p, &["agent_type", "agentType"]),
            pick_str(s, &["agentType"]),
        ])),
    );
    ctx.insert(
        "agentId".into(),
        json!(first_non_empty(&[
            pick_str(p, &["agent_id"]),
            pick_str(s, &["agentId"]),
        ])),
    );
    ctx.insert("session".into(), json!(session_short));
    ctx.insert("project".into(), json!(project));
    ctx.insert(
        "task".into(),
        json!(pomodoro_core::collapse(&pick_str(s, &["task"]), 160)),
    );
    ctx.insert("tool".into(), json!(pick_str(s, &["lastTool"])));
    ctx.insert("toolDetail".into(), json!(pick_str(s, &["lastToolDetail"])));
    Value::Object(ctx)
}

/// 把 `{...ctx, tool, toolDetail: x}` 这类覆盖合进去（JS 的 `{...ctx, ...extra}`）。
///
/// ⚠ 覆盖空串是**有意义的**：`handleAsk` 会把 `toolDetail` 覆盖成 `''`，
/// 让提问弹窗不去显示「在动哪个文件」。所以这里不能"空值就跳过"。
pub fn with_extra(ctx: Value, extra: &[(&str, Value)]) -> Value {
    let mut base = match ctx {
        Value::Object(o) => o,
        _ => Map::new(),
    };
    for (k, v) in extra {
        base.insert((*k).to_string(), v.clone());
    }
    Value::Object(base)
}

fn first_non_empty(list: &[String]) -> String {
    list.iter().find(|s| !s.is_empty()).cloned().unwrap_or_default()
}

/// `rememberContext(payload, event, source)`
///
/// `event` 用的是**归一化后**的事件名（Cursor 的 camelCase 事件会先经
/// `cursor_event_alias` 转成 PascalCase 再进来）。
pub fn remember_context(payload: &Value, event: &str, source: &str) -> Option<Value> {
    let session_id = pick_str(payload, &["session_id", "sessionId"]);
    if session_id.is_empty() {
        return None;
    }

    let mut patch = Map::new();
    if !source.is_empty() {
        patch.insert("source".into(), json!(source));
    }

    let cwd = pick(payload, &["cwd", "working_directory"])
        .map(crate::util::as_str_lossy)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::util::arr(payload, "workspace_roots")
                .first()
                .map(crate::util::as_str_lossy)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    if !cwd.is_empty() {
        patch.insert("cwd".into(), json!(cwd));
    }

    let agent_type = pick_str(payload, &["agent_type", "agentType"]);
    if !agent_type.is_empty() {
        patch.insert("agentType".into(), json!(agent_type));
    }

    match event {
        "SessionStart" => {
            // 新会话：清掉上一轮的任务描述，否则弹窗会一直显示上一个任务
            patch.insert("task".into(), json!(""));
            if let Some(model) = pick(payload, &["model"]) {
                patch.insert("model".into(), json!(crate::util::as_str_lossy(model)));
            }
        }
        "UserPromptSubmit" => {
            if let Some(prompt) = pick(payload, &["prompt"]) {
                patch.insert(
                    "task".into(),
                    json!(pomodoro_core::collapse(
                        &crate::util::as_str_lossy(prompt),
                        400
                    )),
                );
            }
        }
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest" => {
            let tool = pick_str(payload, &["tool_name"]);
            if !tool.is_empty() {
                let input = pick(payload, &["tool_input"]).cloned().unwrap_or(Value::Null);
                patch.insert("lastTool".into(), json!(tool));
                patch.insert(
                    "lastToolDetail".into(),
                    json!(pomodoro_core::collapse(
                        &crate::proto::rule_content_for(&tool, &input),
                        160
                    )),
                );
            }
        }
        "Stop" => {
            if let Some(last) = pick(payload, &["last_assistant_message"]) {
                patch.insert(
                    "lastAssistant".into(),
                    json!(pomodoro_core::collapse(&crate::util::as_str_lossy(last), 200)),
                );
            }
        }
        _ => {}
    }

    if patch.is_empty() {
        return read(&session_id);
    }
    merge(&session_id, patch).or_else(|| read(&session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unique_id(tag: &str) -> String {
        format!("test-{tag}-{}", now_ms())
    }

    #[test]
    fn merge_then_read_roundtrip() {
        let id = unique_id("sess");
        assert!(read(&id).is_none());
        let mut patch = Map::new();
        patch.insert("source".into(), json!("zcode"));
        let written = merge(&id, patch).unwrap();
        assert_eq!(written["sessionId"], id.as_str());
        assert_eq!(written["source"], "zcode");

        let back = read(&id).unwrap();
        assert_eq!(back["source"], "zcode");
        // 空 id 一律 None（JS 的 `if (!sessionId) return null`）
        assert!(read("").is_none());
        assert!(merge("", Map::new()).is_none());
    }

    #[test]
    fn merge_is_shallow_and_forces_ids() {
        let id = unique_id("force");
        let mut p1 = Map::new();
        p1.insert("task".into(), json!("老任务"));
        p1.insert("sessionId".into(), json!("伪造的"));
        merge(&id, p1).unwrap();
        let mut p2 = Map::new();
        p2.insert("lastTool".into(), json!("Bash"));
        let merged = merge(&id, p2).unwrap();
        // sessionId 必须被强制写成真实 id，patch 里带的假值不能生效
        assert_eq!(merged["sessionId"], id.as_str());
        // 合并是浅的：老任务还在
        assert_eq!(merged["task"], "老任务");
        assert_eq!(merged["lastTool"], "Bash");
    }

    #[test]
    fn remember_context_records_task_and_tool() {
        let id = unique_id("ctx");
        let payload = json!({
            "session_id": id,
            "cwd": "C:\\proj\\alpha",
            "prompt": "  帮我把   这个重构一下 ",
        });
        let s = remember_context(&payload, "UserPromptSubmit", "codex").unwrap();
        assert_eq!(s["task"], "帮我把 这个重构一下");
        assert_eq!(s["cwd"], "C:\\proj\\alpha");
        assert_eq!(s["source"], "codex");

        let payload = json!({
            "session_id": id,
            "tool_name": "Bash",
            "tool_input": { "command": "npm test" },
        });
        let s = remember_context(&payload, "PreToolUse", "codex").unwrap();
        assert_eq!(s["lastTool"], "Bash");
        assert_eq!(s["lastToolDetail"], "npm test");

        // SessionStart 清任务
        let s = remember_context(&json!({ "session_id": id }), "SessionStart", "codex").unwrap();
        assert_eq!(s["task"], "");

        // 没有 sessionId → 不记（返回 None）
        assert!(remember_context(&json!({ "prompt": "x" }), "UserPromptSubmit", "codex").is_none());
    }

    #[test]
    fn build_context_shape_matches_normalized_fields() {
        let session = json!({ "task": "重构弹窗", "lastTool": "Bash", "lastToolDetail": "npm test" });
        let payload = json!({ "session_id": "abcdefghij", "agent_type": "Explore" });
        let ctx = build_context(&payload, "zcode", Some(&session));
        assert_eq!(ctx["agent"], "zcode");
        assert_eq!(ctx["agentType"], "Explore");
        // session 只露尾部 6 位
        assert_eq!(ctx["session"], "efghij");
        assert_eq!(ctx["task"], "重构弹窗");
        assert_eq!(ctx["tool"], "Bash");
        assert_eq!(ctx["toolDetail"], "npm test");
        // project 只在拿不到 cwd 时用会话里记的
        assert_eq!(ctx["project"], "");
        // 8 个字段一个不少（网关的 CTX_CAPS 正好 8 项）
        for k in [
            "agent", "agentType", "agentId", "session", "project", "task", "tool",
            "toolDetail",
        ] {
            assert!(ctx.get(k).is_some(), "缺字段 {k}");
        }
    }

    #[test]
    fn build_context_prefers_payload_cwd_over_session() {
        let session = json!({ "cwd": "C:\\old\\proj", "project": "老项目" });
        let payload = json!({ "cwd": "C:\\new\\alpha" });
        let ctx = build_context(&payload, "claude-code", Some(&session));
        assert_eq!(ctx["project"], "alpha");
        // 没有 payload.cwd 时**先退回 session.cwd**（JS 的 `p.cwd || p.working_directory || s.cwd`），
        // 而不是直接拿 session.project —— 这条顺序错了会让「换目录」的会话显示成老项目名
        let ctx = build_context(&json!({}), "claude-code", Some(&session));
        assert_eq!(ctx["project"], "proj");
        // 两层 cwd 都没有才用 session.project
        let session = json!({ "project": "只有项目名" });
        let ctx = build_context(&json!({}), "claude-code", Some(&session));
        assert_eq!(ctx["project"], "只有项目名");
        // working_directory 是 cwd 的别名
        let ctx = build_context(
            &json!({ "working_directory": "D:\\ws\\beta" }),
            "claude-code",
            None,
        );
        assert_eq!(ctx["project"], "beta");
    }

    #[test]
    fn with_extra_overrides_even_with_empty() {
        let ctx = json!({ "tool": "Bash", "toolDetail": "npm test" });
        let out = with_extra(ctx, &[("toolDetail", json!("")), ("tool", json!("AskUserQuestion"))]);
        assert_eq!(out["toolDetail"], "", "空串覆盖必须生效（提问弹窗靠它清掉工具详情）");
        assert_eq!(out["tool"], "AskUserQuestion");
    }

    #[test]
    fn list_is_sorted_newest_first() {        let a = unique_id("listA");
        let b = unique_id("listB");
        let mut pa = Map::new();
        pa.insert("task".into(), json!("A"));
        merge(&a, pa).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut pb = Map::new();
        pb.insert("task".into(), json!("B"));
        merge(&b, pb).unwrap();

        let all = list();
        let ia = all.iter().position(|s| s["task"] == "A");
        let ib = all.iter().position(|s| s["task"] == "B");
        assert!(ia.is_some() && ib.is_some());
        assert!(ib.unwrap() < ia.unwrap(), "新的要排在前面");
    }

    /// 🔒 与 Node 逐字节对齐的黄金值（参考值来自 `scripts/keygen-ref.mjs`）。
    ///
    /// 会话文件名也只差这个 SHA-1；算法一歪，两版就会各自读到一半的空会话，
    /// 弹窗上的「任务 / 项目」会在混用期随机丢失。
    #[test]
    fn session_key_matches_node_reference_values() {
        // `String(sessionId || 'unknown')` —— 空串与字面 'unknown' 同键
        assert_eq!(sha1_hex_prefix("unknown", 16), "50d8b4a941c26b89");
        assert_eq!(session_file(""), session_file("unknown"));
        assert_eq!(sha1_hex_prefix("abcdefghij", 16), "d68c19a0a345b7ea");
        assert_eq!(
            sha1_hex_prefix("1a2b3c4d-5e6f-7890-abcd-ef1234567890", 16),
            "4b953287f113b091"
        );
        // 文件名形状
        let name = session_file("abcdefghij")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(name, "session-d68c19a0a345b7ea.json");
    }
}
