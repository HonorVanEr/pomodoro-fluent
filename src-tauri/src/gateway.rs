//! Agent 网关 —— 本地 HTTP 服务。对应 Electron 版 `gateway.js`（772 行）。
//!
//! 供 ZCode / Claude Code hooks / OpenCode 插件 / 任意脚本与番茄钟联动：
//!
//! ```text
//! GET  /health          存活探测（免鉴权，只暴露端口是否活着）
//! GET  /api/status      定时器状态 + 本专注期 agent 活动计数
//! POST /api/notify      主动弹一条通知 {title, message, sub, type}
//! POST /api/event       agent 事件上报 {kind, ...}（计数 + 策略弹窗）
//! POST /api/interaction 交互弹窗（**长轮询**等用户在弹窗内决策）
//! POST /api/confirm     兼容旧接口：等价于 kind=custom
//! POST /api/timer       远程控制 {command: toggle|reset|skip}
//! ```
//!
//! # 并发模型
//!
//! 用 `tiny_http` 而不是 axum/hyper：后者的异步栈要多背一个 tokio（MB 级体积），
//! 而这里的并发需求恰恰是「**同步阻塞**」—— 长轮询就是让那条连接上的线程
//! 在 `recv_timeout` 里等用户点按钮（见 [`crate::interaction`]）。
//! tiny_http 每条连接一个线程（池子满了就再开一个），正好对上。
//!
//! # 安全
//!
//! 仅绑定 `127.0.0.1`；除 `/health` 外全部要求 `Authorization: Bearer <token>`；
//! 校验 `Host` 头只允许 `127.0.0.1` / `localhost` / `[::1]`（防 DNS rebinding）。
//! token 每次启动重新生成，写进 `<userData>/gateway.json` 供 CLI 发现。
//!
//! ⚠ 别把绑定地址改成 `0.0.0.0`。也别去掉 Host 校验 —— 浏览器页面能发起跨域请求，
//! 没有这层校验就变成一个「网页可读的本地弹窗接口」。

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::payload;
use crate::state::{lock, Activity, AppState};
use crate::{interaction, popup, window};

/// 发现文件里的协议版本（与 `gateway.js` 的 `GATEWAY_VERSION` 一致）
pub const GATEWAY_VERSION: u32 = 2;

const DEFAULT_PORT: u16 = 5277;
/// 端口被占时依次 +1 重试的次数
const PORT_ATTEMPTS: u16 = 20;
/// 请求体上限（提问 / 工具输入可能较长）
const BODY_LIMIT: usize = 256 * 1024;
/// 「休息建议」弹窗冷却
const BREAK_SUGGEST_COOLDOWN_MS: i64 = 10 * 60 * 1000;
const BREAK_SUGGEST_TIMEOUT_MS: u64 = 2 * 60 * 1000;
/// accept 循环的轮询间隔：只为定期检查停机标志
const ACCEPT_TICK: Duration = Duration::from_millis(300);

/// `/api/event` 认的事件类型
const EVENT_KINDS: [&str; 12] = [
    "notification",
    "ask",
    "permission",
    "stop",
    "subagent-start",
    "subagent-stop",
    "tool-before",
    "tool-after",
    "session-start",
    "session-end",
    "pre-compact",
    "prompt",
];

// ---------------------------------------------------------------------------
// 运行时句柄（不放进 AppState：Server 既不是 Debug 也不是 Default）
// ---------------------------------------------------------------------------

static SERVER: Mutex<Option<Arc<Server>>> = Mutex::new(None);
static TOKEN: Mutex<String> = Mutex::new(String::new());
/// 监听端口；0 = 未运行。用原子量是为了让 `/api/status`、命令层能无锁快读。
static PORT: AtomicU16 = AtomicU16::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn port() -> Option<u16> {
    match PORT.load(Ordering::Relaxed) {
        0 => None,
        p => Some(p),
    }
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// 给 `/api/status` / 命令层用的活动计数快照
pub fn activity(app: &AppHandle) -> Activity {
    let st = app.state::<AppState>();
    let g = lock(&st.gateway);
    g.activity.clone()
}

// ---------------------------------------------------------------------------
// 启动 / 停止
// ---------------------------------------------------------------------------

/// 启动网关。返回监听端口。
pub fn start(app: &AppHandle) -> Result<u16, String> {
    if let Some(p) = port() {
        return Ok(p);
    }
    let token = random_hex(24);
    let base = std::env::var("POMODORO_GATEWAY_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);

    let mut bound: Option<(Server, u16)> = None;
    let mut last_err = String::new();
    for off in 0..PORT_ATTEMPTS {
        let candidate = base + off;
        match Server::http(("127.0.0.1", candidate)) {
            // tiny_http 把绑定错误包成 trait object，没法可靠地只挑 EADDRINUSE 重试；
            // 干脆每种错误都试下一个端口，最后把**最后一个**错误报出去。
            Ok(srv) => {
                bound = Some((srv, candidate));
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let Some((server, chosen)) = bound else {
        return Err(format!(
            "端口 {base}~{} 均无法监听：{last_err}",
            base + PORT_ATTEMPTS - 1
        ));
    };

    let server = Arc::new(server);
    *lock(&SERVER) = Some(server.clone());
    *lock(&TOKEN) = token;
    PORT.store(chosen, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    write_discovery();

    let handle = app.clone();
    std::thread::spawn(move || {
        while RUNNING.load(Ordering::Relaxed) {
            match server.recv_timeout(ACCEPT_TICK) {
                // 每条请求一个线程：长轮询会占住自己的线程最多 1 小时，
                // 不能让后面的 /health 排队。
                Ok(Some(req)) => {
                    let h = handle.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_request(&h, req) {
                            eprintln!("[gateway] 处理请求失败: {e}");
                        }
                    });
                }
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("[gateway] accept 出错，停止监听: {e}");
                    break;
                }
            }
        }
    });

    eprintln!("[gateway] Agent 网关已启动: http://127.0.0.1:{chosen}");
    Ok(chosen)
}

/// 停止网关：让 accept 线程退出、把挂起的交互按 `dismissed` 收尾、删掉发现文件。
///
/// 顺序有讲究：先停监听（不再来新请求），再收尾挂起的交互（让卡住的 hook 立刻拿到
/// 「未决策」而不是干等 1 小时），最后删发现文件（否则 CLI 会指着一个死端口）。
pub fn stop(app: &AppHandle) {
    if !RUNNING.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(server) = lock(&SERVER).take() {
        server.unblock();
    }
    PORT.store(0, Ordering::Relaxed);
    interaction::dismiss_all(app, false);
    remove_discovery();
    *lock(&TOKEN) = String::new();
    eprintln!("[gateway] Agent 网关已停止");
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if getrandom::fill(&mut buf).is_err() {
        // 拿不到 CSPRNG 时**宁可失败也不要生成弱 token**：一个可预测的 token
        // 等于把本地弹窗接口开放给同机所有进程。
        eprintln!("[gateway] 系统随机数不可用，网关不启动");
        return String::new();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// 发现文件
// ---------------------------------------------------------------------------

fn write_discovery() {
    let path = pomodoro_core::gateway_file();
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[gateway] 建目录失败 {}: {e}", dir.display());
            return;
        }
    }
    let token = lock(&TOKEN).clone();
    if token.is_empty() {
        return;
    }
    let body = json!({
        "port": PORT.load(Ordering::Relaxed),
        "token": token,
        "pid": std::process::id(),
        "version": GATEWAY_VERSION,
        "startedAt": iso8601_now(),
    });
    let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".into());
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!("[gateway] 写发现文件失败 {}: {e}", path.display());
    }
}

fn remove_discovery() {
    let path = pomodoro_core::gateway_file();
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("[gateway] 删发现文件失败 {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// 定时器阶段观察 + 活动计数
// ---------------------------------------------------------------------------

/// 观察定时器阶段：**切回 work 视为新专注期开始**，计数清零。
///
/// 两个调用点：`tray:update`（渲染层每秒上报）与 `/api/status`（外部查询）。
pub fn observe_timer_state(app: &AppHandle, phase: &str) {
    if phase.is_empty() {
        return;
    }
    let st = app.state::<AppState>();
    let mut g = lock(&st.gateway);
    if g.prev_phase.as_deref() == Some(phase) {
        return;
    }
    if phase == "work" {
        g.activity = Activity {
            since: Some(interaction::now_ms()),
            ..Activity::default()
        };
    }
    g.prev_phase = Some(phase.to_string());
    drop(g);
    if phase == "work" {
        publish_activity(app);
    }
}

/// 活动计数变化后：推给主窗口 + 刷新网关状态面板。
///
/// 渲染层两份都读（`onAgentActivity` 与 `onGatewayState.activity`），保持一致。
fn publish_activity(app: &AppHandle) {
    let a = activity(app);
    window::emit(app, "state:agent-activity", a);
    push_gateway_state(app);
}

/// 把网关状态推给渲染层（开关 / 端口 / hook 路径 / 活动计数）
pub fn push_gateway_state(app: &AppHandle) {
    let activity = serde_json::to_value(activity(app)).unwrap_or(Value::Null);
    window::emit(
        app,
        "state:gateway",
        json!({
            "enabled": is_running(),
            "port": port(),
            "hookPath": crate::config::hook_path().to_string_lossy(),
            "pluginPath": crate::config::opencode_plugin_path().to_string_lossy(),
            "activity": activity,
        }),
    );
}

// ---------------------------------------------------------------------------
// HTTP 处理
// ---------------------------------------------------------------------------

fn handle_request(app: &AppHandle, mut req: Request) -> Result<(), String> {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();

    // /health 免鉴权：只暴露"端口活着"，不泄露任何状态
    if method == Method::Get && path == "/health" {
        return send(req, 200, json!({ "ok": true, "app": "pomodoro-fluent", "gateway": GATEWAY_VERSION }));
    }
    if !host_ok(&req) {
        return send(req, 403, json!({ "ok": false, "error": "forbidden host" }));
    }
    if !authorized(&req) {
        return send(req, 401, json!({ "ok": false, "error": "unauthorized" }));
    }

    if method == Method::Get && path == "/api/status" {
        return send(req, 200, status_body(app));
    }

    let is_post = method == Method::Post;
    if is_post
        && matches!(
            path.as_str(),
            "/api/notify" | "/api/event" | "/api/confirm" | "/api/interaction" | "/api/timer"
        )
    {
        let raw = match read_body(&mut req) {
            Ok(r) => r,
            Err(code) => {
                let msg = if code == 413 { "body too large" } else { "read error" };
                return send(req, code, json!({ "ok": false, "error": msg }));
            }
        };
        let body: Value = if raw.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return send(req, 400, json!({ "ok": false, "error": "invalid json" })),
            }
        };

        match path.as_str() {
            "/api/notify" => {
                notify_popup(app, body);
                return send(req, 200, json!({ "ok": true }));
            }
            "/api/event" => return send(req, 200, handle_event(app, body)),
            "/api/timer" => {
                let cmd = body.get("command").and_then(Value::as_str).unwrap_or("");
                if !matches!(cmd, "toggle" | "reset" | "skip") {
                    return send(
                        req,
                        400,
                        json!({ "ok": false, "error": "command must be toggle|reset|skip" }),
                    );
                }
                window::send_command(app, cmd);
                return send(req, 200, json!({ "ok": true }));
            }
            // 长轮询：会在这里阻塞到用户决策 / 超时 / 被顶掉
            _ => {
                let payload = if path == "/api/confirm" {
                    let mut o = body.as_object().cloned().unwrap_or_else(Map::new);
                    o.insert("kind".into(), json!("custom"));
                    Value::Object(o)
                } else {
                    body
                };
                let result = request(app, payload);
                return send(req, 200, result);
            }
        }
    }

    send(req, 404, json!({ "ok": false, "error": "not found" }))
}

fn status_body(app: &AppHandle) -> Value {
    let phase = {
        let st = app.state::<AppState>();
        let t = lock(&st.timer);
        t.clone()
    };
    observe_timer_state(app, &phase.phase);

    let st = app.state::<AppState>();
    let activity = lock(&st.gateway).activity.clone();

    json!({
        "ok": true,
        "timer": {
            "phase": phase.phase,
            "running": phase.running,
            "remainMs": phase.remain_ms,
            "totalMs": phase.total_ms,
            "completedFocus": phase.completed_focus,
            "roundInCycle": phase.round_in_cycle,
            "rounds": phase.rounds,
        },
        "activity": activity,
        "pending": interaction::count(app),
        "gateway": { "version": GATEWAY_VERSION, "port": PORT.load(Ordering::Relaxed) },
    })
}

/// 读请求体（带上限）。`as_reader()` 内部会先把 `Expect: 100-continue` 处理掉 ——
/// curl 传超过 1KB 的 `-d` 会先发这个头并等 100，不处理的话每次都被拖 1 秒。
fn read_body(req: &mut Request) -> Result<String, u16> {
    let mut buf = Vec::new();
    let limit = (BODY_LIMIT as u64) + 1;
    req.as_reader()
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(|_| 400u16)?;
    if buf.len() > BODY_LIMIT {
        return Err(413);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
        .expect("静态 header 一定合法")
}

fn no_store_header() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).expect("静态 header 一定合法")
}

fn send(req: Request, code: u16, body: Value) -> Result<(), String> {
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    let resp = Response::from_string(text)
        .with_status_code(code)
        .with_header(json_header())
        .with_header(no_store_header());
    req.respond(resp).map_err(|e| e.to_string())
}

/// Host 头只允许回环地址（防 DNS rebinding：恶意域名解析到 127.0.0.1 后，
/// 浏览器会带着那个域名当 Host 来打这个端口）。
///
/// ⚠ 这里必须检查**全部** Host 头，不能只看第一个：
/// RFC 7230 §5.4 规定一个请求里 Host 只能有一个，多于一个本身就是可疑请求。
/// 只看第一个的话，一个"先塞合法 Host 再塞非法 Host"的请求就能绕过校验。
/// （这不是纯理论 —— 本文件的自检就踩过：minreq 会在请求行后**强制写自己的
/// `Host`**，我们额外设的 `Host` 变成第二条，于是校验被绕过、自检永远拿到 200。）
fn host_ok(req: &Request) -> bool {
    let hosts: Vec<String> = req
        .headers()
        .iter()
        .filter(|h| h.field.as_str().as_str().eq_ignore_ascii_case("host"))
        .map(|h| h.value.as_str().to_string())
        .collect();
    !hosts.is_empty() && hosts.iter().all(|h| host_is_loopback(h))
}

/// 单个 Host 头是不是回环地址（允许 `host` 或 `host:port`；IPv6 是 `[::1]:port`）。
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, tail)) => {
                if !tail.is_empty() && !tail.starts_with(':') {
                    return false;
                }
                addr.to_string()
            }
            None => return false,
        }
    } else {
        match host.split_once(':') {
            Some((n, port)) => {
                if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                n.to_string()
            }
            None => host.clone(),
        }
    };
    matches!(name.as_str(), "127.0.0.1" | "localhost" | "::1")
}

fn header_value(req: &Request, name: &str) -> Option<String> {
    req.headers().iter().find_map(|h| {
        if h.field.as_str().as_str().eq_ignore_ascii_case(name) {
            Some(h.value.as_str().to_string())
        } else {
            None
        }
    })
}

fn authorized(req: &Request) -> bool {
    let Some(m) = header_value(req, "authorization") else {
        return false;
    };
    let Some(token) = m.strip_prefix("Bearer ").or_else(|| m.strip_prefix("bearer ")) else {
        return false;
    };
    let want = lock(&TOKEN).clone();
    if want.is_empty() {
        return false;
    }
    constant_time_eq(token.trim().as_bytes(), want.as_bytes())
}

/// 定长时间比较：不做短路，避免按前缀逐字节泄漏 token。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// 交互 / 通知 / 事件
// ---------------------------------------------------------------------------

/// 发起一次交互请求（**会阻塞**直到有结果）。
///
/// 三种收尾：用户在弹窗内作答（`user`）、被顶掉/交给终端（`dismissed`，`action=null`）、
/// 到兜底上限（`timeout`，落该 kind 的安全默认值）。
/// `notification`（无按钮）与「没有任何可作答控件的 ask」不等待，弹完立即返回 `shown`。
pub fn request(app: &AppHandle, raw: Value) -> Value {
    let mut norm = payload::normalize_interaction(&raw);
    let kind = norm
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("custom")
        .to_string();
    let has_actions = norm
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    if kind == "notification" && !has_actions {
        notify_popup(app, norm);
        return interaction::Decision::shown().to_json(&kind);
    }
    // ask 没有任何可作答控件时降级成通知 —— 否则弹出一个无法提交的窗口
    if kind == "ask"
        && norm
            .get("questions")
            .and_then(Value::as_array)
            .map(|q| q.is_empty())
            .unwrap_or(true)
    {
        if let Some(o) = norm.as_object_mut() {
            o.insert("kind".into(), json!("notification"));
            o.insert("flavor".into(), json!("ask"));
        }
        notify_popup(app, norm);
        return interaction::Decision::shown().to_json("notification");
    }

    let timeout_ms = payload::timeout_of(&norm);
    let id = interaction::new_id();
    norm.as_object_mut()
        .expect("归一化结果一定是对象")
        .insert("id".into(), json!(id));

    let started = Instant::now();
    let rx = interaction::register(app, &id, norm.clone(), timeout_ms);
    if let Err(e) = popup::show(app, norm) {
        eprintln!("[gateway] 弹窗创建失败: {e}（按 dismissed 收尾，交回终端）");
        interaction::resolve(app, &id, interaction::Decision::dismissed());
        return interaction::Decision::dismissed().to_json(&kind);
    }
    // 弹窗创建耗时也算进兜底总时长里（JS 的 setTimeout 是从请求时刻起算的）
    let elapsed = started.elapsed().as_millis() as u64;
    let remaining = timeout_ms.saturating_sub(elapsed).max(1);

    let decision = interaction::wait(app, &id, rx, remaining);
    decision.to_json(&kind)
}

/// 弹一条**不等结果**的通知（`POST /api/notify` 与 `handle_event` 的弹窗都走这里）。
fn notify_popup(app: &AppHandle, payload: Value) {
    // 与 JS 的 `{ kind: 'notification', ...(payload||{}) }` 同一个语义：
    // 调用方自带 kind 时**以调用方为准**（`/api/notify` 传 kind=ask 就是交互窗的形状）。
    let mut o = payload.as_object().cloned().unwrap_or_else(Map::new);
    o.entry("kind".to_string()).or_insert(json!("notification"));
    let norm = payload::normalize_interaction(&Value::Object(o));
    // 通知会顶掉正在等决策的交互窗，但不动「暂时收起」的
    interaction::dismiss_all(app, true);
    if let Err(e) = popup::show(app, norm) {
        eprintln!("[gateway] 通知弹窗创建失败: {e}");
    }
}

/// `POST /api/event`：只计数 + 按策略弹窗，不等用户。
pub fn handle_event(app: &AppHandle, ev: Value) -> Value {
    let kind = ev.get("kind").and_then(Value::as_str).unwrap_or("");
    if !EVENT_KINDS.contains(&kind) {
        return json!({ "ok": false, "error": format!("unknown kind: {kind}") });
    }

    let mut triggered = Value::Null;
    match kind {
        "tool-after" => {
            let st = app.state::<AppState>();
            lock(&st.gateway).activity.tool_calls += 1;
        }
        "notification" | "ask" | "permission" => {
            {
                let st = app.state::<AppState>();
                lock(&st.gateway).activity.interruptions += 1;
            }
            // 事件流的权限 / 提问只是「有个东西在等你」的提示，**不带可作答控件** ——
            // 真正的作答走 `/api/interaction`。别把它升级成交互窗。
            let title = ev
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if kind == "permission" {
                        "Agent 需要权限确认".to_string()
                    } else {
                        "Agent 需要你的确认".to_string()
                    }
                });
            let source = ev.get("source").and_then(Value::as_str).unwrap_or("");
            let mut o = Map::new();
            o.insert("kind".into(), json!(kind));
            o.insert(
                "title".into(),
                json!(payload::truncate_str(&title, 120)),
            );
            o.insert(
                "message".into(),
                json!(payload::truncate_str(
                    ev.get("message").and_then(Value::as_str).unwrap_or(""),
                    360
                )),
            );
            o.insert("sub".into(), json!(payload::source_label(source)));
            o.insert("source".into(), json!(source));
            if let Some(ctx) = ev.get("context") {
                o.insert("context".into(), ctx.clone());
            }
            notify_popup(app, Value::Object(o));
            triggered = json!("notify");
        }
        "stop" => {
            {
                let st = app.state::<AppState>();
                lock(&st.gateway).activity.stops += 1;
            }
            // 主 agent 回合结束 = 任务完成 / 空闲 → 可能建议休息
            if maybe_suggest_break(app, &ev) {
                triggered = json!("break-suggest");
            }
        }
        "session-start" | "session-end" => {
            let st = app.state::<AppState>();
            lock(&st.gateway).activity.sessions += 1;
        }
        // tool-before / subagent-* / pre-compact / prompt：只用于弹窗上下文与调试
        _ => {}
    }

    let activity = serde_json::to_value(activity(app)).unwrap_or(Value::Null);
    publish_activity(app);
    json!({ "ok": true, "triggered": triggered, "activity": activity })
}

/// Agent 空闲且仍在专注时段 → 弹「休息建议」，一键跳到休息。
///
/// 会**新起一个线程**等用户决定：事件上报本身不能阻塞（`stop` 事件是回合结束，
/// 宿主那边不期望它挂住）。这与 JS 里 `requestConfirm(...).then(...)` 等价。
fn maybe_suggest_break(app: &AppHandle, ev: &Value) -> bool {
    // 有未决交互时不打扰：别把正在等决策的权限 / 提问窗顶掉
    if interaction::count(app) > 0 {
        return false;
    }
    let (phase, running) = {
        let st = app.state::<AppState>();
        let t = lock(&st.timer);
        (t.phase.clone(), t.running)
    };
    if phase != "work" || !running {
        return false;
    }
    {
        let st = app.state::<AppState>();
        let mut g = lock(&st.gateway);
        let now = interaction::now_ms();
        if now - g.last_break_suggest_at < BREAK_SUGGEST_COOLDOWN_MS {
            return false;
        }
        g.last_break_suggest_at = now;
    }

    let task = ev
        .get("context")
        .and_then(|c| c.get("task"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = if task.is_empty() {
        "这轮任务跑完了，要趁机休息一下吗？".to_string()
    } else {
        format!("「{task}」这轮跑完了，要趁机休息一下吗？")
    };
    let mut o = Map::new();
    o.insert("kind".into(), json!("custom"));
    o.insert("title".into(), json!("Agent 空闲了"));
    o.insert("message".into(), json!(message));
    o.insert(
        "sub".into(),
        json!("选择「休息一下」将提前结束本段专注进入休息"),
    );
    o.insert(
        "actions".into(),
        json!([
            { "id": "break", "label": "休息一下", "style": "primary" },
            { "id": "keep", "label": "继续专注" },
            { "id": "ignore", "label": "忽略" },
        ]),
    );
    o.insert("defaultAction".into(), json!("ignore"));
    o.insert("timeoutMs".into(), json!(BREAK_SUGGEST_TIMEOUT_MS));
    if let Some(src) = ev.get("source") {
        o.insert("source".into(), src.clone());
    }
    if let Some(ctx) = ev.get("context") {
        o.insert("context".into(), ctx.clone());
    }

    let handle = app.clone();
    std::thread::spawn(move || {
        let result = request(&handle, Value::Object(o));
        if result.get("action").and_then(Value::as_str) == Some("break") {
            window::send_command(&handle, "skip");
        }
    });
    true
}

// ---------------------------------------------------------------------------
// 自检（POMODORO_GATEWAY_SMOKE=1）
// ---------------------------------------------------------------------------

/// 网关自检：拿真实 HTTP 打一遍自己的全部端点，把 `PASS/FAIL` 打到 stderr。
///
/// 走真实 socket（而不是直接调函数）是刻意的：要验的正是「token 校验、Host 校验、
/// JSON 解析、长轮询超时」这些**只有过一遍 HTTP 才会暴露**的东西。
///
/// `POMODORO_GATEWAY_POPUP=1` 时再加一轮「弹窗内真实作答」：等挂起的交互出现后，
/// 用 `allow-always` 落定它，并断言 HTTP 侧拿到的是 `decidedBy=user`
/// —— 这条是唯一能覆盖「弹窗 → bridge → 命令 → 网关 resolve → HTTP 响应」全链路的用例。
pub fn smoke(app: &AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        let mut results: Vec<String> = Vec::new();

        record(&mut results, "health", smoke_health());
        record(&mut results, "status", smoke_status());
        record(&mut results, "host-reject(403)", smoke_host_reject());
        record(&mut results, "auth-reject(401)", smoke_auth_reject());

        record(
            &mut results,
            "event(notification)",
            self_request(
                "POST",
                "/api/event",
                Some(json!({ "kind": "notification", "message": "[smoke] 权限确认测试", "source": "manual" })),
            )
            .and_then(|(code, body)| {
                if code != 200 || body.get("ok").and_then(Value::as_bool) != Some(true) {
                    Err(format!("status={code} body={body}"))
                } else {
                    Ok(())
                }
            }),
        );

        record(
            &mut results,
            "event(tool-after)",
            self_request(
                "POST",
                "/api/event",
                Some(json!({ "kind": "tool-after", "tool": "smoke" })),
            )
            .and_then(|(_, body)| {
                let n = body
                    .get("activity")
                    .and_then(|a| a.get("toolCalls"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if n >= 1 {
                    Ok(())
                } else {
                    Err(format!("toolCalls={n}"))
                }
            }),
        );

        // 三层超时里最内层：番茄钟兜底。这里用 5s 验证它真的会到点，
        // 且 permission 落 deny、ask 落 cancel（安全默认值不能反）。
        record(
            &mut results,
            "interaction(permission, timeout→deny)",
            smoke_timeout(
                json!({
                    "kind": "permission", "source": "manual", "title": "[smoke] 权限测试",
                    "message": "5 秒后自动超时",
                    "permission": { "tool": "Bash", "rule": "npm test", "canAlways": true },
                    "timeoutMs": 5000
                }),
                "timeout",
                "deny",
                None,
            ),
        );

        record(
            &mut results,
            "interaction(ask, timeout→cancel)",
            smoke_timeout(
                json!({
                    "kind": "ask", "source": "manual", "title": "[smoke] 提问测试",
                    "questions": [{
                        "question": "继续吗？", "header": "确认",
                        "options": [{ "label": "继续" }, { "label": "停下" }]
                    }],
                    "timeoutMs": 5000
                }),
                "timeout",
                "cancel",
                None,
            ),
        );

        record(
            &mut results,
            "confirm(custom, timeout)",
            smoke_timeout(
                json!({
                    "title": "[smoke] 确认测试", "message": "5 秒后自动超时", "timeoutMs": 5000,
                    "defaultAction": "deny",
                    "actions": [{ "id": "allow", "label": "允许" }, { "id": "deny", "label": "拒绝" }]
                }),
                "timeout",
                "deny",
                Some("/api/confirm"),
            ),
        );

        if std::env::var_os("POMODORO_GATEWAY_POPUP").is_some() {
            record(
                &mut results,
                "interaction(permission, 弹窗内作答→user)",
                smoke_popup_answer(&handle),
            );
        }

        record(
            &mut results,
            "interaction(permission, 收起→唤回→作答)",
            smoke_hold_reopen(&handle),
        );

        // M4 补：交互语义（不依赖 POMODORO_GATEWAY_POPUP）
        record(
            &mut results,
            "interaction(ask, 用户提交答案→user)",
            smoke_ask_submit(&handle),
        );
        record(
            &mut results,
            "interaction(permission, 用户拒绝→deny+备注)",
            smoke_permission_deny(&handle),
        );
        record(
            &mut results,
            "interaction(ask, 用户取消→cancel)",
            smoke_ask_cancel(&handle),
        );
        record(
            &mut results,
            "interaction(notification, 立即 shown 不阻塞)",
            smoke_notification_shown(),
        );
        record(
            &mut results,
            "interaction(ask 空题目, 降级→shown)",
            smoke_ask_degrade(),
        );
        record(
            &mut results,
            "event(permission/ask, 降级为通知+打断计数)",
            smoke_event_degrade(),
        );
        record(
            &mut results,
            "hold 幂等（第二次→None）",
            smoke_hold_idempotent(&handle),
        );
        record(
            &mut results,
            "新弹窗不顶掉已收起的（放过 held）",
            smoke_new_popup_spares_held(&handle),
        );
        record(
            &mut results,
            "reopen 守卫（不存在/已 active→不复活）+ 原样 payload",
            smoke_reopen_guards(&handle),
        );
        record(&mut results, "收尾后 pending 无泄漏", smoke_no_leak(&handle));

        let failed = results.iter().filter(|r| r.starts_with("FAIL")).count();
        eprintln!(
            "[gateway-smoke] {} 项，{} 失败\n  {}",
            results.len(),
            failed,
            results.join("\n  ")
        );
        eprintln!("[gateway-smoke] done");
    });
}

fn record(results: &mut Vec<String>, name: &str, r: Result<(), String>) {
    match r {
        Ok(()) => results.push(format!("PASS {name}")),
        Err(e) => results.push(format!("FAIL {name}: {e}")),
    }
}

fn smoke_health() -> Result<(), String> {
    let (code, body) = self_request("GET", "/health", None)?;
    if code == 200 && body.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("status={code} body={body}"))
    }
}

fn smoke_status() -> Result<(), String> {
    let (code, body) = self_request("GET", "/api/status", None)?;
    if code == 200 && body.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("status={code} body={body}"))
    }
}

/// Host 头不是回环地址时必须 403 —— 这条防的是 DNS rebinding。
///
/// ⚠ **不能**用 minreq 发这条请求：minreq 在请求行后强制写自己的 `Host`
/// （见其 `request.rs` 的 `"{} {} HTTP/1.1\r\nHost: {}"`），我们额外设的 Host
/// 只会变成第二条，而校验读的是第一条 —— 于是这条用例会"永远通过"，
/// 等于把一条安全回归白白放过去（第一版就是这样，实际拿到 200）。
/// 所以走裸 socket，Host 头完全由我们说了算。
fn smoke_host_reject() -> Result<(), String> {
    // ① 非回环 Host（浏览器 DNS rebinding 的真实形状）→ 403
    let code = raw_status("evil.example.com", "/api/status")?;
    if code != 403 {
        return Err(format!("非回环 Host 期望 403，实际 {code}"));
    }
    // ② 回环 Host 但不带 token → 401。用来证明 ① 拦的是 Host，不是别的什么。
    let code = raw_status("127.0.0.1", "/api/status")?;
    if code != 401 {
        return Err(format!("回环 Host 无 token 期望 401，实际 {code}"));
    }
    Ok(())
}

/// 「暂时收起」状态机：收起**不结束**交互，唤回能重新弹出来，最后用户作答仍算 `user`。
///
/// 这是 M2 验收项「三个唤回入口全通」里**能自动化验证**的那部分：
/// 三个入口（主窗提示条 `#heldChip` / 托盘左键单击 / 托盘右键子菜单）最终都汇到
/// [`interaction::reopen_held`]，这里把它的前置状态（`hold` 之后的样子）、
/// 主窗提示条读的那份数据（[`interaction::held_chip_items`]）都钉住，
/// 并证明「收起期间兜底定时器没被停、也没被提前 resolve」——最后拿到的必须是 `user`。
///
/// 托盘那两个 switch 分支是纯 UI 事件接线（`on_tray_event` / `on_menu_event`），
/// 没有可注入的入口，只能在真机上手点一次确认。
fn smoke_hold_reopen(app: &AppHandle) -> Result<(), String> {
    // 60s 兜底：够跑完这条用例，失败时也不会把整个自检拖太久
    let http = std::thread::spawn(move || {
        self_request(
            "POST",
            "/api/interaction",
            Some(json!({
                "kind": "permission", "source": "manual", "title": "[smoke] 收起并唤回",
                "permission": { "tool": "Bash", "rule": "npm test" },
                "timeoutMs": 60000
            })),
        )
    });

    // 等它真的挂起（顺带说明弹窗也建出来了）
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut found = None;
    while Instant::now() < deadline {
        if let Some(x) = interaction::first_pending_id(app) {
            found = Some(x);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let id = found.ok_or("权限请求没有挂起（弹窗没建出来？）")?;

    if !interaction::list_held(app).is_empty() {
        return Err("还没收起，held 列表就不该有东西".into());
    }

    // ① 收起：state → held，但**不 resolve、不停兜底定时器**
    interaction::hold(app, &id).ok_or_else(|| "hold 没找到这条交互".to_string())?;
    let held = interaction::list_held(app);
    if held.len() != 1 {
        return Err(format!("收起后 held 应为 1，实际 {}", held.len()));
    }
    if interaction::count(app) != 1 {
        return Err("收起把交互从表里删掉了 —— 那等于替用户做决定".into());
    }
    // ② 主窗口提示条读的就是这份数据（入口 ① 的数据通路）
    let chip = interaction::held_chip_items(app);
    let label = chip[0]["label"].as_str().unwrap_or("");
    if label != "权限 · Bash" {
        return Err(format!("提示条标签应为 `权限 · Bash`，实际 {label:?}"));
    }

    // ③ 唤回 —— 三个入口共用的那条路
    if !interaction::reopen_held(app, Some(&id)) {
        return Err("reopen_held 返回 false，唤回失败".into());
    }
    if !interaction::list_held(app).is_empty() {
        return Err("唤回后 held 应为空".into());
    }
    if interaction::count(app) != 1 {
        return Err("唤回过程中交互被提前收尾了".into());
    }

    // ④ 用户在（重弹出来的）弹窗里作答 → HTTP 侧必须拿到 decidedBy=user。
    //    任何一环把这次交互提前按 dismissed/timeout 收掉，这里都会露出来。
    interaction::resolve(
        app,
        &id,
        interaction::Decision::user("allow-always", json!({}), "smoke 备注".into()),
    );

    let (code, res) = match http.join() {
        Ok(r) => r?,
        Err(_) => return Err("HTTP 线程 panic".into()),
    };
    let by = res.get("decidedBy").and_then(Value::as_str).unwrap_or("");
    let action = res.get("action").and_then(Value::as_str).unwrap_or("");
    if code != 200 || by != "user" || action != "allow-always" {
        return Err(format!(
            "status={code} decidedBy={by} action={action} —— 收起/唤回期间被提前收尾了"
        ));
    }
    Ok(())
}

/// 裸 socket 发一个最小 HTTP/1.1 请求，只取状态码。
fn raw_status(host_header: &str, path: &str) -> Result<u16, String> {
    use std::io::{Read, Write};
    let port = port().ok_or("网关未运行")?;
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("连接失败：{e}"))?;
    let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes())
        .map_err(|e| format!("写请求失败：{e}"))?;
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or("");
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("响应无法解析：{line:?}"))
}

/// 不带 token 必须 401
fn smoke_auth_reject() -> Result<(), String> {
    let port = port().ok_or("网关未运行")?;
    let url = format!("http://127.0.0.1:{port}/api/status");
    let resp = minreq::get(url)
        .with_timeout(12)
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status_code == 401 {
        Ok(())
    } else {
        Err(format!("期望 401，实际 {}", resp.status_code))
    }
}

/// `path = None` 时打 `/api/interaction`。
fn smoke_timeout(
    body: Value,
    want_by: &str,
    want_action: &str,
    path: Option<&str>,
) -> Result<(), String> {
    let p = path.unwrap_or("/api/interaction");
    let (code, res) = self_request("POST", p, Some(body))?;
    let by = res.get("decidedBy").and_then(Value::as_str).unwrap_or("");
    let action = res.get("action").and_then(Value::as_str).unwrap_or("");
    if code != 200 || by != want_by || action != want_action {
        return Err(format!("status={code} decidedBy={by} action={action} body={res}"));
    }
    Ok(())
}

/// 弹窗内真实作答：发一个**不超时**的权限请求，等挂起后模拟用户点「始终允许」。
///
/// 这是唯一能覆盖「弹窗 → bridge → 命令 → 网关 resolve → HTTP 响应」全链路的用例。
/// 断言的是 `decidedBy=user` —— 只要有一环断了（比如 bridge 漏了方法、命令没注册），
/// 请求就会一路挂到超时，拿到的会是 `timeout`。
fn smoke_popup_answer(app: &AppHandle) -> Result<(), String> {
    let before = popup::payload_fetch_count();
    let app2 = app.clone();
    let answer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        // 先等弹窗页把 payload 取回去 —— 这一步涨了，才说明
        // 「窗口 → bridge 注入 → await invoke → 回包」整条链路是通的。
        while popup::payload_fetch_count() == before {
            if Instant::now() > deadline {
                return Err(
                    "弹窗页始终没取回 payload（窗口没建起来 / 页面没加载 / bridge 没注入）".to_string(),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("[gateway-smoke] 弹窗页已取回 payload，整条 invoke 链路通");
        // 再等它把按钮渲染出来（量高度 → 上报 → show 之后才有可点的按钮）
        std::thread::sleep(Duration::from_millis(700));
        let Some(id) = interaction::first_pending_id(&app2) else {
            return Err("弹窗出来了但挂起条目不见了".to_string());
        };
        let ok = interaction::resolve(
            &app2,
            &id,
            interaction::Decision::user("allow-always", json!({}), "smoke 备注".into()),
        );
        if ok {
            eprintln!("[gateway-smoke] 已在弹窗侧作答: allow-always id={id}");
            Ok(())
        } else {
            Err(format!("resolve 失败（id={id} 已被收走？）"))
        }
    });

    let (code, res) = self_request(
        "POST",
        "/api/interaction",
        Some(json!({
            "kind": "permission", "source": "manual", "title": "[smoke] 弹窗作答",
            "message": "自检会替你点「始终允许」",
            "permission": { "tool": "Bash", "rule": "smoke-popup", "canAlways": true },
            "timeoutMs": 30000
        })),
    )?;
    let by = res.get("decidedBy").and_then(Value::as_str).unwrap_or("");
    let action = res.get("action").and_then(Value::as_str).unwrap_or("");
    if code != 200 || by != "user" || action != "allow-always" {
        return Err(format!("status={code} decidedBy={by} action={action} body={res}"));
    }
    match answer.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("作答线程 panic".into()),
    }
}

// ---------------------------------------------------------------------------
// M4 补：交互语义（对应 Node 交互冒烟的 [1] / [1.5] 组）
//
// 这一组**不依赖 `POMODORO_GATEWAY_POPUP`**：要验的是「表里的状态怎么迁移」与
// 「HTTP 侧拿到什么形状」，而不是弹窗页渲染 —— 后者由 `smoke_popup_answer` 覆盖。
// ---------------------------------------------------------------------------

/// 在 `request()` 阻塞期间模拟用户点击：等交互挂起后按 `make` 落定它。
fn spawn_answer<F>(app: &AppHandle, wait_ms: u64, make: F) -> std::thread::JoinHandle<Result<(), String>>
where
    F: FnOnce(&str) -> interaction::Decision + Send + 'static,
{
    let app = app.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() <= deadline {
            if interaction::first_pending_id(&app).is_some() {
                // 等窗口真的建出来再答：`request` 是「先 register、后 popup::show」，
                // 抢在 show 之前 resolve 会让窗口建出来没人收（用例不受影响，但不真实）。
                std::thread::sleep(Duration::from_millis(500));
                let Some(id) = interaction::first_pending_id(&app) else {
                    return Err("交互在作答前被收走了".into());
                };
                return if interaction::resolve(&app, &id, make(&id)) {
                    Ok(())
                } else {
                    Err(format!("resolve 失败（id={id} 已被收走？）"))
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("交互始终没挂起（弹窗没建出来？）".into())
    })
}

/// 打一次 `/api/interaction`，同时让另一根线程按 `make` 作答，返回响应体。
fn interaction_roundtrip<F>(app: &AppHandle, body: Value, make: F) -> Result<Value, String>
where
    F: FnOnce(&str) -> interaction::Decision + Send + 'static,
{
    let answer = spawn_answer(app, 15_000, make);
    let (code, res) = self_request("POST", "/api/interaction", Some(body))?;
    answer.join().map_err(|_| "作答线程 panic".to_string())??;
    if code != 200 {
        return Err(format!("status={code} body={res}"));
    }
    Ok(res)
}

/// 直接往交互表里放一条（不走 HTTP、不建窗），用于验纯状态迁移。
fn direct_register(app: &AppHandle, id: &str, title: &str) {
    let mut p = payload::normalize_interaction(&json!({
        "kind": "permission", "source": "manual", "title": title,
        "permission": { "tool": "Bash", "rule": "npm test" }
    }));
    p.as_object_mut()
        .expect("归一化结果一定是对象")
        .insert("id".into(), json!(id));
    // 故意丢弃 Receiver：本组只验状态迁移，不等待决策
    drop(interaction::register(app, id, p, 60_000));
}

fn decision_of(res: &Value) -> (&str, &str) {
    (
        res.get("decidedBy").and_then(Value::as_str).unwrap_or(""),
        res.get("action").and_then(Value::as_str).unwrap_or(""),
    )
}

/// 提问的**答案通道**：用户选了选项 → HTTP 侧必须原样拿到 `answers`。
///
/// 所有宿主的 `AskUserQuestion` / `askQuestions` 都依赖这个形状，
/// 而此前只有**权限**通道被自检覆盖过（`smoke_popup_answer`）。
fn smoke_ask_submit(app: &AppHandle) -> Result<(), String> {
    let res = interaction_roundtrip(
        app,
        json!({
            "kind": "ask", "source": "manual", "title": "[smoke] 提问作答",
            "questions": [{
                "question": "选哪个？", "header": "方案",
                "options": [{ "label": "A" }, { "label": "B" }]
            }]
        }),
        |_| interaction::Decision::user("submit", json!({ "q": ["A"] }), String::new()),
    )?;
    let (by, action) = decision_of(&res);
    if by != "user" || action != "submit" {
        return Err(format!("decidedBy={by} action={action} body={res}"));
    }
    // answers 必须原样回传（键由题目 id 决定，这里只验值与提交的一致）
    let first = res
        .get("answers")
        .and_then(Value::as_object)
        .and_then(|o| o.values().next())
        .cloned()
        .unwrap_or(Value::Null);
    if first != json!(["A"]) {
        return Err(format!("answers 没原样回传：{first}"));
    }
    Ok(())
}

/// 用户**主动拒绝** —— 与「超时落 deny」是两条不同路径，两条都要有。顺带钉住备注回传。
fn smoke_permission_deny(app: &AppHandle) -> Result<(), String> {
    let res = interaction_roundtrip(
        app,
        json!({
            "kind": "permission", "source": "manual", "title": "[smoke] 用户拒绝",
            "permission": { "tool": "Bash", "rule": "rm -rf /" }
        }),
        |_| interaction::Decision::user("deny", json!({}), "不要跑这个".into()),
    )?;
    let (by, action) = decision_of(&res);
    let text = res.get("text").and_then(Value::as_str).unwrap_or("");
    if by != "user" || action != "deny" || text != "不要跑这个" {
        return Err(format!(
            "decidedBy={by} action={action} text={text:?} body={res}"
        ));
    }
    Ok(())
}

/// 用户**主动取消**提问 → `cancel`（不是 `dismissed` —— 那等于交给终端再问一遍）。
fn smoke_ask_cancel(app: &AppHandle) -> Result<(), String> {
    let res = interaction_roundtrip(
        app,
        json!({
            "kind": "ask", "source": "manual", "title": "[smoke] 提问取消",
            "questions": [{ "question": "继续吗？", "options": [{ "label": "继续" }] }]
        }),
        |_| interaction::Decision::user("cancel", json!({}), String::new()),
    )?;
    let (by, action) = decision_of(&res);
    if by != "user" || action != "cancel" {
        return Err(format!("decidedBy={by} action={action} body={res}"));
    }
    Ok(())
}

/// 纯通知（无按钮）**不等待**，弹完立即 `shown` —— 否则会把 hook 挂住。
fn smoke_notification_shown() -> Result<(), String> {
    let t0 = Instant::now();
    let (code, res) = self_request(
        "POST",
        "/api/interaction",
        Some(json!({
            "kind": "notification", "source": "manual",
            "title": "[smoke] 纯通知", "message": "不该阻塞"
        })),
    )?;
    let (by, action) = decision_of(&res);
    if code != 200 || by != "shown" || action != "shown" {
        return Err(format!(
            "status={code} decidedBy={by} action={action} body={res}"
        ));
    }
    let ms = t0.elapsed().as_millis();
    if ms > 3000 {
        return Err(format!("纯通知不该阻塞，实际花了 {ms}ms"));
    }
    Ok(())
}

/// 没有任何可作答控件的 ask 降级成通知 —— 否则会弹出一个无法提交的窗口。
fn smoke_ask_degrade() -> Result<(), String> {
    let (code, res) = self_request(
        "POST",
        "/api/interaction",
        Some(json!({
            "kind": "ask", "source": "manual",
            "title": "[smoke] 空提问", "questions": []
        })),
    )?;
    let (by, action) = decision_of(&res);
    if code != 200 || by != "shown" || action != "shown" {
        return Err(format!(
            "status={code} decidedBy={by} action={action} body={res}"
        ));
    }
    Ok(())
}

/// `/api/event` 上的 permission / ask **只当提示**（不带可作答控件），
/// 且必须计入「打断」—— 「有个东西在等你」正是番茄钟要提醒的事。
fn smoke_event_degrade() -> Result<(), String> {
    let before = interruptions()?;
    for kind in ["permission", "ask"] {
        let (code, body) = self_request(
            "POST",
            "/api/event",
            Some(json!({
                "kind": kind, "source": "manual",
                "message": format!("[smoke] 事件降级 {kind}")
            })),
        )?;
        if code != 200 || body.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!("{kind}: status={code} body={body}"));
        }
    }
    let after = interruptions()?;
    if after != before + 2 {
        return Err(format!(
            "打断计数应从 {before} 涨到 {}，实际 {after}",
            before + 2
        ));
    }
    Ok(())
}

fn interruptions() -> Result<u64, String> {
    let (_, body) = self_request("GET", "/api/status", None)?;
    body.get("activity")
        .and_then(|a| a.get("interruptions"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("status 里读不到 activity.interruptions：{body}"))
}

/// 「暂时收起」幂等：第二次必须是 `None`（否则托盘会多出一条点不动的死条目）。
/// 顺带钉住摘要里带上了标题与工具（托盘菜单 / 提示条都读它）。
fn smoke_hold_idempotent(app: &AppHandle) -> Result<(), String> {
    let id = "smoke-hold-idem";
    direct_register(app, id, "[smoke] 收起幂等");

    let first = interaction::hold(app, id).ok_or("第一次 hold 应返回摘要")?;
    if first.get("title").and_then(Value::as_str) != Some("[smoke] 收起幂等") {
        return Err(format!("摘要里的标题不对：{first}"));
    }
    if first.get("tool").and_then(Value::as_str) != Some("Bash") {
        return Err(format!("摘要里的工具名不对：{first}"));
    }
    if interaction::hold(app, id).is_some() {
        return Err("第二次 hold 应返回 None（幂等）".into());
    }
    interaction::resolve(app, id, interaction::Decision::dismissed());
    Ok(())
}

/// **新弹窗不顶掉已收起的** —— 用户点名要稍后处理，不该被一条新通知替他丢掉。
///
/// `register()` 会把所有 `active` 的按 `dismissed` 收尾，但必须放过 `held`。
fn smoke_new_popup_spares_held(app: &AppHandle) -> Result<(), String> {
    let (held_id, new_id) = ("smoke-spare-held", "smoke-spare-new");
    direct_register(app, held_id, "[smoke] 先收起");
    interaction::hold(app, held_id).ok_or("hold 没找到这条交互")?;

    // 新来的这条会顶掉 active 的，但放过 held 的
    direct_register(app, new_id, "[smoke] 后到的新弹窗");

    let held = interaction::list_held(app);
    if held.len() != 1 {
        return Err(format!("held 应仍为 1 条，实际 {}", held.len()));
    }
    if held[0].get("id").and_then(Value::as_str) != Some(held_id) {
        return Err(format!(
            "收起的那条不该被顶掉，实际留下的是：{}",
            held[0]
        ));
    }
    if interaction::count(app) != 2 {
        return Err(format!(
            "两条都该在表里，实际 {}",
            interaction::count(app)
        ));
    }

    interaction::resolve(app, new_id, interaction::Decision::dismissed());
    interaction::resolve(app, held_id, interaction::Decision::dismissed());
    Ok(())
}

/// `reopen` 的守卫 + 原样重弹：不存在的 id、还 `active` 的都不能被"复活"。
fn smoke_reopen_guards(app: &AppHandle) -> Result<(), String> {
    let id = "smoke-reopen-guard";
    direct_register(app, id, "[smoke] 唤回守卫");

    // ① 还在弹（active）时 reopen → None（不重复弹）
    if interaction::reopen(app, id).is_some() {
        return Err("active 的交互不该被 reopen".into());
    }
    // ② 不存在的 id → None
    if interaction::reopen(app, "smoke-nonexistent").is_some() {
        return Err("不存在的 id 不该被 reopen".into());
    }
    // ③ 收起后 reopen → 拿回**原样**的 payload
    interaction::hold(app, id).ok_or("hold 没找到这条交互")?;
    let back = interaction::reopen(app, id).ok_or("收起后 reopen 应成功")?;
    if back.get("id").and_then(Value::as_str) != Some(id) {
        return Err(format!("唤回的 payload id 不对：{back}"));
    }
    if back.get("title").and_then(Value::as_str) != Some("[smoke] 唤回守卫") {
        return Err(format!("唤回的 payload 标题不对：{back}"));
    }
    if back.get("permission").and_then(|p| p.get("tool")).and_then(Value::as_str) != Some("Bash") {
        return Err(format!("唤回的 payload 少了 permission.tool：{back}"));
    }
    // ④ 唤回后状态回到 active（不再是 held）
    if !interaction::list_held(app).is_empty() {
        return Err("唤回后不该还留在 held".into());
    }
    interaction::resolve(app, id, interaction::Decision::dismissed());
    Ok(())
}

/// 收尾后一条都不许剩（泄漏的挂起项会让托盘永远显示"N 条待处理"）。
fn smoke_no_leak(app: &AppHandle) -> Result<(), String> {
    interaction::dismiss_all(app, false);
    let n = interaction::count(app);
    if n != 0 {
        return Err(format!("收尾后表里还剩 {n} 条"));
    }
    if !interaction::list_held(app).is_empty() {
        return Err("收尾后 held 列表应为空".into());
    }
    Ok(())
}

/// 发一个真实 HTTP 请求到自己的网关。
///
/// ⚠ 想自定义 **Host** 头不能走这里 —— minreq 会先写一个自己的 Host，你设的会变成
/// 第二条（见 [`smoke_host_reject`]）。要那么干就用 [`raw_status`] 的裸 socket。
fn self_request(method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
    let port = port().ok_or("网关未运行")?;
    let token = lock(&TOKEN).clone();
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut req = if method == "GET" {
        minreq::get(url)
    } else {
        minreq::post(url)
    };
    req = req.with_timeout(30);
    if !token.is_empty() {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    if let Some(b) = body {
        req = req
            .with_header("Content-Type", "application/json")
            .with_body(b.to_string());
    }
    let resp = req.send().map_err(|e| format!("请求失败：{e}"))?;
    let text = resp.as_str().unwrap_or("").to_string();
    let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((resp.status_code as u16, parsed))
}

// ---------------------------------------------------------------------------
// ISO 8601（只为发现文件里的 startedAt，不引 chrono）
// ---------------------------------------------------------------------------

fn iso8601_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso8601(ms)
}

/// epoch 毫秒 → `YYYY-MM-DDTHH:MM:SS.mmmZ`（UTC，与 JS `toISOString()` 同形）。
///
/// 实现在 [`pomodoro_core::iso8601_ms`] —— hook CLI 的 `sessions` 子命令也要用它，
/// 两处各写一遍这种"日期算法"迟早会在某天差一个闰年。
fn iso8601(ms: i64) -> String {
    pomodoro_core::iso8601_ms(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_plain_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn iso8601_matches_js_to_iso_string() {
        // 参考值由 Node 的 `new Date(ms).toISOString()` 生成，逐字节对齐
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_789_948_800_000), "2026-09-21T00:00:00.000Z");
        assert_eq!(iso8601(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
        assert_eq!(iso8601(1_704_067_199_999), "2023-12-31T23:59:59.999Z");
        assert_eq!(iso8601(951_868_800_000), "2000-03-01T00:00:00.000Z");
    }

    #[test]
    fn iso8601_now_has_expected_shape() {
        let s = iso8601_now();
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn random_hex_length_and_charset() {
        let t = random_hex(24);
        assert_eq!(t.len(), 48);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_hex(24), t);
    }

    /// DNS rebinding 的守门函数 —— 这条纯函数值得单独钉住。
    #[test]
    fn host_is_loopback_only_accepts_loopback() {
        // 放行：回环 + 可选端口（大小写、IPv6 括号形式都要认）
        for ok in ["127.0.0.1", "127.0.0.1:5277", "localhost", "LocalHost:1", "[::1]", "[::1]:5277", " 127.0.0.1 "] {
            assert!(host_is_loopback(ok), "{ok} 应被放行");
        }
        // 拦下：任何别的主机名 / 可疑写法
        for bad in [
            "evil.example.com",
            "evil.example.com:5277",
            "127.0.0.1.evil.com",
            "localhost.evil.com",
            "127.0.0.2",
            "[::2]",
            "127.0.0.1:",
            "127.0.0.1:abc",
            "[::1]x",
            "",
        ] {
            assert!(!host_is_loopback(bad), "{bad:?} 应被拦下");
        }
    }
}
