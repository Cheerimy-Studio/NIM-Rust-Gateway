//! JSON 存储层：按「表」分文件落盘（data/db/*.json），原子写、内存缓存、默认配置与迁移。
//!
//! 旧版是单个 db.json：账号、日志、训练资料全在一个文件里，每次落盘都是全量重写，
//! 文件体积随使用线性膨胀。现在按访问模式拆成 9 个表文件，flush 只写有变化的表：
//!   config.json      配置 + 渠道预设
//!   keys.json        账号池
//!   upstreams.json   渠道
//!   logs.json        请求日志（高频）
//!   training.json    训练资料（体积大头）
//!   sessions.json    会话审计
//!   intercepted.json 拦截记录
//!   queue.json       排队（高频）
//!   metrics.json     统计与滑动窗口（stats/buckets/pool_*/up_recent/model_breaker/model_missing）
//!
//! 兼容：首次启动发现旧版 data/db.json 时自动拆分迁移，原文件改名 db.json.migrated 留存。

use crate::util;
use hmac::{Hmac, Mac};
pub use serde_json::{Map, Value};
use sha2::Sha256;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

pub type Db = Value;
pub type Obj = Map<String, Value>;

pub struct Store {
    inner: Mutex<Inner>,
}

struct Inner {
    memo: Option<Value>,
    dir_fp: Option<(i64, usize)>,
    memo_at: Instant,
    dirty: bool,
    saved: Vec<u64>, // 每个表上次成功落盘内容的哈希
}

pub fn data_dir() -> PathBuf {
    match std::env::var("NGW_DATA_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        // 默认当前工作目录下的 data/（与 run.sh 「cd 到脚本目录再启动」的语义一致；
        // 不能用编译期路径——部署后的二进制里那是构建机的目录）
        _ => Path::new("data").to_path_buf(),
    }
}

/// 分表存储目录：DATA_DIR/db/
pub fn db_dir() -> PathBuf {
    data_dir().join("db")
}

fn warn_persist(msg: &str) {
    eprintln!("[store] 写盘失败({}): 数据在内存中，等待下次写入", msg);
}

/// 表名 → 顶层键 的分组（组内键同文件落盘）。
const GROUPS: &[(&str, &[&str])] = &[
    ("config", &["config", "channel_presets"]),
    ("keys", &["keys"]),
    ("upstreams", &["upstreams"]),
    ("logs", &["logs"]),
    ("training", &["training"]),
    ("sessions", &["sessions"]),
    ("intercepted", &["intercepted"]),
    ("queue", &["queue"]),
    ("users", &["users", "user_tokens", "user_logs", "user_rpm"]),
    (
        "metrics",
        &[
            "stats",
            "buckets",
            "pool_buckets",
            "pool_daily",
            "up_recent",
            "model_breaker",
            "model_missing",
        ],
    ),
];

fn group_file(group: &str) -> PathBuf {
    db_dir().join(format!("{}.json", group))
}

pub fn default_config() -> Vec<(&'static str, Value)> {
    use serde_json::json;
    vec![
        ("rate_limit_per_minute", json!(20)),
        ("tpm_limit", json!(50000)),
        ("account_cooldown_ms", json!(500)),
        ("hourly_request_limit", json!(5)),
        ("acct_concurrency", json!(0)),
        ("total_concurrency", json!(0)),
        ("pool_rpm_cap", json!(0)),
        ("pool_daily_cap", json!(0)),
        ("daily_request_cap", json!(100)),
        ("daily_token_limit", json!(900000)),
        ("warmup_seconds", json!(300)),
        ("max_retries", json!(2)),
        ("retry_backoff_base_ms", json!(500)),
        ("retry_backoff_max_ms", json!(4000)),
        ("retry_min_wait_ms", json!(0)),
        ("request_timeout", json!(120)),
        ("connect_timeout", json!(10)),
        ("ban_step_seconds", json!(5)),
        ("ban_max_seconds", json!(300)),
        ("hard_fail_ban_seconds", json!(600)),
        ("hard_fail_disable_count", json!(3)),
        ("queue_enabled", json!(true)),
        ("queue_max_wait", json!(15)),
        ("queue_poll_ms", json!(400)),
        ("update_enabled", json!(false)),
        ("update_token", json!("")),
        ("update_auto_check_hours", json!(0)),
        ("restart_interval_hours", json!(0)),
        ("model_missing_ttl", json!(3600)),
        ("intercept_enabled", json!(false)),
        ("intercept_log_max", json!(100)),
        ("model_whitelist", json!("")),
        ("model_blacklist", json!("")),
        ("hide_upstream_errors", json!(true)),
        ("hide_mapped_names", json!(true)),
        ("param_overrides", json!("")),
        ("cool_429_seconds", json!(30)),
        ("cool_5xx_seconds", json!(30)),
        ("cool_timeout_seconds", json!(45)),
        ("cool_conn_seconds", json!(10)),
        ("breaker_enabled", json!(true)),
        ("breaker_threshold", json!(3)),
        ("breaker_seconds", json!(60)),
        ("ttfb_timeout", json!(60)),
        ("sse_idle_timeout", json!(60)),
        ("pool_max_connections", json!(400)),
        ("upstream_base", json!("https://integrate.api.nvidia.com/v1")),
        ("log_enabled", json!(true)),
        ("log_max", json!(200)),
        ("session_log_max", json!(100)),
        ("training_log_max", json!(500)),
        ("training_min_chars", json!(20)),
        ("watchdog_enabled", json!(true)),
        ("watchdog_minutes", json!(3)),
        ("timezone", json!("Asia/Shanghai")),
        ("verify_tls", json!(true)),
        ("admin_username", json!("admin")),
    ]
}

const LEGACY_KEYS: &[&str] = &["cooldown_seconds", "auto_disable_threshold"];

pub fn session_cookie(cfg: &Value) -> String {
    let secret = util::str_or(cfg.get("session_secret"), "");
    hmac_hex(secret.as_bytes(), b"admin")
}

pub fn csrf_token(cfg: &Value) -> String {
    let secret = util::str_or(cfg.get("session_secret"), "");
    hmac_hex(secret.as_bytes(), b"csrf")
}

pub fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    out.iter().map(|x| format!("{:02x}", x)).collect()
}

pub fn hash_password(pw: &str) -> String {
    use base64::Engine;
    use pbkdf2::pbkdf2_hmac;
    use rand::RngCore;
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let mut dk = [0u8; 32];
    pbkdf2_hmac::<Sha256>(pw.as_bytes(), &salt, 120_000, &mut dk);
    format!(
        "pbkdf2${}${}",
        base64::engine::general_purpose::STANDARD.encode(salt),
        base64::engine::general_purpose::STANDARD.encode(dk)
    )
}

pub fn verify_password(pw: &str, stored: &str) -> bool {
    use base64::Engine;
    use pbkdf2::pbkdf2_hmac;
    use subtle::ConstantTimeEq;
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 3 {
        return false;
    }
    let (Ok(salt), Ok(dk_stored)) = (
        base64::engine::general_purpose::STANDARD.decode(parts[1]),
        base64::engine::general_purpose::STANDARD.decode(parts[2]),
    ) else {
        return false;
    };
    let mut dk = [0u8; 32];
    pbkdf2_hmac::<Sha256>(pw.as_bytes(), &salt, 120_000, &mut dk);
    dk.ct_eq(&dk_stored).into()
}

/// 表文件指纹：只统计 9 张表文件的 (mtime 秒, 大小) 汇总。
/// 刻意忽略 .tmp / .corrupt：flush 期间临时文件的存在会让「目录级」指纹瞬时变化，
/// 被误判成外部改动 → 重读磁盘 → 把尚未落盘的内存变更丢掉（实测丢过训练记录）。
/// 该指纹只服务于「检测手工/外部编辑」，漏检的代价仅是下次重启才生效，误检代价是数据丢失，
/// 因此宁可迟钝也要稳定。
fn dir_fingerprint() -> (i64, usize) {
    let mut newest: i64 = 0;
    let mut total: usize = 0;
    for (group, _) in GROUPS {
        let p = group_file(group);
        let Ok(md) = fs::metadata(&p) else { continue };
        total += md.len() as usize;
        if let Ok(t) = md.modified() {
            let secs = t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if secs > newest {
                newest = secs;
            }
        }
    }
    (newest, total)
}

fn write_group_file(group: &str, payload: &str) -> Result<(), String> {
    let path = group_file(group);
    let tmp = format!("{}.{}.tmp", path.display(), std::process::id());
    fs::write(&tmp, payload.as_bytes()).map_err(|e| e.to_string())?;
    // 表文件含 API Key 与口令哈希：Unix 上收紧到 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    // 断电/崩溃时元数据日志可能只落下 rename 而数据块还没写盘，
    // 重命名前强制刷盘，避免表文件（含余额/口令哈希）损坏或半截
    if let Ok(f) = fs::OpenOptions::new().write(true).open(&tmp) {
        let _ = f.sync_all();
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

fn read_group_file(group: &str) -> Value {
    let path = group_file(group);
    let Ok(raw) = fs::read(&path) else { return Value::Object(Obj::new()) };
    match serde_json::from_slice::<Value>(&raw) {
        Ok(v) if v.is_object() => v,
        _ => {
            // 损坏：另存副本并告警，该表按空表继续（不影响其它表）
            let bak = format!("{}.corrupt-{}", path.display(), util::now_i());
            let _ = fs::rename(&path, &bak);
            eprintln!("[store] {}.json 解析失败，已另存原文件，该表重置为空", group);
            Value::Object(Obj::new())
        }
    }
}

fn group_hash(obj: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(obj).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// 从内存树提取一个组的对象。
fn extract_group(tree: &Value, keys: &[&str]) -> Value {
    let mut m = Obj::new();
    for k in keys {
        if let Some(v) = tree.get(*k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

fn legacy_db_json() -> Option<PathBuf> {
    let p = data_dir().join("db.json");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

impl Store {
    pub fn new() -> Store {
        let dir = data_dir();
        let _ = fs::create_dir_all(&dir);
        let guard = dir.join(".htaccess");
        if !guard.exists() {
            let _ = fs::write(&guard, "Require all denied\n");
        }
        let _ = fs::create_dir_all(db_dir());
        let s = Store {
            inner: Mutex::new(Inner {
                memo: None,
                dir_fp: None,
                memo_at: Instant::now(),
                dirty: false,
                saved: vec![0; GROUPS.len()],
            }),
        };
        s.bootstrap_env_admin_pw();
        s
    }

    /// NGW_ADMIN_PASSWORD 是「权威值」：设了就按它重置管理员密码（进程启动时一次）。
    fn bootstrap_env_admin_pw(&self) {
        let env_pw = match std::env::var("NGW_ADMIN_PASSWORD") {
            Ok(p) if !p.is_empty() => p,
            _ => return,
        };
        let mut db = self.load();
        {
            let cfg = db.as_object_mut().map(|o| o.entry("config").or_insert(Value::Object(Obj::new())));
            let Some(cfg) = cfg else { return };
            let stored = util::str_or(cfg.get("admin_password_hash"), "");
            if verify_password(&env_pw, &stored) {
                return;
            }
            if let Some(o) = cfg.as_object_mut() {
                o.insert("admin_password_hash".into(), Value::from(hash_password(&env_pw)));
            }
        }
        self.update(|d| {
            if let Some(o) = d.as_object_mut() {
                if let Some(c) = o.get_mut("config") {
                    *c = db.get("config").cloned().unwrap_or(Value::Object(Obj::new()));
                }
            }
        });
        self.flush();
        let user = util::str_or(db.pointer("/config/admin_username"), "admin");
        eprintln!("[admin] 已按 NGW_ADMIN_PASSWORD 重置管理员密码（{}）", user);
    }

    /// 读取快照（克隆）。分表目录为空且存在旧版 db.json 时做一次性拆分迁移。
    pub fn load(&self) -> Value {
        let mut inner = self.inner.lock().unwrap();
        if inner.memo.is_some() && inner.memo_at.elapsed().as_millis() < 50 {
            return inner.memo.clone().unwrap();
        }
        let fp = dir_fingerprint();
        if inner.memo.is_some() && inner.dir_fp == Some(fp) {
            inner.memo_at = Instant::now();
            return inner.memo.clone().unwrap();
        }
        // 指纹不符时，若内存里还有未落盘变更：先落盘，避免随后的重读把变更丢掉
        // （flush 会同时刷新指纹，之后再看就是「无外部改动」）
        if inner.memo.is_some() && inner.dirty {
            drop(inner);
            self.flush();
            inner = self.inner.lock().unwrap();
            let fp2 = dir_fingerprint();
            if inner.memo.is_some() && inner.dir_fp == Some(fp2) {
                inner.memo_at = Instant::now();
                return inner.memo.clone().unwrap();
            }
        }
        // 分表读取
        let mut db = Value::Object(Obj::new());
        let mut loaded_any = false;
        let mut saved = vec![0u64; GROUPS.len()];
        for (gi, (group, keys)) in GROUPS.iter().enumerate() {
            let obj = read_group_file(group);
            let empty = obj.as_object().map(|m| m.is_empty()).unwrap_or(true);
            if !empty {
                loaded_any = true;
                saved[gi] = group_hash(&obj);
                if let Some(m) = obj.as_object() {
                    for (k, v) in m {
                        db.as_object_mut().unwrap().insert(k.clone(), v.clone());
                    }
                }
            }
            let _ = keys;
        }
        if !loaded_any {
            // 首次启动：尝试旧版单文件迁移
            if let Some(legacy) = legacy_db_json() {
                if let Ok(raw) = fs::read(&legacy) {
                    if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                        if v.is_object() {
                            db = v;
                            eprintln!("[store] db.json 已迁移至 db/ 分表，原文件留存为 db.json.migrated");
                        }
                    }
                }
            }
        }
        let before = canonical(&db);
        migrate(&mut db);
        if canonical(&db) != before || !loaded_any {
            // 首建 / 迁移变更：全表落盘一次
            for (gi, (group, keys)) in GROUPS.iter().enumerate() {
                let obj = extract_group(&db, keys);
                let payload = serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into());
                match write_group_file(group, &payload) {
                    Ok(()) => saved[gi] = group_hash(&obj),
                    Err(e) => warn_persist(&e),
                }
            }
            if let Some(legacy) = legacy_db_json() {
                let renamed = data_dir().join("db.json.migrated");
                let _ = fs::rename(&legacy, &renamed);
            }
        }
        inner.saved = saved;
        inner.memo = Some(db.clone());
        inner.dir_fp = Some(dir_fingerprint());
        inner.memo_at = Instant::now();
        db
    }

    /// 加锁读-改-写；memo 未失效时直接复用内存态，落盘由 flush 按表统一处理。
    pub fn update<F: FnOnce(&mut Value)>(&self, f: F) -> Value {
        let mut inner = self.inner.lock().unwrap();
        let fp = dir_fingerprint();
        if inner.memo.is_some() && inner.dir_fp == Some(fp) {
            let db = inner.memo.as_mut().unwrap();
            f(db);
            inner.dirty = true;
            bump_gen();
            return inner.memo.clone().unwrap();
        }
        // memo 失效（外部改动/首次）：先把自己未落盘的变更写下去，再重读，
        // 否则重读会把只有内存里才有的变更静默覆盖掉
        let has_memo = inner.memo.is_some();
        let dirty = inner.dirty;
        drop(inner);
        if has_memo && dirty {
            self.flush();
        }
        let mut db = self.load();
        let mut inner = self.inner.lock().unwrap();
        // load() 已把磁盘态放进 memo；从 load 到重新拿锁之间若有并发快路径 update
        // 改过 memo，必须把 f 应用在最新 memo 上 —— 用旧 db 会覆盖掉并发更新（丢数据）
        match inner.memo.as_mut() {
            Some(memo) => {
                f(memo);
                db = memo.clone();
            }
            None => {
                f(&mut db);
                inner.memo = Some(db.clone());
            }
        }
        inner.dirty = true;
        bump_gen();
        inner.memo_at = Instant::now();
        db
    }

    /// 把内存态按表落盘：只写内容有变化的表。
    ///
    /// 写文件刻意放在锁内：若在锁外写，写盘期间 `dirty` 已被清空、磁盘又是「写了一半」
    /// 的状态，任何并发读盘都会把快照里已有、尚未写出的记录从内存里挤掉（实测丢过
    /// 训练记录）。每张表按需写、通常只有几十 KB 到 1MB，持锁几毫秒换取数据一致性。
    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirty || inner.memo.is_none() {
            return;
        }
        let snapshot = inner.memo.clone().unwrap();
        let mut all_ok = true;
        for (gi, (group, keys)) in GROUPS.iter().enumerate() {
            let obj = extract_group(&snapshot, keys);
            let hash = group_hash(&obj);
            if hash == inner.saved[gi] {
                continue; // 该表没变，跳过 —— 大表（training/logs）不反复重写
            }
            let payload = serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into());
            match write_group_file(group, &payload) {
                Ok(()) => inner.saved[gi] = hash,
                Err(e) => {
                    warn_persist(&e);
                    all_ok = false; // 失败的表保持旧哈希：下一轮 flush 只重试它
                }
            }
        }
        if all_ok {
            inner.dirty = false;
        }
        // 指纹刷新与写盘同锁：本进程写盘不会被误判成「外部改动」
        inner.dir_fp = Some(dir_fingerprint());
    }
}

/// Python json.dumps(sort_keys=True) 的等价规范化（比较用）。
fn canonical(v: &Value) -> String {
    fn sorted(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).unwrap());
                    out.push(':');
                    sorted(&m[*k], out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    sorted(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap()),
        }
    }
    let mut s = String::new();
    sorted(v, &mut s);
    s
}

pub fn migrate(db: &mut Value) {
    let obj = match db.as_object_mut() {
        Some(o) => o,
        None => {
            *db = Value::Object(Obj::new());
            db.as_object_mut().unwrap()
        }
    };
    for k in [
        "upstreams",
        "pool_buckets",
        "pool_daily",
        "up_recent",
        "model_breaker",
        "keys",
        "buckets",
        "stats",
        "logs",
        "queue",
        "sessions",
        "channel_presets",
        "users",
        "user_tokens",
        "user_logs",
        "user_rpm",
    ] {
        obj.entry(k)
            .or_insert(if k == "upstreams" || k == "keys" || k == "logs" || k == "queue" || k == "sessions"
                || k == "users" || k == "user_tokens" || k == "user_logs" {
                Value::Array(vec![])
            } else {
                Value::Object(Obj::new())
            });
    }
    let cfg = obj
        .entry("config")
        .or_insert_with(|| Value::Object(Obj::new()));
    if let Some(co) = cfg.as_object_mut() {
        for (k, v) in default_config() {
            co.entry(k.to_string()).or_insert_with(|| v.clone());
        }
        for legacy in LEGACY_KEYS {
            co.remove(*legacy);
        }
    }
    for (k, kind) in [
        ("logs", "arr"),
        ("intercepted", "arr"),
        ("model_missing", "obj"),
        ("training", "arr"),
        ("keys", "arr"),
        ("queue", "arr"),
        ("users", "arr"),
        ("user_tokens", "arr"),
        ("user_logs", "arr"),
        ("user_rpm", "obj"),
        ("buckets", "obj"),
        ("pool_buckets", "obj"),
        ("pool_daily", "obj"),
        ("up_recent", "obj"),
        ("model_breaker", "obj"),
    ] {
        let bad = match obj.get(k) {
            Some(Value::Array(_)) => kind != "arr",
            Some(Value::Object(_)) => kind != "obj",
            _ => true,
        };
        if bad {
            obj.insert(
                k.into(),
                if kind == "arr" {
                    Value::Array(vec![])
                } else {
                    Value::Object(Obj::new())
                },
            );
        }
    }
    if obj
        .get("channel_presets")
        .and_then(|x| x.as_object())
        .map(|m| m.is_empty())
        .unwrap_or(true)
    {
        obj.insert("channel_presets".into(), default_channel_presets());
    }
    let cfg = obj.get_mut("config").unwrap();
    let session_missing = util::str_or(cfg.get("session_secret"), "").is_empty();
    if session_missing {
        if let Some(co) = cfg.as_object_mut() {
            co.insert("session_secret".into(), Value::from(util::rand_hex(24)));
        }
    }
    if util::str_or(cfg.get("admin_username"), "").is_empty() {
        if let Some(co) = cfg.as_object_mut() {
            co.insert("admin_username".into(), Value::from("admin"));
        }
    }
    let h = util::str_or(cfg.get("admin_password_hash"), "");
    if !h.starts_with("pbkdf2$") {
        let env_pw = std::env::var("NGW_ADMIN_PASSWORD").unwrap_or_default();
        let pw = if env_pw.is_empty() {
            util::token_urlsafe(12)
        } else {
            env_pw
        };
        if let Some(co) = cfg.as_object_mut() {
            co.insert("admin_password_hash".into(), Value::from(hash_password(&pw)));
        }
        let username = util::str_or(cfg.get("admin_username"), "admin");
        println!(
            "\n==============================================================\n  首次初始化管理员账号\n    用户名: {}\n    密  码: {}\n  请立即登录后台修改密码。\n==============================================================",
            username, pw
        );
    }
    let gt_missing = cfg
        .get("gateway_tokens")
        .map(|x| !x.is_array())
        .unwrap_or(true);
    if gt_missing {
        if let Some(co) = cfg.as_object_mut() {
            co.insert("gateway_tokens".into(), Value::Array(vec![]));
        }
    }
    let mut struct_tokens: Vec<Value> = Vec::new();
    if let Some(arr) = cfg.get("gateway_tokens").and_then(|x| x.as_array()) {
        for t in arr {
            match t {
                Value::String(s) if !s.is_empty() => {
                    struct_tokens.push(serde_json::json!({"t": s, "m": []}));
                }
                Value::Object(m) => {
                    if let Some(tv) = m.get("t").and_then(|x| x.as_str()) {
                        if !tv.is_empty() {
                            let models: Vec<Value> = m
                                .get("m")
                                .and_then(|x| x.as_array())
                                .map(|a| {
                                    a.iter()
                                        .map(|x| Value::from(util::str_or(Some(x), "")))
                                        .collect()
                                })
                                .unwrap_or_default();
                            struct_tokens.push(serde_json::json!({"t": tv, "m": models}));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if struct_tokens.is_empty() {
        struct_tokens.push(serde_json::json!({"t": format!("sk-gw-{}", util::rand_hex(12)), "m": []}));
    }
    if let Some(co) = cfg.as_object_mut() {
        co.insert("gateway_tokens".into(), Value::Array(struct_tokens));
    }
}

fn default_channel_presets() -> Value {
    serde_json::json!({
        "nvidia_nim": {
            "name": "NVIDIA NIM", "rpm": 25, "tpm": 50000,
            "account_cooldown_ms": 500, "max_retries": 2,
            "retry_backoff_base_ms": 500, "retry_backoff_max_ms": 4000,
            "ban_step_seconds": 5, "ban_max_seconds": 300,
            "hard_fail_ban_seconds": 600, "hard_fail_disable_count": 3,
            "daily_request_cap": -1, "daily_token_limit": -1,
            "hourly_request_limit": 5, "request_timeout": 120, "connect_timeout": 10,
        },
        "openai_compat": {
            "name": "OpenAI 兼容", "rpm": -1, "tpm": -1,
            "account_cooldown_ms": 200, "max_retries": 2,
            "retry_backoff_base_ms": 500, "retry_backoff_max_ms": 5000,
            "ban_step_seconds": 10, "ban_max_seconds": 600,
            "hard_fail_ban_seconds": 300, "hard_fail_disable_count": 5,
            "daily_request_cap": -1, "daily_token_limit": -1,
            "hourly_request_limit": -1, "request_timeout": 120, "connect_timeout": 5,
        },
    })
}

pub static STORE: std::sync::OnceLock<Store> = std::sync::OnceLock::new();

/// 每次 update()/flush() 递增：cfg_all 的 1 秒缓存据此立即失效。
/// Python 版的 cfg_all 缓存的是同一个活 dict（update 原地改，缓存天然即时生效）；
/// Rust 快照语义必须靠代数对齐，否则设置/拦截/令牌的变更会延迟最多 1 秒生效。
static STORE_GEN: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn store_gen() -> i64 {
    STORE_GEN.load(std::sync::atomic::Ordering::SeqCst)
}

fn bump_gen() {
    STORE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

pub fn store() -> &'static Store {
    STORE.get_or_init(Store::new)
}
