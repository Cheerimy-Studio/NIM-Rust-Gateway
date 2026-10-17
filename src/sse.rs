//! Incremental SSE framing shared by the proxy and protocol converters.

const MAX_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    after_cr: bool,
    started: bool,
}

impl Decoder {
    /// Normalize line endings only after framing bytes, so CRLF and UTF-8 may
    /// cross arbitrary transport boundaries. Never dispatch an incomplete event.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            self.buffer.push(if self.after_cr { b'\n' } else { byte });
            if self.buffer.len() > MAX_EVENT_BYTES {
                return Err("上游 SSE 单个事件超过 1 MiB".into());
            }
            if self.buffer.ends_with(b"\n\n") {
                let bytes = std::mem::take(&mut self.buffer);
                let mut event = String::from_utf8(bytes)
                    .map_err(|_| "上游 SSE 包含无效 UTF-8".to_string())?;
                if !self.started {
                    event = event.trim_start_matches('\u{feff}').to_string();
                    self.started = true;
                }
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn has_pending(&self) -> bool {
        self.buffer.iter().any(|b| !b.is_ascii_whitespace())
    }
}

pub fn data(event: &str) -> Option<String> {
    let lines: Vec<&str> = event.lines().filter_map(|line| {
        if line == "data" {
            Some("")
        } else {
            line.strip_prefix("data:").map(|v| v.strip_prefix(' ').unwrap_or(v))
        }
    }).collect();
    if lines.is_empty() { None } else { Some(lines.join("\n")) }
}

/// Replace a parsed JSON payload while retaining SSE event/id/retry metadata.
pub fn replace_data(event: &str, payload: &str) -> String {
    let mut output = String::new();
    let mut replaced = false;
    for line in event.split_inclusive('\n') {
        if line.starts_with("data:") || line == "data\n" {
            if !replaced {
                output.push_str("data: ");
                output.push_str(payload);
                output.push('\n');
                replaced = true;
            }
        } else {
            output.push_str(line);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_transport_boundary_preserves_unicode_and_crlf() {
        let input = "\u{feff}data: {\"text\":\"你好\"}\r\n\r\ndata: [DONE]\r\n\r\n";
        for split in 0..=input.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.feed(&input.as_bytes()[..split]).unwrap();
            events.extend(decoder.feed(&input.as_bytes()[split..]).unwrap());
            assert_eq!(events, ["data: {\"text\":\"你好\"}\n\n", "data: [DONE]\n\n"]);
            assert!(!decoder.has_pending());
        }
    }

    #[test]
    fn multiline_data_and_comments() {
        let mut decoder = Decoder::default();
        let events = decoder.feed(b": comment\r\rdata: first\rdata:  second\rdata\r\r").unwrap();
        assert_eq!(data(&events[0]), None);
        assert_eq!(data(&events[1]).as_deref(), Some("first\n second\n"));
    }

    #[test]
    fn partial_event_is_not_dispatched_and_memory_is_bounded() {
        let mut decoder = Decoder::default();
        assert!(decoder.feed(b"data: {\"text\":").unwrap().is_empty());
        assert!(decoder.has_pending());
        assert!(decoder.feed(&vec![b'x'; MAX_EVENT_BYTES]).is_err());
    }

    #[test]
    fn rewriting_json_preserves_event_metadata_and_nested_content() {
        let event = "id: 42\nevent: message\nretry: 1000\ndata: {\"model\":\"private\",\ndata: \"nested\":{\"model\":\"keep\"}}\n\n";
        let mut value: serde_json::Value = serde_json::from_str(&data(event).unwrap()).unwrap();
        value["model"] = serde_json::json!("public\"$1\\alias");
        let rewritten = replace_data(event, &value.to_string());
        assert!(rewritten.starts_with("id: 42\nevent: message\nretry: 1000\n"));
        let parsed: serde_json::Value = serde_json::from_str(&data(&rewritten).unwrap()).unwrap();
        assert_eq!(parsed["model"], "public\"$1\\alias");
        assert_eq!(parsed["nested"]["model"], "keep");
    }
}
