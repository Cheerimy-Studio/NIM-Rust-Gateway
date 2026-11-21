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
    let cfg = cfg.get("config").unwrap_or(&Value::Null);
    let got = cookie_get(headers, "ngw_session").unwrap_or_default();
    !got.is_empty() && crate::store::session_valid(cfg, &got)
}

static LOGIN_RATE: Mutex<Option<Vec<(String, Vec<f64>)>>> = Mutex::new(None);

pub(crate) fn login_rate_ok(ip: &str) -> bool {
    let now = util::now_f();
    let mut g = LOGIN_RATE.lock().unwrap();
    let m = g.get_or_insert_with(Vec::new);
    if m.len() >= 4096 {
        m.retain(|(_, v)| v.last().is_some_and(|t| now - t < 300.0));
        if m.len() >= 4096 && !m.iter().any(|(k, _)| k == ip) {
            return false;
        }
    }
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

pub(crate) static CODE_RATE: Mutex<Option<Vec<(String, f64)>>> = Mutex::new(None);

/// 验证码发送限流：每 IP 每分钟最多 1 次。
pub(crate) fn code_rate_ok(ip: &str) -> bool {
    let now = util::now_f();
    let mut g = CODE_RATE.lock().unwrap();
    let m = g.get_or_insert_with(Vec::new);
    if let Some(pos) = m.iter().position(|(k, _)| *k == *ip) {
        if now - m[pos].1 < 60.0 {
            return false;
        }
        m[pos].1 = now;
        return true;
    }
    m.push((ip.to_string(), now));
    if m.len() > 5000 {
        m.retain(|(_, t)| now - *t < 60.0);
    }
    true
}

pub(crate) fn login_rate_clear(ip: &str) {
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
        if !csrf_header_matches(headers, &expected) {
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
            return percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned();
        }
    }
    String::new()
}

// ---------------------------------------------------------------- 路由处理

pub async fn login(headers: &HeaderMap, ip: String, body: Bytes) -> Response {
    if !login_content_type_ok(headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE,
            axum::Json(json!({"error": {"message": "登录请求必须使用 application/json", "type": "auth"}}))).into_response();
    }
    if !login_origin_ok(headers) {
        return (StatusCode::FORBIDDEN,
            axum::Json(json!({"error": {"message": "跨站登录被拒绝", "type": "auth"}}))).into_response();
    }
    let body_v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cfg = store().load();
    let cfg = cfg.get("config").cloned().unwrap_or(json!({}));
    let username = util::str_or(body_v.get("username"), "").trim().to_string();
    let password = util::str_or(body_v.get("password"), "");
    let rate_key = format!("admin:{ip}:{}", util::char_prefix(&username.to_lowercase(), 320));
    if !login_rate_ok(&rate_key) {
        return login_throttled();
    }
    let ok_user = username.eq_ignore_ascii_case(&util::str_or(cfg.get("admin_username"), ""));
    let ok_pass = match verify_login_password(password, Some(util::str_or(cfg.get("admin_password_hash"), ""))).await {
        Ok(ok) => ok,
        Err(response) => return response,
    };
    if !(ok_user && ok_pass) {
        let body = json!({"error": {"message": "账号或密码错误", "type": "auth"}});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }
    login_rate_clear(&rate_key);
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

pub async fn logout(headers: &HeaderMap) -> Response {
    let cookie = cookie_get(headers, "ngw_session").unwrap_or_default();
    if !cookie.is_empty() {
        crate::store::revoke_admin_session(&cookie);
    }
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
    let page = query_get(&query, "page").parse::<i64>().unwrap_or(1).clamp(1, 1_000_000);
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

/// 最近请求状态（新→旧，最多 10 个）：0=失败，1=成功，2=重试后成功。
fn recent_flags(db: &Value, key: &str) -> Vec<i64> {
    db.get("up_recent")
        .and_then(|r| r.get(key))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(10)
                .map(|v| v.as_i64().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
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
        o.insert("recent_flags".into(), json!(recent_flags(db, &uid)));
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

/// 调用看板：按本地日聚合请求统计与计费口径，返回模型/渠道/付费占比和趋势。
pub async fn analytics(headers: &HeaderMap, query: Option<String>) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let days = query_get(&query, "days").parse::<i64>().unwrap_or(7).clamp(1, 14);
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(json!({}));
    let auto_names: std::collections::HashSet<String> = crate::routing::auto_names(&cfg)
        .into_iter()
        .map(|n| n.to_lowercase())
        .collect();
    let now = util::now_i();
    let day_keys: Vec<String> = (0..days).rev().map(|i| util::local_day(now - i * 86400)).collect();
    let wanted: std::collections::HashSet<&str> = day_keys.iter().map(|s| s.as_str()).collect();

    // 升级前的数据没有 stats 计费字段，用用户计费日志兜底（日志有保留上限，仅作近期补充）
    #[derive(Default)]
    struct DayAgg {
        cost: f64,
        in_tok: i64,
        out_tok: i64,
        cached_tok: i64,
        paid: i64,
        free: i64,
        channels: HashMap<String, i64>,
        models: HashMap<String, i64>,
        model_cost: HashMap<String, f64>,
    }
    let mut fallback: HashMap<String, DayAgg> = HashMap::new();
    for arr_name in ["user_logs_paid", "user_logs_free"] {
        if let Some(arr) = db.get(arr_name).and_then(|v| v.as_array()) {
            for r in arr {
                let day = util::local_day(util::int_or(r.get("t"), 0));
                if !wanted.contains(day.as_str()) {
                    continue;
                }
                let a = fallback.entry(day).or_default();
                let cost = util::f64_or(r.get("cost"), 0.0);
                a.cost += cost;
                a.in_tok += util::int_or(r.get("in_tok"), 0);
                a.out_tok += util::int_or(r.get("out_tok"), 0);
                a.cached_tok += util::int_or(r.get("cached_tok"), 0);
                if cost > 0.0 {
                    a.paid += 1;
                } else {
                    a.free += 1;
                }
                let ch = util::str_or(r.get("channel"), "");
                *a.channels
                    .entry(if ch.is_empty() { "-".into() } else { ch })
                    .or_insert(0) += 1;
                let model = util::str_or(r.get("model"), "");
                if !model.is_empty() {
                    *a.models.entry(model.clone()).or_insert(0) += 1;
                    *a.model_cost.entry(model).or_insert(0.0) += cost;
                }
            }
        }
    }

    let mut ch_names: HashMap<String, String> = HashMap::new();
    if let Some(arr) = db.get("upstreams").and_then(|v| v.as_array()) {
        for u in arr {
            ch_names.insert(util::str_or(u.get("id"), ""), util::str_or(u.get("name"), ""));
        }
    }

    let mut calls = 0i64;
    let mut success = 0i64;
    let mut fail = 0i64;
    let mut cost_sum = 0.0f64;
    let mut in_tok = 0i64;
    let mut out_tok = 0i64;
    let mut cached_tok = 0i64;
    let mut paid = 0i64;
    let mut free = 0i64;
    let mut models: HashMap<String, (i64, f64)> = HashMap::new();
    let mut channels: HashMap<String, (i64, f64)> = HashMap::new();
    let mut trend: Vec<Value> = Vec::new();
    let stats = db.get("stats").and_then(|v| v.as_object());
    for day in &day_keys {
        let st = stats.and_then(|s| s.get(day));
        let d_calls = st.map(|s| util::int_or(s.get("total"), 0)).unwrap_or(0);
        let d_success = st.map(|s| util::int_or(s.get("success"), 0)).unwrap_or(0);
        let d_fail = st.map(|s| util::int_or(s.get("fail"), 0)).unwrap_or(0);
        let fb = fallback.get(day);
        let billed = st.map(|s| s.get("billed").is_some()).unwrap_or(false);
        let (d_cost, d_in, d_out, d_cached, d_paid, d_free) = if billed {
            let s = st.unwrap();
            (
                util::f64_or(s.get("cost"), 0.0),
                util::int_or(s.get("in_tok"), 0),
                util::int_or(s.get("out_tok"), 0),
                util::int_or(s.get("cached_tok"), 0),
                util::int_or(s.get("paid"), 0),
                util::int_or(s.get("free"), 0),
            )
        } else if let Some(f) = fb {
            (f.cost, f.in_tok, f.out_tok, f.cached_tok, f.paid, f.free)
        } else {
            (0.0, 0, 0, 0, 0, 0)
        };
        calls += d_calls;
        success += d_success;
        fail += d_fail;
        cost_sum += d_cost;
        in_tok += d_in;
        out_tok += d_out;
        cached_tok += d_cached;
        paid += d_paid;
        free += d_free;
        let mut day_models: HashMap<String, i64> = HashMap::new();
        if let Some(m) = st.and_then(|s| s.get("models")).and_then(|m| m.as_object()) {
            for (k, v) in m {
                day_models.insert(k.clone(), util::int_or(Some(v), 0));
            }
        } else if let Some(f) = fb {
            day_models = f.models.clone();
        }
        let mut day_model_cost: HashMap<String, f64> = HashMap::new();
        if let Some(m) = st.and_then(|s| s.get("model_cost")).and_then(|m| m.as_object()) {
            for (k, v) in m {
                day_model_cost.insert(k.clone(), util::f64_or(Some(v), 0.0));
            }
        } else if let Some(f) = fb {
            day_model_cost = f.model_cost.clone();
        }
        let mut day_channels: HashMap<String, i64> = HashMap::new();
        if let Some(m) = st.and_then(|s| s.get("channels")).and_then(|m| m.as_object()) {
            for (k, v) in m {
                day_channels.insert(k.clone(), util::int_or(Some(v), 0));
            }
        } else if let Some(f) = fb {
            day_channels = f.channels.clone();
        }
        for (name, count) in day_models {
            let e = models.entry(name.clone()).or_insert((0, 0.0));
            e.0 += count;
            e.1 += day_model_cost.get(&name).copied().unwrap_or(0.0);
        }
        for (id, count) in day_channels {
            let e = channels.entry(id).or_insert((0, 0.0));
            e.0 += count;
        }
        trend.push(json!({
            "day": if day.len() >= 10 { &day[5..] } else { day.as_str() },
            "calls": d_calls,
            "success": d_success,
            "fail": d_fail,
            "cost": util::round6(d_cost),
        }));
    }
    let mut model_rows: Vec<(String, i64, f64)> =
        models.into_iter().map(|(m, (c, cost))| (m, c, cost)).collect();
    model_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let model_rows: Vec<Value> = model_rows
        .into_iter()
        .take(50)
        .map(|(model, count, cost)| json!({"model": model, "count": count, "cost": util::round6(cost)}))
        .collect();
    let mut channel_rows: Vec<(String, i64)> = channels.into_iter().map(|(id, (c, _))| (id, c)).collect();
    channel_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let channel_rows: Vec<Value> = channel_rows
        .into_iter()
        .map(|(id, count)| {
            let name = ch_names.get(&id).cloned().unwrap_or_default();
            json!({
                "id": id,
                "name": if id == "-" { "未分配".to_string() } else if name.is_empty() { id.clone() } else { name },
                "count": count,
            })
        })
        .collect();
    let auto_calls: i64 = model_rows
        .iter()
        .filter(|m| auto_names.contains(&util::str_or(m.get("model"), "").to_lowercase()))
        .map(|m| util::int_or(m.get("count"), 0))
        .sum();
    let auto_cost: f64 = model_rows
        .iter()
        .filter(|m| auto_names.contains(&util::str_or(m.get("model"), "").to_lowercase()))
        .map(|m| util::f64_or(m.get("cost"), 0.0))
        .sum();
    json_resp(json!({
        "days": days,
        "server_time": now,
        "totals": {
            "calls": calls,
            "success": success,
            "fail": fail,
            "rate": if calls > 0 { Some((success * 100 / calls) as i64) } else { None },
            "cost": util::round6(cost_sum),
            "in_tok": in_tok,
            "out_tok": out_tok,
            "cached_tok": cached_tok,
            "paid": paid,
            "free": free,
        },
        "trend": trend,
        "models": model_rows,
        "channels": channel_rows,
        "auto": {"calls": auto_calls, "cost": util::round6(auto_cost)},
    }))
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
    "smtp_port",
    "recovery_max_ms",
    "tool_compact_threshold",
    "tool_compact_max",
];
const STR_SETTINGS: &[&str] = &[
    "upstream_base",
    "timezone",
    "model_whitelist",
    "model_blacklist",
    "param_overrides",
    "update_token",
    "smtp_host",
    "smtp_user",
    "smtp_pass",
    "smtp_from",
    "reg_email_regex",
    "reg_email_error_msg",
    "model_chains",
    "auto_routes",
    "model_limits",
    "public_origin",
    "allowed_origins",
    "trusted_proxies",
];
const BOOL_SETTINGS: &[&str] = &[
    "recovery_enabled",
    "tool_compact_enabled",
    "transparent_upstream",
    "device_login_enabled",
    "log_enabled",
    "queue_enabled",
    "update_enabled",
    "hide_upstream_errors",
    "hide_mapped_names",
    "breaker_enabled",
    "watchdog_enabled",
    "sign_enabled",
    "smtp_tls",
    "reg_email_enabled",
];
/// 浮点设置（金额类）：round6 量化，负值忽略。
const FLOAT_SETTINGS: &[&str] = &["sign_min", "sign_max"];

fn cfg_without_secrets(cfg: &Value) -> Value {
    let mut c = cfg.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("admin_password_hash");
        o.remove("session_secret");
        o.remove("smtp_pass");
        // 旧版单 Auto 配置在这里归一化成 auto_routes，前端只处理一种结构。
        o.remove("routing_rules");
        o.remove("auto_enabled");
        o.remove("auto_price");
        o.remove("verify_tls");
        o.remove("allow_private_upstreams");
        o.insert(
            "auto_routes".into(),
            Value::from(crate::routing::routes_json(&crate::routing::auto_routes(cfg))),
        );
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

// ---------------------------------------------------------------- 模型接入（上游模型 → 别名 → 暴露）

fn model_limits_set(text: &str, key: &str, ctx: i64, out: i64, tps: i64) -> String {
    let low = key.to_lowercase();
    let mut kept: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let name = t.split('=').next().unwrap_or("").trim().to_lowercase();
        if name == low {
            continue;
        }
        kept.push(t.to_string());
    }
    if ctx > 0 || out > 0 || tps > 0 {
        let mut parts: Vec<String> = Vec::new();
        if ctx > 0 {
            parts.push(format!("ctx:{ctx}"));
        }
        if out > 0 {
            parts.push(format!("out:{out}"));
        }
        if tps > 0 {
            parts.push(format!("tps:{tps}"));
        }
        kept.push(format!("{key}={}", parts.join(",")));
    }
    kept.join("\n")
}

/// 别名改名/隐藏后，旧键的标注行永远没人读：一并清掉
fn model_limits_remove(text: &str, keys: &[String]) -> String {
    if keys.is_empty() {
        return text.to_string();
    }
    let drop: Vec<String> = keys.iter().map(|k| k.to_lowercase()).collect();
    text.lines()
        .filter(|line| {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                return true;
            }
            let name = t.split('=').next().unwrap_or("").trim().to_lowercase();
            !drop.contains(&name)
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

fn upstream_alias_rows(db: &Value) -> Vec<Value> {
    let cfg = db.get("config").cloned().unwrap_or(json!({}));
    let global_hide = cfg.get("hide_mapped_names").map(util::truthy).unwrap_or(true);
    let transparent = crate::upstreams::transparent_upstream(&cfg);
    let limits = crate::upstreams::parse_model_limits(&cfg);
    let catalog = db.get("upstream_catalog").cloned().unwrap_or(json!({}));
    let mut rows: Vec<Value> = Vec::new();
    let Some(ups) = db.get("upstreams").and_then(|x| x.as_array()) else { return rows };
    for u in ups {
        if !u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false) {
            continue;
        }
        let uid = util::str_or(u.get("id"), "");
        let uname = util::str_or(u.get("name"), "");
        let models_list: Vec<String> = u
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().map(|m| util::str_or(Some(m), "")).filter(|m| !m.is_empty()).collect())
            .unwrap_or_default();
        let map = u.get("model_map").and_then(|m| m.as_object()).cloned().unwrap_or_default();
        let prices = u.get("prices").and_then(|m| m.as_object()).cloned().unwrap_or_default();
        let cat_models: Vec<String> = catalog
            .get(&uid)
            .and_then(|c| c.get("models"))
            .and_then(|m| m.as_array())
            .map(|a| a.iter().map(|m| util::str_or(Some(m), "")).filter(|m| !m.is_empty()).collect())
            .unwrap_or_default();
        // 上游真名集合 = 映射目标 ∪ 上游目录 ∪ 清单里手列的真名（不是任何别名）
        let mut ups_names: Vec<String> = Vec::new();
        {
            let push_name = |n: &str, names: &mut Vec<String>| {
                if !n.is_empty() && names.len() < 800 && !names.iter().any(|x| x == n) {
                    names.push(n.to_string());
                }
            };
            for v in map.values() {
                push_name(&util::str_or(Some(v), ""), &mut ups_names);
            }
            for m in &cat_models {
                push_name(m, &mut ups_names);
            }
            for m in &models_list {
                if !map.contains_key(m) {
                    push_name(m, &mut ups_names);
                }
            }
        }
        ups_names.sort();
        for upstream in &ups_names {
            // 反查别名：优先取已在 models 清单里生效的
            let mut alias = String::new();
            for (k, v) in &map {
                if util::str_or(Some(v), "") == *upstream && models_list.iter().any(|m| m == k) {
                    alias = k.clone();
                    break;
                }
            }
            if alias.is_empty() {
                for (k, v) in &map {
                    if util::str_or(Some(v), "") == *upstream {
                        alias = k.clone();
                        break;
                    }
                }
            }
            let exposed = models_list.iter().any(|m| m == upstream)
                || (!alias.is_empty() && models_list.iter().any(|m| m == &alias));
            let client = if alias.is_empty() { upstream.clone() } else { alias.clone() };
            let meta = limits.get(&client.to_lowercase()).copied().unwrap_or_default();
            let (ctx, out, tps) = (meta.ctx, meta.out, meta.tps);
            let routable = crate::upstreams::channel_serves_model(u, &client, global_hide, transparent)
                || models_list.iter().any(|m| m == upstream);
            let price = prices.get(&client).cloned().unwrap_or(json!(0));
            let recent = recent_flags(db, &format!("{}{}{}", uid, '\u{0}', client));
            rows.push(json!({
                "channel_id": uid, "channel_name": uname,
                "upstream": upstream, "alias": alias,
                "exposed": exposed, "routable": routable,
                "ctx": ctx, "out": out, "tps": tps, "price": price,
                "recent": recent,
            }));
        }
    }
    rows
}

/// 模型路由文本校验（与前端 chainDump 同口径）：
/// 结构（名字/成员数/字符集/重名/嵌套）+ 链名不得遮蔽已接入的模型名。
pub fn validate_model_chains(db: &Value, text: &str) -> Result<(), String> {
    let mut taken: Vec<String> = Vec::new();
    for u in db.get("upstreams").and_then(|x| x.as_array()).unwrap_or(&vec![]) {
        if let Some(ms) = u.get("models").and_then(|m| m.as_array()) {
            for m in ms {
                let n = util::str_or(Some(m), "");
                if !n.is_empty() && !taken.contains(&n) {
                    taken.push(n);
                }
            }
        }
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for k in mm.keys() {
                if !taken.contains(k) {
                    taken.push(k.clone());
                }
            }
        }
    }
    for route in crate::routing::auto_routes(db.get("config").unwrap_or(&Value::Null)) {
        if !taken.iter().any(|n| n.eq_ignore_ascii_case(&route.name)) {
            taken.push(route.name);
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else {
            return Err(format!("模型路由第 {} 行缺少「=」", idx + 1));
        };
        let name = line[..eq].trim().to_string();
        let tag = if name.is_empty() { format!("第 {} 行", idx + 1) } else { name.clone() };
        if name.is_empty() {
            return Err(format!("模型路由{}：路由名不能为空", tag));
        }
        if name.chars().count() > 80 || name.contains(['=', '|', '#']) {
            return Err(format!("模型路由{}：路由名过长或含 = | # 字符", tag));
        }
        if seen.contains(&name) {
            return Err(format!("模型路由{}：路由名重复", tag));
        }
        if taken.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            return Err(format!("模型路由{}：与已接入的模型/别名/Auto 路由重名，会遮蔽原模型", tag));
        }
        seen.push(name);
        let mut rest = line[eq + 1..].trim().to_string();
        if let Some(bar) = rest.rfind('|') {
            let flag = rest[bar + 1..].trim().to_lowercase();
            if flag == "fb" || flag == "fallback" || flag.starts_with("rr") {
                rest = rest[..bar].trim().to_string();
            }
        }
        let members: Vec<String> = rest
            .split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        if members.len() < 2 {
            return Err(format!("模型路由{}：至少需要 2 个成员", tag));
        }
        for mem in &members {
            if mem.chars().count() > 160 || mem.contains(['=', '|', '#']) {
                return Err(format!("模型路由{}：成员 {} 含非法字符或过长", tag, util::char_prefix(mem, 40)));
            }
            if seen.contains(mem) {
                return Err(format!("模型路由{}：成员 {} 是路由名，不支持嵌套", tag, mem));
            }
        }
    }
    Ok(())
}

/// 模型接入视图：每个启用渠道的上游模型（渠道ID/模型名）+ 现有别名/暴露状态/上限
pub async fn upstream_models(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let db = store().load();
    let rows = upstream_alias_rows(&db);
    let transparent = db
        .get("config")
        .map(|c| crate::upstreams::transparent_upstream(c))
        .unwrap_or(false);
    json_resp(json!({ "rows": rows, "transparent": transparent }))
}

/// 别名/隐藏 + 上下文/输出上限，一次行内保存
pub async fn upstream_model_op(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let ch_id = util::str_or(body.get("channel_id"), "");
    let upstream = util::str_or(body.get("upstream"), "").trim().to_string();
    let alias = util::str_or(body.get("alias"), "").trim().to_string();
    let ctx = util::int_or(body.get("ctx"), 0);
    let out = util::int_or(body.get("out"), 0);
    let tps = util::int_or(body.get("tps"), 0);
    // 价格可选：不传 = 不动现有定价（批量标注场景只发 ctx/out/tps）
    let price_req = body.get("price").cloned();
    if ch_id.is_empty() || upstream.is_empty() || upstream.chars().count() > 160 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "参数缺失或模型名过长"}}))).into_response();
    }
    if alias.chars().count() > 160 || alias.chars().any(char::is_control) || upstream.chars().any(char::is_control) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "别名含非法字符"}}))).into_response();
    }
    // 渠道表单把 model_map/models 按文本（= > , ）往返编辑，这些字符进别名
    // 会在下一次表单保存时把配置撕碎
    if alias.contains(['=', '>', '|', ',', '#']) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "别名不能包含 = > | , # 字符"}}))).into_response();
    }
    if !alias.is_empty() {
        let cfg = store().load().get("config").cloned().unwrap_or(json!({}));
        if crate::upstreams::parse_model_chains(&cfg)
            .iter()
            .any(|c| c.name == alias)
        {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "别名与模型路由重名，会让其中一个不可达"}}))).into_response();
        }
    }
    if ctx < 0 || out < 0 || ctx > 100_000_000 || out > 100_000_000 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "上限数值非法"}}))).into_response();
    }
    if tps < 0 || tps > 1_000_000 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "速度数值非法"}}))).into_response();
    }
    if let Some(price) = &price_req {
        if crate::billing::validate(price).is_err() {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "价格数值非法"}}))).into_response();
        }
    }
    let mut err: Option<String> = None;
    store().update(|db| {
        let Some(u) = db
            .get_mut("upstreams")
            .and_then(|x| x.as_array_mut())
            .and_then(|a| a.iter_mut().find(|u| util::str_or(u.get("id"), "") == ch_id))
        else {
            err = Some("渠道不存在".into());
            return;
        };
        let models_list = u.get("models").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        let mut new_models: Vec<String> = models_list
            .iter()
            .map(|m| util::str_or(Some(m), ""))
            .filter(|m| !m.is_empty())
            .collect();
        let mut map = u.get("model_map").and_then(|m| m.as_object()).cloned().unwrap_or_default();
        // 旧别名 = 当前指向该上游名的所有映射键
        let old_aliases: Vec<String> = map
            .iter()
            .filter(|(_, v)| util::str_or(Some(v), "") == upstream)
            .map(|(k, _)| k.clone())
            .collect();
        // 清掉旧暴露：映射键 + models 里对应的别名项 + 手列的上游真名本身
        for k in &old_aliases {
            map.remove(k);
        }
        new_models.retain(|m| m != &upstream && !old_aliases.contains(m));
        if !alias.is_empty() {
            if alias == upstream {
                // 同名暴露：不加自映射，只进清单
                if !new_models.contains(&upstream) {
                    new_models.push(upstream.clone());
                }
            } else {
                map.insert(alias.clone(), Value::from(upstream.clone()));
                if !new_models.contains(&alias) {
                    new_models.push(alias.clone());
                }
            }
        }
        if let Some(o) = u.as_object_mut() {
            o.insert("models".into(), Value::Array(new_models.into_iter().map(Value::from).collect()));
            o.insert("model_map".into(), Value::Object(map));
        }
        // 价格生命周期：改名/取消接入时旧名字的定价一并清理，避免
        // 「模型都没了还在列表里留着价格」；请求带 price 时再写新价
        let client = if alias.is_empty() { upstream.clone() } else { alias.clone() };
        let mut stale_names: Vec<String> = old_aliases.clone();
        if models_list.iter().any(|m| m == &upstream) {
            stale_names.push(upstream.clone());
        }
        stale_names.retain(|k| k.as_str() != client);
        if let Some(o) = u.as_object_mut() {
            let prices = o.entry("prices".to_string()).or_insert_with(|| json!({}));
            if let Some(pm) = prices.as_object_mut() {
                for n in &stale_names {
                    pm.remove(n);
                }
                if alias.is_empty() {
                    pm.remove(&client);
                }
                if let Some(price) = &price_req {
                    if crate::billing::paid(price) {
                        pm.insert(client.clone(), price.clone());
                    } else {
                        pm.remove(&client);
                    }
                }
                if pm.is_empty() {
                    o.remove("prices");
                }
            }
        }
        // 标注写进 config.model_limits，键 = 客户端可见名；改名/隐藏后
        // 旧别名键的行一并清掉（留着永远没人读，只会越积越脏）
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            let text = util::str_or(cfg.get("model_limits"), "");
            let text2 = model_limits_remove(&text, &stale_names);
            let new_text = model_limits_set(&text2, &client, ctx, out, tps);
            cfg.insert("model_limits".into(), Value::from(new_text));
        }
    });
    if let Some(e) = err {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": e}}))).into_response();
    }
    store().flush();
    crate::proxy::invalidate_cfg_cache();
    let db = store().load();
    let rows = upstream_alias_rows(&db)
        .into_iter()
        .find(|r| r["channel_id"] == json!(ch_id) && r["upstream"] == json!(upstream))
        .unwrap_or(json!({}));
    json_resp(json!({"ok": true, "row": rows}))
}

/// 从上游拉取模型目录（仅显式触发；结果缓存进 upstream_catalog，默认隐藏不暴露）
/// 自动检测渠道 Base URL：依次尝试原地址、补 /v1、站点根 /v1 等候选，
/// 用 /models 判断哪一层是真正的 API 前缀。2xx 优先，401/403 视为路径存在。
pub async fn upstream_probe(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let raw = util::str_or(body.get("base"), "").trim().to_string();
    if raw.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "先填写 Base URL"}}))).into_response();
    }
    let Ok(url) = reqwest::Url::parse(&raw) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "Base URL 不是有效地址"}}))).into_response();
    };
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |v: String| {
        let v = v.trim_end_matches('/').to_string();
        if !v.is_empty() && !candidates.contains(&v) && candidates.len() < 5 {
            candidates.push(v);
        }
    };
    let base = raw.trim_end_matches('/').to_string();
    push(base.clone());
    if !base.ends_with("/v1") && !base.contains("/v1/") {
        push(format!("{base}/v1"));
    }
    if let Some(host) = url.host_str() {
        let mut origin = format!("{}://{}", url.scheme(), host);
        if let Some(port) = url.port() {
            origin.push_str(&format!(":{port}"));
        }
        push(format!("{origin}/v1"));
        push(origin);
    }
    let (apikey, auth_mode) = {
        let db = store().load();
        let ch_id = util::str_or(body.get("channel_id"), "");
        let key = if ch_id.is_empty() {
            String::new()
        } else {
            db.get("keys")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter().find(|k| {
                        util::str_or(k.get("upstream_id"), "") == ch_id
                            && k.get("enabled").map(util::truthy).unwrap_or(false)
                    })
                })
                .map(|k| util::str_or(k.get("apikey"), ""))
                .unwrap_or_default()
        };
        let mode = db
            .get("upstreams")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find(|u| util::str_or(u.get("id"), "") == ch_id))
            .map(|u| util::str_or(u.get("auth_mode"), "auto"))
            .unwrap_or_else(|| "auto".into());
        (key, mode)
    };
    let client = get_http(6);
    let mut tried = Vec::new();
    let mut best: Option<(String, u16, i64)> = None;
    for candidate in &candidates {
        let started = std::time::Instant::now();
        let mut req = client
            .get(format!("{candidate}/models"))
            .timeout(std::time::Duration::from_secs(8));
        if !apikey.is_empty() {
            req = if auth_mode == "x-api-key" {
                req.header("x-api-key", &apikey)
            } else {
                req.bearer_auth(&apikey)
            };
        }
        let status = match req.send().await {
            Ok(resp) => resp.status().as_u16(),
            Err(_) => 0,
        };
        let ms = started.elapsed().as_millis() as i64;
        tried.push(json!({"base": candidate, "status": status, "ms": ms}));
        if (200..300).contains(&status) {
            best = Some((candidate.clone(), status, ms));
            break;
        }
        if matches!(status, 401 | 403) && best.is_none() {
            best = Some((candidate.clone(), status, ms));
        }
    }
    if let Some((base, status, ms)) = best {
        json_resp(json!({"ok": (200..300).contains(&status), "base": base, "status": status, "ms": ms, "tried": tried}))
    } else {
        json_resp(json!({"ok": false, "base": "", "tried": tried}))
    }
}

/// 渠道 + 模型测速：用该渠道一个可用账号发一条最小请求，返回状态与耗时。
pub async fn upstream_model_test(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let ch_id = util::str_or(body.get("channel_id"), "");
    let model = util::str_or(body.get("model"), "").trim().to_string();
    if ch_id.is_empty() || model.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "缺少渠道或模型"}}))).into_response();
    }
    let (base, apikey, auth_mode, endpoint) = {
        let db = store().load();
        let Some(u) = db
            .get("upstreams")
            .and_then(|x| x.as_array())
            .and_then(|a| a.iter().find(|u| util::str_or(u.get("id"), "") == ch_id))
            .cloned()
        else {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "渠道不存在"}}))).into_response();
        };
        let key = db
            .get("keys")
            .and_then(|x| x.as_array())
            .and_then(|a| {
                a.iter().find(|k| {
                    util::str_or(k.get("upstream_id"), "") == ch_id
                        && k.get("enabled").map(util::truthy).unwrap_or(false)
                })
            })
            .map(|k| util::str_or(k.get("apikey"), ""))
            .unwrap_or_default();
        (
            util::str_or(u.get("base"), "").trim_end_matches('/').to_string(),
            key,
            util::str_or(u.get("auth_mode"), "auto"),
            util::str_or(u.get("api_endpoint"), "auto"),
        )
    };
    if base.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "渠道缺少 Base URL"}}))).into_response();
    }
    if apikey.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "渠道没有可用账号"}}))).into_response();
    }
    let ep = if endpoint == "auto" { "chat/completions" } else { endpoint.as_str() };
    let (url, payload) = match ep {
        "responses" => (
            format!("{base}/responses"),
            json!({"model":model,"input":"ping","max_output_tokens":1,"stream":false}),
        ),
        "messages" => (
            format!("{base}/messages"),
            json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":"ping"}]}),
        ),
        "completions" => (
            format!("{base}/completions"),
            json!({"model":model,"prompt":"ping","max_tokens":1,"stream":false}),
        ),
        _ => (
            format!("{base}/chat/completions"),
            json!({"model":model,"messages":[{"role":"user","content":"ping"}],"max_tokens":1,"stream":false}),
        ),
    };
    let client = get_http(10);
    let mut req = client.post(&url).timeout(std::time::Duration::from_secs(30)).json(&payload);
    req = if auth_mode == "x-api-key" || (auth_mode == "auto" && ep == "messages") {
        req.header("x-api-key", &apikey)
    } else {
        req.bearer_auth(&apikey)
    };
    let started = std::time::Instant::now();
    let result = req.send().await;
    let ms = started.elapsed().as_millis() as i64;
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(200).collect();
            json_resp(json!({
                "ok": (200..300).contains(&status),
                "status": status, "ms": ms, "model": model,
                "error": if (200..300).contains(&status) { String::new() } else { snippet },
            }))
        }
        Err(e) => json_resp(json!({
            "ok": false, "status": 0, "ms": ms, "model": model, "error": e.to_string(),
        })),
    }
}

pub async fn upstream_models_fetch(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let ch_id = util::str_or(body.get("channel_id"), "");
    let (base, apikey, auth_mode) = {
        let db = store().load();
        let Some(u) = db
            .get("upstreams")
            .and_then(|x| x.as_array())
            .and_then(|a| a.iter().find(|u| util::str_or(u.get("id"), "") == ch_id))
            .cloned()
        else {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "渠道不存在"}}))).into_response();
        };
        let key = db
            .get("keys")
            .and_then(|x| x.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|k| util::str_or(k.get("upstream_id"), "") == ch_id && k.get("enabled").map(|x| util::truthy(x)).unwrap_or(false))
            })
            .map(|k| util::str_or(k.get("apikey"), ""))
            .unwrap_or_default();
        let mode = util::str_or(u.get("auth_mode"), "auto");
        (util::str_or(u.get("base"), ""), key, mode)
    };
    if base.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "渠道缺少 Base URL"}}))).into_response();
    }
    if apikey.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "这个渠道还没有可用账号，先到「账号池」导入再拉取"}}))).into_response();
    }
    let url = format!("{}/models", base.trim_end_matches('/'));
    let client = crate::webhttp::get_http(10);
    let mut req = client.get(&url).timeout(std::time::Duration::from_secs(20));
    req = if auth_mode == "x-api-key" {
        req.header("x-api-key", &apikey)
    } else {
        req.bearer_auth(&apikey)
    };
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": format!("上游不可达: {e}")}}))).into_response();
        }
    };
    let status = resp.status().as_u16();
    let Ok(text) = resp.text().await else {
        return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": "上游响应读取失败"}}))).into_response();
    };
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(160).collect();
        return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": format!("上游返回 HTTP {status}: {snippet}")}}))).into_response();
    }
    let Ok(v) = serde_json::from_slice::<Value>(text.as_bytes()) else {
        return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": "上游响应不是 JSON"}}))).into_response();
    };
    let mut models: Vec<String> = Vec::new();
    if let Some(data) = v.get("data").or_else(|| v.get("models")).and_then(|d| d.as_array()) {
        for m in data {
            let id = util::str_or(m.get("id"), "");
            if !id.is_empty() && id.chars().count() <= 160 && !models.contains(&id) && models.len() < 800 {
                models.push(id);
            }
        }
    }
    if models.is_empty() {
        return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": "上游未返回任何模型"}}))).into_response();
    }
    models.sort();
    let n = models.len();
    store().update(|db| {
        let cat = db.as_object_mut().unwrap().entry("upstream_catalog".to_string()).or_insert_with(|| json!({}));
        if let Some(o) = cat.as_object_mut() {
            o.insert(ch_id.clone(), json!({"models": models, "fetched_at": util::now_i()}));
        }
    });
    store().flush();
    json_resp(json!({"ok": true, "count": n}))
}

/// 模型路由编辑器候选清单：启用渠道的模型/别名 + 现有链名，附可路由标记
pub async fn routable_models(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    json_resp(json!({ "models": crate::upstreams::routing_candidates() }))
}

fn model_alias_names(db: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for u in db.get("upstreams").and_then(Value::as_array).into_iter().flatten() {
        if let Some(models) = u.get("models").and_then(Value::as_array) {
            for m in models {
                let name = util::str_or(Some(m), "");
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        if let Some(map) = u.get("model_map").and_then(Value::as_object) {
            for name in map.keys() {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
    out
}

fn chain_names(db: &Value, text: Option<&str>) -> Vec<String> {
    let fallback = db
        .pointer("/config/model_chains")
        .and_then(Value::as_str)
        .unwrap_or("");
    (text.unwrap_or(fallback))
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once('=')
                .map(|(name, _)| name.trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .collect()
}

fn validate_auto_routes(db: &Value, raw: &str, chains_text: Option<&str>) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| "Auto 路由不是合法 JSON")?;
    let list = value.as_array().ok_or("Auto 路由必须是数组")?;
    if list.len() > 20 {
        return Err("Auto 路由最多 20 个".into());
    }
    let mut taken = model_alias_names(db);
    taken.extend(chain_names(db, chains_text));
    let mut names: Vec<String> = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let tag = format!("第 {} 个 Auto 路由", i + 1);
        let obj = item.as_object().ok_or_else(|| format!("{tag}格式错误"))?;
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if !crate::routing::valid_name(name) {
            return Err(format!("{tag}的名称只能含字母、数字、点、下划线、连字符，最长 64"));
        }
        if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(format!("{tag}的名称重复"));
        }
        if taken.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(format!("{tag}与现有模型/别名/模型路由重名"));
        }
        names.push(name.to_string());
        if let Some(price) = obj.get("price") {
            crate::billing::validate(price).map_err(|e| format!("{tag}的价格：{e}"))?;
        }
        let rules = obj
            .get("rules")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{tag}的规则必须是数组"))?;
        if rules.len() > 100 {
            return Err(format!("{tag}最多 100 条规则"));
        }
        for (j, rule) in rules.iter().enumerate() {
            let rtag = format!("{tag}第 {} 条规则", j + 1);
            let robj = rule.as_object().ok_or_else(|| format!("{rtag}格式错误"))?;
            let target = robj.get("target").and_then(Value::as_str).unwrap_or("").trim();
            if target.is_empty() || target.chars().count() > 160 {
                return Err(format!("{rtag}的目标不能为空且不超过 160 字符"));
            }
            if robj
                .get("image")
                .and_then(Value::as_str)
                .is_some_and(|v| !["any", "yes", "no"].contains(&v))
            {
                return Err(format!("{rtag}的图片条件只能是 any/yes/no"));
            }
            if let Some(keywords) = robj.get("keywords") {
                let arr = keywords.as_array().ok_or_else(|| format!("{rtag}的关键词必须是数组"))?;
                if arr.len() > 30 {
                    return Err(format!("{rtag}最多 30 个关键词"));
                }
                for keyword in arr {
                    let text = keyword.as_str().unwrap_or("");
                    if text.trim().is_empty() || text.chars().count() > 60 {
                        return Err(format!("{rtag}的关键词不能为空且不超过 60 字符"));
                    }
                }
            }
            if robj
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.chars().count() > 60)
            {
                return Err(format!("{rtag}的名称不超过 60 字符"));
            }
        }
    }
    for (i, item) in list.iter().enumerate() {
        let Some(rules) = item.get("rules").and_then(Value::as_array) else {
            continue;
        };
        for (j, rule) in rules.iter().enumerate() {
            let target = rule.get("target").and_then(Value::as_str).unwrap_or("");
            if names.iter().any(|n| n.eq_ignore_ascii_case(target)) {
                return Err(format!(
                    "第 {} 个 Auto 路由第 {} 条规则的目标不能是 Auto 路由",
                    i + 1,
                    j + 1
                ));
            }
        }
    }
    Ok(())
}

fn validate_sign_settings(current: &Value, incoming: &Value) -> Result<(), String> {
    if incoming.get("sign_min").is_none() && incoming.get("sign_max").is_none() { return Ok(()); }
    let read = |key:&str,default:f64| {
        let raw=incoming.get(key).or_else(|| current.get(key));
        let n=util::f64_or(raw,default);
        if !(0.000001..=1000.0).contains(&n) { return Err("签到金额须在 0.000001 至 1000 元之间".to_string()); }
        Ok(util::round6(n))
    };
    let min=read("sign_min",0.001)?;
    let max=read("sign_max",0.01)?;
    // 显式无效值不能被当作缺省配置接受。
    for key in ["sign_min","sign_max"] {
        if let Some(raw)=incoming.get(key) {
            if util::f64_or(Some(raw),-1.0) < 0.000001 || raw.is_boolean() {
                return Err("签到金额格式错误".into());
            }
        }
    }
    if min > max { return Err("签到最小金额不能大于最大金额".into()); }
    Ok(())
}

fn apply_settings(db: &mut Value, incoming: &Value, applied: &mut i64) {
    let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) else { return };
    cfg.remove("verify_tls");
    cfg.remove("allow_private_upstreams");
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
        // SMTP 授权码特殊：空值=保持不变（前端回显时不带密码，防止泄露与误存占位符）
        if k.starts_with("smtp_pass") {
            if let Some(Value::String(s)) = incoming.get(*k) {
                let t = s.trim();
                if !t.is_empty() {
                    cfg.insert(k.to_string(), Value::from(t));
                    *applied += 1;
                }
            }
            continue;
        }
        if let Some(Value::String(s)) = incoming.get(*k) {
            cfg.insert(k.to_string(), Value::from(s.trim()));
            if *k == "auto_routes" {
                cfg.remove("routing_rules");
                cfg.remove("auto_price");
                cfg.remove("auto_enabled");
            }
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
    for k in FLOAT_SETTINGS {
        if let Some(v) = incoming.get(*k) {
            let s = util::str_or(Some(v), "");
            if !s.is_empty() {
                let f = util::f64_or(Some(v), -1.0);
                if f >= 0.0 {
                    cfg.insert(k.to_string(), serde_json::json!(util::round6(f)));
                    *applied += 1;
                }
            }
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
    // 模型路由：保存前校验结构，链名遮蔽现有模型/嵌套在这里就拦下
    if let Some(chains_text) = incoming.get("model_chains").and_then(|v| v.as_str()) {
        if let Err(e) = validate_model_chains(&store().load(), chains_text) {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": e}}))).into_response();
        }
    }
    if let Some(raw) = incoming.get("auto_routes").and_then(|v| v.as_str()) {
        let chains = incoming.get("model_chains").and_then(Value::as_str);
        if let Err(e) = validate_auto_routes(&store().load(), raw, chains) {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": e}}))).into_response();
        }
    }
    // 设备登录接入配置：公网入口只收纯 HTTPS 源，可信代理解析为 IP
    if let Some(v) = incoming.get("public_origin").and_then(|x| x.as_str()) {
        let t = v.trim();
        if !t.is_empty() && crate::device::origin_allowed(t).is_none() {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "公网入口必须是纯 HTTPS 源（本地联调可用 http://127.0.0.1:端口），不能带路径、参数或凭据"}}))).into_response();
        }
    }
    if let Some(v) = incoming.get("trusted_proxies").and_then(|x| x.as_str()) {
        // `*` = 信任全部来源（需确保网关只能经反代访问且反代覆盖转发头）
        let bad: Vec<&str> = v
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty() && *x != "*" && x.parse::<std::net::IpAddr>().is_err())
            .collect();
        if !bad.is_empty() {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": format!("可信代理必须是 IP 地址：{}", bad.join(", "))}}))).into_response();
        }
    }
    // 邮箱注册正则：保存前校验可编译，避免线上写出永远匹配失败的规则
    if let Some(re_pat) = incoming.get("reg_email_regex").and_then(|v| v.as_str()) {
        let t = re_pat.trim();
        if !t.is_empty() {
            if let Err(e) = regex::Regex::new(t) {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": {"message": format!("邮箱正则无效: {e}")}})),
                )
                    .into_response();
            }
        }
    }
    let mut validation_error=None;
    store().update(|db| {
        if let Err(e)=validate_sign_settings(db.get("config").unwrap_or(&Value::Null),&incoming) {
            validation_error=Some(e);return;
        }
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
    if let Some(message)=validation_error {
        return (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"message":message}}))).into_response();
    }
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
            cfg.remove("admin_revoked_sessions");
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
    let mut validation_error=None;
    store().update(|db| {
        if let Err(e)=validate_sign_settings(db.get("config").unwrap_or(&Value::Null),&incoming) {
            validation_error=Some(e);return;
        }
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
    if let Some(message)=validation_error {
        return (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"message":message}}))).into_response();
    }
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

// ============================================================ 多用户管理

pub async fn users_list(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let rows = crate::users::list_users();
    let token_counts = |uid: &str| -> usize {
        store()
            .load()
            .get("user_tokens")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter(|t| {
                        util::str_or(t.get("user_id"), "") == uid
                            && t.get("enabled").map(util::truthy).unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            let uid = util::str_or(r.get("id"), "");
            if let Some(o) = r.as_object_mut() {
                o.insert("key_count".into(), json!(token_counts(&uid)));
            }
            r
        })
        .collect();
    json_resp(json!({"rows": rows}))
}

pub async fn users_add(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "请求体格式错误"}})),
        )
            .into_response();
    };
    let (row, err) = crate::users::add_user(
        &util::str_or(body.get("username"), ""),
        &util::str_or(body.get("password"), ""),
        util::f64_or(body.get("balance"), 0.0),
        util::int_or(body.get("free_rpm"), 10).max(0),
        util::int_or(body.get("paid_rpm"), 0).max(0),
    );
    if let Some(e) = (!err.is_empty()).then_some(err) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": e}})),
        )
            .into_response();
    }
    json_resp(json!({"ok": true, "user": row}))
}

pub async fn users_op(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) {
        return e;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "请求体格式错误"}})),
        )
            .into_response();
    };
    let uid = util::str_or(body.get("id"), "");
    let op = util::str_or(body.get("op"), "");
    let (ok, err) = if op == "reset-password" {
        match password_work(move || crate::users::user_op(&uid, &op, &body)).await {
            Ok(result) => result,
            Err(response) => return response,
        }
    } else { crate::users::user_op(&uid, &op, &body) };
    if !ok {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": err}})),
        )
            .into_response();
    }
    store().flush();
    json_resp(json!({"ok": true}))
}

/// 管理端查看某个用户的调用日志。GET /api/user-logs?id=<uid>&kind=all|paid|free&page=1&per=100
pub async fn user_logs_admin(headers: &HeaderMap, raw_query: Option<String>) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let mut id = String::new();
    let mut kind = String::from("all");
    let mut page: i64 = 1;
    let mut per: usize = 100;
    if let Some(q) = raw_query {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "id" => id = v.to_string(),
                    "kind" => kind = v.to_string(),
                    "page" => page = v.parse::<i64>().unwrap_or(1),
                    "per" => per = v.parse::<usize>().unwrap_or(100),
                    _ => {}
                }
            }
        }
    }
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "缺少 id 参数"}})),
        )
            .into_response();
    }
    if crate::users::get_user_by_id(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": {"message": "用户不存在"}})),
        )
            .into_response();
    }
    let kind = match kind.as_str() {
        "paid" | "free" => kind,
        _ => "all".to_string(),
    };
    let (rows, total) = crate::users::user_logs_page(&id, &kind, page, per.clamp(1, 300));
    json_resp(json!({"rows": rows, "total": total, "kind": kind, "per": per.clamp(1, 300)}))
}

// ============================================================ 模型定价

/// 聚合所有渠道的模型定价，供管理端定价面板展示。
pub async fn prices_list(headers: &HeaderMap) -> Response {
    if let Err(e) = require(headers, false) { return e; }
    let db = store().load();
    let mut rows = Vec::new();
    let cfg = db.get("config").unwrap_or(&Value::Null);
    let transparent = crate::upstreams::transparent_upstream(cfg);
    for u in db.get("upstreams").and_then(Value::as_array).into_iter().flatten() {
        let prices = u.get("prices").and_then(Value::as_object).cloned().unwrap_or_default();
        let listed: Vec<String> = u
            .get("models")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let mut models = std::collections::BTreeSet::new();
        for m in &listed { models.insert(m.clone()); }
        // 映射键只有在 models 清单里才对外可调；没进清单的别名不再挂牌
        if let Some(mm) = u.get("model_map").and_then(Value::as_object) {
            for k in mm.keys() {
                if listed.iter().any(|m| m == k) { models.insert(k.clone()); }
            }
        }
        // 透传渠道（空清单）允许为任意已配价模型挂牌
        if transparent && listed.is_empty() {
            models.extend(prices.keys().cloned());
        }
        for model in models {
            let price = prices.get(&model).cloned().unwrap_or(json!(0));
            rows.push(json!({"channel_id":u["id"],"channel_name":u["name"],"model":model,
                "free":!crate::billing::paid(&price),"price":price,"enabled":u["enabled"]}));
        }
    }
    json_resp(json!({"rows":rows}))
}

/// 保存某个渠道的模型定价。body: {channel_id, prices: {model: price}}
pub async fn prices_save(headers: &HeaderMap, body: Bytes) -> Response {
    if let Err(e) = require(headers, true) { return e; }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "请求体格式错误"}}))).into_response();
    };
    let ch_id = util::str_or(body.get("channel_id"), "");
    let Some(new_prices) = body.get("prices").and_then(|p| p.as_object()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "缺少 prices 字段"}}))).into_response();
    };
    if ch_id.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "缺少 channel_id"}}))).into_response();
    }
    if new_prices.len() > 500 { return (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"message":"最多配置 500 个模型"}}))).into_response(); }
    for (model, price) in new_prices {
        if model.is_empty() || model.chars().count() > 160 || model.chars().any(char::is_control) {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"message":"模型名称无效"}}))).into_response();
        }
        if let Err(e) = crate::billing::validate(price) {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"message":format!("{model}: {e}")}}))).into_response();
        }
    }
    let mut found = false;
    let mut err: Option<String> = None;
    store().update(|db| {
        let transparent = crate::upstreams::transparent_upstream(db.get("config").unwrap_or(&Value::Null));
        if let Some(ups) = db.get_mut("upstreams").and_then(Value::as_array_mut) {
            if let Some(u) = ups.iter_mut().find(|u| util::str_or(u.get("id"), "") == ch_id) {
                let listed: Vec<String> = u
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                // 只能给已接入的模型定价：没有模型却有价格，正是后台列表里
                // 幽灵定价的来源。透传渠道（清单为空）保留任意名定价能力。
                if !(transparent && listed.is_empty()) {
                    if let Some(bad) = new_prices
                        .keys()
                        .find(|k| !listed.iter().any(|m| m == *k))
                    {
                        err = Some(format!("模型 {} 未接入该渠道，请先在模型接入中添加", bad));
                        return;
                    }
                }
                u["prices"] = Value::Object(new_prices.clone());
                found = true;
            }
        }
    });
    if let Some(e) = err {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"message":e}}))).into_response();
    }
    if !found { return (StatusCode::NOT_FOUND, axum::Json(json!({"error":{"message":"渠道不存在"}}))).into_response(); }

    store().flush();
    json_resp(json!({"ok": true}))
}

// ---------------------------------------------------------------- 用户资金明细（管理端）

fn parse_id_page(raw: &Option<String>) -> (String, i64, usize) {
    let mut id = String::new();
    let mut page: i64 = 1;
    let mut per: usize = 100;
    if let Some(q) = raw {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "id" => id = v.to_string(),
                    "page" => page = v.parse::<i64>().unwrap_or(1),
                    "per" => per = v.parse::<usize>().unwrap_or(100),
                    _ => {}
                }
            }
        }
    }
    (id, page, per.clamp(1, 300))
}

/// GET /api/user-funds?id=&page=&per= — 用户资金变动流水。
pub async fn user_funds_admin(headers: &HeaderMap, raw_query: Option<String>) -> Response {
    if let Err(e) = require(headers, false) {
        return e;
    }
    let (id, page, per) = parse_id_page(&raw_query);
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "缺少 id 参数"}})),
        )
            .into_response();
    }
    if crate::users::get_user_by_id(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": {"message": "用户不存在"}})),
        )
            .into_response();
    }
    let (rows, total) = crate::users::user_funds_page(&id, page, per);
    json_resp(json!({"rows": rows, "total": total, "per": per}))
}

#[cfg(test)]
#[test]
fn sign_settings_reject_invalid_and_inverted_ranges() {
    let current=json!({"sign_min":0.001,"sign_max":0.01});
    for incoming in [json!({"sign_min":0}),json!({"sign_max":"NaN"}),json!({"sign_min":true}),json!({"sign_min":0.02}),json!({"sign_max":1001}),json!({"sign_min":null})] {
        assert!(validate_sign_settings(&current,&incoming).is_err(),"{incoming}");
    }
    assert!(validate_sign_settings(&current,&json!({"sign_min":0.000001,"sign_max":0.000001})).is_ok());
    assert!(validate_sign_settings(&current,&json!({"sign_enabled":false})).is_ok());
}

#[cfg(test)]
mod chain_validate_tests {
    use super::*;

    fn db_with(models: &str, map: &str) -> Value {
        // 真实存储里 models 是数组、model_map 是对象：按 parse 口径构造
        let arr: Vec<Value> = models
            .split(',')
            .filter(|m| !m.trim().is_empty())
            .map(|m| json!(m.trim()))
            .collect();
        let mut mo = serde_json::Map::new();
        for pair in map.split(',') {
            let pair = pair.trim();
            if pair.is_empty() { continue; }
            if let Some((k, v)) = pair.split_once('=') {
                mo.insert(k.trim().to_string(), json!(v.trim()));
            }
        }
        json!({
            "upstreams": [{"id": "u1", "name": "T", "base": "https://x.test/v1", "enabled": true,
                           "models": arr, "model_map": mo}],
            "config": {}
        })
    }

    #[test]
    fn chain_validation_rejects_shadowing_and_structure() {
        let db = db_with("openmock", "");
        // 遮蔽已接入模型
        assert!(validate_model_chains(&db, "openmock = a, b").is_err());
        // 正常链通过
        assert!(validate_model_chains(&db, "combo = openmock, other").is_ok());
        // 结构问题
        assert!(validate_model_chains(&db, "solo = only-one").is_err(), "成员不足");
        assert!(validate_model_chains(&db, "x = a, b|zz").is_err(), "成员含非法字符");
        assert!(validate_model_chains(&db, "dup = a, b\ndup = c, d").is_err(), "重复");
        assert!(validate_model_chains(&db, "aa = a, b
nested = aa, x").is_err(), "嵌套");
        // 空文本 = 清空路由，合法
        assert!(validate_model_chains(&db, "").is_ok());
    }
}

#[cfg(test)]
mod login_regression_tests {
    use super::*;

    #[test]
    fn limits_are_reserved_before_password_work_and_isolated_by_account() {
        let key = "test-login:shared-ip:account-a";
        let other = "test-login:shared-ip:account-b";
        login_rate_clear(key); login_rate_clear(other);
        for _ in 0..10 { assert!(login_rate_ok(key)); }
        assert!(!login_rate_ok(key));
        assert!(login_rate_ok(other));
        login_rate_clear(other);
        assert!(!login_rate_ok(key));
        login_rate_clear(key);
        assert!(login_rate_ok(key));
        login_rate_clear(key);
    }

    #[test]
    fn query_decoding_preserves_encoded_plus() {
        assert_eq!(query_get(&Some("q=a%2Bb+c%26d".into()), "q"), "a+b c&d");
    }
}
