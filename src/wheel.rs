//! 大转盘：多个自定义转盘、按权重抽奖（展示权重 vs 真实权重分离）、
//! 余额类奖品直接入账本，体验卡/专属额度发放独立 prize Key。
//! 消耗优先扣赠金。存储走 wheels 组（wheels/prize_keys/draw_logs 三张表）。

use crate::store::{store, Obj};
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

pub fn get_wheel(id: &str) -> Option<Value> {
    all_wheels().into_iter().find(|w| util_str(w.get("id")) == id)
}

fn util_str(v: Option<&Value>) -> String {
    crate::util::str_or(v, "")
}

/// 生成一个 prize Key（体验卡/专属额度的调用凭据），复用用户 Key 前缀但独立表存储。
fn new_prize_key() -> String {
    format!("sk-prz-{}", crate::util::rand_hex(20))
}

// ---------------------------------------------------------------- 管理端

/// 新建/保存转盘。结构：
/// { id?, name, enabled, cost, prizes: [{ id?, label, type, color, weight(展示), real_weight(暗改,缺省=weight),
///    amount(余额类), model(模型类), duration_hours(体验卡), quota(专属额度次数), concurrency(体验卡并发) }] }
pub fn save_wheel(body: &Value) -> (Option<Value>, String) {
    let name = util_str(body.get("name")).trim().to_string();
    if name.is_empty() || name.chars().count() > 40 {
        return (None, "转盘名称不能为空且不超过 40 字符".into());
    }
    let cost = crate::util::f64_or(body.get("cost"), 0.0);
    if cost < 0.0 {
        return (None, "单次消耗不能为负数".into());
    }
    let Some(prizes) = body.get("prizes").and_then(|p| p.as_array()) else {
        return (None, "缺少奖品列表".into());
    };
    if prizes.is_empty() || prizes.len() > 24 {
        return (None, "奖品数量须为 1~24".into());
    }
    let mut out_prizes: Vec<Value> = Vec::new();
    for (i, p) in prizes.iter().enumerate() {
        let label = util_str(p.get("label")).trim().to_string();
        if label.is_empty() || label.chars().count() > 40 {
            return (None, format!("第 {} 个奖品名称不能为空且不超过 40 字符", i + 1));
        }
        let ptype = match util_str(p.get("type")).as_str() {
            "recharge" | "grant" | "model_unlimited" | "model_quota" | "none" => util_str(p.get("type")),
            _ => return (None, format!("第 {} 个奖品类型未知", i + 1)),
        };
        if ptype == "recharge" || ptype == "grant" {
            if crate::util::f64_or(p.get("amount"), 0.0) <= 0.0 {
                return (None, format!("第 {} 个余额类奖品必须填写金额", i + 1));
            }
        }
        if ptype == "model_unlimited" || ptype == "model_quota" {
            let model = util_str(p.get("model"));
            if model.is_empty() {
                return (None, format!("第 {} 个 Key 类奖品必须填写模型名", i + 1));
            }
            // 纯数字模型名 = 旧版回显 bug 写坏的数据（input 显示 amount 0 被原样保存）。
            // 真实模型名一定含字母/连字符；拦下来强制用户改回真实模型，杜绝再次静默落库
            if model.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return (
                    None,
                    format!("第 {} 个奖品的模型名是纯数字（数据可能被旧版显示问题覆盖），请重新填写模型名", i + 1),
                );
            }
        }
        if ptype == "model_quota" && crate::util::f64_or(p.get("quota"), 0.0) <= 0.0 {
            return (None, format!("第 {} 个专属额度奖品必须填写次数", i + 1));
        }
        let weight = crate::util::f64_or(p.get("weight"), 0.0);
        if weight <= 0.0 {
            return (None, format!("第 {} 个奖品展示权重必须大于 0", i + 1));
        }
        let real_weight = if p.get("real_weight").is_some() {
            let rw = crate::util::f64_or(p.get("real_weight"), weight);
            if rw < 0.0 {
                return (None, format!("第 {} 个奖品真实权重不能为负", i + 1));
            }
            rw
        } else {
            weight
        };
        let prize = json!({
            "id": if util_str(p.get("id")).is_empty() { format!("pz_{}", crate::util::rand_hex(5)) } else { util_str(p.get("id")) },
            "label": crate::util::str_cut(&label, 40),
            "type": ptype,
            "color": crate::util::str_cut(&util_str(p.get("color")), 16),
            "weight": weight,
            "real_weight": real_weight,
            "amount": crate::util::f64_or(p.get("amount"), 0.0).max(0.0),
            "model": crate::util::str_cut(&util_str(p.get("model")), 80),
            "duration_hours": crate::util::f64_or(p.get("duration_hours"), 0.0).max(0.0),
            "quota": crate::util::f64_or(p.get("quota"), 0.0).max(0.0),
            "concurrency": crate::util::int_or(p.get("concurrency"), 1).clamp(1, 64),
        });
        out_prizes.push(prize);
    }
    let id_in = util_str(body.get("id"));
    // 展示权重总和必须为 100（前端转盘与「概率」列直接以百分比呈现）
    let weight_sum: f64 = out_prizes
        .iter()
        .map(|p| crate::util::f64_or(p.get("weight"), 0.0))
        .sum();
    if (weight_sum - 100.0).abs() > 0.01 {
        return (
            None,
            format!("展示权重总和必须为 100，当前为 {}", crate::util::round6(weight_sum)),
        );
    }
    let row = json!({
        "id": if id_in.is_empty() { format!("wh_{}", crate::util::rand_hex(5)) } else { id_in },
        "name": crate::util::str_cut(&name, 40),
        "enabled": body.get("enabled").map(crate::util::truthy).unwrap_or(true),
        "cost": crate::util::round6(cost),
        "prizes": out_prizes,
        "created_at": crate::util::now_i(),
    });
    let row2 = row.clone();
    let wid = util_str(row.get("id"));
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        let arr = obj.entry("wheels").or_insert_with(|| Value::Array(vec![]));
        if let Some(a) = arr.as_array_mut() {
            if let Some(pos) = a.iter().position(|w| util_str(w.get("id")) == wid) {
                a[pos] = row2.clone();
            } else {
                a.push(row2.clone());
            }
        }
    });
    (Some(row), String::new())
}

pub fn delete_wheel(id: &str) -> bool {
    let mut ok = false;
    store().update(|db| {
        let obj = db.as_object_mut().unwrap();
        if let Some(a) = obj.get_mut("wheels").and_then(|w| w.as_array_mut()) {
            let before = a.len();
            a.retain(|w| util_str(w.get("id")) != id);
            ok = a.len() < before;
        }
    });
    ok
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
        .filter(|w| w.get("enabled").map(crate::util::truthy).unwrap_or(false))
        .map(|w| {
            let raw = w.get("prizes").and_then(|p| p.as_array()).cloned().unwrap_or_default();
            let total: f64 = raw
                .iter()
                .map(|p| crate::util::f64_or(p.get("weight"), 0.0).max(0.0))
                .sum();
            let prizes: Vec<Value> = raw
                .iter()
                .map(|p| {
                    let weight = crate::util::f64_or(p.get("weight"), 0.0).max(0.0);
                    let percent = if total > 0.0 { (weight / total * 100.0 * 100.0).round() / 100.0 } else { 0.0 };
                    json!({
                        "id": util_str(p.get("id")),
                        "label": util_str(p.get("label")),
                        "type": util_str(p.get("type")),
                        "color": util_str(p.get("color")),
                        "weight": weight,
                        "percent": percent,
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
            })
        })
        .collect()
}

/// 按真实权重抽奖（暗改核心：展示权重只用于前端转盘渲染）。
fn pick_prize(w: &Value) -> Option<Value> {
    let prizes = w.get("prizes")?.as_array()?;
    let total: f64 = prizes
        .iter()
        .map(|p| crate::util::f64_or(p.get("real_weight"), crate::util::f64_or(p.get("weight"), 1.0)).max(0.0))
        .sum();
    if total <= 0.0 {
        return None;
    }
    let mut roll = rand::Rng::gen::<f64>(&mut rand::thread_rng()) * total;
    for p in prizes {
        roll -= crate::util::f64_or(p.get("real_weight"), crate::util::f64_or(p.get("weight"), 1.0)).max(0.0);
        if roll < 0.0 {
            return Some(p.clone());
        }
    }
    prizes.last().cloned()
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

/// 用户抽奖主流程。返回 (http_ok, body)。
/// 单次 store().update 完成「校验 → 扣费 → 发奖 → 记流水」，避免扣了费却没发奖的中间态。
pub fn draw(uid: &str, wheel_id: &str) -> (bool, Value) {
    let Some(w) = get_wheel(wheel_id) else {
        return (false, json!({"error": {"message": "活动不存在"}}));
    };
    if !w.get("enabled").map(crate::util::truthy).unwrap_or(false) {
        return (false, json!({"error": {"message": "活动未开启"}}));
    }
    let cost = crate::util::f64_or(w.get("cost"), 0.0);
    let Some(mut prize) = pick_prize(&w) else {
        return (false, json!({"error": {"message": "活动奖品配置无效"}}));
    };
    let mut ptype = util_str(prize.get("type"));
    let mut amount = crate::util::round6(crate::util::f64_or(prize.get("amount"), 0.0));
    let mut model = util_str(prize.get("model"));
    let mut hours = crate::util::f64_or(prize.get("duration_hours"), 0.0);
    let mut quota = crate::util::round6(crate::util::f64_or(prize.get("quota"), 0.0));
    let mut concurrency = crate::util::int_or(prize.get("concurrency"), 1).clamp(1, 64);
    // 奖品配置预校验：余额类必须为正金额，Key 类必须有模型（避免扣费后发不出奖）
    if matches!(ptype.as_str(), "recharge" | "grant") && amount <= 0.0 {
        return (false, json!({"error": {"message": "奖品未配置有效金额"}}));
    }
    if matches!(ptype.as_str(), "model_unlimited" | "model_quota") && model.is_empty() {
        return (false, json!({"error": {"message": "奖品未配置模型"}}));
    }
    let now = crate::util::now_i();
    let key = new_prize_key();
    let pk_id = format!("pk_{}", crate::util::rand_hex(5));
    // Key 类奖品（体验卡/专属额度）都支持时长：填了小时数即到期失效
    let mut expires_at = if matches!(ptype.as_str(), "model_unlimited" | "model_quota") && hours > 0.0 {
        now + (hours * 3600.0) as i64
    } else {
        0
    };
    let mut log = json!({
        "t": now,
        "user_id": uid,
        "wheel_id": util_str(w.get("id")),
        "wheel_name": util_str(w.get("name")),
        "prize_id": util_str(prize.get("id")),
        "label": util_str(prize.get("label")),
        "type": ptype,
        "cost": crate::util::round6(cost),
    });
    let mut err = String::new();
    let mut cost_from_grant = 0.0f64;
    let mut cost_from_recharge = 0.0f64;
    let mut awarded = false;
    store().update(|db| {
        let Some(obj) = db.as_object_mut() else {
            err = "存储异常".into();
            return;
        };
        // 0) 临界区内复验转盘状态：预读与扣费之间转盘可能被停用/改价，
        //    沿用过期快照会按旧价扣费发旧奖。状态有变则本次拒绝（未扣费）。
        let mut w_now_val = Value::Null;
        {
            let w_now = obj
                .get("wheels")
                .and_then(|x| x.as_array())
                .and_then(|a| a.iter().find(|x| util_str(x.get("id")) == wheel_id));
            let Some(w_now) = w_now else {
                err = "活动不存在".into();
                return;
            };
            if !w_now.get("enabled").map(crate::util::truthy).unwrap_or(false) {
                err = "活动未开启".into();
                return;
            }
            if crate::util::f64_or(w_now.get("cost"), 0.0) != cost {
                err = "活动配置已变化，请重试".into();
                return;
            }
            w_now_val = w_now.clone();
            // 以最新配置重选奖品（管理员可能刚改过奖品金额/类型），并回写外部
            // 变量：扣费、发奖、日志、响应全部以临界区内选中的为准
            let Some(pz) = pick_prize(&w_now_val) else {
                err = "活动奖品配置无效".into();
                return;
            };
            let pt = util_str(pz.get("type"));
            let am = crate::util::round6(crate::util::f64_or(pz.get("amount"), 0.0));
            let md = util_str(pz.get("model"));
            if matches!(pt.as_str(), "recharge" | "grant") && am <= 0.0 {
                err = "奖品未配置有效金额".into();
                return;
            }
            if matches!(pt.as_str(), "model_unlimited" | "model_quota") && md.is_empty() {
                err = "奖品未配置模型".into();
                return;
            }
            prize = pz.clone();
            ptype = pt;
            amount = am;
            model = md;
            hours = crate::util::f64_or(pz.get("duration_hours"), 0.0);
            quota = crate::util::round6(crate::util::f64_or(pz.get("quota"), 0.0));
            concurrency = crate::util::int_or(pz.get("concurrency"), 1).clamp(1, 64);
            expires_at = if matches!(ptype.as_str(), "model_unlimited" | "model_quota") && hours > 0.0 {
                now + (hours * 3600.0) as i64
            } else {
                0
            };
            // 日志重建（prize_id/label 以最新选中为准）
            log = json!({
                "t": now,
                "user_id": uid,
                "wheel_id": util_str(w_now_val.get("id")),
                "wheel_name": util_str(w_now_val.get("name")),
                "prize_id": util_str(prize.get("id")),
                "label": util_str(prize.get("label")),
                "type": ptype,
                "cost": crate::util::round6(cost),
            });
        }
        // 1) 找用户并扣费（优先赠金）
        {
            let users = obj.entry("users").or_insert_with(|| Value::Array(vec![]));
            let Some(a) = users.as_array_mut() else {
                err = "存储异常".into();
                return;
            };
            let Some(u) = a.iter_mut().find(|u| {
                util_str(u.get("id")) == uid && u.get("enabled").map(crate::util::truthy).unwrap_or(false)
            }) else {
                err = "用户不存在或已停用".into();
                return;
            };
            match charge_cost(u, cost) {
                Ok((g, r)) => {
                    cost_from_grant = g;
                    cost_from_recharge = r;
                }
                Err(e) => {
                    err = e;
                    return;
                }
            }
            // 2) 发余额类奖品（同一个闭包内，与扣费原子）
            if matches!(ptype.as_str(), "recharge" | "grant") {
                crate::users::credit(u, &ptype, amount);
                awarded = true;
            }
        }
        // 资金变动流水：抽奖消耗（优先赠金）+ 余额类奖品入账
        if cost_from_grant > 0.0 || cost_from_recharge > 0.0 {
            crate::users::fund_log(
                db,
                uid,
                "draw",
                -cost_from_grant,
                -cost_from_recharge,
                &util_str(w.get("name")),
            );
        }
        if matches!(ptype.as_str(), "recharge" | "grant") && amount > 0.0 {
            let (dg, dr) = if ptype == "grant" { (amount, 0.0) } else { (0.0, amount) };
            crate::users::fund_log(db, uid, "prize", dg, dr, &util_str(prize.get("label")));
        }
        // 3) 发 Key 类奖品 + 记流水
        let obj = db.as_object_mut().unwrap();
        if matches!(ptype.as_str(), "model_unlimited" | "model_quota") {
            if let Some(a) = obj.entry("prize_keys").or_insert_with(|| Value::Array(vec![])).as_array_mut() {
                a.push(json!({
                    "id": pk_id,
                    "user_id": uid,
                    "wheel_id": util_str(w.get("id")),
                    "prize_id": util_str(prize.get("id")),
                    "key": key,
                    "type": ptype,
                    "model": model,
                    "concurrency": concurrency,
                    "quota": quota,
                    "used": 0.0,
                    "created_at": now,
                    "expires_at": expires_at,
                    "enabled": true,
                }));
            }
            awarded = true;
        }
        if ptype == "none" {
            awarded = true;
        }
        if let Some(a) = obj.entry("draw_logs").or_insert_with(|| Value::Array(vec![])).as_array_mut() {
            let mut entry = log.clone();
            if let Some(o) = entry.as_object_mut() {
                o.insert("cost_grant".into(), json!(crate::util::round6(cost_from_grant)));
                o.insert("cost_recharge".into(), json!(crate::util::round6(cost_from_recharge)));
            }
            a.insert(0, entry);
            a.truncate(2000);
        }
    });
    if !err.is_empty() {
        return (false, json!({"error": {"message": err}}));
    }
    if !awarded {
        return (false, json!({"error": {"message": "奖品发放失败"}}));
    }
    // id 用于前端指针落点：转盘必须停在中奖那一格
    let prize_id = util_str(prize.get("id"));
    let award = match ptype.as_str() {
        "recharge" | "grant" => json!({"id": prize_id, "type": ptype, "label": util_str(prize.get("label")), "color": util_str(prize.get("color")), "amount": amount}),
        "model_unlimited" | "model_quota" => json!({
            "id": prize_id,
            "type": ptype,
            "label": util_str(prize.get("label")),
            "color": util_str(prize.get("color")),
            "model": model,
            "key": key,
            "expires_at": expires_at,
            "quota": quota,
            "concurrency": concurrency,
        }),
        _ => json!({"id": prize_id, "type": "none", "label": util_str(prize.get("label")), "color": util_str(prize.get("color"))}),
    };
    // 返回最新钱包
    let wallet = store()
        .load()
        .get("users")
        .and_then(|u| u.as_array())
        .and_then(|a| a.iter().find(|u| util_str(u.get("id")) == uid))
        .map(crate::users::wallet_row)
        .unwrap_or(json!({}));
    (
        true,
        json!({
            "ok": true,
            "prize": award,
            "wallet": wallet,
            "cost": {
                "grant": crate::util::round6(cost_from_grant),
                "recharge": crate::util::round6(cost_from_recharge),
            },
        }),
    )
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
    let row = rows
        .iter()
        .find(|k| util_str(k.get("key")) == key)
        .ok_or("prize key 无效")?;
    if !row.get("enabled").map(crate::util::truthy).unwrap_or(false) {
        return Err("prize key 已停用");
    }
    let expires = crate::util::int_or(row.get("expires_at"), 0);
    if expires > 0 && now >= expires {
        return Err("prize key 已过期");
    }
    let quota = crate::util::f64_or(row.get("quota"), 0.0);
    if quota > 0.0 && crate::util::f64_or(row.get("used"), 0.0) >= quota {
        return Err("prize key 额度已用尽");
    }
    let owner = util_str(row.get("user_id"));
    let user_ok = db
        .get("users")
        .and_then(|u| u.as_array())
        .map(|a| {
            a.iter()
                .any(|u| util_str(u.get("id")) == owner && u.get("enabled").map(crate::util::truthy).unwrap_or(false))
        })
        .unwrap_or(false);
    if !user_ok {
        return Err("prize key 所属用户已停用");
    }
    Ok(row.clone())
}

/// prize Key 的模型限制：只允许 row.model。
pub fn prize_key_allows_model(row: &Value, model: &str) -> bool {
    util_str(row.get("model")) == model
}

/// prize Key 并发占用登记：INFLIGHT 风格内存计数（并发限制=体验卡独有语义）。
fn prize_inflight_key(key: &str) -> String {
    format!("prz:{}", key)
}

/// 奖品 Key 并发注册表：acquire/release 必须共享同一份（函数内 static 会各自独立）。
static PRIZE_INFLIGHT: std::sync::Mutex<Option<std::collections::HashMap<String, i64>>> =
    std::sync::Mutex::new(None);

/// 尝试占一个 prize Key 并发位；超限返回 false。
pub fn prize_key_acquire(key: &str, limit: i64) -> bool {
    let mut g = PRIZE_INFLIGHT.lock().unwrap();
    let m = g.get_or_insert_with(std::collections::HashMap::new);
    let cur = m.entry(prize_inflight_key(key)).or_insert(0);
    if limit > 0 && *cur >= limit {
        return false;
    }
    *cur += 1;
    true
}

pub fn prize_key_release(key: &str) {
    let mut g = PRIZE_INFLIGHT.lock().unwrap();
    let m = g.get_or_insert_with(std::collections::HashMap::new);
    if let Some(c) = m.get_mut(&prize_inflight_key(key)) {
        *c -= 1;
        if *c <= 0 {
            m.remove(&prize_inflight_key(key));
        }
    }
}

/// prize Key 计数：quota>0 时每成功调用 +1。
pub fn prize_key_consume(key: &str) {
    store().update(|db| {
        if let Some(a) = db.get_mut("prize_keys").and_then(|k| k.as_array_mut()) {
            for k in a.iter_mut() {
                if util_str(k.get("key")) == key {
                    if let Some(o) = k.as_object_mut() {
                        let used = crate::util::f64_or(o.get("used"), 0.0);
                        o.insert("used".into(), json!(crate::util::round6(used + 1.0)));
                    }
                    break;
                }
            }
        }
    });
}
