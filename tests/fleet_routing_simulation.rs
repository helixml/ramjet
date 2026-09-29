//! Closed-loop simulation of prefix routing across fleets of 2 to 40 replicas.
//!
//! One balancer fronting many nodes changes what the affinity score sees. On a
//! two-replica box every replica eventually holds every application's shared
//! prompt; across twenty nodes most replicas hold only a few applications, and
//! a session's history lives on exactly one. This drives the real [`Router`]
//! with a coding-agent fleet — applications with shared system prompts larger
//! than the affinity cap, developers whose context grows every turn, bounded
//! per-replica caches, and overlapping request lifetimes on a virtual clock —
//! and reports what each affinity basis does at each fleet size.
//!
//! The router's own served-prefix index stands in for the engines' caches: it
//! is sized like an H200 TP4 KV pool and evicts least-recently served blocks.
//! That makes the recompute figure an estimate of the prefill each placement
//! costs, not an engine measurement; the stickiness and balance figures are
//! exact properties of the routing decisions.
//!
//! The default tests keep to fleet sizes a debug build simulates in seconds.
//! The 2-to-40-replica table takes minutes unoptimized, so it is ignored by
//! default; run it optimized:
//!
//! ```text
//! cargo test --release --test fleet_routing_simulation -- --ignored --nocapture
//! ```

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    sync::Arc,
};

use ramjet::{
    affinity_horizon::AffinityHorizonConfig,
    config::{Affinity, AffinityBasis, SpeculationProfile, SpeculationRouteMode},
    router::{LoadGuard, Router, RouterConfig},
};
use serde_json::{Value, json};
use url::Url;

/// Blocks are a quarter of production's 2KiB so the simulation hashes a
/// quarter of the bytes; every size below is expressed in blocks, which is all
/// the router scores, so the policy sees production-shaped prompts.
const CHUNK_BYTES: usize = 512;
/// About 2M tokens at roughly 500 tokens per production block: one H200 TP4
/// KV pool.
const CACHE_BLOCKS: usize = 4096;
const APPS: usize = 12;
/// A shared system prompt of 48 blocks (~96KiB in production), past the
/// 32-block cap.
const SYSTEM_BYTES: usize = 48 * CHUNK_BYTES;
/// Three blocks of tool output per turn.
const TURN_BYTES: usize = 3 * CHUNK_BYTES;
const TURNS_PER_SESSION: usize = 14;
const DEVELOPERS_PER_REPLICA: usize = 6;

/// Deterministic xorshift so every basis sees the identical workload.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low)
    }
}

fn router(replicas: usize, basis: AffinityBasis) -> Arc<Router> {
    Arc::new(Router::new(RouterConfig {
        upstreams: (0..replicas)
            .map(|index| {
                Url::parse(&format!("http://node-{}-{}:8000", index / 2, index % 2)).unwrap()
            })
            .collect(),
        alpha: 4.0,
        chunk_bytes: CHUNK_BYTES,
        max_prefix_bytes: 1024 * CHUNK_BYTES,
        max_overlap_blocks: 32,
        index_capacity: CACHE_BLOCKS,
        load_unit_bytes: 16 * CHUNK_BYTES,
        max_load_units: 8,
        projected_load: false,
        speculation_mode: SpeculationRouteMode::Off,
        speculation_profiles: vec![SpeculationProfile::Standard; replicas],
        affinity: Affinity::Prefix,
        affinity_horizon: AffinityHorizonConfig::off(),
        affinity_basis: basis,
        affinity_groups: Vec::new(),
    }))
}

fn filler(tag: &str, bytes: usize) -> String {
    let line = format!("{tag} lorem ipsum dolor sit amet consectetur\n");
    line.repeat(bytes / line.len() + 1)[..bytes].to_owned()
}

struct Developer {
    app: usize,
    session: u64,
    history: Vec<Value>,
    /// Replica that served this session's previous turn.
    home: Option<usize>,
}

struct Inflight {
    developer: usize,
    upstream: usize,
    fingerprints: Vec<u64>,
    _guard: LoadGuard,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Outcome {
    requests: u64,
    follow_ups: u64,
    stayed_home: u64,
    prompt_blocks: u64,
    recomputed_blocks: u64,
    busiest_share: f64,
}

#[allow(clippy::cast_precision_loss)] // Simulation counts stay far below 2^52.
fn ratio(numerator: u64, denominator: u64) -> f64 {
    numerator as f64 / denominator.max(1) as f64
}

impl Outcome {
    fn stickiness(&self) -> f64 {
        ratio(self.stayed_home, self.follow_ups)
    }

    fn recompute_ratio(&self) -> f64 {
        ratio(self.recomputed_blocks, self.prompt_blocks)
    }
}

enum Event {
    Arrive(usize),
    Complete(u64),
}

#[allow(clippy::too_many_lines)] // One event loop owns the whole fleet state.
fn simulate(replicas: usize, basis: AffinityBasis, simulated_ms: u64) -> Outcome {
    let router = router(replicas, basis);
    let systems = (0..APPS)
        .map(|app| filler(&format!("app {app} manual"), SYSTEM_BYTES))
        .collect::<Vec<_>>();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut developers = (0..replicas * DEVELOPERS_PER_REPLICA)
        .map(|index| Developer {
            // Skewed popularity: a few applications carry most developers.
            app: (index * index + index / 3) % APPS,
            session: index as u64,
            history: Vec::new(),
            home: None,
        })
        .collect::<Vec<_>>();
    let mut queue = BinaryHeap::new();
    let mut events = HashMap::new();
    let mut next_event = 0_u64;
    let mut schedule = |queue: &mut BinaryHeap<Reverse<(u64, u64)>>,
                        events: &mut HashMap<u64, Event>,
                        at: u64,
                        event: Event| {
        next_event += 1;
        events.insert(next_event, event);
        queue.push(Reverse((at, next_event)));
    };
    for developer in 0..developers.len() {
        let start = rng.range(0, 60_000);
        schedule(&mut queue, &mut events, start, Event::Arrive(developer));
    }
    let mut inflight: HashMap<u64, Inflight> = HashMap::new();
    let mut next_request = 0_u64;
    let mut served = vec![0_u64; replicas];
    let mut outcome = Outcome::default();

    while let Some(Reverse((now, id))) = queue.pop() {
        if now > simulated_ms {
            break;
        }
        match events.remove(&id).expect("scheduled event") {
            Event::Arrive(index) => {
                let developer = &mut developers[index];
                if developer.history.len() >= TURNS_PER_SESSION * 2 {
                    developer.session += developers_len_marker(replicas);
                    developer.history.clear();
                    developer.home = None;
                }
                let turn = developer.history.len() / 2;
                developer.history.push(json!({
                    "role": "user",
                    "content": filler(
                        &format!("session {} turn {turn} tool output", developer.session),
                        TURN_BYTES,
                    ),
                }));
                let mut messages =
                    vec![json!({"role": "system", "content": systems[developer.app]})];
                messages.extend(developer.history.iter().cloned());
                let body =
                    serde_json::to_vec(&json!({"model": "m", "messages": messages})).unwrap();
                let (decision, fingerprints) = router.route_with_fingerprints(&body);
                let upstream = decision.candidates[0];
                let chosen = decision
                    .candidate_state
                    .iter()
                    .find(|candidate| candidate.index == upstream)
                    .expect("winner has state");
                let uncached = fingerprints.len().saturating_sub(chosen.overlap_blocks);
                outcome.requests += 1;
                outcome.prompt_blocks += fingerprints.len() as u64;
                outcome.recomputed_blocks += uncached as u64;
                if let Some(home) = developer.home {
                    outcome.follow_ups += 1;
                    outcome.stayed_home += u64::from(home == upstream);
                }
                served[upstream] += 1;
                // ~40ms of prefill per uncached block, then a 1.5-4.5s decode.
                let service = 40 * uncached as u64 + rng.range(1_500, 4_500);
                next_request += 1;
                inflight.insert(
                    next_request,
                    Inflight {
                        developer: index,
                        upstream,
                        fingerprints,
                        _guard: router.acquire(upstream, decision.load_units),
                    },
                );
                schedule(
                    &mut queue,
                    &mut events,
                    now + service,
                    Event::Complete(next_request),
                );
            }
            Event::Complete(request) => {
                let done = inflight.remove(&request).expect("inflight request");
                router.observe(done.upstream, &done.fingerprints);
                let developer = &mut developers[done.developer];
                developer.home = Some(done.upstream);
                developer.history.push(json!({
                    "role": "assistant",
                    "content": format!("session {} reply {}", developer.session, developer.history.len()),
                }));
                // Tool execution and review between turns.
                let think = rng.range(2_000, 9_000);
                schedule(
                    &mut queue,
                    &mut events,
                    now + think,
                    Event::Arrive(done.developer),
                );
            }
        }
    }
    outcome.busiest_share = ratio(
        *served.iter().max().unwrap() * replicas as u64,
        outcome.requests,
    );
    outcome
}

/// Distinct session ids per developer without colliding with its peers.
fn developers_len_marker(replicas: usize) -> u64 {
    (replicas * DEVELOPERS_PER_REPLICA) as u64
}

const BASES: [AffinityBasis; 3] = [
    AffinityBasis::Absolute,
    AffinityBasis::Marginal,
    AffinityBasis::Relative,
];

const MINUTE_MS: u64 = 60 * 1000;

fn table(sizes: &[usize], simulated_ms: u64) -> HashMap<(usize, &'static str), Outcome> {
    // Each cell owns its router and workload, so cells run concurrently.
    let cells = std::thread::scope(|scope| {
        let handles = sizes
            .iter()
            .flat_map(|&replicas| BASES.map(|basis| (replicas, basis)))
            .map(|(replicas, basis)| {
                let handle = scope.spawn(move || simulate(replicas, basis, simulated_ms));
                (replicas, basis, handle)
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(replicas, basis, handle)| (replicas, basis, handle.join().unwrap()))
            .collect::<Vec<_>>()
    });
    eprintln!("replicas basis     requests stickiness recompute busiest/mean");
    for (replicas, basis, outcome) in &cells {
        eprintln!(
            "{replicas:>8} {:<9} {:>9} {:>9.1}% {:>8.1}% {:>12.2}",
            basis.label(),
            outcome.requests,
            100.0 * outcome.stickiness(),
            100.0 * outcome.recompute_ratio(),
            outcome.busiest_share,
        );
    }
    cells
        .into_iter()
        .map(|(replicas, basis, outcome)| ((replicas, basis.label()), outcome))
        .collect()
}

fn assert_relative_holds_sessions(
    results: &HashMap<(usize, &'static str), Outcome>,
    replicas: usize,
) {
    let relative = results[&(replicas, "relative")];
    for other in ["absolute", "marginal"] {
        let other = results[&(replicas, other)];
        assert!(
            relative.stickiness() > other.stickiness() + 0.05,
            "{replicas} replicas: relative {relative:?} vs {other:?}"
        );
        assert!(
            relative.recompute_ratio() < other.recompute_ratio(),
            "{replicas} replicas: relative {relative:?} vs {other:?}"
        );
    }
    // Affinity must not concentrate the fleet onto a few replicas.
    assert!(
        relative.busiest_share < 2.5,
        "{replicas} replicas: relative {relative:?}"
    );
}

#[test]
fn relative_and_marginal_are_the_same_policy_for_two_replicas() {
    assert_eq!(
        simulate(2, AffinityBasis::Marginal, 4 * MINUTE_MS),
        simulate(2, AffinityBasis::Relative, 4 * MINUTE_MS)
    );
}

#[test]
fn relative_keeps_sessions_home_past_two_replicas() {
    let results = table(&[12], 3 * MINUTE_MS);
    assert_relative_holds_sessions(&results, 12);
}

#[test]
#[ignore = "minutes unoptimized; run with --release --ignored"]
fn fleet_scaling_table() {
    let results = table(&[2, 4, 10, 20, 40], 12 * MINUTE_MS);
    for replicas in [10, 20, 40] {
        assert_relative_holds_sessions(&results, replicas);
    }
}
