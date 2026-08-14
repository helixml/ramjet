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
        if self.mode == SessionAffinityMode::Off || endpoint == Endpoint::Other {
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
        let Some((primary, secondary)) = rendezvous_pair(session_id, key, self.upstream_count)
        else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let Some(states) = validated_states(decision, self.upstream_count) else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let Some(selected) = decision.candidates.first().copied() else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let Some(selected_state) = states.get(selected).copied() else {
            return self.observation(SessionAffinityOutcome::InvalidDecision, None);
        };
        let Some(min_load) = states
            .iter()
            .filter(|state| state.healthy)
            .map(|state| state.load_units)
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
        let target = if states[primary].healthy && states[primary].load_units <= admitted_load {
            Some((primary, TargetKind::Primary))
        } else if states[secondary].healthy && states[secondary].load_units <= admitted_load {
            Some((
                secondary,
                if states[primary].healthy {
                    TargetKind::SecondaryPrimaryLoadGated
                } else {
                    TargetKind::SecondaryPrimaryUnhealthy
                },
            ))
        } else {
            None
        };
        let Some((target, target_kind)) = target else {
            let outcome = if !states[primary].healthy && !states[secondary].healthy {
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
        let target_state = states[target];
        if !target_wins_with_bonus(
            target_state,
            selected_state,
            self.alpha,
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

fn validated_states(decision: &Decision, upstream_count: usize) -> Option<Vec<&CandidateState>> {
    if decision.candidates.len() != upstream_count
        || decision.candidate_state.len() != upstream_count
    {
        return None;
    }
    let mut states = vec![None; upstream_count];
    for state in &decision.candidate_state {
        if state.index >= upstream_count || states[state.index].replace(state).is_some() {
            return None;
        }
    }
    let states = states.into_iter().collect::<Option<Vec<_>>>()?;
    let mut seen = vec![false; upstream_count];
    for candidate in &decision.candidates {
        if *candidate >= upstream_count || std::mem::replace(&mut seen[*candidate], true) {
            return None;
        }
    }
    Some(states)
}

fn rendezvous_pair(session_id: &[u8], key: &[u8], upstream_count: usize) -> Option<(usize, usize)> {
    if upstream_count < 2 || session_id.is_empty() || key.is_empty() {
        return None;
    }
    let mut ranked = (0..upstream_count)
        .map(|index| {
            let ordinal = u64::try_from(index)
                .expect("upstream ordinal fits u64")
                .to_be_bytes();
            let digest = hmac_sha256(key, &[SESSION_AFFINITY_DOMAIN, session_id, &ordinal]);
            let score = u128::from_be_bytes(digest[..16].try_into().expect("SHA-256 has 16 bytes"));
            (score, index)
        })
        .collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Some((ranked[0].1, ranked[1].1))
}

fn base_score(candidate: &CandidateState, alpha: f64) -> f64 {
    usize_to_f64(candidate.affinity_blocks) - alpha * usize_to_f64(candidate.load_units)
}

#[allow(clippy::float_cmp)] // Exact equality is required for Router comparator parity.
fn target_wins_with_bonus(
    target: &CandidateState,
    selected: &CandidateState,
    alpha: f64,
    bonus_blocks: usize,
    rotation: usize,
    candidate_count: usize,
) -> bool {
    if target.healthy != selected.healthy {
        return target.healthy;
    }
    let target_score = base_score(target, alpha) + usize_to_f64(bonus_blocks);
    let selected_score = base_score(selected, alpha);
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
            ("DS4_UPSTREAM", "http://a:8000,http://b:8000,http://c:8000"),
            ("DS4_SESSION_AFFINITY_MODE", "shadow"),
            (
                "DS4_SESSION_AFFINITY_KEY",
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
        let pair = rendezvous_pair(b"session-a", key, 3).unwrap();
        // Independently verified with OpenSSL HMAC-SHA256. The first 128 bits
        // rank ordinals 1, 2, 0 in descending order.
        assert_eq!(pair, (1, 2));
        assert_ne!(pair.0, pair.1);
        for _ in 0..20 {
            assert_eq!(rendezvous_pair(b"session-a", key, 3), Some(pair));
        }
        assert_ne!(
            rendezvous_pair(b"session-a", key, 3),
            rendezvous_pair(b"session-a", b"fedcba9876543210fedcba9876543210", 3)
        );
    }

    #[test]
    fn rendezvous_primaries_are_balanced_and_topology_order_is_explicit() {
        let key = b"0123456789abcdef0123456789abcdef";
        let mut primary_counts = [0_usize; 3];
        for ordinal in 0..12_000 {
            let session = format!("session-{ordinal}");
            let (primary, secondary) = rendezvous_pair(session.as_bytes(), key, 3).unwrap();
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
            rendezvous_pair(b"session-a", key, 3),
            rendezvous_pair(b"session-a", key, 4)
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
            3,
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
            3,
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
            3,
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
            3,
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
}
