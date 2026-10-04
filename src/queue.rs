//! 排队系统（core/queue.py 的移植）：FIFO 队列、位置追踪、统计、公开状态查询。

use crate::store::{store, Obj, Value};
use crate::util;

const QUEUE_MAX_ENTRIES: usize = 500;

fn cfg_max_wait() -> i64 {
    let v = store()
        .load()
        .pointer("/config/queue_max_wait")
        .and_then(util::py_int)
        .unwrap_or(15);
    if v == 0 {
        0
    } else {
        v.max(5)
    }
}

pub fn add(ep: &str, model: &str, ip: &str, tok: &str, reason: &str) -> String {
    let qid = format!("q_{}", util::rand_hex(6));
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let q = obj.entry("queue").or_insert_with(|| Value::Array(vec![]));
        if !q.is_array() {
            *q = Value::Array(vec![]);
        }
        let arr = q.as_array_mut().unwrap();
        arr.push(serde_json::json!({
            "id": qid,
            "t": util::now_f(),
            "ip": ip,
            "ep": util::str_cut(ep, 8),
            "model": util::str_cut(model, 60),
            "tok": util::str_cut(tok, 20),
            "reason": util::str_cut(reason, 90),
        }));
        if arr.len() > QUEUE_MAX_ENTRIES {
            let keep: Vec<Value> = arr[arr.len() - QUEUE_MAX_ENTRIES..].to_vec();
            *arr = keep;
        }
    });
    qid
}

/// 刷新某条等待的阻塞原因；只在内容真的变了才落一次存储。
pub fn set_reason(qid: &str, reason: &str) {
    let r = util::str_cut(reason, 90);
    {
        let db = store().load();
        if let Some(arr) = db.get("queue").and_then(|x| x.as_array()) {
            for e in arr {
                if util::str_or(e.get("id"), "") == qid {
                    if util::str_or(e.get("reason"), "") == r {
                        return;
                    }
                    break;
                }
            }
        }
    }
    store().update(|db| {
        if let Some(arr) = db.get_mut("queue").and_then(|x| x.as_array_mut()) {
            for e in arr.iter_mut() {
                if util::str_or(e.get("id"), "") == qid {
                    if let Some(o) = e.as_object_mut() {
                        o.insert("reason".into(), Value::from(r.clone()));
                    }
                    return;
                }
            }
        }
    });
}

/// 把阻塞原因归类成对外可说的粗粒度结论（公开队列页用）。
pub fn public_hint(reason: &str) -> &'static str {
    let r = reason;
    if r.is_empty() {
        return "排队等待中";
    }
    if r.contains("熔断") {
        return "模型熔断恢复中";
    }
    if ["封禁", "冷却", "RPM", "TPM", "日限", "上游RPM", "上游日限"]
        .iter()
        .any(|k| r.contains(k))
    {
        return "账号限流冷却中";
    }
    if ["账户并发", "渠道并发"].iter().any(|k| r.contains(k)) {
        return "账号繁忙";
    }
    if ["渠道模型", "原名禁用", "模型不存在"].iter().any(|k| r.contains(k)) {
        return "该模型当前不可用";
    }
    "等待可用账号"
}

pub fn remove(qid: &str) {
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            let q = o.entry("queue").or_insert_with(|| Value::Array(vec![]));
            if let Some(arr) = q.as_array_mut() {
                arr.retain(|e| e.is_object() && util::str_or(e.get("id"), "") != qid);
            }
        }
    });
}

pub fn clear() {
    store().update(|db| {
        if let Some(o) = db.as_object_mut() {
            o.insert("queue".into(), Value::Array(vec![]));
        }
    });
}

pub fn stats(public: bool) -> Value {
    let db = store().load();
    let max_wait = cfg_max_wait();
    let now = util::now_f();
    let cutoff = now - (max_wait as f64) * 2.0;
    let mut rows: Vec<Value> = Vec::new();
    let mut entries: Vec<Value> = db
        .get("queue")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter(|e| e.is_object()).cloned().collect())
        .unwrap_or_default();
    entries.sort_by(|a, b| {
        let ta = util::f64_or(a.get("t"), 0.0);
        let tb = util::f64_or(b.get("t"), 0.0);
        ta.partial_cmp(&tb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                util::str_or(a.get("id"), "").cmp(&util::str_or(b.get("id"), ""))
            })
    });
    for e in entries {
        let t = util::f64_or(e.get("t"), 0.0);
        if t < cutoff {
            continue;
        }
        let mut row = serde_json::json!({
            "wait": ((now - t).max(0.0) * 10.0).round() / 10.0,
            "ep": util::str_or(e.get("ep"), ""),
            "model": util::str_or(e.get("model"), ""),
        });
        if public {
            row["hint"] = Value::from(public_hint(&util::str_or(e.get("reason"), "")));
        } else {
            row["ip"] = Value::from(util::str_or(e.get("ip"), "-"));
            row["tok"] = Value::from(util::str_or(e.get("tok"), ""));
            row["reason"] = Value::from(util::str_or(e.get("reason"), ""));
        }
        rows.push(row);
    }
    let length = rows.len() as i64;
    serde_json::json!({
        "length": length,
        "max_wait": max_wait,
        "rows": rows,
    })
}

