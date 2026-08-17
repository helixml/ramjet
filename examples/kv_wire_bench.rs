use std::{hint::black_box, time::Instant};

use ramjet::kv_wire::{KvWireLimits, decode_batch};
use serde::Serialize;
use serde_json::json;

#[derive(Serialize)]
struct ResultRow {
    payload_bytes: usize,
    token_ids: usize,
    block_hashes: usize,
    iterations: u32,
    decode_us: f64,
    payload_mib_per_second: f64,
    token_ids_per_second: f64,
}

fn main() {
    for (token_ids, iterations) in [(256_usize, 10_000_u32), (18_944, 1_000), (82_176, 250)] {
        let block_size = 256_usize;
        let block_hashes = token_ids.div_ceil(block_size);
        let hashes: Vec<u64> = (0..block_hashes)
            .map(|index| u64::try_from(index).expect("benchmark hash index"))
            .collect();
        let tokens: Vec<u32> = (0..token_ids)
            .map(|index| u32::try_from(index % 129_280).expect("benchmark token ID"))
            .collect();
        let payload = rmp_serde::to_vec(&json!([
            1.5,
            [{
                "type": "BlockStored",
                "block_hashes": hashes,
                "parent_block_hash": null,
                "token_ids": tokens,
                "block_size": block_size,
                "medium": "GPU",
                "group_idx": 0,
                "kv_cache_spec_kind": "full"
            }],
            0
        ]))
        .expect("encode synthetic event batch");

        for _ in 0..10 {
            black_box(decode_batch(&payload, KvWireLimits::default()).expect("warm-up decode"));
        }
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(decode_batch(&payload, KvWireLimits::default()).expect("benchmark decode"));
        }
        let elapsed = started.elapsed().as_secs_f64();
        let decode_us = elapsed * 1_000_000.0 / f64::from(iterations);
        let payload_bytes = u32::try_from(payload.len()).expect("benchmark payload size");
        let token_count = u32::try_from(token_ids).expect("benchmark token count");
        let total_bytes = f64::from(payload_bytes) * f64::from(iterations);
        let total_tokens = f64::from(token_count) * f64::from(iterations);
        println!(
            "{}",
            serde_json::to_string(&ResultRow {
                payload_bytes: payload.len(),
                token_ids,
                block_hashes,
                iterations,
                decode_us,
                payload_mib_per_second: total_bytes / elapsed / (1024.0 * 1024.0),
                token_ids_per_second: total_tokens / elapsed,
            })
            .expect("serialize result")
        );
    }
}
