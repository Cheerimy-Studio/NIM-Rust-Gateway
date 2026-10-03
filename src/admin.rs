//! 管理端 API（admin_api.py 的移植）：认证、概览、账号/渠道/日志/排队/设置/配置同步。

use crate::pool;
use crate::proxy;
use crate::queue;
use crate::store::{store, csrf_token, session_cookie, verify_password, hash_password, Value};
use crate::upstreams;
use crate::util;
use crate::webhttp::*;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

const CSRF_COOKIE: &str = "ngw_csrf";

// ---------------------------------------------------------------- 认证

fn get_cookies(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(cookie) = headers.get("cookie").and_then(|c| c.to_str().ok()) {
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(eq) = part.find('=') {
                out.push((part[..eq].to_string(), part[eq + 1..].to_string()));
            }
        }
    }
    out
}

pub fn cookie_get(headers: &HeaderMap, name: &str) -> Option<String> {
    get_cookies(headers)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

fn authed(headers: &HeaderMap) -> bool {
    let cfg = store().load();
    let expected = session_cookie(cfg.get("config").unwrap_or(&Value::Null));
    let got = cookie_get(headers, "ngw_session").unwrap_or_default();
    !got.is_empty() && constant_eq(&got, &expected)
}

static LOGIN_RATE: Mutex<Option<Vec<(String, Vec<f64>)>>> = Mutex::new(None);

fn login_rate_ok(ip: &str) -> bool {
    let now = util::now_f();
    let mut g = LOGIN_RATE.lock().unwrap();
    let m = g.get_or_insert_with(Vec::new);
    let mut attempts: Vec<f64> = m
        .iter()
        .find(|(k, _)| k == ip)
        .map(|(_, v)| v.iter().filter(|t| now - *t < 300.0).copied().collect())
        .unwrap_or_default();
    if attempts.len() >= 10 {
        if let Some(e) = m.iter_mut().find(|(k, _)| k == ip) {
            e.1 = attempts;
        }
        return false;
    }
    attempts.push(now);
    if let Some(e) = m.iter_mut().find(|(k, _)| k == ip) {
        e.1 = attempts;
    } else {
        m.push((ip.to_string(), attempts));
    }
    if m.len() > 1000 {
        m.retain(|(_, v)| !v.is_empty() && now - v[v.len() - 1] < 300.0);
    }
    true
}

fn login_rate_clear(ip: &str) {
    let mut g = LOGIN_RATE.lock().unwrap();
    if let Some(m) = g.as_mut() {
        m.retain(|(k, _)| k != ip);
    }
}

fn auth_error() -> Response {
    let body = json!({"error": {"message": "未登录或会话已过期", "type": "auth"}});
    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

fn csrf_error() -> Response {
    let body = json!({"error": {"message": "CSRF 校验失败，请刷新页面", "type": "auth"}});
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

/// 登录态 + CSRF 校验（POST 需要 CSRF）。
pub fn require(headers: &HeaderMap, is_post: bool) -> Result<(), Response> {
    if !authed(headers) {
        return Err(auth_error());
    }
    if is_post {
        let cfg = store().load();
        let expected = csrf_token(cfg.get("config").unwrap_or(&Value::Null));
        // Python 语义是 `headers.get("x-csrf") or cookies.get(...) or ""`：
        // 前端 window.__csrf 不存在，JS 发的是空 X-CSRF，必须回退到浏览器
        // 自动携带的 ngw_csrf cookie；把空串当真值会让整个后台 403
        let got = headers
            .get("x-csrf")
            .and_then(|x| x.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| cookie_get(headers, CSRF_COOKIE))
            .unwrap_or_default();
        if !constant_eq(&got, &expected) {
            return Err(csrf_error());
        }
    }
    Ok(())
}

fn constant_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn json_dict(body: &Bytes) -> Option<Value> {
    serde_json::from_slice::<Value>(body).ok().filter(|v| v.is_object())
}

fn set_cookie_header(name: &str, value: &str, http_only: bool, delete: bool, secure: bool) -> String {
    let mut s = format!("{}={}; Path=/; SameSite=Lax", name, value);
    if http_only {
        s.push_str("; HttpOnly");
    }
    if secure {
        s.push_str("; Secure");
    }
    if delete {
        s.push_str("; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT");
    }
    s
}

fn query_get(query: &Option<String>, key: &str) -> String {
    let Some(q) = query else { return String::new() };
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if k == key {
            return percent_encoding::percent_decode_str(v)
                .decode_utf8_lossy()
                .replace('+', " ");
        }
    }
    String::new()
}

// ---------------------------------------------------------------- 路由处理

pub async fn login(headers: &HeaderMap, ip: String, body: Bytes) -> Response {
    if !login_rate_ok(&ip) {
        let body = json!({"error": {"message": "尝试过于频繁，请 5 分钟后重试", "type": "auth"}});
        return (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    }
    let body_v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cfg = store().load();
    let cfg = cfg.get("config").cloned().unwrap_or(json!({}));
    let username = util::str_or(body_v.get("username"), "").trim().to_string();
    let password = util::str_or(body_v.get("password"), "");
    let ok_user = username.to_lowercase() == util::str_or(cfg.get("admin_username"), "").to_lowercase();
    let ok_pass = verify_password(&password, &util::str_or(cfg.get("admin_password_hash"), ""));
    if !(ok_user && ok_pass) {
        let body = json!({"error": {"message": "账号或密码错误", "type": "auth"}});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }
    login_rate_clear(&ip);
    let session = session_cookie(&cfg);
    let csrf = csrf_token(&cfg);
    // https 部署下会话 Cookie 带 Secure（反代成 http 的内网部署不能设，否则浏览器
    // 拒绝回传）；反代场景从 X-Forwarded-Proto 识别。CSRF cookie 补 HttpOnly：
    // 双提交的比对在服务端读 cookie 完成，JS 无需读取
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|x| x.to_str().ok())
        .map(|p| p.split(',').next().unwrap_or("").trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let mut resp = axum::Json(json!({"ok": true, "csrf": csrf})).into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header("ngw_session", &session, true, false, secure)).unwrap(),
    );
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header(CSRF_COOKIE, &csrf, true, false, secure)).unwrap(),
    );
    resp
}

pub async fn logout() -> Response {
    let mut resp = axum::Json(json!({"ok": true})).into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header("ngw_session", "", true, true, false)).unwrap(),
    );
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header(CSRF_COOKIE, "", true, true, false)).unwrap(),
    );
    resp
}

fn pool_state() -> Value {
    json!({
        "max_connections": POOL_CAP.load(std::sync::atomic::Ordering::SeqCst),
        "inflight": pool::inflight_total(),
        "odd_releases": pool::inflight_odd_releases(),
    })
}

pub async fn overview(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(json!({}));
    let now = util::now_i();
    let day = util::local_day(now);
    let mut counts = json!({"total": 0, "enabled": 0, "banned": 0, "invalid": 0, "manual_disabled": 0});
    let mut daily_req = 0i64;
    let mut daily_tok = 0i64;
    let mut risky: Vec<Value> = Vec::new();
    if let Some(keys) = db.get("keys").and_then(|k| k.as_array()) {
        for k in keys {
            counts["total"] = json!(util::int_or(Some(&counts["total"]), 0) + 1);
            let enabled = k.get("enabled").map(util::truthy).unwrap_or(false);
            if enabled {
                counts["enabled"] = json!(util::int_or(Some(&counts["enabled"]), 0) + 1);
                if util::int_or(k.get("banned_until"), 0) > now {
                    counts["banned"] = json!(util::int_or(Some(&counts["banned"]), 0) + 1);
                }
                if util::str_or(k.get("status"), "") == "invalid" {
                    counts["invalid"] = json!(util::int_or(Some(&counts["invalid"]), 0) + 1);
                }
            } else {
                counts["manual_disabled"] = json!(util::int_or(Some(&counts["manual_disabled"]), 0) + 1);
            }
            let d = k.get("daily").and_then(|d| d.get(&day)).cloned().unwrap_or(json!({}));
            daily_req += util::int_or(d.get("requests"), 0);
            daily_tok += util::int_or(d.get("tokens"), 0);
            let total_requests = util::int_or(k.get("total_requests"), 0);
            let total_fail = util::int_or(k.get("total_fail"), 0);
            let ratio = if total_requests > 0 { total_fail as f64 / total_requests as f64 } else { 0.0 };
            let consec = util::int_or(k.get("consecutive_failures"), 0);
            if (ratio >= 0.4 && total_fail >= 3) || consec >= 2 {
                risky.push(json!({
                    "id": util::str_or(k.get("id"), ""),
                    "email": util::str_or(k.get("email"), ""),
                    "fail_ratio": (ratio * 100.0).round() as i64,
                    "consecutive": consec,
                    "total_fail": total_fail,
                    "last_error": util::str_cut(&util::str_or(k.get("last_error"), ""), 80),
                }));
            }
        }
    }
    risky.sort_by(|a, b| {
        let ca = util::int_or(a.get("consecutive"), 0);
        let cb = util::int_or(b.get("consecutive"), 0);
        cb.cmp(&ca).then_with(|| {
            let fa = util::int_or(a.get("fail_ratio"), 0);
            let fb = util::int_or(b.get("fail_ratio"), 0);
            fb.cmp(&fa)
        })
    });
    let mut rpm = 0i64;
    if let Some(buckets) = db.get("buckets").and_then(|b| b.as_object()) {
        for b in buckets.values() {
            if let Some(a) = b.as_array() {
                rpm += a.iter().filter(|t| t.as_f64().map(|tv| now as f64 - tv < 60.0).unwrap_or(false)).count() as i64;
            }
        }
    }
    let today = db.get("stats").and_then(|s| s.get(&day)).cloned().unwrap_or(json!({"total": 0, "success": 0, "fail": 0, "models": {}}));
    let mut models: Vec<(String, i64)> = today
        .get("models")
        .and_then(|m| m.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), util::int_or(Some(v), 0))).collect())
        .unwrap_or_default();
    models.sort_by(|a, b| b.1.cmp(&a.1));
    let models: Vec<Value> = models
        .into_iter()
        .take(10)
        .map(|(m, c)| json!({"model": m, "count": c}))
        .collect();
    let recent_errors: Vec<Value> = db
        .get("logs")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| {
                    let st = r.as_array().and_then(|x| x.get(4)).and_then(|x| x.as_i64()).unwrap_or(0);
                    st >= 400
                })
                .take(8)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mw = util::cfg_int(&cfg, "queue_max_wait", 15);
    let mw = if mw == 0 { 0 } else { mw.max(5) };
    let qcut = now - mw * 2;
    let queue_now = db
        .get("queue")
        .and_then(|q| q.as_array())
        .map(|a| a.iter().filter(|e| e.is_object() && util::f64_or(e.get("t"), 0.0) >= qcut as f64).count())
        .unwrap_or(0);
    let rl_cfg = util::cfg_int(&cfg, "rate_limit_per_minute", 20);
    let enabled_n = util::int_or(Some(&counts["enabled"]), 0);
    let today_total = util::int_or(today.get("total"), 0);
    let today_success = util::int_or(today.get("success"), 0);
    json_resp(json!({
        "keys": counts,
        "rpm": rpm,
        "rpm_limit_total": if rl_cfg <= 0 { -1 } else { enabled_n * rl_cfg.max(1) },
        "daily": {"requests": daily_req, "tokens": daily_tok},
        "queue": queue_now,
        "today": {
            "total": today_total,
            "success": today_success,
            "fail": util::int_or(today.get("fail"), 0),
            "rate": if today_total > 0 { Some((today_success * 100 / today_total) as i64) } else { None },
        },
        "models": models,
        "recent_errors": recent_errors,
        "risky": risky.into_iter().take(10).collect::<Vec<_>>(),
        "pool": pool_state(),
        "server_time": now,
    }))
}

pub async fn keys(headers: &HeaderMap, query: Option<String>) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(json!({}));
    let q = query_get(&query, "q").trim().to_lowercase();
    let status = {
        let s = query_get(&query, "status");
        if s.is_empty() { "all".to_string() } else { s }
    };
    let page = query_get(&query, "page").parse::<i64>().unwrap_or(1).max(1);
    let per = 20;
    let now = util::now_i();
    let day = util::local_day(now);
    let up_names: HashMap<String, String> = db
        .get("upstreams")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().map(|u| (util::str_or(u.get("id"), ""), util::str_or(u.get("name"), ""))).collect())
        .unwrap_or_default();
    let mut rows: Vec<Value> = Vec::new();
    let keys: Vec<Value> = db
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|a| a.iter().cloned().collect())
        .unwrap_or_default();
    for k in keys.iter().rev() {
        let email = util::str_or(k.get("email"), "");
        let apikey = util::str_or(k.get("apikey"), "");
        if !q.is_empty() && !email.to_lowercase().contains(&q) && !apikey.to_lowercase().contains(&q) {
            continue;
        }
        let enabled = k.get("enabled").map(util::truthy).unwrap_or(false);
        let banned = util::int_or(k.get("banned_until"), 0) > now;
        if status == "active" && (!enabled || banned) {
            continue;
        }
        if status == "disabled" && enabled && !banned {
            continue;
        }
        if status == "banned" && !banned {
            continue;
        }
        let mut row = k.clone();
        let kid = util::str_or(k.get("id"), "");
        let rpm_used = db
            .get("buckets")
            .and_then(|b| b.get(&kid))
            .and_then(|b| b.as_array())
            .map(|a| a.iter().filter(|t| t.as_f64().map(|tv| now as f64 - tv < 60.0).unwrap_or(false)).count())
            .unwrap_or(0);
        row["rpm_used"] = json!(rpm_used);
        let tr = util::int_or(k.get("total_requests"), 0);
        let tf = util::int_or(k.get("total_fail"), 0);
        row["fail_ratio"] = json!(if tr > 0 { tf * 100 / tr } else { 0 });
        row["today"] = k.get("daily").and_then(|d| d.get(&day)).cloned().unwrap_or(json!({"requests": 0, "tokens": 0}));
        row["upstream_name"] = Value::from(up_names.get(&util::str_or(k.get("upstream_id"), "")).cloned().unwrap_or_else(|| "-".into()));
        row["inflight"] = json!(pool::inflight_of(&kid));
        rows.push(row);
    }
    let total = rows.len() as i64;
    let start = ((page - 1) * per) as usize;
    let end = (page * per) as usize;
    let rows: Vec<Value> = rows.into_iter().skip(start as usize).take(per as usize).collect();
    json_resp(json!({
        "total": total,
        "page": page,
        "pages": ((total + per - 1) / per).max(1),
        "rows": rows,
        "rate_limit": util::cfg_int(&cfg, "rate_limit_per_minute", 20),
        "daily_cap": util::cfg_int(&cfg, "daily_request_cap", 0),
    }))
}

pub async fn keydetail(headers: &HeaderMap, query: Option<String>) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let kid = query_get(&query, "id");
    let db = store().load();
    let now = util::now_i();
    let day = util::local_day(now);
    if let Some(keys) = db.get("keys").and_then(|k| k.as_array()) {
        for k in keys {
            if util::str_or(k.get("id"), "") == kid {
                let mut row = k.clone();
                let rpm_used = db
                    .get("buckets")
                    .and_then(|b| b.get(&kid))
                    .and_then(|b| b.as_array())
                    .map(|a| a.iter().filter(|t| t.as_f64().map(|tv| now as f64 - tv < 60.0).unwrap_or(false)).count())
                    .unwrap_or(0);
                row["rpm_used"] = json!(rpm_used);
                row["today"] = k.get("daily").and_then(|d| d.get(&day)).cloned().unwrap_or(json!({"requests": 0, "tokens": 0}));
                row["rate_limit"] = json!(util::cfg_int(db.get("config").unwrap_or(&json!({})), "rate_limit_per_minute", 20));
                if let Some(ups) = db.get("upstreams").and_then(|u| u.as_array()) {
                    for u in ups {
                        if util::str_or(u.get("id"), "") == util::str_or(k.get("upstream_id"), "") {
                            row["upstream_name"] = u.get("name").cloned().unwrap_or(Value::Null);
                        }
                    }
                }
                let recent = k.get("recent").and_then(|r| r.as_array()).map(|a| a.iter().take(10).cloned().collect()).unwrap_or(vec![]);
                return json_resp(json!({"key": row, "recent": recent}));
            }
        }
    }
    (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "密钥不存在", "type": "not_found"}}))).into_response()
}

pub async fn keys_import(headers: &HeaderMap, multipart: bool, body: Bytes, form: Option<(String, String, Vec<u8>)>) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let (text, file_text, upstream_id) = if multipart {
        match form {
            Some((t, uid, file)) => {
                if file.len() > 5 * 1024 * 1024 {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "文件过大（>5MB）"}}))).into_response();
                }
                (t, String::from_utf8_lossy(&file).to_string(), uid)
            }
            None => (String::new(), String::new(), String::new()),
        }
    } else {
        match json_dict(&body) {
            Some(b) => (
                util::str_or(b.get("text"), ""),
                String::new(),
                util::str_or(b.get("upstream_id"), ""),
            ),
            None => (String::new(), String::new(), String::new()),
        }
    };
    if text.trim().is_empty() && file_text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请粘贴账户数据或选择 CSV 文件"}}))).into_response();
    }
    let ups = upstreams::all_upstreams();
    let valid_ids: Vec<String> = ups.iter().map(|u| util::str_or(u.get("id"), "")).collect();
    let mut upstream_id = upstream_id;
    if !valid_ids.contains(&upstream_id) {
        let enabled: Vec<String> = ups
            .iter()
            .filter(|u| u.get("enabled").map(util::truthy).unwrap_or(false))
            .map(|u| util::str_or(u.get("id"), ""))
            .collect();
        upstream_id = if !enabled.is_empty() {
            enabled[0].clone()
        } else if !valid_ids.is_empty() {
            valid_ids[0].clone()
        } else {
            String::new()
        };
    }
    let res = pool::import_accounts(&text, &upstream_id, &file_text);
    let mut out = json!({"ok": true});
    if let (Some(a), Some(bv)) = (out.as_object_mut(), res.as_object()) {
        for (k, v) in bv {
            a.insert(k.clone(), v.clone());
        }
    }
    json_resp(out)
}

fn csv_escape(v: &str) -> String {
    let v = v.to_string();
    if v.contains(',') || v.contains('"') || v.contains('\n') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

pub async fn keys_export(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let mut lines = vec!["email,password,apikey".to_string()];
    if let Some(keys) = db.get("keys").and_then(|k| k.as_array()) {
        for k in keys {
            lines.push(format!(
                "{},{},{}",
                csv_escape(&util::str_or(k.get("email"), "")),
                csv_escape(&util::str_or(k.get("password"), "")),
                csv_escape(&util::str_or(k.get("apikey"), ""))
            ));
        }
    }
    let mut resp = lines.join("\n").into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=keys.csv"),
    );
    resp
}

pub async fn keys_op(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let op = util::str_or(body.get("op"), "");
    let kid = util::str_or(body.get("id"), "");
    match op.as_str() {
        "test" => {
            let result = pool::test_key(&kid).await;
            json_resp(json!({"ok": true, "test": result}))
        }
        "enable" => json_resp(json!({"ok": pool::set_enabled(&kid, true)})),
        "disable" => json_resp(json!({"ok": pool::set_enabled(&kid, false)})),
        "unban" => json_resp(json!({"ok": pool::unban(&kid)})),
        "delete" => json_resp(json!({"ok": pool::delete_key(&kid)})),
        "reset" => json_resp(json!({"ok": pool::reset_stats(&kid)})),
        _ => (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "未知操作"}}))).into_response(),
    }
}

pub async fn keys_batch(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let op = util::str_or(body.get("op"), "");
    let ids: Vec<String> = body
        .get("ids")
        .and_then(|i| i.as_array())
        .map(|a| a.iter().map(|x| util::str_or(Some(x), "")).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "未选择账号"}}))).into_response();
    }
    if op == "test" {
        let mut results = Map::new();
        for i in ids.iter().take(20) {
            let r = pool::test_key(i).await;
            results.insert(i.clone(), r);
        }
        return json_resp(json!({"ok": true, "results": results}));
    }
    let mut results = Map::new();
    match op.as_str() {
        "enable" => {
            for i in &ids {
                results.insert(i.clone(), json!(pool::set_enabled(i, true)));
            }
        }
        "unban" => {
            for i in &ids {
                results.insert(i.clone(), json!(pool::unban(i)));
            }
        }
        "disable" => {
            for i in &ids {
                results.insert(i.clone(), json!(pool::set_enabled(i, false)));
            }
        }
        "reset" => {
            for i in &ids {
                results.insert(i.clone(), json!(pool::reset_stats(i)));
            }
        }
        "delete" => {
            for i in &ids {
                results.insert(i.clone(), json!(pool::delete_key(i)));
            }
        }
        "move" => {
            let target = util::str_or(body.get("upstream_id"), "");
            let valid: Vec<String> = upstreams::all_upstreams().iter().map(|u| util::str_or(u.get("id"), "")).collect();
            if !valid.contains(&target) {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "目标渠道不存在"}}))).into_response();
            }
            store().update(|db| {
                if let Some(keys) = db.get_mut("keys").and_then(|k| k.as_array_mut()) {
                    for k in keys.iter_mut() {
                        if ids.contains(&util::str_or(k.get("id"), "")) {
                            if let Some(o) = k.as_object_mut() {
                                o.insert("upstream_id".into(), Value::from(target.clone()));
                            }
                        }
                    }
                }
            });
            store().flush();
            return json_resp(json!({"ok": true, "moved": ids.len()}));
        }
        _ => {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "未知操作"}}))).into_response();
        }
    }
    json_resp(json!({"ok": true, "results": results}))
}

pub async fn keys_clear_all(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    if util::str_or(body.get("confirm"), "") != "yes" {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "缺少确认参数"}}))).into_response();
    }
    json_resp(json!({"ok": true, "removed": pool::clear_all()}))
}

pub async fn queue_list(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    json_resp(queue::stats(false))
}

pub async fn queue_clear(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    queue::clear();
    json_resp(json!({"ok": true}))
}

fn upstream_row(db: &Value, u: &Value) -> Value {
    let now = util::now_i();
    let minute = util::utc_minute(now);
    let day = util::local_day(now);
    let uid = util::str_or(u.get("id"), "");
    let keys: Vec<&Value> = db
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|a| a.iter().filter(|k| util::str_or(k.get("upstream_id"), "") == uid).collect())
        .unwrap_or_default();
    let today_req: i64 = keys
        .iter()
        .map(|k| {
            k.get("daily")
                .and_then(|d| d.get(&day))
                .and_then(|d| d.get("requests"))
                .and_then(util::py_int)
                .unwrap_or(0)
        })
        .sum();
    let rec = db.get("up_recent").and_then(|r| r.get(&uid)).and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let ok_n = rec.iter().filter(|v| v.as_i64().unwrap_or(0) != 0).count();
    let score = ((ok_n as f64 + 5.0) / (rec.len() as f64 + 10.0) * 100.0).round() as i64;
    let mut row = u.clone();
    if let Some(o) = row.as_object_mut() {
        o.insert("keys".into(), json!(keys.len()));
        o.insert("enabled_keys".into(), json!(keys.iter().filter(|k| k.get("enabled").map(util::truthy).unwrap_or(false)).count()));
        o.insert("today_requests".into(), json!(today_req));
        let minute_used = db
            .get("pool_buckets")
            .and_then(|b| b.get(&uid));
        let mu = match minute_used {
            Some(Value::Array(a)) => a.iter().filter(|t| t.as_f64().map(|tv| now as f64 - tv < 60.0).unwrap_or(false)).count() as i64,
            Some(Value::Object(m)) => m.get(&minute).and_then(util::py_int).unwrap_or(0),
            _ => 0,
        };
        o.insert("minute_used".into(), json!(mu));
        o.insert("feasibility".into(), json!(score));
        o.insert("recent".into(), json!(rec.len()));
        o.insert("model_map_count".into(), json!(u.get("model_map").and_then(|m| m.as_object()).map(|m| m.len()).unwrap_or(0)));
        o.insert("hide_errors_global".into(), json!(db.pointer("/config/hide_upstream_errors").map(util::truthy).unwrap_or(true)));
        o.insert("hide_mapped_global".into(), json!(db.pointer("/config/hide_mapped_names").map(util::truthy).unwrap_or(true)));
    }
    row
}

pub async fn upstreams_list(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let rows: Vec<Value> = db
        .get("upstreams")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().map(|u| upstream_row(&db, u)).collect())
        .unwrap_or_default();
    json_resp(json!({"rows": rows}))
}

pub async fn upstreams_save(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let (row, err) = upstreams::save(&body);
    let Some(row) = row else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": err}}))).into_response();
    };
    let db = store().load();
    let rows: Vec<Value> = db
        .get("upstreams")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().map(|u| upstream_row(&db, u)).collect())
        .unwrap_or_default();
    json_resp(json!({"ok": true, "upstream": row, "rows": rows}))
}

pub async fn upstreams_delete(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let (ok, err) = upstreams::delete(&util::str_or(body.get("id"), ""));
    if !ok {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": err}}))).into_response();
    }
    let db = store().load();
    let rows: Vec<Value> = db
        .get("upstreams")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().map(|u| upstream_row(&db, u)).collect())
        .unwrap_or_default();
    json_resp(json!({"ok": true, "rows": rows}))
}

pub async fn logs(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let rows = db.get("logs").cloned().unwrap_or(json!([]));
    let enabled = db.pointer("/config/log_enabled").map(util::truthy).unwrap_or(true);
    json_resp(json!({"rows": rows, "enabled": enabled}))
}

pub async fn logs_clear(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("logs".into(), Value::Array(vec![]));
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

const INT_SETTINGS: &[&str] = &[
    "rate_limit_per_minute",
    "tpm_limit",
    "account_cooldown_ms",
    "max_retries",
    "retry_backoff_base_ms",
    "retry_backoff_max_ms",
    "retry_min_wait_ms",
    "ban_step_seconds",
    "ban_max_seconds",
    "hard_fail_ban_seconds",
    "hard_fail_disable_count",
    "daily_request_cap",
    "daily_token_limit",
    "hourly_request_limit",
    "queue_max_wait",
    "queue_poll_ms",
    "cool_429_seconds",
    "cool_5xx_seconds",
    "cool_timeout_seconds",
    "cool_conn_seconds",
    "breaker_threshold",
    "breaker_seconds",
    "ttfb_timeout",
    "sse_idle_timeout",
    "pool_max_connections",
    "restart_interval_hours",
    "update_auto_check_hours",
    "model_missing_ttl",
    "intercept_log_max",
    "request_timeout",
    "connect_timeout",
    "log_max",
    "session_log_max",
    "training_log_max",
    "training_min_chars",
    "watchdog_minutes",
    "acct_concurrency",
    "total_concurrency",
    "pool_rpm_cap",
    "pool_daily_cap",
    "warmup_seconds",
];
const STR_SETTINGS: &[&str] = &[
    "upstream_base",
    "timezone",
    "model_whitelist",
    "model_blacklist",
    "param_overrides",
    "update_token",
];
const BOOL_SETTINGS: &[&str] = &[
    "log_enabled",
    "verify_tls",
    "queue_enabled",
    "update_enabled",
    "hide_upstream_errors",
    "hide_mapped_names",
    "breaker_enabled",
    "watchdog_enabled",
];

fn cfg_without_secrets(cfg: &Value) -> Value {
    let mut c = cfg.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("admin_password_hash");
        o.remove("session_secret");
    }
    c
}

pub async fn settings_get(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
    json_resp(cfg_without_secrets(&cfg))
}

fn apply_settings(db: &mut Value, incoming: &Value, applied: &mut i64) {
    let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) else { return };
    for k in INT_SETTINGS {
        if let Some(v) = incoming.get(*k) {
            let s = util::str_or(Some(v), "");
            if !s.is_empty() {
                if let Some(i) = util::py_int(v) {
                    cfg.insert(k.to_string(), Value::from(i));
                    *applied += 1;
                }
            }
        }
    }
    for k in STR_SETTINGS {
        if let Some(Value::String(s)) = incoming.get(*k) {
            cfg.insert(k.to_string(), Value::from(s.trim()));
            *applied += 1;
        }
    }
    for k in BOOL_SETTINGS {
        if let Some(v) = incoming.get(*k) {
            let default = cfg.get(*k).map(util::truthy).unwrap_or(false);
            cfg.insert(k.to_string(), Value::from(util::as_bool(v, default)));
            *applied += 1;
        }
    }
    // gateway_tokens 文本形式：token|model1,model2
    if let Some(Value::String(gt)) = incoming.get("gateway_tokens") {
        let mut struct_tokens: Vec<Value> = Vec::new();
        for line in gt.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(2, '|').map(|p| p.trim()).collect();
            if parts[0].chars().count() < 8 {
                continue;
            }
            let models = if parts.len() == 1 || parts[1] == "*" {
                vec![]
            } else {
                parts[1]
                    .replace(';', ",")
                    .split(',')
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect()
            };
            struct_tokens.push(json!({"t": parts[0], "m": models}));
        }
        if !struct_tokens.is_empty() {
            cfg.insert("gateway_tokens".into(), Value::Array(struct_tokens));
        }
    }
    if let Some(u) = incoming.get("admin_username") {
        let s = util::str_or(Some(u), "").trim().to_string();
        if !s.is_empty() {
            cfg.insert("admin_username".into(), Value::from(util::str_cut(&s, 32)));
        }
    }
    let base = util::str_or(cfg.get("upstream_base"), "");
    if !base.is_empty() && !base.starts_with("http") {
        cfg.insert("upstream_base".into(), Value::from("https://integrate.api.nvidia.com/v1"));
    }
}

pub async fn settings_save(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let incoming = body.get("config").cloned().unwrap_or(json!({}));
    store().update(|db| {
        let mut applied = 0;
        apply_settings(db, &incoming, &mut applied);
        // 保留条数改小时立即裁剪存量，而不是等下一次拦截才掉
        let cap = util::cfg_int(
            db.get("config").unwrap_or(&json!({})),
            "intercept_log_max",
            100,
        );
        if let Some(logs) = db.get_mut("intercepted").and_then(|l| l.as_array_mut()) {
            logs.truncate(cap.max(0) as usize);
        }
    });
    store().flush();
    proxy::invalidate_cfg_cache();
    let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
    json_resp(cfg_without_secrets(&cfg))
}

pub async fn password_change(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let cfg = store().load();
    let cfg = cfg.get("config").cloned().unwrap_or(json!({}));
    if !verify_password(
        &util::str_or(body.get("old"), ""),
        &util::str_or(cfg.get("admin_password_hash"), ""),
    ) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "当前密码错误"}}))).into_response();
    }
    let new_pw = util::str_or(body.get("new"), "");
    if new_pw.chars().count() < 6 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "新密码至少 6 位"}}))).into_response();
    }
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            cfg.insert("admin_password_hash".into(), Value::from(hash_password(&new_pw)));
            // 轮换会话密钥：改密前的所有登录会话全部失效
            cfg.insert("session_secret".into(), Value::from(util::rand_hex(24)));
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

pub async fn stats_reset(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    pool::reset_all_stats();
    json_resp(json!({"ok": true}))
}

fn tokens_rows(cfg: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(tokens) = cfg.get("gateway_tokens").and_then(|t| t.as_array()) {
        for t in tokens {
            match t {
                Value::String(s) => out.push(json!({"t": s, "m": [], "last_ip": "", "last_at": 0})),
                Value::Object(m) => out.push(json!({
                    "t": util::str_or(m.get("t"), ""),
                    "m": m.get("m").and_then(|x| x.as_array()).map(|a| a.iter().map(|x| util::str_or(Some(x), "")).collect::<Vec<_>>()).unwrap_or_default(),
                    "last_ip": util::str_or(m.get("last_ip"), ""),
                    "last_at": util::int_or(m.get("last_at"), 0),
                })),
                _ => {}
            }
        }
    }
    out
}

pub async fn poolmap(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(json!({}));
    let now = util::now_i();
    let conc = util::cfg_int(&cfg, "acct_concurrency", 0);
    let up_names: HashMap<String, String> = db
        .get("upstreams")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().map(|u| (util::str_or(u.get("id"), ""), util::str_or(u.get("name"), ""))).collect())
        .unwrap_or_default();
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(keys) = db.get("keys").and_then(|k| k.as_array()) {
        for k in keys {
            let gname = up_names
                .get(&util::str_or(k.get("upstream_id"), ""))
                .cloned()
                .unwrap_or_else(|| "未分组".into());
            let g = match groups.iter_mut().find(|(n, _)| *n == gname) {
                Some((_, cells)) => cells,
                None => {
                    groups.push((gname, Vec::new()));
                    &mut groups.last_mut().unwrap().1
                }
            };
            let (s, why) = if !k.get("enabled").map(util::truthy).unwrap_or(false) {
                (2, "停用".to_string())
            } else if util::str_or(k.get("status"), "") == "invalid" {
                (2, "密钥失效".to_string())
            } else if util::int_or(k.get("banned_until"), 0) > now {
                (2, {
                    let r = util::str_or(k.get("ban_reason"), "");
                    if r.is_empty() { "封禁".to_string() } else { r }
                })
            } else if util::int_or(k.get("cooldown_until"), 0) > now {
                (1, "冷却中".to_string())
            } else if conc > 0 && pool::inflight_of(&util::str_or(k.get("id"), "")) >= conc {
                (1, "并发占用中".to_string())
            } else {
                (0, "可用".to_string())
            };
            g.push(json!({"id": util::str_or(k.get("id"), ""), "s": s, "w": why}));
        }
    }
    let groups_json: Vec<Value> = groups
        .into_iter()
        .map(|(name, cells)| {
            json!({
                "name": name,
                "cells": cells,
                "total": cells.len(),
                "ok": cells.iter().filter(|c| c["s"] == 0).count(),
                "busy": cells.iter().filter(|c| c["s"] == 1).count(),
                "bad": cells.iter().filter(|c| c["s"] == 2).count(),
            })
        })
        .collect();
    json_resp(json!({"groups": groups_json}))
}

pub async fn tokens_list(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
    json_resp(json!({"rows": tokens_rows(&cfg)}))
}

fn list_arg(v: Option<&Value>) -> Vec<String> {
    let items: Vec<String> = match v {
        Some(Value::String(s)) => s.replace('\n', ",").split(',').map(|x| x.to_string()).collect(),
        Some(Value::Array(a)) => a.iter().map(|x| util::str_or(Some(x), "")).collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for x in items {
        let s = util::str_cut(x.trim(), 80);
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out.truncate(50);
    out
}

pub async fn tokens_add(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let mut t = util::str_or(body.get("t"), "").trim().to_string();
    let m = body.get("m").cloned().unwrap_or(Value::Null);
    if t.is_empty() {
        t = format!("sk-gw-{}", util::rand_hex(16));
    }
    if t.chars().count() < 8 || t.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "令牌长度需在 8-200 之间"}}))).into_response();
    }
    let raw_list: Vec<String> = match &m {
        Value::String(s) => s.replace('，', ",").split(',').map(|x| x.trim().to_string()).collect(),
        Value::Array(a) => a.iter().map(|x| util::str_or(Some(x), "")).collect(),
        Value::Null => Vec::new(),
        other => vec![util::str_or(Some(other), "")],
    };
    let mut mlist: Vec<String> = Vec::new();
    for x in raw_list {
        let s = x.trim().to_string();
        if !s.is_empty() && !mlist.contains(&s) {
            mlist.push(s);
        }
    }
    let mut dup = false;
    store().update(|db| {
        let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) else { return };
        let toks = cfg
            .entry("gateway_tokens".to_string())
            .or_insert_with(|| Value::Array(vec![]));
        if let Some(arr) = toks.as_array_mut() {
            for x in arr.iter() {
                let xt = match x {
                    Value::Object(m) => util::str_or(m.get("t"), ""),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                if xt == t {
                    dup = true;
                    return;
                }
            }
            arr.push(json!({"t": t, "m": mlist}));
        }
    });
    store().flush();
    if dup {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "令牌已存在"}}))).into_response();
    }
    json_resp(json!({"ok": true, "token": t, "m": mlist}))
}

pub async fn tokens_update(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let t = util::str_or(body.get("t"), "");
    let m = body.get("m").cloned().unwrap_or(Value::Null);
    let raw_list: Vec<String> = match &m {
        Value::String(s) => s.replace('，', ",").split(',').map(|x| x.trim().to_string()).collect(),
        Value::Array(a) => a.iter().map(|x| util::str_or(Some(x), "")).collect(),
        _ => Vec::new(),
    };
    let mut mlist: Vec<String> = Vec::new();
    for x in raw_list {
        let s = x.trim().to_string();
        if !s.is_empty() && !mlist.contains(&s) {
            mlist.push(s);
        }
    }
    let mut found = false;
    store().update(|db| {
        if let Some(tokens) = db
            .get_mut("config")
            .and_then(|c| c.get_mut("gateway_tokens"))
            .and_then(|t| t.as_array_mut())
        {
            for x in tokens.iter_mut() {
                let xt = match x {
                    Value::Object(mm) => util::str_or(mm.get("t"), ""),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                if xt == t {
                    *x = json!({"t": t, "m": mlist});
                    found = true;
                    return;
                }
            }
        }
    });
    store().flush();
    if !found {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "令牌不存在"}}))).into_response();
    }
    json_resp(json!({"ok": true}))
}

pub async fn tokens_delete(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let t = util::str_or(body.get("t"), "");
    let mut removed = false;
    store().update(|db| {
        if let Some(tokens) = db
            .get_mut("config")
            .and_then(|c| c.get_mut("gateway_tokens"))
            .and_then(|t2| t2.as_array_mut())
        {
            let before = tokens.len();
            tokens.retain(|x| {
                let xt = match x {
                    Value::Object(m) => util::str_or(m.get("t"), ""),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                xt != t
            });
            removed = tokens.len() != before;
        }
    });
    store().flush();
    if !removed {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "令牌不存在"}}))).into_response();
    }
    json_resp(json!({"ok": true}))
}

pub async fn intercept_get(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(json!({}));
    let rules: Vec<Value> = cfg
        .get("custom_rules")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter(|r| r.is_object()).cloned().collect())
        .unwrap_or_default();
    let cap = util::cfg_int(&cfg, "intercept_log_max", 100).max(1);
    let logs: Vec<Value> = db
        .get("intercepted")
        .and_then(|l| l.as_array())
        .map(|a| a.iter().filter(|x| x.is_object()).take(cap as usize).cloned().collect())
        .unwrap_or_default();
    json_resp(json!({
        "enabled": cfg.get("intercept_enabled").map(util::truthy).unwrap_or(false),
        "rules": rules,
        "logs": logs,
        "cap": cap,
    }))
}

pub async fn intercept_rule_add(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let mode = util::str_or(body.get("match_mode"), "contains");
    if !["contains", "equals", "prefix", "suffix", "regex"].contains(&mode.as_str()) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "未知匹配模式"}}))).into_response();
    }
    // 先按存储上限截断、再校验；尾部换行去掉（textarea 里的回车会让回复多一个空行）
    let mut pattern = util::str_cut(util::str_or(body.get("pattern"), "").trim(), 1000);
    pattern = util::unescape_quote_entities(&pattern);
    let reply = {
        let r = util::str_or(body.get("reply"), "");
        let trimmed = r.trim_end_matches(['\r', '\n']);
        util::str_cut(trimmed, 2000)
    };
    let name = util::str_cut(util::str_or(body.get("name"), "").trim(), 40);
    let models = list_arg(body.get("models"));
    let by_name: HashMap<String, String> = store()
        .load()
        .get("upstreams")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().filter(|u| u.is_object()).map(|u| (util::str_or(u.get("name"), ""), util::str_or(u.get("id"), ""))).collect())
        .unwrap_or_default();
    let upstreams_arg: Vec<String> = list_arg(body.get("upstreams"))
        .into_iter()
        .map(|x| by_name.get(&x).cloned().unwrap_or(x))
        .collect();
    if pattern.is_empty() || reply.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "匹配内容与回复内容不能为空"}}))).into_response();
    }
    if mode == "regex" {
        if let Err(e) = regex::Regex::new(&pattern) {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": format!("正则无效: {}", e)}}))).into_response();
        }
        // 嵌套量词在长输入上会灾难性回溯（移植保留同一护栏，行为与 Python 版一致）
        let nested = regex::Regex::new(r"\([^()]*[+*][^()]*\)\s*[+*{]").unwrap();
        if nested.is_match(&pattern) {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"message": "正则含嵌套量词(如 (a+)+),长输入会灾难性回溯,请改写"}})),
            )
                .into_response();
        }
    }
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            let rules = cfg
                .entry("custom_rules".to_string())
                .or_insert_with(|| Value::Array(vec![]));
            if let Some(arr) = rules.as_array_mut() {
                arr.push(json!({
                    "id": format!("r_{}", util::rand_hex(4)),
                    "name": name,
                    "match_mode": mode,
                    "pattern": pattern,
                    "reply": reply,
                    "models": models,
                    "upstreams": upstreams_arg,
                }));
            }
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

pub async fn intercept_rule_delete(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let rid = util::str_or(body.get("id"), "");
    let mut removed = false;
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            if let Some(rules) = cfg.get_mut("custom_rules").and_then(|r| r.as_array()) {
                let before = rules.len();
            }
            if let Some(rules) = cfg.get_mut("custom_rules").and_then(|r| r.as_array_mut()) {
                let before = rules.len();
                rules.retain(|r| !(r.is_object() && util::str_or(r.get("id"), "") == rid));
                removed = rules.len() != before;
            }
        }
    });
    store().flush();
    json_resp(json!(removed))
}

pub async fn intercept_test(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let mode = util::str_or(body.get("match_mode"), "contains");
    if !["contains", "equals", "prefix", "suffix", "regex"].contains(&mode.as_str()) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "未知匹配模式"}}))).into_response();
    }
    let raw = util::str_or(body.get("pattern"), "");
    let sample = util::str_or(body.get("sample"), "");
    let fixed = util::unescape_quote_entities(&raw);
    let mut reason = String::new();
    if raw.trim().is_empty() {
        reason = "匹配内容为空".into();
    } else if sample.trim().is_empty() {
        reason = "样本为空".into();
    } else if mode == "regex" {
        if let Err(e) = regex::Regex::new(&fixed) {
            reason = format!("正则无效:{}", e);
        }
    }
    let matched = reason.is_empty() && util::match_text(&mode, &fixed, &sample, 0);
    if !matched && reason.is_empty() {
        reason = if mode == "regex" {
            "正则不匹配（常见原因：粘贴时正则被截断；或引号被 HTML 转义成 &quot;）".into()
        } else {
            "样本里没有这段内容".into()
        };
    }
    json_resp(json!({
        "ok": true,
        "matched": matched,
        "pattern": fixed,
        "entity_fixed": fixed != raw,
        "reason": if matched { "" } else { &reason },
    }))
}

pub async fn intercept_toggle(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Some(body) = json_dict(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let enabled = util::as_bool(body.get("enabled").unwrap_or(&Value::Bool(false)), false);
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            cfg.insert("intercept_enabled".into(), Value::from(enabled));
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

pub async fn intercept_clear(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("intercepted".into(), Value::Array(vec![]));
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

pub async fn training_list(headers: &HeaderMap, query: Option<String>) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let max_n = util::cfg_int(db.get("config").unwrap_or(&json!({})), "training_log_max", 0);
    let n = query_get(&query, "n").parse::<i64>().unwrap_or(50).clamp(1, 200);
    let all = db.get("training").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let total = all.len() as i64;
    let rows: Vec<Value> = all.into_iter().take(n as usize).collect();
    json_resp(json!({"rows": rows, "total": total, "max": max_n}))
}

pub async fn training_clear(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("training".into(), Value::Array(vec![]));
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

pub async fn training_export(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let mut lines: Vec<String> = Vec::new();
    if let Some(all) = db.get("training").and_then(|t| t.as_array()) {
        for e in all.iter().rev() {
            let Some(eo) = e.as_object() else { continue };
            let mut msgs: Vec<Value> = eo
                .get("messages")
                .and_then(|m| m.as_array())
                .map(|a| a.iter().filter(|m| m.is_object()).cloned().collect())
                .unwrap_or_default();
            msgs.push(json!({"role": "assistant", "content": util::str_or(eo.get("response"), "")}));
            lines.push(
                serde_json::to_string(&json!({"messages": msgs, "model": util::str_or(eo.get("model"), "")}))
                    .unwrap_or_default(),
            );
        }
    }
    let mut resp = format!("{}\n", lines.join("\n")).into_response();
    if lines.is_empty() {
        resp = String::new().into_response();
    }
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/jsonl; charset=utf-8"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=training.jsonl"),
    );
    resp
}

pub async fn sessions_list(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let max_n = util::cfg_int(db.get("config").unwrap_or(&json!({})), "session_log_max", 100);
    let rows = db
        .get("sessions")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().take(max_n.max(0) as usize).cloned().collect())
        .unwrap_or(json!([]));
    json_resp(json!({"rows": rows, "max": max_n}))
}

pub async fn sessions_clear(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("sessions".into(), Value::Array(vec![]));
        }
    });
    store().flush();
    json_resp(json!({"ok": true}))
}

pub async fn config_export(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
    json_resp(json!({
        "_type": "gateway-config",
        "version": "1.5.0",
        "config": cfg_without_secrets(&cfg),
    }))
}

pub async fn config_import(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body_v) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "配置文件不是合法 JSON"}}))).into_response();
    };
    if !body_v.is_object() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "配置文件格式错误：顶层必须是对象"}}))).into_response();
    }
    let incoming = if body_v.get("config").and_then(|c| c.as_object()).is_some() {
        body_v.get("config").cloned().unwrap()
    } else {
        body_v.clone()
    };
    let mut applied: i64 = 0;
    store().update(|db| {
        apply_settings(db, &incoming, &mut applied);
        // config_import 的 gateway_tokens 走数组形式
        if let Some(gt) = incoming.get("gateway_tokens").and_then(|g| g.as_array()) {
            let mut struct_tokens: Vec<Value> = Vec::new();
            for t in gt {
                match t {
                    Value::String(s) if !s.is_empty() => struct_tokens.push(json!({"t": s, "m": []})),
                    Value::Object(m) => {
                        let tv = util::str_or(m.get("t"), "");
                        if !tv.is_empty() {
                            let models: Vec<String> = m
                                .get("m")
                                .and_then(|x| x.as_array())
                                .map(|a| a.iter().map(|x| util::str_or(Some(x), "")).collect())
                                .unwrap_or_default();
                            struct_tokens.push(json!({"t": tv, "m": models}));
                        }
                    }
                    _ => {}
                }
            }
            if !struct_tokens.is_empty() {
                if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
                    cfg.insert("gateway_tokens".into(), Value::Array(struct_tokens));
                    applied += 1;
                }
            }
        }
    });
    store().flush();
    proxy::invalidate_cfg_cache();
    json_resp(json!({"ok": true, "applied": applied}))
}

pub async fn remote_update(headers: &HeaderMap) -> Response {
    // 路径A(管理令牌):Authorization: Bearer <update_token>；路径B(管理会话):admin 登录+CSRF
    let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
    let token = util::str_or(cfg.get("update_token"), "");
    let auth = headers.get("authorization").and_then(|x| x.to_str().ok()).unwrap_or("");
    let bearer = if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        auth[7..].trim().to_string()
    } else {
        String::new()
    };
    let token_ok = !token.is_empty() && !bearer.is_empty() && constant_eq(&token, &bearer);
    if !token_ok {
        if let Err(e) = require(headers, true) {
            return e;
        }
    }
    if !cfg.get("update_enabled").map(util::truthy).unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "远程更新未开启(后台「设置 → 排队与其他」打开开关)"}})),
        )
            .into_response();
    }
    let (ok, msg) = crate::update::remote_update().await;
    if !ok {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": {"message": msg}})),
        )
            .into_response();
    }
    json_resp(json!({"ok": true, "note": msg}))
}

pub async fn remote_rollback(headers: &HeaderMap) -> Response {
    let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
    let token = util::str_or(cfg.get("update_token"), "");
    let auth = headers.get("authorization").and_then(|x| x.to_str().ok()).unwrap_or("");
    let bearer = if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        auth[7..].trim().to_string()
    } else {
        String::new()
    };
    let token_ok = !token.is_empty() && !bearer.is_empty() && constant_eq(&token, &bearer);
    if !token_ok {
        if let Err(e) = require(headers, true) {
            return e;
        }
    }
    if !cfg.get("update_enabled").map(util::truthy).unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "远程更新未开启,回滚不可用"}})),
        )
            .into_response();
    }
    let (ok, msg) = crate::update::remote_rollback().await;
    if !ok {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": {"message": msg}})),
        )
            .into_response();
    }
    json_resp(json!({"ok": true, "note": msg}))
}
