use std::{cmp::Ordering, collections::HashSet, num::NonZeroUsize, sync::Arc};

use lru::LruCache;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Map, Value};
use url::Url;

use crate::config::{Affinity, SpeculationProfile, SpeculationRouteMode};

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
    pub projected_load: bool,
    pub speculation_mode: SpeculationRouteMode,
    pub speculation_profiles: Vec<SpeculationProfile>,
    pub affinity: Affinity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpeculationPreference {
    Neutral,
    Standard,
    Mtp,
}

impl SpeculationPreference {
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Standard => "standard",
            Self::Mtp => "mtp",
        }
    }

    const fn matches(self, profile: SpeculationProfile) -> bool {
        matches!(
            (self, profile),
            (Self::Standard, SpeculationProfile::Standard) | (Self::Mtp, SpeculationProfile::Mtp)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpeculationRouteOutcome {
    Off,
    Neutral,
    Match,
    WouldMove,
    Moved,
    ScoreBlocked,
    Unavailable,
}

impl SpeculationRouteOutcome {
    pub(crate) const ALL: [Self; 7] = [
        Self::Off,
        Self::Neutral,
        Self::Match,
        Self::WouldMove,
        Self::Moved,
        Self::ScoreBlocked,
        Self::Unavailable,
    ];

    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Neutral => "neutral",
            Self::Match => "match",
            Self::WouldMove => "would_move",
            Self::Moved => "moved",
            Self::ScoreBlocked => "score_blocked",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SpeculationRouteObservation {
    pub(crate) preference: SpeculationPreference,
    pub(crate) outcome: SpeculationRouteOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestLoadEstimator {
    chunk_bytes: usize,
    load_unit_bytes: usize,
    max_load_units: usize,
}

impl RequestLoadEstimator {
    pub(crate) fn new(chunk_bytes: usize, load_unit_bytes: usize, max_load_units: usize) -> Self {
        assert!(chunk_bytes > 0, "route chunk size must be positive");
        assert!(load_unit_bytes > 0, "route load unit must be positive");
        assert!(max_load_units > 0, "maximum route load must be positive");
        Self {
            chunk_bytes,
            load_unit_bytes,
            max_load_units,
        }
    }

    pub(crate) fn from_router_config(config: &RouterConfig) -> Self {
        Self::new(
            config.chunk_bytes,
            config.load_unit_bytes,
            config.max_load_units,
        )
    }

    pub(crate) fn estimate_blocks(self, body_bytes: usize, overlap_blocks: usize) -> usize {
        self.estimate_reusable_bytes(body_bytes, overlap_blocks.saturating_mul(self.chunk_bytes))
    }

    pub(crate) fn estimate_exact_tokens(
        self,
        body_bytes: usize,
        overlap_tokens: usize,
        prompt_tokens: usize,
    ) -> Option<usize> {
        if prompt_tokens == 0 || overlap_tokens > prompt_tokens {
            return None;
        }
        let reusable =
            (body_bytes as u128).saturating_mul(overlap_tokens as u128) / (prompt_tokens as u128);
        Some(
            self.estimate_reusable_bytes(
                body_bytes,
                usize::try_from(reusable).unwrap_or(usize::MAX),
            ),
        )
    }

    fn estimate_reusable_bytes(self, body_bytes: usize, reusable_bytes: usize) -> usize {
        body_bytes
            .saturating_sub(reusable_bytes)
            .div_ceil(self.load_unit_bytes)
            .max(1)
            .min(self.max_load_units)
    }
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

impl Decision {
    /// Preserve a bounded decode reservation after any exact-prefix
    /// recomputation has adjusted candidate-specific prefill work.
    pub(crate) fn apply_request_load_floor(&mut self, load_floor: usize) {
        let load_floor = load_floor.max(1);
        for candidate in &mut self.candidate_state {
            candidate.request_load_units = candidate.request_load_units.max(load_floor);
        }
        if let Some(&selected) = self.candidates.first()
            && let Some(candidate) = self
                .candidate_state
                .iter()
                .find(|candidate| candidate.index == selected)
        {
            self.load_units = candidate.request_load_units;
        }
    }
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
    /// Withheld from routing by the idle-drain policy. Kept separate from
    /// `healthy` so a parked replica is never confused with a failing one:
    /// `upstream_up` continues to report reachability, and only the dedicated
    /// drain gauges explain why an available replica is not receiving traffic.
    drained: bool,
}

impl UpstreamState {
    /// Whether this replica may receive new requests. Both conditions are
    /// checked everywhere routing admits work, so a drain cannot be bypassed by
    /// a path that only consulted health.
    fn serving(&self) -> bool {
        self.healthy && !self.drained
    }
}

struct Inner {
    states: Vec<UpstreamState>,
    rr: usize,
}

#[derive(Clone, Debug)]
struct Score {
    index: usize,
    overlap: usize,
    affinity: usize,
    load: usize,
    request_load: usize,
    weighted: f64,
    healthy: bool,
}

fn compare_scores(
    left: &Score,
    right: &Score,
    rotation: usize,
    candidate_count: usize,
    preference: SpeculationPreference,
    profiles: &[SpeculationProfile],
    profile_tie_break: bool,
) -> Ordering {
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
            if profile_tie_break {
                preference
                    .matches(profiles[right.index])
                    .cmp(&preference.matches(profiles[left.index]))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| {
            ((left.index + rotation) % candidate_count)
                .cmp(&((right.index + rotation) % candidate_count))
        })
}

fn speculation_route_outcome(
    scores: &[Score],
    mode: SpeculationRouteMode,
    profiles: &[SpeculationProfile],
    preference: SpeculationPreference,
) -> SpeculationRouteOutcome {
    if mode == SpeculationRouteMode::Off {
        return SpeculationRouteOutcome::Off;
    }
    if preference == SpeculationPreference::Neutral {
        return SpeculationRouteOutcome::Neutral;
    }
    let baseline = &scores[0];
    if preference.matches(profiles[baseline.index]) {
        return SpeculationRouteOutcome::Match;
    }
    let preferred_available = scores
        .iter()
        .any(|score| score.healthy && preference.matches(profiles[score.index]));
    if !preferred_available {
        return SpeculationRouteOutcome::Unavailable;
    }
    #[allow(clippy::float_cmp)] // Tie-only routing mirrors the established score policy.
    let preferred_tied = scores.iter().any(|score| {
        score.healthy
            && preference.matches(profiles[score.index])
            && score.weighted == baseline.weighted
            && score.overlap == baseline.overlap
    });
    if !preferred_tied {
        return SpeculationRouteOutcome::ScoreBlocked;
    }
    match mode {
        SpeculationRouteMode::Shadow => SpeculationRouteOutcome::WouldMove,
        SpeculationRouteMode::Prefer => SpeculationRouteOutcome::Moved,
        SpeculationRouteMode::Off => unreachable!("off mode returned above"),
    }
}

fn decision_from_scores(scores: &[Score], total_blocks: usize, rotation: usize) -> Decision {
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
            request_load_units: score.request_load,
            healthy: score.healthy,
        })
        .collect();
    Decision {
        candidates: scores.iter().map(|score| score.index).collect(),
        candidate_state,
        overlap_blocks: winner.overlap,
        total_blocks,
        affinity_blocks: winner.affinity,
        load_units: winner.request_load,
        rotation,
        outcome,
    }
}

pub struct Router {
    config: RouterConfig,
    load_estimator: RequestLoadEstimator,
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
        assert_eq!(
            config.speculation_profiles.len(),
            config.upstreams.len(),
            "router needs one speculation profile per upstream"
        );
        let capacity = NonZeroUsize::new(config.index_capacity).expect("positive index capacity");
        let load_estimator = RequestLoadEstimator::from_router_config(&config);
        let states = config
            .upstreams
            .iter()
            .map(|_| UpstreamState {
                index: HashSet::with_capacity(config.index_capacity.min(4_096)),
                lru: LruCache::new(capacity),
                inflight: 0,
                load: 0,
                healthy: true,
                drained: false,
            })
            .collect();
        Self {
            config,
            load_estimator,
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
        self.route_fingerprints(
            body.len(),
            &fingerprints,
            1,
            SpeculationPreference::Neutral,
            true,
        )
        .0
    }

    /// Prepares fingerprints once and returns them with the decision so a
    /// successful proxy response can update the cache index without reparsing
    /// and rehashing a potentially multi-megabyte prompt.
    #[must_use]
    pub fn route_with_fingerprints(&self, body: &[u8]) -> (Decision, Vec<u64>) {
        let fingerprints = self.fingerprints(body);
        let decision = self
            .route_fingerprints(
                body.len(),
                &fingerprints,
                1,
                SpeculationPreference::Neutral,
                true,
            )
            .0;
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
        self.route_fingerprints(
            body_bytes,
            fingerprints,
            1,
            SpeculationPreference::Neutral,
            true,
        )
        .0
    }

    #[cfg(test)]
    pub(crate) fn route_prepared_with_load_floor(
        &self,
        body_bytes: usize,
        fingerprints: &[u64],
        load_floor: usize,
    ) -> Decision {
        self.route_fingerprints(
            body_bytes,
            fingerprints,
            load_floor,
            SpeculationPreference::Neutral,
            true,
        )
        .0
    }

    #[cfg(test)]
    pub(crate) fn route_prepared_profiled(
        &self,
        body_bytes: usize,
        fingerprints: &[u64],
        preference: SpeculationPreference,
    ) -> (Decision, SpeculationRouteObservation) {
        self.route_prepared_profiled_with_load_floor(body_bytes, fingerprints, 1, preference)
    }

    pub(crate) fn route_prepared_profiled_with_load_floor(
        &self,
        body_bytes: usize,
        fingerprints: &[u64],
        load_floor: usize,
        preference: SpeculationPreference,
    ) -> (Decision, SpeculationRouteObservation) {
        self.route_fingerprints(body_bytes, fingerprints, load_floor, preference, true)
    }

    fn route_fingerprints(
        &self,
        body_bytes: usize,
        fingerprints: &[u64],
        load_floor: usize,
        preference: SpeculationPreference,
        advance_rotation: bool,
    ) -> (Decision, SpeculationRouteObservation) {
        let mut inner = self.inner.lock();
        let next_rotation = inner.rr.wrapping_add(1);
        if advance_rotation {
            inner.rr = next_rotation;
        }
        let rotation = next_rotation % inner.states.len();
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
                let request_load = self
                    .load_estimator
                    .estimate_blocks(body_bytes, overlap)
                    .max(load_floor.clamp(1, self.config.max_load_units));
                let additional_load = if self.config.projected_load {
                    request_load.saturating_sub(1)
                } else {
                    0
                };
                let projected_load = state.load.saturating_add(additional_load);
                Score {
                    index,
                    overlap,
                    affinity,
                    load: state.load,
                    request_load,
                    #[allow(clippy::cast_precision_loss)]
                    weighted: affinity as f64 - self.config.alpha * projected_load as f64,
                    healthy: state.serving(),
                }
            })
            .collect::<Vec<_>>();
        let candidate_count = scores.len();
        scores.sort_by(|left, right| {
            compare_scores(
                left,
                right,
                rotation,
                candidate_count,
                preference,
                &self.config.speculation_profiles,
                false,
            )
        });
        let profile_outcome = speculation_route_outcome(
            &scores,
            self.config.speculation_mode,
            &self.config.speculation_profiles,
            preference,
        );
        if self.config.speculation_mode == SpeculationRouteMode::Prefer
            && profile_outcome == SpeculationRouteOutcome::Moved
        {
            scores.sort_by(|left, right| {
                compare_scores(
                    left,
                    right,
                    rotation,
                    candidate_count,
                    preference,
                    &self.config.speculation_profiles,
                    true,
                )
            });
        }
        let decision = decision_from_scores(&scores, fingerprints.len(), rotation);
        (
            decision,
            SpeculationRouteObservation {
                preference,
                outcome: profile_outcome,
            },
        )
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
        self.acquire_gated(upstream, units, true)
    }

    /// Reserves load on a replica that is currently marked unhealthy.
    ///
    /// This is the proxy's fail-open reservation, used only when no upstream is
    /// healthy at all. The idle-drain fence is still honoured: a parked replica
    /// may be stopped by the converging actor at any moment, so it must never
    /// receive traffic, whereas an unhealthy replica may merely be too busy to
    /// answer a probe and will still serve the request.
    pub fn acquire_failing_open(
        self: &Arc<Self>,
        upstream: usize,
        units: usize,
    ) -> Option<LoadGuard> {
        self.acquire_gated(upstream, units, false)
    }

    fn acquire_gated(
        self: &Arc<Self>,
        upstream: usize,
        units: usize,
        require_healthy: bool,
    ) -> Option<LoadGuard> {
        let units = units.max(1);
        {
            let mut inner = self.inner.lock();
            let state = inner.states.get_mut(upstream)?;
            if state.drained || (require_healthy && !state.serving()) {
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

    /// Withholds an upstream from routing without touching its health.
    ///
    /// Used by the idle-drain policy. Clearing the flag restores routing
    /// immediately; the replica still has to pass its own health probe before
    /// it is admitted, so resuming a container that has not finished starting
    /// cannot route traffic into it.
    pub fn set_drained(&self, upstream: usize, drained: bool) {
        if let Some(state) = self.inner.lock().states.get_mut(upstream) {
            state.drained = drained;
        }
    }

    /// Replaces every drain fence under one router lock. Used for topology
    /// membership changes where a partially published shape could otherwise
    /// dispatch into an engine while its peer is being reconfigured.
    pub fn set_drained_mask(&self, drained: &[bool]) -> bool {
        let mut inner = self.inner.lock();
        if inner.states.len() != drained.len() {
            return false;
        }
        for (state, drained) in inner.states.iter_mut().zip(drained) {
            state.drained = *drained;
        }
        true
    }

    /// Whether an upstream is currently withheld by the idle-drain policy.
    #[must_use]
    pub fn drained(&self, upstream: usize) -> bool {
        self.inner
            .lock()
            .states
            .get(upstream)
            .is_some_and(|state| state.drained)
    }

    /// Requests currently dispatched to an upstream, for drain observation.
    #[must_use]
    pub fn inflight(&self, upstream: usize) -> usize {
        self.inner
            .lock()
            .states
            .get(upstream)
            .map_or(0, |state| state.inflight)
    }

    pub fn state(&self, upstream: usize) -> Option<(usize, usize, usize, bool)> {
        self.inner
            .lock()
            .states
            .get(upstream)
            .map(|state| (state.inflight, state.load, state.index.len(), state.healthy))
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

    /// Reduce this request's reservation without releasing its inflight slot.
    ///
    /// The proxy uses this at the observed prefill/decode boundary. `DistServe`
    /// motivates accounting for TTFT/prefill and TPOT/decode independently:
    /// <https://www.usenix.org/conference/osdi24/presentation/zhong-yinmin>.
    /// Growing a reservation after dispatch is intentionally unsupported.
    pub fn reduce_to(&mut self, units: usize) -> bool {
        if self.released {
            return false;
        }
        let units = units.max(1);
        if units >= self.units {
            return false;
        }
        let released = self.units - units;
        let mut inner = self.router.inner.lock();
        let Some(state) = inner.states.get_mut(self.upstream) else {
            return false;
        };
        state.load = state.load.saturating_sub(released);
        self.units = units;
        true
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
        "chat_template_kwargs",
        "enable_thinking",
        "preserve_thinking",
        "mm_processor_kwargs",
        "add_generation_prompt",
        "continue_final_message",
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
            projected_load: false,
            speculation_mode: SpeculationRouteMode::Off,
            speculation_profiles: vec![SpeculationProfile::Standard; 2],
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
    fn request_load_estimator_is_shared_bounded_and_conservative() {
        let estimator = RequestLoadEstimator::new(2_048, 32 << 10, 8);
        assert_eq!(estimator.estimate_blocks(1 << 20, 0), 8);
        assert_eq!(estimator.estimate_blocks(1 << 20, 512), 1);
        assert_eq!(estimator.estimate_exact_tokens(1 << 20, 0, 4_096), Some(8));
        assert_eq!(
            estimator.estimate_exact_tokens(1 << 20, 2_048, 4_096),
            Some(8)
        );
        assert_eq!(
            RequestLoadEstimator::new(2_048, 128 << 10, 8).estimate_exact_tokens(
                1 << 20,
                2_048,
                4_096
            ),
            Some(4)
        );
        assert_eq!(
            estimator.estimate_exact_tokens(1 << 20, 4_096, 4_096),
            Some(1)
        );
        assert_eq!(estimator.estimate_exact_tokens(1 << 20, 1, 0), None);
        assert_eq!(estimator.estimate_exact_tokens(1 << 20, 4_097, 4_096), None);
    }

    #[test]
    fn decode_floor_survives_candidate_specific_reservation_recompute() {
        let router = Router::new(config());
        let mut decision = router.route_prepared_with_load_floor(1, &[], 4);
        assert!(
            decision
                .candidate_state
                .iter()
                .all(|candidate| candidate.request_load_units == 4)
        );
        for candidate in &mut decision.candidate_state {
            candidate.request_load_units = 1;
        }
        decision.load_units = 1;
        decision.apply_request_load_floor(4);
        assert!(
            decision
                .candidate_state
                .iter()
                .all(|candidate| candidate.request_load_units == 4)
        );
        assert_eq!(decision.load_units, 4);
    }

    #[test]
    fn speculation_profile_moves_only_a_final_score_tie_and_shadow_is_immutable() {
        let mut shadow_config = config();
        shadow_config.speculation_mode = SpeculationRouteMode::Shadow;
        shadow_config.speculation_profiles =
            vec![SpeculationProfile::Mtp, SpeculationProfile::Standard];
        let shadow = Router::new(shadow_config);
        let (decision, observation) =
            shadow.route_prepared_profiled(1, &[], SpeculationPreference::Mtp);
        assert_eq!(decision.candidates[0], 1);
        assert_eq!(observation.outcome, SpeculationRouteOutcome::WouldMove);

        let mut prefer_config = config();
        prefer_config.speculation_mode = SpeculationRouteMode::Prefer;
        prefer_config.speculation_profiles =
            vec![SpeculationProfile::Mtp, SpeculationProfile::Standard];
        let prefer = Router::new(prefer_config);
        let (decision, observation) =
            prefer.route_prepared_profiled(1, &[], SpeculationPreference::Mtp);
        assert_eq!(decision.candidates[0], 0);
        assert_eq!(observation.outcome, SpeculationRouteOutcome::Moved);
    }

    #[test]
    fn speculation_profile_never_overrides_better_prefix_locality() {
        let mut configured = config();
        configured.speculation_mode = SpeculationRouteMode::Prefer;
        configured.speculation_profiles =
            vec![SpeculationProfile::Mtp, SpeculationProfile::Standard];
        let router = Router::new(configured);
        router.observe(1, &[7]);
        let (decision, observation) =
            router.route_prepared_profiled(1, &[7], SpeculationPreference::Mtp);
        assert_eq!(decision.candidates[0], 1);
        assert_eq!(observation.outcome, SpeculationRouteOutcome::ScoreBlocked);
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
    fn projected_load_preserves_all_cold_placement() {
        let mut baseline_config = config();
        baseline_config.max_load_units = 32;
        let mut projected_config = baseline_config.clone();
        projected_config.projected_load = true;
        let baseline = Router::new(baseline_config);
        let projected = Router::new(projected_config);
        let cold = vec![b'x'; 1 << 20];

        assert_eq!(
            baseline.route(&cold).candidates,
            projected.route(&cold).candidates
        );
    }

    #[test]
    fn projected_load_accounts_for_candidate_specific_uncached_work() {
        let mut baseline_config = config();
        baseline_config.max_load_units = 32;
        let mut projected_config = baseline_config.clone();
        projected_config.projected_load = true;
        let baseline = Arc::new(Router::new(baseline_config));
        let projected = Arc::new(Router::new(projected_config));
        let prompt = vec![b'x'; 1 << 20];

        for router in [&baseline, &projected] {
            router.observe(0, &router.fingerprints(&prompt));
        }
        let _baseline_load = baseline.acquire(0, 9);
        let _projected_load = projected.acquire(0, 9);

        assert_eq!(baseline.route(&prompt).candidates[0], 1);
        assert_eq!(projected.route(&prompt).candidates[0], 0);
    }

    #[test]
    fn projected_load_does_not_change_single_unit_requests() {
        let mut baseline_config = config();
        baseline_config.max_load_units = 32;
        let mut projected_config = baseline_config.clone();
        projected_config.projected_load = true;
        let baseline = Arc::new(Router::new(baseline_config));
        let projected = Arc::new(Router::new(projected_config));
        let prompt = vec![b'x'; 16 << 10];

        for router in [&baseline, &projected] {
            router.observe(0, &router.fingerprints(&prompt));
        }
        let _baseline_load = baseline.acquire(0, 9);
        let _projected_load = projected.acquire(0, 9);

        assert_eq!(
            baseline.route(&prompt).candidates,
            projected.route(&prompt).candidates
        );
    }

    #[test]
    fn projected_sort_retains_each_candidates_admission_reservation() {
        let mut projected_config = config();
        projected_config.max_load_units = 32;
        projected_config.projected_load = true;
        let router = Arc::new(Router::new(projected_config));
        let prompt = vec![b'x'; 1 << 20];
        router.observe(0, &router.fingerprints(&prompt));
        let _warm_load = router.acquire(0, 9);

        let decision = router.route(&prompt);
        let warm = decision
            .candidate_state
            .iter()
            .find(|candidate| candidate.index == 0)
            .unwrap();
        let cold = decision
            .candidate_state
            .iter()
            .find(|candidate| candidate.index == 1)
            .unwrap();
        assert_eq!(warm.request_load_units, 1);
        assert_eq!(cold.request_load_units, 32);
        assert_eq!(decision.load_units, warm.request_load_units);

        let cold_units = cold.request_load_units;
        let _failover_load = router.acquire(1, cold_units);
        assert_eq!(router.state(1).map(|state| state.1), Some(cold_units));
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
    fn drained_upstream_sorts_last_and_is_not_a_serving_candidate() {
        let router = Router::new(config());
        let body = chat("sys", "hello");
        let winner = router.route(&body).candidates[0];
        router.observe(winner, &router.fingerprints(&body));
        // Warm prefix affinity would normally pin this request to `winner`.
        router.set_drained(winner, true);
        let decision = router.route(&body);
        assert_eq!(decision.candidates.last(), Some(&winner));
        let drained = decision
            .candidate_state
            .iter()
            .find(|state| state.index == winner)
            .expect("candidate present");
        assert!(
            !drained.healthy,
            "a drained replica must not be offered as a serving candidate"
        );
    }

    #[test]
    fn drain_is_independent_of_health_and_reversible() {
        let router = Router::new(config());
        let upstream = router.route(&chat("sys", "hello")).candidates[0];
        router.set_drained(upstream, true);
        assert!(router.drained(upstream));
        // Draining must not rewrite health: the replica is still reachable, and
        // `upstream_up` has to keep saying so.
        assert_eq!(router.state(upstream).map(|state| state.3), Some(true));
        router.set_drained(upstream, false);
        assert!(!router.drained(upstream));
    }

    #[test]
    fn drain_mask_is_all_or_nothing() {
        let router = Router::new(config());
        assert!(router.set_drained_mask(&[true, false]));
        assert!(router.drained(0));
        assert!(!router.drained(1));
        assert!(!router.set_drained_mask(&[false]));
        assert!(
            router.drained(0),
            "invalid cardinality must not mutate state"
        );
        assert!(!router.drained(1));
    }

    #[test]
    fn dispatch_reservation_refuses_a_drained_replica() {
        let router = Arc::new(Router::new(config()));
        let upstream = router.route(&chat("sys", "hello")).candidates[0];
        router.set_drained(upstream, true);
        assert!(router.acquire_if_healthy(upstream, 4).is_none());
        assert_eq!(router.inflight(upstream), 0);
        // Resuming restores admission without any health transition.
        router.set_drained(upstream, false);
        let guard = router.acquire_if_healthy(upstream, 4).expect("resumed");
        assert_eq!(router.inflight(upstream), 1);
        drop(guard);
        assert_eq!(router.inflight(upstream), 0);
    }

    #[test]
    fn draining_an_unhealthy_replica_keeps_it_out_after_it_recovers() {
        let router = Arc::new(Router::new(config()));
        let upstream = router.route(&chat("sys", "hello")).candidates[0];
        router.set_healthy(upstream, false);
        router.set_drained(upstream, true);
        // Health recovering alone must not readmit a replica the policy parked.
        router.set_healthy(upstream, true);
        assert!(router.acquire_if_healthy(upstream, 1).is_none());
        router.set_drained(upstream, false);
        assert!(router.acquire_if_healthy(upstream, 1).is_some());
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
    fn fail_open_reservation_ignores_health_but_never_a_drain() {
        let router = Arc::new(Router::new(config()));
        let upstream = router.route(&chat("sys", "hello")).candidates[0];
        router.set_healthy(upstream, false);
        // A starved probe marked this replica down; the fail-open path still has
        // to be able to reserve on it, because shedding the request is worse.
        let guard = router
            .acquire_failing_open(upstream, 2)
            .expect("unhealthy replica is still reservable when failing open");
        assert_eq!(router.inflight(upstream), 1);
        drop(guard);
        // Parking is a deliberate decision by an actor that may stop the
        // container, so it outranks fail-open.
        router.set_drained(upstream, true);
        assert!(router.acquire_failing_open(upstream, 2).is_none());
        assert_eq!(router.inflight(upstream), 0);
    }

    #[test]
    fn reservation_can_shrink_without_releasing_inflight() {
        let router = Arc::new(Router::new(config()));
        let mut guard = router.acquire(0, 4);
        assert_eq!(
            router.state(0).map(|state| (state.0, state.1)),
            Some((1, 4))
        );
        assert!(guard.reduce_to(1));
        assert_eq!(
            router.state(0).map(|state| (state.0, state.1)),
            Some((1, 1))
        );
        assert!(!guard.reduce_to(2));
        assert_eq!(
            router.state(0).map(|state| (state.0, state.1)),
            Some((1, 1))
        );
        drop(guard);
        assert_eq!(
            router.state(0).map(|state| (state.0, state.1)),
            Some((0, 0))
        );
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
    fn fingerprints_include_prompt_rendering_controls() {
        let cases = [
            (
                "chat_template_kwargs",
                json!({"enable_thinking": true}),
                json!({"enable_thinking": false}),
            ),
            ("enable_thinking", json!(true), json!(false)),
            ("preserve_thinking", json!(true), json!(false)),
            (
                "mm_processor_kwargs",
                json!({"max_pixels": 1_048_576}),
                json!({"max_pixels": 2_097_152}),
            ),
            ("add_generation_prompt", json!(true), json!(false)),
            ("continue_final_message", json!(true), json!(false)),
        ];

        for (key, first_value, second_value) in cases {
            let router = Router::new(config());
            let mut first = json!({
                "messages": [{"role": "user", "content": "hello"}],
            });
            first
                .as_object_mut()
                .expect("request is an object")
                .insert(key.to_owned(), first_value);
            let mut second = json!({
                "messages": [{"role": "user", "content": "hello"}],
            });
            second
                .as_object_mut()
                .expect("request is an object")
                .insert(key.to_owned(), second_value);

            let first_body = serde_json::to_vec(&first).unwrap();
            let second_body = serde_json::to_vec(&second).unwrap();
            let first_fingerprints = router.fingerprints(&first_body);
            let second_fingerprints = router.fingerprints(&second_body);
            assert_ne!(
                first_fingerprints, second_fingerprints,
                "{key} must affect the canonical prompt"
            );

            router.observe(0, &first_fingerprints);
            let decision = router.route_prepared(second_body.len(), &second_fingerprints);
            assert!(
                decision.overlap_blocks < decision.total_blocks,
                "a changed {key} must not claim the complete cached prefix"
            );
        }
    }

    #[test]
    fn prompt_rendering_control_object_order_is_canonical() {
        let router = Router::new(config());
        let first = br#"{"messages":[{"role":"user","content":"hello"}],"chat_template_kwargs":{"enable_thinking":true,"nested":{"z":1,"a":2}},"mm_processor_kwargs":{"max_pixels":1048576,"min_pixels":3136},"enable_thinking":true,"preserve_thinking":false,"add_generation_prompt":true,"continue_final_message":false}"#;
        let second = br#"{"continue_final_message":false,"add_generation_prompt":true,"preserve_thinking":false,"enable_thinking":true,"mm_processor_kwargs":{"min_pixels":3136,"max_pixels":1048576},"chat_template_kwargs":{"nested":{"a":2,"z":1},"enable_thinking":true},"messages":[{"content":"hello","role":"user"}]}"#;
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
