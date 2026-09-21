//! 网关发现 + 极简 HTTP/1.1 客户端。
//!
//! 对应 `pomodoro-hook.js` 的 `findGateway()` / `request()` 两个函数。
//!
//! ## 为什么手写 HTTP 而不是引 ureq / reqwest
//!
//! ① hook 是**每次工具调用都要跑**的热路径，任何多余依赖都会体现在进程启动时间上
//!    （reqwest 会把 tokio 整套拉进来，那是几百 KB 和一堆初始化）；
//! ② 这里要的能力只有一个：POST 一段 JSON、等应答（可能等 65 分钟）、拿回 JSON。
//!    没有重定向、没有 TLS（127.0.0.1）、没有连接池、不需要 cookie。
//! ③ 长轮询的语义就是「一个 socket 上挂着」，`TcpStream` + `set_read_timeout`
//!    天然对上，用异步栈反而要围着它转。
//!
//! ## 超时的语义（必须和 Node 的 `timeout` 选项一致）
//!
//! Node 的 `http.request({timeout})` 是 **socket 空闲超时**，不是总耗时超时：
//! 只要还有数据在流动就不会触发。`set_read_timeout` 在 Windows 上也是**每次
//! read 调用**的超时 —— 语义正好相同，所以「弹窗挂了 59 分钟没人点」不会误杀。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

/// 连不上本机网关卡住 3s 就够 —— 127.0.0.1 上没有"网络慢"，只有通与不通。
const CONNECT_TIMEOUT_MS: u64 = 3_000;

#[derive(Debug, Clone)]
pub struct Gateway {
    pub port: u16,
    pub token: String,
}

#[derive(Debug)]
pub struct HttpError(pub String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 让 `run()` 那种「上层统一用 String 报错」的地方能直接 `?` ——
/// hook 的失败处理只有一种：stderr 打一行、exit 0，不需要错误分类。
impl From<HttpError> for String {
    fn from(e: HttpError) -> String {
        e.0
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, HttpError> {
    Err(HttpError(msg.into()))
}

// ---------------------------------------------------------------------------
// 网关发现
// ---------------------------------------------------------------------------

/// `findGateway()`：环境变量 → `gateway.json` 候选（顺序见
/// [`pomodoro_core::gateway_candidates`]）→ `None`。
///
/// 返回 `None` 就代表「番茄钟没在跑」，hook 必须**静默退出**（见 main.rs）。
pub fn find_gateway() -> Option<Gateway> {
    let port_env = pomodoro_core::env_str("POMODORO_PORT")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let token_env = pomodoro_core::env_str("POMODORO_TOKEN").unwrap_or_default();
    if port_env > 0 && !token_env.is_empty() {
        return Some(Gateway {
            port: port_env as u16,
            token: token_env,
        });
    }

    for file in pomodoro_core::gateway_candidates() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(info) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let port = crate::util::get(&info, "port")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let token = crate::util::pick_str(&info, &["token"]);
        // JS: `if (info && info.port && info.token)` —— 两者都要 truthy
        if port > 0 && port <= u16::MAX as u64 && !token.is_empty() {
            return Some(Gateway {
                port: port as u16,
                token,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

/// `request(port, token, method, apiPath, body, timeoutMs)`。
///
/// 语义对齐 JS：
///  * 状态码 ≥ 400 → `Err("HTTP <code>: <正文前 200 字符>")`（调用方不再区分，
///    一律按"没拿到决策"处理 → 交回宿主原生询问）；
///  * 正文不是 JSON / 空 → `Ok(Value::Null)`（JS 的 `json = null`）；
///  * 任何 IO 失败 → `Err`。
pub fn request(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout_ms: u64,
) -> Result<Value, HttpError> {
    let (status, bytes) = raw_request(port, token, method, path, body, timeout_ms)?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    if status >= 400 {
        let head: String = text.chars().take(200).collect();
        return err(format!("HTTP {status}: {head}"));
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    // JS 用 try/catch 包着 JSON.parse，解析失败给 null 而不是抛错
    Ok(serde_json::from_str::<Value>(&text).unwrap_or(Value::Null))
}

/// 只关心状态码的请求（OpenCode 回传用）。
pub fn status_request(
    url: &str,
    token: Option<&str>,
    body: &Value,
    timeout_ms: u64,
) -> Result<u16, HttpError> {
    let Some(u) = parse_http_url(url) else {
        return err(format!("无法解析 URL: {url}"));
    };
    let (status, _) = raw_request(
        u.port,
        token.unwrap_or(""),
        "POST",
        &u.path,
        Some(body),
        timeout_ms,
    )?;
    Ok(status)
}

fn raw_request(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout_ms: u64,
) -> Result<(u16, Vec<u8>), HttpError> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(CONNECT_TIMEOUT_MS))
        .map_err(|e| HttpError(format!("连接网关失败: {e}")))?;

    let data = body.map(|b| b.to_string());
    let mut req = String::with_capacity(256);
    req.push_str(method);
    req.push(' ');
    req.push_str(path);
    req.push_str(" HTTP/1.1\r\n");
    // Host 必须带：网关那侧有 Host 校验（防 DNS rebinding），
    // 而且值必须是回环地址 —— 写 "localhost" 也行，这里与 Node 一致写 127.0.0.1:port
    req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    req.push_str("Accept: application/json\r\n");
    if !token.is_empty() {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(d) = &data {
        req.push_str("Content-Type: application/json\r\n");
        // 必须按**字节数**算，不是字符数：中文标题/备注会让 Content-Length 对不上，
        // 服务端要么等不到数据（读到超时）、要么把多余字节当成下一个请求
        req.push_str(&format!("Content-Length: {}\r\n", d.as_bytes().len()));
    }
    // Connection: close —— 一个请求一个 socket，收完就关。
    // 长轮询挂几十分钟，复用连接毫无意义，反而让"何时算结束"变得模糊。
    req.push_str("Connection: close\r\n\r\n");

    let timeout = Duration::from_millis(timeout_ms.max(1));
    let _ = stream.set_write_timeout(Some(timeout));
    let _ = stream.set_read_timeout(Some(timeout));

    stream
        .write_all(req.as_bytes())
        .map_err(|e| HttpError(format!("写请求失败: {e}")))?;
    if let Some(d) = &data {
        stream
            .write_all(d.as_bytes())
            .map_err(|e| HttpError(format!("写请求体失败: {e}")))?;
    }
    stream
        .flush()
        .map_err(|e| HttpError(format!("刷新请求失败: {e}")))?;

    read_response(&mut stream)
}

struct Headers {
    content_length: Option<usize>,
    chunked: bool,
}

fn read_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), HttpError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 8192];
    let mut head: Option<(u16, Headers, usize)> = None;

    loop {
        // 1) 先把响应头切出来
        if head.is_none() {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                let end = pos + 4;
                let text = String::from_utf8_lossy(&buf[..end]).to_string();
                let (status, headers) = parse_head(&text)?;
                head = Some((status, headers, end));
            }
        }

        // 2) 头齐了就判断正文是否也齐了
        if let Some((status, headers, end)) = &head {
            if let Some(len) = headers.content_length {
                if buf.len() >= end + len {
                    return Ok((*status, buf[*end..*end + len].to_vec()));
                }
            } else if headers.chunked {
                if let Some(body) = decode_chunked(&buf[*end..]) {
                    return Ok((*status, body));
                }
            }
        }

        // 3) 继续读
        match stream.read(&mut tmp) {
            Ok(0) => {
                // 对端关闭。我们发了 Connection: close，正常响应也会走到这里
                let Some((status, headers, end)) = head else {
                    return err("响应头不完整（连接被关闭）");
                };
                let rest = &buf[end..];
                let body = if headers.chunked {
                    decode_chunked(rest).unwrap_or_default()
                } else {
                    rest.to_vec()
                };
                return Ok((status, body));
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => {
                // 读超时 = 宿主那层要掐表的时刻，必须报出来而不是当成空响应：
                // 空响应会被上层当成「网关回了 null」，进而误判成"网关还活着"
                let kind = e.kind();
                if kind == std::io::ErrorKind::WouldBlock || kind == std::io::ErrorKind::TimedOut {
                    return err("读取响应超时");
                }
                return err(format!("读取响应失败: {e}"));
            }
        }
    }
}

fn parse_head(text: &str) -> Result<(u16, Headers), HttpError> {
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| HttpError(format!("状态行无法解析: {status_line}")))?;

    let mut headers = Headers {
        content_length: None,
        chunked: false,
    };
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        match k.as_str() {
            "content-length" => headers.content_length = v.parse::<usize>().ok(),
            "transfer-encoding" => {
                if v.to_ascii_lowercase().contains("chunked") {
                    headers.chunked = true;
                }
            }
            _ => {}
        }
    }
    Ok((code, headers))
}

/// 解 chunked 正文。数据不完整（还没收到结束的 0 块）时返回 `None`。
fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        let line_end = find_subslice(&data[i..], b"\r\n")? + i;
        let size_text = std::str::from_utf8(&data[i..line_end]).ok()?;
        // 分块长度后面可能跟 ";ext=..."，取分号前的部分
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        i = line_end + 2;
        if size == 0 {
            // 结束块：后面只应剩一个空行（trailer 忽略）
            return Some(out);
        }
        if i + size > data.len() {
            return None; // 正文还没收全
        }
        out.extend_from_slice(&data[i..i + size]);
        i += size;
        // 每块数据后面必须跟 CRLF
        if data.len() < i + 2 {
            return None;
        }
        i += 2;
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// `http://host[:port][/path]` —— 只要够 OpenCode 回传用就行。
struct ParsedUrl {
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Option<ParsedUrl> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(80)),
        None => (authority, 80u16),
    };
    if host.is_empty() {
        return None;
    }
    Some(ParsedUrl {
        port,
        path: path.to_string(),
    })
}

/// `encodeURIComponent()` —— 保留 `A-Za-z0-9 - _ . ! ~ * ' ( )`，其余转 `%XX`
/// （UTF-8 逐字节转，不是逐字符）。
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')');
        if safe {
            out.push(c);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_content_length() {
        let text = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n";
        let (code, h) = parse_head(text).unwrap();
        assert_eq!(code, 200);
        assert_eq!(h.content_length, Some(17));
        assert!(!h.chunked);
    }

    #[test]
    fn detects_chunked() {
        let text = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let (_, h) = parse_head(text).unwrap();
        assert!(h.chunked);
        assert_eq!(h.content_length, None);
    }

    #[test]
    fn decodes_chunked_body() {
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(body).unwrap(), b"hello world");
        // 带扩展参数的块
        let body = b"5;foo=bar\r\nhello\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(body).unwrap(), b"hello");
        // 不完整必须回 None（否则会把半个 JSON 交给解析器）
        assert!(decode_chunked(b"5\r\nhel").is_none());
    }

    #[test]
    fn finds_header_terminator() {
        assert_eq!(find_subslice(b"abc\r\n\r\nxyz", b"\r\n\r\n"), Some(3));
        assert_eq!(find_subslice(b"abc", b"\r\n\r\n"), None);
        // 不能把单独的 \r\n 当成结束：`a\r\nb\r\n\r\n` 里真正的结束在偏移 4
        assert_eq!(find_subslice(b"a\r\nb\r\n\r\n", b"\r\n\r\n"), Some(4));
    }

    #[test]
    fn parses_http_url() {
        let u = parse_http_url("http://127.0.0.1:4096").unwrap();
        assert_eq!(u.port, 4096);
        assert_eq!(u.path, "/");
        let u = parse_http_url("http://127.0.0.1:4096/session/a%20b/question/reply").unwrap();
        assert_eq!(u.path, "/session/a%20b/question/reply");
        // 缺端口走 80
        assert_eq!(parse_http_url("http://127.0.0.1/x").unwrap().port, 80);
        assert!(parse_http_url("https://127.0.0.1:1/x").is_none());
        assert!(parse_http_url("").is_none());
    }

    #[test]
    fn encode_matches_encode_uri_component() {
        assert_eq!(encode_uri_component("abc-_.!~*'()"), "abc-_.!~*'()");
        assert_eq!(encode_uri_component("a b"), "a%20b");
        assert_eq!(encode_uri_component("a/b"), "a%2Fb");
        // 中文按 UTF-8 逐字节转（"中" = E4 B8 AD）
        assert_eq!(encode_uri_component("中"), "%E4%B8%AD");
        assert_eq!(encode_uri_component(""), "");
    }
}
