//! JSON 存储层（core/store.py 的移植）：原子写、mtime 校验缓存、默认配置与迁移。

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
    memo_mtime: i64,
    memo_size: u64,
    memo_at: Instant,
    dirty: bool,
}

pub fn data_dir() -> PathBuf {
    match std::env::var("NGW_DATA_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        // 默认当前工作目录下的 data/（与 run.sh 「cd 到脚本目录再启动」的语义一致；
        // 不能用编译期路径——部署后的二进制里那是构建机的目录）
        _ => Path::new("data").to_path_buf(),
    }
}

pub fn db_path() -> PathBuf {
    data_dir().join("db.json")
}

fn warn_persist(msg: &str) {
    eprintln!(
        "[store] 落盘失败({});状态仍在内存、下一轮会重试,持续失败则重启会丢这段变更 —— 检查磁盘空间与 data/ 权限",
        msg
    );
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

fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
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

fn file_fingerprint(path: &Path) -> Option<(i64, u64)> {
    let md = fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((mtime, md.len()))
}

impl Store {
    pub fn new() -> Store {
        let dir = data_dir();
        let _ = fs::create_dir_all(&dir);
        let guard = dir.join(".htaccess");
        if !guard.exists() {
            let _ = fs::write(&guard, "Require all denied\n");
        }
        let s = Store {
            inner: Mutex::new(Inner {
                memo: None,
                memo_mtime: 0,
                memo_size: 0,
                memo_at: Instant::now(),
                dirty: false,
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
        self.write_now(&db);
        let user = util::str_or(db.pointer("/config/admin_username"), "admin");
        eprintln!(
            "[admin] 已按 NGW_ADMIN_PASSWORD 重置管理员密码（用户名 {}）；删掉该环境变量后不再覆盖",
            user
        );
    }

    fn read_file(&self) -> Value {
        let raw = fs::read(db_path()).unwrap_or_default();
        let mut db: Option<Value> = None;
        if !raw.is_empty() {
            match serde_json::from_slice::<Value>(&raw) {
                Ok(v) => db = Some(v),
                Err(_) => {
                    let bak = format!(
                        "{}.corrupt-{}",
                        db_path().display(),
                        util::now_i()
                    );
                    if fs::write(&bak, &raw).is_ok() {
                        eprintln!(
                            "[store] db.json 解析失败({} 字节),原文件已另存 {},本次以默认配置继续;找回数据:用该备份或 backup/data/db.json 覆盖 data/db.json 后重启",
                            raw.len(),
                            Path::new(&bak)
                                .file_name()
                                .map(|x| x.to_string_lossy().to_string())
                                .unwrap_or_default()
                        );
                    }
                }
            }
        }
        match db {
            Some(v) if v.is_object() => v,
            _ => Value::Object(Obj::new()),
        }
    }

    /// 读取快照（克隆）。高并发下如遇性能问题可再引入 Arc 缓存。
    pub fn load(&self) -> Value {
        let mut inner = self.inner.lock().unwrap();
        if inner.memo.is_some() && inner.memo_at.elapsed().as_millis() < 50 {
            return inner.memo.clone().unwrap();
        }
        let fp = file_fingerprint(&db_path());
        let (mut mtime, mut size) = fp.unwrap_or((0, 0));
        if inner.memo.is_some() && inner.memo_mtime == mtime && inner.memo_size == size {
            inner.memo_at = Instant::now();
            return inner.memo.clone().unwrap();
        }
        let raw_db = self.read_file();
        let before = canonical(&raw_db);
        let mut db = raw_db;
        migrate(&mut db);
        if canonical(&db) != before {
            self.write_now(&db);
            if let Some(f) = file_fingerprint(&db_path()) {
                mtime = f.0;
                size = f.1;
            }
        }
        inner.memo = Some(db.clone());
        inner.memo_mtime = mtime;
        inner.memo_size = size;
        inner.memo_at = Instant::now();
        db
    }

    fn write_now(&self, db: &Value) {
        let tmp = format!("{}.{}.tmp", db_path().display(), std::process::id());
        if let Ok(payload) = serde_json::to_string(db) {
            if fs::write(&tmp, payload.as_bytes()).is_ok() {
                // db.json 含 API Key 与口令哈希：Unix 上收紧到 0600
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
                }
                let _ = fs::rename(&tmp, db_path());
            }
        }
    }

    /// 加锁读-改-写；memo 未失效时直接复用内存态。
    pub fn update<F: FnOnce(&mut Value)>(&self, f: F) -> Value {
        let mut inner = self.inner.lock().unwrap();
        let fp = file_fingerprint(&db_path());
        let (mtime, size) = fp.unwrap_or((0, 0));
        if inner.memo.is_some() && inner.memo_mtime == mtime && inner.memo_size == size {
            let db = inner.memo.as_mut().unwrap();
            f(db);
            inner.dirty = true;
            bump_gen();
            return inner.memo.clone().unwrap();
        }
        let raw_db = self.read_file();
        let before = canonical(&raw_db);
        let mut db = raw_db;
        migrate(&mut db);
        f(&mut db);
        let after = canonical(&db);
        if after != before {
            drop(inner);
            self.write_now(&db);
            inner = self.inner.lock().unwrap();
            inner.dirty = false;
            bump_gen();
            if let Some(f2) = file_fingerprint(&db_path()) {
                inner.memo = Some(db.clone());
                inner.memo_mtime = f2.0;
                inner.memo_size = f2.1;
                inner.memo_at = Instant::now();
            }
        }
        db
    }

    /// 把内存态写盘（后台定期调用）。磁盘写在锁外执行，锁内只做序列化。
    pub fn flush(&self) {
        let payload = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty || inner.memo.is_none() {
                return;
            }
            let db = inner.memo.as_ref().unwrap();
            let p = match serde_json::to_string(db) {
                Ok(p) => p,
                Err(_) => return,
            };
            inner.dirty = false;
            p
        };
        let tmp = format!("{}.{}.tmp", db_path().display(), std::process::id());
        let write_res = fs::write(&tmp, payload.as_bytes());
        match write_res {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
                }
                let mut inner = self.inner.lock().unwrap();
                let _ = fs::rename(&tmp, db_path());
                bump_gen();
                if let Some(f) = file_fingerprint(&db_path()) {
                    inner.memo_mtime = f.0;
                    inner.memo_size = f.1;
                    inner.memo_at = Instant::now();
                }
            }
            Err(e) => {
                let mut inner = self.inner.lock().unwrap();
                inner.dirty = true;
                drop(inner);
                warn_persist(&e.to_string());
            }
        }
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
    ] {
        obj.entry(k)
            .or_insert(if k == "upstreams" || k == "keys" || k == "logs" || k == "queue" || k == "sessions" {
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
