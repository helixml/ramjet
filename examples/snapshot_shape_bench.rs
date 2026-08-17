//! GPU-free snapshot wire benchmark at node06's captured resident shape.
//!
//! Run with:
//! `cargo run --release --locked --example snapshot_shape_bench`.

use std::{fs, hint::black_box, time::Instant};

use ramjet::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexLimits, SnapshotGroupKey},
    kv_snapshot::{
        AttentionKind, DigestAlgorithm, DigestRecord, DigestSpec, EngineIncarnation,
        GroupDisposition, GroupMetadata, ResetScope, SnapshotBlockHash, SnapshotBody,
        SnapshotCapacity, SnapshotExpectations, SnapshotLimits, decode_snapshot, encode_snapshot,
    },
    snapshot_bootstrap::prepare_authenticated_snapshot,
    snapshot_session::{
        SnapshotSessionBinding, SnapshotSessionChallenge, SnapshotSessionExpectations,
        SnapshotSessionLimits, SnapshotSessionSecret, decode_authenticated_snapshot,
        encode_authenticated_snapshot,
    },
};

const BLOCKS: usize = 36_612;
const BLOCK_SIZE: u32 = 256;
const SOURCE_TOKEN_IDS: usize = BLOCKS * BLOCK_SIZE as usize;
const ITERATIONS: u32 = 10;
const DIGEST_SECRET: [u8; 32] = *b"0123456789abcdef0123456789abcdef";
const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x41; 32]);
const GENERATION: u64 = 7;

fn main() {
    let digester = BlockDigester::new(DIGEST_SECRET);
    let mut body = captured_shape(digester.key_id().to_vec());
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

    let session_secret = SnapshotSessionSecret::new(SESSION_SECRET);
    let session_limits = SnapshotSessionLimits::default();
    let wire_started = Instant::now();
    let response = encode_authenticated_snapshot(
        &frame,
        SnapshotSessionBinding {
            challenge: CHALLENGE,
            engine_incarnation: &body.engine_incarnation,
            snapshot_watermark: body.watermark,
            digest_key_id: digester.key_id().as_bytes(),
            companion_generation: GENERATION,
        },
        &session_secret,
        session_limits,
    )
    .expect("encode authenticated captured-shape response");
    let wire_encode_ms = wire_started.elapsed().as_secs_f64() * 1_000.0;
    let wire_decode_started = Instant::now();
    let authenticated = decode_authenticated_snapshot(
        &response,
        SnapshotSessionExpectations {
            challenge: CHALLENGE,
            engine_incarnation: &body.engine_incarnation,
            digest_key_id: digester.key_id().as_bytes(),
            minimum_snapshot_watermark: body.watermark,
            minimum_companion_generation: GENERATION,
        },
        &session_secret,
        session_limits,
    )
    .expect("decode authenticated captured-shape response");
    let wire_decode_ms = wire_decode_started.elapsed().as_secs_f64() * 1_000.0;
    let rebuild_started = Instant::now();
    let prepared = prepare_authenticated_snapshot(
        authenticated,
        &DIGEST_SECRET,
        limits,
        DigestIndexLimits::default(),
        SnapshotGroupKey {
            data_parallel_rank: 0,
            group_idx: 0,
        },
        body.watermark,
    )
    .expect("rebuild private captured-shape index");
    let private_rebuild_ms = rebuild_started.elapsed().as_secs_f64() * 1_000.0;

    let repeated_started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(decode_snapshot(&frame, limits, expected).expect("repeat captured-shape decode"));
    }
    let repeated_decode_ms =
        repeated_started.elapsed().as_secs_f64() * 1_000.0 / f64::from(ITERATIONS);

    assert_eq!(decoded.records.len(), BLOCKS);
    let index_stats = prepared.index().stats();
    assert_eq!(index_stats.logical_token_ids, SOURCE_TOKEN_IDS);
    assert_eq!(index_stats.external_hashes, BLOCKS);
    println!(
        "blocks={BLOCKS} source_token_ids={SOURCE_TOKEN_IDS} frame_bytes={} response_bytes={} encode_ms={encode_ms:.3} decode_ms={decode_ms:.3} repeated_decode_ms={repeated_decode_ms:.3} wire_encode_ms={wire_encode_ms:.3} wire_decode_ms={wire_decode_ms:.3} private_rebuild_ms={private_rebuild_ms:.3} vm_rss_kib={} vm_hwm_kib={}",
        frame.len(),
        response.len(),
        proc_status_kib("VmRSS:").unwrap_or(0),
        proc_status_kib("VmHWM:").unwrap_or(0),
    );
}

fn captured_shape(key_id: Vec<u8>) -> SnapshotBody {
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
            present: true,
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
            key_id,
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

fn proc_status_kib(field: &str) -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix(field)?.trim();
        value.strip_suffix("kB")?.trim().parse().ok()
    })
}

fn digest_for(index: usize) -> Vec<u8> {
    let bytes = u64::try_from(index)
        .expect("record index fits u64")
        .to_be_bytes();
    bytes.into_iter().cycle().take(32).collect()
}
