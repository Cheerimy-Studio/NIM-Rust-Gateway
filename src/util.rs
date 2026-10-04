//! 通用工具函数（core/util.py 的逐行移植）。

use md5::{Digest, Md5};
use regex::Regex;
use serde_json::{Map, Value};

/// os.urandom(6).hex()
pub fn rand_id(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut b);
    let mut s = String::with_capacity(prefix.len() + 12);
    s.push_str(prefix);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

pub fn rand_hex(n: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// secrets.token_urlsafe(12)
pub fn token_urlsafe(n: usize) -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// 判断思考内容是否退化：感叹号族（! / ！）占 90% 以上且 strip 后长度 ≥16。
pub fn degenerate_reasoning(text: &Value) -> bool {
    let Some(s) = text.as_str() else { return false };
    let s = s.trim();
    if s.chars().count() < 16 {
        return false;
    }
    let ex = s.matches('!').count() + s.matches('！').count();
    ex >= (s.chars().count() as f64 * 0.9) as usize
}

pub fn str_cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `int(v)` 的宽容版：能转就转，失败返回 None。
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                n.as_f64().map(|f| f as i64)
            }
        }
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// Python `int(v or 0)`：None/缺失按 0。
pub fn int_or(v: Option<&Value>, default: i64) -> i64 {
    match v {
        None | Some(Value::Null) => default,
        Some(x) => py_int(x).unwrap_or(default),
    }
}

pub fn cfg_int(cfg: &Value, name: &str, default: i64) -> i64 {
    match cfg.get(name) {
        None | Some(Value::Null) => default,
        Some(x) => py_int(x).unwrap_or(default),
    }
}

/// Python `float(v or 0)`。
pub fn f64_or(v: Option<&Value>, default: f64) -> f64 {
    match v {
        None | Some(Value::Null) => default,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::Bool(b)) => *b as i32 as f64,
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(default),
        _ => default,
    }
}

pub fn str_or<'a>(v: Option<&'a Value>, default: &'a str) -> String {
    match v {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => default.to_string(),
    }
}

/// 预估请求 token：请求体按 3 字节≈1 token，输出按 max_tokens 预留。
pub fn estimate_request_tokens(body: &str, req: Option<&Value>) -> i64 {
    let mut max_tokens: i64 = 300;
    if let Some(r) = req {
        let raw = r
            .get("max_tokens")
            .or_else(|| r.get("max_output_tokens"))
            .filter(|x| !x.is_null());
        if let Some(x) = raw {
            if let Some(i) = py_int(x) {
                max_tokens = if i == 0 { 300 } else { i };
            }
        }
    }
    let n = body.chars().count() as i64;
    (n / 3) + ((n % 3) > 0) as i64 + max_tokens.max(1)
}

pub fn estimate_output_tokens(data_bytes: usize) -> i64 {
    let v = (data_bytes as i64) / 3 + ((data_bytes % 3) > 0) as i64;
    v.max(0)
}

fn split_loose(s: &str) -> Vec<String> {
    s.split(|c: char| c == '\r' || c == '\n' || c == ',' || c == ';')
        .map(|x| x.to_string())
        .collect()
}

/// 把「dict / list / 文本 / 字面量字符串」统一成条目列表。
pub fn literal_items(raw: &Value) -> Vec<String> {
    match raw {
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| format!("{}={}", k, json_compact(v)))
            .collect(),
        Value::Array(a) => a.iter().map(json_compact).collect(),
        Value::Null => Vec::new(),
        Value::String(text) => {
            let text = text.trim();
            let chars: Vec<char> = text.chars().collect();
            if chars.len() >= 2
                && (chars[0] == '[' || chars[0] == '{')
                && (chars[chars.len() - 1] == ']' || chars[chars.len() - 1] == '}')
            {
                let inner: String = chars[1..chars.len() - 1].iter().collect();
                let normalized = text.replace('\'', "\"");
                match serde_json::from_str::<Value>(&normalized) {
                    Ok(Value::Object(m)) => {
                        return m
                            .iter()
                            .map(|(k, v)| format!("{}={}", k, json_compact(v)))
                            .collect()
                    }
                    Ok(Value::Array(a)) => return a.iter().map(json_compact).collect(),
                    _ => return split_loose(&inner),
                }
            }
            split_loose(text)
        }
        other => vec![json_compact(other)],
    }
}

pub fn json_compact(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 安全取子字典：值不是 object 时返回空。
pub fn sub_dict<'a>(v: Option<&'a Value>) -> Option<&'a Map<String, Value>> {
    v.and_then(|x| x.as_object())
}

/// Python bool() 的真值语义（非空字符串即真，不做 "false" 识别）。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// bool / 数字 / 字符串（"false"、"0"、"no"）统一成 bool。
pub fn as_bool(v: &Value, default: bool) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::Null => false,
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" => true,
            "false" | "0" | "no" | "n" | "off" | "" => false,
            _ => default,
        },
        _ => default,
    }
}

/// bool / 数字 / 字符串统一成 bool 的便捷版（默认 false）。
pub fn as_bool2(v: &Value) -> bool {
    as_bool(v, false)
}

/// 解析渠道可用模型列表，容忍逗号/分号/换行、列表字面量、已是数组等粘贴格式。
pub fn parse_model_list(s: &Value) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for raw in literal_items(s) {
        let mut p = raw.trim().to_string();
        let chars: Vec<char> = p.chars().collect();
        if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && (chars[0] == '"' || chars[0] == '\'')
        {
            p = chars[1..chars.len() - 1].iter().collect::<String>().trim().to_string();
        }
        let n = p.chars().count();
        if !p.is_empty() && n <= 160 && !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen
}

/// 从上游响应提取人类可读错误。res = {error?, body?, status?}
pub fn upstream_snippet(res: &Value) -> String {
    // Python 的 `if res.get("error")`：空串/None/0 都算「没有错误信息」
    if let Some(e) = res.get("error").filter(|x| truthy(x)) {
        return str_cut(&json_compact(e), 200);
    }
    let body = str_or(res.get("body"), "");
    if let Ok(j) = serde_json::from_str::<Value>(&body) {
        if let Some(obj) = j.as_object() {
            let err = obj.get("error");
            let candidates = [
                err.and_then(|e| e.as_object()).and_then(|e| e.get("message")),
                obj.get("message"),
                obj.get("detail"),
                obj.get("title"),
            ];
            for m in candidates.into_iter().flatten() {
                if let Some(s) = m.as_str() {
                    if !s.is_empty() {
                        return str_cut(s, 200);
                    }
                }
            }
        }
    }
    let body = body.trim();
    if !body.is_empty() {
        str_cut(body, 160)
    } else {
        format!("上游返回 HTTP {}", int_or(res.get("status"), 0))
    }
}

pub fn mask_email(email: &str) -> String {
    let email = email.to_string();
    match email.find('@') {
        None => {
            let chars: Vec<char> = email.chars().collect();
            let mut s: String = chars.iter().take(2).collect();
            s.push_str("***");
            s
        }
        Some(at) => {
            let chars: Vec<char> = email.chars().collect();
            let head: String = chars.iter().take(at.min(2)).collect();
            let tail: String = chars[at..].iter().collect();
            format!("{}***{}", head, tail)
        }
    }
}

const QUOTE_ENTS: [(&str, &str); 6] = [
    ("&quot;", "\""),
    ("&#34;", "\""),
    ("&#x22;", "\""),
    ("&apos;", "'"),
    ("&#39;", "'"),
    ("&#x27;", "'"),
];

/// 把引号类 HTML 实体还原成真正的引号（只含 `&` 时才动手）。
pub fn unescape_quote_entities(s: &str) -> String {
    if s.is_empty() || !s.contains('&') {
        return s.to_string();
    }
    let mut out = s.to_string();
    if ["&amp;quot;", "&amp;#34;", "&amp;apos;", "&amp;#39;"]
        .iter()
        .any(|x| out.contains(x))
    {
        out = out.replace("&amp;", "&");
    }
    for (a, b) in QUOTE_ENTS {
        if out.contains(a) {
            out = out.replace(a, b);
        }
    }
    out
}

/// 拦截规则的匹配实现（运行时与后台「测试」按钮共用同一份）。
/// contains 全文；equals/prefix/suffix 先 strip；regex 只扫描前 scan_max 字符（0=全文）。
pub fn match_text(mode: &str, pattern: &str, text: &str, scan_max: usize) -> bool {
    let pat = unescape_quote_entities(pattern);
    let s = text.to_string();
    let m = if mode.is_empty() { "contains" } else { mode };
    match m {
        "contains" => !pat.is_empty() && s.contains(&pat),
        "equals" => !pat.is_empty() && s.trim() == pat,
        "prefix" => !pat.is_empty() && s.trim().starts_with(&pat),
        "suffix" => !pat.is_empty() && s.trim().ends_with(&pat),
        "regex" => {
            if pat.is_empty() {
                return false;
            }
            let subject: String = if scan_max > 0 {
                s.chars().take(scan_max).collect()
            } else {
                s
            };
            match Regex::new(&pat) {
                Ok(re) => re.is_match(&subject),
                Err(_) => false,
            }
        }
        _ => false,
    }
}

pub fn now_i() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn now_f() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// UTC 分钟键 "%Y%m%d%H%M"。
pub fn utc_minute(now: i64) -> String {
    chrono::DateTime::from_timestamp(now, 0)
        .map(|d| d.format("%Y%m%d%H%M").to_string())
        .unwrap_or_default()
}

/// UTC 小时键 "%Y%m%d%H"。
pub fn utc_hour(now: i64) -> String {
    chrono::DateTime::from_timestamp(now, 0)
        .map(|d| d.format("%Y%m%d%H").to_string())
        .unwrap_or_default()
}

/// 本地日期键 "%Y-%m-%d"。
pub fn local_day(now: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(now, 0) {
        chrono::LocalResult::Single(d) => d.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

/// 明天本地零点的 epoch 秒（日上限触发后的封禁截止）。
pub fn tomorrow_midnight(now: i64) -> i64 {
    use chrono::{Duration, TimeZone};
    match chrono::Local.timestamp_opt(now, 0) {
        chrono::LocalResult::Single(dt) => {
            let next = dt.date_naive() + Duration::days(1);
            next.and_hms_opt(0, 0, 0)
                .and_then(|nd| chrono::Local.from_local_datetime(&nd).single())
                .map(|d| d.timestamp())
                .unwrap_or(now + 86400)
        }
        _ => now + 86400,
    }
}

/// md5 hex 前 8 位（导入兜底：无 email 的账号生成 unknown-xxxxxxxx）。
pub fn md5_prefix8(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    d.iter().map(|x| format!("{:02x}", x)).collect::<String>()[..8].to_string()
}

pub fn value_get_num(v: Option<&Value>) -> i64 {
    int_or(v, 0)
}

/// Python str() 的语义：None→"None"、True/False 首字母大写。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b { "True".to_string() } else { "False".to_string() }
        }
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python s[:n] 的字符语义：取前 n 个字符的 UTF-8 安全前缀。
/// 按字节切片在中文等多字节字符上会 panic（线上实测），必须走这里。
pub fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// 金额统一保留 6 位小数，避免二进制浮点尾差入库。
pub fn round6(f: f64) -> f64 {
    (f * 1_000_000.0).round() / 1_000_000.0
}
