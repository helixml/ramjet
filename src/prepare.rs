use bytes::Bytes;
use serde::Serialize;
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
    pub output_limit: OutputLimitObservation,
}

const OUTPUT_LIMIT_POLICY_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum OutputLimitBucket {
    #[serde(rename = "unset")]
    Unset,
    #[serde(rename = "invalid")]
    Invalid,
    #[serde(rename = "1_64")]
    OneTo64,
    #[serde(rename = "65_256")]
    SixtyFiveTo256,
    #[serde(rename = "257_1024")]
    TwoFiftySevenTo1024,
    #[serde(rename = "1025_4096")]
    OneThousandTwentyFiveTo4096,
    #[serde(rename = "4097_plus")]
    FourThousandNinetySevenPlus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputLimitSource {
    None,
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputLimitMutation {
    Unchanged,
    MaxTokensStripped,
    MaxCompletionTokensStripped,
    BothStripped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    Unset,
    NonStreaming,
    Streaming,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OutputLimitObservation {
    policy_version: u8,
    requested_bucket: OutputLimitBucket,
    requested_source: OutputLimitSource,
    effective_bucket: OutputLimitBucket,
    effective_source: OutputLimitSource,
    mutation: OutputLimitMutation,
    stream_mode: StreamMode,
}

impl OutputLimitObservation {
    pub(crate) fn unparseable() -> Self {
        Self {
            policy_version: OUTPUT_LIMIT_POLICY_VERSION,
            requested_bucket: OutputLimitBucket::Invalid,
            requested_source: OutputLimitSource::None,
            effective_bucket: OutputLimitBucket::Invalid,
            effective_source: OutputLimitSource::None,
            mutation: OutputLimitMutation::Unchanged,
            stream_mode: StreamMode::Invalid,
        }
    }

    fn from_object(
        endpoint: Endpoint,
        requested: (OutputLimitSource, OutputLimitBucket),
        object: &Map<String, Value>,
        had_max_tokens: bool,
        had_max_completion_tokens: bool,
        stream_mode: StreamMode,
    ) -> Self {
        let effective = output_limit(endpoint, object);
        let stripped_max_tokens = had_max_tokens && !object.contains_key("max_tokens");
        let stripped_max_completion_tokens =
            had_max_completion_tokens && !object.contains_key("max_completion_tokens");
        let mutation = match (stripped_max_tokens, stripped_max_completion_tokens) {
            (false, false) => OutputLimitMutation::Unchanged,
            (true, false) => OutputLimitMutation::MaxTokensStripped,
            (false, true) => OutputLimitMutation::MaxCompletionTokensStripped,
            (true, true) => OutputLimitMutation::BothStripped,
        };
        Self {
            policy_version: OUTPUT_LIMIT_POLICY_VERSION,
            requested_source: requested.0,
            requested_bucket: requested.1,
            effective_source: effective.0,
            effective_bucket: effective.1,
            mutation,
            stream_mode,
        }
    }
}

fn output_limit(
    endpoint: Endpoint,
    object: &Map<String, Value>,
) -> (OutputLimitSource, OutputLimitBucket) {
    let present = |field: &str| object.get(field).is_some_and(|value| !value.is_null());
    let (source, field) = match endpoint {
        Endpoint::Chat if present("max_completion_tokens") => (
            OutputLimitSource::MaxCompletionTokens,
            "max_completion_tokens",
        ),
        Endpoint::Chat | Endpoint::Completions if present("max_tokens") => {
            (OutputLimitSource::MaxTokens, "max_tokens")
        }
        Endpoint::Messages if object.contains_key("max_tokens") => {
            (OutputLimitSource::MaxTokens, "max_tokens")
        }
        Endpoint::Responses if present("max_output_tokens") => {
            (OutputLimitSource::MaxOutputTokens, "max_output_tokens")
        }
        _ => return (OutputLimitSource::None, OutputLimitBucket::Unset),
    };
    let bucket = object
        .get(field)
        .and_then(Value::as_u64)
        .map_or(OutputLimitBucket::Invalid, output_limit_bucket);
    (source, bucket)
}

fn output_limit_bucket(value: u64) -> OutputLimitBucket {
    match value {
        1..=64 => OutputLimitBucket::OneTo64,
        65..=256 => OutputLimitBucket::SixtyFiveTo256,
        257..=1024 => OutputLimitBucket::TwoFiftySevenTo1024,
        1025..=4096 => OutputLimitBucket::OneThousandTwentyFiveTo4096,
        4097.. => OutputLimitBucket::FourThousandNinetySevenPlus,
        0 => OutputLimitBucket::Invalid,
    }
}

fn stream_mode(object: &Map<String, Value>) -> StreamMode {
    match object.get("stream") {
        None => StreamMode::Unset,
        Some(Value::Bool(false) | Value::Null) => StreamMode::NonStreaming,
        Some(Value::Bool(true)) => StreamMode::Streaming,
        Some(_) => StreamMode::Invalid,
    }
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
        let (body, object, output_limit) = match parsed {
            Some(Value::Object(mut object)) => {
                let requested = output_limit(endpoint, &object);
                let had_max_tokens = object.contains_key("max_tokens");
                let had_max_completion_tokens = object.contains_key("max_completion_tokens");
                let stream_mode = stream_mode(&object);
                let changed = sanitize_object(endpoint, &mut object, threshold);
                let output_limit = OutputLimitObservation::from_object(
                    endpoint,
                    requested,
                    &object,
                    had_max_tokens,
                    had_max_completion_tokens,
                    stream_mode,
                );
                let body = if changed {
                    serde_json::to_vec(&object).unwrap_or_else(|_| raw.to_vec())
                } else {
                    raw.to_vec()
                };
                (body, Some(object), output_limit)
            }
            _ => (raw.to_vec(), None, OutputLimitObservation::unparseable()),
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
            output_limit,
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
            projected_load: false,
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

    #[test]
    fn output_limit_buckets_have_exact_closed_boundaries() {
        let cases = [
            (0, OutputLimitBucket::Invalid),
            (1, OutputLimitBucket::OneTo64),
            (64, OutputLimitBucket::OneTo64),
            (65, OutputLimitBucket::SixtyFiveTo256),
            (256, OutputLimitBucket::SixtyFiveTo256),
            (257, OutputLimitBucket::TwoFiftySevenTo1024),
            (1024, OutputLimitBucket::TwoFiftySevenTo1024),
            (1025, OutputLimitBucket::OneThousandTwentyFiveTo4096),
            (4096, OutputLimitBucket::OneThousandTwentyFiveTo4096),
            (4097, OutputLimitBucket::FourThousandNinetySevenPlus),
            (u64::MAX, OutputLimitBucket::FourThousandNinetySevenPlus),
        ];
        for (value, expected) in cases {
            assert_eq!(output_limit_bucket(value), expected);
        }
    }

    #[test]
    fn output_limit_precedence_records_requested_effective_and_strip_action() {
        let prepared = PreparedRequest::new(
            Endpoint::Chat,
            br#"{"messages":[],"max_tokens":64,"max_completion_tokens":100000,"stream":true}"#,
            100_000,
            &router(),
        );
        assert_eq!(
            prepared.output_limit,
            OutputLimitObservation {
                policy_version: 1,
                requested_bucket: OutputLimitBucket::FourThousandNinetySevenPlus,
                requested_source: OutputLimitSource::MaxCompletionTokens,
                effective_bucket: OutputLimitBucket::OneTo64,
                effective_source: OutputLimitSource::MaxTokens,
                mutation: OutputLimitMutation::MaxCompletionTokensStripped,
                stream_mode: StreamMode::Streaming,
            }
        );
        let wire: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(wire["max_tokens"], 64);
        assert!(wire.get("max_completion_tokens").is_none());

        let both = PreparedRequest::new(
            Endpoint::Chat,
            br#"{"messages":[],"max_tokens":100001,"max_completion_tokens":100000}"#,
            100_000,
            &router(),
        );
        assert_eq!(both.output_limit.effective_bucket, OutputLimitBucket::Unset);
        assert_eq!(both.output_limit.effective_source, OutputLimitSource::None);
        assert_eq!(
            both.output_limit.mutation,
            OutputLimitMutation::BothStripped
        );

        let null_preferred = PreparedRequest::new(
            Endpoint::Chat,
            br#"{"messages":[],"max_completion_tokens":null,"max_tokens":64,"stream":null}"#,
            100_000,
            &router(),
        );
        assert_eq!(
            null_preferred.output_limit.requested_source,
            OutputLimitSource::MaxTokens
        );
        assert_eq!(
            null_preferred.output_limit.requested_bucket,
            OutputLimitBucket::OneTo64
        );
        assert_eq!(
            null_preferred.output_limit.stream_mode,
            StreamMode::NonStreaming
        );

        let both_null = PreparedRequest::new(
            Endpoint::Chat,
            br#"{"messages":[],"max_completion_tokens":null,"max_tokens":null}"#,
            100_000,
            &router(),
        );
        assert_eq!(
            both_null.output_limit.requested_source,
            OutputLimitSource::None
        );
        assert_eq!(
            both_null.output_limit.requested_bucket,
            OutputLimitBucket::Unset
        );

        let invalid_preferred = PreparedRequest::new(
            Endpoint::Chat,
            br#"{"messages":[],"max_completion_tokens":"invalid","max_tokens":64}"#,
            100_000,
            &router(),
        );
        assert_eq!(
            invalid_preferred.output_limit.requested_source,
            OutputLimitSource::MaxCompletionTokens
        );
        assert_eq!(
            invalid_preferred.output_limit.requested_bucket,
            OutputLimitBucket::Invalid
        );
    }

    #[test]
    fn output_limit_fields_are_endpoint_specific_and_never_mutate_other_apis() {
        let cases = [
            (
                Endpoint::Messages,
                br#"{"max_tokens":1025,"stream":false}"#.as_slice(),
                OutputLimitSource::MaxTokens,
                OutputLimitBucket::OneThousandTwentyFiveTo4096,
                StreamMode::NonStreaming,
            ),
            (
                Endpoint::Responses,
                br#"{"max_output_tokens":257}"#.as_slice(),
                OutputLimitSource::MaxOutputTokens,
                OutputLimitBucket::TwoFiftySevenTo1024,
                StreamMode::Unset,
            ),
            (
                Endpoint::Responses,
                br#"{"max_output_tokens":null,"stream":null}"#.as_slice(),
                OutputLimitSource::None,
                OutputLimitBucket::Unset,
                StreamMode::NonStreaming,
            ),
            (
                Endpoint::Messages,
                br#"{"max_tokens":null,"stream":null}"#.as_slice(),
                OutputLimitSource::MaxTokens,
                OutputLimitBucket::Invalid,
                StreamMode::NonStreaming,
            ),
            (
                Endpoint::Completions,
                br#"{"prompt":"x","max_tokens":65}"#.as_slice(),
                OutputLimitSource::MaxTokens,
                OutputLimitBucket::SixtyFiveTo256,
                StreamMode::Unset,
            ),
            (
                Endpoint::Completions,
                br#"{"prompt":"x","max_tokens":65,"stream":null}"#.as_slice(),
                OutputLimitSource::MaxTokens,
                OutputLimitBucket::SixtyFiveTo256,
                StreamMode::NonStreaming,
            ),
            (
                Endpoint::Completions,
                br#"{"prompt":"x","max_tokens":null}"#.as_slice(),
                OutputLimitSource::None,
                OutputLimitBucket::Unset,
                StreamMode::Unset,
            ),
        ];
        for (endpoint, raw, source, bucket, expected_stream) in cases {
            let prepared = PreparedRequest::new(endpoint, raw, 100_000, &router());
            assert_eq!(prepared.body, raw);
            assert_eq!(prepared.output_limit.requested_source, source);
            assert_eq!(prepared.output_limit.requested_bucket, bucket);
            assert_eq!(prepared.output_limit.effective_source, source);
            assert_eq!(prepared.output_limit.effective_bucket, bucket);
            assert_eq!(
                prepared.output_limit.mutation,
                OutputLimitMutation::Unchanged
            );
            assert_eq!(prepared.output_limit.stream_mode, expected_stream);
        }
    }

    #[test]
    fn malformed_limits_and_json_are_bounded_invalid_observations() {
        for raw in [
            br#"{"messages":[],"max_tokens":0}"#.as_slice(),
            br#"{"messages":[],"max_tokens":true}"#.as_slice(),
            br#"{"messages":[],"max_tokens":-1}"#.as_slice(),
        ] {
            let prepared = PreparedRequest::new(Endpoint::Chat, raw, 100_000, &router());
            assert_eq!(
                prepared.output_limit.requested_source,
                OutputLimitSource::MaxTokens
            );
            assert_eq!(
                prepared.output_limit.requested_bucket,
                OutputLimitBucket::Invalid
            );
            assert_eq!(
                prepared.output_limit.effective_bucket,
                OutputLimitBucket::Invalid
            );
        }

        let invalid = PreparedRequest::new(Endpoint::Responses, b"not-json", 100_000, &router());
        assert_eq!(invalid.output_limit, OutputLimitObservation::unparseable());
    }
}
