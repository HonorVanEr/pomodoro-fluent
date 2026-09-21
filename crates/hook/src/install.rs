//! `install` 子命令：把 hook 写进各宿主配置（写前先备份）。
//!
//! 对应 `pomodoro-hook.js` 第 1172–1555 行。
//!
//! ## 从「node 脚本」迁移到「exe」时改了哪三处
//!
//! 老用户配置里写的是 `node "…/pomodoro-hook.js" --source x`，新版写的是
//! `"…/pomodoro-hook.exe" --source x`。为此：
//!  1. [`Installer::hook_command_shell`] 不再加 `node ` 前缀；
//!  2. [`is_pomodoro_entry`] 从「匹配 `pomodoro-hook.js`」放宽到「匹配 `pomodoro-hook`」——
//!     这样 `--clean` 才会把老的 `.js` 条目当成"自己的旧副本"清掉，
//!     而不是留着它跟自己一起跑（那会让每次工具调用弹两次窗）；
//!  3. Codex 的 `notify` 数组从 `["node", "<脚本>", "codex-notify"]` 变成
//!     `["<exe>", "codex-notify"]`。
//!
//! ## 退出码是接口的一部分
//!
//! 设置面板的「一键安装」**只看退出码**判断成败。所以未知宿主、写盘失败都必须
//! 非零退出 —— 以前这里只往 stderr 写一行就继续，会被误报成「安装完成」。

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::proto::HOST_WAIT_MS;
use crate::proto::HOST_WAIT_SEC;
use crate::util::{arr, as_str_lossy, is_truthy, pick_str};

/// 支持的宿主列表（`install` 与帮助文本共用一份，别两处各写一遍）。
pub const HOOK_AGENT_NAMES: [&str; 8] = [
    "zcode", "claude", "vscode", "trae", "cursor", "opencode", "codex", "qwen",
];

/// Codex 的 `matcher` 是**真生效的正则**，所以可以拿它收窄，省掉每个工具调用都
/// spawn 一次进程。
///
/// ⚠ Codex 的 `UserPromptSubmit` 与 `Stop` 的 matcher **不生效**，别给它们写 matcher。
const CODEX_TOOL_MATCHER: &str = "Bash|apply_patch|Edit|Write|mcp__.*";

/// Trae 的 PreToolUse matcher —— 2026-09-18 起它只处理提问，所以收窄到提问工具。
const TRAE_PRETOOL_MATCHER: &str = "AskUserQuestion";

/// 安全上限：读用户配置文件时的最大字节数（防止误指向一个巨大的文件把内存吃光）。
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

pub struct Installer {
    pub print: bool,
    pub clean: bool,
    pub with_notify: bool,
    /// 本 CLI 自己的绝对路径 —— 写进宿主配置的就是它
    own: String,
}

impl Installer {
    pub fn new(print: bool, clean: bool, with_notify: bool) -> Self {
        Self {
            print,
            clean,
            with_notify,
            own: own_path(),
        }
    }

    /// `runInstall(args)`
    pub fn run(&self, args: &[String]) -> i32 {
        let agent = flag_value(args, "agent").unwrap_or_else(|| "all".to_string());

        // 未知宿主必须非零退出：设置面板的「一键安装」靠退出码判断成败
        if agent != "all" && !HOOK_AGENT_NAMES.contains(&agent.as_str()) {
            eprintln!(
                "[pomodoro-hook] 未知 agent: {agent}（可选：{} / all）",
                HOOK_AGENT_NAMES.join(" / ")
            );
            return 1;
        }

        let targets: Vec<&str> = if agent == "all" {
            HOOK_AGENT_NAMES.to_vec()
        } else {
            // 上面已校验过 agent 一定在名单里
            vec![HOOK_AGENT_NAMES
                .iter()
                .find(|n| **n == agent)
                .copied()
                .unwrap_or("claude")]
        };
        let mut failed: Vec<&str> = Vec::new();
        for t in targets {
            let r = match t {
                "claude" => self.install_claude(),
                "zcode" => self.install_zcode(),
                "vscode" => self.install_vscode(),
                "trae" => self.install_trae(),
                "cursor" => self.install_cursor(),
                "opencode" => self.install_opencode(),
                "codex" => self.install_codex(),
                "qwen" => self.install_qwen(),
                _ => Ok(()),
            };
            if let Err(e) = r {
                failed.push(t);
                eprintln!("[pomodoro-hook] 安装 {t} 失败：{e}");
            }
        }

        // 写盘失败也必须非零退出（权限、磁盘满、目录被占用…），否则前端会误报成功
        if !failed.is_empty() {
            eprintln!(
                "[pomodoro-hook] 有 {} 个宿主安装失败：{}",
                failed.len(),
                failed.join("、")
            );
            return 1;
        }
        if !self.print {
            println!("改动需重启对应 agent 会话后生效。");
        }
        0
    }

    // -----------------------------------------------------------------------
    // 共用
    // -----------------------------------------------------------------------

    /// 各宿主配置里统一用 `"<exe>" --source <来源>`：
    /// 显式标记来源，弹窗徽标才不会认错宿主（ZCode 与 Claude Code 协议同形）。
    ///
    /// ⚠ 路径带引号是必须的：装在 `Program Files` 或用户名含空格时，不带引号的
    /// 路径会被宿主按空格切开，hook 直接起不来（表现为"配了但没弹窗"）。
    fn hook_command_shell(&self, source: &str) -> Value {
        json!({ "type": "command", "command": format!("\"{}\" --source {source}", self.own) })
    }

    /// `writeJson(file, obj, print)`
    fn write_json(&self, file: &Path, obj: &Value) -> Result<(), String> {
        let pretty = serde_json::to_string_pretty(obj).unwrap_or_else(|_| "{}".into());
        if self.print {
            println!("--- {} ---", file.display());
            println!("{pretty}");
            return Ok(());
        }
        // 备份失败不阻断安装（用户可能没写权限，或者原文件压根不存在）
        if file.exists() {
            let _ = std::fs::copy(file, format!("{}.pomodoro.bak", file.display()));
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("建目录 {} 失败：{e}", dir.display()))?;
        }
        std::fs::write(file, pretty).map_err(|e| format!("写入 {} 失败：{e}", file.display()))?;
        let base = file
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        println!(
            "已写入 {}（原文件备份为 {base}.pomodoro.bak）",
            file.display()
        );
        Ok(())
    }

    /// `pushHook(container, eventName, entry)`
    ///
    /// `--clean` 会先清掉指向**别的副本**的旧条目 —— 不清的话，上次装在别处的 hook
    /// 会跟这次装的一起跑，每次工具调用弹两次窗。
    fn push_hook(&self, container: &mut Map<String, Value>, event: &str, entry: Value) {
        let mut list: Vec<Value> = match container.get(event) {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        if self.clean {
            list.retain(|g| {
                let fp = hook_fingerprint(g);
                // 不是番茄钟的条目一律留着（别碰别人的 hook）；
                // 是自己但指向别的路径的，删掉
                !is_pomodoro_entry(&fp) || fp.contains(&self.own)
            });
        }
        let fp = hook_fingerprint(&entry);
        if !list.iter().any(|g| hook_fingerprint(g) == fp) {
            list.push(entry);
        }
        container.insert(event.to_string(), Value::Array(list));
    }

    // -----------------------------------------------------------------------
    // Claude Code / ZCode / VS Code / Qwen 同族（Claude 兼容格式）
    // -----------------------------------------------------------------------

    fn install_ancli_hooks(&self, file: &Path, source: &str) -> Result<Value, String> {
        let mut cfg = read_json(file);
        let mut hooks = take_object(&mut cfg, "hooks");
        let h = self.hook_command_shell(source);
        let with_timeout = |e: &Value, secs: u64| {
            let mut m = match e {
                Value::Object(o) => o.clone(),
                _ => Map::new(),
            };
            m.insert("timeout".into(), json!(secs));
            Value::Object(m)
        };

        self.push_hook(&mut hooks, "Notification", json!({ "hooks": [h] }));
        self.push_hook(
            &mut hooks,
            "PermissionRequest",
            json!({ "matcher": "*", "hooks": [with_timeout(&h, HOST_WAIT_SEC)] }),
        );
        self.push_hook(
            &mut hooks,
            "PreToolUse",
            json!({ "matcher": "AskUserQuestion|askQuestions|askQuestion",
                    "hooks": [with_timeout(&h, HOST_WAIT_SEC)] }),
        );
        // UserPromptSubmit：唯一携带「用户这次让 agent 干什么」的事件。
        // ⚠ 漏了它，弹窗上的任务名永远是空的（`context.task` 只从这里来）
        self.push_hook(&mut hooks, "UserPromptSubmit", json!({ "hooks": [h] }));
        self.push_hook(&mut hooks, "Stop", json!({ "hooks": [h] }));
        self.push_hook(&mut hooks, "SubagentStop", json!({ "hooks": [h] }));
        self.push_hook(
            &mut hooks,
            "PostToolUse",
            json!({ "matcher": "*", "hooks": [h] }),
        );

        cfg.insert("hooks".into(), Value::Object(hooks));
        Ok(Value::Object(cfg))
    }

    fn install_claude(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir().join(".claude").join("settings.json");
        let cfg = self.install_ancli_hooks(&file, "claude-code")?;
        self.write_json(&file, &cfg)
    }

    fn install_qwen(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir().join(".qwen").join("settings.json");
        let cfg = self.install_ancli_hooks(&file, "qwen")?;
        self.write_json(&file, &cfg)
    }

    /// VS Code Copilot Agent hooks：与 Claude Code 同格式，但事件集只有 8 个
    /// （`SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` /
    /// `PreCompact` / `SubagentStart` / `SubagentStop` / `Stop`），
    /// **没有** `PermissionRequest` 与 `Notification`。
    ///
    /// 用户级放 `~/.copilot/hooks/*.json`；条目用 `timeout`（单位：秒，默认 30）。
    /// 长轮询等用户在弹窗里点按钮，默认 30s 会直接被宿主掐断 → 显式放大到
    /// [`HOST_WAIT_SEC`]。
    fn install_vscode(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir()
            .join(".copilot")
            .join("hooks")
            .join("pomodoro.json");
        let mut cfg = read_json(&file);
        if !cfg.get("version").map(is_truthy).unwrap_or(false) {
            cfg.insert("version".into(), json!(1));
        }
        let mut hooks = take_object(&mut cfg, "hooks");
        let cmd = format!("\"{}\" --source vscode", self.own);

        let add = |hooks: &mut Map<String, Value>, evt: &str, timeout: Option<u64>| {
            // ⚠ 先判存在再取可变引用：`match hooks.get_mut(..)` 的借用活到整个 match，
            // 在 `_` 分支里再 `hooks.insert` 会被借用检查拦下
            if !hooks.contains_key(evt) {
                hooks.insert(evt.to_string(), Value::Array(Vec::new()));
            }
            let Some(Value::Array(list)) = hooks.get_mut(evt) else {
                return;
            };
            let already = list
                .iter()
                .any(|e| pick_str(e, &["command"]).contains("pomodoro-hook"));
            if already {
                return;
            }
            let mut entry = Map::new();
            entry.insert("type".into(), json!("command"));
            entry.insert("command".into(), json!(cmd));
            if let Some(t) = timeout {
                entry.insert("timeout".into(), json!(t));
            }
            list.push(Value::Object(entry));
        };

        // 提问靠 PreToolUse（VS Code 没有 PermissionRequest）；2026-09-18 起不再做工具审批
        add(&mut hooks, "PreToolUse", Some(HOST_WAIT_SEC));
        add(&mut hooks, "PostToolUse", Some(30));
        add(&mut hooks, "SessionStart", Some(30));
        add(&mut hooks, "UserPromptSubmit", Some(30));
        add(&mut hooks, "SubagentStart", Some(30));
        add(&mut hooks, "SubagentStop", Some(30));
        add(&mut hooks, "PreCompact", Some(30));
        add(&mut hooks, "Stop", Some(30));

        cfg.insert("hooks".into(), Value::Object(hooks));
        self.write_json(&file, &Value::Object(cfg))?;
        if !self.print {
            println!(
                "提示：VS Code 默认还会读取 ~/.claude/settings.json（Claude Code 的 hooks），\n\
                 \x20     两处都装会对同一次工具调用跑两遍。不想重复就在 VS Code 设置里加：\n\
                 \x20     \"chat.hookFilesLocations\": {{ \"~/.claude/settings.json\": false }}"
            );
        }
        Ok(())
    }

    /// Trae（字节）：6 个事件 `SessionStart` / `UserPromptSubmit` / `PreToolUse` /
    /// `PostToolUse` / `Stop` / `Notification` —— **有 `Notification`，但没有
    /// `PermissionRequest`**。
    ///
    /// 配置是 Claude Code 那种嵌套结构，全局放 `%userprofile%/.trae-cn/hooks.json`。
    fn install_trae(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir().join(".trae-cn").join("hooks.json");
        let mut cfg = read_json(&file);
        // Trae 的 version 是**强制**写 1（不是 `|| 1`），照抄
        cfg.insert("version".into(), json!(1));
        let mut hooks = take_object(&mut cfg, "hooks");
        let h = self.hook_command_shell("trae");
        let with_timeout = |e: &Value, secs: u64| {
            let mut m = match e {
                Value::Object(o) => o.clone(),
                _ => Map::new(),
            };
            m.insert("timeout".into(), json!(secs));
            Value::Object(m)
        };

        // PreToolUse 要等用户点弹窗 → 超时必须放大（Trae 默认 30 秒，会直接掐掉）
        self.push_hook(
            &mut hooks,
            "PreToolUse",
            json!({ "matcher": TRAE_PRETOOL_MATCHER, "hooks": [with_timeout(&h, HOST_WAIT_SEC)] }),
        );
        self.push_hook(
            &mut hooks,
            "Notification",
            json!({ "hooks": [with_timeout(&h, 30)] }),
        );
        self.push_hook(&mut hooks, "Stop", json!({ "hooks": [with_timeout(&h, 30)] }));
        self.push_hook(
            &mut hooks,
            "SessionStart",
            json!({ "hooks": [with_timeout(&h, 30)] }),
        );
        self.push_hook(
            &mut hooks,
            "UserPromptSubmit",
            json!({ "hooks": [with_timeout(&h, 30)] }),
        );
        self.push_hook(
            &mut hooks,
            "PostToolUse",
            json!({ "matcher": "*", "hooks": [with_timeout(&h, 30)] }),
        );

        cfg.insert("hooks".into(), Value::Object(hooks));
        self.write_json(&file, &Value::Object(cfg))?;
        if !self.print {
            println!(
                "Trae 两条注意事项：\n\
                 \x20 1) 创建 Hook 时选「本地自动运行」而非「沙箱运行」—— 沙箱会限制系统权限，\n\
                 \x20    hook 可能连不上本机 127.0.0.1:5277 的番茄钟网关（连不上就静默跳过，不弹窗）。\n\
                 \x20 2) Trae 会同时读 Claude Code 的 Hook 配置并合并执行；若 ~/.claude/settings.json\n\
                 \x20    里也有番茄钟的 hook，同一次调用会跑两遍。二选一，或用 --clean 收敛。"
            );
        }
        Ok(())
    }

    /// Cursor（`~/.cursor/hooks.json`，用户级；项目级可放 `.cursor/hooks.json`）
    fn install_cursor(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir().join(".cursor").join("hooks.json");
        let mut cfg = read_json(&file);
        if !cfg.get("version").map(is_truthy).unwrap_or(false) {
            cfg.insert("version".into(), json!(1));
        }
        let mut hooks = take_object(&mut cfg, "hooks");
        let cmd = format!("\"{}\" --source cursor", self.own);

        let add = |hooks: &mut Map<String, Value>, evt: &str, timeout: Option<u64>| {
            // ⚠ 这里的去重是「按 `command` 里有没有 pomodoro-hook」而不是整条指纹：
            // Cursor 同一事件下**改完 timeout 再装不会叠加**一条
            if !hooks.contains_key(evt) {
                hooks.insert(evt.to_string(), Value::Array(Vec::new()));
            }
            let Some(Value::Array(list)) = hooks.get_mut(evt) else {
                return;
            };
            if list
                .iter()
                .any(|e| pick_str(e, &["command"]).contains("pomodoro-hook"))
            {
                return;
            }
            let mut entry = Map::new();
            entry.insert("command".into(), json!(cmd));
            if let Some(t) = timeout {
                entry.insert("timeout".into(), json!(t));
            }
            list.push(Value::Object(entry));
        };

        add(&mut hooks, "beforeShellExecution", Some(HOST_WAIT_SEC));
        add(&mut hooks, "preToolUse", Some(HOST_WAIT_SEC));
        add(&mut hooks, "beforeMCPExecution", Some(HOST_WAIT_SEC));
        add(&mut hooks, "beforeSubmitPrompt", None);
        add(&mut hooks, "afterFileEdit", None);
        add(&mut hooks, "afterShellExecution", None);
        add(&mut hooks, "afterAgentResponse", None);
        add(&mut hooks, "stop", None);

        cfg.insert("hooks".into(), Value::Object(hooks));
        self.write_json(&file, &Value::Object(cfg))
    }

    // -----------------------------------------------------------------------
    // Codex CLI
    // -----------------------------------------------------------------------

    /// Codex（codex-cli 0.154.x）：**12 个 hook 事件**，配置放 `~/.codex/hooks.json`
    /// （或 config.toml 的内联 `[hooks]`；同一层两者都有会**都加载并告警** → 只用
    /// hooks.json）。
    ///
    /// 与其他宿主最大的不同：Codex **有独立的 `PermissionRequest` 事件**，而且只在
    /// 「Codex 本来就要问用户」时才触发 —— 这正是想要的语义，不用像 VS Code / Trae
    /// 那样在 PreToolUse 上猜哪些调用会被宿主拦，也就没有「用户已在宿主设了自动允许、
    /// 弹窗还在问」的误报。
    fn install_codex(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir().join(".codex").join("hooks.json");
        let mut cfg = read_json(&file);
        let mut hooks = take_object(&mut cfg, "hooks");
        let h = self.hook_command_shell("codex");
        let with_timeout = |e: &Value, secs: u64| {
            let mut m = match e {
                Value::Object(o) => o.clone(),
                _ => Map::new(),
            };
            m.insert("timeout".into(), json!(secs));
            Value::Object(m)
        };

        // 审批：Codex 唯一认 allow/deny 的地方，要等用户点弹窗 → timeout 必须放大
        self.push_hook(
            &mut hooks,
            "PermissionRequest",
            json!({ "hooks": [with_timeout(&h, HOST_WAIT_SEC)] }),
        );
        // 以下都只上报，不回决策
        for evt in ["PreToolUse", "PostToolUse"] {
            self.push_hook(
                &mut hooks,
                evt,
                json!({ "matcher": CODEX_TOOL_MATCHER, "hooks": [with_timeout(&h, 30)] }),
            );
        }
        // UserPromptSubmit：唯一带「用户这次让 agent 干什么」的事件（payload.prompt）。
        // ⚠ 它的 matcher 在 Codex 上不生效，所以不给它写 matcher
        for evt in [
            "UserPromptSubmit",
            "Stop",
            "SubagentStart",
            "SubagentStop",
            "SessionStart",
            "SessionEnd",
            "PreCompact",
            "PostCompact",
            "Interrupt",
        ] {
            self.push_hook(&mut hooks, evt, json!({ "hooks": [with_timeout(&h, 30)] }));
        }

        cfg.insert("hooks".into(), Value::Object(hooks));
        self.write_json(&file, &Value::Object(cfg))?;

        if self.print {
            // 干跑也要把将要写入的 notify 显示出来，否则 --print 会漏报这次改动
            if self.with_notify {
                self.install_codex_notify()?;
            }
            return Ok(());
        }

        let mut notes = vec![
            "Codex 注意事项：".to_string(),
            format!(
                "  1) 配置在 {}（用户级，不受项目信任影响）；项目级 .codex/ 只在项目被信任后加载。",
                file.display()
            ),
            "     同一层里 hooks.json 与 config.toml 的 [hooks] 同时存在会两条都跑并告警 → 二选一。"
                .to_string(),
            "  2) 审批走 PermissionRequest（只在 Codex 本来就要问时才触发）。PreToolUse 只上报活动："
                .to_string(),
            "     它的 allow/ask 在 Codex 上不生效，只有 deny 有效（带非空 reason）。".to_string(),
            "  3) 本命令不改写 config.toml 的 notify，避免覆盖你已有的通知工具；".to_string(),
            "     需要旧的回合结束回调时加：install --agent codex --with-notify".to_string(),
        ];
        if codex_hooks_disabled() {
            notes.push(
                "  ⚠ 你的 ~/.codex/config.toml 里写了 [features] hooks = false —— hooks 被关了，\
                 \n     弹窗不会出现。删掉这行或改成 true 再试。"
                    .to_string(),
            );
        }
        if codex_inline_hooks() {
            notes.push(
                "  ⚠ 你的 ~/.codex/config.toml 里已有内联 [hooks] 段 —— 会与本次写入的 hooks.json\
                 \n     同时加载并告警，同一次调用可能跑两遍。建议二选一。"
                    .to_string(),
            );
        }
        println!("{}", notes.join("\n"));

        if self.with_notify {
            self.install_codex_notify()?;
        }
        Ok(())
    }

    /// Codex 的老通道（`~/.codex/config.toml` 的 `notify`）：回合结束回调一次。
    ///
    /// ⚠ 默认**不**改写它 —— 用户可能已经把它指向别的工具（例如 codex-computer-use）。
    fn install_codex_notify(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir().join(".codex").join("config.toml");
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        // TOML 字符串里的反斜杠要转义：`C:\a` 会被 TOML 当成转义序列，`\a` 不是合法
        // 转义 → 解析直接报错，notify 静默失效
        let hook_arg = self.own.replace('\\', "\\\\");
        // 老版是 `["node", "<脚本>", "codex-notify"]`，现在是 exe，直接作为 argv[0]
        let line = format!("notify = [\"{hook_arg}\", \"codex-notify\"]");

        let mut lines: Vec<String> = text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
        match lines.iter().position(|l| {
            let t = l.trim_start();
            t.starts_with("notify") && t[6..].trim_start().starts_with('=')
        }) {
            Some(i) => lines[i] = line,
            None => {
                while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                    lines.pop();
                }
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push("# 番茄钟：回合结束时通知（由 pomodoro-hook install 写入）".to_string());
                lines.push(line);
            }
        }
        let next = format!("{}\n", lines.join("\n"));

        if self.print {
            println!("--- {} ---", file.display());
            println!("{next}");
            return Ok(());
        }
        if file.exists() {
            let _ = std::fs::copy(&file, format!("{}.pomodoro.bak", file.display()));
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
        }
        std::fs::write(&file, next).map_err(|e| format!("写入 {} 失败：{e}", file.display()))?;
        println!(
            "已写入 {}（原文件备份为 config.toml.pomodoro.bak）",
            file.display()
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ZCode / OpenCode
    // -----------------------------------------------------------------------

    fn install_zcode(&self) -> Result<(), String> {
        let file = pomodoro_core::home_dir()
            .join(".zcode")
            .join("cli")
            .join("config.json");
        let mut cfg = read_json(&file);
        let mut hooks = take_object(&mut cfg, "hooks");
        // 必须显式开启 —— ZCode 的 hooks 默认是关的
        hooks.insert("enabled".into(), json!(true));
        if !hooks.get("timeoutMs").map(is_truthy).unwrap_or(false) {
            hooks.insert("timeoutMs".into(), json!(HOST_WAIT_MS));
        }
        let mut events = take_object(&mut hooks, "events");
        let h = self.hook_command_shell("zcode");
        // ZCode 是**毫秒制**宿主
        let long = {
            let mut m = match &h {
                Value::Object(o) => o.clone(),
                _ => Map::new(),
            };
            m.insert("timeoutMs".into(), json!(HOST_WAIT_MS));
            Value::Object(m)
        };

        // AskUserQuestion：ZCode 会同时触发 PreToolUse 与 PermissionRequest，两个都接上
        self.push_hook(
            &mut events,
            "PermissionRequest",
            json!({ "matcher": "*", "hooks": [long] }),
        );
        self.push_hook(
            &mut events,
            "PreToolUse",
            json!({ "matcher": "AskUserQuestion", "hooks": [long] }),
        );
        self.push_hook(&mut events, "Stop", json!({ "hooks": [h] }));
        self.push_hook(
            &mut events,
            "PostToolUse",
            json!({ "matcher": "*", "hooks": [h] }),
        );
        self.push_hook(
            &mut events,
            "PostToolUseFailure",
            json!({ "matcher": "*", "hooks": [h] }),
        );

        hooks.insert("events".into(), Value::Object(events));
        cfg.insert("hooks".into(), Value::Object(hooks));
        self.write_json(&file, &Value::Object(cfg))
    }

    fn install_opencode(&self) -> Result<(), String> {
        let home = pomodoro_core::home_dir();
        // 源文件解析走 pomodoro_core：GUI 释放插件时用的是同一个函数，
        // 两边各写一份迟早写歪（写歪的表现就是"插件装了但 OpenCode 认不出"）。
        let plugin_src = pomodoro_core::resolve_plugin_source().ok_or_else(|| {
            let tried = pomodoro_core::plugin_source_candidates()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" / ");
            format!("找不到插件源文件（找过：{tried}）")
        })?;
        let cfg_dir = match pomodoro_core::env_str("XDG_CONFIG_HOME") {
            Some(x) => PathBuf::from(x).join("opencode"),
            None => home.join(".config").join("opencode"),
        };
        let plugin_dst = cfg_dir.join("plugins").join("pomodoro-opencode.ts");
        let cfg_file = cfg_dir.join("opencode.json");

        if !plugin_src.exists() {
            return Err(format!("找不到插件源文件: {}", plugin_src.display()));
        }
        if self.print {
            println!("--- 复制 {} → {} ---", plugin_src.display(), plugin_dst.display());
        } else {
            if let Some(dir) = plugin_dst.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
            }
            std::fs::copy(&plugin_src, &plugin_dst).map_err(|e| {
                format!(
                    "复制插件 {} → {} 失败：{e}",
                    plugin_src.display(),
                    plugin_dst.display()
                )
            })?;
            println!("已安装插件 {}", plugin_dst.display());
        }

        let mut cfg = read_json(&cfg_file);
        // OpenCode 的 plugin 字段要 `file://` URL，且路径分隔符必须转成正斜杠
        let entry = format!("file://{}", plugin_dst.display().to_string().replace('\\', "/"));
        let mut list: Vec<Value> = match cfg.get("plugin") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        if !list.iter().any(|v| v.as_str() == Some(entry.as_str())) {
            list.push(json!(entry));
        }
        cfg.insert("plugin".into(), Value::Array(list));
        self.write_json(&cfg_file, &Value::Object(cfg))
    }
}

// ---------------------------------------------------------------------------
// 自由函数
// ---------------------------------------------------------------------------

/// `ownPath()` —— 写进宿主配置的就是这个绝对路径。
///
/// `POMODORO_HOOK_PATH` 优先，给"我就是要让它指向别处的副本"留个口子
/// （Electron 版也是这样，插件侧同样读它）。
pub fn own_path() -> String {
    if let Some(p) = pomodoro_core::env_str("POMODORO_HOOK_PATH") {
        return p;
    }
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "pomodoro-hook.exe".to_string())
}

/// 读一个 JSON 配置。不存在 / 坏掉 / 不是对象 → 空对象
/// —— 配置坏了不该让安装直接失败。
fn read_json(file: &Path) -> Map<String, Value> {
    let Ok(meta) = std::fs::metadata(file) else {
        return Map::new();
    };
    if meta.len() > MAX_CONFIG_BYTES {
        return Map::new();
    }
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// `cfg.hooks = cfg.hooks || {}`：取出一个对象字段（不是对象就换新的）。
fn take_object(cfg: &Map<String, Value>, key: &str) -> Map<String, Value> {
    match cfg.get(key) {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    }
}

/// `hookFingerprint(entry)`
///
/// 忽略 `--source`：同一脚本换个来源标记重装，应该被认成同一条而不是叠加一条。
fn hook_fingerprint(entry: &Value) -> String {
    let hooks = arr(entry, "hooks");
    let joined = hooks
        .iter()
        .map(|h| {
            let cmd = pick_str(h, &["command"]);
            let args = arr(h, "args")
                .iter()
                .map(as_str_lossy)
                .collect::<Vec<_>>()
                .join(" ");
            format!("{cmd} {args}")
        })
        .collect::<Vec<_>>()
        .join("|");
    strip_source_flags(&joined)
        .replace(['"', '\''], "")
        .trim()
        .to_string()
}

/// 正则 `/--source\s+\S+/g` 的等价实现（去掉 `--source <值>`）。
fn strip_source_flags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("--source") {
            let mut j = i + "--source".len();
            // 至少一个空白
            let ws_start = j;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j > ws_start {
                // 再吃掉一段非空白
                while j < bytes.len() && !(bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 这一条是不是「番茄钟自己的」hook 条目。
///
/// ⚠ **必须是前缀匹配 `pomodoro-hook` 而不是 `pomodoro-hook.js`**：老用户的配置里
/// 是 `node "…/pomodoro-hook.js"`，新版是 `"…/pomodoro-hook.exe"`。只认 `.js` 的话
/// `--clean` 会把老条目留在原地，于是新旧两条一起跑 —— 每次工具调用弹两次窗，
/// 而且用户根本看不出来为什么。
fn is_pomodoro_entry(fp: &str) -> bool {
    fp.contains("pomodoro-hook")
}

/// `codexHooksDisabled()` —— 用户可能在 config.toml 里把 hooks 关了
/// （`[features]` 段里的 `hooks = false`）。
fn codex_hooks_disabled() -> bool {
    let Ok(text) = std::fs::read_to_string(pomodoro_core::home_dir().join(".codex").join("config.toml"))
    else {
        return false;
    };
    let mut in_features = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('[') {
            // 段头：`[features]` → 进入；其它段头 → 离开
            let name = rest.trim_end_matches(']').trim();
            in_features = name == "features";
            continue;
        }
        if in_features {
            let no_space: String = t.chars().filter(|c| !c.is_whitespace()).collect();
            if no_space == "hooks=false" || no_space == "codex_hooks=false" {
                return true;
            }
        }
    }
    false
}

/// `codexInlineHooks()` —— config.toml 里已经有内联 `[hooks]` → 会与 hooks.json
/// 双重加载并告警。
fn codex_inline_hooks() -> bool {
    let Ok(text) = std::fs::read_to_string(pomodoro_core::home_dir().join(".codex").join("config.toml"))
    else {
        return false;
    };
    text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("[hooks]") || t.starts_with("[[hooks.")
    })
}

/// `collectFlag(args, name)` —— 取 `--name v1 --name v2` 的所有值。
pub fn collect_flag(args: &[String], name: &str) -> Vec<String> {
    let flag = format!("--{name}");
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if *a == flag {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
            }
        }
    }
    out
}

/// `--name value` 取第一个值（不带值 → `None`）。
pub fn flag_value(args: &[String], name: &str) -> Option<String> {
    collect_flag(args, name).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> Installer {
        Installer {
            print: true,
            clean: false,
            with_notify: false,
            own: "C:\\app\\pomodoro-hook.exe".to_string(),
        }
    }

    #[test]
    fn fingerprint_ignores_source_and_quotes() {
        let a = json!({ "hooks": [{ "type": "command",
            "command": "\"C:\\app\\pomodoro-hook.exe\" --source claude-code" }] });
        let b = json!({ "hooks": [{ "type": "command",
            "command": "\"C:\\app\\pomodoro-hook.exe\" --source qwen" }] });
        assert_eq!(hook_fingerprint(&a), hook_fingerprint(&b));
        assert_eq!(hook_fingerprint(&a), "C:\\app\\pomodoro-hook.exe");
    }

    #[test]
    fn fingerprint_handles_legacy_node_form() {
        // 老版本：`node "<脚本>" --source x`
        let legacy = json!({ "hooks": [{ "type": "command",
            "command": "node \"C:\\app\\bin\\pomodoro-hook.js\" --source claude-code" }] });
        let fp = hook_fingerprint(&legacy);
        assert_eq!(fp, "node C:\\app\\bin\\pomodoro-hook.js");
        // 关键：老条目必须被认成「番茄钟自己的」——否则 --clean 清不掉它
        assert!(is_pomodoro_entry(&fp));
    }

    #[test]
    fn strip_source_flags_does_not_eat_neighbours() {
        assert_eq!(strip_source_flags("a --source b c"), "a  c");
        // 没有值可吃时原样保留
        assert_eq!(strip_source_flags("a --source"), "a --source");
        assert_eq!(strip_source_flags("--source x"), "");
        // 字符串里恰好出现 --source（例如某个路径）也照吃 —— 与 JS 正则一致
        assert_eq!(strip_source_flags("cmd --sourceish x"), "cmd --sourceish x");
    }

    #[test]
    fn clean_removes_stale_copies_but_keeps_others() {
        let i = Installer {
            print: false,
            clean: true,
            with_notify: false,
            own: "C:\\app\\pomodoro-hook.exe".to_string(),
        };
        let mut container = Map::new();
        container.insert(
            "PreToolUse".into(),
            json!([
                { "hooks": [{ "type": "command", "command": "node \"D:\\old\\pomodoro-hook.js\" --source claude-code" }] },
                { "hooks": [{ "type": "command", "command": "someone-elses-hook" }] },
                { "hooks": [{ "type": "command", "command": "\"C:\\app\\pomodoro-hook.exe\" --source claude-code" }] }
            ]),
        );
        let entry = json!({ "matcher": "*", "hooks": [{ "type": "command",
            "command": "\"C:\\app\\pomodoro-hook.exe\" --source claude-code" }] });
        i.push_hook(&mut container, "PreToolUse", entry);
        let list = container["PreToolUse"].as_array().unwrap();
        assert_eq!(list.len(), 2, "老副本被清掉、别人的留下、自己那条不重复");
        // 别人的 hook 必须原样留着（不能误删用户其它工具的 hook）
        assert!(list
            .iter()
            .any(|e| hook_fingerprint(e) == "someone-elses-hook"));
        // 自己的那条留着
        assert!(list
            .iter()
            .any(|e| hook_fingerprint(e).contains("C:\\app\\pomodoro-hook.exe")));
    }

    #[test]
    fn without_clean_stale_entries_survive() {
        let i = inst(); // clean = false
        let mut container = Map::new();
        container.insert(
            "Stop".into(),
            json!([{ "hooks": [{ "type": "command", "command": "node \"D:\\old\\pomodoro-hook.js\"" }] }]),
        );
        let entry = json!({ "hooks": [{ "type": "command", "command": "\"C:\\app\\pomodoro-hook.exe\"" }] });
        i.push_hook(&mut container, "Stop", entry);
        assert_eq!(container["Stop"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn same_entry_is_not_pushed_twice() {
        let i = inst();
        let mut container = Map::new();
        let entry = json!({ "matcher": "*", "hooks": [{ "type": "command",
            "command": "\"C:\\app\\pomodoro-hook.exe\" --source trae", "timeout": 4200 }] });
        i.push_hook(&mut container, "PreToolUse", entry.clone());
        // 换个 --source 再装：指纹相同 → 不该叠加
        let mut entry2 = entry.clone();
        entry2["hooks"][0]["command"] = json!("\"C:\\app\\pomodoro-hook.exe\" --source vscode");
        i.push_hook(&mut container, "PreToolUse", entry2);
        assert_eq!(container["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hook_command_quotes_path() {
        let cmd = inst().hook_command_shell("codex")["command"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(cmd, "\"C:\\app\\pomodoro-hook.exe\" --source codex");
        // 不能带 node 前缀（那会去执行 exe 当脚本）
        assert!(!cmd.starts_with("node "));
    }

    #[test]
    fn collect_flag_gathers_repeats() {
        let args: Vec<String> = ["--question", "A", "--question", "B", "--option", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(collect_flag(&args, "question"), vec!["A", "B"]);
        assert_eq!(collect_flag(&args, "option"), vec!["x"]);
        assert!(collect_flag(&args, "nope").is_empty());
        assert_eq!(flag_value(&args, "question").unwrap(), "A");
        // 末尾没有值的 --flag 不应 panic
        let args: Vec<String> = vec!["--agent".to_string()];
        assert!(flag_value(&args, "agent").is_none());
    }
}
