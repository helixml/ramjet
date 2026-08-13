//! Matched raw-token/digest lookup and snapshot-to-index benchmark.
//!
//! Run with `cargo run --release --locked --example digest_index_bench`.

use std::{fs, hint::black_box, time::Instant};

use mini_dynamo::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    exact_index::{ExactIndexLimits, ExactKvIndex},
    kv_snapshot::{
        AttentionKind, DigestAlgorithm, DigestRecord, DigestSpec, EngineIncarnation,
        GroupDisposition, GroupMetadata, ResetScope, SnapshotBlockHash, SnapshotBody,
        SnapshotCapacity,
    },
    kv_wire::{BlockStored, ExternalBlockHash},
};

const SECRET: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
const BLOCK_SIZE: usize = 256;
const MATCHED_BLOCKS: usize = 316;
const LONG_BLOCKS: usize = 2_048;
const SNAPSHOT_BLOCKS: usize = 36_612;
const LOOKUP_ITERATIONS: usize = 1_000;
const CAPACITY_BLOCKS: usize = 15_168;

fn main() {
    if let Ok(mode) = std::env::var("BENCH_MODE")
        && matches!(mode.as_str(), "exact-rss" | "digest-rss")
    {
        capacity_rss(&mode);
        return;
    }

    let matched_tokens = tokens(MATCHED_BLOCKS);
    let event = store_event(&matched_tokens, MATCHED_BLOCKS);

    let mut exact = ExactKvIndex::new(ExactIndexLimits::default());
    let started = Instant::now();
    exact.store(&event).expect("exact build");
    let exact_build_ms = elapsed_ms(started);

    let mut digest =
        DigestKvIndex::from_secret(DigestIndexLimits::default(), SECRET).expect("digest index");
    let started = Instant::now();
    digest.store(&event).expect("digest build");
    let digest_build_ms = elapsed_ms(started);

    let exact_lookup_us = lookup_us(LOOKUP_ITERATIONS, || {
        black_box(exact.find_longest(&matched_tokens).expect("exact lookup"));
    });
    let digest_lookup_us = lookup_us(LOOKUP_ITERATIONS, || {
        black_box(digest.find_longest(&matched_tokens).expect("digest lookup"));
    });
    println!(
        "phase=matched blocks={MATCHED_BLOCKS} token_ids={} exact_build_ms={exact_build_ms:.3} digest_build_ms={digest_build_ms:.3} exact_lookup_us={exact_lookup_us:.3} digest_lookup_us={digest_lookup_us:.3} lookup_ratio={:.2}",
        matched_tokens.len(),
        digest_lookup_us / exact_lookup_us,
    );

    let long_tokens = tokens(LONG_BLOCKS);
    let mut long_digest = DigestKvIndex::from_secret(DigestIndexLimits::default(), SECRET)
        .expect("long digest index");
    long_digest
        .store(&store_event(&long_tokens, LONG_BLOCKS))
        .expect("long digest build");
    let long_lookup_us = lookup_us(100, || {
        black_box(
            long_digest
                .find_longest(&long_tokens)
                .expect("long digest lookup"),
        );
    });
    println!(
        "phase=long_lookup blocks={LONG_BLOCKS} token_ids={} digest_lookup_us={long_lookup_us:.3}",
        long_tokens.len(),
    );

    let snapshot = snapshot_body();
    let mut restored = DigestKvIndex::from_secret(DigestIndexLimits::default(), SECRET)
        .expect("restored digest index");
    let started = Instant::now();
    let imported = restored
        .replace_from_snapshot(
            &snapshot,
            SnapshotGroupKey {
                data_parallel_rank: 0,
                group_idx: 0,
            },
        )
        .expect("snapshot import");
    let import_ms = elapsed_ms(started);
    assert_eq!(imported, SNAPSHOT_BLOCKS);
    println!(
        "phase=snapshot_import blocks={imported} logical_token_ids={} import_ms={import_ms:.3} commitment_bytes={}",
        restored.stats().logical_token_ids,
        restored.stats().commitment_bytes,
    );
}

fn capacity_rss(mode: &str) {
    let capacity_tokens = tokens(CAPACITY_BLOCKS);
    let event = store_event(&capacity_tokens, CAPACITY_BLOCKS);
    let before = rss_kib();
    let started = Instant::now();
    let (build_ms, nodes, logical_token_ids, after) = if mode == "exact-rss" {
        let mut index = ExactKvIndex::new(ExactIndexLimits::default());
        index.store(&event).expect("exact capacity build");
        let stats = index.stats();
        let result = (elapsed_ms(started), stats.nodes, stats.token_ids, rss_kib());
        black_box(index);
        result
    } else {
        let mut index = DigestKvIndex::from_secret(DigestIndexLimits::default(), SECRET)
            .expect("digest capacity index");
        index.store(&event).expect("digest capacity build");
        let stats = index.stats();
        let result = (
            elapsed_ms(started),
            stats.nodes,
            stats.logical_token_ids,
            rss_kib(),
        );
        black_box(index);
        result
    };
    println!(
        "phase={mode} blocks={nodes} logical_token_ids={logical_token_ids} build_ms={build_ms:.3} rss_delta_kib={}",
        after.saturating_sub(before),
    );
}

fn lookup_us(iterations: usize, mut operation: impl FnMut()) -> f64 {
    for _ in 0..100 {
        operation();
    }
    let started = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    started.elapsed().as_secs_f64() * 1_000_000.0
        / f64::from(u32::try_from(iterations).expect("benchmark iterations fit u32"))
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn rss_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .expect("read process status")
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .expect("VmRSS")
}

fn tokens(blocks: usize) -> Vec<u32> {
    (0..blocks * BLOCK_SIZE)
        .map(|index| u32::try_from(index % 129_280).expect("synthetic token"))
        .collect()
}

fn store_event(token_ids: &[u32], blocks: usize) -> BlockStored {
    BlockStored {
        block_hashes: (1..=blocks)
            .map(|hash| ExternalBlockHash::Unsigned(u64::try_from(hash).expect("block hash")))
            .collect(),
        parent_block_hash: None,
        token_ids: token_ids.to_vec(),
        block_size: BLOCK_SIZE,
        group_idx: Some(0),
        kv_cache_spec_kind: None,
        kv_cache_spec_sliding_window: None,
        medium: Some("GPU".to_owned()),
        locality: Some("LOCAL".to_owned()),
        lora_name: None,
        cache_namespace: None,
        has_extra_keys: false,
    }
}

fn snapshot_body() -> SnapshotBody {
    let digester = BlockDigester::new(*SECRET);
    let records = (0..SNAPSHOT_BLOCKS)
        .map(|index| {
            let block_tokens = (0..BLOCK_SIZE)
                .map(|offset| {
                    u32::try_from((index * BLOCK_SIZE + offset) % 129_280).expect("synthetic token")
                })
                .collect::<Vec<_>>();
            DigestRecord {
                group_slot: 0,
                parent_record: index
                    .checked_sub(1)
                    .map(|parent| u32::try_from(parent).expect("snapshot parent")),
                external_hash: SnapshotBlockHash::Unsigned(
                    u64::try_from(index + 1).expect("snapshot hash"),
                ),
                block_digest: digester
                    .commit(&block_tokens)
                    .expect("block digest")
                    .digest_bytes()
                    .to_vec(),
                block_token_ids: u32::try_from(BLOCK_SIZE).expect("block size"),
                prefix_token_ids: u64::try_from((index + 1) * BLOCK_SIZE)
                    .expect("prefix token count"),
                present: true,
            }
        })
        .collect();
    let mut body = SnapshotBody {
        engine_incarnation: EngineIncarnation {
            engine_id: "captured-node06".to_owned(),
            model_revision: "9e165c30e2704aec5d9d593cce3eebd58bbef1cb".to_owned(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            process_started_unix_ns: 1,
            attestation_sha256: vec![0x11; 32],
        },
        watermark: 9_506,
        reset_scope: ResetScope::full_engine(),
        digest: DigestSpec {
            algorithm: DigestAlgorithm::HmacSha256V1,
            key_id: digester.key_id().to_vec(),
            digest_bytes: 32,
        },
        capacity: SnapshotCapacity::default(),
        groups: vec![GroupMetadata {
            data_parallel_rank: 0,
            group_idx: 0,
            attention_kind: AttentionKind::MlaAttention,
            disposition: GroupDisposition::Indexed,
            block_size: u32::try_from(BLOCK_SIZE).expect("block size"),
        }],
        records,
    };
    body.refresh_capacity().expect("snapshot capacity");
    body
}
