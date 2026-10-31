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
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;

const REGEX_SCAN_MAX: usize = 8000;
// 心跳间隔：必须远小于中间代理空闲超时（nginx 默认 60s、EdgeOne 实测约 10s）。
// 5s 而不是 10s：腾讯 EdgeOne 对「10s 内无任何响应字节」直接返回 520，
// 10s 心跳刚好卡在边界上，ZCode 流式请求会间歇性 520。
const HB: f64 = 5.0;
const QUEUE_POLL_MAX: f64 = 3.0;
const QUEUE_MAX_WAITING: i64 = 200;
static WAITING: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

fn waiting_try_add(model: &str) -> bool {
    let mut g = WAITING.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    let count = m.entry(model.to_string()).or_insert(0);
    if *count >= QUEUE_MAX_WAITING { return false; }
    *count += 1;
    true
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

/// 「该渠道 + 上游模型不接受思考参数」学习表：任一 400 自愈（剥离思考参数）
/// 重试成功后记住，后续请求发送前直接剥离。否则 ZCode 每轮都注入
/// enable_thinking/reasoning，每个请求都要先吃一次 400 再重试（m3.1-flash 实测）。
/// 键 = upstream_id \0 上游模型名；值 = 过期时间（epoch 秒）。
/// 只记成功样本，6 小时后自然过期，避免一次误判永久削掉思考能力。
static THINKING_STRIP_LEARNED: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
const THINKING_LEARN_TTL: i64 = 6 * 3600;
const THINKING_LEARN_MAX: usize = 512;

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn thinking_key(key: &Value, up_model: &str) -> String {
    format!("{}\u{0}{}", util::str_or(key.get("upstream_id"), ""), up_model)
}

fn thinking_strip_learned(key: &Value, up_model: &str) -> bool {
    let mut g = THINKING_STRIP_LEARNED.lock().unwrap();
    let Some(m) = g.as_mut() else { return false };
    let k = thinking_key(key, up_model);
    let now = epoch_secs();
    match m.get(&k) {
        Some(exp) if *exp > now => true,
        Some(_) => {
            m.remove(&k);
            false
        }
        None => false,
    }
}

fn learn_thinking_strip(composed: &str) {
    let mut g = THINKING_STRIP_LEARNED.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    let now = epoch_secs();
    m.retain(|_, exp| *exp > now);
    if m.len() >= THINKING_LEARN_MAX {
        if let Some(k) = m.keys().next().cloned() {
            m.remove(&k);
        }
    }
    m.insert(composed.to_string(), now + THINKING_LEARN_TTL);
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
            if auth.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer ")) {
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
    let p = util::int_or(out.get("prompt_tokens"), 0).max(0);
    let c = util::int_or(out.get("completion_tokens"), 0).max(0);
    out["prompt_tokens"] = Value::from(p);
    out["completion_tokens"] = Value::from(c);
    if out.get("total_tokens").map(|x| x.is_null()).unwrap_or(true) {
        out["total_tokens"] = Value::from(p.saturating_add(c));
    }
    out
}

fn usage_has_prompt(u: Option<&Value>) -> bool {
    u.and_then(|u| u.as_object())
        .map(|u| u.contains_key("prompt_tokens") && !u.get("prompt_tokens").map(|x| x.is_null()).unwrap_or(true))
        .unwrap_or(false)
}

fn stream_value(value: Option<&Value>) -> Result<bool, String> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err("stream 必须为布尔值".into()),
        },
        _ => Err("stream 必须为布尔值".into()),
    }
}

fn validate_json_reply(body: &str, embeddings: bool) -> Result<(), String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "上游返回无效 JSON 响应".to_string())?;
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(error.get("message").and_then(Value::as_str)
            .unwrap_or("上游返回错误响应").to_string());
    }
    let field = if embeddings { "data" } else { "choices" };
    if !value.get(field).and_then(Value::as_array).is_some_and(|rows| !rows.is_empty()) {
        return Err(format!("上游响应缺少有效 {}", field));
    }
    let rows = value[field].as_array().unwrap();
    if !embeddings && rows.iter().any(|choice| {
        !choice.is_object() || !(choice.get("text").is_some_and(Value::is_string)
            || choice.get("message").is_some_and(|message| message.is_object()
                && (message.get("content").is_some() || message.get("tool_calls").is_some())))
    }) {
        return Err("上游 choices 缺少有效的 message 或 text".into());
    }
    Ok(())
}

/// 非流式回复的正文/思考是否泄漏模板特殊 token（K3 词沙拉指纹）。
/// 只查内容字段，不碰 JSON 结构；Embeddings 无正文，直接放行。
fn reply_token_leak(body: &str, embeddings: bool) -> Option<String> {
    if embeddings {
        return None;
    }
    let value: Value = serde_json::from_str(body).ok()?;
    let mut text = String::new();
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let Some(msg) = choice.get("message") else { continue };
            if let Some(content) = msg.get("content") {
                text.push_str(&match content {
                    Value::String(s) => s.clone(),
                    other => convert::flatten_content(other),
                });
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(reasoning) = msg.get(key).and_then(Value::as_str) {
                    text.push_str(reasoning);
                }
            }
        }
    }
    if util::has_token_leak(&text) {
        Some("上游输出乱码（特殊 token 泄漏），已丢弃".into())
    } else {
        None
    }
}

/// 从单个流式事件里取正文/思考文本（只用于特殊 token 泄漏嗅探）。
fn frame_content_text(value: &Value) -> String {
    let mut text = String::new();
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            for pointer in ["/delta/content", "/delta/reasoning_content", "/delta/reasoning", "/text"] {
                if let Some(part) = choice.pointer(pointer).and_then(Value::as_str) {
                    text.push_str(part);
                }
            }
        }
    }
    text
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

/// 直通流的保活帧：必须是合法 data: 空 chunk，不能用 SSE 注释行 ——
/// New-API 一类中转只转发 data: 行，注释行会被丢弃，下游仍会静默超时。
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

/// 合成 usage 尾帧：choices 必须显式为空数组（ZCode 的 chunk schema 硬要求）。
fn synth_usage_frame(prompt_tokens: i64, completion_tokens: i64, created: i64) -> Bytes {
    let u = full_usage(Some(&json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
    })));
    let frame = json!({
        "id": "chatcmpl-gw-usage",
        "object": "chat.completion.chunk",
        "created": created,
        "choices": [],
        "usage": u,
    });
    Bytes::from(format!("data: {}\n\n", py_json(&frame)))
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
    let br = pool::breaker_open(&pool::capability_key(model, ep));
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
        let a = pool::acquire(est_tokens, model, ep);
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
        let global_hide = db
            .get("config")
            .map(|c| c.get("hide_mapped_names").map(util::truthy).unwrap_or(true))
            .unwrap_or(true);
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
                if let Some(u) = up {
                    if upstreams::hidden_target(u, model, global_hide) {
                        continue;
                    }
                }
                let models = up.and_then(|u| u.get("models").and_then(|m| m.as_array()));
                if let Some(ms) = models {
                    if !ms.is_empty() && !ms.iter().any(|m| util::str_or(Some(m), "") == model) {
                        continue;
                    }
                }
                if pool::model_missing_fresh(&db, &uid, &pool::capability_key(model, ep)) {
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
    if !waiting_try_add(model) {
        return Taken {
            ok: false,
            key: None,
            status: 503,
            message: format!("排队已满（{} 个请求在等账号），请稍后重试", QUEUE_MAX_WAITING),
        };
    }
    let mut q_reason = acq_reason.clone();
    if let Some(b) = &br {
        q_reason = breaker_queue_reason(b);
    }
    let qid = queue::add(ep, model, ip, &tok_mask(tok), &q_reason);
    let _qguard = QueueGuard { qid: qid.clone(), model: model.to_string() };
    let deadline = Instant::now() + Duration::from_secs_f64(max_wait as f64);
    let poll = (util::cfg_int(cfg, "queue_poll_ms", 400) as f64 / 1000.0).max(0.05);
    let mut waiting_breaker = br.is_some();
    let mut backoff = poll;
    let mut last_reason = acq_reason.clone();
    let mut got: Option<Value> = None;
    while Instant::now() < deadline {
        let hint;
        if !waiting_breaker || pool::breaker_open(model).is_none() {
            waiting_breaker = false;
            let a = pool::acquire(est_tokens, model, ep);
            if a.result == "ok" {
                let _ = hint; // 命中即出队：本迭代的等待提示不再被消费
                got = a.key;
                break;
            }
            queue::set_reason(&qid, &a.reason);
            hint = a.wait_hint;
            last_reason = a.reason;
            let crowded = waiting_get(model) > 4;
            backoff = if hint > 0.0 || !crowded { poll } else { (backoff * 2.0).min(QUEUE_POLL_MAX) };
        } else {
            let b = pool::breaker_open(&pool::capability_key(model, ep)).unwrap_or(json!({}));
            hint = util::f64_or(b.get("left"), 0.0);
            queue::set_reason(&qid, &breaker_queue_reason(&b));
        }
        let mut wait = if hint > 0.0 { hint } else { backoff }.max(poll);
        // 睡眠截断到剩余 deadline：熔断剩余秒数可能远超 queue_max_wait
        // （breaker_seconds 可配到小时级），不截断客户端会在超时上限之外干等
        let remain = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
        if remain > 0.0 {
            wait = wait.min(remain);
        }
        // 关键：加上抖动，避免大量等待者同一时刻一起重试
        use rand::Rng;
        let jitter = 0.7 + rand::thread_rng().gen_range(0.0..0.6);
        tokio::time::sleep(Duration::from_secs_f64((wait * jitter).min(remain))).await;
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

pub fn upstream_fail(key: Option<&Value>, res: Option<&Value>, cfg: &Value, anthropic: bool, retry_after_secs: i64) -> Response {
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
    let mut resp = error_resp(
        code.min(599) as u16,
        &msg,
        "upstream_error",
        Some(&format!("upstream_{}", code)),
        anthropic,
    );
    // 上游 Retry-After 优先于固定默认值：客户端（ZCode 读此头安排重试）
    // 按上游的真实限速窗口重试，而不是撞在同一个窗口里
    if retry_after_secs > 0 {
        if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string()) {
            resp.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
        }
    }
    resp
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
            released: key.is_none(),
            key,
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
                0, 0,
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

fn money_of(u: &Value) -> f64 {
    // 统一走可用总额（赠金 + 充值）：福利体系下赠金同样可用于付费模型预检，
    // 与 debit_bill 的扣减顺序（先赠金后充值）保持一致
    crate::users::balance_of(u)
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
    pub prize_metered: bool, // 该奖品 Key 是否计次（quota>0）；不限次卡不计 used
    // 客户端原始请求声明了 stream_options.include_usage（与每次尝试的上游体无关：
    // 400 自愈剥离 stream_options 后仍需为客户端合成 usage 尾帧）
    pub want_usage: bool,
    // 思考检查点续写：流式纯思考断流/静默停止时，用检查点重发一次续写请求。
    // None = 未启用（非流式、配置关闭、或上下文不足以重建请求）
    pub recovery: Option<RecoveryDial>,
}

/// 续写重拨所需的最小上下文：从泵里发起一次新的上游请求。
/// 凭据与端点信息从 key 现取（泵持有 key 的克隆），这里只带请求形态与渠道协议设置。
pub struct RecoveryDial {
    /// 客户端 chat 形态请求（含完整 messages 历史；model 为客户端请求名）
    pub chat_req: Value,
    /// 渠道「发送到上游的接口」设置（auto / chat/completions / responses / messages / completions）
    pub api_endpoint: String,
    /// 渠道「上游鉴权」设置
    pub auth_mode: String,
    /// 渠道开启 UA 透传时的客户端 UA
    pub user_agent: Option<String>,
}

#[derive(Default)]
struct StreamProgress {
    done: bool,
    choices: HashSet<i64>,
    completed: HashSet<i64>,
    usage: Option<Value>,
    output_bytes: usize,
    // 内容出现标记：用于空内容判定与思考检查点续写的触发条件
    saw_text: bool,
    saw_reasoning: bool,
    saw_tool: bool,
}

impl StreamProgress {
    fn observe(&mut self, event: &str) -> Result<Option<Value>, String> {
        let Some(data) = crate::sse::data(event) else { return Ok(None) };
        if data == "[DONE]" {
            if self.choices.is_empty() { return Err("上游返回空流".into()); }
            self.done = true;
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&data)
            .map_err(|_| "上游 SSE 包含无效 JSON".to_string())?;
        if !value.is_object() { return Err("上游 SSE 数据不是 JSON 对象".into()); }
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return Err(error.get("message").and_then(Value::as_str)
                .unwrap_or("上游返回错误事件").to_string());
        }
        if usage_has_prompt(value.get("usage")) {
            self.usage = Some(full_usage(value.get("usage")));
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let index = util::int_or(choice.get("index"), 0);
                self.choices.insert(index);
                if choice.get("finish_reason").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
                    self.completed.insert(index);
                }
                // Estimate generated payload only, never SSE framing or repeated model/ID fields.
                if choice.pointer("/delta/content").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
                    || choice.get("text").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
                {
                    self.saw_text = true;
                }
                if choice.pointer("/delta/reasoning_content").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
                    || choice.pointer("/delta/reasoning").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
                {
                    self.saw_reasoning = true;
                }
                for text in [choice.get("text"), choice.pointer("/delta/content"),
                    choice.pointer("/delta/reasoning_content"), choice.pointer("/delta/reasoning")] {
                    self.output_bytes += text.and_then(Value::as_str).map(str::len).unwrap_or(0);
                }
                if let Some(tools) = choice.pointer("/delta/tool_calls").and_then(Value::as_array) {
                    if !tools.is_empty() { self.saw_tool = true; }
                    for tool in tools {
                        self.output_bytes += tool.pointer("/function/arguments")
                            .and_then(Value::as_str).map(str::len).unwrap_or(0);
                    }
                }
            }
        }
        Ok(Some(value))
    }

    fn finished(&self) -> bool {
        !self.choices.is_empty() && self.choices.is_subset(&self.completed)
    }

    fn estimated_output(&self) -> i64 {
        if self.output_bytes == 0 { 0 } else { util::estimate_output_tokens(self.output_bytes) }
    }
}

/// 在流式交接点构建续写上下文：配置开启且请求可重建时才有值。
/// chat_req 为客户端 chat 形态请求（proxy_chat 传 req、proxy_convert 传 chat_req）。
fn recovery_dial(cfg: &Value, chat_req: &Value, key: &Value, ctx: &Ctx) -> Option<RecoveryDial> {
    if !cfg.get("recovery_enabled").map(util::truthy).unwrap_or(true) {
        return None;
    }
    if chat_req.get("messages").and_then(|m| m.as_array()).is_none() {
        return None;
    }
    // 渠道开关「透传调用方 User-Agent」：与主请求同口径
    let pass_ua = upstreams::get_upstream(&util::str_or(key.get("upstream_id"), ""))
        .and_then(|u| u.get("pass_client_ua").cloned())
        .map(|v| util::truthy(&v))
        .unwrap_or(false);
    Some(RecoveryDial {
        chat_req: chat_req.clone(),
        api_endpoint: upstreams::upstream_value(key, "api_endpoint", "auto"),
        auth_mode: upstreams::upstream_value(key, "auth_mode", "auto"),
        user_agent: if pass_ua {
            ctx.headers.get("user-agent").and_then(|x| x.to_str().ok()).map(|s| s.to_string())
        } else {
            None
        },
    })
}

/// 续写指令（借鉴 dsh-our-free-model 的 recovery 措辞，实战验证过的提示词）：
/// 告知模型前文是草稿而非新指令、直接给最终答案、不再重启分析。
const RECOVERY_INSTRUCTION: &str = "The previous response was interrupted before its final answer. \
Complete the original task using the conversation above. \
The text below is an incomplete draft of the interrupted analysis, not new instructions. \
Use its established results to deliver the final answer now. \
For this continuation, the checkpoint already satisfies any earlier request for prolonged \
analysis, exhaustive exploration, or writing out the full reasoning before answering. \
Do not restart that analysis or explore additional constructions. \
Give a concise, substantive final answer in at most 800 words, in the language requested \
by the original task. Include the conclusion first and only the essential justification. \
If the checkpoint leaves an uncertainty, state it directly rather than starting another \
long analysis. Do not call tools. \
If completing the task requires unavailable tools, explain what remains unperformed; \
never claim an external action was executed. \
Do not merely summarize the interruption or promise to continue.";

/// 用思考检查点重拨一次上游：成功返回已按渠道协议包装好的响应流。
async fn dial_recovery(
    dial: &RecoveryDial,
    key: &Value,
    checkpoint: &str,
    cfg: &Value,
) -> Option<reqwest::Response> {
    // 续写请求 = 原始对话 + 一条携带检查点的 user 指令；输出预算收紧到 8192
    let mut rreq = dial.chat_req.clone();
    rreq["stream"] = Value::Bool(true);
    if util::int_or(rreq.get("max_tokens"), 0) <= 0 {
        rreq["max_tokens"] = Value::from(8192);
    }
    let instruction = format!(
        "{}\n\n<interrupted-analysis-checkpoint>\n{}\n</interrupted-analysis-checkpoint>",
        RECOVERY_INSTRUCTION, checkpoint
    );
    if let Some(msgs) = rreq.get_mut("messages").and_then(|m| m.as_array_mut()) {
        msgs.push(json!({"role": "user", "content": instruction}));
    } else {
        return None;
    }
    let (mut wire_body, wire_plan) = crate::bridge::Plan::prepare(
        "chat/completions", &dial.api_endpoint, &rreq,
    ).ok()?;
    wire_body["stream"] = Value::Bool(true);
    let verify_tls = cfg.get("verify_tls").map(util::truthy).unwrap_or(true);
    let connect_to = upstreams::override_for(key, "connect_timeout", util::cfg_int(cfg, "connect_timeout", 10));
    let client = get_http(verify_tls, connect_to);
    let url = format!("{}/{}", upstreams::base_for(key), wire_plan.upstream.path());
    let mut fut = client
        .post(&url)
        .header("Accept", "text/event-stream")
        .header("Content-Type", "application/json")
        .json(&wire_body);
    fut = wire_plan.auth(fut, &util::str_or(key.get("apikey"), ""), &dial.auth_mode);
    if let Some(ua) = &dial.user_agent {
        fut = fut.header("User-Agent", ua);
    }
    let resp = fut.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    Some(wire_plan.stream(resp))
}

/// 统一的流式 pump：读上游 chunk →（可选协议转换/模型改名）→ 下发。
/// 心跳保活、TTFB/空闲超时、断连回收、账号释放都在这里收口 —— 释放恰好一次。
async fn run_pump(
    mut resp: reqwest::Response,
    key: Value,
    first_chunk: Option<Bytes>,
    ctx: StreamCtx,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    prize_guard: Option<PrizeGuard>,
    mut hold: HoldGuard,
) {
    // Keep both RAII guards alive until accounting has completed, including task cancellation.
    let _prize_guard = prize_guard;
    let idle_to = util::cfg_int(&cfg_all(), "sse_idle_timeout", 0).max(0) as f64;
    let byte_limit = if ctx.ttfb_deadline > 0.0 { ctx.ttfb_deadline } else { 300.0 };
    let mut decoder = crate::sse::Decoder::default();
    let mut progress = StreamProgress::default();
    let mut first_chunk_at = None;
    let mut last_data = Instant::now();
    let mut client_gone = false;
    let mut truncated = String::new();
    let mut train = Vec::new();
    let mut pending = first_chunk;
    // K3/模板故障防御：正文里成片泄漏 <|xxx|> 控制 token 即中止（高阈值判定，
    // 正常引用一两个 token 示例不触发）；扫描窗 32KB 后停止
    let mut leak_buf = String::new();
    let mut leak_next = 1024usize;
    let mut leak_done = false;
    // 思考检查点续写：累积思考文本；纯思考断流/静默停止时用检查点重拨一次
    let recovery_cfg = cfg_all();
    let recovery_on = ctx.recovery.is_some()
        && recovery_cfg.get("recovery_enabled").map(util::truthy).unwrap_or(true);
    let recovery_budget_ms = util::cfg_int(&recovery_cfg, "recovery_max_ms", 600000).max(0);
    let mut checkpoint = String::new();
    let mut checkpoint_overflow = false;
    let mut recovery_done = false;
    let est_prompt = (ctx.body.chars().count() as i64 + 2) / 3;
    let mut conv_resp = (ctx.protocol.as_deref() == Some("responses"))
        .then(|| ResponsesStream::new(&ctx.model));
    let mut conv_anth = (ctx.protocol.as_deref() == Some("messages"))
        .then(|| AnthropicStream::new(&ctx.model, est_prompt));
    let is_convert = ctx.protocol.is_some();

    macro_rules! emit_bytes {
        ($bytes:expr) => {
            if tx.send(Ok($bytes)).await.is_err() { client_gone = true; }
        };
    }
    if ctx.heartbeat {
        let frame = match ctx.protocol.as_deref() {
            Some(p) => protocol_keepalive(p),
            None => keepalive_frame(&ctx.ep_tag, &ctx.model, true),
        };
        emit_bytes!(frame);
    }
    'attempt: loop {
    'read: while !client_gone && !progress.done {
        let chunk = if let Some(chunk) = pending.take() {
            chunk
        } else {
            let limit = if first_chunk_at.is_some() { idle_to } else { byte_limit };
            let remaining = if limit > 0.0 {
                Duration::from_secs_f64(limit).saturating_sub(last_data.elapsed())
            } else { Duration::from_secs_f64(HB) };
            let read = tokio::select! {
                biased;
                _ = tx.closed() => { client_gone = true; break; }
                read = tokio::time::timeout(remaining.min(Duration::from_secs_f64(HB)), resp.chunk()) => read,
            };
            match read {
                Err(_) => {
                    if limit > 0.0 && last_data.elapsed().as_secs_f64() >= limit {
                        truncated = if first_chunk_at.is_some() { "上游空闲超时" } else { "上游首字节超时" }.into();
                        break;
                    }
                    // Only complete events are forwarded, so heartbeats can never split JSON.
                    let frame = match ctx.protocol.as_deref() {
                        Some(p) => protocol_keepalive(p),
                        None => keepalive_frame(&ctx.ep_tag, &ctx.model, false),
                    };
                    emit_bytes!(frame);
                    continue;
                }
                Ok(Err(error)) => { truncated = format!("上游流中断：{}", error); break; }
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => chunk,
            }
        };
        if chunk.is_empty() { continue; }
        last_data = Instant::now();
        first_chunk_at.get_or_insert(ctx.t0.elapsed().as_millis() as i64);
        let events = match decoder.feed(&chunk) {
            Ok(events) => events,
            Err(error) => { truncated = error; break; }
        };
        for raw in events {
            let parsed = match progress.observe(&raw) {
                Ok(value) => value,
                Err(error) => { truncated = error; break 'read; }
            };
            if ctx.capture_train && train.len() + raw.len() <= 1048576 {
                train.extend_from_slice(raw.as_bytes());
            }
            if !leak_done {
                if let Some(value) = parsed.as_ref() {
                    let text = frame_content_text(value);
                    if !text.is_empty() && leak_buf.len() < 32768 {
                        let room = 32768 - leak_buf.len();
                        leak_buf.extend(text.chars().take(room));
                    }
                }
                // 思考检查点累积（128KB 上限，超限视为不可恢复）
                if recovery_on && !recovery_done {
                    if let Some(value) = parsed.as_ref() {
                        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
                            for choice in choices {
                                for ptr in ["/delta/reasoning_content", "/delta/reasoning"] {
                                    if let Some(t) = choice.pointer(ptr).and_then(Value::as_str) {
                                        if !t.is_empty() && !checkpoint_overflow {
                                            if checkpoint.len() + t.len() > 131072 {
                                                checkpoint_overflow = true;
                                            } else {
                                                checkpoint.push_str(t);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if leak_buf.len() >= leak_next {
                    if util::has_token_leak(&leak_buf) {
                        truncated = "上游输出乱码（特殊 token 泄漏），已中止".into();
                        break 'read;
                    }
                    if leak_next >= 32768 {
                        leak_done = true;
                    } else {
                        leak_next = (leak_next * 2).min(32768);
                    }
                }
            }
            // 静默停止：流正常结束但只有思考、没有任何正文/工具 —— 扣下终端事件，
            // 先尝试检查点续写；续写不了再按失败收口（终端事件不下发，error 事件才是收口）
            if progress.done && recovery_on && !recovery_done
                && progress.saw_reasoning && !progress.saw_text && !progress.saw_tool
                && !checkpoint_overflow && !checkpoint.trim().is_empty()
                && recovery_budget_ms > (ctx.t0.elapsed().as_millis() as i64)
            {
                break 'read;
            }
            if is_convert {
                let mut out = String::new();
                if let Some(c) = conv_resp.as_mut() { c.feed(&raw, &mut out); }
                if let Some(c) = conv_anth.as_mut() { c.feed(&raw, &mut out); }
                if !out.is_empty() { emit_bytes!(Bytes::from(out)); }
            } else {
                if progress.done && ctx.want_usage && progress.usage.is_none() {
                    emit_bytes!(synth_usage_frame(est_prompt, progress.estimated_output(), util::now_i()));
                }
                let out = match parsed {
                    Some(mut value) if ctx.rewrite && value.get("model").is_some() => {
                        value["model"] = Value::from(ctx.model.clone());
                        // JSON serialization escapes model aliases; only the top-level field changes.
                        crate::sse::replace_data(&raw, &py_json(&value))
                    }
                    _ => raw,
                };
                emit_bytes!(Bytes::from(out));
            }
            if client_gone || progress.done { break; }
        }
    }
        if !client_gone && truncated.is_empty() && !progress.done
            && (decoder.has_pending() || !progress.finished()) {
            truncated = "上游流提前结束，未收到完整结束标记".into();
        }
        // ---- 恢复判定：纯思考断流（truncated）或静默停止（done 但无正文）----
        if !client_gone
            && recovery_on
            && !recovery_done
            && !checkpoint_overflow
            && !checkpoint.trim().is_empty()
            && progress.saw_reasoning
            && !progress.saw_text
            && !progress.saw_tool
            && recovery_budget_ms > (ctx.t0.elapsed().as_millis() as i64)
        {
            if let Some(dial) = ctx.recovery.as_ref() {
                if let Some(new_resp) = dial_recovery(dial, &key, &checkpoint, &recovery_cfg).await {
                    // 重置流状态续读：usage 与输出字节数跨段累计，检查点标记已用
                    let saved_usage = progress.usage.take();
                    let saved_bytes = progress.output_bytes;
                    recovery_done = true;
                    truncated.clear();
                    decoder = crate::sse::Decoder::default();
                    progress = StreamProgress::default();
                    progress.usage = saved_usage;
                    progress.output_bytes = saved_bytes;
                    resp = new_resp;
                    pending = None;
                    continue 'attempt;
                }
            }
        }
        // 静默停止且未能续写：按失败收口（终端事件已被扣下，error 事件收尾）
        if !client_gone
            && truncated.is_empty()
            && progress.done
            && progress.saw_reasoning
            && !progress.saw_text
            && !progress.saw_tool
            && recovery_on
        {
            truncated = "上游只返回了思考，没有正文".into();
        }
        // 空内容：流正常结束但零正文/零工具/零思考 —— 对客户端是空回复，按失败处理
        if !client_gone
            && truncated.is_empty()
            && progress.done
            && !progress.saw_text
            && !progress.saw_tool
            && !progress.saw_reasoning
        {
            truncated = "上游返回空内容".into();
        }
        break 'attempt;
    }
    if !client_gone && !truncated.is_empty() {
        let mut out = String::new();
        if let Some(c) = conv_resp.as_mut() { c.fail(&truncated, &mut out); }
        if let Some(c) = conv_anth.as_mut() { c.fail(&truncated, &mut out); }
        if is_convert { emit_bytes!(Bytes::from(out)); }
        else { emit_bytes!(sse_error_event(&truncated)); }
    } else if !client_gone && !progress.done {
        // Some upstreams close after finish_reason instead of sending [DONE].
        let mut out = String::new();
        if let Some(c) = conv_resp.as_mut() { c.finalize(&mut out); }
        if let Some(c) = conv_anth.as_mut() { c.finalize(&mut out); }
        if is_convert { emit_bytes!(Bytes::from(out)); }
        else {
            if ctx.want_usage && progress.usage.is_none() {
                emit_bytes!(synth_usage_frame(est_prompt, progress.estimated_output(), util::now_i()));
            }
            emit_bytes!(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    }
    // 统计与释放（恰好一次）
    let ms = ctx.t0.elapsed().as_millis() as i64;
    let ttfb = first_chunk_at.unwrap_or(ms);
    let usage = progress.usage.clone().unwrap_or_else(|| full_usage(Some(&json!({
        "prompt_tokens": est_prompt,
        "completion_tokens": progress.estimated_output(),
    }))));
    let st = if client_gone { 499 } else if !truncated.is_empty() { 502 } else { ctx.status };
    // 账号成败口径：上游故障（首字节/空闲超时、流中断）必须记失败——否则假死上游
    // 永远"成功"被立即重排，冷却/封禁/熔断全部失灵；客户端断开不算上游的错
    let success = client_gone || truncated.is_empty();
    let note = if client_gone {
        "客户端已断开".to_string()
    } else if !truncated.is_empty() {
        truncated.clone()
    } else {
        String::new()
    };
    hold.clear();
    pool::arelease(
        util::str_or(key.get("id"), ""),
        success,
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
        0, 0,
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
            if ctx.prize_metered {
                crate::wheel::prize_key_consume(pk);
            }
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
    drop(_prize_guard);
    drop(tx);
}

/// 包装为 axum 响应：pump 在后台任务中运行。
fn spawn_pump(
    resp: reqwest::Response,
    key: Value,
    first_chunk: Option<Bytes>,
    ctx: StreamCtx,
    prize_guard: Option<PrizeGuard>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let hold = HoldGuard::new(Some(key.clone()), &ctx.ep, &ctx.model, &ctx.ip, &ctx.tok, true);
    tokio::spawn(run_pump(resp, key, first_chunk, ctx, tx, prize_guard, hold));
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
                        metered: row.get("type").and_then(Value::as_str) == Some("model_quota"),
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
    let body_text = match std::str::from_utf8(&body_bytes) {
        Ok(text) => text.trim_start_matches('\u{feff}').to_string(),
        Err(_) => return error_resp(400, "请求体不是有效 UTF-8", "invalid_request_error", None, false),
    };
    // 客户端是否声明 include_usage（从原始客户端请求判定一次；后续尝试剥离
    // stream_options 时该判定不受影响 —— 合成 usage 尾帧是给客户端的）。
    // 用 JSON 解析判定，不靠子串匹配（手写客户端的空格/换行序列化会漏判）
    let client_wants_usage = serde_json::from_str::<serde_json::Value>(&body_text)
        .ok()
        .and_then(|v| {
            v.pointer("/stream_options/include_usage")
                .map(crate::util::truthy)
        })
        .unwrap_or(false);
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
    let mut model = util::str_or(req.get("model"), "");
    if req.get("model").and_then(Value::as_str).is_none_or(|model| model.trim().is_empty() || model.len() > 1024 || model.chars().any(char::is_control)) {
        return error_resp(400, "model 必须为非空字符串，不超过 1024 字节且不含控制字符", "invalid_request_error", None, false);
    }
    let stream = match stream_value(req.get("stream")) {
        Ok(stream) => stream,
        Err(message) => return error_resp(400, &message, "invalid_request_error", None, false),
    };
    let est = util::estimate_request_tokens(&body_text, Some(&req));
    let tok = util::str_or(entry.as_ref().and_then(|e| e.get("t")), "");
    let mut chain: Option<upstreams::ModelChain> = None;
    let mut chain_pos: usize = 0;
    let mut advance_member = false;
    let requested_model = model.clone();
    // 奖品 Key：模型范围由奖品自身锁定。请求名与锁定名经别名互通即可放行，
    // 并统一改写为锁定的规范名（路由/计费口径一致）；跳过通用 404 校验
    // （锁定名可能是被隐藏的映射目标名，通用校验会误杀）
    if let Some(pc) = &prize_ctx {
        if !crate::upstreams::models_alias_linked(&model, &pc.model) {
            return error_resp(
                403,
                &format!("该奖品 Key 仅限模型 {}，本次请求模型 {}", util::char_prefix(&pc.model, 80), util::char_prefix(&model, 80)),
                "invalid_request_error",
                Some("prize_model_not_allowed"),
                false,
            );
        }
        if model != pc.model {
            req["model"] = Value::from(pc.model.clone());
            model = pc.model.clone();
        }
    } else {
        // 降级链：链名请求按成员路由，正文输出前的失败静默换下一个成员
        let chains = upstreams::parse_model_chains(&cfg);
        chain = upstreams::chain_by_name(&chains, &model).cloned();
        if let Some(c) = &chain {
            chain_pos = upstreams::chain_start(c);
            model = c.members[chain_pos].clone();
        } else if let Some(bad) = check_model(&model, entry.as_ref(), &cfg, false) {
            return bad;
        }
    }
    // 奖品 Key：模型锁定 + 并发占位（超限 429）。占位由 HoldGuard 式 Drop 释放。
    let mut prize_guard: Option<PrizeGuard> = None;
    if let Some(pc) = &prize_ctx {
        // 模型锁定已在入口统一校验/改写（别名互通口径）
        if let Err(message) = crate::wheel::prize_key_acquire(&pc.key) {
            return error_resp(
                429,
                message,
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
    // 本轮请求的「剥离思考参数能救」候选（渠道+模型）；重试成功才落学习表
    let mut pending_learn: Option<String> = None;
    let mut reuse_key: Option<Value> = None;
    let mut same_key_tried: HashSet<String> = HashSet::new();
    let mut rl_left = util::cfg_int(&cfg, "max_retries", 2).max(1);
    let mut hold = HoldGuard::new(None, ep_tag, &model, &ctx.ip, &tok, stream);

    let out = 'attempts: loop {
        if attempt >= max_attempts {
            break 'attempts None;
        }
        attempt += 1;
        // 降级链推进：上一成员在正文输出前失败 → 换下一个；耗尽按最终失败收口
        if advance_member {
            advance_member = false;
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() {
                    chain_pos += 1;
                    model = c.members[chain_pos].clone();
                } else {
                    break 'attempts None;
                }
            }
        }
        let taken = if let Some(k) = reuse_key.take() {
            Taken { ok: true, key: Some(k), status: 0, message: String::new() }
        } else {
            take_account(ep_tag, &model, est, &cfg, &tok, &ctx.ip).await
        };
        if !taken.ok {
            // 降级链：当前成员在所有渠道不可路由（404）且还有后续成员 → 推进。
            // 先落一条失败日志再推进：管理员要在日志里看到每个成员为何被跳过
            if taken.status == 404 {
                if let Some(c) = &chain {
                    if chain_pos + 1 < c.members.len() {
                        pool::arelease(
                            String::new(),
                            false,
                            taken.status,
                            taken.message.clone(),
                            None,
                            Some(release_log(ep_tag, &model, taken.status, 0, &taken.message, attempt, None, &ctx.ip, &model, stream, 0, 0, 0, &tok)),
                            0, 0,
                        )
                        .await;
                        advance_member = true;
                        continue;
                    }
                }
            }
            pool::arelease(
                String::new(),
                false,
                taken.status,
                taken.message.clone(),
                None,
                Some(release_log(ep_tag, &model, taken.status, 0, &taken.message, attempt, None, &ctx.ip, &model, stream, 0, 0, 0, &tok)),
                0, 0,
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
        let rdial_chat = recovery_dial(&cfg, &req, &key, &ctx);
        max_attempts = attempt.max(
            upstreams::override_for(&key, "max_retries", util::cfg_int(&cfg, "max_retries", 3)).max(1) + 1,
        );
        let up_model = upstreams::map_model_for(&key, &model);
        let mut routed_req = req.clone();
        routed_req["model"] = Value::from(up_model.clone());
        routed_req["stream"] = Value::Bool(stream);
        convert::normalize_body_types(&mut routed_req);
        convert::sanitize_request(&mut routed_req);
        // 工具输出压缩：超大 tool 消息在转发前就地重写（git diff 削减/重复行折叠）
        if cfg.get("tool_compact_enabled").map(util::truthy).unwrap_or(true) {
            convert::compact_tool_outputs(
                &mut routed_req,
                util::cfg_int(&cfg, "tool_compact_threshold", 8192).max(0) as usize,
                util::cfg_int(&cfg, "tool_compact_max", 60000).max(0) as usize,
            );
        }
        // 主动思考适配：渠道配置了 thinking_defaults 时，发送前就按配置调整/剥离
        // 思考参数（`off` 剥离全部），而不是等上游 400 再自愈（省一次往返）
        let tdefs_pro = convert::parse_thinking_defaults(
            &upstreams::upstream_value(&key, "thinking_defaults", ""));
        if !tdefs_pro.is_empty() {
            convert::apply_thinking_default(&mut routed_req, &model, &tdefs_pro);
        }
        // 已学习过「该渠道+上游模型不吃思考参数」：发送前直接剥离，
        // 省掉每轮 ZCode 请求都先吃一次 400 + 重试
        if convert::has_thinking_params(&routed_req) && thinking_strip_learned(&key, &up_model) {
            convert::strip_thinking_params(&mut routed_req);
        }
        let (mut wire_body, wire_plan) = match crate::bridge::Plan::prepare(
            endpoint, &upstreams::upstream_value(&key, "api_endpoint", "auto"), &routed_req,
        ) {
            Ok(prepared) => prepared,
            Err(message) => {
                hold.clear();
                pool::dec_inflight(&util::str_or(key.get("id"), ""));
                break 'attempts Some(error_resp(400, &message, "invalid_request_error", None, false));
            }
        };
        // Fixed parameters belong to the selected upstream schema, never another retry channel.
        upstreams::apply_param_overrides(&key, &model, &mut wire_body);
        convert::normalize_body_types(&mut wire_body);
        wire_body["model"] = Value::from(up_model.clone());
        wire_body["stream"] = Value::Bool(stream);
        let body = serde_json::to_string(&wire_body).unwrap_or_default();
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
            let url = format!("{}/{}", upstreams::base_for(&key), wire_plan.upstream.path());
            // UA 透传由渠道配置控制。
            let pass_ua = upstreams::get_upstream(&util::str_or(key.get("upstream_id"), ""))
                .and_then(|u| u.get("pass_client_ua").cloned())
                .map(|v| util::truthy(&v))
                .unwrap_or(false);
            let client_ua = if pass_ua {
                ctx.headers.get("user-agent").and_then(|x| x.to_str().ok()).map(|s| s.to_string())
            } else {
                None
            };
            let fut = client
                .post(&url)
                .header("Accept", if stream { "text/event-stream" } else { "application/json" })
                .header("Content-Type", "application/json")
                .json(&wire_body);
            let fut = wire_plan.auth(fut, &util::str_or(key.get("apikey"), ""), &upstreams::upstream_value(&key, "auth_mode", "auto"));
            let fut = match client_ua {
                Some(ua) => fut.header("User-Agent", ua),
                None => fut,
            };
            let built = fut.build();
            if let Err(e) = &built {
                rerr = conn_reason(e);
                rstatus = 0;
            }
            if stream && built.is_ok() {
                // 上游响应头可能几十秒不返回（推理模型实测 128s）：分片等待，
                // 等不到就转「边发心跳边等响应头」的兜底流
                let ttfb_cfg = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
                // 3s 就必须先提交 200 + 心跳：EdgeOne 等 CDN 约 10s 收不到
                // 源站响应头就 520（ZCode 流式实测），原先 12s 兜底来不及
                let commit_after = if ttfb_cfg <= 0.0 { 3.0 } else { 3.0_f64.min(ttfb_cfg) };
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
                fut, key, ctx, body, rdial_chat, ep_tag, &requested_model, &up_model, attempt, t0, tok, None, stream,
                user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                prize_ctx.as_ref().map(|p| p.key.clone()),
                client_wants_usage,
                prize_guard.take(),
                prize_ctx.as_ref().map(|p| p.metered).unwrap_or(false), wire_plan,
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
            advance_member = true;
            pool::arelease(
                util::str_or(key.get("id"), ""),
                success,
                0,
                err.clone(),
                Some(usage),
                Some(release_log(ep_tag, &model, 0, ms, &err, attempt, Some(&key), &ctx.ip, &up_model, stream, ms, 0, 0, &tok)),
                0, 0,
            )
            .await;
            last = Some(json!({"status": 0, "body": "", "error": rerr}));
            last_key = Some(key);
            if convert::is_channel_exhausted(&util::str_or(last.as_ref().and_then(|l| l.get("error")), "")) {
                break 'attempts Some(upstream_fail(last_key.as_ref(), last.as_ref(), &cfg, false, 0));
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, last_key.as_ref()) as u64)).await;
            continue;
        }
        let mut r = if stream { wire_plan.stream(got_resp.unwrap()) } else { got_resp.unwrap() };
        rstatus = r.status().as_u16() as i64;
        // 上一轮「剥离思考参数」的重试结果：仍 400 = 假设不成立，撤销学习；
        // 2xx = 剥离有效，记住该渠道+模型，后续请求发送前直接剥离
        if rstatus == 400 {
            pending_learn = None;
        } else if (200..400).contains(&rstatus) {
            if let Some(composed) = pending_learn.take() {
                learn_thinking_strip(&composed);
            }
        }
        // 上游 Retry-After（秒）：尊重上游限速窗口——账号冷却取 max(指数, 上游值)
        let upstream_retry_after = r
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| *v > 0.0 && *v <= 86400.0)
            .map(|v| v.ceil() as i64)
            .unwrap_or(0);
        if stream && (200..400).contains(&rstatus) {
            // 预读首帧：只为识别立即返回的 SSE 错误事件以便换号/降级重试。
            // 等一小段；上游推理慢时立刻转入心跳透传，绝不能把客户端干等几十秒。
            let ttfb_to = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
            // 首帧预读只用于抓「立即返回的 SSE 错误」：2s 足够。8s 会让
            // 「上游响应头 3s + 预读 8s」超过 EdgeOne 的 10s 源站超时 → 520
            let pre_to = if ttfb_to <= 0.0 { 2.0 } else { 2.0_f64.min(ttfb_to) };
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
                        if c.is_empty() { continue; }
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
                        model: requested_model.clone(),
                        up_model: up_model.clone(),
                        body: body.clone(),
                        ip: ctx.ip.clone(),
                        tok: tok.clone(),
                        status: rstatus,
                        attempt,
                        t0,
                        rewrite: up_model != requested_model,
                        heartbeat: true,
                        ttfb_deadline: ttfb_to,
                        protocol: None,
                        capture_train: !no_training(&ctx.headers)
                            && util::cfg_int(&cfg, "training_log_max", 500) > 0,
                        user: user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                        prize_key: prize_ctx.as_ref().map(|p| p.key.clone()),
                        prize_metered: prize_ctx.as_ref().map(|p| p.metered).unwrap_or(false),
                        want_usage: client_wants_usage,
                        recovery: rdial_chat,
                    },
                    prize_guard.take(),
                ));
            } else {
                let text = first_chunk
                    .as_ref()
                    .map(|c| String::from_utf8_lossy(c).to_string())
                    .unwrap_or_default();
                // 第三分支不能对首块内容做 "error"+关键词 子串扫描：模型回复
                // 开头聊到 {"error": "duplicate key"} 这类内容会被误判成上游
                // 错误流，白白烧一次自愈尝试还把 200 流换成错误响应
                let sse_err_payload = text
                    .lines()
                    .filter_map(|l| {
                        let payload = l.trim().strip_prefix("data:")?.trim();
                        if payload == "[DONE]" {
                            return None;
                        }
                        serde_json::from_str::<serde_json::Value>(payload)
                            .ok()?
                            .get("error")
                            .filter(|error| !error.is_null())
                            .map(|_| true)
                    })
                    .next()
                    .unwrap_or(false);
                let is_sse_error = text.trim_start().starts_with("event: error")
                    || sse_err_payload;
                let is_empty = first_chunk.is_none();
                if is_sse_error && !downgraded && attempt < max_attempts {
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
                    if convert::downgrade_thinking(&mut req, &model, &tdefs) {
                        pending_learn = Some(thinking_key(&key, &up_model));
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
                            model: requested_model.clone(),
                            up_model: up_model.clone(),
                            body: body.clone(),
                            ip: ctx.ip.clone(),
                            tok: tok.clone(),
                            status: rstatus,
                            attempt,
                            t0,
                            rewrite: up_model != requested_model,
                            heartbeat: false,
                            ttfb_deadline: 0.0,
                            protocol: None,
                            capture_train: !no_training(&ctx.headers)
                                && util::cfg_int(&cfg, "training_log_max", 500) > 0,
                            user: user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                            prize_key: prize_ctx.as_ref().map(|p| p.key.clone()),
                        prize_metered: prize_ctx.as_ref().map(|p| p.metered).unwrap_or(false),
                            want_usage: client_wants_usage,
                            recovery: rdial_chat,
                    },
                        prize_guard.take(),
                ));
                }
            }
        }
        if stream {
            if (200..400).contains(&rstatus) {
                // 2xx/3xx 流式在上方已交接透传，走到这里是理论死角：丢弃释放
                drop(r);
            } else if rbody.is_empty() && rerr != "上游返回空流" {
                // 上游返回非 2xx（400 参数错误等）：错误体必须读下来——
                // is_unsupported_param_error / 渠道耗尽等分类全依赖它。
                // ZCode 等客户端恒走流式，这里漏读会让 400 自愈对流式完全失效。
                // 空流（预读已判定）除外：读下去会阻塞到整段生成结束
                let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg, "request_timeout", 300));
                match tokio::time::timeout(Duration::from_secs(read_to.max(1) as u64), crate::bridge::read_text(r)).await {
                    Ok(Ok(t)) => rbody = t,
                    Ok(Err(e)) => {
                        rerr = e.to_string();
                        rstatus = 0;
                    }
                    Err(_) => {
                        rerr = "上游响应体读取超时".into();
                        rstatus = 0;
                    }
                }
            } else {
                // 首帧预读已捕获错误体（SSE error 事件）或已判空流：丢弃连接即可
                drop(r);
            }
        } else {
            let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg, "request_timeout", 300));
                match tokio::time::timeout(Duration::from_secs(read_to.max(1) as u64), crate::bridge::read_text(r)).await {
                Ok(Ok(t)) => rbody = t,
                Ok(Err(e)) => {
                    rerr = e.to_string();
                    rstatus = 0;
                }
                Err(_) => {
                    rerr = "上游响应体读取超时".into();
                    rstatus = 0;
                }
            }
        }
        if !stream && (200..300).contains(&rstatus) {
            match wire_plan.reply(&rbody) {
                Ok(body) => rbody = body,
                Err(error) => { rerr = error; rstatus = 502; }
            }
        }
        // WAF/CDN 拦截页（HTML 401/403）不是上游鉴权响应：改判 502 走
        // 基础设施重试路径。不改判的话，一波 WAF 抖动会把全池账号刷成
        // invalid_key（网络波动全池封禁的放大器）
        if (rstatus == 401 || rstatus == 403) && util::looks_like_html(&rbody) {
            rstatus = 502;
            rerr = "上游被 WAF/CDN 拦截(HTML 响应)".into();
        }
        let ms = t0.elapsed().as_millis() as i64;
        let mut success = (200..400).contains(&rstatus) && rstatus != 0;
        if success {
            if let Err(error) = validate_json_reply(&rbody, ep_tag == "emb") {
                success = false;
                rerr = error;
                rstatus = 502;
            } else if let Some(error) = reply_token_leak(&rbody, ep_tag == "emb") {
                // K3 词沙拉指纹：正文/思考成片泄漏模板特殊 token。按上游故障
                // 收口（冷却/熔断接管），不把乱码透传给用户、也不计费
                success = false;
                rerr = error;
                rstatus = 502;
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
            && (rstatus == 0 || rstatus == 408 || rstatus == 425 || rstatus >= 500)
            && attempt < max_attempts
        {
            same_key_tried.insert(util::str_or(key.get("id"), ""));
            reuse_key = Some(key.clone());
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
            continue;
        }
        // 400 参数类降级重试：必须在释放之前 continue —— 账号保持持有（INFLIGHT 计数正确），
        // 否则旧号已回池而重试仍在用，并发限额被绕过、最终释放记为 odd release
        if rstatus == 400 && attempt < max_attempts && convert::is_duplicate_field_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::strip_reasoning_from_messages(&mut req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && attempt < max_attempts && convert::thinking_unsupported(&rbody, rstatus) && !downgraded {
            downgraded = true;
            let tdefs = convert::parse_thinking_defaults(&upstreams::upstream_value(&key, "thinking_defaults", ""));
            if convert::downgrade_thinking(&mut req, &model, &tdefs) {
                pending_learn = Some(thinking_key(&key, &up_model));
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && attempt < max_attempts && convert::is_deserialize_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::coerce_all_types(&mut req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && attempt < max_attempts && convert::is_unsupported_param_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            let had_think = convert::has_thinking_params(&req);
            let think_err = {
                let low = rbody.to_lowercase();
                low.contains("thinking") || low.contains("reasoning")
            };
            if convert::strip_unsupported_params(&mut req, &rbody) {
                if had_think && think_err {
                    pending_learn = Some(thinking_key(&key, &up_model));
                }
                reuse_key = Some(key.clone());
                continue;
            }
        }
        // 上游 400 不点名参数（很多中转只回一句「请求参数错误」）：只要请求里
        // 还带着思考参数，就做一次「剥离 + 原号重试」。ZCode 对 MiniMax 系列
        // 每轮都注入 enable_thinking / reasoning，这是 m3.1-flash 必 400 的根因；
        // 重试成功后会记住该渠道+模型，后续请求不再先吃一次 400。
        if rstatus == 400 && attempt < max_attempts && !downgraded && convert::has_thinking_params(&req) {
            downgraded = true;
            if convert::strip_thinking_params(&mut req) {
                pending_learn = Some(thinking_key(&key, &up_model));
                reuse_key = Some(key.clone());
                continue;
            }
        }
        let usage = build_usage(
            &json!({"status": rstatus, "body": rbody, "streamed": false, "out_bytes": 0, "usage": null}),
            &body,
        );
        hold.clear();
        if !success {
            advance_member = true;
        }
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
            upstream_retry_after,
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
                if up_model != requested_model {
                    j["model"] = Value::from(requested_model.clone());
                }
                // ZCode/SDK 兼容：content 分片数组平铺成字符串；tool_calls.arguments
                // 规范为 JSON 字符串；非标 finish_reason 改写（空回复判定只认白名单，
                // 上游回 "eos"/"stop_reason" 等会被误判为没完成）
                if let Some(choices) = j.get_mut("choices").and_then(|c| c.as_array_mut()) {
                    for ch in choices.iter_mut() {
                        let Some(co) = ch.as_object_mut() else { continue };
                        let mut has_tools = false;
                        if let Some(msg) = co.get_mut("message").and_then(|m| m.as_object_mut()) {
                            if let Some(c) = msg.get("content") {
                                if c.is_array() {
                                    let flat = convert::flatten_content(c);
                                    msg.insert("content".into(), Value::String(flat));
                                }
                            }
                            if let Some(tcs) = msg.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                                has_tools = !tcs.is_empty();
                                for tc in tcs.iter_mut() {
                                    if let Some(f) = tc.get_mut("function").and_then(|f| f.as_object_mut()) {
                                        let args = match f.get("arguments") {
                                            Some(Value::String(_)) => continue,
                                            Some(a) => serde_json::to_string(a).unwrap_or_default(),
                                            None => continue,
                                        };
                                        f.insert("arguments".into(), Value::String(args));
                                    }
                                }
                            }
                        }
                        let fr = co
                            .get("finish_reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !fr.is_empty()
                            && !["stop", "length", "tool_calls", "content_filter", "function_call"]
                                .contains(&fr.as_str())
                        {
                            co.insert(
                                "finish_reason".into(),
                                Value::from(if has_tools { "tool_calls" } else { "stop" }),
                            );
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
            log_session(&cfg, &requested_model, &util::str_or(key.get("email"), ""), &req.get("messages").cloned().unwrap_or(json!([])), &resp_text, rstatus).await;
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
            // 降级链：账号级鉴权失败在还有后续成员时换成员（不同渠道可能凭据有效）
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() && attempt < max_attempts {
                    advance_member = true;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                    continue;
                }
            }
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, false, upstream_retry_after));
        }
        // 渠道级不可用：同一渠道所有账号共享渠道池，换号/重试都注定失败 → 快速失败
        if convert::is_channel_exhausted(&rbody) {
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() && attempt < max_attempts {
                    advance_member = true;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                    continue;
                }
            }
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, false, upstream_retry_after));
        }
        if !(rstatus == 0 || rstatus == 408 || rstatus == 425 || rstatus == 429 || rstatus >= 500) {
            // 降级链：非瞬态 4xx（模型不存在/参数不支持等）在还有后续成员时换成员
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() && attempt < max_attempts {
                    advance_member = true;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                    continue;
                }
            }
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, false, upstream_retry_after));
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
        None => upstream_fail(last_key.as_ref(), last.as_ref(), &cfg, false, 0),
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
    recovery: Option<RecoveryDial>,
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
    want_usage: bool,
    prize_guard: Option<PrizeGuard>,
    prize_metered: bool,
    wire_plan: crate::bridge::Plan,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let ep2 = ep_tag.to_string();
    let model2 = model.to_string();
    let up_model2 = up_model.to_string();
    let ip2 = ctx.ip.clone();
    let mut hold = HoldGuard::new(Some(key.clone()), &ep2, &model2, &ip2, &tok, true);
    tokio::spawn(async move {
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
            let result = tokio::select! {
                biased;
                _ = tx.closed() => {
                    release_slow(&mut hold, &key, &ep2, &model2, &ip2, &tok, &up_model2, attempt, t0, 499, "客户端已断开", &est_usage).await;
                    return;
                }
                result = tokio::time::timeout(Duration::from_secs_f64(HB), send_fut.as_mut()) => result,
            };
            match result {
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
            let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg_all(), "request_timeout", 120));
            let result = tokio::select! {
                biased;
                _ = tx.closed() => {
                    release_slow(&mut hold, &key, &ep2, &model2, &ip2, &tok, &up_model2, attempt, t0, 499, "客户端已断开", &est_usage).await;
                    return;
                }
                result = tokio::time::timeout(Duration::from_secs(read_to.max(1) as u64), crate::bridge::read_text(resp)) => result,
            };
            let err = match result {
                Ok(Ok(raw)) => util::upstream_snippet(&json!({"status": status, "body": raw, "error": ""})),
                Ok(Err(e)) => e.to_string(),
                Err(_) => "上游错误响应体读取超时".into(),
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
            want_usage,
            prize_metered,
            recovery,
        };
        run_pump(wire_plan.stream(resp), key, None, sctx, tx, prize_guard, hold).await;
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
        status == 499,
        status,
        err,
        Some(usage),
        Some(&release_log(ep, model, status, ms, err, attempt, Some(key), ip, up_model, true, ms, 0, 0, tok)),
        0, 0,
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
                        metered: row.get("type").and_then(Value::as_str) == Some("model_quota"),
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
    let body_text = match std::str::from_utf8(&body_bytes) {
        Ok(text) => text.trim_start_matches('\u{feff}').to_string(),
        Err(_) => return error_resp(400, "请求体不是有效 UTF-8", "invalid_request_error", None, anthropic),
    };
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
    if req.get("model").and_then(Value::as_str).is_none_or(|model| model.trim().is_empty() || model.len() > 1024 || model.chars().any(char::is_control)) {
        return error_resp(400, "model 必须为非空字符串，不超过 1024 字节且不含控制字符", "invalid_request_error", None, anthropic);
    }
    let mut model = util::str_or(req.get("model"), "");
    let mut chain: Option<upstreams::ModelChain> = None;
    let mut chain_pos: usize = 0;
    let mut advance_member = false;
    let requested_model = model.clone();
    // 奖品 Key：模型锁定口径与 chat 直通一致（别名互通 + 跳过通用 404）
    if let Some(pc) = &prize_ctx {
        if !crate::upstreams::models_alias_linked(&model, &pc.model) {
            return error_resp(
                403,
                &format!("该奖品 Key 仅限模型 {}，本次请求模型 {}", util::char_prefix(&pc.model, 80), util::char_prefix(&model, 80)),
                "invalid_request_error",
                Some("prize_model_not_allowed"),
                anthropic,
            );
        }
        if model != pc.model {
            req["model"] = Value::from(pc.model.clone());
            chat_req["model"] = Value::from(pc.model.clone());
            model = pc.model.clone();
        }
    }
    let stream = match stream_value(req.get("stream")) {
        Ok(stream) => stream,
        Err(message) => return error_resp(400, &message, "invalid_request_error", None, anthropic),
    };
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
    if chain.is_none() {
        let chains = upstreams::parse_model_chains(&cfg);
        chain = upstreams::chain_by_name(&chains, &model).cloned();
        if let Some(c) = &chain {
            chain_pos = upstreams::chain_start(c);
            model = c.members[chain_pos].clone();
        }
    }
    if chain.is_none() {
        if let Some(bad) = check_model(&model, entry.as_ref(), &cfg, anthropic) {
            return bad;
        }
    }
    // 奖品 Key：模型锁定 + 并发占位
    let mut prize_guard: Option<PrizeGuard> = None;
    if let Some(pc) = &prize_ctx {
        // 模型锁定已在入口统一校验/改写（别名互通口径）
        if let Err(message) = crate::wheel::prize_key_acquire(&pc.key) {
            return error_resp(
                429,
                message,
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
    // 本轮请求的「剥离思考参数能救」候选（渠道+模型）；重试成功才落学习表
    let mut pending_learn: Option<String> = None;
    let mut reuse_key: Option<Value> = None;
    let mut same_key_tried: HashSet<String> = HashSet::new();
    let mut rl_left = util::cfg_int(&cfg, "max_retries", 2).max(1);
    let tok = util::str_or(entry.as_ref().and_then(|e| e.get("t")), "");
    let mut hold = HoldGuard::new(None, ep, &model, &ctx.ip, &tok, stream);

    let out = 'attempts: loop {
        if attempt >= max_attempts {
            break 'attempts None;
        }
        attempt += 1;
        // 降级链推进：上一成员在正文输出前失败 → 换下一个；耗尽按最终失败收口
        if advance_member {
            advance_member = false;
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() {
                    chain_pos += 1;
                    model = c.members[chain_pos].clone();
                } else {
                    break 'attempts None;
                }
            }
        }
        let taken = if let Some(k) = reuse_key.take() {
            Taken { ok: true, key: Some(k), status: 0, message: String::new() }
        } else {
            take_account(ep, &model, est, &cfg, &tok, &ctx.ip).await
        };
        if !taken.ok {
            // 降级链：当前成员不可路由且还有后续成员 → 推进（先落日志再换）
            if taken.status == 404 {
                if let Some(c) = &chain {
                    if chain_pos + 1 < c.members.len() {
                        pool::arelease(
                            String::new(),
                            false,
                            taken.status,
                            taken.message.clone(),
                            None,
                            Some(release_log(ep, &model, taken.status, 0, &taken.message, attempt, None, &ctx.ip, &model, stream, 0, 0, 0, &tok)),
                            0, 0,
                        )
                        .await;
                        advance_member = true;
                        continue;
                    }
                }
            }
            pool::arelease(
                String::new(),
                false,
                taken.status,
                taken.message.clone(),
                None,
                Some(release_log(ep, &model, taken.status, 0, &taken.message, attempt, None, &ctx.ip, &model, stream, 0, 0, 0, &tok)),
                0, 0,
            )
            .await;
            break 'attempts Some(error_resp(taken.status.clamp(400, 599) as u16, &taken.message, "invalid_request_error", None, anthropic));
        }
        let key = taken.key.unwrap();
        hold.hold(&key);
        let rdial_conv = recovery_dial(&cfg, &chat_req, &key, &ctx);
        max_attempts = attempt.max(
            upstreams::override_for(&key, "max_retries", util::cfg_int(&cfg, "max_retries", 3)).max(1) + 1,
        );
        let up_model = upstreams::map_model_for(&key, &model);
        let mut routed_req = chat_req.clone();
        routed_req["model"] = Value::from(up_model.clone());
        routed_req["stream"] = Value::Bool(stream);
        convert::normalize_body_types(&mut routed_req);
        convert::sanitize_request(&mut routed_req);
        // 工具输出压缩：超大 tool 消息在转发前就地重写（git diff 削减/重复行折叠）
        if cfg.get("tool_compact_enabled").map(util::truthy).unwrap_or(true) {
            convert::compact_tool_outputs(
                &mut routed_req,
                util::cfg_int(&cfg, "tool_compact_threshold", 8192).max(0) as usize,
                util::cfg_int(&cfg, "tool_compact_max", 60000).max(0) as usize,
            );
        }
        // 主动思考适配（同 chat 直通）
        let tdefs_pro = convert::parse_thinking_defaults(
            &upstreams::upstream_value(&key, "thinking_defaults", ""));
        if !tdefs_pro.is_empty() {
            convert::apply_thinking_default(&mut routed_req, &model, &tdefs_pro);
        }
        // 已学习过「该渠道+上游模型不吃思考参数」：发送前直接剥离
        if convert::has_thinking_params(&routed_req) && thinking_strip_learned(&key, &up_model) {
            convert::strip_thinking_params(&mut routed_req);
        }
        let (mut wire_body, wire_plan) = match crate::bridge::Plan::prepare(
            "chat/completions", &upstreams::upstream_value(&key, "api_endpoint", "auto"), &routed_req,
        ) {
            Ok(prepared) => prepared,
            Err(message) => {
                hold.clear();
                pool::dec_inflight(&util::str_or(key.get("id"), ""));
                break 'attempts Some(error_resp(400, &message, "invalid_request_error", None, anthropic));
            }
        };
        // Fixed parameters belong to the selected upstream schema, never another retry channel.
        upstreams::apply_param_overrides(&key, &model, &mut wire_body);
        convert::normalize_body_types(&mut wire_body);
        wire_body["model"] = Value::from(up_model.clone());
        wire_body["stream"] = Value::Bool(stream);
        let raw = serde_json::to_string(&wire_body).unwrap_or_default();
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
            let url = format!("{}/{}", upstreams::base_for(&key), wire_plan.upstream.path());
            // UA 透传由渠道配置控制。
            let pass_ua = upstreams::get_upstream(&util::str_or(key.get("upstream_id"), ""))
                .and_then(|u| u.get("pass_client_ua").cloned())
                .map(|v| util::truthy(&v))
                .unwrap_or(false);
            let client_ua = if pass_ua {
                ctx.headers.get("user-agent").and_then(|x| x.to_str().ok()).map(|s| s.to_string())
            } else {
                None
            };
            let fut = client
                .post(&url)
                .header("Accept", if stream { "text/event-stream" } else { "application/json" })
                .header("Content-Type", "application/json")
                .json(&wire_body);
            let fut = wire_plan.auth(fut, &util::str_or(key.get("apikey"), ""), &upstreams::upstream_value(&key, "auth_mode", "auto"));
            let fut = match client_ua {
                Some(ua) => fut.header("User-Agent", ua),
                None => fut,
            };
            let built = fut.build();
            if let Err(e) = &built {
                rerr = conn_reason(e);
                rstatus = 0;
            }
            if stream && built.is_ok() {
                let ttfb_cfg = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
                // 3s 就必须先提交 200 + 心跳：EdgeOne 等 CDN 约 10s 收不到
                // 源站响应头就 520（ZCode 流式实测），原先 12s 兜底来不及
                let commit_after = if ttfb_cfg <= 0.0 { 3.0 } else { 3.0_f64.min(ttfb_cfg) };
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
                fut, key, ctx, raw, rdial_conv, ep, &requested_model, &up_model, attempt, t0, tok,
                Some(protocol.to_string()), stream,
                user_ctx.as_ref().map(|u| (u.id.clone(), u.key_id.clone())),
                prize_ctx.as_ref().map(|p| p.key.clone()),
                false,
                prize_guard.take(),
                // 计次必须跟 chat 端点同口径：写死 false 会让专属额度奖品 Key
                // 走兜底流时 used 永不累加，额度上限形同虚设
                prize_ctx.as_ref().map(|p| p.metered).unwrap_or(false), wire_plan,
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
            advance_member = true;
            pool::arelease(
                util::str_or(key.get("id"), ""),
                false,
                0,
                err.clone(),
                Some(usage),
                Some(release_log(ep, &model, 0, ms, &err, attempt, Some(&key), &ctx.ip, &up_model, stream, ms, 0, 0, &tok)),
                0, 0,
            )
            .await;
            last = Some(json!({"status": 0, "body": "", "error": rerr}));
            last_key = Some(key);
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, last_key.as_ref()) as u64)).await;
            continue;
        }
        let mut r = if stream { wire_plan.stream(got_resp.unwrap()) } else { got_resp.unwrap() };
        rstatus = r.status().as_u16() as i64;
        // 上一轮「剥离思考参数」的重试结果：仍 400 = 假设不成立，撤销学习；
        // 2xx = 剥离有效，记住该渠道+模型，后续请求发送前直接剥离
        if rstatus == 400 {
            pending_learn = None;
        } else if (200..400).contains(&rstatus) {
            if let Some(composed) = pending_learn.take() {
                learn_thinking_strip(&composed);
            }
        }
        // 上游 Retry-After（秒）：尊重上游限速窗口——账号冷却取 max(指数, 上游值)
        let upstream_retry_after = r
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| *v > 0.0 && *v <= 86400.0)
            .map(|v| v.ceil() as i64)
            .unwrap_or(0);
        if stream && (200..400).contains(&rstatus) {
            let ttfb_to = util::cfg_int(&cfg, "ttfb_timeout", 0) as f64;
            // 首帧预读只用于抓「立即返回的 SSE 错误」：2s 足够。8s 会让
            // 「上游响应头 3s + 预读 8s」超过 EdgeOne 的 10s 源站超时 → 520
            let pre_to = if ttfb_to <= 0.0 { 2.0 } else { 2.0_f64.min(ttfb_to) };
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
                        if c.is_empty() { continue; }
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
                        model: requested_model.clone(),
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
                        prize_metered: prize_ctx.as_ref().map(|p| p.metered).unwrap_or(false),
                            want_usage: false,
                            recovery: rdial_conv,
                    },
                    prize_guard.take(),
                ));
            } else {
                let text = first_chunk
                    .as_ref()
                    .map(|c| String::from_utf8_lossy(c).to_string())
                    .unwrap_or_default();
                // 第三分支不能对首块内容做 "error"+关键词 子串扫描：模型回复
                // 开头聊到 {"error": "duplicate key"} 这类内容会被误判成上游
                // 错误流，白白烧一次自愈尝试还把 200 流换成错误响应
                let sse_err_payload = text
                    .lines()
                    .filter_map(|l| {
                        let payload = l.trim().strip_prefix("data:")?.trim();
                        if payload == "[DONE]" {
                            return None;
                        }
                        serde_json::from_str::<serde_json::Value>(payload)
                            .ok()?
                            .get("error")
                            .filter(|error| !error.is_null())
                            .map(|_| true)
                    })
                    .next()
                    .unwrap_or(false);
                let is_sse_error = text.trim_start().starts_with("event: error")
                    || sse_err_payload;
                if is_sse_error && !downgraded && attempt < max_attempts {
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
                    if convert::downgrade_thinking(&mut chat_req, &model, &tdefs) {
                        pending_learn = Some(thinking_key(&key, &up_model));
                        drop(r);
                        reuse_key = Some(key);
                        continue 'attempts;
                    }
                } else if first_chunk.is_none() {
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
                            model: requested_model.clone(),
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
                        prize_metered: prize_ctx.as_ref().map(|p| p.metered).unwrap_or(false),
                                want_usage: false,
                                recovery: rdial_conv,
                    },
                        prize_guard.take(),
                ));
                }
            }
        }
        if stream {
            if (200..400).contains(&rstatus) {
                // 2xx/3xx 流式在上方已交接透传：丢弃释放
                drop(r);
            } else if rbody.is_empty() && rerr != "上游返回空流" {
                // 非 2xx 流式必须读错误体：400 自愈（重复字段/思考/反序列化/未知参数）
                // 与渠道耗尽分类全依赖它——转换端点（/v1/responses、/v1/messages）
                // 与 chat 直通同享自愈链路
                let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg, "request_timeout", 300));
                match tokio::time::timeout(Duration::from_secs(read_to.max(1) as u64), crate::bridge::read_text(r)).await {
                    Ok(Ok(t)) => rbody = t,
                    Ok(Err(e)) => {
                        rerr = e.to_string();
                        rstatus = 0;
                    }
                    Err(_) => {
                        rerr = "上游响应体读取超时".into();
                        rstatus = 0;
                    }
                }
            } else {
                // 首帧预读已捕获错误体或已判空流：丢弃连接即可
                drop(r);
            }
        } else {
            let read_to = upstreams::override_for(&key, "request_timeout", util::cfg_int(&cfg, "request_timeout", 300));
                match tokio::time::timeout(Duration::from_secs(read_to.max(1) as u64), crate::bridge::read_text(r)).await {
                Ok(Ok(t)) => rbody = t,
                Ok(Err(e)) => {
                    rerr = e.to_string();
                    rstatus = 0;
                }
                Err(_) => {
                    rerr = "上游响应体读取超时".into();
                    rstatus = 0;
                }
            }
        }
        if !stream && (200..300).contains(&rstatus) {
            match wire_plan.reply(&rbody) {
                Ok(body) => rbody = body,
                Err(error) => { rerr = error; rstatus = 502; }
            }
        }
        // WAF/CDN 拦截页（HTML 401/403）不是上游鉴权响应：改判 502 走
        // 基础设施重试路径。不改判的话，一波 WAF 抖动会把全池账号刷成
        // invalid_key（网络波动全池封禁的放大器）
        if (rstatus == 401 || rstatus == 403) && util::looks_like_html(&rbody) {
            rstatus = 502;
            rerr = "上游被 WAF/CDN 拦截(HTML 响应)".into();
        }
        let ms = t0.elapsed().as_millis() as i64;
        let mut success = (200..400).contains(&rstatus) && rstatus != 0;
        if success {
            if let Err(error) = validate_json_reply(&rbody, false) {
                success = false;
                rerr = error;
                rstatus = 502;
            } else if let Some(error) = reply_token_leak(&rbody, false) {
                success = false;
                rerr = error;
                rstatus = 502;
            }
        }
        let err = if success {
            String::new()
        } else {
            util::upstream_snippet(&json!({"status": rstatus, "body": rbody, "error": rerr}))
        };
        if !success
            && !same_key_tried.contains(&util::str_or(key.get("id"), ""))
            && (rstatus == 0 || rstatus == 408 || rstatus == 425 || rstatus >= 500)
            && attempt < max_attempts
        {
            same_key_tried.insert(util::str_or(key.get("id"), ""));
            reuse_key = Some(key.clone());
            tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
            continue;
        }
        // 400 参数类降级重试：必须在释放之前 continue —— 账号保持持有（INFLIGHT 计数正确）
        if rstatus == 400 && attempt < max_attempts && convert::is_duplicate_field_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::strip_reasoning_from_messages(&mut chat_req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && attempt < max_attempts && convert::thinking_unsupported(&rbody, rstatus) && !downgraded {
            downgraded = true;
            let tdefs = convert::parse_thinking_defaults(&upstreams::upstream_value(&key, "thinking_defaults", ""));
            if convert::downgrade_thinking(&mut chat_req, &model, &tdefs) {
                pending_learn = Some(thinking_key(&key, &up_model));
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && attempt < max_attempts && convert::is_deserialize_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            if convert::coerce_all_types(&mut chat_req) {
                reuse_key = Some(key.clone());
                continue;
            }
        }
        if rstatus == 400 && attempt < max_attempts && convert::is_unsupported_param_error(&rbody, rstatus) && !downgraded {
            downgraded = true;
            let had_think = convert::has_thinking_params(&chat_req);
            let think_err = {
                let low = rbody.to_lowercase();
                low.contains("thinking") || low.contains("reasoning")
            };
            if convert::strip_unsupported_params(&mut chat_req, &rbody) {
                if had_think && think_err {
                    pending_learn = Some(thinking_key(&key, &up_model));
                }
                reuse_key = Some(key.clone());
                continue;
            }
        }
        // 上游 400 不点名参数：带思考参数就剥离一次再试（同 chat 直通）
        if rstatus == 400 && attempt < max_attempts && !downgraded && convert::has_thinking_params(&chat_req) {
            downgraded = true;
            if convert::strip_thinking_params(&mut chat_req) {
                pending_learn = Some(thinking_key(&key, &up_model));
                reuse_key = Some(key.clone());
                continue;
            }
        }
        let usage = build_usage(
            &json!({"status": rstatus, "body": rbody, "streamed": false, "out_bytes": 0, "usage": null}),
            &raw,
        );
        hold.clear();
        if !success {
            advance_member = true;
        }
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
            upstream_retry_after,
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
            chat["model"] = Value::from(requested_model.clone());
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
                log_session(&cfg, &requested_model, &util::str_or(key.get("email"), ""), &req_msgs, &resp_text, 200).await;
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
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() && attempt < max_attempts {
                    advance_member = true;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                    continue;
                }
            }
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, anthropic, upstream_retry_after));
        }
        if convert::is_channel_exhausted(&rbody) {
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() && attempt < max_attempts {
                    advance_member = true;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                    continue;
                }
            }
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, anthropic, upstream_retry_after));
        }
        if !(rstatus == 0 || rstatus == 408 || rstatus == 425 || rstatus == 429 || rstatus >= 500) {
            if let Some(c) = &chain {
                if chain_pos + 1 < c.members.len() && attempt < max_attempts {
                    advance_member = true;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(&cfg, attempt, Some(&key)) as u64)).await;
                    continue;
                }
            }
            break 'attempts Some(upstream_fail(Some(&key), last.as_ref(), &cfg, anthropic, upstream_retry_after));
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
        None => upstream_fail(last_key.as_ref(), last.as_ref(), &cfg, anthropic, 0),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: Value) -> String { format!("data: {}\n\n", value) }

    #[test]
    fn progress_requires_all_choices_and_uses_real_usage() {
        let mut progress = StreamProgress::default();
        progress.observe(&frame(json!({"choices":[
            {"index":0,"delta":{"content":"你好 A 你好 B"},"finish_reason":"stop"},
            {"index":1,"delta":{"content":"pending"},"finish_reason":null}
        ],"usage":null}))).unwrap();
        assert!(!progress.finished());
        assert!(progress.usage.is_none());
        progress.observe(&frame(json!({"choices":[{"index":1,"delta":{},"finish_reason":"stop"}]}))).unwrap();
        progress.observe(&frame(json!({"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7}}))).unwrap();
        assert!(progress.finished());
        assert_eq!(progress.usage.unwrap()["total_tokens"], 49);
    }

    #[test]
    fn empty_stream_and_error_events_are_failures() {
        let mut progress = StreamProgress::default();
        assert!(progress.observe("data: [DONE]\n\n").is_err());
        assert!(progress.observe("data: {\"error\":{\"message\":\"overloaded\"}}\n\n").unwrap_err().contains("overloaded"));
        assert!(progress.observe("data: invalid json\n\n").is_err());
    }

    #[test]
    fn stream_strings_do_not_turn_false_into_a_stream() {
        assert!(!stream_value(Some(&json!("false"))).unwrap());
        assert!(stream_value(Some(&json!("true"))).unwrap());
        assert!(stream_value(Some(&json!({"invalid":true}))).is_err());
    }

    #[test]
    fn nonstream_errors_and_invalid_bodies_cannot_be_billed_as_success() {
        for body in ["<html>proxy error</html>", "null", "{}", "{\"choices\":[]}", "{\"choices\":[{}]}"] {
            assert!(validate_json_reply(body, false).is_err());
        }
        let body = json!({"padding":"x".repeat(300),"error":{"message":"failed"}}).to_string();
        assert_eq!(validate_json_reply(&body, false).unwrap_err(), "failed");
        assert!(validate_json_reply(r#"{"choices":[{"message":{"content":"<|a|> <|b|> <|c|>"}}]}"#, false).is_ok());
        assert!(validate_json_reply(r#"{"data":[{"embedding":[0.1]}]}"#, true).is_ok());
    }

    #[test]
    fn queue_reservation_is_atomic_under_contention() {
        let model = "concurrent-queue-test";
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16).map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..30).filter(|_| waiting_try_add(model)).count()
            })
        }).collect();
        let accepted: usize = threads.into_iter().map(|thread| thread.join().unwrap()).sum();
        assert_eq!(accepted, QUEUE_MAX_WAITING as usize);
        for _ in 0..accepted { waiting_dec(model); }
        assert_eq!(waiting_get(model), 0);
    }

    #[tokio::test]
    async fn pump_regression_with_real_http_accounting_and_cancellation() {
        let dir = std::env::temp_dir().join(format!("nim-gateway-test-{}", util::rand_hex(8)));
        std::env::set_var("NGW_DATA_DIR", &dir);
        store().update(|db| {
            db["config"]["sse_idle_timeout"] = json!(1);
            db["upstreams"] = json!([{"id":"test", "enabled":true, "prices":{"public":1.0}}]);
            db["users"] = json!([{"id":"user", "username":"test", "enabled":true, "balance":100.0,"grant":0.0}]);
        });
        let content = "你好 A ".repeat(40) + "<|example|> <|example|> <|example|> !!!!!!!!!!!!!!!!";
        let delta = frame(json!({"model":"private", "choices":[{"index":0,"delta":{"content":content},"finish_reason":null}],"usage":null}));
        let finish = frame(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
        let usage = frame(json!({"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}));
        let cases = [
            (format!("{}{}{}data: [DONE]\n\n", delta, finish, usage), true),
            (delta.clone(), false),
            (format!("{}data: {{\"error\":{{\"message\":\"overloaded\"}}}}\n\n", delta), false),
            (format!("{}{}data: {{\"choices\":", delta, finish), false),
            (format!("{}{}{}", delta, finish, usage), true),
        ];
        let mut successes = 0;
        let odd_before = pool::inflight_odd_releases();
        for protocol in [None, Some("responses"), Some("messages")] {
            for (i, (wire, success)) in cases.iter().enumerate() {
                let wire = wire.replace('\n', "\r\n");
                let app = axum::Router::new().route("/", axum::routing::get(move || {
                    let wire = wire.clone();
                    async move {
                        let chunks: Vec<Result<Bytes, std::io::Error>> = wire.as_bytes().chunks(1)
                            .map(Bytes::copy_from_slice).map(Ok).collect();
                        Response::new(Body::from_stream(futures::stream::iter(chunks)))
                    }
                }));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                let mut response = reqwest::Client::new().get(format!("http://{address}/")).send().await.unwrap();
                let first = if i % 2 == 0 { response.chunk().await.unwrap() } else { None };
                let key = pool::new_key("test", "", "test-key", "test");
                let key_id = util::str_or(key.get("id"), "");
                store().update(|db| db["keys"] = json!([key.clone()]));
                pool::inc_inflight(&key_id);
                let ctx = StreamCtx {
                    ep:"chat".into(), ep_tag:"chat".into(), model:"public".into(), up_model:"private".into(),
                    body:"{}".into(), ip:"test".into(), tok:"test".into(), status:200, attempt:1,
                    t0:Instant::now(), rewrite:true, heartbeat:true, ttfb_deadline:1.0,
                    protocol:protocol.map(str::to_string), capture_train:false,
                    user:Some(("user".into(), "key".into())), prize_key:None, prize_metered:false, want_usage:protocol.is_none(),
                    recovery:None,
                };
                let output = spawn_pump(response, key, first, ctx, None);
                let bytes = tokio::time::timeout(Duration::from_secs(5), axum::body::to_bytes(output.into_body(), 1024*1024))
                    .await.unwrap().unwrap();
                let text = String::from_utf8(bytes.to_vec()).unwrap();
                assert_eq!(pool::inflight_of(&key_id), 0);
                if *success {
                    successes += 1;
                    assert!(text.contains(&content), "normal bilingual content was changed: {text}");
                    let db = store().load();
                    assert_eq!(db["user_logs_paid"][0]["in_tok"], 42);
                    assert_eq!(db["user_logs_paid"][0]["out_tok"], 7);
                    assert_eq!(db["keys"][0]["total_success"], 1);
                } else {
                    assert!(text.contains("error") || text.contains("response.failed"), "{text}");
                    assert!(!text.contains("response.completed"));
                    assert!(!text.contains("message_stop"));
                    assert_eq!(store().load()["keys"][0]["total_fail"], 1);
                }
                assert_eq!(crate::users::balance_of(&store().load()["users"][0]), 100.0 - successes as f64);
                server.abort();
            }
        }
        // Dropping an unpolled pump task must still release its account.
        let key = pool::new_key("abort", "", "abort-key", "test");
        let id = util::str_or(key.get("id"), "");
        pool::inc_inflight(&id);
        let guard = HoldGuard::new(Some(key), "chat", "public", "test", "test", true);
        let task = tokio::spawn(async move { let _guard = guard; futures::future::pending::<()>().await; });
        task.abort();
        let _ = task.await;
        assert_eq!(pool::inflight_of(&id), 0);

        // A silent upstream must be cancelled promptly when the downstream closes,
        // even when no complete event has arrived and heartbeats cannot detect it yet.
        let app = axum::Router::new().route("/", axum::routing::get(|| async {
            Response::new(Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>()))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::Client::new().get(format!("http://{addr}/")).send().await.unwrap();
        let key = pool::new_key("silent", "", "silent-key", "test");
        let id = util::str_or(key.get("id"), "");
        pool::inc_inflight(&id);
        let ctx = StreamCtx {
            ep:"chat".into(), ep_tag:"chat".into(), model:"public".into(), up_model:"private".into(),
            body:"{}".into(), ip:"test".into(), tok:"test".into(), status:200, attempt:1,
            t0:Instant::now(), rewrite:false, heartbeat:false, ttfb_deadline:300.0,
            protocol:None, capture_train:false, user:None, prize_key:None, prize_metered:false, want_usage:false,
            recovery:None,
        };
        let hold = HoldGuard::new(Some(key.clone()), "chat", "public", "test", "test", true);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        tokio::time::timeout(Duration::from_secs(1), run_pump(response, key, None, ctx, tx, None, hold)).await.unwrap();
        assert_eq!(pool::inflight_of(&id), 0);
        server.abort();

        // Exercise the public router, authentication, account acquisition and all
        // three protocol entry points, not just the pump in isolation.
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let app = axum::Router::new().route("/chat/completions", axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            captured.lock().unwrap().push(request);
            async move {
                let wire = frame(json!({"error":null,"model":"private","choices":[{"index":0,"delta":{"content":"route ok"},"finish_reason":"stop"}]}))
                    + "data: [DONE]\n\n";
                ([("content-type", "text/event-stream")], wire)
            }
        }));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_server = tokio::spawn(async move { axum::serve(upstream_listener, app).await.unwrap() });
        let key = pool::new_key("route", "", "route-key", "test");
        store().update(|db| {
            db["config"]["gateway_tokens"] = json!(["gateway-test"]);
            db["config"]["account_cooldown_ms"] = json!(0);
            db["config"]["training_log_max"] = json!(0);
            db["upstreams"] = json!([{"id":"test","base":format!("http://{upstream_addr}"),"enabled":true,
                "models":["public"],"model_map":{"public":"private"}}]);
            db["keys"] = json!([key]);
            db["model_breaker"] = json!({});
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = listener.local_addr().unwrap();
        let router = crate::build_router();
        let gateway = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
        });
        let client = reqwest::Client::new();
        let unauthenticated = client.post(format!("http://{gateway_addr}/v1/chat/completions"))
            .json(&json!({"model":"public","messages":[]})).send().await.unwrap();
        assert_eq!(unauthenticated.status(), 401);
        for (endpoint, request, terminal) in [
            ("chat/completions", json!({"messages":[{"role":"user","content":"hi"}]}), "[DONE]"),
            ("responses", json!({"input":"hi"}), "response.completed"),
            ("messages", json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":16}), "message_stop"),
        ] {
            let mut request = request;
            request["model"] = json!("public");
            request["stream"] = json!(true);
            let response = client.post(format!("http://{gateway_addr}/v1/{endpoint}"))
                .bearer_auth("gateway-test").json(&request).send().await.unwrap();
            assert_eq!(response.status(), 200);
            let text = response.text().await.unwrap();
            assert!(text.contains("route ok") && text.contains(terminal), "{endpoint}: {text}");
            assert!(!text.contains("private"), "upstream model name leaked: {text}");
        }
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert!(seen.lock().unwrap().iter().all(|request| request["model"] == "private"));
        gateway.abort();
        upstream_server.abort();
        crate::bridge::tests::gateway_matrix().await;
        crate::store::reward_recovery_regressions();
        crate::wheel::tests::reward_regressions();
        assert_eq!(pool::inflight_odd_releases(), odd_before);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
