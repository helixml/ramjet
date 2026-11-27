//! Privacy-bounded session affinity shadow evaluation.
//!
//! A caller-provided opaque session ID is mapped to a deterministic primary
//! and secondary with keyed rendezvous hashing. Shadow mode evaluates a small
//! cache-equivalent bonus under an independent load-delta gate, but never
//! mutates the route. Raw session IDs and keyed scores are neither retained nor
//! exposed through metrics or journal records.

use std::sync::Arc;

use serde::Serialize;

use crate::{
    config::{Config, SessionAffinityMode},
    metrics::Metrics,
    router::{CandidateState, Decision},
    session::{OpaqueSession, hmac_sha256},
    shims::Endpoint,
};

const SESSION_AFFINITY_DOMAIN: &[u8] = b"mini-dynamo session affinity rendezvous v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionAffinityOutcome {
    MissingSession,
    InvalidSession,
    ApproximateAlreadyPrimary,
    ApproximateAlreadySecondaryHealth,
    ApproximateAlreadySecondaryLoad,
    WouldPreferPrimary,
    WouldPreferSecondaryHealth,
    WouldPreferSecondaryLoad,
    KeptPairLoad,
    KeptScore,
    NoHealthyAssigned,
    NoHealthyUpstream,
    InvalidDecision,
}

impl SessionAffinityOutcome {
    pub(crate) const ALL: [Self; 13] = [
        Self::MissingSession,
        Self::InvalidSession,
        Self::ApproximateAlreadyPrimary,
        Self::ApproximateAlreadySecondaryHealth,
        Self::ApproximateAlreadySecondaryLoad,
        Self::WouldPreferPrimary,
        Self::WouldPreferSecondaryHealth,
        Self::WouldPreferSecondaryLoad,
        Self::KeptPairLoad,
        Self::KeptScore,
        Self::NoHealthyAssigned,
        Self::NoHealthyUpstream,
        Self::InvalidDecision,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::MissingSession => "missing_session",
            Self::InvalidSession => "invalid_session",
            Self::ApproximateAlreadyPrimary => "approximate_already_primary",
            Self::ApproximateAlreadySecondaryHealth => {
                "approximate_already_secondary_primary_unhealthy"
            }
            Self::ApproximateAlreadySecondaryLoad => {
                "approximate_already_secondary_primary_load_gated"
            }
            Self::WouldPreferPrimary => "would_prefer_primary",
            Self::WouldPreferSecondaryHealth => "would_prefer_secondary_primary_unhealthy",
            Self::WouldPreferSecondaryLoad => "would_prefer_secondary_primary_load_gated",
            Self::KeptPairLoad => "kept_assigned_pair_load_gated",
            Self::KeptScore => "kept_score",
            Self::NoHealthyAssigned => "no_healthy_assigned_pair",
            Self::NoHealthyUpstream => "no_healthy_upstream",
            Self::InvalidDecision => "invalid_decision",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SessionAffinityObservation {
    pub(crate) policy_version: u8,
    pub(crate) bonus_blocks: usize,
    pub(crate) max_load_delta: usize,
    pub(crate) outcome: SessionAffinityOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) primary: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) secondary: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<usize>,
}

pub(crate) struct SessionAffinity {
    mode: SessionAffinityMode,
    key: Option<Arc<[u8]>>,
    bonus_blocks: usize,
    max_load_delta: usize,
    alpha: f64,
    projected_load: bool,
    upstream_count: usize,
    metrics: Arc<Metrics>,
}

impl SessionAffinity {
    #[must_use]
    pub(crate) fn new(config: &Config, metrics: Arc<Metrics>) -> Self {
        Self {
            mode: config.session_affinity_mode,
            key: config
                .session_affinity_key
                .as_ref()
                .map(|key| Arc::from(key.as_bytes())),
            bonus_blocks: config.session_affinity_bonus_blocks,
            max_load_delta: config.session_affinity_max_load_delta,
            alpha: config.route_alpha,
            projected_load: config.route_projected_load,
            upstream_count: config.upstreams.len(),
            metrics,
        }
    }

    /// Evaluate the proposed affinity target without changing candidate order.
    #[must_use]
    pub(crate) fn observe(
        &self,
        endpoint: Endpoint,
        session: OpaqueSession<'_>,
        decision: &Decision,
    ) -> Option<SessionAffinityObservation> {
        if self.mode == SessionAffinityMode::Off || !endpoint.is_generation() {
            return None;
        }
        let observation = match session {
            OpaqueSession::Missing => {
                self.observation(SessionAffinityOutcome::MissingSession, None)
            }
            OpaqueSession::Invalid => {
                self.observation(SessionAffinityOutcome::InvalidSession, None)
            }
            OpaqueSession::Valid(session_id) => self.evaluate(session_id, decision),
        };
        self.metrics
            .session_affinity
            .with_label_values(&[endpoint.label(), observation.outcome.label()])
            .inc();
        Some(observation)
    }

    fn evaluate(&self, session_id: &[u8], decision: &Decision) -> SessionAffinityObservation {
        let Some(key) = self.key.as_deref() else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let Some(states) = validated_states(decision, self.upstream_count) else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        // The pair is drawn from the replicas this request may use (its model,
        // API profile, and lane), so a restricted request is never assigned
        // to a replica that cannot serve it. Rendezvous hashing keeps the
        // assignment stable as long as the assigned replicas stay eligible.
        let Some((primary, secondary)) = rendezvous_pair(session_id, key, &decision.candidates)
        else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let selected = decision.candidates[0];
        let Some(selected_state) = states[selected] else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let eligible = || {
            decision
                .candidates
                .iter()
                .filter_map(|candidate| states[*candidate])
        };
        let Some(min_load) = eligible()
            .filter(|state| state.healthy)
            .map(|state| self.load(state))
            .min()
        else {
            return self.assigned_observation(
                SessionAffinityOutcome::NoHealthyUpstream,
                primary,
                secondary,
                None,
            );
        };
        let admitted_load = min_load.saturating_add(self.max_load_delta);
        let primary_state = states[primary].expect("candidates are validated");
        let secondary_state = states[secondary].expect("candidates are validated");
        let admitted = |state: &CandidateState| state.healthy && self.load(state) <= admitted_load;
        let target = if admitted(primary_state) {
            Some((primary, TargetKind::Primary))
        } else if secondary != primary && admitted(secondary_state) {
            Some((
                secondary,
                if primary_state.healthy {
                    TargetKind::SecondaryPrimaryLoadGated
                } else {
                    TargetKind::SecondaryPrimaryUnhealthy
                },
            ))
        } else {
            None
        };
        let Some((target, target_kind)) = target else {
            let outcome = if !primary_state.healthy && !secondary_state.healthy {
                SessionAffinityOutcome::NoHealthyAssigned
            } else {
                SessionAffinityOutcome::KeptPairLoad
            };
            return self.assigned_observation(outcome, primary, secondary, None);
        };
        if selected == target {
            return self.assigned_observation(
                target_kind.already_outcome(),
                primary,
                secondary,
                Some(target),
            );
        }
        let target_state = states[target].expect("candidates are validated");
        if !target_wins_with_bonus(
            target_state,
            selected_state,
            self.alpha,
            self.projected_load,
            self.bonus_blocks,
            decision.rotation,
            self.upstream_count,
        ) {
            return self.assigned_observation(
                SessionAffinityOutcome::KeptScore,
                primary,
                secondary,
                Some(target),
            );
        }
        self.assigned_observation(
            target_kind.prefer_outcome(),
            primary,
            secondary,
            Some(target),
        )
    }

    /// Load as Router scored it: projected load also counts this request's
    /// own reservation beyond its first unit.
    fn load(&self, state: &CandidateState) -> usize {
        projected_load_units(state, self.projected_load)
    }

    fn observation(
        &self,
        outcome: SessionAffinityOutcome,
        target: Option<usize>,
    ) -> SessionAffinityObservation {
        SessionAffinityObservation {
            policy_version: 1,
            bonus_blocks: self.bonus_blocks,
            max_load_delta: self.max_load_delta,
            outcome,
            primary: None,
            secondary: None,
            target,
        }
    }

    fn assigned_observation(
        &self,
        outcome: SessionAffinityOutcome,
        primary: usize,
        secondary: usize,
        target: Option<usize>,
    ) -> SessionAffinityObservation {
        SessionAffinityObservation {
            policy_version: 1,
            bonus_blocks: self.bonus_blocks,
            max_load_delta: self.max_load_delta,
            outcome,
            primary: Some(primary),
            secondary: Some(secondary),
            target,
        }
    }
}

#[derive(Clone, Copy)]
enum TargetKind {
    Primary,
    SecondaryPrimaryUnhealthy,
    SecondaryPrimaryLoadGated,
}

impl TargetKind {
    const fn already_outcome(self) -> SessionAffinityOutcome {
        match self {
            Self::Primary => SessionAffinityOutcome::ApproximateAlreadyPrimary,
            Self::SecondaryPrimaryUnhealthy => {
                SessionAffinityOutcome::ApproximateAlreadySecondaryHealth
            }
            Self::SecondaryPrimaryLoadGated => {
                SessionAffinityOutcome::ApproximateAlreadySecondaryLoad
            }
        }
    }

    const fn prefer_outcome(self) -> SessionAffinityOutcome {
        match self {
            Self::Primary => SessionAffinityOutcome::WouldPreferPrimary,
            Self::SecondaryPrimaryUnhealthy => SessionAffinityOutcome::WouldPreferSecondaryHealth,
            Self::SecondaryPrimaryLoadGated => SessionAffinityOutcome::WouldPreferSecondaryLoad,
        }
    }
}

/// Indexes candidate state by upstream for the replicas this request may use.
///
/// The proxy restricts `candidates` to the request's model, API profile, and
/// lane before affinity runs, so the dispatch order is usually a strict subset
/// of the fleet. Candidate state must name each upstream at most once and in
/// range, and every dispatch candidate must have state; neither needs to cover
/// the whole fleet.
fn validated_states(
    decision: &Decision,
    upstream_count: usize,
) -> Option<Vec<Option<&CandidateState>>> {
    if decision.candidates.is_empty() || decision.candidate_state.len() > upstream_count {
        return None;
    }
    let mut states = vec![None; upstream_count];
    for state in &decision.candidate_state {
        if state.index >= upstream_count || states[state.index].replace(state).is_some() {
            return None;
        }
    }
    let mut seen = vec![false; upstream_count];
    for candidate in &decision.candidates {
        if *candidate >= upstream_count
            || states[*candidate].is_none()
            || std::mem::replace(&mut seen[*candidate], true)
        {
            return None;
        }
    }
    Some(states)
}

/// Highest-random-weight primary and secondary among `candidates`. Each
/// upstream's weight depends only on its configured ordinal, so restricting
/// the eligible set never reshuffles the replicas that remain. A single
/// eligible replica is both primary and secondary.
fn rendezvous_pair(session_id: &[u8], key: &[u8], candidates: &[usize]) -> Option<(usize, usize)> {
    if candidates.is_empty() || session_id.is_empty() || key.is_empty() {
        return None;
    }
    let mut ranked = candidates
        .iter()
        .map(|&index| {
            let ordinal = u64::try_from(index)
                .expect("upstream ordinal fits u64")
                .to_be_bytes();
            let digest = hmac_sha256(key, &[SESSION_AFFINITY_DOMAIN, session_id, &ordinal]);
            let score = u128::from_be_bytes(digest[..16].try_into().expect("SHA-256 has 16 bytes"));
            (score, index)
        })
        .collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let primary = ranked[0].1;
    Some((primary, ranked.get(1).map_or(primary, |second| second.1)))
}

/// Mirrors `Router`'s projected load: with projection on, a request's own
/// reservation beyond its first unit counts against the replica.
fn projected_load_units(candidate: &CandidateState, projected_load: bool) -> usize {
    let additional = if projected_load {
        candidate.request_load_units.saturating_sub(1)
    } else {
        0
    };
    candidate.load_units.saturating_add(additional)
}

fn base_score(candidate: &CandidateState, alpha: f64, projected_load: bool) -> f64 {
    usize_to_f64(candidate.affinity_blocks)
        - alpha * usize_to_f64(projected_load_units(candidate, projected_load))
}

#[allow(clippy::float_cmp)] // Exact equality is required for Router comparator parity.
fn target_wins_with_bonus(
    target: &CandidateState,
    selected: &CandidateState,
    alpha: f64,
    projected_load: bool,
    bonus_blocks: usize,
    rotation: usize,
    candidate_count: usize,
) -> bool {
    if target.healthy != selected.healthy {
        return target.healthy;
    }
    let target_score = base_score(target, alpha, projected_load) + usize_to_f64(bonus_blocks);
    let selected_score = base_score(selected, alpha, projected_load);
    if target_score != selected_score {
        return target_score > selected_score;
    }
    if target.overlap_blocks != selected.overlap_blocks {
        return target.overlap_blocks > selected.overlap_blocks;
    }
    (target.index + rotation) % candidate_count < (selected.index + rotation) % candidate_count
}

#[allow(clippy::cast_precision_loss)]
fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        config::{Affinity, Config},
        router::{CandidateState, Outcome},
    };
    use prometheus::Registry;

    fn config() -> Config {
        let values = HashMap::from([
            ("RJ_UPSTREAM", "http://a:8000,http://b:8000,http://c:8000"),
            ("RJ_SESSION_AFFINITY_MODE", "shadow"),
            (
                "RJ_SESSION_AFFINITY_KEY",
                "0123456789abcdef0123456789abcdef",
            ),
        ]);
        let mut config =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        config.affinity = Affinity::Prefix;
        config
    }

    fn candidate(index: usize, rank: usize, affinity: usize, load: usize) -> CandidateState {
        CandidateState {
            index,
            rank,
            overlap_blocks: affinity,
            affinity_blocks: affinity,
            load_units: load,
            request_load_units: 1,
            healthy: true,
            stale_blocks: 0,
            horizon_ms: None,
            overlap_ages_ms: Vec::new(),
        }
    }

    fn decision() -> Decision {
        Decision {
            candidates: vec![0, 1, 2],
            candidate_state: vec![
                candidate(0, 0, 0, 0),
                candidate(1, 1, 0, 0),
                candidate(2, 2, 0, 0),
            ],
            overlap_blocks: 0,
            total_blocks: 8,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::RoundRobin,
        }
    }

    fn evaluator(config: &Config) -> SessionAffinity {
        SessionAffinity::new(config, Arc::new(Metrics::new(&Registry::new()).unwrap()))
    }

    #[test]
    fn rendezvous_pair_is_stable_distinct_keyed_and_golden() {
        let key = b"0123456789abcdef0123456789abcdef";
        let pair = rendezvous_pair(b"session-a", key, &[0, 1, 2]).unwrap();
        // Independently verified with OpenSSL HMAC-SHA256. The first 128 bits
        // rank ordinals 1, 2, 0 in descending order.
        assert_eq!(pair, (1, 2));
        assert_ne!(pair.0, pair.1);
        for _ in 0..20 {
            assert_eq!(rendezvous_pair(b"session-a", key, &[0, 1, 2]), Some(pair));
        }
        assert_ne!(
            rendezvous_pair(b"session-a", key, &[0, 1, 2]),
            rendezvous_pair(
                b"session-a",
                b"fedcba9876543210fedcba9876543210",
                &[0, 1, 2]
            )
        );
    }

    #[test]
    fn rendezvous_primaries_are_balanced_and_topology_order_is_explicit() {
        let key = b"0123456789abcdef0123456789abcdef";
        let mut primary_counts = [0_usize; 3];
        for ordinal in 0..12_000 {
            let session = format!("session-{ordinal}");
            let (primary, secondary) =
                rendezvous_pair(session.as_bytes(), key, &[0, 1, 2]).unwrap();
            assert_ne!(primary, secondary);
            primary_counts[primary] += 1;
        }
        for count in primary_counts {
            assert!(
                (3_600..=4_400).contains(&count),
                "biased primary count: {count}"
            );
        }

        // The ordinal is deliberately part of the identity. Reordering the
        // configured upstream list or rotating the key starts a new mapping.
        assert_ne!(
            rendezvous_pair(b"session-a", key, &[0, 1, 2]),
            rendezvous_pair(b"session-a", key, &[0, 1, 2, 3])
        );
    }

    #[test]
    fn shadow_is_off_for_other_and_classifies_missing_invalid() {
        let config = config();
        let evaluator = evaluator(&config);
        assert_eq!(
            evaluator.observe(
                Endpoint::Other,
                OpaqueSession::Valid(b"session"),
                &decision()
            ),
            None
        );
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Missing, &decision())
                .unwrap()
                .outcome,
            SessionAffinityOutcome::MissingSession
        );
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Invalid, &decision())
                .unwrap()
                .outcome,
            SessionAffinityOutcome::InvalidSession
        );
    }

    #[test]
    fn off_mode_observes_nothing_and_shadow_never_mutates_the_route() {
        let mut disabled_config = config();
        disabled_config.session_affinity_mode = SessionAffinityMode::Off;
        disabled_config.session_affinity_key = None;
        assert_eq!(
            evaluator(&disabled_config).observe(
                Endpoint::Chat,
                OpaqueSession::Valid(b"session"),
                &decision(),
            ),
            None
        );

        let config = config();
        let route = decision();
        let original = route.clone();
        let result =
            evaluator(&config).observe(Endpoint::Chat, OpaqueSession::Valid(b"session"), &route);
        assert!(result.is_some());
        assert_eq!(route, original);
    }

    #[test]
    fn primary_secondary_shadow_classifies_health_fallback() {
        let config = config();
        let evaluator = evaluator(&config);
        let session = b"session-a";
        let (primary, secondary) = rendezvous_pair(
            session,
            config.session_affinity_key.as_ref().unwrap().as_bytes(),
            &[0, 1, 2],
        )
        .unwrap();
        let mut route = decision();
        route.candidates.retain(|candidate| *candidate != primary);
        route.candidates.insert(0, primary);
        let observed = evaluator
            .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
            .unwrap();
        assert_eq!(
            observed.outcome,
            SessionAffinityOutcome::ApproximateAlreadyPrimary
        );
        assert_eq!(
            (observed.primary, observed.secondary, observed.target),
            (Some(primary), Some(secondary), Some(primary))
        );

        route.candidates.retain(|candidate| *candidate != secondary);
        route.candidates.insert(0, secondary);
        route.candidate_state[primary].healthy = false;
        let observed = evaluator
            .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
            .unwrap();
        assert_eq!(
            observed.outcome,
            SessionAffinityOutcome::ApproximateAlreadySecondaryHealth
        );
        assert_eq!(observed.target, Some(secondary));
    }

    #[test]
    fn bonus_never_overrides_load_or_stronger_cache_score() {
        let mut config = config();
        config.session_affinity_bonus_blocks = 4;
        config.session_affinity_max_load_delta = 0;
        let evaluator = evaluator(&config);
        let session = b"session-a";
        let (primary, secondary) = rendezvous_pair(
            session,
            config.session_affinity_key.as_ref().unwrap().as_bytes(),
            &[0, 1, 2],
        )
        .unwrap();
        let selected = (0..3)
            .find(|candidate| *candidate != primary && *candidate != secondary)
            .unwrap();
        let mut route = decision();
        route.candidates.retain(|candidate| *candidate != selected);
        route.candidates.insert(0, selected);
        route.candidate_state[primary].load_units = 1;
        route.candidate_state[secondary].load_units = 1;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::KeptPairLoad
        );

        route.candidate_state[primary].load_units = 0;
        route.candidate_state[secondary].load_units = 0;
        route.candidate_state[selected].affinity_blocks = 5;
        route.candidate_state[selected].overlap_blocks = 5;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::KeptScore
        );
        route.candidate_state[selected].affinity_blocks = 4;
        // Equal weighted scores retain the selected replica because its raw
        // overlap is deeper, matching Router's full comparator.
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::KeptScore
        );
        route.candidate_state[selected].affinity_blocks = 3;
        route.candidate_state[selected].overlap_blocks = 3;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::WouldPreferPrimary
        );
    }

    #[test]
    fn score_ties_follow_raw_overlap_then_rotation() {
        let mut config = config();
        config.session_affinity_bonus_blocks = 4;
        config.session_affinity_max_load_delta = 1;
        let evaluator = evaluator(&config);
        let session = b"session-a";
        let (primary, secondary) = rendezvous_pair(
            session,
            config.session_affinity_key.as_ref().unwrap().as_bytes(),
            &[0, 1, 2],
        )
        .unwrap();
        let selected = (0..3)
            .find(|candidate| *candidate != primary && *candidate != secondary)
            .unwrap();
        let mut route = decision();
        route.candidates.retain(|candidate| *candidate != selected);
        route.candidates.insert(0, selected);
        route.candidate_state[primary].affinity_blocks = 4;
        route.candidate_state[primary].overlap_blocks = 4;
        route.candidate_state[primary].load_units = 1;
        route.candidate_state[selected].affinity_blocks = 4;
        route.candidate_state[selected].overlap_blocks = 4;

        route.rotation = (3 - primary) % 3;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::WouldPreferPrimary
        );
        route.rotation = (3 - selected) % 3;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::KeptScore
        );
    }

    #[test]
    fn unavailable_pair_distinguishes_load_health_and_global_outage() {
        let config = config();
        let evaluator = evaluator(&config);
        let session = b"session-a";
        let (primary, secondary) = rendezvous_pair(
            session,
            config.session_affinity_key.as_ref().unwrap().as_bytes(),
            &[0, 1, 2],
        )
        .unwrap();
        let other = (0..3)
            .find(|candidate| *candidate != primary && *candidate != secondary)
            .unwrap();
        let mut route = decision();
        route.candidate_state[primary].healthy = false;
        route.candidate_state[secondary].healthy = false;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::NoHealthyAssigned
        );
        route.candidate_state[other].healthy = false;
        assert_eq!(
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::NoHealthyUpstream
        );
    }

    #[test]
    fn malformed_candidate_sets_fail_closed_without_session_content() {
        let config = config();
        let evaluator = evaluator(&config);
        let mut route = decision();
        route.candidate_state[1].index = 0;
        let result = evaluator
            .observe(
                Endpoint::Chat,
                OpaqueSession::Valid(b"private-session"),
                &route,
            )
            .unwrap();
        assert_eq!(result.outcome, SessionAffinityOutcome::InvalidDecision);
        assert!(!format!("{result:?}").contains("private-session"));
    }

    fn four_upstream_config() -> Config {
        let values = HashMap::from([
            (
                "RJ_UPSTREAM",
                "http://a:8000,http://b:8000,http://c:8000,http://d:8000",
            ),
            ("RJ_SESSION_AFFINITY_MODE", "shadow"),
            (
                "RJ_SESSION_AFFINITY_KEY",
                "0123456789abcdef0123456789abcdef",
            ),
        ]);
        let mut config =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        config.affinity = Affinity::Prefix;
        config
    }

    fn four_upstream_decision() -> Decision {
        Decision {
            candidates: vec![0, 1, 2, 3],
            candidate_state: (0..4).map(|index| candidate(index, index, 0, 0)).collect(),
            overlap_blocks: 0,
            total_blocks: 8,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::RoundRobin,
        }
    }

    fn key(config: &Config) -> &[u8] {
        config.session_affinity_key.as_ref().unwrap().as_bytes()
    }

    #[test]
    fn restricted_decisions_are_evaluated_not_rejected() {
        let config = four_upstream_config();
        let evaluator = evaluator(&config);
        // Two models: upstreams 0 and 2 serve this request's model.
        for session in [b"session-a".as_slice(), b"session-b", b"session-c", b"s-d"] {
            let mut route = four_upstream_decision();
            assert!(route.restrict_to(&[true, false, true, false]));
            assert_eq!(route.candidates, vec![0, 2]);
            assert_eq!(route.candidate_state.len(), 4);
            let observed = evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap();
            assert_ne!(observed.outcome, SessionAffinityOutcome::InvalidDecision);
            let (primary, secondary) = (observed.primary.unwrap(), observed.secondary.unwrap());
            // The pair never names a replica of another model.
            assert!([0, 2].contains(&primary) && [0, 2].contains(&secondary));
            assert_ne!(primary, secondary);
            assert!(
                observed
                    .target
                    .is_some_and(|target| [0, 2].contains(&target))
            );
        }
    }

    #[test]
    fn lane_confined_single_replica_is_its_own_pair() {
        let config = four_upstream_config();
        let evaluator = evaluator(&config);
        let mut route = four_upstream_decision();
        assert!(route.restrict_to(&[false, false, false, true]));
        let observed = evaluator
            .observe(Endpoint::Chat, OpaqueSession::Valid(b"session-a"), &route)
            .unwrap();
        assert_eq!(
            observed.outcome,
            SessionAffinityOutcome::ApproximateAlreadyPrimary
        );
        assert_eq!(
            (observed.primary, observed.secondary, observed.target),
            (Some(3), Some(3), Some(3))
        );
    }

    #[test]
    fn rendezvous_over_a_subset_keeps_the_full_fleet_order() {
        let config = four_upstream_config();
        let key = key(&config);
        for ordinal in 0..500 {
            let session = format!("session-{ordinal}");
            let (full_primary, _) =
                rendezvous_pair(session.as_bytes(), key, &[0, 1, 2, 3]).unwrap();
            // Removing a replica other than the primary leaves the primary in
            // place: restriction never reshuffles surviving assignments.
            let subset = (0..4)
                .filter(|index| *index != (full_primary + 1) % 4)
                .collect::<Vec<_>>();
            let (primary, secondary) = rendezvous_pair(session.as_bytes(), key, &subset).unwrap();
            assert_eq!(primary, full_primary);
            assert!(subset.contains(&secondary));
            // Candidate order is irrelevant to the assignment.
            let mut reversed = subset.clone();
            reversed.reverse();
            assert_eq!(
                rendezvous_pair(session.as_bytes(), key, &reversed),
                Some((primary, secondary))
            );
        }
        assert_eq!(rendezvous_pair(b"session-a", key, &[]), None);
        assert_eq!(rendezvous_pair(b"session-a", key, &[2]), Some((2, 2)));
    }

    #[test]
    fn ineligible_replicas_never_set_the_load_floor() {
        let mut config = four_upstream_config();
        config.session_affinity_max_load_delta = 0;
        let evaluator = evaluator(&config);
        let session = b"session-a";
        let (primary, secondary) = rendezvous_pair(session, key(&config), &[0, 2]).unwrap();
        let mut route = four_upstream_decision();
        // Every eligible replica carries load 3; the other model's replicas
        // are idle. Their load must not gate this model's pair.
        for state in &mut route.candidate_state {
            state.load_units = if [0, 2].contains(&state.index) { 3 } else { 0 };
        }
        assert!(route.restrict_to(&[true, false, true, false]));
        route.candidates.retain(|candidate| *candidate != primary);
        route.candidates.insert(0, primary);
        let observed = evaluator
            .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
            .unwrap();
        assert_eq!(
            observed.outcome,
            SessionAffinityOutcome::ApproximateAlreadyPrimary
        );
        assert_eq!(observed.secondary, Some(secondary));
    }

    #[test]
    fn projected_load_counts_against_the_score_like_router() {
        let mut config = config();
        config.route_alpha = 2.0;
        config.session_affinity_bonus_blocks = 4;
        config.session_affinity_max_load_delta = 10;
        let session = b"session-a";
        let (primary, _) = rendezvous_pair(session, key(&config), &[0, 1, 2]).unwrap();
        let selected = (0..3).find(|candidate| *candidate != primary).unwrap();
        let mut route = decision();
        route.candidates.retain(|candidate| *candidate != selected);
        route.candidates.insert(0, selected);
        // The primary would carry a five-unit request; elsewhere it is one.
        for state in &mut route.candidate_state {
            state.request_load_units = if state.index == primary { 5 } else { 1 };
        }

        config.route_projected_load = false;
        assert_eq!(
            evaluator(&config)
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::WouldPreferPrimary
        );
        // Projected: 4 - 2 * 4 = -4 against the selected 0.
        config.route_projected_load = true;
        assert_eq!(
            evaluator(&config)
                .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
                .unwrap()
                .outcome,
            SessionAffinityOutcome::KeptScore
        );
    }

    #[test]
    fn projected_load_gates_the_primary() {
        let mut config = config();
        config.route_projected_load = true;
        config.session_affinity_bonus_blocks = 4;
        config.session_affinity_max_load_delta = 2;
        let evaluator = evaluator(&config);
        let session = b"session-a";
        let (primary, secondary) = rendezvous_pair(session, key(&config), &[0, 1, 2]).unwrap();
        let selected = (0..3)
            .find(|candidate| *candidate != primary && *candidate != secondary)
            .unwrap();
        let mut route = decision();
        route.candidates.retain(|candidate| *candidate != selected);
        route.candidates.insert(0, selected);
        route.candidate_state[primary].request_load_units = 4;
        let observed = evaluator
            .observe(Endpoint::Chat, OpaqueSession::Valid(session), &route)
            .unwrap();
        assert_eq!(
            observed.outcome,
            SessionAffinityOutcome::WouldPreferSecondaryLoad
        );
        assert_eq!(observed.target, Some(secondary));
    }

    #[test]
    fn malformed_restricted_decisions_still_fail_closed() {
        let config = four_upstream_config();
        let evaluator = evaluator(&config);
        let outcome = |route: &Decision| {
            evaluator
                .observe(Endpoint::Chat, OpaqueSession::Valid(b"session-a"), route)
                .unwrap()
                .outcome
        };
        let mut empty = four_upstream_decision();
        empty.candidates.clear();
        assert_eq!(outcome(&empty), SessionAffinityOutcome::InvalidDecision);

        let mut duplicate = four_upstream_decision();
        duplicate.candidates = vec![1, 1];
        assert_eq!(outcome(&duplicate), SessionAffinityOutcome::InvalidDecision);

        let mut out_of_range = four_upstream_decision();
        out_of_range.candidates = vec![1, 4];
        assert_eq!(
            outcome(&out_of_range),
            SessionAffinityOutcome::InvalidDecision
        );

        let mut stateless = four_upstream_decision();
        stateless.candidate_state.retain(|state| state.index != 2);
        stateless.candidates = vec![0, 2];
        assert_eq!(outcome(&stateless), SessionAffinityOutcome::InvalidDecision);

        // Candidate state need not cover the fleet, only the candidates.
        let mut partial = four_upstream_decision();
        partial
            .candidate_state
            .retain(|state| [0, 2].contains(&state.index));
        partial.candidates = vec![0, 2];
        assert_ne!(outcome(&partial), SessionAffinityOutcome::InvalidDecision);
    }
}
