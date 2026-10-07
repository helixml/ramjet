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
//!
//! Prompts below the threshold may share the lane (`shared`, the default) or
//! stay off it while a protected replica serves (`exclusive`). Keeping short
//! prompts off is for a lane replica that is slower at ordinary work, such as
//! one serving a far longer context window. The same availability rule applies
//! in reverse: a short prompt uses the lane when none of its model's other
//! replicas is serving, and an excluded short prompt may still fail over to a
//! serving lane member after its protected replicas fail.

use serde::Serialize;

use crate::{config::LongPromptLaneSharing, router::Decision};

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
    /// An exclusive lane kept a prompt below the threshold off its members.
    Excluded,
    /// An exclusive lane, but none of the model's non-lane replicas is
    /// serving, so the short prompt routes as usual and may use the lane.
    ExcludedFallback,
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
            Self::Excluded => "excluded",
            Self::ExcludedFallback => "excluded_fallback",
        }
    }

    /// Whether this request was long enough to be governed by a lane, which
    /// is the population the `ramjet_route_long_prompt_total` counter records.
    #[must_use]
    pub const fn counted(self) -> bool {
        matches!(self, Self::Lane | Self::Fallback)
    }

    /// Whether this request was a short prompt governed by an exclusive lane,
    /// the population `ramjet_route_long_prompt_short_total` records.
    #[must_use]
    pub const fn short_counted(self) -> bool {
        matches!(self, Self::Excluded | Self::ExcludedFallback)
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

/// The mask a prompt should be restricted to.
///
/// `members` is aligned with the configured upstreams; an empty slice or a
/// zero threshold means the feature is off. `sharing` says whether prompts
/// below the threshold may use lane members. `candidates` are the upstreams
/// that already own the requested model and API profile, and `serving` reports
/// whether an upstream is currently admitted (healthy, not fenced, not
/// drained). The returned mask is `Some` only for
/// [`LongPromptLaneOutcome::Lane`] and [`LongPromptLaneOutcome::Excluded`].
#[must_use]
pub fn select(
    prompt_bytes: usize,
    threshold_bytes: usize,
    members: &[bool],
    sharing: LongPromptLaneSharing,
    candidates: &[usize],
    serving: impl Fn(usize) -> bool,
) -> (LongPromptLaneOutcome, Option<Vec<bool>>) {
    if threshold_bytes == 0 || members.is_empty() {
        return (LongPromptLaneOutcome::Off, None);
    }
    let is_member = |upstream: usize| members.get(upstream).copied().unwrap_or(false);
    if prompt_bytes < threshold_bytes {
        // Only a model with both lane and non-lane replicas has a lane to
        // keep short prompts off.
        if sharing == LongPromptLaneSharing::Shared
            || !candidates.iter().any(|&upstream| is_member(upstream))
            || candidates.iter().all(|&upstream| is_member(upstream))
        {
            return (LongPromptLaneOutcome::Below, None);
        }
        let mut mask = vec![false; members.len()];
        let mut any = false;
        for &upstream in candidates {
            if !is_member(upstream) && serving(upstream) {
                mask[upstream] = true;
                any = true;
            }
        }
        return if any {
            (LongPromptLaneOutcome::Excluded, Some(mask))
        } else {
            (LongPromptLaneOutcome::ExcludedFallback, None)
        };
    }
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
///
/// Also returns the serving lane members an excluded short request may fail
/// over to once its protected replicas fail: exclusion protects latency, not
/// availability. A long prompt confined to the lane gets none; its retries
/// stay on the lane, as before.
pub fn confine_with_failover(
    decision: &mut Decision,
    prompt_bytes: usize,
    threshold_bytes: usize,
    members: &[bool],
    sharing: LongPromptLaneSharing,
) -> (LongPromptLaneOutcome, Vec<usize>) {
    let serving = |decision: &Decision, upstream: usize| {
        decision
            .candidate_state
            .iter()
            .any(|state| state.index == upstream && state.healthy)
    };
    let (outcome, mask) = select(
        prompt_bytes,
        threshold_bytes,
        members,
        sharing,
        &decision.candidates,
        |upstream| serving(decision, upstream),
    );
    let Some(mask) = mask else {
        return (outcome, Vec::new());
    };
    let failover = if outcome == LongPromptLaneOutcome::Excluded {
        decision
            .candidates
            .iter()
            .copied()
            .filter(|&upstream| !mask[upstream] && serving(decision, upstream))
            .collect()
    } else {
        Vec::new()
    };
    if decision.restrict_to(&mask) {
        return (outcome, failover);
    }
    // Defensive: the mask always holds a serving candidate of full length, so
    // restriction does not fail in practice.
    if outcome == LongPromptLaneOutcome::Excluded {
        (LongPromptLaneOutcome::ExcludedFallback, Vec::new())
    } else {
        (LongPromptLaneOutcome::Fallback, Vec::new())
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
    const SHARED: LongPromptLaneSharing = LongPromptLaneSharing::Shared;
    const EXCLUSIVE: LongPromptLaneSharing = LongPromptLaneSharing::Exclusive;

    fn confine(
        decision: &mut Decision,
        prompt_bytes: usize,
        threshold_bytes: usize,
        members: &[bool],
        sharing: LongPromptLaneSharing,
    ) -> LongPromptLaneOutcome {
        confine_with_failover(decision, prompt_bytes, threshold_bytes, members, sharing).0
    }

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
            affinity_basis: crate::config::AffinityBasis::Absolute,
            affinity_groups: Vec::new(),
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
            assert_eq!(
                confine(&mut decision, bytes, threshold, members, SHARED),
                expected
            );
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
                confine(&mut decision, THRESHOLD, THRESHOLD, &LIVE_LANE, SHARED),
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
                confine(&mut decision, THRESHOLD * 2, THRESHOLD, &LIVE_LANE, SHARED),
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
                confine(&mut decision, THRESHOLD * 4, THRESHOLD, &LIVE_LANE, SHARED),
                LongPromptLaneOutcome::NoLane
            );
            assert_eq!(decision, before);
        }
    }

    #[test]
    fn a_lane_covering_every_replica_keeps_them_all() {
        let (outcome, mask) = select(THRESHOLD, THRESHOLD, &[true, true], SHARED, &[1, 0], |_| {
            true
        });
        assert_eq!(outcome, LongPromptLaneOutcome::Lane);
        assert_eq!(mask, Some(vec![true, true]));
        // Only serving members enter the mask.
        let (outcome, mask) = select(
            THRESHOLD,
            THRESHOLD,
            &[true, true],
            SHARED,
            &[1, 0],
            |upstream| upstream == 0,
        );
        assert_eq!(outcome, LongPromptLaneOutcome::Lane);
        assert_eq!(mask, Some(vec![true, false]));
    }

    #[test]
    fn lane_members_outside_the_candidate_set_never_widen_it() {
        // Upstream 2 is a lane member but does not own the requested model.
        let (outcome, mask) = select(
            THRESHOLD,
            THRESHOLD,
            &[false, false, true],
            SHARED,
            &[0, 1],
            |_| true,
        );
        assert_eq!(outcome, LongPromptLaneOutcome::NoLane);
        assert_eq!(mask, None);
    }

    #[test]
    fn an_exclusive_lane_keeps_short_prompts_on_the_protected_replica() {
        let router = router(4);
        let mut firsts = std::collections::HashSet::new();
        for _ in 0..4 {
            let mut decision = model_decision(&router, "glm-5.3-flash", 1_000);
            firsts.insert(decision.candidates[0]);
            assert_eq!(
                confine(&mut decision, 1_000, THRESHOLD, &LIVE_LANE, EXCLUSIVE),
                LongPromptLaneOutcome::Excluded
            );
            assert_eq!(decision.candidates, [1]);
            assert_eq!(decision.outcome, Outcome::Single);
            assert_eq!(decision.candidate_state.len(), 4);
        }
        assert_eq!(
            firsts,
            [1, 2].into(),
            "ordinary routing picks either replica"
        );
        // Long prompts still go to the lane.
        let mut decision = model_decision(&router, "glm-5.3-flash", THRESHOLD);
        assert_eq!(
            confine(&mut decision, THRESHOLD, THRESHOLD, &LIVE_LANE, EXCLUSIVE),
            LongPromptLaneOutcome::Lane
        );
        assert_eq!(decision.candidates, [2]);
    }

    #[test]
    fn an_exclusive_lane_serves_short_prompts_when_nothing_else_can() {
        for fence in [
            |router: &Router| router.set_healthy(1, false),
            |router: &Router| router.set_drained(1, true),
        ] {
            let router = router(4);
            fence(&router);
            let mut decision = model_decision(&router, "glm-5.3-flash", 1_000);
            let before = decision.clone();
            assert_eq!(
                confine(&mut decision, 1_000, THRESHOLD, &LIVE_LANE, EXCLUSIVE),
                LongPromptLaneOutcome::ExcludedFallback
            );
            assert_eq!(decision, before);
            assert_eq!(decision.candidates[0], 2, "the lane serves");
        }
    }

    #[test]
    fn an_exclusive_lane_leaves_other_models_and_all_lane_fleets_alone() {
        let router = router(4);
        for model in ["qwen3.8-flash-next", "kev-latest"] {
            let mut decision = model_decision(&router, model, 1_000);
            let before = decision.clone();
            assert_eq!(
                confine(&mut decision, 1_000, THRESHOLD, &LIVE_LANE, EXCLUSIVE),
                LongPromptLaneOutcome::Below
            );
            assert_eq!(decision, before);
        }
        // Every replica is a lane member: there is nothing to keep short
        // prompts on, so they route as usual.
        let (outcome, mask) = select(1_000, THRESHOLD, &[true, true], EXCLUSIVE, &[1, 0], |_| {
            true
        });
        assert_eq!(outcome, LongPromptLaneOutcome::Below);
        assert_eq!(mask, None);
        // Exclusion is inert when the lane itself is off.
        let (outcome, mask) = select(1_000, 0, &LIVE_LANE, EXCLUSIVE, &[1, 2], |_| true);
        assert_eq!(outcome, LongPromptLaneOutcome::Off);
        assert_eq!(mask, None);
    }

    #[test]
    fn an_excluded_short_prompt_may_fail_over_to_the_lane() {
        let router = router(4);
        let mut decision = model_decision(&router, "glm-5.3-flash", 1_000);
        let (outcome, failover) =
            confine_with_failover(&mut decision, 1_000, THRESHOLD, &LIVE_LANE, EXCLUSIVE);
        assert_eq!(outcome, LongPromptLaneOutcome::Excluded);
        assert_eq!(
            decision.candidates,
            [1],
            "the first attempt stays protected"
        );
        assert_eq!(
            failover,
            [2],
            "the serving lane member is the failover tail"
        );

        // A lane member that is not serving is no failover target.
        router.set_healthy(2, false);
        let mut decision = model_decision(&router, "glm-5.3-flash", 1_000);
        let (_, failover) =
            confine_with_failover(&mut decision, 1_000, THRESHOLD, &LIVE_LANE, EXCLUSIVE);
        assert!(failover.is_empty());
        router.set_healthy(2, true);

        // Long prompts keep their retries on the lane, and shared adds nothing.
        for (bytes, sharing) in [(THRESHOLD, EXCLUSIVE), (1_000, SHARED)] {
            let mut decision = model_decision(&router, "glm-5.3-flash", bytes);
            let (_, failover) =
                confine_with_failover(&mut decision, bytes, THRESHOLD, &LIVE_LANE, sharing);
            assert!(failover.is_empty(), "{bytes} {sharing:?}");
        }
    }

    #[test]
    fn each_outcome_is_counted_in_exactly_one_series() {
        for (outcome, long, short, label) in [
            (LongPromptLaneOutcome::Off, false, false, "off"),
            (LongPromptLaneOutcome::Below, false, false, "below"),
            (LongPromptLaneOutcome::NoLane, false, false, "no_lane"),
            (LongPromptLaneOutcome::Lane, true, false, "lane"),
            (LongPromptLaneOutcome::Fallback, true, false, "fallback"),
            (LongPromptLaneOutcome::Excluded, false, true, "excluded"),
            (
                LongPromptLaneOutcome::ExcludedFallback,
                false,
                true,
                "excluded_fallback",
            ),
        ] {
            assert_eq!(outcome.counted(), long, "{label}");
            assert_eq!(outcome.short_counted(), short, "{label}");
            assert_eq!(outcome.label(), label);
            assert_eq!(LongPromptLaneObservation::from(outcome).outcome, label);
        }
    }
}
