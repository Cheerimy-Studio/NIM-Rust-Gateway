//! HTTP 公共层：CORS、错误响应、静态页面、上游客户端注册表。

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::RwLock;

pub const MAX_BODY: usize = 20 * 1024 * 1024;
pub const VERSION: &str = "1.7.4";

pub const FALLBACK_MODELS: &[&str] = &[
    "deepseek-ai/deepseek-r1",
    "deepseek-ai/deepseek-v3",
    "meta/llama-3.3-70b-instruct",
    "meta/llama-3.1-70b-instruct",
    "qwen/qwen2.5-coder-32b-instruct",
    "microsoft/phi-4",
    "nvidia/llama-3.1-nemotron-70b-instruct",
];

/// web/ 目录的静态资源（与 Python 版共用同一批文件，UI 不变）。
pub mod web_assets {
    // 前端随仓库分发（web/ 在仓库根），编译期内嵌进二进制
    pub const ADMIN_HTML: &str = include_str!("../web/admin.html");
    pub const ADMIN_JS: &str = include_str!("../web/admin.js");
    pub const LANDING_HTML: &str = include_str!("../web/landing.html");
    pub const LOGIN_HTML: &str = include_str!("../web/login.html");
    pub const QUEUE_HTML: &str = include_str!("../web/queue.html");
}

pub fn cors_headers() -> Vec<(String, String)> {
    vec![
        ("Access-Control-Allow-Origin".into(), "*".into()),
        ("Access-Control-Allow-Methods".into(), "GET, POST, OPTIONS".into()),
        (
            "Access-Control-Allow-Headers".into(),
            "Authorization, Content-Type, X-API-Key, X-Request-Id, anthropic-version, \
             anthropic-beta, anthropic-dangerous-direct-browser-access, x-stainless-lang, \
             x-stainless-package-version, x-stainless-os, x-stainless-arch, x-stainless-runtime, \
             x-stainless-runtime-version, x-stainless-retry-count, x-stainless-timeout, \
             x-requested-with"
                .into(),
        ),
        ("Access-Control-Max-Age".into(), "86400".into()),
        ("Cache-Control".into(), "no-store".into()),
    ]
}

fn apply_headers(resp: &mut Response, headers: Vec<(String, String)>) {
    let hm = resp.headers_mut();
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::try_from(k.as_str()),
            HeaderValue::from_str(&v),
        ) {
            hm.insert(name, val);
        }
    }
}

pub fn with_cors(mut resp: Response) -> Response {
    apply_headers(&mut resp, cors_headers());
    resp
}

/// OpenAI 风格错误 JSON（与 Python _error 完全一致）。
pub fn error_resp(
    status: u16,
    message: &str,
    type_: &str,
    code: Option<&str>,
    anthropic: bool,
) -> Response {
    let mut type_ = type_.to_string();
    if anthropic {
        let t = match status {
            404 => "not_found_error",
            400 => "invalid_request_error",
            401 => "authentication_error",
            429 => "rate_limit_error",
            s if s >= 500 => "api_error",
            _ => "api_error",
        };
        let body = serde_json::json!({"type": "error", "error": {"type": t, "message": message}});
        return with_cors(
            (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(body)).into_response(),
        );
    }
    if type_ == "invalid_request_error" {
        if status == 429 {
            type_ = "rate_limit_error".into();
        } else if status == 502 || status == 503 || status >= 500 {
            type_ = "server_error".into();
        }
    }
    let body = serde_json::json!({
        "error": {"message": message, "type": type_, "param": null, "code": code},
    });
    with_cors(
        (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(body)).into_response(),
    )
}

pub fn json_resp(v: Value) -> Response {
    with_cors(axum::Json(v).into_response())
}

pub fn text_resp(content: &str, content_type: &str, no_cache: bool) -> Response {
    let mut resp = content.to_string().into_response();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        resp.headers_mut().insert(axum::http::header::CONTENT_TYPE, v);
    }
    if no_cache {
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    resp
}

/// 令牌遮罩：可辨识但不泄露完整令牌。
pub fn tok_mask(t: &str) -> String {
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= 14 {
        return t.to_string();
    }
    let head: String = chars[..10].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}…{}", head, tail)
}

// ---------------------------------------------------------------- 上游客户端注册表

struct HttpEntry {
    verify_tls: bool,
    connect_to: i64,
    client: reqwest::Client,
}

static HTTP_CLIENTS: RwLock<Option<Vec<HttpEntry>>> = RwLock::new(None);

/// 实际生效的「连接池上限」（容量语义与 Python 一致：max(配置值, 账号数×并发+50)）。
/// reqwest 没有全局硬上限，因此这个值仅作为容量水位展示与告警依据 —— 满负荷时
/// 连接按需新建而不是抛 PoolTimeout，这正是迁移要消除的故障模式。
pub static POOL_CAP: AtomicI64 = AtomicI64::new(0);

pub fn pool_size(cfg: &Value, accounts: i64) -> i64 {
    let mut configured = crate::util::int_or(cfg.get("pool_max_connections"), 400);
    if configured < 50 {
        configured = 50;
    }
    let mut acct_conc = crate::util::int_or(cfg.get("acct_concurrency"), 0);
    if acct_conc <= 0 {
        acct_conc = 4; // 账号并发未限制：按每账号 4 条在途粗估
    }
    let need = accounts.max(0) * acct_conc + 50;
    configured.max(need)
}

fn build_client(verify_tls: bool, connect_to: i64) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_nodelay(true);
    if !verify_tls {
        b = b.danger_accept_invalid_certs(true);
    }
    if connect_to > 0 {
        b = b.connect_timeout(std::time::Duration::from_secs(connect_to as u64));
    }
    b.build().unwrap_or_default()
}

/// 取（或建）一个与 (verify_tls, connect_timeout) 匹配的共享客户端。
pub fn get_http(verify_tls: bool, connect_to: i64) -> reqwest::Client {
    {
        let guard = HTTP_CLIENTS.read().unwrap();
        if let Some(list) = guard.as_ref() {
            for e in list {
                if e.verify_tls == verify_tls && e.connect_to == connect_to {
                    return e.client.clone();
                }
            }
        }
    }
    let client = build_client(verify_tls, connect_to);
    let mut guard = HTTP_CLIENTS.write().unwrap();
    let list = guard.get_or_insert_with(Vec::new);
    for e in list.iter() {
        if e.verify_tls == verify_tls && e.connect_to == connect_to {
            return e.client.clone();
        }
    }
    list.push(HttpEntry {
        verify_tls,
        connect_to,
        client: client.clone(),
    });
    client
}

pub fn init_http(cfg: &Value) {
    let verify = cfg.get("verify_tls").map(crate::util::truthy).unwrap_or(true);
    let _ = get_http(verify, 10);
    let accounts = crate::store::store()
        .load()
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|a| a.iter().filter(|x| x.get("enabled").map(crate::util::truthy).unwrap_or(false)).count() as i64)
        .unwrap_or(0);
    let cap = pool_size(cfg, accounts);
    POOL_CAP.store(cap, Ordering::SeqCst);
    eprintln!(
        "[http] 连接池上限 {}(配置 {} / 账号 {})",
        cap,
        crate::util::int_or(cfg.get("pool_max_connections"), 400),
        accounts
    );
}

/// 每次取客户端时按配置刷新容量水位（看门狗每 30s 也会重算）。
pub fn refresh_pool_cap() {
    let db = crate::store::store().load();
    let cfg = match db.get("config") {
        Some(c) => c.clone(),
        None => return,
    };
    let accounts = db
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|a| a.iter().filter(|x| x.get("enabled").map(crate::util::truthy).unwrap_or(false)).count() as i64)
        .unwrap_or(0);
    let cap = pool_size(&cfg, accounts);
    POOL_CAP.store(cap, Ordering::SeqCst);
}

#[allow(unused)]
fn _touch(h: HashMap<String, String>) -> HashMap<String, String> {
    h
}
