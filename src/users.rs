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
                        // 账本字段补齐（旧用户迁移为只读展示，不写盘）
                        let balance = util::round6(util::f64_or(o.get("balance"), 0.0));
                        o.entry("grant".to_string()).or_insert(json!(0.0));
                        o.entry("grant_total".to_string()).or_insert(json!(0.0));
                        o.entry("recharge_total".to_string()).or_insert(json!(balance.max(0.0)));
                        o.entry("free_calls".to_string()).or_insert(json!(0));
                        o.entry("paid_calls".to_string()).or_insert(json!(0));
                        o.entry("total_cost".to_string()).or_insert(json!(0.0));
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
    let bal = util::round6(balance.max(0.0));
    let mut created_id = String::new();
    store().update(|db| {
        users_mut(db, |arr| {
            if arr.iter().any(|u| {
                util::str_or(u.get("username"), "").eq_ignore_ascii_case(username)
            }) {
                err = "用户名已存在".into();
                return;
            }
            let now = util::now_i();
            // 建户余额全部划入充值账本（注册赠送走 gift-balance 才是赠金）
            let row = json!({
                "id": format!("u8_{}", util::rand_hex(5)),
                "username": username,
                "password_hash": crate::store::hash_password(password),
                "balance": bal,
                "recharge_total": bal,
                "grant": 0.0,
                "grant_total": 0.0,
                "free_rpm": free_rpm.max(0),
                "paid_rpm": paid_rpm.max(0),
                "enabled": true,
                "created_at": now,
                "updated_at": now,
                "note": "",
            });
            let new_id = util::str_or(row.get("id"), "");
            out = Some(row.clone());
            arr.push(row);
            created_id = new_id;
        });
        // users_mut 借用结束后再写流水（同一次 update 内，不嵌套加锁）
        if !created_id.is_empty() && bal > 0.0 {
            fund_log(db, &created_id, "recharge", 0.0, bal, "建户初始余额");
        }
    });
    (out, err)
}

/// 用户操作：set-balance / add-balance / enable / disable / delete / reset-password / set-limits
pub fn user_op(user_id: &str, op: &str, body: &Value) -> (bool, String) {
    let mut ok = false;
    let mut err = String::new();
    let mut disable_keys = false;
    let mut note: Option<(&'static str, f64, f64, String)> = None;
    store().update(|db| {
        users_mut(db, |arr| {
            let Some(u) = find_user_mut(arr, user_id) else {
                err = "用户不存在".into();
                return;
            };
            match op {
                "set-balance" => {
                    // kind=recharge（默认，兼容旧调用）设充值当前；kind=grant 设赠金当前
                    let v = money(body.get("balance"));
                    if v < 0.0 {
                        err = "余额不能为负数".into();
                        return;
                    }
                    let kind = util::str_or(body.get("kind"), "recharge");
                    let (before_g, before_r) = (grant_of(u), recharge_of(u));
                    if let Some(o) = u.as_object_mut() {
                        if kind == "grant" {
                            o.insert("grant".into(), json!(util::round6(v)));
                            if !o.contains_key("grant_total") {
                                o.insert("grant_total".into(), json!(0.0));
                            }
                        } else {
                            o.insert("balance".into(), json!(util::round6(v)));
                            if !o.contains_key("recharge_total") {
                                o.insert("recharge_total".into(), json!(0.0));
                            }
                        }
                    }
                    let (after_g, after_r) = (grant_of(u), recharge_of(u));
                    note = Some((
                        "adjust",
                        util::round6(after_g - before_g),
                        util::round6(after_r - before_r),
                        format!("设置{}余额", if kind == "grant" { "赠金" } else { "充值" }),
                    ));
                    ok = true;
                }
                "add-balance" => {
                    // kind=recharge（默认，兼容旧调用）加充值账本并计累计
                    let v = money(body.get("amount"));
                    if v < 0.0 {
                        err = "金额不能为负数（扣减请用 set-balance）".into();
                        return;
                    }
                    let kind = util::str_or(body.get("kind"), "recharge");
                    if let Some(o) = u.as_object_mut() {
                        if kind == "grant" {
                            credit(u, "grant", v);
                        } else {
                            credit(u, "recharge", v);
                        }
                    }
                    note = Some((
                        if kind == "grant" { "grant" } else { "recharge" },
                        if kind == "grant" { util::round6(v) } else { 0.0 },
                        if kind == "recharge" { util::round6(v) } else { 0.0 },
                        "管理员发放".into(),
                    ));
                    ok = true;
                }
                "gift-balance" => {
                    // 注册赠送：金额入充值账本且计累计（语义=运营送的充值额度）
                    let v = money(body.get("amount"));
                    if v < 0.0 {
                        err = "金额不能为负数".into();
                        return;
                    }
                    if let Some(o) = u.as_object_mut() {
                        credit(u, "recharge", v);
                    }
                    note = Some(("recharge", 0.0, util::round6(v), "注册赠送".into()));
                    ok = true;
                }
                "gift-grant" => {
                    // 赠金发放（含签到以外的运营补偿）
                    let v = money(body.get("amount"));
                    if v < 0.0 {
                        err = "金额不能为负数".into();
                        return;
                    }
                    if let Some(o) = u.as_object_mut() {
                        credit(u, "grant", v);
                    }
                    note = Some(("grant", util::round6(v), 0.0, "管理员发放".into()));
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
                    let next_epoch = user_pw_epoch(u) + 1;
                    if let Some(o) = u.as_object_mut() {
                        o.insert("password_hash".into(), Value::from(crate::store::hash_password(&pw)));
                        // 改密即踢会话：纪元 +1，所有旧 Cookie 的签名载荷里的纪元即刻不匹配
                        o.insert("pw_epoch".into(), json!(next_epoch));
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
        // 删用户时清掉抽奖记录/奖品 Key（活跃凭据），但【保留资金流水】：
        // 欠款与充值记录是财务凭据，删除用户不应抹掉审计轨迹
        // （fund_logs 每用户上限 500 条，孤儿数据无害）
        if ok && op == "delete" {
            if let Some(a) = db.get_mut("draw_logs").and_then(|l| l.as_array_mut()) {
                a.retain(|r| util::str_or(r.get("user_id"), "") != user_id);
            }
            if let Some(a) = db.get_mut("prize_keys").and_then(|k| k.as_array_mut()) {
                a.retain(|k| util::str_or(k.get("user_id"), "") != user_id);
            }
        }
        // 资金变动流水（同一次 update 内写入，与账本变更原子）
        if ok {
            if let Some((kind, dg, dr, nt)) = note.take() {
                fund_log(db, user_id, kind, dg, dr, &nt);
            }
        }
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
/// 会话有效期（秒）：30 天强制过期，配合改密失效把「永久有效 Cookie」风险兜住
const USER_SESSION_TTL: i64 = 30 * 24 * 3600;

/// 该用户当前会话纪元：改密时 +1，所有携旧纪元的 Cookie 即刻失效（精确，无秒级边界）
fn user_pw_epoch(user: &Value) -> i64 {
    util::int_or(user.get("pw_epoch"), 0)
}

pub fn user_session_value(secret: &str, user_id: &str) -> String {
    use base64::Engine;
    let epoch = get_user_by_id(user_id).map(|u| user_pw_epoch(&u)).unwrap_or(0);
    let issued = util::now_i();
    let payload = format!("{}|{}|{}", user_id, epoch, issued);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let sig = crate::store::hmac_hex(secret.as_bytes(), format!("user:{}", payload).as_bytes());
    format!("{}.{}", b64, sig)
}

/// 校验会话 Cookie 并还原 (user_id, 纪元, 签发时间)。
fn user_session_info(secret: &str, cookie_value: &str) -> Option<(String, i64, i64)> {
    let (b64, sig) = cookie_value.split_once('.')?;
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64).ok()?;
    let payload = String::from_utf8(payload).ok()?;
    let expect = crate::store::hmac_hex(secret.as_bytes(), format!("user:{}", payload).as_bytes());
    use subtle::ConstantTimeEq;
    let ok: bool = sig.as_bytes().ct_eq(expect.as_bytes()).into();
    if !ok {
        return None;
    }
    let (user_id, rest) = payload.split_once('|')?;
    let (epoch, issued) = rest.split_once('|')?;
    let (epoch, issued) = (epoch.parse::<i64>().ok()?, issued.parse::<i64>().ok()?);
    if issued <= 0 || util::now_i() - issued > USER_SESSION_TTL {
        return None;
    }
    Some((user_id.to_string(), epoch, issued))
}

/// 校验会话 Cookie 并还原 user_id：签名 + 有效期 + 改密失效（纪元不匹配拒绝）。
pub fn user_session_id(secret: &str, cookie_value: &str) -> Option<String> {
    let (uid, epoch, _issued) = user_session_info(secret, cookie_value)?;
    let u = get_user_by_id(&uid)?;
    if user_pw_epoch(&u) != epoch {
        return None;
    }
    if auth_user(&uid).is_some() {
        Some(uid)
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
    add_key_kind(user_id, name, "all")
}

/// kind: "all" / "free" / "paid" —— Key 的模型类型限制；无字段的历史 Key 视为 "all"。
pub fn add_key_kind(user_id: &str, name: &str, kind: &str) -> Value {
    let kind = match kind {
        "free" | "paid" => kind,
        _ => "all",
    };
    // 数量上限：防止脚本化滥建 Key 撑大 users 组（含停用的合计）
    const MAX_KEYS_PER_USER: usize = 20;
    let count = store()
        .load()
        .get("user_tokens")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter(|t| util::str_or(t.get("user_id"), "") == user_id).count())
        .unwrap_or(0);
    if count >= MAX_KEYS_PER_USER {
        return json!({"error": "每个用户最多 20 个 Key，请先删除不用的"});
    }
    let key = format!("{}{}", user_key_prefix(), util::rand_hex(20));
    let row = json!({
        "id": format!("ut_{}", util::rand_hex(5)),
        "user_id": user_id,
        "key": key,
        "name": util::str_cut(name, 40),
        "kind": kind,
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

/// Key 的模型类型限制："all" / "free" / "paid"；无字段的历史 Key = 全部。
pub fn key_kind(t: &Value) -> &'static str {
    match util::str_or(t.get("kind"), "all").as_str() {
        "free" => "free",
        "paid" => "paid",
        _ => "all",
    }
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

// ---------------------------------------------------------------- 金额与账本

/// 余额统一口径（内部字段 balance=充值当前）+grant（赠金当前）：
/// 计费先扣赠金再扣充值；grant_total/recharge_total 为累计获得，只增不减。
pub fn grant_of(u: &Value) -> f64 {
    util::f64_or(u.get("grant"), 0.0)
}

pub fn recharge_of(u: &Value) -> f64 {
    util::f64_or(u.get("balance"), 0.0)
}

/// 账面可用余额 = 赠金当前 + 充值当前（兼容旧字段 grant_total/recharge_total 缺失=0）。
pub fn balance_of(u: &Value) -> f64 {
    util::round6(grant_of(u) + recharge_of(u))
}

/// 「余额」判断统一走可用总额（余额预检、旧引用 money_of 的用户侧语义）。
pub fn user_balance_value(u: &Value) -> f64 {
    balance_of(u)
}

/// 用户「当前/总」信息（面板展示）。
pub fn wallet_row(u: &Value) -> Value {
    json!({
        "grant": util::round6(grant_of(u)),
        "grant_total": util::round6(util::f64_or(u.get("grant_total"), 0.0)),
        "recharge": util::round6(recharge_of(u)),
        "recharge_total": util::round6(util::f64_or(u.get("recharge_total"), 0.0)),
        "balance": balance_of(u),
    })
}

/// 记一笔获得（来源 grant=赠金 / recharge=充值）：加当前 + 加累计。
/// 赠金累计只记赠金来源；充值累计只记充值来源，互不污染。
pub fn credit(user: &mut Value, kind: &str, amount: f64) {
    let amt = util::round6(amount.max(0.0));
    let cur_key = if kind == "grant" { "grant" } else { "balance" };
    let total_key = if kind == "grant" { "grant_total" } else { "recharge_total" };
    if let Some(o) = user.as_object_mut() {
        let cur = util::f64_or(o.get(cur_key), 0.0);
        o.insert(cur_key.into(), json!(util::round6(cur + amt)));
        let tot = util::f64_or(o.get(total_key), 0.0);
        o.insert(total_key.into(), json!(util::round6(tot + amt)));
    }
}

/// 扣一笔计费：先扣赠金当前，不够再扣充值当前（round6 量化，避免浮点尾数）。
/// 余额不足时允许扣成负数（记录欠款）：预检在请求入口拦截 balance_of<=0，
/// 并发突发最多产生一段有账可查的欠款，充值后自动冲抵；若在此钳到 0，
/// 欠款消失、记账与实扣脱节（等于每次充值后可无限白嫖到下一次预检）。
/// 返回 (扣的赠金, 扣的充值)。
pub fn debit_bill(user: &mut Value, cost: f64) -> (f64, f64) {
    let mut left = util::round6(cost);
    let mut from_grant = 0.0f64;
    let mut from_recharge = 0.0f64;
    if let Some(o) = user.as_object_mut() {
        let g = util::round6(util::f64_or(o.get("grant"), 0.0));
        if g > 0.0 {
            from_grant = g.min(left);
            o.insert("grant".into(), json!(util::round6(g - from_grant)));
            left = util::round6(left - from_grant);
        }
        if left > 0.0 {
            let r = util::round6(util::f64_or(o.get("balance"), 0.0));
            // 充值腿允许扣成负数（记录欠款；充值后自动冲抵）
            from_recharge = left;
            o.insert("balance".into(), json!(util::round6(r - from_recharge)));
        }
    }
    (from_grant, from_recharge)
}

/// 记一笔资金变动（必须在 store().update() 闭包内调用）。
/// kind: signup/adjust/grant/recharge/sign/draw/prize/call。dg/dr = 赠金/充值增减量。
/// 每用户保留最近 500 条。
pub fn fund_log(db: &mut Value, user_id: &str, kind: &str, dg: f64, dr: f64, note: &str) {
    if dg == 0.0 && dr == 0.0 {
        return;
    }
    let obj = match db.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let arr = obj.entry("fund_logs").or_insert_with(|| Value::Array(vec![]));
    // +0.0 归一化负零：round6(0.0) 在扣减路径可能产生 -0.0，序列化后前端显示 "-0.0000"
    let dg = util::round6(dg) + 0.0;
    let dr = util::round6(dr) + 0.0;
    if let Some(a) = arr.as_array_mut() {
        a.insert(
            0,
            json!({
                "t": util::now_i(),
                "user_id": user_id,
                "kind": kind,
                "dg": dg,
                "dr": dr,
                "note": util::str_cut(note, 60),
            }),
        );
        let mut seen = 0usize;
        a.retain(|r| {
            if util::str_or(r.get("user_id"), "") != user_id {
                return true;
            }
            seen += 1;
            seen <= 500
        });
    }
}

/// 用户资金变动流水（时间倒序，分页）。
pub fn user_funds_page(user_id: &str, page: i64, per: usize) -> (Vec<Value>, i64) {
    let db = store().load();
    let mut rows: Vec<Value> = db
        .get("fund_logs")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| util::str_or(r.get("user_id"), "") == user_id)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let total = rows.len() as i64;
    let per = per.max(1) as i64;
    let pages = ((total + per - 1) / per).max(1);
    let page = page.clamp(1, pages);
    let start = ((page - 1) * per) as usize;
    rows = rows.into_iter().skip(start).take(per as usize).collect();
    (rows, total)
}

/// 用户抽奖记录（分页）。
pub fn user_draws_page(user_id: &str, page: i64, per: usize) -> (Vec<Value>, i64) {
    let db = store().load();
    let mut rows: Vec<Value> = db
        .get("draw_logs")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| util::str_or(r.get("user_id"), "") == user_id)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let total = rows.len() as i64;
    let per = per.max(1) as i64;
    let pages = ((total + per - 1) / per).max(1);
    let page = page.clamp(1, pages);
    let start = ((page - 1) * per) as usize;
    rows = rows.into_iter().skip(start).take(per as usize).collect();
    (rows, total)
}

/// 迁移旧用户行：补 grant/grant_total/recharge_total 字段（老账本 balance 全额划入充值账本，
/// 当前与累计一致；grant 缺失即视为从未有过赠金）。在 list_users 读路径上做，幂等零迁移成本。
fn ensure_money_fields(u: &mut Value) {
    if let Some(o) = u.as_object_mut() {
        let has_grant = o.contains_key("grant");
        let balance = util::round6(util::f64_or(o.get("balance"), 0.0));
        let grant = util::round6(util::f64_or(o.get("grant"), 0.0));
        let recharge = util::round6(util::f64_or(o.get("balance"), 0.0));
        if !has_grant {
            o.insert("grant".into(), json!(0.0));
            o.insert("grant_total".into(), json!(0.0));
            o.insert("recharge_total".into(), json!(balance.max(0.0)));
        } else if !o.contains_key("grant_total") {
            o.insert("grant_total".into(), json!(grant));
        }
        if !o.contains_key("recharge_total") {
            o.insert("recharge_total".into(), json!(recharge.max(0.0)));
        }
    }
}

// ---------------------------------------------------------------- 签到

/// 签到记录存储：data/db/signs/ 下每天一个文件 sign_YYYY-MM-DD.json（按天分片，
/// 只存当天记录；过期文件可整删）。注意：仅允许在 store().update() 闭包内调用。
pub fn sign_store_set(day: &str, user_id: &str, amount: f64) -> bool {
    let dir = crate::store::signs_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let f = dir.join(format!("sign_{}.json", day));
    let mut doc: Value = std::fs::read_to_string(&f)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(o) = doc.as_object_mut() {
        o.insert(user_id.to_string(), json!(util::round6(amount)));
    }
    // 写盘失败（磁盘满/权限）必须让调用方放弃入账：否则签到记录缺失，
    // 用户可以反复签到重复领奖
    let tmp = f.with_extension("json.tmp");
    match std::fs::write(&tmp, serde_json::to_string(&doc).unwrap_or_default()) {
        Ok(()) => std::fs::rename(&tmp, &f).is_ok(),
        Err(_) => false,
    }
}

pub fn sign_store_get(day: &str, user_id: &str) -> Option<f64> {
    let f = crate::store::signs_dir().join(format!("sign_{}.json", day));
    let doc: Value = std::fs::read_to_string(&f).ok().and_then(|s| serde_json::from_str(&s).ok())?;
    doc.get(user_id).and_then(|v| v.as_f64())
}

/// 签到设置（config）：sign_enabled 默认关；sign_min/sign_max 默认 0.001/0.01。
pub fn sign_config(cfg: &Value) -> (bool, f64, f64) {
    let enabled = util::truthy(cfg.get("sign_enabled").unwrap_or(&Value::Bool(false)));
    let min = util::f64_or(cfg.get("sign_min"), 0.001).clamp(0.000001, 1000.0);
    let max = util::f64_or(cfg.get("sign_max"), 0.01).clamp(min, 1000.0);
    (enabled, min, max)
}

/// 用户签到：一天一次（按网关本地日），金额 = min..max 随机（6 位小数量化）入赠金账本。
/// 返回 (ok, 消息, 金额)。
pub fn sign_today(uid: &str) -> (bool, String, f64) {
    let cfg_v = store().load();
    let cfg = cfg_v.get("config").cloned().unwrap_or(json!({}));
    let (enabled, min, max) = sign_config(&cfg);
    if !enabled {
        return (false, "签到未开启".into(), 0.0);
    }
    if get_user_by_id(uid).is_none() {
        return (false, "用户不存在".into(), 0.0);
    }
    let day = crate::util::local_day(crate::util::now_i());
    let span = (max - min).max(0.0);
    let raw = min + rand::Rng::gen::<f64>(&mut rand::thread_rng()) * span;
    let amount = util::round6(raw).clamp(0.000001, 1000.0);
    // 查重、入账、落签到记录必须在同一个串行化临界区（store 互斥锁）里完成：
    // 签到记录是独立文件，若在锁外查重/落盘，并发请求可重复领奖（先查后写的竞态）
    let mut already = false;
    let mut credited = false;
    store().update(|db| {
        if sign_store_get(&day, uid).is_some() {
            already = true;
            return;
        }
        let mut ok = false;
        users_mut(db, |arr| {
            if let Some(u) = find_user_mut(arr, uid) {
                credit(u, "grant", amount);
                ok = true;
            }
        });
        // 先落签到记录（写盘失败 → 不入账不放流水，用户可重试；
        // 记录成功而 credit 只在 memo 的情况：崩溃丢失一次签到额，可接受）
        if ok && sign_store_set(&day, uid, amount) {
            fund_log(db, uid, "sign", amount, 0.0, "每日签到");
            credited = true;
        }
    });
    if !credited {
        let msg = if already { "今天已经签到过了" } else { "用户不存在" };
        return (false, msg.to_string(), 0.0);
    }
    (true, "签到成功".into(), amount)
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
/// 日志按付费/免费分表：付费保留 user_log_paid_max（默认 250）条，免费保留
/// user_log_free_max（默认 50）条，均为「每个用户」各自的保留条数。
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
    let (arr_key, cap, legacy_caps) = {
        let cfg = store().load();
        let empty = json!({});
        let c = cfg.get("config").unwrap_or(&empty);
        let paid_cap = util::cfg_int(c, "user_log_paid_max", 250).clamp(0, 5000) as usize;
        let free_cap = util::cfg_int(c, "user_log_free_max", 50).clamp(0, 5000) as usize;
        if cost > 0.0 {
            ("user_logs_paid", paid_cap, (paid_cap, free_cap))
        } else {
            ("user_logs_free", free_cap, (paid_cap, free_cap))
        }
    };
    store().update(|db| {
        // 旧版混存表（单数组、全局上限）拆分为付费/免费两张表（caps 已在锁外读取）
        split_legacy_user_logs(db, legacy_caps);
        // 日志：插入该用户的分表，并只把「该用户」的条数截到保留上限
        {
            let obj = db.as_object_mut().unwrap();
            let logs = obj.entry(arr_key).or_insert_with(|| Value::Array(vec![]));
            if let Some(a) = logs.as_array_mut() {
                a.insert(0, row);
                let mut seen = 0usize;
                a.retain(|r| {
                    if util::str_or(r.get("user_id"), "") != user_id {
                        return true;
                    }
                    seen += 1;
                    seen <= cap
                });
            }
        }
        // 扣费（仅付费模型）：先扣赠金，不足部分扣充值
        // 累计计数器直接加在用户行上（free_calls/paid_calls/total_cost）：
        // 日志表有保留上限会被裁剪，对日志求和不是真累计
        {
            let mut paid_split = (0.0f64, 0.0f64);
            if let Some(arr) = db.get_mut("users").and_then(|u| u.as_array_mut()) {
                for u in arr.iter_mut() {
                    if util::str_or(u.get("id"), "") != user_id {
                        continue;
                    }
                    if cost > 0.0 {
                        paid_split = debit_bill(u, cost);
                    }
                    if let Some(o) = u.as_object_mut() {
                        let kind_key = if cost > 0.0 { "paid_calls" } else { "free_calls" };
                        let n = util::int_or(o.get(kind_key), 0);
                        o.insert(kind_key.into(), json!(n + 1));
                        let tc = util::f64_or(o.get("total_cost"), 0.0);
                        o.insert("total_cost".into(), json!(util::round6(tc + cost)));
                    }
                    break;
                }
            }
            if cost > 0.0 {
                fund_log(
                    db,
                    user_id,
                    "call",
                    -util::round6(paid_split.0),
                    -util::round6(paid_split.1),
                    model,
                );
            }
        }
    });
    cost
}

/// 旧版混存日志表（user_logs 单数组）按 cost>0 拆到付费/免费两张表；拆完移除旧键。
/// 截断口径与 bill() 一致读配置上限（写死数值会在管理员调高上限时不可逆丢行）。
/// caps 由调用方传入：本函数只在 store().update() 闭包内运行，内部再调 store().load()
/// 会自锁（Mutex 不可重入，实测整实例挂死）。
fn split_legacy_user_logs(db: &mut Value, caps: (usize, usize)) {
    let Some(obj) = db.as_object_mut() else { return };
    let Some(legacy) = obj.remove("user_logs") else { return };
    let arr = legacy.as_array().cloned().unwrap_or_default();
    let (paid_cap, free_cap) = caps;
    let mut paid: Vec<Value> = Vec::new();
    let mut free: Vec<Value> = Vec::new();
    for r in arr {
        if util::f64_or(r.get("cost"), 0.0) > 0.0 {
            paid.push(r);
        } else {
            free.push(r);
        }
    }
    // 旧表本就时间倒序，截断即保留最近
    paid.truncate(paid_cap);
    free.truncate(free_cap);
    obj.entry("user_logs_paid")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .map(|a| {
            let mut merged = paid;
            merged.extend(a.iter().cloned());
            merged.sort_by_key(|r| -util::int_or(r.get("t"), 0));
            *a = merged;
        });
    obj.entry("user_logs_free")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .map(|a| {
            let mut merged = free;
            merged.extend(a.iter().cloned());
            merged.sort_by_key(|r| -util::int_or(r.get("t"), 0));
            *a = merged;
        });
}

/// 启动期数据迁移（幂等，写盘一次）：
/// 1) 老用户行补账本字段：老 balance 全额划入充值账本（当前=累计），赠金从 0 起；
/// 2) 补累计计数器 free_calls/paid_calls/total_cost，并从现存日志表回填（日志有保留上限，
///    回填是能拿到的历史下限，之后由 bill() 精确累加），保证「累计消费/累计调用」不因
///    日志裁剪而回退；3) 归一化 count 字段类型。
pub fn migrate_user_fields_once() {
    let needs = {
        let db = store().load();
        db.get("users")
            .and_then(|u| u.as_array())
            .map(|a| {
                a.iter().any(|u| {
                    !u.as_object().map(|o| o.contains_key("grant")).unwrap_or(false)
                        || !u.as_object().map(|o| o.contains_key("total_cost")).unwrap_or(false)
                        || !u.as_object().map(|o| o.contains_key("free_calls")).unwrap_or(false)
                })
            })
            .unwrap_or(false)
    };
    if !needs {
        return;
    }
    // 回填来源：两张日志表（锁外先取快照，避免闭包内 load 自锁）
    let db = store().load();
    let mut paid_rows: Vec<(String, f64, i64)> = Vec::new();
    for key in ["user_logs_paid", "user_logs_free"] {
        if let Some(a) = db.get(key).and_then(|l| l.as_array()) {
            for r in a {
                paid_rows.push((
                    util::str_or(r.get("user_id"), ""),
                    util::f64_or(r.get("cost"), 0.0),
                    if key == "user_logs_paid" { 1 } else { 0 },
                ));
            }
        }
    }
    store().update(|db| {
        let Some(arr) = db.get_mut("users").and_then(|u| u.as_array_mut()) else { return };
        for u in arr.iter_mut() {
            let Some(o) = u.as_object_mut() else { continue };
            let balance = util::round6(util::f64_or(o.get("balance"), 0.0)).max(0.0);
            if !o.contains_key("grant") {
                o.insert("grant".into(), json!(0.0));
            }
            if !o.contains_key("grant_total") {
                let g = util::round6(util::f64_or(o.get("grant"), 0.0));
                o.insert("grant_total".into(), json!(g));
            }
            if !o.contains_key("recharge_total") {
                o.insert("recharge_total".into(), json!(balance));
            }
            let uid = util::str_or(o.get("id"), "");
            let need_calls = !o.contains_key("free_calls") || !o.contains_key("paid_calls");
            let need_cost = !o.contains_key("total_cost");
            if need_calls || need_cost {
                let back_paid = paid_rows.iter().filter(|(u, _, k)| *u == uid && *k == 1).count() as i64;
                let back_free = paid_rows.iter().filter(|(u, _, k)| *u == uid && *k == 0).count() as i64;
                let back_cost: f64 = paid_rows.iter().filter(|(u, _, _)| *u == uid).map(|(_, c, _)| *c).sum();
                if !o.contains_key("paid_calls") {
                    o.insert("paid_calls".into(), json!(back_paid));
                }
                if !o.contains_key("free_calls") {
                    o.insert("free_calls".into(), json!(back_free));
                }
                if need_cost {
                    o.insert("total_cost".into(), json!(util::round6(back_cost)));
                }
            }
        }
    });
    store().flush();
}

/// 旧混存表若还在磁盘上，启动时立即拆一次：否则概览统计/日志计数
/// （只读分表）在用户第一次点开日志页前会一直显示 0。函数本身幂等。
pub fn migrate_legacy_logs_once() {
    let has_legacy = store()
        .load()
        .get("user_logs")
        .and_then(|l| l.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if has_legacy {
        let caps = legacy_log_caps();
        store().update(|db| split_legacy_user_logs(db, caps));
    }
}

/// 两个日志保留上限（锁外读取配置用）：update 闭包内禁止再 load()。
fn legacy_log_caps() -> (usize, usize) {
    let cfg = store().load();
    let empty = json!({});
    let c = cfg.get("config").unwrap_or(&empty);
    (
        util::cfg_int(c, "user_log_paid_max", 250).clamp(0, 5000) as usize,
        util::cfg_int(c, "user_log_free_max", 50).clamp(0, 5000) as usize,
    )
}

/// 用户调用日志（分页）。kind: "paid" / "free" / "all"（合并按时间倒序）。
/// 返回 (当前页行, 总条数)。每页 per 条，page 越界自动收敛到最后一页。
pub fn user_logs_page(user_id: &str, kind: &str, page: i64, per: usize) -> (Vec<Value>, i64) {
    {
        // 旧版混存表迁移（存在才写一次）
        let has_legacy = store().load().get("user_logs").and_then(|l| l.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
        if has_legacy {
            let caps = legacy_log_caps();
            store().update(|db| split_legacy_user_logs(db, caps));
        }
    }
    let db = store().load();
    let collect = |key: &str| -> Vec<Value> {
        db.get(key)
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter(|r| util::str_or(r.get("user_id"), "") == user_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut rows = match kind {
        "paid" => collect("user_logs_paid"),
        "free" => collect("user_logs_free"),
        _ => {
            let mut all = collect("user_logs_paid");
            all.extend(collect("user_logs_free"));
            all.sort_by_key(|r| -util::int_or(r.get("t"), 0));
            all
        }
    };
    let total = rows.len() as i64;
    let per = per.max(1) as i64;
    let pages = ((total + per - 1) / per).max(1);
    let page = page.clamp(1, pages);
    let start = ((page - 1) * per) as usize;
    rows = rows.into_iter().skip(start).take(per as usize).collect();
    (rows, total)
}

/// 该用户日志总条数（概览/统计用）：付费 + 免费。
pub fn user_logs_count(user_id: &str) -> i64 {
    let db = store().load();
    ["user_logs_paid", "user_logs_free"]
        .iter()
        .map(|k| {
            db.get(*k)
                .and_then(|l| l.as_array())
                .map(|a| a.iter().filter(|r| util::str_or(r.get("user_id"), "") == user_id).count() as i64)
                .unwrap_or(0)
        })
        .sum()
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

