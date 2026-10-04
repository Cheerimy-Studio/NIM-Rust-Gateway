//! 多用户系统：用户、用户 Key（调用令牌）、计费、用户级限速、用户调用日志。
//!
//! 计费口径：管理员按「渠道 + 模型」设置单次成功请求价格（元/次），
//! 仅成功调用计费；渠道未设置价格的模型为免费模型。
//! 余额以元为单位按 f64 存储，写入时统一保留 6 位小数。

use crate::store::{store, Obj, Value};
use crate::util;
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Mutex;

pub fn round6(f: f64) -> f64 {
    util::round6(f)
}

fn money(v: Option<&Value>) -> f64 {
    util::f64_or(v, 0.0)
}

// ---------------------------------------------------------------- 用户 CRUD

pub fn list_users() -> Vec<Value> {
    store()
        .load()
        .get("users")
        .and_then(|u| u.as_array())
        .map(|a| {
            a.iter()
                .map(|u| {
                    let mut row = u.clone();
                    if let Some(o) = row.as_object_mut() {
                        o.remove("password_hash"); // 口令哈希不出后台
                    }
                    row
                })
                .collect()
        })
        .unwrap_or_default()
}

fn users_mut<R>(db: &mut Value, f: impl FnOnce(&mut Vec<Value>) -> R) -> R {
    let obj = db.as_object_mut().unwrap();
    let arr = obj.entry("users").or_insert_with(|| Value::Array(vec![]));
    if !arr.is_array() {
        *arr = Value::Array(vec![]);
    }
    f(arr.as_array_mut().unwrap())
}

pub fn get_user_by_id(user_id: &str) -> Option<Value> {
    store()
        .load()
        .get("users")
        .and_then(|u| u.as_array())
        .and_then(|a| {
            a.iter()
                .find(|u| util::str_or(u.get("id"), "") == user_id)
                .cloned()
        })
}

fn find_user_mut<'a>(arr: &'a mut Vec<Value>, user_id: &str) -> Option<&'a mut Value> {
    arr.iter_mut().find(|u| util::str_or(u.get("id"), "") == user_id)
}

/// 管理员添加用户（不做自助注册）。返回 (用户行, 错误)。
pub fn add_user(username: &str, password: &str, balance: f64, free_rpm: i64, paid_rpm: i64) -> (Option<Value>, String) {
    let username = username.trim();
    if username.is_empty() || username.chars().count() > 32 {
        return (None, "用户名不能为空且不超过 32 字符".into());
    }
    if !username.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '@' || c == '.') {
        return (None, "用户名仅支持字母、数字、_ - @ .".into());
    }
    if password.chars().count() < 6 {
        return (None, "密码至少 6 位".into());
    }
    let mut out: Option<Value> = None;
    let mut err = String::new();
    let mut disable_keys = false;
    store().update(|db| {
        users_mut(db, |arr| {
            if arr.iter().any(|u| {
                util::str_or(u.get("username"), "").eq_ignore_ascii_case(username)
            }) {
                err = "用户名已存在".into();
                return;
            }
            let now = util::now_i();
            let row = json!({
                "id": format!("u8_{}", util::rand_hex(5)),
                "username": username,
                "password_hash": crate::store::hash_password(password),
                "balance": util::round6(balance.max(0.0)),
                "free_rpm": free_rpm.max(0),
                "paid_rpm": paid_rpm.max(0),
                "enabled": true,
                "created_at": now,
                "updated_at": now,
                "note": "",
            });
            out = Some(row.clone());
            arr.push(row);
        });
    });
    (out, err)
}

/// 用户操作：set-balance / add-balance / enable / disable / delete / reset-password / set-limits
pub fn user_op(user_id: &str, op: &str, body: &Value) -> (bool, String) {
    let mut ok = false;
    let mut err = String::new();
    let mut disable_keys = false;
    store().update(|db| {
        users_mut(db, |arr| {
            let Some(u) = find_user_mut(arr, user_id) else {
                err = "用户不存在".into();
                return;
            };
            match op {
                "set-balance" => {
                    let v = money(body.get("balance"));
                    if let Some(o) = u.as_object_mut() {
                        o.insert("balance".into(), json!(util::round6(v)));
                    }
                    ok = true;
                }
                "add-balance" => {
                    let v = money(body.get("amount"));
                    let cur = money(u.get("balance"));
                    if let Some(o) = u.as_object_mut() {
                        o.insert("balance".into(), json!(util::round6(cur + v)));
                    }
                    ok = true;
                }
                "enable" | "disable" => {
                    let enabled = op == "enable";
                    if let Some(o) = u.as_object_mut() {
                        o.insert("enabled".into(), Value::from(enabled));
                    }
                    ok = true;
                }
                "reset-password" => {
                    let pw = util::str_or(body.get("password"), "");
                    if pw.chars().count() < 6 {
                        err = "密码至少 6 位".into();
                        return;
                    }
                    if let Some(o) = u.as_object_mut() {
                        o.insert("password_hash".into(), Value::from(crate::store::hash_password(&pw)));
                    }
                    ok = true;
                }
                "set-limits" => {
                    if let Some(o) = u.as_object_mut() {
                        if let Some(v) = body.get("free_rpm") {
                            o.insert("free_rpm".into(), json!(util::int_or(Some(v), 0).max(0)));
                        }
                        if let Some(v) = body.get("paid_rpm") {
                            o.insert("paid_rpm".into(), json!(util::int_or(Some(v), 0).max(0)));
                        }
                    }
                    ok = true;
                }
                "delete" => {
                    disable_keys = true;
                    let id = util::str_or(u.get("id"), "");
                    arr.retain(|x| util::str_or(x.get("id"), "") != id);
                    ok = true;
                }
                _ => err = "未知操作".into(),
            }
        });
    });
    if disable_keys {
        store().update(|db| {
            if let Some(tokens) = db.get_mut("user_tokens").and_then(|t| t.as_array_mut()) {
                for t in tokens.iter_mut() {
                    if util::str_or(t.get("user_id"), "") == user_id {
                        if let Some(o) = t.as_object_mut() {
                            o.insert("enabled".into(), Value::from(false));
                        }
                    }
                }
            }
        });
    }
    (ok, err)
}

// ---------------------------------------------------------------- 用户会话

/// 用户会话 Cookie：base64(user_id).HMAC(secret, "user:"+user_id)——无状态、不可伪造。
pub fn user_session_value(secret: &str, user_id: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(user_id.as_bytes());
    let sig = crate::store::hmac_hex(secret.as_bytes(), format!("user:{}", user_id).as_bytes());
    format!("{}.{}", b64, sig)
}

/// 校验会话 Cookie 并还原 user_id。
pub fn user_session_id(secret: &str, cookie_value: &str) -> Option<String> {
    let (b64, sig) = cookie_value.split_once('.')?;
    use base64::Engine;
    let user_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64).ok()?;
    let user_id = String::from_utf8(user_id).ok()?;
    let expect = crate::store::hmac_hex(secret.as_bytes(), format!("user:{}", user_id).as_bytes());
    use subtle::ConstantTimeEq;
    if sig.as_bytes().ct_eq(expect.as_bytes()).into() {
        Some(user_id)
    } else {
        None
    }
}

pub fn user_csrf_token(secret: &str, user_id: &str) -> String {
    crate::store::hmac_hex(secret.as_bytes(), format!("ucsrfs:{}", user_id).as_bytes())
}

/// 读取（启用中的）用户行；口令哈希剔除。
pub fn auth_user(user_id: &str) -> Option<Value> {
    let u = get_user_by_id(user_id)?;
    if !u.get("enabled").map(util::truthy).unwrap_or(false) {
        return None;
    }
    Some(u)
}

// ---------------------------------------------------------------- 用户 Key

pub fn user_key_prefix() -> &'static str {
    "sk-usr-"
}

pub fn list_keys(user_id: &str) -> Vec<Value> {
    store()
        .load()
        .get("user_tokens")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter(|t| util::str_or(t.get("user_id"), "") == user_id)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub fn add_key(user_id: &str, name: &str) -> Value {
    let key = format!("{}{}", user_key_prefix(), util::rand_hex(20));
    let row = json!({
        "id": format!("ut_{}", util::rand_hex(5)),
        "user_id": user_id,
        "key": key,
        "name": util::str_cut(name, 40),
        "enabled": true,
        "created_at": util::now_i(),
        "last_used_at": 0,
    });
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let arr = obj.entry("user_tokens").or_insert_with(|| Value::Array(vec![]));
        if let Some(a) = arr.as_array_mut() {
            a.push(row.clone());
        }
    });
    row
}

pub fn key_op(user_id: &str, key_id: &str, op: &str) -> bool {
    let mut ok = false;
    store().update(|db| {
        if let Some(arr) = db.get_mut("user_tokens").and_then(|t| t.as_array_mut()) {
            for t in arr.iter_mut() {
                if util::str_or(t.get("id"), "") != key_id
                    || util::str_or(t.get("user_id"), "") != user_id
                {
                    continue;
                }
                match op {
                    "enable" | "disable" => {
                        if let Some(o) = t.as_object_mut() {
                            o.insert("enabled".into(), Value::from(op == "enable"));
                        }
                        ok = true;
                    }
                    "delete" => {
                        ok = true; // retain 在下方统一处理
                    }
                    _ => {}
                }
            }
            if op == "delete" {
                arr.retain(|t| {
                    !((util::str_or(t.get("id"), "") == key_id)
                        && (util::str_or(t.get("user_id"), "") == user_id))
                });
            }
        }
    });
    ok
}

/// Bearer Key → 启用中的 (用户行, 令牌行)。管理员网关令牌不在此列（调用方先查）。
pub fn lookup_user_key(key: &str) -> Option<(Value, Value)> {
    if !key.starts_with(user_key_prefix()) {
        return None;
    }
    let db = store().load();
    let tokens = db.get("user_tokens").and_then(|t| t.as_array())?;
    let t = tokens
        .iter()
        .find(|t| util::str_or(t.get("key"), "") == key && t.get("enabled").map(util::truthy).unwrap_or(false))?;
    let user_id = util::str_or(t.get("user_id"), "");
    let u = auth_user(&user_id)?;
    Some((u, t.clone()))
}

/// 用户面板对话测试：取本人第一个启用中的 Key；一个都没有则自动建「对话测试」Key。
pub fn ensure_test_key(user_id: &str) -> Option<String> {
    for t in list_keys(user_id) {
        if !t.get("enabled").map(util::truthy).unwrap_or(false) {
            continue;
        }
        let k = util::str_or(t.get("key"), "");
        if !k.is_empty() {
            return Some(k);
        }
    }
    let row = add_key(user_id, "对话测试");
    let k = util::str_or(row.get("key"), "");
    if k.is_empty() { None } else { Some(k) }
}

// ---------------------------------------------------------------- 计费与限速

/// 渠道上某模型的单次价格（元/次）。未设置 = 免费模型。
pub fn price_for(upstream_id: &str, model: &str) -> f64 {
    let ups = crate::upstreams::all_upstreams();
    let Some(u) = ups.iter().find(|u| util::str_or(u.get("id"), "") == upstream_id) else {
        return 0.0;
    };
    money(u.get("prices").and_then(|p| p.get(model)))
}

/// 该模型在任一启用渠道上是否收费（用于请求前置的余额检查）。
pub fn model_is_paid(model: &str) -> bool {
    crate::upstreams::all_upstreams()
        .iter()
        .any(|u| {
            u.get("enabled").map(util::truthy).unwrap_or(false)
                && money(u.get("prices").and_then(|p| p.get(model))) > 0.0
        })
}

/// 用户级每分钟滑动窗口限速。kind: "free" / "paid"。
/// 返回 Ok(()) = 放行；Err(还需等待的秒数) = 超限。
pub fn check_user_rpm(user_id: &str, kind: &str, rpm: i64) -> Result<(), f64> {
    if rpm <= 0 {
        return Ok(()); // 0 = 不限
    }
    let now = util::now_f();
    let mut wait = 0.0f64;
    let mut denied = false;
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let all = obj
            .entry("user_rpm")
            .or_insert_with(|| Value::Object(Obj::new()));
        if !all.is_object() {
            *all = Value::Object(Obj::new());
        }
        let m = all.as_object_mut().unwrap();
        let ent = m
            .entry(user_id.to_string())
            .or_insert_with(|| json!({}));
        if !ent.is_object() {
            *ent = json!({});
        }
        let e = ent.as_object_mut().unwrap();
        let k = format!("{}_window", kind);
        let win = e
            .entry(k.clone())
            .or_insert_with(|| Value::Array(vec![]))
            .clone();
        let mut ts: Vec<f64> = win
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
            .unwrap_or_default();
        ts.retain(|t| now - t < 60.0);
        if ts.len() as i64 >= rpm {
            denied = true;
            // 最早一条滑出窗口所需的时间
            wait = (60.0 - (now - ts[0])).max(0.5);
        } else {
            ts.push(now);
            e.insert(k, json!(ts));
        }
    });
    if denied {
        Err(wait)
    } else {
        Ok(())
    }
}

/// 成功调用计费 + 用户调用日志。channel 上未设置价格的模型 = 免费（cost=0 也记日志）。
pub fn bill(
    user_id: &str,
    key_id: &str,
    channel_id: &str,
    model: &str,
    upstream_model: &str,
    st: i64,
    ms: i64,
    in_tok: i64,
    out_tok: i64,
    stream: bool,
) -> f64 {
    let price = price_for(channel_id, model);
    let cost = if price > 0.0 { util::round6(price) } else { 0.0 };
    let now = util::now_i();
    let row = json!({
        "t": now,
        "user_id": user_id,
        "key_id": key_id,
        "model": util::str_cut(model, 80),
        "up_model": util::str_cut(upstream_model, 80),
        "channel": channel_id,
        "st": st,
        "ms": ms,
        "cost": cost,
        "in_tok": in_tok,
        "out_tok": out_tok,
        "stream": stream,
    });
    let cap = {
        let cfg = store().load();
        util::cfg_int(cfg.get("config").unwrap_or(&json!({})), "user_log_max", 1000).max(0) as usize
    };
    store().update(|db| {
        // 日志（全局上限 user_log_max，默认 1000）
        {
            let obj = db.as_object_mut().unwrap();
            let logs = obj.entry("user_logs").or_insert_with(|| Value::Array(vec![]));
            if let Some(a) = logs.as_array_mut() {
                a.insert(0, row);
                a.truncate(cap);
            }
        }
        // 扣费（仅付费模型）
        if cost > 0.0 {
            if let Some(arr) = db.get_mut("users").and_then(|u| u.as_array_mut()) {
                for u in arr.iter_mut() {
                    if util::str_or(u.get("id"), "") != user_id {
                        continue;
                    }
                    if let Some(o) = u.as_object_mut() {
                        let cur = money(o.get("balance"));
                        o.insert("balance".into(), json!(util::round6(cur - cost)));
                    }
                    break;
                }
            }
        }
    });
    cost
}

/// 用户调用日志（时间倒序，n 条）。
pub fn user_logs(user_id: &str, n: usize) -> Vec<Value> {
    store()
        .load()
        .get("user_logs")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| util::str_or(r.get("user_id"), "") == user_id)
                .take(n)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 模型广场：对外可用模型 + 价格（取各渠道最低价；无价 = 免费）。
pub fn model_square() -> Vec<Value> {
    let db = store().load();
    let ups = db.get("upstreams").and_then(|u| u.as_array()).cloned().unwrap_or_default();
    let mut best: HashMap<String, f64> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for u in &ups {
        if !u.get("enabled").map(util::truthy).unwrap_or(false) {
            continue;
        }
        let prices = u.get("prices").and_then(|p| p.as_object()).cloned().unwrap_or_default();
        let models: Vec<String> = u
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().map(|m| util::str_or(Some(m), "")).collect())
            .unwrap_or_default();
        for m in models {
            if m.is_empty() {
                continue;
            }
            let price = prices
                .get(&m)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let e = best.entry(m.clone()).or_insert(f64::INFINITY);
            if price < *e {
                *e = price;
            }
            if !order.contains(&m) {
                order.push(m);
            }
        }
        // model_map 别名同样进广场（按别名自身计价，与计费口径一致）
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for (alias, _target) in mm {
                let price = prices.get(alias).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let e = best.entry(alias.clone()).or_insert(f64::INFINITY);
                if price < *e {
                    *e = price;
                }
                if !order.contains(alias) {
                    order.push(alias.clone());
                }
            }
        }
    }
    // 健康度：从 up_recent 取各渠道该模型的最近成功率，取最优值
    let up_recent = db.get("up_recent").and_then(|r| r.as_object()).cloned().unwrap_or_default();
    let ups_list = db.get("upstreams").and_then(|u| u.as_array()).cloned().unwrap_or_default();
    order
        .into_iter()
        .map(|m| {
            let price = best.get(&m).copied().unwrap_or(f64::INFINITY);
            let mut health: Option<f64> = None;
            for u in &ups_list {
                let ch_id = util::str_or(u.get("id"), "");
                if ch_id.is_empty() { continue; }
                let key = format!("{}{}{}", ch_id, '\u{0}', m);
                if let Some(rec) = up_recent.get(&key).and_then(|r| r.as_array()) {
                    if rec.is_empty() { continue; }
                    let ok = rec.iter().filter(|v| v.as_i64().unwrap_or(0) != 0).count();
                    let ratio = ok as f64 / rec.len() as f64;
                    if health.map_or(true, |h| ratio > h) { health = Some(ratio); }
                }
            }
            let health_pct = health.map(|h| (h * 100.0).round() as i64);
            json!({
                "model": m,
                "free": !(price.is_finite() && price > 0.0),
                "price": if price.is_finite() && price > 0.0 { json!(util::round6(price)) } else { json!(0) },
                "health": health_pct,
            })
        })
        .collect()
}

/// 计入/查询用户免费、付费窗口的辅助（管理端展示用）。
pub fn rpm_snapshot(user_id: &str) -> Value {
    let db = store().load();
    let now = util::now_f();
    let ent = db
        .pointer("/user_rpm")
        .and_then(|m| m.get(user_id))
        .cloned()
        .unwrap_or(json!({}));
    let count = |k: &str| -> i64 {
        ent.get(k)
            .and_then(|w| w.as_array())
            .map(|a| a.iter().filter(|t| now - t.as_f64().unwrap_or(0.0) < 60.0).count() as i64)
            .unwrap_or(0)
    };
    json!({"free": count("free_window"), "paid": count("paid_window")})
}

