//! HTTP 公共层：CORS、错误响应、静态页面、上游客户端注册表。

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::RwLock;

pub const MAX_BODY: usize = 20 * 1024 * 1024;
// 跟随 Cargo.toml：历史硬编码导致 1.7.x 各版资源 URL 不变，浏览器一直用旧缓存的 admin.js
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A browser-supplied Cookie is not a CSRF proof. Require a separate token header.
pub fn csrf_header_matches(headers: &HeaderMap, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    headers.get("x-csrf").and_then(|value| value.to_str().ok())
        .map(str::trim).filter(|value| !value.is_empty())
        .is_some_and(|value| !expected.is_empty() && bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
}

pub fn constant_time_token(expected: &str, actual: &str) -> bool {
    use subtle::ConstantTimeEq;
    !expected.is_empty() && bool::from(expected.as_bytes().ct_eq(actual.as_bytes()))
}

/// 登录类端点只接受 application/json：浏览器跨站表单只能发
/// application/x-www-form-urlencoded / multipart / text/plain，挡掉这三类即可
/// 阻断「登录 CSRF」（把受害者静默登入攻击者账号）。
pub fn login_content_type_ok(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false)
}

fn origin_allowlisted(origin: &str, raw: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(origin) else {
        return false;
    };
    let host = u
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    for item in raw.split(|c: char| c == ',' || c == '\n' || c == ';' || c.is_whitespace()) {
        let item = item.trim().trim_end_matches('/');
        if item.is_empty() {
            continue;
        }
        if item == "*" {
            return true;
        }
        if item.contains("://") {
            let Ok(a) = reqwest::Url::parse(item) else { continue };
            let a_host = a
                .host_str()
                .unwrap_or("")
                .trim_matches(['[', ']'])
                .to_ascii_lowercase();
            if a_host == host
                && a.scheme() == u.scheme()
                && a.port_or_known_default() == u.port_or_known_default()
            {
                return true;
            }
            continue;
        }
        if let Some(rest) = item.strip_prefix("*.") {
            let rest = rest.to_ascii_lowercase();
            if host == rest || host.ends_with(&format!(".{rest}")) {
                return true;
            }
            continue;
        }
        if item.to_ascii_lowercase() == host {
            return true;
        }
    }
    false
}

/// 登录来源校验：拒绝跨站表单；同源（含 CDN 回源场景）直接放行；
/// Host 被反代改写时，再按后台配置的允许来源/域名放行。
pub fn login_origin_ok(headers: &HeaderMap) -> bool {
    let site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    if site.as_deref() == Some("cross-site") {
        return false;
    }
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if site.as_deref() == Some("same-origin") {
        return true;
    }
    let Ok(u) = reqwest::Url::parse(origin) else {
        return false;
    };
    let origin_host = u
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let host = host
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(&host)
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if !origin_host.is_empty() && !host.is_empty() && origin_host == host {
        return true;
    }
    let allowed = crate::store::store()
        .load()
        .pointer("/config/allowed_origins")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    origin_allowlisted(origin, &allowed)
}

pub fn login_throttled() -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({"error":{"message":"尝试过于频繁，请 5 分钟后重试","type":"auth"}})),
    ).into_response();
    response.headers_mut().insert("retry-after", HeaderValue::from_static("300"));
    response
}

/// Password work must not occupy Tokio workers or build an unbounded blocking queue.
pub async fn password_work<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let Ok(permit) = SLOTS.try_acquire() else {
        let mut response = (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error":{"message":"账户服务繁忙，请稍后重试"}}))).into_response();
        response.headers_mut().insert("retry-after", HeaderValue::from_static("2"));
        return Err(response);
    };
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    }).await.map_err(|_| (StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error":{"message":"账户服务暂不可用，请稍后重试"}}))).into_response())
}

pub async fn verify_login_password(password: String, stored: Option<String>) -> Result<bool, Response> {
    password_work(move || {
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let hash = stored.as_deref().unwrap_or_else(|| DUMMY.get_or_init(|| crate::store::hash_password("ngw-dummy")));
        let valid = crate::store::verify_password(&password, hash);
        stored.is_some() && valid
    }).await
}

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
    pub const USER_HTML: &str = include_str!("../web/user.html");
    pub const USER_JS: &str = include_str!("../web/user.js");
    pub const WEBCLAW_HTML: &str = include_str!("../web/webclaw.html");
    pub const WEBCLAW_JS: &str = include_str!("../web/webclaw.js");
}

pub fn bundled_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    match name {
        "webclaw-agent.js" => Some(("application/javascript; charset=utf-8", include_bytes!("../web/webclaw-agent.js"))),
        "ui.css" => Some(("text/css; charset=utf-8", include_bytes!("../web/ui.css"))),
        "ui.js" => Some(("application/javascript; charset=utf-8", include_bytes!("../web/ui.js"))),
        "vendor/bootstrap.min.css" => Some(("text/css; charset=utf-8", include_bytes!("../web/vendor/bootstrap.min.css"))),
        "vendor/bootstrap-icons.min.css" => Some(("text/css; charset=utf-8", include_bytes!("../web/vendor/bootstrap-icons.min.css"))),
        "vendor/bootstrap.bundle.min.js" => Some(("application/javascript; charset=utf-8", include_bytes!("../web/vendor/bootstrap.bundle.min.js"))),
        "vendor/marked.min.js" => Some(("application/javascript; charset=utf-8", include_bytes!("../web/vendor/marked.min.js"))),
        "vendor/purify.min.js" => Some(("application/javascript; charset=utf-8", include_bytes!("../web/vendor/purify.min.js"))),
        "vendor/fonts/bootstrap-icons.woff2" => Some(("font/woff2", include_bytes!("../web/vendor/fonts/bootstrap-icons.woff2"))),
        "vendor/fonts/bootstrap-icons.woff" => Some(("font/woff", include_bytes!("../web/vendor/fonts/bootstrap-icons.woff"))),
        _ => None,
    }
}

pub fn cors_headers() -> Vec<(String, String)> {
    vec![
        (
            // 每响应唯一：ZCode 遥测读取，用户拿它对网关日志定位问题
            "x-request-id".into(),
            crate::util::rand_hex(16),
        ),
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
        return err_retry_after(
            with_cors(
                (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(body)).into_response(),
            ),
            status,
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
    err_retry_after(
        with_cors(
            (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(body)).into_response(),
        ),
        status,
    )
}

/// 错误响应补 retry-after（ZCode 的重试策略读取）：429 默认 60s、5xx 默认 10s。
/// 调用方的精确值（如用户限速的剩余秒数）在返回后 .insert 会覆盖这里的默认值。
fn err_retry_after(mut resp: Response, status: u16) -> Response {
    if status == 429 {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, HeaderValue::from_static("60"));
    } else if status >= 500 {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, HeaderValue::from_static("10"));
    }
    resp
}

pub fn json_resp(v: Value) -> Response {
    with_cors(axum::Json(v).into_response())
}

/// 令牌遮罩：可辨识但不泄露完整令牌。
pub fn tok_mask(t: &str) -> String {
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= 14 {
        return if t.is_empty() { String::new() } else { "***".into() };
    }
    let head: String = chars[..10].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}…{}", head, tail)
}

// ---------------------------------------------------------------- 上游客户端注册表

struct HttpEntry {
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

fn build_client(connect_to: i64) -> reqwest::Client {
    // 关键：NVIDIA NIM 等上游会在 15-60s 内关闭空闲连接。空闲连接留在池里太久，
    // 下一个请求复用死连接时 hyper 不会重试（请求带 body），结果就是 200s 超时。
    // 短空闲超时 + TCP keepalive 确保死连接被及时清理。
    let mut b = reqwest::Client::builder()
        // Never forward configured upstream credentials through a redirect.
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(std::time::Duration::from_secs(15))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(std::time::Duration::from_secs(15))
        .tcp_nodelay(true);
    if connect_to > 0 {
        b = b.connect_timeout(std::time::Duration::from_secs(connect_to as u64));
    }
    b.build().unwrap_or_default()
}

/// 取（或建）一个与 connect_timeout 匹配的共享客户端。
pub fn get_http(connect_to: i64) -> reqwest::Client {
    {
        let guard = HTTP_CLIENTS.read().unwrap();
        if let Some(list) = guard.as_ref() {
            for e in list {
                if e.connect_to == connect_to {
                    return e.client.clone();
                }
            }
        }
    }
    let client = build_client(connect_to);
    let mut guard = HTTP_CLIENTS.write().unwrap();
    let list = guard.get_or_insert_with(Vec::new);
    for e in list.iter() {
        if e.connect_to == connect_to {
            return e.client.clone();
        }
    }
    list.push(HttpEntry {
        connect_to,
        client: client.clone(),
    });
    client
}

pub fn init_http(cfg: &Value) {
    let _ = get_http(10);
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

#[cfg(test)]
mod csrf_tests {
    use super::*;
    #[test]
    fn automatic_cookies_are_not_a_csrf_proof() {
        let mut headers=HeaderMap::new();
        headers.insert("cookie","ngw_session=session; ngw_csrf=expected; ngw_ucsrfs=expected".parse().unwrap());
        assert!(!csrf_header_matches(&headers,"expected"));
        for bad in ["","wrong"] {headers.insert("x-csrf",bad.parse().unwrap());assert!(!csrf_header_matches(&headers,"expected"));}
        headers.insert("x-csrf","expected".parse().unwrap());
        assert!(csrf_header_matches(&headers,"expected"));
        assert!(!csrf_header_matches(&headers,""));
    }
}
