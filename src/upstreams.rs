//! 上游渠道管理（core/upstreams.py 的移植）：CRUD、模型策略、模型映射、固定参数。

use crate::store::{store, Obj, Value};
use crate::util;
use std::collections::HashSet;

pub fn all_upstreams() -> Vec<Value> {
    let db = store().load();
    match db.get("upstreams").and_then(|x| x.as_array()) {
        Some(a) => a.iter().filter(|u| u.is_object()).cloned().collect(),
        None => Vec::new(),
    }
}

pub fn get_upstream(uid: &str) -> Option<Value> {
    all_upstreams().into_iter().find(|u| util::str_or(u.get("id"), "") == uid)
}

pub fn ensure_default() {
    let db = store().load();
    let ids: Vec<String> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|u| util::str_or(u.get("id"), "")).collect())
        .unwrap_or_default();
    let need = ids.is_empty()
        || db.get("keys").and_then(|x| x.as_array()).map(|keys| {
            keys.iter()
                .any(|k| !ids.contains(&util::str_or(k.get("upstream_id"), "")))
        }).unwrap_or(false);
    if need {
        store().update(|db| migrate(db));
    }
}

pub fn migrate(db: &mut Value) {
    let has = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has {
        let base = util::str_or(
            db.pointer("/config/upstream_base"),
            "https://integrate.api.nvidia.com/v1",
        );
        let row = serde_json::json!({
            "id": format!("u_{}", util::rand_hex(6)),
            "name": "NVIDIA NIM",
            "base": base,
            "enabled": true,
            "weight": 10,
            "rpm_cap": 0,
            "daily_cap": 0,
            "rpm": 0,
            "tpm": 0,
            "daily_request_cap": 0,
            "daily_token_limit": 0,
            "request_timeout": 0,
            "models": [],
            "model_map": {},
            "hide_errors": 0,
            "hide_mapped": 0,
            "param_overrides": {},
        });
        if let Some(o) = db.as_object_mut() {
            let arr = o.entry("upstreams").or_insert_with(|| Value::Array(vec![]));
            if let Some(a) = arr.as_array_mut() {
                a.push(row);
            }
        }
    }
    let ids: Vec<String> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|u| util::str_or(u.get("id"), "")).collect())
        .unwrap_or_default();
    let default_id = db
        .pointer("/upstreams/0/id")
        .map(|x| util::str_or(Some(x), ""))
        .unwrap_or_default();
    if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
        for k in keys.iter_mut() {
            let uid = util::str_or(k.get("upstream_id"), "");
            if !ids.contains(&uid) {
                if let Some(o) = k.as_object_mut() {
                    o.insert("upstream_id".into(), Value::from(default_id.clone()));
                }
            }
        }
    }
}

fn find_up<'a>(ups: &'a [Value], uid: &str) -> Option<&'a Value> {
    ups.iter().find(|u| util::str_or(u.get("id"), "") == uid)
}

pub fn base_for(key: &Value) -> String {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        return util::str_or(u.get("base"), "").trim_end_matches('/').to_string();
    }
    let db = store().load();
    util::str_or(db.pointer("/config/upstream_base"), "")
        .trim_end_matches('/')
        .to_string()
}

/// 重试/超时类字段覆盖：0 或 -1 均继承全局。
pub fn override_for(key: &Value, field: &str, global_value: i64) -> i64 {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        let v = util::int_or(u.get(field), 0);
        return if v > 0 { v } else { global_value };
    }
    global_value
}

pub fn flag_for(uid: &str, field: &str, global_on: bool) -> bool {
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, uid) {
        let v = util::int_or(u.get(field), 0);
        return if v == 0 { global_on } else { v == 1 };
    }
    global_on
}

pub fn upstream_value(key: &Value, field: &str, default: &str) -> String {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        let v = util::str_or(u.get(field), "").trim().to_string();
        if !v.is_empty() {
            return v;
        }
        return default.to_string();
    }
    default.to_string()
}

pub fn map_model_for(key: &Value, model: &str) -> String {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        return util::str_or(
            u.get("model_map").and_then(|m| m.get(model)),
            model,
        );
    }
    model.to_string()
}

fn join_lines(raw: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for x in util::literal_items(raw) {
        let mut t = x.trim().to_string();
        let chars: Vec<char> = t.chars().collect();
        if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && (chars[0] == '"' || chars[0] == '\'') {
            t = chars[1..chars.len() - 1].iter().collect::<String>().trim().to_string();
        }
        if !t.is_empty() {
            parts.push(t);
        }
    }
    let joined = parts.join("\n");
    util::str_cut(&joined, 2000)
}

/// 解析模型价格表：每行 model=价格（元/次），容忍逗号分隔与字面量对象。
/// 价格 ≤0 或非法的行忽略（忽略即免费）。
pub fn parse_price_map(raw: &Value) -> Obj {
    let mut out = Obj::new();
    for line in util::literal_items(raw) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let (model, price) = (line[..eq].trim(), line[eq + 1..].trim());
        if model.is_empty() || model.chars().count() > 160 {
            continue;
        }
        let Ok(v) = price.parse::<f64>() else { continue };
        if !(v.is_finite()) || v <= 0.0 {
            continue;
        }
        out.insert(model.to_string(), serde_json::json!(util::round6(v)));
        if out.len() >= 500 {
            break;
        }
    }
    out
}

pub fn parse_model_map(raw: &Value) -> Obj {
    let mut out = Obj::new();
    for line in util::literal_items(raw) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // re.split(r"[=>]", line, maxsplit=1)
        let split_at = line.find(['=', '>']);
        if let Some(pos) = split_at {
            let (a, b) = (&line[..pos], &line[pos + 1..]);
            let (ka, kb) = (a.trim(), b.trim());
            if !ka.is_empty()
                && !kb.is_empty()
                && ka.chars().count() <= 160
                && kb.chars().count() <= 160
            {
                out.insert(ka.to_string(), Value::from(kb.to_string()));
            }
        }
        if out.len() >= 200 {
            break;
        }
    }
    out
}

/// 把参数值归一化为 JSON 原生类型。
pub fn coerce_param_value(val: &Value) -> Value {
    let s = match val {
        Value::String(s) => s,
        other => return other.clone(),
    };
    let v = s.trim();
    let low = v.to_lowercase();
    if low == "true" || low == "yes" {
        return Value::Bool(true);
    }
    if low == "false" || low == "no" {
        return Value::Bool(false);
    }
    // fullmatch(r"-?\d+") / fullmatch(r"-?\d+\.\d+")
    let is_int_re = {
        let body = v.strip_prefix('-').unwrap_or(v);
        !body.is_empty() && body.chars().all(|c| c.is_ascii_digit())
    };
    if is_int_re {
        return Value::from(v.parse::<i64>().unwrap_or(i64::MAX));
    }
    let is_float_re = {
        let body = v.strip_prefix('-').unwrap_or(v);
        match body.split_once('.') {
            Some((a, b)) => {
                !a.is_empty()
                    && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && b.chars().all(|c| c.is_ascii_digit())
            }
            None => false,
        }
    };
    if is_float_re {
        if let Ok(f) = v.parse::<f64>() {
            return serde_json::from_str::<Value>(&format!("{}", f))
                .unwrap_or_else(|_| Value::from(f));
        }
    }
    if v.starts_with('[') || v.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
    }
    Value::from(v.to_string())
}

fn is_full_match(s: &str, f: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(f)
}

fn valid_param_name(name: &str) -> bool {
    let n = name.chars().count();
    if n == 0 || n > 32 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn parse_param_pairs(text: &str) -> Obj {
    let mut out = Obj::new();
    let mut pieces: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.matches('=').count() > 1 {
            for piece in line.split([';', ',']) {
                pieces.push(piece.to_string());
            }
        } else {
            pieces.push(line.to_string());
        }
    }
    for piece in pieces {
        let piece = piece.trim();
        let Some(eq) = piece.find('=') else { continue };
        let name = piece[..eq].trim();
        let val = piece[eq + 1..].trim();
        if !valid_param_name(name) || val.chars().count() > 2000 {
            continue;
        }
        out.insert(name.to_string(), coerce_param_value(&Value::from(val)));
    }
    out
}

fn parse_scoped_text(text: &str) -> Obj {
    let mut scoped = Obj::new();
    let mut scope = "*".to_string();
    for line in text.lines() {
        let mut line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // ^([^=:]{1,160}):\s*(.+)$ 且 group(2) 里有 '='
        if let Some(colon) = line.find([':', '=']) {
            if line.as_bytes().get(colon) == Some(&b':') {
                let head = &line[..colon];
                let rest = line[colon + 1..].trim();
                if head.chars().count() <= 160
                    && !head.contains(['=', ':'])
                    && rest.contains('=')
                {
                    scope = head.trim().to_string();
                    line = rest.to_string();
                }
            }
        }
        for piece in line.split([';', ',']) {
            let piece = piece.trim();
            let Some(eq) = piece.find('=') else { continue };
            let name = piece[..eq].trim();
            let val = piece[eq + 1..].trim();
            if !valid_param_name(name) || val.chars().count() > 200 {
                continue;
            }
            let entry = scoped
                .entry(scope.clone())
                .or_insert_with(|| Value::Object(Obj::new()));
            if let Some(o) = entry.as_object_mut() {
                o.insert(name.to_string(), coerce_param_value(&Value::from(val)));
            }
        }
        scope = "*".to_string();
    }
    scoped
}

pub fn parse_param_overrides(raw: &Value) -> Value {
    if let Some(m) = raw.as_object() {
        let mut scoped = Obj::new();
        let mut flat = false;
        for (k, v) in m {
            if let Some(vm) = v.as_object() {
                let mut inner = Obj::new();
                for (pk, pv) in vm {
                    inner.insert(pk.clone(), coerce_param_value(pv));
                }
                scoped.insert(k.clone(), Value::Object(inner));
            } else {
                flat = true;
            }
        }
        if flat || (scoped.is_empty() && !m.is_empty()) {
            let lines: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}={}", k, util::json_compact(v)))
                .collect();
            return serde_json::json!({"*": parse_param_pairs(&lines.join("\n"))});
        }
        return Value::Object(scoped);
    }
    Value::Object(parse_scoped_text(&util::str_or(Some(raw), "")))
}

fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut re = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    regex::Regex::new(&re).ok()
}

fn fnmatch_case(pattern: &str, name: &str) -> bool {
    match glob_to_regex(pattern) {
        Some(re) => re.is_match(name),
        None => false,
    }
}

/// 全局 → 渠道(*) → 渠道(模型)，逐级强制覆盖。返回新 body。
pub fn apply_param_overrides(key: &Value, model: &str, body: &mut Value) {
    let cfg = store().load();
    let mut merged = parse_param_pairs(&util::str_or(cfg.pointer("/config/param_overrides"), ""));
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        let scoped = parse_param_overrides(u.get("param_overrides").unwrap_or(&Value::Null));
        if let Some(sm) = scoped.as_object() {
            if let Some(star) = sm.get("*").and_then(|x| x.as_object()) {
                for (k, v) in star {
                    merged.insert(k.clone(), v.clone());
                }
            }
            for (scope, params) in sm {
                if scope != "*" && (scope == model || fnmatch_case(scope, model)) {
                    if let Some(po) = params.as_object() {
                        for (k, v) in po {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
    }
    if let Some(bo) = body.as_object_mut() {
        for (name, val) in &merged {
            if valid_param_name(name) {
                bo.insert(name.clone(), val.clone());
            }
        }
    }
}

pub fn model_routable(model: &str, hide_mapped_global: bool) -> bool {
    let mut any_enabled = false;
    let db = store().load();
    let ups = db.get("upstreams").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    for u in &ups {
        if !u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false) {
            continue;
        }
        any_enabled = true;
        let models = u.get("models").and_then(|x| x.as_array());
        if let Some(ms) = models {
            let contains = ms.iter().any(|m| util::str_or(Some(m), "") == model);
            if !ms.is_empty() && !contains {
                continue;
            }
        }
        let hm = util::int_or(u.get("hide_mapped"), 0);
        let hide = if hm == 0 { hide_mapped_global } else { hm == 1 };
        if hide {
            let targets = u
                .get("model_map")
                .and_then(|m| m.as_object())
                .map(|m| m.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            if targets.iter().any(|t| util::str_or(Some(t), "") == model) {
                continue;
            }
        }
        return true;
    }
    !any_enabled
}

pub fn hide_original(up: Option<&Value>, cfg: Option<&Value>) -> bool {
    let v = match up {
        Some(u) => util::int_or(u.get("hide_mapped"), 0),
        None => 0,
    };
    if v == 1 {
        return true;
    }
    if v == 0 {
        let global = match cfg {
            Some(c) => c.get("hide_mapped_names").map(|x| util::truthy(x)).unwrap_or(true),
            None => store()
                .load()
                .pointer("/config/hide_mapped_names")
                .map(|x| util::truthy(x))
                .unwrap_or(true),
        };
        return global;
    }
    false
}

/// 网关对外可调用的模型清单（只由渠道配置决定，不访问上游）。
pub fn curated_models() -> Vec<String> {
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(Value::Object(Obj::new()));
    let ups: Vec<Value> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|u| u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut push = |m: &str, out: &mut Vec<String>| {
        if !m.is_empty() && !out.iter().any(|x| x == m) {
            out.push(m.to_string());
        }
    };
    let mut curated = false;
    for u in &ups {
        let hidden: HashSet<String> = if hide_original(Some(u), Some(&cfg)) {
            u.get("model_map")
                .and_then(|m| m.as_object())
                .map(|m| m.values().map(|v| util::str_or(Some(v), "")).collect())
                .unwrap_or_default()
        } else {
            HashSet::new()
        };
        if let Some(ms) = u.get("models").and_then(|x| x.as_array()) {
            for m in ms {
                let s = util::str_or(Some(m), "");
                if !s.is_empty() && !hidden.contains(&s) {
                    push(&s, &mut out);
                    curated = true;
                }
            }
        }
    }
    if curated {
        for u in &ups {
            if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
                for m in mm.keys() {
                    push(m, &mut out);
                }
            }
        }
        return out;
    }
    for u in &ups {
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for m in mm.keys() {
                push(m, &mut out);
            }
        }
    }
    out
}

fn clamp(v: &Value, lo: i64, hi: i64, default: i64) -> i64 {
    let mut v = v.clone();
    if let Value::String(s) = &v {
        let low = s.trim().to_lowercase();
        if ["true", "false", "yes", "no", "on", "off"].contains(&low.as_str()) {
            v = Value::from(util::as_bool(&v, false));
        }
    }
    match util::py_int(&v) {
        Some(iv) => {
            if iv == -1 {
                return -1;
            }
            hi.min(lo.max(iv))
        }
        None => default,
    }
}

pub fn validate_save(data: &Value) -> (Option<Value>, String) {
    let name = util::str_or(data.get("name"), "").trim().to_string();
    let mut base = util::str_or(data.get("base"), "").trim().to_string();
    base = base.trim_end_matches('/').to_string();
    if name.is_empty() {
        return (None, "名称不能为空".into());
    }
    if !base.starts_with("http://") && !base.starts_with("https://") {
        return (None, "Base URL 必须以 http(s):// 开头".into());
    }
    for suffix in [
        "/chat/completions",
        "/completions",
        "/embeddings",
        "/responses",
        "/messages",
    ] {
        if base.ends_with(suffix) {
            return (
                None,
                "Base URL 只填到 /v1 这一层,不要带 /chat/completions 这类端点".into(),
            );
        }
    }
    let models_raw = data.get("models").cloned().unwrap_or(Value::from(""));
    let mut models = util::parse_model_list(&models_raw);
    models.truncate(300);
    let mut row = serde_json::json!({
        "name": util::str_cut(&name, 40),
        "base": base,
        "weight": clamp(data.get("weight").unwrap_or(&Value::from(10)), 1, 100, 10),
        "rpm_cap": clamp(data.get("rpm_cap").unwrap_or(&Value::from(0)), 0, 1_000_000, 0),
        "daily_cap": clamp(data.get("daily_cap").unwrap_or(&Value::from(0)), 0, 100_000_000, 0),
        "rpm": clamp(data.get("rpm").unwrap_or(&Value::from(0)), 0, 1_000_000, 0),
        "tpm": clamp(data.get("tpm").unwrap_or(&Value::from(0)), 0, 100_000_000, 0),
        "daily_request_cap": clamp(data.get("daily_request_cap").unwrap_or(&Value::from(0)), 0, 100_000_000, 0),
        "daily_token_limit": clamp(data.get("daily_token_limit").unwrap_or(&Value::from(0)), 0, 10_000_000_000, 0),
        "request_timeout": clamp(data.get("request_timeout").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "connect_timeout": clamp(data.get("connect_timeout").unwrap_or(&Value::from(0)), 0, 120, 0),
        "account_cooldown_ms": clamp(data.get("account_cooldown_ms").unwrap_or(&Value::from(0)), 0, 60_000, 0),
        "acct_concurrency": clamp(data.get("acct_concurrency").unwrap_or(&Value::from(0)), 0, 10_000, 0),
        "total_concurrency": clamp(data.get("total_concurrency").unwrap_or(&Value::from(0)), 0, 10_000, 0),
        "hourly_request_limit": clamp(data.get("hourly_request_limit").unwrap_or(&Value::from(0)), 0, 100_000, 0),
        "max_retries": clamp(data.get("max_retries").unwrap_or(&Value::from(0)), 0, 20, 0),
        "retry_backoff_base_ms": clamp(data.get("retry_backoff_base_ms").unwrap_or(&Value::from(0)), 0, 60_000, 0),
        "retry_backoff_max_ms": clamp(data.get("retry_backoff_max_ms").unwrap_or(&Value::from(0)), 0, 300_000, 0),
        "retry_min_wait_ms": clamp(data.get("retry_min_wait_ms").unwrap_or(&Value::from(0)), 0, 60_000, 0),
        "ban_step_seconds": clamp(data.get("ban_step_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "ban_max_seconds": clamp(data.get("ban_max_seconds").unwrap_or(&Value::from(0)), 0, 86_400, 0),
        "hard_fail_ban_seconds": clamp(data.get("hard_fail_ban_seconds").unwrap_or(&Value::from(0)), 0, 86_400, 0),
        "hard_fail_disable_count": clamp(data.get("hard_fail_disable_count").unwrap_or(&Value::from(0)), 0, 100, 0),
        "cool_429_seconds": clamp(data.get("cool_429_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "cool_5xx_seconds": clamp(data.get("cool_5xx_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "cool_timeout_seconds": clamp(data.get("cool_timeout_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "cool_conn_seconds": clamp(data.get("cool_conn_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "breaker_threshold": clamp(data.get("breaker_threshold").unwrap_or(&Value::from(0)), 0, 50, 0),
        "breaker_seconds": clamp(data.get("breaker_seconds").unwrap_or(&Value::from(0)), 0, 86_400, 0),
        "models": models,
        "model_map": parse_model_map(data.get("model_map").unwrap_or(&Value::Object(Obj::new()))),
        "prices": parse_price_map(data.get("prices").unwrap_or(&Value::Object(Obj::new()))),
        "hide_errors": clamp(data.get("hide_errors").unwrap_or(&Value::from(0)), 0, 2, 0),
        "hide_mapped": clamp(data.get("hide_mapped").unwrap_or(&Value::from(0)), 0, 2, 0),
        "param_overrides": parse_param_overrides(data.get("param_overrides").unwrap_or(&Value::Object(Obj::new()))),
        "thinking_defaults": join_lines(data.get("thinking_defaults").unwrap_or(&Value::Null)),
        "enabled": util::as_bool(data.get("enabled").unwrap_or(&Value::Bool(true)), true),
    });
    // 载荷未带 prices 时不要插入空对象：否则 save() 的旧字段保留循环会认为
    // 新行已有该键，导致渠道表单保存（前端从不发送 prices）抹掉已设定价
    if data.get("prices").is_none() {
        if let Some(o) = row.as_object_mut() {
            o.remove("prices");
        }
    }
    (Some(row), String::new())
}

pub fn save(data: &Value) -> (Option<Value>, String) {
    let (row, err) = validate_save(data);
    let Some(mut row) = row else { return (None, err) };
    let uid_in = util::str_or(data.get("id"), "");
    let mut final_id = String::new();
    store().update(|db| {
        let ups_val = db
            .as_object_mut()
            .map(|o| o.entry("upstreams").or_insert_with(|| Value::Array(vec![])))
            .unwrap();
        let arr = match ups_val.as_array_mut() {
            Some(a) => a,
            None => return,
        };
        let mut replaced = false;
        if !uid_in.is_empty() {
            for u in arr.iter_mut() {
                if util::str_or(u.get("id"), "") == uid_in {
                    if let (Some(old), Some(newo)) = (u.as_object(), row.as_object_mut()) {
                        // 「载荷里没出现的字段」一律保留旧值：渠道表单提交全量字段不受影响，
                        // 而停用/启用等只提交部分字段的操作不会把 models/model_map/
                        // param_overrides/thinking_defaults/prices 等整块抹成默认值
                        for (k, v) in old {
                            if !data.get(k).is_some() {
                                newo.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    let enabled = row.get("enabled").map(util::as_bool2).unwrap_or(false);
                    if let Some(newo) = row.as_object_mut() {
                        newo.insert("id".into(), Value::from(uid_in.clone()));
                        if enabled {
                            newo.remove("auto_disabled_at");
                            newo.remove("auto_reason");
                        }
                    }
                    *u = row.clone();
                    final_id = uid_in.clone();
                    replaced = true;
                    break;
                }
            }
        }
        if !replaced {
            let mut new_row = row.clone();
            let new_id = format!("u_{}", util::rand_hex(6));
            if let Some(o) = new_row.as_object_mut() {
                o.insert("id".into(), Value::from(new_id.clone()));
                o.insert("created_at".into(), Value::from(util::now_i()));
            }
            arr.push(new_row);
            final_id = new_id;
        }
    });
    (get_upstream(&final_id), String::new())
}

pub fn delete(uid: &str) -> (bool, String) {
    #[derive(Default)]
    struct State {
        found: bool,
        used: i64,
        last: bool,
    }
    let mut state = State::default();
    store().update(|db| {
        let ups = db.get("upstreams").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        state.found = ups.iter().any(|u| util::str_or(u.get("id"), "") == uid);
        if !state.found {
            return;
        }
        state.used = db
            .get("keys")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter(|k| util::str_or(k.get("upstream_id"), "") == uid).count() as i64)
            .unwrap_or(0);
        if state.used > 0 {
            return;
        }
        if ups.len() <= 1 {
            state.last = true;
            return;
        }
        if let Some(o) = db.as_object_mut() {
            o.insert(
                "upstreams".into(),
                Value::Array(
                    ups.into_iter()
                        .filter(|u| util::str_or(u.get("id"), "") != uid)
                        .collect(),
                ),
            );
            if let Some(pb) = o.get_mut("pool_buckets").and_then(|x| x.as_object_mut()) {
                pb.remove(uid);
            }
            if let Some(pd) = o.get_mut("pool_daily").and_then(|x| x.as_object_mut()) {
                pd.remove(uid);
            }
            if let Some(recent) = o.get_mut("up_recent").and_then(|x| x.as_object_mut()) {
                let drop_keys: Vec<String> = recent
                    .keys()
                    .filter(|k| k.as_str() == uid || k.starts_with(&format!("{}\0", uid)))
                    .cloned()
                    .collect();
                for k in drop_keys {
                    recent.remove(&k);
                }
            }
        }
    });
    if !state.found {
        return (false, "上游不存在".into());
    }
    if state.used > 0 {
        return (false, format!("该上游仍有 {} 个账号，请先删除或转移", state.used));
    }
    if state.last {
        return (false, "至少保留一个上游".into());
    }
    (true, String::new())
}
