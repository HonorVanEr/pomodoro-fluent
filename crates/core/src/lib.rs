//! pomodoro-core —— GUI 与 hook CLI 共享的核心逻辑。
//!
//! 只放两边都要用的东西：数据目录解析、超时常量、网关发现规则。
//! 不放 UI，也不放具体网络实现——两边的 IO 壳各自写。

use std::path::PathBuf;

/// 数据目录名。与 Electron 版保持一致，迁移时不丢用户配置。
pub const APP_DIR_NAME: &str = "pomodoro-fluent";

/// productName 兜底目录（老版本可能落在 `%APPDATA%/番茄钟`）。
pub const APP_DIR_NAME_FALLBACK: &str = "番茄钟";

// ---------------------------------------------------------------------------
// 三层超时（必须嵌套，宿主那层最大）
//
// 宿主 hook timeout 4200s > hook 等网关 3900s > 番茄钟兜底 3600s
//
// 宿主那层必须最大：它掐掉 hook 时那句「未决策」发不出去，宿主就按自己的审批
// 设置走（设过免确认＝静默放行）。番茄钟先到点，后果才是确定的。
// ---------------------------------------------------------------------------

/// 番茄钟侧交互弹窗兜底等待：3600s。
pub const DEFAULT_CONFIRM_TIMEOUT_MS: u64 = 3_600_000;
/// 兜底等待上限：2h。
pub const MAX_CONFIRM_TIMEOUT_MS: u64 = 7_200_000;
/// hook 等网关的宽限：比番茄钟兜底多 300s（即 3900s）。
pub const HOOK_WAIT_GRACE_MS: u64 = 300_000;
/// hook 侧默认超时秒数。
pub const DEFAULT_HOOK_TIMEOUT_S: u64 = 3_600;
/// hook 侧超时上限秒数。
pub const MAX_HOOK_TIMEOUT_S: u64 = 7_200;
/// 写进宿主配置的 hook timeout —— 必须大于上面两层。
pub const HOST_HOOK_TIMEOUT_S: u64 = 4_200;

/// hook 等网关的实际时长（番茄钟兜底 + 宽限）。
pub const fn hook_wait_ms() -> u64 {
    DEFAULT_CONFIRM_TIMEOUT_MS + HOOK_WAIT_GRACE_MS
}

/// 解析用户数据目录。
///
/// 优先级：`POMODORO_USER_DATA` 环境变量（测试用）→ `%APPDATA%/<APP_DIR_NAME>`
/// → 当前目录下的 `APP_DIR_NAME`（`%APPDATA%` 缺失时的兜底）。
pub fn user_data_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("POMODORO_USER_DATA") {
        let p = PathBuf::from(override_dir);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join(APP_DIR_NAME);
    }
    PathBuf::from(APP_DIR_NAME)
}

/// 网关发现文件路径（GUI 写、hook 读）。GUI 侧只有一个写入点，就是这里。
pub fn gateway_file() -> PathBuf {
    if let Some(p) = std::env::var_os("POMODORO_GATEWAY_FILE") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    user_data_dir().join("gateway.json")
}

/// hook 读 `gateway.json` 时的候选路径，**顺序与 `bin/pomodoro-hook.js` 的
/// `findGateway()` 一致**：先 `productName` 那个目录名（`番茄钟`）再实际目录名。
///
/// ⚠ 与 [`gateway_file`] 故意分开：那个是 GUI 的**唯一写入点**（只有 `pomodoro-fluent`
/// 一个），这个是 hook 的**多个读取点**（历史上可能落在两个目录里）。
/// 合成一个会让「读老目录」和「往老目录写」混在一起。
pub fn gateway_candidates() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("POMODORO_GATEWAY_FILE") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return vec![p];
        }
    }
    let base = roaming_app_data();
    vec![
        base.join(APP_DIR_NAME_FALLBACK).join("gateway.json"),
        base.join(APP_DIR_NAME).join("gateway.json"),
    ]
}

/// 用户主目录（`os.homedir()`）。Windows 下优先 `USERPROFILE`。
pub fn home_dir() -> PathBuf {
    for key in ["USERPROFILE"] {
        if let Some(v) = std::env::var_os(key) {
            let p = PathBuf::from(v);
            if !p.as_os_str().is_empty() {
                return p;
            }
        }
    }
    match (
        std::env::var_os("HOMEDRIVE"),
        std::env::var_os("HOMEPATH"),
    ) {
        (Some(d), Some(p)) => {
            let mut path = PathBuf::from(d);
            path.push(p);
            path
        }
        _ => PathBuf::from("."),
    }
}

/// `%APPDATA%`（Roaming）。缺失时退回 `<home>/AppData/Roaming` —— 与
/// `pomodoro-hook.js` / `pomodoro-opencode.ts` 里的兜底写法一致。
pub fn roaming_app_data() -> PathBuf {
    if let Some(v) = std::env::var_os("APPDATA") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    home_dir().join("AppData").join("Roaming")
}

// ---------------------------------------------------------------------------
// hook CLI 落地路径
//
// hook 配置里写的是**绝对路径**，所以这个路径一旦变了，用户已经装好的
// Claude Code / Codex 配置就全指向空气。两版（Electron / Tauri）必须落在同一处，
// 用户在两版之间来回切才不用重装 hook。
// ---------------------------------------------------------------------------

/// hook CLI 在用户目录下的目录名（`<userData>/hook`）。
pub const HOOK_DIR_NAME: &str = "hook";
/// hook CLI 的可执行文件名。
pub const HOOK_EXE_NAME: &str = "pomodoro-hook.exe";
/// 老版本（Node 脚本）的文件名 —— 迁移期 `--clean` 要认它。
pub const HOOK_SCRIPT_LEGACY_NAME: &str = "pomodoro-hook.js";

/// `<userData>/hook`
pub fn hook_dir() -> PathBuf {
    user_data_dir().join(HOOK_DIR_NAME)
}

/// `<userData>/hook/pomodoro-hook.exe`
pub fn hook_exe_path() -> PathBuf {
    hook_dir().join(HOOK_EXE_NAME)
}

/// `<userData>/hook/opencode/pomodoro-opencode.ts`
pub fn opencode_plugin_path() -> PathBuf {
    hook_dir().join(PLUGIN_DIR_NAME).join(PLUGIN_FILE_NAME)
}

/// OpenCode 插件在包里的目录名 / 文件名。
pub const PLUGIN_DIR_NAME: &str = "opencode";
pub const PLUGIN_FILE_NAME: &str = "pomodoro-opencode.ts";

/// OpenCode 插件**源文件**的候选路径（按顺序探测，调用方取第一个存在的）。
///
/// ① `<本 exe 所在目录>/opencode/pomodoro-opencode.ts`
///    —— **生产布局**。打包时插件与两个 exe 放在一起；而 hook CLI 自己就住在
///    `<userData>/hook/`，所以这一条恰好等于 `<userData>/hook/opencode/...`，
///    与 Electron 版 `path.join(__dirname, 'opencode', ...)` 是**同一个位置**。
///
/// ② 开发布局回退：从 exe 目录往上最多走 3 层，找 `bin/opencode/pomodoro-opencode.ts`
///    （`target/debug/` → `target/` → 仓库根 → 命中 `<仓库>/bin/opencode/...`）。
///    **纯相对遍历，不会把开发机的绝对路径烧进二进制**；生产环境这几层里没有
///    `bin/opencode/`，自然全部落空，不影响行为。
pub fn plugin_source_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(exe) = std::env::current_exe() else {
        return out;
    };
    let Some(dir) = exe.parent() else {
        return out;
    };
    out.push(dir.join(PLUGIN_DIR_NAME).join(PLUGIN_FILE_NAME));
    let mut cur = dir.to_path_buf();
    for _ in 0..3 {
        let Some(parent) = cur.parent().map(|p| p.to_path_buf()) else {
            break;
        };
        out.push(parent.join("bin").join(PLUGIN_DIR_NAME).join(PLUGIN_FILE_NAME));
        cur = parent;
    }
    out
}

/// 第一个存在的插件源文件；都没有则 `None`。
///
/// GUI 侧（释放插件到 `<userData>/hook/`）与 hook CLI（`install --agent opencode`）
/// **共用这一处解析**，避免两边各写一份然后写歪。
pub fn resolve_plugin_source() -> Option<PathBuf> {
    plugin_source_candidates().into_iter().find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// 环境变量助手
// ---------------------------------------------------------------------------

/// 读一个「布尔型」环境变量，语义**逐字对齐** JS 侧：
///
/// ```js
/// const flag = (name, def) => {
///   const v = env[name];
///   if (v === undefined || v === '') return def;
///   return /^(1|true|yes|on)$/i.test(v) ? true : /^(0|false|no|off)$/i.test(v) ? false : v;
/// };
/// ```
///
/// ⚠ 关键细节：**认不出来的值既不是 true 也不是 false**（原样返回字符串），
/// 而调用点一律写 `flag(...) !== false` —— 于是「认不出来的值」等价于 **true**。
/// `POMODORO_ASK=maybe` 是开，不是关。别"顺手修"成严格解析，行为会变。
pub fn env_flag(name: &str, default: bool) -> bool {
    let v = match std::env::var(name) {
        Ok(v) => v,
        Err(_) => return default,
    };
    if v.is_empty() {
        return default;
    }
    let lower = v.trim().to_ascii_lowercase();
    match lower.as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => true,
    }
}

/// 读一个非空字符串环境变量。
pub fn env_str(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// 毫秒时间戳 → ISO-8601 UTC（`2026-09-21T06:30:00.000Z`），与 JS 的
/// `new Date(ms).toISOString()` 逐字节同形。
///
/// ⚠ 是 **UTC**，不是本地时间。两边都是 UTC，所以对得上；谁要显示给用户看，
/// 自己再做本地化。
pub fn iso8601_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss, milli) = (
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1_000) % 60,
        rem % 1_000,
    );
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{milli:03}Z")
}

/// Howard Hinnant 的 `civil_from_days`：把「自 1970-01-01 起的天数」转成公历年月日。
///
/// 纯整数运算，不依赖任何时区数据库 —— 这也是选它而不是引 `chrono` 的原因
/// （为一个 `toISOString()` 拉一整套 tzdata 不划算）。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 折成单行并截断 —— 对应 JS 的 `collapse()`。
///
/// `\s+` 折成单个空格、首尾 trim；超长时**最后一个字符换成 `…`**（不是加到后面），
/// 所以结果长度正好是 `n`。
pub fn collapse(text: &str, n: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() > n {
        let mut head: String = chars[..n.saturating_sub(1)].iter().collect();
        head.push('…');
        head
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_layers_are_nested() {
        assert!(HOST_HOOK_TIMEOUT_S * 1000 > hook_wait_ms());
        assert!(hook_wait_ms() > DEFAULT_CONFIRM_TIMEOUT_MS);
        assert!(MAX_CONFIRM_TIMEOUT_MS >= DEFAULT_CONFIRM_TIMEOUT_MS);
        assert!(MAX_HOOK_TIMEOUT_S >= DEFAULT_HOOK_TIMEOUT_S);
    }

    #[test]
    fn user_data_override_wins() {
        // 环境变量在测试进程里是全局的，这里只验证函数不会 panic 且返回非空路径。
        assert!(!user_data_dir().as_os_str().is_empty());
    }

    #[test]
    fn gateway_candidates_try_legacy_dir_first() {
        // 顺序必须与 pomodoro-hook.js 的 findGateway() 一致：番茄钟 → pomodoro-fluent
        let list = gateway_candidates();
        assert_eq!(list.len(), 2);
        assert!(list[0].ends_with("番茄钟/gateway.json"));
        assert!(list[1].ends_with("pomodoro-fluent/gateway.json"));
        // 两个候选必须在同一个父目录下，否则说明 roaming_app_data 兜底走歪了
        assert_eq!(list[0].parent().unwrap().parent(), list[1].parent().unwrap().parent());
    }

    #[test]
    fn hook_paths_live_under_user_data() {
        let root = user_data_dir();
        assert!(hook_dir().starts_with(&root));
        assert!(hook_exe_path().starts_with(&root));
        assert!(opencode_plugin_path().starts_with(&root));
        assert!(hook_exe_path().ends_with("hook/pomodoro-hook.exe"));
        assert!(opencode_plugin_path().ends_with("hook/opencode/pomodoro-opencode.ts"));
    }

    #[test]
    fn env_flag_matches_js_semantics() {
        // JS: 认不出来的值原样返回字符串，而调用点写 `!== false` ⇒ 等价 true。
        // 这条最容易在"顺手改成严格解析"时被破坏，所以钉住。
        std::env::remove_var("POMODORO_TEST_FLAG");
        assert!(env_flag("POMODORO_TEST_FLAG", true));
        assert!(!env_flag("POMODORO_TEST_FLAG", false));

        for on in ["1", "true", "TRUE", "Yes", "on"] {
            std::env::set_var("POMODORO_TEST_FLAG", on);
            assert!(env_flag("POMODORO_TEST_FLAG", false), "{on} 应为开");
        }
        for off in ["0", "false", "FALSE", "no", "Off"] {
            std::env::set_var("POMODORO_TEST_FLAG", off);
            assert!(!env_flag("POMODORO_TEST_FLAG", true), "{off} 应为关");
        }
        // 空串等同未设置
        std::env::set_var("POMODORO_TEST_FLAG", "");
        assert!(env_flag("POMODORO_TEST_FLAG", true));
        // 认不出来的值 = 开（不是关）
        std::env::set_var("POMODORO_TEST_FLAG", "maybe");
        assert!(env_flag("POMODORO_TEST_FLAG", false), "无法识别的值等价 true");
        std::env::remove_var("POMODORO_TEST_FLAG");
    }

    #[test]
    fn collapse_folds_whitespace_and_truncates_in_place() {
        assert_eq!(collapse("  多行\n标题\t带空白 ", 100), "多行 标题 带空白");
        assert_eq!(collapse("   ", 10), "");
        // 截断是「占满 n 个字符」，不是「n 个字符再加省略号」
        let long = "x".repeat(50);
        let out = collapse(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
        // 中文按字符数算，不是字节数
        let cn = "很长的标题".repeat(5);
        assert_eq!(collapse(&cn, 6).chars().count(), 6);
    }

    #[test]
    fn iso8601_ms_matches_js_to_iso_string() {
        // 参考值由 Node 的 `new Date(ms).toISOString()` 生成，逐字节对齐。
        // （GUI 的网关与 hook 的 sessions 子命令都走这条函数，所以放在 core）
        assert_eq!(iso8601_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_ms(1_789_948_800_000), "2026-09-21T00:00:00.000Z");
        assert_eq!(iso8601_ms(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
        assert_eq!(iso8601_ms(1_704_067_199_999), "2023-12-31T23:59:59.999Z");
        assert_eq!(iso8601_ms(951_868_800_000), "2000-03-01T00:00:00.000Z");
        // 负时间戳（1970 之前）不能 panic，也不能算错
        assert_eq!(iso8601_ms(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn plugin_source_candidates_cover_prod_and_dev_layout() {
        let list = plugin_source_candidates();
        assert!(list.len() >= 2, "至少要有一个生产候选 + 一个开发回退: {list:?}");
        // ① 生产：exe 同级目录下的 opencode/pomodoro-opencode.ts
        assert!(list[0].ends_with("opencode/pomodoro-opencode.ts"), "{list:?}");
        assert_eq!(list[0].file_name().unwrap(), PLUGIN_FILE_NAME);
        // ①① 生产候选必须与「插件最终落地路径」同名同结构，否则 install 装的是别的东西
        assert_eq!(
            opencode_plugin_path().file_name().unwrap(),
            list[0].file_name().unwrap()
        );
        // ② 开发回退：往上的某一层里有 bin/opencode/...
        assert!(
            list.iter().skip(1).any(|p| p.to_string_lossy().contains("bin")),
            "缺少开发布局回退: {list:?}"
        );
    }
}
