//! GPU-free replay of node06's captured 192-batch eviction burst.
//!
//! Run with:
//! `cargo run --release --locked --example companion_eviction_bench`.
//!
//! `apply_live` owns the source mutex for the complete measured call. With no
//! subscribers or competing callers, its elapsed time is a conservative upper
//! bound on the source-lock hold rather than a scheduler-contention benchmark.

use std::{hint::black_box, time::Instant};

use bytes::Bytes;
use ramjet::{
    companion_index_source::{CompanionIndexSource, CompanionIndexSourceConfig},
    digest_index::DigestIndexLimits,
    kv_snapshot::{
        AttentionKind, EngineIncarnation, GroupDisposition, GroupMetadata, SnapshotLimits,
    },
    kv_transport::SequencedBatch,
    kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
};

const SECRET: [u8; 32] = *b"0123456789abcdef0123456789abcdef";
const BLOCK_SIZE: usize = 256;
const STORED_MAIN_BLOCKS: usize = 3_456;
const RETAINED_MAIN_BLOCKS: usize = 2_574;
const BATCHES: usize = 192;
const GROUP_REMOVALS: [usize; 5] = [882, 195, 195, 130, 1_040];
const DEFAULT_ITERATIONS: usize = 20;

fn main() {
    let iterations = std::env::var("ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ITERATIONS);
    let batches = captured_eviction_batches();
    let mut samples_us = Vec::with_capacity(iterations * BATCHES);

    for _ in 0..iterations {
        let source = ready_source();
        let mut removed = 0_usize;
        let mut filtered = 0_usize;
        for batch in &batches {
            let started = Instant::now();
            let summary = source.apply_live(batch).expect("captured eviction apply");
            samples_us.push(started.elapsed().as_secs_f64() * 1_000_000.0);
            removed = removed.saturating_add(summary.removed_blocks);
            filtered = filtered.saturating_add(summary.filtered_events);
        }
        assert_eq!(removed, GROUP_REMOVALS[0]);
        assert_eq!(filtered, GROUP_REMOVALS[1..].iter().sum::<usize>());
        assert_eq!(source.status().indexed_blocks, RETAINED_MAIN_BLOCKS);
        black_box(source);
    }

    samples_us.sort_by(f64::total_cmp);
    println!(
        "iterations={iterations} batches_per_iteration={BATCHES} removal_events={} main_removals={} filtered_removals={} retained_main_blocks={RETAINED_MAIN_BLOCKS} apply_live_p50_us={:.3} apply_live_p95_us={:.3} apply_live_p99_us={:.3} apply_live_max_us={:.3}",
        GROUP_REMOVALS.iter().sum::<usize>(),
        GROUP_REMOVALS[0],
        GROUP_REMOVALS[1..].iter().sum::<usize>(),
        percentile(&samples_us, 50),
        percentile(&samples_us, 95),
        percentile(&samples_us, 99),
        samples_us.last().copied().unwrap_or_default(),
    );
}

fn ready_source() -> CompanionIndexSource {
    let source = CompanionIndexSource::new(
        CompanionIndexSourceConfig {
            group: GroupMetadata {
                data_parallel_rank: 0,
                group_idx: 0,
                attention_kind: AttentionKind::MlaAttention,
                disposition: GroupDisposition::Indexed,
                block_size: u32::try_from(BLOCK_SIZE).expect("block size"),
            },
            index_limits: DigestIndexLimits::default(),
            snapshot_limits: SnapshotLimits::default(),
            max_active_sessions: 2,
        },
        EngineIncarnation {
            engine_id: "captured-node06-a".to_owned(),
            model_revision: "9e165c30e2704aec5d9d593cce3eebd58bbef1cb".to_owned(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            process_started_unix_ns: 1,
            attestation_sha256: vec![0x11; 32],
        },
        1,
        &SECRET,
    )
    .expect("companion source");
    source
        .apply_replay(&initial_store_batch())
        .expect("initial captured main state");
    source.finish_replay(0).expect("publish initial state");
    assert_eq!(source.status().indexed_blocks, STORED_MAIN_BLOCKS);
    source
}

fn initial_store_batch() -> SequencedBatch {
    SequencedBatch {
        sequence: 0,
        payload: Bytes::new(),
        batch: KvEventBatch {
            timestamp: 0.0,
            data_parallel_rank: Some(0),
            events: vec![KvEvent::BlockStored(BlockStored {
                block_hashes: (1..=STORED_MAIN_BLOCKS).map(hash).collect(),
                parent_block_hash: None,
                token_ids: (0..STORED_MAIN_BLOCKS * BLOCK_SIZE)
                    .map(|token| u32::try_from(token % 129_280).expect("synthetic token"))
                    .collect(),
                block_size: BLOCK_SIZE,
                group_idx: Some(0),
                kv_cache_spec_kind: Some("mla_attention".to_owned()),
                kv_cache_spec_sliding_window: None,
                medium: Some("GPU".to_owned()),
                locality: Some("LOCAL".to_owned()),
                lora_name: None,
                cache_namespace: None,
                has_extra_keys: false,
            })],
        },
    }
}

fn captured_eviction_batches() -> Vec<SequencedBatch> {
    let mut events = Vec::with_capacity(GROUP_REMOVALS.iter().sum());
    for (group, count) in GROUP_REMOVALS.into_iter().enumerate() {
        for offset in 0..count {
            let block_hash = if group == 0 {
                STORED_MAIN_BLOCKS - offset
            } else {
                (group + 1) * 1_000_000 + offset
            };
            events.push((offset % BATCHES, removal(group, block_hash)));
        }
    }

    (0..BATCHES)
        .map(|batch_index| SequencedBatch {
            sequence: u64::try_from(batch_index + 1).expect("batch sequence"),
            payload: Bytes::new(),
            batch: KvEventBatch {
                timestamp: f64::from(u32::try_from(batch_index).expect("batch index")),
                data_parallel_rank: Some(0),
                events: events
                    .iter()
                    .filter(|(assigned, _)| *assigned == batch_index)
                    .map(|(_, event)| event.clone())
                    .collect(),
            },
        })
        .collect()
}

fn removal(group: usize, block_hash: usize) -> KvEvent {
    KvEvent::BlockRemoved(BlockRemoved {
        block_hashes: vec![hash(block_hash)],
        group_idx: Some(u32::try_from(group).expect("group index")),
        medium: Some("GPU".to_owned()),
        locality: Some("LOCAL".to_owned()),
    })
}

fn hash(value: usize) -> ExternalBlockHash {
    ExternalBlockHash::Unsigned(u64::try_from(value).expect("block hash"))
}

fn percentile(sorted: &[f64], percentile: usize) -> f64 {
    let index = sorted
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len().saturating_sub(1));
    sorted.get(index).copied().unwrap_or_default()
}
