//! Engine parking actuation for the idle-drain policy.
//!
//! [`crate::idle_drain`] decides *whether* a replica should be parked. This
//! module decides *how*, and is the first actuator the load balancer is
//! allowed to carry out itself.
//!
//! That split originally existed because converging containers needs a Docker
//! socket, which is root-equivalent on the host, and the balancer sits in the
//! production request path. vLLM's sleep mode removes the objection rather
//! than working around it: parking becomes `POST /sleep` and resuming becomes
//! `POST /wake_up`, authenticated with the same upstream bearer token the
//! balancer already holds for its readiness probes. The authority gained is
//! reversible, engine-scoped, and strictly smaller than the routing authority
//! the balancer already exercises every request.
//!
//! Two invariants matter more than the state machine:
//!
//! * A parked or waking replica stays fenced from routing regardless of what
//!   the policy currently intends. Unfencing a replica whose weights are still
//!   in host memory would dispatch real traffic into a sleeping engine, and
//!   the policy's own resume path is deliberately immediate — it cannot wait
//!   for a device copy that has not happened yet.
//! * Level-1 sleep offloads weights to host RAM, so parking is bounded by a
//!   concurrency cap as well as by the warm floor. On node06 one Qwen3.8-27B
//!   engine is roughly 27GB against 30GiB available after the ZFS ARC cap:
//!   one replica may sleep, four may not. The warm floor alone does not
//!   express that, because it counts replicas rather than host memory.
//!
//! Actuation never reports failure into `/health` or upstream health. A
//! replica whose sleep or wake call failed is fenced and reconciled, not
//! marked down: the engine is still serving, and a balancer that cannot park
//! an idle replica has lost an optimisation, not a capability.

use crate::idle_drain::UpstreamIntent;

/// How the balancer converges a replica onto the policy's intent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParkActuator {
    /// Publish intent only and leave convergence to an external actor. This is
    /// the historic behaviour, retained for deployments whose parking is
    /// container stop/start rather than engine sleep.
    Off,
    /// Park with vLLM sleep mode over the engine's own authenticated HTTP API.
    #[default]
    Sleep,
}

impl ParkActuator {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Sleep => "sleep",
        }
    }

    /// Whether this balancer issues sleep and wake calls itself.
    #[must_use]
    pub const fn actuates(self) -> bool {
        matches!(self, Self::Sleep)
    }
}

/// vLLM sleep depth, as accepted by `POST /sleep?level=`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SleepLevel {
    /// Offload weights to host memory and discard the KV cache. Waking is a
    /// host-to-device copy, so it is fast but the offloaded weights occupy
    /// host RAM for the whole parked window.
    #[default]
    Offload,
    /// Discard weights as well as the KV cache. Costs no host RAM while
    /// parked, but waking re-reads the model, which a small page cache or a
    /// capped ZFS ARC will not absorb.
    Discard,
}

impl SleepLevel {
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Offload => 1,
            Self::Discard => 2,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Offload => "offload",
            Self::Discard => "discard",
        }
    }
}

/// Tuning for the actuator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineParkConfig {
    pub actuator: ParkActuator,
    pub level: SleepLevel,
    /// Replicas that may be parked at once. Bounds host-memory consumption of
    /// level-1 offload independently of the warm floor.
    pub max_parked: usize,
}

impl Default for EngineParkConfig {
    fn default() -> Self {
        Self {
            actuator: ParkActuator::Sleep,
            level: SleepLevel::Offload,
            max_parked: 1,
        }
    }
}

/// What this balancer believes about a replica's parked state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParkState {
    /// Weights are resident and the replica may serve.
    #[default]
    Awake,
    /// A sleep call is in flight.
    Parking,
    /// The engine acknowledged sleep.
    Parked,
    /// A wake call is in flight.
    Waking,
    /// The last actuation failed, so the real state is not known. Treated as
    /// unroutable until a reconciliation observes `/is_sleeping`.
    Unknown,
}

impl ParkState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Awake => "awake",
            Self::Parking => "parking",
            Self::Parked => "parked",
            Self::Waking => "waking",
            Self::Unknown => "unknown",
        }
    }

    /// Stable numeric encoding for the exported gauge.
    #[must_use]
    pub const fn code(self) -> f64 {
        match self {
            Self::Awake => 0.0,
            Self::Parking => 1.0,
            Self::Parked => 2.0,
            Self::Waking => 3.0,
            Self::Unknown => 4.0,
        }
    }

    /// Whether routing must withhold this replica irrespective of policy
    /// intent.
    ///
    /// `Parking` is included because the sleep call may already have taken
    /// effect on the engine by the time the balancer sees an error or a
    /// timeout; `Unknown` because a failed call proves nothing either way.
    #[must_use]
    pub const fn must_fence(self) -> bool {
        matches!(
            self,
            Self::Parking | Self::Parked | Self::Waking | Self::Unknown
        )
    }

    /// Whether the replica is holding host memory for offloaded weights.
    #[must_use]
    pub const fn occupies_park_slot(self) -> bool {
        matches!(self, Self::Parking | Self::Parked)
    }
}

/// One actuation the balancer should perform this round.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkAction {
    /// `POST /sleep?level=&mode=abort`.
    Sleep,
    /// `POST /wake_up`.
    Wake,
    /// `GET /is_sleeping`, to recover from an unknown state.
    Reconcile,
}

impl ParkAction {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Wake => "wake",
            Self::Reconcile => "reconcile",
        }
    }
}

/// Chooses this round's actuations from the policy's intent and the balancer's
/// current belief about each replica.
///
/// Resuming is evaluated before parking and is never rate-limited or capped,
/// mirroring the asymmetry in the policy itself: being short of capacity costs
/// a wake latency on real traffic, while parking late costs watt-minutes.
///
/// Returns one optional action per upstream, in upstream order, so the caller
/// can dispatch them without re-deriving indices.
#[must_use]
pub fn plan(
    intents: &[UpstreamIntent],
    states: &[ParkState],
    config: &EngineParkConfig,
) -> Vec<Option<ParkAction>> {
    let mut actions = vec![None; intents.len()];
    if !config.actuator.actuates() {
        return actions;
    }

    // Resume first. A replica the policy wants running must leave the parked
    // set before anything else is allowed to enter it, so a fleet flipping
    // from idle to busy never transiently exceeds the cap.
    for (upstream, intent) in intents.iter().enumerate() {
        let Some(state) = states.get(upstream).copied() else {
            continue;
        };
        if !intent.desired_running {
            continue;
        }
        actions[upstream] = match state {
            ParkState::Parked | ParkState::Parking => Some(ParkAction::Wake),
            ParkState::Unknown => Some(ParkAction::Reconcile),
            ParkState::Awake | ParkState::Waking => None,
        };
    }

    // Park only what remains within the cap, counting replicas already parked
    // or mid-sleep so concurrent rounds cannot oversubscribe host memory.
    let mut occupied = states
        .iter()
        .filter(|state| state.occupies_park_slot())
        .count();
    for (upstream, intent) in intents.iter().enumerate() {
        if actions[upstream].is_some() {
            continue;
        }
        let Some(state) = states.get(upstream).copied() else {
            continue;
        };
        if intent.desired_running || !intent.safe_to_stop || state != ParkState::Awake {
            continue;
        }
        if occupied >= config.max_parked {
            continue;
        }
        actions[upstream] = Some(ParkAction::Sleep);
        occupied += 1;
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(desired_running: bool, safe_to_stop: bool) -> UpstreamIntent {
        UpstreamIntent {
            state: crate::idle_drain::UpstreamDrainState::Warm,
            fenced: !desired_running,
            desired_running,
            safe_to_stop,
        }
    }

    fn config(max_parked: usize) -> EngineParkConfig {
        EngineParkConfig {
            actuator: ParkActuator::Sleep,
            level: SleepLevel::Offload,
            max_parked,
        }
    }

    #[test]
    fn actuator_off_plans_nothing_even_when_the_policy_wants_a_park() {
        let intents = [intent(true, false), intent(false, true)];
        let states = [ParkState::Awake, ParkState::Awake];
        let plan = plan(
            &intents,
            &states,
            &EngineParkConfig {
                actuator: ParkActuator::Off,
                ..config(1)
            },
        );
        assert_eq!(plan, vec![None, None]);
    }

    #[test]
    fn a_replica_the_policy_released_is_slept_once_it_is_safe_to_stop() {
        let intents = [intent(true, false), intent(false, true)];
        let states = [ParkState::Awake, ParkState::Awake];
        assert_eq!(
            plan(&intents, &states, &config(1)),
            vec![None, Some(ParkAction::Sleep)]
        );
    }

    #[test]
    fn a_fenced_replica_is_not_slept_until_its_drain_grace_has_elapsed() {
        // `safe_to_stop` is the policy's statement that inflight work has
        // finished; sleeping before it would abort live requests.
        let intents = [intent(true, false), intent(false, false)];
        let states = [ParkState::Awake, ParkState::Awake];
        assert_eq!(plan(&intents, &states, &config(1)), vec![None, None]);
    }

    #[test]
    fn the_concurrency_cap_bounds_host_memory_independently_of_the_warm_floor() {
        // Three releasable replicas, but level-1 offload only has room for one.
        let intents = [
            intent(false, true),
            intent(false, true),
            intent(false, true),
        ];
        let states = [ParkState::Awake; 3];
        assert_eq!(
            plan(&intents, &states, &config(1)),
            vec![Some(ParkAction::Sleep), None, None]
        );
    }

    #[test]
    fn an_already_parked_replica_occupies_its_slot() {
        let intents = [intent(false, true), intent(false, true)];
        let states = [ParkState::Parked, ParkState::Awake];
        assert_eq!(plan(&intents, &states, &config(1)), vec![None, None]);
    }

    #[test]
    fn an_in_flight_sleep_occupies_its_slot_so_rounds_cannot_oversubscribe() {
        let intents = [intent(false, true), intent(false, true)];
        let states = [ParkState::Parking, ParkState::Awake];
        assert_eq!(plan(&intents, &states, &config(1)), vec![None, None]);
    }

    #[test]
    fn resuming_is_immediate_and_ignores_the_cap() {
        let intents = [intent(true, false), intent(true, false)];
        let states = [ParkState::Parked, ParkState::Parked];
        assert_eq!(
            plan(&intents, &states, &config(1)),
            vec![Some(ParkAction::Wake), Some(ParkAction::Wake)]
        );
    }

    #[test]
    fn a_wake_frees_its_slot_before_another_replica_may_park_in_the_same_round() {
        // Replica 0 is being resumed while replica 1 becomes releasable. The
        // cap is one, and 0 still holds the slot, so 1 must wait a round
        // rather than briefly doubling offloaded weights in host memory.
        let intents = [intent(true, false), intent(false, true)];
        let states = [ParkState::Parked, ParkState::Awake];
        assert_eq!(
            plan(&intents, &states, &config(1)),
            vec![Some(ParkAction::Wake), None]
        );
    }

    #[test]
    fn a_failed_actuation_is_reconciled_rather_than_assumed_awake() {
        let intents = [intent(true, false)];
        let states = [ParkState::Unknown];
        assert_eq!(
            plan(&intents, &states, &config(1)),
            vec![Some(ParkAction::Reconcile)]
        );
    }

    #[test]
    fn an_unknown_replica_is_never_slept_because_it_may_already_be_asleep() {
        let intents = [intent(false, true)];
        let states = [ParkState::Unknown];
        assert_eq!(plan(&intents, &states, &config(1)), vec![None]);
    }

    #[test]
    fn a_wake_already_in_flight_is_not_reissued() {
        let intents = [intent(true, false)];
        let states = [ParkState::Waking];
        assert_eq!(plan(&intents, &states, &config(1)), vec![None]);
    }

    #[test]
    fn every_state_that_may_be_mid_transition_fences_routing() {
        assert!(!ParkState::Awake.must_fence());
        for state in [
            ParkState::Parking,
            ParkState::Parked,
            ParkState::Waking,
            ParkState::Unknown,
        ] {
            assert!(state.must_fence(), "{} must fence", state.label());
        }
    }

    #[test]
    fn state_codes_and_labels_are_unique() {
        let states = [
            ParkState::Awake,
            ParkState::Parking,
            ParkState::Parked,
            ParkState::Waking,
            ParkState::Unknown,
        ];
        let mut codes = states.iter().map(|state| state.code()).collect::<Vec<_>>();
        codes.sort_by(f64::total_cmp);
        codes.dedup();
        assert_eq!(codes.len(), states.len());
        let mut labels = states.iter().map(|state| state.label()).collect::<Vec<_>>();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), states.len());
    }

    #[test]
    fn sleep_levels_map_to_the_documented_vllm_wire_values() {
        assert_eq!(SleepLevel::Offload.wire(), 1);
        assert_eq!(SleepLevel::Discard.wire(), 2);
    }
}
