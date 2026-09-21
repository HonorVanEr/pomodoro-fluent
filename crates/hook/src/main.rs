//! pomodoro-hook —— 宿主 hook CLI。
//!
//! 与运行中的番茄钟（本地 Agent 网关）联动，把 agent 的提问 / 权限请求 / 通知
//! 变成可在弹窗内直接交互的桌面弹窗。**番茄钟未运行时静默退出，绝不阻断 agent。**
//!
//! **本 crate 绝对不能加 `windows_subsystem = "windows"`**：hook 的输出是决策
//! JSON，必须可靠写进 stdout。GUI subsystem 的进程在部分宿主/终端组合下拿不到
//! console，写不出 stdout 就等于弹窗白等一整轮超时。
//!
//! 适配三类协议（与 `bin/pomodoro-hook.js` 一致）：
//!   * Cursor / Claude 系（含 ZCode / VS Code Copilot / Qwen Code）—— stdin 一个 JSON
//!   * OpenCode —— 插件把 JSON 走 stdin 交进来，stdout 回 `{status}` / `{answers}`
//!   * Codex —— `hooks.json`（同 Claude 系）与老的 `config.toml` notify 两条通道
//!
//! 用法：
//! ```text
//!   pomodoro-hook.exe                       # hook 模式：stdin JSON（自动识别宿主协议）
//!   pomodoro-hook.exe codex-notify          # Codex CLI：回合结束通知
//!   pomodoro-hook.exe opencode-permission   # OpenCode 插件：权限请求 → stdout {status}
//!   pomodoro-hook.exe opencode-question     # OpenCode 插件：提问 → stdout {answers}
//!   pomodoro-hook.exe opencode-event        # OpenCode 插件：会话事件上报
//!   pomodoro-hook.exe ask --question "Q" --option A --option B
//!   pomodoro-hook.exe permission --tool Bash --detail "npm test"
//!   pomodoro-hook.exe notify --title T --message M
//!   pomodoro-hook.exe status / sessions
//!   pomodoro-hook.exe install --agent <宿主> [--print] [--clean] [--with-notify]
//! ```
//!
//! 环境变量：`POMODORO_GATEWAY_FILE` / `POMODORO_PORT` / `POMODORO_TOKEN` /
//! `POMODORO_SOURCE` / `POMODORO_ASK` / `POMODORO_PERMISSION` /
//! `POMODORO_LOCAL_ALWAYS_ALLOW` / `POMODORO_ALWAYS_ALLOW` / `POMODORO_PERMISSION_DEST` /
//! `POMODORO_TIMEOUT_S` / `POMODORO_ASK_MODE` / `POMODORO_HOOK_PATH`

mod ancli;
mod cache;
mod codex;
mod cursor;
mod http;
mod install;
mod manual;
mod opencode;
mod proto;
mod session;
mod util;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};

use serde_json::{json, Value};

use crate::util::{basename, get, is_truthy, pick_str, short_session};

/// 版本号跟整个仓库走（workspace 继承自 `package.json`）。
///
/// ⚠ 老版 Node CLI 里有个独立的 `HOOK_VERSION = '2.1.0'` —— 那个号**已经作废**。
/// 两版同一个 release 共发，hook 版本必须与安装包版本一致，否则用户报问题时
/// 完全对不上到底是哪个构建。
const HOOK_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

/// `process.stdout.write(JSON.stringify(v))` —— **不带换行**。
///
/// 不带换行是刻意的：宿主读的是「整个 stdout 当一段 JSON」，多一个换行虽然一般
/// 也能解析，但 OpenCode 插件那边是 `JSON.parse(out)`，容错完全靠运气。
pub fn print_compact(v: &Value) {
    let mut out = std::io::stdout();
    let _ = out.write_all(v.to_string().as_bytes());
    let _ = out.flush();
}

/// `JSON.stringify(v, null, 2) + '\n'`（手动调试 / status / sessions 用）
pub fn print_pretty_line(v: &Value) {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".into());
    println!("{text}");
}

fn output(v: Option<Value>) {
    if let Some(v) = v {
        print_compact(&v);
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() {
    // 任何失败都不阻断 agent：stderr 留痕，**exit 0**（install 的退出码除外，
    // 那是给设置面板判断成败用的接口）
    let code = run().unwrap_or_else(|e| {
        eprintln!("[pomodoro-hook] {e}");
        0
    });
    // ⚠ 显式 flush：`process::exit` 不会跑析构，stdout 的行缓冲里可能还压着
    // 「已写入 xxx」这类输出，不 flush 就丢了（表现为"安装成功但界面没内容"）
    let _ = std::io::stdout().flush();
    if code != 0 {
        std::process::exit(code);
    }
}

fn run() -> Result<i32, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // `--source <宿主>`：install 生成的命令会带上，用来显式标记来源
    // （弹窗徽标才不会把 ZCode 认成 Claude Code —— 两者协议同形）
    if let Some(i) = args.iter().position(|a| a == "--source") {
        if let Some(v) = args.get(i + 1) {
            std::env::set_var("POMODORO_SOURCE", v);
            args.drain(i..i + 2);
        }
    }

    // 这两个不需要网关在线
    match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | Some("help") => {
            usage();
            return Ok(0);
        }
        Some("install") => {
            let has = |f: &str| args.iter().any(|a| a == f);
            let inst = install::Installer::new(has("--print"), has("--clean"), has("--with-notify"));
            return Ok(inst.run(&args[1..]));
        }
        _ => {}
    }

    let Some(gw) = http::find_gateway() else {
        // 番茄钟未运行：hook 场景**静默**退出（不阻断 agent），手动调用给一句提示
        if !args.is_empty() {
            eprintln!("[pomodoro-hook] 找不到番茄钟网关（应用未启动？）");
        }
        return Ok(0);
    };

    // ---- hook 模式：stdin JSON，自动识别协议 ----
    if args.is_empty() {
        let raw = read_stdin()?;
        let payload = parse_json_maybe(&raw);
        match proto::detect_protocol(&payload) {
            "cursor" => output(cursor::run(&gw, &payload)?),
            "opencode-permission" => output(opencode::run_permission(&gw, &payload)?),
            "opencode-question" => {
                let out = opencode::run_question(&gw, &payload)?;
                let fin =
                    opencode::finish_question(&payload, out.as_ref().unwrap_or(&Value::Null))?;
                output(fin);
            }
            "opencode-event" => output(opencode::run_event(&gw, &payload)?),
            _ => output(ancli::run(&gw, &payload)?),
        }
        return Ok(0);
    }

    let cmd = args[0].as_str();
    match cmd {
        "opencode-permission" => {
            let raw = read_stdin()?;
            output(opencode::run_permission(&gw, &parse_json_maybe(&raw))?);
            return Ok(0);
        }
        "opencode-question" => {
            let raw = read_stdin()?;
            let payload = parse_json_maybe(&raw);
            let out = opencode::run_question(&gw, &payload)?;
            let fin = opencode::finish_question(&payload, out.as_ref().unwrap_or(&Value::Null))?;
            output(fin);
            return Ok(0);
        }
        "opencode-event" => {
            let raw = read_stdin()?;
            output(opencode::run_event(&gw, &parse_json_maybe(&raw))?);
            return Ok(0);
        }
        // Codex 的 notify 会把 JSON 当参数传过来（也可能走 stdin，两种都支持）
        "codex-notify" => {
            let inline = args[1..]
                .iter()
                .rev()
                .find(|a| a.trim().starts_with('{'))
                .cloned();
            let payload = match inline {
                Some(j) => parse_json_maybe(&j),
                None => Value::Null,
            };
            let payload = if is_truthy(&payload) {
                payload
            } else {
                match read_stdin() {
                    Ok(s) => parse_json_maybe(&s),
                    Err(_) => json!({}),
                }
            };
            output(codex::run_notify(&gw, &payload)?);
            return Ok(0);
        }
        "status" => {
            let r = http::request(gw.port, &gw.token, "GET", "/api/status", None, 8000)
                .map_err(|e| e.to_string())?;
            print_pretty_line(&r);
            return Ok(0);
        }
        // 看看 hook 都跟踪到了哪些会话（任务 / 最近工具 / 项目）
        "sessions" => {
            print_pretty_line(&sessions_view());
            return Ok(0);
        }
        "ask" => {
            output(manual::run_ask(&gw, &args[1..])?);
            return Ok(0);
        }
        "permission" => {
            output(manual::run_permission(&gw, &args[1..])?);
            return Ok(0);
        }
        "notify" => {
            // `--key value` 成对解析，最后一个值不参与（没有配对的 value）
            let mut opts: HashMap<String, String> = HashMap::new();
            for i in 1..args.len().saturating_sub(1) {
                if let Some(k) = args[i].strip_prefix("--") {
                    if let Some(v) = args.get(i + 1) {
                        opts.insert(k.to_string(), v.clone());
                    }
                }
            }
            let get_opt = |k: &str, d: &str| -> String {
                opts.get(k).cloned().unwrap_or_else(|| d.to_string())
            };
            let r = http::request(
                gw.port,
                &gw.token,
                "POST",
                "/api/notify",
                Some(&json!({
                    "title": get_opt("title", "番茄钟"),
                    "message": get_opt("message", ""),
                    "sub": get_opt("sub", ""),
                    "type": get_opt("type", "agent"),
                })),
                8000,
            )
            .map_err(|e| e.to_string())?;
            if !is_truthy(&r) || !is_truthy(&get(&r, "ok").cloned().unwrap_or(Value::Null)) {
                return Err("notify 失败".into());
            }
            return Ok(0);
        }
        _ => {}
    }

    // 认不出来的子命令：帮助 + 非零退出
    usage();
    Ok(1)
}

/// `sessions` 子命令的输出形状。
///
/// ⚠ 与 JS 版有一处**有意保留**的差异：`since` 是 ISO-8601 **UTC**
/// （`2026-09-21T06:30:00.000Z`）而不是 `toLocaleString()` 的本地时间串。
/// 理由是为了一个纯调试字段引一整套时区库（chrono/tzdata）不值得，
/// 而且本机时区是 UTC+8，肉眼换算没有风险。
fn sessions_view() -> Value {
    let list = session::list()
        .into_iter()
        .map(|s| {
            let cwd = pick_str(&s, &["cwd"]);
            let project = {
                let p = pick_str(&s, &["project"]);
                if !p.is_empty() {
                    p
                } else if cwd.is_empty() {
                    String::new()
                } else {
                    basename(&cwd)
                }
            };
            json!({
                "session": short_session(&pick_str(&s, &["sessionId"])),
                "source": pick_str(&s, &["source"]),
                "project": project,
                "task": pick_str(&s, &["task"]),
                "lastTool": pick_str(&s, &["lastTool"]),
                "lastToolDetail": pick_str(&s, &["lastToolDetail"]),
                "agentType": pick_str(&s, &["agentType"]),
                "since": pomodoro_core::iso8601_ms(
                    get(&s, "at").and_then(Value::as_i64).unwrap_or(0)
                ),
            })
        })
        .collect::<Vec<_>>();
    Value::Array(list)
}

/// `readStdin()` —— 5s 内没读完就放弃。
///
/// 兜底的意义：hook 配置写歪时（比如宿主把 stdin 开着却不写）不能让 hook 挂死，
/// 挂死就是 agent 卡住。
fn read_stdin() -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        return Err("no stdin".into());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let result = std::io::stdin()
            .read_to_string(&mut buf)
            .map(|_| buf)
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    match rx.recv_timeout(std::time::Duration::from_millis(5_000)) {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(format!("stdin: {e}")),
        Err(_) => Err("stdin timeout".into()),
    }
}

/// `parseJsonMaybe(text)` —— 解析失败给空对象（不是报错）。
///
/// 宿主偶尔会往 stdin 里塞非 JSON（日志、警告），那种情况下应当按"没有 payload"
/// 走，而不是让 hook 崩掉。
fn parse_json_maybe(text: &str) -> Value {
    if text.is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!({}))
}

fn usage() {
    println!(
        "番茄钟 Agent Hook CLI v{HOOK_VERSION}\n\
         用法:\n\
         \x20 pomodoro-hook.exe                       # hook 模式：stdin JSON（自动识别宿主协议）\n\
         \x20 pomodoro-hook.exe codex-notify          # Codex CLI：回合结束通知（JSON 走参数或 stdin）\n\
         \x20 pomodoro-hook.exe opencode-permission   # OpenCode 插件：权限请求 → stdout {{status}}\n\
         \x20 pomodoro-hook.exe opencode-question     # OpenCode 插件：提问 → stdout {{answers}}\n\
         \x20 pomodoro-hook.exe opencode-event        # OpenCode 插件：会话事件上报\n\
         \x20 pomodoro-hook.exe ask --question \"Q\" --option A --option B [--task \"任务\" --agent zcode]\n\
         \x20 pomodoro-hook.exe permission --tool Bash --detail \"npm test\" [--task \"任务\"]\n\
         \x20 pomodoro-hook.exe notify --title T --message M [--sub S] [--type agent]\n\
         \x20 pomodoro-hook.exe status\n\
         \x20 pomodoro-hook.exe sessions               # 查看 hook 跟踪到的会话（任务/最近工具/项目）\n\
         \x20 pomodoro-hook.exe install --agent <宿主> [--print] [--clean] [--with-notify]\n\
         \n\
         \x20 宿主：{}\n\
         \x20 --print        只打印将要写入的配置，不动文件\n\
         \x20 --clean        同时清掉指向番茄钟 hook 其它副本的旧条目\n\
         \x20 --with-notify  仅 Codex：额外改写 config.toml 的 notify（默认不动）",
        install::HOOK_AGENT_NAMES.join(" / ") + " / all"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_maybe_never_panics() {
        assert_eq!(parse_json_maybe(""), json!({}));
        assert_eq!(parse_json_maybe("不是 json"), json!({}));
        assert_eq!(parse_json_maybe("[1,2]"), json!([1, 2]));
        assert_eq!(parse_json_maybe(r#"{"a":1}"#)["a"], 1);
    }

    #[test]
    fn sessions_view_shape_is_stable() {
        // 空会话列表也必须回数组（渲染层是 `Array.isArray` 判断）
        let v = sessions_view();
        assert!(v.is_array());
    }

    #[test]
    fn usage_text_mentions_every_host() {
        // 帮助文本里的宿主列表来自 HOOK_AGENT_NAMES，漏一个就说明两处又分叉了
        let names = install::HOOK_AGENT_NAMES.join(" / ");
        assert_eq!(names, "zcode / claude / vscode / trae / cursor / opencode / codex / qwen");
    }

    #[test]
    fn json_key_order_is_preserved() {
        // ⚠ 这条钉的是 `serde_json` 的 `preserve_order` 特性，**不是**随便一个断言。
        // 它必须在 `crates/hook/Cargo.toml` 和 `src-tauri/Cargo.toml` **两处**都开：
        // 关掉会退回 BTreeMap，序列化时按键排序 → 写进用户配置文件的键序变了
        // （和 Electron 版一字排开的顺序不一致，也会在 `install --print` 的输出上
        // 产生无意义的 diff）。回归时这条会立刻变红。
        let v: serde_json::Value = serde_json::from_str(r#"{"z":1,"a":2,"m":3}"#).unwrap();
        assert_eq!(v.to_string(), r#"{"z":1,"a":2,"m":3}"#);
    }
}
