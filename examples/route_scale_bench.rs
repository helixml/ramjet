//! Routing cost against fleet size.
//!
//! Scoring walks every replica's served-prefix index under one lock, so its
//! cost grows with replicas x prompt blocks. This measures one decision at
//! 1-40 replicas in the worst case for that walk: a 1MiB agent prompt whose
//! whole prefix is warm on every replica, so no walk stops early. Fingerprint
//! preparation (JSON parse and hashing) is timed separately because it runs
//! before the lock and does not depend on the fleet.
//!
//! ```text
//! cargo run --release --locked --example route_scale_bench
//! ```

use std::time::{Duration, Instant};

use ramjet::{
    affinity_horizon::AffinityHorizonConfig,
    config::{Affinity, AffinityBasis, SpeculationProfile, SpeculationRouteMode},
    router::{Router, RouterConfig},
};
use serde_json::json;
use url::Url;

fn router(replicas: usize) -> Router {
    Router::new(RouterConfig {
        upstreams: (0..replicas)
            .map(|index| Url::parse(&format!("http://replica-{index}:8000")).unwrap())
            .collect(),
        alpha: 4.0,
        chunk_bytes: 2048,
        max_prefix_bytes: 2 << 20,
        max_overlap_blocks: 32,
        index_capacity: 100_000,
        load_unit_bytes: 32 << 10,
        max_load_units: 8,
        projected_load: false,
        speculation_mode: SpeculationRouteMode::Off,
        speculation_profiles: vec![SpeculationProfile::Standard; replicas],
        affinity: Affinity::Prefix,
        affinity_horizon: AffinityHorizonConfig::off(),
        affinity_basis: AffinityBasis::Relative,
        affinity_groups: Vec::new(),
    })
}

fn median_micros(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1e6
}

fn main() {
    let history = "tool output line with some code in it\n".repeat(26_000);
    let body = serde_json::to_vec(&json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "shared manual\n".repeat(8_000)},
            {"role": "user", "content": history},
        ],
    }))
    .unwrap();
    println!(
        "prompt {} KiB; median microseconds per decision (route excludes fingerprinting)",
        body.len() >> 10
    );
    for replicas in [1, 2, 4, 10, 20, 40] {
        let router = router(replicas);
        let fingerprints = router.fingerprints(&body);
        for upstream in 0..replicas {
            router.observe(upstream, &fingerprints);
        }
        let iterations = 400;
        let mut route_times = Vec::with_capacity(iterations);
        let mut fingerprint_times = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            let prepared = router.fingerprints(&body);
            let fingerprinted = Instant::now();
            let (decision, _) = router.route_with_fingerprints(&body);
            let total = started.elapsed();
            std::hint::black_box((prepared, decision));
            fingerprint_times.push(fingerprinted - started);
            // route_with_fingerprints prepares again; subtract that share. The
            // difference of two noisy timings is only meaningful at the median.
            route_times.push(total.saturating_sub(2 * (fingerprinted - started)));
        }
        println!(
            "{replicas:>3} replicas, {} blocks: route {:>6.0}  fingerprint {:>6.0}",
            fingerprints.len(),
            median_micros(route_times),
            median_micros(fingerprint_times),
        );
    }
}
