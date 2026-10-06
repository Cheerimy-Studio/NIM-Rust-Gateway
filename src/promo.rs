//! 拉人活动（拼多多式）：管理员只设「提现金额 + 拉人次数」，阶梯自动计算。
//! 活动固定 7 天有效期（到点即止，不可重开）；允许多活动并行。
//! 阶梯（自动方案，target=T, amount=A）：
//!   阶段0  注册进活动：进度条直接显示已集 A-0.01（离目标仅差 0.01）
//!   阶段1  还差 0.01 元 → 需 20 钻石：每拉 t1 人得 4 钻，集 20 钻兑换 +0.01
//!   阶段2  又差 0.01 元 → 需 20 金币：每拉 t1 人得 4 金币，集 20 金币兑换 +0.01
//!   阶段3  审核加速中：拉满剩余人数（至 T）→ 提现 A 元入充值账本
//!   其中 t1 = ceil(T/5)；翻倍卡：每拉 5 人额外 +1 张（抽奖余额奖品翻倍一次）
//!   每拉 1 人 +1 次抽奖（抽奖时可抵扣单次消耗）
//! 试玩模式：加入即视为拉满，全流程免拉人走通（管理员体验用）。

use crate::store::store;
use crate::util;
use serde_json::{json, Value};

pub const EVENT_TTL_SECS: i64 = 7 * 24 * 3600;

fn util_str(v: Option<&Value>) -> String {
    util::str_or(v, "")
}

pub fn events_arr<'a>(db: &'a mut Value) -> &'a mut Vec<Value> {
    db.as_object_mut()
        .unwrap()
        .entry("invite_events")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .unwrap()
}

pub fn members_arr<'a>(db: &'a mut Value) -> &'a mut Vec<Value> {
    db.as_object_mut()
        .unwrap()
        .entry("invite_members")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .unwrap()
}

pub fn hits_arr<'a>(db: &'a mut Value) -> &'a mut Vec<Value> {
    db.as_object_mut()
        .unwrap()
        .entry("invite_hits")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .unwrap()
}

pub fn get_event(id: &str) -> Option<Value> {
    store()
        .load()
        .get("invite_events")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().find(|e| util_str(e.get("id")) == id).cloned())
}

pub fn expired(e: &Value) -> bool {
    let exp = util::int_or(e.get("expires_at"), 0);
    exp > 0 && util::now_i() >= exp
}

pub fn active_ok(e: &Value) -> bool {
    e.get("enabled").map(util::truthy).unwrap_or(false) && !expired(e)
}

/// 阶梯参数（管理员只设 withdraw/target，这里自动推方案）
pub fn ladder(target: i64) -> (i64, i64, i64, i64, i64, i64) {
    // (t1宝石拉人数, t2金币拉人数, 每轮奖励个数=4, 宝石目标=20, 金币目标=20, 总拉人=target)
    let t = target.max(1);
    let t1 = ((t + 4) / 5).max(1);
    (t1, t1, 4, 20, 20, t)
}

fn member_row(db: &mut Value, event_id: &str, uid: &str) -> Option<Value> {
    members_arr(db)
        .iter()
        .find(|m| {
            util_str(m.get("event_id")) == event_id
                && util_str(m.get("user_id")) == uid
        })
        .cloned()
}

fn member_mut<'a>(db: &'a mut Value, event_id: &str, uid: &str) -> Option<&'a mut Value> {
    members_arr(db).iter_mut().find(|m| {
        util_str(m.get("event_id")) == event_id && util_str(m.get("user_id")) == uid
    })
}

/// 用户加入活动（幂等；试玩模式直接置为拉满）
pub fn join(event_id: &str, uid: &str) -> Result<Value, String> {
    let e = get_event(event_id).ok_or("活动不存在")?;
    if !active_ok(&e) {
        return Err(if expired(&e) { "活动已过期" } else { "活动未开启" }.into());
    }
    store().update(|db| {
        let trial = e.get("trial").map(util::truthy).unwrap_or(false);
        let target = util::int_or(e.get("target"), 1).max(1);
        if member_mut(db, event_id, uid).is_none() {
            members_arr(db).push(json!({
                "event_id": event_id,
                "user_id": uid,
                "joined_at": util::now_i(),
                "invited": if trial { target } else { 0 },
                "diamonds": 0, "golds": 0,
                "p1": false, "p2": false, "p3": false, "paid": false,
                "doubler": 0, "draw_credits": 0,
                "trial": trial,
            }));
        }
    });
    Ok(my_status(event_id, uid))
}

/// 注册归因：新用户带 inv=<event_id>.<inviter_uid> 注册时调用。
/// 邀请人 +1 拉人数、+1 抽奖次数；每满 5 人 +1 翻倍卡。被拉人自动加入活动。
pub fn attribute(inv: &str, invitee_uid: &str) {
    let Some((event_id, inviter)) = inv.split_once('.') else { return };
    let e = match get_event(event_id) {
        Some(e) if active_ok(&e) => e,
        _ => return,
    };
    store().update(|db| {
        // 防重：同一 (event, invitee) 只算一次
        if hits_arr(db).iter().any(|h| {
            util_str(h.get("event_id")) == event_id
                && util_str(h.get("invitee")) == invitee_uid
        }) {
            return;
        }
        hits_arr(db).push(json!({
            "event_id": event_id, "inviter": inviter, "invitee": invitee_uid, "t": util::now_i(),
        }));
        // 邀请人进度
        let target = util::int_or(e.get("target"), 1).max(1);
        if let Some(m) = member_mut(db, event_id, inviter) {
            let invited = util::int_or(m.get("invited"), 0).min(target) + 1;
            let doubler = util::int_or(m.get("doubler"), 0)
                + if invited % 5 == 0 { 1 } else { 0 };
            let credits = util::int_or(m.get("draw_credits"), 0) + 1;
            if let Some(o) = m.as_object_mut() {
                o.insert("invited".into(), json!(invited));
                o.insert("doubler".into(), json!(doubler));
                o.insert("draw_credits".into(), json!(credits));
            }
        }
        // 被拉人自动加入活动（非试玩）
        if member_mut(db, event_id, invitee_uid).is_none() {
            members_arr(db).push(json!({
                "event_id": event_id,
                "user_id": invitee_uid,
                "joined_at": util::now_i(),
                "invited": 0, "diamonds": 0, "golds": 0,
                "p1": false, "p2": false, "p3": false, "paid": false,
                "doubler": 0, "draw_credits": 0, "trial": false,
            }));
        }
    });
}

/// 领取阶段奖励（阶段1/2 的宝石/金币兑换、阶段3 提现）。
/// step: 1=兑换钻石 2=兑换金币 3=提现
pub fn claim(event_id: &str, uid: &str, step: i64) -> Result<Value, String> {
    let e = get_event(event_id).ok_or("活动不存在")?;
    if !active_ok(&e) {
        return Err(if expired(&e) { "活动已过期" } else { "活动未开启" }.into());
    }
    let amount = util::f64_or(e.get("amount"), 0.0);
    let target = util::int_or(e.get("target"), 1).max(1);
    let trial = e.get("trial").map(util::truthy).unwrap_or(false);
    let (t1, _t2, _per, gem_need, gold_need, total) = ladder(target);
    let mut out = my_status(event_id, uid);
    let mut ledger: Option<(f64, f64, String)> = None;
    store().update(|db| {
        let Some(m) = member_mut(db, event_id, uid) else { return; };
        let invited = util::int_or(m.get("invited"), 0).min(target);
        let eff_invited = if trial { target } else { invited };
        let (p1, p2, p3, paid) = (
            m.get("p1").map(util::truthy).unwrap_or(false),
            m.get("p2").map(util::truthy).unwrap_or(false),
            m.get("p3").map(util::truthy).unwrap_or(false),
            m.get("paid").map(util::truthy).unwrap_or(false),
        );
        let (diamonds, golds) = (
            util::int_or(m.get("diamonds"), 0),
            util::int_or(m.get("golds"), 0),
        );
        // 判定阶段（基于拷贝的字段，避免借用冲突）
        let can = match step {
            1 => !p1 && eff_invited >= t1,
            2 => !p2 && p1 && eff_invited >= t1 * 2,
            3 => !p3 && p2 && p1 && eff_invited >= total,
            4 => !paid && p1 && p2 && p3,
            _ => false,
        };
        if !can {
            return;
        }
        let Some(m) = member_mut(db, event_id, uid) else { return; };
        if let Some(o) = m.as_object_mut() {
            match step {
                1 => {
                    o.insert("diamonds".into(), json!((diamonds + 4).min(gem_need)));
                    o.insert("p1".into(), json!(true));
                }
                2 => {
                    o.insert("golds".into(), json!((golds + 4).min(gold_need)));
                    o.insert("p2".into(), json!(true));
                }
                3 => {
                    o.insert("p3".into(), json!(true));
                }
                4 => {
                    o.insert("paid".into(), json!(true));
                }
                _ => {}
            }
        }
        if step == 4 {
            // 提现：A 元入充值账本（真正的发放点）
            if let Some(arr) = db.get_mut("users").and_then(|u| u.as_array_mut()) {
                for u in arr.iter_mut() {
                    if util_str(u.get("id")) == uid {
                        if let Some(o) = u.as_object_mut() {
                            let cur = util::f64_or(o.get("balance"), 0.0);
                            o.insert("balance".into(), json!(util::round6(cur + amount)));
                        }
                        break;
                    }
                }
            }
            ledger = Some((0.0, amount, "拉人活动提现".into()));
        }
        if let Some((dg, dr, note)) = ledger {
            crate::users::fund_log(db, uid, "prize", dg, dr, &note);
        }
    });
    out = my_status(event_id, uid);
    Ok(out)
}

/// 用邀请次数抵扣一次抽奖消耗（wheel draw 原子闭包内调用）。返回是否抵扣成功。
pub fn consume_draw_credit(db: &mut Value, uid: &str) -> bool {
    let obj = db.as_object_mut().unwrap();
    consume_draw_credit_obj(obj, uid)
}

pub fn consume_draw_credit_obj(obj: &mut serde_json::Map<String, Value>, uid: &str) -> bool {
    // ⚠ 调用方持有 store 互斥锁：此处绝不可再走 store().load()（重入死锁），
    // 活动状态直接从同一 db 的 invite_events 里读
    // 预构建“进行中活动”集合，避免闭包借用冲突
    let live: std::collections::HashSet<String> = obj
        .get("invite_events")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter(|e| {
                    let exp = util::int_or(e.get("expires_at"), 0);
                    e.get("enabled").map(util::truthy).unwrap_or(false)
                        && !(exp > 0 && util::now_i() >= exp)
                })
                .map(|e| util_str(e.get("id")))
                .collect()
        })
        .unwrap_or_default();
    let ev_ok = |eid: &str| live.contains(eid);
    let arr = obj.entry("invite_members").or_insert_with(|| Value::Array(vec![]));
    for m in arr.as_array_mut().unwrap().iter_mut() {
        if util_str(m.get("user_id")) != uid {
            continue;
        }
        // 只要有任一进行中活动的次数即可抵扣
        let eid = util_str(m.get("event_id"));
        if !ev_ok(&eid) {
            continue;
        }
        let c = util::int_or(m.get("draw_credits"), 0);
        if c > 0 {
            if let Some(o) = m.as_object_mut() {
                o.insert("draw_credits".into(), json!(c - 1));
            }
            return true;
        }
    }
    false
}

/// 翻倍卡数量（跨活动合计）。
pub fn doubler_count(uid: &str) -> i64 {
    store()
        .load()
        .get("invite_members")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter(|m| util_str(m.get("user_id")) == uid)
                .map(|m| util::int_or(m.get("doubler"), 0))
                .sum()
        })
        .unwrap_or(0)
}

/// 消耗一张翻倍卡（抽奖余额奖品翻倍时调用；跨活动找第一张）。返回是否成功。
pub fn consume_doubler(db: &mut Value, uid: &str) -> bool {
    let obj = db.as_object_mut().unwrap();
    consume_doubler_obj(obj, uid)
}

pub fn consume_doubler_obj(obj: &mut serde_json::Map<String, Value>, uid: &str) -> bool {
    let arr = obj.entry("invite_members").or_insert_with(|| Value::Array(vec![]));
    for m in arr.as_array_mut().unwrap().iter_mut() {
        if util_str(m.get("user_id")) != uid {
            continue;
        }
        let n = util::int_or(m.get("doubler"), 0);
        if n > 0 {
            if let Some(o) = m.as_object_mut() {
                o.insert("doubler".into(), json!(n - 1));
            }
            return true;
        }
    }
    false
}

/// 我的进度视图（含展示用阶梯文案）。
pub fn my_status(event_id: &str, uid: &str) -> Value {
    let e = get_event(event_id).unwrap_or(json!({}));
    let amount = util::f64_or(e.get("amount"), 0.0);
    let target = util::int_or(e.get("target"), 1).max(1);
    let (t1, _t2, per, gem_need, gold_need, total) = ladder(target);
    let db = store().load();
    let m = db
        .get("invite_members")
        .and_then(|a| a.as_array())
        .and_then(|a| {
            a.iter()
                .find(|m| {
                    util_str(m.get("event_id")) == event_id
                        && util_str(m.get("user_id")) == uid
                })
                .cloned()
        });
    let Some(m) = m else {
        return json!({"joined": false});
    };
    let trial = m.get("trial").map(util::truthy).unwrap_or(false);
    let invited = util::int_or(m.get("invited"), 0).min(target);
    let eff = if trial { total } else { invited };
    let (p1, p2, p3, paid) = (
        m.get("p1").map(util::truthy).unwrap_or(false),
        m.get("p2").map(util::truthy).unwrap_or(false),
        m.get("p3").map(util::truthy).unwrap_or(false),
        m.get("paid").map(util::truthy).unwrap_or(false),
    );
    let collected = (amount - 0.03 + if paid { 0.03 } else { 0.01 * (p1 as i64 + p2 as i64 + p3 as i64) as f64 })
        .clamp(0.0, amount);
    // 邀请链接
    let link = format!("?inv={}.{}", event_id, uid);
    json!({
        "joined": true,
        "name": util_str(e.get("name")),
        "amount": util::round6(amount),
        "target": target,
        "invited": invited,
        "eff_invited": eff,
        "trial": trial,
        "t1": t1,
        "collected": util::round6(collected),
        "remain": util::round6((amount - collected).max(0.0)),
        "diamonds": util::int_or(m.get("diamonds"), 0),
        "golds": util::int_or(m.get("golds"), 0),
        "gem_need": gem_need, "gold_need": gold_need,
        "per": per, "total": total,
        "p1": p1, "p2": p2, "p3": p3, "paid": paid,
        "draw_credits": util::int_or(m.get("draw_credits"), 0),
        "doubler": util::int_or(m.get("doubler"), 0),
        "expires_at": util::int_or(e.get("expires_at"), 0),
        "enabled": e.get("enabled").map(util::truthy).unwrap_or(false),
        "link": link,
        "friends": db.get("invite_hits").and_then(|a| a.as_array()).map(|a| {
            a.iter()
                .filter(|h| {
                    util_str(h.get("event_id")) == event_id
                        && util_str(h.get("inviter")) == uid
                })
                .map(|h| json!({
                    "t": util::int_or(h.get("t"), 0),
                    "invitee": util::str_cut(&util::mask_email(&util_str(h.get("invitee"))), 40),
                }))
                .collect::<Vec<_>>()
        }).unwrap_or_default(),
    })
}

/// 管理端活动列表（含参与人数）。
pub fn admin_list() -> Vec<Value> {
    let db = store().load();
    let Some(arr) = db.get("invite_events").and_then(|a| a.as_array()) else {
        return vec![];
    };
    let now = util::now_i();
    arr.iter()
        .map(|e| {
            let id = util_str(e.get("id"));
            let members = db
                .get("invite_members")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter(|m| util_str(m.get("event_id")) == id).count())
                .unwrap_or(0);
            let invites = db
                .get("invite_hits")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter(|h| util_str(h.get("event_id")) == id).count())
                .unwrap_or(0);
            let exp = util::int_or(e.get("expires_at"), 0);
            json!({
                "id": id,
                "name": util_str(e.get("name")),
                "amount": util::f64_or(e.get("amount"), 0.0),
                "target": util::int_or(e.get("target"), 0),
                "trial": e.get("trial").map(util::truthy).unwrap_or(false),
                "enabled": e.get("enabled").map(util::truthy).unwrap_or(false),
                "expired": expired(e),
                "left_days": ((exp - now).max(0) / 86400),
                "members": members,
                "invites": invites,
                "created_at": util::int_or(e.get("created_at"), 0),
            })
        })
        .collect()
}

