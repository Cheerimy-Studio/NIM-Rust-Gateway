//! 协议转换（core/convert.py 的移植）：Responses / Anthropic ↔ Chat Completions。

use crate::util;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub fn py_str(v: &Value) -> String {
    util::py_str(v)
}

pub fn flatten_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(arr) = content.as_array() {
        for c in arr {
            if let Some(s) = c.as_str() {
                parts.push(s.to_string());
                continue;
            }
            let Some(obj) = c.as_object() else { continue };
            let t = util::str_or(obj.get("type"), "");
            if ["input_text", "output_text", "text", "summary_text"].contains(&t.as_str())
                && obj.contains_key("text")
            {
                parts.push(py_str(obj.get("text").unwrap()));
            } else if t == "refusal" && obj.contains_key("refusal") {
                parts.push(py_str(obj.get("refusal").unwrap()));
            }
        }
    }
    parts.join("")
}

/// 把 Anthropic/Responses 的图片块统一成 OpenAI 的 image_url 内容块。
fn image_part(c: &Value) -> Option<Value> {
    let obj = c.as_object()?;
    let t = util::str_or(obj.get("type"), "");
    if t == "image" {
        if let Some(src) = obj.get("source").and_then(|s| s.as_object()) {
            if util::str_or(src.get("type"), "") == "base64" {
                let data = util::str_or(src.get("data"), "");
                if !data.is_empty() {
                    let media = util::str_or(src.get("media_type"), "image/png");
                    return Some(json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:{};base64,{}", media, data)}
                    }));
                }
                return None;
            }
            if util::str_or(src.get("type"), "") == "url" {
                let url = util::str_or(src.get("url"), "");
                if !url.is_empty() {
                    return Some(json!({"type": "image_url", "image_url": {"url": url}}));
                }
            }
        }
        return None;
    }
    if t == "input_image" {
        let url = util::str_or(
            obj.get("image_url").filter(|x| !x.is_null()),
            "",
        );
        let url = if url.is_empty() {
            util::str_or(obj.get("url"), "")
        } else {
            url
        };
        if !url.is_empty() {
            return Some(json!({"type": "image_url", "image_url": {"url": url}}));
        }
    }
    None
}

/// 块列表 → OpenAI 多模态 content；无图片时返回 None（调用方退回纯文本）。
fn content_with_images(blocks: &Value) -> Option<Vec<Value>> {
    let arr = blocks.as_array()?;
    let mut parts: Vec<Value> = Vec::new();
    let mut has_image = false;
    for b in arr {
        if let Some(s) = b.as_str() {
            if !s.is_empty() {
                parts.push(json!({"type": "text", "text": s}));
            }
            continue;
        }
        let Some(obj) = b.as_object() else { continue };
        let t = util::str_or(obj.get("type"), "");
        if t == "image" || t == "input_image" {
            if let Some(img) = image_part(b) {
                parts.push(img);
                has_image = true;
            }
        } else if t == "text" || t == "input_text" || t == "output_text" {
            let txt = util::str_or(obj.get("text").filter(|x| !x.is_null()), "");
            if !txt.is_empty() {
                parts.push(json!({"type": "text", "text": txt}));
            }
        }
    }
    if !has_image {
        return None;
    }
    Some(parts)
}

/// 转发前清理会被严格上游拒绝的请求体。
pub fn sanitize_request(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    if obj.get("tool_choice").map(|v| !v.is_null()).unwrap_or(false) {
        let tools_ok = obj
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !tools_ok {
            obj.remove("tool_choice");
        }
    }
    for k in ["max_tokens", "max_completion_tokens"] {
        let v = match obj.get(k) {
            Some(v) => v.clone(),
            None => continue,
        };
        if v.is_boolean() {
            obj.remove(k);
            continue;
        }
        if let Some(f) = v.as_f64() {
            if f <= 0.0 {
                obj.remove(k);
            }
        }
    }
}

/// Responses 协议的 usage（该协议的唯一产出点）。
pub fn map_usage(u: Option<&Value>) -> Value {
    let u = u.cloned().unwrap_or(Value::Null);
    let tin = util::int_or(u.get("prompt_tokens"), 0);
    let tout = util::int_or(u.get("completion_tokens"), 0);
    let total = util::int_or(u.get("total_tokens"), 0);
    json!({
        "input_tokens": tin,
        "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
        "output_tokens": tout,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": if total == 0 { tin + tout } else { total },
    })
}

// ============================================================ Responses API

pub fn responses_to_chat(req: &Value) -> Result<Value, String> {
    let prev = util::str_or(req.get("previous_response_id"), "");
    if !prev.is_empty() {
        return Err(
            "本网关为无状态网关，不支持 previous_response_id；请把完整历史放入 input。".into(),
        );
    }
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instructions) = req.get("instructions").and_then(|x| x.as_str()) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }
    let inp = req.get("input").cloned().unwrap_or(Value::from(""));
    match &inp {
        Value::String(s) => {
            if !s.is_empty() {
                messages.push(json!({"role": "user", "content": s}));
            }
        }
        Value::Array(items) => {
            for item in items {
                let Some(obj) = item.as_object() else { continue };
                // str(item.get("type") or "message")：缺失/None/空 → "message"
                let typ = match obj.get("type").filter(|x| util::truthy(x)) {
                    Some(v) => util::str_or(Some(v), "message"),
                    None => "message".to_string(),
                };
                if typ == "message" {
                    let mut role = util::str_or(obj.get("role"), "user");
                    if role.is_empty() {
                        role = "user".into();
                    }
                    if role == "developer" {
                        role = "system".into();
                    }
                    let content = obj.get("content").cloned().unwrap_or(Value::Null);
                    if let Some(multi) = content_with_images(&content) {
                        messages.push(json!({"role": role, "content": multi}));
                    } else {
                        messages.push(json!({"role": role, "content": flatten_content(&content)}));
                    }
                } else if typ == "function_call" {
                    let call_id = util::str_or(
                        obj.get("call_id").filter(|x| !x.is_null()),
                        "",
                    );
                    let call_id = if call_id.is_empty() {
                        util::str_or(obj.get("id"), "")
                    } else {
                        call_id
                    };
                    let call_id = if call_id.is_empty() {
                        util::rand_id("call_")
                    } else {
                        call_id
                    };
                    messages.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": util::str_or(obj.get("name"), ""),
                                "arguments": util::str_or(obj.get("arguments"), "{}"),
                            },
                        }],
                    }));
                } else if typ == "function_call_output" {
                    let out = obj.get("output").cloned().unwrap_or(Value::Null);
                    let content = match &out {
                        Value::String(s) => s.clone(),
                        other => flatten_content(other),
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": util::str_or(obj.get("call_id"), ""),
                        "content": content,
                    }));
                }
            }
        }
        _ => {}
    }
    if messages.is_empty() {
        return Err("input 不能为空".into());
    }

    let mut chat = json!({
        "model": util::str_or(req.get("model"), ""),
        "messages": messages,
    });
    let chat_obj = chat.as_object_mut().unwrap();
    for k in ["temperature", "top_p", "stream", "parallel_tool_calls", "user", "seed"] {
        if let Some(v) = req.get(k) {
            chat_obj.insert(k.to_string(), v.clone());
        }
    }
    // int(req["max_output_tokens"]) > 0 → max_tokens
    for key in ["max_output_tokens", "max_completion_tokens"] {
        let v = req.get(key).filter(|x| !x.is_null());
        if let Some(v) = v {
            if let Some(i) = util::py_int(v) {
                if i > 0 {
                    chat_obj.insert("max_tokens".into(), Value::from(i));
                    break;
                }
            }
            break;
        }
    }
    if let Some(effort) = req
        .get("reasoning")
        .and_then(|r| r.as_object())
        .and_then(|r| r.get("effort"))
        .filter(|x| !x.is_null())
    {
        if util::truthy(effort) {
            chat_obj.insert("reasoning_effort".into(), effort.clone());
        }
    }

    let mut tools: Vec<Value> = Vec::new();
    if let Some(arr) = req.get("tools").and_then(|x| x.as_array()) {
        for t in arr {
            if let Some(obj) = t.as_object() {
                if util::str_or(obj.get("type"), "") == "function" {
                    if obj.contains_key("function") {
                        tools.push(t.clone());
                    } else {
                        tools.push(json!({
                            "type": "function",
                            "function": {
                                "name": util::str_or(obj.get("name"), ""),
                                "description": util::str_or(obj.get("description"), ""),
                                "parameters": obj.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": []})),
                            },
                        }));
                    }
                }
            }
        }
    }
    if !tools.is_empty() {
        chat_obj.insert("tools".into(), Value::Array(tools.clone()));
    }
    let tc = req.get("tool_choice").filter(|x| !x.is_null()).cloned();
    // tools 为空时丢弃 tool_choice
    if !tools.is_empty() {
        if let Some(tc) = tc {
            let is_fn = tc
                .as_object()
                .map(|o| {
                    util::str_or(o.get("type"), "") == "function"
                        && o.contains_key("name")
                        && !o.contains_key("function")
                })
                .unwrap_or(false);
            if is_fn {
                let name = util::str_or(tc.get("name"), "");
                chat_obj.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "function": {"name": name}}),
                );
            } else {
                chat_obj.insert("tool_choice".into(), tc);
            }
        }
    }
    Ok(chat)
}

fn rand_id_value(prefix: &str) -> Value {
    Value::from(util::rand_id(prefix))
}

pub fn chat_to_responses(chat: &Value, meta: &Value) -> Value {
    let choices = chat.get("choices").and_then(|c| c.as_array());
    let choice: Value = choices
        .and_then(|c| c.first().cloned())
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let finish = util::str_or(choice.get("finish_reason"), "");

    let mut output: Vec<Value> = Vec::new();
    let reasoning = {
        let r1 = msg.get("reasoning_content").filter(|x| util::truthy(x));
        match r1 {
            Some(r) => Some(r.clone()),
            None => msg.get("reasoning").filter(|x| util::truthy(x)).cloned(),
        }
    };
    if let Some(r) = &reasoning {
        if r.is_string() && !util::degenerate_reasoning(r) {
            output.push(json!({
                "id": rand_id_value("rs_"),
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": r.as_str().unwrap()}],
            }));
        }
    }
    let text = match msg.get("content") {
        Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
        _ => flatten_content(&msg.get("content").cloned().unwrap_or(Value::Null)),
    };
    let has_tool_calls = msg
        .get("tool_calls")
        .map(|t| util::truthy(t))
        .unwrap_or(false);
    if !text.is_empty() || !has_tool_calls {
        output.push(json!({
            "id": rand_id_value("msg_"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            let call_id = util::str_or(tc.get("id").filter(|x| !x.is_null()), "");
            let call_id = if call_id.is_empty() {
                util::rand_id("call_")
            } else {
                call_id
            };
            output.push(json!({
                "id": rand_id_value("fc_"),
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": util::str_or(f.get("name"), ""),
                "arguments": util::str_or(f.get("arguments"), "{}"),
            }));
        }
    }

    let (status, incomplete) = match finish.as_str() {
        "length" => ("incomplete", Some(json!({"reason": "max_output_tokens"}))),
        "content_filter" => ("incomplete", Some(json!({"reason": "content_filter"}))),
        _ => ("completed", None),
    };

    let usage = chat.get("usage").filter(|u| u.is_object()).cloned();
    let chat_id = util::str_or(chat.get("id").filter(|x| !x.is_null()), "");
    let id = if chat_id.is_empty() {
        format!("resp_{}", util::rand_id(""))
    } else {
        format!("resp_{}", chat_id)
    };
    let created = util::int_or(chat.get("created").filter(|x| !x.is_null()), 0);
    let created = if created == 0 {
        util::now_i()
    } else {
        created
    };
    // chat.get("model") or meta.get("model") or ""
    let model_v = [chat.get("model"), meta.get("model")]
        .into_iter()
        .flatten()
        .find(|v| util::truthy(v))
        .map(|v| py_str(v))
        .unwrap_or_default();
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "incomplete_details": incomplete,
        "error": null,
        "model": model_v,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": {"effort": meta.get("reasoning_effort"), "summary": null},
        "temperature": meta.get("temperature"),
        "top_p": meta.get("top_p"),
        "max_output_tokens": meta.get("max_output_tokens"),
        "tools": [],
        "tool_choice": "auto",
        "usage": map_usage(usage.as_ref()),
        "metadata": {},
    })
}

pub fn anthropic_to_chat(req: &Value) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();
    let system = req.get("system").cloned().unwrap_or(Value::Null);
    if util::truthy(&system) {
        let text = match &system {
            Value::String(s) => s.clone(),
            other => flatten_content(other),
        };
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    let mut has_messages = false;
    if let Some(arr) = req.get("messages").and_then(|x| x.as_array()) {
        for m in arr {
            let Some(obj) = m.as_object() else { continue };
            has_messages = true;
            let role = if util::str_or(obj.get("role"), "") == "assistant" {
                "assistant"
            } else {
                "user"
            };
            let content = obj.get("content").cloned().unwrap_or(Value::from(""));
            if let Value::String(s) = &content {
                messages.push(json!({"role": role, "content": s}));
                continue;
            }
            let multi = content_with_images(&content);
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_results: Vec<Value> = Vec::new();
            if let Some(blocks) = content.as_array() {
                for b in blocks {
                    if let Some(s) = b.as_str() {
                        text_parts.push(s.to_string());
                        continue;
                    }
                    let Some(bo) = b.as_object() else { continue };
                    let t = util::str_or(bo.get("type"), "");
                    if t == "text" {
                        text_parts.push(util::str_or(bo.get("text").filter(|x| !x.is_null()), ""));
                    } else if t == "tool_use" {
                        let id = util::str_or(bo.get("id").filter(|x| !x.is_null()), "");
                        let id = if id.is_empty() {
                            util::rand_id("call_")
                        } else {
                            id
                        };
                        let input = bo.get("input").cloned().unwrap_or(json!({}));
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": util::str_or(bo.get("name"), ""),
                                "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
                            },
                        }));
                    } else if t == "tool_result" {
                        let inner = bo.get("content").cloned().unwrap_or(Value::from(""));
                        let c = match &inner {
                            Value::String(s) => s.clone(),
                            other => flatten_content(other),
                        };
                        tool_results.push(json!({
                            "tool_call_id": util::str_or(bo.get("tool_use_id"), ""),
                            "content": c,
                        }));
                    }
                }
            }
            if !tool_calls.is_empty() {
                let joined = text_parts.concat();
                messages.push(json!({
                    "role": "assistant",
                    "content": if joined.is_empty() { Value::Null } else { Value::from(joined) },
                    "tool_calls": tool_calls,
                }));
            } else if let Some(ref multi) = multi {
                messages.push(json!({"role": role, "content": multi}));
            } else if !text_parts.is_empty() {
                messages.push(json!({"role": role, "content": text_parts.concat()}));
            }
            for tr in &tool_results {
                messages.push(
                    json!({
                        "role": "tool",
                        "tool_call_id": tr.get("tool_call_id"),
                        "content": tr.get("content"),
                    }),
                );
            }
            if text_parts.is_empty() && multi.is_none() && tool_calls.is_empty() && tool_results.is_empty() {
                messages.push(json!({"role": role, "content": ""}));
            }
        }
    }
    if !has_messages {
        return Err("messages 不能为空".into());
    }

    let max_tokens = {
        let v = req.get("max_tokens").filter(|x| !x.is_null()).cloned().unwrap_or(Value::from(4096));
        match util::py_int(&v) {
            Some(i) => i.max(1),
            None => return Err("invalid max_tokens".into()),
        }
    };
    let mut chat = json!({
        "model": util::str_or(req.get("model"), ""),
        "messages": messages,
        "max_tokens": max_tokens,
    });
    let chat_obj = chat.as_object_mut().unwrap();
    for k in ["temperature", "top_p", "top_k", "stream"] {
        if let Some(v) = req.get(k) {
            chat_obj.insert(k.to_string(), v.clone());
        }
    }
    if let Some(stop) = req.get("stop_sequences").filter(|x| util::truthy(x)) {
        chat_obj.insert("stop".into(), stop.clone());
    }
    // Anthropic thinking 参数(budget_tokens)映射为 reasoning_effort
    if let Some(th) = req.get("thinking").and_then(|t| t.as_object()) {
        if util::str_or(th.get("type"), "") == "enabled" {
            let budget = util::int_or(th.get("budget_tokens").filter(|x| !x.is_null()), 0);
            if budget > 0 {
                let effort = if budget >= 8192 {
                    "high"
                } else if budget >= 2048 {
                    "medium"
                } else {
                    "low"
                };
                chat_obj.insert("reasoning_effort".into(), Value::from(effort));
            }
        }
    }
    let mut tools: Vec<Value> = Vec::new();
    if let Some(arr) = req.get("tools").and_then(|x| x.as_array()) {
        for t in arr {
            if let Some(obj) = t.as_object() {
                let name = util::str_or(obj.get("name"), "");
                if !name.is_empty() {
                    tools.push(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": util::str_or(obj.get("description"), ""),
                            "parameters": obj.get("input_schema").cloned().unwrap_or(json!({"type": "object", "properties": []})),
                        },
                    }));
                }
            }
        }
    }
    if !tools.is_empty() {
        chat_obj.insert("tools".into(), Value::Array(tools.clone()));
    }
    if !tools.is_empty() {
        if let Some(tc) = req.get("tool_choice").and_then(|t| t.as_object()) {
            let ttype = util::str_or(tc.get("type"), "auto");
            match ttype.as_str() {
                "any" => {
                    chat_obj.insert("tool_choice".into(), Value::from("required"));
                }
                "tool" => {
                    chat_obj.insert(
                        "tool_choice".into(),
                        json!({"type": "function", "function": {"name": util::str_or(tc.get("name"), "")}}),
                    );
                }
                _ => {
                    chat_obj.insert("tool_choice".into(), Value::from("auto"));
                }
            }
        }
    }
    Ok(chat)
}

fn stop_map(finish: &str) -> &str {
    match finish {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    }
}

pub fn chat_to_anthropic(chat: &Value) -> Value {
    let choices = chat.get("choices").and_then(|c| c.as_array());
    let choice: Value = choices
        .and_then(|c| c.first().cloned())
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let finish = util::str_or(choice.get("finish_reason"), "stop");
    let mut content: Vec<Value> = Vec::new();
    let reasoning = {
        let r1 = msg.get("reasoning_content").filter(|x| util::truthy(x));
        match r1 {
            Some(r) => Some(r.clone()),
            None => msg.get("reasoning").filter(|x| util::truthy(x)).cloned(),
        }
    };
    if let Some(r) = &reasoning {
        if let Some(rs) = r.as_str() {
            if !rs.is_empty() && !util::degenerate_reasoning(r) {
            // signature 是 ThinkingBlock 的必填字段：给空串即可
                content.push(json!({"type": "thinking", "thinking": rs, "signature": ""}));
            }
        }
    }
    let text = match msg.get("content") {
        Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
        _ => flatten_content(&msg.get("content").cloned().unwrap_or(Value::Null)),
    };
    let has_tool_calls = msg.get("tool_calls").map(|t| util::truthy(t)).unwrap_or(false);
    if !text.is_empty() || !has_tool_calls {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            let args = util::str_or(f.get("arguments"), "{}");
            let inp: Value = serde_json::from_str(&args).unwrap_or(json!({}));
            let id = util::str_or(tc.get("id").filter(|x| !x.is_null()), "");
            let id = if id.is_empty() {
                util::rand_id("toolu_")
            } else {
                id
            };
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": util::str_or(f.get("name"), ""),
                "input": if inp.is_object() { inp } else { json!({}) },
            }));
        }
    }
    let usage = chat.get("usage").cloned().unwrap_or(json!({}));
    let chat_id = util::str_or(chat.get("id").filter(|x| !x.is_null()), "");
    let id = if chat_id.is_empty() {
        format!("msg_{}", util::rand_id(""))
    } else {
        format!("msg_{}", chat_id)
    };
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": util::str_or(chat.get("model").filter(|x| !x.is_null()), ""),
        "content": content,
        "stop_reason": stop_map(&finish),
        "stop_sequence": null,
        "usage": {
            "input_tokens": util::int_or(usage.get("prompt_tokens"), 0),
            "output_tokens": util::int_or(usage.get("completion_tokens"), 0),
        },
    })
}

// ---------------------------------------------------------------- 思考参数降级

/// 解析模型默认思考强度配置：每行 model=effort。
pub fn parse_thinking_defaults(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap();
        let k = k.trim().to_lowercase();
        let v = v.trim().to_lowercase();
        if !k.is_empty() && ["minimal", "low", "medium", "high", "max"].contains(&v.as_str()) {
            out.insert(k, v);
        }
    }
    out
}

pub fn thinking_unsupported(body_text: &str, status: i64) -> bool {
    if status != 400 || body_text.is_empty() {
        return false;
    }
    let low = body_text.to_lowercase();
    let has_think = ["thinking", "reasoning_effort", "reasoning"]
        .iter()
        .any(|k| low.contains(k));
    let has_unsupported = ["unsupported", "not supported", "not support", "supported values", "invalid"]
        .iter()
        .any(|k| low.contains(k));
    has_think && has_unsupported
}

const EFFORT_KEYS: &[&str] = &[
    "reasoning_effort",
    "thinking_effort",
    "reasoning_effort_level",
    "thinking_budget_level",
];
const THINKING_DROP_KEYS: &[&str] = &[
    "reasoning_effort",
    "thinking_effort",
    "reasoning_effort_level",
    "thinking_budget_level",
    "reasoning",
    "thinking",
    "enable_thinking",
    "clear_thinking",
];
const THINKING_KWARG_KEYS: &[&str] = &["thinking", "enable_thinking", "clear_thinking"];

/// 思考参数报错后自动降级。返回是否做了改动。
pub fn downgrade_thinking(body: &mut Value, model: &str, defaults: &HashMap<String, String>) -> bool {
    let Some(obj) = body.as_object_mut() else { return false };
    let m = model.to_lowercase();
    let mut default_effort: Option<&String> = None;
    for (pat, eff) in defaults {
        if m.contains(pat.as_str()) {
            default_effort = Some(eff);
            break;
        }
    }
    let mut changed = false;
    if let Some(eff) = default_effort {
        for k in EFFORT_KEYS {
            if let Some(v) = obj.get_mut(*k) {
                if util::str_or(Some(v), "") != *eff {
                    *v = Value::from(eff.clone());
                    changed = true;
                }
            }
        }
        if let Some(reasoning) = obj.get_mut("reasoning").and_then(|r| r.as_object_mut()) {
            if let Some(e) = reasoning.get("effort").filter(|x| util::truthy(x)) {
                if util::str_or(Some(e), "") != *eff {
                    reasoning.insert("effort".into(), Value::from(eff.clone()));
                    changed = true;
                }
            }
        }
        if !changed && !EFFORT_KEYS.iter().any(|k| obj.contains_key(*k)) {
            obj.insert("reasoning_effort".into(), Value::from(eff.clone()));
            changed = true;
        }
    } else {
        for k in THINKING_DROP_KEYS {
            if obj.remove(*k).map(|v| !v.is_null()).unwrap_or(false) {
                changed = true;
            }
        }
        let has_ctk = obj.get("chat_template_kwargs").map(|v| v.is_object()).unwrap_or(false);
        if has_ctk {
            let ctk = obj.get_mut("chat_template_kwargs").unwrap().as_object_mut().unwrap();
            for k in THINKING_KWARG_KEYS {
                if ctk.remove(*k).map(|v| !v.is_null()).unwrap_or(false) {
                    changed = true;
                }
            }
            if ctk.is_empty() {
                obj.remove("chat_template_kwargs");
            }
        }
    }
    changed
}

// ---------------------------------------------------------------- 请求体类型归一化

const NUMERIC_FIELDS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "max_completion_tokens",
    "min_tokens",
    "n",
    "seed",
    "frequency_penalty",
    "presence_penalty",
    "repetition_penalty",
    "length_penalty",
    "logprobs",
    "top_logprobs",
    "best_of",
    "max_input_tokens",
    "max_output_tokens",
    "max_response_tokens",
    "budget_tokens",
    "parallel_tool_calls",
    "store",
    "ttl_after_seconds",
    "compression_threshold",
    "context_size",
    "num_ctx",
    "num_predict",
    "repeat_penalty",
    "temperature_override",
];
const BOOL_FIELDS: &[&str] = &[
    "stream",
    "parallel_tool_calls",
    "store",
    "logprobs",
    "stream_options",
];
const PARAM_BAGS: &[&str] = &["reasoning", "thinking", "chat_template_kwargs"];

/// 把请求体里已知数值/布尔字段的字符串形式归一化为 JSON 原生类型。
pub fn normalize_body_types(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        let v = obj.get(&k).cloned().unwrap_or(Value::Null);
        if BOOL_FIELDS.contains(&k.as_str()) && v.is_string() {
            let sv = v.as_str().unwrap().trim().to_lowercase();
            obj.insert(k, Value::from(["true", "1", "yes"].contains(&sv.as_str())));
        } else if NUMERIC_FIELDS.contains(&k.as_str()) && v.is_string() {
            let s = v.as_str().unwrap().trim().to_string();
            let is_int = {
                let b = s.strip_prefix('-').unwrap_or(&s);
                !b.is_empty() && b.chars().all(|c| c.is_ascii_digit())
            };
            let is_float = {
                match s.strip_prefix('-').unwrap_or(&s).split_once('.') {
                    Some((a, b)) => {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.chars().all(|c| c.is_ascii_digit())
                            && b.chars().all(|c| c.is_ascii_digit())
                    }
                    None => false,
                }
            };
            if is_int {
                if let Ok(i) = s.parse::<i64>() {
                    obj.insert(k, Value::from(i));
                }
            } else if is_float {
                if let Ok(f) = s.parse::<f64>() {
                    obj.insert(k, serde_json::from_str::<Value>(&format!("{}", f)).unwrap_or(Value::from(f)));
                }
            }
        } else if PARAM_BAGS.contains(&k.as_str()) && v.is_object() {
            normalize_body_types(obj.get_mut(&k).unwrap());
        }
    }
}

pub fn is_deserialize_error(body_text: &str, status: i64) -> bool {
    if status != 400 || body_text.is_empty() {
        return false;
    }
    let low = body_text.to_lowercase();
    [
        "failed to deserialize",
        "expected f32",
        "expected i64",
        "invalid type: string",
        "expected a float",
        "expected an integer",
        "deserializ",
    ]
    .iter()
    .any(|k| low.contains(k))
}

const STRING_ONLY_FIELDS: &[&str] = &[
    "model",
    "user",
    "response_format",
    "stop",
    "messages",
    "tools",
    "functions",
    "tool_choice",
    "function_call",
    "reasoning_effort",
    "thinking_effort",
];

/// 激进转换：顶层及参数袋中所有「看起来像数字/布尔」的字符串→原生类型。
pub fn coerce_all_types(body: &mut Value) -> bool {
    let Some(obj) = body.as_object_mut() else { return false };
    let mut changed = false;
    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        if STRING_ONLY_FIELDS.contains(&k.as_str()) {
            continue;
        }
        let v = obj.get(&k).cloned().unwrap_or(Value::Null);
        if v.is_string() {
            let sv = v.as_str().unwrap().trim().to_string();
            let low = sv.to_lowercase();
            if low == "true" || low == "false" {
                obj.insert(k, Value::from(low == "true"));
                changed = true;
            } else {
                let is_int = {
                    let b = sv.strip_prefix('-').unwrap_or(&sv);
                    !b.is_empty() && b.chars().all(|c| c.is_ascii_digit())
                };
                let is_float = match sv.strip_prefix('-').unwrap_or(&sv).split_once('.') {
                    Some((a, b)) => {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.chars().all(|c| c.is_ascii_digit())
                            && b.chars().all(|c| c.is_ascii_digit())
                    }
                    None => false,
                };
                if is_int {
                    if let Ok(i) = sv.parse::<i64>() {
                        obj.insert(k, Value::from(i));
                        changed = true;
                    }
                } else if is_float {
                    if let Ok(f) = sv.parse::<f64>() {
                        obj.insert(
                            k,
                            serde_json::from_str::<Value>(&format!("{}", f)).unwrap_or(Value::from(f)),
                        );
                        changed = true;
                    }
                }
            }
        } else if PARAM_BAGS.contains(&k.as_str()) && v.is_object() {
            if coerce_all_types(obj.get_mut(&k).unwrap()) {
                changed = true;
            }
        }
    }
    changed
}

pub fn is_unsupported_param_error(body_text: &str, status: i64) -> bool {
    if status != 400 || body_text.is_empty() {
        return false;
    }
    let low = body_text.to_lowercase();
    low.contains("unsupported parameter")
        || low.contains("unsupported parameter(s)")
        || (low.contains("validation:") && low.contains("unsupported"))
}

/// 上游报不支持参数时，从请求体移除被点名的参数（含 thinking 相关）。
pub fn strip_unsupported_params(body: &mut Value, body_text: &str) -> bool {
    let Some(obj) = body.as_object_mut() else { return false };
    let mut changed = false;
    let mut names: Vec<String> = Vec::new();
    let re = regex::Regex::new(r"`([a-z_]+)`").unwrap();
    for cap in re.captures_iter(body_text) {
        if let Some(m) = cap.get(1) {
            let name = m.as_str().to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    for name in names {
        if obj.remove(&name).map(|v| !v.is_null()).unwrap_or(false) {
            changed = true;
        }
    }
    for k in THINKING_DROP_KEYS {
        if obj.remove(*k).map(|v| !v.is_null()).unwrap_or(false) {
            changed = true;
        }
    }
    if obj.get("chat_template_kwargs").map(|v| v.is_object()).unwrap_or(false) {
        let ctk = obj
            .get_mut("chat_template_kwargs")
            .unwrap()
            .as_object_mut()
            .unwrap();
        for k in ["thinking", "enable_thinking", "clear_thinking"] {
            if ctk.remove(k).map(|v| !v.is_null()).unwrap_or(false) {
                changed = true;
            }
        }
        if ctk.is_empty() {
            obj.remove("chat_template_kwargs");
        }
    }
    changed
}

pub fn is_duplicate_field_error(body_text: &str, status: i64) -> bool {
    if status != 400 || body_text.is_empty() {
        return false;
    }
    let low = body_text.to_lowercase();
    low.contains("duplicate field") || low.contains("duplicate key")
}

/// 从多轮对话历史消息中移除思考内容字段。
pub fn strip_reasoning_from_messages(body: &mut Value) -> bool {
    let Some(obj) = body.as_object_mut() else { return false };
    let Some(msgs) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for m in msgs.iter_mut() {
        if let Some(mo) = m.as_object_mut() {
            for k in ["reasoning_content", "reasoning"] {
                if mo.remove(k).map(|v| !v.is_null()).unwrap_or(false) {
                    changed = true;
                }
            }
        }
    }
    changed
}

// ---------------------------------------------------------------- 上游错误分类

pub fn is_channel_exhausted(body_text: &str) -> bool {
    let s = body_text.to_lowercase();
    ["no available channel", "no channel available", "无可用渠道", "channel_exhausted"]
        .iter()
        .any(|k| s.contains(k))
}

#[allow(unused)]
fn _touch(m: Map<String, Value>) -> Map<String, Value> {
    m
}
