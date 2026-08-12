use serde_json::Value;

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
}

impl PreparedRequest {
    #[must_use]
    pub fn new(endpoint: Endpoint, raw: &[u8], threshold: i64, router: &Router) -> Self {
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
        Self { body, fingerprints }
    }

    #[must_use]
    pub fn route(&self, router: &Router) -> Decision {
        router.route_prepared(self.body.len(), &self.fingerprints)
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
}
