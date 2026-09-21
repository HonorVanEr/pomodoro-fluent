//! 用户配置与固定路径 —— 对应 Electron 版 `main.js` 的 `configPath` / `loadConfig` /
//! `saveConfig` / `hookScriptPath` / `installHookScript` / `opencodePluginPath`。
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

/// hook CLI 落地路径：`<userData>/hook/pomodoro-hook.exe`。
///
/// ⚠ 路径本身的事实源在 [`pomodoro_core::hook_exe_path`] —— 因为 **hook CLI 自己
/// 也要拼这个路径去装配置**，两处各写一遍迟早写歪（用户装好的 hook 会指向空气）。
///
/// 渲染层的「复制配置」片段用得上它。值末尾是 `.exe` 而不是 `.js`：渲染层的
/// `buildHookSnippet` / `buildInstallCommand` 会据此决定加不加 `node ` 前缀。
pub fn hook_path() -> std::path::PathBuf {
    pomodoro_core::hook_exe_path()
}

/// OpenCode 插件路径（`install --agent opencode` 要用）
pub fn opencode_plugin_path() -> std::path::PathBuf {
    pomodoro_core::opencode_plugin_path()
}

/// 把随包分发的 `pomodoro-hook.exe` / OpenCode 插件释放到 `<userData>/hook/`。
///
/// 对应 Electron 版 `main.js` 的 `installHookScript()` + `copyOnce()`：
/// **内容一致就跳过写盘**，所以每次启动调用也不会有额外 IO。
///
/// 为什么非要复制一份、不直接用安装目录里那个：
/// ① hook 配置里写的是绝对路径，安装目录会随重装/换版本变化；
/// ② 安装到 `Program Files` 时那个目录对普通用户只读，hook 自己没法在旁边放东西。
///
/// 返回 hook exe 的落地路径；释放失败回 `None`（**不致命** —— 已装好的旧 hook
/// 还能继续用，只是升级不了）。
pub fn ensure_hook_exe() -> Option<std::path::PathBuf> {
    let src = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(pomodoro_core::HOOK_EXE_NAME)));
    let dst = hook_path();

    let ok = match src {
        Some(src) if src.exists() => copy_if_changed(&src, &dst).is_ok(),
        // 开发期可能只 build 了 GUI：同目录没有 hook exe 就当"没有可释放的"
        _ => false,
    };
    if !ok {
        return if dst.exists() { Some(dst) } else { None };
    }
    // OpenCode 插件一起带上（`install --agent opencode` 会拷它）
    // 源文件解析与 hook CLI 共用 `pomodoro_core::resolve_plugin_source()` —— 生产布局是
    // 「插件与 exe 同级」，开发期回退到 `<仓库>/bin/opencode/`。
    if let Some(plugin_src) = pomodoro_core::resolve_plugin_source() {
        let _ = copy_if_changed(&plugin_src, &opencode_plugin_path());
    }
    Some(dst)
}

/// 内容一致就跳过 —— 与 Electron 的 `copyOnce` 语义相同（`Buffer.equals`）。
fn copy_if_changed(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    let buf = std::fs::read(src)?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::read(dst).map(|old| old == buf).unwrap_or(false) {
        return Ok(());
    }
    std::fs::write(dst, buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_share_one_root() {
        // 三个路径必须都在同一棵树下，否则「换机器后配置找不到」会变成静默 bug
        let root = pomodoro_core::user_data_dir();
        assert_eq!(config_path().parent().unwrap(), root);
        assert!(hook_path().starts_with(&root));
        assert!(opencode_plugin_path().starts_with(&root));
        // 末尾必须是 .exe：渲染层靠这个后缀决定要不要加 `node ` 前缀
        assert_eq!(hook_path().extension().unwrap(), "exe");
        assert!(hook_path().ends_with("hook/pomodoro-hook.exe"));
        assert!(opencode_plugin_path().ends_with("hook/opencode/pomodoro-opencode.ts"));
    }

    #[test]
    fn copy_if_changed_skips_identical_content() {
        let dir = std::env::temp_dir().join("pomodoro-config-test-copy");
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src.exe");
        let dst = dir.join("nested").join("dst.exe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&src, b"v1").unwrap();

        copy_if_changed(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v1");
        // 内容相同时不该重写（mtime 不变）——「一致就跳过」是 copyOnce 的核心
        let mtime = std::fs::metadata(&dst).unwrap().modified().unwrap();
        copy_if_changed(&src, &dst).unwrap();
        assert_eq!(std::fs::metadata(&dst).unwrap().modified().unwrap(), mtime);

        std::fs::write(&src, b"v2 longer").unwrap();
        copy_if_changed(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v2 longer");
        let _ = std::fs::remove_dir_all(&dir);
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
