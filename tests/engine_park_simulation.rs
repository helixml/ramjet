//! Closed-loop simulation of engine parking against scripted traffic.
//!
//! The node06 soak that motivated this could not prove what it needed to. A
//! production box supplies exactly one traffic pattern, only while you are
//! watching, and it changes underneath the experiment: the fleet the soak
//! started against had four engines and one busy replica, and by the time it
//! had run an hour it had three engines and a different distribution. Worse,
//! the interesting cases — a burst arriving at a parked replica, a sleep call
//! failing, the last warm replica dying while a peer is parked — are ones you
//! cannot ask a production fleet to perform.
//!
//! Everything the balancer *decides* is a pure function of observations and
//! time, so all of it can be simulated here: scripted arrivals, a virtual
//! clock, injected failures, and a fleet whose engines refuse work while
//! asleep. Each scenario is deterministic and runs in microseconds.
//!
//! What this cannot prove, and what still needs a real engine: that vLLM's
//! `/sleep` actually frees device memory on the pinned fork, how long a wake
//! takes, whether the offloaded weights fit in host RAM, and what parking
//! costs in lost prefix-cache residency. Those are engine and hardware facts.
//! This file covers the balancer's half, which is the half that has bugs.
//!
//! The fence rule is deliberately *not* reimplemented here. The simulation
//! calls [`ramjet::engine_park::fenced`], the same function the proxy applies,
//! so a test cannot pass against a copy of the rule that has drifted.

use ramjet::{
    engine_park::{
        EngineParkConfig, ParkAction, ParkActuator, ParkState, SleepLevel, fenced, plan,
    },
    idle_drain::{
        IdleDrainConfig, IdleDrainMode, IdleDrainPolicy, IdleDrainRelease, UpstreamObservation,
    },
};
use std::time::Duration;

/// How an engine responds to actuation in a given scenario.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EngineBehaviour {
    /// Sleeps and wakes immediately, the happy path.
    #[default]
    Prompt,
    /// Wakes only after this many ticks, modelling the host-to-device copy.
    SlowWake(u32),
    /// Every sleep call fails. The engine's real state stays awake, which is
    /// the case the balancer cannot distinguish and must not assume.
    SleepFails,
}

#[derive(Clone, Copy, Debug)]
struct SimEngine {
    healthy: bool,
    /// The engine's *actual* state, which the balancer only learns through
    /// call results and reconciliation.
    asleep: bool,
    behaviour: EngineBehaviour,
    wake_ticks_remaining: u32,
    inflight: usize,
}

impl SimEngine {
    fn new(behaviour: EngineBehaviour) -> Self {
        Self {
            healthy: true,
            asleep: false,
            behaviour,
            wake_ticks_remaining: 0,
            inflight: 0,
        }
    }
}

/// One tick's outcome, for assertions.
#[derive(Debug, Default)]
struct TickReport {
    /// Requests the simulation tried to dispatch to a sleeping engine. Must
    /// always be zero: this is the failure the whole fencing design prevents.
    dispatched_into_sleeping: usize,
    /// Requests that found no routable replica at all.
    dropped: usize,
    sleeps: usize,
    wakes: usize,
}

struct Simulation {
    policy: IdleDrainPolicy,
    park: Vec<ParkState>,
    engines: Vec<SimEngine>,
    config: EngineParkConfig,
    now_ms: u64,
    tick_ms: u64,
    total: TickReport,
    park_events: Vec<(u64, usize, ParkState)>,
}

impl Simulation {
    fn new(policy_config: IdleDrainConfig, park_config: EngineParkConfig, engines: usize) -> Self {
        Self {
            policy: IdleDrainPolicy::new(policy_config, engines, 0),
            park: vec![ParkState::Awake; engines],
            engines: vec![SimEngine::new(EngineBehaviour::Prompt); engines],
            config: park_config,
            now_ms: 0,
            tick_ms: 15_000,
            total: TickReport::default(),
            park_events: Vec::new(),
        }
    }

    fn with_behaviour(mut self, upstream: usize, behaviour: EngineBehaviour) -> Self {
        self.engines[upstream].behaviour = behaviour;
        self
    }

    /// Advances one policy interval.
    ///
    /// `arrivals` is the in-flight request count each replica *would* carry if
    /// it were routable; a fenced replica's share is redistributed to the
    /// replicas that are still serving, which is what the router does.
    fn tick(&mut self, arrivals: &[usize]) -> TickReport {
        let mut report = TickReport::default();

        // Finish any wake that was in progress before this round's routing, so
        // a replica that has completed its transfer can serve immediately.
        for engine in &mut self.engines {
            if engine.wake_ticks_remaining > 0 {
                engine.wake_ticks_remaining -= 1;
                if engine.wake_ticks_remaining == 0 {
                    engine.asleep = false;
                }
            }
        }

        // Route. The fence expression is the production one.
        let intents = self.policy_intents();
        let routable: Vec<usize> = (0..self.engines.len())
            .filter(|index| {
                !fenced(intents[*index], self.park[*index]) && self.engines[*index].healthy
            })
            .collect();
        let offered: usize = arrivals.iter().sum();
        for engine in &mut self.engines {
            engine.inflight = 0;
        }
        if routable.is_empty() {
            report.dropped += offered;
        } else {
            // Model the prefix router: each arrival has a preferred replica —
            // the one holding its warm prefix — and only falls elsewhere when
            // that replica is not routable. Round-robining every arrival
            // instead would erase the very pattern these scenarios are about,
            // because a workload pinned to one replica would look evenly
            // spread the moment nothing was parked.
            let mut fallback = 0usize;
            for (preferred, count) in arrivals.iter().enumerate() {
                for _ in 0..*count {
                    let target = if routable.contains(&preferred) {
                        preferred
                    } else {
                        let picked = routable[fallback % routable.len()];
                        fallback += 1;
                        picked
                    };
                    self.engines[target].inflight += 1;
                    if self.engines[target].asleep {
                        // The invariant this whole design exists to protect.
                        report.dispatched_into_sleeping += 1;
                    }
                }
            }
        }
        if offered > 0 {
            self.policy.observe_activity(self.now_ms);
        }

        // Tick the policy against what the fleet now looks like.
        let observations: Vec<UpstreamObservation> = self
            .engines
            .iter()
            .map(|engine| UpstreamObservation {
                healthy: engine.healthy,
                inflight: engine.inflight,
            })
            .collect();
        let decision = self.policy.tick(self.now_ms, &observations);

        // Plan and apply actuation, exactly as the proxy does.
        let actions = plan(&decision.upstreams, &self.park, &self.config);
        for (upstream, action) in actions.iter().enumerate() {
            let Some(action) = action else { continue };
            match action {
                ParkAction::Sleep => {
                    self.park[upstream] = ParkState::Parking;
                    report.sleeps += 1;
                    if self.engines[upstream].behaviour == EngineBehaviour::SleepFails {
                        self.park[upstream] = ParkState::Unknown;
                    } else {
                        self.engines[upstream].asleep = true;
                        self.park[upstream] = ParkState::Parked;
                    }
                }
                ParkAction::Wake => {
                    self.park[upstream] = ParkState::Waking;
                    report.wakes += 1;
                    if let EngineBehaviour::SlowWake(ticks) = self.engines[upstream].behaviour {
                        self.engines[upstream].wake_ticks_remaining = ticks;
                    } else {
                        self.engines[upstream].asleep = false;
                        self.park[upstream] = ParkState::Awake;
                    }
                }
                ParkAction::Reconcile => {
                    // `/is_sleeping` reports the engine's real state.
                    self.park[upstream] = if self.engines[upstream].asleep {
                        ParkState::Parked
                    } else {
                        ParkState::Awake
                    };
                }
            }
            self.park_events
                .push((self.now_ms, upstream, self.park[upstream]));
        }

        // A slow wake that has now finished must clear its own park state.
        for (upstream, engine) in self.engines.iter().enumerate() {
            if self.park[upstream] == ParkState::Waking && engine.wake_ticks_remaining == 0 {
                self.park[upstream] = ParkState::Awake;
            }
        }

        self.now_ms += self.tick_ms;
        self.total.dispatched_into_sleeping += report.dispatched_into_sleeping;
        self.total.dropped += report.dropped;
        self.total.sleeps += report.sleeps;
        self.total.wakes += report.wakes;
        report
    }

    /// Re-derives the current intents without advancing the state machine.
    fn policy_intents(&self) -> Vec<ramjet::idle_drain::UpstreamIntent> {
        (0..self.engines.len())
            .map(|index| {
                let state = self.policy.state(index).unwrap_or_default();
                ramjet::idle_drain::UpstreamIntent {
                    state,
                    fenced: state.fenced() && self.policy.config().mode.fences_routing(),
                    desired_running: !state.fenced(),
                    safe_to_stop: false,
                }
            })
            .collect()
    }

    fn parked_count(&self) -> usize {
        self.park
            .iter()
            .filter(|state| **state == ParkState::Parked)
            .count()
    }
}

fn utilization_config() -> IdleDrainConfig {
    IdleDrainConfig {
        mode: IdleDrainMode::Drain,
        release: IdleDrainRelease::Utilization,
        idle_after: Duration::from_mins(15),
        min_warm: 1,
        cooldown: Duration::from_mins(5),
        drain_grace: Duration::from_secs(30),
        upstream_idle_after: Duration::from_mins(5),
        resume_load_per_replica: 4,
    }
}

fn park_config(max_parked: usize) -> EngineParkConfig {
    EngineParkConfig {
        actuator: ParkActuator::Sleep,
        level: SleepLevel::Offload,
        max_parked,
    }
}

/// The measured node06 workload: continuous light traffic that the prefix
/// router pins to a single replica, with the rest idle but resident.
///
/// This is the pattern the production soak could not act on, because
/// fleet-idle release requires the *fleet* to be quiet and this fleet never
/// is. It is also the pattern where index-based target selection would park
/// the only engine doing work.
#[test]
fn the_node06_pattern_parks_idle_replicas_and_never_the_busy_one() {
    let mut sim = Simulation::new(utilization_config(), park_config(3), 4);
    for _ in 0..200 {
        // All traffic on replica 3, concurrency 2, forever.
        let report = sim.tick(&[0, 0, 0, 2]);
        assert_eq!(report.dispatched_into_sleeping, 0);
        assert_eq!(report.dropped, 0);
        assert_ne!(
            sim.park[3],
            ParkState::Parked,
            "the replica carrying the workload must never be parked"
        );
    }
    assert!(
        sim.parked_count() >= 1,
        "at least one idle replica should have been released, got {:?}",
        sim.park
    );
}

/// The cap is about host memory, not replica count: level-1 sleep offloads
/// weights into host RAM, and node06 fits exactly one.
#[test]
fn the_park_cap_bounds_how_many_replicas_hold_offloaded_weights() {
    let mut sim = Simulation::new(utilization_config(), park_config(1), 4);
    for _ in 0..200 {
        sim.tick(&[0, 0, 0, 2]);
        assert!(sim.parked_count() <= 1, "cap exceeded: {:?}", sim.park);
    }
}

/// A burst arriving at a shrunken fleet must restore capacity, and must not be
/// served by a replica whose weights are still moving.
#[test]
fn a_burst_wakes_the_fleet_without_dispatching_into_a_waking_replica() {
    let mut sim = Simulation::new(utilization_config(), park_config(3), 4)
        .with_behaviour(0, EngineBehaviour::SlowWake(4))
        .with_behaviour(1, EngineBehaviour::SlowWake(4))
        .with_behaviour(2, EngineBehaviour::SlowWake(4));
    for _ in 0..120 {
        sim.tick(&[0, 0, 0, 2]);
    }
    assert!(sim.parked_count() >= 1, "expected a park before the burst");

    for _ in 0..40 {
        let report = sim.tick(&[0, 0, 0, 40]);
        assert_eq!(
            report.dispatched_into_sleeping, 0,
            "a waking replica must stay fenced until its transfer completes"
        );
    }
    assert_eq!(
        sim.parked_count(),
        0,
        "the burst must restore every replica"
    );
}

/// The anti-flap invariant, stated as an outcome rather than as a rule: hold
/// load just below the resume threshold for a long time and count transitions.
#[test]
fn sustained_load_near_the_resume_threshold_does_not_oscillate() {
    let mut sim = Simulation::new(utilization_config(), park_config(3), 4);
    // Three requests spread over the fleet sits just under the threshold of
    // four, which is exactly where a naive policy parks and wakes forever.
    for _ in 0..400 {
        sim.tick(&[1, 1, 1, 0]);
    }
    let transitions = sim.park_events.len();
    assert!(
        transitions <= 4,
        "expected a stable outcome, saw {transitions} actuations: {:?}",
        sim.park_events
    );
    assert_eq!(sim.total.dispatched_into_sleeping, 0);
}

/// A sleep call that fails leaves the engine's real state unknown. The
/// balancer must not assume it stayed awake, because it may not have.
#[test]
fn a_failed_sleep_never_leads_to_dispatch_into_a_sleeping_engine() {
    let mut sim = Simulation::new(utilization_config(), park_config(3), 4)
        .with_behaviour(0, EngineBehaviour::SleepFails)
        .with_behaviour(1, EngineBehaviour::SleepFails)
        .with_behaviour(2, EngineBehaviour::SleepFails);
    for _ in 0..200 {
        let report = sim.tick(&[0, 0, 0, 2]);
        assert_eq!(report.dispatched_into_sleeping, 0);
        assert_eq!(
            report.dropped, 0,
            "the busy replica keeps serving throughout"
        );
    }
}

/// The failure that makes a safe park retroactively unsafe: the replica that
/// stayed warm dies while its peer is parked. Nothing arrives to notice,
/// because during a quiet window nothing arrives at all.
#[test]
fn losing_the_warm_replica_restores_a_parked_one_without_waiting_for_traffic() {
    let mut sim = Simulation::new(utilization_config(), park_config(3), 4);
    for _ in 0..120 {
        sim.tick(&[0, 0, 0, 2]);
    }
    assert!(sim.parked_count() >= 1);

    // The serving replica fails. No further traffic arrives.
    sim.engines[3].healthy = false;
    for _ in 0..40 {
        sim.tick(&[0, 0, 0, 0]);
    }
    let serving = (0..4)
        .filter(|index| sim.engines[*index].healthy && sim.park[*index] == ParkState::Awake)
        .count();
    assert!(
        serving >= 1,
        "the warm floor must be re-established from the parked set: {:?}",
        sim.park
    );
}

/// Fleet-idle release is the conservative floor and must stay conservative:
/// under the same continuous traffic it parks nothing at all.
#[test]
fn fleet_idle_release_parks_nothing_under_continuous_traffic() {
    let mut config = utilization_config();
    config.release = IdleDrainRelease::FleetIdle;
    let mut sim = Simulation::new(config, park_config(3), 4);
    for _ in 0..400 {
        sim.tick(&[0, 0, 0, 2]);
    }
    assert_eq!(sim.parked_count(), 0);
    assert_eq!(sim.total.sleeps, 0);
    assert_eq!(sim.total.dispatched_into_sleeping, 0);
}

/// A genuinely idle fleet is the case fleet-idle release exists for.
#[test]
fn fleet_idle_release_parks_down_to_the_warm_floor_when_nothing_arrives() {
    let mut config = utilization_config();
    config.release = IdleDrainRelease::FleetIdle;
    let mut sim = Simulation::new(config, park_config(3), 4);
    for _ in 0..400 {
        sim.tick(&[0, 0, 0, 0]);
    }
    assert!(
        sim.parked_count() >= 1,
        "an idle fleet should shrink: {:?}",
        sim.park
    );
    assert_eq!(sim.total.dispatched_into_sleeping, 0);
}

/// Traffic that moves between replicas over a long horizon: the pattern is not
/// stationary, so a replica that goes quiet and then busy again must not be
/// parked into.
#[test]
fn a_shifting_traffic_pattern_never_parks_a_replica_that_is_about_to_serve() {
    let mut sim = Simulation::new(utilization_config(), park_config(2), 4);
    for round in 0..600 {
        // The hot replica rotates every 100 ticks, as a prefix cache would
        // move if the working set changed.
        let hot = (round / 100) % 4;
        let mut arrivals = [0usize; 4];
        arrivals[hot] = 2;
        let report = sim.tick(&arrivals);
        assert_eq!(report.dispatched_into_sleeping, 0);
        assert_eq!(report.dropped, 0);
    }
}
