//! Explicit upstream protocol selection. Downstream wire formats stay independent.
use crate::{sse, util};
use axum::body::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Chat,
    Responses,
    Messages,
    Completions,
    Embeddings,
}
impl Protocol {
    pub fn path(self) -> &'static str {
        match self {
            Self::Chat => "chat/completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::Completions => "completions",
            Self::Embeddings => "embeddings",
        }
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "chat/completions" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            "messages" => Ok(Self::Messages),
            "completions" => Ok(Self::Completions),
            "embeddings" => Ok(Self::Embeddings),
            _ => Err("不支持的上游端点".into()),
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub upstream: Protocol,
    pub internal: Protocol,
}
impl Plan {
    pub fn prepare(source: &str, setting: &str, request: &Value) -> Result<(Value, Self), String> {
        let internal = Protocol::parse(source)?;
        // Embeddings are a separate task, never turn vectors into chat text.
        let upstream = if internal == Protocol::Embeddings || setting == "auto" {
            internal
        } else {
            Protocol::parse(setting)?
        };
        let plan = Self { upstream, internal };
        if upstream == internal {
            return Ok((request.clone(), plan));
        }
        if upstream == Protocol::Embeddings {
            return Err("Embeddings 渠道不能处理对话请求".into());
        }
        let chat = if internal == Protocol::Completions {
            completion_request(request)?
        } else {
            request.clone()
        };
        let body = match upstream {
            Protocol::Chat => chat,
            Protocol::Responses => response_request(&chat)?,
            Protocol::Messages => message_request(&chat)?,
            Protocol::Completions => legacy_request(&chat)?,
            Protocol::Embeddings => unreachable!(),
        };
        Ok((body, plan))
    }
    pub fn auth(
        self,
        builder: reqwest::RequestBuilder,
        key: &str,
        mode: &str,
    ) -> reqwest::RequestBuilder {
        let builder = if self.upstream == Protocol::Messages {
            builder.header("anthropic-version", "2023-06-01")
        } else {
            builder
        };
        let use_api_key =
            mode == "x-api-key" || (mode != "bearer" && self.upstream == Protocol::Messages);
        if use_api_key {
            builder.header("x-api-key", key)
        } else {
            builder.bearer_auth(key)
        }
    }
    pub fn reply(self, raw: &str) -> Result<String, String> {
        if self.upstream == self.internal {
            return Ok(raw.to_string());
        }
        let value: Value =
            serde_json::from_str(raw).map_err(|_| "上游返回无效 JSON".to_string())?;
        if value.get("error").is_some_and(|v| !v.is_null()) {
            return Ok(raw.to_string());
        }
        let mut chat = reply_to_chat(self.upstream, &value)?;
        if self.internal == Protocol::Completions {
            chat = chat_to_completion(chat, false)?;
        }
        serde_json::to_string(&chat).map_err(|e| e.to_string())
    }
    pub fn stream(self, response: reqwest::Response) -> reqwest::Response {
        if self.upstream == self.internal || !response.status().is_success() {
            return response;
        }
        let status = response.status();
        let mut headers = response.headers().clone();
        // Decoded/transcoded bytes no longer match compressed entity metadata.
        for name in ["content-length", "content-encoding", "etag"] {
            headers.remove(name);
        }
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        let state = (
            response.bytes_stream(),
            NativeStream::new(self),
            VecDeque::new(),
            false,
        );
        let stream = futures::stream::unfold(
            state,
            |(mut upstream, mut bridge, mut pending, mut ended)| async move {
                loop {
                    if let Some(item) = pending.pop_front() {
                        return Some((item, (upstream, bridge, pending, ended)));
                    }
                    if ended {
                        return None;
                    }
                    match upstream.next().await {
                        Some(Ok(bytes)) => match bridge.feed(&bytes) {
                            Ok(events) => {
                                pending.extend(events.into_iter().map(Ok));
                                if bridge.terminal {
                                    ended = true;
                                }
                            }
                            Err(error) => {
                                pending.push_back(Err(io::Error::other(error)));
                                ended = true;
                            }
                        },
                        Some(Err(error)) => {
                            pending.push_back(Err(io::Error::other(error)));
                            ended = true;
                        }
                        None => {
                            ended = true;
                            if !bridge.terminal {
                                pending
                                    .push_back(Err(io::Error::other("上游原生协议未收到终端事件")));
                            }
                        }
                    }
                }
            },
        );
        let mut wrapped = axum::http::Response::new(reqwest::Body::wrap_stream(stream));
        *wrapped.status_mut() = status;
        *wrapped.headers_mut() = headers;
        reqwest::Response::from(wrapped)
    }
}

fn copy_fields(source: &Value, dest: &mut Value, fields: &[&str]) {
    for &key in fields {
        if let Some(value) = source.get(key) {
            dest[key] = value.clone();
        }
    }
}
fn nonempty<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少有效 {}", key))
}
fn single_choice(req: &Value) -> Result<(), String> {
    if req.get("n").is_some_and(|n| n.as_u64() != Some(1)) {
        return Err("该上游协议只支持 n=1".into());
    }
    for key in [
        "audio",
        "modalities",
        "logprobs",
        "top_logprobs",
        "functions",
        "function_call",
    ] {
        if req
            .get(key)
            .is_some_and(|v| !v.is_null() && v != &json!(false))
        {
            return Err(format!(
                "该上游协议不支持参数 {}，请使用 Chat Completions 渠道",
                key
            ));
        }
    }
    Ok(())
}
fn completion_request(req: &Value) -> Result<Value, String> {
    let prompt = req
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or("跨协议转换只支持字符串 prompt，不支持批量或 token 数组")?;
    if req.get("echo").is_some_and(util::truthy) || req.get("suffix").is_some_and(|v| !v.is_null())
    {
        return Err("跨协议转换不支持 echo/suffix".into());
    }
    let mut chat = req.clone();
    for key in ["prompt", "echo", "suffix", "best_of"] {
        chat.as_object_mut().ok_or("请求必须为对象")?.remove(key);
    }
    chat["messages"] = json!([{"role":"user","content":prompt}]);
    Ok(chat)
}
fn legacy_request(req: &Value) -> Result<Value, String> {
    if req
        .get("tools")
        .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()))
    {
        return Err("旧版 Completions 上游不支持工具调用".into());
    }
    let mut prompt = String::new();
    for m in req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages 必须为数组")?
    {
        if m.get("tool_calls").is_some() || m["role"] == "tool" {
            return Err("旧版 Completions 不支持工具历史".into());
        }
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            _ => return Err("旧版 Completions 不支持多模态内容".into()),
        };
        prompt.push_str(&format!("{}: {}\n", nonempty(m, "role")?, text));
    }
    prompt.push_str("assistant:");
    let mut out = json!({"model":req["model"],"prompt":prompt});
    copy_fields(
        req,
        &mut out,
        &[
            "stream",
            "temperature",
            "top_p",
            "stop",
            "seed",
            "n",
            "presence_penalty",
            "frequency_penalty",
            "logit_bias",
            "user",
        ],
    );
    if let Some(limit) = req
        .get("max_completion_tokens")
        .or_else(|| req.get("max_tokens"))
    {
        out["max_tokens"] = limit.clone();
    }
    if let Some(options) = req.get("stream_options") {
        out["stream_options"] = options.clone();
    }
    Ok(out)
}
fn content_parts(content: &Value, anthropic: bool, assistant: bool) -> Result<Value, String> {
    if let Some(text) = content.as_str() {
        return if anthropic {
            Ok(json!([{"type":"text","text":text}]))
        } else {
            Ok(json!([{"type":if assistant {"output_text"} else {"input_text"},"text":text}]))
        };
    }
    if content.is_null() {
        return Ok(json!([]));
    }
    let mut parts = Vec::new();
    for part in content.as_array().ok_or("content 必须为字符串或数组")? {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(json!({"type":if anthropic {"text"} else if assistant {"output_text"} else {"input_text"},"text":part.get("text").and_then(Value::as_str).ok_or("text 必须为字符串")?})),
            Some("image_url") if !assistant => {
                let image=part.get("image_url").ok_or("缺少 image_url")?;
                let url=image.as_str().or_else(|| image.get("url").and_then(Value::as_str)).ok_or("缺少图片 URL")?;
                if anthropic {
                    if let Some(rest)=url.strip_prefix("data:") {
                        let (mime,data)=rest.split_once(";base64,").ok_or("图片 data URL 必须为 base64")?;
                        if !["image/jpeg","image/png","image/gif","image/webp"].contains(&mime) { return Err("不支持的图片媒体类型".into()); }
                        parts.push(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}));
                    } else { parts.push(json!({"type":"image","source":{"type":"url","url":url}})); }
                } else {
                    let mut p=json!({"type":"input_image","image_url":url});
                    if let Some(detail)=image.get("detail") {p["detail"]=detail.clone();}
                    parts.push(p);
                }
            }
            _=>return Err("该上游协议不支持此内容类型，未丢弃或降级内容".into()),
        }
    }
    Ok(Value::Array(parts))
}
fn tool_definitions(req: &Value, anthropic: bool) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    if let Some(tools) = req.get("tools") {
        for tool in tools.as_array().ok_or("tools 必须为数组")? {
            if tool["type"] != "function" {
                return Err("跨协议转换只支持 function 工具".into());
            }
            let f = tool.get("function").ok_or("缺少工具 function")?;
            if anthropic && f.get("strict").is_some_and(util::truthy) {
                return Err("Messages 上游暂不支持 strict 工具约束转换".into());
            }
            let mut item = if anthropic {
                json!({"name":nonempty(f,"name")?,"input_schema":f.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}}))})
            } else {
                json!({"type":"function","name":nonempty(f,"name")?,"parameters":f.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}}))})
            };
            if let Some(description) = f.get("description") {
                item["description"] = description.clone();
            }
            if !anthropic {
                if let Some(strict) = f.get("strict") {
                    item["strict"] = strict.clone();
                }
            }
            out.push(item);
        }
    }
    Ok(out)
}
fn choice(req: &Value, anthropic: bool) -> Result<Option<Value>, String> {
    let Some(c) = req.get("tool_choice").filter(|c| !c.is_null()) else {
        return Ok(None);
    };
    let v = if let Some(s) = c.as_str() {
        if !["auto", "none", "required"].contains(&s) {
            return Err("无效 tool_choice".into());
        }
        if anthropic {
            json!({"type":if s=="required" {"any"} else {s}})
        } else {
            c.clone()
        }
    } else {
        // 指定 function 之外的 tool_choice 形式（allowed_tools、内置工具引用）
        // 在目标协议没有等价形式：丢弃约束而非拒绝请求（工具定义仍随请求转发）
        let Some(name) = c.pointer("/function/name").and_then(Value::as_str) else {
            return Ok(None);
        };
        json!({"type":if anthropic {"tool"} else {"function"},"name":name})
    };
    Ok(Some(v))
}
fn response_request(req: &Value) -> Result<Value, String> {
    single_choice(req)?;
    for field in [
        "stop",
        "seed",
        "top_k",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
    ] {
        if req
            .get(field)
            .is_some_and(|v| !v.is_null() && v != &json!(0))
        {
            return Err(format!("Responses 上游不支持跨协议参数 {}", field));
        }
    }
    let mut input = Vec::new();
    for m in req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages 必须为数组")?
    {
        let role = nonempty(m, "role")?;
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":nonempty(m,"tool_call_id")?,"output":m.get("content").cloned().unwrap_or(json!(""))}));
            continue;
        }
        if !["user", "assistant", "system", "developer"].contains(&role) {
            return Err("不支持的消息角色".into());
        }
        let content = content_parts(
            m.get("content").unwrap_or(&Value::Null),
            false,
            role == "assistant",
        )?;
        if content.as_array().is_some_and(|a| !a.is_empty()) {
            input.push(json!({"type":"message","role":role,"content":content}));
        }
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                input.push(json!({"type":"function_call","call_id":nonempty(call,"id")?,"name":nonempty(&call["function"],"name")?,"arguments":nonempty(&call["function"],"arguments")?}));
            }
        }
    }
    let mut out = json!({"model":req["model"],"input":input,"store":false});
    copy_fields(
        req,
        &mut out,
        &[
            "stream",
            "temperature",
            "top_p",
            "parallel_tool_calls",
            "user",
            "metadata",
        ],
    );
    if let Some(limit) = req
        .get("max_completion_tokens")
        .or_else(|| req.get("max_tokens"))
    {
        out["max_output_tokens"] = limit.clone();
    }
    if let Some(effort) = req.get("reasoning_effort") {
        out["reasoning"] = json!({"effort":effort});
    }
    if let Some(format) = req.get("response_format") {
        let mut f = format.clone();
        if !format.is_object()
            || ![Some("text"), Some("json_object"), Some("json_schema")]
                .contains(&format["type"].as_str())
        {
            return Err("无效 response_format".into());
        }
        if format["type"] == "json_schema" {
            f = Value::Object(
                format
                    .get("json_schema")
                    .and_then(Value::as_object)
                    .ok_or("json_schema 必须为对象")?
                    .clone(),
            );
            f["type"] = json!("json_schema");
        }
        out["text"] = json!({"format":f});
    }
    let tools = tool_definitions(req, false)?;
    if !tools.is_empty() {
        out["tools"] = json!(tools);
        if let Some(c) = choice(req, false)? {
            out["tool_choice"] = c;
        }
    }
    Ok(out)
}
fn message_request(req: &Value) -> Result<Value, String> {
    single_choice(req)?;
    for field in [
        "response_format",
        "reasoning_effort",
        "seed",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
    ] {
        if req.get(field).is_some_and(|v| !v.is_null()) {
            return Err(format!("Messages 上游不支持跨协议参数 {}", field));
        }
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut system = Vec::new();
    for m in req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages 必须为数组")?
    {
        let role = nonempty(m, "role")?;
        if ["system", "developer"].contains(&role) {
            let parts = content_parts(m.get("content").unwrap_or(&Value::Null), true, false)?;
            if parts
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p["type"] != "text"))
            {
                return Err("system 消息只能包含文本".into());
            }
            system.extend(parts.as_array().unwrap().iter().cloned());
            continue;
        }
        let mut blocks = if role == "tool" {
            vec![
                json!({"type":"tool_result","tool_use_id":nonempty(m,"tool_call_id")?,"content":m.get("content").cloned().unwrap_or(json!(""))}),
            ]
        } else {
            if !["user", "assistant"].contains(&role) {
                return Err("不支持的消息角色".into());
            }
            content_parts(
                m.get("content").unwrap_or(&Value::Null),
                true,
                role == "assistant",
            )?
            .as_array()
            .unwrap()
            .clone()
        };
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let args: Value = serde_json::from_str(nonempty(&call["function"], "arguments")?)
                    .map_err(|_| "工具 arguments 不是合法 JSON")?;
                if !args.is_object() {
                    return Err("工具 arguments 必须为对象".into());
                }
                blocks.push(json!({"type":"tool_use","id":nonempty(call,"id")?,"name":nonempty(&call["function"],"name")?,"input":args}));
            }
        }
        let target_role = if role == "tool" { "user" } else { role };
        if messages
            .last()
            .is_some_and(|last| last["role"] == target_role)
        {
            messages.last_mut().unwrap()["content"]
                .as_array_mut()
                .unwrap()
                .append(&mut blocks);
        } else {
            messages.push(json!({"role":target_role,"content":blocks}));
        }
    }
    let mut out = json!({"model":req["model"],"messages":messages,"max_tokens":req.get("max_completion_tokens").or_else(||req.get("max_tokens")).cloned().unwrap_or(json!(4096))});
    copy_fields(req, &mut out, &["stream", "temperature", "top_p", "top_k"]);
    if !system.is_empty() {
        out["system"] = json!(system);
    }
    if let Some(stop) = req.get("stop") {
        out["stop_sequences"] = if stop.is_string() {
            json!([stop])
        } else {
            stop.clone()
        };
    }
    let tools = tool_definitions(req, true)?;
    if !tools.is_empty() {
        out["tools"] = json!(tools);
        if let Some(c) = choice(req, true)? {
            out["tool_choice"] = c;
        }
    }
    Ok(out)
}

fn usage(protocol: Protocol, u: &Value) -> Value {
    let (input, output) = if protocol == Protocol::Messages || protocol == Protocol::Responses {
        ("input_tokens", "output_tokens")
    } else {
        ("prompt_tokens", "completion_tokens")
    };
    let mut p = util::int_or(u.get(input), 0).max(0);
    if protocol == Protocol::Messages {
        p = p
            .saturating_add(util::int_or(u.get("cache_creation_input_tokens"), 0).max(0))
            .saturating_add(util::int_or(u.get("cache_read_input_tokens"), 0).max(0));
    }
    let c = util::int_or(u.get(output), 0).max(0);
    let mut out =
        json!({"prompt_tokens":p,"completion_tokens":c,"total_tokens":p.saturating_add(c)});
    if protocol == Protocol::Responses {
        if let Some(v) = u.get("input_tokens_details") {
            out["prompt_tokens_details"] = v.clone();
        }
        if let Some(v) = u.get("output_tokens_details") {
            out["completion_tokens_details"] = v.clone();
        }
    }
    if protocol == Protocol::Messages {
        out["prompt_tokens_details"] = json!({"cached_tokens":util::int_or(u.get("cache_read_input_tokens"),0).max(0),"cache_write_tokens":util::int_or(u.get("cache_creation_input_tokens"),0).max(0)});
    }
    out
}
fn stop(protocol: Protocol, reason: &str) -> &'static str {
    match (protocol, reason) {
        (_, "length" | "max_tokens" | "model_context_window_exceeded") => "length",
        (_, "content_filter" | "refusal") => "content_filter",
        (_, "tool_use" | "tool_calls") => "tool_calls",
        _ => "stop",
    }
}
fn validate_message_stop(reason: &str) -> Result<(), String> {
    if [
        "end_turn",
        "stop_sequence",
        "max_tokens",
        "tool_use",
        "refusal",
        "model_context_window_exceeded",
    ]
    .contains(&reason)
    {
        Ok(())
    } else {
        Err(format!("无法跨协议表示 Messages stop_reason {}", reason))
    }
}
fn response_finish(v: &Value) -> Result<&'static str, String> {
    match v["status"].as_str() {
        Some("completed") => Ok("stop"),
        Some("incomplete") => Ok(
            if v.pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                == Some("content_filter")
            {
                "content_filter"
            } else {
                "length"
            },
        ),
        _ => Err("Responses 上游未返回 completed/incomplete 状态".into()),
    }
}
fn reply_to_chat(protocol: Protocol, v: &Value) -> Result<Value, String> {
    if protocol == Protocol::Chat {
        return Ok(v.clone());
    }
    if protocol == Protocol::Completions {
        let mut out = v.clone();
        out["object"] = json!("chat.completion");
        for c in out
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .ok_or("缺少 choices")?
        {
            let text = c
                .get("text")
                .and_then(Value::as_str)
                .ok_or("缺少 completion text")?
                .to_string();
            c.as_object_mut().unwrap().remove("text");
            c["message"] = json!({"role":"assistant","content":text});
        }
        return Ok(out);
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools = Vec::new();
    let finish = if protocol == Protocol::Responses {
        let finish = response_finish(v)?;
        for item in v
            .get("output")
            .and_then(Value::as_array)
            .ok_or("缺少 Responses output")?
        {
            match item["type"].as_str() {
                Some("message")=>for p in item.get("content").and_then(Value::as_array).ok_or("缺少 message content")? {match p["type"].as_str(){Some("output_text")=>text.push_str(p["text"].as_str().ok_or("缺少 output_text")?),Some("refusal")=>text.push_str(p["refusal"].as_str().unwrap_or("")),_=>return Err("无法转换上游输出内容类型".into())}},
                Some("reasoning")=>if let Some(parts)=item.get("summary").and_then(Value::as_array){for p in parts {reasoning.push_str(p["text"].as_str().unwrap_or(""));}},
                Some("function_call")=>tools.push(json!({"id":nonempty(item,"call_id")?,"type":"function","function":{"name":nonempty(item,"name")?,"arguments":nonempty(item,"arguments")?}})),
                // 服务端内置工具动作（web_search_call 等）：结果已体现在后续
                // message 文本里，chat 协议无对应承载形式，跳过
                _=>{},
            }
        }
        if !tools.is_empty() && finish == "stop" {
            "tool_calls"
        } else {
            finish
        }
    } else {
        let reason = nonempty(v, "stop_reason")?;
        validate_message_stop(reason)?;
        for p in v
            .get("content")
            .and_then(Value::as_array)
            .ok_or("缺少 Messages content")?
        {
            match p["type"].as_str(){
                Some("text")=>text.push_str(p["text"].as_str().ok_or("缺少 text")?),
                Some("thinking")=>reasoning.push_str(p["thinking"].as_str().unwrap_or("")),
                Some("redacted_thinking")=>{},
                Some("tool_use")=>tools.push(json!({"id":nonempty(p,"id")?,"type":"function","function":{"name":nonempty(p,"name")?,"arguments":serde_json::to_string(p.get("input").ok_or("缺少工具 input")?).map_err(|e|e.to_string())?}})),
                _=>return Err("跨协议转换不支持此 Messages 输出类型".into()),
            }
        }
        stop(protocol, reason)
    };
    let mut message = json!({"role":"assistant","content":text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tools.is_empty() {
        message["tool_calls"] = json!(tools);
    }
    let mut out = json!({"id":v.get("id").cloned().unwrap_or(json!(util::rand_id("chatcmpl-"))),"object":"chat.completion","created":v.get("created_at").cloned().unwrap_or(json!(util::now_i())),"model":v["model"],"choices":[{"index":0,"message":message,"finish_reason":finish}]});
    if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
        out["usage"] = usage(protocol, u);
    }
    Ok(out)
}
fn chat_to_completion(mut chat: Value, stream: bool) -> Result<Value, String> {
    chat["object"] = json!("text_completion");
    for c in chat
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .ok_or("缺少 choices")?
    {
        let key = if stream { "delta" } else { "message" };
        if c.get(key).is_some_and(|m| {
            m.get("tool_calls")
                .is_some_and(|t| t.as_array().is_some_and(|a| !a.is_empty()))
        }) {
            return Err("Completions 下游不能表示工具调用".into());
        }
        let text = c
            .get(key)
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        c.as_object_mut().ok_or("无效 choice")?.remove(key);
        c["text"] = json!(text);
    }
    Ok(chat)
}

struct NativeStream {
    plan: Plan,
    decoder: sse::Decoder,
    terminal: bool,
    id: String,
    model: String,
    tools: HashMap<i64, i64>,
    args: HashSet<i64>,
    seen_text: bool,
    seen_reasoning: bool,
    usage: Value,
    finish: Option<String>,
}
impl NativeStream {
    fn new(plan: Plan) -> Self {
        Self {
            plan,
            decoder: sse::Decoder::default(),
            terminal: false,
            id: util::rand_id("chatcmpl-"),
            model: String::new(),
            tools: HashMap::new(),
            args: HashSet::new(),
            seen_text: false,
            seen_reasoning: false,
            usage: json!({}),
            finish: None,
        }
    }
    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":util::now_i(),"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }
    fn emit(&self, v: Value, out: &mut Vec<Bytes>) -> Result<(), String> {
        let v = if self.plan.internal == Protocol::Completions {
            chat_to_completion(v, true)?
        } else {
            v
        };
        out.push(Bytes::from(format!("data: {}\n\n", v)));
        Ok(())
    }
    fn done(&mut self, finish: &str, out: &mut Vec<Bytes>) -> Result<(), String> {
        self.emit(self.chunk(json!({}), Some(finish)), out)?;
        if self.usage.is_object() && !self.usage.as_object().unwrap().is_empty() {
            let mut u = self.chunk(json!({}), None);
            u["choices"] = json!([]);
            u["usage"] = usage(self.plan.upstream, &self.usage);
            self.emit(u, out)?;
        }
        out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        self.terminal = true;
        Ok(())
    }
    fn tool(&mut self, native: i64) -> Result<i64, String> {
        if let Some(&i) = self.tools.get(&native) {
            return Ok(i);
        }
        if self.tools.len() >= 1024 {
            return Err("上游工具调用数量超过限制".into());
        }
        let i = self.tools.len() as i64;
        self.tools.insert(native, i);
        Ok(i)
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Bytes>, String> {
        let mut out = Vec::new();
        for raw in self.decoder.feed(bytes)? {
            if self.terminal {
                break;
            }
            let Some(data) = sse::data(&raw) else {
                continue;
            };
            if data == "[DONE]" {
                if matches!(self.plan.upstream, Protocol::Chat | Protocol::Completions) {
                    out.push(Bytes::from_static(b"data: [DONE]\n\n"));
                    self.terminal = true;
                    continue;
                }
                return Err("原生协议在终端事件前返回 [DONE]".into());
            }
            let v: Value = serde_json::from_str(&data).map_err(|_| "上游原生 SSE 包含无效 JSON")?;
            if v.get("error").is_some_and(|e| !e.is_null())
                || v["type"] == "error"
                || v["type"] == "response.failed"
            {
                return Err(v
                    .pointer("/error/message")
                    .or_else(|| v.pointer("/response/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("上游原生协议返回错误")
                    .into());
            }
            match self.plan.upstream {
                Protocol::Chat => self.emit(v, &mut out)?,
                Protocol::Completions => {
                    let mut chat = v;
                    chat["object"] = json!("chat.completion.chunk");
                    for c in chat
                        .get_mut("choices")
                        .and_then(Value::as_array_mut)
                        .ok_or("缺少 choices")?
                    {
                        let text = c
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        c.as_object_mut().ok_or("无效 choice")?.remove("text");
                        c["delta"] = json!({"content":text});
                    }
                    self.emit(chat, &mut out)?;
                }
                Protocol::Responses => self.response_event(&v, &mut out)?,
                Protocol::Messages => self.message_event(&v, &mut out)?,
                Protocol::Embeddings => return Err("Embeddings 不支持 SSE 转换".into()),
            }
        }
        Ok(out)
    }
    fn response_event(&mut self, v: &Value, out: &mut Vec<Bytes>) -> Result<(), String> {
        let typ = v["type"].as_str().ok_or("Responses 事件缺少 type")?;
        let native = v["output_index"].as_i64().unwrap_or(0);
        match typ {
            "response.created" | "response.in_progress" => {
                let r = &v["response"];
                if let Some(id) = r["id"].as_str() {
                    self.id = id.into();
                }
                if let Some(model) = r["model"].as_str() {
                    self.model = model.into();
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let text = v["delta"].as_str().ok_or("无效文本 delta")?;
                self.seen_text |= !text.is_empty();
                self.emit(self.chunk(json!({"content":text}), None), out)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let text = v["delta"].as_str().ok_or("无效推理 delta")?;
                self.seen_reasoning |= !text.is_empty();
                self.emit(self.chunk(json!({"reasoning_content":text}), None), out)?;
            }
            "response.output_item.added" => {
                let item = &v["item"];
                if item["type"] == "function_call" {
                    let i = self.tool(native)?;
                    let args = item["arguments"].as_str().unwrap_or("");
                    if !args.is_empty() {
                        self.args.insert(native);
                    }
                    self.emit(self.chunk(json!({"tool_calls":[{"index":i,"id":nonempty(item,"call_id")?,"type":"function","function":{"name":nonempty(item,"name")?,"arguments":args}}]}),None),out)?;
                }
                // message/reasoning/function_call 之外的项目（web_search_call 等
                // 服务端内置工具）chat 协议无承载形式：跳过，不中断流
            }
            "response.function_call_arguments.delta" => {
                let i = *self
                    .tools
                    .get(&native)
                    .ok_or("工具参数事件缺少对应的工具开始事件")?;
                self.args.insert(native);
                self.emit(self.chunk(json!({"tool_calls":[{"index":i,"function":{"arguments":nonempty(v,"delta")?}}]}),None),out)?;
            }
            "response.function_call_arguments.done" => {
                if !self.args.contains(&native) {
                    let i = *self
                        .tools
                        .get(&native)
                        .ok_or("工具结束事件缺少对应的开始事件")?;
                    let arguments = v["arguments"]
                        .as_str()
                        .ok_or("工具结束事件缺少 arguments")?;
                    self.emit(
                        self.chunk(
                            json!({"tool_calls":[{"index":i,"function":{"arguments":arguments}}]}),
                            None,
                        ),
                        out,
                    )?;
                    self.args.insert(native);
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = &v["response"];
                let chat = reply_to_chat(Protocol::Responses, r)?;
                if !self.seen_text {
                    if let Some(text) = chat
                        .pointer("/choices/0/message/content")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        self.emit(self.chunk(json!({"content":text}), None), out)?;
                    }
                }
                if !self.seen_reasoning {
                    if let Some(text) = chat
                        .pointer("/choices/0/message/reasoning_content")
                        .and_then(Value::as_str)
                    {
                        self.emit(self.chunk(json!({"reasoning_content":text}), None), out)?;
                    }
                }
                if self.tools.is_empty() {
                    if let Some(calls) = chat
                        .pointer("/choices/0/message/tool_calls")
                        .and_then(Value::as_array)
                    {
                        for (i, c) in calls.iter().enumerate() {
                            let mut c = c.clone();
                            c["index"] = json!(i);
                            self.emit(self.chunk(json!({"tool_calls":[c]}), None), out)?;
                        }
                    }
                }
                self.usage = r.get("usage").cloned().unwrap_or(json!({}));
                self.done(
                    chat.pointer("/choices/0/finish_reason")
                        .and_then(Value::as_str)
                        .unwrap_or("stop"),
                    out,
                )?;
            }
            _ => {}
        }
        Ok(())
    }
    fn message_event(&mut self, v: &Value, out: &mut Vec<Bytes>) -> Result<(), String> {
        let typ = v["type"].as_str().ok_or("Messages 事件缺少 type")?;
        let native = v["index"].as_i64().unwrap_or(0);
        match typ {
            "message_start" => {
                let m = &v["message"];
                self.id = nonempty(m, "id")?.into();
                self.model = m["model"].as_str().unwrap_or("").into();
                self.usage = m.get("usage").cloned().unwrap_or(json!({}));
                self.emit(self.chunk(json!({"role":"assistant"}), None), out)?;
            }
            "content_block_start" => {
                let b = &v["content_block"];
                match b["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = b["text"].as_str().filter(|s| !s.is_empty()) {
                            self.emit(self.chunk(json!({"content":text}), None), out)?;
                        }
                    }
                    Some("thinking") | Some("redacted_thinking") => {}
                    Some("tool_use") => {
                        let i = self.tool(native)?;
                        let args = if b
                            .get("input")
                            .and_then(Value::as_object)
                            .is_some_and(|a| !a.is_empty())
                        {
                            self.args.insert(native);
                            b["input"].to_string()
                        } else {
                            String::new()
                        };
                        self.emit(self.chunk(json!({"tool_calls":[{"index":i,"id":nonempty(b,"id")?,"type":"function","function":{"name":nonempty(b,"name")?,"arguments":args}}]}),None),out)?;
                    }
                    _ => {}
                    // 未知内容块（document 等）：chat 协议无承载形式，跳过不中断
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        self.emit(self.chunk(json!({"content":d["text"]}), None), out)?
                    }
                    Some("thinking_delta") => self.emit(
                        self.chunk(json!({"reasoning_content":d["thinking"]}), None),
                        out,
                    )?,
                    Some("input_json_delta") => {
                        let i = *self
                            .tools
                            .get(&native)
                            .ok_or("工具参数缺少对应的开始事件")?;
                        self.args.insert(native);
                        self.emit(self.chunk(json!({"tool_calls":[{"index":i,"function":{"arguments":d["partial_json"]}}]}),None),out)?;
                    }
                    Some("signature_delta") => {}
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(&i) = self.tools.get(&native) {
                    if !self.args.contains(&native) {
                        self.emit(
                            self.chunk(
                                json!({"tool_calls":[{"index":i,"function":{"arguments":"{}"}}]}),
                                None,
                            ),
                            out,
                        )?;
                    }
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage").and_then(Value::as_object) {
                    let obj = self.usage.as_object_mut().ok_or("无效 Messages usage")?;
                    for (k, val) in u {
                        obj.insert(k.clone(), val.clone());
                    }
                }
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    validate_message_stop(reason)?;
                    self.finish = Some(stop(Protocol::Messages, reason).into());
                }
            }
            "message_stop" => {
                let reason = self.finish.clone().ok_or("Messages 未返回 stop_reason")?;
                self.done(&reason, out)?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Bound decompressed bodies before allocation, including upstream HTTP errors.
pub async fn read_text(response: reqwest::Response) -> Result<String, io::Error> {
    read_text_limit(response, crate::webhttp::MAX_BODY).await
}
async fn read_text_limit(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<String, io::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(io::Error::other("上游响应体超过大小上限"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| io::Error::other("上游响应不是有效 UTF-8"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    fn chat_request() -> Value {
        json!({"model":"model","messages":[{"role":"system","content":"rules"},{"role":"user","content":"你好"}],"stream":true,"max_tokens":32})
    }
    fn event(v: Value) -> String {
        format!("data: {}\r\n\r\n", v)
    }
    fn response_fixture() -> Value {
        json!({"id":"resp_test","model":"private","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"matrix ok"}]}],"usage":{"input_tokens":5,"output_tokens":7}})
    }
    fn message_fixture() -> Value {
        json!({"id":"msg_test","type":"message","model":"private","role":"assistant","content":[{"type":"text","text":"matrix ok"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":7}})
    }
    pub(super) fn wire(protocol: Protocol, tool: bool) -> String {
        let mut frames = Vec::new();
        match protocol {
            Protocol::Responses => {
                frames.push(json!({"type":"response.created","response":{"id":"resp_test","model":"private"}}));
                if tool {
                    frames.push(json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_test","name":"weather","arguments":""}}));
                    frames.push(json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"city\":\"上海\"}"}));
                } else {
                    frames.push(json!({"type":"response.output_text.delta","output_index":0,"delta":"matrix ok"}));
                }
                let mut response = response_fixture();
                if tool {
                    response["output"] = json!([{"type":"function_call","call_id":"call_test","name":"weather","arguments":"{\"city\":\"上海\"}"}]);
                }
                frames.push(json!({"type":"response.completed","response":response}));
            }
            Protocol::Messages => {
                frames.push(json!({"type":"message_start","message":{"id":"msg_test","model":"private","usage":{"input_tokens":5,"output_tokens":0}}}));
                if tool {
                    frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_test","name":"weather","input":{}}}));
                    frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"上海\"}"}}));
                } else {
                    frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                    frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"matrix ok"}}));
                }
                frames.push(json!({"type":"content_block_stop","index":0}));
                frames.push(json!({"type":"message_delta","delta":{"stop_reason":if tool{"tool_use"}else{"end_turn"}},"usage":{"output_tokens":7}}));
                frames.push(json!({"type":"message_stop"}));
            }
            Protocol::Chat | Protocol::Completions => {
                let choice = if protocol == Protocol::Completions {
                    json!({"index":0,"text":"matrix ok","finish_reason":"stop"})
                } else if tool {
                    json!({"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_test","type":"function","function":{"name":"weather","arguments":"{\"city\":\"上海\"}"}}]},"finish_reason":"tool_calls"})
                } else {
                    json!({"index":0,"delta":{"content":"matrix ok"},"finish_reason":"stop"})
                };
                frames.push(json!({"id":"chatcmpl-test","model":"private","choices":[choice],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}));
            }
            Protocol::Embeddings => unreachable!(),
        }
        let mut text = frames.into_iter().map(event).collect::<String>();
        if matches!(protocol, Protocol::Chat | Protocol::Completions) {
            text.push_str("data: [DONE]\r\n\r\n");
        }
        text
    }
    #[test]
    fn upstream_selection_and_legacy_compatibility() {
        let chat = chat_request();
        for setting in ["chat/completions", "responses", "messages", "completions"] {
            let (body, p) = Plan::prepare("chat/completions", setting, &chat).unwrap();
            assert_eq!(p.upstream.path(), setting);
            assert_eq!(body["model"], "model");
            let (_, cmp) = Plan::prepare(
                "completions",
                setting,
                &json!({"model":"model","prompt":"hi"}),
            )
            .unwrap();
            assert_eq!(cmp.upstream.path(), setting);
        }
        assert_eq!(
            Plan::prepare("chat/completions", "auto", &chat).unwrap().0,
            chat
        );
        assert_eq!(
            Plan::prepare("completions", "auto", &json!({}))
                .unwrap()
                .1
                .upstream,
            Protocol::Completions
        );
        assert_eq!(
            Plan::prepare("embeddings", "messages", &json!({"input":"hi"}))
                .unwrap()
                .1
                .upstream,
            Protocol::Embeddings
        );
        assert!(Plan::prepare("chat/completions", "https://evil.test/path", &chat).is_err());
    }
    #[test]
    fn function_history_and_images_survive_request_conversion() {
        let mut req = chat_request();
        req["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_test","type":"function","function":{"name":"weather","arguments":"{\"city\":\"上海\"}"}}]}),
            json!({"role":"tool","tool_call_id":"call_test","content":"sunny"}),
            json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]})
        ]);
        req["tools"] = json!([{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{}}}}]);
        req["tool_choice"] = json!({"type":"function","function":{"name":"weather"}});
        let responses = response_request(&req).unwrap();
        assert_eq!(responses["input"][2]["type"], "function_call");
        assert_eq!(responses["input"][3]["type"], "function_call_output");
        assert_eq!(responses["input"][4]["content"][0]["type"], "input_image");
        assert_eq!(responses["tool_choice"]["name"], "weather");
        let messages = message_request(&req).unwrap();
        assert_eq!(messages["system"][0]["text"], "rules");
        assert_eq!(messages["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(messages["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            messages["messages"][2]["content"][1]["source"]["type"],
            "base64"
        );
        assert!(legacy_request(&req).is_err());
    }
    #[test]
    fn malformed_and_unrepresentable_parameters_are_rejected() {
        let mut req = chat_request();
        req["response_format"] = json!({"type":"json_schema","json_schema":"invalid"});
        assert!(response_request(&req).is_err());
        req = chat_request();
        req["n"] = json!(2);
        assert!(response_request(&req).is_err());
        assert!(message_request(&req).is_err());
        req = chat_request();
        req["messages"][0]["content"] = json!([{"type":"input_audio","data":"test"}]);
        assert!(response_request(&req).is_err());
        assert!(completion_request(&json!({"prompt":["a","b"]})).is_err());
    }
    #[test]
    fn native_streams_preserve_tools_usage_and_all_byte_boundaries() {
        for protocol in [
            Protocol::Responses,
            Protocol::Messages,
            Protocol::Completions,
        ] {
            for tool in [false, true] {
                if tool && protocol == Protocol::Completions {
                    continue;
                }
                let wire = wire(protocol, tool);
                let mut bridge = NativeStream::new(Plan {
                    upstream: protocol,
                    internal: Protocol::Chat,
                });
                let mut output = Vec::new();
                for byte in wire.as_bytes().chunks(1) {
                    output.extend(bridge.feed(byte).unwrap());
                }
                assert!(bridge.terminal);
                let text = String::from_utf8(output.into_iter().flat_map(|b| b.to_vec()).collect())
                    .unwrap();
                assert!(text.contains("[DONE]"));
                assert!(text.contains("prompt_tokens"));
                if tool {
                    assert_eq!(text.matches("weather").count(), 1);
                    assert!(text.contains("上海"));
                } else {
                    assert!(text.contains("matrix ok"));
                }
            }
        }
    }
    #[test]
    fn tool_arguments_done_is_used_when_the_provider_omits_deltas() {
        let mut bridge = NativeStream::new(Plan {
            upstream: Protocol::Responses,
            internal: Protocol::Chat,
        });
        let wire = event(
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_test","name":"weather","arguments":""}}),
        ) + &event(
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{}"}),
        );
        let bytes = bridge.feed(wire.as_bytes()).unwrap();
        let text = String::from_utf8(bytes.into_iter().flat_map(|b| b.to_vec()).collect()).unwrap();
        let payloads: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            payloads[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }

    #[test]
    fn native_error_and_truncated_terminal_are_not_completed() {
        for protocol in [Protocol::Responses, Protocol::Messages] {
            let mut bridge = NativeStream::new(Plan {
                upstream: protocol,
                internal: Protocol::Chat,
            });
            assert!(bridge
                .feed(event(json!({"type":"error","error":{"message":"overloaded"}})).as_bytes())
                .is_err());
            assert!(!bridge.terminal);
        }
        let mut bridge = NativeStream::new(Plan {
            upstream: Protocol::Responses,
            internal: Protocol::Chat,
        });
        assert!(bridge.feed(b"data: [DONE]\n\n").is_err());
        assert!(
            reply_to_chat(Protocol::Responses, &json!({"status":"failed","output":[]})).is_err()
        );
    }
    #[test]
    fn usage_handles_cache_and_extreme_numbers_without_overflow() {
        assert_eq!(
            usage(
                Protocol::Messages,
                &json!({"input_tokens":5,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"output_tokens":7})
            )["prompt_tokens"],
            10
        );
        assert_eq!(
            usage(
                Protocol::Messages,
                &json!({"input_tokens":i64::MAX,"cache_read_input_tokens":i64::MAX,"output_tokens":i64::MAX})
            )["total_tokens"],
            i64::MAX
        );
        assert_eq!(
            crate::proxy::full_usage(Some(
                &json!({"prompt_tokens":i64::MAX,"completion_tokens":i64::MAX})
            ))["total_tokens"],
            i64::MAX
        );
    }
    #[tokio::test]
    async fn response_size_and_utf8_are_bounded() {
        let response =
            reqwest::Response::from(axum::http::Response::new(reqwest::Body::from(vec![
                b'a';
                32
            ])));
        assert!(read_text_limit(response, 16)
            .await
            .unwrap_err()
            .to_string()
            .contains("大小"));
        let response =
            reqwest::Response::from(axum::http::Response::new(reqwest::Body::from(vec![255])));
        assert!(read_text_limit(response, 16).await.is_err());
    }
    #[test]
    fn native_features_degrade_without_errors_and_cache_usage_roundtrips() {
        // 无 file_id 的输入文件/图片引用仍显式拒绝（无法转换内容本体）
        assert!(crate::convert::responses_to_chat(&json!({"input":[{"role":"user","content":[{"type":"input_file","file_id":"file_123"}]}]})).is_err());
        assert!(crate::convert::responses_to_chat(&json!({"input":[{"role":"user","content":[{"type":"input_image","file_id":"file_123"}]}]})).is_err());

        // 服务端内置工具（web_search 等）：跳过而非整单拒绝——chat 上游没有
        // 承载形式，但请求本身（文本/函数工具）应照常转发
        let chat = crate::convert::responses_to_chat(
            &json!({"input":"hi","tools":[{"type":"web_search"},{"type":"function","name":"w","parameters":{"type":"object"}}],"tool_choice":{"type":"allowed_tools","mode":"auto"}}),
        )
        .unwrap();
        assert_eq!(chat["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(chat["tools"][0]["type"], "function");
        assert!(chat.get("tool_choice").is_none());
        // 服务端动作历史（web_search_call / reasoning）：跳过
        let chat = crate::convert::responses_to_chat(&json!({"input":[
            {"type":"web_search_call"},
            {"type":"reasoning","summary":[]},
            {"role":"user","content":"hi"}
        ]}))
        .unwrap();
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(1));
        // 带签名的思考历史：转成 reasoning_content 继续携带（不丢弃）
        let chat = crate::convert::anthropic_to_chat(&json!({"messages":[
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"step 1","signature":"sig"},
                {"type":"text","text":"answer"}
            ]}
        ]}))
        .unwrap();
        assert_eq!(chat["messages"][0]["reasoning_content"], "step 1");
        assert_eq!(chat["messages"][0]["content"], "answer");
        // 工具结果里的图片：提取为 tool 消息之后的多模态 user 消息
        let chat = crate::convert::anthropic_to_chat(&json!({"messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}}
                ]}
            ]}
        ]}))
        .unwrap();
        assert_eq!(chat["messages"][0]["role"], "tool");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(chat["messages"][1]["content"][0]["type"], "image_url");
        let chat_usage = usage(
            Protocol::Messages,
            &json!({"input_tokens":5,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":7}),
        );
        assert_eq!(
            crate::convert::anthropic_usage(Some(&chat_usage), 0),
            json!({"input_tokens":5,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2})
        );
        let request=crate::convert::responses_to_chat(&json!({"input":"hi","text":{"format":{"type":"json_schema","name":"test","schema":{"type":"object"}}}})).unwrap();
        assert_eq!(
            response_request(&request).unwrap()["text"]["format"]["name"],
            "test"
        );
    }
    #[tokio::test]
    async fn configured_auth_is_independent_of_client_protocol() {
        let client = reqwest::Client::new();
        let messages = Plan {
            upstream: Protocol::Messages,
            internal: Protocol::Chat,
        };
        let request = messages
            .auth(
                client.post("https://example.test/v1/messages"),
                "own-key",
                "bearer",
            )
            .build()
            .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer own-key");
        assert!(!request.headers().contains_key("x-api-key"));
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        let responses = Plan {
            upstream: Protocol::Responses,
            internal: Protocol::Chat,
        };
        let request = responses
            .auth(
                client.post("https://example.test/v1/responses"),
                "own-key",
                "x-api-key",
            )
            .build()
            .unwrap();
        assert_eq!(request.headers()["x-api-key"], "own-key");
        assert!(!request.headers().contains_key("authorization"));
    }

    pub async fn gateway_matrix() {
        use crate::{pool, store::store, users};
        use axum::{
            http::{HeaderMap, StatusCode},
            response::{IntoResponse, Response},
        };
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        let seen = Arc::new(Mutex::new(Vec::<(String, HeaderMap, Value)>::new()));
        let captured = seen.clone();
        let mode = Arc::new(Mutex::new(String::new()));
        let mode_for_handler = mode.clone();
        let destination_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = destination_hits.clone();
        let destination = axum::Router::new().fallback(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "should not receive credentials"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_addr = listener.local_addr().unwrap();
        let redirect_server =
            tokio::spawn(async move { axum::serve(listener, destination).await.unwrap() });
        let app=axum::Router::new().route("/{*endpoint}",axum::routing::post(move |axum::extract::Path(endpoint):axum::extract::Path<String>,headers:HeaderMap,axum::Json(request):axum::Json<Value>| {
            captured.lock().unwrap().push((endpoint.clone(),headers,request.clone()));
            let mode=mode_for_handler.lock().unwrap().clone();
            async move {
                if mode=="redirect" {return (StatusCode::TEMPORARY_REDIRECT,[("location",format!("http://{redirect_addr}/collect"))]).into_response();}
                if mode == "slow" || mode == "silentheaders" {
                    tokio::time::sleep(Duration::from_millis(if mode=="slow" { 1300 } else { 5000 })).await;
                }
                if endpoint=="embeddings"{return axum::Json(json!({"object":"list","data":[{"index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":5,"completion_tokens":0}})).into_response();}
                let protocol=Protocol::parse(&endpoint).unwrap();
                if request["stream"]==true {
                    let text=if mode=="truncated" {
                        match protocol {Protocol::Responses=>event(json!({"type":"response.output_text.delta","delta":"partial"})),Protocol::Messages=>event(json!({"type":"message_start","message":{"id":"msg_test","model":"private","usage":{"input_tokens":5,"output_tokens":0}}}))+&event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}})),_=>unreachable!()}
                    } else {wire(protocol,false)};
                    let chunks:Vec<Result<Bytes,io::Error>>=text.as_bytes().chunks(3).map(|bytes|Ok(Bytes::copy_from_slice(bytes))).collect();
                    let mut response=Response::new(axum::body::Body::from_stream(futures::stream::iter(chunks)));
                    response.headers_mut().insert("content-type","text/event-stream".parse().unwrap());response
                } else {
                    let body=match protocol {
                        Protocol::Responses=>response_fixture(),Protocol::Messages=>message_fixture(),
                        Protocol::Chat=>json!({"id":"chat_test","model":"private","choices":[{"index":0,"message":{"role":"assistant","content":"matrix ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}),
                        Protocol::Completions=>json!({"id":"cmpl_test","model":"private","choices":[{"index":0,"text":"matrix ok","finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}),_=>unreachable!()
                    };axum::Json(body).into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = listener.local_addr().unwrap();
        let router = crate::build_router();
        let gateway = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        store().update(|db|{
            db["users"]=json!([{"id":"user","username":"test","enabled":true,"balance":1000.0,"grant":0.0,"free_rpm":0,"paid_rpm":0}]);
            for field in ["rate_limit_per_minute","tpm_limit","account_cooldown_ms","daily_request_cap","daily_token_limit","hourly_request_limit","warmup_seconds","retry_backoff_base_ms","retry_backoff_max_ms","cool_5xx_seconds","ban_step_seconds"]{db["config"][field]=json!(0);}
            db["config"]["queue_enabled"]=json!(false);db["config"]["max_retries"]=json!(1);
        });
        let user_key = users::add_key("user", "matrix");
        let token = user_key["key"].as_str().unwrap();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap();
        let mut success_count = 0;
        for upstream in ["chat/completions", "responses", "messages", "completions"] {
            let key = pool::new_key("matrix", "", "upstream-secret", "matrix");
            store().update(|db|{
                db["upstreams"]=json!([{"id":"matrix","name":"matrix","base":format!("http://{upstream_addr}"),"api_endpoint":upstream,"enabled":true,"models":["public"],"model_map":{"public":"private"},"prices":{"public":1.0}}]);db["keys"]=json!([key]);db["model_breaker"]=json!({});
            });
            for stream in [false, true] {
                for downstream in ["chat/completions", "responses", "messages", "completions"] {
                    let mut request = match downstream {
                        "responses" => json!({"input":"hi"}),
                        "completions" => json!({"prompt":"hi"}),
                        _ => json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":32}),
                    };
                    request["model"] = json!("public");
                    request["stream"] = json!(stream);
                    let response = client
                        .post(format!("http://{gateway_addr}/v1/{downstream}"))
                        .bearer_auth(token)
                        .header("x-api-key", "client-secret-never-forward")
                        .header("cookie", "session=customer")
                        .header("user-agent", "client-agent-never-forward")
                        .json(&request)
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let text = response.text().await.unwrap();
                    assert_eq!(
                        status, 200,
                        "upstream={upstream} downstream={downstream} stream={stream}: {text}"
                    );
                    assert!(
                        text.contains("matrix ok"),
                        "{upstream} -> {downstream}: {text}"
                    );
                    assert!(!text.contains("private"), "alias leaked: {text}");
                    if stream {
                        assert!(
                            text.contains(match downstream {
                                "responses" => "response.completed",
                                "messages" => "message_stop",
                                _ => "[DONE]",
                            }),
                            "{text}"
                        );
                    } else {
                        let result: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(result["model"], "public");
                        match downstream {
                            "responses" => {
                                assert_eq!(result["output"][0]["content"][0]["text"], "matrix ok")
                            }
                            "messages" => assert_eq!(result["content"][0]["text"], "matrix ok"),
                            "completions" => assert_eq!(result["choices"][0]["text"], "matrix ok"),
                            _ => {
                                assert_eq!(result["choices"][0]["message"]["content"], "matrix ok")
                            }
                        }
                    }
                    let (path, headers, body) = seen.lock().unwrap().last().unwrap().clone();
                    assert_eq!(path, upstream);
                    assert_eq!(body["model"], "private");
                    assert_eq!(body["stream"], stream);
                    if upstream == "messages" {
                        assert_eq!(headers["x-api-key"], "upstream-secret");
                        assert_eq!(headers["anthropic-version"], "2023-06-01");
                        assert!(!headers.contains_key("authorization"));
                    } else {
                        assert_eq!(headers["authorization"], "Bearer upstream-secret");
                        assert!(!headers.contains_key("x-api-key"));
                    }
                    assert!(!headers.contains_key("cookie"));
                    assert_ne!(
                        headers.get("user-agent").and_then(|v| v.to_str().ok()),
                        Some("client-agent-never-forward")
                    );
                    success_count += 1;
                    let db = store().load();
                    assert_eq!(
                        users::balance_of(&db["users"][0]),
                        1000.0 - success_count as f64
                    );
                    assert_eq!(db["user_logs_paid"][0]["in_tok"], 5);
                    assert_eq!(db["user_logs_paid"][0]["out_tok"], 7);
                    assert_eq!(pool::inflight_total(), 0);
                }
            }
        }
        // The same channel setting never turns an embeddings request into chat.
        let response = client
            .post(format!("http://{gateway_addr}/v1/embeddings"))
            .bearer_auth(token)
            .json(&json!({"model":"public","input":"hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();
        assert_eq!(seen.lock().unwrap().last().unwrap().0, "embeddings");
        // Reject invalid bytes and incompatible tool requests without touching upstream.
        let before = seen.lock().unwrap().len();
        let response = client
            .post(format!("http://{gateway_addr}/v1/chat/completions"))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(vec![b'{', b'"', 255, b'"', b'}'])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let response=client.post(format!("http://{gateway_addr}/v1/chat/completions")).bearer_auth(token).json(&json!({"model":"public","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"weather","parameters":{}}}]})).send().await.unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(seen.lock().unwrap().len(), before);
        assert_eq!(pool::inflight_total(), 0);
        for upstream in ["responses", "messages"] {
            let key = pool::new_key("truncated", "", "upstream-secret", "matrix");
            store().update(|db| {
                db["upstreams"][0]["api_endpoint"] = json!(upstream);
                db["keys"] = json!([key]);
                db["model_breaker"] = json!({});
            });
            *mode.lock().unwrap() = "truncated".into();
            let balance = users::balance_of(&store().load()["users"][0]);
            let response=client.post(format!("http://{gateway_addr}/v1/chat/completions")).bearer_auth(token).json(&json!({"model":"public","stream":true,"messages":[{"role":"user","content":"hi"}]})).send().await.unwrap();
            let text = response.text().await.unwrap();
            assert!(text.contains("error"), "{text}");
            assert_eq!(users::balance_of(&store().load()["users"][0]), balance);
            assert_eq!(pool::inflight_total(), 0);
        }
        *mode.lock().unwrap() = "redirect".into();
        let key = pool::new_key("redirect", "", "upstream-secret", "matrix");
        store().update(|db| {
            db["keys"] = json!([key]);
            db["upstreams"][0]["api_endpoint"] = json!("messages");
            db["model_breaker"] = json!({});
        });
        let response = client
            .post(format!("http://{gateway_addr}/v1/chat/completions"))
            .bearer_auth(token)
            .json(&json!({"model":"public","messages":[{"role":"user","content":"hi"}]}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_server_error());
        assert_eq!(
            destination_hits.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        // Commit a heartbeat before slow native headers, then retain the native adapter.
        *mode.lock().unwrap() = "slow".into();
        let key = pool::new_key("slow", "", "upstream-secret", "matrix");
        store().update(|db| {
            db["config"]["ttfb_timeout"] = json!(1);
            db["keys"] = json!([key]);
            db["model_breaker"] = json!({});
        });
        let response = client
            .post(format!("http://{gateway_addr}/v1/responses"))
            .bearer_auth(token)
            .json(&json!({"model":"public","input":"hi","stream":true}))
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        assert!(
            text.contains("matrix ok") && text.contains("response.completed"),
            "{text}"
        );
        *mode.lock().unwrap() = "silentheaders".into();
        let key = pool::new_key("cancel-slow", "", "upstream-secret", "matrix");
        store().update(|db| {
            db["keys"] = json!([key]);
            db["model_breaker"] = json!({});
        });
        let balance = users::balance_of(&store().load()["users"][0]);
        let response=client.post(format!("http://{gateway_addr}/v1/chat/completions")).bearer_auth(token)
            .json(&json!({"model":"public","messages":[{"role":"user","content":"hi"}],"stream":true})).send().await.unwrap();
        drop(response);
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool::inflight_total() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(users::balance_of(&store().load()["users"][0]), balance);
        let (saved, error) = crate::upstreams::save(
            &json!({"id":"matrix","name":"matrix","base":format!("http://{upstream_addr}"),"enabled":false}),
        );
        assert!(error.is_empty());
        assert_eq!(saved.unwrap()["api_endpoint"], "messages");
        assert_eq!(pool::inflight_total(), 0);
        gateway.abort();
        upstream_server.abort();
        redirect_server.abort();
    }
}
