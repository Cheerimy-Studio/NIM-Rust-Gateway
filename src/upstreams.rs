//! 上游渠道管理（core/upstreams.py 的移植）：CRUD、模型策略、模型映射、固定参数。

use crate::store::{store, Obj, Value};
use crate::util;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

pub fn all_upstreams() -> Vec<Value> {
    let db = store().load();
    match db.get("upstreams").and_then(|x| x.as_array()) {
        Some(a) => a.iter().filter(|u| u.is_object()).cloned().collect(),
        None => Vec::new(),
    }
}

pub fn get_upstream(uid: &str) -> Option<Value> {
    all_upstreams().into_iter().find(|u| util::str_or(u.get("id"), "") == uid)
}

pub fn ensure_default() {
    let db = store().load();
    let ids: Vec<String> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|u| util::str_or(u.get("id"), "")).collect())
        .unwrap_or_default();
    let need = ids.is_empty()
        || db.pointer("/config/endpoint_cache_v1").and_then(Value::as_bool) != Some(true)
        || db.get("keys").and_then(|x| x.as_array()).map(|keys| {
            keys.iter()
                .any(|k| !ids.contains(&util::str_or(k.get("upstream_id"), "")))
        }).unwrap_or(false);
    if need {
        store().update(|db| migrate(db));
    }
}

pub fn migrate(db: &mut Value) {
    // Old unscoped entries may have been created by an incompatible endpoint.
    // Reset once on upgrade; never discard fresh, task-scoped state on later loads.
    if db.pointer("/config/endpoint_cache_v1").and_then(Value::as_bool) != Some(true) {
        db["model_missing"] = serde_json::json!({});
        db["model_breaker"] = serde_json::json!({});
        if let Some(cfg) = db.get_mut("config").and_then(Value::as_object_mut) {
            cfg.insert("endpoint_cache_v1".into(), Value::Bool(true));
        }
    }
    let has = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has {
        let base = util::str_or(
            db.pointer("/config/upstream_base"),
            "https://integrate.api.nvidia.com/v1",
        );
        let row = serde_json::json!({
            "id": format!("u_{}", util::rand_hex(6)),
            "name": "NVIDIA NIM",
            "base": base,
            "api_endpoint": "auto",
            "auth_mode": "auto",
            "enabled": true,
            "weight": 10,
            "rpm_cap": 0,
            "daily_cap": 0,
            "rpm": 0,
            "tpm": 0,
            "daily_request_cap": 0,
            "daily_token_limit": 0,
            "request_timeout": 0,
            "models": [],
            "model_map": {},
            "hide_errors": 0,
            "hide_mapped": 0,
            "param_overrides": {},
        });
        if let Some(o) = db.as_object_mut() {
            let arr = o.entry("upstreams").or_insert_with(|| Value::Array(vec![]));
            if let Some(a) = arr.as_array_mut() {
                a.push(row);
            }
        }
    }
    let ids: Vec<String> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|u| util::str_or(u.get("id"), "")).collect())
        .unwrap_or_default();
    let default_id = db
        .pointer("/upstreams/0/id")
        .map(|x| util::str_or(Some(x), ""))
        .unwrap_or_default();
    if let Some(keys) = db.get_mut("keys").and_then(|x| x.as_array_mut()) {
        for k in keys.iter_mut() {
            let uid = util::str_or(k.get("upstream_id"), "");
            if !ids.contains(&uid) {
                if let Some(o) = k.as_object_mut() {
                    o.insert("upstream_id".into(), Value::from(default_id.clone()));
                }
            }
        }
    }
}

fn find_up<'a>(ups: &'a [Value], uid: &str) -> Option<&'a Value> {
    ups.iter().find(|u| util::str_or(u.get("id"), "") == uid)
}

pub fn base_for(key: &Value) -> String {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        return util::str_or(u.get("base"), "").trim_end_matches('/').to_string();
    }
    let db = store().load();
    util::str_or(db.pointer("/config/upstream_base"), "")
        .trim_end_matches('/')
        .to_string()
}

/// 重试/超时类字段覆盖：0 或 -1 均继承全局。
pub fn override_for(key: &Value, field: &str, global_value: i64) -> i64 {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        let v = util::int_or(u.get(field), 0);
        return if v > 0 { v } else { global_value };
    }
    global_value
}

pub fn flag_for(uid: &str, field: &str, global_on: bool) -> bool {
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, uid) {
        let v = util::int_or(u.get(field), 0);
        return if v == 0 { global_on } else { v == 1 };
    }
    global_on
}

pub fn upstream_value(key: &Value, field: &str, default: &str) -> String {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        let v = util::str_or(u.get(field), "").trim().to_string();
        if !v.is_empty() {
            return v;
        }
        return default.to_string();
    }
    default.to_string()
}

pub fn map_model_for(key: &Value, model: &str) -> String {
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        return util::str_or(
            u.get("model_map").and_then(|m| m.get(model)),
            model,
        );
    }
    model.to_string()
}

fn join_lines(raw: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for x in util::literal_items(raw) {
        let mut t = x.trim().to_string();
        let chars: Vec<char> = t.chars().collect();
        if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && (chars[0] == '"' || chars[0] == '\'') {
            t = chars[1..chars.len() - 1].iter().collect::<String>().trim().to_string();
        }
        if !t.is_empty() {
            parts.push(t);
        }
    }
    let joined = parts.join("\n");
    util::str_cut(&joined, 2000)
}

/// 解析模型价格表：每行 model=价格（元/次），容忍逗号分隔与字面量对象。
/// 价格 ≤0 或非法的行忽略（忽略即免费）。
pub fn parse_price_map(raw: &Value) -> Obj {
    let mut out = Obj::new();
    for line in util::literal_items(raw) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let (model, price) = (line[..eq].trim(), line[eq + 1..].trim());
        if model.is_empty() || model.chars().count() > 160 {
            continue;
        }
        let Ok(v) = price.parse::<f64>() else { continue };
        if !(v.is_finite()) || v <= 0.0 {
            continue;
        }
        out.insert(model.to_string(), serde_json::json!(util::round6(v)));
        if out.len() >= 500 {
            break;
        }
    }
    out
}

pub fn parse_model_map(raw: &Value) -> Obj {
    let mut out = Obj::new();
    for line in util::literal_items(raw) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // re.split(r"[=>]", line, maxsplit=1)
        let split_at = line.find(['=', '>']);
        if let Some(pos) = split_at {
            let (a, b) = (&line[..pos], &line[pos + 1..]);
            let (ka, kb) = (a.trim(), b.trim());
            if !ka.is_empty()
                && !kb.is_empty()
                && ka.chars().count() <= 160
                && kb.chars().count() <= 160
            {
                out.insert(ka.to_string(), Value::from(kb.to_string()));
            }
        }
        if out.len() >= 200 {
            break;
        }
    }
    out
}

/// 把参数值归一化为 JSON 原生类型。
pub fn coerce_param_value(val: &Value) -> Value {
    let s = match val {
        Value::String(s) => s,
        other => return other.clone(),
    };
    let v = s.trim();
    let low = v.to_lowercase();
    if low == "true" || low == "yes" {
        return Value::Bool(true);
    }
    if low == "false" || low == "no" {
        return Value::Bool(false);
    }
    // fullmatch(r"-?\d+") / fullmatch(r"-?\d+\.\d+")
    let is_int_re = {
        let body = v.strip_prefix('-').unwrap_or(v);
        !body.is_empty() && body.chars().all(|c| c.is_ascii_digit())
    };
    if is_int_re {
        return Value::from(v.parse::<i64>().unwrap_or(i64::MAX));
    }
    let is_float_re = {
        let body = v.strip_prefix('-').unwrap_or(v);
        match body.split_once('.') {
            Some((a, b)) => {
                !a.is_empty()
                    && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && b.chars().all(|c| c.is_ascii_digit())
            }
            None => false,
        }
    };
    if is_float_re {
        if let Ok(f) = v.parse::<f64>() {
            return serde_json::from_str::<Value>(&format!("{}", f))
                .unwrap_or_else(|_| Value::from(f));
        }
    }
    if v.starts_with('[') || v.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
    }
    Value::from(v.to_string())
}

fn valid_param_name(name: &str) -> bool {
    let n = name.chars().count();
    if n == 0 || n > 32 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn parse_param_pairs(text: &str) -> Obj {
    let mut out = Obj::new();
    let mut pieces: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.matches('=').count() > 1 {
            for piece in line.split([';', ',']) {
                pieces.push(piece.to_string());
            }
        } else {
            pieces.push(line.to_string());
        }
    }
    for piece in pieces {
        let piece = piece.trim();
        let Some(eq) = piece.find('=') else { continue };
        let name = piece[..eq].trim();
        let val = piece[eq + 1..].trim();
        if !valid_param_name(name) || val.chars().count() > 2000 {
            continue;
        }
        out.insert(name.to_string(), coerce_param_value(&Value::from(val)));
    }
    out
}

fn parse_scoped_text(text: &str) -> Obj {
    let mut scoped = Obj::new();
    let mut scope = "*".to_string();
    for line in text.lines() {
        let mut line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // ^([^=:]{1,160}):\s*(.+)$ 且 group(2) 里有 '='
        if let Some(colon) = line.find([':', '=']) {
            if line.as_bytes().get(colon) == Some(&b':') {
                let head = &line[..colon];
                let rest = line[colon + 1..].trim();
                if head.chars().count() <= 160
                    && !head.contains(['=', ':'])
                    && rest.contains('=')
                {
                    scope = head.trim().to_string();
                    line = rest.to_string();
                }
            }
        }
        for piece in line.split([';', ',']) {
            let piece = piece.trim();
            let Some(eq) = piece.find('=') else { continue };
            let name = piece[..eq].trim();
            let val = piece[eq + 1..].trim();
            if !valid_param_name(name) || val.chars().count() > 200 {
                continue;
            }
            let entry = scoped
                .entry(scope.clone())
                .or_insert_with(|| Value::Object(Obj::new()));
            if let Some(o) = entry.as_object_mut() {
                o.insert(name.to_string(), coerce_param_value(&Value::from(val)));
            }
        }
        scope = "*".to_string();
    }
    scoped
}

pub fn parse_param_overrides(raw: &Value) -> Value {
    if let Some(m) = raw.as_object() {
        let mut scoped = Obj::new();
        let mut flat = false;
        for (k, v) in m {
            if let Some(vm) = v.as_object() {
                let mut inner = Obj::new();
                for (pk, pv) in vm {
                    inner.insert(pk.clone(), coerce_param_value(pv));
                }
                scoped.insert(k.clone(), Value::Object(inner));
            } else {
                flat = true;
            }
        }
        if flat || (scoped.is_empty() && !m.is_empty()) {
            let lines: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}={}", k, util::json_compact(v)))
                .collect();
            return serde_json::json!({"*": parse_param_pairs(&lines.join("\n"))});
        }
        return Value::Object(scoped);
    }
    Value::Object(parse_scoped_text(&util::str_or(Some(raw), "")))
}

fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut re = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    regex::Regex::new(&re).ok()
}

fn fnmatch_case(pattern: &str, name: &str) -> bool {
    match glob_to_regex(pattern) {
        Some(re) => re.is_match(name),
        None => false,
    }
}

/// 全局 → 渠道(*) → 渠道(模型)，逐级强制覆盖。返回新 body。
pub fn apply_param_overrides(key: &Value, model: &str, body: &mut Value) {
    let cfg = store().load();
    let mut merged = parse_param_pairs(&util::str_or(cfg.pointer("/config/param_overrides"), ""));
    let uid = util::str_or(key.get("upstream_id"), "");
    let ups = all_upstreams();
    if let Some(u) = find_up(&ups, &uid) {
        let scoped = parse_param_overrides(u.get("param_overrides").unwrap_or(&Value::Null));
        if let Some(sm) = scoped.as_object() {
            if let Some(star) = sm.get("*").and_then(|x| x.as_object()) {
                for (k, v) in star {
                    merged.insert(k.clone(), v.clone());
                }
            }
            for (scope, params) in sm {
                if scope != "*" && (scope == model || fnmatch_case(scope, model)) {
                    if let Some(po) = params.as_object() {
                        for (k, v) in po {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
    }
    if let Some(bo) = body.as_object_mut() {
        for (name, val) in &merged {
            if valid_param_name(name) {
                bo.insert(name.clone(), val.clone());
            }
        }
    }
}

/// 两个模型名是否经由任一启用渠道的 model_map 互通（别名↔目标视为同一模型）。
/// 奖品 Key/用户体验卡的模型锁定按此口径放行，避免"存目标名、调别名"被拒。
pub fn models_alias_linked(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() || a == b {
        return a == b;
    }
    for u in all_upstreams() {
        if !u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false) {
            continue;
        }
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for (alias, target) in mm {
                let t = util::str_or(Some(target), "");
                if (alias == a && t == b) || (alias == b && t == a) {
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------- 模型上下文/输出上限

/// 解析模型参数上限：每行 `名字=ctx:上下文,out:输出` 或 `名字=上下文`（等价 ctx:N）。
/// 键按客户端可见模型名精确匹配（小写）。0 = 不设限。
/// 模型标注：上下文/最大输出上限（网关钳制）+ 预估出字速度（仅展示）。
#[derive(Clone, Copy, Default, PartialEq)]
pub struct ModelMeta {
    pub ctx: i64,
    pub out: i64,
    pub tps: i64,
}

pub fn parse_model_limits(cfg: &Value) -> HashMap<String, ModelMeta> {
    let text = util::str_or(cfg.get("model_limits"), "");
    let mut out: HashMap<String, ModelMeta> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let name = line[..eq].trim().to_lowercase();
        if name.is_empty() || name.chars().count() > 160 {
            continue;
        }
        let mut ctx = 0i64;
        let mut outn = 0i64;
        let mut tps = 0i64;
        for part in line[eq + 1..].split(',') {
            let part = part.trim().to_lowercase();
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once(':') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => ("ctx", part.as_str()),
            };
            let Ok(n) = v.parse::<i64>() else { continue };
            if n <= 0 {
                continue;
            }
            match k {
                "ctx" | "context" => ctx = n,
                "out" | "output" | "maxout" => outn = n,
                "tps" | "speed" => tps = n,
                _ => {}
            }
        }
        if ctx > 0 || outn > 0 || tps > 0 {
            out.insert(name, ModelMeta { ctx, out: outn, tps });
        }
        if out.len() >= 500 {
            break;
        }
    }
    out
}

/// 发送前按上限钳制请求体：输出上限直接 min；上下文上限扣除本请求估算
/// prompt 后作为 max_tokens 上限。只降不升，没配限的模型原样放行。
pub fn apply_model_limits(body: &mut Value, model: &str, limits: &HashMap<String, ModelMeta>, est_prompt: i64) -> bool {
    let Some(meta) = limits.get(&model.to_lowercase()).copied() else {
        return false;
    };
    let (ctx, out) = (meta.ctx, meta.out);
    let Some(obj) = body.as_object_mut() else { return false };
    let mut changed = false;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let Some(v) = obj.get(key).and_then(|x| x.as_i64()) else { continue };
        if v <= 0 {
            continue;
        }
        let mut cap = out;
        if ctx > 0 {
            // 上下文 = prompt + 输出：给推理/模板留 16 token 余量
            let room = (ctx - est_prompt - 16).max(1);
            cap = if cap > 0 { cap.min(room) } else { room };
        }
        if cap > 0 && v > cap {
            obj.insert(key.to_string(), Value::from(cap));
            changed = true;
        }
    }
    changed
}

/// 透传开关迁移：旧库渠道 models 为空 = 任意名可调。升级后默认收紧为隐藏，
/// 直接收紧会让现存透传渠道瞬间全部 404。首次升级时检测到这种渠道就自动
/// 保持旧行为（开关置 true），管理员在「模型接入」分页关掉即启用新语义。
pub fn migrate_transparent_once() {
    let empty_listed = {
        let db = store().load();
        // 哨兵键判断是否跑过：config 的默认值合并会让 transparent_upstream
        // 键永远存在（load 时填默认），不能用「键是否存在」当判断
        let already = db
            .get("config")
            .map(|c| c.get("transparent_migrated").map(util::truthy).unwrap_or(false))
            .unwrap_or(false);
        if already {
            return;
        }
        db.get("upstreams")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .any(|u| {
                        u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false)
                            && u.get("models")
                                .and_then(|m| m.as_array())
                                .map(|ms| ms.is_empty())
                                .unwrap_or(true)
                    })
            })
            .unwrap_or(false)
    };
    if !empty_listed {
        return;
    }
    store().update(|db| {
        if let Some(cfg) = db.get_mut("config").and_then(|c| c.as_object_mut()) {
            if cfg.get("transparent_migrated").map(util::truthy).unwrap_or(false) {
                return;
            }
            // 存在未列模型的启用渠道 → 保持旧行为；否则维持默认（隐藏）
            cfg.insert("transparent_upstream".into(), Value::from(true));
            cfg.insert("transparent_migrated".into(), Value::from(true));
        }
    });
    eprintln!("[migrate] 渠道存在未列模型的透传配置：已保持旧行为（任意模型名可调）。");
    eprintln!("[migrate] 新语义是「上游模型默认隐藏，加别名才对外」；在后台「模型接入」分页关闭透传开关即生效。");
}

// ---------------------------------------------------------------- 模型降级链

/// 一条命名链：请求链名时按序尝试成员，正文输出前的失败对调用方不可见。
/// 借鉴 dsh-our-free-model 的 combos 设计。
#[derive(Clone)]
pub struct ModelChain {
    pub name: String,
    pub members: Vec<String>,
    pub rr: bool,
    pub sticky: i64,
}

/// 解析配置 `model_chains`：每行 `名称 = 模型A, 模型B`，可选后缀
/// `| fb`（fallback，固定链首，默认）/ `| rr` / `| rrN`（round-robin，N 轮粘滞）。
pub fn parse_model_chains(cfg: &Value) -> Vec<ModelChain> {
    let text = util::str_or(cfg.get("model_chains"), "");
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let name = line[..eq].trim().to_string();
        let mut rest = line[eq + 1..].trim().to_string();
        if name.is_empty() || name.chars().count() > 80 {
            continue;
        }
        let mut rr = false;
        let mut sticky = 3i64;
        if let Some(bar) = rest.rfind('|') {
            let flag = rest[bar + 1..].trim().to_lowercase();
            let head = rest[..bar].trim().to_string();
            if flag == "fb" || flag == "fallback" {
                rest = head;
                rr = false;
            } else if flag.starts_with("rr") {
                rest = head;
                rr = true;
                let n = flag[2..].trim();
                if let Ok(v) = n.parse::<i64>() {
                    sticky = v.clamp(1, 100);
                }
            }
        }
        let mut members: Vec<String> = Vec::new();
        for m in rest.split(',') {
            let m = m.trim().to_string();
            // 重复成员只会浪费一次同号重试：解析期直接去重
            if m.is_empty() || m.chars().count() > 160 || members.iter().any(|x| x == &m) {
                continue;
            }
            members.push(m);
        }
        if members.len() >= 2 && out.iter().all(|c: &ModelChain| c.name != name) {
            out.push(ModelChain { name, members, rr, sticky });
        }
        if out.len() >= 50 {
            break;
        }
    }
    out
}

/// round-robin 起点状态：(链名 → (当前起点, 剩余粘滞次数))。
/// 粘滞语义：同一链连续 sticky 次请求保持同一起点分摊负载，
/// 之后推进到下一成员。fallback 链恒为 0。
static CHAIN_ROTATION: Mutex<Option<HashMap<String, (usize, i64)>>> = Mutex::new(None);

pub fn chain_by_name<'a>(chains: &'a [ModelChain], name: &str) -> Option<&'a ModelChain> {
    chains.iter().find(|c| c.name == name)
}

pub fn chain_start(chain: &ModelChain) -> usize {
    if !chain.rr || chain.members.len() < 2 {
        return 0;
    }
    let mut g = CHAIN_ROTATION.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    // 已删除链名的轮询状态会永远残留：超过上限整体清零重建
    // （丢一次轮询起点无正确性影响，比无界堆积干净）
    if m.len() > 256 {
        m.clear();
    }
    // 初始粘滞计数给满 sticky：旧实现给 sticky-1，导致链首成员永远少分一次
    // （|rr 时甚至直接从成员 1 起步）。推进路径的 sticky-1 是对的：当次调用
    // 本身就消耗一次配额，剩余 sticky-1 次留给后续调用
    let entry = m.entry(chain.name.clone()).or_insert((0, chain.sticky));
    if entry.1 > 0 {
        entry.1 -= 1;
        return entry.0 % chain.members.len();
    }
    entry.0 = (entry.0 + 1) % chain.members.len();
    entry.1 = chain.sticky - 1;
    entry.0
}

/// 渠道未列任何模型时是否放行任意模型名（旧行为=透传）。
/// 新部署默认 false：上游模型默认全部隐藏，加了别名才对外。
pub fn transparent_upstream(cfg: &Value) -> bool {
    cfg.get("transparent_upstream").map(util::truthy).unwrap_or(false)
}

/// 渠道能否服务该模型（models 清单门 + 隐原名门）。调用方先确认渠道 enabled。
/// transparent = 渠道没列模型时放行任意名（旧行为，见 transparent_upstream）。
pub fn channel_serves_model(u: &Value, model: &str, global_hide: bool, transparent: bool) -> bool {
    let models = u.get("models").and_then(|x| x.as_array());
    if let Some(ms) = models {
        let contains = ms.iter().any(|m| util::str_or(Some(m), "") == model);
        if !ms.is_empty() && !contains {
            return false;
        }
        if ms.is_empty() && !transparent {
            return false;
        }
    } else if !transparent {
        // models 字段缺失与空清单同口径：默认隐藏
        return false;
    }
    !hidden_target(u, model, global_hide)
}

pub fn model_routable(model: &str, hide_mapped_global: bool) -> bool {
    let mut any_enabled = false;
    let db = store().load();
    let cfg = db.get("config").unwrap_or(&Value::Null);
    let transparent = transparent_upstream(cfg);
    // 降级链：链名可路由 = 任一成员可路由（成员级路由在请求循环里逐个走）
    {
        let chains = parse_model_chains(cfg);
        if let Some(c) = chains.iter().find(|c| c.name == model) {
            return c.members.iter().any(|m| model_routable(m, hide_mapped_global));
        }
    }
    let ups = db.get("upstreams").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    for u in &ups {
        if !u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false) {
            continue;
        }
        any_enabled = true;
        if channel_serves_model(u, model, hide_mapped_global, transparent) {
            return true;
        }
    }
    !any_enabled
}

/// 某模型是否被该渠道的「隐原名」规则挡住（只对 model_map 的目标名生效）。
/// 渠道显式「禁止访问」(hide_mapped=1) 恒生效；「继承全局」(0) 时，显式列进
/// models 清单的目标名视为已对外授权，不被全局隐藏拦截（否则清单里同时列了
/// 别名和裸名的渠道，裸名会静默 404 —— ZCode 直呼 gpt-6-luna 就踩了这个坑）；
/// 「允许访问」(2) 不拦。
pub fn hidden_target(u: &Value, model: &str, global_hide: bool) -> bool {
    let hm = util::int_or(u.get("hide_mapped"), 0);
    let hide = if hm == 0 { global_hide } else { hm == 1 };
    if !hide {
        return false;
    }
    if hm != 1 {
        if let Some(ms) = u.get("models").and_then(|x| x.as_array()) {
            if ms.iter().any(|m| util::str_or(Some(m), "") == model) {
                return false;
            }
        }
    }
    u.get("model_map")
        .and_then(|m| m.as_object())
        .map(|mm| mm.values().any(|v| util::str_or(Some(v), "") == model))
        .unwrap_or(false)
}

/// 网关对外可调用的模型清单（只由渠道配置决定，不访问上游）。
pub fn curated_models() -> Vec<String> {
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(Value::Object(Obj::new()));
    let ups: Vec<Value> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|u| u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let push = |m: &str, out: &mut Vec<String>| {
        if !m.is_empty() && !out.iter().any(|x| x == m) {
            out.push(m.to_string());
        }
    };
    let mut curated = false;
    let global_hide = cfg.get("hide_mapped_names").map(util::truthy).unwrap_or(true);
    let transparent = transparent_upstream(&cfg);
    for u in &ups {
        // 隐藏集 = 被隐原名规则挡住的映射目标名；显式列进 models 的不算（见 hidden_target）
        let hidden: HashSet<String> = u
            .get("model_map")
            .and_then(|m| m.as_object())
            .map(|mm| {
                mm.values()
                    .map(|v| util::str_or(Some(v), ""))
                    .filter(|t| hidden_target(u, t, global_hide))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(ms) = u.get("models").and_then(|x| x.as_array()) {
            for m in ms {
                let s = util::str_or(Some(m), "");
                if !s.is_empty() && !hidden.contains(&s) {
                    push(&s, &mut out);
                    curated = true;
                }
            }
        }
    }
    if curated {
        for u in &ups {
            let models_list: HashSet<String> = u
                .get("models")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().map(|m| util::str_or(Some(m), "")).collect())
                .unwrap_or_default();
            if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
                for m in mm.keys() {
                    // 别名只有进了 models 清单才真的可路由；没进的挂出来只会
                    // 「清单里有、调用 404」。透传模式下维持旧行为
                    if transparent || models_list.contains(m) {
                        push(m, &mut out);
                    }
                }
            }
        }
        // 降级链名：任一成员可路由才对外，否则清单挂着却调不通
        for c in parse_model_chains(&cfg) {
            let routable = transparent
                || c.members.iter().any(|mem| {
                    ups.iter()
                        .any(|u| channel_serves_model(u, mem, global_hide, transparent))
                });
            if routable {
                push(&c.name, &mut out);
            }
        }
        return out;
    }
    if !transparent {
        // 默认隐藏：没有任何接入就老实返回空清单
        return out;
    }
    for u in &ups {
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for m in mm.keys() {
                push(m, &mut out);
            }
        }
    }
    for c in parse_model_chains(&cfg) {
        push(&c.name, &mut out);
    }
    out
}

/// 模型路由编辑器候选清单：启用渠道的 models 条目 + model_map 别名 + 现有链名。
/// 每项带 routable（按 model_routable 同口径判定当前是否真实可路由）与
/// chain（是否为链名）标记。整表一次载入完成，避免逐名重载 db。
pub fn routing_candidates() -> Vec<Value> {
    let db = store().load();
    let cfg = db.get("config").cloned().unwrap_or(Value::Null);
    let global_hide = cfg.get("hide_mapped_names").map(util::truthy).unwrap_or(true);
    let chains = parse_model_chains(&cfg);
    let ups: Vec<&Value> = db
        .get("upstreams")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|u| u.get("enabled").map(|x| util::truthy(x)).unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for u in &ups {
        if let Some(ms) = u.get("models").and_then(|x| x.as_array()) {
            for m in ms {
                let s = util::str_or(Some(m), "");
                if !s.is_empty() && names.len() < 500 && !names.iter().any(|x| x == &s) {
                    names.push(s);
                }
            }
        }
        if let Some(mm) = u.get("model_map").and_then(|m| m.as_object()) {
            for k in mm.keys() {
                if !k.is_empty() && names.len() < 500 && !names.iter().any(|x| x == k) {
                    names.push(k.clone());
                }
            }
        }
    }
    for c in &chains {
        if names.len() < 500 && !names.iter().any(|x| x == &c.name) {
            names.push(c.name.clone());
        }
    }
    names.sort();
    // 与 model_routable 同口径的裸名判定（不递归链）
    let plain_routable = |name: &str| -> bool {
        if ups.is_empty() {
            return true; // 无启用渠道 = 透传模式，任意名可路由
        }
        for u in &ups {
            let models = u.get("models").and_then(|x| x.as_array());
            if let Some(ms) = models {
                let contains = ms.iter().any(|m| util::str_or(Some(m), "") == name);
                if !ms.is_empty() && !contains {
                    continue;
                }
            }
            if hidden_target(u, name, global_hide) {
                continue;
            }
            return true;
        }
        false
    };
    let mut out: Vec<Value> = Vec::with_capacity(names.len());
    for name in &names {
        let is_chain = chains.iter().any(|c| &c.name == name);
        let routable = if is_chain {
            chains
                .iter()
                .find(|c| &c.name == name)
                .map(|c| c.members.iter().any(|m| plain_routable(m)))
                .unwrap_or(false)
        } else {
            plain_routable(name)
        };
        out.push(json!({"name": name, "routable": routable, "chain": is_chain}));
    }
    out
}

fn clamp(v: &Value, lo: i64, hi: i64, default: i64) -> i64 {
    let mut v = v.clone();
    if let Value::String(s) = &v {
        let low = s.trim().to_lowercase();
        if ["true", "false", "yes", "no", "on", "off"].contains(&low.as_str()) {
            v = Value::from(util::as_bool(&v, false));
        }
    }
    match util::py_int(&v) {
        Some(iv) => {
            if iv == -1 {
                return -1;
            }
            hi.min(lo.max(iv))
        }
        None => default,
    }
}

pub fn validate_save(data: &Value) -> (Option<Value>, String) {
    let name = util::str_or(data.get("name"), "").trim().to_string();
    // 自定义渠道 ID：进「渠道ID/模型名」展示与映射键，字符集必须收敛
    let id_in = util::str_or(data.get("id"), "").trim().to_string();
    if !id_in.is_empty()
        && (id_in.chars().count() > 48
            || !id_in
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    {
        return (None, "渠道 ID 只能含字母、数字、下划线、连字符，最长 48".into());
    }
    let raw_base = util::str_or(data.get("base"), "");
    if raw_base.chars().any(char::is_control) { return (None, "Base URL 不能包含控制字符".into()); }
    let mut base = raw_base.trim().to_string();
    base = base.trim_end_matches('/').to_string();
    if name.is_empty() {
        return (None, "名称不能为空".into());
    }
    let Ok(url) = reqwest::Url::parse(&base) else { return (None, "Base URL 不是有效 URL".into()) };
    if !["http", "https"].contains(&url.scheme()) || url.host_str().is_none() {
        return (None, "Base URL 必须是有效的 http(s) URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some()
        || base.chars().any(char::is_control) {
        return (None, "Base URL 不能包含用户密码、查询参数、片段或控制字符；密钥请通过账号配置".into());
    }
    let endpoint = match data.get("api_endpoint") {
        None => "auto",
        Some(Value::String(value)) => value.as_str(),
        _ => return (None, "api_endpoint 必须为字符串".into()),
    };
    if endpoint != "auto" && !["chat/completions", "responses", "messages", "completions"].contains(&endpoint) {
        return (None, "不支持的上游端点，请从协议列表选择".into());
    }
    let auth_mode = match data.get("auth_mode") {
        None => "auto",
        Some(Value::String(mode)) if ["auto", "bearer", "x-api-key"].contains(&mode.as_str()) => mode.as_str(),
        _ => return (None, "auth_mode 必须为 auto、bearer 或 x-api-key".into()),
    };
    for suffix in [
        "/chat/completions",
        "/completions",
        "/embeddings",
        "/responses",
        "/messages",
    ] {
        if base.ends_with(suffix) {
            return (
                None,
                "Base URL 只填到 /v1 这一层,不要带 /chat/completions 这类端点".into(),
            );
        }
    }
    let models_raw = data.get("models").cloned().unwrap_or(Value::from(""));
    let mut models = util::parse_model_list(&models_raw);
    models.truncate(300);
    let mut row = serde_json::json!({
        "name": util::str_cut(&name, 40),
        "base": base,
        "api_endpoint": endpoint,
        "auth_mode": auth_mode,
        "weight": clamp(data.get("weight").unwrap_or(&Value::from(10)), 1, 100, 10),
        "rpm_cap": clamp(data.get("rpm_cap").unwrap_or(&Value::from(0)), -1, 1_000_000, 0),
        "daily_cap": clamp(data.get("daily_cap").unwrap_or(&Value::from(0)), -1, 100_000_000, 0),
        "rpm": clamp(data.get("rpm").unwrap_or(&Value::from(0)), -1, 1_000_000, 0),
        "tpm": clamp(data.get("tpm").unwrap_or(&Value::from(0)), -1, 100_000_000, 0),
        "daily_request_cap": clamp(data.get("daily_request_cap").unwrap_or(&Value::from(0)), -1, 100_000_000, 0),
        "daily_token_limit": clamp(data.get("daily_token_limit").unwrap_or(&Value::from(0)), -1, 10_000_000_000, 0),
        "request_timeout": clamp(data.get("request_timeout").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "connect_timeout": clamp(data.get("connect_timeout").unwrap_or(&Value::from(0)), 0, 120, 0),
        "account_cooldown_ms": clamp(data.get("account_cooldown_ms").unwrap_or(&Value::from(0)), -1, 60_000, 0),
        "acct_concurrency": clamp(data.get("acct_concurrency").unwrap_or(&Value::from(0)), -1, 10_000, 0),
        "total_concurrency": clamp(data.get("total_concurrency").unwrap_or(&Value::from(0)), -1, 10_000, 0),
        "hourly_request_limit": clamp(data.get("hourly_request_limit").unwrap_or(&Value::from(0)), -1, 100_000, 0),
        "max_retries": clamp(data.get("max_retries").unwrap_or(&Value::from(0)), 0, 20, 0),
        "retry_backoff_base_ms": clamp(data.get("retry_backoff_base_ms").unwrap_or(&Value::from(0)), 0, 60_000, 0),
        "retry_backoff_max_ms": clamp(data.get("retry_backoff_max_ms").unwrap_or(&Value::from(0)), 0, 300_000, 0),
        "retry_min_wait_ms": clamp(data.get("retry_min_wait_ms").unwrap_or(&Value::from(0)), 0, 60_000, 0),
        "ban_step_seconds": clamp(data.get("ban_step_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "ban_max_seconds": clamp(data.get("ban_max_seconds").unwrap_or(&Value::from(0)), 0, 86_400, 0),
        "hard_fail_ban_seconds": clamp(data.get("hard_fail_ban_seconds").unwrap_or(&Value::from(0)), 0, 86_400, 0),
        "hard_fail_disable_count": clamp(data.get("hard_fail_disable_count").unwrap_or(&Value::from(0)), 0, 100, 0),
        "cool_429_seconds": clamp(data.get("cool_429_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "cool_5xx_seconds": clamp(data.get("cool_5xx_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "cool_timeout_seconds": clamp(data.get("cool_timeout_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "cool_conn_seconds": clamp(data.get("cool_conn_seconds").unwrap_or(&Value::from(0)), 0, 3600, 0),
        "breaker_threshold": clamp(data.get("breaker_threshold").unwrap_or(&Value::from(0)), 0, 50, 0),
        "breaker_seconds": clamp(data.get("breaker_seconds").unwrap_or(&Value::from(0)), 0, 86_400, 0),
        "models": models,
        "model_map": parse_model_map(data.get("model_map").unwrap_or(&Value::Object(Obj::new()))),
        "prices": parse_price_map(data.get("prices").unwrap_or(&Value::Object(Obj::new()))),
        "hide_errors": clamp(data.get("hide_errors").unwrap_or(&Value::from(0)), 0, 2, 0),
        "hide_mapped": clamp(data.get("hide_mapped").unwrap_or(&Value::from(0)), 0, 2, 0),
        "param_overrides": parse_param_overrides(data.get("param_overrides").unwrap_or(&Value::Object(Obj::new()))),
        "thinking_defaults": join_lines(data.get("thinking_defaults").unwrap_or(&Value::Null)),
        "enabled": util::as_bool(data.get("enabled").unwrap_or(&Value::Bool(true)), true),
        // 渠道开关：把调用方客户端的 User-Agent 透传给上游（默认关）
        "pass_client_ua": util::as_bool(data.get("pass_client_ua").unwrap_or(&Value::Bool(false)), false),
    });
    // 载荷未带 prices 时不要插入空对象：否则 save() 的旧字段保留循环会认为
    // 新行已有该键，导致渠道表单保存（前端从不发送 prices）抹掉已设定价
    if data.get("prices").is_none() {
        if let Some(o) = row.as_object_mut() {
            o.remove("prices");
        }
    }
    (Some(row), String::new())
}

pub fn save(data: &Value) -> (Option<Value>, String) {
    let (row, err) = validate_save(data);
    let Some(mut row) = row else { return (None, err) };
    let uid_in = util::str_or(data.get("id"), "");
    let prev_id = util::str_or(data.get("prev_id"), "");
    let mut final_id = String::new();
    let mut save_err: Option<String> = None;
    store().update(|db| {
        let ups_val = db
            .as_object_mut()
            .map(|o| o.entry("upstreams").or_insert_with(|| Value::Array(vec![])))
            .unwrap();
        let arr = match ups_val.as_array_mut() {
            Some(a) => a,
            None => return,
        };
        let mut replaced = false;
        let mut renamed_from = String::new();
        let mut routing_changed = false;
        // 带着旧 ID 的请求是「改名意图」：新 ID 命中别的渠道时绝不能静默更新
        // 那个渠道（改名该被「ID 已占用」拒绝），也不要误伤别人的配置
        let rename_intent = !prev_id.is_empty() && prev_id != uid_in;
        if !uid_in.is_empty() && !rename_intent {
            for u in arr.iter_mut() {
                if util::str_or(u.get("id"), "") == uid_in {
                    if let (Some(old), Some(newo)) = (u.as_object(), row.as_object_mut()) {
                        // 「载荷里没出现的字段」一律保留旧值：渠道表单提交全量字段不受影响，
                        // 而停用/启用等只提交部分字段的操作不会把 models/model_map/
                        // param_overrides/thinking_defaults/prices 等整块抹成默认值
                        for (k, v) in old {
                            if !data.get(k).is_some() {
                                newo.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    routing_changed = ["base", "api_endpoint", "auth_mode", "model_map", "models"]
                        .iter().any(|field| u.get(*field) != row.get(*field));
                    let enabled = row.get("enabled").map(util::as_bool2).unwrap_or(false);
                    if let Some(newo) = row.as_object_mut() {
                        newo.insert("id".into(), Value::from(uid_in.clone()));
                        if enabled {
                            newo.remove("auto_disabled_at");
                            newo.remove("auto_reason");
                        }
                    }
                    *u = row.clone();
                    final_id = uid_in.clone();
                    replaced = true;
                    break;
                }
            }
        }
        // 改名：新 ID 没命中任何渠道、但 prev_id 命中 → 显式改名并级联迁移，
        // 而不是被当成「新建」造成重复渠道 + 孤儿账号
        if !replaced && !uid_in.is_empty() && !prev_id.is_empty() && prev_id != uid_in {
            let hit = arr
                .iter()
                .position(|u| util::str_or(u.get("id"), "") == prev_id);
            let Some(idx) = hit else {
                save_err = Some(format!("要改名的渠道不存在（{}）", prev_id));
                return;
            };
            let dupe = arr
                .iter()
                .any(|u| util::str_or(u.get("id"), "") == uid_in);
            if dupe {
                save_err = Some(format!("渠道 ID {} 已被占用", uid_in));
                return;
            }
            let u = &mut arr[idx];
            if let (Some(old), Some(newo)) = (u.as_object(), row.as_object_mut()) {
                for (k, v) in old {
                    if !data.get(k).is_some() {
                        newo.insert(k.clone(), v.clone());
                    }
                }
            }
            routing_changed = ["base", "api_endpoint", "auth_mode", "model_map", "models"]
                .iter().any(|field| arr[idx].get(*field) != row.get(*field));
            let enabled = row.get("enabled").map(util::as_bool2).unwrap_or(false);
            if let Some(newo) = row.as_object_mut() {
                newo.insert("id".into(), Value::from(uid_in.clone()));
                if enabled {
                    newo.remove("auto_disabled_at");
                    newo.remove("auto_reason");
                }
            }
            arr[idx] = row.clone();
            renamed_from = prev_id.clone();
            final_id = uid_in.clone();
            replaced = true;
        }
        if !replaced {
            let mut new_row = row.clone();
            let new_id = if uid_in.is_empty() {
                format!("u_{}", util::rand_hex(6))
            } else {
                // 新建时也允许自定义 ID（已确认无冲突）
                uid_in.clone()
            };
            if arr.iter().any(|u| util::str_or(u.get("id"), "") == new_id) {
                save_err = Some(format!("渠道 ID {} 已被占用", new_id));
                return;
            }
            if let Some(o) = new_row.as_object_mut() {
                o.insert("id".into(), Value::from(new_id.clone()));
                o.insert("created_at".into(), Value::from(util::now_i()));
            }
            arr.push(new_row);
            final_id = new_id;
        }
        // 改名级联：账号归属 / 目录缓存 / 池与配额 / 负缓存 / 最近统计
        if !renamed_from.is_empty() {
            if let Some(keys) = db.get_mut("keys").and_then(Value::as_array_mut) {
                for k in keys.iter_mut() {
                    if util::str_or(k.get("upstream_id"), "") == prev_id {
                        if let Some(o) = k.as_object_mut() {
                            o.insert("upstream_id".into(), Value::from(uid_in.clone()));
                        }
                    }
                }
            }
            for map_name in ["upstream_catalog", "pool_buckets", "pool_daily", "model_missing"] {
                if let Some(m) = db.get_mut(map_name).and_then(Value::as_object_mut) {
                    if let Some(v) = m.remove(prev_id.as_str()) {
                        m.insert(uid_in.clone(), v);
                    }
                }
            }
            if let Some(m) = db.get_mut("up_recent").and_then(Value::as_object_mut) {
                let sep: String = std::iter::once('\0').collect();
                let old_prefix = format!("{}{}", prev_id, sep);
                let new_prefix = format!("{}{}", uid_in, sep);
                let old_keys: Vec<String> = m
                    .keys()
                    .filter(|k| k.as_str() == prev_id || k.starts_with(&old_prefix))
                    .cloned()
                    .collect();
                for k in old_keys {
                    if let Some(v) = m.remove(&k) {
                        let nk = if k == prev_id {
                            uid_in.clone()
                        } else {
                            k.replacen(&old_prefix, &new_prefix, 1)
                        };
                        m.insert(nk, v);
                    }
                }
            }
        }
        if routing_changed {
            if let Some(cache) = db.get_mut("model_missing").and_then(Value::as_object_mut) { cache.remove(&final_id); }
            // Breakers are model-global in the existing store; clear them after changing routing.
            if let Some(obj) = db.as_object_mut() { obj.insert("model_breaker".into(), serde_json::json!({})); }
        }
    });
    if let Some(e) = save_err {
        return (None, e);
    }
    (get_upstream(&final_id), String::new())
}

pub fn delete(uid: &str) -> (bool, String) {
    #[derive(Default)]
    struct State {
        found: bool,
        used: i64,
        last: bool,
    }
    let mut state = State::default();
    store().update(|db| {
        let ups = db.get("upstreams").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        state.found = ups.iter().any(|u| util::str_or(u.get("id"), "") == uid);
        if !state.found {
            return;
        }
        state.used = db
            .get("keys")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter(|k| util::str_or(k.get("upstream_id"), "") == uid).count() as i64)
            .unwrap_or(0);
        if state.used > 0 {
            return;
        }
        if ups.len() <= 1 {
            state.last = true;
            return;
        }
        if let Some(o) = db.as_object_mut() {
            o.insert(
                "upstreams".into(),
                Value::Array(
                    ups.into_iter()
                        .filter(|u| util::str_or(u.get("id"), "") != uid)
                        .collect(),
                ),
            );
            if let Some(pb) = o.get_mut("pool_buckets").and_then(|x| x.as_object_mut()) {
                pb.remove(uid);
            }
            if let Some(pd) = o.get_mut("pool_daily").and_then(|x| x.as_object_mut()) {
                pd.remove(uid);
            }
            if let Some(recent) = o.get_mut("up_recent").and_then(|x| x.as_object_mut()) {
                let drop_keys: Vec<String> = recent
                    .keys()
                    .filter(|k| k.as_str() == uid || k.starts_with(&format!("{}\0", uid)))
                    .cloned()
                    .collect();
                for k in drop_keys {
                    recent.remove(&k);
                }
            }
        }
    });
    if !state.found {
        return (false, "上游不存在".into());
    }
    if state.used > 0 {
        return (false, format!("该上游仍有 {} 个账号，请先删除或转移", state.used));
    }
    if state.last {
        return (false, "至少保留一个上游".into());
    }
    (true, String::new())
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    #[test]
    fn channel_unlimited_override_is_not_erased_by_validation() {
        let mut request=serde_json::json!({"name":"test","base":"https://api.example.test/v1"});
        for field in ["rpm","tpm","daily_request_cap","daily_token_limit","hourly_request_limit","account_cooldown_ms","acct_concurrency","total_concurrency","rpm_cap","daily_cap"] {request[field]=serde_json::json!(-1);}
        let row=validate_save(&request).0.unwrap();
        for field in ["rpm","tpm","daily_request_cap","daily_token_limit","hourly_request_limit","account_cooldown_ms","acct_concurrency","total_concurrency","rpm_cap","daily_cap"] {assert_eq!(row[field],-1);}
    }

    #[test]
    fn unscoped_cache_is_reset_once_during_upgrade() {
        let mut db = serde_json::json!({"config":{},"upstreams":[{"id":"test"}],"keys":[],"model_missing":{"test":{"model":999999999999.0}},"model_breaker":{"model":{"fails":3}}});
        migrate(&mut db);
        assert_eq!(db["model_missing"],serde_json::json!({}));
        assert_eq!(db["model_breaker"],serde_json::json!({}));
        db["model_missing"]=serde_json::json!({"fresh":true});
        migrate(&mut db);
        assert_eq!(db["model_missing"],serde_json::json!({"fresh":true}));
    }

    #[test]
    fn endpoint_and_base_url_are_validated_without_accepting_arbitrary_routes() {
        for endpoint in ["auto","chat/completions","responses","messages","completions"] {
            let (row,error)=validate_save(&serde_json::json!({"name":"test","base":"https://api.example.test/v1","api_endpoint":endpoint}));
            assert!(error.is_empty(),"{error}");assert_eq!(row.unwrap()["api_endpoint"],endpoint);
        }
        for endpoint in ["../admin","responses?api_key=secret","https://evil.test","embeddings",""] {
            assert!(validate_save(&serde_json::json!({"name":"test","base":"https://api.example.test/v1","api_endpoint":endpoint})).0.is_none());
        }
        assert!(validate_save(&serde_json::json!({"name":"test","base":"https://api.example.test/v1","api_endpoint":true})).0.is_none());
        for base in ["https://user:secret@example.test/v1","https://example.test/v1?secret=x","https://example.test/v1#x","file:///tmp/test","http://","https://example.test/v1\n"] {
            assert!(validate_save(&serde_json::json!({"name":"test","base":base})).0.is_none(),"{base}");
        }
    }
}

#[cfg(test)]
mod chain_tests {
    use super::*;
    use crate::store::store;

    fn cfg_of(text: &str) -> Value {
        serde_json::json!({ "model_chains": text })
    }

    #[test]
    fn parse_dedupes_members_and_keeps_first_chain_name() {
        let chains = parse_model_chains(&cfg_of("c1 = a, a, b ,\nc1 = x, y\n# comment\n\nbad line\nc2 = only-one\n"));
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].name, "c1");
        assert_eq!(chains[0].members, vec!["a", "b"]);
        assert!(!chains[0].rr);
        assert_eq!(chains[0].sticky, 3);
    }

    #[test]
    fn parse_flags_fb_rr_rrn_with_clamp() {
        let chains = parse_model_chains(&cfg_of("f = a, b | fb\nr = a, b | rr\nr5 = a, b | rr5\nr0 = a, b | rr0\nrx = a, b | rrx\n"));
        let get = |n: &str| chains.iter().find(|c| c.name == n).unwrap();
        assert!(!get("f").rr);
        assert!(get("r").rr && get("r").sticky == 3, "|rr 默认粘滞 3");
        assert!(get("r5").rr && get("r5").sticky == 5);
        assert!(get("r0").rr && get("r0").sticky == 1, "rr0 钳到 1");
        assert!(get("rx").rr && get("rx").sticky == 3, "非法 N 保持默认");
    }

    #[test]
    fn chain_start_round_robin_sticky_and_wrap() {
        let chain = ModelChain { name: "rt".into(), members: vec!["a".into(), "b".into()], rr: true, sticky: 2 };
        // 粘滞 2：两次同起点，再推进；对成员数取模
        let s1 = chain_start(&chain);
        let s2 = chain_start(&chain);
        assert_eq!(s1, s2, "粘滞期内起点不变");
        let s3 = chain_start(&chain);
        assert_eq!(s3, (s1 + 1) % 2, "粘滞结束后推进一格");
        // fallback 链恒 0，不写轮询状态
        let fb = ModelChain { name: "fb".into(), members: vec!["a".into(), "b".into()], rr: false, sticky: 3 };
        assert_eq!(chain_start(&fb), 0);
    }

    #[test]
    fn routing_candidates_marks_routable_and_chain() {
        let _g = crate::store::test_lock();
        let db = store().load();
        let saved_ups = db.get("upstreams").cloned();
        let saved_cfg = db.get("config").and_then(|c| c.get("model_chains")).cloned();
        store().update(|d| {
            d["upstreams"] = serde_json::json!([
                {"id":"u1","name":"T","base":"https://x.test/v1","enabled":true,
                 "models":["alpha","beta"],"model_map":{"gamma":"delta"}}
            ]);
            d["config"]["model_chains"] = serde_json::json!("mix = alpha, beta");
            d["config"]["hide_mapped_names"] = serde_json::json!(true);
        });
        let out = routing_candidates();
        let find = |n: &str| out.iter().find(|x| x["name"] == n).cloned();
        let alpha = find("alpha").expect("alpha in candidates");
        assert_eq!(alpha["routable"], true);
        assert_eq!(alpha["chain"], false);
        let mix = find("mix").expect("chain in candidates");
        assert_eq!(mix["chain"], true);
        assert_eq!(mix["routable"], true, "链可路由=任一成员可路由");
        // 隐藏目标名 delta：不在 models 清单 → 被隐原名规则挡住 → 不可路由也不进候选
        // （map 键 gamma 进候选但不在 models 清单 → 同样不可路由，UI 会标警告）
        let gamma = find("gamma").expect("gamma in candidates");
        assert_eq!(gamma["routable"], false);
        // 还原
        store().update(|d| {
            d["upstreams"] = saved_ups.unwrap_or(serde_json::json!([]));
            if let Some(v) = saved_cfg { d["config"]["model_chains"] = v; } else { d["config"].as_object_mut().unwrap().remove("model_chains"); }
            d["config"]["hide_mapped_names"] = serde_json::json!(true);
        });
    }
}

#[cfg(test)]
mod limits_tests {
    use super::*;

    #[test]
    fn parse_model_limits_forms_and_garbage() {
        let cfg = serde_json::json!({"model_limits": "a=ctx:4096,out:1024\nb = 2048\nC=CTX:10\nbad line\nd=ctx:-5\ne=ctx:0,out:7\nf=tps:120\n# comment\n"});
        let m = parse_model_limits(&cfg);
        assert_eq!(m.get("a").map(|x| (x.ctx, x.out)), Some((4096, 1024)));
        assert_eq!(m.get("b").map(|x| (x.ctx, x.out)), Some((2048, 0)), "裸数字=ctx");
        assert_eq!(m.get("c").map(|x| (x.ctx, x.out)), Some((10, 0)), "键大小写不敏感");
        assert!(!m.contains_key("d"));
        assert_eq!(m.get("e").map(|x| (x.ctx, x.out)), Some((0, 7)), "ctx:0 忽略，out 保留");
        assert_eq!(m.get("f").map(|x| x.tps), Some(120), "仅速度也成行");
    }

    #[test]
    fn apply_model_limits_clamps_down_only() {
        let cfg = serde_json::json!({"model_limits": "m=ctx:100,out:5\nbig=ctx:200\n"});
        let limits = parse_model_limits(&cfg);
        // 输出上限直接压
        let mut body = serde_json::json!({"max_tokens": 50});
        assert!(apply_model_limits(&mut body, "m", &limits, 2));
        assert_eq!(body["max_tokens"], 5);
        // 上下文扣除 prompt 估算（含 16 余量）作为上限；out 与 ctx 取更小
        let mut body = serde_json::json!({"max_completion_tokens": 300});
        assert!(apply_model_limits(&mut body, "big", &limits, 50));
        assert_eq!(body["max_completion_tokens"], 200 - 50 - 16);
        // 已经在上限内：不动
        let mut body = serde_json::json!({"max_tokens": 4});
        assert!(!apply_model_limits(&mut body, "m", &limits, 2));
        assert_eq!(body["max_tokens"], 4);
        // 未配置的模型：完全不动
        let mut body = serde_json::json!({"max_tokens": 999999});
        assert!(!apply_model_limits(&mut body, "other", &limits, 2));
        assert_eq!(body["max_tokens"], 999999);
        // 极小 ctx：至少给 1
        let cfg2 = serde_json::json!({"model_limits": "tiny=ctx:20\n"});
        let mut body = serde_json::json!({"max_tokens": 100});
        assert!(apply_model_limits(&mut body, "tiny", &parse_model_limits(&cfg2), 10));
        assert_eq!(body["max_tokens"], 1);
    }

    #[test]
    fn channel_serves_model_gates_transparent_and_hidden() {
        let listed = serde_json::json!({"models": ["a", "b"]});
        let empty = serde_json::json!({"models": []});
        let nomodel_field = serde_json::json!({});
        // 列了名单：命中才可路由，与透传开关无关
        assert!(channel_serves_model(&listed, "a", true, false));
        assert!(!channel_serves_model(&listed, "zz", true, false));
        assert!(!channel_serves_model(&listed, "zz", true, true));
        // 没列名单：新语义默认隐藏
        assert!(!channel_serves_model(&empty, "anything", true, false));
        assert!(!channel_serves_model(&nomodel_field, "anything", true, false));
        // 透传开关恢复旧行为
        assert!(channel_serves_model(&empty, "anything", true, true));
        assert!(channel_serves_model(&nomodel_field, "anything", true, true));
    }
}

#[cfg(test)]
mod curated_tests {
    use super::*;
    use crate::store::store;

    #[test]
    fn curated_lists_chain_when_transparent_on() {
        let _g = crate::store::test_lock();
        let db = store().load();
        let saved_ups = db.get("upstreams").cloned();
        let saved = db.get("config").cloned();
        store().update(|d| {
            d["upstreams"] = serde_json::json!([
                {"id":"u1","name":"T","base":"https://x.test/v1","enabled":true,
                 "models":["lim2"],"model_map":{"lim2":"limcheck"}}
            ]);
            d["config"]["model_chains"] = serde_json::json!("plazachain = chfail, mock-model");
            d["config"]["transparent_upstream"] = serde_json::json!(true);
            d["config"]["hide_mapped_names"] = serde_json::json!(true);
        });
        let out = curated_models();
        assert!(out.contains(&"lim2".to_string()), "lim2 in {:?}", out);
        assert!(out.contains(&"plazachain".to_string()), "chain in {:?}", out);
        store().update(|d| {
            d["upstreams"] = saved_ups.unwrap_or(serde_json::json!([]));
            if let Some(v) = saved { d["config"] = v; }
        });
    }
}
