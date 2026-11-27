use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use parking_lot::Mutex;
use serde::Serialize;

use crate::{
    config::PrefixSingleFlightMode,
    router::{Decision, Outcome},
};

pub const PREFIX_SINGLE_FLIGHT_OUTCOMES: [&str; 11] = [
    "off",
    "short",
    "exact_blocked",
    "warm",
    "unavailable",
    "capacity",
    "leader",
    "already_home",
    "load_blocked",
    "would_move",
    "moved",
];

#[derive(Clone, Copy, Debug)]
pub struct PrefixSingleFlightConfig {
    pub mode: PrefixSingleFlightMode,
    pub min_blocks: usize,
    pub capacity: usize,
    pub max_load_delta: usize,
}

/// What a request may be served by, beyond its prefix: the model, API
/// profile, and long-prompt lane it was routed under. Requests that share a
/// prefix but not a scope cannot share a replica's warm cache, so they must
/// never join one another's flight.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FlightScope(u64);

impl FlightScope {
    #[must_use]
    pub fn new(model: Option<&str>, api_profile: &str, lane: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        model.hash(&mut hasher);
        api_profile.hash(&mut hasher);
        lane.hash(&mut hasher);
        Self(hasher.finish())
    }

    /// Binds the scope to the exact set of replicas the request may use, so
    /// even two requests whose labels match can only coalesce when they are
    /// eligible for the same replicas.
    fn with_eligible(self, candidates: &[usize]) -> u64 {
        let mut eligible = candidates.to_vec();
        eligible.sort_unstable();
        let mut hasher = DefaultHasher::new();
        self.0.hash(&mut hasher);
        eligible.hash(&mut hasher);
        hasher.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FlightKey {
    scope: u64,
    prefix_blocks: usize,
    fingerprint: u64,
}

#[derive(Clone, Copy, Debug)]
struct Flight {
    generation: u64,
    target: usize,
    requests: usize,
}

#[derive(Default)]
struct State {
    flights: HashMap<FlightKey, Flight>,
    generation: u64,
}

pub struct PrefixSingleFlight {
    config: PrefixSingleFlightConfig,
    state: Arc<Mutex<State>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PrefixSingleFlightObservation {
    pub mode: &'static str,
    pub outcome: &'static str,
}

impl PrefixSingleFlightObservation {
    #[must_use]
    pub const fn off() -> Self {
        Self {
            mode: "off",
            outcome: "off",
        }
    }
}

pub struct PrefixSingleFlightGuard {
    state: Arc<Mutex<State>>,
    key: FlightKey,
    generation: u64,
    retarget_allowed: bool,
}

impl PrefixSingleFlightGuard {
    /// Update the shared target to the replica the leader is about to send
    /// its prefill to. Call it before dispatch, so followers join the replica
    /// warming the prefix rather than the one first chosen. Only the flight's
    /// leader may move it: a follower's failover is its own, and must not
    /// drag later followers away from the replica the leader is warming.
    /// Generation matching prevents an old guard from changing a later flight
    /// that reused the key.
    pub fn retarget(&mut self, upstream: usize) {
        if !self.retarget_allowed {
            return;
        }
        let mut state = self.state.lock();
        if let Some(flight) = state.flights.get_mut(&self.key)
            && flight.generation == self.generation
        {
            flight.target = upstream;
        }
    }
}

impl Drop for PrefixSingleFlightGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        let remove = state
            .flights
            .get_mut(&self.key)
            .filter(|flight| flight.generation == self.generation)
            .is_some_and(|flight| {
                flight.requests = flight.requests.saturating_sub(1);
                flight.requests == 0
            });
        if remove {
            state.flights.remove(&self.key);
        }
    }
}

impl PrefixSingleFlight {
    /// Create a bounded in-process flight table.
    ///
    /// # Panics
    ///
    /// Panics when the minimum prefix depth or table capacity is zero.
    #[must_use]
    pub fn new(config: PrefixSingleFlightConfig) -> Self {
        assert!(
            config.min_blocks > 0,
            "single-flight prefix must be positive"
        );
        assert!(
            config.capacity > 0,
            "single-flight capacity must be positive"
        );
        Self {
            config,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Join concurrent requests that share the configured leading fingerprint.
    /// Only cold approximate decisions participate; exact placement and an
    /// already warm approximate prefix retain authority. `shadow` maintains
    /// the same bounded flight lifetimes but never changes the decision.
    ///
    /// A follower that cannot join (its flight's target is not a serving
    /// candidate, or is too loaded) routes elsewhere and gets no guard, so it
    /// neither holds the flight open nor can move it.
    pub fn route(
        &self,
        scope: FlightScope,
        fingerprints: &[u64],
        decision: &mut Decision,
    ) -> (
        PrefixSingleFlightObservation,
        Option<PrefixSingleFlightGuard>,
    ) {
        let mode = self.config.mode.label();
        if self.config.mode == PrefixSingleFlightMode::Off {
            return (PrefixSingleFlightObservation::off(), None);
        }
        if fingerprints.len() < self.config.min_blocks {
            return (observation(mode, "short"), None);
        }
        if decision.outcome == Outcome::Exact {
            return (observation(mode, "exact_blocked"), None);
        }
        if decision.overlap_blocks >= self.config.min_blocks {
            return (observation(mode, "warm"), None);
        }
        let Some(&leader) = decision.candidates.first() else {
            return (observation(mode, "unavailable"), None);
        };
        let key = FlightKey {
            scope: scope.with_eligible(&decision.candidates),
            prefix_blocks: self.config.min_blocks,
            fingerprint: fingerprints[self.config.min_blocks - 1],
        };
        let mut state = self.state.lock();
        let Some(flight) = state.flights.get_mut(&key) else {
            if state.flights.len() >= self.config.capacity {
                return (observation(mode, "capacity"), None);
            }
            state.generation = state.generation.wrapping_add(1).max(1);
            let generation = state.generation;
            state.flights.insert(
                key,
                Flight {
                    generation,
                    target: leader,
                    requests: 1,
                },
            );
            drop(state);
            return (
                observation(mode, "leader"),
                Some(self.guard(key, generation, true)),
            );
        };
        let (target, generation) = (flight.target, flight.generation);
        let outcome = if target == leader {
            "already_home"
        } else {
            let target_state = decision.candidate_state.iter().find(|candidate| {
                candidate.index == target
                    && candidate.healthy
                    && decision.candidates.contains(&target)
            });
            match (decision.candidate_state.first(), target_state) {
                (Some(winner), Some(target_state))
                    if target_state.load_units
                        <= winner.load_units.saturating_add(self.config.max_load_delta) =>
                {
                    if self.config.mode == PrefixSingleFlightMode::Shadow {
                        "would_move"
                    } else {
                        "moved"
                    }
                }
                (Some(_), Some(_)) => return (observation(mode, "load_blocked"), None),
                _ => return (observation(mode, "unavailable"), None),
            }
        };
        flight.requests = flight.requests.saturating_add(1);
        drop(state);
        if outcome == "moved" {
            move_to_front(decision, target);
        }
        (
            observation(mode, outcome),
            Some(self.guard(key, generation, false)),
        )
    }

    fn guard(&self, key: FlightKey, generation: u64, leader: bool) -> PrefixSingleFlightGuard {
        PrefixSingleFlightGuard {
            state: Arc::clone(&self.state),
            key,
            generation,
            retarget_allowed: leader,
        }
    }
}

fn observation(mode: &'static str, outcome: &'static str) -> PrefixSingleFlightObservation {
    PrefixSingleFlightObservation { mode, outcome }
}

fn move_to_front(decision: &mut Decision, target: usize) {
    let Some(position) = decision
        .candidates
        .iter()
        .position(|candidate| *candidate == target)
    else {
        return;
    };
    let candidate = decision.candidates.remove(position);
    decision.candidates.insert(0, candidate);
    let Some(position) = decision
        .candidate_state
        .iter()
        .position(|candidate| candidate.index == target)
    else {
        return;
    };
    let candidate = decision.candidate_state.remove(position);
    decision.candidate_state.insert(0, candidate);
    for (rank, candidate) in decision.candidate_state.iter_mut().enumerate() {
        candidate.rank = rank;
    }
    let winner = &decision.candidate_state[0];
    decision.overlap_blocks = winner.overlap_blocks;
    decision.affinity_blocks = winner.affinity_blocks;
    decision.load_units = winner.request_load_units;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{CandidateState, Outcome};

    fn decision(first: usize, loads: [usize; 2]) -> Decision {
        let second = 1 - first;
        Decision {
            candidates: vec![first, second],
            candidate_state: vec![
                CandidateState {
                    index: first,
                    rank: 0,
                    overlap_blocks: 0,
                    affinity_blocks: 0,
                    load_units: loads[first],
                    request_load_units: 1,
                    healthy: true,
                    stale_blocks: 0,
                    horizon_ms: None,
                    overlap_ages_ms: Vec::new(),
                },
                CandidateState {
                    index: second,
                    rank: 1,
                    overlap_blocks: 0,
                    affinity_blocks: 0,
                    load_units: loads[second],
                    request_load_units: 1,
                    healthy: true,
                    stale_blocks: 0,
                    horizon_ms: None,
                    overlap_ages_ms: Vec::new(),
                },
            ],
            overlap_blocks: 0,
            total_blocks: 8,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::RoundRobin,
        }
    }

    fn router(mode: PrefixSingleFlightMode, max_load_delta: usize) -> PrefixSingleFlight {
        PrefixSingleFlight::new(PrefixSingleFlightConfig {
            mode,
            min_blocks: 2,
            capacity: 2,
            max_load_delta,
        })
    }

    fn scope() -> FlightScope {
        FlightScope::new(Some("model-a"), "openai", "off")
    }

    fn route(
        router: &PrefixSingleFlight,
        fingerprints: &[u64],
        decision: &mut Decision,
    ) -> (
        PrefixSingleFlightObservation,
        Option<PrefixSingleFlightGuard>,
    ) {
        router.route(scope(), fingerprints, decision)
    }

    fn requests(router: &PrefixSingleFlight) -> Vec<usize> {
        let state = router.state.lock();
        let mut requests = state
            .flights
            .values()
            .map(|flight| flight.requests)
            .collect::<Vec<_>>();
        requests.sort_unstable();
        requests
    }

    fn target(router: &PrefixSingleFlight) -> usize {
        let state = router.state.lock();
        assert_eq!(state.flights.len(), 1);
        state.flights.values().next().unwrap().target
    }

    #[test]
    fn prefer_coalesces_a_cold_shared_prefix_until_the_leader_finishes() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let fingerprints = [7, 8, 9];
        let mut leader = decision(0, [0, 0]);
        let (observation, leader_guard) = route(&router, &fingerprints, &mut leader);
        assert_eq!(observation.outcome, "leader");

        let mut follower = decision(1, [1, 0]);
        let (observation, follower_guard) = route(&router, &fingerprints, &mut follower);
        assert_eq!(observation.outcome, "moved");
        assert_eq!(follower.candidates[0], 0);

        drop(leader_guard);
        drop(follower_guard);
        let mut next = decision(1, [0, 0]);
        let (observation, _) = route(&router, &fingerprints, &mut next);
        assert_eq!(observation.outcome, "leader");
        assert_eq!(next.candidates[0], 1);
    }

    #[test]
    fn shadow_is_immutable_and_load_delta_blocks_prefer() {
        let fingerprints = [7, 8, 9];
        let shadow = router(PrefixSingleFlightMode::Shadow, 1);
        let mut leader = decision(0, [0, 0]);
        let (_, _leader_guard) = route(&shadow, &fingerprints, &mut leader);
        let mut follower = decision(1, [0, 0]);
        let (observation, _) = route(&shadow, &fingerprints, &mut follower);
        assert_eq!(observation.outcome, "would_move");
        assert_eq!(follower.candidates[0], 1);

        let prefer = router(PrefixSingleFlightMode::Prefer, 1);
        let mut leader = decision(0, [0, 0]);
        let (_, _leader_guard) = route(&prefer, &fingerprints, &mut leader);
        let mut follower = decision(1, [3, 0]);
        let (observation, blocked_guard) = route(&prefer, &fingerprints, &mut follower);
        assert_eq!(observation.outcome, "load_blocked");
        assert_eq!(follower.candidates[0], 1);
        assert!(blocked_guard.is_none());
        let mut later = decision(1, [0, 0]);
        let (observation, _) = route(&prefer, &fingerprints, &mut later);
        assert_eq!(observation.outcome, "moved");
        assert_eq!(later.candidates[0], 0);
    }

    #[test]
    fn warm_exact_short_and_capacity_paths_never_move() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let mut short = decision(0, [0, 0]);
        assert_eq!(route(&router, &[7], &mut short).0.outcome, "short");

        let mut exact = decision(0, [0, 0]);
        exact.outcome = Outcome::Exact;
        assert_eq!(
            route(&router, &[7, 8], &mut exact).0.outcome,
            "exact_blocked"
        );

        let mut warm = decision(0, [0, 0]);
        warm.overlap_blocks = 2;
        assert_eq!(route(&router, &[7, 8], &mut warm).0.outcome, "warm");

        let mut one = decision(0, [0, 0]);
        let (_, _one) = route(&router, &[1, 2], &mut one);
        let mut two = decision(0, [0, 0]);
        let (_, _two) = route(&router, &[3, 4], &mut two);
        let mut three = decision(0, [0, 0]);
        assert_eq!(route(&router, &[5, 6], &mut three).0.outcome, "capacity");
    }

    #[test]
    fn unavailable_followers_do_not_pin_a_dead_flight() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let fingerprints = [7, 8, 9];
        let mut leader = decision(0, [0, 0]);
        let (_, leader_guard) = route(&router, &fingerprints, &mut leader);

        // The flight's target goes unhealthy; steady follower traffic routes
        // elsewhere and must not keep the flight alive.
        let mut followers = Vec::new();
        for _ in 0..5 {
            let mut follower = decision(1, [0, 0]);
            follower.candidate_state[1].healthy = false;
            let (observation, guard) = route(&router, &fingerprints, &mut follower);
            assert_eq!(observation.outcome, "unavailable");
            assert_eq!(follower.candidates[0], 1);
            assert!(guard.is_none());
            followers.push(guard);
        }
        assert_eq!(requests(&router), vec![1]);

        // Once the leader finishes the flight is gone, so the next request
        // leads a fresh flight on a serving replica.
        drop(leader_guard);
        assert!(requests(&router).is_empty());
        let mut next = decision(1, [0, 0]);
        let (observation, _next_guard) = route(&router, &fingerprints, &mut next);
        assert_eq!(observation.outcome, "leader");
        assert_eq!(target(&router), 1);
    }

    #[test]
    fn load_blocked_followers_do_not_pin_an_overloaded_flight() {
        let router = router(PrefixSingleFlightMode::Prefer, 0);
        let fingerprints = [7, 8, 9];
        let mut leader = decision(0, [0, 0]);
        let (_, leader_guard) = route(&router, &fingerprints, &mut leader);
        let mut blocked = Vec::new();
        for _ in 0..3 {
            let mut follower = decision(1, [5, 0]);
            let (observation, guard) = route(&router, &fingerprints, &mut follower);
            assert_eq!(observation.outcome, "load_blocked");
            blocked.push(guard);
        }
        assert!(blocked.iter().all(Option::is_none));
        assert_eq!(requests(&router), vec![1]);
        drop(leader_guard);
        assert!(requests(&router).is_empty());
    }

    #[test]
    fn only_the_leader_may_retarget_the_flight() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let fingerprints = [7, 8, 9];
        let mut leader = decision(0, [0, 0]);
        let (_, mut leader_guard) = route(&router, &fingerprints, &mut leader);

        // An already-home follower fails over: the warming leader keeps the
        // flight.
        let mut home = decision(0, [0, 0]);
        let (observation, mut home_guard) = route(&router, &fingerprints, &mut home);
        assert_eq!(observation.outcome, "already_home");
        home_guard.as_mut().unwrap().retarget(1);
        assert_eq!(target(&router), 0);

        // A moved follower fails over: same.
        let mut moved = decision(1, [0, 0]);
        let (observation, mut moved_guard) = route(&router, &fingerprints, &mut moved);
        assert_eq!(observation.outcome, "moved");
        moved_guard.as_mut().unwrap().retarget(1);
        assert_eq!(target(&router), 0);

        // The leader's own failover moves the flight for later followers.
        leader_guard.as_mut().unwrap().retarget(1);
        assert_eq!(target(&router), 1);
        let mut later = decision(0, [0, 0]);
        let (observation, later_guard) = route(&router, &fingerprints, &mut later);
        assert_eq!(observation.outcome, "moved");
        assert_eq!(later.candidates[0], 1);

        // Followers still hold the flight open until they finish.
        drop(leader_guard);
        assert_eq!(requests(&router), vec![3]);
        drop(home_guard);
        drop(moved_guard);
        drop(later_guard);
        assert!(requests(&router).is_empty());
    }

    #[test]
    fn stale_leader_guard_cannot_retarget_a_reused_key() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let fingerprints = [7, 8, 9];
        let mut first = decision(0, [0, 0]);
        let (_, mut stale) = route(&router, &fingerprints, &mut first);
        // Expire the first flight out from under its guard, then let a new
        // flight reuse the key on upstream 1.
        let key = stale.as_ref().unwrap().key;
        router.state.lock().flights.remove(&key);
        let mut second = decision(1, [0, 0]);
        let (observation, _second_guard) = route(&router, &fingerprints, &mut second);
        assert_eq!(observation.outcome, "leader");
        stale.as_mut().unwrap().retarget(0);
        assert_eq!(target(&router), 1);
        drop(stale);
        assert_eq!(requests(&router), vec![1]);
    }

    #[test]
    fn flights_are_scoped_by_model_profile_lane_and_eligible_set() {
        let router = PrefixSingleFlight::new(PrefixSingleFlightConfig {
            mode: PrefixSingleFlightMode::Prefer,
            min_blocks: 2,
            capacity: 8,
            max_load_delta: 1,
        });
        let fingerprints = [7, 8, 9];
        let mut leader = decision(0, [0, 0]);
        let (_, _leader) = router.route(
            FlightScope::new(Some("model-a"), "openai", "off"),
            &fingerprints,
            &mut leader,
        );
        for scope in [
            FlightScope::new(Some("model-b"), "openai", "off"),
            FlightScope::new(None, "openai", "off"),
            FlightScope::new(Some("model-a"), "systemone", "off"),
            FlightScope::new(Some("model-a"), "openai", "confined"),
        ] {
            let mut other = decision(1, [0, 0]);
            let (observation, _guard) = router.route(scope, &fingerprints, &mut other);
            assert_eq!(observation.outcome, "leader", "{scope:?}");
            assert_eq!(other.candidates[0], 1);
        }

        // Same labels but a different eligible replica set: separate flight.
        let mut restricted = decision(1, [0, 0]);
        restricted.candidates = vec![1];
        let (observation, _guard) = router.route(
            FlightScope::new(Some("model-a"), "openai", "off"),
            &fingerprints,
            &mut restricted,
        );
        assert_eq!(observation.outcome, "leader");
        assert_eq!(restricted.candidates, vec![1]);

        // The matching scope joins the original flight regardless of order.
        let mut same = decision(1, [0, 0]);
        let (observation, _guard) = router.route(
            FlightScope::new(Some("model-a"), "openai", "off"),
            &fingerprints,
            &mut same,
        );
        assert_eq!(observation.outcome, "moved");
        assert_eq!(same.candidates[0], 0);
    }

    #[test]
    fn every_outcome_label_is_preinitialized() {
        for outcome in [
            "short",
            "exact_blocked",
            "warm",
            "unavailable",
            "capacity",
            "leader",
            "already_home",
            "load_blocked",
            "would_move",
            "moved",
        ] {
            assert!(
                PREFIX_SINGLE_FLIGHT_OUTCOMES.contains(&outcome),
                "{outcome}"
            );
        }
    }
}
