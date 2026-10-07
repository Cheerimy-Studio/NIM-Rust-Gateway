//! 流式转换状态机（core/streams.py 的移植）：上游 chat chunk SSE → 下游 Responses / Anthropic 事件流。
//!
//! feed(chunk) 逐块喂入，产出的事件写到 out（SSE 文本，`event: ...\ndata: {...}\n\n`）。

use crate::convert::map_usage;
use crate::util;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Python json.dumps(ensure_ascii=False) 的分隔符风格（", " / ": "）。
/// SSE 帧保持与 Python 版逐字节一致（测试/客户端会对原始文本做 grep）。
pub fn py_json(v: &Value) -> String {
    use serde_json::ser::Formatter;
    use std::io::{self, Write};

    struct PyFmt;
    impl Formatter for PyFmt {
        fn begin_object_key<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_value<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
            w.write_all(b": ")
        }
        fn begin_array_value<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFmt);
    v.serialize(&mut ser).ok();
    String::from_utf8_lossy(&buf).to_string()
}

pub fn sse_frame(event: &str, data: &Value) -> String {
    // OpenAI Responses SDK 用 data 里的 "type" 字段做判别（discriminator="type"）：
    // 所有经此发出的协议事件 data 缺 type 时补上（Anthropic 事件本就带）。
    let payload: Value = match data {
        Value::Object(m) if !m.contains_key("type") => {
            let mut m2 = m.clone();
            m2.insert("type".into(), Value::from(event));
            Value::Object(m2)
        }
        other => other.clone(),
    };
    format!("event: {}\ndata: {}\n\n", event, py_json(&payload))
}

struct Base {
    buffer: String,
    usage: Option<Value>,
    done: bool,
}

enum Raw {
    None,
    Chunk(Value),
    Done,
}

impl Base {
    fn new() -> Self {
        Base {
            buffer: String::new(),
            usage: None,
            done: false,
        }
    }

    /// 处理一个 SSE 事件块。
    fn handle_raw(&mut self, raw: &str) -> Raw {
        let mut data = String::new();
        for line in raw.split('\n') {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            return Raw::None;
        }
        if data == "[DONE]" {
            return Raw::Done;
        }
        match serde_json::from_str::<Value>(&data) {
            Ok(v) if v.is_object() => Raw::Chunk(v),
            _ => Raw::None,
        }
    }
}

// ============================================================ Responses

struct RsTool {
    id: String,
    oi: i64,
    call_id: String,
    name: String,
    args: String,
}

pub struct ResponsesStream {
    base: Base,
    model: String,
    resp_id: String,
    rs_id: String,
    msg_id: String,
    created: i64,
    started: bool,
    finish: Option<String>,
    rs_open: bool,
    rs_oi: i64,
    rs_acc: String,
    part_open: bool,
    msg_oi: i64,
    text_acc: String,
    tools: HashMap<i64, RsTool>,
    next_oi: i64,
}

impl ResponsesStream {
    pub fn new(model: &str) -> Self {
        ResponsesStream {
            base: Base::new(),
            model: model.to_string(),
            resp_id: util::rand_id("resp_"),
            rs_id: util::rand_id("rs_"),
            msg_id: util::rand_id("msg_"),
            created: 0,
            started: false,
            finish: None,
            rs_open: false,
            rs_oi: 0,
            rs_acc: String::new(),
            part_open: false,
            msg_oi: 0,
            text_acc: String::new(),
            tools: HashMap::new(),
            next_oi: 0,
        }
    }

    pub fn feed(&mut self, chunk: &str, out: &mut String) {
        self.base.buffer.push_str(&chunk.replace("\r\n", "\n"));
        loop {
            let Some(pos) = self.base.buffer.find("\n\n") else { return };
            let raw: String = self.base.buffer[..pos].to_string();
            self.base.buffer = self.base.buffer[pos + 2..].to_string();
            match self.base.handle_raw(&raw) {
                Raw::Chunk(c) => self.handle_chunk(&c, out),
                Raw::Done => self.finalize(out),
                Raw::None => {}
            }
        }
    }

    fn handle_chunk(&mut self, c: &Value, out: &mut String) {
        if !self.started {
            if self.model.is_empty() {
                self.model = util::str_or(c.get("model"), "");
            }
            self.created = util::int_or(c.get("created").filter(|x| !x.is_null()), 0);
            if self.created == 0 {
                self.created = util::now_i();
            }
            self.start(out);
        }
        if let Some(u) = c.get("usage") {
            if u.is_object() {
                self.base.usage = Some(u.clone());
            }
        }
        if c.get("error").map(|e| util::truthy(e)).unwrap_or(false) {
            self.base.done = true;
            let mut resp = self.skeleton("failed", true);
            let msg = c
                .get("error")
                .and_then(|e| e.as_object())
                .and_then(|e| e.get("message"))
                .map(|m| util::py_str(m))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "upstream error".to_string());
            resp["error"] = json!({"code": "upstream_error", "message": msg});
            out.push_str(&sse_frame("response.failed", &json!({"response": resp})));
            return;
        }
        let choice = c
            .get("choices")
            .and_then(|x| x.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(Value::Null);
        let Some(choice_obj) = choice.as_object() else { return };
        if choice_obj.get("finish_reason").map(util::truthy).unwrap_or(false) {
            self.finish = Some(util::str_or(choice_obj.get("finish_reason"), ""));
        }
        let delta = choice_obj.get("delta").cloned().unwrap_or(Value::Null);
        let Some(d) = delta.as_object() else { return };
        self.reasoning(d, out);
        self.content(d, out);
        self.tools_delta(d, out);
    }

    fn start(&mut self, out: &mut String) {
        self.started = true;
        let resp = self.skeleton("in_progress", false);
        out.push_str(&sse_frame("response.created", &json!({"response": resp})));
        let resp = self.skeleton("in_progress", false);
        out.push_str(&sse_frame("response.in_progress", &json!({"response": resp})));
    }

    fn skeleton(&self, status: &str, with_output: bool) -> Value {
        let incomplete = if status == "incomplete" {
            Some(json!({"reason": if self.finish.as_deref() == Some("content_filter") { "content_filter" } else { "max_output_tokens" }}))
        } else {
            None
        };
        json!({
            "id": self.resp_id,
            "object": "response",
            "created_at": if self.created != 0 { self.created } else { util::now_i() },
            "status": status,
            "error": null,
            "incomplete_details": incomplete,
            "model": self.model,
            "output": if with_output { self.build_output() } else { Vec::<Value>::new() },
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": {"effort": null, "summary": null},
            "temperature": null,
            "top_p": null,
            "max_output_tokens": null,
            "tools": [],
            "tool_choice": "auto",
            "usage": map_usage(self.base.usage.as_ref()),
            "metadata": {},
        })
    }

    fn build_output(&self) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        if !self.rs_acc.is_empty() {
            out.push(json!({
                "id": self.rs_id,
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": self.rs_acc}],
            }));
        }
        if !self.text_acc.is_empty() || self.tools.is_empty() {
            out.push(json!({
                "id": self.msg_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": self.text_acc, "annotations": []}],
            }));
        }
        let mut idxs: Vec<i64> = self.tools.keys().copied().collect();
        idxs.sort();
        for i in idxs {
            let t = &self.tools[&i];
            out.push(self.tool_item(t, "completed"));
        }
        out
    }

    fn tool_item(&self, t: &RsTool, status: &str) -> Value {
        json!({
            "id": t.id,
            "type": "function_call",
            "status": status,
            "call_id": t.call_id,
            "name": t.name,
            "arguments": t.args,
        })
    }

    fn final_status(&self) -> &'static str {
        match self.finish.as_deref() {
            Some("length") | Some("content_filter") => "incomplete",
            _ => "completed",
        }
    }

    fn reasoning(&mut self, delta: &Map<String, Value>, out: &mut String) {
        let rs = delta
            .get("reasoning_content")
            .filter(|x| util::truthy(x))
            .or_else(|| delta.get("reasoning").filter(|x| util::truthy(x)))
            .cloned();
        let Some(rs) = rs else { return };
        let Some(rs) = rs.as_str() else { return };
        if rs.is_empty() {
            return;
        }
        // 退化思考整段丢弃，不向下游转发
        if util::degenerate_reasoning(&Value::from(rs)) {
            return;
        }
        if !self.rs_open {
            self.rs_open = true;
            self.rs_oi = self.next_oi;
            self.next_oi += 1;
            out.push_str(&sse_frame(
                "response.output_item.added",
                &json!({"output_index": self.rs_oi, "item": {"id": self.rs_id, "type": "reasoning", "summary": []}}),
            ));
            out.push_str(&sse_frame(
                "response.reasoning_summary_part.added",
                &json!({
                    "item_id": self.rs_id,
                    "output_index": self.rs_oi,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": ""},
                }),
            ));
        }
        self.rs_acc.push_str(rs);
        out.push_str(&sse_frame(
            "response.reasoning_summary_text.delta",
            &json!({"item_id": self.rs_id, "output_index": self.rs_oi, "summary_index": 0, "delta": rs}),
        ));
    }

    fn close_reasoning(&mut self, out: &mut String) {
        let text = self.rs_acc.clone();
        out.push_str(&sse_frame(
            "response.reasoning_summary_text.done",
            &json!({"item_id": self.rs_id, "output_index": self.rs_oi, "summary_index": 0, "text": text}),
        ));
        out.push_str(&sse_frame(
            "response.output_item.done",
            &json!({
                "output_index": self.rs_oi,
                "item": {
                    "id": self.rs_id,
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": self.rs_acc}],
                },
            }),
        ));
        self.rs_open = false;
    }

    fn content(&mut self, delta: &Map<String, Value>, out: &mut String) {
        let content = delta.get("content");
        let Some(content) = content.and_then(|c| c.as_str()) else { return };
        if content.is_empty() {
            return;
        }
        if self.rs_open {
            self.close_reasoning(out);
        }
        if !self.part_open {
            self.part_open = true;
            self.msg_oi = self.next_oi;
            self.next_oi += 1;
            out.push_str(&sse_frame(
                "response.output_item.added",
                &json!({
                    "output_index": self.msg_oi,
                    "item": {
                        "id": self.msg_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    },
                }),
            ));
            out.push_str(&sse_frame(
                "response.content_part.added",
                &json!({
                    "item_id": self.msg_id,
                    "output_index": self.msg_oi,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            ));
        }
        self.text_acc.push_str(content);
        out.push_str(&sse_frame(
            "response.output_text.delta",
            &json!({"item_id": self.msg_id, "output_index": self.msg_oi, "content_index": 0, "delta": content}),
        ));
    }

    fn close_message(&mut self, out: &mut String) {
        let part = json!({"type": "output_text", "text": self.text_acc, "annotations": []});
        out.push_str(&sse_frame(
            "response.output_text.done",
            &json!({"item_id": self.msg_id, "output_index": self.msg_oi, "content_index": 0, "text": self.text_acc}),
        ));
        out.push_str(&sse_frame(
            "response.content_part.done",
            &json!({"item_id": self.msg_id, "output_index": self.msg_oi, "content_index": 0, "part": part}),
        ));
        out.push_str(&sse_frame(
            "response.output_item.done",
            &json!({
                "output_index": self.msg_oi,
                "item": {
                    "id": self.msg_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [part],
                },
            }),
        ));
        self.part_open = false;
    }

    fn open_empty(&mut self, out: &mut String) {
        self.part_open = true;
        self.msg_oi = self.next_oi;
        self.next_oi += 1;
        out.push_str(&sse_frame(
            "response.output_item.added",
            &json!({
                "output_index": self.msg_oi,
                "item": {
                    "id": self.msg_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ));
        out.push_str(&sse_frame(
            "response.content_part.added",
            &json!({
                "item_id": self.msg_id,
                "output_index": self.msg_oi,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            }),
        ));
    }

    fn tools_delta(&mut self, delta: &Map<String, Value>, out: &mut String) {
        let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) else { return };
        for tc in tcs {
            let Some(tco) = tc.as_object() else { continue };
            let idx = util::int_or(tco.get("index").filter(|x| !x.is_null()), 0);
            if !self.tools.contains_key(&idx) {
                if self.part_open {
                    self.close_message(out);
                }
                // 与 Anthropic 版对齐：先输出 reasoning 块再开 tool 块，
                // 否则 reasoning 的 done 事件会落到 function_call 事件之后
                if self.rs_open {
                    self.close_reasoning(out);
                }
                let call_id = util::str_or(tco.get("id").filter(|x| !x.is_null()), "");
                let call_id = if call_id.is_empty() {
                    util::rand_id("call_")
                } else {
                    call_id
                };
                let tool = RsTool {
                    id: util::rand_id("fc_"),
                    oi: self.next_oi,
                    call_id,
                    name: util::str_or(tco.get("function").and_then(|f| f.as_object()).and_then(|f| f.get("name")).filter(|x| util::truthy(x)), ""),
                    args: String::new(),
                };
                self.next_oi += 1;
                let item = self.tool_item(&tool, "in_progress");
                let oi = tool.oi;
                self.tools.insert(idx, tool);
                out.push_str(&sse_frame(
                    "response.output_item.added",
                    &json!({"output_index": oi, "item": item}),
                ));
            }
            let t = self.tools.get_mut(&idx).unwrap();
            if let Some(id) = tco.get("id").filter(|x| util::truthy(x)) {
                t.call_id = util::py_str(id);
            }
            if let Some(fname) = tco
                .get("function")
                .and_then(|f| f.as_object())
                .and_then(|f| f.get("name"))
                .filter(|x| util::truthy(x))
            {
                t.name.push_str(&util::py_str(fname));
            }
            let frag = util::str_or(
                tco.get("function")
                    .and_then(|f| f.as_object())
                    .and_then(|f| f.get("arguments"))
                    .filter(|x| !x.is_null()),
                "",
            );
            if !frag.is_empty() {
                t.args.push_str(&frag);
                out.push_str(&sse_frame(
                    "response.function_call_arguments.delta",
                    &json!({"item_id": t.id, "output_index": t.oi, "delta": frag}),
                ));
            }
        }
    }

    pub fn finalize(&mut self, out: &mut String) {
        if self.base.done {
            return;
        }
        self.base.done = true;
        if !self.started {
            self.start(out);
        }
        if self.rs_open {
            self.close_reasoning(out);
        }
        if self.part_open {
            self.close_message(out);
        } else if self.tools.is_empty() {
            self.open_empty(out);
            self.close_message(out);
        }
        let mut idxs: Vec<i64> = self.tools.keys().copied().collect();
        idxs.sort();
        for i in idxs {
            let t = &self.tools[&i];
            out.push_str(&sse_frame(
                "response.function_call_arguments.done",
                &json!({"item_id": t.id, "output_index": t.oi, "arguments": t.args}),
            ));
            out.push_str(&sse_frame(
                "response.output_item.done",
                &json!({"output_index": t.oi, "item": self.tool_item(t, "completed")}),
            ));
        }
        let status = self.final_status();
        let resp = self.skeleton(status, true);
        out.push_str(&sse_frame("response.completed", &json!({"response": resp})));
    }

    /// 上游中断/超时：发出 response.failed 事件并终止。
    /// 训练资料取全文用。
    pub fn text_acc(&self) -> String {
        self.text_acc.clone()
    }

    pub fn reasoning_acc(&self) -> String {
        self.rs_acc.clone()
    }

    pub fn fail(&mut self, message: &str, out: &mut String) {
        if self.base.done {
            return;
        }
        self.base.done = true;
        if !self.started {
            self.start(out);
        }
        let mut resp = self.skeleton("failed", true);
        resp["error"] = json!({"code": "upstream_error", "message": message});
        out.push_str(&sse_frame("response.failed", &json!({"response": resp})));
    }
}

// ============================================================ Anthropic

struct AnTool {
    idx: i64,
    id: String,
    name: String,
    args: String,
}

pub struct AnthropicStream {
    base: Base,
    model: String,
    est_input: i64,
    started: bool,
    finish: Option<String>,
    next_index: i64,
    think_idx: Option<i64>,
    text_idx: Option<i64>,
    text_acc: String,
    think_acc: String,
    tools: HashMap<i64, AnTool>,
    msg_id: String,
}

impl AnthropicStream {
    pub fn new(model: &str, est_input: i64) -> Self {
        AnthropicStream {
            base: Base::new(),
            model: model.to_string(),
            est_input,
            started: false,
            finish: None,
            next_index: 0,
            think_idx: None,
            text_idx: None,
            text_acc: String::new(),
            think_acc: String::new(),
            tools: HashMap::new(),
            msg_id: util::rand_id("msg_"),
        }
    }

    pub fn feed(&mut self, chunk: &str, out: &mut String) {
        self.base.buffer.push_str(&chunk.replace("\r\n", "\n"));
        loop {
            let Some(pos) = self.base.buffer.find("\n\n") else { return };
            let raw: String = self.base.buffer[..pos].to_string();
            self.base.buffer = self.base.buffer[pos + 2..].to_string();
            match self.base.handle_raw(&raw) {
                Raw::Chunk(c) => self.handle_chunk(&c, out),
                Raw::Done => self.finalize(out),
                Raw::None => {}
            }
        }
    }

    fn handle_chunk(&mut self, c: &Value, out: &mut String) {
        if !self.started {
            if self.model.is_empty() {
                self.model = util::str_or(c.get("model"), "");
            }
            self.start(out);
        }
        if let Some(u) = c.get("usage") {
            if u.is_object() {
                self.base.usage = Some(u.clone());
            }
        }
        if c.get("error").map(|e| util::truthy(e)).unwrap_or(false) {
            self.base.done = true;
            let msg = c
                .get("error")
                .and_then(|e| e.as_object())
                .and_then(|e| e.get("message"))
                .map(|m| util::py_str(m))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "upstream error".to_string());
            out.push_str(&sse_frame(
                "error",
                &json!({"type": "error", "error": {"type": "api_error", "message": msg}}),
            ));
            return;
        }
        let choice = c
            .get("choices")
            .and_then(|x| x.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(Value::Null);
        let Some(choice_obj) = choice.as_object() else { return };
        if choice_obj.get("finish_reason").map(util::truthy).unwrap_or(false) {
            self.finish = Some(util::str_or(choice_obj.get("finish_reason"), ""));
        }
        let delta = choice_obj.get("delta").cloned().unwrap_or(Value::Null);
        let Some(d) = delta.as_object() else { return };
        self.reasoning(d, out);
        self.content(d, out);
        self.tools_delta(d, out);
    }

    fn start(&mut self, out: &mut String) {
        self.started = true;
        out.push_str(&sse_frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.msg_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": self.est_input, "output_tokens": 0},
                },
            }),
        ));
    }

    fn reasoning(&mut self, delta: &Map<String, Value>, out: &mut String) {
        let rs = delta
            .get("reasoning_content")
            .filter(|x| util::truthy(x))
            .or_else(|| delta.get("reasoning").filter(|x| util::truthy(x)))
            .cloned();
        let Some(rs) = rs.and_then(|r| r.as_str().map(|s| s.to_string())) else {
            return;
        };
        if rs.is_empty() {
            return;
        }
        if util::degenerate_reasoning(&Value::from(rs.clone())) {
            return;
        }
        if let Some(ti) = self.text_idx {
            out.push_str(&sse_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": ti}),
            ));
            self.text_idx = None;
        }
        if self.think_idx.is_none() {
            self.think_idx = Some(self.next_index);
            self.next_index += 1;
            out.push_str(&sse_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.think_idx,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""},
                }),
            ));
        }
        self.think_acc.push_str(&rs);
        out.push_str(&sse_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.think_idx,
                "delta": {"type": "thinking_delta", "thinking": rs},
            }),
        ));
    }

    fn content(&mut self, delta: &Map<String, Value>, out: &mut String) {
        let content = delta.get("content").and_then(|c| c.as_str());
        let Some(content) = content else { return };
        if content.is_empty() {
            return;
        }
        if let Some(ti) = self.think_idx {
            out.push_str(&sse_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": ti}),
            ));
            self.think_idx = None;
        }
        if self.text_idx.is_none() {
            self.text_idx = Some(self.next_index);
            self.next_index += 1;
            out.push_str(&sse_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.text_idx,
                    "content_block": {"type": "text", "text": ""},
                }),
            ));
        }
        self.text_acc.push_str(content);
        out.push_str(&sse_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.text_idx,
                "delta": {"type": "text_delta", "text": content},
            }),
        ));
    }

    fn tools_delta(&mut self, delta: &Map<String, Value>, out: &mut String) {
        let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) else { return };
        for tc in tcs {
            let Some(tco) = tc.as_object() else { continue };
            let idx = util::int_or(tco.get("index").filter(|x| !x.is_null()), 0);
            if !self.tools.contains_key(&idx) {
                if let Some(ti) = self.text_idx {
                    out.push_str(&sse_frame(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": ti}),
                    ));
                    self.text_idx = None;
                }
                if let Some(ti) = self.think_idx {
                    out.push_str(&sse_frame(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": ti}),
                    ));
                    self.think_idx = None;
                }
                let blk = self.next_index;
                self.next_index += 1;
                let id = util::str_or(tco.get("id").filter(|x| !x.is_null()), "");
                let id = if id.is_empty() {
                    util::rand_id("toolu_")
                } else {
                    id
                };
                let name = util::str_or(
                    tco.get("function")
                        .and_then(|f| f.as_object())
                        .and_then(|f| f.get("name"))
                        .filter(|x| util::truthy(x)),
                    "",
                );
                self.tools.insert(idx, AnTool { idx: blk, id, name, args: String::new() });
                let t = &self.tools[&idx];
                out.push_str(&sse_frame(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": blk,
                        "content_block": {"type": "tool_use", "id": t.id, "name": t.name, "input": {}},
                    }),
                ));
            }
            let t = self.tools.get_mut(&idx).unwrap();
            if let Some(id) = tco.get("id").filter(|x| util::truthy(x)) {
                t.id = util::py_str(id);
            }
            if let Some(fname) = tco
                .get("function")
                .and_then(|f| f.as_object())
                .and_then(|f| f.get("name"))
                .filter(|x| util::truthy(x))
            {
                t.name.push_str(&util::py_str(fname));
            }
            let frag = util::str_or(
                tco.get("function")
                    .and_then(|f| f.as_object())
                    .and_then(|f| f.get("arguments"))
                    .filter(|x| !x.is_null()),
                "",
            );
            if !frag.is_empty() {
                t.args.push_str(&frag);
                out.push_str(&sse_frame(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": t.idx,
                        "delta": {"type": "input_json_delta", "partial_json": frag},
                    }),
                ));
            }
        }
    }

    fn stop_reason(&self) -> &'static str {
        match self.finish.as_deref() {
            Some("tool_calls") => "tool_use",
            Some("length") => "max_tokens",
            _ => "end_turn",
        }
    }

    pub fn finalize(&mut self, out: &mut String) {
        if self.base.done {
            return;
        }
        self.base.done = true;
        if !self.started {
            self.start(out);
        }
        if self.text_acc.is_empty() && self.tools.is_empty() && self.think_acc.is_empty() {
            self.text_idx = Some(self.next_index);
            self.next_index += 1;
            out.push_str(&sse_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.text_idx,
                    "content_block": {"type": "text", "text": ""},
                }),
            ));
        }
        let mut idxs: Vec<i64> = Vec::new();
        if let Some(i) = self.think_idx {
            idxs.push(i);
        }
        if let Some(i) = self.text_idx {
            idxs.push(i);
        }
        let mut tool_idxs: Vec<i64> = self.tools.values().map(|t| t.idx).collect();
        idxs.append(&mut tool_idxs);
        for idx in idxs {
            out.push_str(&sse_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}),
            ));
        }
        let output_tokens = self
            .base
            .usage
            .as_ref()
            .and_then(|u| u.get("completion_tokens"))
            .and_then(util::py_int)
            .unwrap_or(0);
        out.push_str(&sse_frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": self.stop_reason(), "stop_sequence": null},
                "usage": {"output_tokens": output_tokens},
            }),
        ));
        out.push_str(&sse_frame("message_stop", &json!({"type": "message_stop"})));
    }

    pub fn text_acc(&self) -> String {
        self.text_acc.clone()
    }

    pub fn reasoning_acc(&self) -> String {
        self.think_acc.clone()
    }

    /// 上游中断/超时：发出 error 事件并终止。
    pub fn fail(&mut self, message: &str, out: &mut String) {
        if self.base.done {
            return;
        }
        self.base.done = true;
        if !self.started {
            self.start(out);
        }
        out.push_str(&sse_frame(
            "error",
            &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
        ));
    }
}
