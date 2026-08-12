use bytes::Bytes;
use serde_json::{Map, Value};

use crate::{
    router::{Decision, Router},
    shims::{Endpoint, sanitize_object},
};

/// The immutable result of the request preparation boundary.
///
/// JSON is parsed at most once. The same post-shim object produces both the
/// upstream wire body and the approximate route fingerprints.
#[derive(Debug)]
pub struct PreparedRequest {
    pub body: Vec<u8>,
    pub fingerprints: Vec<u64>,
    pub tokenizer_body: Option<Bytes>,
}

impl PreparedRequest {
    #[must_use]
    pub fn new(endpoint: Endpoint, raw: &[u8], threshold: i64, router: &Router) -> Self {
        Self::with_tokenizer(endpoint, raw, threshold, router, false)
    }

    #[must_use]
    pub fn with_tokenizer(
        endpoint: Endpoint,
        raw: &[u8],
        threshold: i64,
        router: &Router,
        prepare_tokenizer_body: bool,
    ) -> Self {
        let parsed = serde_json::from_slice::<Value>(raw).ok();
        let (body, object) = match parsed {
            Some(Value::Object(mut object)) => {
                let changed = sanitize_object(endpoint, &mut object, threshold);
                let body = if changed {
                    serde_json::to_vec(&object).unwrap_or_else(|_| raw.to_vec())
                } else {
                    raw.to_vec()
                };
                (body, Some(object))
            }
            _ => (raw.to_vec(), None),
        };
        let fingerprints = router.fingerprints_preparsed(&body, object.as_ref());
        let tokenizer_body = prepare_tokenizer_body
            .then(|| {
                object
                    .as_ref()
                    .and_then(|object| tokenization_body(endpoint, object))
            })
            .flatten()
            .map(Bytes::from);
        Self {
            body,
            fingerprints,
            tokenizer_body,
        }
    }

    #[must_use]
    pub fn route(&self, router: &Router) -> Decision {
        router.route_prepared(self.body.len(), &self.fingerprints)
    }
}

fn tokenization_body(endpoint: Endpoint, object: &Map<String, Value>) -> Option<Vec<u8>> {
    match endpoint {
        Endpoint::Chat if object.get("messages").is_some_and(Value::is_array) => {
            let mut request = object.clone();
            let continue_final = request
                .get("continue_final_message")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            request
                .entry("add_generation_prompt")
                .or_insert(Value::Bool(!continue_final));
            request.insert("return_token_strs".to_owned(), Value::Bool(false));
            let mut template_kwargs = request
                .get("chat_template_kwargs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for key in ["documents", "reasoning_effort"] {
                if let Some(value) = request
                    .get(key)
                    .filter(|value| !value.is_null() && value.as_str() != Some("auto"))
                {
                    template_kwargs.insert(key.to_owned(), value.clone());
                }
            }
            if let Some(effort) = request.get("reasoning_effort").and_then(Value::as_str)
                && !template_kwargs.contains_key("enable_thinking")
            {
                template_kwargs.insert("enable_thinking".to_owned(), Value::Bool(effort != "none"));
            }
            if !template_kwargs.is_empty() {
                request.insert(
                    "chat_template_kwargs".to_owned(),
                    Value::Object(template_kwargs),
                );
            }
            serde_json::to_vec(&request).ok()
        }
        Endpoint::Completions if object.get("prompt").is_some_and(Value::is_string) => {
            let mut request = object.clone();
            request.insert("return_token_strs".to_owned(), Value::Bool(false));
            serde_json::to_vec(&request).ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::{config::Affinity, router::RouterConfig, shims::sanitize_request};

    fn router() -> Router {
        Router::new(RouterConfig {
            upstreams: vec![Url::parse("http://engine:8000").unwrap()],
            alpha: 4.0,
            chunk_bytes: 64,
            max_prefix_bytes: 2 << 20,
            max_overlap_blocks: 32,
            index_capacity: 100_000,
            load_unit_bytes: 32 << 10,
            max_load_units: 8,
            affinity: Affinity::Prefix,
        })
    }

    #[test]
    fn matches_sequential_sanitize_and_fingerprint_contract() {
        let router = router();
        let cases: &[(Endpoint, &[u8])] = &[
            (
                Endpoint::Chat,
                br#"{"messages":[{"role":"assistant","content":[{"text":"hello"}]}],"max_tokens":100000,"reasoning_effort":"none"}"#,
            ),
            (
                Endpoint::Messages,
                br#"{"system":"system","messages":[{"role":"user","content":"hello"}]}"#,
            ),
            (Endpoint::Chat, br#"{"messages":"malformed"}"#),
            (Endpoint::Other, b"not-json"),
        ];
        for (endpoint, raw) in cases {
            let expected_body = sanitize_request(*endpoint, raw, 100_000);
            let expected_fingerprints = router.fingerprints(&expected_body);
            let prepared = PreparedRequest::new(*endpoint, raw, 100_000, &router);
            assert_eq!(prepared.body, expected_body);
            assert_eq!(prepared.fingerprints, expected_fingerprints);
            assert_eq!(
                prepared.route(&router).total_blocks,
                expected_fingerprints.len()
            );
        }
    }

    #[test]
    fn unchanged_wire_body_is_preserved_byte_for_byte() {
        let router = router();
        let raw = br#"{ "messages": [ { "role": "user", "content": "hello" } ] }"#;
        assert_eq!(
            PreparedRequest::new(Endpoint::Chat, raw, 100_000, &router).body,
            raw
        );
    }

    #[test]
    fn derives_tokenizer_payload_from_the_sanitized_parse() {
        let router = router();
        let raw = br#"{"model":"model","messages":[{"role":"user","content":"hello"}],"max_tokens":100000,"continue_final_message":true,"reasoning_effort":"none","chat_template_kwargs":{"custom":7}}"#;
        let prepared = PreparedRequest::with_tokenizer(Endpoint::Chat, raw, 100_000, &router, true);
        let body: Value =
            serde_json::from_slice(prepared.tokenizer_body.as_ref().unwrap()).unwrap();
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["add_generation_prompt"], false);
        assert_eq!(body["return_token_strs"], false);
        assert_eq!(body["messages"][0]["content"], "hello");
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], "none");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["chat_template_kwargs"]["custom"], 7);
    }

    #[test]
    fn tokenizer_payload_is_selective_by_endpoint_and_flag() {
        let router = router();
        let chat = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
        assert!(
            PreparedRequest::new(Endpoint::Chat, chat, 100_000, &router)
                .tokenizer_body
                .is_none()
        );
        assert!(
            PreparedRequest::with_tokenizer(Endpoint::Messages, chat, 100_000, &router, true)
                .tokenizer_body
                .is_none()
        );
    }
}
