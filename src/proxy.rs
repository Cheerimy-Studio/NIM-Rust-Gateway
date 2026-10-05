//! /v1 数据平面（server.py 透传端的移植）：取号排队、重试与降级、流式透传、协议转换、拦截。

use crate::convert;
use crate::pool;
use crate::queue;
use crate::store::{store, Value};
use crate::streams::{py_json, AnthropicStream, ResponsesStream};
use crate::upstreams;
use crate::util;
use crate::webhttp::*;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use regex::bytes::Regex as BRegex;
use serde_json::{json, Map};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;

const REGEX_SCAN_MAX: usize = 8000;
const HB: f64 = 10.0; // 心跳间隔：必须远小于中间代理空闲超时（nginx 默认 60s）
const QUEUE_POLL_MAX: f64 = 3.0;
const QUEUE_MAX_WAITING: i64 = 200;

static WAITING: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

fn waiting_add(model: &str) {
    let mut g = WAITING.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    *m.entry(model.to_string()).or_insert(0) += 1;
}

fn waiting_dec(model: &str) {
    let mut g = WAITING.lock().unwrap();
    if let Some(m) = g.as_mut() {
        if let Some(v) = m.get_mut(model) {
            *v -= 1;
            if *v <= 0 {
                m.remove(model);
            }
        }
    }
}

fn waiting_get(model: &str) -> i64 {
    let g = WAITING.lock().unwrap();
    g.as_ref().and_then(|m| m.get(model)).copied().unwrap_or(0)
}

// ---------------------------------------------------------------- 配置缓存

static CFG_CACHE: RwLock<Option<(Instant, i64, Value)>> = RwLock::new(None);

pub fn cfg_all() -> Value {
    let gen = crate::store::store_gen();
    {
        let g = CFG_CACHE.read().unwrap();
        if let Some((at, g2, cfg)) = g.as_ref() {
            if at.elapsed() < Duration::from_secs(1) && *g2 == gen {
                return cfg.clone();
            }
        }
    }
    let cfg = store()
        .load()
        .get("config")
        .cloned()
        .unwrap_or_else(|| json!({}));
    *CFG_CACHE.write().unwrap() = Some((Instant::now(), gen, cfg.clone()));
    cfg
}

pub fn invalidate_cfg_cache() {
    *CFG_CACHE.write().unwrap() = None;
}

// ---------------------------------------------------------------- 请求上下文

pub struct Ctx {
    pub ip: String,
    pub headers: HeaderMap,
}

impl Ctx {
    pub fn new(headers: HeaderMap, addr: Option<SocketAddr>) -> Ctx {
        let mut ip = addr.map(|a| a.ip().to_string()).unwrap_or_else(|| "-".into());
        for k in ["cf-connecting-ip", "x-real-ip", "x-forwarded-for"] {
            if let Some(v) = headers.get(k).and_then(|x| x.to_str().ok()) {
                if !v.is_empty() {
                    ip = v.split(',').next().unwrap_or("").trim().to_string();
                    break;
                }
            }
        }
        Ctx { ip, headers }
    }

    pub fn bearer(&self) -> Option<String> {
        if let Some(auth) = self.headers.get("authorization").and_then(|x| x.to_str().ok()) {
            if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
                return Some(auth[7..].trim().to_string());
            }
        }
        self.headers
            .get("x-api-key")
            .and_then(|x| x.to_str().ok())
            .map(|s| s.to_string())
    }
}

pub fn token_entry(ctx: &Ctx, cfg: &Value) -> Option<Value> {
    let tokens = cfg.get("gateway_tokens").and_then(|t| t.as_array());
    let Some(tokens) = tokens else { return Some(json!({})) };
    if tokens.is_empty() {
        return Some(json!({}));
    }
    let given = ctx.bearer();
    for t in tokens {
        let (tok, models) = match t {
            Value::String(s) => (s.clone(), Vec::new()),
            Value::Object(m) => (
                util::str_or(m.get("t"), ""),
                m.get("m").and_then(|x| x.as_array()).cloned().unwrap_or_default(),
            ),
            _ => continue,
        };
        if let Some(g) = &given {
            if constant_eq(&tok, g) {
                return Some(json!({"t": tok, "m": models}));
            }
        }
    }
    None
}

fn constant_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

pub fn has_auth(cfg: &Value) -> bool {
    cfg.get("gateway_tokens").map(|t| util::truthy(t)).unwrap_or(false)
}

pub fn model_allowed(model: &str, entry: Option<&Value>, cfg: &Value) -> bool {
    if model.is_empty() {
        return true;
    }
    let wl_text = util::str_or(cfg.get("model_whitelist"), "");
    let wl = util::parse_model_list(&Value::from(wl_text));
    if !wl.is_empty() && !wl.iter().any(|m| m == model) {
        return false;
    }
    let bl_text = util::str_or(cfg.get("model_blacklist"), "");
    let bl = util::parse_model_list(&Value::from(bl_text));
    if !bl.is_empty() && bl.iter().any(|m| m == model) {
        return false;
    }
    let tm = entry
        .and_then(|e| e.get("m"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    if !tm.is_empty() && !tm.iter().any(|m| util::str_or(Some(m), "") == model) {
        return false;
    }
    true
}

pub fn check_model(model: &str, entry: Option<&Value>, cfg: &Value, anthropic: bool) -> Option<Response> {
    let routable = upstreams::model_routable(
        model,
        cfg.get("hide_mapped_names").map(util::truthy).unwrap_or(true),
    );
    if model_allowed(model, entry, cfg) && routable {
        return None;
    }
    Some(error_resp(
        404,
        &format!("The model '{}' does not exist or is not available for this token.", model),
        "invalid_request_error",
        Some("model_not_found"),
        anthropic,
    ))
}

// ---------------------------------------------------------------- 日志标签

const ERR_TAGS: &[(&str, &str)] = &[
    ("429", "限流"),
    ("auth", "鉴权"),
    ("payment", "余额"),
    ("timeout", "超时"),
    ("conn", "连接"),
    ("5xx", "上游5xx"),
    ("model", "模型"),
    ("channel", "渠道"),
    ("req", "请求"),
    ("pool_exhausted", "网关连接池"),
];

fn should_tag(status: i64, msg: &str) -> bool {
    if status >= 400 || status == 0 {
        return true;
    }
    ["中断", "超时", "失败", "异常", "PoolTimeout", "兜底", "错误"]
        .iter()
        .any(|k| msg.contains(k))
}

pub fn err_tag(status: i64, err: &str) -> String {
    let msg = err.trim();
    if msg.is_empty() {
        return String::new();
    }
    if status == 499 || msg.contains("客户端已断开") {
        return "[客户端断开]".into();
    }
    if msg.contains("兜底释放账号") {
        return "[网关异常]".into();
    }
    if msg.contains("排队") && msg.contains("超时") {
        return "[排队超时]".into();
    }
    let cls = pool::classify(status, 0, msg);
    match ERR_TAGS.iter().find(|(c, _)| *c == cls) {
        Some((_, tag)) => format!("[{}]", tag),
        None => "[其他]".into(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn release_log(
    ep: &str,
    model: &str,
    status: i64,
    ms: i64,
    err: &str,
    attempt: i64,
    key: Option<&Value>,
    ip: &str,
    up_model: &str,
    stream: bool,
    ttfb_ms: i64,
    in_tok: i64,
    out_tok: i64,
    tok: &str,
) -> Value {
    let mut msg = err.to_string();
    if !msg.is_empty() && should_tag(status, &msg) {
        let tag = err_tag(status, &msg);
        if !tag.is_empty() && !msg.starts_with('[') {
            msg = format!("{} {}", tag, msg).trim().to_string();
        }
    }
    json!({
        "t": util::now_i(),
        "ep": ep,
        "model": model,
        "key": key.map(|k| util::mask_email(&util::str_or(k.get("email"), ""))).unwrap_or_else(|| "-".into()),
        "st": status,
        "ms": ms,
        "err": util::str_cut(&msg, 140),
        "ip": ip,
        "att": attempt,
        "up_model": up_model,
        "stream": stream,
        "ttfb": ttfb_ms,
        "in_tok": in_tok,
        "out_tok": out_tok,
        "tok": tok_mask(tok),
        "tok_full": tok,
    })
}

/// 把连接层异常翻译成人能看懂的原因（reqwest/hyper 版）。
pub fn conn_reason(e: &reqwest::Error) -> String {
    // Python 版此路径抛 httpx.InvalidURL（非法 URL，如 IPv6 端口写错），
    // 日志/测试依赖这个异常名定位「请求体/渠道配置错误」而非账号故障
    if e.is_builder() {
        return util::str_cut(&format!("InvalidURL: {}", e), 300);
    }
    let txt = format!("{}", e);
    let low = txt.to_lowercase();
    let hint = if low.contains("dns")
        || low.contains("getaddrinfo")
        || low.contains("nodename")
        || low.contains("name resolution")
        || low.contains("temporary failure")
        || low.contains("failed to lookup")
    {
        "（DNS 解析失败：无法解析上游域名）"
    } else if low.contains("ssl") || low.contains("certificate") || low.contains("tls") || low.contains("handshake") {
        "（TLS 握手失败：证书/时间/出网被拦）"
    } else if e.is_connect() {
        if e.is_timeout() {
            "（TCP 连接超时：出网被阻断或上游不可达）"
        } else {
            "（TCP 连接失败：出网被阻断或上游不可达）"
        }
    } else if e.is_timeout() {
        "（读取超时：已连上但上游迟迟不返回）"
    } else {
        ""
    };
    util::str_cut(&format!("{}{}", txt, hint), 300)
}

// ---------------------------------------------------------------- 退避与用量

fn backoff_ms(cfg: &Value, attempt: i64, key: Option<&Value>) -> i64 {
    let eff = |field: &str, default: i64| -> i64 {
        let up = key
            .and_then(|k| upstreams::get_upstream(&util::str_or(k.get("upstream_id"), "")));
        let v = match &up {
            Some(u) => util::int_or(u.get(field), 0),
            None => 0,
        };
        if v > 0 { v } else { default }
    };
    let base = eff("retry_backoff_base_ms", util::cfg_int(cfg, "retry_backoff_base_ms", 500)).max(0);
    let cap = eff("retry_backoff_max_ms", util::cfg_int(cfg, "retry_backoff_max_ms", 8000)).max(base);
    let target = cap.min(base * (1i64 << attempt.saturating_sub(1).min(20)));
    let mut wait = 0;
    if target > 0 {
        use rand::Rng;
        let lo = (target + 1) / 2;
        wait = rand::thread_rng().gen_range(lo..=target);
    }
    let min_wait = eff("retry_min_wait_ms", util::cfg_int(cfg, "retry_min_wait_ms", 0)).max(0);
    wait.max(min_wait)
}

pub fn full_usage(u: Option<&Value>) -> Value {
    let mut out = match u {
        Some(v) if v.is_object() => v.clone(),
        _ => json!({}),
    };
    let p = util::int_or(out.get("prompt_tokens"), 0);
    let c = util::int_or(out.get("completion_tokens"), 0);
    out["prompt_tokens"] = Value::from(p);
    out["completion_tokens"] = Value::from(c);
    if out.get("total_tokens").map(|x| x.is_null()).unwrap_or(true) {
        out["total_tokens"] = Value::from(p + c);
    }
    out
}

fn usage_has_prompt(u: Option<&Value>) -> bool {
    u.and_then(|u| u.as_object())
        .map(|u| u.contains_key("prompt_tokens") && !u.get("prompt_tokens").map(|x| x.is_null()).unwrap_or(true))
        .unwrap_or(false)
}

pub fn build_usage(res: &Value, req_body: &str) -> Value {
    if usage_has_prompt(res.get("usage")) {
        return full_usage(res.get("usage"));
    }
    if !res.get("streamed").map(util::truthy).unwrap_or(false) {
        if let Ok(j) = serde_json::from_str::<Value>(&util::str_or(res.get("body"), "")) {
            if usage_has_prompt(j.get("usage")) {
                return full_usage(j.get("usage"));
            }
        }
    }
    let est_in = req_body.chars().count() as i64;
    let est_in = est_in / 3 + ((est_in % 3) > 0) as i64;
    let out_bytes = util::int_or(res.get("out_bytes"), 0) as usize;
    full_usage(Some(&json!({
        "prompt_tokens": est_in,
        "completion_tokens": util::estimate_output_tokens(out_bytes),
    })))
}

// ---------------------------------------------------------------- 会话/训练记录

pub fn train_messages(req: &Value) -> Option<Value> {
    let msgs = req.get("messages").and_then(|m| m.as_array())?;
    if msgs.is_empty() {
        return None;
    }
    let mut out: Vec<Value> = Vec::new();
    for m in msgs {
        let Some(mo) = m.as_object() else { continue };
        let mut role = util::str_or(mo.get("role"), "user");
        if !["system", "user", "assistant", "tool"].contains(&role.as_str()) {
            role = "user".into();
        }
        let content = mo.get("content").cloned().unwrap_or(Value::Null);
        let content = match &content {
            Value::String(s) => s.clone(),
            Value::Array(_) => convert::flatten_content(&content),
            Value::Null => String::new(),
            other => util::py_str(other),
        };
        out.push(json!({"role": role, "content": content}));
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

pub fn train_sse_text(raw: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(raw);
    let mut content: Vec<String> = Vec::new();
    let mut reasoning: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(j) = serde_json::from_str::<Value>(data) else { continue };
        let Some(choices) = j.get("choices").and_then(|c| c.as_array()) else { continue };
        for ch in choices {
            let Some(delta) = ch.get("delta").and_then(|d| d.as_object()) else { continue };
            if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                content.push(c.to_string());
            }
            let r = delta
                .get("reasoning_content")
                .filter(|x| util::truthy(x))
                .or_else(|| delta.get("reasoning").filter(|x| util::truthy(x)));
            if let Some(r) = r.and_then(|x| x.as_str()) {
                reasoning.push(r.to_string());
            }
        }
    }
    (content.concat(), reasoning.concat())
}

pub fn no_training(headers: &HeaderMap) -> bool {
    headers.contains_key("x-ngw-skip-training")
}

pub async fn record_training(
    cfg: &Value,
    ep: &str,
    model: &str,
    messages: Option<Value>,
    resp_text: &str,
    reasoning: &str,
    usage: Option<&Value>,
) {
    let Some(messages) = messages else { return };
    if resp_text.is_empty() && reasoning.is_empty() {
        return;
    }
    let max_n = util::cfg_int(cfg, "training_log_max", 500).max(0);
    if max_n == 0 {
        return;
    }
    let min_chars = util::cfg_int(cfg, "training_min_chars", 20);
    if min_chars > 0 {
        if resp_text.trim().chars().count() < min_chars as usize {
            return;
        }
        let has_real_user = messages
            .as_array()
            .map(|a| {
                a.iter().any(|m| {
                    util::str_or(m.get("role"), "") == "user"
                        && util::str_or(m.get("content"), "").trim().chars().count() >= 4
                })
            })
            .unwrap_or(false);
        if !has_real_user {
            return;
        }
    }
    let entry = json!({
        "t": util::now_i(),
        "ep": util::str_cut(ep, 8),
        "model": util::str_cut(model, 80),
        "messages": messages,
        "response": resp_text,
        "reasoning": reasoning,
        "usage": {
            "prompt_tokens": util::int_or(usage.and_then(|u| u.get("prompt_tokens")), 0),
            "completion_tokens": util::int_or(usage.and_then(|u| u.get("completion_tokens")), 0),
        },
    });
    if serde_json::to_string(&entry).map(|s| s.len() > 512 * 1024).unwrap_or(true) {
        return;
    }
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let tr = obj.entry("training").or_insert_with(|| Value::Array(vec![]));
        if let Some(a) = tr.as_array_mut() {
            a.insert(0, entry);
            a.truncate(max_n as usize);
        }
    });
}

pub async fn log_session(cfg: &Value, model: &str, key_email: &str, req_messages: &Value, resp_content: &str, status: i64) {
    let max_n = util::cfg_int(cfg, "session_log_max", 100).max(0);
    if max_n == 0 {
        return;
    }
    let row = json!({
        "t": util::now_i(),
        "model": util::str_cut(model, 80),
        "key": util::str_cut(&util::mask_email(key_email), 40),
        "status": status,
        "req": util::str_cut(&serde_json::to_string(req_messages).unwrap_or_default(), 2000),
        "resp": util::str_cut(resp_content, 2000),
    });
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let sessions = obj.entry("sessions").or_insert_with(|| Value::Array(vec![]));
        if let Some(a) = sessions.as_array_mut() {
            a.insert(0, row);
            a.truncate(max_n as usize);
        }
    });
}

// ---------------------------------------------------------------- 拦截（自定义回复）

fn resolve_channel(model: &str) -> String {
    let db = store().load();
    pool::resolve_channel(&db, model)
}

fn rule_scope_hit(rule: &Value, model: &str) -> bool {
    let ms: Vec<String> = rule
        .get("models")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().map(|x| util::str_or(Some(x), "")).filter(|x| !x.trim().is_empty()).collect())
        .unwrap_or_default();
    if !ms.is_empty() && (model.is_empty() || !ms.iter().any(|m| m == model)) {
        return false;
    }
    let us: Vec<String> = rule
        .get("upstreams")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().map(|x| util::str_or(Some(x), "")).filter(|x| !x.trim().is_empty()).collect())
        .unwrap_or_default();
    if us.is_empty() {
        return true;
    }
    let cur = resolve_channel(model);
    if !cur.is_empty() {
        return us.iter().any(|u| *u == cur);
    }
    let db = store().load();
    let capable = pool::capable_channels(&db, model);
    us.iter().any(|u| capable.contains(u))
}

pub fn match_custom_rule(req: &Value, cfg: &Value, allow_prompt: bool, model: &str) -> Option<(Value, String)> {
    if !cfg.get("intercept_enabled").map(util::truthy).unwrap_or(false) {
        return None;
    }
    let rules = cfg.get("custom_rules").and_then(|r| r.as_array())?;
    if rules.is_empty() {
        return None;
    }
    let mut last_user = String::new();
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for m in msgs.iter().rev() {
            let Some(mo) = m.as_object() else { continue };
            if util::str_or(mo.get("role"), "") == "user" {
                let c = mo.get("content").cloned().unwrap_or(Value::Null);
                last_user = match &c {
                    Value::String(s) => s.clone(),
                    Value::Array(_) => convert::flatten_content(&c),
                    _ => String::new(),
                };
                break;
            }
        }
    }
    if last_user.is_empty() && allow_prompt {
        let mut p = req.get("prompt").cloned().unwrap_or(Value::Null);
        if let Some(arr) = p.as_array_mut() {
            let joined: Vec<String> = arr.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect();
            p = Value::from(joined.join(" "));
        }
        if let Some(ps) = p.as_str() {
            if !ps.is_empty() {
                last_user = ps.to_string();
            }
        }
    }
    if last_user.is_empty() {
        return None;
    }
    for r in rules {
        let Some(ro) = r.as_object() else { continue };
        let mode = util::str_or(ro.get("match_mode"), "contains");
        let mode = if mode.is_empty() { "contains".to_string() } else { mode };
        let pat = util::str_or(ro.get("pattern"), "");
        if pat.is_empty() {
            continue;
        }
        if !rule_scope_hit(r, model) {
            continue;
        }
        if util::match_text(&mode, &pat, &last_user, REGEX_SCAN_MAX) {
            return Some((r.clone(), last_user));
        }
    }
    None
}

pub async fn log_intercept(cfg: &Value, ep: &str, rule: &Value, content: &str, ip: &str, tok: &str, model: &str) {
    let row = json!({
        "t": util::now_i(),
        "ep": util::str_cut(ep, 8),
        "rule": util::str_cut(&util::str_or(rule.get("name"), ""), 40),
        "mode": util::str_or(rule.get("match_mode"), ""),
        "pattern": util::str_cut(&util::str_or(rule.get("pattern"), ""), 80),
        "ip": util::str_cut(ip, 45),
        "tok": tok_mask(tok),
        "model": util::str_cut(model, 60),
        "content": util::str_cut(content, 200),
    });
    let rid = util::str_or(rule.get("id"), "");
    let cap = util::cfg_int(cfg, "intercept_log_max", 100).max(0) as usize;
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let logs = obj.entry("intercepted").or_insert_with(|| Value::Array(vec![]));
        if let Some(a) = logs.as_array_mut() {
            a.insert(0, row);
            a.truncate(cap);
        }
        if let Some(rules) = obj
            .get_mut("config")
            .and_then(|c| c.get_mut("custom_rules"))
            .and_then(|r| r.as_array_mut())
        {
            for r in rules.iter_mut() {
                if util::str_or(r.get("id"), "") == rid {
                    if let Some(o) = r.as_object_mut() {
                        let hits = util::int_or(o.get("hits"), 0);
                        o.insert("hits".into(), Value::from(hits + 1));
                        o.insert("last_hit_at".into(), Value::from(util::now_i()));
                    }
                    break;
                }
            }
        }
    });
}

fn json_resp_raw(v: &Value) -> Response {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    with_cors(resp)
}

pub fn sse_response(body: String) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    with_cors(resp)
}

pub fn protocol_keepalive(protocol: &str) -> Bytes {
    if protocol == "messages" {
        Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n")
    } else {
        Bytes::from_static(b": keepalive\n\n")
    }
}

pub fn sse_error_event(msg: &str) -> Bytes {
    let payload = json!({"error": {"message": msg, "type": "upstream_error"}});
    Bytes::from(format!(
        "data: {}\n\ndata: [DONE]\n\n",
        py_json(&payload)
    ))
}

pub fn keepalive_frame(ep_tag: &str, model: &str, first: bool) -> Bytes {
    let created = util::now_i();
    let obj = if ep_tag == "cmpl" {
        json!({
            "id": "cmpl-keepalive", "object": "text_completion", "created": created, "model": model,
            "choices": [{"index": 0, "text": "", "finish_reason": null}],
        })
    } else {
        json!({
            "id": "chatcmpl-keepalive", "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": (if first { json!({"role": "assistant"}) } else { json!({}) }), "finish_reason": null}],
        })
    };
    Bytes::from(format!("data: {}\n\n", py_json(&obj)))
}

/// 构造拦截回复的响应（按客户端协议；流式输出单块内容 + 终端帧）。
pub fn custom_reply_response(reply_text: &str, model: &str, stream: bool, protocol: &str, meta: &Value) -> Response {
    let comp_len = (reply_text.chars().count() / 3).max(1) as i64;
    let chat = json!({
        "id": util::rand_id("chatcmpl-"),
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": reply_text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 0, "completion_tokens": comp_len, "total_tokens": comp_len},
    });
    let sse_body = |chunk: Value, tail: Value| {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            py_json(&chunk),
            py_json(&tail)
        )
    };
    if protocol == "completions" {
        let comp = json!({
            "id": util::rand_id("cmpl-"),
            "object": "text_completion",
            "model": model,
            "choices": [{"index": 0, "text": reply_text, "logprobs": null, "finish_reason": "stop"}],
            "usage": chat["usage"],
        });
        if stream {
            let id = comp["id"].clone();
            let chunk = json!({
                "id": id, "object": "text_completion", "model": model,
                "choices": [{"index": 0, "text": reply_text, "logprobs": null, "finish_reason": null}],
            });
            let tail = json!({
                "id": id, "object": "text_completion", "model": model,
                "choices": [{"index": 0, "text": "", "logprobs": null, "finish_reason": "stop"}],
            });
            return sse_response(sse_body(chunk, tail));
        }
        return json_resp_raw(&comp);
    }
    if stream {
        if protocol == "chat" {
            let id = chat["id"].clone();
            let chunk = json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": reply_text}, "finish_reason": null}],
            });
            let tail = json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            });
            return sse_response(sse_body(chunk, tail));
        }
        let mut pend = String::new();
        let fake_chunk = json!({"model": model, "choices": [{"delta": {"role": "assistant", "content": reply_text}}]});
        let fake = format!("data: {}\n\ndata: [DONE]\n\n", py_json(&fake_chunk));
        if protocol == "responses" {
            let mut conv = ResponsesStream::new(model);
            conv.feed(&fake, &mut pend);
            conv.finalize(&mut pend);
        } else {
            let mut conv = AnthropicStream::new(model, 1);
            conv.feed(&fake, &mut pend);
            conv.finalize(&mut pend);
        }
        return sse_response(pend);
    }
    if protocol == "anthropic" {
        return json_resp_raw(&convert::chat_to_anthropic(&chat));
    }
    if protocol == "responses" {
        return json_resp_raw(&convert::chat_to_responses(&chat, meta));
    }
    json_resp_raw(&chat)
}

// ---------------------------------------------------------------- 熔断与取号

fn breaker_queue_reason(br: &Value) -> String {
    format!(
        "模型熔断 · 失败 {} 次 · {} 秒后重试",
        util::int_or(br.get("fails"), 0),
        util::int_or(br.get("left"), 0)
    )
}

/// 排队参与守卫：客户端在排队期间断开时 hyper 丢弃 handler，
/// 队列条目与等待计数器由 Drop 兜底回收，否则泄漏到重启为止。
struct QueueGuard {
    qid: String,
    model: String,
}
impl Drop for QueueGuard {
    fn drop(&mut self) {
        queue::remove(&self.qid);
        waiting_dec(&self.model);
    }
}

pub struct Taken {
    pub ok: bool,
    pub key: Option<Value>,
    pub status: i64,
    pub message: String,
}

pub async fn take_account(ep: &str, model: &str, est_tokens: i64, cfg: &Value, tok: &str, ip: &str) -> Taken {
    let mut max_wait = util::cfg_int(cfg, "queue_max_wait", 30);
    let cool429 = util::cfg_int(cfg, "cool_429_seconds", 30);
    if max_wait > 0 && cool429 > 0 {
        max_wait = max_wait.max((cool429 + 2).min(600));
    }
    let br = pool::breaker_open(model);
    if let Some(b) = &br {
        let queue_on = cfg.get("queue_enabled").map(util::truthy).unwrap_or(true);
        if !queue_on || max_wait <= 0 {
            return Taken {
                ok: false,
                key: None,
                status: 503,
                message: format!(
                    "模型 {} 此前连续失败 {} 次，熔断中，约 {} 秒后自动恢复探测",
                    model,
                    util::int_or(b.get("fails"), 0),
                    util::int_or(b.get("left"), 0)
                ),
            };
        }
    }

    // 熔断打开时不尝试取号，直接进排队等待恢复
    let mut acq_reason = String::new();
    let mut acq_total: i64 = 0;
    let mut acquired: Option<Value> = None;
    if br.is_none() {
        let a = pool::acquire(est_tokens, model);
        acq_reason = a.reason;
        acq_total = a.total;
        if a.result == "ok" {
            acquired = a.key;
        }
    }
    if let Some(k) = acquired {
        return Taken { ok: true, key: Some(k), status: 0, message: String::new() };
    }
    if acq_total == 0 && br.is_none() {
        return Taken { ok: false, key: None, status: 503, message: "密钥池为空，请先在后台导入密钥".into() };
    }
    let permanent = {
        let db = store().load();
        // permanent 由 acquire 计算：重算一次（total>0 且结构上无账号可服务）
        let mut total = 0i64;
        let mut model_ok = 0i64;
        if let Some(keys) = db.get("keys").and_then(|x| x.as_array()) {
            let ups: HashMap<String, Value> = db
                .get("upstreams")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter(|u| u.is_object()).map(|u| (util::str_or(u.get("id"), ""), u.clone())).collect())
                .unwrap_or_default();
            for k in keys {
                if !k.get("enabled").map(util::truthy).unwrap_or(false) {
                    continue;
                }
                total += 1;
                let uid = util::str_or(k.get("upstream_id"), "");
                let up = ups.get(&uid);
                if let Some(u) = up {
                    if !u.get("enabled").map(util::truthy).unwrap_or(false) {
                        continue;
                    }
                }
                if model.is_empty() {
                    model_ok += 1;
                    continue;
                }
                let hide = upstreams::hide_original(up, db.get("config"));
                let targets: Vec<String> = up
                    .and_then(|u| u.get("model_map").and_then(|m| m.as_object()))
                    .map(|m| m.values().map(|v| util::str_or(Some(v), "")).collect())
                    .unwrap_or_default();
                if hide && targets.iter().any(|t| t == model) {
                    continue;
                }
                let models = up.and_then(|u| u.get("models").and_then(|m| m.as_array()));
                if let Some(ms) = models {
                    if !ms.is_empty() && !ms.iter().any(|m| util::str_or(Some(m), "") == model) {
                        continue;
                    }
                }
                if pool::model_missing_fresh(&db, &uid, model) {
                    continue;
                }
                model_ok += 1;
            }
        }
        total > 0 && model_ok == 0
    };
    let queue_on = cfg.get("queue_enabled").map(util::truthy).unwrap_or(true);
    if !queue_on || max_wait <= 0 || permanent {
        if permanent {
            return Taken {
                ok: false,
                key: None,
                status: 404,
                message: format!("模型 {} 在所有渠道均不可用：{}", model, acq_reason),
            };
        }
        return Taken { ok: false, key: None, status: 429, message: format!("暂无可用账号：{}", acq_reason) };
    }
    if waiting_get(model) >= QUEUE_MAX_WAITING {
        return Taken {
            ok: false,
            key: None,
            status: 503,
            message: format!("排队已满（{} 个请求在等账号），请稍后重试", waiting_get(model)),
        };
    }
    let mut q_reason = acq_reason.clone();
    if let Some(b) = &br {
        q_reason = breaker_queue_reason(b);
    }
    let qid = queue::add(ep, model, ip, &tok_mask(tok), &q_reason);
    waiting_add(model);
    let _qguard = QueueGuard { qid: qid.clone(), model: model.to_string() };
    let deadline = Instant::now() + Duration::from_secs_f64(max_wait as f64);
    let poll = (util::cfg_int(cfg, "queue_poll_ms", 400) as f64 / 1000.0).max(0.05);
    let mut waiting_breaker = br.is_some();
    let mut backoff = poll;
    let mut last_reason = acq_reason.clone();
    let mut got: Option<Value> = None;
    while Instant::now() < deadline {
        let mut hint = 0.0f64;
        if !waiting_breaker || pool::breaker_open(model).is_none() {
            waiting_breaker = false;
            let a = pool::acquire(est_tokens, model);
            if a.result == "ok" {
                got = a.key;
                break;
            }
            queue::set_reason(&qid, &a.reason);
            hint = a.wait_hint;
            last_reason = a.reason;
            let crowded = waiting_get(model) > 4;
            backoff = if hint > 0.0 || !crowded { poll } else { (backoff * 2.0).min(QUEUE_POLL_MAX) };
        } else {
            let b = pool::breaker_open(model).unwrap_or(json!({}));
            hint = util::f64_or(b.get("left"), 0.0);
            queue::set_reason(&qid, &breaker_queue_reason(&b));
        }
        let wait = if hint > 0.0 { hint } else { backoff }.max(poll);
        // 关键：加上抖动，避免大量等待者同一时刻一起重试
        use rand::Rng;
        let jitter = 0.7 + rand::thread_rng().gen_range(0.0..0.6);
        tokio::time::sleep(Duration::from_secs_f64(wait * jitter)).await;
    }
    drop(_qguard); // 守卫统一出队；显式 drop 保证在返回值构造前完成
    if let Some(k) = got {
        return Taken { ok: true, key: Some(k), status: 0, message: String::new() };
    }
    if let Some(b) = br {
        return Taken {
            ok: false,
            key: None,
            status: 503,
            message: format!(
                "模型 {} 此前连续失败 {} 次，熔断中，已等待 {} 秒仍未恢复",
                model,
                util::int_or(b.get("fails"), 0),
                max_wait
            ),
        };
    }
    Taken {
        ok: false,
        key: None,
        status: 429,
        message: format!("排队等待 {} 秒后超时，暂无可用账号：{}", max_wait, last_reason),
    }
}

// ---------------------------------------------------------------- 上游失败响应

pub fn upstream_fail(key: Option<&Value>, res: Option<&Value>, cfg: &Value, anthropic: bool) -> Response {
    let status = util::int_or(res.and_then(|r| r.get("status")), 0);
    let code = if status >= 400 { status } else { 502 };
    let mut hide = cfg.get("hide_upstream_errors").map(util::truthy).unwrap_or(true);
    if let Some(k) = key {
        hide = upstreams::flag_for(&util::str_or(k.get("upstream_id"), ""), "hide_errors", hide);
    }
    if !hide {
        let body = util::str_or(res.and_then(|r| r.get("body")), "").trim().to_string();
        if status >= 400 && !body.is_empty() {
            let mut resp = body.into_response();
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if let Ok(sc) = StatusCode::from_u16(status.min(599) as u16) {
                *resp.status_mut() = sc;
            }
            return with_cors(resp);
        }
        let msg = {
            let m = util::str_or(res.and_then(|r| r.get("error")), "");
            if m.is_empty() { "上游请求失败".to_string() } else { m }
        };
        return error_resp(code.min(599) as u16, &msg, "upstream_error", None, anthropic);
    }
    let cls = pool::classify(status, 0, &util::str_or(res.and_then(|r| r.get("error")), ""));
    let specific = match cls {
        "timeout" => Some("上游响应超时，请稍后重试"),
        "conn" => Some("无法连接上游，请稍后重试"),
        _ => None,
    };
    let msg: String = specific.map(|s| s.to_string()).unwrap_or_else(|| match code {
        429 => "渠道限流，请稍后重试".into(),
        401 | 403 => "渠道鉴权失败，请联系管理员".into(),
        404 => "渠道不支持该请求或模型".into(),
        400 => "渠道拒绝了请求参数，原因见管理后台日志".into(),
        c if c >= 500 => "渠道暂不可用，请稍后重试".into(),
        c => format!("请求未被渠道接受，HTTP {}，原因见管理后台日志", c),
    });
    error_resp(
        code.min(599) as u16,
        &msg,
        "upstream_error",
        Some(&format!("upstream_{}", code)),
        anthropic,
    )
}

// ---------------------------------------------------------------- 账号持有守卫

/// 账号持有守卫：任何未正常释放的路径（异常、客户端断连导致 handler 被丢弃）在 Drop 时兜底释放。
pub struct HoldGuard {
    key: Option<Value>,
    released: bool,
    ep: String,
    model: String,
    ip: String,
    tok: String,
    stream: bool,
    t0: Instant,
}

impl HoldGuard {
    pub fn new(key: Option<Value>, ep: &str, model: &str, ip: &str, tok: &str, stream: bool) -> HoldGuard {
        HoldGuard {
            key,
            released: true,
            ep: ep.to_string(),
            model: model.to_string(),
            ip: ip.to_string(),
            tok: tok.to_string(),
            stream,
            t0: Instant::now(),
        }
    }

    /// 开始持有账号（take_account 成功后调用）。
    pub fn hold(&mut self, key: &Value) {
        self.key = Some(key.clone());
        self.released = false;
    }

    /// 释放责任移交通知（流式 pump / 慢启动 pump 接管后调用）。
    pub fn handoff(&mut self) {
        self.released = true;
    }

    /// 正常释放完成，Drop 不再动作。
    pub fn clear(&mut self) {
        self.key = None;
        self.released = true;
    }
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Some(k) = self.key.clone() {
            // handler 被 hyper 丢弃（客户端断连）或 panic 时到这里
            let ms = self.t0.elapsed().as_millis() as i64;
            pool::release(
                &util::str_or(k.get("id"), ""),
                true,
                499,
                "客户端已断开",
                None,
                Some(&release_log(
                    &self.ep, &self.model, 499, ms, "客户端已断开", 1, Some(&k), &self.ip,
                    "", self.stream, ms, 0, 0, &self.tok,
                )),
                0,
            );
        }
    }
}

/// 奖品 Key 并发位守卫：Drop 时释放（断连/panic 也安全）。
struct PrizeGuard {
    key: String,
}
impl Drop for PrizeGuard {
    fn drop(&mut self) {
        crate::wheel::prize_key_release(&self.key);
    }
}

// ---------------------------------------------------------------- 统一流式 pump

/// SSE chunk 按网络帧对齐，多字节 UTF-8 字符可能被拆到两个 chunk；
/// 逐块 lossy 转换会把半个字符变成 U+FFFD。把末尾不完整的序列留给下一块。
fn split_incomplete_utf8(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<u8> {
    carry.extend_from_slice(chunk);
    let mut cut = carry.len();
    if let Some(pos) = carry.iter().rposition(|&b| b & 0xC0 != 0x80) {
        let need = match carry[pos] {
            b if b < 0x80 => 0,
            b if b >= 0xF0 => 4,
            b if b >= 0xE0 => 3,
            b if b >= 0xC0 => 2,
            _ => 0,
        };
        if need > 0 && carry.len() - pos < need {
            cut = pos;
        }
    } else {
        cut = 0;
    }
    carry.drain(..cut).collect()
}

fn money_of(u: &Value) -> f64 {
    util::f64_or(u.get("balance"), 0.0)
}

/// 多用户计费上下文：Some((用户id, Key id)) 表示本次调用来自用户 Key。
/// kind 在鉴权时一次性解析（"all"/"free"/"paid"），预检与 /v1/models 过滤共用。
#[derive(Clone)]
pub struct UserCtx {
    pub id: String,
    pub key_id: String,
    pub kind: &'static str,
}

/// 奖品 Key 上下文：体验卡（限时+并发限制）/ 专属额度（计次）。
/// 不计费、不做用户 RPM；模型锁定 prize.model。
#[derive(Clone)]
pub struct PrizeCtx {
    pub key: String,
    pub model: String,
    pub concurrency: i64,
    pub metered: bool,
}

pub struct StreamCtx {
    pub ep: String,
    pub ep_tag: String,
    pub model: String,
    pub up_model: String,
    pub body: String,
    pub ip: String,
    pub tok: String,
    pub status: i64,
    pub attempt: i64,
    pub t0: Instant,
    pub rewrite: bool,
    pub heartbeat: bool,
    pub ttfb_deadline: f64,
    pub protocol: Option<String>, // None = chat 直通；Some("responses"/"messages") = 转换
    pub capture_train: bool,
    pub user: Option<(String, String)>, // (用户id, Key id) —— 成功后计费
    pub prize_key: Option<String>, // 奖品 Key（专属额度计次）
}

/// 统一的流式 pump：读上游 chunk →（可选协议转换/模型改名/退化思考清理）→ 下发。
/// 心跳保活、TTFB/空闲超时、断连回收、账号释放都在这里收口 —— 释放恰好一次。
async fn run_pump(
    mut resp: reqwest::Response,
    key: Value,
    first_chunk: Option<Bytes>,
    ctx: StreamCtx,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let idle_to = util::cfg_int(&cfg_all(), "sse_idle_timeout", 0) as f64;
    let byte_limit = if ctx.ttfb_deadline > 0.0 {
        ctx.ttfb_deadline
    } else if idle_to > 0.0 {
        idle_to
    } else {
        300.0
    };
    let model_re = BRegex::new(r#""model"\s*:\s*"[^"]*""#).ok();
    let model_to = format!("\"model\":\"{}\"", ctx.model);
    let bang_run = BRegex::new(r"!{16,}").ok();
    let bang_fw = BRegex::new(r"(?:\xEF\xBC\x81){16,}").ok();
    let t0_f = util::now_f() - ctx.t0.elapsed().as_secs_f64();

    let mut out_bytes: usize = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut first_chunk_at: f64 = 0.0;
    let mut idle: f64 = 0.0;
    let mut started = false;
    let mut client_gone = false;
    let mut truncated = String::new();
    let mut scrubbed = false;
    let mut bang_tail: usize = 0;
    let mut train: Vec<u8> = Vec::new();

    let is_convert = ctx.protocol.is_some();
    let mut conv_resp: Option<ResponsesStream> = None;
    let mut conv_anth: Option<AnthropicStream> = None;
    if ctx.protocol.as_deref() == Some("responses") {
        conv_resp = Some(ResponsesStream::new(&ctx.model));
    } else if ctx.protocol.as_deref() == Some("messages") {
        let est_input = util::estimate_request_tokens(&ctx.body, None);
        conv_anth = Some(AnthropicStream::new(&ctx.model, est_input));
    }

    // 发送辅助：转换模式喂状态机产事件；直通模式发原始字节。返回 false = 客户端已断开。
    macro_rules! emit_bytes {
        ($b:expr) => {{
            if tx.send(Ok($b)).await.is_err() {
                client_gone = true;
            }
        }};
    }
    macro_rules! feed_conv {
        ($text:expr) => {{
            let mut pend = String::new();
            if let Some(c) = conv_resp.as_mut() {
                c.feed($text, &mut pend);
            }
            if let Some(c) = conv_anth.as_mut() {
                c.feed($text, &mut pend);
            }
            if !pend.is_empty() {
                emit_bytes!(Bytes::from(pend));
            }
        }};
    }

    let mut utf8_carry: Vec<u8> = Vec::new();
    if let Some(fc) = first_chunk {
        out_bytes += fc.len();
        first_chunk_at = util::now_f();
        started = true;
        // 训练语料必须包含首帧（Python 版经 _out(first_chunk) 一并累积）
        if ctx.capture_train && train.len() < 1048576 {
            train.extend_from_slice(&fc);
        }
        if is_convert {
            let ready = split_incomplete_utf8(&mut utf8_carry, &fc);
            feed_conv!(&String::from_utf8_lossy(&ready));
        } else {
            emit_bytes!(fc);
        }
    }
    if !client_gone && ctx.heartbeat {
        let frame = match &ctx.protocol {
            Some(p) => protocol_keepalive(p),
            None => keepalive_frame(&ctx.ep_tag, &ctx.model, true),
        };
        emit_bytes!(frame);
    }
    while !client_gone {
        let chunk = match tokio::time::timeout(Duration::from_secs_f64(HB), resp.chunk()).await {
            Err(_) => {
                idle += HB;
                let limit = if started { idle_to } else { byte_limit };
                if limit > 0.0 && idle >= limit {
                    if !started {
                        // 上游一直无输出：与 Python 一致按错误收口
                        truncated = format!("上游首字节超时（{}s）", limit as i64);
                    } else {
                        truncated = format!("上游空闲超时（{}s）", limit as i64);
                    }
                    break;
                }
                let frame = match &ctx.protocol {
                    Some(p) => protocol_keepalive(p),
                    None => keepalive_frame(&ctx.ep_tag, &ctx.model, false),
                };
                emit_bytes!(frame);
                continue;
            }
            Ok(Err(e)) => {
                if started {
                    truncated = format!("上游流中断：{}", e);
                } else {
                    truncated = format!("上游连接中断：{}", e);
                }
                break;
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(c))) => c,
        };
        idle = 0.0;
        started = true;
        out_bytes += chunk.len();
        if first_chunk_at == 0.0 {
            first_chunk_at = util::now_f();
        }
        if ctx.capture_train && train.len() < 1048576 {
            train.extend_from_slice(&chunk);
        }
        if is_convert {
            let ready = split_incomplete_utf8(&mut utf8_carry, &chunk);
            feed_conv!(&String::from_utf8_lossy(&ready));
            continue;
        }
        let mut b: Vec<u8> = chunk.to_vec();
        if ctx.rewrite {
            buf.extend_from_slice(&b);
            match buf.iter().rposition(|&c| c == b'\n') {
                None => continue,
                Some(cut) => {
                    let out = buf[..cut + 1].to_vec();
                    buf = buf[cut + 1..].to_vec();
                    let rewritten = model_re
                        .as_ref()
                        .map(|re| re.replace_all(&out, model_to.as_bytes()).to_vec())
                        .unwrap_or(out);
                    emit_bytes!(Bytes::from(rewritten));
                    continue;
                }
            }
        }
        // 退化思考清理:成片 '!'(半/全角 16+ 连发)字节级清除,跨块边界合并
        let has_bang4 = b.windows(4).any(|w| w == b"!!!!");
        if has_bang4 || (bang_tail >= 4 && b.first() == Some(&b'!')) {
            if bang_tail > 0 {
                let lead = b.iter().take_while(|&&c| c == b'!').count();
                if lead > 0 && bang_tail + lead >= 16 {
                    b = b[lead..].to_vec();
                    scrubbed = true;
                    bang_tail = b.iter().rev().take_while(|&&c| c == b'!').count();
                    emit_bytes!(Bytes::from(b));
                    continue;
                }
                bang_tail = 0;
            }
            let mut nb = b.clone();
            if let Some(re) = &bang_run {
                nb = re.replace_all(&nb, &b""[..]).to_vec();
            }
            if let Some(re) = &bang_fw {
                nb = re.replace_all(&nb, &b""[..]).to_vec();
            }
            if nb != b {
                scrubbed = true;
                b = nb;
            }
        }
        bang_tail = if b.last() == Some(&b'!') {
            b.iter().rev().take_while(|&&c| c == b'!').count()
        } else {
            0
        };
        emit_bytes!(Bytes::from(b));
    }
    // 收尾：先把残留在 carry 里的最后几字节（跨块截断的字符）喂给状态机，再做终端事件
    if is_convert && !utf8_carry.is_empty() {
        let rest = std::mem::take(&mut utf8_carry);
        feed_conv!(&String::from_utf8_lossy(&rest));
    }
    // 截断要显式告知下游，不能把半截内容当完整回复（静默截断比显式报错危险）
    if !client_gone && !truncated.is_empty() {
        if is_convert {
            let mut pend = String::new();
            if let Some(c) = conv_resp.as_mut() {
                c.fail(&truncated, &mut pend);
            }
            if let Some(c) = conv_anth.as_mut() {
                c.fail(&truncated, &mut pend);
            }
            if !pend.is_empty() {
                emit_bytes!(Bytes::from(pend));
            }
        } else if started {
            emit_bytes!(sse_error_event(&truncated));
        }
    } else if !client_gone && is_convert {
        // 上游正常结束（无论发没发 [DONE]）都必须产出终端事件，
        // 否则严格 SDK 会一直等 message_stop / response.completed 而挂起
        let mut pend = String::new();
        if let Some(c) = conv_resp.as_mut() {
            c.finalize(&mut pend);
        }
        if let Some(c) = conv_anth.as_mut() {
            c.finalize(&mut pend);
        }
        if !pend.is_empty() {
            emit_bytes!(Bytes::from(pend));
        }
    }
    if !client_gone && ctx.rewrite && !buf.is_empty() {
        let rewritten = model_re
            .as_ref()
            .map(|re| re.replace_all(&buf, model_to.as_bytes()).to_vec())
            .unwrap_or_else(|| buf.clone());
        emit_bytes!(Bytes::from(rewritten));
    }
    // 统计与释放（恰好一次）
    let ms = ctx.t0.elapsed().as_millis() as i64;
    let ttfb = if first_chunk_at > 0.0 {
        ((first_chunk_at - t0_f) * 1000.0) as i64
    } else {
        ms
    };
    let est_in = ctx.body.chars().count() as i64;
    let est_in = est_in / 3 + ((est_in % 3) > 0) as i64;
    let usage = json!({
        "prompt_tokens": est_in,
        "completion_tokens": util::estimate_output_tokens(out_bytes),
    });
    let st = if client_gone { 499 } else { ctx.status };
    let note = if client_gone {
        "客户端已断开".to_string()
    } else if !truncated.is_empty() {
        truncated.clone()
    } else if scrubbed {
        "思考退化已清理".to_string()
    } else {
        String::new()
    };
    pool::arelease(
        util::str_or(key.get("id"), ""),
        true,
        st,
        note.clone(),
        Some(usage.clone()),
        Some(release_log(
            &ctx.ep, &ctx.model, st, ms, &note, ctx.attempt, Some(&key), &ctx.ip,
            &ctx.up_model, true, ttfb,
            util::int_or(usage.get("prompt_tokens"), 0),
            util::int_or(usage.get("completion_tokens"), 0),
            &ctx.tok,
        )),
        0,
    )
    .await;
    // 多用户计费：仅完整成功的调用计费（「仅成功计费」口径与面板提示一致）；
    // 客户端断连（499）与上游截断/中断的半截回复不扣费
    if st == ctx.status && !client_gone && truncated.is_empty() {
        if let Some((uid, kid)) = &ctx.user {
            crate::users::bill(
                uid, kid,
                &util::str_or(key.get("upstream_id"), ""),
                &ctx.model, &ctx.up_model,
                st, ms,
                util::int_or(usage.get("prompt_tokens"), 0),
                util::int_or(usage.get("completion_tokens"), 0),
                true,
            );
        }
        if let Some(pk) = &ctx.prize_key {
            crate::wheel::prize_key_consume(pk);
        }
    }
    // 训练资料：正常结束的流式对话全文（截断/断连的半截语料污染训练集，不要）
    if !client_gone && truncated.is_empty() && ctx.capture_train {
        let (content, reasoning) = if is_convert {
            match (&conv_resp, &conv_anth) {
                (Some(c), _) => (c.text_acc(), c.reasoning_acc()),
                (_, Some(c)) => (c.text_acc(), c.reasoning_acc()),
                _ => (String::new(), String::new()),
            }
        } else {
            train_sse_text(&train)
        };
        let cfgv = cfg_all();
        let req_v: Value = serde_json::from_str(&ctx.body).unwrap_or(Value::Null);
        record_training(&cfgv, &ctx.ep_tag, &ctx.model, train_messages(&req_v), &content, &reasoning, Some(&usage)).await;
    }
    // 响应体在「释放 + 记录」全部完成之后才关闭：客户端一收到流结束，
    // 训练记录/释放日志就必须已经在库里（与 Python 生成器 finally 的语义一致），
    // 否则紧接着的查询会看不到刚结束的对话（实测套件就因此失败）
    drop(tx);
}

/// 包装为 axum 响应：pump 在后台任务中运行。
fn spawn_pump(resp: reqwest::Response, key: Value, first_chunk: Option<Bytes>, ctx: StreamCtx) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(run_pump(resp, key, first_chunk, ctx, tx));
    sse_body_response(rx)
}

fn sse_body_response(rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Response {
    let body = Body::from_stream(ReceiverStream::new(rx));
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    with_cors(resp)
}

// ---------------------------------------------------------------- 主代理循环（chat/completions/embeddings 直通）

#[allow(clippy::too_many_arguments)]
pub async fn proxy_chat(ctx: Ctx, endpoint: &str, ep_tag: &str, body_bytes: Bytes) -> Response {
    let cfg = cfg_all();
    let entry = token_entry(&ctx, &cfg);
    // 奖品 Key（sk-prz-…）：体验卡/专属额度，单模型受限调用，不走用户计费
    let mut prize_ctx: Option<PrizeCtx> = None;
    // 用户 Key（sk-usr-…）：多用户计费链路
    let mut user_ctx: Option<UserCtx> = None;
    if let Some(b) = ctx.bearer() {
        if b.starts_with(crate::users::user_key_prefix()) {
            match crate::users::lookup_user_key(&b) {
                Some((u, t)) => {
                    user_ctx = Some(UserCtx {
                        id: util::str_or(u.get("id"), ""),
                        key_id: util::str_or(t.get("id"), ""),
                        kind: crate::users::key_kind(&t),
                    });
                }
                None => {
                    return error_resp(
                        401,
                        "调用 Key 无效或已被停用",
                        "invalid_request_error",
                        Some("invalid_api_key"),
                        false,
                    );
                }
            }
        } else if b.starts_with(crate::wheel::PRIZE_KEY_PREFIX) {
            match crate::wheel::lookup_prize_key(&b) {
                Ok(row) => {
                    prize_ctx = Some(PrizeCtx {
                        key: b.clone(),
                        model: util::str_or(row.get("model"), ""),
                        concurrency: util::int_or(row.get("concurrency"), 1),
                        metered: util::f64_or(row.get("quota"), 0.0) > 0.0,
                    });
                }
                Err(e) => {
                    return error_resp(401, e, "invalid_request_error", Some("invalid_api_key"), false);
                }
            }
        }
    }
    if user_ctx.is_none() && prize_ctx.is_none() && has_auth(&cfg) && entry.is_none() {
        return error_resp(
            401,
            "访问令牌无效。请在后台「系统设置」中配置访问令牌，并以 Authorization: Bearer <令牌> 调用。",
            "invalid_request_error",
            None,
            false,
        );
    }
    let body_text = String::from_utf8_lossy(&body_bytes).trim_start_matches('\u{feff}').to_string();
    if body_text.len() > MAX_BODY {
        return error_resp(413, "请求体过大，上限 20MB", "invalid_request_error", None, false);
    }
    let Ok(req0) = serde_json::from_str::<Value>(&body_text) else {
        return error_resp(400, "请求体不是合法 JSON", "invalid_request_error", None, false);
    };
    if !req0.is_object() {
        return error_resp(400, "请求体不是合法 JSON", "invalid_request_error", None, false);
    }
    let mut req = req0;
    let model = util::str_or(req.get("model"), "");
    let stream = req.get("stream").map(util::truthy).unwrap_or(false);
    let est = util::estimate_request_tokens(&body_text, Some(&req));
    let tok = util::str_or(entry.as_ref().and_then(|e| e.get("t")), "");
    if let Some(bad) = check_model(&model, entry.as_ref(), &cfg, false) {
        return bad;
    }
    // 奖品 Key：模型锁定 + 并发占位（超限 429）。占位由 HoldGuard 式 Drop 释放。
    let mut prize_guard: Option<PrizeGuard> = None;
    if let Some(pc) = &prize_ctx {
        let prize_ok = crate::wheel::lookup_prize_key(&pc.key)
            .map(|row| crate::wheel::prize_key_allows_model(&row, &model))
            .unwrap_or(false);
        if !prize_ok {
            return error_resp(
                403,
                &format!("该奖品 Key 仅限模型 {}，本次请求模型 {}", util::char_prefix(&pc.model, 80), util::char_prefix(&model, 80)),
                "invalid_request_error",
                Some("prize_model_not_allowed"),
                false,
            );
        }
        if !crate::wheel::prize_key_acquire(&pc.key, pc.concurrency) {
            return error_resp(
                429,
                &format!("该奖品 Key 并发已满（上限 {}），请稍后重试", pc.concurrency),
                "rate_limit_error",
                None,
                false,
            );
        }
        prize_guard = Some(PrizeGuard { key: pc.key.clone() });
    }
    // 多用户链路：Key 类型限制（免费/付费专用 Key）+ 余额预检 + 用户级每分钟限速
    if let Some(uc) = &user_ctx {
        let Some(u) = crate::users::auth_user(&uc.id) else {
            return error_resp(401, "用户已被停用", "invalid_request_error", None, false);
        };
        let paid = crate::users::model_is_paid(&model);
        let kind_name = if uc.kind == "free" { "免费" } else { "付费" };
        let allowed = match uc.kind {
            "free" => !paid,
            "paid" => paid,
            _ => true,
        };
        if !allowed {
            return error_resp(
                403,
                &format!("该 Key 仅限调用{}模型（模型 {} 不在允许范围内）", kind_name, util::char_prefix(&model, 80)),
                "invalid_request_error",
                Some("key_model_not_allowed"),
                false,
            );
        }
        if paid && money_of(&u) <= 0.0 {
            return error_resp(
                402,
                "余额不足，无法调用付费模型（免费模型不受影响）",
                "insufficient_quota",
                Some("insufficient_quota"),
                false,
            );
        }
        let rpm = if paid {
            util::int_or(u.get("paid_rpm"), 0)
        } else {
            util::int_or(u.get("free_rpm"), 0)
        };
        if let Err(wait) = crate::users::check_user_rpm(&uc.id, if paid { "paid" } else { "free" }, rpm) {
            let mut resp = error_resp(
                429,
                &format!("调用过于频繁（每分钟 {} 次上限），约 {} 秒后可重试", rpm, wait.ceil() as i64),
                "rate_limit_error",
                None,
                false,
            );
            if let Ok(v) = axum::http::HeaderValue::from_str(&(wait.ceil() as i64).to_string()) {
                resp.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
            return resp;
        }
    }
    if ep_tag != "emb" {
        if let Some((rule, content)) = match_custom_rule(&req, &cfg, ep_tag == "cmpl", &model) {
            log_intercept(&cfg, ep_tag, &rule, &content, &ctx.ip, &tok, &model).await;
            let proto = if ep_tag == "cmpl" { "completions" } else { "chat" };
            return custom_reply_response(&util::str_or(rule.get("reply"), ""), &model, stream, proto, &json!({}));
        }
    }
    let mut max_attempts = util::cfg_int(&cfg, "max_retries", 3).max(1) + 1;
    let mut attempt: i64 = 0;
    let mut last: Option<Value> = None;
    let mut last_key: Option<Value> = None;
    let mut downgraded = false;
    let mut reuse_key: Option<Value> = None;
    let mut same_key_tried: HashSet<String> = HashSet::new();
    let mut rl_left = util::cfg_int(&cfg, "max_retries", 2).max(1);
    let mut up_model = model.clone();
    let mut hold = HoldGuard::new(None, ep_tag, &model, &ctx.ip, &tok, stream);

    let out = 'attempts: loop {
        if attempt >= max_attempts {
            break 'attempts None;
        }
        attempt += 1;
        let taken = if let Some(k) = reuse_key.take() {
            Taken { ok: true, key: Some(k), status: 0, message: String::new() }
        } else {
            take_account(ep_tag, &model, est, &cfg, &tok, &ctx.ip).await
        };
        if !taken.ok {
            pool::arelease(
                String::new(),
                false,
                taken.status,
                taken.message.clone(),
                None,
                Some(release_log(ep_tag, &model, taken.status, 0, &taken.message, attempt, None, &ctx.ip, &model, stream, 0, 0, 0, &tok)),
                0,
            )
            .await;
            break 'attempts Some(error_resp(
                taken.status.clamp(400, 599) as u16,
                &taken.message,
                "invalid_request_error",
                None,
                false,
            ));
        }
        let key = taken.key.unwrap();
        hold.hold(&key);
        max_attempts = attempt.max(
            upstreams::override_for(&key, "max_retries", util::cfg_int(&cfg, "max_retries", 3)).max(1) + 1,
        );
        up_model = upstreams::map_model_for(&key, &model);
        req["model"] = Value::from(up_model.clone());
        upstreams::apply_param_overrides(&key, &model, &mut req);
        convert::normalize_body_types(&mut req);
        convert::sanitize_request(&mut req);
        let body = serde_json::to_string(&req).unwrap_or_default();
        let t0 = Instant::now();
        let mut rerr = String::new();
        let mut rstatus: i64 = 0;
        let mut rbody = String::new();
        let mut got_resp: Option<reqwest::Response> = None;
        let mut slow_fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send>>> = None;
        {
            let verify_tls = cfg.get("verify_tls").map(util::truthy).unwrap_or(true);
            let connect_to = upstreams::override_for(&key, "connect_timeout", util::cfg_int(&cfg, "connect_timeout", 10));
            let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg, "request_timeout", 300));
            let client = get_http(verify_tls, connect_to);
            let url = format!("{}/{}", upstreams::base_for(&key), endpoint);
            let fut = client
                .post(&url)
                .header("Accept", if stream { "text/event-stream" } else { "application/json" })
                .header("Authorization", format!("Bearer {}", util::str_or(key.get("apikey"), "")))
                .header("Content-Type", "application/json")
                .body(body.clone());
            let built = fut.build();
            if let Err(e) = &built {
                rerr = conn_reason(e);
                rstatus = 0;
            }
            if stream && built.is_ok() {
                // 上游响应头可能几十秒不返回（推理模型实测 128s）：分片等待，
                // 等不到就转「边发心跳边等响应头」的兜底流
                let ttfb_cfg = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
                let commit_after = if ttfb_cfg <= 0.0 { 12.0 } else { 12.0_f64.min(ttfb_cfg) };
                let fut2 = client.execute(built.unwrap());
                let mut pinned = Box::pin(fut2);
                let mut waited = 0.0;
                while waited < commit_after {
                    match tokio::time::timeout(Duration::from_secs_f64(0.5), pinned.as_mut()).await {
                        Ok(Ok(r)) => {
                            got_resp = Some(r);
                            break;
                        }
                        Ok(Err(e)) => {
                            rerr = conn_reason(&e);
                            rstatus = 0;
                            break;
                        }
                        Err(_) => {
                            waited += 0.5;
                            if waited >= commit_after {
                                slow_fut = Some(pinned);
                                break;
                            }
                        }
                    }
                }
            } else if built.is_ok() {
                match tokio::time::timeout(
                    Duration::from_secs(read_to.max(1) as u64),
                    client.execute(built.unwrap()),
                )
                .await
                {
                    Ok(Ok(r)) => got_resp = Some(r),
                    Ok(Err(e)) => {
                        rerr = conn_reason(&e);
                        rstatus = 0;
                    }
                    Err(_) => {
                        rerr = format!("上游 {} 秒内未返回", read_to);
                        rstatus = 0;
                    }
                }
            }
        }
        // 响应头迟迟不返回：交给兜底流（先提交 200 + 心跳，拿到响应头后顺势透传）
        if got_resp.is_none() && slow_fut.is_some() {
            let fut = slow_fut.take().unwrap();
            hold.handoff();
            break 'attempts Some(slow_start_response(
                fut, key, ctx, body, ep_tag, &model, &up_model, attempt, t0, tok, None, stream,
                user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                prize_ctx.as_ref().map(|p| p.key.clone()),
            ));
        }
        if rstatus == 0 && !rerr.is_empty() && got_resp.is_none() {
            // 连接层异常，走统一的失败处理
            let ms = t0.elapsed().as_millis() as i64;
            let success = false;
            let err = util::upstream_snippet(&json!({"status": 0, "body": "", "error": rerr}));
            let usage = build_usage(&json!({"status": 0, "body": "", "streamed": false, "out_bytes": 0}), &body);
            if !same_key_tried.contains(&util::str_or(key.get("id"), ""))
                && attempt < max_attempts
            {
                same_key_tried.insert(util::str_or(key.get("id"), ""));
                reuse_key = Some(key.clone());
                tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                continue;
            }
            hold.clear();
            pool::arelease(
                util::str_or(key.get("id"), ""),
                success,
                0,
                err.clone(),
                Some(usage),
                Some(release_log(ep_tag, &model, 0, ms, &err, attempt, Some(&key), &ctx.ip, &up_model, stream, ms, 0, 0, &tok)),
                0,
            )
            .await;
            last = Some(json!({"status": 0, "body": "", "error": rerr}));
            last_key = Some(key);
            if convert::is_channel_exhausted(&util::str_or(last.as_ref().and_then(|l| l.get("error")), "")) {
                break 'attempts Some(upstream_fail(last_key.as_ref(), last.as_ref(), &cfg, false));
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, last_key.as_ref()) as u64)).await;
            continue;
        }
        let mut r = got_resp.unwrap();
        rstatus = r.status().as_u16() as i64;
        if stream && (200..400).contains(&rstatus) {
            // 预读首帧：只为识别立即返回的 SSE 错误事件以便换号/降级重试。
            // 等一小段；上游推理慢时立刻转入心跳透传，绝不能把客户端干等几十秒。
            let ttfb_to = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
            let pre_to = if ttfb_to <= 0.0 { 8.0 } else { 8.0_f64.min(ttfb_to) };
            let mut first_chunk: Option<Bytes> = None;
            let mut pre_timed_out = false;
            let mut pre_err = String::new();
            let pre_deadline = Instant::now() + Duration::from_secs_f64(pre_to);
            loop {
                let left = pre_deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    pre_timed_out = true;
                    break;
                }
                match tokio::time::timeout(left.min(Duration::from_millis(500)), r.chunk()).await {
                    Err(_) => continue, // 分片轮询
                    Ok(Err(e)) => {
                        pre_err = conn_reason(&e);
                        break;
                    }
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(c))) => {
                        first_chunk = Some(c);
                        break;
                    }
                }
            }
            if !pre_err.is_empty() {
                rerr = pre_err;
                rstatus = 0;
            } else if pre_timed_out {
                hold.handoff();
                break 'attempts Some(spawn_pump(
                    r,
                    key,
                    None,
                    StreamCtx {
                        ep: ep_tag.to_string(),
                        ep_tag: ep_tag.to_string(),
                        model: model.clone(),
                        up_model: up_model.clone(),
                        body: body.clone(),
                        ip: ctx.ip.clone(),
                        tok: tok.clone(),
                        status: rstatus,
                        attempt,
                        t0,
                        rewrite: up_model != model,
                        heartbeat: true,
                        ttfb_deadline: ttfb_to,
                        protocol: None,
                        capture_train: !no_training(&ctx.headers)
                            && util::cfg_int(&cfg, "training_log_max", 500) > 0,
                        user: user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                        prize_key: prize_ctx.as_ref().map(|p| p.key.clone()),
                    },
                ));
            } else {
                let text = first_chunk
                    .as_ref()
                    .map(|c| String::from_utf8_lossy(c).to_string())
                    .unwrap_or_default();
                let head = util::char_prefix(&text, 500);
                let is_sse_error = text.trim_start().starts_with("event: error")
                    || text.trim_start().starts_with("data: {\"error\"")
                    || (head.contains("\"error\"")
                        && ["thinking", "unsupported", "duplicate"].iter().any(|k| text.to_lowercase().contains(k)));
                let is_empty = text.trim().is_empty();
                if is_sse_error && !downgraded {
                    downgraded = true;
                    rstatus = 400;
                    if convert::is_duplicate_field_error(&text, 400)
                        && convert::strip_reasoning_from_messages(&mut req)
                    {
                        drop(r);
                        reuse_key = Some(key);
                        continue 'attempts;
                    }
                    let tdefs = convert::parse_thinking_defaults(&upstreams::upstream_value(&key, "thinking_defaults", ""));
                    if convert::downgrade_thinking(&mut req, &up_model, &tdefs) {
                        drop(r);
                        reuse_key = Some(key);
                        continue 'attempts;
                    }
                    rbody = text;
                } else if is_empty {
                    // 空流：标记 502 走错误路径（换号重试；尝试耗尽后返回 502），绝不透传空流
                    rstatus = 502;
                    rerr = "上游返回空流".into();
                    rbody = String::new();
                } else {
                    hold.handoff();
                    break 'attempts Some(spawn_pump(
                        r,
                        key,
                        first_chunk,
                        StreamCtx {
                            ep: ep_tag.to_string(),
                            ep_tag: ep_tag.to_string(),
                            model: model.clone(),
                            up_model: up_model.clone(),
                            body: body.clone(),
                            ip: ctx.ip.clone(),
                            tok: tok.clone(),
                            status: rstatus,
                            attempt,
                            t0,
                            rewrite: up_model != model,
                            heartbeat: false,
                            ttfb_deadline: 0.0,
                            protocol: None,
                            capture_train: !no_training(&ctx.headers)
                                && util::cfg_int(&cfg, "training_log_max", 500) > 0,
                            user: user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                            prize_key: prize_ctx.as_ref().map(|p| p.key.clone()),
                        },
                    ));
                }
            }
        }
        if stream {
            // 流式响应没能交接给透传（空流 / SSE 错误且降级失败等）：丢弃响应释放连接
            drop(r);
        } else {
            match r.text().await {
                Ok(t) => rbody = t,
                Err(e) => {
                    rerr = conn_reason(&e);
                    rstatus = 0;
                }
            }
        }
        let ms = t0.elapsed().as_millis() as i64;
        let mut success = (200..400).contains(&rstatus) && rstatus != 0;
        if success {
            let st = rbody.trim();
            if st.is_empty() || st == "{}" {
                success = false;
                rerr = "上游返回空响应".into();
                rstatus = 502;
            } else if st.starts_with('{') && util::char_prefix(st, 200).contains("\"error\"") {
                if let Ok(j) = serde_json::from_str::<Value>(st) {
                    if j.get("error").map(|e| util::truthy(e)).unwrap_or(false) {
                        success = false;
                        rerr = util::upstream_snippet(&json!({"status": 200, "body": st, "error": ""}));
                        rstatus = 502;
                    }
                }
            }
        }
        let err = if success {
            String::new()
        } else {
            util::upstream_snippet(&json!({"status": rstatus, "body": rbody, "error": rerr}))
        };
        // 瞬态错误（连接失败/超时/5xx/空响应）先同号快速重试一次（不惩罚账号）
        if !success
            && !same_key_tried.contains(&util::str_or(key.get("id"), ""))
            && (rstatus == 0 || rstatus >= 500)
            && attempt < max_attempts
        {
            same_key_tried.insert(util::str_or(key.get("id"), ""));
            reuse_key = Some(key.clone());
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
            continue;
        }
        // 400 参数类降级重试：必须在释放之前 continue —— 账号保持持有（INFLIGHT 计数正确），
        // 否则旧号已回池而重试仍在用，并发限额被绕过、最终释放记为 odd release
        if rstatus == 400 && convert::is_duplicate_field_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::strip_reasoning_from_messages(&mut req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && convert::thinking_unsupported(&rbody, rstatus) && !downgraded {
            downgraded = true;
            let tdefs = convert::parse_thinking_defaults(&upstreams::upstream_value(&key, "thinking_defaults", ""));
            if convert::downgrade_thinking(&mut req, &up_model, &tdefs) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && convert::is_deserialize_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::coerce_all_types(&mut req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && convert::is_unsupported_param_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::strip_unsupported_params(&mut req, &rbody) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        let usage = build_usage(
            &json!({"status": rstatus, "body": rbody, "streamed": false, "out_bytes": 0, "usage": null}),
            &body,
        );
        hold.clear();
        pool::arelease(
            util::str_or(key.get("id"), ""),
            success,
            rstatus,
            err.clone(),
            Some(usage.clone()),
            Some(release_log(
                ep_tag, &model, rstatus, ms, &err, attempt, Some(&key), &ctx.ip, &up_model, stream, ms,
                util::int_or(usage.get("prompt_tokens"), 0),
                util::int_or(usage.get("completion_tokens"), 0),
                &tok,
            )),
            0,
        )
        .await;
        if success {
            if let Some((uid, kid)) = user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())) {
                crate::users::bill(
                    &uid, &kid,
                    &util::str_or(key.get("upstream_id"), ""),
                    &model, &up_model,
                    rstatus, ms,
                    util::int_or(usage.get("prompt_tokens"), 0),
                    util::int_or(usage.get("completion_tokens"), 0),
                    false,
                );
            }
            // 奖品 Key：专属额度计次（仅成功调用）
            if let Some(pc) = &prize_ctx {
                if pc.metered {
                    crate::wheel::prize_key_consume(&pc.key);
                }
            }
        }
        if success {
            let mut j: Value = serde_json::from_str(&rbody).unwrap_or(Value::Null);
            let mut out_body = rbody.clone();
            if j.is_object() {
                if up_model != model {
                    j["model"] = Value::from(model.clone());
                }
                // 退化思考清理:成片重复感叹号(推理栈故障)不透传给下游
                if let Some(ch0) = j
                    .get_mut("choices")
                    .and_then(|c| c.as_array_mut())
                    .and_then(|a| a.first_mut())
                    .and_then(|c| c.as_object_mut())
                {
                    if let Some(msg) = ch0.get_mut("message").and_then(|m| m.as_object_mut()) {
                        for rk in ["reasoning_content", "reasoning"] {
                            if let Some(rv) = msg.get(rk) {
                                if util::degenerate_reasoning(rv) {
                                    msg.insert(rk.to_string(), Value::from(""));
                                }
                            }
                        }
                    }
                }
                // 统一补全 usage：部分上游不返回 usage，严格客户端会因缺字段判定失败
                if !j.get("usage").map(|u| u.is_object()).unwrap_or(false) {
                    j["usage"] = full_usage(Some(&usage));
                } else {
                    let u = j.get("usage").cloned();
                    j["usage"] = full_usage(u.as_ref());
                }
                out_body = serde_json::to_string(&j).unwrap_or(out_body);
            }
            let resp_text = j
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .map(|c| match c {
                    Value::String(s) => s.clone(),
                    other => convert::flatten_content(other),
                })
                .unwrap_or_default();
            log_session(&cfg, &model, &util::str_or(key.get("email"), ""), &req.get("messages").cloned().unwrap_or(json!([])), &resp_text, rstatus).await;
            if !no_training(&ctx.headers) && util::cfg_int(&cfg, "training_log_max", 500) > 0 {
                let m = j
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("message"))
                    .cloned()
                    .unwrap_or(json!({}));
                let txt = m.get("content").map(|c| match c {
                    Value::String(s) => s.clone(),
                    other => convert::flatten_content(other),
                }).unwrap_or_default();
                let rm = m
                    .get("reasoning_content")
                    .filter(|x| util::truthy(x))
                    .or_else(|| m.get("reasoning").filter(|x| util::truthy(x)))
                    .map(|x| util::py_str(x))
                    .unwrap_or_default();
                record_training(&cfg, ep_tag, &model, train_messages(&req), &txt, &rm, Some(&usage)).await;
            }
            let mut resp = out_body.into_response();
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            break 'attempts Some(with_cors(resp));
        }
        last = Some(json!({"status": rstatus, "body": rbody, "error": rerr, "content_type": ""}));
        last_key = Some(key.clone());
        // 401/403 为账号级鉴权失败：立即返回（账号已被硬封禁，重试只会得到 429 封禁掩码）
        if rstatus == 401 || rstatus == 403 {
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, false));
        }
        // 渠道级不可用：同一渠道所有账号共享渠道池，换号/重试都注定失败 → 快速失败
        if convert::is_channel_exhausted(&rbody) {
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, false));
        }
        if !(rstatus == 0 || rstatus == 429 || rstatus >= 500) {
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, false));
        }
        // 429 吸收：换号重试，不透传 429 给下游。账号已在上方统一释放（触发 429 冷却），
        // 这里绝不能再 arelease 一次 —— 否则冷却翻倍、统计/日志/失败计数全部双记
        if rstatus == 429 && rl_left > 0 && cfg.get("queue_enabled").map(util::truthy).unwrap_or(true) {
            rl_left -= 1;
            max_attempts += 1;
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
            continue;
        }
        if attempt < max_attempts {
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
        }
    };
    hold.clear();
    match out {
        Some(resp) => resp,
        None => upstream_fail(last_key.as_ref(), last.as_ref(), &cfg, false),
    }
}

type SendFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send>>;

/// 兜底流：上游响应头迟迟不返回时先提交 200 并持续发心跳，拿到响应头后顺势透传。
/// 代价：响应头一旦发出就不能再改状态码、无法换号重试，上游报错改为流内 error 事件，
/// 但账号仍按真实状态码释放，冷却/封禁逻辑不受影响。
#[allow(clippy::too_many_arguments)]
fn slow_start_response(
    send_fut: SendFut,
    key: Value,
    ctx: Ctx,
    body: String,
    ep_tag: &str,
    model: &str,
    up_model: &str,
    attempt: i64,
    t0: Instant,
    tok: String,
    protocol: Option<String>,
    _stream: bool,
    user: Option<(String, String)>,
    prize_key: Option<String>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let ep2 = ep_tag.to_string();
    let model2 = model.to_string();
    let up_model2 = up_model.to_string();
    let ip2 = ctx.ip.clone();
    tokio::spawn(async move {
        let mut hold = HoldGuard::new(Some(key.clone()), &ep2, &model2, &ip2, &tok, true);
        let mut send_fut = send_fut;
        let deadline = 300.0f64;
        let est_in = body.chars().count() as i64;
        let est_in = est_in / 3 + ((est_in % 3) > 0) as i64;
        let est_usage = json!({"prompt_tokens": est_in, "completion_tokens": 0});

        macro_rules! send_or_die {
            ($b:expr) => {
                if tx.send(Ok($b)).await.is_err() {
                    release_slow(&mut hold, &key, &ep2, &model2, &ip2, &tok, &up_model2, attempt, t0, 499, "客户端已断开", &est_usage).await;
                    return;
                }
            };
        }

        // 进入兜底流时已经静默了 commit_after 秒，立刻先发一个心跳占住连接
        {
            let frame = match &protocol {
                Some(p) => protocol_keepalive(p),
                None => keepalive_frame(&ep2, &model2, true),
            };
            send_or_die!(frame);
        }
        let mut waited = 0.0f64;
        let mut r: Option<reqwest::Response> = None;
        let mut outcome = String::new();
        while r.is_none() {
            match tokio::time::timeout(Duration::from_secs_f64(HB), send_fut.as_mut()).await {
                Err(_) => {
                    waited += HB;
                    if waited >= deadline {
                        outcome = format!("上游首字节超时（{}s）", deadline as i64);
                        break;
                    }
                    let frame = match &protocol {
                        Some(p) => protocol_keepalive(p),
                        None => keepalive_frame(&ep2, &model2, false),
                    };
                    send_or_die!(frame);
                }
                Ok(Err(e)) => {
                    outcome = conn_reason(&e);
                    break;
                }
                Ok(Ok(resp)) => {
                    r = Some(resp);
                    break;
                }
            }
        }
        let Some(resp) = r else {
            // 失败/超时：断连时连接已关闭，无需再发错误帧
            if outcome != "客户端已断开" {
                if protocol.is_none() {
                    send_or_die!(sse_error_event(&outcome));
                } else {
                    send_or_die!(proto_fail_event(&protocol, &model2, &outcome));
                }
            }
            let st = if outcome == "客户端已断开" { 499 } else { 504 };
            release_slow(&mut hold, &key, &ep2, &model2, &ip2, &tok, &up_model2, attempt, t0, st, &outcome, &est_usage).await;
            return;
        };
        let status = resp.status().as_u16() as i64;
        if !(200..400).contains(&status) {
            // 上游报错：读错误体 → 流内报错，并按真实状态码释放（仍触发冷却/封禁）
            let err = match resp.text().await {
                Ok(raw) => util::upstream_snippet(&json!({"status": status, "body": raw, "error": ""})),
                Err(e) => e.to_string(),
            };
            let msg = if err.is_empty() { format!("上游返回 HTTP {}", status) } else { err.clone() };
            if protocol.is_none() {
                send_or_die!(sse_error_event(&msg));
            } else {
                send_or_die!(proto_fail_event(&protocol, &model2, &msg));
            }
            release_slow(&mut hold, &key, &ep2, &model2, &ip2, &tok, &up_model2, attempt, t0, status, &msg, &est_usage).await;
            return;
        }
        // 2xx：交给统一的流式 pump 正常透传（复用改名/心跳/统计/释放逻辑）
        hold.clear();
        let capture_train = !no_training(&ctx.headers);
        let sctx = StreamCtx {
            ep: ep2.clone(),
            ep_tag: ep2.clone(),
            model: model2.clone(),
            up_model: up_model2.clone(),
            body,
            ip: ip2.clone(),
            tok,
            status,
            attempt,
            t0,
            rewrite: up_model2 != model2,
            heartbeat: false,
            ttfb_deadline: deadline,
            protocol,
            capture_train,
            user,
            prize_key,
        };
        run_pump(resp, key, None, sctx, tx).await;
    });
    sse_body_response(rx)
}

async fn release_slow(
    hold: &mut HoldGuard,
    key: &Value,
    ep: &str,
    model: &str,
    ip: &str,
    tok: &str,
    up_model: &str,
    attempt: i64,
    t0: Instant,
    status: i64,
    err: &str,
    usage: &Value,
) {
    hold.clear();
    let ms = t0.elapsed().as_millis() as i64;
    pool::release(
        &util::str_or(key.get("id"), ""),
        false,
        status,
        err,
        Some(&usage.clone()),
        Some(&release_log(ep, model, status, ms, err, attempt, Some(key), ip, up_model, true, ms, 0, 0, tok)),
        0,
    );
}

fn proto_fail_event(protocol: &Option<String>, model: &str, msg: &str) -> Bytes {
    if protocol.as_deref() == Some("messages") {
        let payload = json!({"type": "error", "error": {"type": "api_error", "message": msg}});
        Bytes::from(format!("event: error\ndata: {}\n\n", py_json(&payload)))
    } else {
        // Responses 的 response.failed 事件体是完整 Response 对象（SDK 按 schema 校验）
        let resp = json!({
            "id": util::rand_id("resp_"),
            "object": "response",
            "created_at": util::now_i(),
            "status": "failed",
            "error": {"code": "upstream_error", "message": msg},
            "incomplete_details": null,
            "model": model,
            "output": [],
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": {"effort": null, "summary": null},
            "temperature": null,
            "top_p": null,
            "max_output_tokens": null,
            "tools": [],
            "tool_choice": "auto",
            "usage": convert::map_usage(Some(&json!({}))),
            "metadata": {},
        });
        let payload = json!({"type": "response.failed", "response": resp});
        Bytes::from(format!("event: response.failed\ndata: {}\n\n", py_json(&payload)))
    }
}

// ---------------------------------------------------------------- 协议转换端点（/v1/responses、/v1/messages）

#[allow(clippy::too_many_arguments)]
pub async fn proxy_convert(ctx: Ctx, protocol: &str, anthropic: bool, body_bytes: Bytes) -> Response {
    let ep = if protocol == "responses" { "resp" } else { "msg" };
    let cfg = cfg_all();
    let entry = token_entry(&ctx, &cfg);
    let mut prize_ctx: Option<PrizeCtx> = None;
    let mut user_ctx: Option<UserCtx> = None;
    if let Some(b) = ctx.bearer() {
        if b.starts_with(crate::users::user_key_prefix()) {
            match crate::users::lookup_user_key(&b) {
                Some((u, t)) => {
                    user_ctx = Some(UserCtx {
                        id: util::str_or(u.get("id"), ""),
                        key_id: util::str_or(t.get("id"), ""),
                        kind: crate::users::key_kind(&t),
                    });
                }
                None => {
                    if anthropic {
                        return error_resp(401, "invalid x-api-key", "invalid_request_error", None, true);
                    }
                    return error_resp(
                        401,
                        "调用 Key 无效或已被停用",
                        "invalid_request_error",
                        Some("invalid_api_key"),
                        false,
                    );
                }
            }
        } else if b.starts_with(crate::wheel::PRIZE_KEY_PREFIX) {
            match crate::wheel::lookup_prize_key(&b) {
                Ok(row) => {
                    prize_ctx = Some(PrizeCtx {
                        key: b.clone(),
                        model: util::str_or(row.get("model"), ""),
                        concurrency: util::int_or(row.get("concurrency"), 1),
                        metered: util::f64_or(row.get("quota"), 0.0) > 0.0,
                    });
                }
                Err(e) => {
                    let msg = if anthropic { "invalid x-api-key" } else { e };
                    return error_resp(401, msg, "invalid_request_error", Some("invalid_api_key"), anthropic);
                }
            }
        }
    }
    if user_ctx.is_none() && prize_ctx.is_none() && has_auth(&cfg) && entry.is_none() {
        if anthropic {
            return error_resp(401, "invalid x-api-key", "invalid_request_error", None, true);
        }
        return error_resp(
            401,
            "访问令牌无效。请在后台「系统设置」中配置访问令牌，并以 Authorization: Bearer <令牌> 调用。",
            "invalid_request_error",
            None,
            false,
        );
    }
    let body_text = String::from_utf8_lossy(&body_bytes).trim_start_matches('\u{feff}').to_string();
    if body_text.len() > MAX_BODY {
        return error_resp(413, "请求体过大，上限 20MB", "invalid_request_error", None, anthropic);
    }
    let Ok(req0) = serde_json::from_str::<Value>(&body_text) else {
        return error_resp(400, "请求体不是合法 JSON", "invalid_request_error", None, anthropic);
    };
    if !req0.is_object() {
        return error_resp(400, "请求体不是合法 JSON", "invalid_request_error", None, anthropic);
    }
    let mut req = req0.clone();
    let chat_req_v = if anthropic {
        convert::anthropic_to_chat(&req)
    } else {
        convert::responses_to_chat(&req)
    };
    let Ok(mut chat_req) = chat_req_v else {
        return error_resp(400, &chat_req_v.unwrap_err(), "invalid_request_error", None, anthropic);
    };
    let model = util::str_or(req.get("model"), "");
    let stream = req.get("stream").map(util::truthy).unwrap_or(false);
    let est = util::estimate_request_tokens(&body_text, Some(&req));
    let reasoning_effort = {
        let sub = req
            .get("reasoning")
            .and_then(|r| r.as_object())
            .and_then(|r| r.get("effort"))
            .filter(|x| util::truthy(x));
        match sub {
            Some(s) => s.clone(),
            None => match req.get("reasoning") {
                Some(Value::String(s)) => Value::from(s.clone()),
                _ => Value::Null,
            },
        }
    };
    let meta = json!({
        "temperature": req.get("temperature").cloned().unwrap_or(Value::Null),
        "top_p": req.get("top_p").cloned().unwrap_or(Value::Null),
        "max_output_tokens": req.get("max_output_tokens").cloned().unwrap_or(Value::Null),
        "reasoning_effort": reasoning_effort,
    });
    if let Some(bad) = check_model(&model, entry.as_ref(), &cfg, anthropic) {
        return bad;
    }
    // 奖品 Key：模型锁定 + 并发占位
    let mut prize_guard: Option<PrizeGuard> = None;
    if let Some(pc) = &prize_ctx {
        let prize_ok = crate::wheel::lookup_prize_key(&pc.key)
            .map(|row| crate::wheel::prize_key_allows_model(&row, &model))
            .unwrap_or(false);
        if !prize_ok {
            return error_resp(
                403,
                &format!("该奖品 Key 仅限模型 {}，本次请求模型 {}", util::char_prefix(&pc.model, 80), util::char_prefix(&model, 80)),
                "invalid_request_error",
                Some("prize_model_not_allowed"),
                anthropic,
            );
        }
        if !crate::wheel::prize_key_acquire(&pc.key, pc.concurrency) {
            return error_resp(
                429,
                &format!("该奖品 Key 并发已满（上限 {}），请稍后重试", pc.concurrency),
                "rate_limit_error",
                None,
                anthropic,
            );
        }
        prize_guard = Some(PrizeGuard { key: pc.key.clone() });
    }
    if let Some(uc) = &user_ctx {
        let Some(u) = crate::users::auth_user(&uc.id) else {
            return error_resp(401, "用户已被停用", "invalid_request_error", None, anthropic);
        };
        let paid = crate::users::model_is_paid(&model);
        let kind_name = if uc.kind == "free" { "免费" } else { "付费" };
        let allowed = match uc.kind {
            "free" => !paid,
            "paid" => paid,
            _ => true,
        };
        if !allowed {
            return error_resp(
                403,
                &format!("该 Key 仅限调用{}模型（模型 {} 不在允许范围内）", kind_name, util::char_prefix(&model, 80)),
                "invalid_request_error",
                Some("key_model_not_allowed"),
                anthropic,
            );
        }
        if paid && money_of(&u) <= 0.0 {
            return error_resp(
                402,
                "余额不足，无法调用付费模型（免费模型不受影响）",
                "insufficient_quota",
                Some("insufficient_quota"),
                anthropic,
            );
        }
        let rpm = if paid {
            util::int_or(u.get("paid_rpm"), 0)
        } else {
            util::int_or(u.get("free_rpm"), 0)
        };
        if let Err(wait) = crate::users::check_user_rpm(&uc.id, if paid { "paid" } else { "free" }, rpm) {
            return error_resp(
                429,
                &format!("调用过于频繁（每分钟 {} 次上限），约 {} 秒后可重试", rpm, wait.ceil() as i64),
                "rate_limit_error",
                None,
                anthropic,
            );
        }
    }
    // 拦截：用规范化后的 chat 请求匹配（input/content 块数组只有 chat 形态是统一的）
    if let Some((rule, content)) = match_custom_rule(&chat_req, &cfg, false, &model) {
        let tok0 = util::str_or(entry.as_ref().and_then(|e| e.get("t")), "");
        log_intercept(&cfg, ep, &rule, &content, &ctx.ip, &tok0, &model).await;
        return custom_reply_response(
            &util::str_or(rule.get("reply"), ""),
            &model,
            stream,
            if anthropic { "anthropic" } else { "responses" },
            &meta,
        );
    }
    let mut max_attempts = util::cfg_int(&cfg, "max_retries", 3).max(1) + 1;
    let mut attempt: i64 = 0;
    let mut last: Option<Value> = None;
    let mut last_key: Option<Value> = None;
    let mut downgraded = false;
    let mut reuse_key: Option<Value> = None;
    let mut same_key_tried: HashSet<String> = HashSet::new();
    let mut rl_left = util::cfg_int(&cfg, "max_retries", 2).max(1);
    let mut up_model = model.clone();
    let tok = util::str_or(entry.as_ref().and_then(|e| e.get("t")), "");
    let mut hold = HoldGuard::new(None, ep, &model, &ctx.ip, &tok, stream);

    let out = 'attempts: loop {
        if attempt >= max_attempts {
            break 'attempts None;
        }
        attempt += 1;
        let taken = if let Some(k) = reuse_key.take() {
            Taken { ok: true, key: Some(k), status: 0, message: String::new() }
        } else {
            take_account(ep, &model, est, &cfg, &tok, &ctx.ip).await
        };
        if !taken.ok {
            pool::arelease(
                String::new(),
                false,
                taken.status,
                taken.message.clone(),
                None,
                Some(release_log(ep, &model, taken.status, 0, &taken.message, attempt, None, &ctx.ip, &model, stream, 0, 0, 0, &tok)),
                0,
            )
            .await;
            break 'attempts Some(error_resp(taken.status.clamp(400, 599) as u16, &taken.message, "invalid_request_error", None, anthropic));
        }
        let key = taken.key.unwrap();
        hold.hold(&key);
        max_attempts = attempt.max(
            upstreams::override_for(&key, "max_retries", util::cfg_int(&cfg, "max_retries", 3)).max(1) + 1,
        );
        up_model = upstreams::map_model_for(&key, &model);
        chat_req["model"] = Value::from(up_model.clone());
        upstreams::apply_param_overrides(&key, &model, &mut chat_req);
        convert::normalize_body_types(&mut chat_req);
        convert::sanitize_request(&mut chat_req);
        let raw = serde_json::to_string(&chat_req).unwrap_or_default();
        let t0 = Instant::now();
        let mut rerr = String::new();
        let mut rstatus: i64 = 0;
        let mut rbody = String::new();
        let mut got_resp: Option<reqwest::Response> = None;
        let mut slow_fut: Option<SendFut> = None;
        {
            let verify_tls = cfg.get("verify_tls").map(util::truthy).unwrap_or(true);
            let connect_to = upstreams::override_for(&key, "connect_timeout", util::cfg_int(&cfg, "connect_timeout", 10));
            let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg, "request_timeout", 300));
            let client = get_http(verify_tls, connect_to);
            let url = format!("{}/chat/completions", upstreams::base_for(&key));
            let fut = client
                .post(&url)
                .header("Accept", if stream { "text/event-stream" } else { "application/json" })
                .header("Authorization", format!("Bearer {}", util::str_or(key.get("apikey"), "")))
                .header("Content-Type", "application/json")
                .body(raw.clone());
            let built = fut.build();
            if let Err(e) = &built {
                rerr = conn_reason(e);
                rstatus = 0;
            }
            if stream && built.is_ok() {
                let ttfb_cfg = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
                let commit_after = if ttfb_cfg <= 0.0 { 12.0 } else { 12.0_f64.min(ttfb_cfg) };
                let fut2 = client.execute(built.unwrap());
                let mut pinned = Box::pin(fut2);
                let mut waited = 0.0;
                while waited < commit_after {
                    match tokio::time::timeout(Duration::from_secs_f64(0.5), pinned.as_mut()).await {
                        Ok(Ok(r)) => {
                            got_resp = Some(r);
                            break;
                        }
                        Ok(Err(e)) => {
                            rerr = conn_reason(&e);
                            rstatus = 0;
                            break;
                        }
                        Err(_) => {
                            waited += 0.5;
                            if waited >= commit_after {
                                slow_fut = Some(pinned);
                                break;
                            }
                        }
                    }
                }
            } else if built.is_ok() {
                match tokio::time::timeout(
                    Duration::from_secs(read_to.max(1) as u64),
                    client.execute(built.unwrap()),
                )
                .await
                {
                    Ok(Ok(r)) => got_resp = Some(r),
                    Ok(Err(e)) => {
                        rerr = conn_reason(&e);
                        rstatus = 0;
                    }
                    Err(_) => {
                        rerr = format!("上游 {} 秒内未返回", read_to);
                        rstatus = 0;
                    }
                }
            }
        }
        if got_resp.is_none() && slow_fut.is_some() {
            let fut = slow_fut.take().unwrap();
            hold.handoff();
            break 'attempts Some(slow_start_response(
                fut, key, ctx, raw, ep, &model, &up_model, attempt, t0, tok,
                Some(protocol.to_string()), stream,
                user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                prize_ctx.as_ref().map(|p| p.key.clone()),
            ));
        }
        if rstatus == 0 && !rerr.is_empty() && got_resp.is_none() {
            let ms = t0.elapsed().as_millis() as i64;
            let err = util::upstream_snippet(&json!({"status": 0, "body": "", "error": rerr}));
            let usage = build_usage(&json!({"status": 0, "body": "", "streamed": false, "out_bytes": 0}), &raw);
            if !same_key_tried.contains(&util::str_or(key.get("id"), "")) && attempt < max_attempts {
                same_key_tried.insert(util::str_or(key.get("id"), ""));
                reuse_key = Some(key.clone());
                tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                continue;
            }
            hold.clear();
            pool::arelease(
                util::str_or(key.get("id"), ""),
                false,
                0,
                err.clone(),
                Some(usage),
                Some(release_log(ep, &model, 0, ms, &err, attempt, Some(&key), &ctx.ip, &up_model, stream, ms, 0, 0, &tok)),
                0,
            )
            .await;
            last = Some(json!({"status": 0, "body": "", "error": rerr}));
            last_key = Some(key);
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, last_key.as_ref()) as u64)).await;
            continue;
        }
        let mut r = got_resp.unwrap();
        rstatus = r.status().as_u16() as i64;
        if stream && (200..400).contains(&rstatus) {
            let ttfb_to = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
            let pre_to = if ttfb_to <= 0.0 { 8.0 } else { 8.0_f64.min(ttfb_to) };
            let mut first_chunk: Option<Bytes> = None;
            let mut pre_timed_out = false;
            let pre_deadline = Instant::now() + Duration::from_secs_f64(pre_to);
            loop {
                let left = pre_deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    pre_timed_out = true;
                    break;
                }
                match tokio::time::timeout(left.min(Duration::from_millis(500)), r.chunk()).await {
                    Err(_) => continue,
                    Ok(Err(e)) => {
                        rerr = conn_reason(&e);
                        rstatus = 0;
                        break;
                    }
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(c))) => {
                        first_chunk = Some(c);
                        break;
                    }
                }
            }
            if rstatus == 0 {
                // 预读阶段连接异常：r 交给后面的统一处理
            } else if pre_timed_out {
                hold.handoff();
                break 'attempts Some(spawn_pump(
                    r,
                    key,
                    None,
                    StreamCtx {
                        ep: ep.to_string(),
                        ep_tag: ep.to_string(),
                        model: model.clone(),
                        up_model: up_model.clone(),
                        body: raw.clone(),
                        ip: ctx.ip.clone(),
                        tok: tok.clone(),
                        status: rstatus,
                        attempt,
                        t0,
                        rewrite: false,
                        heartbeat: true,
                        ttfb_deadline: ttfb_to,
                        protocol: Some(protocol.to_string()),
                        capture_train: !no_training(&ctx.headers)
                            && util::cfg_int(&cfg, "training_log_max", 500) > 0,
                        user: user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                        prize_key: prize_ctx.as_ref().map(|p| p.key.clone()),
                    },
                ));
            } else {
                let text = first_chunk
                    .as_ref()
                    .map(|c| String::from_utf8_lossy(c).to_string())
                    .unwrap_or_default();
                let head = util::char_prefix(&text, 500);
                let is_sse_error = text.trim_start().starts_with("event: error")
                    || text.trim_start().starts_with("data: {\"error\"")
                    || (head.contains("\"error\"")
                        && ["thinking", "unsupported", "duplicate"].iter().any(|k| text.to_lowercase().contains(k)));
                if is_sse_error && !downgraded {
                    downgraded = true;
                    rstatus = 400;
                    rbody = text.clone();
                    if convert::is_duplicate_field_error(&text, 400)
                        && convert::strip_reasoning_from_messages(&mut chat_req)
                    {
                        drop(r);
                        reuse_key = Some(key);
                        continue 'attempts;
                    }
                    let tdefs = convert::parse_thinking_defaults(&upstreams::upstream_value(&key, "thinking_defaults", ""));
                    if convert::downgrade_thinking(&mut chat_req, &up_model, &tdefs) {
                        drop(r);
                        reuse_key = Some(key);
                        continue 'attempts;
                    }
                } else if text.trim().is_empty() {
                    // 空流保护：上游 200 但流为空 → 标错误走重试，不透传空流
                    rstatus = 502;
                    rerr = "上游返回空流".into();
                } else {
                    hold.handoff();
                    break 'attempts Some(spawn_pump(
                        r,
                        key,
                        first_chunk,
                        StreamCtx {
                            ep: ep.to_string(),
                            ep_tag: ep.to_string(),
                            model: model.clone(),
                            up_model: up_model.clone(),
                            body: raw.clone(),
                            ip: ctx.ip.clone(),
                            tok: tok.clone(),
                            status: rstatus,
                            attempt,
                            t0,
                            rewrite: false,
                            heartbeat: false,
                            ttfb_deadline: 0.0,
                            protocol: Some(protocol.to_string()),
                            capture_train: !no_training(&ctx.headers)
                                && util::cfg_int(&cfg, "training_log_max", 500) > 0,
                            user: user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                            prize_key: prize_ctx.as_ref().map(|p| p.key.clone()),
                        },
                    ));
                }
            }
        }
        if stream {
            drop(r);
        } else {
            match r.text().await {
                Ok(t) => rbody = t,
                Err(e) => {
                    rerr = conn_reason(&e);
                    rstatus = 0;
                }
            }
        }
        let ms = t0.elapsed().as_millis() as i64;
        let mut success = (200..400).contains(&rstatus) && rstatus != 0;
        if success {
            let st = rbody.trim();
            if st.is_empty() || st == "{}" {
                success = false;
                rerr = "上游返回空响应".into();
                rstatus = 502;
            } else if st.starts_with('{') && util::char_prefix(st, 200).contains("\"error\"") {
                if let Ok(j) = serde_json::from_str::<Value>(st) {
                    if j.get("error").map(|e| util::truthy(e)).unwrap_or(false) {
                        success = false;
                        rerr = util::upstream_snippet(&json!({"status": 200, "body": st, "error": ""}));
                        rstatus = 502;
                    }
                }
            }
        }
        let err = if success {
            String::new()
        } else {
            util::upstream_snippet(&json!({"status": rstatus, "body": rbody, "error": rerr}))
        };
        if !success
            && !same_key_tried.contains(&util::str_or(key.get("id"), ""))
            && (rstatus == 0 || rstatus >= 500)
            && attempt < max_attempts
        {
            same_key_tried.insert(util::str_or(key.get("id"), ""));
            reuse_key = Some(key.clone());
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
            continue;
        }
        // 400 参数类降级重试：必须在释放之前 continue —— 账号保持持有（INFLIGHT 计数正确）
        if rstatus == 400 && convert::is_duplicate_field_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::strip_reasoning_from_messages(&mut chat_req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && convert::thinking_unsupported(&rbody, rstatus) && !downgraded {
            downgraded = true;
            let tdefs = convert::parse_thinking_defaults(&upstreams::upstream_value(&key, "thinking_defaults", ""));
            if convert::downgrade_thinking(&mut chat_req, &up_model, &tdefs) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && convert::is_deserialize_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::coerce_all_types(&mut chat_req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && convert::is_unsupported_param_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::strip_unsupported_params(&mut chat_req, &rbody) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        let usage = build_usage(
            &json!({"status": rstatus, "body": rbody, "streamed": false, "out_bytes": 0, "usage": null}),
            &raw,
        );
        hold.clear();
        pool::arelease(
            util::str_or(key.get("id"), ""),
            success,
            rstatus,
            err.clone(),
            Some(usage.clone()),
            Some(release_log(
                ep, &model, rstatus, ms, &err, attempt, Some(&key), &ctx.ip, &up_model, stream, ms,
                util::int_or(usage.get("prompt_tokens"), 0),
                util::int_or(usage.get("completion_tokens"), 0),
                &tok,
            )),
            0,
        )
        .await;
        if success {
            if let Some((uid, kid)) = user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())) {
                crate::users::bill(
                    &uid, &kid,
                    &util::str_or(key.get("upstream_id"), ""),
                    &model, &up_model,
                    rstatus, ms,
                    util::int_or(usage.get("prompt_tokens"), 0),
                    util::int_or(usage.get("completion_tokens"), 0),
                    false,
                );
            }
            // 奖品 Key：专属额度计次（仅成功调用）
            if let Some(pc) = &prize_ctx {
                if pc.metered {
                    crate::wheel::prize_key_consume(&pc.key);
                }
            }
        }
        if success {
            let mut chat: Value = serde_json::from_str(&rbody).unwrap_or(Value::Null);
            if !chat.is_object() || chat.get("choices").and_then(|c| c.as_array()).is_none() {
                break 'attempts Some(error_resp(502, "上游返回了无法解析的响应", "upstream_error", None, anthropic));
            }
            chat["model"] = Value::from(model.clone());
            // 会话审计
            {
                let first_msg = chat
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("message"))
                    .cloned()
                    .unwrap_or(json!({}));
                let resp_text = util::str_or(first_msg.get("content"), "");
                let req_msgs = if let Some(Value::String(s)) = req.get("input") {
                    json!([{"role": "user", "content": s}])
                } else if req.get("messages").and_then(|m| m.as_array()).is_some() {
                    req.get("messages").cloned().unwrap_or(json!([]))
                } else if let Some(arr) = req.get("input").and_then(|i| i.as_array()) {
                    Value::Array(
                        arr.iter()
                            .filter_map(|i| {
                                let o = i.as_object()?;
                                Some(json!({
                                    "role": util::str_or(o.get("role"), "user"),
                                    "content": convert::flatten_content(o.get("content").unwrap_or(&Value::Null)),
                                }))
                            })
                            .collect(),
                    )
                } else {
                    json!([{"role": "user", "content": util::str_or(req.get("input"), "")}])
                };
                log_session(&cfg, &model, &util::str_or(key.get("email"), ""), &req_msgs, &resp_text, 200).await;
                if !no_training(&ctx.headers) && util::cfg_int(&cfg, "training_log_max", 500) > 0 {
                    let txt = match first_msg.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => convert::flatten_content(other),
                        None => String::new(),
                    };
                    let rm = first_msg
                        .get("reasoning_content")
                        .filter(|x| util::truthy(x))
                        .or_else(|| first_msg.get("reasoning").filter(|x| util::truthy(x)))
                        .map(|x| util::py_str(x))
                        .unwrap_or_default();
                    record_training(&cfg, ep, &model, train_messages(&chat_req), &txt, &rm, Some(&usage)).await;
                }
            }
            if anthropic {
                break 'attempts Some(json_resp_raw(&convert::chat_to_anthropic(&chat)));
            }
            break 'attempts Some(json_resp_raw(&convert::chat_to_responses(&chat, &meta)));
        }
        last = Some(json!({"status": rstatus, "body": rbody, "error": rerr, "content_type": ""}));
        last_key = Some(key.clone());
        if rstatus == 401 || rstatus == 403 {
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, anthropic));
        }
        if convert::is_channel_exhausted(&rbody) {
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, anthropic));
        }
        if !(rstatus == 0 || rstatus == 429 || rstatus >= 500) {
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, anthropic));
        }
        // 429 吸收：账号已在上方统一释放（触发 429 冷却），这里不能再 arelease 一次
        if rstatus == 429 && rl_left > 0 && cfg.get("queue_enabled").map(util::truthy).unwrap_or(true) {
            rl_left -= 1;
            max_attempts += 1;
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
            continue;
        }
        if attempt < max_attempts {
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
        }
    };
    hold.clear();
    match out {
        Some(resp) => resp,
        None => upstream_fail(last_key.as_ref(), last.as_ref(), &cfg, anthropic),
    }
}

// ---------------------------------------------------------------- /v1/models 等

pub fn gateway_model_ids() -> Vec<String> {
    let names = upstreams::curated_models();
    if !names.is_empty() {
        return names;
    }
    let mut aliases: Vec<String> = Vec::new();
    for u in upstreams::all_upstreams() {
        if !u.get("enabled").map(util::truthy).unwrap_or(false) {
            continue;
        }
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for k in mm.keys() {
                if !aliases.contains(k) {
                    aliases.push(k.clone());
                }
            }
        }
    }
    if aliases.is_empty() {
        FALLBACK_MODELS.iter().map(|s| s.to_string()).collect()
    } else {
        aliases
    }
}

pub async fn v1_models(ctx: Ctx) -> Response {
    let cfg = cfg_all();
    let entry = token_entry(&ctx, &cfg);
    // 用户 Key：按 Key 类型过滤可见模型（免费 Key 只见免费模型，付费 Key 只见付费模型）
    let mut user_kind: Option<&'static str> = None;
    // 奖品 Key：只见锁定的单个模型
    let mut prize_model: Option<String> = None;
    if let Some(b) = ctx.bearer() {
        if b.starts_with(crate::users::user_key_prefix()) {
            match crate::users::lookup_user_key(&b) {
                Some((_u, t)) => user_kind = Some(crate::users::key_kind(&t)),
                None => {
                    return error_resp(401, "调用 Key 无效或已被停用", "invalid_request_error", Some("invalid_api_key"), false);
                }
            }
        } else if b.starts_with(crate::wheel::PRIZE_KEY_PREFIX) {
            match crate::wheel::lookup_prize_key(&b) {
                Ok(row) => prize_model = Some(util::str_or(row.get("model"), "")),
                Err(e) => {
                    return error_resp(401, e, "invalid_request_error", Some("invalid_api_key"), false);
                }
            }
        }
    }
    if user_kind.is_none() && prize_model.is_none() && has_auth(&cfg) && entry.is_none() {
        return error_resp(401, "访问令牌无效", "invalid_request_error", Some("invalid_api_key"), false);
    }
    let out: Vec<Value> = gateway_model_ids()
        .into_iter()
        .filter(|m| model_allowed(m, entry.as_ref(), &cfg))
        .filter(|m| match &prize_model {
            Some(pm) => m == pm,
            None => true,
        })
        .filter(|m| match user_kind {
            Some("free") => !crate::users::model_is_paid(m),
            Some("paid") => crate::users::model_is_paid(m),
            _ => true,
        })
        .map(|m| {
            json!({"id": m, "object": "model", "created": 0, "owned_by": "gateway", "permission": []})
        })
        .collect();
    json_resp(json!({"object": "list", "data": out}))
}

pub async fn v1_model_retrieve(ctx: Ctx, model_id: String) -> Response {
    let cfg = cfg_all();
    let entry = token_entry(&ctx, &cfg);
    let mut user_kind: Option<&'static str> = None;
    let mut prize_model: Option<String> = None;
    if let Some(b) = ctx.bearer() {
        if b.starts_with(crate::users::user_key_prefix()) {
            match crate::users::lookup_user_key(&b) {
                Some((_u, t)) => user_kind = Some(crate::users::key_kind(&t)),
                None => {
                    return error_resp(401, "调用 Key 无效或已被停用", "invalid_request_error", Some("invalid_api_key"), false);
                }
            }
        } else if b.starts_with(crate::wheel::PRIZE_KEY_PREFIX) {
            match crate::wheel::lookup_prize_key(&b) {
                Ok(row) => prize_model = Some(util::str_or(row.get("model"), "")),
                Err(e) => {
                    return error_resp(401, e, "invalid_request_error", Some("invalid_api_key"), false);
                }
            }
        }
    }
    if user_kind.is_none() && prize_model.is_none() && has_auth(&cfg) && entry.is_none() {
        return error_resp(401, "访问令牌无效", "invalid_request_error", Some("invalid_api_key"), false);
    }
    if prize_model.is_some() && prize_model.as_deref() != Some(model_id.as_str()) {
        return error_resp(
            404,
            &format!("The model '{}' does not exist", model_id),
            "invalid_request_error",
            Some("model_not_found"),
            false,
        );
    }
    if user_kind.is_some() && !match user_kind {
        Some("free") => !crate::users::model_is_paid(&model_id),
        Some("paid") => crate::users::model_is_paid(&model_id),
        _ => true,
    } {
        return error_resp(
            404,
            &format!("The model '{}' does not exist", model_id),
            "invalid_request_error",
            Some("model_not_found"),
            false,
        );
    }
    if check_model(&model_id, entry.as_ref(), &cfg, false).is_some() {
        return error_resp(
            404,
            &format!("The model '{}' does not exist", model_id),
            "invalid_request_error",
            Some("model_not_found"),
            false,
        );
    }
    // 只有渠道显式配置了模型白名单时才能判定「不支持」；未配置时网关支持任意模型名
    let known = upstreams::curated_models();
    if !known.is_empty() && !known.iter().any(|m| m == &model_id) {
        return error_resp(
            404,
            &format!("The model '{}' does not exist", model_id),
            "invalid_request_error",
            Some("model_not_found"),
            false,
        );
    }
    json_resp(json!({"id": model_id, "object": "model", "created": 0, "owned_by": "gateway"}))
}

