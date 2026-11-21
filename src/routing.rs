//! Auto 虚拟模型：每个路由有自己的价格和内容分支，对外表现为一个模型名。

use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageCond {
    Any,
    Yes,
    No,
}

impl ImageCond {
    fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    pub image: ImageCond,
    pub keywords: Vec<String>,
    pub target: String,
}

#[derive(Clone, Debug)]
pub struct AutoRoute {
    pub name: String,
    pub enabled: bool,
    pub price: Value,
    pub rules: Vec<Rule>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn parse_price(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn parse_image(value: Option<&Value>) -> ImageCond {
    match value.and_then(Value::as_str).unwrap_or("any") {
        "yes" => ImageCond::Yes,
        "no" => ImageCond::No,
        _ => ImageCond::Any,
    }
}

fn parse_keywords(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .take(30)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_rule(item: &Value) -> Option<Rule> {
    let target = item.get("target").and_then(Value::as_str)?.trim();
    if target.is_empty() || target.chars().count() > 160 {
        return None;
    }
    Some(Rule {
        name: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .chars()
            .take(60)
            .collect(),
        enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        image: parse_image(item.get("image")),
        keywords: parse_keywords(item.get("keywords")),
        target: target.to_string(),
    })
}

fn parse_rules(value: Option<&Value>) -> Vec<Rule> {
    let items = match value {
        Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Array(items)) => items,
            _ => return Vec::new(),
        },
        Some(Value::Array(items)) => items.clone(),
        _ => return Vec::new(),
    };
    items.iter().take(100).filter_map(parse_rule).collect()
}

fn parse_route(item: &Value) -> Option<AutoRoute> {
    let name = item.get("name").and_then(Value::as_str)?.trim();
    if !valid_name(name) {
        return None;
    }
    Some(AutoRoute {
        name: name.to_string(),
        enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        price: parse_price(item.get("price")),
        rules: parse_rules(item.get("rules")),
    })
}

fn parse_routes(value: &Value) -> Vec<AutoRoute> {
    let items = match value {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Array(items)) => items,
            _ => return Vec::new(),
        },
        Value::Array(items) => items.clone(),
        _ => return Vec::new(),
    };
    let mut out: Vec<AutoRoute> = Vec::new();
    for item in items.iter().take(20) {
        let Some(route) = parse_route(item) else { continue };
        if out
            .iter()
            .any(|r| r.name.eq_ignore_ascii_case(&route.name))
        {
            continue;
        }
        out.push(route);
    }
    out
}

/// 旧配置只有单个 auto；升级后第一次读取时映射成一个同名路由。
fn legacy_route(cfg: &Value) -> Option<AutoRoute> {
    let enabled = cfg
        .get("auto_enabled")
        .map(crate::util::truthy)
        .unwrap_or(false);
    let rules = parse_rules(cfg.get("routing_rules"));
    let price = parse_price(cfg.get("auto_price"));
    if !enabled && rules.is_empty() && price.is_null() {
        return None;
    }
    Some(AutoRoute {
        name: "auto".into(),
        enabled,
        price,
        rules,
    })
}

pub fn auto_routes(cfg: &Value) -> Vec<AutoRoute> {
    match cfg.get("auto_routes") {
        Some(Value::String(raw)) if raw.trim().is_empty() => {
            legacy_route(cfg).into_iter().collect()
        }
        Some(value) => parse_routes(value),
        None => legacy_route(cfg).into_iter().collect(),
    }
}

pub fn auto_route(cfg: &Value, name: &str) -> Option<AutoRoute> {
    auto_routes(cfg)
        .into_iter()
        .find(|r| r.enabled && r.name.eq_ignore_ascii_case(name))
}

pub fn auto_names(cfg: &Value) -> Vec<String> {
    auto_routes(cfg)
        .into_iter()
        .filter(|r| r.enabled)
        .map(|r| r.name)
        .collect()
}

fn content_has_image(content: &Value) -> bool {
    match content {
        Value::Array(items) => items.iter().any(content_has_image),
        Value::Object(obj) => {
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "image_url" | "input_image" | "image") {
                return true;
            }
            obj.get("image_url").map(|v| !v.is_null()).unwrap_or(false)
                || obj.get("input_image").map(|v| !v.is_null()).unwrap_or(false)
                || obj.get("image").map(|v| !v.is_null()).unwrap_or(false)
        }
        _ => false,
    }
}

fn has_image(req: &Value) -> bool {
    if req
        .get("images")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        return true;
    }
    if let Some(messages) = req.get("messages").and_then(Value::as_array) {
        for m in messages {
            if m.get("content").map(content_has_image).unwrap_or(false) {
                return true;
            }
        }
    }
    req.get("input").map(content_has_image).unwrap_or(false)
}

fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        Value::Object(obj) => {
            for key in [
                "messages", "content", "text", "input_text", "input", "prompt", "system",
            ] {
                if let Some(v) = obj.get(key) {
                    collect_text(v, out);
                }
            }
        }
        _ => {}
    }
}

/// 返回命中的目标模型；无规则命中时返回 None。
pub fn resolve(route: &AutoRoute, req: &Value) -> Option<String> {
    let image = has_image(req);
    let mut text = String::new();
    collect_text(req, &mut text);
    let text = text.to_lowercase();
    for rule in &route.rules {
        if !rule.enabled {
            continue;
        }
        match rule.image {
            ImageCond::Yes if !image => continue,
            ImageCond::No if image => continue,
            _ => {}
        }
        if !rule.keywords.is_empty()
            && !rule.keywords.iter().any(|k| text.contains(k.as_str()))
        {
            continue;
        }
        if rule.target.eq_ignore_ascii_case(&route.name) {
            continue;
        }
        return Some(rule.target.clone());
    }
    None
}

fn rule_value(rule: &Rule) -> Value {
    json!({
        "name": rule.name,
        "enabled": rule.enabled,
        "image": rule.image.as_str(),
        "keywords": rule.keywords,
        "target": rule.target,
    })
}

pub fn route_value(route: &AutoRoute) -> Value {
    json!({
        "name": route.name,
        "enabled": route.enabled,
        "price": route.price,
        "rules": route.rules.iter().map(rule_value).collect::<Vec<_>>(),
    })
}

pub fn routes_json(routes: &[AutoRoute]) -> String {
    Value::Array(routes.iter().map(route_value).collect()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_routes_keep_their_own_price_and_rules() {
        let cfg = json!({"auto_routes": serde_json::to_string(&json!([
            {"name":"auto","enabled":true,"price":{"mode":"token","input":1,"output":2},
             "rules":[{"name":"默认","image":"any","keywords":[],"target":"text-model"}]},
            {"name":"auto-vision","enabled":true,"price":0.1,
             "rules":[{"name":"图片","image":"yes","keywords":[],"target":"vision-model"},
                      {"name":"画图","image":"any","keywords":["画图","draw"],"target":"image-model"}]},
            {"name":"disabled","enabled":false,"rules":[{"target":"never"}]}
        ])).unwrap()});
        let names = auto_names(&cfg);
        assert_eq!(names, vec!["auto", "auto-vision"]);
        let vision = auto_route(&cfg, "auto-vision").unwrap();
        assert_eq!(vision.price, json!(0.1));
        let img = json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]});
        assert_eq!(resolve(&vision, &img).as_deref(), Some("vision-model"));
        let draw = json!({"messages":[{"role":"user","content":"帮我画图"}]});
        assert_eq!(resolve(&vision, &draw).as_deref(), Some("image-model"));
        assert!(auto_route(&cfg, "disabled").is_none());
        assert_eq!(resolve(&auto_route(&cfg, "auto").unwrap(), &img).as_deref(), Some("text-model"));
    }

    #[test]
    fn legacy_single_auto_is_migrated() {
        let cfg = json!({
            "auto_enabled": true,
            "auto_price": "{\"mode\":\"token\",\"input\":3,\"output\":4}",
            "routing_rules": serde_json::to_string(&json!([
                {"name":"图片","enabled":true,"model":"auto*","image":"yes","keywords":[],"target":"vision"}
            ])).unwrap()
        });
        let routes = auto_routes(&cfg);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].name, "auto");
        assert_eq!(routes[0].price["input"], 3);
        let img = json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]});
        assert_eq!(resolve(&routes[0], &img).as_deref(), Some("vision"));
    }
}
