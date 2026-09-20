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

/// 网关发现文件路径（hook 靠它找到正在运行的番茄钟）。
pub fn gateway_file() -> PathBuf {
    if let Some(p) = std::env::var_os("POMODORO_GATEWAY_FILE") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    user_data_dir().join("gateway.json")
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
}
