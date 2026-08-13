use mini_dynamo::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{
        AttentionKind, DigestAlgorithm, DigestRecord, DigestSpec, EngineIncarnation,
        GroupDisposition, GroupMetadata, ResetScope, SnapshotBlockHash, SnapshotBody,
        SnapshotCapacity, SnapshotExpectations, SnapshotLimits, decode_snapshot, encode_snapshot,
    },
    snapshot_session::{
        SnapshotSessionBinding, SnapshotSessionChallenge, SnapshotSessionExpectations,
        SnapshotSessionLimits, SnapshotSessionSecret, decode_authenticated_snapshot,
        decode_client_hello, encode_authenticated_snapshot, encode_client_hello,
    },
    snapshot_tail::{SnapshotAction, SnapshotTailFence, SnapshotTailState},
};

const DIGEST_SECRET: [u8; 32] = *b"0123456789abcdef0123456789abcdef";
const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x41; 32]);

#[test]
fn authenticated_snapshot_builds_private_index_before_catch_up() {
    let digester = BlockDigester::new(DIGEST_SECRET);
    let key_id = *digester.key_id().as_bytes();
    let mut body = snapshot_body(&digester, key_id);
    body.refresh_capacity().unwrap();
    let snapshot_limits = SnapshotLimits::default();
    let snapshot_frame = encode_snapshot(&body, snapshot_limits).unwrap();

    let session_secret = SnapshotSessionSecret::new(SESSION_SECRET);
    let session_limits = SnapshotSessionLimits::default();
    let hello = encode_client_hello(CHALLENGE, &session_secret, session_limits).unwrap();
    assert_eq!(
        decode_client_hello(&hello, &session_secret, session_limits).unwrap(),
        CHALLENGE
    );
    let response = encode_authenticated_snapshot(
        &snapshot_frame,
        SnapshotSessionBinding {
            challenge: CHALLENGE,
            engine_incarnation: &body.engine_incarnation,
            snapshot_watermark: body.watermark,
            digest_key_id: &key_id,
            companion_generation: 7,
        },
        &session_secret,
        session_limits,
    )
    .unwrap();
    let authenticated = decode_authenticated_snapshot(
        &response,
        SnapshotSessionExpectations {
            challenge: CHALLENGE,
            engine_incarnation: &body.engine_incarnation,
            digest_key_id: &key_id,
            minimum_snapshot_watermark: body.watermark,
            minimum_companion_generation: 7,
        },
        &session_secret,
        session_limits,
    )
    .unwrap();

    let decoded = decode_snapshot(
        authenticated.snapshot_frame(),
        snapshot_limits,
        SnapshotExpectations {
            engine_incarnation: &body.engine_incarnation,
            reset_scope: ResetScope::full_engine(),
            digest: &body.digest,
        },
    )
    .unwrap();
    let mut private_index =
        DigestKvIndex::from_secret(DigestIndexLimits::default(), &DIGEST_SECRET).unwrap();
    assert_eq!(
        private_index
            .replace_from_snapshot(
                &decoded,
                SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
            )
            .unwrap(),
        2
    );
    assert_eq!(
        private_index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids,
        4
    );

    let mut lifecycle = SnapshotTailFence::start_bootstrap(
        body.engine_incarnation.clone(),
        body.watermark,
        key_id.to_vec(),
        7,
    );
    assert_eq!(
        lifecycle.accept_snapshot(&authenticated, decoded.reset_scope),
        SnapshotAction::Accepted {
            snapshot_watermark: body.watermark,
            delivery_sequence: 0,
        }
    );
    assert_eq!(lifecycle.status().state, SnapshotTailState::CatchingUp);
}

fn snapshot_body(digester: &BlockDigester, key_id: [u8; 32]) -> SnapshotBody {
    let blocks = [[1_u32, 2], [3, 4]];
    let mut prefix = 0_u64;
    let records = blocks
        .iter()
        .enumerate()
        .map(|(index, tokens)| {
            prefix += u64::try_from(tokens.len()).unwrap();
            DigestRecord {
                group_slot: 0,
                parent_record: index
                    .checked_sub(1)
                    .map(|parent| u32::try_from(parent).unwrap()),
                external_hash: SnapshotBlockHash::Unsigned(u64::try_from(index + 1).unwrap()),
                block_digest: digester.commit(tokens).unwrap().digest_bytes().to_vec(),
                block_token_ids: u32::try_from(tokens.len()).unwrap(),
                prefix_token_ids: prefix,
                present: true,
            }
        })
        .collect();
    SnapshotBody {
        engine_incarnation: EngineIncarnation {
            engine_id: "engine-a".to_owned(),
            model_revision: "revision-a".to_owned(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            process_started_unix_ns: 42,
            attestation_sha256: vec![0x11; 32],
        },
        watermark: 9_123,
        reset_scope: ResetScope::full_engine(),
        digest: DigestSpec {
            algorithm: DigestAlgorithm::HmacSha256V1,
            key_id: key_id.to_vec(),
            digest_bytes: 32,
        },
        capacity: SnapshotCapacity::default(),
        groups: vec![GroupMetadata {
            data_parallel_rank: 0,
            group_idx: 0,
            attention_kind: AttentionKind::MlaAttention,
            disposition: GroupDisposition::Indexed,
            block_size: 2,
        }],
        records,
    }
}
