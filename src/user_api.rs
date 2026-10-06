//! 用户端 API（/api/user/*）：登录、概览、Key 管理、调用日志、模型广场。
//!
//! 会话：ngw_user cookie（HMAC 签名，含用户 id）；POST 双提交校验 ngw_ucsrfs cookie。

use crate::store::{csrf_token, session_cookie, store};pub fn user_session_id_pub(secret: &str, cookie: &str) -> Option<String> {
    crate::users::user_session_id(secret, cookie)
}
pub fn auth_user_pub(uid: &str) -> Option<Value> {
    crate::users::auth_user(uid)
}
use crate::users;
use crate::util;
use crate::webhttp::*;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

pub fn cookie_get(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut out = None;
    if let Some(cookie) = headers.get("cookie").and_then(|c| c.to_str().ok()) {
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(eq) = part.find('=') {
                if &part[..eq] == name {
                    out = Some(part[eq + 1..].to_string());
                }
            }
        }
    }
    out
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

fn user_require_err() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({"error": {"message": "未登录或会话已过期", "type": "auth"}})),
    )
        .into_response()
}

fn constant_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// 用户会话校验：返回 user_id。POST 额外校验 ngw_ucsrfs cookie（服务端双提交）。
pub fn user_require(headers: &HeaderMap, is_post: bool) -> Result<String, Response> {
    let cfg = store().load();
    let secret = util::str_or(cfg.get("config").and_then(|c| c.get("session_secret")), "");
    let got = cookie_get(headers, "ngw_user").unwrap_or_default();
    let Some(uid) = users::user_session_id(&secret, &got) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "未登录或会话已过期", "type": "auth"}})),
        )
            .into_response());
    };
    if !users::auth_user(&uid).is_some() {
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "用户已被停用", "type": "auth"}})),
        )
            .into_response());
    }
    if is_post {
        let expect = users::user_csrf_token(&secret, &uid);
        let got_csrf = cookie_get(headers, "ngw_ucsrfs").unwrap_or_default();
        if !constant_eq(&got_csrf, &expect) {
            return Err((
                StatusCode::FORBIDDEN,
                axum::Json(json!({"error": {"message": "CSRF 校验失败，请刷新页面", "type": "auth"}})),
            )
                .into_response());
        }
    }
    Ok(uid)
}

pub async fn login(headers: &HeaderMap, ip: String, body: Bytes) -> Response {
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|x| x.to_str().ok())
        .map(|p| p.split(',').next().unwrap_or("").trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let body_v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cfg = store().load();
    let cfg = cfg.get("config").cloned().unwrap_or(json!({}));
    let username = util::str_or(body_v.get("username"), "").trim().to_string();
    let password = util::str_or(body_v.get("password"), "");
    let users = store().load();
    let found = users
        .get("users")
        .and_then(|u| u.as_array())
        .and_then(|a| {
            a.iter().find(|u| {
                util::str_or(u.get("username"), "").eq_ignore_ascii_case(&username)
                    && u.get("enabled").map(util::truthy).unwrap_or(false)
            })
        })
        .cloned();
    // 时序均衡：用户不存在时也对固定假哈希跑一遍 pbkdf2，
    // 否则「用户不存在」立即返回可被用来枚举用户名（与管理端同口径）
    let ok_pass = match &found {
        Some(u) => crate::store::verify_password(&password, &util::str_or(u.get("password_hash"), "")),
        None => {
            static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            let _ = crate::store::verify_password(&password, DUMMY.get_or_init(|| crate::store::hash_password("ngw-dummy")));
            false
        }
    };
    if found.is_none() || !ok_pass {
        // 先验证、失败才计入限流预算（与管理端一致）：反代/公网部署下所有流量共享
        // 同一来源 IP，验证前计数会把正常用户也挡在门外
        if !crate::admin::login_rate_ok(&ip) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({"error": {"message": "尝试过于频繁，请 5 分钟后重试", "type": "auth"}})),
            )
                .into_response();
        }
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "账号或密码错误", "type": "auth"}})),
        )
            .into_response();
    }
    let u = found.unwrap();
    crate::admin::login_rate_clear(&ip);
    let uid = util::str_or(u.get("id"), "");
    let secret = util::str_or(cfg.get("session_secret"), "");
    let session = users::user_session_value(&secret, &uid);
    let csrf = users::user_csrf_token(&secret, &uid);
    let mut resp = axum::Json(json!({"ok": true, "csrf": csrf, "username": util::str_or(u.get("username"), "")}))
        .into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header("ngw_user", &session, true, false, secure)).unwrap(),
    );
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header("ngw_ucsrfs", &csrf, true, false, secure)).unwrap(),
    );
    resp
}

pub async fn logout(headers: &HeaderMap) -> Response {
    // 删除 Cookie 与种入 Cookie 的 Secure 标记保持一致（https 部署下删除也应带 Secure）
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|x| x.to_str().ok())
        .map(|p| p.split(',').next().unwrap_or("").trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let mut resp = axum::Json(json!({"ok": true})).into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header("ngw_user", "", true, true, secure)).unwrap(),
    );
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&set_cookie_header("ngw_ucsrfs", "", true, true, secure)).unwrap(),
    );
    resp
}

/// 登录后设置 CSRF cookie（用户面板刷新页面时 JS 无 Cookie 可读，走服务端种入）。
pub async fn refresh_csrf(headers: &HeaderMap) -> Response {
    match user_require(headers, false) {
        Ok(uid) => {
            let cfg = store().load();
            let secret = util::str_or(cfg.get("config").and_then(|c| c.get("session_secret")), "");
            let csrf = users::user_csrf_token(&secret, &uid);
            json_resp(json!({"ok": true, "csrf": csrf}))
        }
        Err(e) => e,
    }
}

fn me_row(u: &Value) -> Value {
    json!({
        "id": util::str_or(u.get("id"), ""),
        "username": util::str_or(u.get("username"), ""),
        "balance": users::balance_of(u),
        "grant": util::round6(users::grant_of(u)),
        "grant_total": util::round6(util::f64_or(u.get("grant_total"), 0.0)),
        "recharge": util::round6(users::recharge_of(u)),
        "recharge_total": util::round6(util::f64_or(u.get("recharge_total"), 0.0)),
        "free_rpm": util::int_or(u.get("free_rpm"), 0),
        "paid_rpm": util::int_or(u.get("paid_rpm"), 0),
        "created_at": util::int_or(u.get("created_at"), 0),
    })
}

/// 签到状态：今日是否已签 + 当前设置（开关与金额区间，供前端渲染）。
pub async fn sign_status(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    let cfg_v = crate::store::store().load();
    let cfg = cfg_v.get("config").cloned().unwrap_or(json!({}));
    let (enabled, min, max) = users::sign_config(&cfg);
    let day = util::local_day(util::now_i());
    let signed_today = users::sign_store_get(&day, &uid).is_some();
    let today_amount = users::sign_store_get(&day, &uid).unwrap_or(0.0);
    json_resp(json!({
        "enabled": enabled,
        "min": min,
        "max": max,
        "signed_today": signed_today,
        "today_amount": util::round6(today_amount),
    }))
}

/// 用户签到（POST）：一天一次，金额随机入赠金。
pub async fn sign(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, true) else { return user_require_err() };
    let (ok, msg, amount) = users::sign_today(&uid);
    if !ok {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": msg}})),
        )
            .into_response();
    }
    // 返回最新钱包
    let Some(u) = users::auth_user(&uid) else {
        return user_require_err();
    };
    json_resp(json!({"ok": true, "message": msg, "amount": util::round6(amount), "wallet": users::wallet_row(&u)}))
}

/// 赠金/充值账本：当前与累计。
pub async fn wallet(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    let Some(u) = users::auth_user(&uid) else { return user_require_err() };
    json_resp(json!({"wallet": users::wallet_row(&u)}))
}

/// 用户端转盘列表（不含真实权重）。
pub async fn wheels(headers: &HeaderMap) -> Response {
    if let Err(e) = user_require(headers, false) { return e; }
    json_resp(json!({"rows": crate::wheel::user_wheels()}))
}

/// 抽奖（POST /api/user/wheels/draw {id}）。
pub async fn wheels_draw(headers: &HeaderMap, body: Bytes) -> Response {
    let Ok(uid) = user_require(headers, true) else { return user_require_err() };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "请求体格式错误"}})),
        )
            .into_response();
    };
    let id = util::str_or(body.get("id"), "");
    let (ok, body) = crate::wheel::draw(&uid, &id);
    if ok {
        json_resp(body)
    } else {
        let status = if body.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())
            .map(|m| m.contains("余额不足"))
            .unwrap_or(false)
        {
            StatusCode::PAYMENT_REQUIRED
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, axum::Json(body)).into_response()
    }
}

/// 我的奖品 Key（体验卡/专属额度）。
pub async fn prize_keys(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    json_resp(json!({"rows": crate::wheel::user_prize_keys(&uid)}))
}

/// 我的抽奖记录。
pub async fn draw_logs(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    json_resp(json!({"rows": crate::wheel::user_draw_logs(&uid)}))
}

pub async fn me(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "未登录或会话已过期", "type": "auth"}})),
        )
            .into_response();
    };
    let Some(u) = users::auth_user(&uid) else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "用户已被停用", "type": "auth"}})),
        )
            .into_response();
    };
    json_resp(me_row(&u))
}

pub async fn keys(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    json_resp(json!({"rows": users::list_keys(&uid)}))
}

pub async fn keys_add(headers: &HeaderMap, body: Bytes) -> Response {
    let Ok(uid) = user_require(headers, true) else { return user_require_err() };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "请求体格式错误"}})),
        )
            .into_response();
    };
    let name = util::str_or(body.get("name"), "");
    let kind = util::str_or(body.get("kind"), "all");
    let row = users::add_key_kind(&uid, &name, &kind);
    if row.get("error").is_some() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": util::str_or(row.get("error"), "创建失败")}})),
        )
            .into_response();
    }
    json_resp(json!({"ok": true, "key": row}))
}

pub async fn keys_op(headers: &HeaderMap, body: Bytes) -> Response {
    let Ok(uid) = user_require(headers, true) else { return user_require_err() };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "请求体格式错误"}})),
        )
            .into_response();
    };
    let op = util::str_or(body.get("op"), "");
    let key_id = util::str_or(body.get("id"), "");
    if !["enable", "disable", "delete"].contains(&op.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "未知操作"}})),
        )
            .into_response();
    }
    let ok = users::key_op(&uid, &key_id, &op);
    json_resp(json!({"ok": ok}))
}

pub async fn logs(headers: &HeaderMap, raw_query: Option<String>) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    let mut kind = String::from("all");
    let mut page: i64 = 1;
    let mut per: usize = 50;
    if let Some(q) = raw_query {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "kind" => kind = v.to_string(),
                    "page" => page = v.parse::<i64>().unwrap_or(1),
                    "per" => per = v.parse::<usize>().unwrap_or(50),
                    _ => {}
                }
            }
        }
    }
    let kind = match kind.as_str() {
        "paid" | "free" => kind,
        _ => "all".to_string(),
    };
    let (rows, total) = users::user_logs_page(&uid, &kind, page, per.clamp(1, 300));
    json_resp(json!({"rows": rows, "total": total, "kind": kind, "per": per.clamp(1, 300)}))
}

/// 模型广场：对外可用模型 + 免费/价格。
pub async fn models(headers: &HeaderMap) -> Response {
    if let Err(e) = user_require(headers, false) {
        return e;
    }
    json_resp(json!({"rows": users::model_square()}))
}

/// 用户面板对话测试：签发/复用本人 Key，前端持 Key 直连 /v1，
/// 计费、限速、调用日志与真实调用完全一致。
pub async fn test_key(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, true) else {
        return user_require_err();
    };
    match users::ensure_test_key(&uid) {
        Some(k) => json_resp(json!({"key": k})),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": {"message": "无法取得测试 Key"}})),
        )
            .into_response(),
    }
}

/// 用户面板与脚本（登录页由 /user 按会话状态切换）。
pub async fn refresh_login(headers: &HeaderMap) -> Response {
    let cfg = store().load();
    let secret = util::str_or(cfg.get("config").and_then(|c| c.get("session_secret")), "");
    let session = cookie_get(headers, "ngw_user").unwrap_or_default();
    match users::user_session_id(&secret, &session) {
        Some(uid) => {
            if users::auth_user(&uid).is_some() {
                let csrf = users::user_csrf_token(&secret, &uid);
                json_resp(json!({"ok": true, "csrf": csrf, "username":
                    users::get_user_by_id(&uid).map(|u| util::str_or(u.get("username"), "")).unwrap_or_default()}))
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"error": {"message": "用户已被停用", "type": "auth"}})),
                )
                    .into_response()
            }
        }
        None => (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "未登录", "type": "auth"}})),
        )
            .into_response(),
    }
}


/// 用户面板概览统计：总调用次数、总消费、Key 数、最近 5 条日志。
pub async fn stats(headers: &HeaderMap) -> Response {
    let Ok(uid) = user_require(headers, false) else { return user_require_err() };
    let db = store().load();
    // 累计口径读用户行上的持久计数器（bill 时累加）：日志有保留上限会被裁剪，
    // 对日志求和只会越用越少，不是真累计
    let Some(u) = db
        .get("users")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().find(|u| util::str_or(u.get("id"), "") == uid))
    else {
        return user_require_err();
    };
    let total_calls = util::int_or(u.get("paid_calls"), 0) + util::int_or(u.get("free_calls"), 0);
    let total_cost = util::f64_or(u.get("total_cost"), 0.0);
    let key_count = db.get("user_tokens").and_then(|t| t.as_array())
        .map(|a| a.iter().filter(|t| util::str_or(t.get("user_id"), "") == uid
            && t.get("enabled").map(util::truthy).unwrap_or(false)).count())
        .unwrap_or(0);
    // 最近调用仍来自日志（只展示，不承担累计口径）
    let mut my_logs: Vec<Value> = Vec::new();
    for key in ["user_logs_paid", "user_logs_free"] {
        if let Some(a) = db.get(key).and_then(|l| l.as_array()) {
            my_logs.extend(a.iter().filter(|r| util::str_or(r.get("user_id"), "") == uid).cloned());
        }
    }
    my_logs.sort_by_key(|r| -util::int_or(r.get("t"), 0));
    let recent: Vec<Value> = my_logs.iter().take(5).map(|r| json!({
        "t": util::int_or(r.get("t"), 0),
        "model": util::str_or(r.get("model"), ""),
        "st": util::int_or(r.get("st"), 0),
        "cost": util::f64_or(r.get("cost"), 0.0),
    })).collect();
    json_resp(json!({
        "total_calls": total_calls,
        "free_calls": util::int_or(u.get("free_calls"), 0),
        "paid_calls": util::int_or(u.get("paid_calls"), 0),
        "total_cost": util::round6(total_cost),
        "key_count": key_count,
        "recent": recent,
    }))
}
