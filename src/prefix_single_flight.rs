use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;
use serde::Serialize;

use crate::{
    config::PrefixSingleFlightMode,
    router::{Decision, Outcome},
};

pub const PREFIX_SINGLE_FLIGHT_OUTCOMES: [&str; 10] = [
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
];

#[derive(Clone, Copy, Debug)]
pub struct PrefixSingleFlightConfig {
    pub mode: PrefixSingleFlightMode,
    pub min_blocks: usize,
    pub capacity: usize,
    pub max_load_delta: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FlightKey {
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
    /// Update the shared target after dispatch failover. Generation matching
    /// prevents an old guard from changing a later flight that reused the key.
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
    pub fn route(
        &self,
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
            prefix_blocks: self.config.min_blocks,
            fingerprint: fingerprints[self.config.min_blocks - 1],
        };
        let mut state = self.state.lock();
        let (target, generation, leader_request) = if let Some(flight) = state.flights.get_mut(&key)
        {
            flight.requests = flight.requests.saturating_add(1);
            (flight.target, flight.generation, false)
        } else {
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
            (leader, generation, true)
        };
        drop(state);
        let mut guard = PrefixSingleFlightGuard {
            state: Arc::clone(&self.state),
            key,
            generation,
            retarget_allowed: false,
        };
        if leader_request {
            guard.retarget_allowed = true;
            return (observation(mode, "leader"), Some(guard));
        }
        if target == leader {
            guard.retarget_allowed = true;
            return (observation(mode, "already_home"), Some(guard));
        }
        let Some(winner) = decision.candidate_state.first() else {
            return (observation(mode, "unavailable"), Some(guard));
        };
        let Some(target_state) = decision
            .candidate_state
            .iter()
            .find(|candidate| candidate.index == target && candidate.healthy)
        else {
            return (observation(mode, "unavailable"), Some(guard));
        };
        if target_state.load_units > winner.load_units.saturating_add(self.config.max_load_delta) {
            return (observation(mode, "load_blocked"), Some(guard));
        }
        if self.config.mode == PrefixSingleFlightMode::Shadow {
            return (observation(mode, "would_move"), Some(guard));
        }
        move_to_front(decision, target);
        guard.retarget_allowed = true;
        (observation(mode, "moved"), Some(guard))
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
                },
                CandidateState {
                    index: second,
                    rank: 1,
                    overlap_blocks: 0,
                    affinity_blocks: 0,
                    load_units: loads[second],
                    request_load_units: 1,
                    healthy: true,
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

    #[test]
    fn prefer_coalesces_a_cold_shared_prefix_until_the_leader_finishes() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let fingerprints = [7, 8, 9];
        let mut leader = decision(0, [0, 0]);
        let (observation, leader_guard) = router.route(&fingerprints, &mut leader);
        assert_eq!(observation.outcome, "leader");

        let mut follower = decision(1, [1, 0]);
        let (observation, follower_guard) = router.route(&fingerprints, &mut follower);
        assert_eq!(observation.outcome, "moved");
        assert_eq!(follower.candidates[0], 0);

        drop(leader_guard);
        drop(follower_guard);
        let mut next = decision(1, [0, 0]);
        let (observation, _) = router.route(&fingerprints, &mut next);
        assert_eq!(observation.outcome, "leader");
        assert_eq!(next.candidates[0], 1);
    }

    #[test]
    fn shadow_is_immutable_and_load_delta_blocks_prefer() {
        let fingerprints = [7, 8, 9];
        let shadow = router(PrefixSingleFlightMode::Shadow, 1);
        let mut leader = decision(0, [0, 0]);
        let (_, _leader_guard) = shadow.route(&fingerprints, &mut leader);
        let mut follower = decision(1, [0, 0]);
        let (observation, _) = shadow.route(&fingerprints, &mut follower);
        assert_eq!(observation.outcome, "would_move");
        assert_eq!(follower.candidates[0], 1);

        let prefer = router(PrefixSingleFlightMode::Prefer, 1);
        let mut leader = decision(0, [0, 0]);
        let (_, _leader_guard) = prefer.route(&fingerprints, &mut leader);
        let mut follower = decision(1, [3, 0]);
        let (observation, mut blocked_guard) = prefer.route(&fingerprints, &mut follower);
        assert_eq!(observation.outcome, "load_blocked");
        assert_eq!(follower.candidates[0], 1);
        blocked_guard.as_mut().unwrap().retarget(1);
        let mut later = decision(1, [0, 0]);
        let (observation, _) = prefer.route(&fingerprints, &mut later);
        assert_eq!(observation.outcome, "moved");
        assert_eq!(later.candidates[0], 0);
    }

    #[test]
    fn warm_exact_short_and_capacity_paths_never_move() {
        let router = router(PrefixSingleFlightMode::Prefer, 1);
        let mut short = decision(0, [0, 0]);
        assert_eq!(router.route(&[7], &mut short).0.outcome, "short");

        let mut exact = decision(0, [0, 0]);
        exact.outcome = Outcome::Exact;
        assert_eq!(router.route(&[7, 8], &mut exact).0.outcome, "exact_blocked");

        let mut warm = decision(0, [0, 0]);
        warm.overlap_blocks = 2;
        assert_eq!(router.route(&[7, 8], &mut warm).0.outcome, "warm");

        let mut one = decision(0, [0, 0]);
        let (_, _one) = router.route(&[1, 2], &mut one);
        let mut two = decision(0, [0, 0]);
        let (_, _two) = router.route(&[3, 4], &mut two);
        let mut three = decision(0, [0, 0]);
        assert_eq!(router.route(&[5, 6], &mut three).0.outcome, "capacity");
    }
}
