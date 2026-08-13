//! GPU-free snapshot wire benchmark at node06's captured resident shape.
//!
//! Run with:
//! `cargo run --release --locked --example snapshot_shape_bench`.

use std::{hint::black_box, time::Instant};

use mini_dynamo::kv_snapshot::{
    AttentionKind, DigestAlgorithm, DigestRecord, DigestSpec, EngineIncarnation, GroupDisposition,
    GroupMetadata, ResetScope, SnapshotBlockHash, SnapshotBody, SnapshotCapacity,
    SnapshotExpectations, SnapshotLimits, decode_snapshot, encode_snapshot,
};

const BLOCKS: usize = 36_612;
const BLOCK_SIZE: u32 = 256;
const SOURCE_TOKEN_IDS: usize = BLOCKS * BLOCK_SIZE as usize;
const ITERATIONS: u32 = 10;

fn main() {
    let mut body = captured_shape();
    body.refresh_capacity().expect("captured shape capacity");
    let limits = SnapshotLimits::default();
    let expected_incarnation = body.engine_incarnation.clone();
    let expected_digest = body.digest.clone();
    let expected = SnapshotExpectations {
        engine_incarnation: &expected_incarnation,
        reset_scope: ResetScope::full_engine(),
        digest: &expected_digest,
    };

    let encode_started = Instant::now();
    let frame = encode_snapshot(&body, limits).expect("encode captured shape");
    let encode_ms = encode_started.elapsed().as_secs_f64() * 1_000.0;
    let decode_started = Instant::now();
    let decoded = decode_snapshot(&frame, limits, expected).expect("decode captured shape");
    let decode_ms = decode_started.elapsed().as_secs_f64() * 1_000.0;

    let repeated_started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(decode_snapshot(&frame, limits, expected).expect("repeat captured-shape decode"));
    }
    let repeated_decode_ms =
        repeated_started.elapsed().as_secs_f64() * 1_000.0 / f64::from(ITERATIONS);

    assert_eq!(decoded.records.len(), BLOCKS);
    println!(
        "blocks={BLOCKS} source_token_ids={SOURCE_TOKEN_IDS} frame_bytes={} encode_ms={encode_ms:.3} decode_ms={decode_ms:.3} repeated_decode_ms={repeated_decode_ms:.3}",
        frame.len(),
    );
}

fn captured_shape() -> SnapshotBody {
    let records = (0..BLOCKS)
        .map(|index| DigestRecord {
            group_slot: 0,
            parent_record: index
                .checked_sub(1)
                .map(|parent| u32::try_from(parent).expect("record index fits u32")),
            external_hash: SnapshotBlockHash::Unsigned(
                u64::try_from(index + 1).expect("record index fits u64"),
            ),
            block_digest: digest_for(index),
            block_token_ids: BLOCK_SIZE,
            prefix_token_ids: u64::try_from(index + 1)
                .expect("record index fits u64")
                .saturating_mul(u64::from(BLOCK_SIZE)),
        })
        .collect();
    SnapshotBody {
        engine_incarnation: EngineIncarnation {
            engine_id: "captured-node06-a".to_owned(),
            model_revision: "9e165c30e2704aec5d9d593cce3eebd58bbef1cb".to_owned(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            process_started_unix_ns: 1,
            attestation_sha256: vec![0x11; 32],
        },
        watermark: 9_506,
        reset_scope: ResetScope::full_engine(),
        digest: DigestSpec {
            algorithm: DigestAlgorithm::HmacSha256V1,
            key_id: vec![0x22; 16],
            digest_bytes: 32,
        },
        capacity: SnapshotCapacity::default(),
        groups: vec![GroupMetadata {
            data_parallel_rank: 0,
            group_idx: 0,
            attention_kind: AttentionKind::MlaAttention,
            disposition: GroupDisposition::Indexed,
            block_size: BLOCK_SIZE,
        }],
        records,
    }
}

fn digest_for(index: usize) -> Vec<u8> {
    let bytes = u64::try_from(index)
        .expect("record index fits u64")
        .to_be_bytes();
    bytes.into_iter().cycle().take(32).collect()
}
