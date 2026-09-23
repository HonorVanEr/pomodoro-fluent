//! 「关于」面板需要的东西：版本信息、检查更新、打开外链。
//!
//! ⚠ **番茄钟本身不联网**。只有用户在设置抽屉里点了「检查更新」，这里才会发一次
//! HTTPS 请求。启动不查、定时不查、后台不查 —— 这是项目铁律，别在这里加"顺便
//! 上报一下"之类的逻辑。请求放主进程是因为渲染层有 CSP（`default-src 'self'`），
//! 直接 fetch GitHub 会被挡掉。
//!
//! 不引 electron-updater 的等价物：发现新版本只给「前往下载」，交给系统浏览器，
//! 应用自己不落地安装包。

use serde_json::{json, Value};
use tauri::AppHandle;

const REPO_OWNER: &str = "HonorVanEr";
const REPO_NAME: &str = "pomodoro-fluent";

/// GitHub release 接口（tag 无前缀，见 docs/release.md）
const UPDATE_API: &str = "https://api.github.com/repos/HonorVanEr/pomodoro-fluent/releases/latest";

const REQUEST_TIMEOUT_S: u64 = 10;
/// release 说明截断长度（渲染层展示用，太长会把抽屉撑爆）
const NOTES_LIMIT: usize = 800;

pub fn repo_url() -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}")
}

// ---------------------------------------------------------------------------
// 版本比较
// ---------------------------------------------------------------------------

/// 只取前导数字段：`v1.1.3` → `[1,1,3]`。
///
/// 遇到非数字段就**停**，所以 `1.1.3-beta` 也是 `[1,1,3]`（预发布尾缀不参与比较，
/// 不会被拆出第 4 段 0）。与 `main.js` 的 `parseVersion` 行为一致。
pub fn parse_version(v: &str) -> Vec<u64> {
    let s = v.trim();
    let s = s
        .strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s);
    let mut out = Vec::new();
    for seg in s.split('.') {
        let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        match digits.parse::<u64>() {
            Ok(n) => out.push(n),
            // 数字长得离谱（溢出 u64）：当作比较到此为止，而不是整段作废
            Err(_) => break,
        }
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

/// 语义化版本比较：`a > b` 返回 1，`a < b` 返回 -1，相等返回 0。缺的段按 0 补。
pub fn compare_version(a: &str, b: &str) -> i32 {
    let pa = parse_version(a);
    let pb = parse_version(b);
    for i in 0..pa.len().max(pb.len()) {
        let d = *pa.get(i).unwrap_or(&0) as i64 - *pb.get(i).unwrap_or(&0) as i64;
        if d != 0 {
            return if d > 0 { 1 } else { -1 };
        }
    }
    0
}

// ---------------------------------------------------------------------------
// 检查更新
// ---------------------------------------------------------------------------

/// 拉最新 release。**这是全应用唯一的网络出口调用点。**
fn fetch_latest_release() -> Result<String, String> {
    let resp = minreq::get(UPDATE_API)
        .with_header("User-Agent", "pomodoro-fluent")
        .with_header("Accept", "application/vnd.github+json")
        .with_timeout(REQUEST_TIMEOUT_S)
        .send()
        .map_err(|e| format!("网络请求失败：{e}"))?;
    match resp.status_code {
        // 匿名调用额度是每小时 60 次，自用足够，但被限要给句人话
        403 => Err("GitHub 接口限流，稍后再试".to_string()),
        404 => Err("仓库还没有 release".to_string()),
        s if s >= 400 => Err(format!("GitHub 返回 {s}")),
        _ => resp
            .as_str()
            .map(str::to_string)
            .map_err(|e| format!("返回内容解析失败：{e}")),
    }
}

/// 检查更新。**阻塞**调用，调用方负责放到后台线程（见 `commands::app_check_update`）。
///
/// 返回值形状与 Electron 版逐字段一致（渲染层的渲染分支按 `ok` / `hasUpdate` 走）。
pub fn check_update(current: &str) -> Value {
    let body = match fetch_latest_release() {
        Ok(b) => b,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let data: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return json!({ "ok": false, "error": "返回内容解析失败" }),
    };

    let tag = data.get("tag_name").and_then(Value::as_str).unwrap_or("");
    let latest = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
        .to_string();
    if latest.is_empty() {
        return json!({ "ok": false, "error": "没读到 release 版本号" });
    }

    // 注意：截断按「字符」而不是 JS 的「UTF-16 码元」。中文说明最多差几位，
    // 不会截断出半个字。
    let notes: String = data
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(NOTES_LIMIT)
        .collect();

    let fallback_url = format!("{}/releases/latest", repo_url());
    json!({
        "ok": true,
        "currentVersion": current,
        "latestVersion": latest,
        "hasUpdate": compare_version(current, &latest) < 0,
        "releaseUrl": data.get("html_url").and_then(Value::as_str).unwrap_or(&fallback_url),
        "publishedAt": data.get("published_at").and_then(Value::as_str).unwrap_or(""),
        "notes": notes,
    })
}

// ---------------------------------------------------------------------------
// 应用信息
// ---------------------------------------------------------------------------

/// 平台标识，**故意写成 Electron 的措辞**（`win32-x64` 而不是 `windows-x86_64`）：
/// 关于面板那一行是渲染层拼的，两版措辞不同会显得像 bug。
fn platform_label() -> String {
    let os = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

pub fn app_info(app: &AppHandle) -> Value {
    let pkg = app.package_info();
    // 渲染层只拼「{platform} · {license}」，不再显示运行环境细节
    // （旧版「Electron  · Node  · …」那行是 M1 的已知显示瑕疵，已随渲染层一并去掉）。
    json!({
        "name": pkg.name,
        "version": pkg.version.to_string(),
        "platform": platform_label(),
        "repoUrl": repo_url(),
        "license": "MIT",
        "author": "HonorVanEr",
    })
}

// ---------------------------------------------------------------------------
// 打开外链
// ---------------------------------------------------------------------------

/// 用系统默认浏览器打开外链（等价 Electron 的 `shell.openExternal`）。
///
/// 只接受 `http://` / `https://` —— 别把 `file://` 之类的东西丢给系统去执行。
///
/// 实现上直接调 shell32 的 `ShellExecuteW`，而不是引 `tauri-plugin-opener`：
/// 后者会连 `open` / `windows` / `schemars` / `url` 一起拖进来，为一次调用不划算
/// （本项目的安装包只有 1 MB 量级，加不起）。`ShellExecuteW` 的 ABI 几十年没变过，
/// 手写声明是安全的。
#[cfg(windows)]
pub fn open_url(url: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut core::ffi::c_void,
            lpoperation: *const u16,
            lpfile: *const u16,
            lpparameters: *const u16,
            lpdirectory: *const u16,
            nshowcmd: i32,
        ) -> *mut core::ffi::c_void;
    }

    /// SW_SHOWNORMAL
    const SW_SHOWNORMAL: i32 = 1;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let op = wide("open");
    let file = wide(url);
    // SAFETY: 两个指针都指向以 NUL 结尾、且在调用期间存活的 UTF-16 缓冲；
    // hwnd / 参数 / 目录传 null（不依赖父窗口，无参数）。
    // 返回值 <= 32 表示失败（微软文档约定），所以先转 isize 再比。
    let ret = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ret as isize > 32
}

/// 非 Windows 平台没有这条路径（本项目只发布 Windows 包）。
#[cfg(not(windows))]
pub fn open_url(_url: &str) -> bool {
    false
}

/// 外链白名单：只有 http(s) 才允许交给系统。
pub fn is_allowed_external(url: &str) -> bool {
    let u = url.trim();
    u.len() > 8 && (u.starts_with("http://") || u.starts_with("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_strips_v_and_takes_leading_digits() {
        assert_eq!(parse_version("v1.1.3"), vec![1, 1, 3]);
        assert_eq!(parse_version("1.1.3"), vec![1, 1, 3]);
        assert_eq!(parse_version("V2.0.0"), vec![2, 0, 0]);
        // 预发布尾缀不参与比较（"3-beta" 以数字开头，前导数字取到 3 就停）
        assert_eq!(parse_version("1.1.3-beta"), vec![1, 1, 3]);
        // ⚠ `+` 构建元信息这里**不按 semver 忽略**，别照直觉改：
        // 算法先按 `.` 切段（与 main.js 的 parseVersion 逐行一致），
        // "1.1.3+build.7" → ["1","1","3+build","7"]，而第 4 段 "7" 是纯数字、
        // 会被照收 → [1,1,3,7]。Electron 版同样返回 [1,1,3,7]。
        // 本项目的 tag 一律是 vX.Y.Z，用不到构建元信息，
        // 「与 Electron 版行为一致」优先于「符合 semver」。
        assert_eq!(parse_version("1.1.3+build.7"), vec![1, 1, 3, 7]);
        assert_eq!(parse_version("  1.2.3  "), vec![1, 2, 3]);
    }

    #[test]
    fn parse_version_never_returns_empty() {
        assert_eq!(parse_version(""), vec![0]);
        assert_eq!(parse_version("v"), vec![0]);
        assert_eq!(parse_version("abc"), vec![0]);
    }

    #[test]
    fn compare_version_orders_correctly() {
        assert_eq!(compare_version("1.1.6", "1.1.6"), 0);
        assert!(compare_version("1.1.6", "1.1.5") > 0);
        assert!(compare_version("1.1.5", "1.1.6") < 0);
        assert!(compare_version("1.2.0", "1.10.0") < 0, "按数字段比，不是字典序");
        assert!(compare_version("2.0.0", "1.99.99") > 0);
    }

    #[test]
    fn compare_version_pads_missing_segments() {
        assert_eq!(compare_version("1.1", "1.1.0"), 0);
        assert!(compare_version("1.1.1", "1.1") > 0);
        assert!(compare_version("1.0", "1.0.1") < 0);
    }

    #[test]
    fn compare_version_ignores_prerelease_suffix() {
        // 与 Electron 版一致：1.1.6-beta 看作 1.1.6
        assert_eq!(compare_version("1.1.6-beta", "1.1.6"), 0);
        assert!(compare_version("1.1.6-beta", "1.1.5") > 0);
    }

    #[test]
    fn platform_label_matches_electron_wording() {
        let p = platform_label();
        assert!(p.starts_with("win32-"), "实际: {p}");
        assert!(p.ends_with("-x64") || p.ends_with("-arm64") || p.ends_with("-x86"), "实际: {p}");
    }

    #[test]
    fn external_url_whitelist() {
        assert!(is_allowed_external("https://github.com/HonorVanEr/pomodoro-fluent"));
        assert!(is_allowed_external("http://example.com"));
        assert!(!is_allowed_external("file:///C:/windows/system32/calc.exe"));
        assert!(!is_allowed_external("javascript:alert(1)"));
        assert!(!is_allowed_external("ms-settings:"));
        assert!(!is_allowed_external(""));
        assert!(!is_allowed_external("https:/"));
    }

    #[test]
    fn repo_url_points_at_the_real_repo() {
        assert_eq!(repo_url(), "https://github.com/HonorVanEr/pomodoro-fluent");
    }
}
