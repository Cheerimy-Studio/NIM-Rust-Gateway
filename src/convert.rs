//! 协议转换（core/convert.py 的移植）：Responses / Anthropic ↔ Chat Completions。

use crate::util;
use serde_json::{json, Value};
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

fn validate_portable_content(content: &Value, allowed: &[&str]) -> Result<(), String> {
    if content.is_string() || content.is_null() { return Ok(()); }
    for block in content.as_array().ok_or("content 必须为字符串或数组")? {
        if block.is_string() { continue; }
        let kind = block.get("type").and_then(Value::as_str).ok_or("内容块缺少 type")?;
        if !allowed.contains(&kind) { return Err(format!("协议适配不支持内容类型 {}，不会静默丢弃", kind)); }
        if ["image", "input_image"].contains(&kind) && image_part(block).is_none() {
            return Err("图片必须提供可转换的 URL 或 base64；不支持原生 file_id 引用".into());
        }
    }
    Ok(())
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
    normalize_for_upstream(obj);
}

/// 转发前按上游最大公约数规范化请求字段（幂等，每次尝试前调用）。
/// 目前处理 ZCode 0.16.x / @ai-sdk 2.x 注入的 max_completion_tokens：
/// 旧版 NIM、vLLM、多数 OpenAI 兼容上游只认 max_tokens；新上游两者都认，
/// 统一改名为 max_tokens 是严格超集兼容，且杜绝 strict 上游按 extra input 拒绝。
fn normalize_for_upstream(obj: &mut serde_json::Map<String, Value>) {
    match obj.get("max_completion_tokens") {
        Some(_) if obj.contains_key("max_tokens") => {
            obj.remove("max_completion_tokens"); // 已有等价字段，去重
        }
        Some(v) => {
            let v = v.clone();
            obj.remove("max_completion_tokens");
            obj.insert("max_tokens".into(), v);
        }
        None => {}
    }
}

/// Responses 协议的 usage（该协议的唯一产出点）。
pub fn map_usage(u: Option<&Value>) -> Value {
    let u = u.cloned().unwrap_or(Value::Null);
    let tin = util::int_or(u.get("prompt_tokens"), 0).max(0);
    let tout = util::int_or(u.get("completion_tokens"), 0).max(0);
    let total = util::int_or(u.get("total_tokens"), 0);
    json!({
        "input_tokens": tin,
        "input_tokens_details": {"cached_tokens": util::int_or(u.pointer("/prompt_tokens_details/cached_tokens"), 0).max(0), "cache_write_tokens": util::int_or(u.pointer("/prompt_tokens_details/cache_write_tokens"), 0).max(0)},
        "output_tokens": tout,
        "output_tokens_details": {"reasoning_tokens": util::int_or(u.pointer("/completion_tokens_details/reasoning_tokens"), 0).max(0)},
        "total_tokens": if total == 0 { tin.saturating_add(tout) } else { total },
    })
}

pub fn anthropic_usage(usage: Option<&Value>, estimated_input: i64) -> Value {
    let empty = json!({});
    let u = usage.unwrap_or(&empty);
    let cached = util::int_or(u.pointer("/prompt_tokens_details/cached_tokens"), 0).max(0);
    let written = util::int_or(u.pointer("/prompt_tokens_details/cache_write_tokens"), 0).max(0);
    let input = util::int_or(u.get("prompt_tokens"), estimated_input).max(0);
    let mut out = json!({
        "input_tokens": input.saturating_sub(cached).saturating_sub(written).max(0),
        "output_tokens": util::int_or(u.get("completion_tokens"), 0).max(0),
    });
    if cached > 0 { out["cache_read_input_tokens"] = json!(cached); }
    if written > 0 { out["cache_creation_input_tokens"] = json!(written); }
    out
}

// ============================================================ Responses API

pub fn responses_to_chat(req: &Value) -> Result<Value, String> {
    if req.get("background").is_some_and(util::truthy) {
        return Err("本网关不支持后台 Responses 任务".into());
    }
    // Responses 服务端内置工具（web_search / local_shell / apply_patch 等）由
    // OpenAI 服务端执行，chat 上游没有对应承载：跳过而非整单拒绝，
    // function 工具照常转换。tool_choice 引用内置工具时一并丢弃（见下方）。
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
                    validate_portable_content(&content, &["input_text", "output_text", "text", "input_image", "image", "refusal"])?;
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
                    validate_portable_content(&out, &["input_text", "output_text", "text"])?;
                    let content = match &out {
                        Value::String(s) => s.clone(),
                        other => flatten_content(other),
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": util::str_or(obj.get("call_id"), ""),
                        "content": content,
                    }));
                } else if matches!(
                    typ.as_str(),
                    "reasoning"
                        | "web_search_call"
                        | "web_search_call_result"
                        | "code_interpreter_call"
                        | "file_search_call"
                        | "image_generation_call"
                        | "local_shell_call"
                        | "mcp_call"
                        | "mcp_list_tools"
                        | "apply_patch_call"
                        | "custom_tool_call"
                        | "item_reference"
                ) {
                    // 服务端动作与推理历史：chat 协议没有对应承载形式，
                    // 其结果已体现在后续 assistant 消息文本里，跳过即可
                    continue;
                } else {
                    return Err(format!("不支持的 Responses input 类型：{}", typ));
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
    if let Some(format) = req.pointer("/text/format") {
        let mut chat_format = format.clone();
        if format["type"] == "json_schema" {
            let mut schema = format.as_object().ok_or("text.format 必须为对象")?.clone();
            schema.remove("type");
            chat_format = json!({"type":"json_schema","json_schema":schema});
        }
        chat_obj.insert("response_format".into(), chat_format);
    }
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
                        let mut tool = json!({
                            "type": "function",
                            "function": {
                                "name": util::str_or(obj.get("name"), ""),
                                "description": util::str_or(obj.get("description"), ""),
                                "parameters": obj.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                            },
                        });
                        if let Some(strict) = obj.get("strict") { tool["function"]["strict"] = strict.clone(); }
                        tools.push(tool);
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
            } else if tc.is_object() && !tc.get("function").is_some() {
                // allowed_tools / 内置工具引用：chat 协议无可映射形式，丢弃
                // （丢弃 tool_choice 不影响请求本身，工具定义仍在）
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
        if r.is_string() {
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
    validate_portable_content(&system, &["text"])?;
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
            let mut reasoning_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_results: Vec<Value> = Vec::new();
            let mut result_images: Vec<Value> = Vec::new();
            if let Some(blocks) = content.as_array() {
                for b in blocks {
                    if let Some(s) = b.as_str() {
                        text_parts.push(s.to_string());
                        continue;
                    }
                    let Some(bo) = b.as_object() else { continue };
                    let t = util::str_or(bo.get("type"), "");
                    if !["text", "image", "thinking", "redacted_thinking", "tool_use", "tool_result"]
                        .contains(&t.as_str())
                    {
                        return Err(format!("协议适配不支持 Messages 内容块 {}；不会丢弃内容历史", t));
                    }
                    if t == "thinking" {
                        // 带签名的思考历史：assistant 消息转成 chat 协议的
                        // reasoning_content 继续携带（signature 是 Anthropic 侧
                        // 字段，chat 无对应形式）；非 assistant 消息里出现
                        // thinking 属协议违规，干净忽略（不半跟踪导致消息丢失）
                        if role == "assistant" {
                            reasoning_parts.push(util::str_or(
                                bo.get("thinking").filter(|x| !x.is_null()),
                                "",
                            ));
                        }
                        continue;
                    }
                    if t == "redacted_thinking" {
                        // 加密推理块无可转换文本，跳过
                        continue;
                    }
                    if t == "image" && image_part(b).is_none() {
                        return Err("无法转换此 Messages 图片块".into());
                    }
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
                        // 工具结果可含文本与图片（CC 截图流）：文本进 tool 消息，
                        // 图片提为 tool 消息之后的 user 消息（chat 协议的 tool 角色
                        // 只承载文本，图片单独走多模态 user 消息才不会丢内容）
                        let mut tr_text = String::new();
                        if let Some(arr) = inner.as_array() {
                            for piece in arr {
                                if let Some(ps) = piece.as_str() {
                                    tr_text.push_str(ps);
                                    continue;
                                }
                                let pt = util::str_or(piece.get("type"), "");
                                if pt == "text" {
                                    let tv = util::str_or(
                                        piece.get("text").filter(|x| !x.is_null()),
                                        "",
                                    );
                                    tr_text.push_str(&tv);
                                } else if pt == "image" {
                                    let img = image_part(piece)
                                        .ok_or("无法转换此工具结果图片块")?;
                                    result_images.push(img);
                                } else if !pt.is_empty() {
                                    return Err(format!(
                                        "协议适配不支持工具结果内容类型 {}",
                                        pt
                                    ));
                                }
                            }
                        } else {
                            tr_text = match &inner {
                                Value::String(s) => s.clone(),
                                other => flatten_content(other),
                            };
                        }
                        tool_results.push(json!({
                            "tool_call_id": util::str_or(bo.get("tool_use_id"), ""),
                            "content": tr_text,
                        }));
                    }
                }
            }
            if !tool_calls.is_empty() {
                let joined = text_parts.concat();
                let mut msg = json!({
                    "role": "assistant",
                    "content": if joined.is_empty() { Value::Null } else { Value::from(joined) },
                    "tool_calls": tool_calls,
                });
                if !reasoning_parts.is_empty() {
                    msg["reasoning_content"] = json!(reasoning_parts.concat());
                }
                messages.push(msg);
            }
            // OpenAI 契约：tool 消息必须紧跟其 assistant(tool_calls) 消息。
            // Anthropic 客户端常把 tool_result 和后续文本放在同一条 user 消息里，
            // 这里必须先压 tool 再压文本，否则严格上游（OpenAI/vLLM）直接 400
            if !tool_results.is_empty() {
                for tr in &tool_results {
                    messages.push(
                        json!({
                            "role": "tool",
                            "tool_call_id": tr.get("tool_call_id"),
                            "content": tr.get("content"),
                        }),
                    );
                }
                if !result_images.is_empty() {
                    // 工具结果里的图片：文本（若有）+ 图片组成 user 多模态消息
                    let mut parts: Vec<Value> = Vec::new();
                    if !text_parts.is_empty() {
                        parts.push(json!({"type": "text", "text": text_parts.concat()}));
                    }
                    parts.extend(result_images.iter().cloned());
                    messages.push(json!({"role": "user", "content": parts}));
                } else if !text_parts.is_empty() && tool_calls.is_empty() {
                    // 同一条 user 消息里的尾部文本仍有意义：作为独立的 user 转发
                    messages.push(json!({"role": role, "content": text_parts.concat()}));
                }
            } else if let Some(ref multi) = multi {
                messages.push(json!({"role": role, "content": multi}));
            } else if !text_parts.is_empty() {
                let mut msg = json!({"role": role, "content": text_parts.concat()});
                if role == "assistant" && !reasoning_parts.is_empty() {
                    msg["reasoning_content"] = json!(reasoning_parts.concat());
                }
                messages.push(msg);
            } else if role == "assistant" && !reasoning_parts.is_empty() {
                messages.push(json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "reasoning_content": reasoning_parts.concat(),
                }));
            }
            if text_parts.is_empty() && multi.is_none() && tool_calls.is_empty()
                && tool_results.is_empty() && reasoning_parts.is_empty()
            {
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
                            "parameters": obj.get("input_schema").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
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
                "none" => {
                    chat_obj.insert("tool_choice".into(), Value::from("none"));
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
            if !rs.is_empty() {
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
        "usage": anthropic_usage(Some(&usage), 0),
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
        // "off" = 剥离该模型的全部思考参数（上游不支持思考时的主动适配）
        // `*` = 匹配所有模型（渠道级兜底）
        if !k.is_empty() && ["off", "minimal", "low", "medium", "high", "max"].contains(&v.as_str()) {
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

/// 匹配模型对应的默认思考强度：先精确匹配（后台下拉框选的就是完整模型名），
/// 再 `*` 通配兜底。未命中返回 None。
fn match_thinking_effort(model: &str, defaults: &HashMap<String, String>) -> Option<String> {
    let m = model.to_lowercase();
    defaults.get(&m).cloned().or_else(|| defaults.get("*").cloned())
}

/// 剥离全部思考相关参数（含 chat_template_kwargs 内层）。返回是否做了改动。
pub fn strip_thinking_params(body: &mut Value) -> bool {
    let Some(obj) = body.as_object_mut() else { return false };
    let mut changed = false;
    for k in THINKING_DROP_KEYS {
        if obj.remove(*k).map(|v| !v.is_null()).unwrap_or(false) {
            changed = true;
        }
    }
    if obj.get("chat_template_kwargs").map(|v| v.is_object()).unwrap_or(false) {
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
    changed
}

/// 请求体里是否带着思考参数（400 后判断「剥离重试是否有意义」）。
pub fn has_thinking_params(body: &Value) -> bool {
    let Some(obj) = body.as_object() else { return false };
    if THINKING_DROP_KEYS
        .iter()
        .any(|k| obj.get(*k).map(|v| !v.is_null()).unwrap_or(false))
    {
        return true;
    }
    obj.get("chat_template_kwargs")
        .and_then(|v| v.as_object())
        .map(|ctk| {
            THINKING_KWARG_KEYS
                .iter()
                .any(|k| ctk.get(*k).map(|v| !v.is_null()).unwrap_or(false))
        })
        .unwrap_or(false)
}

fn set_thinking_effort(body: &mut Value, eff: &str) -> bool {
    if eff == "off" {
        // off = 该上游/模型不支持思考参数：全部剥离（含 chat_template_kwargs）
        return strip_thinking_params(body);
    }
    let Some(obj) = body.as_object_mut() else { return false };
    let mut changed = false;
    for k in EFFORT_KEYS {
        if let Some(v) = obj.get_mut(*k) {
            if util::str_or(Some(v), "") != eff {
                *v = Value::from(eff);
                changed = true;
            }
        }
    }
    if let Some(reasoning) = obj.get_mut("reasoning").and_then(|r| r.as_object_mut()) {
        if let Some(e) = reasoning.get("effort").filter(|x| util::truthy(x)) {
            if util::str_or(Some(e), "") != eff {
                reasoning.insert("effort".into(), Value::from(eff));
                changed = true;
            }
        }
    }
    if !changed && !EFFORT_KEYS.iter().any(|k| obj.contains_key(*k)) {
        obj.insert("reasoning_effort".into(), Value::from(eff));
        changed = true;
    }
    changed
}

/// 主动应用渠道思考默认值：只有配置命中该模型（或 `*`）才动请求体；
/// 未配置的模型原样放行（后台提示「未匹配的模型不做调整」）。
pub fn apply_thinking_default(body: &mut Value, model: &str, defaults: &HashMap<String, String>) -> bool {
    match match_thinking_effort(model, defaults) {
        Some(eff) => set_thinking_effort(body, &eff),
        None => false,
    }
}

/// 思考参数报错后自动降级：未命中配置时剥离全部思考参数（400 自愈路径）。
pub fn downgrade_thinking(body: &mut Value, model: &str, defaults: &HashMap<String, String>) -> bool {
    match match_thinking_effort(model, defaults) {
        Some(eff) => set_thinking_effort(body, &eff),
        None => strip_thinking_params(body),
    }
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
        // NVIDIA Pydantic（严格模式）与新版 ZCode 场景：文案里没有参数名，
        // param 字段形如 "thinking.type[0]"，靠特征词同样要触发自动降级
        || low.contains("extra inputs are not permitted")
        || low.contains("extra inputs are not allowed")
        // 旧版 vLLM / 部分 python 上游
        || low.contains("unexpected keyword argument")
        || low.contains("unrecognized keyword argument")
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
    // NVIDIA Pydantic 错误把参数名放在 JSON 的 error.param（如 "thinking.type[0]"），
    // 消息文本里没有——直接解析取顶层名。结构键（对话本体/工具定义）绝不剥离：
    // 剥掉 messages/tools 后即使重试成功，回复也不再基于客户端的原始请求
    if let Ok(v) = serde_json::from_str::<Value>(body_text) {
        if let Some(p) = v.pointer("/error/param").and_then(|x| x.as_str()) {
            const STRUCTURAL: [&str; 6] = ["messages", "input", "tools", "model", "stream", "n"];
            let top = p.split(['.', '[']).next().unwrap_or(p);
            if !top.is_empty()
                && !STRUCTURAL.contains(&top)
                && !names.contains(&top.to_string())
            {
                names.push(top.to_string());
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
    // 兜底：NVIDIA 只回 "Extra inputs are not permitted"（参数名在 param 字段，反引号
    // 提取不到）；此时按已知注入集剥离——stream_options 上游不认时网关会自行补
    // usage 尾帧，max_completion_tokens 与 max_tokens 等价（已预处理去重）。
    for k in ["stream_options", "max_completion_tokens"] {
        if obj.remove(k).map(|v| !v.is_null()).unwrap_or(false) {
            changed = true;
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

// ---------------------------------------------------------------- 工具输出压缩

/// 判断文本是否是 unified diff（git diff）。
fn looks_like_diff(text: &str) -> bool {
    text.contains("\ndiff --git ") || text.starts_with("diff --git ")
}

/// 压缩 git diff：去掉 index/mode 等元数据行，hunk 上下文只保留变更块
/// 相邻的一行（标准 smart-diff 削减）。变更行(+/-)与 hunk 头全部保留。
fn compact_diff_lines(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_hunk = false;
    let mut ctx_run = 0usize;
    for line in text.split('\n') {
        if line.starts_with("diff --git ") || line.starts_with("--- ") || line.starts_with("+++ ")
            || line.starts_with("rename from") || line.starts_with("rename to")
            || line.starts_with("Binary files") || line.starts_with("new file mode")
            || line.starts_with("deleted file mode") || line.starts_with("old mode") || line.starts_with("new mode")
        {
            out.push(line.to_string());
            in_hunk = false;
            ctx_run = 0;
            continue;
        }
        if line.starts_with("index ") || line.starts_with("similarity index ") {
            continue; // 纯元数据：对模型回答没有信息量
        }
        if line.starts_with("@@") {
            out.push(line.to_string());
            in_hunk = true;
            ctx_run = 0;
            continue;
        }
        if !in_hunk {
            out.push(line.to_string());
            continue;
        }
        let is_ctx = line.starts_with(' ') || line.is_empty();
        let is_change = line.starts_with('+') || line.starts_with('-');
        if is_change {
            out.push(line.to_string());
            ctx_run = 0;
        } else if is_ctx {
            // 上下文行（变更块前后未变动的行）对回答贡献极小：
            // 丢弃并按连续段折叠为一个省略标记；文件身份由文件头与 hunk 头保留
            ctx_run += 1;
            if ctx_run == 1 {
                out.push("…".into());
            }
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

/// 折叠连续重复行（构建日志/编号转储的典型浪费）。
fn collapse_repeats(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run = 1usize;
    let mut prev: Option<&str> = None;
    for line in text.split('\n') {
        if prev == Some(line) {
            run += 1;
            continue;
        }
        if let Some(p) = prev {
            out.push(p.to_string());
            if run >= 4 {
                out.push(format!("\u{2026} (上一行重复 {} 次)", run));
            }
        }
        prev = Some(line);
        run = 1;
    }
    if let Some(p) = prev {
        out.push(p.to_string());
        if run >= 4 {
            out.push(format!("\u{2026} (上一行重复 {} 次)", run));
        }
    }
    out.join("\n")
}

/// 头尾保留的截断：中间用标记替代。
fn head_tail_cap(text: &str, max_bytes: usize) -> String {
    let b = text.as_bytes();
    if b.len() <= max_bytes {
        return text.to_string();
    }
    let head = max_bytes * 2 / 3;
    let tail = max_bytes / 3;
    // 按字符边界找安全切点
    let head_cut = {
        let mut i = head;
        while i < b.len() && (b[i] & 0xC0) == 0x80 { i += 1; }
        i
    };
    let tail_start = {
        let mut i = b.len() - tail;
        while i < b.len() && (b[i] & 0xC0) == 0x80 { i += 1; }
        i
    };
    format!(
        "{}\n\u{2026}[输出过长，已截断：省略 {} 字节]\n{}",
        String::from_utf8_lossy(&b[..head_cut]).to_string(),
        b.len() - head_cut - (b.len() - tail_start),
        String::from_utf8_lossy(&b[tail_start..]).to_string(),
    )
}

/// 对超大 tool 输出做压缩（git diff 削减 + 重复行折叠 + 头尾截断）。
/// 返回节省的字节数。幂等：压缩后的内容带标记，通常已低于阈值不会再触发。
pub fn compact_tool_outputs(body: &mut Value, threshold: usize, max_out: usize) -> usize {
    if threshold == 0 || max_out == 0 {
        return 0;
    }
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };
    let mut saved = 0usize;
    for m in msgs.iter_mut() {
        if util::str_or(m.get("role"), "") != "tool" {
            continue;
        }
        let Some(content) = m.get("content").and_then(|c| c.as_str()) else {
            continue;
        };
        let size = content.len();
        if size <= threshold {
            continue;
        }
        let compacted = if looks_like_diff(content) {
            compact_diff_lines(content)
        } else {
            collapse_repeats(content)
        };
        let compacted = head_tail_cap(&compacted, max_out);
        let new_size = compacted.len() + 48;
        if new_size < size {
            let marked = format!(
                "[tool output compacted: {} bytes -> {} bytes]\n{}",
                size, new_size, compacted
            );
            if let Some(o) = m.as_object_mut() {
                o.insert("content".into(), Value::from(marked));
            }
            saved += size - new_size;
        }
    }
    saved
}

pub fn is_channel_exhausted(body_text: &str) -> bool {
    let s = body_text.to_lowercase();
    ["no available channel", "no channel available", "无可用渠道", "channel_exhausted"]
        .iter()
        .any(|k| s.contains(k))
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    #[test]
    fn compact_tool_outputs_smoke() {
        let mut diff = String::from("diff --git a/x.rs b/x.rs
index aaa..bbb 100644
");
        for i in 0..200 {
            diff.push_str(&format!("--- a/x.rs
+++ b/x.rs
@@ -{},3 +{},3 @@
", i, i));
            diff.push_str(&format!(" ctx {} a
 ctx {} b
-old {}
+new {}
 ctx {} c
", i, i, i, i, i));
        }
        let mut body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "c1", "content": diff},
        ]});
        let saved = compact_tool_outputs(&mut body, 8192, 60000);
        let content = body["messages"][1]["content"].as_str().unwrap();
        println!("saved={} content_len={} has_marker={}", saved, content.len(), content.contains("tool output compacted"));
        assert!(saved > 0, "nothing saved, diff len={}", diff.len());
        assert!(content.contains("tool output compacted"));
    }
}

#[cfg(test)]
mod thinking_tests {
    use super::*;
    use std::collections::HashMap;

    fn defaults(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn proactive_default_leaves_unmatched_models_alone() {
        // 渠道只配了 m3.1-flash=off：glm 的思考参数不得被顺手剥掉
        let d = defaults(&[("m3.1-flash", "off")]);
        let mut body = json!({"reasoning_effort": "high", "thinking": {"type": "enabled"}});
        assert!(!apply_thinking_default(&mut body, "glm-5.3", &d));
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_some());

        let mut body = json!({"enable_thinking": true, "reasoning": {"effort": "low"}});
        assert!(apply_thinking_default(&mut body, "m3.1-flash", &d));
        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn wildcard_and_exact_match() {
        let d = defaults(&[("*", "low"), ("glm-5.3", "max")]);
        let mut body = json!({"reasoning_effort": "minimal"});
        assert!(apply_thinking_default(&mut body, "glm-5.3", &d));
        assert_eq!(body["reasoning_effort"], "max");
        let mut body = json!({"reasoning_effort": "minimal"});
        assert!(apply_thinking_default(&mut body, "other", &d));
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn reactive_downgrade_strips_all_without_match() {
        // 400 自愈路径：配置没命中也要剥（上游明确说思考参数不支持）
        let d = defaults(&[("some-other-model", "low")]);
        let mut body = json!({
            "reasoning_effort": "high",
            "thinking": {"type": "enabled"},
            "chat_template_kwargs": {"thinking": true, "foo": 1},
        });
        assert!(downgrade_thinking(&mut body, "m3.1-flash", &d));
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
        assert_eq!(body["chat_template_kwargs"]["foo"], 1);
        assert!(body["chat_template_kwargs"].get("thinking").is_none());
    }

    #[test]
    fn has_thinking_params_sees_nested_kwargs() {
        assert!(has_thinking_params(&json!({"enable_thinking": true})));
        assert!(has_thinking_params(&json!({"reasoning": {"effort": "low"}})));
        assert!(has_thinking_params(&json!({"chat_template_kwargs": {"thinking": true}})));
        assert!(!has_thinking_params(&json!({"chat_template_kwargs": {"foo": true}})));
        assert!(!has_thinking_params(&json!({"messages": []})));
    }
}
