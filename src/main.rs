//! 入口：路由、静态页、看门狗、定时落盘、自重启。

mod admin;
mod convert;
mod pool;
mod proxy;
mod queue;
mod store;
mod streams;
mod upstreams;
mod update;
mod util;
mod webhttp;

use crate::store::{csrf_token, session_cookie, store};
use crate::webhttp::*;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, options, post};
use axum::Router;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};

fn page(name: &str, csrf: &str) -> String {
    let raw = match name {
        "admin.html" => web_assets::ADMIN_HTML,
        "login.html" => web_assets::LOGIN_HTML,
        "landing.html" => web_assets::LANDING_HTML,
        "queue.html" => web_assets::QUEUE_HTML,
        _ => "",
    };
    raw.replace("{{CSRF}}", csrf)
        .replace("{{VERSION}}", VERSION)
        .replace("CSRF_FROM_COOKIE", csrf)
}

fn html_resp(name: &str, csrf: &str, no_cache: bool) -> Response {
    let mut resp = page(name, csrf).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp.headers_mut().insert(
        axum::http::header::X_FRAME_OPTIONS,
        axum::http::HeaderValue::from_static("DENY"),
    );
    resp.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    if no_cache {
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    resp
}

fn ip_of(headers: &HeaderMap, addr: Option<SocketAddr>) -> String {
    proxy::Ctx::new(headers.clone(), addr).ip
}

// ---------------------------------------------------------------- /v1 处理器

async fn h_options(headers: HeaderMap) -> Response {
    let mut h = cors_headers();
    // 预检对 Allow-Headers 做精确匹配：直接回显浏览器申请的头集合
    if let Some(req_h) = headers.get("access-control-request-headers").and_then(|x| x.to_str().ok()) {
        if !req_h.is_empty() {
            h.retain(|(k, _)| k != "Access-Control-Allow-Headers");
            h.push(("Access-Control-Allow-Headers".into(), req_h.to_string()));
        }
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    for (k, v) in h {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::try_from(k.as_str()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            resp.headers_mut().insert(name, val);
        }
    }
    resp
}

async fn h_v1_models(headers: HeaderMap) -> Response {
    proxy::v1_models(proxy::Ctx::new(headers, None)).await
}

async fn h_v1_model_retrieve(path: axum::extract::Path<String>, headers: HeaderMap) -> Response {
    proxy::v1_model_retrieve(proxy::Ctx::new(headers, None), path.0).await
}

fn v1_404(method: &str, rest: &str) -> Response {
    error_resp(404, &format!("Invalid URL ({} /v1/{})", method, rest), "invalid_request_error", None, false)
}

async fn h_v1_get(req: Request) -> Response {
    let rest = req.uri().path().trim_start_matches("/v1/").trim_end_matches('/').to_string();
    v1_404("GET", &rest)
}

async fn h_v1_post(req: Request) -> Response {
    let rest = req.uri().path().trim_start_matches("/v1/").trim_end_matches('/').to_string();
    v1_404("POST", &rest)
}

async fn h_chat(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let ctx = proxy::Ctx::new(headers, Some(addr));
    proxy::proxy_chat(ctx, "chat/completions", "chat", body).await
}

async fn h_completions(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let ctx = proxy::Ctx::new(headers, Some(addr));
    proxy::proxy_chat(ctx, "completions", "cmpl", body).await
}

async fn h_embeddings(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let ctx = proxy::Ctx::new(headers, Some(addr));
    proxy::proxy_chat(ctx, "embeddings", "emb", body).await
}

async fn h_responses(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let ctx = proxy::Ctx::new(headers, Some(addr));
    proxy::proxy_convert(ctx, "responses", false, body).await
}

async fn h_messages(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let ctx = proxy::Ctx::new(headers, Some(addr));
    proxy::proxy_convert(ctx, "messages", true, body).await
}

// ---------------------------------------------------------------- 页面与公开接口

async fn h_admin(headers: HeaderMap) -> Response {
    let cfg = store().load();
    let cfg = cfg.get("config").cloned().unwrap_or(json!({}));
    let expected = session_cookie(&cfg);
    let session = admin::cookie_get(&headers, "ngw_session").unwrap_or_default();
    if session.is_empty() || !constant_eq(&session, &expected) {
        return html_resp("login.html", "", true);
    }
    let csrf = csrf_token(&cfg);
    html_resp("admin.html", &csrf, true)
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

async fn h_root() -> Response {
    html_resp("landing.html", "", true)
}

async fn h_queue_page() -> Response {
    let mut resp = web_assets::QUEUE_HTML.to_string().into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

async fn h_presets() -> Response {
    let presets = store().load().get("channel_presets").cloned().unwrap_or(json!({}));
    json_resp(json!({"presets": presets}))
}

async fn h_queue_public() -> Response {
    json_resp(queue::stats(true))
}

async fn h_admin_js() -> Response {
    let mut resp = web_assets::ADMIN_JS.to_string().into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/javascript"),
    );
    resp.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
    );
    resp
}

// ---------------------------------------------------------------- /api 处理器

async fn h_login(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    // 限流按 socket 来源 IP：X-Forwarded-For 可被客户端伪造，用来做限流键等于没有限流
    admin::login(&headers, addr.ip().to_string(), body).await
}

async fn h_logout() -> Response {
    admin::logout().await
}

async fn h_overview(headers: HeaderMap) -> Response {
    admin::overview(&headers).await
}

async fn h_keys(headers: HeaderMap, raw_query: axum::extract::RawQuery) -> Response {
    admin::keys(&headers, raw_query.0).await
}

async fn h_keydetail(headers: HeaderMap, raw_query: axum::extract::RawQuery) -> Response {
    admin::keydetail(&headers, raw_query.0).await
}

async fn h_keys_import(headers: HeaderMap, req: Request) -> Response {
    let ct = headers
        .get("content-type")
        .and_then(|x| x.to_str().ok())
        .unwrap_or("")
        .to_string();
    if ct.contains("multipart") {
        let boundary = ct
            .split(';')
            .map(|s| s.trim())
            .find_map(|s| s.strip_prefix("boundary="))
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        let body = match axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": {"message": "请求体读取失败"}})),
                )
                    .into_response();
            }
        };
        let (text, uid, file) = parse_multipart(&body, &boundary);
        admin::keys_import(&headers, true, Bytes::new(), Some((text, uid, file))).await
    } else {
        let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
            Ok(b) => b,
            Err(_) => Bytes::new(),
        };
        admin::keys_import(&headers, false, body, None).await
    }
}

/// 最小 multipart/form-data 解析（字段 text/upstream_id + 文件 file）。
fn parse_multipart(body: &[u8], boundary: &str) -> (String, String, Vec<u8>) {
    let delim = format!("--{}", boundary);
    let mut text = String::new();
    let mut uid = String::new();
    let mut file: Vec<u8> = Vec::new();
    let mut positions: Vec<usize> = Vec::new();
    let dl = delim.as_bytes();
    if dl.len() <= body.len() {
        for i in 0..=(body.len() - dl.len()) {
            if &body[i..i + dl.len()] == dl {
                positions.push(i);
            }
        }
    }
    for (pi, &start) in positions.iter().enumerate() {
        let seg_start = start + dl.len();
        let seg_end = positions.get(pi + 1).copied().unwrap_or(body.len());
        if seg_start >= body.len() {
            continue;
        }
        let seg = &body[seg_start..seg_end.min(body.len())];
        let Some(hdr_end) = seg.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let headers = String::from_utf8_lossy(&seg[..hdr_end]);
        let mut content = &seg[hdr_end + 4..];
        if content.ends_with(b"\r\n") {
            content = &content[..content.len() - 2];
        }
        let is_file = headers.to_lowercase().contains("filename=");
        let name = headers
            .to_lowercase()
            .split("name=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("")
            .to_string();
        if is_file {
            file = content.to_vec();
        } else if name == "text" {
            text = String::from_utf8_lossy(content).to_string();
        } else if name == "upstream_id" {
            uid = String::from_utf8_lossy(content).to_string();
        }
    }
    (text, uid, file)
}

async fn h_keys_export(headers: HeaderMap) -> Response {
    admin::keys_export(&headers).await
}

async fn h_keys_op(headers: HeaderMap, body: Bytes) -> Response {
    admin::keys_op(&headers, body).await
}

async fn h_keys_batch(headers: HeaderMap, body: Bytes) -> Response {
    admin::keys_batch(&headers, body).await
}

async fn h_keys_clear_all(headers: HeaderMap, body: Bytes) -> Response {
    admin::keys_clear_all(&headers, body).await
}

async fn h_queue_list(headers: HeaderMap) -> Response {
    admin::queue_list(&headers).await
}

async fn h_queue_clear(headers: HeaderMap) -> Response {
    admin::queue_clear(&headers).await
}

async fn h_upstreams_list(headers: HeaderMap) -> Response {
    admin::upstreams_list(&headers).await
}

async fn h_upstreams_save(headers: HeaderMap, body: Bytes) -> Response {
    admin::upstreams_save(&headers, body).await
}

async fn h_upstreams_delete(headers: HeaderMap, body: Bytes) -> Response {
    admin::upstreams_delete(&headers, body).await
}

async fn h_logs(headers: HeaderMap) -> Response {
    admin::logs(&headers).await
}

async fn h_logs_clear(headers: HeaderMap) -> Response {
    admin::logs_clear(&headers).await
}

async fn h_settings_get(headers: HeaderMap) -> Response {
    admin::settings_get(&headers).await
}

async fn h_settings_save(headers: HeaderMap, body: Bytes) -> Response {
    admin::settings_save(&headers, body).await
}

async fn h_password(headers: HeaderMap, body: Bytes) -> Response {
    admin::password_change(&headers, body).await
}

async fn h_stats_reset(headers: HeaderMap) -> Response {
    admin::stats_reset(&headers).await
}

async fn h_poolmap(headers: HeaderMap) -> Response {
    admin::poolmap(&headers).await
}

async fn h_tokens_list(headers: HeaderMap) -> Response {
    admin::tokens_list(&headers).await
}

async fn h_tokens_add(headers: HeaderMap, body: Bytes) -> Response {
    admin::tokens_add(&headers, body).await
}

async fn h_tokens_update(headers: HeaderMap, body: Bytes) -> Response {
    admin::tokens_update(&headers, body).await
}

async fn h_tokens_delete(headers: HeaderMap, body: Bytes) -> Response {
    admin::tokens_delete(&headers, body).await
}

async fn h_update(headers: HeaderMap) -> Response {
    admin::remote_update(&headers).await
}

async fn h_rollback(headers: HeaderMap) -> Response {
    admin::remote_rollback(&headers).await
}

async fn h_intercept_get(headers: HeaderMap) -> Response {
    admin::intercept_get(&headers).await
}

async fn h_intercept_rules(headers: HeaderMap, body: Bytes) -> Response {
    admin::intercept_rule_add(&headers, body).await
}

async fn h_intercept_rules_delete(headers: HeaderMap, body: Bytes) -> Response {
    admin::intercept_rule_delete(&headers, body).await
}

async fn h_intercept_test(headers: HeaderMap, body: Bytes) -> Response {
    admin::intercept_test(&headers, body).await
}

async fn h_intercept_toggle(headers: HeaderMap, body: Bytes) -> Response {
    admin::intercept_toggle(&headers, body).await
}

async fn h_intercept_clear(headers: HeaderMap) -> Response {
    admin::intercept_clear(&headers).await
}

async fn h_training_list(headers: HeaderMap, raw_query: axum::extract::RawQuery) -> Response {
    admin::training_list(&headers, raw_query.0).await
}

async fn h_training_clear(headers: HeaderMap) -> Response {
    admin::training_clear(&headers).await
}

async fn h_training_export(headers: HeaderMap) -> Response {
    admin::training_export(&headers).await
}

async fn h_sessions_list(headers: HeaderMap) -> Response {
    admin::sessions_list(&headers).await
}

async fn h_sessions_clear(headers: HeaderMap) -> Response {
    admin::sessions_clear(&headers).await
}

async fn h_config_export(headers: HeaderMap) -> Response {
    admin::config_export(&headers).await
}

async fn h_config_import(headers: HeaderMap, body: Bytes) -> Response {
    admin::config_import(&headers, body).await
}

// ---------------------------------------------------------------- 看门狗 / 自重启

static RESTARTING: AtomicBool = AtomicBool::new(false);
static POOL_CAP_WARN_AT: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);

/// 雪崩判定：所有启用账号都被封禁/冷却/并发占满，且队列里还有等待者。
pub fn watchdog_dead(
    db: &serde_json::Value,
    inflight: &std::collections::HashMap<String, i64>,
    now: i64,
    cfg: &serde_json::Value,
) -> (bool, serde_json::Value) {
    let enabled: Vec<&serde_json::Value> = db
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|a| {
            a.iter()
                .filter(|k| k.is_object() && k.get("enabled").map(util::truthy).unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    if enabled.is_empty() {
        return (false, json!({}));
    }
    let conc = util::cfg_int(cfg, "acct_concurrency", 0);
    let mut bad = 0i64;
    for k in &enabled {
        if util::int_or(k.get("banned_until"), 0) > now || util::int_or(k.get("cooldown_until"), 0) > now {
            bad += 1;
        } else if conc > 0 && inflight.get(&util::str_or(k.get("id"), "")).copied().unwrap_or(0) >= conc {
            bad += 1;
        }
    }
    let qn = db
        .get("queue")
        .and_then(|q| q.as_array())
        .map(|a| {
            a.iter()
                .filter(|e| e.is_object() && now as f64 - util::f64_or(e.get("t"), 0.0) < 900.0)
                .count()
        })
        .unwrap_or(0);
    let info = json!({"bad": bad, "total": enabled.len(), "queue": qn});
    (bad >= enabled.len() as i64 && qn > 0, info)
}

/// 近 10 条日志里的 PoolTimeout 是否值得「重启」处置（必须是连接泄漏特征才重启）。
fn pool_timeout_verdict(recent_logs: &serde_json::Value, inflight: i64, cap: i64) -> (bool, String) {
    let mut n = 0;
    if let Some(a) = recent_logs.as_array() {
        for x in a.iter().take(10) {
            if let Some(row) = x.as_array() {
                if row.len() > 6 && util::str_or(row.get(6), "").contains("PoolTimeout") {
                    n += 1;
                }
            }
        }
    }
    if n < 5 {
        return (false, String::new());
    }
    if cap > 0 && inflight >= (cap as f64 * 0.8) as i64 {
        return (false, String::new());
    }
    (
        true,
        format!(
            "近 10 条日志中 {} 条 PoolTimeout,而在途仅 {}(池上限 {})→ 疑似连接泄漏",
            n, inflight, if cap > 0 { cap.to_string() } else { "未知".into() }
        ),
    )
}

fn pool_capacity_warn(pool_timeout: i64, used: i64, cap: i64) {
    let mut w = POOL_CAP_WARN_AT.lock().unwrap();
    let now = util::now_f();
    if now - *w < 300.0 {
        return;
    }
    *w = now;
    eprintln!(
        "[pool] 池接近上限 {}/{}:近 10 条日志 PoolTimeout {} 次,建议 pool_max_connections ≥ {}",
        used, cap, pool_timeout, cap.max(used) + 100
    );
}

fn prepare_restart(reason: &str, tag: &str) {
    store().flush();
    let row = json!([
        util::now_i(), tag, "updater", "-", 200, 0,
        util::str_cut(&format!("{};已重启网关(队列条目已清)", reason), 140), "-", 1, "", 0, 0, 0, 0,
    ]);
    let log_on = store()
        .load()
        .pointer("/config/log_enabled")
        .map(util::truthy)
        .unwrap_or(true);
    let max_logs = store()
        .load()
        .pointer("/config/log_max")
        .and_then(util::py_int)
        .unwrap_or(200)
        .max(0) as usize;
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("queue".into(), serde_json::Value::Array(vec![]));
            if log_on {
                if let Some(o) = db.as_object_mut() {
                    let logs = o.entry("logs").or_insert_with(|| serde_json::Value::Array(vec![]));
                    if let Some(a) = logs.as_array_mut() {
                        a.insert(0, row);
                        a.truncate(max_logs);
                    }
                }
            }
        }
    });
    store().flush();
}

/// 自我重启：拉起新进程（它会重试绑定端口直到旧进程退出），本进程退出。
/// Windows 无 execv，这是语义等价的实现。
pub fn self_restart(reason: &str) -> bool {
    if RESTARTING.swap(true, Ordering::SeqCst) {
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => {
            RESTARTING.store(false, Ordering::SeqCst);
            return false;
        }
    };
    prepare_restart(reason, "watch");
    eprintln!("[restart] {} → 重启进程", reason);
    // 必须透传原始启动参数（--port 等）：否则 --port 12345 的实例重启后会绑回默认端口
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Unix 用 execv：同 PID 替换进程映像、监听端口随之释放，与 Python 版语义一致；
    // Windows 没有 execv，退而拉起新进程（子进程带重试绑端口）再退出
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe)
            .args(args)
            .env("NGW_RESTART_CHILD", "1")
            .exec();
        RESTARTING.store(false, Ordering::SeqCst);
        eprintln!("[restart] execv 失败：{}", err);
        return false;
    }
    #[cfg(not(unix))]
    {
        // 刚刚替换过 exe：杀毒/索引器可能短暂锁住新文件导致启动失败，
        // 重试几次再放弃（旧进程在此期间继续服务，不产生停机）
        let mut last = String::new();
        for attempt in 0..3 {
            match std::process::Command::new(&exe).args(&args).env("NGW_RESTART_CHILD", "1").spawn() {
                Ok(_) => std::process::exit(0),
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }
        }
        RESTARTING.store(false, Ordering::SeqCst);
        eprintln!("[restart] 无法自助重启：{}", last);
        // 界面上已显示「更新完成」，这里必须留下可见痕迹，否则版本静默停留在旧版
        let row = json!([
            util::now_i(), "watch", "updater", "-", 500, 0,
            util::str_cut(&format!("[网关异常] 自助重启失败：{}；更新包已就位，请手动重启进程", last), 140),
            "-", 1, "", 0, 0, 0, 0,
        ]);
        let max_logs = store()
            .load()
            .pointer("/config/log_max")
            .and_then(util::py_int)
            .unwrap_or(200)
            .max(0) as usize;
        store().update(|db| {
            if let Some(o) = db.as_object_mut() {
                let logs = o.entry("logs").or_insert_with(|| serde_json::Value::Array(vec![]));
                if let Some(a) = logs.as_array_mut() {
                    a.insert(0, row);
                    a.truncate(max_logs);
                }
            }
        });
        store().flush();
        false
    }
}

/// 定时自动检查更新（可选）：`update_enabled` + `update_auto_check_hours > 0` 时生效。
/// 发现比当前版本新的正式 Release 就记一条日志，并走与手动更新完全相同的
/// 「备份 → 替换 → 重启」流程（回滚备份照常生成）。
async fn update_auto_loop() {
    let mut first = true;
    loop {
        let hours = {
            // 每轮读取实时配置；关闭时保持 60s 空转，等设置生效
            // (trace 保留：自动升级属低频关键动作，stderr 留痕便于排查)
            let cfg = proxy::cfg_all();
            if !cfg.get("update_enabled").map(util::truthy).unwrap_or(false) {
                0
            } else {
                util::cfg_int(&cfg, "update_auto_check_hours", 0)
            }
        };
        if hours <= 0 {
            first = true;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            continue;
        }
        let wait = if first { 60 } else { hours as u64 * 3600 };
        eprintln!("[update] 自动检查任务: {}s 后检查 (间隔 {} 小时)", wait, hours);
        first = false;
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        let hours_now = {
            let cfg = proxy::cfg_all();
            util::cfg_int(&cfg, "update_auto_check_hours", 0)
        };
        if !cfg_now_enabled() || hours_now <= 0 {
            continue;
        }
        match update::check_newer_tag().await {
            Ok(Some(tag)) => {
                eprintln!("[update] 自动检查:发现新版本 {},开始自动升级", tag);
                let row = json!([
                    util::now_i(), "watch", "updater", "-", 200, 0,
                    util::str_cut(&format!("自动检查更新:发现 {},开始自动升级", tag), 140),
                    "-", 1, "", 0, 0, 0, 0,
                ]);
                let log_on = store()
                    .load()
                    .pointer("/config/log_enabled")
                    .map(util::truthy)
                    .unwrap_or(true);
                let max_logs = store()
                    .load()
                    .pointer("/config/log_max")
                    .and_then(util::py_int)
                    .unwrap_or(200)
                    .max(0) as usize;
                store().update(|db| {
                    if log_on {
                        if let Some(o) = db.as_object_mut() {
                            let logs = o.entry("logs").or_insert_with(|| serde_json::Value::Array(vec![]));
                            if let Some(a) = logs.as_array_mut() {
                                a.insert(0, row);
                                a.truncate(max_logs);
                            }
                        }
                    }
                });
                let (ok, msg) = update::remote_update().await;
                eprintln!("[update] 自动升级: {} {}", ok, msg);
                if ok {
                    return; // remote_update 已安排自重启
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[update] 自动检查失败: {}", e);
            }
        }
    }
}

fn cfg_now_enabled() -> bool {
    proxy::cfg_all()
        .get("update_enabled")
        .map(util::truthy)
        .unwrap_or(false)
}

/// SIGTERM/SIGINT 优雅退出：等价于 Python 版 uvicorn shutdown（落盘后退出）。
async fn signal_watch() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else { return };
        let Ok(mut int) = signal(SignalKind::interrupt()) else { return };
        tokio::select! {
            _ = term.recv() => { eprintln!("[shutdown] 收到 SIGTERM，落盘退出"); }
            _ = int.recv() => { eprintln!("[shutdown] 收到 SIGINT，落盘退出"); }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("[shutdown] 收到退出信号，落盘退出");
    }
    store().flush();
    std::process::exit(0);
}

async fn flush_loop() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tokio::task::spawn_blocking(|| store().flush()).await.ok();
    }
}

async fn watchdog_loop() {
    let mut dead_streak: i64 = 0;
    loop {
        let t0 = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let lag = t0.elapsed().as_secs_f64() - 30.0;
        let cfg = proxy::cfg_all();
        if !cfg.get("watchdog_enabled").map(util::truthy).unwrap_or(true) {
            dead_streak = 0;
            continue;
        }
        let minutes = util::cfg_int(&cfg, "watchdog_minutes", 3).max(1) * 60;
        let mut hit = false;
        let mut why = String::new();
        // 形态1:账号层全池不可用
        let db = store().load();
        let (dead, info) = watchdog_dead(&db, &pool::inflight_snapshot(), util::now_i(), &cfg);
        if dead && util::int_or(info.get("queue"), 0) > 0 {
            hit = true;
            why = format!(
                "全池不可用({}/{})且队列 {} 个等待",
                util::int_or(info.get("bad"), 0),
                util::int_or(info.get("total"), 0),
                util::int_or(info.get("queue"), 0)
            );
        }
        // 形态2:事件循环滞后
        if lag > 10.0 {
            hit = true;
            why = format!("事件循环滞后 {:.1} 秒(锁竞争/同步阻塞)", lag);
        }
        // 形态3:连接池持续满（必须是泄漏特征才重启；容量不足只提示）
        let used = pool::inflight_total();
        refresh_pool_cap();
        let cap = POOL_CAP.load(Ordering::SeqCst);
        let (restart_pool, why_pool) = pool_timeout_verdict(db.get("logs").unwrap_or(&serde_json::Value::Null), used, cap);
        if restart_pool {
            hit = true;
            why = why_pool;
        } else {
            let mut n_pt = 0;
            if let Some(a) = db.get("logs").and_then(|l| l.as_array()) {
                for x in a.iter().take(10) {
                    if let Some(row) = x.as_array() {
                        if row.len() > 6 && util::str_or(row.get(6), "").contains("PoolTimeout") {
                            n_pt += 1;
                        }
                    }
                }
            }
            if n_pt >= 5 {
                pool_capacity_warn(n_pt, used, cap);
            }
        }
        if hit {
            dead_streak += 30;
            if dead_streak >= minutes {
                self_restart(&format!("看门狗触发:{}，持续 {} 秒", why, dead_streak));
                return;
            }
        } else {
            dead_streak = 0;
        }
    }
}

async fn periodic_restart_loop() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let cfg = proxy::cfg_all();
        let interval = util::cfg_int(&cfg, "restart_interval_hours", 0);
        if interval <= 0 {
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval as u64 * 3600)).await;
        let cfg = proxy::cfg_all();
        if util::cfg_int(&cfg, "restart_interval_hours", 0) <= 0 {
            continue;
        }
        self_restart(&format!("定时重启(每 {} 小时)", interval));
        return;
    }
}

fn prune_queue() {
    store().update(|db| {
        let cfg = db.get("config").cloned().unwrap_or(json!({}));
        let now = util::now_f();
        let mut mw = util::cfg_int(&cfg, "queue_max_wait", 15);
        mw = if mw == 0 { 0 } else { mw.max(5) };
        let cool429 = util::cfg_int(&cfg, "cool_429_seconds", 0);
        let allowed = if mw > 0 && cool429 > 0 { mw.max((cool429 + 2).min(600)) } else { mw };
        let cutoff = now - (if allowed > 0 { allowed + 60 } else { 900 }) as f64;
        if let Some(o) = db.as_object_mut() {
            let q = o.entry("queue").or_insert_with(|| serde_json::Value::Array(vec![]));
            if let Some(a) = q.as_array_mut() {
                a.retain(|e| e.is_object() && util::f64_or(e.get("t"), 0.0) >= cutoff);
            }
        }
    });
}

// ---------------------------------------------------------------- 启动

#[tokio::main]
async fn main() {
    let mut port: u16 = 8100;
    let args: Vec<String> = std::env::args().collect();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--port" {
            if let Some(v) = it.next() {
                port = v.parse().unwrap_or(8100);
            }
        } else if let Some(rest) = a.strip_prefix("--port=") {
            port = rest.parse().unwrap_or(8100);
        } else if a == "--reset-password" {
            if let Some(pw) = it.next() {
                std::process::exit(cli_reset_password(pw));
            }
            eprintln!("用法：nim-gateway --reset-password 新密码");
            std::process::exit(2);
        }
    }
    if let Ok(p) = std::env::var("NGW_PORT").or_else(|_| std::env::var("PORT")) {
        if let Ok(v) = p.parse() {
            port = v;
        }
    }

    let store_ref = store();
    upstreams::ensure_default();
    {
        let cfg = store_ref.load();
        if let Some(c) = cfg.get("config") {
            init_http(c);
        }
    }
    store_ref.flush();
    prune_queue();

    tokio::spawn(flush_loop());
    tokio::spawn(signal_watch());
    tokio::spawn(update_auto_loop());
    tokio::spawn(watchdog_loop());
    tokio::spawn(periodic_restart_loop());

    let app = build_router();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match bind_with_retry(addr, 15).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[start] 端口 {} 绑定失败:{}", port, e);
            std::process::exit(1);
        }
    };
    eprintln!("[start] NIM Gateway (Rust) 监听 http://0.0.0.0:{} — 后台 /admin", port);
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        eprintln!("[start] 服务异常退出:{}", e);
    }
    store_ref.flush();
}

async fn bind_with_retry(addr: SocketAddr, max_secs: u64) -> std::io::Result<tokio::net::TcpListener> {
    // 普通启动也短暂重试：手动快速重启 / 旧进程被杀后，Windows 上 TIME_WAIT
    // 的同端口套接字会让首次 bind 失败（无 SO_REUSEADDR），等 2 秒即过
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if std::env::var("NGW_RESTART_CHILD").is_ok() { max_secs } else { 5 });
    loop {
        let attempt = || -> std::io::Result<tokio::net::TcpListener> {
            #[cfg(unix)]
            {
                // Unix 上显式设 SO_REUSEADDR：重启后立即重绑，不被旧连接的 TIME_WAIT 卡住
                use socket2::{Domain, Socket, Type};
                let sock = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
                sock.set_reuse_address(true)?;
                sock.set_nonblocking(true)?;
                sock.bind(&addr.into())?;
                sock.listen(1024)?;
                return tokio::net::TcpListener::from_std(std::net::TcpListener::from(sock));
            }
            #[cfg(not(unix))]
            {
                // Windows 的 SO_REUSEADDR 语义不同（允许双绑），保持默认
                std::net::TcpListener::bind(addr)
                    .and_then(|l| l.set_nonblocking(true).map(|_| l))
                    .and_then(tokio::net::TcpListener::from_std)
            }
        };
        match attempt() {
            Ok(l) => return Ok(l),
            Err(e) => {
                // 重启子进程：等旧进程释放端口
                if std::env::var("NGW_RESTART_CHILD").is_ok() && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

fn cli_reset_password(pw: &str) -> i32 {
    let pw = pw.trim();
    if pw.len() < 6 || pw.is_empty() {
        eprintln!("新密码至少 6 位且不能全为空白");
        return 2;
    }
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            cfg.insert(
                "admin_password_hash".into(),
                serde_json::Value::from(store::hash_password(pw)),
            );
        }
    });
    store().flush();
    let user = store()
        .load()
        .pointer("/config/admin_username")
        .map(|x| util::str_or(Some(x), "admin"))
        .unwrap_or_else(|| "admin".into());
    println!("已重置管理员密码（用户名 {}）。请用新密码登录后台。", user);
    0
}

fn build_router() -> Router {
    Router::new()
        // /v1 数据面
        .route("/v1/models", get(h_v1_models).options(h_options))
        .route("/v1/models/", get(h_v1_models).options(h_options))
        .route("/v1/chat/completions", post(h_chat).options(h_options))
        .route("/v1/chat/completions/", post(h_chat).options(h_options))
        .route("/v1/completions", post(h_completions).options(h_options))
        .route("/v1/completions/", post(h_completions).options(h_options))
        .route("/v1/embeddings", post(h_embeddings).options(h_options))
        .route("/v1/embeddings/", post(h_embeddings).options(h_options))
        .route("/v1/responses", post(h_responses).options(h_options))
        .route("/v1/responses/", post(h_responses).options(h_options))
        .route("/v1/messages", post(h_messages).options(h_options))
        .route("/v1/messages/", post(h_messages).options(h_options))
        .route("/v1/models/{*rest}", get(h_v1_model_retrieve))
        .route("/v1/{*rest}", get(h_v1_get).post(h_v1_post))
        .route("/v1/{*rest}", options(h_options))
        // 页面与公开接口
        .route("/", get(h_root))
        .route("/admin", get(h_admin))
        .route("/queue", get(h_queue_page))
        .route("/api/presets", get(h_presets))
        .route("/api/queue/public", get(h_queue_public))
        .route("/assets/admin.js", get(h_admin_js))
        // 管理 API
        .route("/api/login", post(h_login))
        .route("/api/logout", post(h_logout))
        .route("/api/overview", get(h_overview))
        .route("/api/keys", get(h_keys))
        .route("/api/keydetail", get(h_keydetail))
        .route("/api/keys/import", post(h_keys_import))
        .route("/api/keys/export", get(h_keys_export))
        .route("/api/keys/op", post(h_keys_op))
        .route("/api/keys/batch", post(h_keys_batch))
        .route("/api/keys/clear-all", post(h_keys_clear_all))
        .route("/api/queue", get(h_queue_list).post(h_queue_clear))
        .route("/api/upstreams", get(h_upstreams_list).post(h_upstreams_save))
        .route("/api/upstreams/delete", post(h_upstreams_delete))
        .route("/api/logs", get(h_logs))
        .route("/api/logs/clear", post(h_logs_clear))
        .route("/api/settings", get(h_settings_get).post(h_settings_save))
        .route("/api/password", post(h_password))
        .route("/api/stats/reset", post(h_stats_reset))
        .route("/api/poolmap", get(h_poolmap))
        .route("/api/tokens", get(h_tokens_list).post(h_tokens_add))
        .route("/api/tokens/update", post(h_tokens_update))
        .route("/api/tokens/delete", post(h_tokens_delete))
        .route("/api/update", post(h_update))
        .route("/api/rollback", post(h_rollback))
        .route("/api/intercept", get(h_intercept_get))
        .route("/api/intercept/rules", post(h_intercept_rules))
        .route("/api/intercept/rules/delete", post(h_intercept_rules_delete))
        .route("/api/intercept/test", post(h_intercept_test))
        .route("/api/intercept/toggle", post(h_intercept_toggle))
        .route("/api/intercept/clear", post(h_intercept_clear))
        .route("/api/training", get(h_training_list))
        .route("/api/training/clear", post(h_training_clear))
        .route("/api/training/export", get(h_training_export))
        .route("/api/sessions", get(h_sessions_list))
        .route("/api/sessions/clear", post(h_sessions_clear))
        .route("/api/config/export", get(h_config_export))
        .route("/api/config/import", post(h_config_import))
        // axum 默认把请求体限制在 2MB：不显式放宽，MAX_BODY=20MB 永远不会生效，
        // 大 prompt 会在提取器阶段被 413 掉。层上限比 MAX_BODY 高 1MB：边界请求
        // 进到处理器里返回友好的「请求体过大」，超过层上限才走 axum 的通用 413
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY + 1024 * 1024))
}
