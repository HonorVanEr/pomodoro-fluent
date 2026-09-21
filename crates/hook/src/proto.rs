//! 协议识别与载荷归一 —— 对应 `pomodoro-hook.js` 第 454–544 行与第 341–426 行。
//!
//! 这一层**全是纯函数**（唯一例外是 `detect_source` 要看两个目录是否存在），
//! 因为它是整个 hook 里最容易出错、也最难在真机上复现的部分：八个宿主的
//! payload 形状各不相同，且大多没有文档，只能靠实机抓包反推。

use serde_json::{json, Map, Value};

use crate::util::{as_str_lossy, get, is_plain_object, is_truthy, pick, pick_str};

// ---------------------------------------------------------------------------
// 时间预算（三层嵌套，顺序不能乱）
//
//   网关兜底     POMODORO_TIMEOUT_S（默认 3600s）：到点回「未决策」，交还宿主原生询问
//   hook 等网关  兜底 + WAIT_HTTP_MARGIN_MS：给往返与渲染留余量
//   宿主超时     HOST_WAIT_*：必须排最后。宿主若先到点，它会直接杀掉 hook，
//              那句「未决策」根本发不出去，宿主就按自己的审批设置走了（可能静默放行）
// ---------------------------------------------------------------------------

/// hook 等网关时在番茄钟兜底之上多留的余量。
pub const WAIT_HTTP_MARGIN_MS: u64 = 5 * 60 * 1000;
/// 写进**秒制**宿主配置的 hook timeout（VS Code / Trae / Cursor / Qwen / Claude Code）。
pub const HOST_WAIT_SEC: u64 = 4_200;
/// 写进**毫秒制**宿主配置的 hook timeout（ZCode）。
pub const HOST_WAIT_MS: u64 = 4_200 * 1_000;
/// 事件上报（非长轮询）的超时，8s 足够。
pub const EVENT_TIMEOUT_MS: u64 = 8_000;

/// 这些宿主的审批**只走 `PermissionRequest`**。
///
/// Codex 的 `PreToolUse` 只强制执行 `deny`，`allow` / `ask` 是「被解析但不生效」
/// （实机文案：`PreToolUse hook returned unsupported permissionDecision:allow`）。
/// 在 PreToolUse 上回 allow 等于什么都没回，宿主照样走自己的审批 → 那次审批又会
/// 触发 PermissionRequest → 弹两次窗。而且 Codex 的 PermissionRequest 只在
/// 「Codex 本来就要问用户」时才触发，条件比 PreToolUse 精确得多。
///
/// 这个集合还决定「始终允许」要不要回写 `updatedPermissions`：Codex 遇到不支持的
/// 字段会 **fail closed**（整条答复作废），所以只能靠本地规则落地。
pub const PERMISSION_EVENT_ONLY_SOURCES: [&str; 1] = ["codex"];

/// `timeoutMs()` —— 弹窗等待秒数。返回值单位是毫秒。
///
/// JS 是 `Math.min(Number(v) || 3600, 7200)` 再 `Math.max(5, Math.round(x)) * 1000`：
/// 非数字 / `0` / 未设置都落到 3600s，上限 7200s（2h），下限 5s。
pub fn timeout_ms() -> u64 {
    let raw = pomodoro_core::env_str("POMODORO_TIMEOUT_S")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v != 0.0 && v.is_finite())
        .unwrap_or(3600.0);
    let capped = raw.min(7200.0);
    let rounded = capped.round().max(5.0);
    (rounded as u64) * 1000
}

// ---------------------------------------------------------------------------
// 工具名归一化
// ---------------------------------------------------------------------------

/// `normalizeToolName(name)`
///
/// 各宿主命名风格差得很远：Claude / ZCode 用 `AskUserQuestion`，VS Code 用
/// `vscode/askQuestions`，Cursor 用 `question`，Qwen 沿用 Claude 的。
///
/// 三步顺序**不能换**：先去命名空间（`vscode/`、`copilot/`），再去非字母数字字符
/// （`_`、`-`、空格），再去前缀式 `vscode`，最后小写。
pub fn normalize_tool_name(name: &str) -> String {
    // 1) 去掉 `^[A-Za-z0-9_.-]+/`
    let no_ns = match name.find('/') {
        Some(i) if i > 0 && name[..i].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) => &name[i + 1..],
        _ => name,
    };
    // 2) 去掉一切非字母数字（含 `_` `-` 空格，以及所有非 ASCII）
    let alnum: String = no_ns.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    // 3) 去掉前缀式 vscode（`^vscode` 大小写不敏感）
    let lower = alnum.to_ascii_lowercase();
    let stripped = match lower.strip_prefix("vscode") {
        Some(rest) => rest.to_string(),
        None => lower,
    };
    stripped
}

const ASK_TOOL_SET: [&str; 6] = [
    "askuserquestion",
    "askuserquestions",
    "askuser",
    "askquestions",
    "askquestion",
    "question",
];

/// `isAskToolName(name)`
pub fn is_ask_tool_name(name: &str) -> bool {
    ASK_TOOL_SET.contains(&normalize_tool_name(name).as_str())
}

/// Cursor 的 hook 事件名是 camelCase（其余宿主是 PascalCase）。
///
/// 这张表同时被 `detect_protocol` 用来**判定协议**：事件名落在这张表里就是 Cursor。
pub const CURSOR_EVENTS: [&str; 21] = [
    "beforeShellExecution",
    "afterShellExecution",
    "beforeMCPExecution",
    "afterMCPExecution",
    "beforeReadFile",
    "afterFileEdit",
    "beforeSubmitPrompt",
    "afterAgentResponse",
    "afterAgentThought",
    "preToolUse",
    "postToolUse",
    "postToolUseFailure",
    "sessionStart",
    "sessionEnd",
    "subagentStart",
    "subagentStop",
    "preCompact",
    "stop",
    "afterTabFileEdit",
    "beforeTabFileRead",
    "workspaceOpen",
];

/// `isAskTool(payload)`
pub fn is_ask_tool(payload: &Value) -> bool {
    let tool = pick_str(payload, &["tool_name", "tool", "name"]);
    if is_ask_tool_name(&tool) {
        return true;
    }
    let ti = pick(payload, &["tool_input", "toolInput"]);
    let Some(ti) = ti else { return false };
    if is_plain_object(ti) {
        let qs = crate::util::arr(ti, "questions");
        if !qs.is_empty()
            && !is_truthy(&get(ti, "command").cloned().unwrap_or(Value::Null))
            && !is_truthy(&get(ti, "file_path").cloned().unwrap_or(Value::Null))
            && !is_truthy(&get(ti, "filePath").cloned().unwrap_or(Value::Null))
        {
            return true;
        }
        // VS Code Copilot 的提问工具入参形如 `{ question: "..." }`
        if get(ti, "question").map(|v| v.is_string()).unwrap_or(false)
            && !is_truthy(&get(ti, "command").cloned().unwrap_or(Value::Null))
        {
            return true;
        }
    }
    false
}

/// `toolInputQuestions(payload)` —— 提问入参兼容三种形状：
/// Claude / ZCode（`{questions}`）、VS Code（`{question, options}`）、Cursor。
pub fn tool_input_questions(payload: &Value) -> Value {
    let ti = pick(payload, &["tool_input", "toolInput"])
        .cloned()
        .unwrap_or_else(|| json!({}));
    let qs = crate::util::arr(&ti, "questions");
    if !qs.is_empty() {
        return ti;
    }
    if let Some(q) = get(&ti, "question").and_then(Value::as_str) {
        // ⚠ 这条分支**刻意只保留 questions**：JS 在 `typeof ti.question === 'string'` 时
        // 新建了一个只有 questions 的对象，别的字段（command 之类）都不带过去。
        let opts = match get(&ti, "options") {
            Some(Value::Array(a)) => a.clone(),
            _ => crate::util::arr(&ti, "choices").to_vec(),
        };
        return json!({
            "questions": [{
                "question": q,
                "header": pick_str(&ti, &["header"]),
                "options": opts,
                "multiSelect": crate::util::pick_bool(&ti, &["multiSelect", "multiple"]),
            }]
        });
    }
    ti
}

// ---------------------------------------------------------------------------
// 来源与协议识别
// ---------------------------------------------------------------------------

/// `detectSource(payload)`
///
/// ⚠ **故意不做 `~/.codex` 目录探测** —— 装 Codex 的人往往同时装了别的宿主，用
/// 「目录存在」猜来源会把 Claude Code 的调用误判成 codex。`install` 生成的命令一律
/// 带 `--source codex`，下面那条 `turn_id` / `trigger` 只是手写配置时的兜底。
pub fn detect_source(payload: &Value) -> String {
    if let Some(s) = pomodoro_core::env_str("POMODORO_SOURCE") {
        return s;
    }
    if let Some(s) = pick(payload, &["source"]) {
        return as_str_lossy(s);
    }
    if pomodoro_core::env_str("ZCODE_PLUGIN_ROOT").is_some()
        || pomodoro_core::env_str("ZCODE_CLI_HOME").is_some()
    {
        return "zcode".into();
    }
    if pick(payload, &["cursor_version", "conversation_id"]).is_some() {
        return "cursor".into();
    }
    // Trae 的 payload 会额外带 llm_tool_name 与 workspace_roots
    if pick(payload, &["llm_tool_name"]).is_some()
        || (pick(payload, &["workspace_roots"]).is_some() && pick(payload, &["tool_use_id"]).is_some())
    {
        return "trae".into();
    }
    // Codex 的 payload 带 turn_id，PermissionRequest 还额外带 trigger
    if pick(payload, &["turn_id", "trigger"]).is_some() {
        return "codex".into();
    }
    let home = pomodoro_core::home_dir();
    if home.join(".zcode").exists() {
        return "zcode".into();
    }
    if home.join(".trae-cn").exists() {
        return "trae".into();
    }
    "claude-code".into()
}

/// `detectProtocol(payload)` → `"cursor" | "ancli" | "opencode-permission" |
/// "opencode-question" | "opencode-event"`
pub fn detect_protocol(payload: &Value) -> &'static str {
    let evt = get(payload, "hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if CURSOR_EVENTS.contains(&evt) {
        return "cursor";
    }
    if !evt.is_empty() {
        // Claude Code / ZCode / VS Code Copilot / Qwen Code 同族
        return "ancli";
    }
    if get(payload, "permission").map(is_plain_object).unwrap_or(false) {
        return "opencode-permission";
    }
    if get(payload, "questions").map(|v| v.is_array()).unwrap_or(false) {
        return "opencode-question";
    }
    if get(payload, "event").map(|v| v.is_string()).unwrap_or(false) {
        return "opencode-event";
    }
    "ancli"
}

// ---------------------------------------------------------------------------
// 展示与规则
// ---------------------------------------------------------------------------

/// `summarizeToolInput(toolInput)`：最多 4 个字段，每个值最长 400 字符。
pub fn summarize_tool_input(tool_input: &Value) -> String {
    let Some(obj) = tool_input.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj {
        let mut s = if let Some(text) = v.as_str() {
            text.to_string()
        } else {
            v.to_string()
        };
        if !s.is_empty() {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() > 400 {
                s = chars[..399].iter().collect::<String>() + "…";
            }
        }
        parts.push(format!("{k}: {s}"));
        if parts.len() >= 4 {
            break;
        }
    }
    parts.join("\n")
}

/// `ruleContentFor(tool, toolInput)`：挑最有代表性的那个入参作为「始终允许」的规则内容。
pub fn rule_content_for(_tool: &str, tool_input: &Value) -> String {
    let candidate = pick(
        tool_input,
        &[
            "command",
            "pattern",
            "file_path",
            "filePath",
            "path",
            "url",
            "description",
        ],
    );
    match candidate {
        Some(Value::String(s)) if !s.is_empty() => {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() > 200 {
                chars[..199].iter().collect::<String>() + "…"
            } else {
                s.clone()
            }
        }
        _ => "*".into(),
    }
}

/// `buildPermissionUpdates(tool, toolInput, suggestions)` →
/// 写进宿主 `updatedPermissions` 的条目数组。
pub fn build_permission_updates(tool: &str, tool_input: &Value, suggestions: &[Value]) -> Vec<Value> {
    let dest = pomodoro_core::env_str("POMODORO_PERMISSION_DEST")
        .unwrap_or_else(|| "projectSettings".into());

    if !suggestions.is_empty() {
        return suggestions
            .iter()
            .take(4)
            .map(|s| {
                if is_plain_object(s) {
                    // 已经是完整条目（含 rules）：只覆盖 behavior 与 destination
                    if let Some(_rules) = get(s, "rules") {
                        let mut m = match s {
                            Value::Object(o) => o.clone(),
                            _ => Map::new(),
                        };
                        m.insert("behavior".into(), json!("allow"));
                        let keep = pick_str(s, &["destination"]);
                        let d = if keep.is_empty() { dest.clone() } else { keep };
                        m.insert("destination".into(), json!(d));
                        return Value::Object(m);
                    }
                    return json!({
                        "type": "addRules",
                        "rules": [s.clone()],
                        "behavior": "allow",
                        "destination": dest,
                    });
                }
                json!({
                    "type": "addRules",
                    "rules": [{ "toolName": tool, "ruleContent": as_str_lossy(s) }],
                    "behavior": "allow",
                    "destination": dest,
                })
            })
            .collect();
    }

    vec![json!({
        "type": "addRules",
        "rules": [{ "toolName": tool, "ruleContent": rule_content_for(tool, tool_input) }],
        "behavior": "allow",
        "destination": dest,
    })]
}

/// `questionsFromToolInput(toolInput)`：工具入参 → 弹窗能吃的 questions。
///
/// 最多 4 题、每题最多 6 个选项；没有选项时**必须**开自定义输入框，否则用户无处作答。
pub fn questions_from_tool_input(tool_input: &Value) -> Vec<Value> {
    let raw = crate::util::arr(tool_input, "questions");
    let mut out = Vec::new();
    for (i, q) in raw.iter().take(4).enumerate() {
        let text = {
            let t = pick_str(q, &["question", "text", "title"]);
            if t.is_empty() {
                format!("问题 {}", i + 1)
            } else {
                t
            }
        };
        let options: Vec<Value> = crate::util::arr(q, "options")
            .iter()
            .take(6)
            .enumerate()
            .map(|(j, o)| match o {
                Value::String(s) => json!({ "id": format!("o{j}"), "label": s, "description": "" }),
                _ => json!({
                    "id": format!("o{j}"),
                    "label": pick_str(o, &["label", "text", "value"]),
                    "description": pick_str(o, &["description", "hint"]),
                }),
            })
            .filter(|o| !pick_str(o, &["label"]).is_empty())
            .collect();

        // custom：显式给就听它；否则「没有选项」或「宿主明说了可自由输入」时为真。
        // allowFreeformInput / openEnded 是 VS Code 提问工具的写法。
        let custom = match get(q, "custom").and_then(Value::as_bool) {
            Some(b) => b,
            None => options.is_empty() || crate::util::pick_bool(q, &["allowFreeformInput", "openEnded"]),
        };

        let question = json!({
            "id": format!("q{i}"),
            "question": text,
            "header": pick_str(q, &["header"]),
            "multiSelect": crate::util::pick_bool(q, &["multiSelect", "multiple"]),
            "custom": custom,
            "options": options,
        });
        if !pick_str(&question, &["question"]).is_empty() {
            out.push(question);
        }
    }
    out
}

/// `buildAnswerMap(questions, answers)`：弹窗按 `question.id` 索引的答案 →
/// agent 要的「按问题文本索引」的形状。
pub fn build_answer_map(questions: &[Value], answers: &Value) -> Value {
    let mut out = Map::new();
    for q in questions {
        let id = pick_str(q, &["id"]);
        let Some(v) = get(answers, &id) else { continue };
        let multi = crate::util::pick_bool(q, &["multiSelect"]);
        // ⚠ 逐字对齐 JS 的 `if (!v || !v.length) return;`：
        // 字符串也是"有 length"的，于是 `v[0]` 取到的是**第一个字符**。
        // 我们的网关只会回数组，这条路走不到 —— 但既然要"行为一致"，就照抄，
        // 免得将来对照两边源码时以为漏了一种形状。
        let value = match v {
            Value::Array(a) => {
                if a.is_empty() {
                    continue;
                }
                if multi {
                    Value::Array(a.clone())
                } else {
                    a[0].clone()
                }
            }
            Value::String(s) => {
                if s.is_empty() {
                    continue;
                }
                let first = s.chars().next().map(|c| c.to_string()).unwrap_or_default();
                if multi {
                    Value::String(s.clone())
                } else {
                    Value::String(first)
                }
            }
            _ => continue,
        };
        out.insert(pick_str(q, &["question"]), value);
    }
    Value::Object(out)
}

/// `formatAnswerMap(map)`：`问题 → 答案`，多项用 `、` 连。
pub fn format_answer_map(map: &Value) -> String {
    let Some(obj) = map.as_object() else {
        return String::new();
    };
    obj.iter()
        .map(|(k, v)| match v {
            Value::Array(a) => {
                let joined = a
                    .iter()
                    .map(as_str_lossy)
                    .collect::<Vec<_>>()
                    .join("、");
                format!("{k} → {joined}")
            }
            _ => format!("{k} → {}", as_str_lossy(v)),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判断来源是否属于「审批只走 PermissionRequest」那一类。
pub fn is_permission_event_only(source: &str) -> bool {
    PERMISSION_EVENT_ONLY_SOURCES.contains(&source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_normalization_covers_every_host_style() {
        // 这条函数是"八个宿主的提问工具都能被认出来"的唯一依据，逐家钉住
        assert_eq!(normalize_tool_name("AskUserQuestion"), "askuserquestion");
        assert_eq!(normalize_tool_name("vscode/askQuestions"), "askquestions");
        assert_eq!(normalize_tool_name("vscode_askQuestions"), "askquestions");
        assert_eq!(normalize_tool_name("question"), "question");
        assert_eq!(normalize_tool_name("ask_user_question"), "askuserquestion");
        assert_eq!(normalize_tool_name("copilot/AskUserQuestion"), "askuserquestion");
        assert_eq!(normalize_tool_name(""), "");
        // 不该把普通工具认成提问
        assert!(!is_ask_tool_name("Bash"));
        assert!(!is_ask_tool_name("apply_patch"));
    }

    #[test]
    fn ask_tool_detected_from_three_payload_shapes() {
        // Claude / ZCode
        assert!(is_ask_tool(&json!({ "tool_name": "AskUserQuestion" })));
        // VS Code：工具名不认得，但入参有 question 字符串
        assert!(is_ask_tool(
            &json!({ "tool_name": "x", "tool_input": { "question": "选哪个？" } })
        ));
        // Cursor：入参有 questions 数组
        assert!(is_ask_tool(
            &json!({ "tool_input": { "questions": [{ "question": "a" }] } })
        ));
        // 有 questions 但带了 command → 不是提问（是普通工具调用里恰好有个叫 questions 的参数）
        assert!(!is_ask_tool(&json!({
            "tool_input": { "questions": [{ "question": "a" }], "command": "ls" }
        })));
        assert!(!is_ask_tool(&json!({ "tool_name": "Bash", "tool_input": { "command": "ls" } })));
    }

    #[test]
    fn tool_input_questions_normalizes_single_question_shape() {
        let p = json!({
            "tool_input": { "question": "用哪个？", "header": "方案",
                            "options": ["A", "B"], "multiSelect": true, "command": "别带我" }
        });
        let q = tool_input_questions(&p);
        let arr = q["questions"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["question"], "用哪个？");
        assert_eq!(arr[0]["header"], "方案");
        assert_eq!(arr[0]["multiSelect"], true);
        assert_eq!(arr[0]["options"].as_array().unwrap().len(), 2);
        // 单问题分支会把其它字段丢掉（照抄 JS）
        assert!(q.get("command").is_none());

        // 已经是 questions 数组 → 原样返回
        let p = json!({ "tool_input": { "questions": [{ "question": "a" }] } });
        assert_eq!(tool_input_questions(&p)["questions"].as_array().unwrap().len(), 1);

        // 认不出来 → 原样返回（保持与 JS 相同）
        let p = json!({ "tool_input": { "command": "ls" } });
        assert_eq!(tool_input_questions(&p)["command"], "ls");
    }

    #[test]
    fn protocol_detection_priority() {
        // 有 hook_event_name 且是 Cursor 事件名 → cursor
        assert_eq!(detect_protocol(&json!({ "hook_event_name": "preToolUse" })), "cursor");
        // 其它 hook_event_name → ancli
        assert_eq!(detect_protocol(&json!({ "hook_event_name": "PreToolUse" })), "ancli");
        // 没有 hook_event_name 才看 OpenCode 的形状
        assert_eq!(
            detect_protocol(&json!({ "permission": { "type": "bash" } })),
            "opencode-permission"
        );
        assert_eq!(detect_protocol(&json!({ "questions": [] })), "opencode-question");
        assert_eq!(detect_protocol(&json!({ "event": "session.idle" })), "opencode-event");
        // 什么都没有 → 当 ancli 处理（静默忽略未知事件）
        assert_eq!(detect_protocol(&json!({})), "ancli");
        // hook_event_name 是非字符串时不参与判定
        assert_eq!(detect_protocol(&json!({ "hook_event_name": 42 })), "ancli");
    }

    #[test]
    fn timeout_ms_clamps_like_js() {
        // 不能并行改环境变量，这里只测"未设置"这条主干 + 直接算边界
        // （设置了 POMODORO_TIMEOUT_S 的用例放在 install/main 的集成测试里做）
        if pomodoro_core::env_str("POMODORO_TIMEOUT_S").is_none() {
            assert_eq!(timeout_ms(), 3_600_000);
        }
        // 手工验算 clamp 边界：min(,7200) 再 max(5,round)
        let clamp = |raw: f64| -> u64 {
            let capped = raw.min(7200.0);
            (capped.round().max(5.0) as u64) * 1000
        };
        assert_eq!(clamp(1.0), 5_000);
        assert_eq!(clamp(0.4), 5_000);
        assert_eq!(clamp(100.6), 101_000);
        assert_eq!(clamp(99_999.0), 7_200_000);
    }

    #[test]
    fn questions_cap_at_four_times_six() {
        let options: Vec<Value> = (0..9).map(|i| json!(format!("选项{i}"))).collect();
        let questions: Vec<Value> = (0..6)
            .map(|i| json!({ "question": format!("第{i}题"), "options": options }))
            .collect();
        let out = questions_from_tool_input(&json!({ "questions": questions }));
        assert_eq!(out.len(), 4, "最多 4 题");
        assert_eq!(out[0]["options"].as_array().unwrap().len(), 6, "每题最多 6 个选项");
        assert_eq!(out[0]["id"], "q0");
        assert_eq!(out[0]["options"][0]["id"], "o0");
        // 有选项 → 不给自定义输入框
        assert_eq!(out[0]["custom"], false);
    }

    #[test]
    fn questions_without_options_force_custom_input() {
        let out = questions_from_tool_input(&json!({
            "questions": [{ "text": "第二题", "multiple": true }]
        }));
        assert_eq!(out.len(), 1);
        // 无选项必须开自定义输入框，否则用户无处作答
        assert_eq!(out[0]["custom"], true);
        assert_eq!(out[0]["multiSelect"], true);
        assert_eq!(out[0]["question"], "第二题");
        // 空文案题目被丢
        let out = questions_from_tool_input(&json!({ "questions": [{ "question": "   " }] }));
        assert_eq!(out.len(), 1, "JS 不做 trim，空白题目会保留");
    }

    #[test]
    fn options_drop_empty_labels() {
        let out = questions_from_tool_input(&json!({
            "questions": [{ "question": "q", "options": [
                "A", { "label": "" }, { "text": "B", "hint": "稳" }
            ]}]
        }));
        let opts = out[0]["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2, "空 label 的选项要丢掉");
        assert_eq!(opts[0]["label"], "A");
        assert_eq!(opts[1]["label"], "B");
        assert_eq!(opts[1]["description"], "稳");
    }

    #[test]
    fn answer_map_keys_by_question_text() {
        let questions = vec![
            json!({ "id": "q0", "question": "用哪种？", "multiSelect": false }),
            json!({ "id": "q1", "question": "哪些？", "multiSelect": true }),
        ];
        let answers = json!({ "q0": ["A 方案"], "q1": ["x", "y"] });
        let map = build_answer_map(&questions, &answers);
        assert_eq!(map["用哪种？"], "A 方案");
        assert_eq!(map["哪些？"].as_array().unwrap().len(), 2);
        assert_eq!(format_answer_map(&map), "用哪种？ → A 方案\n哪些？ → x、y");
        // 没答的题不进结果
        let map = build_answer_map(&questions, &json!({ "q1": ["only"] }));
        assert!(map.get("用哪种？").is_none());
        assert_eq!(map["哪些？"][0], "only");
    }

    #[test]
    fn permission_updates_shape() {
        let updates = build_permission_updates("Bash", &json!({ "command": "npm test" }), &[]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["type"], "addRules");
        assert_eq!(updates[0]["behavior"], "allow");
        assert_eq!(updates[0]["rules"][0]["toolName"], "Bash");
        assert_eq!(updates[0]["rules"][0]["ruleContent"], "npm test");
        assert_eq!(updates[0]["destination"], "projectSettings");

        // 宿主给的 suggestions：字符串会被包成 addRules，对象原样带着
        let updates = build_permission_updates(
            "Bash",
            &json!({}),
            &[json!("npm run build"), json!({ "toolName": "Bash", "ruleContent": "ls" })],
        );
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0]["rules"][0]["ruleContent"], "npm run build");
        assert_eq!(updates[1]["rules"][0]["toolName"], "Bash");

        // 已是完整条目（带 rules）→ 只覆盖 behavior / destination
        let updates = build_permission_updates(
            "Bash",
            &json!({}),
            &[json!({ "type": "addRules", "rules": [], "destination": "userSettings" })],
        );
        assert_eq!(updates[0]["behavior"], "allow");
        assert_eq!(updates[0]["destination"], "userSettings", "已有 destination 要保留");
    }

    #[test]
    fn rule_content_and_summary() {
        assert_eq!(rule_content_for("Bash", &json!({ "command": "npm test" })), "npm test");
        // 取第一个命中的键（command 优先于 file_path）
        assert_eq!(
            rule_content_for("x", &json!({ "file_path": "/a", "command": "ls" })),
            "ls"
        );
        assert_eq!(rule_content_for("x", &json!({})), "*");
        assert_eq!(rule_content_for("x", &json!({ "command": 42 })), "*", "非字符串落 *");
        // 超长截断到 199 + 省略号
        let long = "a".repeat(500);
        let got = rule_content_for("x", &json!({ "command": long }));
        assert_eq!(got.chars().count(), 200);
        assert!(got.ends_with('…'));

        let summary = summarize_tool_input(&json!({
            "a": "1", "b": { "n": 2 }, "c": "3", "d": "4", "e": "5"
        }));
        assert_eq!(summary.split('\n').count(), 4, "最多 4 个字段");
        assert!(summary.starts_with("a: 1"));
        assert!(summary.contains(r#"b: {"n":2}"#), "对象按紧凑 JSON 拼: {summary}");
        assert_eq!(summarize_tool_input(&json!("不是对象")), "");
    }

    #[test]
    fn source_detection_uses_explicit_markers() {
        // --source 走环境变量，优先级最高（这里不设，只验 payload 侧）
        assert_eq!(detect_source(&json!({ "source": "trae" })), "trae");
        assert_eq!(detect_source(&json!({ "conversation_id": "x" })), "cursor");
        assert_eq!(detect_source(&json!({ "turn_id": "x" })), "codex");
        assert_eq!(detect_source(&json!({ "trigger": "x" })), "codex");
        assert_eq!(
            detect_source(&json!({ "llm_tool_name": "x" })),
            "trae",
            "Trae 的 llm_tool_name"
        );
        assert_eq!(
            detect_source(&json!({ "workspace_roots": ["/a"], "tool_use_id": "t" })),
            "trae"
        );
    }
}
