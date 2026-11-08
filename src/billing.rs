//! 按次或按 token 结算。长度档位按输入 token 命中，整次请求使用该档单价。
use serde_json::{json, Value};

fn rate(v: Option<&Value>) -> Result<f64, String> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite() && (0.0..=100_000.0).contains(x))
        .ok_or_else(|| "价格必须是 0 到 100000 之间的数字".into())
}

pub fn validate(v: &Value) -> Result<(), String> {
    if v.is_number() { rate(Some(v))?; return Ok(()); }
    if v.get("mode").and_then(Value::as_str) != Some("token") {
        return Err("计费方式必须为按次数字或 token 价格对象".into());
    }
    rate(v.get("input"))?; rate(v.get("output"))?;
    if let Some(tiers) = v.get("tiers") {
        let tiers = tiers.as_array().ok_or("长度档位必须为数组")?;
        if tiers.len() > 16 { return Err("最多设置 16 个长度档位".into()); }
        let mut last = 0;
        for tier in tiers {
            let min = tier.get("min_input").and_then(Value::as_i64).filter(|n| *n > last && *n <= 100_000_000)
                .ok_or("档位起点必须为递增正整数，且不超过 100000000")?;
            rate(tier.get("input"))?; rate(tier.get("output"))?; last = min;
        }
    }
    Ok(())
}

pub fn paid(v: &Value) -> bool {
    if let Some(n) = v.as_f64() { return n > 0.0; }
    v.get("input").and_then(Value::as_f64).unwrap_or(0.0) > 0.0
        || v.get("output").and_then(Value::as_f64).unwrap_or(0.0) > 0.0
        || v.get("tiers").and_then(Value::as_array).is_some_and(|ts| ts.iter().any(paid))
}

pub fn snapshot(v: &Value, input: i64) -> Value {
    if v.is_number() || v.is_null() {
        return json!({"mode":"request", "price":v.as_f64().unwrap_or(0.0)});
    }
    let tier = v.get("tiers").and_then(Value::as_array).and_then(|ts| ts.iter().rev()
        .find(|t| t.get("min_input").and_then(Value::as_i64).is_some_and(|n| input >= n))).unwrap_or(v);
    json!({"mode":"token", "input":tier.get("input").and_then(Value::as_f64).unwrap_or(0.0),
        "output":tier.get("output").and_then(Value::as_f64).unwrap_or(0.0),
        "min_input":tier.get("min_input").and_then(Value::as_i64).unwrap_or(0), "unit":1_000_000})
}

pub fn cost(v: &Value, input: i64, output: i64) -> f64 {
    let p = snapshot(v, input);
    let value = if p["mode"] == "token" {
        (input.max(0) as f64 * p["input"].as_f64().unwrap_or(0.0)
            + output.max(0) as f64 * p["output"].as_f64().unwrap_or(0.0)) / 1_000_000.0
    } else { p["price"].as_f64().unwrap_or(0.0) };
    // A positive charge never silently rounds to free at the ledger's micro-yuan precision.
    if value > 0.0 { crate::util::round6(value).max(0.000001) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn length_pricing_and_validation() {
        let p=json!({"mode":"token","input":2,"output":8,"tiers":[{"min_input":128001,"input":4,"output":16}]});
        validate(&p).unwrap();
        assert_eq!(cost(&p,100000,1000),0.208);
        assert_eq!(cost(&p,128000,1000),0.264);
        assert_eq!(cost(&p,128001,1000),0.528004);
        assert_eq!(cost(&json!(0.1),999999,999999),0.1);
        assert_eq!(cost(&json!(0),5,6),0.0);
        assert_eq!(cost(&json!({"mode":"token","input":0.001,"output":0}),1,0),0.000001);
        assert!(paid(&json!({"mode":"token","input":0,"output":0,"tiers":[{"min_input":10,"input":1,"output":0}]})));
        for p in [json!(-1),json!("2"),json!({"mode":"token","input":1}),json!({"mode":"token","input":-1,"output":0}),
            json!({"mode":"token","input":1,"output":1,"tiers":[{"min_input":2,"input":1,"output":1},{"min_input":2,"input":1,"output":1}]})] {
            assert!(validate(&p).is_err(),"{p}");
        }
    }
}

// Pending requests share a process-local budget; only completed usage mutates the ledger.
fn pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, f64>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, f64>>> = std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}
#[derive(Debug)]
pub struct Reservation {
    pub pricing: Value,
    pub model: String,
    user: String,
    amount: f64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut pending = pending().lock().unwrap_or_else(|e|e.into_inner());
        if let Some(total) = pending.get_mut(&self.user) {
            *total = (*total - self.amount).max(0.0);
            if *total < 0.0000001 { pending.remove(&self.user); }
        }
    }
}
pub fn reserve(user: &str, channel: &str, model: &str, request: &Value, input: i64, embeddings: bool) -> Result<std::sync::Arc<Reservation>, String> {
    let pricing = crate::users::pricing_for(channel,model);
    let output = if embeddings { 0 } else { ["max_completion_tokens","max_tokens","max_output_tokens"].iter()
        .find_map(|k| request.get(k).and_then(Value::as_i64)).unwrap_or(1024).max(0) };
    let amount = cost(&pricing,input,output);
    let mut pending = pending().lock().unwrap_or_else(|e|e.into_inner());
    let Some(row) = crate::users::auth_user(user) else { return Err("用户已被停用".into()); };
    let available = crate::users::balance_of(&row) - pending.get(user).copied().unwrap_or(0.0);
    if amount > 0.0 && available + 0.0000001 < amount {
        return Err(format!("余额不足：本次预估需 ¥{amount:.6}，扣除进行中请求后可用 ¥{:.6}", available.max(0.0)));
    }
    *pending.entry(user.into()).or_default() += amount;
    Ok(std::sync::Arc::new(Reservation {pricing,model:model.into(),user:user.into(),amount}))
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    #[test]
    fn concurrent_budgets_release_and_snapshot_settlement() {
        let _lock = crate::store::test_lock();
        let store = crate::store::store(); let before = store.load();
        store.update(|db| {
            db["users"] = json!([{"id":"budget-test","enabled":true,"balance":1.0,"grant":0.0}]);
            db["upstreams"] = json!([{"id":"budget-channel","enabled":true,"prices":{"budget-model":0.6}}]);
        });
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let finish = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads:Vec<_> = (0..8).map(|_| {
            let start=start.clone(); let finish=finish.clone();
            std::thread::spawn(move || {
                start.wait(); let hold=reserve("budget-test","budget-channel","budget-model",&json!({}),10,false);
                finish.wait(); hold.is_ok()
            })
        }).collect();
        assert_eq!(threads.into_iter().filter_map(|t| t.join().ok()).filter(|ok|*ok).count(),1);
        let held = reserve("budget-test","budget-channel","budget-model",&json!({}),10,false).unwrap();
        assert!(reserve("budget-test","budget-channel","budget-model",&json!({}),10,false).is_err());
        store.update(|db|db["upstreams"][0]["prices"]["budget-model"]=json!(0.9));
        let charged=crate::users::bill("budget-test","key","budget-channel","budget-model","upstream-model",200,1,10,20,false,Some(&held.pricing));
        assert_eq!(charged,0.6);
        assert_eq!(crate::users::balance_of(&crate::users::auth_user("budget-test").unwrap()),0.4);
        drop(held);
        assert!(pending().lock().unwrap().get("budget-test").is_none());
        store.update(|db|*db=before);
    }
}
