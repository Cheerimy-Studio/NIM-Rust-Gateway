//! 账号池（core/pool.py 的移植）：CSV 解析导入、多维限速调度、分级冷却、熔断、最近记录。

use crate::store::{store, Obj, Value};
use crate::util;
use crate::{queue, upstreams};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

// ---------------------------------------------------------------- 在途计数

static INFLIGHT: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
static ODD_RELEASE: AtomicI64 = AtomicI64::new(0);

fn with_inflight<R>(f: impl FnOnce(&mut HashMap<String, i64>) -> R) -> R {
    let mut g = INFLIGHT.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

pub fn inc_inflight(key_id: &str) {
    with_inflight(|m| *m.entry(key_id.to_string()).or_insert(0) += 1);
}

/// 在途 -1，返回剩余在途。计数不允许为负。
pub fn dec_inflight(key_id: &str) -> i64 {
    if key_id.is_empty() {
        // 「取不到账号」的路径也会调 release("", ...)：那是写失败日志，不是释放
        return 0;
    }
    with_inflight(|m| {
        let c = m.get(key_id).copied().unwrap_or(0);
        if c <= 0 {
            m.remove(key_id);
            ODD_RELEASE.fetch_add(1, Ordering::SeqCst);
            return 0;
        }
        let c = c - 1;
        if c == 0 {
            m.remove(key_id);
        } else {
            m.insert(key_id.to_string(), c);
        }
        c
    })
}

pub fn inflight_of(key_id: &str) -> i64 {
    with_inflight(|m| m.get(key_id).copied().unwrap_or(0))
}

pub fn inflight_total() -> i64 {
    with_inflight(|m| m.values().sum())
}

pub fn inflight_snapshot() -> HashMap<String, i64> {
    with_inflight(|m| m.clone())
}

pub fn inflight_odd_releases() -> i64 {
    ODD_RELEASE.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------- 错误分级

const MODEL_NEG_NOT_FOUND: &[&str] = &[
    "not found",
    "does not exist",
    "不存在",
    "no such",
    "已关闭",
    "已下线",
];

const MODEL_STRONG: &[&str] = &[
    "model_not_found",
    "model not found",
    "model_disabled",
    "model disabled",
    "模型已关闭",
    "模型不存在",
    "not available on any",
];

pub fn classify(http_status: i64, errno: i64, error: &str) -> &'static str {
    let low = error.to_lowercase();
    if low.contains("pooltimeout") || low.contains("连接池耗尽") {
        return "pool_exhausted";
    }
    if ["no available channel", "no channel available", "无可用渠道", "channel_exhausted"]
        .iter()
        .any(|k| low.contains(k))
    {
        return "channel";
    }
    if (http_status == 400 || http_status == 404)
        && (low.contains("model") || low.contains("模型"))
        && MODEL_NEG_NOT_FOUND.iter().any(|k| low.contains(k))
    {
        return "model";
    }
    if http_status >= 400 && MODEL_STRONG.iter().any(|k| low.contains(k)) {
        return "model";
    }
    if http_status == 429 {
        return "429";
    }
    if http_status == 402 {
        return "payment";
    }
    if http_status == 401 || http_status == 403 {
        return "auth";
    }
    if errno == 28 || low.contains("timed out") || low.contains("timeout") {
        return "timeout";
    }
    if [6, 7, 35, 52, 56].contains(&errno) || http_status == 0 {
        return "conn";
    }
    if http_status >= 500 {
        return "5xx";
    }
    "req"
}

// ---------------------------------------------------------------- CSV 解析与导入

/// 单行 CSV 拆分（Python csv.reader 默认语义：逗号分隔、双引号转义）。
fn csv_row(line: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                ',' => {
                    fields.push(std::mem::take(&mut cur));
                }
                '"' if cur.is_empty() => in_quotes = true,
                _ => cur.push(c),
            }
        }
    }
    fields.push(cur);
    fields
}

pub fn parse_accounts(text: &str) -> (Vec<Value>, i64) {
    parse_rows(text, false)
}

pub fn parse_accounts_loose(text: &str) -> (Vec<Value>, i64) {
    parse_rows(text, true)
}

fn parse_rows(text: &str, loose: bool) -> (Vec<Value>, i64) {
    let mut accounts: Vec<Value> = Vec::new();
    let mut invalid: i64 = 0;
    let mut n = 0;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        if n > 20000 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        n += 1;
        let low = line.to_lowercase();
        if low.contains("apikey") && low.contains("email") {
            continue;
        }
        if (line.starts_with("email")
            || line.starts_with("邮箱")
            || line.starts_with("账号")
            || line.starts_with("user")
            || line.starts_with("用户名"))
            && !low.contains("nvapi-")
        {
            continue;
        }
        let account = if loose {
            parse_line_loose(line)
        } else {
            let mut cells: Vec<String> = csv_row(line).iter().map(|c| c.trim().to_string()).collect();
            while cells.last().map(|c| c.is_empty()).unwrap_or(false) {
                cells.pop();
            }
            from_cells(&cells)
        };
        match account {
            Some(a) => accounts.push(a),
            None => invalid += 1,
        }
    }
    (accounts, invalid)
}

fn parse_line_loose(line: &str) -> Option<Value> {
    let mut cells: Vec<String> = Vec::new();
    if line.contains('"') {
        cells = csv_row(line).iter().map(|c| c.trim().to_string()).collect();
        if cells.iter().filter(|c| !c.is_empty()).count() < 2 {
            cells = Vec::new();
        }
    }
    if cells.is_empty() {
        cells = line
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '|')
            .filter(|p| !p.is_empty())
            .map(|c| c.trim().trim_matches('"').to_string())
            .collect();
    }
    let mut api_key = String::new();
    let mut email = String::new();
    for c in &cells {
        if c.is_empty() {
            continue;
        }
        if api_key.is_empty() && c.to_lowercase().starts_with("nvapi-") {
            api_key = c.clone();
            continue;
        }
        if email.is_empty() && c.contains('@') {
            let domain = c.split('@').last().unwrap_or("");
            if domain.contains('.') && !c.contains(' ') {
                email = c.clone();
                continue;
            }
        }
    }
    if api_key.is_empty() {
        let non_empty: Vec<&String> = cells.iter().filter(|c| !c.is_empty()).collect();
        if non_empty.len() >= 2 {
            let first = non_empty[0];
            let domain = first.split('@').last().unwrap_or("");
            if first.contains('@') && domain.contains('.') {
                email = first.clone();
                api_key = non_empty[non_empty.len() - 1].clone();
            }
        } else if non_empty.len() == 1 {
            let only = non_empty[0];
            if only.chars().count() >= 16 && !only.contains('@') {
                api_key = only.clone();
            }
        }
    }
    if api_key.is_empty() || api_key.chars().count() < 8 {
        return None;
    }
    let mut password = String::new();
    for c in &cells {
        if c.is_empty() || *c == api_key || (!email.is_empty() && c.to_lowercase() == email.to_lowercase()) {
            continue;
        }
        password = c.clone();
        break;
    }
    let email_final = if email.is_empty() {
        let mut h = Md5::new();
        h.update(api_key.as_bytes());
        let d = h.finalize();
        let hex: String = d.iter().map(|x| format!("{:02x}", x)).collect();
        format!("unknown-{}", &hex[..8])
    } else {
        email
    };
    Some(serde_json::json!({
        "email": email_final,
        "password": password,
        "apikey": api_key,
    }))
}

fn from_cells(cells: &[String]) -> Option<Value> {
    if cells.len() != 3 {
        return None;
    }
    let (email, password, api_key) = (&cells[0], &cells[1], &cells[2]);
    let domain = email.split('@').last().unwrap_or("");
    if !email.contains('@') || !domain.contains('.') || api_key.chars().count() < 8 {
        return None;
    }
    Some(serde_json::json!({
        "email": email,
        "password": password,
        "apikey": api_key,
    }))
}

pub fn new_key(email: &str, password: &str, apikey: &str, upstream_id: &str) -> Value {
    let now = util::now_i();
    serde_json::json!({
        "id": format!("k_{}", util::rand_hex(5)),
        "email": email,
        "password": password,
        "apikey": apikey,
        "upstream_id": upstream_id,
        "enabled": true,
        "status": "active",
        "created_at": now,
        "updated_at": now,
        "last_used_at": 0,
        "first_seen_at": 0,
        "total_requests": 0,
        "total_success": 0,
        "total_fail": 0,
        "consecutive_failures": 0,
        "hard_fail_count": 0,
        "rl_streak": 0,
        "banned_until": 0,
        "ban_reason": "",
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "last_error": "",
        "last_error_at": 0,
        "daily": {},
        "minute_tokens": {},
        "hour_requests": {},
        "recent": [],
    })
}

pub fn import_accounts(text: &str, upstream_id: &str, loose_text: &str) -> Value {
    let (mut accounts, mut invalid) = parse_accounts(text);
    if !loose_text.is_empty() {
        let (l_accounts, l_invalid) = parse_accounts_loose(loose_text);
        let mut merged = l_accounts;
        merged.extend(accounts);
        accounts = merged;
        invalid += l_invalid;
    }
    let lines_count = format!("{}\n{}", text, loose_text)
        .replace('\r', "")
        .split('\n')
        .filter(|x| !x.trim().is_empty())
        .count() as i64;
    let mut res = serde_json::json!({
        "added": 0, "updated": 0, "duplicate": 0,
        "invalid": invalid, "lines": lines_count, "total": 0,
    });
    if accounts.is_empty() {
        return res;
    }
    let mut added_n: i64 = 0;
    let mut updated_n: i64 = 0;
    let mut duplicate_n: i64 = 0;
    let mut total_n: i64 = 0;
    store().update(|db| {
        let keys = db
            .as_object_mut()
            .map(|o| o.entry("keys").or_insert_with(|| Value::Array(vec![])))
            .unwrap();
        let arr = match keys.as_array_mut() {
            Some(a) => a,
            None => return,
        };
        let mut by_email: HashMap<String, usize> = arr
            .iter()
            .enumerate()
            .map(|(i, k)| (util::str_or(k.get("email"), "").to_lowercase(), i))
            .collect();
        let mut by_key: HashMap<String, usize> = arr
            .iter()
            .enumerate()
            .map(|(i, k)| (util::str_or(k.get("apikey"), ""), i))
            .collect();
        for a in &accounts {
            let ek = util::str_or(a.get("email"), "").to_lowercase();
            let apikey = util::str_or(a.get("apikey"), "");
            if by_key.contains_key(&apikey) {
                duplicate_n += 1;
                continue;
            }
            if let Some(&i) = by_email.get(&ek) {
                let mut changed = false;
                let row = &mut arr[i];
                if util::str_or(row.get("apikey"), "") != apikey {
                    if let Some(o) = row.as_object_mut() {
                        o.insert("apikey".into(), Value::from(apikey.clone()));
                        o.insert("upstream_id".into(), Value::from(upstream_id.to_string()));
                    }
                    changed = true;
                }
                let pw = util::str_or(a.get("password"), "");
                if !pw.is_empty() && util::str_or(row.get("password"), "") != pw {
                    if let Some(o) = row.as_object_mut() {
                        o.insert("password".into(), Value::from(pw));
                    }
                    changed = true;
                }
                if changed {
                    if let Some(o) = row.as_object_mut() {
                        o.insert("updated_at".into(), Value::from(util::now_i()));
                    }
                    updated_n += 1;
                } else {
                    duplicate_n += 1;
                }
                continue;
            }
            let nk = new_key(
                &util::str_or(a.get("email"), ""),
                &util::str_or(a.get("password"), ""),
                &apikey,
                upstream_id,
            );
            arr.push(nk);
            by_email.insert(ek, arr.len() - 1);
            by_key.insert(apikey, arr.len() - 1);
            added_n += 1;
        }
        total_n = arr.len() as i64;
    });
    res["added"] = Value::from(added_n);
    res["updated"] = Value::from(updated_n);
    res["duplicate"] = Value::from(duplicate_n);
    res["total"] = Value::from(total_n);
    res
}

// ---------------------------------------------------------------- 调度

fn fail_ratio(k: &Value) -> f64 {
    let tr = util::int_or(k.get("total_requests"), 0);
    if tr == 0 {
        0.0
    } else {
        util::int_or(k.get("total_fail"), 0) as f64 / tr as f64
    }
}

/// 池内择优排序：第一优先级是「最久未用」（LRU），失败数/失败率只作次级判据。
fn compare_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    // last_used_at 必须按浮点秒比较：LRU 轮换发生在亚秒级，截断成整秒会让
    // 同一秒内的时间戳全部相等、退化为按 id 决胜，锁死同一个账号
    let (la, ca, fa, ia) = (
        util::f64_or(a.get("last_used_at"), 0.0),
        util::int_or(a.get("consecutive_failures"), 0),
        fail_ratio(a),
        util::str_or(a.get("id"), ""),
    );
    let (lb, cb, fb, ib) = (
        util::f64_or(b.get("last_used_at"), 0.0),
        util::int_or(b.get("consecutive_failures"), 0),
        fail_ratio(b),
        util::str_or(b.get("id"), ""),
    );
    la.partial_cmp(&lb)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(ca.cmp(&cb))
        .then(fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal))
        .then(ia.cmp(&ib))
}

pub fn model_key(uid: &str, model: &str) -> String {
    format!("{}\u{0}{}", uid, model)
}

const MODEL_RECENT_MIN: usize = 3;
const MODEL_MISSING_MAX: usize = 2000;

fn model_missing_ttl(cfg: &Value) -> i64 {
    match cfg.get("model_missing_ttl") {
        None | Some(Value::Null) => 3600,
        Some(v) => util::py_int(v).unwrap_or(3600).max(0),
    }
}

pub fn model_missing_fresh(db: &Value, uid: &str, model: &str) -> bool {
    if uid.is_empty() || model.is_empty() {
        return false;
    }
    if model_missing_ttl(db.get("config").unwrap_or(&Value::Null)) <= 0 {
        return false;
    }
    let ent = db
        .get("model_missing")
        .and_then(|x| x.get(uid))
        .and_then(|x| x.as_object());
    match ent {
        Some(e) => match e.get(model) {
            Some(v) => util::f64_or(Some(v), 0.0) > util::now_f(),
            None => false,
        },
        None => false,
    }
}

pub fn mark_model_missing(db: &mut Value, uid: &str, model: &str) {
    if uid.is_empty() || model.is_empty() {
        return;
    }
    let ttl = model_missing_ttl(db.get("config").unwrap_or(&Value::Null));
    if ttl <= 0 {
        return;
    }
    let now = util::now_f();
    let Some(obj) = db.as_object_mut() else { return };
    let store_val = obj
        .entry("model_missing")
        .or_insert_with(|| Value::Object(Obj::new()));
    if !store_val.is_object() {
        *store_val = Value::Object(Obj::new());
    }
    let sm = store_val.as_object_mut().unwrap();
    let ent = sm
        .entry(uid.to_string())
        .or_insert_with(|| Value::Object(Obj::new()));
    if !ent.is_object() {
        *ent = Value::Object(Obj::new());
    }
    let em = ent.as_object_mut().unwrap();
    em.insert(util::str_cut(model, 80), Value::from(now + ttl as f64));
    let expired: Vec<String> = em
        .iter()
        .filter(|(_, v)| util::f64_or(Some(v), 0.0) <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for k in expired {
        em.remove(&k);
    }
    let total: usize = sm.values().filter_map(|v| v.as_object()).map(|o| o.len()).sum();
    if total > MODEL_MISSING_MAX {
        let drop_keys: Vec<String> = sm
            .keys()
            .filter(|k| k.as_str() != uid)
            .cloned()
            .collect();
        for k in drop_keys {
            sm.remove(&k);
        }
    }
}

/// 「结构上」能服务该模型的渠道集合（按账号配置顺序去重，保证后续排序的平分确定性）。
pub fn capable_channels(db: &Value, model: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let cfg = db.get("config").cloned().unwrap_or(Value::Null);
    let global_hide = cfg.get("hide_mapped_names").map(util::truthy).unwrap_or(true);
    let ups: HashMap<String, &Value> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|u| u.is_object())
                .map(|u| (util::str_or(u.get("id"), ""), u))
                .collect()
        })
        .unwrap_or_default();
    let keys = match db.get("keys").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return out,
    };
    for k in keys {
        if !k.get("enabled").map(util::truthy).unwrap_or(false) {
            continue;
        }
        let uid = util::str_or(k.get("upstream_id"), "");
        let up = ups.get(&uid).copied();
        if let Some(u) = up {
            if !u.get("enabled").map(util::truthy).unwrap_or(false) {
                continue;
            }
        }
        if !model.is_empty() {
            if let Some(u) = up {
                if upstreams::hidden_target(u, model, global_hide) {
                    continue;
                }
                let models = u.get("models").and_then(|m| m.as_array());
                if let Some(ms) = models {
                    if !ms.is_empty() && !ms.iter().any(|m| util::str_or(Some(m), "") == model) {
                        continue;
                    }
                }
            }
        }
        if !model.is_empty() && model_missing_fresh(db, &uid, model) {
            continue;
        }
        if !out.contains(&uid) {
            out.push(uid);
        }
    }
    out
}

/// 预测这次请求会被调度到哪个渠道（只读预测，不取号）。
pub fn resolve_channel(db: &Value, model: &str) -> String {
    let cand = capable_channels(db, model);
    if cand.is_empty() {
        return String::new();
    }
    let recent = db.get("up_recent").cloned().unwrap_or(Value::Null);
    let mut weights: HashMap<String, i64> = HashMap::new();
    if let Some(ups) = db.get("upstreams").and_then(|x| x.as_array()) {
        for u in ups {
            if u.is_object() {
                weights.insert(
                    util::str_or(u.get("id"), ""),
                    util::int_or(u.get("weight"), 10).max(1),
                );
            }
        }
    }
    let mut scored: Vec<(String, f64, i64)> = Vec::new();
    for pid in cand.iter() {
        let rec_m = recent
            .get(&model_key(pid, model))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let rec = if rec_m.len() >= MODEL_RECENT_MIN {
            rec_m
        } else {
            recent
                .get(pid)
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
        };
        let ok_n = rec.iter().filter(|v| v.as_i64().unwrap_or(0) != 0).count();
        let score = (ok_n as f64 + 5.0) / (rec.len() as f64 + 10.0);
        scored.push((pid.clone(), score, *weights.get(pid).unwrap_or(&10)));
    }
    // (score, weight) 降序，稳定排序保持原序
    scored.sort_by(|a, b| {
        (b.1, b.2)
            .partial_cmp(&(a.1, a.2))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.first().map(|x| x.0.clone()).unwrap_or_default()
}

fn apply_ban(k: &mut Value, until: i64, reason: &str) {
    let cur = util::int_or(k.get("banned_until"), 0);
    if until > cur {
        if let Some(o) = k.as_object_mut() {
            o.insert("banned_until".into(), Value::from(until));
            o.insert("ban_reason".into(), Value::from(reason));
        }
    }
    if reason == "invalid_key" {
        if let Some(o) = k.as_object_mut() {
            o.insert("status".into(), Value::from("invalid"));
        }
    }
}

fn replace_key(db: &mut Value, k: &Value) {
    let kid = util::str_or(k.get("id"), "");
    if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
        for row in keys.iter_mut() {
            if util::str_or(row.get("id"), "") == kid {
                *row = k.clone();
                return;
            }
        }
    }
}

pub struct AcquireOut {
    pub result: String,
    pub key: Option<Value>,
    pub reason: String,
    pub total: i64,
    pub permanent: bool,
    pub wait_hint: f64,
}

impl AcquireOut {
    fn new() -> Self {
        AcquireOut {
            result: "none".into(),
            key: None,
            reason: String::new(),
            total: 0,
            permanent: false,
            wait_hint: 0.0,
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "result": self.result,
            "key": self.key.clone(),
            "reason": self.reason,
            "total": self.total,
            "permanent": self.permanent,
            "wait_hint": self.wait_hint,
        })
    }
}

/// 取号：在 STORE 锁内执行调度逻辑。
pub fn acquire(est_tokens: i64, model: &str) -> AcquireOut {
    let mut out = AcquireOut::new();
    store().update(|db| acquire_fn(db, &mut out, est_tokens, model));
    out
}

const R_BANNED: usize = 0;
const R_COOLDOWN: usize = 1;
const R_RPM: usize = 2;
const R_TPM: usize = 3;
const R_DAILY: usize = 4;
const R_POOL: usize = 5;
const R_POOL_RPM: usize = 6;
const R_POOL_DAILY: usize = 7;
const R_CHANNEL_MODEL: usize = 8;
const R_MODEL_HIDDEN: usize = 9;
const R_MODEL_MISSING: usize = 10;
const R_ACCT_CONC: usize = 11;
const R_CHAN_CONC: usize = 12;

fn reason_text(r: &[i64; 13], total: i64) -> String {
    if total == 0 {
        return "密钥池为空".into();
    }
    let labels = [
        ("封禁", R_BANNED),
        ("冷却", R_COOLDOWN),
        ("RPM", R_RPM),
        ("TPM", R_TPM),
        ("日限", R_DAILY),
        ("上游停用", R_POOL),
        ("上游RPM", R_POOL_RPM),
        ("上游日限", R_POOL_DAILY),
        ("渠道模型", R_CHANNEL_MODEL),
        ("原名禁用", R_MODEL_HIDDEN),
        ("模型不存在", R_MODEL_MISSING),
        ("账户并发", R_ACCT_CONC),
        ("渠道并发", R_CHAN_CONC),
    ];
    let parts: Vec<String> = labels
        .iter()
        .filter(|(_, idx)| r[*idx] > 0)
        .map(|(label, idx)| format!("{} {}", label, r[*idx]))
        .collect();
    let joined = if parts.is_empty() {
        "无匹配".to_string()
    } else {
        parts.join(" · ")
    };
    format!("{} / 共 {}", joined, total)
}

pub fn acquire_fn(db: &mut Value, out: &mut AcquireOut, est_tokens: i64, model: &str) {
    let cfg = db.get("config").cloned().unwrap_or(Value::Object(Obj::new()));
    let now_f = util::now_f();
    let now = now_f as i64;
    let minute = util::utc_minute(now);
    let hour = util::utc_hour(now);
    let day = util::local_day(now);

    // 全局限额读取：0 或 -1 = 不限（返回 0），字段缺失才用默认值。
    let gnum = |name: &str, default: i64| -> i64 {
        match cfg.get(name) {
            None | Some(Value::Null) => default,
            Some(v) => match util::py_int(v) {
                Some(iv) if iv > 0 => iv,
                Some(_) => 0,
                None => default,
            },
        }
    };
    let cfg_rpm = gnum("rate_limit_per_minute", 20);
    let cfg_tpm = gnum("tpm_limit", 50000);
    let cfg_cooldown_ms = gnum("account_cooldown_ms", 500);
    let cfg_daily_cap = gnum("daily_request_cap", 100);
    let cfg_daily_tok = gnum("daily_token_limit", 900000);
    let cfg_hourly = gnum("hourly_request_limit", 5);
    let cfg_acct_conc = gnum("acct_concurrency", 0);
    let cfg_chan_conc = gnum("total_concurrency", 0);
    let cfg_pool_rpm = gnum("pool_rpm_cap", 0);
    let cfg_pool_daily = gnum("pool_daily_cap", 0);

    let ups: HashMap<String, Value> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|u| u.is_object())
                .map(|u| (util::str_or(u.get("id"), ""), u.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut reason = [0i64; 13];

    // 渠道在途总量
    let mut chan_inflight: HashMap<String, i64> = HashMap::new();
    if let Some(keys) = db.get("keys").and_then(|x| x.as_array()) {
        for k in keys {
            let c = inflight_of(&util::str_or(k.get("id"), ""));
            if c > 0 {
                *chan_inflight
                    .entry(util::str_or(k.get("upstream_id"), ""))
                    .or_insert(0) += c;
            }
        }
    }

    // 渠道覆盖设置按渠道预计算一次
    let global_hide = cfg.get("hide_mapped_names").map(util::truthy).unwrap_or(true);
    let chan_eff = |up: Option<&Value>, field: &str, default: i64| -> i64 {
        let v = match up {
            Some(u) => util::int_or(u.get(field), 0),
            None => 0,
        };
        if v == -1 {
            0
        } else if v > 0 {
            v
        } else {
            default
        }
    };
    let mut cfg_cache: HashMap<String, (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, Vec<Value>)> =
        HashMap::new();
    let mut chan_cfg = |up: Option<&Value>| -> (
        i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, Vec<Value>,
    ) {
        (
            chan_eff(up, "rpm", cfg_rpm),
            chan_eff(up, "tpm", cfg_tpm),
            chan_eff(up, "daily_request_cap", cfg_daily_cap),
            chan_eff(up, "daily_token_limit", cfg_daily_tok),
            chan_eff(up, "hourly_request_limit", cfg_hourly),
            chan_eff(up, "account_cooldown_ms", cfg_cooldown_ms),
            chan_eff(up, "acct_concurrency", cfg_acct_conc),
            chan_eff(up, "total_concurrency", cfg_chan_conc),
            chan_eff(up, "rpm_cap", cfg_pool_rpm),
            chan_eff(up, "daily_cap", cfg_pool_daily),
            up.and_then(|u| u.get("models").and_then(|m| m.as_array()).cloned())
                .unwrap_or_default(),
        )
    };

    struct Pool {
        w: i64,
        items: Vec<usize>,
    }
    let mut pools: HashMap<String, Pool> = HashMap::new();
    // Python 的 pools 是插入有序 dict；这里显式记录插入顺序，保证多渠道平分时排序稳定
    let mut pool_order: Vec<String> = Vec::new();
    let mut total: i64 = 0;
    let mut model_ok: i64 = 0;

    let keys_len = db.get("keys").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
    for ki in 0..keys_len {
        let mut k = db.get("keys").and_then(|x| x.as_array()).unwrap()[ki].clone();
        if !k.get("enabled").map(util::truthy).unwrap_or(false) {
            continue;
        }
        total += 1;
        let uid = util::str_or(k.get("upstream_id"), "");
        let up = ups.get(&uid);
        if let Some(u) = up {
            if !u.get("enabled").map(util::truthy).unwrap_or(false) {
                reason[R_POOL] += 1;
                continue;
            }
        }
        let (rpm, tpm, daily_cap, daily_tok, hourly, cooldown_ms, acct_conc, chan_conc, pool_rpm_cap, pool_daily_cap, chan_models) =
            match cfg_cache.get(&uid) {
                Some(t) => t.clone(),
                None => {
                    let t = chan_cfg(up);
                    cfg_cache.insert(uid.clone(), t.clone());
                    t
                }
            };
        if !model.is_empty() && up.is_some() {
            if let Some(u) = up {
                if upstreams::hidden_target(u, model, global_hide) {
                    reason[R_MODEL_HIDDEN] += 1;
                    continue;
                }
            }
            if !chan_models.is_empty() && !chan_models.iter().any(|m| util::str_or(Some(m), "") == model) {
                reason[R_CHANNEL_MODEL] += 1;
                continue;
            }
        }
        if !model.is_empty() && model_missing_fresh(db, &uid, model) {
            reason[R_MODEL_MISSING] += 1;
            continue;
        }
        model_ok += 1;

        if util::int_or(k.get("banned_until"), 0) > now {
            reason[R_BANNED] += 1;
            continue;
        }
        if util::int_or(k.get("cooldown_until"), 0) > now {
            reason[R_COOLDOWN] += 1;
            continue;
        }
        if cooldown_ms > 0 && (now_f - util::f64_or(k.get("last_used_at"), 0.0)) * 1000.0 < cooldown_ms as f64 {
            reason[R_COOLDOWN] += 1;
            continue;
        }
        if acct_conc > 0 && inflight_of(&util::str_or(k.get("id"), "")) >= acct_conc {
            reason[R_ACCT_CONC] += 1;
            continue;
        }
        if chan_conc > 0 && chan_inflight.get(&uid).copied().unwrap_or(0) >= chan_conc {
            reason[R_CHAN_CONC] += 1;
            continue;
        }
        let kid = util::str_or(k.get("id"), "");
        let win: Vec<f64> = db
            .get("buckets")
            .and_then(|b| b.get(&kid))
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
            .unwrap_or_default();
        let win: Vec<f64> = win.into_iter().filter(|t| now_f - t < 60.0).collect();
        let mut first_seen = util::f64_or(k.get("first_seen_at"), 0.0);
        if first_seen == 0.0 {
            first_seen = now_f;
            if let Some(o) = k.as_object_mut() {
                o.insert("first_seen_at".into(), serde_json::json!(first_seen));
            }
            db.get_mut("keys").and_then(|x| x.as_array_mut()).unwrap()[ki] = k.clone();
        }
        if rpm > 0 {
            let warmup_s = util::cfg_int(&cfg, "warmup_seconds", 300).max(0);
            let mut eff_rpm = rpm;
            if warmup_s > 0 && first_seen > 0.0 {
                let elapsed = now_f - first_seen;
                if elapsed < warmup_s as f64 {
                    eff_rpm = ((rpm as f64 * (elapsed / warmup_s as f64).max(0.5)) as i64).max(1);
                }
            }
            if win.len() as i64 >= eff_rpm {
                reason[R_RPM] += 1;
                continue;
            }
        }
        let used_min = k
            .get("minute_tokens")
            .and_then(|m| m.get(&minute))
            .and_then(|v| util::py_int(v))
            .unwrap_or(0);
        if tpm > 0 && (used_min >= tpm || (est_tokens <= tpm && used_min + est_tokens > tpm)) {
            reason[R_TPM] += 1;
            continue;
        }
        let d_req = k
            .get("daily")
            .and_then(|d| d.get(&day))
            .and_then(|d| d.get("requests"))
            .and_then(util::py_int)
            .unwrap_or(0);
        if daily_cap > 0 && d_req >= daily_cap {
            let tomorrow = util::tomorrow_midnight(now);
            apply_ban(&mut k, tomorrow, "daily_cap");
            db.get_mut("keys").and_then(|x| x.as_array_mut()).unwrap()[ki] = k.clone();
            reason[R_DAILY] += 1;
            continue;
        }
        let d_tok = k
            .get("daily")
            .and_then(|d| d.get(&day))
            .and_then(|d| d.get("tokens"))
            .and_then(util::py_int)
            .unwrap_or(0);
        let hr_count = k
            .get("hour_requests")
            .and_then(|h| h.get(&hour))
            .and_then(util::py_int)
            .unwrap_or(0);
        if daily_tok > 0 && d_tok >= daily_tok && hourly > 0 && hr_count >= hourly {
            reason[R_DAILY] += 1;
            continue;
        }

        let mut weight = 10;
        if up.is_some() {
            let pb_min = db
                .get("pool_buckets")
                .and_then(|b| b.get(&uid))
                .and_then(|b| b.get(&minute))
                .and_then(util::py_int)
                .unwrap_or(0);
            if pool_rpm_cap > 0 && pb_min >= pool_rpm_cap {
                reason[R_POOL_RPM] += 1;
                continue;
            }
            let pd_day = db
                .get("pool_daily")
                .and_then(|b| b.get(&uid))
                .and_then(|b| b.get(&day))
                .and_then(util::py_int)
                .unwrap_or(0);
            if pool_daily_cap > 0 && pd_day >= pool_daily_cap {
                reason[R_POOL_DAILY] += 1;
                continue;
            }
            weight = up
                .and_then(|u| u.get("weight"))
                .and_then(util::py_int)
                .map(|w| w.max(1))
                .unwrap_or(10);
            if !pools.contains_key(&uid) {
                pool_order.push(uid.clone());
            }
            pools
                .entry(uid.clone())
                .or_insert_with(|| Pool { w: weight, items: Vec::new() })
                .items
                .push(ki);
        }
    }

    out.reason = reason_text(&reason, total);
    out.total = total;
    out.permanent = total > 0 && model_ok == 0;

    if pools.is_empty() {
        let now_i = util::now_i();
        let mut waits: Vec<i64> = Vec::new();
        if let Some(keys) = db.get("keys").and_then(|x| x.as_array()) {
            for k in keys {
                if !k.get("enabled").map(util::truthy).unwrap_or(false) {
                    continue;
                }
                for w in [
                    util::int_or(k.get("banned_until"), 0) - now_i,
                    util::int_or(k.get("cooldown_until"), 0) - now_i,
                ] {
                    if w > 0 {
                        waits.push(w);
                    }
                }
            }
        }
        if !waits.is_empty() {
            out.wait_hint = (*waits.iter().min().unwrap() as f64).clamp(0.5, 5.0);
        }
        return;
    }

    let recent = db.get("up_recent").cloned().unwrap_or(Value::Null);
    let mut pool_ids: Vec<String> = pool_order;
    let mut scores: HashMap<String, f64> = HashMap::new();
    for pid in &pool_ids {
        let rec_m = recent
            .get(&model_key(pid, model))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let rec = if rec_m.len() >= MODEL_RECENT_MIN {
            rec_m
        } else {
            recent
                .get(pid)
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
        };
        let ok_n = rec.iter().filter(|v| v.as_i64().unwrap_or(0) != 0).count();
        scores.insert(pid.clone(), (ok_n as f64 + 5.0) / (rec.len() as f64 + 10.0));
    }
    pool_ids.sort_by(|a, b| {
        let ka = (scores.get(a).copied().unwrap_or(0.0), pools.get(a).map(|p| p.w).unwrap_or(10));
        let kb = (scores.get(b).copied().unwrap_or(0.0), pools.get(b).map(|p| p.w).unwrap_or(10));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    let chosen = pool_ids[0].clone();
    let mut items = pools.get(&chosen).unwrap().items.clone();
    {
        let keys_arr = db.get("keys").and_then(|x| x.as_array()).unwrap();
        items.sort_by(|&a, &b| compare_keys(&keys_arr[a], &keys_arr[b]));
    }
    let kidx = items[0];
    let mut k = db.get("keys").and_then(|x| x.as_array()).unwrap()[kidx].clone();
    if let Some(o) = k.as_object_mut() {
        o.insert("last_used_at".into(), serde_json::json!(now_f));
        let tr = util::int_or(o.get("total_requests"), 0);
        o.insert("total_requests".into(), Value::from(tr + 1));
    }
    let kid = util::str_or(k.get("id"), "");
    inc_inflight(&kid);

    // RPM 时间戳只给「真正被使用的账号」打点
    {
        let obj = db.as_object_mut().unwrap();
        let buckets = obj
            .entry("buckets")
            .or_insert_with(|| Value::Object(Obj::new()));
        if !buckets.is_object() {
            *buckets = Value::Object(Obj::new());
        }
        let bm = buckets.as_object_mut().unwrap();
        let mut kw: Vec<Value> = bm
            .get(&kid)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter(|t| t.as_f64().map(|tv| now_f - tv < 60.0).unwrap_or(false))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        kw.push(serde_json::json!(now_f));
        bm.insert(kid.clone(), Value::Array(kw));
    }
    if !chosen.is_empty() {
        {
            let obj = db.as_object_mut().unwrap();
            let pb_all = obj
                .entry("pool_buckets")
                .or_insert_with(|| Value::Object(Obj::new()));
            if !pb_all.is_object() {
                *pb_all = Value::Object(Obj::new());
            }
            let pb = pb_all
                .as_object_mut()
                .unwrap()
                .entry(chosen.clone())
                .or_insert_with(|| Value::Object(Obj::new()));
            if !pb.is_object() {
                *pb = Value::Object(Obj::new());
            }
            let pm = pb.as_object_mut().unwrap();
            let cnt = pm.get(&minute).and_then(util::py_int).unwrap_or(0);
            pm.insert(minute.clone(), Value::from(cnt + 1));
            if pm.len() > 2 {
                let keep: Vec<String> = vec![
                    (minute.parse::<i64>().unwrap_or(0) - 1).to_string(),
                    minute.clone(),
                ];
                let drop: Vec<String> = pm
                    .keys()
                    .filter(|m| !keep.contains(m))
                    .cloned()
                    .collect();
                for m in drop {
                    pm.remove(&m);
                }
            }
        }
        {
            let obj = db.as_object_mut().unwrap();
            let pd_all = obj
                .entry("pool_daily")
                .or_insert_with(|| Value::Object(Obj::new()));
            if !pd_all.is_object() {
                *pd_all = Value::Object(Obj::new());
            }
            let pd = pd_all
                .as_object_mut()
                .unwrap()
                .entry(chosen.clone())
                .or_insert_with(|| Value::Object(Obj::new()));
            if !pd.is_object() {
                *pd = Value::Object(Obj::new());
            }
            let dm = pd.as_object_mut().unwrap();
            let cnt = dm.get(&day).and_then(util::py_int).unwrap_or(0);
            dm.insert(day.clone(), Value::from(cnt + 1));
            if dm.len() > 3 {
                let mut ds: Vec<String> = dm.keys().cloned().collect();
                ds.sort();
                let drop: Vec<String> = ds[..ds.len() - 3].to_vec();
                for d in drop {
                    dm.remove(&d);
                }
            }
        }
    }
    replace_key(db, &k);
    out.result = "ok".into();
    out.key = Some(k);
    out.reason = String::new();
}

pub fn release(
    key_id: &str,
    success: bool,
    http_status: i64,
    error: &str,
    usage: Option<&Value>,
    log: Option<&Value>,
    errno: i64,
) {
    dec_inflight(key_id);
    store().update(|db| release_fn(db, key_id, success, http_status, error, usage, log, errno));
}

pub async fn arelease(
    key_id: String,
    success: bool,
    http_status: i64,
    error: String,
    usage: Option<Value>,
    log: Option<Value>,
    errno: i64,
) {
    dec_inflight(&key_id);
    let db_res = tokio::task::spawn_blocking(move || {
        store().update(|db| release_fn(db, &key_id, success, http_status, &error, usage.as_ref(), log.as_ref(), errno));
    });
    let _ = db_res.await;
}

fn release_fn(
    db: &mut Value,
    key_id: &str,
    success: bool,
    http_status: i64,
    error: &str,
    usage: Option<&Value>,
    log: Option<&Value>,
    errno: i64,
) {
    let now = util::now_i();
    let cfg = db.get("config").cloned().unwrap_or(Value::Object(Obj::new()));
    let minute = util::utc_minute(now);
    let hour = util::utc_hour(now);
    let day = util::local_day(now);

    let cfg_int_ = |name: &str, default: i64| util::cfg_int(&cfg, name, default);

    if !key_id.is_empty() {
        let keys_len = db.get("keys").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        for ki in 0..keys_len {
            let k = db.get_mut("keys").and_then(|x| x.as_array_mut()).unwrap().get_mut(ki).unwrap().clone();
            if util::str_or(k.get("id"), "") != key_id {
                continue;
            }
            if !k.is_object() {
                continue; // 手工编辑过的 db.json 可能有脏行，跳过而不是 panic
            }
            let uid = util::str_or(k.get("upstream_id"), "");
            let up_row = db
                .get("upstreams")
                .and_then(|x| x.as_array())
                .and_then(|a| a.iter().find(|u| util::str_or(u.get("id"), "") == uid))
                .cloned();
            // 渠道覆盖：-1=不惩罚（返回 0 跳过），0=继承全局，正数=覆盖
            let eff = |field: &str, default: i64| -> i64 {
                let v = match &up_row {
                    Some(u) => util::int_or(u.get(field), 0),
                    None => 0,
                };
                if v == -1 {
                    0
                } else if v > 0 {
                    v
                } else {
                    default
                }
            };
            let mut k = k;
            if success {
                if let Some(o) = k.as_object_mut() {
                    let ts = util::int_or(o.get("total_success"), 0);
                    o.insert("total_success".into(), Value::from(ts + 1));
                    o.insert("consecutive_failures".into(), Value::from(0));
                    o.insert("rl_streak".into(), Value::from(0));
                }
            } else {
                if let Some(o) = k.as_object_mut() {
                    let tf = util::int_or(o.get("total_fail"), 0);
                    o.insert("total_fail".into(), Value::from(tf + 1));
                }
                let cls = classify(http_status, errno, error);
                if cls == "model" && (http_status == 400 || http_status == 404) && log.is_some() {
                    let model_name = util::str_or(log.and_then(|l| l.get("model")), "");
                    mark_model_missing(db, &uid, &model_name);
                }
                match cls {
                    "req" | "channel" | "model" | "pool_exhausted" => {
                        if let Some(o) = k.as_object_mut() {
                            o.insert("consecutive_failures".into(), Value::from(0));
                        }
                    }
                    "429" => {
                        let streak;
                        {
                            let o = k.as_object_mut().unwrap();
                            let s = util::int_or(o.get("rl_streak"), 0) + 1;
                            o.insert("rl_streak".into(), Value::from(s));
                            o.insert("consecutive_failures".into(), Value::from(0));
                            streak = s;
                        }
                        let base = eff("cool_429_seconds", cfg_int_("cool_429_seconds", 30));
                        if base > 0 {
                            let cool = (base * (1i64 << streak.saturating_sub(1).min(4))).min(600);
                            let cur = util::int_or(k.get("cooldown_until"), 0);
                            if let Some(o) = k.as_object_mut() {
                                o.insert("cooldown_until".into(), Value::from(cur.max(now + cool)));
                            }
                        }
                    }
                    "auth" | "payment" => {
                        let cfail;
                        let hfail;
                        {
                            let o = k.as_object_mut().unwrap();
                            let c = util::int_or(o.get("consecutive_failures"), 0) + 1;
                            let h = util::int_or(o.get("hard_fail_count"), 0) + 1;
                            o.insert("consecutive_failures".into(), Value::from(c));
                            o.insert("hard_fail_count".into(), Value::from(h));
                            cfail = c;
                            hfail = h;
                        }
                        let _ = cfail;
                        let reason = if cls == "payment" { "no_credit" } else { "invalid_key" };
                        let limit = eff("hard_fail_disable_count", cfg_int_("hard_fail_disable_count", 3));
                        if limit > 0 && hfail >= limit {
                            apply_ban(&mut k, now + 86400, reason);
                        } else {
                            let ban_s = eff("hard_fail_ban_seconds", cfg_int_("hard_fail_ban_seconds", 600));
                            if ban_s > 0 {
                                apply_ban(&mut k, now + ban_s, reason);
                            }
                        }
                    }
                    _ => {
                        // 5xx / 超时 / 连接失败：阶梯封禁 + 分级冷却
                        let cfail;
                        {
                            let o = k.as_object_mut().unwrap();
                            let c = util::int_or(o.get("consecutive_failures"), 0) + 1;
                            o.insert("consecutive_failures".into(), Value::from(c));
                            cfail = c;
                        }
                        let step = eff("ban_step_seconds", cfg_int_("ban_step_seconds", 5));
                        let cap = eff("ban_max_seconds", cfg_int_("ban_max_seconds", 300));
                        if step > 0 {
                            apply_ban(&mut k, now + (step * cfail).min(cap), "fail_ladder");
                        }
                        let cool_field = match cls {
                            "5xx" => "cool_5xx_seconds",
                            "timeout" => "cool_timeout_seconds",
                            "conn" => "cool_conn_seconds",
                            _ => "",
                        };
                        if !cool_field.is_empty() {
                            let cool = eff(cool_field, cfg_int_(cool_field, 30));
                            if cool > 0 {
                                let cur = util::int_or(k.get("cooldown_until"), 0);
                                if let Some(o) = k.as_object_mut() {
                                    o.insert("cooldown_until".into(), Value::from(cur.max(now + cool)));
                                }
                            }
                        }
                    }
                }
                if !error.is_empty() {
                    if let Some(o) = k.as_object_mut() {
                        o.insert("last_error".into(), Value::from(util::str_cut(error, 200)));
                        o.insert("last_error_at".into(), Value::from(now));
                    }
                }
            }

            let tin = usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(util::py_int)
                .unwrap_or(0);
            let tout = usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(util::py_int)
                .unwrap_or(0);
            {
                let o = k.as_object_mut().unwrap();
                let pt = util::int_or(o.get("prompt_tokens"), 0);
                let ct = util::int_or(o.get("completion_tokens"), 0);
                o.insert("prompt_tokens".into(), Value::from(pt + tin));
                o.insert("completion_tokens".into(), Value::from(ct + tout));
            }
            // daily
            {
                let o = k.as_object_mut().unwrap();
                let daily = o.entry("daily").or_insert_with(|| Value::Object(Obj::new()));
                if !daily.is_object() {
                    *daily = Value::Object(Obj::new());
                }
                let dm = daily.as_object_mut().unwrap();
                let d = dm
                    .entry(day.clone())
                    .or_insert_with(|| serde_json::json!({"requests": 0, "tokens": 0}));
                if !d.is_object() {
                    *d = serde_json::json!({"requests": 0, "tokens": 0});
                }
                let dobj = d.as_object_mut().unwrap();
                let rq = dobj.get("requests").and_then(util::py_int).unwrap_or(0);
                let tk = dobj.get("tokens").and_then(util::py_int).unwrap_or(0);
                dobj.insert("requests".into(), Value::from(rq + 1));
                dobj.insert("tokens".into(), Value::from(tk + tin + tout));
                if dm.len() > 3 {
                    let mut ds: Vec<String> = dm.keys().cloned().collect();
                    ds.sort();
                    let drop: Vec<String> = ds[..ds.len() - 3].to_vec();
                    for d in drop {
                        dm.remove(&d);
                    }
                }
            }
            // minute_tokens
            {
                let o = k.as_object_mut().unwrap();
                let mt = o.entry("minute_tokens").or_insert_with(|| Value::Object(Obj::new()));
                if !mt.is_object() {
                    *mt = Value::Object(Obj::new());
                }
                let mm = mt.as_object_mut().unwrap();
                let cur = mm.get(&minute).and_then(util::py_int).unwrap_or(0);
                mm.insert(minute.clone(), Value::from(cur + tin + tout));
                if mm.len() > 2 {
                    let keep: Vec<String> = vec![
                        (minute.parse::<i64>().unwrap_or(0) - 1).to_string(),
                        minute.clone(),
                    ];
                    let drop: Vec<String> = mm.keys().filter(|m| !keep.contains(m)).cloned().collect();
                    for m in drop {
                        mm.remove(&m);
                    }
                }
            }
            // hour_requests
            {
                let o = k.as_object_mut().unwrap();
                let hr = o.entry("hour_requests").or_insert_with(|| Value::Object(Obj::new()));
                if !hr.is_object() {
                    *hr = Value::Object(Obj::new());
                }
                let hm = hr.as_object_mut().unwrap();
                let cur = hm.get(&hour).and_then(util::py_int).unwrap_or(0);
                hm.insert(hour.clone(), Value::from(cur + 1));
                if hm.len() > 2 {
                    let keep: Vec<String> = vec![
                        (hour.parse::<i64>().unwrap_or(0) - 1).to_string(),
                        hour.clone(),
                    ];
                    let drop: Vec<String> = hm.keys().filter(|h| !keep.contains(h)).cloned().collect();
                    for h in drop {
                        hm.remove(&h);
                    }
                }
            }
            // recent（每账号最近 10 次）
            {
                let o = k.as_object_mut().unwrap();
                let recent = o.entry("recent").or_insert_with(|| Value::Array(vec![]));
                if !recent.is_array() {
                    *recent = Value::Array(vec![]);
                }
                let ra = recent.as_array_mut().unwrap();
                let row = serde_json::json!([
                    now,
                    util::str_cut(&util::str_or(log.and_then(|l| l.get("ep")), ""), 8),
                    util::str_cut(&util::str_or(log.and_then(|l| l.get("model")), ""), 60),
                    http_status,
                    util::int_or(log.and_then(|l| l.get("ms")), 0),
                    util::str_cut(error, 120),
                ]);
                ra.insert(0, row);
                ra.truncate(10);
            }
            // 渠道近期可行性
            let uid2 = util::str_or(k.get("upstream_id"), "");
            if !uid2.is_empty() {
                let flag = if success { 1 } else { 0 };
                {
                    let obj = db.as_object_mut().unwrap();
                    let rec_all = obj.entry("up_recent").or_insert_with(|| Value::Object(Obj::new()));
                    if !rec_all.is_object() {
                        *rec_all = Value::Object(Obj::new());
                    }
                    let rm = rec_all.as_object_mut().unwrap();
                    let rec = rm.entry(uid2.clone()).or_insert_with(|| Value::Array(vec![]));
                    if !rec.is_array() {
                        *rec = Value::Array(vec![]);
                    }
                    let r = rec.as_array_mut().unwrap();
                    r.insert(0, Value::from(flag));
                    r.truncate(20);
                }
                let m_name = util::str_or(log.and_then(|l| l.get("model")), "");
                if !m_name.is_empty() && m_name != "-" {
                    let mkey = model_key(&uid2, &m_name);
                    let obj = db.as_object_mut().unwrap();
                    let rec_all = obj.entry("up_recent").or_insert_with(|| Value::Object(Obj::new()));
                    let rm = rec_all.as_object_mut().unwrap();
                    let rec = rm.entry(mkey).or_insert_with(|| Value::Array(vec![]));
                    if !rec.is_array() {
                        *rec = Value::Array(vec![]);
                    }
                    let r = rec.as_array_mut().unwrap();
                    r.insert(0, Value::from(flag));
                    r.truncate(20);
                }
            }
            // 模型熔断：仅统计真实上游侧失败（5xx / 超时 / 连接失败）
            if !uid2.is_empty()
                && !success
                && ["5xx", "timeout", "conn"].contains(&classify(http_status, errno, error))
            {
                let model_name = util::str_or(log.and_then(|l| l.get("model")), "");
                if !model_name.is_empty() && model_name != "-" && cfg.get("breaker_enabled").map(util::truthy).unwrap_or(true) {
                    let obj = db.as_object_mut().unwrap();
                    let br_all = obj.entry("model_breaker").or_insert_with(|| Value::Object(Obj::new()));
                    if !br_all.is_object() {
                        *br_all = Value::Object(Obj::new());
                    }
                    let br = br_all
                        .as_object_mut()
                        .unwrap()
                        .entry(model_name.clone())
                        .or_insert_with(|| serde_json::json!({"fails": 0, "opened_until": 0}));
                    if !br.is_object() {
                        *br = serde_json::json!({"fails": 0, "opened_until": 0});
                    }
                    let bm = br.as_object_mut().unwrap();
                    let fails = util::int_or(bm.get("fails"), 0) + 1;
                    bm.insert("fails".into(), Value::from(fails));
                    let th = eff("breaker_threshold", cfg_int_("breaker_threshold", 3));
                    let bs = eff("breaker_seconds", cfg_int_("breaker_seconds", 60));
                    if th > 0 && bs > 0 && fails >= th {
                        bm.insert("opened_until".into(), Value::from(now + bs));
                        bm.insert("fails".into(), Value::from(0));
                    }
                }
            }
            replace_key(db, &k);
            break;
        }
    }

    // 每日统计（保留 14 天）—— 仅统计实际到达上游的请求
    if !key_id.is_empty() {
        if let Some(obj) = db.as_object_mut() {
            let stats = obj.entry("stats").or_insert_with(|| Value::Object(Obj::new()));
            if !stats.is_object() {
                *stats = Value::Object(Obj::new());
            }
            let sm = stats.as_object_mut().unwrap();
            let st = sm
                .entry(day.clone())
                .or_insert_with(|| serde_json::json!({"total": 0, "success": 0, "fail": 0, "models": {}}));
            if !st.is_object() {
                *st = serde_json::json!({"total": 0, "success": 0, "fail": 0, "models": {}});
            }
            let so = st.as_object_mut().unwrap();
            let total = util::int_or(so.get("total"), 0) + 1;
            so.insert("total".into(), Value::from(total));
            if success {
                let s = util::int_or(so.get("success"), 0);
                so.insert("success".into(), Value::from(s + 1));
            } else {
                let f = util::int_or(so.get("fail"), 0);
                so.insert("fail".into(), Value::from(f + 1));
            }
            let model = util::str_or(log.and_then(|l| l.get("model")), "");
            if success && !model.is_empty() && model != "-" {
                let models = so.entry("models").or_insert_with(|| Value::Object(Obj::new()));
                if !models.is_object() {
                    *models = Value::Object(Obj::new());
                }
                let mm = models.as_object_mut().unwrap();
                let c = mm.get(&model).and_then(util::py_int).unwrap_or(0);
                mm.insert(model, Value::from(c + 1));
                if mm.len() > 50 {
                    let mut items: Vec<(String, i64)> =
                        mm.iter().map(|(k, v)| (k.clone(), util::int_or(Some(v), 0))).collect();
                    items.sort_by(|a, b| b.1.cmp(&a.1));
                    let keep: std::collections::HashSet<String> =
                        items.into_iter().take(50).map(|x| x.0).collect();
                    let drop: Vec<String> = mm.keys().filter(|k| !keep.contains(k.as_str())).cloned().collect();
                    for k in drop {
                        mm.remove(&k);
                    }
                }
            }
            if sm.len() > 14 {
                let mut ds: Vec<String> = sm.keys().cloned().collect();
                ds.sort();
                let drop: Vec<String> = ds[..ds.len() - 14].to_vec();
                for d in drop {
                    sm.remove(&d);
                }
            }
        }
    }

    // 请求日志
    if log.is_some() && cfg.get("log_enabled").map(util::truthy).unwrap_or(true) {
        let log = log.unwrap();
        let obj = db.as_object_mut().unwrap();
        let logs = obj.entry("logs").or_insert_with(|| Value::Array(vec![]));
        if !logs.is_array() {
            *logs = Value::Array(vec![]);
        }
        let la = logs.as_array_mut().unwrap();
        let row = serde_json::json!([
            util::int_or(log.get("t"), now),
            util::str_cut(&util::str_or(log.get("ep"), ""), 8),
            util::str_cut(&util::str_or(log.get("model"), ""), 80),
            util::str_cut(&util::str_or(log.get("key"), ""), 40),
            util::int_or(log.get("st"), 0),
            util::int_or(log.get("ms"), 0),
            util::str_cut(&util::str_or(log.get("err"), ""), 140),
            util::str_cut(&util::str_or(log.get("ip"), ""), 45),
            util::int_or(log.get("att"), 1),
            util::str_cut(&util::str_or(log.get("up_model"), ""), 80),
            if log.get("stream").map(util::truthy).unwrap_or(false) { 1 } else { 0 },
            util::int_or(log.get("ttfb"), 0),
            util::int_or(log.get("in_tok"), 0),
            util::int_or(log.get("out_tok"), 0),
            util::str_cut(&util::str_or(log.get("tok"), ""), 20),
        ]);
        let max_logs = cfg_int_("log_max", 200).max(0) as usize;
        if max_logs > 0 {
            la.insert(0, row);
            la.truncate(max_logs);
        }
    }

    // 访问令牌使用追踪
    let tok_full = util::str_or(log.and_then(|l| l.get("tok_full")), "");
    if !tok_full.is_empty() {
        if let Some(cfg_mut) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            if let Some(tokens) = cfg_mut.get_mut("gateway_tokens").and_then(|t| t.as_array_mut()) {
                for x in tokens.iter_mut() {
                    if util::str_or(x.get("t"), "") == tok_full {
                        if let Some(o) = x.as_object_mut() {
                            o.insert(
                                "last_ip".into(),
                                Value::from(util::str_cut(&util::str_or(log.and_then(|l| l.get("ip")), ""), 45)),
                            );
                            o.insert("last_at".into(), Value::from(now));
                        }
                        break;
                    }
                }
            }
        }
    }
}

pub fn breaker_open(model: &str) -> Option<Value> {
    let db = store().load();
    let cfg = db.get("config")?;
    if !cfg.get("breaker_enabled").map(util::truthy).unwrap_or(true) {
        return None;
    }
    let br = db.get("model_breaker").and_then(|b| b.get(model))?;
    let left = util::int_or(br.get("opened_until"), 0) - util::now_i();
    if left <= 0 {
        return None;
    }
    Some(
        serde_json::json!({
            "fails": util::int_or(br.get("fails"), 0),
            "left": left,
        }),
    )
}

// ---------------------------------------------------------------- 管理操作

pub fn set_enabled(key_id: &str, enabled: bool) -> bool {
    let mut found = false;
    store().update(|db| {
        if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
            for k in keys.iter_mut() {
                if util::str_or(k.get("id"), "") == key_id {
                    found = true;
                    if let Some(o) = k.as_object_mut() {
                        o.insert("enabled".into(), Value::from(enabled));
                        if enabled {
                            o.insert("status".into(), Value::from("active"));
                            o.insert("consecutive_failures".into(), Value::from(0));
                            o.insert("hard_fail_count".into(), Value::from(0));
                            o.insert("banned_until".into(), Value::from(0));
                            o.insert("ban_reason".into(), Value::from(""));
                            o.insert("cooldown_until".into(), Value::from(0));
                            o.insert("rl_streak".into(), Value::from(0));
                        } else {
                            o.insert("status".into(), Value::from("manual_disabled"));
                        }
                        o.insert("updated_at".into(), Value::from(util::now_i()));
                    }
                    break;
                }
            }
        }
    });
    found
}

pub fn unban(key_id: &str) -> bool {
    let mut found = false;
    store().update(|db| {
        if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
            for k in keys.iter_mut() {
                if util::str_or(k.get("id"), "") == key_id {
                    found = true;
                    let was_enabled = k.get("enabled").map(util::truthy).unwrap_or(false);
                    let status = util::str_or(k.get("status"), "");
                    if let Some(o) = k.as_object_mut() {
                        o.insert("banned_until".into(), Value::from(0));
                        o.insert("ban_reason".into(), Value::from(""));
                        o.insert("cooldown_until".into(), Value::from(0));
                        o.insert("rl_streak".into(), Value::from(0));
                        o.insert("hard_fail_count".into(), Value::from(0));
                        o.insert("consecutive_failures".into(), Value::from(0));
                        if was_enabled && status == "invalid" {
                            o.insert("status".into(), Value::from("active"));
                        }
                        o.insert("updated_at".into(), Value::from(util::now_i()));
                    }
                    break;
                }
            }
        }
    });
    found
}

pub fn delete_key(key_id: &str) -> bool {
    let mut found = false;
    store().update(|db| {
        if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
            let before = keys.len();
            keys.retain(|k| {
                let id = util::str_or(k.get("id"), "");
                if id == key_id {
                    found = true;
                    return false;
                }
                true
            });
            if found && keys.len() < before {
                if let Some(o) = db.as_object_mut() {
                    if let Some(buckets) = o.get_mut("buckets").and_then(|b| b.as_object_mut()) {
                        buckets.remove(key_id);
                    }
                }
            }
        }
    });
    found
}

fn reset_key_fields(k: &mut Value) {
    if let Some(o) = k.as_object_mut() {
        for f in [
            "total_requests",
            "total_success",
            "total_fail",
            "consecutive_failures",
            "hard_fail_count",
            "prompt_tokens",
            "completion_tokens",
            "rl_streak",
        ] {
            o.insert(f.into(), Value::from(0));
        }
        o.insert("last_error".into(), Value::from(""));
        o.insert("last_error_at".into(), Value::from(0));
        o.insert("banned_until".into(), Value::from(0));
        o.insert("ban_reason".into(), Value::from(""));
        o.insert("cooldown_until".into(), Value::from(0));
        o.insert("daily".into(), serde_json::json!({}));
        o.insert("recent".into(), serde_json::json!([]));
        if o.get("enabled").map(util::truthy).unwrap_or(false) {
            o.insert("status".into(), Value::from("active"));
        }
    }
}

pub fn reset_stats(key_id: &str) -> bool {
    let mut found = false;
    store().update(|db| {
        if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
            for k in keys.iter_mut() {
                if util::str_or(k.get("id"), "") == key_id {
                    found = true;
                    reset_key_fields(k);
                    break;
                }
            }
        }
    });
    found
}

pub fn clear_all() -> i64 {
    let n = store()
        .load()
        .get("keys")
        .and_then(|x| x.as_array())
        .map(|a| a.len() as i64)
        .unwrap_or(0);
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("keys".into(), Value::Array(vec![]));
            o.insert("buckets".into(), serde_json::json!({}));
        }
    });
    n
}

pub fn reset_all_stats() {
    store().update(|db| {
        if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
            for k in keys.iter_mut() {
                reset_key_fields(k);
            }
        }
        if let Some(o) = db.as_object_mut() {
            o.insert("stats".into(), serde_json::json!({}));
        }
    });
}

pub async fn test_key(key_id: &str) -> Value {
    let key = store()
        .load()
        .get("keys")
        .and_then(|x| x.as_array())
        .and_then(|a| a.iter().find(|k| util::str_or(k.get("id"), "") == key_id))
        .cloned();
    let Some(key) = key else {
        return serde_json::json!({"ok": false, "error": "密钥不存在"});
    };
    let base = upstreams::base_for(&key);
    let apikey = util::str_or(key.get("apikey"), "");
    let t0 = std::time::Instant::now();
    let mut status: i64 = 0;
    let mut body = String::new();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_default();
    match client
        .get(format!("{}/models", base))
        .header("Authorization", format!("Bearer {}", apikey))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) => {
            status = r.status().as_u16() as i64;
            body = r.text().await.unwrap_or_default();
        }
        Err(e) => body = e.to_string(),
    }
    let ms = t0.elapsed().as_millis() as i64;
    let data: Option<Value> = serde_json::from_str(&body).ok();
    let ok = (200..400).contains(&status) && data.as_ref().map(|d| d.is_object()).unwrap_or(false);
    let count = if ok {
        data.as_ref()
            .and_then(|d| d.get("data"))
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    } else {
        0
    };
    let err = if ok {
        String::new()
    } else {
        let mut res = serde_json::json!({"status": status, "body": body});
        if status == 0 {
            res["error"] = Value::from(body.clone());
        }
        util::upstream_snippet(&res)
    };
    let email = util::str_or(key.get("email"), "");
    let log_row = serde_json::json!({
        "t": util::now_i(),
        "ep": "test",
        "model": "-",
        "key": util::mask_email(&email),
        "st": status,
        "ms": ms,
        "err": err,
        "ip": "-",
        "att": 1,
    });
    // test_key 从未取号（没有 inc_inflight），release 里却会无条件 dec_inflight：
    // 不补一对入账的话，计数为 0 时走 odd-release 分支污染泄漏诊断指标，
    // 更糟的是会把这个账号真实在途请求的并发计数错扣一个
    if !key_id.is_empty() {
        inc_inflight(key_id);
    }
    release(key_id, ok, status, &err, None, Some(&log_row), 0);
    serde_json::json!({"ok": ok, "status": status, "ms": ms, "models": count, "error": err})
}

// queue 模块引用（避免未使用告警）
#[allow(unused)]
fn _touch_queue() {
    let _ = queue::stats(false);
}
