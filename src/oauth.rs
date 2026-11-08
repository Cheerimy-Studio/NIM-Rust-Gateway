use crate::{device, store::store, user_api, users, util};
use axum::{
    extract::{ConnectInfo, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Mutex, OnceLock},
};
struct Attempt {
    verifier: String,
    expires: i64,
    return_code: String,
}
fn attempts() -> &'static Mutex<HashMap<String, Attempt>> {
    static M: OnceLock<Mutex<HashMap<String, Attempt>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
fn configured() -> bool {
    std::env::var("NGW_GITHUB_CLIENT_ID").is_ok_and(|v| !v.is_empty())
        && std::env::var("NGW_GITHUB_CLIENT_SECRET").is_ok_and(|v| !v.is_empty())
        && device::origin().is_some()
}
pub async fn providers() -> Response {
    axum::Json(json!({"github":configured()})).into_response()
}
pub async fn start(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !device::secure(&h, addr.ip()) || !configured() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !crate::admin::login_rate_ok(&format!("oauth:{}", device::client_ip(&h, addr.ip()))) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let state = util::rand_hex(32);
    let verifier = util::rand_hex(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let code = q
        .get("device")
        .filter(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .cloned()
        .unwrap_or_default();
    {
        let mut m = attempts().lock().unwrap();
        m.retain(|_, a| a.expires > util::now_i());
        if m.len() >= 10000 {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        m.insert(
            state.clone(),
            Attempt {
                verifier,
                expires: util::now_i() + 600,
                return_code: code,
            },
        );
    }
    let mut url = reqwest::Url::parse("https://github.com/login/oauth/authorize").unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", std::env::var("NGW_GITHUB_CLIENT_ID").unwrap()),
        (
            "redirect_uri",
            format!("{}/api/oauth/github/callback", device::origin().unwrap()),
        ),
        ("state", state.clone()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256".into()),
    ]);
    let mut r = Redirect::to(url.as_str()).into_response();
    r.headers_mut().append("set-cookie",format!("ngw_oauth={state}; Path=/api/oauth/github; Max-Age=600; Secure; HttpOnly; SameSite=Lax").parse().unwrap());
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    r
}
fn consume(state: &str, cookie: &str) -> Option<Attempt> {
    if state.is_empty() || !crate::webhttp::constant_time_token(state, cookie) {
        return None;
    }
    attempts()
        .lock()
        .unwrap()
        .remove(state)
        .filter(|a| a.expires > util::now_i())
}
pub async fn callback(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !device::secure(&h, addr.ip()) || !configured() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let state = q.get("state").map(String::as_str).unwrap_or("");
    let cookie = user_api::cookie_get(&h, "ngw_oauth").unwrap_or_default();
    let Some(attempt) = consume(state, &cookie) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(code) = q.get("code").filter(|c| c.len() < 1024) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let result = async {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("NIM-Rust-Gateway")
            .build()
            .map_err(|_| ())?;
        let v: Value = http
            .post("https://github.com/login/oauth/access_token")
            .header("accept", "application/json")
            .form(&[
                ("client_id", std::env::var("NGW_GITHUB_CLIENT_ID").unwrap()),
                (
                    "client_secret",
                    std::env::var("NGW_GITHUB_CLIENT_SECRET").unwrap(),
                ),
                ("code", code.clone()),
                (
                    "redirect_uri",
                    format!("{}/api/oauth/github/callback", device::origin().unwrap()),
                ),
                ("code_verifier", attempt.verifier.clone()),
            ])
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?
            .json()
            .await
            .map_err(|_| ())?;
        let token = v["access_token"].as_str().ok_or(())?;
        let profile: Value = http
            .get("https://api.github.com/user")
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?
            .json()
            .await
            .map_err(|_| ())?;
        let id = profile["id"].as_u64().ok_or(())?.to_string();
        tokio::task::spawn_blocking(move || resolve_user(&id))
            .await
            .map_err(|_| ())?
            .ok_or(())
    }
    .await;
    let Ok(user) = result else {
        return (StatusCode::BAD_REQUEST, "GitHub 登录未完成，请重新尝试").into_response();
    };
    if users::auth_user(user["id"].as_str().unwrap_or("")).is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let db = store().load();
    let secret = util::str_or(db["config"].get("session_secret"), "");
    let session = users::user_session_from_row(&secret, &user);
    let csrf = users::user_csrf_token(&secret, user["id"].as_str().unwrap_or(""));
    let target = if attempt.return_code.is_empty() {
        "/user".into()
    } else {
        format!("/user/device?code={}", attempt.return_code)
    };
    let mut r = Redirect::to(&target).into_response();
    for cookie in [
        format!("ngw_user={session}; Path=/; Secure; HttpOnly; SameSite=Lax"),
        format!("ngw_ucsrfs={csrf}; Path=/; Secure; HttpOnly; SameSite=Lax"),
        "ngw_oauth=; Path=/api/oauth/github; Max-Age=0; Secure; HttpOnly; SameSite=Lax".into(),
    ] {
        r.headers_mut()
            .append("set-cookie", cookie.parse().unwrap());
    }
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    r
}
fn resolve_user(subject: &str) -> Option<Value> {
    // Serialize identity lookup and creation; never link accounts by a matching email or username.
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let db = store().load();
    if let Some(u) = db["users"]
        .as_array()
        .and_then(|a| a.iter().find(|u| u["github_subject"] == subject))
    {
        return Some(u.clone());
    }
    if std::env::var("NGW_GITHUB_ALLOW_SIGNUP").is_ok_and(|v| v == "false") {
        return None;
    }
    let username = format!("gh_{}", util::rand_hex(10));
    let (row, _) = users::add_user(
        &username,
        &util::rand_hex(32),
        0.0,
        util::int_or(db["config"].get("reg_free_rpm"), 10),
        0,
    );
    let mut row = row?;
    row["github_subject"] = json!(subject);
    store().update(|db| {
        if let Some(u) = db["users"]
            .as_array_mut()
            .and_then(|a| a.iter_mut().find(|u| u["id"] == row["id"]))
        {
            u["github_subject"] = json!(subject);
        }
    });
    Some(row)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csrf_and_single_use() {
        let state = util::rand_hex(32);
        attempts().lock().unwrap().insert(
            state.clone(),
            Attempt {
                verifier: "v".into(),
                expires: util::now_i() + 60,
                return_code: String::new(),
            },
        );
        assert!(consume(&state, "wrong").is_none());
        assert!(consume(&state, &state).is_some());
        assert!(consume(&state, &state).is_none());
    }
}
