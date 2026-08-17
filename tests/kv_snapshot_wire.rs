use ramjet::kv_snapshot::*;
use serde::Serialize;
use serde_bytes::ByteBuf;
use sha2::{Digest, Sha256};

fn incarnation(engine_id: &str) -> EngineIncarnation {
    EngineIncarnation {
        engine_id: engine_id.to_owned(),
        model_revision: "deepseek-v4-flash".to_owned(),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        process_started_unix_ns: 1_000,
        attestation_sha256: vec![0x11; 32],
    }
}

fn digest() -> DigestSpec {
    DigestSpec {
        algorithm: DigestAlgorithm::HmacSha256V1,
        key_id: vec![0x22; 32],
        digest_bytes: 32,
    }
}

fn body() -> SnapshotBody {
    let mut body = SnapshotBody {
        engine_incarnation: incarnation("engine-a"),
        watermark: 42,
        reset_scope: ResetScope::full_engine(),
        digest: digest(),
        capacity: SnapshotCapacity::default(),
        groups: vec![
            GroupMetadata {
                data_parallel_rank: 0,
                group_idx: 0,
                attention_kind: AttentionKind::MlaAttention,
                disposition: GroupDisposition::Indexed,
                block_size: 4,
            },
            GroupMetadata {
                data_parallel_rank: 0,
                group_idx: 1,
                attention_kind: AttentionKind::Mamba,
                disposition: GroupDisposition::Filtered,
                block_size: 4,
            },
        ],
        records: vec![
            DigestRecord {
                group_slot: 0,
                parent_record: None,
                external_hash: SnapshotBlockHash::Unsigned(10),
                block_digest: vec![0x31; 32],
                block_token_ids: 4,
                prefix_token_ids: 4,
                present: true,
            },
            DigestRecord {
                group_slot: 0,
                parent_record: Some(0),
                external_hash: SnapshotBlockHash::Bytes(ByteBuf::from(vec![0x41; 8])),
                block_digest: vec![0x32; 32],
                block_token_ids: 3,
                prefix_token_ids: 7,
                present: true,
            },
        ],
    };
    body.refresh_capacity().unwrap();
    body
}

fn expected(body: &SnapshotBody) -> SnapshotExpectations<'_> {
    SnapshotExpectations {
        engine_incarnation: &body.engine_incarnation,
        reset_scope: body.reset_scope,
        digest: &body.digest,
    }
}

#[derive(Serialize)]
struct TestEnvelope {
    schema_version: u16,
    checksum_algorithm: &'static str,
    payload_bytes: u64,
    #[serde(with = "serde_bytes")]
    checksum: Vec<u8>,
    #[serde(with = "serde_bytes")]
    payload: Vec<u8>,
}

fn envelope(schema_version: u16, checksum_algorithm: &'static str, payload: Vec<u8>) -> Vec<u8> {
    rmp_serde::to_vec_named(&TestEnvelope {
        schema_version,
        checksum_algorithm,
        payload_bytes: payload.len() as u64,
        checksum: Sha256::digest(&payload).to_vec(),
        payload,
    })
    .unwrap()
}

#[test]
fn round_trips_versioned_snapshot() {
    let source = body();
    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    let decoded = decode_snapshot(&frame, SnapshotLimits::default(), expected(&source)).unwrap();
    assert_eq!(decoded, source);
    assert_eq!(decoded.watermark, 42);
}

#[test]
fn default_wire_capacity_matches_the_production_index() {
    let limits = SnapshotLimits::default();
    assert_eq!(limits.max_records, 131_072);
    assert_eq!(limits.max_prefix_token_ids, 16_777_216);
    assert_eq!(limits.max_frame_bytes, 32 * 1024 * 1024);
    assert_eq!(limits.max_key_id_bytes, 32);
}

#[test]
fn rejects_malformed_outer_and_inner_messagepack() {
    let source = body();
    assert_eq!(
        decode_snapshot(&[0xc1], SnapshotLimits::default(), expected(&source)),
        Err(SnapshotError::InvalidMessagePack)
    );

    let frame = envelope(SNAPSHOT_SCHEMA_VERSION, "sha256", vec![0xc1]);
    assert_eq!(
        decode_snapshot(&frame, SnapshotLimits::default(), expected(&source)),
        Err(SnapshotError::InvalidMessagePack)
    );
}

#[test]
fn rejects_checksum_corruption_before_body_decode() {
    let source = body();
    let mut frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    *frame.last_mut().unwrap() ^= 1;
    assert_eq!(
        decode_snapshot(&frame, SnapshotLimits::default(), expected(&source)),
        Err(SnapshotError::InvalidChecksum)
    );
}

#[test]
fn rejects_schema_and_checksum_algorithm_mismatches() {
    let source = body();
    let payload = rmp_serde::to_vec_named(&source).unwrap();
    assert_eq!(
        decode_snapshot(
            &envelope(SNAPSHOT_SCHEMA_VERSION + 1, "sha256", payload.clone(),),
            SnapshotLimits::default(),
            expected(&source)
        ),
        Err(SnapshotError::UnsupportedSchema)
    );
    assert_eq!(
        decode_snapshot(
            &envelope(SNAPSHOT_SCHEMA_VERSION, "crc32", payload),
            SnapshotLimits::default(),
            expected(&source)
        ),
        Err(SnapshotError::UnsupportedChecksum)
    );
}

#[test]
fn rejects_frame_and_record_capacity_breaches() {
    let source = body();
    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    let limits = SnapshotLimits {
        max_frame_bytes: frame.len() - 1,
        ..SnapshotLimits::default()
    };
    assert_eq!(
        decode_snapshot(&frame, limits, expected(&source)),
        Err(SnapshotError::FrameTooLarge)
    );

    let limits = SnapshotLimits {
        max_records: 1,
        ..SnapshotLimits::default()
    };
    assert_eq!(
        decode_snapshot(&frame, limits, expected(&source)),
        Err(SnapshotError::TooManyRecords)
    );

    let limits = SnapshotLimits {
        max_total_external_hash_bytes: 8,
        ..SnapshotLimits::default()
    };
    assert_eq!(
        decode_snapshot(&frame, limits, expected(&source)),
        Err(SnapshotError::CapacityExceeded)
    );
}

#[test]
fn rejects_invalid_and_mismatched_incarnations() {
    let mut invalid = body();
    invalid.engine_incarnation.engine_id.clear();
    assert_eq!(
        encode_snapshot(&invalid, SnapshotLimits::default()),
        Err(SnapshotError::InvalidIncarnation)
    );

    let source = body();
    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    let other = incarnation("engine-b");
    assert_eq!(
        decode_snapshot(
            &frame,
            SnapshotLimits::default(),
            SnapshotExpectations {
                engine_incarnation: &other,
                reset_scope: source.reset_scope,
                digest: &source.digest,
            }
        ),
        Err(SnapshotError::IncarnationMismatch)
    );
}

#[test]
fn rejects_reset_digest_and_capacity_declaration_mismatches() {
    let source = body();
    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    let rank_scope = ResetScope {
        kind: ResetKind::DataParallelRank,
        data_parallel_rank: Some(0),
        group_idx: None,
    };
    assert_eq!(
        decode_snapshot(
            &frame,
            SnapshotLimits::default(),
            SnapshotExpectations {
                engine_incarnation: &source.engine_incarnation,
                reset_scope: rank_scope,
                digest: &source.digest,
            }
        ),
        Err(SnapshotError::ResetScopeMismatch)
    );

    let other_digest = DigestSpec {
        key_id: vec![9; 16],
        ..source.digest.clone()
    };
    assert_eq!(
        decode_snapshot(
            &frame,
            SnapshotLimits::default(),
            SnapshotExpectations {
                engine_incarnation: &source.engine_incarnation,
                reset_scope: source.reset_scope,
                digest: &other_digest,
            }
        ),
        Err(SnapshotError::DigestMismatch)
    );

    let mut wrong_capacity = body();
    wrong_capacity.capacity.records += 1;
    assert_eq!(
        encode_snapshot(&wrong_capacity, SnapshotLimits::default()),
        Err(SnapshotError::CapacityMismatch)
    );
}

#[test]
fn rejects_non_bfs_and_cross_group_records() {
    let mut source = body();
    source.records[0].parent_record = Some(1);
    assert_eq!(
        encode_snapshot(&source, SnapshotLimits::default()),
        Err(SnapshotError::InvalidRecord)
    );

    let mut source = body();
    source.records[1].group_slot = 1;
    assert_eq!(
        encode_snapshot(&source, SnapshotLimits::default()),
        Err(SnapshotError::InvalidRecord)
    );
}

#[test]
fn rejects_empty_external_hash_identity() {
    let mut source = body();
    source.records[0].external_hash = SnapshotBlockHash::Bytes(ByteBuf::new());
    source.refresh_capacity().unwrap();
    assert_eq!(
        encode_snapshot(&source, SnapshotLimits::default()),
        Err(SnapshotError::InvalidRecord)
    );
}

#[test]
fn validates_bfs_depth_independently_per_indexed_group() {
    let mut source = body();
    source.groups.push(GroupMetadata {
        data_parallel_rank: 1,
        group_idx: 0,
        attention_kind: AttentionKind::MlaAttention,
        disposition: GroupDisposition::Indexed,
        block_size: 4,
    });
    source.records.push(DigestRecord {
        group_slot: 2,
        parent_record: None,
        external_hash: SnapshotBlockHash::Unsigned(20),
        block_digest: vec![0x51; 32],
        block_token_ids: 4,
        prefix_token_ids: 4,
        present: true,
    });
    source.refresh_capacity().unwrap();

    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    assert_eq!(
        decode_snapshot(&frame, SnapshotLimits::default(), expected(&source)).unwrap(),
        source
    );
}

#[test]
fn preserves_absent_ancestors_with_live_descendants() {
    let mut source = body();
    source.records[0].present = false;
    source.refresh_capacity().unwrap();

    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    assert_eq!(
        decode_snapshot(&frame, SnapshotLimits::default(), expected(&source)).unwrap(),
        source
    );
}

#[test]
fn rejects_absent_leaf_records() {
    let mut source = body();
    source.records[1].present = false;
    source.refresh_capacity().unwrap();

    assert_eq!(
        encode_snapshot(&source, SnapshotLimits::default()),
        Err(SnapshotError::InvalidRecord)
    );
}

#[test]
fn cancellation_never_returns_partial_state() {
    let source = body();
    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    let mut checks = 0;
    let result =
        decode_snapshot_with_cancel(&frame, SnapshotLimits::default(), expected(&source), || {
            checks += 1;
            checks >= 6
        });
    assert_eq!(result, Err(SnapshotError::Cancelled));
}

#[test]
fn errors_and_reason_codes_do_not_expose_wire_values() {
    let mut source = body();
    source.engine_incarnation.engine_id = "private-engine-sentinel".to_owned();
    let frame = encode_snapshot(&source, SnapshotLimits::default()).unwrap();
    let other = incarnation("expected-engine-sentinel");
    let error = decode_snapshot(
        &frame,
        SnapshotLimits::default(),
        SnapshotExpectations {
            engine_incarnation: &other,
            reset_scope: source.reset_scope,
            digest: &source.digest,
        },
    )
    .unwrap_err();
    assert_eq!(error.reason(), "incarnation_mismatch");
    let rendered = error.to_string();
    assert!(!rendered.contains("private-engine-sentinel"));
    assert!(!rendered.contains("expected-engine-sentinel"));
}
