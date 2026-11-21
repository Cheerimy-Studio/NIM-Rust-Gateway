use crate::{store::store, user_api, users, util};
use axum::{
    body::Bytes,
    extract::{ConnectInfo, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Mutex, OnceLock},
};

#[derive(Clone)]
struct Grant {
    code: String,
    ip: String,
    client: String,
    created: i64,
    exp: i64,
    last: i64,
    interval: i64,
    state: String,
    uid: String,
    key_id: String,
}
#[derive(Default)]
struct State {
    grants: HashMap<String, Grant>,
    // 不按 IP 限额/绑定（网络切换、NAT、代理都会误伤），只保留全局窗口
    // 防止批量刷验证码；授权码本身仍是一次性、10 分钟过期的 256 位随机串。
    global_codes: HashMap<String, Vec<i64>>,
    global_failures: HashMap<String, Vec<i64>>,
}
fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn err(e: &str) -> Response {
    let mut r = reply(json!({"error":e}));
    *r.status_mut() = StatusCode::BAD_REQUEST;
    r
}
fn reply(v: Value) -> Response {
    let mut r = axum::Json(v).into_response();
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    r.headers_mut()
        .insert("pragma", "no-cache".parse().unwrap());
    r
}
fn cfg_str(key: &str) -> String {
    store()
        .load()
        .get("config")
        .and_then(|c| c.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 后台「01Code 设备登录」可关闭整条授权链路（默认开启）。
fn enabled() -> bool {
    store()
        .load()
        .get("config")
        .map(|c| c.get("device_login_enabled").map(util::truthy).unwrap_or(true))
        .unwrap_or(true)
}

/// 校验并规范化公网入口：生产必须是纯 HTTPS 源；本地联调允许
/// http://127.0.0.1|localhost|::1（仅回环，供 01Code 桌面端直连测试）。
pub fn origin_allowed(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let u = reqwest::Url::parse(raw).ok()?;
    if u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.path() != "/"
    {
        return None;
    }
    let host = u.host_str().unwrap_or("");
    let loopback = host == "localhost" || host == "::1" || host.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false);
    match u.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return None,
    }
    Some(raw.trim_end_matches('/').to_string())
}

// 公网入口优先取后台配置，留空回退环境变量。绝不从 Host 拼授权链接。
pub fn origin() -> Option<String> {
    let mut s = cfg_str("public_origin");
    if s.is_empty() {
        s = std::env::var("NGW_PUBLIC_ORIGIN").ok()?;
    }
    origin_allowed(&s)
}
fn trusted(peer: std::net::IpAddr) -> bool {
    let mut configured = cfg_str("trusted_proxies");
    if configured.is_empty() {
        configured = std::env::var("NGW_TRUSTED_PROXIES").unwrap_or_default();
    }
    let items: Vec<&str> = configured.split(',').map(str::trim).collect();
    // `*` = 信任所有来源。只有网关确实只能经反代访问、且反代覆盖
    // X-Forwarded-Proto / X-Real-IP 时才安全；否则客户端可伪造协议与来源 IP
    if items.iter().any(|v| *v == "*") {
        return true;
    }
    items
        .iter()
        .filter_map(|v| v.parse::<std::net::IpAddr>().ok())
        .any(|p| p == peer)
}
pub fn client_ip(h: &HeaderMap, peer: std::net::IpAddr) -> String {
    if trusted(peer) {
        if let Some(ip) = h
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<std::net::IpAddr>().ok())
        {
            return ip.to_string();
        }
    }
    peer.to_string()
}
pub fn secure(h: &HeaderMap, peer: std::net::IpAddr) -> bool {
    if origin().is_none() {
        return false;
    }
    // 本机回环直连（01Code 桌面端本地联调、浏览器访问 127.0.0.1）：
    // 不经过反代，也不存在伪造转发头的问题
    if peer.is_loopback() {
        return true;
    }
    trusted(peer) && h.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
}

fn limited(
    map: &mut HashMap<String, Vec<i64>>,
    ip: &str,
    now: i64,
    cap: usize,
    window: i64,
) -> bool {
    map.retain(|_, v| {
        v.retain(|t| *t > now - window);
        !v.is_empty()
    });
    let v = map.entry(ip.into()).or_default();
    if v.len() >= cap {
        return true;
    }
    v.push(now);
    false
}
fn normalize(s: &str) -> String {
    s.trim().to_ascii_uppercase().replace('-', "")
}

pub async fn code(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    body: Bytes,
) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if v["client_id"] != "01code" {
        return err("invalid_client");
    }
    if v["scope"] != "llm" {
        return err("invalid_scope");
    }
    let now = util::now_i();
    let ip = client_ip(&h, addr.ip());
    let mut s = state().lock().unwrap();
    s.grants.retain(|_, g| g.exp > now - 600);
    if limited(&mut s.global_codes, "all", now, 300, 60) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"error":"slow_down"})),
        )
            .into_response();
    }
    if s.grants.len() >= 10000 {
        return err("temporarily_unavailable");
    }
    let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let user_code = loop {
        let c: String = (0..8)
            .map(|_| alphabet[rand::thread_rng().gen_range(0..alphabet.len())] as char)
            .collect();
        if !s.grants.values().any(|g| g.code == c) {
            break c;
        }
    };
    let dc = util::rand_hex(32);
    s.grants.insert(
        digest(&dc),
        Grant {
            code: user_code.clone(),
            ip,
            client: "01code".into(),
            created: now,
            exp: now + 600,
            last: 0,
            interval: 5,
            state: "authorization_pending".into(),
            uid: String::new(),
            key_id: String::new(),
        },
    );
    reply(
        json!({"device_code":dc,"user_code":user_code,"verification_uri":format!("{}/user/device?code={}",origin().unwrap(),user_code),"expires_in":600,"interval":5}),
    )
}
fn poll(s: &mut State, dc: &str, client: &str, now: i64) -> Result<String, &'static str> {
    let g = s.grants.get_mut(&digest(dc)).ok_or("expired_token")?;
    // 只绑定 client_id，不绑定 IP：客户端换网络/走代理后仍可完成授权
    if g.client != client {
        return Err("invalid_grant");
    }
    if g.exp <= now {
        return Err("expired_token");
    }
    if g.state == "access_denied" {
        return Err("access_denied");
    }
    if g.state == "consumed" {
        return Err("expired_token");
    }
    // 留 1 秒容差：客户端/网络抖动导致的提前几百毫秒不应直接判 slow_down
    if g.last != 0 && now - g.last < g.interval.saturating_sub(1) {
        g.last = now;
        g.interval = (g.interval + 5).min(120);
        return Err("slow_down");
    }
    g.last = now;
    if g.state != "approved" {
        return Err("authorization_pending");
    }
    Ok(g.uid.clone())
}
pub async fn token(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    body: Bytes,
) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if v["grant_type"] != "urn:ietf:params:oauth:grant-type:device_code" {
        return err("unsupported_grant_type");
    }
    let dc = v["device_code"].as_str().unwrap_or("");
    let client = v["client_id"].as_str().unwrap_or("");
    let mut s = state().lock().unwrap();
    let uid = match poll(&mut s, dc, client, util::now_i()) {
        Ok(u) => u,
        Err(e) => return err(e),
    };
    if users::auth_user(&uid).is_none() {
        return err("access_denied");
    }
    let g = s.grants.get_mut(&digest(dc)).unwrap();
    // 浏览器确认时已经创建 Key（用户中心立即可见授权历史）；这里复用同一把。
    let (row, created) = if !g.key_id.is_empty() {
        match users::find_key(&uid, &g.key_id) {
            Some(k) => (k, false),
            None => return err("access_denied"),
        }
    } else {
        let row = users::add_key(&uid, "01Code-设备");
        (row, true)
    };
    if row.get("error").is_some()
        || !row.get("enabled").map(util::truthy).unwrap_or(false)
    {
        return err("access_denied");
    }
    if created {
        store().update(|db| {
            if let Some(t) = db["user_tokens"]
                .as_array_mut()
                .and_then(|a| a.iter_mut().find(|t| t["id"] == row["id"]))
            {
                t["device_client"] = json!(client);
                t["device_ip"] = json!(g.ip);
            }
        });
    }
    g.state = "consumed".into();
    reply(json!({"access_token":row["key"],"token_type":"Bearer","expires_in":0,"scope":"llm"}))
}
pub async fn devices(ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let Some((u, t)) = bearer(&h) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let uid = util::str_or(u.get("id"), "");
    let rows: Vec<Value> = users::list_keys(&uid)
        .into_iter()
        .filter(|k| k["device_client"] == "01code")
        .map(|k| {
            json!({
                "id": k["id"], "name": k["name"], "device_ip": k["device_ip"],
                "created_at": k["created_at"], "last_used_at": k["last_used_at"],
                "enabled": k["enabled"], "current": k["id"] == t["id"],
            })
        })
        .collect();
    reply(json!({"rows": rows}))
}

fn bearer(h: &HeaderMap) -> Option<(Value, Value)> {
    let a = h.get("authorization")?.to_str().ok()?;
    if !a.get(..7)?.eq_ignore_ascii_case("bearer ") {
        return None;
    }
    users::lookup_user_key(a[7..].trim())
}
pub async fn revoke(ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let Some((u, t)) = bearer(&h) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if t["device_client"] != "01code" {
        return StatusCode::FORBIDDEN.into_response();
    }
    users::key_op(
        u["id"].as_str().unwrap_or(""),
        t["id"].as_str().unwrap_or(""),
        "delete",
    );
    StatusCode::NO_CONTENT.into_response()
}
pub async fn me(ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let Some((u, t)) = bearer(&h) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let k = t["key"].as_str().unwrap_or("");
    reply(
        json!({"username":u["username"],"key_prefix":util::char_prefix(k,14),"balance":users::balance_of(&u),"free_rpm":u["free_rpm"],"paid_rpm":u["paid_rpm"]}),
    )
}
pub async fn usage(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let Some((u, _)) = bearer(&h) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let days = q
        .get("days")
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(7)
        .clamp(1, 90);
    let db = store().load();
    let today = chrono::Utc::now().date_naive();
    let mut rows = Vec::new();
    for i in (0..days).rev() {
        let day = (today - chrono::Duration::days(i)).to_string();
        let r = db["device_usage"][u["id"].as_str().unwrap_or("")][&day].clone();
        rows.push(json!({"day":day,"requests":r["requests"].as_i64().unwrap_or(0),"input_tokens":r["input_tokens"].as_i64().unwrap_or(0),"output_tokens":r["output_tokens"].as_i64().unwrap_or(0),"cost":r["cost"].as_f64().unwrap_or(0.0)}));
    }
    reply(json!(rows))
}
pub async fn decide(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    body: Bytes,
) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let uid = match user_api::user_require(&h, true) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let code = normalize(v["code"].as_str().unwrap_or(""));
    if !matches!(v["decision"].as_str(), Some("allow" | "deny")) {
        return err("invalid_request");
    }
    let now = util::now_i();
    let mut s = state().lock().unwrap();
    if limited(&mut s.global_failures, "all", now, 300, 600) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"error":"access_denied"})),
        )
            .into_response();
    }
    let Some(g) = s
        .grants
        .values_mut()
        .find(|g| g.code == code && g.exp > now && g.state == "authorization_pending")
    else {
        return err("expired_token");
    };
    let allow = v["decision"] == "allow";
    g.uid = uid.clone();
    if allow {
        // 浏览器确认即生成设备 Key：用户中心马上能看到授权历史，客户端随后轮询领取
        let row = users::add_key(&uid, "01Code-设备");
        if row.get("error").is_some() {
            return err("access_denied");
        }
        let key_id = util::str_or(row.get("id"), "");
        let ip = g.ip.clone();
        store().update(|db| {
            if let Some(t) = db["user_tokens"]
                .as_array_mut()
                .and_then(|a| a.iter_mut().find(|t| t["id"] == row["id"]))
            {
                t["device_client"] = json!("01code");
                t["device_ip"] = json!(ip);
            }
        });
        g.key_id = key_id;
        g.state = "approved".into();
    } else {
        g.state = "access_denied".into();
    }
    reply(json!({"ok":true}))
}
pub async fn page(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !secure(&h, addr.ip()) {
        return err("invalid_request");
    }
    let code = normalize(q.get("code").map(String::as_str).unwrap_or(""));
    if user_api::user_require(&h, false).is_err() {
        return axum::response::Redirect::to(&format!(
            "/user?device={}",
            percent_encoding::utf8_percent_encode(&code, percent_encoding::NON_ALPHANUMERIC)
        ))
        .into_response();
    }
    let mut s = state().lock().unwrap();
    if limited(&mut s.global_failures, "all", util::now_i(), 300, 600) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let Some(g) = s
        .grants
        .values()
        .find(|g| g.code == code && g.exp > util::now_i() && g.state == "authorization_pending")
    else {
        return err("expired_token");
    };
    let mut page = include_str!("../web/device.html").to_string();
    let data = json!({"code":code,"ip":g.ip,"created":g.created,"client":"01Code"})
        .to_string()
        .replace('<', r"\u003c");
    page = page.replace("DEVICE_DATA", &data);
    let mut r = axum::response::Html(page).into_response();
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    r.headers_mut()
        .insert("x-frame-options", "DENY".parse().unwrap());
    r.headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    // 纵深防御：即使模板/转义将来出问题，内联脚本也无法外联或把
    // 验证码/会话发往第三方；禁止被嵌入与表单提交
    r.headers_mut().insert(
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    r.headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    r
}
#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> State {
        let mut s = State::default();
        s.grants.insert(
            digest("secret"),
            Grant {
                code: "ABCDEFGH".into(),
                ip: "ip".into(),
                client: "01code".into(),
                created: 100,
                exp: 700,
                last: 0,
                interval: 5,
                state: "authorization_pending".into(),
                uid: "u".into(),
                key_id: String::new(),
            },
        );
        s
    }
    #[test]
    fn trusted_proxy_wildcard_and_ips() {
        let _lock = crate::store::test_lock();
        let saved = store().load();
        let peer: std::net::IpAddr = "203.0.113.5".parse().unwrap();
        store().update(|db| { db["config"]["trusted_proxies"] = json!("127.0.0.1, 198.51.100.7"); });
        assert!(!trusted(peer), "未列出的地址不信任");
        store().update(|db| { db["config"]["trusted_proxies"] = json!("*"); });
        assert!(trusted(peer), "* 允许全部来源");
        store().update(|db| { db["config"]["trusted_proxies"] = json!(" * , 198.51.100.7"); });
        assert!(trusted(peer), "* 与空格混排也可");
        store().update(|db| *db = saved);
    }

    #[test]
    fn polling_state_machine() {
        let mut s = setup();
        assert_eq!(
            poll(&mut s, "secret", "01code", 100),
            Err("authorization_pending")
        );
        assert_eq!(
            poll(&mut s, "secret", "01code", 101),
            Err("slow_down")
        );
        assert_eq!(s.grants[&digest("secret")].interval, 10);
        assert_eq!(
            poll(&mut s, "secret", "other", 120),
            Err("invalid_grant")
        );
        // 换 IP 不再影响轮询（Grant.ip 仅用于审计展示）
        s.grants.get_mut(&digest("secret")).unwrap().ip = "other".into();
        s.grants.get_mut(&digest("secret")).unwrap().state = "approved".into();
        assert_eq!(poll(&mut s, "secret", "01code", 111), Ok("u".into()));
        s.grants.get_mut(&digest("secret")).unwrap().state = "consumed".into();
        assert_eq!(
            poll(&mut s, "secret", "01code", 120),
            Err("expired_token")
        );
    }
    #[test]
    fn poll_tolerates_jitter_and_ignores_ip_changes() {
        let mut s = setup();
        assert_eq!(poll(&mut s, "secret", "01code", 100), Err("authorization_pending"));
        assert_eq!(poll(&mut s, "secret", "01code", 103), Err("slow_down"));
        // slow_down 后 interval=10；第 9 秒落在 1 秒容差内，仍按 pending 处理
        assert_eq!(poll(&mut s, "secret", "01code", 112), Err("authorization_pending"));
        let g = s.grants.get_mut(&digest("secret")).unwrap();
        g.ip = "changed-network".into();
        g.state = "approved".into();
        assert_eq!(poll(&mut s, "secret", "01code", 122), Ok("u".into()));
    }

    #[test]
    fn denied_expired_and_rate() {
        let mut s = setup();
        s.grants.get_mut(&digest("secret")).unwrap().state = "access_denied".into();
        assert_eq!(
            poll(&mut s, "secret", "01code", 110),
            Err("access_denied")
        );
        assert_eq!(
            poll(&mut s, "secret", "01code", 700),
            Err("expired_token")
        );
        for _ in 0..5 {
            assert!(!limited(&mut s.global_codes, "all", 100, 5, 60));
        }
        assert!(limited(&mut s.global_codes, "all", 101, 5, 60));
        assert!(!limited(&mut s.global_codes, "all", 160, 5, 60));
    }
}
