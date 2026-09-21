//! 决策缓存 + 本地「始终允许」规则。
//!
//! 对应 `pomodoro-hook.js` 的 `cacheDir` / `cacheKeyFor` / `cacheRead` /
//! `cacheWrite` 与 `localRule*` 六个函数。
//!
//! 两者都存在 **系统临时目录** 下（`%TEMP%/pomodoro-hook-cache`），不是用户数据目录 ——
//! 它们全是可再生的短命状态，跟着系统清理走正合适。
//!
//! ⚠ 这个目录是 **Electron 版与 Rust 版共用的**。文件名算法（SHA-1 前 20/16 位）
//! 必须完全一致，否则混用期会出现「一次提问弹两次窗」——那正是缓存本来要解决的问题。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::util::{is_truthy, sha1_hex_prefix};

/// 决策缓存有效期：90s。
///
/// 上游场景：ZCode 一次 `AskUserQuestion` 会**同时**触发 PreToolUse 与
/// PermissionRequest，两个独立的 hook 进程各弹一次窗。缓存把第二次的决策复用掉。
/// 90s 是「够两个进程先后跑完，又不至于让上一次的答案影响下一次提问」的量级。
pub const CACHE_TTL_MS: i64 = 90 * 1000;

/// 本地「始终允许」规则有效期：30 天。
pub const LOCAL_RULE_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// 最多留多少条规则（超出从最旧开始丢）。
const LOCAL_RULE_MAX: usize = 200;

/// 这些宿主的「始终允许」只能靠本地规则落地 —— 它们的权限事件输出里**没有**
/// `updatedPermissions` 字段（或者有但会 fail closed）：
///  * VS Code / Cursor：PreToolUse 输出里没这个字段；
///  * Trae：支持 `permissionDecision` / `updatedInput` / `additionalContext`，但没有
///    `updatedPermissions`；
///  * Codex：`PermissionRequest` 遇到不支持的字段会 **fail closed**（实机文案
///    "PermissionRequest hook returned unsupported updatedPermissions"）。
/// 只回一次 allow 的话下次同类调用还会再弹窗 —— 用户选了「始终允许」却每次都问，
/// 那就是假的。
const LOCAL_RULE_SOURCES: [&str; 4] = ["vscode", "trae", "cursor", "codex"];

/// `os.tmpdir()/pomodoro-hook-cache`
pub fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("pomodoro-hook-cache")
}

/// `Date.now()`
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 决策缓存
// ---------------------------------------------------------------------------

/// `cacheKeyFor(payload)`
pub fn cache_key_for(payload: &Value) -> String {
    let id = match crate::util::pick(payload, &["tool_use_id", "tool_useId", "toolUseId"]) {
        Some(v) => crate::util::as_str_lossy(v),
        None => {
            let tool = crate::util::pick_str(payload, &["tool_name"]);
            let input = crate::util::get(payload, "tool_input")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // ⚠ 这里的 `input.to_string()` 必须与 JS 的 `JSON.stringify(input)` 同形：
            // serde_json 默认就是紧凑输出（无空格、保留键序），与 JS 一致。
            format!("{tool}:{input}")
        }
    };
    sha1_hex_prefix(&id, 20)
}

/// `cacheRead(key)`：过期 / 坏文件 / 不存在一律 `None`。
pub fn cache_read(key: &str) -> Option<Value> {
    let text = std::fs::read_to_string(cache_dir().join(format!("{key}.json"))).ok()?;
    let obj: Value = serde_json::from_str(&text).ok()?;
    let at = crate::util::get(&obj, "at").and_then(Value::as_i64).unwrap_or(0);
    if now_ms() - at > CACHE_TTL_MS {
        return None;
    }
    crate::util::get(&obj, "result").cloned()
}

/// `cacheWrite(key, result)`：写失败不影响主流程（缓存只是优化）。
pub fn cache_write(key: &str, result: &Value) {
    let dir = cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let body = json!({ "at": now_ms(), "result": result });
    let _ = std::fs::write(dir.join(format!("{key}.json")), body.to_string());
}

// ---------------------------------------------------------------------------
// 本地「始终允许」规则
// ---------------------------------------------------------------------------

fn local_rule_file() -> PathBuf {
    cache_dir().join("always-allow.json")
}

/// `POMODORO_LOCAL_ALWAYS_ALLOW` 关掉整条链路；再要求来源在允许名单里。
pub fn local_rule_enabled(source: &str) -> bool {
    if !pomodoro_core::env_flag("POMODORO_LOCAL_ALWAYS_ALLOW", true) {
        return false;
    }
    LOCAL_RULE_SOURCES.contains(&source)
}

/// 读全部未过期规则。
pub fn local_rule_read_all() -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(local_rule_file()) else {
        return vec![];
    };
    let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&text) else {
        return vec![];
    };
    let now = now_ms();
    list.into_iter()
        .filter(|r| {
            if !is_truthy(r) {
                return false;
            }
            match crate::util::get(r, "at").and_then(Value::as_i64) {
                // JS: `!r.at || now - r.at < TTL` —— 没有 at 的条目**不过期**
                None => true,
                Some(at) => now - at < LOCAL_RULE_TTL_MS,
            }
        })
        .collect()
}

fn local_rule_key(source: &str, tool: &str) -> String {
    format!("{source}:{}", crate::proto::normalize_tool_name(tool))
}

/// `localRuleMatch(source, tool, rule)`：规则内容做「互相包含」判断 ——
/// `npm run build --watch` 命中已存的 `npm run build` 算允许。
pub fn local_rule_match(source: &str, tool: &str, rule: &str) -> bool {
    if !local_rule_enabled(source) {
        return false;
    }
    let key = local_rule_key(source, tool);
    // JS: `String(rule || '*')` —— 空串也落成 `*`
    let r = if rule.is_empty() { "*" } else { rule };
    local_rule_read_all().iter().any(|item| {
        let item_source = crate::util::pick_str(item, &["source"]);
        let item_tool = crate::util::pick_str(item, &["tool"]);
        if local_rule_key(&item_source, &item_tool) != key {
            return false;
        }
        let saved_raw = crate::util::pick_str(item, &["rule"]);
        let saved = if saved_raw.is_empty() { "*" } else { saved_raw.as_str() };
        if saved == "*" || r == "*" {
            return true;
        }
        r.contains(saved) || saved.contains(r)
    })
}

/// `localRuleAdd(source, tool, rule)`：记不上就算了（下次还会问，不影响安全）。
pub fn local_rule_add(source: &str, tool: &str, rule: &str) {
    if !local_rule_enabled(source) {
        return;
    }
    let key = local_rule_key(source, tool);
    let r = if rule.is_empty() { "*" } else { rule };

    // 同工具下已有更宽泛（或相同）的规则就不必再记一条 —— 否则规则表会越滚越长，
    // 而且 `*` 之后再记具体规则是多余的
    let mut list: Vec<Value> = local_rule_read_all()
        .into_iter()
        .filter(|item| {
            let item_source = crate::util::pick_str(item, &["source"]);
            let item_tool = crate::util::pick_str(item, &["tool"]);
            if local_rule_key(&item_source, &item_tool) != key {
                return true;
            }
            let saved_raw = crate::util::pick_str(item, &["rule"]);
            let saved = if saved_raw.is_empty() { "*" } else { saved_raw.as_str() };
            !(saved == "*" || saved == r)
        })
        .collect();

    list.push(json!({ "source": source, "tool": tool, "rule": r, "at": now_ms() }));
    let start = list.len().saturating_sub(LOCAL_RULE_MAX);
    let trimmed = &list[start..];

    let dir = cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // JS 用 `JSON.stringify(list, null, 0)` —— 紧凑输出
    let body = Value::Array(trimmed.to_vec()).to_string();
    let _ = std::fs::write(local_rule_file(), body);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些用例会读写真实 `%TEMP%` 下的目录 —— 用随机 key / 唯一 source 避免互相干扰，
    /// 也不去 **删除** 目录（可能删掉另一个正在跑的 hook 的缓存）。
    #[test]
    fn cache_roundtrip_and_ttl() {
        let key = format!("test-{}", sha1_hex_prefix(&format!("{}", now_ms()), 12));
        assert!(cache_read(&key).is_none());
        cache_write(&key, &json!({ "ok": true, "decidedBy": "user" }));
        let got = cache_read(&key).expect("刚写的应该能读回来");
        assert_eq!(got["decidedBy"], "user");
        // 过期就当作没有
        let stale = cache_dir().join(format!("{key}-stale.json"));
        let _ = std::fs::create_dir_all(cache_dir());
        let _ = std::fs::write(
            &stale,
            json!({ "at": now_ms() - CACHE_TTL_MS - 1000, "result": { "ok": true } }).to_string(),
        );
        assert!(cache_read(&format!("{key}-stale")).is_none());
    }

    #[test]
    fn cache_key_prefers_tool_use_id() {
        let a = json!({ "tool_use_id": "abc", "tool_name": "AskUserQuestion" });
        let b = json!({ "tool_use_id": "abc", "tool_name": "别的" });
        assert_eq!(cache_key_for(&a), cache_key_for(&b));
        assert_eq!(cache_key_for(&a).len(), 20);
        // 没有 id 时退到 `tool:input`
        let c = json!({ "tool_name": "Bash", "tool_input": { "command": "ls" } });
        let d = json!({ "tool_name": "Bash", "tool_input": { "command": "pwd" } });
        assert_ne!(cache_key_for(&c), cache_key_for(&d));
        assert_ne!(cache_key_for(&c), cache_key_for(&a));
    }

    /// 🔒 与 Node 逐字节对齐的黄金值。
    ///
    /// 参考值由 `scripts/keygen-ref.mjs`（**跑的是真的 `crypto.createHash('sha1')`**）
    /// 生成。改动 [`cache_key_for`] 或 [`crate::util::sha1_hex_prefix`] 时，
    /// 必须重跑那个脚本并把新值贴过来 —— 这条一致性是「两版共用同一个
    /// `%TEMP%/pomodoro-hook-cache`」的前提，破了就会出现「一次提问弹两次窗」。
    #[test]
    fn cache_key_matches_node_reference_values() {
        let cases: [(Value, &str); 6] = [
            (
                json!({ "tool_use_id": "toolu_01ABC", "tool_name": "AskUserQuestion" }),
                "66e360e7cbbf78f9605d",
            ),
            (
                json!({ "tool_name": "Bash", "tool_input": { "command": "npm test" } }),
                "e0577db1ad921bbd923c",
            ),
            (
                // 多字段 + 中文：验证 `JSON.stringify` ↔ `Value::to_string` 的紧凑写法一致
                json!({ "tool_name": "Bash",
                        "tool_input": { "command": "ls", "description": "列出文件" } }),
                "31a0563aa3f874a34cdc",
            ),
            (
                json!({ "tool_name": "", "tool_input": {} }),
                "9a61b7a78c1cfef7acbb",
            ),
            (
                // Windows 路径（反斜杠要转义）+ 空格 + 中文目录
                json!({ "tool_name": "Write",
                        "tool_input": { "file_path": "C:\\项目\\a b.ts", "content": "x=1" } }),
                "ee81053d4420ea5addb9",
            ),
            (
                json!({ "tool_name": "AskUserQuestion",
                        "tool_input": { "questions": [{ "question": "继续吗？", "options": ["是", "否"] }] } }),
                "019f6a3e6fc706ce4bc3",
            ),
        ];
        for (payload, want) in cases {
            assert_eq!(cache_key_for(&payload), want, "payload={payload}");
        }
    }

    #[test]
    fn local_rules_only_apply_to_known_sources() {
        // Claude Code / ZCode 能回写规则，不该走本地规则（否则会多一份状态要同步）
        assert!(!local_rule_enabled("claude-code"));
        assert!(!local_rule_enabled("zcode"));
        assert!(local_rule_enabled("vscode"));
        assert!(local_rule_enabled("trae"));
        assert!(local_rule_enabled("cursor"));
        assert!(local_rule_enabled("codex"));
    }

    #[test]
    fn normalize_tool_name_used_in_rule_key() {
        // 规则键用归一化后的工具名：`vscode/askQuestions` 与 `AskUserQuestion`
        // 若不同源就不该串味
        let k1 = local_rule_key("cursor", "vscode/askQuestions");
        let k2 = local_rule_key("cursor", "ask_questions");
        assert_eq!(k1, k2);
        assert_ne!(local_rule_key("cursor", "Bash"), local_rule_key("trae", "Bash"));
    }
}
