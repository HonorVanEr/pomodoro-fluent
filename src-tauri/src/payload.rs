//! 交互 payload 归一化 —— **逐行对应** Electron 版 `gateway.js` 的 `normalize*` 系列。
//!
//! 为什么不直接透传调用方的 JSON：各宿主（Claude Code / ZCode / VS Code / OpenCode…）
//! 的字段名和层级都不一样，调用方还可能是随手写的 curl。归一化把「谁发来的」收窄成
//! 「弹窗能直接渲染的固定形状」，于是 `renderer/notify.js` 只需要认一套字段。
//!
//! ✅ 这一层是**纯函数**，所以它同时是 M2 里最值得写单元测试的地方 ——
//! 超时夹取、空选项降级、字段长度上限这些边界，出错的代价是弹窗渲染异常或
//! 用户看到一个无法作答的窗口。
//!
//! ⚠ 改这里的任何一个字段名/上限，都要同步看 `renderer/notify.js` 与
//! `docs/agent-hooks.md` 的字段表 —— 三处是同一份契约。

use serde_json::{json, Map, Value};

use pomodoro_core::{DEFAULT_CONFIRM_TIMEOUT_MS, MAX_CONFIRM_TIMEOUT_MS};

/// 文案长度上限（超长截断）
const CAP_TITLE: usize = 120;
const CAP_MESSAGE: usize = 360;
const CAP_SUB: usize = 160;
const CAP_DETAIL: usize = 2000;

/// 上下文各字段上限
const CTX_CAPS: [(&str, usize); 8] = [
    ("agent", 32),
    ("agentType", 60),
    ("agentId", 40),
    ("session", 16),
    ("project", 60),
    ("task", 200),
    ("tool", 60),
    ("toolDetail", 160),
];

/// 四种交互形态 + 各自的超时兜底动作。
///
/// `default_action == None` 表示「不替调用方做任何决定」——
/// `notification` 本就不等结果，`custom` 没有安全默认值。
pub struct KindSpec {
    pub title: &'static str,
    pub default_action: Option<&'static str>,
}

pub fn kind_spec(kind: &str) -> Option<KindSpec> {
    match kind {
        "ask" => Some(KindSpec {
            title: "Agent 提问",
            default_action: Some("cancel"),
        }),
        "permission" => Some(KindSpec {
            title: "需要权限确认",
            default_action: Some("deny"),
        }),
        "notification" => Some(KindSpec {
            title: "番茄钟",
            default_action: None,
        }),
        "custom" => Some(KindSpec {
            title: "需要确认",
            default_action: None,
        }),
        _ => None,
    }
}

/// 需要等用户在弹窗内决策的三种形态（`notification` 弹完即走）。
pub fn is_interactive(kind: &str) -> bool {
    matches!(kind, "ask" | "permission" | "custom")
}

/// JS `str.slice(0, n-1) + '…'` 的 Rust 版：结果总长 ≤ n 个**字符**。
///
/// 与 JS 的唯一差别：JS 的 `.length` 是 UTF-16 码元，所以它可能把 emoji 的
/// 代理对从中间切开，而这里按 Unicode 标量切 —— 这是修好了，不是不等价。
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 取字符串字段并截断。非字符串（数字 / null / 缺字段）一律当空串 —— 与 `capText` 一致。
pub fn cap_text(v: Option<&Value>, n: usize) -> String {
    match v {
        Some(Value::String(s)) => truncate(s, n),
        _ => String::new(),
    }
}

/// 直接截断一个 `&str`（网关用它给事件通知的文案限量）。
pub fn truncate_str(s: &str, n: usize) -> String {
    truncate(s, n)
}

/// 非空字符串字段（`''` 视为缺省）
fn non_empty(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// 布尔字段（缺省用 `default`）
fn bool_or(v: Option<&Value>, default: bool) -> bool {
    v.and_then(Value::as_bool).unwrap_or(default)
}

/// 兜底等待时长：`clamp(round(ms), 5000, 2h)`，非法值落回默认 1h。
///
/// 字符串数字也认（`Number("120000")` 在 JS 里是合法数），
/// 因为手写 curl 的人很容易把 ms 写成字符串。
pub fn clamp_timeout_ms(v: Option<&Value>) -> u64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match n {
        Some(f) if f.is_finite() && f > 0.0 => {
            (f.round() as i64).clamp(5_000, MAX_CONFIRM_TIMEOUT_MS as i64) as u64
        }
        _ => DEFAULT_CONFIRM_TIMEOUT_MS,
    }
}

// ---------------------------------------------------------------------------
// 各子结构
// ---------------------------------------------------------------------------

/// 提问的问题列表：最多 4 题 × 6 选项；没有可作答控件时强制开自定义输入框
/// （否则弹出一个无法提交的窗口 —— 那是 Electron 版踩过的坑）。
fn normalize_questions(v: Option<&Value>) -> Value {
    let Some(Value::Array(list)) = v else {
        return json!([]);
    };
    let mut out = Vec::new();
    for (i, q) in list.iter().take(4).enumerate() {
        let Some(obj) = q.as_object() else { continue };
        // 题目文案取 question / text / title 三选一，全空则整题丢弃
        let text = ["question", "text", "title"]
            .iter()
            .map(|k| cap_text(obj.get(*k), 200))
            .find(|s| !s.is_empty());
        let Some(text) = text else { continue };

        let options: Vec<Value> = obj
            .get("options")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .take(6)
                    .enumerate()
                    .filter_map(|(j, o)| normalize_option(o, j))
                    .collect()
            })
            .unwrap_or_default();

        let id = non_empty(obj.get("id")).unwrap_or_else(|| format!("q{i}"));
        out.push(json!({
            "id": id,
            "question": text,
            "header": cap_text(obj.get("header"), 24),
            "multiSelect": bool_or(obj.get("multiSelect"), false)
                || bool_or(obj.get("multiple"), false),
            "custom": obj.get("custom").and_then(Value::as_bool)
                .unwrap_or(options.is_empty()),
            "options": options,
        }));
    }
    Value::Array(out)
}

/// 单个选项：纯字符串形式也认（`["A 方案", "B 方案"]`）。
fn normalize_option(o: &Value, j: usize) -> Option<Value> {
    if let Value::String(s) = o {
        let label = truncate(s, 60);
        if label.is_empty() {
            return None;
        }
        return Some(json!({ "id": format!("o{j}"), "label": label, "description": "" }));
    }
    let obj = o.as_object()?;
    let label = ["label", "text", "value", "name"]
        .iter()
        .map(|k| cap_text(obj.get(*k), 60))
        .find(|s| !s.is_empty())?;
    // description 允许从 `hint` 兜底（与 Electron 版一致）
    let desc = {
        let d = cap_text(obj.get("description"), 160);
        if d.is_empty() {
            cap_text(obj.get("hint"), 160)
        } else {
            d
        }
    };
    Some(json!({
        "id": non_empty(obj.get("id")).unwrap_or_else(|| format!("o{j}")),
        "label": label,
        "description": desc,
    }))
}

/// 权限详情。`o` 是顶层入参 —— 工具名允许从顶层 `toolName` / `tool` 兜底，
/// 因为直连 HTTP 的调用方经常只传顶层字段。
fn normalize_permission(p: Option<&Value>, o: &Map<String, Value>) -> Value {
    let pp = p.and_then(Value::as_object);
    let pick = |k: &str| -> Option<&Value> { pp.and_then(|m| m.get(k)) };

    let mut suggestions = Vec::new();
    if let Some(arr) = pick("suggestions").and_then(Value::as_array) {
        for (i, s) in arr.iter().take(4).enumerate() {
            if let Some(v) = normalize_suggestion(s, i) {
                suggestions.push(v);
            }
        }
    }

    let tool = {
        let t = cap_text(pick("tool"), 60);
        if t.is_empty() {
            let a = cap_text(o.get("toolName"), 60);
            if a.is_empty() {
                cap_text(o.get("tool"), 60)
            } else {
                a
            }
        } else {
            t
        }
    };

    json!({
        "tool": tool,
        "rule": cap_text(pick("rule"), 200),
        "suggestions": Value::Array(suggestions),
        // 只有显式 false 才关掉「始终允许」
        "canAlways": pick("canAlways").and_then(Value::as_bool).unwrap_or(true),
    })
}

fn normalize_suggestion(s: &Value, i: usize) -> Option<Value> {
    if let Value::String(text) = s {
        let label = truncate(text, 60);
        if label.is_empty() {
            return None;
        }
        return Some(json!({
            "id": format!("s{i}"),
            "label": label,
            "toolName": "",
            "ruleContent": truncate(text, 200),
        }));
    }
    let obj = s.as_object()?;
    let rule_content = ["ruleContent", "rule", "pattern"]
        .iter()
        .map(|k| cap_text(obj.get(*k), 200))
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let label = ["label", "ruleContent", "rule", "pattern"]
        .iter()
        .map(|k| cap_text(obj.get(*k), 60))
        .find(|s| !s.is_empty())?;
    Some(json!({
        "id": non_empty(obj.get("id")).unwrap_or_else(|| format!("s{i}")),
        "label": label,
        "toolName": cap_text(obj.get("toolName"), 60),
        "ruleContent": rule_content,
    }))
}

/// 文本输入框。权限弹窗默认开（拒绝理由 / 补充说明），提问默认关。
fn normalize_input(i: Option<&Value>, kind: &str) -> Value {
    let ii = i.and_then(Value::as_object);
    let enabled = ii
        .and_then(|m| m.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(kind == "permission");
    json!({
        "enabled": enabled,
        "label": cap_text(ii.and_then(|m| m.get("label")), 40),
        "placeholder": cap_text(ii.and_then(|m| m.get("placeholder")), 80),
        "required": bool_or(ii.and_then(|m| m.get("required")), false),
    })
}

/// 自定义按钮：必须是 `{id, label}` 都是字符串的条目才收，最多 4 个。
fn normalize_actions(v: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(list)) = v else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|a| {
            let obj = a.as_object()?;
            let id = obj.get("id")?.as_str()?;
            let label = obj.get("label")?.as_str()?;
            let style = match obj.get("style").and_then(Value::as_str) {
                Some("primary") => "primary",
                Some("danger") => "danger",
                _ => "default",
            };
            Some(json!({ "id": id, "label": truncate(label, 24), "style": style }))
        })
        .take(4)
        .collect()
}

/// 上下文：缺哪项留空串，弹窗按空隐藏。
fn normalize_context(c: Option<&Value>) -> Value {
    let o = c.and_then(Value::as_object);
    let mut out = Map::new();
    for (k, cap) in CTX_CAPS {
        out.insert(k.to_string(), json!(cap_text(o.and_then(|m| m.get(k)), cap)));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 归一化一次交互请求。返回的对象就是**弹窗能直接渲染的那份 payload**
/// （`id` / `interactive` 由调用方补上）。
///
/// 未知 `kind` 一律落 `custom`（宽容：调用方拼错不该让请求 400）。
pub fn normalize_interaction(input: &Value) -> Value {
    let empty = Map::new();
    let o = input.as_object().unwrap_or(&empty);

    let kind = non_empty(o.get("kind"))
        .filter(|k| kind_spec(k).is_some())
        .unwrap_or_else(|| "custom".to_string());
    let spec = kind_spec(&kind).expect("kind 已校验");

    let actions = normalize_actions(o.get("actions"));

    // 视觉风味：ask / permission 用自身 kind；其余沿用调用方给的 type，
    // 都没有就按 agent 事件渲染。
    let flavor = non_empty(o.get("flavor"))
        .or_else(|| {
            if kind == "ask" || kind == "permission" {
                Some(kind.clone())
            } else {
                non_empty(o.get("type"))
            }
        })
        .unwrap_or_else(|| "agent".to_string());

    let default_action = non_empty(o.get("defaultAction"))
        .or_else(|| spec.default_action.map(str::to_string));

    let actions = if !actions.is_empty() {
        actions
    } else if kind == "custom" {
        // 自定义确认没有按钮就没法继续，给一个「知道了」兜底
        vec![json!({ "id": "ok", "label": "知道了", "style": "default" })]
    } else {
        Vec::new()
    };

    // 标题：调用方没给（或给了空白）就落该 kind 的默认标题
    let title = {
        let t = cap_text(o.get("title"), CAP_TITLE);
        if t.is_empty() {
            spec.title.to_string()
        } else {
            t
        }
    };

    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("flavor".into(), json!(truncate(&flavor, 24)));
    out.insert("source".into(), json!(cap_text(o.get("source"), 32)));
    out.insert("title".into(), json!(title));
    out.insert("message".into(), json!(cap_text(o.get("message"), CAP_MESSAGE)));
    out.insert("sub".into(), json!(cap_text(o.get("sub"), CAP_SUB)));
    out.insert("detail".into(), json!(cap_text(o.get("detail"), CAP_DETAIL)));
    out.insert(
        "timeoutMs".into(),
        json!(clamp_timeout_ms(o.get("timeoutMs"))),
    );
    out.insert(
        "defaultAction".into(),
        match default_action {
            Some(a) => json!(a),
            None => Value::Null,
        },
    );
    out.insert("actions".into(), Value::Array(actions));
    out.insert("input".into(), normalize_input(o.get("input"), &kind));
    out.insert("context".into(), normalize_context(o.get("context")));

    if kind == "ask" {
        out.insert("questions".into(), normalize_questions(o.get("questions")));
    }
    if kind == "permission" {
        out.insert(
            "permission".into(),
            normalize_permission(o.get("permission"), o),
        );
    }
    Value::Object(out)
}

/// 从归一化后的 payload 里读回兜底时长（状态机要用）。
pub fn timeout_of(payload: &Value) -> u64 {
    clamp_timeout_ms(payload.get("timeoutMs"))
}

/// 从归一化后的 payload 里读回超时兜底动作。
pub fn default_action_of(payload: &Value) -> Option<String> {
    payload
        .get("defaultAction")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 弹窗页请求的初始尺寸（逻辑像素），与 `main.js` 的 `popupSizeFor` 一致。
pub fn popup_size(kind: &str) -> (f64, f64) {
    match kind {
        "ask" => (460.0, 260.0),
        "permission" => (432.0, 240.0),
        _ => (400.0, 176.0),
    }
}

/// 来源标识 → 展示名（`SOURCE_LABELS`）。未知来源原样返回。
pub fn source_label(s: &str) -> String {
    match s {
        "zcode" => "ZCode".into(),
        "claude-code" => "Claude Code".into(),
        "vscode" => "VS Code".into(),
        "trae" => "Trae".into(),
        "cursor" => "Cursor".into(),
        "opencode" => "OpenCode".into(),
        "codex" => "Codex".into(),
        "qwen" => "Qwen Code".into(),
        "manual" => "手动".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: Value) -> Value {
        normalize_interaction(&v)
    }

    #[test]
    fn unknown_kind_falls_back_to_custom() {
        let p = norm(json!({ "kind": "nonsense" }));
        assert_eq!(p["kind"], "custom");
        // custom 没按钮就无法继续，必须补一个兜底按钮
        assert_eq!(p["actions"][0]["id"], "ok");
        assert_eq!(p["defaultAction"], Value::Null);
    }

    #[test]
    fn kind_defaults_are_safe() {
        // permission 超时必须落 deny，ask 落 cancel —— 这是安全默认值，不能反
        assert_eq!(norm(json!({"kind":"permission"}))["defaultAction"], "deny");
        assert_eq!(norm(json!({"kind":"ask"}))["defaultAction"], "cancel");
        assert_eq!(norm(json!({"kind":"notification"}))["defaultAction"], Value::Null);
    }

    #[test]
    fn explicit_default_action_wins() {
        let p = norm(json!({ "kind": "permission", "defaultAction": "allow" }));
        assert_eq!(p["defaultAction"], "allow");
    }

    #[test]
    fn timeout_is_clamped() {
        assert_eq!(clamp_timeout_ms(Some(&json!(120_000))), 120_000);
        // 下限 5s
        assert_eq!(clamp_timeout_ms(Some(&json!(10))), 5_000);
        // 上限 2h
        assert_eq!(clamp_timeout_ms(Some(&json!(99_999_999))), MAX_CONFIRM_TIMEOUT_MS);
        // 非法值落默认 1h
        assert_eq!(clamp_timeout_ms(Some(&json!(0))), DEFAULT_CONFIRM_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(Some(&json!(-5))), DEFAULT_CONFIRM_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(Some(&json!("abc"))), DEFAULT_CONFIRM_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(None), DEFAULT_CONFIRM_TIMEOUT_MS);
        // 字符串数字要认（手写 curl 常见）
        assert_eq!(clamp_timeout_ms(Some(&json!("120000"))), 120_000);
    }

    #[test]
    fn titles_default_per_kind() {
        assert_eq!(norm(json!({"kind":"ask"}))["title"], "Agent 提问");
        assert_eq!(norm(json!({"kind":"permission"}))["title"], "需要权限确认");
        assert_eq!(norm(json!({"kind":"notification"}))["title"], "番茄钟");
        assert_eq!(norm(json!({"kind":"custom"}))["title"], "需要确认");
        assert_eq!(norm(json!({"kind":"ask","title":"选个方案"}))["title"], "选个方案");
    }

    #[test]
    fn questions_are_normalized_and_capped() {
        let p = norm(json!({
            "kind": "ask",
            "questions": [
                { "question": "用哪种？", "header": "方案",
                  "options": ["A 方案", { "id": "b", "label": "B 方案", "description": "稳" }] },
                { "text": "第二题", "multiple": true },
                { "question": "   " },
                { "question": "第四题" },
                { "question": "第五题会被丢掉" }
            ]
        }));
        let qs = p["questions"].as_array().unwrap();
        // 先取前 4 题再逐题校验 → 第 5 题根本没进循环，留在 4 题。
        // ⚠ 第 3 题是纯空白，但 **Electron 的 `capText` 不 trim**，`'   '` 在 JS 里
        // 是真值，所以它**不会被丢掉** —— 两边行为必须一致（同一个 release 里两版共发）。
        assert_eq!(qs.len(), 4);
        assert_eq!(qs[0]["id"], "q0");
        assert_eq!(qs[0]["question"], "用哪种？");
        assert_eq!(qs[0]["options"][0]["label"], "A 方案");
        assert_eq!(qs[0]["options"][0]["id"], "o0");
        assert_eq!(qs[0]["options"][1]["description"], "稳");
        // 有选项 → custom 关闭
        assert_eq!(qs[0]["custom"], false);
        // 无选项 → 必须开自定义输入框，否则用户无处作答
        assert_eq!(qs[1]["custom"], true);
        // multiple 是 multiSelect 的别名
        assert_eq!(qs[1]["multiSelect"], true);
        // 空白题保留原样（与 Electron 一致 —— 这是它的既有怪癖，不是本版的改动）
        assert_eq!(qs[2]["question"], "   ");
        assert_eq!(qs[3]["question"], "第四题");
    }

    #[test]
    fn ask_without_questions_still_normalizes() {
        // 网关会把这种降级成通知，但归一化本身不该 panic / 给 null
        let p = norm(json!({ "kind": "ask" }));
        assert_eq!(p["questions"], json!([]));
    }

    #[test]
    fn permission_reads_tool_from_top_level() {
        // 直连 API 的调用方常用顶层 tool / toolName
        let p = norm(json!({ "kind": "permission", "tool": "Bash" }));
        assert_eq!(p["permission"]["tool"], "Bash");
        let p2 = norm(json!({ "kind": "permission", "toolName": "Edit" }));
        assert_eq!(p2["permission"]["tool"], "Edit");
        // permission.tool 优先
        let p3 = norm(json!({ "kind": "permission", "toolName": "Edit",
                              "permission": { "tool": "Bash", "rule": "npm test" } }));
        assert_eq!(p3["permission"]["tool"], "Bash");
        assert_eq!(p3["permission"]["rule"], "npm test");
        assert_eq!(p3["permission"]["canAlways"], true);
    }

    #[test]
    fn can_always_only_off_when_explicit_false() {
        assert_eq!(norm(json!({"kind":"permission"}))["permission"]["canAlways"], true);
        assert_eq!(
            norm(json!({"kind":"permission","permission":{"canAlways":false}}))["permission"]["canAlways"],
            false
        );
    }

    #[test]
    fn permission_input_defaults_on_ask_defaults_off() {
        assert_eq!(norm(json!({"kind":"permission"}))["input"]["enabled"], true);
        assert_eq!(norm(json!({"kind":"ask"}))["input"]["enabled"], false);
        // 显式给值要覆盖默认
        assert_eq!(norm(json!({"kind":"permission","input":{"enabled":false}}))["input"]["enabled"], false);
        assert_eq!(norm(json!({"kind":"ask","input":{"enabled":true}}))["input"]["enabled"], true);
    }

    #[test]
    fn actions_require_id_and_label_and_are_capped() {
        // ⚠ 顺序是「先按合法性过滤、再截前 4 个」（Electron 的 `.filter(...).slice(0,4)`）——
        // 不是「先截前 4 个再过滤」。所以要多给几条**合法**的，才能真正压到上限。
        let p = norm(json!({
            "kind": "custom",
            "actions": [
                { "id": "a", "label": "第一个", "style": "primary" },
                { "id": "b", "label": "第二个", "style": "danger" },
                { "id": "c", "label": "第三个", "style": "weird" },
                { "label": "没有 id 会被丢" },
                { "id": "d" },
                { "id": "e", "label": "第四个" },
                { "id": "f", "label": "第五个会被截掉" }
            ]
        }));
        let a = p["actions"].as_array().unwrap();
        // 合法的是 a/b/c/e/f 五条，截前 4 → a/b/c/e
        assert_eq!(a.len(), 4, "非法条目要丢，且合法条目总数不超过 4");
        assert_eq!(a[0]["id"], "a");
        assert_eq!(a[0]["style"], "primary");
        assert_eq!(a[1]["style"], "danger");
        // 未知 style 落 default
        assert_eq!(a[2]["style"], "default");
        assert_eq!(a[3]["id"], "e", "第 5 条合法项（f）应被上限截掉");
    }

    #[test]
    fn text_caps_truncate_with_ellipsis() {
        let long = "x".repeat(500);
        let p = norm(json!({ "kind": "custom", "message": long.clone(), "detail": long.clone() }));
        // message 的上限（360）小于输入长度 → 截断并加省略号
        assert_eq!(p["message"].as_str().unwrap().chars().count(), CAP_MESSAGE);
        assert!(p["message"].as_str().unwrap().ends_with('…'));
        // detail 的上限（2000）大于输入长度 → 原样保留，**不该**出现省略号
        assert!(CAP_DETAIL > 500, "这条断言的前提是 detail 的上限比输入长");
        assert_eq!(p["detail"].as_str().unwrap().chars().count(), 500);
        assert!(!p["detail"].as_str().unwrap().ends_with('…'));
        // 短的不能被动
        let p2 = norm(json!({ "kind": "custom", "message": "短" }));
        assert_eq!(p2["message"], "短");
    }

    #[test]
    fn context_keeps_every_key_and_caps_values() {
        let p = norm(json!({
            "kind": "permission",
            "context": { "agent": "claude-code", "tool": "Bash", "task": "重构缓存层",
                         "project": "pomodoro-fluent", "unknown": "应被忽略" }
        }));
        let c = p["context"].as_object().unwrap();
        assert_eq!(c.len(), CTX_CAPS.len(), "固定 8 个键，多余的丢弃");
        assert_eq!(c["agent"], "claude-code");
        assert_eq!(c["task"], "重构缓存层");
        // 没给的字段留空串（弹窗按空隐藏）
        assert_eq!(c["session"], "");
        assert_eq!(c["toolDetail"], "");
        assert!(!c.contains_key("unknown"));
    }

    #[test]
    fn flavor_derivation() {
        assert_eq!(norm(json!({"kind":"ask"}))["flavor"], "ask");
        assert_eq!(norm(json!({"kind":"permission"}))["flavor"], "permission");
        // 非 ask/permission 时用 type（定时器通知的 work / break…）
        assert_eq!(norm(json!({"kind":"notification","type":"work"}))["flavor"], "work");
        // 都没有 → agent
        assert_eq!(norm(json!({"kind":"notification"}))["flavor"], "agent");
        // 显式 flavor 优先
        assert_eq!(
            norm(json!({"kind":"notification","type":"work","flavor":"break"}))["flavor"],
            "break"
        );
    }

    #[test]
    fn non_string_fields_are_treated_as_empty() {
        // 调用方传了数字/对象给文案字段时不能 panic，也不能把 JSON 塞进弹窗
        let p = norm(json!({ "kind": "custom", "title": 42, "message": {"a":1}, "sub": null }));
        assert_eq!(p["title"], "需要确认");
        assert_eq!(p["message"], "");
        assert_eq!(p["sub"], "");
    }

    #[test]
    fn popup_sizes_match_main_js() {
        assert_eq!(popup_size("ask"), (460.0, 260.0));
        assert_eq!(popup_size("permission"), (432.0, 240.0));
        assert_eq!(popup_size("notification"), (400.0, 176.0));
        assert_eq!(popup_size("custom"), (400.0, 176.0));
    }

    #[test]
    fn interactive_kinds() {
        assert!(is_interactive("ask"));
        assert!(is_interactive("permission"));
        assert!(is_interactive("custom"));
        assert!(!is_interactive("notification"));
        assert!(!is_interactive("未知"));
    }
}
