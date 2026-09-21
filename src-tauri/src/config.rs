//! 用户配置与固定路径 —— 对应 Electron 版 `main.js` 的 `configPath` / `loadConfig` /
//! `saveConfig` / `hookScriptPath` / `opencodePluginPath`。
//!
//! ⚠ **目录必须和 Electron 版是同一个**：`%APPDATA%/pomodoro-fluent`。
//! 实测本机只有这一个目录（没有 `%APPDATA%/番茄钟`），因为 Electron 的
//! `app.getName()` 取的是 package.json 顶层的 `name`（`productName` 只写在
//! `build` 段里，`app.getPath('userData')` 读不到）。
//! 两版共用同一份 `config.json`，用户在两版之间切换时「网关开关」等设置不会丢。
//!
//! 目录来自 [`pomodoro_core::user_data_dir`]（**不是** Tauri 的 `app_data_dir`）——
//! 后者按 identifier `com.pomodoro.fluent` 拼，和现有安装不是一个地方，
//! hook CLI 也就找不到 `gateway.json` 了。

use std::path::PathBuf;

use serde_json::{json, Value};

/// `config.json` 路径
pub fn config_path() -> PathBuf {
    pomodoro_core::user_data_dir().join("config.json")
}

/// 读配置。文件不存在 / 坏掉一律当空对象 —— 配置坏了不该让应用起不来。
pub fn load() -> Value {
    let text = match std::fs::read_to_string(config_path()) {
        Ok(t) => t,
        Err(_) => return json!({}),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

/// 合并写入一条配置（读-改-写，与 `saveConfig` 一致）。返回合并后的完整配置。
pub fn save_patch(patch: Value) -> Value {
    let mut merged = load();
    if let (Some(dst), Some(src)) = (merged.as_object_mut(), patch.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    write(&merged);
    merged
}

fn write(value: &Value) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[config] 建目录失败 {}: {e}", dir.display());
            return;
        }
    }
    // 与 Electron 的 JSON.stringify(merged, null, 2) 同形（缩进 2 空格）
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!("[config] 写配置失败 {}: {e}", path.display());
    }
}

/// 网关开关。**缺省为开** —— 与 `loadConfig().gatewayEnabled !== false` 等价。
pub fn gateway_enabled() -> bool {
    load()
        .get("gatewayEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// hook 脚本落地的固定路径：`<userData>/hook/pomodoro-hook.js`。
///
/// 渲染层的「复制配置」片段用得上它。M2 里这个文件还由 Electron 版/用户自己放；
/// M3 把 hook CLI Rust 化后，变成 Rust 版自己释放（到时只改这一处）。
pub fn hook_script_path() -> PathBuf {
    pomodoro_core::user_data_dir().join("hook").join("pomodoro-hook.js")
}

/// OpenCode 插件路径（`install --agent opencode` 要用）
pub fn opencode_plugin_path() -> PathBuf {
    pomodoro_core::user_data_dir()
        .join("hook")
        .join("opencode")
        .join("pomodoro-opencode.ts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_share_one_root() {
        // 三个路径必须都在同一棵树下，否则「换机器后配置找不到」会变成静默 bug
        let root = pomodoro_core::user_data_dir();
        assert_eq!(config_path().parent().unwrap(), root);
        assert!(hook_script_path().starts_with(&root));
        assert!(opencode_plugin_path().starts_with(&root));
        assert!(hook_script_path().ends_with("hook/pomodoro-hook.js"));
        assert!(opencode_plugin_path().ends_with("hook/opencode/pomodoro-opencode.ts"));
    }

    #[test]
    fn save_patch_merges_rather_than_replaces() {
        // 合并语义：只覆盖带来的那个键（这是 `saveConfig` 的核心约定，
        // 换成整体覆盖会静默清掉窗口位置等其它配置）
        let mut base = json!({ "gatewayEnabled": true, "other": 1 });
        let patch = json!({ "gatewayEnabled": false });
        if let (Some(dst), Some(src)) = (base.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        assert_eq!(base["gatewayEnabled"], false);
        assert_eq!(base["other"], 1, "没带来的键必须留着");
    }
}
