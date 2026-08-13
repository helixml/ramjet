use std::{cmp::Ordering, collections::HashSet, num::NonZeroUsize, sync::Arc};

use lru::LruCache;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Map, Value};
use url::Url;

use crate::config::Affinity;

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub upstreams: Vec<Url>,
    pub alpha: f64,
    pub chunk_bytes: usize,
    pub max_prefix_bytes: usize,
    pub max_overlap_blocks: usize,
    pub index_capacity: usize,
    pub load_unit_bytes: usize,
    pub max_load_units: usize,
    pub affinity: Affinity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub candidates: Vec<usize>,
    pub candidate_state: Vec<CandidateState>,
    pub overlap_blocks: usize,
    pub total_blocks: usize,
    pub affinity_blocks: usize,
    pub load_units: usize,
    pub rotation: usize,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Overlap,
    Load,
    RoundRobin,
    Single,
    Exact,
}

impl Outcome {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Overlap => "overlap",
            Self::Load => "load",
            Self::RoundRobin => "rr",
            Self::Single => "single",
            Self::Exact => "exact",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CandidateState {
    #[serde(rename = "upstream")]
    pub index: usize,
    pub rank: usize,
    pub overlap_blocks: usize,
    pub affinity_blocks: usize,
    pub load_units: usize,
    pub request_load_units: usize,
    pub healthy: bool,
}

struct UpstreamState {
    index: HashSet<u64>,
    lru: LruCache<u64, ()>,
    inflight: usize,
    load: usize,
    healthy: bool,
}

struct Inner {
    states: Vec<UpstreamState>,
    rr: usize,
}

#[derive(Debug)]
struct Score {
    index: usize,
    overlap: usize,
    affinity: usize,
    load: usize,
    weighted: f64,
    healthy: bool,
}

pub struct Router {
    config: RouterConfig,
    inner: Mutex<Inner>,
}

impl Router {
    /// Creates a router with one bounded cache index per upstream.
    ///
    /// # Panics
    ///
    /// Panics if there are no upstreams or the configured index capacity is zero.
    #[must_use]
    pub fn new(config: RouterConfig) -> Self {
        assert!(!config.upstreams.is_empty(), "router needs an upstream");
        let capacity = NonZeroUsize::new(config.index_capacity).expect("positive index capacity");
        let states = config
            .upstreams
            .iter()
            .map(|_| UpstreamState {
                index: HashSet::with_capacity(config.index_capacity.min(4_096)),
                lru: LruCache::new(capacity),
                inflight: 0,
                load: 0,
                healthy: true,
            })
            .collect();
        Self {
            config,
            inner: Mutex::new(Inner { states, rr: 0 }),
        }
    }

    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    pub fn fingerprints(&self, body: &[u8]) -> Vec<u64> {
        let object = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| match value {
                Value::Object(object) => Some(object),
                _ => None,
            });
        self.fingerprints_preparsed(body, object.as_ref())
    }

    pub fn route(&self, body: &[u8]) -> Decision {
        let fingerprints = self.fingerprints(body);
        self.route_fingerprints(body.len(), &fingerprints)
    }

    /// Prepares fingerprints once and returns them with the decision so a
    /// successful proxy response can update the cache index without reparsing
    /// and rehashing a potentially multi-megabyte prompt.
    #[must_use]
    pub fn route_with_fingerprints(&self, body: &[u8]) -> (Decision, Vec<u64>) {
        let fingerprints = self.fingerprints(body);
        let decision = self.route_fingerprints(body.len(), &fingerprints);
        (decision, fingerprints)
    }

    pub(crate) fn fingerprints_preparsed(
        &self,
        body: &[u8],
        object: Option<&Map<String, Value>>,
    ) -> Vec<u64> {
        chain_fingerprints(
            &canonical_prompt(body, object, self.config.max_prefix_bytes),
            self.config.chunk_bytes,
        )
    }

    pub(crate) fn route_prepared(&self, body_bytes: usize, fingerprints: &[u64]) -> Decision {
        self.route_fingerprints(body_bytes, fingerprints)
    }

    fn route_fingerprints(&self, body_bytes: usize, fingerprints: &[u64]) -> Decision {
        let mut inner = self.inner.lock();
        inner.rr = inner.rr.wrapping_add(1);
        let rotation = inner.rr % inner.states.len();
        let mut scores = inner
            .states
            .iter()
            .enumerate()
            .map(|(index, state)| {
                let overlap = if self.config.affinity == Affinity::Prefix {
                    fingerprints
                        .iter()
                        .take_while(|fingerprint| state.index.contains(fingerprint))
                        .count()
                } else {
                    0
                };
                let affinity = overlap.min(self.config.max_overlap_blocks);
                Score {
                    index,
                    overlap,
                    affinity,
                    load: state.load,
                    #[allow(clippy::cast_precision_loss)]
                    weighted: affinity as f64 - self.config.alpha * state.load as f64,
                    healthy: state.healthy,
                }
            })
            .collect::<Vec<_>>();
        let candidate_count = scores.len();
        scores.sort_by(|left, right| {
            right
                .healthy
                .cmp(&left.healthy)
                .then_with(|| {
                    right
                        .weighted
                        .partial_cmp(&left.weighted)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| right.overlap.cmp(&left.overlap))
                .then_with(|| {
                    ((left.index + rotation) % candidate_count)
                        .cmp(&((right.index + rotation) % candidate_count))
                })
        });
        let winner = &scores[0];
        #[allow(clippy::float_cmp)] // Exact equality is the established routing policy.
        let scores_differ = scores
            .iter()
            .skip(1)
            .any(|score| score.weighted != winner.weighted);
        let outcome = if scores.len() == 1 {
            Outcome::Single
        } else if winner.overlap > 0 {
            Outcome::Overlap
        } else if scores_differ {
            Outcome::Load
        } else {
            Outcome::RoundRobin
        };
        let candidate_state = scores
            .iter()
            .enumerate()
            .map(|(rank, score)| CandidateState {
                index: score.index,
                rank,
                overlap_blocks: score.overlap,
                affinity_blocks: score.affinity,
                load_units: score.load,
                request_load_units: self.estimated_load_units(body_bytes, score.overlap),
                healthy: score.healthy,
            })
            .collect();
        Decision {
            candidates: scores.iter().map(|score| score.index).collect(),
            candidate_state,
            overlap_blocks: winner.overlap,
            total_blocks: fingerprints.len(),
            affinity_blocks: winner.affinity,
            load_units: self.estimated_load_units(body_bytes, winner.overlap),
            rotation,
            outcome,
        }
    }

    pub fn observe(&self, upstream: usize, fingerprints: &[u64]) {
        if fingerprints.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        let Some(state) = inner.states.get_mut(upstream) else {
            return;
        };
        for fingerprint in fingerprints {
            if let Some((evicted, ())) = state.lru.push(*fingerprint, ()) {
                state.index.remove(&evicted);
            }
            state.index.insert(*fingerprint);
        }
    }

    pub fn acquire(self: &Arc<Self>, upstream: usize, units: usize) -> LoadGuard {
        let units = units.max(1);
        {
            let mut inner = self.inner.lock();
            if let Some(state) = inner.states.get_mut(upstream) {
                state.inflight += 1;
                state.load += units;
            }
        }
        LoadGuard {
            router: Arc::clone(self),
            upstream,
            units,
            released: false,
        }
    }

    /// Atomically rechecks serving health while reserving load. This closes the
    /// route-to-dispatch window where a probe may have already fenced a replica.
    pub fn acquire_if_healthy(
        self: &Arc<Self>,
        upstream: usize,
        units: usize,
    ) -> Option<LoadGuard> {
        let units = units.max(1);
        {
            let mut inner = self.inner.lock();
            let state = inner.states.get_mut(upstream)?;
            if !state.healthy {
                return None;
            }
            state.inflight += 1;
            state.load += units;
        }
        Some(LoadGuard {
            router: Arc::clone(self),
            upstream,
            units,
            released: false,
        })
    }

    pub fn set_healthy(&self, upstream: usize, healthy: bool) {
        if let Some(state) = self.inner.lock().states.get_mut(upstream) {
            state.healthy = healthy;
        }
    }

    pub fn state(&self, upstream: usize) -> Option<(usize, usize, usize, bool)> {
        self.inner
            .lock()
            .states
            .get(upstream)
            .map(|state| (state.inflight, state.load, state.index.len(), state.healthy))
    }

    fn estimated_load_units(&self, body_bytes: usize, overlap_blocks: usize) -> usize {
        let uncached =
            body_bytes.saturating_sub(overlap_blocks.saturating_mul(self.config.chunk_bytes));
        uncached
            .div_ceil(self.config.load_unit_bytes)
            .max(1)
            .min(self.config.max_load_units)
    }
}

pub struct LoadGuard {
    router: Arc<Router>,
    upstream: usize,
    units: usize,
    released: bool,
}

impl LoadGuard {
    pub fn release(mut self) {
        self.do_release();
    }

    fn do_release(&mut self) {
        if self.released {
            return;
        }
        if let Some(state) = self.router.inner.lock().states.get_mut(self.upstream) {
            state.inflight = state.inflight.saturating_sub(1);
            state.load = state.load.saturating_sub(self.units);
        }
        self.released = true;
    }
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.do_release();
    }
}

fn canonical_prompt(body: &[u8], object: Option<&Map<String, Value>>, max_bytes: usize) -> Vec<u8> {
    let fallback = || body[..body.len().min(max_bytes)].to_vec();
    let Some(object) = object else {
        return fallback();
    };
    let messages = match object.get("messages") {
        None | Some(Value::Null) => None,
        Some(Value::Array(messages)) => Some(messages),
        Some(_) => return fallback(),
    };
    if messages.is_none() && object.get("system").is_none_or(Value::is_null) {
        return fallback();
    }
    let messages = messages.map(Vec::as_slice).unwrap_or_default();
    let mut output = Vec::with_capacity(max_bytes.min(64 << 10));
    if let Some(system) = object.get("system").filter(|value| !value.is_null()) {
        let synthetic = Map::from_iter([
            ("role".to_owned(), Value::String("system".to_owned())),
            ("content".to_owned(), system.clone()),
        ]);
        append_message(&mut output, &synthetic);
    }
    let mut leading = 0;
    while let Some(Value::Object(message)) = messages.get(leading) {
        if !matches!(
            message.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        ) {
            break;
        }
        append_message(&mut output, message);
        leading += 1;
    }
    for key in [
        "tools",
        "functions",
        "tool_choice",
        "function_call",
        "parallel_tool_calls",
        "reasoning_effort",
        "thinking",
        "response_format",
    ] {
        append_field(&mut output, key, object.get(key));
    }
    for message in &messages[leading..] {
        if let Value::Object(message) = message {
            append_message(&mut output, message);
        }
    }
    output.truncate(max_bytes);
    output
}

fn append_message(output: &mut Vec<u8>, message: &Map<String, Value>) {
    output.extend_from_slice(b"message\0");
    for key in [
        "role",
        "name",
        "content",
        "reasoning_content",
        "reasoning",
        "tool_calls",
        "function_call",
        "tool_call_id",
    ] {
        append_field(output, key, message.get(key));
    }
}

fn append_field(output: &mut Vec<u8>, key: &str, value: Option<&Value>) {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return;
    };
    output.extend_from_slice(key.as_bytes());
    output.push(0);
    append_canonical_json(output, value);
    output.push(0);
}

/// Serialize JSON with recursively sorted object keys. Fingerprints must not
/// depend on `serde_json`'s optional `preserve_order` feature, which transitive
/// tokenizer dependencies can enable for the entire crate graph.
fn append_canonical_json(output: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                append_canonical_json(output, value);
            }
            output.push(b']');
        }
        Value::Object(object) => {
            output.push(b'{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .expect("serializing a JSON object key into Vec cannot fail");
                output.push(b':');
                append_canonical_json(output, &object[key]);
            }
            output.push(b'}');
        }
        _ => serde_json::to_writer(output, value)
            .expect("serializing a JSON scalar into Vec cannot fail"),
    }
}

fn chain_fingerprints(prompt: &[u8], chunk_bytes: usize) -> Vec<u64> {
    if prompt.is_empty() || chunk_bytes == 0 {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(prompt.len().div_ceil(chunk_bytes));
    let mut previous = 0_u64;
    for chunk in prompt.chunks(chunk_bytes) {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in previous.to_le_bytes().iter().chain(chunk) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        previous = hash;
        output.push(hash);
    }
    output
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config() -> RouterConfig {
        RouterConfig {
            upstreams: vec![
                Url::parse("http://a:8000").unwrap(),
                Url::parse("http://b:8000").unwrap(),
            ],
            alpha: 4.0,
            chunk_bytes: 64,
            max_prefix_bytes: 2 << 20,
            max_overlap_blocks: 32,
            index_capacity: 100_000,
            load_unit_bytes: 32 << 10,
            max_load_units: 8,
            affinity: Affinity::Prefix,
        }
    }

    fn chat(system: &str, user: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        }))
        .unwrap()
    }

    #[test]
    fn sticky_and_template_shared_requests_co_locate() {
        let router = Router::new(config());
        let first = chat(&"shared ".repeat(100), "one");
        let decision = router.route(&first);
        let home = decision.candidates[0];
        router.observe(home, &router.fingerprints(&first));
        let second = chat(&"shared ".repeat(100), "two");
        let decision = router.route(&second);
        assert_eq!(decision.candidates[0], home);
        assert_eq!(decision.outcome, Outcome::Overlap);
        assert!(decision.overlap_blocks > 0);
    }

    #[test]
    fn weighted_load_reserves_prefill_engine() {
        let mut config = config();
        config.load_unit_bytes = 128;
        config.max_load_units = 4;
        let router = Arc::new(Router::new(config));
        let large = chat(&"cold ".repeat(1_000), "fresh");
        let prefill = router.route(&large);
        assert_eq!(prefill.load_units, 4);
        let _guard = router.acquire(prefill.candidates[0], prefill.load_units);
        let small = router.route(&chat("small", "x"));
        assert_ne!(small.candidates[0], prefill.candidates[0]);
    }

    #[test]
    fn exact_score_tie_prefers_warm_prefix() {
        let mut config = config();
        config.alpha = 1.0;
        config.max_overlap_blocks = 4;
        let router = Arc::new(Router::new(config));
        let body = chat(&"warm ".repeat(100), "task");
        let home = router.route(&body).candidates[0];
        router.observe(home, &router.fingerprints(&body));
        let mut guards = Vec::new();
        for _ in 0..4 {
            guards.push(router.acquire(home, 1));
        }
        let decision = router.route(&body);
        assert_eq!(decision.affinity_blocks, 4);
        assert_eq!(decision.candidates[0], home);
    }

    #[test]
    fn anthropic_and_openai_systems_canonicalize_equally() {
        let router = Router::new(config());
        let openai = br#"{"messages":[{"role":"system","content":"shared system"},{"role":"user","content":"hello"}],"tools":[{"name":"lookup"}]}"#;
        let anthropic = br#"{"system":"shared system","tools":[{"name":"lookup"}],"messages":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(router.fingerprints(openai), router.fingerprints(anthropic));
    }

    #[test]
    fn unhealthy_upstream_sorts_last() {
        let router = Router::new(config());
        let body = chat("sys", "hello");
        let winner = router.route(&body).candidates[0];
        router.observe(winner, &router.fingerprints(&body));
        router.set_healthy(winner, false);
        let decision = router.route(&body);
        assert_eq!(decision.candidates.last(), Some(&winner));
    }

    #[test]
    fn dispatch_reservation_rechecks_current_health_atomically() {
        let router = Arc::new(Router::new(config()));
        let upstream = router.route(&chat("sys", "hello")).candidates[0];
        router.set_healthy(upstream, false);
        assert!(router.acquire_if_healthy(upstream, 4).is_none());
        assert_eq!(
            router.state(upstream).map(|state| (state.0, state.1)),
            Some((0, 0))
        );
        router.set_healthy(upstream, true);
        let guard = router.acquire_if_healthy(upstream, 4).expect("healthy");
        assert_eq!(
            router.state(upstream).map(|state| (state.0, state.1)),
            Some((1, 4))
        );
        drop(guard);
    }

    #[test]
    fn recovered_upstream_reenters_routing() {
        let router = Router::new(config());
        let body = chat(&"recovery ".repeat(100), "hello");
        let home = router.route(&body).candidates[0];
        router.observe(home, &router.fingerprints(&body));
        router.set_healthy(home, false);
        assert_ne!(router.route(&body).candidates[0], home);
        router.set_healthy(home, true);
        assert_eq!(router.route(&body).candidates[0], home);
    }

    #[test]
    fn fingerprints_match_legacy_goldens() {
        let router = Router::new(config());
        let cases: &[(&[u8], &[u64])] = &[
            (
                br#"{"messages":[{"role":"system","content":"shared system"},{"role":"user","content":"hello"}],"tools":[{"name":"lookup","description":"x"}]}"#,
                &[1_194_147_559_601_608_630, 12_960_585_294_022_105_508],
            ),
            (
                br#"{"system":"shared system","tools":[{"description":"x","name":"lookup"}],"messages":[{"content":"hello","role":"user"}]}"#,
                &[1_194_147_559_601_608_630, 12_960_585_294_022_105_508],
            ),
            (
                br#"{"messages":[{"role":"assistant","content":"","reasoning":"think","tool_calls":[{"id":"call-1","function":{"name":"lookup","arguments":"{}"}}]}],"reasoning_effort":"high"}"#,
                &[
                    1_364_963_793_087_020_995,
                    7_759_282_652_774_235_360,
                    15_258_068_751_277_170_983,
                ],
            ),
            (
                br#"{"system":"x","messages":"not-an-array"}"#,
                &[7_245_889_978_181_980_159],
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(router.fingerprints(body), *expected);
        }
    }

    #[test]
    fn fingerprints_ignore_nested_object_insertion_order() {
        let router = Router::new(config());
        let first = br#"{"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"z":{"type":"integer"},"a":{"type":"string"}}}}}]}"#;
        let second = br#"{"tools":[{"function":{"parameters":{"properties":{"a":{"type":"string"},"z":{"type":"integer"}},"type":"object"},"name":"lookup"},"type":"function"}],"messages":[{"content":"hello","role":"user"}]}"#;
        assert_eq!(router.fingerprints(first), router.fingerprints(second));
    }

    #[test]
    fn prepared_route_reuses_returned_fingerprints() {
        let router = Router::new(config());
        let body = chat(&"long shared prompt ".repeat(100), "task");
        let expected = router.fingerprints(&body);
        let (decision, fingerprints) = router.route_with_fingerprints(&body);
        assert_eq!(fingerprints, expected);
        assert_eq!(decision.total_blocks, expected.len());
        router.observe(decision.candidates[0], &fingerprints);
        assert!(router.route(&body).overlap_blocks > 0);
    }
}
