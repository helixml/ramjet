use serde_json::{Map, Number, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    Chat,
    Messages,
    Responses,
    Completions,
    Other,
}

impl Endpoint {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Messages => "messages",
            Self::Responses => "responses",
            Self::Completions => "completions",
            Self::Other => "other",
        }
    }
}

#[must_use]
pub fn endpoint(path: &str) -> Endpoint {
    if path.starts_with("/v1/chat/completions") {
        Endpoint::Chat
    } else if path.starts_with("/v1/messages") {
        Endpoint::Messages
    } else if path.starts_with("/v1/responses") {
        Endpoint::Responses
    } else if path.starts_with("/v1/completions") {
        Endpoint::Completions
    } else {
        Endpoint::Other
    }
}

#[must_use]
pub fn finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" | "end_turn" | "completed" => "stop",
        "length" | "max_tokens" | "incomplete" => "length",
        "tool_calls" | "tool_use" => "tool_calls",
        "cancelled" | "canceled" => "cancelled",
        _ => "other",
    }
}

#[must_use]
pub fn sanitize_request(endpoint: Endpoint, body: &[u8], threshold: i64) -> Vec<u8> {
    if !matches!(endpoint, Endpoint::Chat | Endpoint::Completions) || body.is_empty() {
        return body.to_vec();
    }
    let Ok(Value::Object(mut object)) = serde_json::from_slice(body) else {
        return body.to_vec();
    };
    let changed = sanitize_object(endpoint, &mut object, threshold);
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&object).unwrap_or_else(|_| body.to_vec())
}

pub(crate) fn sanitize_object(
    endpoint: Endpoint,
    object: &mut Map<String, Value>,
    threshold: i64,
) -> bool {
    if !matches!(endpoint, Endpoint::Chat | Endpoint::Completions) {
        return false;
    }
    let mut changed = false;
    for field in ["max_tokens", "max_completion_tokens"] {
        let oversized = object
            .get(field)
            .and_then(Value::as_i64)
            .is_some_and(|value| value >= threshold);
        if oversized {
            object.remove(field);
            changed = true;
        }
    }
    changed |= flatten_content_parts(object);
    let valid_effort = object.get("reasoning_effort").is_none_or(|effort| {
        matches!(
            effort.as_str(),
            Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
        )
    });
    if !valid_effort {
        object.remove("reasoning_effort");
        changed = true;
    }
    changed
}

fn flatten_content_parts(object: &mut Map<String, Value>) -> bool {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for message in messages {
        let Some(message) = message.as_object_mut() else {
            continue;
        };
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        let mut text = String::new();
        for part in parts {
            if let Some(value) = part.as_str() {
                text.push_str(value);
            } else if let Some(value) = part.get("text").and_then(Value::as_str) {
                text.push_str(value);
            }
        }
        message.insert("content".to_owned(), Value::String(text));
        changed = true;
    }
    changed
}

pub fn shrink_advertised_context(body: &[u8], margin: i64) -> Vec<u8> {
    if margin <= 0 {
        return body.to_vec();
    }
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(data) = root.get_mut("data").and_then(Value::as_array_mut) else {
        return body.to_vec();
    };
    let mut changed = false;
    for value in data {
        changed = shrink_map(value, margin) || changed;
    }
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&root).unwrap_or_else(|_| body.to_vec())
}

fn shrink_map(value: &mut Value, margin: i64) -> bool {
    let Value::Object(object) = value else {
        return false;
    };
    let mut changed = false;
    for key in ["max_model_len", "context_length", "max_context_length"] {
        if let Some(Value::Number(number)) = object.get_mut(key)
            && let Some(current) = number.as_i64().filter(|current| *current > margin)
        {
            *number = Number::from(current - margin);
            changed = true;
        }
    }
    for child in object.values_mut() {
        changed |= shrink_map(child, margin);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_go_compatibility_fields() {
        let body = br#"{"messages":[{"role":"assistant","content":[{"text":"hello"},{"type":"text"}]}],"max_tokens":100000,"reasoning_effort":"none"}"#;
        let value: Value =
            serde_json::from_slice(&sanitize_request(Endpoint::Chat, body, 100_000)).unwrap();
        assert!(value.get("max_tokens").is_none());
        assert_eq!(value["reasoning_effort"], "none");
        assert_eq!(value["messages"][0]["content"], "hello");
    }

    #[test]
    fn drops_only_unsupported_reasoning_effort() {
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let body = format!(r#"{{"messages":[],"reasoning_effort":"{effort}"}}"#);
            let value: Value =
                serde_json::from_slice(&sanitize_request(Endpoint::Chat, body.as_bytes(), 100_000))
                    .unwrap();
            assert_eq!(value["reasoning_effort"], effort);
        }

        let body = br#"{"messages":[],"reasoning_effort":"invalid"}"#;
        let value: Value =
            serde_json::from_slice(&sanitize_request(Endpoint::Chat, body, 100_000)).unwrap();
        assert!(value.get("reasoning_effort").is_none());
    }

    #[test]
    fn preserves_bounded_caller_policy_fields() {
        let body = br#"{"messages":[],"reasoning_effort":"high","max_tokens":256,"max_completion_tokens":512}"#;
        let value: Value =
            serde_json::from_slice(&sanitize_request(Endpoint::Chat, body, 100_000)).unwrap();
        assert_eq!(value["reasoning_effort"], "high");
        assert_eq!(value["max_tokens"], 256);
        assert_eq!(value["max_completion_tokens"], 512);
    }

    #[test]
    fn rewrites_nested_context_fields() {
        let body = br#"{"data":[{"max_model_len":393216,"nested":{"context_length":393216}}]}"#;
        let value: Value =
            serde_json::from_slice(&shrink_advertised_context(body, 131_072)).unwrap();
        assert_eq!(value["data"][0]["max_model_len"], 262_144);
        assert_eq!(value["data"][0]["nested"]["context_length"], 262_144);
    }
}
