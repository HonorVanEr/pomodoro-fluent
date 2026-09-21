//! 值语义小工具：JS ↔ Rust 的边界都在这里。
//!
//! 这一层存在的理由：`pomodoro-hook.js` 里到处是 `a || b || ''`、`String(x)`、
//! `!!x` 这类 JS 惯用法，直接翻成 Rust 会**悄悄改变分支走向**（JS 的 falsy 有
//! `''` / `0` / `false` / `null` / `undefined` / `NaN` 六种）。所以统一收在这里，
//! 每个函数都写明它对应哪句 JS。

use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// JS falsy / 取值
// ---------------------------------------------------------------------------

/// `!!v`。注意 `0` 与 `''` 在 JS 里是 falsy —— 这是最常见的一处「翻译事故」。
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        // 数组/对象恒为真（`[]`、`{}` 在 JS 里都是真值）
        _ => true,
    }
}

/// `String(v)`。
///
/// ⚠ 有意简化：JS 对数组/对象分别给 `"1,2"` / `"[object Object]"`，这里给空串。
/// 所有真实调用点取的字段（tool_name / question / prompt / message…）都是字符串，
/// 走到这两个分支说明宿主的 payload 形状变了，那时给空串比给 `[object Object]`
/// 更好排查（弹窗上会出现空白而不是一行乱码）。
pub fn as_str_lossy(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// `v[k]`，键不存在给 `None`（**不**做 falsy 判断）。
pub fn get<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(k))
}

/// `v[k1] || v[k2] || v[k3]`：返回第一个 **truthy** 的值。
pub fn pick<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|k| get(v, k))
        .find(|x| is_truthy(x))
}

/// `String(v[k1] || v[k2] || '')`
pub fn pick_str(v: &Value, keys: &[&str]) -> String {
    pick(v, keys).map(as_str_lossy).unwrap_or_default()
}

/// `!!(v[k1] || v[k2])`
pub fn pick_bool(v: &Value, keys: &[&str]) -> bool {
    pick(v, keys).map(is_truthy).unwrap_or(false)
}

/// `Array.isArray(v[k]) ? v[k] : []`
pub fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    get(v, k)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// `typeof v === 'object' && v !== null && !Array.isArray(v)`
pub fn is_plain_object(v: &Value) -> bool {
    v.is_object()
}

/// 会话 id 只露尾部 —— `String(id).slice(-6)`（按字符，不是字节）。
pub fn short_session(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    let n = chars.len();
    chars[n.saturating_sub(6)..].iter().collect()
}

/// `path.basename(cwd)`
///
/// 只做「取最后一段」——`cwd` 可能混着 `/` 与 `\`（VS Code 与 Cursor 各有习惯），
/// 两种分隔符都要切。
pub fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.rsplit(|c| c == '/' || c == '\\').next() {
        Some(s) => s.to_string(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// SHA-1
//
// JS 侧用 crypto.createHash('sha1') 生成缓存文件名（决策缓存 / 会话文件）。
// 缓存目录 `%TEMP%/pomodoro-hook-cache` 是**两版共用的**：Electron 版写下的
// 去重记录，Rust 版必须能读到同一个文件名，否则「一次提问弹两次窗」的老毛病
// 会在两版混用期复活。所以这里必须字节级对齐，不能换个哈希。
// ---------------------------------------------------------------------------

/// SHA-1（RFC 3174）。返回 20 字节摘要。
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];

    // 填充：0x80 + 若干个 0，直到长度 ≡ 56 (mod 64)，再补 8 字节大端位长
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            let b = [
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ];
            w[i] = u32::from_be_bytes(b);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `crypto.createHash('sha1').update(s).digest('hex').slice(0, n)`
pub fn sha1_hex_prefix(s: &str, n: usize) -> String {
    let digest = sha1(s.as_bytes());
    let mut out = String::with_capacity(40);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out.truncate(n);
    out
}

// ---------------------------------------------------------------------------
// JSON 便捷构造
// ---------------------------------------------------------------------------

/// 建一个空对象（`Map` 在开了 preserve_order 后是 IndexMap，键序＝插入序）。
pub fn map() -> Map<String, Value> {
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_falsy_set_is_respected() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&json!(false)));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&json!("")));
        // JS 里 [] 与 {} 都是真值 —— 这条最容易翻错
        assert!(is_truthy(&json!([])));
        assert!(is_truthy(&json!({})));
        assert!(is_truthy(&json!("0")));
    }

    #[test]
    fn pick_skips_falsy_like_js_or() {
        let p = json!({ "a": "", "b": "有值", "c": "后面" });
        assert_eq!(pick(&p, &["a", "b", "c"]).unwrap(), "有值");
        assert_eq!(pick_str(&p, &["a", "b"]), "有值");
        // 全 falsy / 全缺 → 空
        assert_eq!(pick_str(&p, &["a", "nope"]), "");
        assert_eq!(pick_str(&json!({}), &["x"]), "");
        // 0 与 false 也会被跳过，与 JS `||` 一致
        assert_eq!(pick_str(&json!({ "n": 0, "b": false, "s": "x" }), &["n", "b", "s"]), "x");
    }

    #[test]
    fn sha1_matches_known_vectors() {
        // RFC 3174 / 公开测试向量 —— 这些通过就能保证与 Node 的
        // crypto.createHash('sha1') 完全一致（同一个算法，不存在实现差异）
        assert_eq!(
            sha1_hex_prefix("", 40),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            sha1_hex_prefix("abc", 40),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha1_hex_prefix("The quick brown fox jumps over the lazy dog", 40),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
        // 跨 64 字节分块边界（55/56/64 是填充逻辑最容易写错的地方）
        assert_eq!(
            sha1_hex_prefix(&"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq", 40),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            sha1_hex_prefix(&"a".repeat(1_000_000), 40),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn sha1_prefix_length_matches_js_slice() {
        // cacheKeyFor 用 slice(0,20)，sessionKey 用 slice(0,16)
        assert_eq!(sha1_hex_prefix("x", 20).len(), 20);
        assert_eq!(sha1_hex_prefix("x", 16).len(), 16);
        assert!(sha1_hex_prefix("x", 20)
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn short_session_keeps_last_six_chars() {
        assert_eq!(short_session("abcdefghij"), "efghij");
        assert_eq!(short_session("abc"), "abc");
        assert_eq!(short_session(""), "");
        // 按字符切，不是按字节（uuid 里不会有中文，但别留下字节切片的坑）
        assert_eq!(short_session("会话标识一二三四五六七八"), "三四五六七八");
    }

    #[test]
    fn basename_handles_both_separators() {
        assert_eq!(basename(r"C:\Users\me\proj"), "proj");
        assert_eq!(basename("/home/me/proj/"), "proj");
        assert_eq!(basename(""), "");
        assert_eq!(basename("单段"), "单段");
    }
}
