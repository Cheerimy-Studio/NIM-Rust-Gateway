//! 01Code 桌面客户端集成：广告下发、设备签名验证与官方客户端优惠。
//!
//! 广告只做展示，不参与计费；防伪签名失败时按普通调用处理，绝不阻断正常请求。
//! 共享密钥只从后台配置或 `CODING01_ATTEST_SECRET` 读取，不写入日志与接口响应。

use crate::{store::store, users, util};
use axum::{
    body::Bytes,
    extract::{ConnectInfo, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ADS: usize = 20;
const MAX_MODELS: usize = 50;
const ATTEST_TTL: i64 = 3600;
const CLOCK_SKEW: i64 = 300;
const NONCE_TTL: i64 = 600;
const MAX_TOKENS: usize = 5000;
const PLACEMENTS: &[&str] = &["modal", "corner", "inline", "tool"];
const LAYOUTS: &[&str] = &["card", "compact", "text"];
const IMAGE_RATIOS: &[&str] = &["wide", "square", "banner"];

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cfg() -> Value {
    store().load().get("config").cloned().unwrap_or_else(|| json!({}))
}

fn cfg_bool(cfg: &Value, key: &str, default: bool) -> bool {
    cfg.get(key).map(util::truthy).unwrap_or(default)
}

fn cfg_str(cfg: &Value, key: &str) -> String {
    util::str_or(cfg.get(key), "").trim().to_string()
}

fn parse_version(s: &str) -> Vec<u64> {
    let t = s.trim().trim_start_matches(['v', 'V']);
    let core = t.split(['-', '+']).next().unwrap_or("").trim();
    core.split('.')
        .filter(|p| !p.is_empty())
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
        .collect()
}

fn version_ge(a: &str, b: &str) -> bool {
    let a = parse_version(a);
    let b = parse_version(b);
    if b.is_empty() {
        return true;
    }
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    true
}

fn valid_version(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let core = s.trim_start_matches(['v', 'V']);
    let parts: Vec<&str> = core.split('.').collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn text_field(obj: &Map<String, Value>, key: &str, default: &str, max: usize) -> Result<String, String> {
    let raw = match obj.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => default.to_string(),
        Some(_) => return Err(format!("{key} 必须是文本")),
    };
    if raw.chars().count() > max {
        return Err(format!("{key} 不能超过 {max} 个字符"));
    }
    Ok(raw)
}

fn int_field(obj: &Map<String, Value>, key: &str, default: i64, min: i64, max: i64) -> Result<i64, String> {
    let n = match obj.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(default),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse::<i64>().unwrap_or(i64::MIN),
        Some(Value::Null) | None => default,
        _ => return Err(format!("{key} 必须是整数")),
    };
    if n < min || n > max {
        return Err(format!("{key} 必须在 {min} 到 {max} 之间"));
    }
    Ok(n)
}

fn bool_field(obj: &Map<String, Value>, key: &str, default: bool) -> bool {
    obj.get(key).map(util::truthy).unwrap_or(default)
}

fn accent(value: &str) -> Result<String, String> {
    let s = value.trim().trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("accent 必须是 6 位十六进制颜色，如 #315FE9".into());
    }
    Ok(format!("#{}", s.to_ascii_uppercase()))
}

fn safe_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    if raw.chars().count() > 500 {
        return Err("url 不能超过 500 个字符".into());
    }
    let u = reqwest::Url::parse(raw).map_err(|_| "url 不是有效链接")?;
    if !u.username().is_empty() || u.password().is_some() {
        return Err("url 不能包含账号密码".into());
    }
    let loopback = u
        .host_str()
        .map(|h| {
            h == "localhost"
                || h == "::1"
                || h.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
        })
        .unwrap_or(false);
    match u.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("url 必须使用 https（本机回环调试可用 http）".into()),
    }
    Ok(raw.to_string())
}

fn models_field(obj: &Map<String, Value>) -> Result<Vec<String>, String> {
    let Some(v) = obj.get("models") else { return Ok(Vec::new()) };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v.as_array().ok_or("models 必须是模型名数组")?;
    if arr.len() > MAX_MODELS {
        return Err(format!("models 最多 {MAX_MODELS} 个"));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in arr {
        let name = item.as_str().ok_or("models 只能包含字符串")?.trim();
        if name.is_empty() {
            continue;
        }
        if name.chars().count() > 128 {
            return Err("模型名不能超过 128 个字符".into());
        }
        if seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// 校验并规范化后台提交的广告 JSON；接受数组或 `{"ads":[...]}`。
pub fn validate_ads(value: &Value) -> Result<Vec<Value>, String> {
    let arr = match value {
        Value::Array(a) => a,
        Value::Object(o) => o
            .get("ads")
            .and_then(Value::as_array)
            .ok_or("广告 JSON 必须是数组或包含 ads 数组")?,
        Value::Null => return Ok(Vec::new()),
        _ => return Err("广告 JSON 必须是数组或包含 ads 数组".into()),
    };
    if arr.len() > MAX_ADS {
        return Err(format!("最多配置 {MAX_ADS} 条广告"));
    }
    let mut out = Vec::with_capacity(arr.len());
    let mut ids = HashSet::new();
    for (index, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("第 {} 条广告必须是对象", index + 1))?;
        let id = text_field(obj, "id", "", 64)?;
        if id.is_empty() {
            return Err(format!("第 {} 条广告缺少 id", index + 1));
        }
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(format!("广告 id 只能包含字母、数字、点、下划线和横线：{id}"));
        }
        if !ids.insert(id.clone()) {
            return Err(format!("广告 id 重复：{id}"));
        }
        // 长度按字符数校验；徽标/按钮允许 “01Code 专属” 这类中英混排，
        // 所以给到 16/12 字符，仍能保证客户端单行显示。
        let title = text_field(obj, "title", "", 40)?;
        if title.is_empty() {
            return Err(format!("广告 {id} 缺少 title"));
        }
        let body = text_field(obj, "body", "", 240)?;
        let badge = text_field(obj, "badge", "推荐", 16)?;
        let cta = text_field(obj, "cta", "了解详情", 12)?;
        let url = safe_url(&text_field(obj, "url", "", 500)?)?;
        let accent = accent(&text_field(obj, "accent", "#315FE9", 7)?)?;
        let weight = int_field(obj, "weight", 1, 1, 100)?;
        let max_displays = int_field(obj, "max_displays", 1, 1, 1000)?;
        let cooldown_secs = int_field(obj, "cooldown_secs", 86_400, 60, 2_592_000)?;
        let starts_at = int_field(obj, "starts_at", 0, 0, 4_102_444_800)?;
        let ends_at = int_field(obj, "ends_at", 0, 0, 4_102_444_800)?;
        if starts_at > 0 && ends_at > 0 && ends_at <= starts_at {
            return Err(format!("广告 {id} 的结束时间必须晚于开始时间"));
        }
        let min_version = text_field(obj, "min_version", "", 32)?;
        if !valid_version(&min_version) {
            return Err(format!("广告 {id} 的 min_version 格式无效"));
        }
        let placement = text_field(obj, "placement", "modal", 16)?.to_ascii_lowercase();
        if !PLACEMENTS.contains(&placement.as_str()) {
            return Err(format!("广告 {id} 的 placement 必须是 modal/corner/inline/tool"));
        }
        let dismiss_secs = int_field(obj, "dismiss_secs", 0, 0, 300)?;
        let image = safe_url(&text_field(obj, "image", "", 500)?)?;
        let layout = text_field(obj, "layout", "card", 16)?.to_ascii_lowercase();
        if !LAYOUTS.contains(&layout.as_str()) {
            return Err(format!("广告 {id} 的 layout 必须是 card/compact/text"));
        }
        let image_ratio = text_field(obj, "image_ratio", "wide", 16)?.to_ascii_lowercase();
        if !IMAGE_RATIOS.contains(&image_ratio.as_str()) {
            return Err(format!("广告 {id} 的 image_ratio 必须是 wide/square/banner"));
        }
        let models = models_field(obj)?;
        let enabled = bool_field(obj, "enabled", true);
        let free_only = bool_field(obj, "free_only", false);
        out.push(json!({
            "id": id,
            "title": title,
            "body": body,
            "badge": badge,
            "cta": cta,
            "url": url,
            "accent": accent,
            "weight": weight,
            "max_displays": max_displays,
            "cooldown_secs": cooldown_secs,
            "starts_at": starts_at,
            "ends_at": ends_at,
            "min_version": min_version,
            "placement": placement,
            "dismiss_secs": dismiss_secs,
            "image": image,
            "layout": layout,
            "image_ratio": image_ratio,
            "free_only": free_only,
            "models": models,
            "enabled": enabled,
        }));
    }
    Ok(out)
}

fn ads_from_cfg(cfg: &Value) -> Vec<Value> {
    let raw = cfg_str(cfg, "client01_ads");
    if raw.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => validate_ads(&v).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn ads_json(ads: &[Value]) -> String {
    if ads.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&Value::Array(ads.to_vec())).unwrap_or_default()
    }
}

fn stats_payload(cfg: &Value) -> Value {
    let db = store().load();
    let stats = db.get("client01_ad_stats").and_then(Value::as_object);
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for ad in ads_from_cfg(cfg) {
        let id = util::str_or(ad.get("id"), "");
        if id.is_empty() {
            continue;
        }
        seen.insert(id.clone());
        let st = stats.and_then(|m| m.get(&id));
        let impressions = st.and_then(|v| v.get("impressions")).and_then(Value::as_i64).unwrap_or(0);
        let unique = st.and_then(|v| v.get("unique")).and_then(Value::as_i64).unwrap_or(0);
        let clicks = st.and_then(|v| v.get("clicks")).and_then(Value::as_i64).unwrap_or(0);
        let closes = st.and_then(|v| v.get("closes")).and_then(Value::as_i64).unwrap_or(0);
        let ctr = if impressions > 0 { (clicks as f64 * 10000.0 / impressions as f64).round() / 100.0 } else { 0.0 };
        rows.push(json!({
            "id": id,
            "title": util::str_or(ad.get("title"), ""),
            "impressions": impressions,
            "unique": unique,
            "clicks": clicks,
            "closes": closes,
            "ctr": ctr,
            "last_at": st.and_then(|v| v.get("last")).and_then(Value::as_i64).unwrap_or(0),
        }));
    }
    if let Some(map) = stats {
        for (id, st) in map {
            if seen.contains(id) {
                continue;
            }
            let impressions = st.get("impressions").and_then(Value::as_i64).unwrap_or(0);
            let unique = st.get("unique").and_then(Value::as_i64).unwrap_or(0);
            let clicks = st.get("clicks").and_then(Value::as_i64).unwrap_or(0);
            let closes = st.get("closes").and_then(Value::as_i64).unwrap_or(0);
            let ctr = if impressions > 0 { (clicks as f64 * 10000.0 / impressions as f64).round() / 100.0 } else { 0.0 };
            rows.push(json!({
                "id": id,
                "title": format!("（已删除）{id}"),
                "impressions": impressions,
                "unique": unique,
                "clicks": clicks,
                "closes": closes,
                "ctr": ctr,
                "last_at": st.get("last").and_then(Value::as_i64).unwrap_or(0),
            }));
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.get("impressions").and_then(Value::as_i64).unwrap_or(0)));
    let sum = |key: &str| rows.iter().map(|r| r.get(key).and_then(Value::as_i64).unwrap_or(0)).sum::<i64>();
    json!({
        "totals": {
            "impressions": sum("impressions"),
            "unique": sum("unique"),
            "clicks": sum("clicks"),
            "closes": sum("closes"),
        },
        "ads": rows,
    })
}

/// 客户端上报一次广告曝光/点击/关闭。按广告累计，并用 (广告, 用户) 去重统计独立用户。
pub async fn ad_event(headers: HeaderMap, body: Bytes) -> Response {
    let key = bearer(&headers);
    if key.is_empty() {
        return error(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let Some((user, _)) = users::lookup_user_key(&key) else {
        return error(StatusCode::UNAUTHORIZED, "invalid_api_key");
    };
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    let Some(obj) = v.as_object() else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    let ad_id = util::str_or(obj.get("ad_id"), "").trim().to_string();
    let event = util::str_or(obj.get("event"), "").trim().to_ascii_lowercase();
    if ad_id.is_empty() || !matches!(event.as_str(), "impression" | "click" | "close") {
        return error(StatusCode::BAD_REQUEST, "invalid_event");
    }
    let cfg = cfg();
    if !ads_from_cfg(&cfg).iter().any(|ad| util::str_or(ad.get("id"), "") == ad_id) {
        return error(StatusCode::BAD_REQUEST, "unknown_ad");
    }
    let user_id = util::str_or(user.get("id"), "");
    let now = now();
    store().update(|db| {
        if !db.get("client01_ad_stats").map(Value::is_object).unwrap_or(false) {
            db["client01_ad_stats"] = json!({});
        }
        if !db.get("client01_ad_seen").map(Value::is_array).unwrap_or(false) {
            db["client01_ad_seen"] = json!([]);
        }
        {
            let Some(stats) = db.get_mut("client01_ad_stats").and_then(Value::as_object_mut) else {
                return;
            };
            let entry = stats.entry(ad_id.clone()).or_insert_with(|| json!({
                "impressions": 0, "unique": 0, "clicks": 0, "closes": 0, "last": 0
            }));
            let field = match event.as_str() {
                "impression" => "impressions",
                "click" => "clicks",
                _ => "closes",
            };
            let count = entry.get(field).and_then(Value::as_i64).unwrap_or(0) + 1;
            if let Some(o) = entry.as_object_mut() {
                o.insert(field.into(), json!(count));
                o.insert("last".into(), json!(now));
            }
        }
        if event == "impression" {
            let unique_added = {
                let seen = db.get_mut("client01_ad_seen").and_then(Value::as_array_mut);
                if let Some(arr) = seen {
                    let exists = arr.iter().any(|r| {
                        util::str_or(r.get("ad"), "") == ad_id
                            && util::str_or(r.get("user"), "") == user_id
                    });
                    if exists {
                        false
                    } else {
                        arr.push(json!({"ad": ad_id, "user": user_id, "at": now}));
                        if arr.len() > 50_000 {
                            let drop = arr.len() - 50_000;
                            arr.drain(0..drop);
                        }
                        true
                    }
                } else {
                    false
                }
            };
            if unique_added {
                if let Some(entry) = db
                    .get_mut("client01_ad_stats")
                    .and_then(Value::as_object_mut)
                    .and_then(|m| m.get_mut(&ad_id))
                    .and_then(Value::as_object_mut)
                {
                    let n = entry.get("unique").and_then(Value::as_i64).unwrap_or(0) + 1;
                    entry.insert("unique".into(), json!(n));
                }
            }
        }
    });
    store().flush();
    no_store(axum::Json(json!({"ok": true})).into_response())
}

pub fn client_version(headers: &HeaderMap) -> String {
    headers
        .get("x-01code-client")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .strip_prefix("01Code/")
        .unwrap_or("")
        .trim()
        .to_string()
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    resp.headers_mut()
        .insert("pragma", "no-cache".parse().unwrap());
    resp
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .strip_prefix("Bearer ")
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 客户端广告接口。只返回启用、在时间窗内且满足最低版本的广告。
/// `free`/`model` 查询参数用于按当前模型预过滤；客户端仍会再过滤一次。
pub async fn ads(headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Response {
    let key = bearer(&headers);
    if key.is_empty() || users::lookup_user_key(&key).is_none() {
        return no_store(error(StatusCode::UNAUTHORIZED, "invalid_api_key"));
    }
    let cfg = cfg();
    if !cfg_bool(&cfg, "client01_ads_enabled", true) {
        return no_store(crate::webhttp::with_cors(axum::Json(json!({"ads": []})).into_response()));
    }
    let version = client_version(&headers);
    if version.is_empty() {
        return no_store(crate::webhttp::with_cors(axum::Json(json!({"ads": []})).into_response()));
    }
    // 只有客户端明确声明当前模型免费时才下发免费模型专属广告；
    // 旧客户端不带该参数，按付费处理，避免误投。
    let free = query
        .get("free")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let model = query.get("model").map(|v| v.trim()).unwrap_or("");
    let global_free_only = cfg_bool(&cfg, "client01_ads_free_only", false);
    let now = now();
    let mut out = Vec::new();
    for ad in ads_from_cfg(&cfg) {
        if !ad.get("enabled").map(util::truthy).unwrap_or(true) {
            continue;
        }
        let starts = ad.get("starts_at").and_then(Value::as_i64).unwrap_or(0);
        let ends = ad.get("ends_at").and_then(Value::as_i64).unwrap_or(0);
        if starts > 0 && now < starts {
            continue;
        }
        if ends > 0 && now > ends {
            continue;
        }
        let min = util::str_or(ad.get("min_version"), "");
        if !version_ge(&version, &min) {
            continue;
        }
        if (global_free_only || util::truthy(ad.get("free_only").unwrap_or(&Value::Bool(false)))) && !free {
            continue;
        }
        if !model.is_empty() {
            if let Some(models) = ad.get("models").and_then(Value::as_array) {
                if !models.is_empty() && !models.iter().any(|m| m.as_str() == Some(model)) {
                    continue;
                }
            }
        }
        out.push(ad);
    }
    no_store(crate::webhttp::with_cors(
        axum::Json(json!({"ads": out})).into_response(),
    ))
}

// ---------------------------------------------------------------- attest 防伪

fn secret(cfg: &Value) -> String {
    let configured = cfg_str(cfg, "client01_attest_secret");
    if !configured.is_empty() {
        return configured;
    }
    std::env::var("CODING01_ATTEST_SECRET").unwrap_or_default().trim().to_string()
}

pub fn secret_set(cfg: &Value) -> bool {
    !secret(cfg).is_empty()
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(s.trim())
        .or_else(|_| STANDARD.decode(s.trim()))
        .ok()
}

fn hash_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn nonce_cache() -> &'static Mutex<HashMap<String, i64>> {
    static C: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn nonce_seen(key: &str, nonce: &str, now: i64) -> bool {
    let mut cache = nonce_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|_, t| now - *t <= NONCE_TTL);
    cache.contains_key(&format!("{key}:{nonce}"))
}

fn nonce_mark(key: &str, nonce: &str, now: i64) {
    let mut cache = nonce_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(format!("{key}:{nonce}"), now);
}

fn rate_cache() -> &'static Mutex<HashMap<String, Vec<i64>>> {
    static C: OnceLock<Mutex<HashMap<String, Vec<i64>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn attest_rate_ok(ip: &str) -> bool {
    let now = now();
    let mut cache = rate_cache().lock().unwrap_or_else(|e| e.into_inner());
    let list = cache.entry(ip.to_string()).or_default();
    list.retain(|t| now - *t < 60);
    if list.len() >= 30 {
        return false;
    }
    list.push(now);
    true
}

fn device_by_key(key_id: &str) -> Option<Value> {
    store()
        .load()
        .get("client01_devices")
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find(|d| util::str_or(d.get("key_id"), "") == key_id).cloned())
}

fn register_device(key_id: &str, public_key: &str, user_id: &str, version: &str) -> Result<(), String> {
    if let Some(existing) = device_by_key(key_id) {
        let stored = util::str_or(existing.get("public_key"), "");
        if stored != public_key {
            return Err("key_id 与已登记公钥不一致".into());
        }
    }
    let mut failed = None;
    store().update(|db| {
        if !db.get("client01_devices").map(Value::is_array).unwrap_or(false) {
            db["client01_devices"] = json!([]);
        }
        let Some(arr) = db.get_mut("client01_devices").and_then(Value::as_array_mut) else {
            failed = Some("设备表不可用".to_string());
            return;
        };
        if let Some(row) = arr
            .iter_mut()
            .find(|d| util::str_or(d.get("key_id"), "") == key_id)
        {
            row["public_key"] = Value::from(public_key);
            row["user_id"] = Value::from(user_id);
            row["version"] = Value::from(version);
            row["last_seen"] = json!(now());
            return;
        }
        arr.push(json!({
            "key_id": key_id,
            "public_key": public_key,
            "user_id": user_id,
            "version": version,
            "created": now(),
            "last_seen": now(),
        }));
    });
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn issue_token(key_id: &str, user_id: &str, version: &str) -> Result<(String, i64), String> {
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let hash = hash_hex(token.as_bytes());
    let expires = now() + ATTEST_TTL;
    let mut failed = None;
    store().update(|db| {
        if !db.get("client01_tokens").map(Value::is_array).unwrap_or(false) {
            db["client01_tokens"] = json!([]);
        }
        let Some(arr) = db.get_mut("client01_tokens").and_then(Value::as_array_mut) else {
            failed = Some("attest 令牌表不可用".to_string());
            return;
        };
        let now = now();
        arr.retain(|t| t.get("expires_at").and_then(Value::as_i64).unwrap_or(0) > now);
        arr.push(json!({
            "hash": hash,
            "key_id": key_id,
            "user_id": user_id,
            "version": version,
            "created": now,
            "expires_at": expires,
        }));
        if arr.len() > MAX_TOKENS {
            let drop = arr.len() - MAX_TOKENS;
            arr.drain(0..drop);
        }
    });
    match failed {
        Some(e) => Err(e),
        None => Ok((token, ATTEST_TTL)),
    }
}

fn token_valid(token: &str, key_id: &str, user_id: &str) -> bool {
    let hash = hash_hex(token.as_bytes());
    let now = now();
    store()
        .load()
        .get("client01_tokens")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter().any(|t| {
                util::str_or(t.get("hash"), "") == hash
                    && util::str_or(t.get("key_id"), "") == key_id
                    && util::str_or(t.get("user_id"), "") == user_id
                    && t.get("expires_at").and_then(Value::as_i64).unwrap_or(0) > now
            })
        })
        .unwrap_or(false)
}

fn proof_ok(secret: &str, text: &str, proof: &[u8]) -> bool {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(text.as_bytes());
    mac.verify_slice(proof).is_ok()
}

/// `POST /api/client/attest`：校验官方构建 HMAC proof，登记设备公钥并签发短期令牌。
pub async fn attest(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let cfg = cfg();
    if !cfg_bool(&cfg, "client01_attest_enabled", false) {
        return error(StatusCode::NOT_FOUND, "not_enabled");
    }
    let key = bearer(&headers);
    if key.is_empty() {
        return error(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let Some((user, _token)) = users::lookup_user_key(&key) else {
        return error(StatusCode::UNAUTHORIZED, "invalid_api_key");
    };
    let secret = secret(&cfg);
    if secret.is_empty() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "attest secret not configured");
    }
    if !attest_rate_ok(&addr.ip().to_string()) {
        return error(StatusCode::TOO_MANY_REQUESTS, "too_many_requests");
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    let Some(obj) = v.as_object() else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    if util::str_or(obj.get("client_id"), "") != "01code" {
        return error(StatusCode::BAD_REQUEST, "invalid_client");
    }
    let version = util::str_or(obj.get("version"), "").trim().to_string();
    let public_key = util::str_or(obj.get("public_key"), "").trim().to_string();
    let key_id = util::str_or(obj.get("key_id"), "").trim().to_string();
    let timestamp = obj.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
    let nonce = util::str_or(obj.get("nonce"), "").trim().to_string();
    let proof = util::str_or(obj.get("proof"), "").trim().to_string();
    if version.is_empty() || version.chars().count() > 32 || !valid_version(&version) {
        return error(StatusCode::BAD_REQUEST, "invalid_version");
    }
    if nonce.len() < 8 || nonce.len() > 128 {
        return error(StatusCode::BAD_REQUEST, "invalid_nonce");
    }
    let now = now();
    if (now - timestamp).abs() > CLOCK_SKEW {
        return error(StatusCode::BAD_REQUEST, "stale_timestamp");
    }
    if nonce_seen(&format!("attest:{key_id}"), &nonce, now) {
        return error(StatusCode::BAD_REQUEST, "nonce_replayed");
    }
    let Some(public_bytes) = b64_decode(&public_key).filter(|b| b.len() == 32) else {
        return error(StatusCode::BAD_REQUEST, "invalid_public_key");
    };
    let Some(key_bytes) = b64_decode(&key_id).filter(|b| b.len() == 16) else {
        return error(StatusCode::BAD_REQUEST, "invalid_key_id");
    };
    let digest = Sha256::digest(&public_bytes);
    if key_bytes.as_slice() != &digest[..16] {
        return error(StatusCode::BAD_REQUEST, "key_id_mismatch");
    }
    let Some(proof_bytes) = b64_decode(&proof) else {
        return error(StatusCode::BAD_REQUEST, "invalid_proof");
    };
    let text = format!("01code\n{version}\n{public_key}\n{key_id}\n{timestamp}\n{nonce}");
    if !proof_ok(&secret, &text, &proof_bytes) {
        return error(StatusCode::UNAUTHORIZED, "invalid_proof");
    }
    nonce_mark(&format!("attest:{key_id}"), &nonce, now);
    let user_id = util::str_or(user.get("id"), "");
    if let Err(e) = register_device(&key_id, &public_key, &user_id, &version) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    match issue_token(&key_id, &user_id, &version) {
        Ok((token, expires_in)) => no_store(axum::Json(json!({
            "attest_token": token,
            "expires_in": expires_in,
        })).into_response()),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}

/// 验证一次模型请求的 01Code 设备签名与 attest token。
/// 任何一步失败都返回 false，调用方按普通请求处理，不拒绝请求。
pub fn verify_request(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &[u8],
    user_id: &str,
) -> bool {
    let cfg = cfg();
    if !cfg_bool(&cfg, "client01_attest_enabled", false) {
        return false;
    }
    let version = client_version(headers);
    if version.is_empty() || !valid_version(&version) {
        return false;
    }
    if headers.get("x-01code-sig-version").and_then(|v| v.to_str().ok()) != Some("1") {
        return false;
    }
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let key_id = get("x-01code-key-id");
    let public_key = get("x-01code-public-key");
    let timestamp = get("x-01code-timestamp").parse::<i64>().unwrap_or(0);
    let nonce = get("x-01code-nonce");
    let signature = get("x-01code-signature");
    if key_id.is_empty() || public_key.is_empty() || nonce.is_empty() || signature.is_empty() {
        return false;
    }
    let now = now();
    if (now - timestamp).abs() > CLOCK_SKEW {
        return false;
    }
    let Some(device) = device_by_key(&key_id) else {
        return false;
    };
    let stored_key = util::str_or(device.get("public_key"), "");
    use subtle::ConstantTimeEq;
    if stored_key.as_bytes().ct_eq(public_key.as_bytes()).unwrap_u8() != 1 {
        return false;
    }
    if nonce_seen(&key_id, &nonce, now) {
        return false;
    }
    let Some(sig_bytes) = b64_decode(&signature).filter(|b| b.len() == 64) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    let Some(pk_bytes) = b64_decode(&public_key).filter(|b| b.len() == 32) else {
        return false;
    };
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&pk_bytes);
    let Ok(verifying) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    let body_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
    let canonical = format!("{method}\n{path}\n{timestamp}\n{nonce}\n{body_hash}");
    if verifying.verify(canonical.as_bytes(), &sig).is_err() {
        return false;
    }
    nonce_mark(&key_id, &nonce, now);
    let attest_token = get("x-01code-attest");
    if attest_token.is_empty() {
        return false;
    }
    token_valid(&attest_token, &key_id, user_id)
}

// ---------------------------------------------------------------- 计费优惠

fn free_models(cfg: &Value) -> Vec<String> {
    cfg_str(cfg, "client01_free_models")
        .split(['\n', ',', '，'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 模型在 01Code 客户端的优惠标记：(是否限免, 折扣百分比)。
/// 只有启用 attest 且版本满足最低优惠版本时才展示，避免承诺未生效的优惠。
pub fn model_benefits(cfg: &Value, model: &str, client_version: &str) -> (bool, f64) {
    if !cfg_bool(cfg, "client01_attest_enabled", false) {
        return (false, 0.0);
    }
    let min = cfg_str(cfg, "client01_discount_min_version");
    if !min.is_empty() && !version_ge(client_version, &min) {
        return (false, 0.0);
    }
    if free_models(cfg).iter().any(|m| m == model) {
        return (true, 0.0);
    }
    let ratio = cfg
        .get("client01_discount_ratio")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .clamp(0.1, 1.0);
    if ratio < 1.0 {
        let percent = ((1.0 - ratio) * 100.0 * 10.0).round() / 10.0;
        (false, percent)
    } else {
        (false, 0.0)
    }
}

/// 返回应用到该模型的价格倍率；None 表示不调整。
pub fn pricing_ratio(cfg: &Value, model: &str, client_version: &str, verified: bool) -> Option<f64> {
    if !verified {
        return None;
    }
    let min = cfg_str(cfg, "client01_discount_min_version");
    if !min.is_empty() && !version_ge(client_version, &min) {
        return None;
    }
    if free_models(cfg).iter().any(|m| m == model) {
        return Some(0.0);
    }
    let ratio = cfg
        .get("client01_discount_ratio")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .clamp(0.1, 1.0);
    if ratio < 1.0 {
        Some(ratio)
    } else {
        None
    }
}

fn scale_number(v: &Value, ratio: f64) -> Value {
    match v.as_f64() {
        Some(n) => json!(util::round6(n * ratio)),
        None => v.clone(),
    }
}

/// 按倍率缩放价格对象（按次、按 token、缓存价与长度档位）。
pub fn scale_pricing(pricing: &Value, ratio: f64) -> Value {
    if ratio >= 1.0 {
        return pricing.clone();
    }
    if pricing.is_number() {
        return scale_number(pricing, ratio);
    }
    let Some(obj) = pricing.as_object() else {
        return pricing.clone();
    };
    let mut out = obj.clone();
    for key in ["price", "input", "output", "cache"] {
        if let Some(v) = out.get(key).cloned() {
            out.insert(key.into(), scale_number(&v, ratio));
        }
    }
    if let Some(tiers) = out.get("tiers").and_then(Value::as_array).cloned() {
        let tiers: Vec<Value> = tiers
            .iter()
            .map(|tier| {
                let Some(t) = tier.as_object() else { return tier.clone() };
                let mut t = t.clone();
                for key in ["input", "output", "cache"] {
                    if let Some(v) = t.get(key).cloned() {
                        t.insert(key.into(), scale_number(&v, ratio));
                    }
                }
                Value::Object(t)
            })
            .collect();
        out.insert("tiers".into(), Value::Array(tiers));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------- 后台 API

fn admin_payload(cfg: &Value) -> Value {
    json!({
        "ads": ads_from_cfg(cfg),
        "stats": stats_payload(cfg),
        "config": {
            "ads_enabled": cfg_bool(cfg, "client01_ads_enabled", true),
            "ads_free_only": cfg_bool(cfg, "client01_ads_free_only", false),
            "attest_enabled": cfg_bool(cfg, "client01_attest_enabled", false),
            "secret_set": secret_set(cfg),
            "discount_ratio": cfg.get("client01_discount_ratio").and_then(Value::as_f64).unwrap_or(1.0),
            "discount_min_version": cfg_str(cfg, "client01_discount_min_version"),
            "free_models": cfg_str(cfg, "client01_free_models"),
            "device_login_enabled": cfg_bool(cfg, "device_login_enabled", true),
            "public_origin": cfg_str(cfg, "public_origin"),
        }
    })
}

pub async fn admin_get(headers: &HeaderMap) -> Response {
    if let Err(e) = crate::admin::require(headers, false) {
        return e;
    }
    axum::Json(admin_payload(&cfg())).into_response()
}

fn normalize_models(text: &str) -> Result<String, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in text.split(['\n', ',', '，']) {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        if name.chars().count() > 128 {
            return Err("模型名不能超过 128 个字符".into());
        }
        if seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    if out.len() > 200 {
        return Err("01Code 免费模型最多 200 个".into());
    }
    Ok(out.join("\n"))
}

pub async fn admin_config_save(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = crate::admin::require(headers, true) {
        return e;
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "请求体格式错误");
    };
    let Some(obj) = v.as_object() else {
        return error(StatusCode::BAD_REQUEST, "请求体格式错误");
    };
    let incoming = obj.get("config").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut ads_enabled = None;
    let mut free_only = None;
    let mut attest_enabled = None;
    let mut ratio = None;
    let mut min_version = None;
    let mut free_models = None;
    if let Some(v) = incoming.get("ads_enabled") {
        ads_enabled = Some(util::as_bool(v, true));
    }
    if let Some(v) = incoming.get("ads_free_only") {
        free_only = Some(util::as_bool(v, false));
    }
    if let Some(v) = incoming.get("attest_enabled") {
        attest_enabled = Some(util::as_bool(v, false));
    }
    if let Some(v) = incoming.get("discount_ratio") {
        let r = v.as_f64().or_else(|| util::str_or(Some(v), "").parse::<f64>().ok()).unwrap_or(1.0);
        if !(0.1..=1.0).contains(&r) {
            return error(StatusCode::BAD_REQUEST, "折扣比例必须在 10% 到 100% 之间");
        }
        ratio = Some(util::round6(r));
    }
    if let Some(v) = incoming.get("discount_min_version") {
        let s = util::str_or(Some(v), "").trim().to_string();
        if !valid_version(&s) {
            return error(StatusCode::BAD_REQUEST, "最低优惠版本格式无效");
        }
        min_version = Some(s);
    }
    if let Some(v) = incoming.get("free_models") {
        match normalize_models(&util::str_or(Some(v), "")) {
            Ok(s) => free_models = Some(s),
            Err(e) => return error(StatusCode::BAD_REQUEST, &e),
        }
    }
    let secret_in = util::str_or(obj.get("secret"), "").trim().to_string();
    let clear_secret = obj.get("clear_secret").map(util::truthy).unwrap_or(false);
    if !secret_in.is_empty() && secret_in.chars().count() < 16 {
        return error(StatusCode::BAD_REQUEST, "共享密钥至少 16 个字符");
    }
    let cfg_now = cfg();
    let will_have_secret = if clear_secret {
        false
    } else if !secret_in.is_empty() {
        true
    } else {
        secret_set(&cfg_now)
    };
    let effective_attest = attest_enabled.unwrap_or_else(|| cfg_bool(&cfg_now, "client01_attest_enabled", false));
    if effective_attest && !will_have_secret {
        return error(StatusCode::BAD_REQUEST, "启用防伪验证前请先配置共享密钥");
    }
    store().update(|db| {
        let Some(cfg) = db.get_mut("config").and_then(Value::as_object_mut) else { return };
        if let Some(v) = ads_enabled {
            cfg.insert("client01_ads_enabled".into(), Value::Bool(v));
        }
        if let Some(v) = free_only {
            cfg.insert("client01_ads_free_only".into(), Value::Bool(v));
        }
        if let Some(v) = attest_enabled {
            cfg.insert("client01_attest_enabled".into(), Value::Bool(v));
        }
        if let Some(v) = ratio {
            cfg.insert("client01_discount_ratio".into(), json!(v));
        }
        if let Some(v) = min_version {
            cfg.insert("client01_discount_min_version".into(), Value::String(v));
        }
        if let Some(v) = free_models {
            cfg.insert("client01_free_models".into(), Value::String(v));
        }
        if clear_secret {
            cfg.insert("client01_attest_secret".into(), Value::String(String::new()));
        } else if !secret_in.is_empty() {
            cfg.insert("client01_attest_secret".into(), Value::String(secret_in));
        }
    });
    store().flush();
    crate::proxy::invalidate_cfg_cache();
    axum::Json(admin_payload(&cfg())).into_response()
}

pub async fn admin_ads_save(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = crate::admin::require(headers, true) {
        return e;
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "请求体格式错误");
    };
    let ads = match validate_ads(&v) {
        Ok(ads) => ads,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let text = ads_json(&ads);
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(Value::as_object_mut) {
            cfg.insert("client01_ads".into(), Value::String(text));
        }
    });
    store().flush();
    crate::proxy::invalidate_cfg_cache();
    axum::Json(admin_payload(&cfg())).into_response()
}

pub async fn admin_stats_reset(headers: &HeaderMap) -> Response {
    if let Err(e) = crate::admin::require(headers, true) {
        return e;
    }
    store().update(|db| {
        db["client01_ad_stats"] = json!({});
        db["client01_ad_seen"] = json!([]);
    });
    store().flush();
    axum::Json(admin_payload(&cfg())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!([{
            "id": "demo",
            "title": "01Code 专属优惠",
            "body": "通过官方客户端调用模型可享优惠。",
            "url": "https://ai.gzmath.cn",
            "accent": "#315fe9",
            "placement": "corner",
            "free_only": true,
            "models": ["auto", "auto"]
        }])
    }

    #[test]
    fn ad_validation_normalizes_and_rejects_bad_input() {
        let ads = validate_ads(&sample()).unwrap();
        assert_eq!(ads[0]["accent"], "#315FE9");
        assert_eq!(ads[0]["placement"], "corner");
        assert_eq!(ads[0]["models"], json!(["auto"]));
        assert_eq!(ads[0]["cooldown_secs"], 86400);
        for bad in [
            json!([{"id":"x","title":"t","url":"http://example.com"}]),
            json!([{"id":"x","title":"t","accent":"red"}]),
            json!([{"id":"x","title":"t","placement":"popup"}]),
            json!([{"id":"x","title":"t"},{"id":"x","title":"u"}]),
        ] {
            assert!(validate_ads(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn version_and_pricing() {
        assert!(version_ge("0.0.2", "0.0.1"));
        assert!(!version_ge("0.0.1", "0.0.2"));
        assert!(valid_version(""));
        assert!(valid_version("1.2.3"));
        assert!(!valid_version("1.x"));
        let cfg = json!({
            "client01_discount_ratio": 0.5,
            "client01_free_models": "free-a\nfree-b"
        });
        assert_eq!(pricing_ratio(&cfg, "paid", "0.0.2", true), Some(0.5));
        assert_eq!(pricing_ratio(&cfg, "free-a", "0.0.2", true), Some(0.0));
        assert_eq!(pricing_ratio(&cfg, "paid", "0.0.2", false), None);
        let p = json!({"mode":"token","input":2,"output":8,"cache":1,"tiers":[{"min_input":1000,"input":4,"output":16}]});
        let scaled = scale_pricing(&p, 0.5);
        assert_eq!(scaled["input"], json!(1.0));
        assert_eq!(scaled["output"], json!(4.0));
        assert_eq!(scaled["cache"], json!(0.5));
        assert_eq!(scaled["tiers"][0]["input"], json!(2.0));
        assert_eq!(scaled["tiers"][0]["min_input"], json!(1000));
    }

    #[test]
    fn canonical_signature_shape() {
        let body = b"{}";
        let hash = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
        let canonical = format!("POST\n/v1/chat/completions\n1760000000\nabc\n{hash}");
        assert!(canonical.starts_with("POST\n/v1/chat/completions\n1760000000\nabc\n"));
    }
}
