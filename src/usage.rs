use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Accumulator {
    pub prompt: Option<f64>,
    pub cached: Option<f64>,
    pub completion: Option<f64>,
    pub finish_reason: String,
    pub content_chars: usize,
    pub reasoning_chars: usize,
    pub tool_call_deltas: usize,
    pub generated: bool,
}

impl Accumulator {
    pub fn feed_json(&mut self, data: &[u8]) {
        if let Ok(value) = serde_json::from_slice(data) {
            self.feed_value(&value);
        }
    }

    pub fn feed_sse_line(&mut self, line: &[u8]) {
        let line = trim_ascii(line);
        let Some(payload) = line.strip_prefix(b"data:") else {
            return;
        };
        let payload = trim_ascii(payload);
        if !payload.is_empty() && payload != b"[DONE]" {
            self.feed_json(payload);
        }
    }

    pub fn feed_value(&mut self, object: &Value) {
        if let Some(choices) = object.get("choices").and_then(Value::as_array) {
            if let Some(reason) = choices
                .first()
                .and_then(|choice| choice.get("finish_reason"))
                .and_then(Value::as_str)
            {
                reason.clone_into(&mut self.finish_reason);
            }
            for choice in choices {
                let node = choice
                    .get("delta")
                    .filter(|value| value.as_object().is_some_and(|map| !map.is_empty()))
                    .or_else(|| choice.get("message"));
                let Some(node) = node else { continue };
                if let Some(content) = node.get("content").and_then(Value::as_str) {
                    self.content_chars += content.chars().count();
                    self.generated |= !content.is_empty();
                }
                for key in ["reasoning_content", "reasoning"] {
                    if let Some(reasoning) = node.get(key).and_then(Value::as_str) {
                        self.reasoning_chars += reasoning.chars().count();
                        self.generated |= !reasoning.is_empty();
                    }
                }
                if node.get("tool_calls").is_some_and(truthy) {
                    self.tool_call_deltas += 1;
                    self.generated = true;
                }
            }
        }
        if let Some(reason) = object.get("stop_reason").filter(|value| !value.is_null()) {
            self.finish_reason = reason
                .as_str()
                .map_or_else(|| reason.to_string(), ToOwned::to_owned);
        }
        if let Some(status @ ("completed" | "incomplete" | "cancelled")) =
            object.get("status").and_then(Value::as_str)
        {
            status.clone_into(&mut self.finish_reason);
        }
        match object.get("delta") {
            Some(Value::Object(delta)) => {
                self.generated |= ["text", "thinking", "reasoning", "partial_json"]
                    .iter()
                    .any(|key| {
                        delta
                            .get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                    });
            }
            Some(Value::String(delta)) => self.generated |= !delta.is_empty(),
            _ => {}
        }

        let usage = object
            .get("message")
            .and_then(|message| message.get("usage"))
            .filter(|usage| usage.as_object().is_some_and(|map| !map.is_empty()))
            .or_else(|| object.get("usage"));
        let Some(usage) = usage else { return };
        if let Some(value) = number(usage.get("prompt_tokens")) {
            self.prompt = Some(value);
            self.cached = usage
                .get("prompt_tokens_details")
                .and_then(|details| number(details.get("cached_tokens")))
                .or_else(|| number(usage.get("cached_tokens")));
        }
        if let Some(value) = number(usage.get("completion_tokens")) {
            self.completion = Some(value);
        }
        if let Some(value) = number(usage.get("input_tokens")) {
            self.prompt = Some(value);
        }
        if let Some(value) = number(usage.get("cache_read_input_tokens")) {
            self.cached = Some(value);
        }
        if let Some(value) = number(usage.get("output_tokens")) {
            self.completion = Some(value);
        }
    }
}

pub fn feed_sse_chunk(accumulator: &mut Accumulator, tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    let mut consumed = 0;
    while let Some(relative) = tail[consumed..].iter().position(|byte| *byte == b'\n') {
        let end = consumed + relative;
        accumulator.feed_sse_line(&tail[consumed..end]);
        consumed = end + 1;
    }
    if consumed > 0 {
        tail.drain(..consumed);
    }
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_openai_usage_and_true_output() {
        let mut usage = Accumulator::default();
        usage.feed_json(br#"{"choices":[{"message":{"content":"hi","reasoning_content":"thinking"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":90}}}"#);
        assert_eq!(usage.prompt, Some(100.0));
        assert_eq!(usage.cached, Some(90.0));
        assert_eq!(usage.completion, Some(5.0));
        assert_eq!(usage.content_chars, 2);
        assert_eq!(usage.reasoning_chars, 8);
        assert!(usage.generated);
    }

    #[test]
    fn reassembles_split_sse_lines() {
        let mut usage = Accumulator::default();
        let mut tail = Vec::new();
        feed_sse_chunk(
            &mut usage,
            &mut tail,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\ndata: {\"usage\":{\"prompt_t",
        );
        feed_sse_chunk(
            &mut usage,
            &mut tail,
            b"okens\":10,\"completion_tokens\":3}}\n",
        );
        assert!(tail.is_empty());
        assert_eq!(usage.prompt, Some(10.0));
        assert_eq!(usage.completion, Some(3.0));
    }

    #[test]
    fn ignores_role_only_but_detects_tool_delta() {
        let mut usage = Accumulator::default();
        usage.feed_json(br#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#);
        assert!(!usage.generated);
        usage.feed_json(br#"{"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}"#);
        assert!(usage.generated);
    }
}
