//! Long-prompt lane: confine very long prompts to designated replicas.
//!
//! A single multi-hundred-thousand-token prefill evicts every other session's
//! cached prefix on the replica it lands on and stalls that replica's other
//! requests for the duration of its chunked prefill. When a model has more
//! than one replica, pinning such prompts to a designated "lane" replica keeps
//! the remaining replicas' caches and latency intact.
//!
//! The size signal is the request body length in bytes, the same total the
//! router already uses for load units. It is not capped by
//! `RJ_ROUTE_MAX_PREFIX_BYTES` (that cap bounds only the hashed prefix), and it
//! is available for every request whether or not a tokenizer is configured.
//! It includes JSON framing, escaping, and tool schemas, so it over-reads the
//! prompt slightly; for English/JSON text roughly four body bytes correspond
//! to one prompt token.
//!
//! The rule is a restriction, never an expansion: it runs after model and API
//! ownership has already narrowed the decision, and it only ever removes
//! candidates. Availability beats isolation, so a model whose lane members are
//! all unavailable routes exactly as it would without a lane.

use serde::Serialize;

use crate::router::Decision;

/// What the lane rule did to one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LongPromptLaneOutcome {
    /// The feature is not configured.
    Off,
    /// The request is shorter than the threshold and routes as usual.
    Below,
    /// No lane member serves the requested model; the rule does not apply.
    NoLane,
    /// The request was confined to serving lane members.
    Lane,
    /// Every lane member for this model is unavailable, so ordinary routing
    /// applies.
    Fallback,
}

impl LongPromptLaneOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Below => "below",
            Self::NoLane => "no_lane",
            Self::Lane => "lane",
            Self::Fallback => "fallback",
        }
    }

    /// Whether this request was long enough to be governed by a lane, which
    /// is the population the `ramjet_route_long_prompt_total` counter records.
    #[must_use]
    pub const fn counted(self) -> bool {
        matches!(self, Self::Lane | Self::Fallback)
    }
}

/// Privacy-bounded route-journal annotation: a fixed outcome label only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LongPromptLaneObservation {
    pub outcome: &'static str,
}

impl From<LongPromptLaneOutcome> for LongPromptLaneObservation {
    fn from(outcome: LongPromptLaneOutcome) -> Self {
        Self {
            outcome: outcome.label(),
        }
    }
}

/// The lane mask a long prompt should be restricted to.
///
/// `members` is aligned with the configured upstreams; an empty slice or a
/// zero threshold means the feature is off. `candidates` are the upstreams
/// that already own the requested model and API profile, and `serving`
/// reports whether an upstream is currently admitted (healthy, not fenced,
/// not drained). The returned mask is `Some` only for [`LongPromptLaneOutcome::Lane`].
#[must_use]
pub fn select(
    prompt_bytes: usize,
    threshold_bytes: usize,
    members: &[bool],
    candidates: &[usize],
    serving: impl Fn(usize) -> bool,
) -> (LongPromptLaneOutcome, Option<Vec<bool>>) {
    if threshold_bytes == 0 || members.is_empty() {
        return (LongPromptLaneOutcome::Off, None);
    }
    if prompt_bytes < threshold_bytes {
        return (LongPromptLaneOutcome::Below, None);
    }
    let is_member = |upstream: usize| members.get(upstream).copied().unwrap_or(false);
    if !candidates.iter().any(|&upstream| is_member(upstream)) {
        return (LongPromptLaneOutcome::NoLane, None);
    }
    let mut mask = vec![false; members.len()];
    let mut any = false;
    for &upstream in candidates {
        if is_member(upstream) && serving(upstream) {
            mask[upstream] = true;
            any = true;
        }
    }
    if any {
        (LongPromptLaneOutcome::Lane, Some(mask))
    } else {
        (LongPromptLaneOutcome::Fallback, None)
    }
}

/// Applies [`select`] to a model-restricted routing decision in place.
///
/// Eligibility is read from the decision itself: its candidate list is the
/// model/API-eligible set and each candidate's `healthy` flag already folds in
/// probe health, `DSpark` quarantine, durable fences, and idle-drain parking.
pub fn confine(
    decision: &mut Decision,
    prompt_bytes: usize,
    threshold_bytes: usize,
    members: &[bool],
) -> LongPromptLaneOutcome {
    let (outcome, mask) = select(
        prompt_bytes,
        threshold_bytes,
        members,
        &decision.candidates,
        |upstream| {
            decision
                .candidate_state
                .iter()
                .any(|state| state.index == upstream && state.healthy)
        },
    );
    match mask {
        Some(mask) if decision.restrict_to(&mask) => outcome,
        Some(_) => LongPromptLaneOutcome::Fallback,
        None => outcome,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use url::Url;

    use super::*;
    use crate::{
        affinity_horizon::AffinityHorizonConfig,
        config::{Affinity, SpeculationProfile, SpeculationRouteMode},
        router::{Outcome, Router, RouterConfig},
    };

    const THRESHOLD: usize = 600_000;

    /// The live node06 shape: qwen, two GLM replicas, and a System One
    /// upstream, with the lane on the second GLM replica only.
    const LIVE_MODELS: [&str; 4] = [
        "qwen3.8-flash-next",
        "glm-5.3-flash",
        "glm-5.3-flash",
        "kev-latest",
    ];
    const LIVE_LANE: [bool; 4] = [false, false, true, false];

    fn router(upstreams: usize) -> Arc<Router> {
        Arc::new(Router::new(RouterConfig {
            upstreams: (0..upstreams)
                .map(|index| Url::parse(&format!("http://engine-{index}:8000")).unwrap())
                .collect(),
            alpha: 4.0,
            chunk_bytes: 2_048,
            max_prefix_bytes: 2 << 20,
            max_overlap_blocks: 32,
            index_capacity: 1_024,
            load_unit_bytes: 32 << 10,
            max_load_units: 8,
            projected_load: false,
            speculation_mode: SpeculationRouteMode::Off,
            speculation_profiles: vec![SpeculationProfile::Standard; upstreams],
            affinity: Affinity::Prefix,
            affinity_horizon: AffinityHorizonConfig::off(),
        }))
    }

    /// Routes like the proxy: score every upstream, then restrict to the
    /// replicas owning `model`.
    fn model_decision(router: &Router, model: &str, body_bytes: usize) -> Decision {
        let mut decision = router.route_prepared(body_bytes, &[]);
        let eligible = LIVE_MODELS
            .iter()
            .map(|owned| *owned == model)
            .collect::<Vec<_>>();
        assert!(decision.restrict_to(&eligible));
        decision
    }

    #[test]
    fn off_and_below_threshold_leave_the_decision_untouched() {
        let router = router(4);
        for (threshold, members, bytes, expected) in [
            (
                0,
                LIVE_LANE.as_slice(),
                10_000_000,
                LongPromptLaneOutcome::Off,
            ),
            (THRESHOLD, &[], 10_000_000, LongPromptLaneOutcome::Off),
            (
                THRESHOLD,
                LIVE_LANE.as_slice(),
                THRESHOLD - 1,
                LongPromptLaneOutcome::Below,
            ),
        ] {
            let mut decision = model_decision(&router, "glm-5.3-flash", bytes);
            let before = decision.clone();
            assert_eq!(confine(&mut decision, bytes, threshold, members), expected);
            assert_eq!(decision, before);
        }
    }

    #[test]
    fn a_long_prompt_is_confined_to_the_serving_lane_member() {
        let router = router(4);
        // Rotate through both GLM orders so the lane wins regardless of the
        // replica ordinary routing would have chosen.
        let mut firsts = std::collections::HashSet::new();
        for _ in 0..4 {
            let mut decision = model_decision(&router, "glm-5.3-flash", THRESHOLD);
            firsts.insert(decision.candidates[0]);
            assert_eq!(
                confine(&mut decision, THRESHOLD, THRESHOLD, &LIVE_LANE),
                LongPromptLaneOutcome::Lane
            );
            assert_eq!(decision.candidates, [2]);
            assert_eq!(decision.outcome, Outcome::Single);
            let serving = decision
                .candidate_state
                .iter()
                .filter(|state| state.healthy)
                .map(|state| state.index)
                .collect::<Vec<_>>();
            assert_eq!(serving, [2]);
            // Full cardinality is retained for diagnostics.
            assert_eq!(decision.candidate_state.len(), 4);
        }
        assert_eq!(
            firsts,
            [1, 2].into(),
            "ordinary routing picks either replica"
        );
    }

    #[test]
    fn an_unavailable_lane_falls_back_to_ordinary_routing() {
        for fence in [
            |router: &Router| router.set_healthy(2, false),
            |router: &Router| router.set_drained(2, true),
        ] {
            let router = router(4);
            fence(&router);
            let mut decision = model_decision(&router, "glm-5.3-flash", THRESHOLD);
            let before = decision.clone();
            assert_eq!(
                confine(&mut decision, THRESHOLD * 2, THRESHOLD, &LIVE_LANE),
                LongPromptLaneOutcome::Fallback
            );
            assert_eq!(decision, before);
            assert_eq!(decision.candidates[0], 1, "the protected replica serves");
        }
    }

    #[test]
    fn models_without_a_lane_member_are_unaffected() {
        let router = router(4);
        for model in ["qwen3.8-flash-next", "kev-latest"] {
            let mut decision = model_decision(&router, model, THRESHOLD * 4);
            let before = decision.clone();
            assert_eq!(
                confine(&mut decision, THRESHOLD * 4, THRESHOLD, &LIVE_LANE),
                LongPromptLaneOutcome::NoLane
            );
            assert_eq!(decision, before);
        }
    }

    #[test]
    fn a_lane_covering_every_replica_keeps_them_all() {
        let (outcome, mask) = select(THRESHOLD, THRESHOLD, &[true, true], &[1, 0], |_| true);
        assert_eq!(outcome, LongPromptLaneOutcome::Lane);
        assert_eq!(mask, Some(vec![true, true]));
        // Only serving members enter the mask.
        let (outcome, mask) = select(THRESHOLD, THRESHOLD, &[true, true], &[1, 0], |upstream| {
            upstream == 0
        });
        assert_eq!(outcome, LongPromptLaneOutcome::Lane);
        assert_eq!(mask, Some(vec![true, false]));
    }

    #[test]
    fn lane_members_outside_the_candidate_set_never_widen_it() {
        // Upstream 2 is a lane member but does not own the requested model.
        let (outcome, mask) = select(THRESHOLD, THRESHOLD, &[false, false, true], &[0, 1], |_| {
            true
        });
        assert_eq!(outcome, LongPromptLaneOutcome::NoLane);
        assert_eq!(mask, None);
    }

    #[test]
    fn only_lane_and_fallback_are_counted() {
        for (outcome, counted, label) in [
            (LongPromptLaneOutcome::Off, false, "off"),
            (LongPromptLaneOutcome::Below, false, "below"),
            (LongPromptLaneOutcome::NoLane, false, "no_lane"),
            (LongPromptLaneOutcome::Lane, true, "lane"),
            (LongPromptLaneOutcome::Fallback, true, "fallback"),
        ] {
            assert_eq!(outcome.counted(), counted);
            assert_eq!(outcome.label(), label);
            assert_eq!(LongPromptLaneObservation::from(outcome).outcome, label);
        }
    }
}
