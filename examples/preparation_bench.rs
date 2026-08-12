use std::{hint::black_box, time::Instant};

use mini_dynamo::{
    config::Affinity,
    prepare::PreparedRequest,
    router::{Router, RouterConfig},
    shims::{Endpoint, sanitize_request},
};
use serde::Serialize;
use url::Url;

#[derive(Serialize)]
struct ResultRow {
    body_bytes: usize,
    iterations: u32,
    single_parse_us: f64,
    legacy_two_parse_us: f64,
    speedup: f64,
}

fn main() {
    for target_bytes in [256 << 10, 2 << 20] {
        let body = format!(
            r#"{{"messages":[{{"role":"system","content":"{}"}},{{"role":"user","content":"summarize"}}],"max_tokens":256}}"#,
            "long-context-ledger-".repeat(target_bytes / 20)
        )
        .into_bytes();
        let iterations = if target_bytes < 1 << 20 {
            100_u32
        } else {
            20_u32
        };
        let router = router();

        for _ in 0..3 {
            black_box(PreparedRequest::new(
                Endpoint::Chat,
                &body,
                100_000,
                &router,
            ));
        }
        let started = Instant::now();
        for _ in 0..iterations {
            let prepared = PreparedRequest::new(Endpoint::Chat, &body, 100_000, &router);
            black_box(prepared.route(&router));
            black_box(prepared.fingerprints);
        }
        let single_parse = started.elapsed().as_secs_f64();

        let started = Instant::now();
        for _ in 0..iterations {
            let sanitized = sanitize_request(Endpoint::Chat, &body, 100_000);
            let (decision, fingerprints) = router.route_with_fingerprints(&sanitized);
            black_box(decision);
            black_box(fingerprints);
        }
        let legacy = started.elapsed().as_secs_f64();
        let single_us = single_parse * 1_000_000.0 / f64::from(iterations);
        let legacy_us = legacy * 1_000_000.0 / f64::from(iterations);
        println!(
            "{}",
            serde_json::to_string(&ResultRow {
                body_bytes: body.len(),
                iterations,
                single_parse_us: single_us,
                legacy_two_parse_us: legacy_us,
                speedup: legacy_us / single_us,
            })
            .expect("serialize result")
        );
    }
}

fn router() -> Router {
    Router::new(RouterConfig {
        upstreams: vec![Url::parse("http://engine:8000").expect("test URL")],
        alpha: 4.0,
        chunk_bytes: 2_048,
        max_prefix_bytes: 2 << 20,
        max_overlap_blocks: 32,
        index_capacity: 100_000,
        load_unit_bytes: 32 << 10,
        max_load_units: 8,
        affinity: Affinity::Prefix,
    })
}
