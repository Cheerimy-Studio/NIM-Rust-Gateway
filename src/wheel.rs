//! 大转盘：多个自定义转盘、按权重抽奖（展示权重 vs 真实权重分离）、
//! 余额类奖品直接入账本，体验卡/专属额度发放独立 prize Key。
//! 消耗优先扣赠金。存储走 wheels 组（wheels/prize_keys/draw_logs 三张表）。

use crate::store::store;
use serde_json::{json, Value};

/// 奖品 Key 前缀（体验卡/专属额度）：与用户 Key 同协议调用 /v1，但独立表鉴权。
pub const PRIZE_KEY_PREFIX: &str = "sk-prz-";

pub fn all_wheels() -> Vec<Value> {
    store()
        .load()
        .get("wheels")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default()
}

fn util_str(v: Option<&Value>) -> String {
    crate::util::str_or(v, "")
}

/// 生成一个 prize Key（体验卡/专属额度的调用凭据），复用用户 Key 前缀但独立表存储。
fn new_prize_key() -> String {
    format!("sk-prz-{}", crate::util::rand_hex(20))
}

// ---------------------------------------------------------------- 管理端

const MAX_MONEY: f64 = 1_000_000_000.0;
const MAX_HOURS: f64 = 876_000.0;

fn number(v: Option<&Value>, default: f64, name: &str, max: f64) -> Result<f64, String> {
    let n = match v {
        None | Some(Value::Null) => default,
        Some(Value::Number(n)) => n.as_f64().ok_or_else(|| format!("{name}格式错误"))?,
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("{name}格式错误"))?,
        _ => return Err(format!("{name}格式错误")),
    };
    if !n.is_finite() || n < 0.0 || n > max {
        return Err(format!("{name}须在 0 至 {max} 之间"));
    }
    Ok(n)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn normalize_wheel(body: &Value) -> Result<Value, String> {
    let name = util_str(body.get("name")).trim().to_string();
    if name.is_empty() || name.chars().count() > 40 {
        return Err("转盘名称须为 1 至 40 个字符".into());
    }
    let cost = number(body.get("cost"), 0.0, "单次消耗", MAX_MONEY)?;
    if cost > 0.0 && crate::util::round6(cost) == 0.0 {
        return Err("单次消耗不能低于 0.000001 元".into());
    }
    let prizes = body
        .get("prizes")
        .and_then(Value::as_array)
        .ok_or("缺少奖品列表")?;
    if prizes.is_empty() || prizes.len() > 24 {
        return Err("奖品数量须为 1 至 24".into());
    }
    let mut ids = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (i, p) in prizes.iter().enumerate() {
        let label = util_str(p.get("label")).trim().to_string();
        if label.is_empty() || label.chars().count() > 40 {
            return Err(format!("第 {} 个奖品名称须为 1 至 40 个字符", i + 1));
        }
        let kind = util_str(p.get("type"));
        if !matches!(
            kind.as_str(),
            "recharge" | "grant" | "model_unlimited" | "model_quota" | "none"
        ) {
            return Err(format!("第 {} 个奖品类型无效", i + 1));
        }
        let mut id = util_str(p.get("id"));
        if id.is_empty() {
            id = format!("pz_{}", crate::util::rand_hex(8));
        }
        if !valid_id(&id) || !ids.insert(id.clone()) {
            return Err("奖品 ID 无效或重复".into());
        }
        let weight = number(p.get("weight"), 0.0, "展示权重", 100.0)?;
        if weight <= 0.0 {
            return Err("展示权重必须大于 0".into());
        }
        let real = number(p.get("real_weight"), weight, "抽取权重", 1_000_000_000.0)?;
        let is_money = matches!(kind.as_str(), "recharge" | "grant");
        let is_key = matches!(kind.as_str(), "model_unlimited" | "model_quota");
        let amount = if is_money {
            crate::util::round6(number(p.get("amount"), 0.0, "奖品金额", MAX_MONEY)?)
        } else {
            0.0
        };
        if is_money && amount <= 0.0 {
            return Err("余额奖品金额不能低于 0.000001 元".into());
        }
        let model = if is_key {
            util_str(p.get("model")).trim().to_string()
        } else {
            String::new()
        };
        if is_key && (model.is_empty() || model.len() > 1024 || model.chars().any(char::is_control))
        {
            return Err("奖品模型名为空、过长或包含控制字符".into());
        }
        let quota = if kind == "model_quota" {
            number(p.get("quota"), 0.0, "奖品次数", 1_000_000_000.0)?
        } else {
            0.0
        };
        if kind == "model_quota" && (quota < 1.0 || quota.fract() != 0.0) {
            return Err("专属额度次数必须为正整数".into());
        }
        let hours = if is_key {
            number(p.get("duration_hours"), 0.0, "有效小时数", MAX_HOURS)?
        } else {
            0.0
        };
        if hours > 0.0 && hours * 3600.0 < 1.0 {
            return Err("有效时长不能低于 1 秒".into());
        }
        let conc = if is_key {
            number(p.get("concurrency"), 1.0, "奖品并发数", 64.0)?
        } else {
            1.0
        };
        if conc < 1.0 || conc.fract() != 0.0 {
            return Err("奖品并发数须为 1 至 64 的整数".into());
        }
        let color = util_str(p.get("color"));
        let color = if matches!(
            color.as_str(),
            "gold" | "blue" | "orange" | "green" | "purple" | "gray"
        ) {
            color
        } else {
            "gray".into()
        };
        out.push(json!({"id":id,"label":label,"type":kind,"color":color,"weight":weight,
            "real_weight":real,"amount":amount,"model":model,"duration_hours":hours,"quota":quota,"concurrency":conc as i64}));
    }
    let total: f64 = out.iter().map(|p| p["weight"].as_f64().unwrap()).sum();
    if (total - 100.0).abs() > 0.01 {
        return Err("展示权重合计必须为 100".into());
    }
    let actual: f64 = out.iter().map(|p| p["real_weight"].as_f64().unwrap()).sum();
    if actual <= 0.0 {
        return Err("至少一个奖品的抽取权重必须大于 0".into());
    }
    let mut id = util_str(body.get("id"));
    if id.is_empty() {
        id = format!("wh_{}", crate::util::rand_hex(8));
    }
    if !valid_id(&id) {
        return Err("活动 ID 无效".into());
    }
    Ok(
        json!({"id":id,"name":name,"enabled":body.get("enabled").map(crate::util::as_bool2).unwrap_or(true),
        "cost":crate::util::round6(cost),"prizes":out,"created_at":crate::util::now_i()}),
    )
}

pub fn save_wheel(body: &Value) -> (Option<Value>, String) {
    let mut row = match normalize_wheel(body) {
        Ok(row) => row,
        Err(e) => return (None, e),
    };
    let result = store().reward_transaction(|db| {
        let arr = db
            .get_mut("wheels")
            .and_then(Value::as_array_mut)
            .ok_or("活动存储异常")?;
        if let Some(pos) = arr.iter().position(|w| w["id"] == row["id"]) {
            row["created_at"] = arr[pos]
                .get("created_at")
                .cloned()
                .unwrap_or(row["created_at"].clone());
            arr[pos] = row.clone();
        } else {
            if !util_str(body.get("id")).is_empty() {
                return Err("活动已删除，请刷新列表".into());
            }
            arr.push(row.clone());
        }
        Ok(row.clone())
    });
    match result {
        Ok(row) => (Some(row), String::new()),
        Err(e) => (None, e),
    }
}

pub fn delete_wheel(id: &str) -> Result<(), String> {
    store().reward_transaction(|db| {
        let arr = db
            .get_mut("wheels")
            .and_then(Value::as_array_mut)
            .ok_or("活动存储异常")?;
        let before = arr.len();
        arr.retain(|w| util_str(w.get("id")) != id);
        if arr.len() == before {
            return Err("活动不存在".into());
        }
        Ok(())
    })
}

/// 兑换码式奖品 Key 列表（管理端）：附转盘名与奖品名。
pub fn list_prize_keys(wheel_id: &str) -> Vec<Value> {
    let wheels = all_wheels();
    let wheel_name = |wid: &str| -> String {
        wheels
            .iter()
            .find(|w| util_str(w.get("id")) == wid)
            .map(|w| util_str(w.get("name")))
            .unwrap_or_default()
    };
    store()
        .load()
        .get("prize_keys")
        .and_then(|k| k.as_array())
        .map(|a| {
            a.iter()
                .filter(|k| wheel_id.is_empty() || util_str(k.get("wheel_id")) == wheel_id)
                .map(|k| {
                    let mut row = k.clone();
                    if let Some(o) = row.as_object_mut() {
                        o.insert(
                            "wheel_name".into(),
                            json!(wheel_name(&util_str(k.get("wheel_id")))),
                        );
                    }
                    row
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 用户端

/// 用户端可见转盘：只给展示信息（标签/颜色/展示权重），绝不泄漏 real_weight。
/// percent = 该奖品展示权重占全部展示权重的比例（旧数据权重和不为 100 时也能正确显示）。
pub fn user_wheels() -> Vec<Value> {
    all_wheels()
        .into_iter()
        .filter(|w| w.get("enabled").map(crate::util::as_bool2).unwrap_or(false))
        .map(|w| public_wheel(&w))
        .collect()
}

fn public_wheel(w: &Value) -> Value {
    let raw = w
        .get("prizes")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let actual_total: f64 = raw
        .iter()
        .map(|p| {
            crate::util::f64_or(
                p.get("real_weight"),
                crate::util::f64_or(p.get("weight"), 0.0),
            )
            .max(0.0)
        })
        .sum();
    let total: f64 = raw
        .iter()
        .map(|p| crate::util::f64_or(p.get("weight"), 0.0).max(0.0))
        .sum();
    let prizes: Vec<Value> = raw
        .iter()
        .map(|p| {
            let weight = crate::util::f64_or(p.get("weight"), 0.0).max(0.0);
            let percent = if total > 0.0 {
                weight / total * 100.0
            } else {
                0.0
            };
            json!({
                "id": util_str(p.get("id")),
                "label": util_str(p.get("label")),
                "type": util_str(p.get("type")),
                "color": util_str(p.get("color")),
                "weight": weight,
                "percent": percent,
                "chance": if actual_total > 0.0 { crate::util::f64_or(p.get("real_weight"),weight).max(0.0) / actual_total * 100.0 } else {0.0},
                "amount": crate::util::round6(crate::util::f64_or(p.get("amount"), 0.0)),
                "model": util_str(p.get("model")),
                "duration_hours": crate::util::f64_or(p.get("duration_hours"), 0.0),
                "quota": crate::util::round6(crate::util::f64_or(p.get("quota"), 0.0)),
                "concurrency": crate::util::int_or(p.get("concurrency"), 1),
            })
        })
        .collect();
    json!({
        "id": util_str(w.get("id")),
        "name": util_str(w.get("name")),
        "cost": crate::util::f64_or(w.get("cost"), 0.0),
        "prizes": prizes,
        "server_time":crate::util::now_i(),
    })
}

/// 按配置的抽取权重选择奖品。
fn pick_prize(w: &Value) -> Option<Value> {
    let prizes = w.get("prizes")?.as_array()?;
    let total: f64 = prizes
        .iter()
        .map(|p| {
            crate::util::f64_or(
                p.get("real_weight"),
                crate::util::f64_or(p.get("weight"), 1.0),
            )
            .max(0.0)
        })
        .sum();
    if total <= 0.0 {
        return None;
    }
    let mut roll = rand::Rng::gen::<f64>(&mut rand::thread_rng()) * total;
    for p in prizes {
        roll -= crate::util::f64_or(
            p.get("real_weight"),
            crate::util::f64_or(p.get("weight"), 1.0),
        )
        .max(0.0);
        if roll < 0.0 {
            return Some(p.clone());
        }
    }
    prizes
        .iter()
        .rev()
        .find(|p| {
            crate::util::f64_or(
                p.get("real_weight"),
                crate::util::f64_or(p.get("weight"), 1.0),
            ) > 0.0
        })
        .cloned()
}

/// 扣抽奖费用：优先赠金。返回 Ok(扣的赠金, 扣的充值) / Err(提示)。
fn charge_cost(user: &mut Value, cost: f64) -> Result<(f64, f64), String> {
    if cost <= 0.0 {
        return Ok((0.0, 0.0));
    }
    let available = crate::users::balance_of(user);
    if available < cost - 1e-9 {
        return Err("余额不足，无法抽奖".into());
    }
    Ok(crate::users::debit_bill(user, cost))
}

/// 扣费、发奖、抵扣次数与回执在同一持久化事务内提交。
pub fn draw(
    uid: &str,
    wheel_id: &str,
    use_credit: bool,
    request_id: &str,
    expected_cost: Option<f64>,
    requested_at: Option<i64>,
) -> (bool, Value) {
    if !request_id.is_empty() && !valid_id(request_id) {
        return (false, json!({"error":{"message":"抽奖请求 ID 无效"}}));
    }
    let result = store().reward_transaction(|db| {
        let now = crate::util::now_i();
        let user_index = db.get("users").and_then(Value::as_array)
            .and_then(|a| a.iter().position(|u| util_str(u.get("id")) == uid && u.get("enabled").map(crate::util::as_bool2).unwrap_or(false)))
            .ok_or("用户不存在或已停用")?;
        // 相同用户、相同请求重放原结果，不再扣费或消耗抵扣次数。
        if !request_id.is_empty() {
            if let Some(r) = db.get("draw_receipts").and_then(Value::as_array).and_then(|a| a.iter().find(|r|
                r["user_id"] == uid && r["request_id"] == request_id && crate::util::int_or(r.get("t"),0) >= now - 86400)) {
                if r["wheel_id"] != wheel_id { return Err("抽奖请求 ID 已用于其他活动".into()); }
                return Ok(r["result"].clone());
            }
        }
        if let Some(at) = requested_at {
            let floor = crate::util::int_or(db["users"][user_index].get("draw_replay_floor"),0);
            if at < now - 86400 || at > now + 300 || at <= floor {
                return Err("抽奖请求已过期，请先查看抽奖记录并刷新活动".into());
            }
        }
        let original = db.get("wheels").and_then(Value::as_array).and_then(|a| a.iter().find(|w| w["id"] == wheel_id)).ok_or("活动不存在")?;
        if !original.get("enabled").map(crate::util::as_bool2).unwrap_or(false) { return Err("活动未开启".into()); }
        let w = normalize_wheel(original)?;
        let prize = pick_prize(&w).ok_or("活动奖品配置无效")?;
        let kind = util_str(prize.get("type"));
        let amount = crate::util::f64_or(prize.get("amount"),0.0);
        let cost = crate::util::f64_or(w.get("cost"),0.0);
        if expected_cost.is_some_and(|expected| !expected.is_finite() || expected != cost) {
            return Err("活动费用已变化，请刷新活动后重试".into());
        }
        let hours = crate::util::f64_or(prize.get("duration_hours"),0.0);
        let expires = if hours > 0.0 { now.checked_add((hours * 3600.0) as i64).ok_or("奖品时长过长")? } else { 0 };
        // 先验证所有目标表。后续失败时事务副本整体丢弃。
        for key in ["prize_keys","draw_logs","invite_members"] {
            let obj = db.as_object_mut().ok_or("存储异常")?;
            if !obj.entry(key).or_insert_with(|| json!([])).is_array() { return Err("奖励存储异常".into()); }
        }
        let used_credit = use_credit && cost > 0.0 && crate::promo::consume_draw_credit_obj(db.as_object_mut().unwrap(),uid);
        let doubled = matches!(kind.as_str(),"recharge"|"grant") && crate::promo::consume_doubler_obj(db.as_object_mut().unwrap(),uid);
        let paid = if used_credit {0.0} else {cost};
        let user = &mut db["users"][user_index];
        let (cg,cr) = charge_cost(user,paid)?;
        let credited = if doubled { amount * 2.0 } else {amount};
        if credited > 0.0 { crate::users::credit(user,&kind,credited); }
        let wallet = crate::users::wallet_row(user);
        if cg > 0.0 || cr > 0.0 { crate::users::fund_log(db,uid,"draw",-cg,-cr,&util_str(w.get("name"))); }
        if credited > 0.0 {
            let (g,r) = if kind == "grant" {(credited,0.0)} else {(0.0,credited)};
            crate::users::fund_log(db,uid,"prize",g,r,&util_str(prize.get("label")));
        }
        let mut award = json!({"id":prize["id"],"type":kind,"label":prize["label"],"color":prize["color"]});
        if matches!(kind.as_str(),"recharge"|"grant") {
            award["amount"] = json!(credited);
            award["doubled"] = json!(doubled);
        } else if matches!(kind.as_str(),"model_unlimited"|"model_quota") {
            let key = new_prize_key();
            let row = json!({"id":format!("pk_{}",crate::util::rand_hex(8)),"user_id":uid,"wheel_id":wheel_id,
                "prize_id":prize["id"],"key":key,"type":kind,"model":prize["model"],"concurrency":prize["concurrency"],
                "quota":prize["quota"],"used":0,"created_at":now,"expires_at":expires,"enabled":true});
            db["prize_keys"].as_array_mut().unwrap().push(row);
            for field in ["model","quota","duration_hours","concurrency"] { award[field] = prize[field].clone(); }
            award["key"] = json!(key);
            award["expires_at"] = json!(expires);
        }
        let log = json!({"t":now,"user_id":uid,"wheel_id":wheel_id,"wheel_name":w["name"],"prize_id":prize["id"],
            "label":prize["label"],"type":kind,"cost":paid,"cost_grant":cg,"cost_recharge":cr,
            "used_credit":used_credit,"doubled":doubled});
        let logs = db["draw_logs"].as_array_mut().unwrap();
        logs.insert(0,log);
        // 各用户分别保留，避免其他用户的抽奖挤掉全部个人历史。
        let mut seen = std::collections::HashMap::<String,usize>::new();
        logs.retain(|r| { let n=seen.entry(util_str(r.get("user_id"))).or_default(); *n+=1; *n<=50 });
        let result = json!({"ok":true,"prize":award,"wallet":wallet,"used_credit":used_credit,"cost":{"grant":cg,"recharge":cr},
            "request_id":request_id,"wheel":public_wheel(&w)});
        if !request_id.is_empty() {
            let receipts = db.as_object_mut().unwrap().entry("draw_receipts").or_insert_with(|| json!([])).as_array_mut().ok_or("回执存储异常")?;
            receipts.insert(0,json!({"t":now,"user_id":uid,"wheel_id":wheel_id,"request_id":request_id,"requested_at":requested_at.unwrap_or(now),"result":result}));
            let mut seen = std::collections::HashMap::<String,usize>::new();
            let mut floors = std::collections::HashMap::<String,i64>::new();
            receipts.retain(|r| {
                let owner=util_str(r.get("user_id"));
                let n=seen.entry(owner.clone()).or_default(); *n+=1;
                let keep=crate::util::int_or(r.get("t"),0) >= now - 86400 && *n<=100;
                if !keep {
                    let at=crate::util::int_or(r.get("requested_at"),crate::util::int_or(r.get("t"),0));
                    floors.entry(owner).and_modify(|floor|*floor=(*floor).max(at)).or_insert(at);
                }
                keep
            });
            if let Some(users)=db.get_mut("users").and_then(Value::as_array_mut) {
                for user in users {
                    if let Some(floor)=floors.get(&util_str(user.get("id"))) {
                        user["draw_replay_floor"]=json!((*floor).max(crate::util::int_or(user.get("draw_replay_floor"),0)));
                    }
                }
            }
        }
        Ok(result)
    });
    match result {
        Ok(body) => (true, body),
        Err(message) => (false, json!({"error":{"message":message}})),
    }
}

/// 用户的奖品 Key 列表。
pub fn user_prize_keys(uid: &str) -> Vec<Value> {
    store()
        .load()
        .get("prize_keys")
        .and_then(|k| k.as_array())
        .map(|a| {
            a.iter()
                .filter(|k| util_str(k.get("user_id")) == uid)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 用户抽奖记录（最近 50 条）。
pub fn user_draw_logs(uid: &str) -> Vec<Value> {
    store()
        .load()
        .get("draw_logs")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| util_str(r.get("user_id")) == uid)
                .take(50)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- Prize Key 鉴权与调用链

/// Bearer sk-prz-… → 启用中的 (prize_key 行)。检查：存在、启用、未过期、
/// model_quota 未超次数。返回 (row, err_msg)。
pub fn lookup_prize_key(key: &str) -> Result<Value, &'static str> {
    let now = crate::util::now_i();
    let db = store().load();
    let rows = db
        .get("prize_keys")
        .and_then(|k| k.as_array())
        .ok_or("prize key 无效")?;
    use subtle::ConstantTimeEq;
    // 奖品 Key 等同秘密令牌：比较走常量时间
    let row = rows
        .iter()
        .find(|k| {
            let m: bool = util_str(k.get("key"))
                .as_bytes()
                .ct_eq(key.as_bytes())
                .into();
            m
        })
        .ok_or("prize key 无效")?;
    if !row
        .get("enabled")
        .map(crate::util::as_bool2)
        .unwrap_or(false)
    {
        return Err("prize key 已停用");
    }
    let expires = crate::util::int_or(row.get("expires_at"), 0);
    if expires > 0 && now >= expires {
        return Err("prize key 已过期");
    }
    let kind = util_str(row.get("type"));
    let quota = crate::util::f64_or(row.get("quota"), 0.0);
    if kind == "model_quota" {
        if quota < 1.0 || quota.fract() != 0.0 {
            return Err("prize key 次数配置无效");
        }
        if crate::util::f64_or(row.get("used"), 0.0) >= quota {
            return Err("prize key 额度已用尽");
        }
    } else if kind != "model_unlimited" {
        return Err("prize key 类型无效");
    }
    let owner = util_str(row.get("user_id"));
    let user_ok = db
        .get("users")
        .and_then(|u| u.as_array())
        .map(|a| {
            a.iter().any(|u| {
                util_str(u.get("id")) == owner
                    && u.get("enabled").map(crate::util::as_bool2).unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if !user_ok {
        return Err("prize key 所属用户已停用");
    }
    Ok(row.clone())
}

#[derive(Default)]
struct PrizeState {
    active: i64,
    blocked: bool,
}

static PRIZE_INFLIGHT: std::sync::Mutex<Option<std::collections::HashMap<String, PrizeState>>> =
    std::sync::Mutex::new(None);

/// 在同一锁下复验有效期、用户状态、可用次数及并发数。剩余次数包含所有在途预留。
pub fn prize_key_acquire(key: &str) -> Result<(), &'static str> {
    let mut guard = PRIZE_INFLIGHT.lock().unwrap();
    let row = lookup_prize_key(key)?;
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    let state = map.entry(key.to_string()).or_default();
    if state.blocked {
        return Err("奖品 Key 计数存储异常，请联系管理员");
    }
    let limit = crate::util::int_or(row.get("concurrency"), 1).clamp(1, 64);
    if state.active >= limit {
        return Err("奖品 Key 并发已满，请稍后重试");
    }
    let quota = crate::util::f64_or(row.get("quota"), 0.0);
    let used = crate::util::f64_or(row.get("used"), 0.0).max(0.0);
    if util_str(row.get("type")) == "model_quota" && used + state.active as f64 >= quota {
        return Err("奖品 Key 剩余次数已用完或正在使用");
    }
    state.active += 1;
    Ok(())
}

pub fn prize_key_release(key: &str) {
    let mut guard = PRIZE_INFLIGHT.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if let Some(state) = map.get_mut(key) {
        state.active = state.active.saturating_sub(1).max(0);
        if state.active == 0 && !state.blocked {
            map.remove(key);
        }
    }
}

/// 仅成功调用提交计数。持久化失败时暂停该 Key，避免重复使用未能保存的额度。
pub fn prize_key_consume(key: &str) {
    let result = store().reward_transaction(|db| {
        let row = db
            .get_mut("prize_keys")
            .and_then(Value::as_array_mut)
            .and_then(|a| a.iter_mut().find(|k| k["key"] == key))
            .ok_or("奖品 Key 已删除")?;
        if util_str(row.get("type")) == "model_quota" {
            let used = crate::util::f64_or(row.get("used"), 0.0).max(0.0);
            row["used"] = json!(used + 1.0);
        }
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("[wheel] 奖品调用计数提交失败，暂停该 Key: {e}");
        PRIZE_INFLIGHT
            .lock()
            .unwrap()
            .get_or_insert_with(std::collections::HashMap::new)
            .entry(key.to_string())
            .or_default()
            .blocked = true;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn config() -> Value {
        json!({"name":"测试活动","enabled":true,"cost":1,
            "prizes":[{"id":"grant","label":"赠金","type":"grant","weight":100,"amount":0.2}]})
    }

    #[test]
    fn rejects_invalid_reward_numbers_and_duplicate_prizes() {
        for (field, value) in [
            ("cost", json!("NaN")),
            ("cost", json!("inf")),
            ("cost", json!(-1)),
            ("cost", json!(0.00000001)),
        ] {
            let mut body = config();
            body[field] = value;
            assert!(normalize_wheel(&body).is_err(), "{body}");
        }
        for (field, value) in [
            ("amount", json!(0.00000001)),
            ("real_weight", json!(0)),
            ("weight", json!("NaN")),
        ] {
            let mut body = config();
            body["prizes"][0][field] = value;
            assert!(normalize_wheel(&body).is_err(), "{body}");
        }
        let mut body = config();
        body["prizes"][0]["weight"] = json!(50);
        let duplicate = body["prizes"][0].clone();
        body["prizes"].as_array_mut().unwrap().push(duplicate);
        assert!(normalize_wheel(&body).is_err());
    }

    #[test]
    fn key_rewards_require_whole_quotas_and_safe_durations() {
        let mut body = config();
        body["prizes"][0]["type"] = json!("model_quota");
        body["prizes"][0]["model"] = json!("123");
        for quota in [0.0, 0.5, 1.5, 1e30] {
            body["prizes"][0]["quota"] = json!(quota);
            assert!(normalize_wheel(&body).is_err());
        }
        body["prizes"][0]["quota"] = json!(2);
        assert!(
            normalize_wheel(&body).is_ok(),
            "numeric model identifiers are valid"
        );
        body["prizes"][0]["duration_hours"] = json!(1e300);
        assert!(normalize_wheel(&body).is_err());
        body["prizes"][0]["duration_hours"] = json!(0.000001);
        assert!(normalize_wheel(&body).is_err());
        body["prizes"][0]["duration_hours"] = json!(0);
        body["prizes"][0]["type"] = json!("model_unlimited");
        body["prizes"][0]["quota"] = json!(10);
        assert_eq!(normalize_wheel(&body).unwrap()["prizes"][0]["quota"], 0.0);
        body["prizes"][0]["model"] = json!("hello\nworld");
        assert!(normalize_wheel(&body).is_err());
    }

    #[test]
    fn probabilities_match_sampling_without_exposing_raw_weights() {
        let mut body = config();
        body["prizes"][0]["weight"] = json!(50);
        body["prizes"][0]["real_weight"] = json!(0);
        body["prizes"].as_array_mut().unwrap().push(
            json!({"id":"none","label":"未中奖","type":"none","weight":50,"real_weight":100}),
        );
        let w = normalize_wheel(&body).unwrap();
        let public = public_wheel(&w);
        assert_eq!(public["prizes"][0]["percent"], 50.0);
        assert_eq!(public["prizes"][0]["chance"], 0.0);
        assert_eq!(public["prizes"][1]["chance"], 100.0);
        assert!(public["prizes"][0].get("real_weight").is_none());
        for _ in 0..100 {
            assert_eq!(pick_prize(&w).unwrap()["id"], "none");
        }
    }

    /// 在现有 HTTP 集成测试的数据目录中串行运行，避免并行测试改变全局存储路径。
    pub fn reward_regressions() {
        let original = store().load();
        store().update(|db| {
            db["users"]=json!([{"id":"reward-user","enabled":true,"balance":1,"grant":0.6,"grant_total":0.6,"recharge_total":1}]);
            db["wheels"]=json!([]); db["prize_keys"]=json!([]); db["draw_logs"]=json!([]); db["draw_receipts"]=json!([]); db["fund_logs"]=json!([]);
            db["invite_events"]=json!([{"id":"live","enabled":true}]);
            db["invite_members"]=json!([{"event_id":"live","user_id":"reward-user","draw_credits":1,"doubler":1}]);
            db["config"]["sign_enabled"]=json!(true); db["config"]["sign_min"]=json!(0.000001); db["config"]["sign_max"]=json!(0.000001);
        });
        let w = save_wheel(&config()).0.unwrap();
        let id = w["id"].as_str().unwrap();
        let before = store().load();
        assert!(!draw("reward-user", id, false, "stale", Some(0.5), None).0);
        assert_eq!(store().load(), before, "stale price must not debit");
        let (ok, body) = draw("reward-user", id, false, "paid", Some(1.0), None);
        assert!(ok, "{body}");
        assert_eq!(body["prize"]["amount"], 0.4);
        assert_eq!(body["cost"]["grant"], 0.6);
        assert_eq!(body["cost"]["recharge"], 0.4);
        assert_eq!(body["wallet"]["grant"], 0.4);
        assert_eq!(body["wallet"]["recharge"], 0.6);
        let after = store().load();
        assert_eq!(
            draw("reward-user", id, false, "paid", Some(1.0), None).1,
            body
        );
        assert_eq!(
            store().load(),
            after,
            "replay must not consume money or doubler"
        );
        let (ok, body) = draw("reward-user", id, true, "credit", Some(1.0), None);
        assert!(ok, "{body}");
        assert_eq!(body["used_credit"], true);
        assert_eq!(body["cost"]["grant"], 0.0);
        assert_eq!(body["cost"]["recharge"], 0.0);
        assert_eq!(store().load()["draw_logs"][0]["cost"], 0.0);
        // A malformed destination discovered after charging must roll back everything.
        store().update(|db| db["draw_receipts"] = json!({}));
        let before = store().load();
        assert!(!draw("reward-user", id, false, "bad-storage", Some(1.0), None).0);
        assert_eq!(store().load(), before);
        store().update(|db| {
            db["draw_receipts"] = json!([]);
            db["users"][0]["grant"] = json!(100);
        });
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let barrier = barrier.clone();
                let id = id.to_string();
                std::thread::spawn(move || {
                    barrier.wait();
                    draw(
                        "reward-user",
                        &id,
                        false,
                        "same-concurrent",
                        Some(1.0),
                        None,
                    )
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(results.iter().all(|r| r.0 && r.1 == results[0].1));
        assert_eq!(store().load()["users"][0]["grant"], 99.2);
        // Concurrent sign-in has exactly one grant and persists before reporting success.
        let threads: Vec<_> = (0..16)
            .map(|_| std::thread::spawn(|| crate::users::sign_today("reward-user")))
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(|r| r.0)
                .count(),
            1
        );
        assert_eq!(store().load()["users"][0]["grant"], 99.200001);
        store().discard_memory();
        assert!(!crate::users::sign_today("reward-user").0);
        assert_eq!(store().load()["users"][0]["grant"], 99.200001);
        // Corrupt legacy sign files cannot be treated as empty.
        store().update(|db| {
            db["users"][0]
                .as_object_mut()
                .unwrap()
                .remove("last_sign_day");
        });
        let day = crate::util::local_day(crate::util::now_i());
        std::fs::create_dir_all(crate::store::signs_dir()).unwrap();
        let sign = crate::store::signs_dir().join(format!("sign_{day}.json"));
        std::fs::write(&sign, b"{broken").unwrap();
        let before = store().load();
        assert!(!crate::users::sign_today("reward-user").0);
        assert!(crate::users::sign_record(&before, "reward-user", &day).is_err());
        assert_eq!(store().load(), before);
        std::fs::write(&sign, br#"{"reward-user":0.1}"#).unwrap();
        assert!(!crate::users::sign_today("reward-user").0);
        std::fs::remove_file(sign).unwrap();
        store().update(|db| db["users"][0]["enabled"] = json!(false));
        assert!(!crate::users::sign_today("reward-user").0);
        let before = store().load();
        assert!(!draw("reward-user", id, true, "disabled", Some(1.0), None).0);
        assert_eq!(store().load(), before);
        store().update(|db| db["users"][0]["enabled"] = json!(true));
        // Awarded limited keys reserve their remaining calls, not just a concurrency slot.
        let mut key_wheel = config();
        key_wheel["prizes"][0]["type"] = json!("model_quota");
        key_wheel["prizes"][0]["model"] = json!("public");
        key_wheel["prizes"][0]["quota"] = json!(2);
        key_wheel["prizes"][0]["concurrency"] = json!(64);
        let key_wheel = save_wheel(&key_wheel).0.unwrap();
        let (ok, result) = draw(
            "reward-user",
            key_wheel["id"].as_str().unwrap(),
            false,
            "key-award",
            Some(1.0),
            None,
        );
        assert!(ok, "{result}");
        let key = result["prize"]["key"].as_str().unwrap().to_string();
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let key = key.clone();
                std::thread::spawn(move || prize_key_acquire(&key).is_ok())
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(|ok| *ok)
                .count(),
            2
        );
        prize_key_consume(&key);
        prize_key_release(&key);
        assert!(
            prize_key_acquire(&key).is_err(),
            "one remaining call is already reserved"
        );
        prize_key_release(&key); // failed request releases its reservation
        assert!(prize_key_acquire(&key).is_ok());
        prize_key_consume(&key);
        prize_key_release(&key);
        assert!(prize_key_acquire(&key).is_err());
        assert_eq!(store().load()["prize_keys"][0]["used"], 2.0);
        store().update(|db| {
            db["prize_keys"][0]["type"] = json!("model_unlimited");
            db["prize_keys"][0]["concurrency"] = json!(1);
        });
        assert!(
            lookup_prize_key(&key).is_ok(),
            "obsolete quota on unlimited card must not limit it"
        );
        assert!(prize_key_acquire(&key).is_ok());
        assert!(prize_key_acquire(&key).is_err());
        prize_key_release(&key);
        store().update(|db| db["prize_keys"][0]["expires_at"] = json!(crate::util::now_i() - 1));
        assert!(prize_key_acquire(&key).is_err());
        // 被回执上限清理的请求不能作为新抽奖再次扣费。
        let now = crate::util::now_i();
        store().update(|db| {
            db["draw_receipts"] = Value::Array(
                (0..100)
                    .map(|i| {
                        json!({"user_id":"reward-user","wheel_id":id,
                "request_id":format!("archived_{i}"),"t":now-i,"requested_at":now-3600,"result":{}})
                    })
                    .collect(),
            );
        });
        assert!(
            draw(
                "reward-user",
                id,
                false,
                "fresh-after-prune",
                Some(1.0),
                Some(now)
            )
            .0
        );
        let before = store().load();
        assert!(
            !draw(
                "reward-user",
                id,
                false,
                "archived_99",
                Some(1.0),
                Some(now - 3600)
            )
            .0
        );
        assert!(
            !draw(
                "reward-user",
                id,
                false,
                "expired-request",
                Some(1.0),
                Some(now - 86401)
            )
            .0
        );
        assert_eq!(store().load(), before);
        store().update(|db| *db = original);
        store().flush();
    }
}
