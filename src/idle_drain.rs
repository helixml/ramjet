//! Idle-driven single-engine drain policy.
//!
//! Two resident TP4 engines draw roughly 800W on node06 whether or not they are
//! serving; an idle engine still holds its weights and KV arena, so its clocks
//! stay pinned and its memory keeps refreshing. Parking one engine during a
//! genuinely idle window recovers about half of that, and — unlike stopping
//! both — it costs nothing at the first request, because the surviving replica
//! is already warm.
//!
//! This module owns only the *decision*. It deliberately does not stop
//! anything: the load balancer sits in the production request path and must not
//! hold a Docker socket, which is root-equivalent on the host. Instead the
//! policy publishes a desired running state per upstream, and a separately
//! privileged actor converges the containers onto it. The load balancer keeps
//! the authority it already has — which replicas it will route to — and nothing
//! more.
//!
//! The state machine is intentionally asymmetric. Draining is slow, bounded by
//! a cooldown, and refuses to cross the warm floor. Resuming is immediate and
//! bypasses every rate limit, because the failure that matters is being short
//! of capacity when traffic arrives, not parking an engine a few seconds late.

use std::time::Duration;

/// How far the policy is permitted to act on its own conclusions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IdleDrainMode {
    /// Disabled. Every upstream stays warm and no intent is published.
    #[default]
    Off,
    /// Evaluate and publish intent, but never fence an upstream from routing.
    ///
    /// This is the safe way to qualify the policy against real traffic: the
    /// exported state shows what it *would* have parked without any serving
    /// consequence if the idle threshold turns out to be badly tuned.
    Observe,
    /// Evaluate, publish intent, and fence drained upstreams from routing.
    Drain,
}

impl IdleDrainMode {
    /// Returns the stable metric/log label for this mode.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Drain => "drain",
        }
    }

    /// Whether the policy is permitted to withhold an upstream from routing.
    #[must_use]
    pub fn fences_routing(self) -> bool {
        matches!(self, Self::Drain)
    }

    /// Whether the policy evaluates at all.
    #[must_use]
    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Tuning for [`IdleDrainPolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleDrainConfig {
    pub mode: IdleDrainMode,
    /// Quiet period before the fleet is considered idle.
    pub idle_after: Duration,
    /// Serving replicas that must always remain warm. Clamped to at least one.
    pub min_warm: usize,
    /// Minimum spacing between drain transitions, to stop the policy flapping
    /// around the idle threshold. Never applies to resuming.
    pub cooldown: Duration,
    /// How long a fenced upstream must sit at zero inflight before it is
    /// reported safe to stop. This covers requests already dispatched when the
    /// fence was applied.
    pub drain_grace: Duration,
}

impl Default for IdleDrainConfig {
    fn default() -> Self {
        Self {
            mode: IdleDrainMode::Off,
            idle_after: Duration::from_mins(15),
            min_warm: 1,
            cooldown: Duration::from_mins(5),
            drain_grace: Duration::from_secs(30),
        }
    }
}

impl IdleDrainConfig {
    /// The effective warm floor. At least one replica must always serve, so a
    /// configured zero is treated as one rather than rejected.
    #[must_use]
    pub fn effective_min_warm(&self) -> usize {
        self.min_warm.max(1)
    }
}

/// What the policy currently believes about one upstream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UpstreamDrainState {
    /// Serving normally.
    #[default]
    Warm,
    /// Fenced from new routing, still finishing in-flight work.
    Draining,
    /// Fenced and quiet. Safe for the privileged actor to stop.
    Drained,
}

impl UpstreamDrainState {
    /// Returns the stable metric/log label for this state.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Draining => "draining",
            Self::Drained => "drained",
        }
    }

    /// Numeric encoding for the exported gauge.
    #[must_use]
    pub fn code(self) -> f64 {
        match self {
            Self::Warm => 0.0,
            Self::Draining => 1.0,
            Self::Drained => 2.0,
        }
    }

    /// Whether this state withholds the upstream from routing.
    #[must_use]
    pub fn fenced(self) -> bool {
        matches!(self, Self::Draining | Self::Drained)
    }
}

/// A single upstream's observed condition, sampled by the caller.
#[derive(Clone, Copy, Debug)]
pub struct UpstreamObservation {
    /// Health as the probe and reliability guards see it, ignoring any drain.
    pub healthy: bool,
    /// Requests currently dispatched to this upstream.
    pub inflight: usize,
}

/// The policy's published conclusion for one upstream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpstreamIntent {
    pub state: UpstreamDrainState,
    /// Whether routing should currently withhold this upstream. Always false in
    /// [`IdleDrainMode::Observe`], even while the state machine advances.
    pub fenced: bool,
    /// Whether the privileged actor should keep this engine running. The actor
    /// converges on this: stop when false and `safe_to_stop`, start when true.
    pub desired_running: bool,
    /// Whether the upstream is fenced and quiet, so stopping it now cannot
    /// interrupt a request the load balancer already dispatched.
    pub safe_to_stop: bool,
}

impl UpstreamIntent {
    fn warm() -> Self {
        Self {
            state: UpstreamDrainState::Warm,
            fenced: false,
            desired_running: true,
            safe_to_stop: false,
        }
    }
}

/// The outcome of one [`IdleDrainPolicy::tick`].
#[derive(Clone, Debug, PartialEq)]
pub struct IdleDrainDecision {
    pub upstreams: Vec<UpstreamIntent>,
    /// Upstreams whose state changed during this tick, for transition metrics.
    pub transitions: Vec<(usize, UpstreamDrainState)>,
    /// Whether the fleet is currently inside its idle window.
    pub idle: bool,
}

impl IdleDrainDecision {
    /// Number of upstreams the policy currently wants left running.
    #[must_use]
    pub fn desired_running(&self) -> usize {
        self.upstreams
            .iter()
            .filter(|intent| intent.desired_running)
            .count()
    }
}

/// Idle-driven drain decisions for a fixed set of upstreams.
///
/// The caller drives this: it records request activity, then ticks the policy
/// with a fresh observation of every upstream. Time is supplied as a monotonic
/// millisecond counter so the state machine stays deterministic under test.
#[derive(Debug)]
pub struct IdleDrainPolicy {
    config: IdleDrainConfig,
    states: Vec<UpstreamDrainState>,
    /// When each fenced upstream last reported non-zero inflight, used to apply
    /// the drain grace period.
    busy_since_ms: Vec<u64>,
    last_activity_ms: u64,
    last_drain_transition_ms: Option<u64>,
}

impl IdleDrainPolicy {
    /// Creates a policy for `upstreams` replicas, all initially warm.
    ///
    /// # Panics
    ///
    /// Panics if `upstreams` is zero; the router always has at least one.
    #[must_use]
    pub fn new(config: IdleDrainConfig, upstreams: usize, now_ms: u64) -> Self {
        assert!(upstreams > 0, "idle drain needs an upstream");
        Self {
            config,
            states: vec![UpstreamDrainState::Warm; upstreams],
            busy_since_ms: vec![now_ms; upstreams],
            last_activity_ms: now_ms,
            last_drain_transition_ms: None,
        }
    }

    #[must_use]
    pub fn config(&self) -> &IdleDrainConfig {
        &self.config
    }

    /// Records that the fleet served a request. Any activity resumes every
    /// parked upstream on the next tick, regardless of cooldown.
    pub fn observe_activity(&mut self, now_ms: u64) {
        self.last_activity_ms = self.last_activity_ms.max(now_ms);
    }

    /// Current state of one upstream, if it exists.
    #[must_use]
    pub fn state(&self, upstream: usize) -> Option<UpstreamDrainState> {
        self.states.get(upstream).copied()
    }

    /// Re-evaluates the policy against a fresh observation of every upstream.
    ///
    /// `observations` must be indexed by upstream. Any upstream missing from
    /// the slice is treated as unhealthy and busy, which is the conservative
    /// reading: it can neither be counted as warm nor be declared safe to stop.
    pub fn tick(&mut self, now_ms: u64, observations: &[UpstreamObservation]) -> IdleDrainDecision {
        let previous = self.states.clone();

        if !self.config.mode.enabled() {
            self.states.fill(UpstreamDrainState::Warm);
            return self.decide(now_ms, observations, &previous, false);
        }

        let idle = self.fleet_idle(now_ms, observations);
        if idle {
            self.advance_drain(now_ms, observations);
        } else {
            // Resume is unconditional and immediate. It deliberately ignores
            // the cooldown: arriving traffic must never wait on a rate limit
            // that exists only to stop the policy flapping while quiet.
            self.states.fill(UpstreamDrainState::Warm);
        }

        self.decide(now_ms, observations, &previous, idle)
    }

    /// The fleet is idle when nothing is in flight anywhere *and* the quiet
    /// period has elapsed. In-flight work on a parked replica counts: a drained
    /// engine that is still finishing a request has not gone quiet yet.
    fn fleet_idle(&self, now_ms: u64, observations: &[UpstreamObservation]) -> bool {
        let any_inflight = observations
            .iter()
            .any(|observation| observation.inflight > 0);
        if any_inflight {
            return false;
        }
        let idle_after_ms = duration_to_ms(self.config.idle_after);
        now_ms.saturating_sub(self.last_activity_ms) >= idle_after_ms
    }

    fn advance_drain(&mut self, now_ms: u64, observations: &[UpstreamObservation]) {
        self.promote_drained(now_ms, observations);

        let warm_serving = self.warm_serving_count(observations);
        let min_warm = self.config.effective_min_warm();
        if warm_serving <= min_warm {
            return;
        }
        if !self.cooldown_elapsed(now_ms) {
            return;
        }
        // Only ever park one replica per cooldown window. Fleet size here is
        // two, but stepping one at a time keeps the warm floor safe for any
        // cardinality without a second guard.
        if let Some(target) = self.select_target(observations) {
            self.states[target] = UpstreamDrainState::Draining;
            self.busy_since_ms[target] = now_ms;
            self.last_drain_transition_ms = Some(now_ms);
        }
    }

    /// Moves fenced upstreams from `Draining` to `Drained` once they have been
    /// quiet for the grace period.
    fn promote_drained(&mut self, now_ms: u64, observations: &[UpstreamObservation]) {
        for (index, state) in self.states.iter_mut().enumerate() {
            if *state != UpstreamDrainState::Draining {
                continue;
            }
            let inflight = observations
                .get(index)
                .map_or(usize::MAX, |observation| observation.inflight);
            if inflight > 0 {
                self.busy_since_ms[index] = now_ms;
                continue;
            }
            let grace_ms = duration_to_ms(self.config.drain_grace);
            if now_ms.saturating_sub(self.busy_since_ms[index]) >= grace_ms {
                *state = UpstreamDrainState::Drained;
            }
        }
    }

    /// Replicas that are both healthy and not already parked. An unhealthy
    /// replica never counts toward the warm floor, so the policy cannot park
    /// its way down to a fleet that only looks safe on paper.
    fn warm_serving_count(&self, observations: &[UpstreamObservation]) -> usize {
        self.states
            .iter()
            .enumerate()
            .filter(|(index, state)| {
                !state.fenced()
                    && observations
                        .get(*index)
                        .is_some_and(|observation| observation.healthy)
            })
            .count()
    }

    /// Picks the highest-indexed healthy warm replica. Deterministic selection
    /// keeps the same engine parked across ticks instead of alternating, which
    /// would defeat the point by cold-starting both.
    fn select_target(&self, observations: &[UpstreamObservation]) -> Option<usize> {
        self.states
            .iter()
            .enumerate()
            .rev()
            .find(|(index, state)| {
                !state.fenced()
                    && observations
                        .get(*index)
                        .is_some_and(|observation| observation.healthy)
            })
            .map(|(index, _)| index)
    }

    fn cooldown_elapsed(&self, now_ms: u64) -> bool {
        let cooldown_ms = duration_to_ms(self.config.cooldown);
        self.last_drain_transition_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= cooldown_ms)
    }

    fn decide(
        &mut self,
        now_ms: u64,
        observations: &[UpstreamObservation],
        previous: &[UpstreamDrainState],
        idle: bool,
    ) -> IdleDrainDecision {
        let fences = self.config.mode.fences_routing();
        let upstreams = self
            .states
            .iter()
            .enumerate()
            .map(|(index, state)| {
                if *state == UpstreamDrainState::Warm {
                    return UpstreamIntent::warm();
                }
                let quiet = observations
                    .get(index)
                    .is_some_and(|observation| observation.inflight == 0);
                UpstreamIntent {
                    state: *state,
                    fenced: fences,
                    desired_running: false,
                    // Only a fully drained, quiet replica may be stopped. In
                    // observe mode nothing is fenced, so stopping it would cut
                    // live traffic — withhold the signal entirely.
                    safe_to_stop: fences && *state == UpstreamDrainState::Drained && quiet,
                }
            })
            .collect();

        let transitions = self
            .states
            .iter()
            .enumerate()
            .filter(|(index, state)| previous.get(*index) != Some(*state))
            .map(|(index, state)| (index, *state))
            .collect();

        // Resuming clears the cooldown so a later idle window is not charged
        // for a drain that traffic already undid.
        if self
            .states
            .iter()
            .all(|state| *state == UpstreamDrainState::Warm)
        {
            self.last_drain_transition_ms = None;
        }
        let _ = now_ms;

        IdleDrainDecision {
            upstreams,
            transitions,
            idle,
        }
    }
}

fn duration_to_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    fn config(mode: IdleDrainMode) -> IdleDrainConfig {
        IdleDrainConfig {
            mode,
            idle_after: Duration::from_mins(10),
            min_warm: 1,
            cooldown: Duration::from_mins(5),
            drain_grace: Duration::from_secs(30),
        }
    }

    fn healthy_idle(count: usize) -> Vec<UpstreamObservation> {
        vec![
            UpstreamObservation {
                healthy: true,
                inflight: 0,
            };
            count
        ]
    }

    /// Drives the policy to a settled drained state and returns the tick clock.
    fn drain_one(policy: &mut IdleDrainPolicy) -> u64 {
        let mut now = 11 * MINUTE;
        policy.tick(now, &healthy_idle(2));
        now += MINUTE;
        policy.tick(now, &healthy_idle(2));
        now
    }

    #[test]
    fn off_mode_never_fences_or_publishes_intent() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Off), 2, 0);
        let decision = policy.tick(24 * 60 * MINUTE, &healthy_idle(2));
        assert!(!decision.idle);
        assert_eq!(decision.desired_running(), 2);
        assert!(decision.upstreams.iter().all(|intent| !intent.fenced));
        assert!(
            decision
                .upstreams
                .iter()
                .all(|intent| intent.state == UpstreamDrainState::Warm)
        );
    }

    #[test]
    fn stays_warm_until_the_idle_window_elapses() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        // One second short of the 10-minute threshold.
        let decision = policy.tick(10 * MINUTE - 1_000, &healthy_idle(2));
        assert!(!decision.idle);
        assert_eq!(decision.desired_running(), 2);
    }

    #[test]
    fn drains_exactly_one_replica_after_the_idle_window() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let decision = policy.tick(11 * MINUTE, &healthy_idle(2));
        assert!(decision.idle);
        assert_eq!(decision.desired_running(), 1);
        // Deterministically the highest index, so the same engine stays parked.
        assert_eq!(decision.upstreams[0].state, UpstreamDrainState::Warm);
        assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Draining);
        assert!(decision.upstreams[1].fenced);
        // Draining is not yet safe to stop: the grace period has not elapsed.
        assert!(!decision.upstreams[1].safe_to_stop);
    }

    #[test]
    fn never_drains_below_the_warm_floor() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let mut now = drain_one(&mut policy);
        // Keep ticking well past several cooldown windows.
        for _ in 0..20 {
            now += 10 * MINUTE;
            let decision = policy.tick(now, &healthy_idle(2));
            assert_eq!(
                decision.desired_running(),
                1,
                "warm floor must hold at one replica"
            );
            assert_eq!(decision.upstreams[0].state, UpstreamDrainState::Warm);
        }
    }

    #[test]
    fn single_upstream_fleet_never_drains() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 1, 0);
        let decision = policy.tick(24 * 60 * MINUTE, &healthy_idle(1));
        assert!(decision.idle);
        assert_eq!(decision.desired_running(), 1);
        assert_eq!(decision.upstreams[0].state, UpstreamDrainState::Warm);
    }

    #[test]
    fn higher_warm_floor_blocks_draining_a_two_engine_fleet() {
        let mut policy = IdleDrainPolicy::new(
            IdleDrainConfig {
                min_warm: 2,
                ..config(IdleDrainMode::Drain)
            },
            2,
            0,
        );
        let decision = policy.tick(24 * 60 * MINUTE, &healthy_idle(2));
        assert_eq!(decision.desired_running(), 2);
    }

    #[test]
    fn zero_warm_floor_is_clamped_to_one() {
        let mut policy = IdleDrainPolicy::new(
            IdleDrainConfig {
                min_warm: 0,
                ..config(IdleDrainMode::Drain)
            },
            2,
            0,
        );
        let mut now = drain_one(&mut policy);
        for _ in 0..10 {
            now += 10 * MINUTE;
            let decision = policy.tick(now, &healthy_idle(2));
            assert_eq!(
                decision.desired_running(),
                1,
                "must never park every engine"
            );
        }
    }

    #[test]
    fn unhealthy_replica_does_not_count_toward_the_warm_floor() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let observations = vec![
            UpstreamObservation {
                healthy: false,
                inflight: 0,
            },
            UpstreamObservation {
                healthy: true,
                inflight: 0,
            },
        ];
        // Only one replica is actually serving, so parking the other would
        // leave nothing warm.
        let decision = policy.tick(24 * 60 * MINUTE, &observations);
        assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Warm);
        assert!(decision.upstreams.iter().all(|intent| !intent.fenced));
    }

    #[test]
    fn promotes_to_drained_only_after_the_grace_period() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let now = 11 * MINUTE;
        policy.tick(now, &healthy_idle(2));
        // 20s later: inside the 30s grace window.
        let decision = policy.tick(now + 20_000, &healthy_idle(2));
        assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Draining);
        assert!(!decision.upstreams[1].safe_to_stop);
        // 40s later: grace elapsed.
        let decision = policy.tick(now + 40_000, &healthy_idle(2));
        assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Drained);
        assert!(decision.upstreams[1].safe_to_stop);
    }

    #[test]
    fn inflight_on_a_draining_replica_restarts_the_grace_period() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let now = 11 * MINUTE;
        policy.tick(now, &healthy_idle(2));
        let busy = vec![
            UpstreamObservation {
                healthy: true,
                inflight: 0,
            },
            UpstreamObservation {
                healthy: true,
                inflight: 1,
            },
        ];
        // A request still draining out resets the clock and un-idles the fleet.
        let decision = policy.tick(now + 25_000, &busy);
        assert!(!decision.idle);
        // Activity implied by inflight work resumes the replica outright.
        assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Warm);
    }

    #[test]
    fn activity_resumes_a_drained_replica_immediately() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let now = drain_one(&mut policy);
        assert_eq!(policy.state(1), Some(UpstreamDrainState::Drained));

        policy.observe_activity(now + 1_000);
        let decision = policy.tick(now + 1_100, &healthy_idle(2));
        assert!(!decision.idle);
        assert_eq!(decision.desired_running(), 2);
        assert!(decision.upstreams.iter().all(|intent| !intent.fenced));
        assert_eq!(decision.transitions, vec![(1, UpstreamDrainState::Warm)]);
    }

    #[test]
    fn resume_bypasses_the_cooldown_that_gates_draining() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let now = drain_one(&mut policy);
        policy.observe_activity(now);
        // Resume lands immediately, far inside the 5-minute cooldown.
        let decision = policy.tick(now + 1, &healthy_idle(2));
        assert_eq!(decision.desired_running(), 2);
    }

    #[test]
    fn cooldown_gates_a_second_drain_after_a_resume() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 3, 0);
        let mut now = 11 * MINUTE;
        policy.tick(now, &healthy_idle(3));
        assert_eq!(policy.state(2), Some(UpstreamDrainState::Draining));
        // Immediately eligible by warm floor, but the cooldown has not elapsed.
        now += MINUTE;
        let decision = policy.tick(now, &healthy_idle(3));
        assert_eq!(decision.desired_running(), 2);
        // Past the cooldown the second replica may park too.
        now += 5 * MINUTE;
        let decision = policy.tick(now, &healthy_idle(3));
        assert_eq!(decision.desired_running(), 1);
        assert_eq!(decision.upstreams[0].state, UpstreamDrainState::Warm);
    }

    #[test]
    fn observe_mode_advances_state_without_fencing_or_stop_intent() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Observe), 2, 0);
        let mut now = 11 * MINUTE;
        policy.tick(now, &healthy_idle(2));
        now += MINUTE;
        let decision = policy.tick(now, &healthy_idle(2));
        assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Drained);
        // The state machine ran, but routing is untouched and nothing may be
        // stopped, because an unfenced replica can still receive traffic.
        assert!(decision.upstreams.iter().all(|intent| !intent.fenced));
        assert!(decision.upstreams.iter().all(|intent| !intent.safe_to_stop));
    }

    #[test]
    fn missing_observation_is_treated_as_unhealthy_and_busy() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let mut now = 11 * MINUTE;
        // Only one observation supplied for a two-replica fleet.
        let short = healthy_idle(1);
        let decision = policy.tick(now, &short);
        // The unobserved replica cannot be counted warm, so nothing is parked.
        assert_eq!(decision.desired_running(), 2);

        // It also cannot be promoted to drained once fenced.
        policy.tick(now, &healthy_idle(2));
        assert_eq!(policy.state(1), Some(UpstreamDrainState::Draining));
        now += 10 * MINUTE;
        let decision = policy.tick(now, &short);
        assert_ne!(decision.upstreams[1].state, UpstreamDrainState::Drained);
        assert!(!decision.upstreams[1].safe_to_stop);
    }

    #[test]
    fn a_stopped_drained_replica_stays_parked_and_is_not_replaced() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let mut now = drain_one(&mut policy);
        // The privileged actor stopped it, so it now probes unhealthy.
        let stopped = vec![
            UpstreamObservation {
                healthy: true,
                inflight: 0,
            },
            UpstreamObservation {
                healthy: false,
                inflight: 0,
            },
        ];
        for _ in 0..10 {
            now += 10 * MINUTE;
            let decision = policy.tick(now, &stopped);
            assert_eq!(decision.upstreams[1].state, UpstreamDrainState::Drained);
            assert_eq!(decision.upstreams[0].state, UpstreamDrainState::Warm);
            assert_eq!(decision.desired_running(), 1);
        }
    }

    #[test]
    fn transitions_are_reported_once_per_change() {
        let mut policy = IdleDrainPolicy::new(config(IdleDrainMode::Drain), 2, 0);
        let now = 11 * MINUTE;
        let decision = policy.tick(now, &healthy_idle(2));
        assert_eq!(
            decision.transitions,
            vec![(1, UpstreamDrainState::Draining)]
        );
        // A tick that changes nothing reports nothing.
        let decision = policy.tick(now + 1_000, &healthy_idle(2));
        assert!(decision.transitions.is_empty());
    }

    #[test]
    fn state_labels_and_codes_are_stable() {
        assert_eq!(UpstreamDrainState::Warm.label(), "warm");
        assert_eq!(UpstreamDrainState::Draining.label(), "draining");
        assert_eq!(UpstreamDrainState::Drained.label(), "drained");
        assert!((UpstreamDrainState::Warm.code() - 0.0).abs() < f64::EPSILON);
        assert!((UpstreamDrainState::Drained.code() - 2.0).abs() < f64::EPSILON);
        assert_eq!(IdleDrainMode::Off.label(), "off");
        assert_eq!(IdleDrainMode::Observe.label(), "observe");
        assert_eq!(IdleDrainMode::Drain.label(), "drain");
    }
}
