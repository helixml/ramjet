use std::{future::pending, sync::Arc, time::Duration};

use mini_dynamo::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{
        AttentionKind, DigestAlgorithm, DigestRecord, DigestSpec, EngineIncarnation,
        GroupDisposition, GroupMetadata, ResetScope, SnapshotBlockHash, SnapshotBody,
        SnapshotCapacity, SnapshotLimits, encode_snapshot,
    },
    kv_wire::KvWireLimits,
    snapshot_actor::{SnapshotActorLimits, SnapshotBootstrapActor},
    snapshot_consumer::{
        SharedSnapshotPublication, SnapshotConsumer, SnapshotConsumerConfig, SnapshotConsumerError,
    },
    snapshot_session::{
        SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES, SnapshotSessionBinding, SnapshotSessionChallenge,
        SnapshotSessionError, SnapshotSessionLimits, SnapshotSessionSecret, decode_client_hello,
        encode_authenticated_snapshot, encode_client_hello,
    },
    snapshot_tail_wire::{
        TAIL_FRAME_LENGTH_PREFIX_BYTES, TailDirection, TailFrameBinding, TailFrameType,
        TailSessionKey, TailWireError, TailWireLimits, encode_tail_frame,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::oneshot,
    time::{Instant, timeout_at},
};

const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
const DIGEST_SECRET: [u8; 32] = [0x91; 32];
const CHALLENGE_A: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0xa1; 32]);
const CHALLENGE_B: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0xa2; 32]);
const WATERMARK: u64 = 100;
const GENERATION: u64 = 7;

fn incarnation(name: &str) -> EngineIncarnation {
    EngineIncarnation {
        engine_id: name.into(),
        model_revision: "revision-a".into(),
        image_digest: "sha256:image-a".into(),
        process_started_unix_ns: 42,
        attestation_sha256: vec![3; 32],
    }
}

fn consumer_for(stream: &UnixStream) -> Arc<SnapshotConsumer> {
    let expected_peer_uid = stream.peer_cred().unwrap().uid();
    Arc::new(
        SnapshotConsumer::new(
            SnapshotConsumerConfig {
                expected_peer_uid,
                expected_engine_incarnation: incarnation("engine-a"),
                minimum_snapshot_watermark: WATERMARK,
                minimum_companion_generation: GENERATION,
                group: SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
                session_limits: SnapshotSessionLimits::default(),
                snapshot_limits: SnapshotLimits::default(),
                index_limits: DigestIndexLimits::default(),
                tail_limits: TailWireLimits::default(),
                event_limits: KvWireLimits::default(),
            },
            SnapshotSessionSecret::new(SESSION_SECRET),
            DIGEST_SECRET,
            SnapshotActorLimits::default(),
        )
        .unwrap(),
    )
}

fn root_record(digester: &BlockDigester, external_hash: u64, token_ids: &[u32]) -> DigestRecord {
    DigestRecord {
        group_slot: 0,
        parent_record: None,
        external_hash: SnapshotBlockHash::Unsigned(external_hash),
        block_digest: digester.commit(token_ids).unwrap().digest_bytes().to_vec(),
        block_token_ids: u32::try_from(token_ids.len()).unwrap(),
        prefix_token_ids: u64::try_from(token_ids.len()).unwrap(),
        present: true,
    }
}

fn snapshot_response(
    challenge: SnapshotSessionChallenge,
    generation: u64,
    engine: &EngineIncarnation,
    records: Vec<DigestRecord>,
) -> Vec<u8> {
    let digester = BlockDigester::new(DIGEST_SECRET);
    let block_size = records
        .iter()
        .map(|record| record.block_token_ids)
        .max()
        .unwrap_or(2);
    let mut body = SnapshotBody {
        engine_incarnation: engine.clone(),
        watermark: WATERMARK,
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
            block_size,
        }],
        records,
    };
    body.refresh_capacity().unwrap();
    let snapshot = encode_snapshot(&body, SnapshotLimits::default()).unwrap();
    encode_authenticated_snapshot(
        &snapshot,
        SnapshotSessionBinding {
            challenge,
            engine_incarnation: engine,
            snapshot_watermark: WATERMARK,
            digest_key_id: digester.key_id().as_bytes(),
            companion_generation: generation,
        },
        &SnapshotSessionSecret::new(SESSION_SECRET),
        SnapshotSessionLimits::default(),
    )
    .unwrap()
}

fn empty_snapshot_response(challenge: SnapshotSessionChallenge, generation: u64) -> Vec<u8> {
    snapshot_response(challenge, generation, &incarnation("engine-a"), Vec::new())
}

fn single_record_response(
    challenge: SnapshotSessionChallenge,
    generation: u64,
    hash: u64,
    tokens: &[u32],
) -> Vec<u8> {
    let digester = BlockDigester::new(DIGEST_SECRET);
    snapshot_response(
        challenge,
        generation,
        &incarnation("engine-a"),
        vec![root_record(&digester, hash, tokens)],
    )
}

async fn read_hello(stream: &mut UnixStream) -> SnapshotSessionChallenge {
    let secret = SnapshotSessionSecret::new(SESSION_SECRET);
    let hello_len = encode_client_hello(
        SnapshotSessionChallenge::new([0; 32]),
        &secret,
        SnapshotSessionLimits::default(),
    )
    .unwrap()
    .len();
    let mut hello = vec![0; hello_len];
    stream.read_exact(&mut hello).await.unwrap();
    decode_client_hello(&hello, &secret, SnapshotSessionLimits::default()).unwrap()
}

fn tail(
    challenge: SnapshotSessionChallenge,
    generation: u64,
    frame_type: TailFrameType,
    message_sequence: u64,
    delivery_sequence: u64,
    event_watermark: u64,
    payload: &[u8],
) -> Vec<u8> {
    let secret = SnapshotSessionSecret::new(SESSION_SECRET);
    let key = TailSessionKey::derive(
        &secret,
        challenge,
        generation,
        TailDirection::CompanionToRouter,
    );
    let digester = BlockDigester::new(DIGEST_SECRET);
    encode_tail_frame(
        payload,
        TailFrameBinding {
            frame_type,
            direction: TailDirection::CompanionToRouter,
            session_id: challenge,
            message_sequence,
            delivery_sequence,
            event_watermark,
            engine_incarnation: &incarnation("engine-a"),
            digest_key_id: digester.key_id().as_bytes(),
            companion_generation: generation,
        },
        &key,
        TailWireLimits::default(),
    )
    .unwrap()
}

async fn wait_for_actor(
    publication: &SharedSnapshotPublication,
    condition: impl Fn(&SnapshotBootstrapActor<DigestKvIndex>) -> bool,
) {
    timeout_at(Instant::now() + Duration::from_secs(2), async {
        loop {
            if condition(&publication.lock()) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn spawn_consumer(
    consumer: Arc<SnapshotConsumer>,
    stream: UnixStream,
    challenge: SnapshotSessionChallenge,
) -> tokio::task::JoinHandle<Result<(), SnapshotConsumerError>> {
    tokio::spawn(async move {
        consumer
            .consume(stream, challenge, Instant::now() + Duration::from_secs(5))
            .await
    })
}

async fn assert_disconnected(task: tokio::task::JoinHandle<Result<(), SnapshotConsumerError>>) {
    assert!(matches!(
        task.await.unwrap().unwrap_err(),
        SnapshotConsumerError::Disconnected
    ));
}

fn index_matches(index: &DigestKvIndex, tokens: &[u32], expected: usize) -> bool {
    index
        .find_longest(tokens)
        .is_ok_and(|matched| matched.token_ids == expected)
}

fn store_payload(hash: u64, tokens: &[u32]) -> Vec<u8> {
    event_payload(vec![json!({
        "type": "BlockStored",
        "block_hashes": [hash],
        "parent_block_hash": null,
        "token_ids": tokens,
        "block_size": tokens.len(),
        "group_idx": 0,
        "kv_cache_spec_kind": "mla_attention",
        "medium": "GPU",
        "locality": "LOCAL"
    })])
}

fn remove_payload(hash: u64) -> Vec<u8> {
    event_payload(vec![json!({
        "type": "BlockRemoved",
        "block_hashes": [hash],
        "group_idx": 0,
        "medium": "GPU",
        "locality": "LOCAL"
    })])
}

fn event_payload(events: Vec<Value>) -> Vec<u8> {
    rmp_serde::to_vec(&(1.5_f64, events, 0_u32)).unwrap()
}

#[tokio::test]
async fn absolute_deadline_after_publication_revokes_owner() {
    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let publication = Arc::clone(consumer.publication());
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        companion
            .write_all(&empty_snapshot_response(challenge, GENERATION))
            .await
            .unwrap();
        companion
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        pending::<()>().await;
    });

    let error = consumer
        .consume(
            router,
            CHALLENGE_A,
            Instant::now() + Duration::from_millis(80),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, SnapshotConsumerError::Timeout));
    assert_eq!(error.reason(), "timeout");
    assert!(publication.lock().published_index().is_none());
    assert_eq!(publication.lock().session_count(), 0);
    companion_task.abort();
}

#[tokio::test]
async fn oversized_snapshot_prefix_is_rejected_before_body_read() {
    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let publication = Arc::clone(consumer.publication());
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        let mut prefix = empty_snapshot_response(challenge, GENERATION);
        prefix[12..20].copy_from_slice(&u64::MAX.to_be_bytes());
        prefix.truncate(SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES);
        companion.write_all(&prefix).await.unwrap();
        pending::<()>().await;
    });
    let error = consumer
        .consume(router, CHALLENGE_A, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SnapshotConsumerError::Session(SnapshotSessionError::ResponseFrameTooLarge)
    ));
    assert_eq!(publication.lock().session_count(), 0);
    companion_task.abort();
}

#[tokio::test]
async fn oversized_tail_prefix_is_rejected_before_payload_read_and_revokes() {
    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let publication = Arc::clone(consumer.publication());
    let (release_tx, release_rx) = oneshot::channel();
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        companion
            .write_all(&empty_snapshot_response(challenge, GENERATION))
            .await
            .unwrap();
        companion
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = release_rx.await;
        let mut prefix = tail(challenge, GENERATION, TailFrameType::Identity, 2, 0, 0, b"");
        prefix[12..20].copy_from_slice(&u64::MAX.to_be_bytes());
        prefix.truncate(TAIL_FRAME_LENGTH_PREFIX_BYTES);
        companion.write_all(&prefix).await.unwrap();
    });
    let consume_task = tokio::spawn({
        let consumer = Arc::clone(&consumer);
        async move {
            consumer
                .consume(router, CHALLENGE_A, Instant::now() + Duration::from_secs(2))
                .await
        }
    });
    wait_for_actor(&publication, |actor| actor.published_index().is_some()).await;
    release_tx.send(()).unwrap();
    let error = consume_task.await.unwrap().unwrap_err();
    assert!(matches!(
        error,
        SnapshotConsumerError::Tail(TailWireError::FrameTooLarge)
    ));
    companion_task.await.unwrap();
    assert!(publication.lock().published_index().is_none());
}

#[tokio::test]
async fn truncated_snapshot_and_tail_frames_fail_closed() {
    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        let response = empty_snapshot_response(challenge, GENERATION);
        companion.write_all(&response[..17]).await.unwrap();
        companion.shutdown().await.unwrap();
    });
    let error = consumer
        .consume(router, CHALLENGE_A, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(error, SnapshotConsumerError::Truncated));
    companion_task.await.unwrap();

    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let publication = Arc::clone(consumer.publication());
    let (release_tx, release_rx) = oneshot::channel();
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        companion
            .write_all(&empty_snapshot_response(challenge, GENERATION))
            .await
            .unwrap();
        companion
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = release_rx.await;
        let frame = tail(challenge, GENERATION, TailFrameType::Identity, 2, 0, 0, b"");
        companion.write_all(&frame[..17]).await.unwrap();
        companion.shutdown().await.unwrap();
    });
    let consume_task = tokio::spawn({
        let consumer = Arc::clone(&consumer);
        async move {
            consumer
                .consume(router, CHALLENGE_A, Instant::now() + Duration::from_secs(2))
                .await
        }
    });
    wait_for_actor(&publication, |actor| actor.published_index().is_some()).await;
    release_tx.send(()).unwrap();
    assert!(matches!(
        consume_task.await.unwrap().unwrap_err(),
        SnapshotConsumerError::Truncated
    ));
    companion_task.await.unwrap();
    assert!(publication.lock().published_index().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_large_blocking_build_releases_actor_immediately() {
    let digester = BlockDigester::new(DIGEST_SECRET);
    let records = (1_u64..=60_000)
        .map(|hash| root_record(&digester, hash, &[u32::try_from(hash).unwrap()]))
        .collect();
    let response = snapshot_response(CHALLENGE_A, GENERATION, &incarnation("engine-a"), records);
    assert!(response.len() > 8 * 1024 * 1024);

    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let publication = Arc::clone(consumer.publication());
    let companion_task = tokio::spawn(async move {
        let _ = read_hello(&mut companion).await;
        let _ = companion.write_all(&response).await;
        pending::<()>().await;
    });
    let consume_task = tokio::spawn(async move {
        consumer
            .consume(
                router,
                CHALLENGE_A,
                Instant::now() + Duration::from_secs(30),
            )
            .await
    });
    wait_for_actor(&publication, |actor| actor.session_count() == 1).await;
    consume_task.abort();
    assert!(consume_task.await.unwrap_err().is_cancelled());
    assert_eq!(publication.lock().session_count(), 0);
    assert!(publication.lock().published_index().is_none());
    companion_task.abort();
}

#[tokio::test]
async fn same_identity_replacement_preserves_then_atomically_replaces_publication() {
    let (router_a, mut incumbent_server) = UnixStream::pair().unwrap();
    let (router_b, mut candidate_server) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router_a);
    let publication = Arc::clone(consumer.publication());
    let (close_a_tx, close_a_rx) = oneshot::channel();
    let incumbent_task = tokio::spawn(async move {
        let challenge = read_hello(&mut incumbent_server).await;
        incumbent_server
            .write_all(&single_record_response(challenge, GENERATION, 11, &[1, 2]))
            .await
            .unwrap();
        incumbent_server
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = close_a_rx.await;
    });
    let consume_a = spawn_consumer(Arc::clone(&consumer), router_a, CHALLENGE_A);
    wait_for_actor(&publication, |actor| {
        actor
            .published_index()
            .is_some_and(|index| index_matches(index, &[1, 2], 2))
    })
    .await;

    let (publish_b_tx, publish_b_rx) = oneshot::channel();
    let (close_b_tx, close_b_rx) = oneshot::channel();
    let candidate_task = tokio::spawn(async move {
        let challenge = read_hello(&mut candidate_server).await;
        candidate_server
            .write_all(&single_record_response(challenge, GENERATION, 22, &[3, 4]))
            .await
            .unwrap();
        let _ = publish_b_rx.await;
        candidate_server
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = close_b_rx.await;
    });
    let consume_b = spawn_consumer(Arc::clone(&consumer), router_b, CHALLENGE_B);
    wait_for_actor(&publication, |actor| actor.session_count() == 2).await;
    assert!(
        publication
            .lock()
            .published_index()
            .is_some_and(|index| index_matches(index, &[1, 2], 2))
    );

    publish_b_tx.send(()).unwrap();
    wait_for_actor(&publication, |actor| {
        actor
            .published_index()
            .is_some_and(|index| index_matches(index, &[3, 4], 2))
    })
    .await;
    close_a_tx.send(()).unwrap();
    assert_disconnected(consume_a).await;
    incumbent_task.await.unwrap();
    assert!(
        publication
            .lock()
            .published_index()
            .is_some_and(|index| index_matches(index, &[3, 4], 2))
    );

    close_b_tx.send(()).unwrap();
    assert_disconnected(consume_b).await;
    candidate_task.await.unwrap();
    assert!(publication.lock().published_index().is_none());
}

#[tokio::test]
async fn generation_rollover_revokes_old_identity_before_republication() {
    let (router_a, mut incumbent_server) = UnixStream::pair().unwrap();
    let (router_b, mut successor_server) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router_a);
    let publication = Arc::clone(consumer.publication());
    let (close_a_tx, close_a_rx) = oneshot::channel();
    let incumbent_task = tokio::spawn(async move {
        let challenge = read_hello(&mut incumbent_server).await;
        incumbent_server
            .write_all(&single_record_response(challenge, GENERATION, 11, &[1, 2]))
            .await
            .unwrap();
        incumbent_server
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = close_a_rx.await;
    });
    let consume_a = spawn_consumer(Arc::clone(&consumer), router_a, CHALLENGE_A);
    wait_for_actor(&publication, |actor| actor.published_index().is_some()).await;

    let next_generation = GENERATION + 1;
    let (publish_b_tx, publish_b_rx) = oneshot::channel();
    let (close_b_tx, close_b_rx) = oneshot::channel();
    let successor_task = tokio::spawn(async move {
        let challenge = read_hello(&mut successor_server).await;
        successor_server
            .write_all(&single_record_response(
                challenge,
                next_generation,
                22,
                &[3, 4],
            ))
            .await
            .unwrap();
        let _ = publish_b_rx.await;
        successor_server
            .write_all(&tail(
                challenge,
                next_generation,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = close_b_rx.await;
    });
    let consume_b = spawn_consumer(Arc::clone(&consumer), router_b, CHALLENGE_B);
    wait_for_actor(&publication, |actor| {
        actor.published_index().is_none() && actor.session_count() == 1
    })
    .await;
    publish_b_tx.send(()).unwrap();
    wait_for_actor(&publication, |actor| {
        actor
            .published_index()
            .is_some_and(|index| index_matches(index, &[3, 4], 2))
    })
    .await;

    close_a_tx.send(()).unwrap();
    assert_disconnected(consume_a).await;
    incumbent_task.await.unwrap();
    assert!(publication.lock().published_index().is_some());
    close_b_tx.send(()).unwrap();
    assert_disconnected(consume_b).await;
    successor_task.await.unwrap();
    assert!(publication.lock().published_index().is_none());
}

#[tokio::test]
async fn authenticated_store_and_remove_mutate_published_index() {
    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let publication = Arc::clone(consumer.publication());
    let (store_tx, store_rx) = oneshot::channel();
    let (remove_tx, remove_rx) = oneshot::channel();
    let (close_tx, close_rx) = oneshot::channel();
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        companion
            .write_all(&empty_snapshot_response(challenge, GENERATION))
            .await
            .unwrap();
        companion
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
            ))
            .await
            .unwrap();
        let _ = store_rx.await;
        companion
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::Event,
                2,
                1,
                WATERMARK + 1,
                &store_payload(55, &[10, 20]),
            ))
            .await
            .unwrap();
        let _ = remove_rx.await;
        companion
            .write_all(&tail(
                challenge,
                GENERATION,
                TailFrameType::Event,
                3,
                2,
                WATERMARK + 2,
                &remove_payload(55),
            ))
            .await
            .unwrap();
        let _ = close_rx.await;
    });
    let consume_task = tokio::spawn({
        let consumer = Arc::clone(&consumer);
        async move {
            consumer
                .consume(router, CHALLENGE_A, Instant::now() + Duration::from_secs(5))
                .await
        }
    });
    wait_for_actor(&publication, |actor| actor.published_index().is_some()).await;
    store_tx.send(()).unwrap();
    wait_for_actor(&publication, |actor| {
        actor
            .published_index()
            .is_some_and(|index| index_matches(index, &[10, 20], 2))
    })
    .await;
    remove_tx.send(()).unwrap();
    wait_for_actor(&publication, |actor| {
        actor
            .published_index()
            .is_some_and(|index| index_matches(index, &[10, 20], 0))
    })
    .await;
    close_tx.send(()).unwrap();
    assert!(matches!(
        consume_task.await.unwrap().unwrap_err(),
        SnapshotConsumerError::Disconnected
    ));
    companion_task.await.unwrap();
}

#[tokio::test]
async fn errors_and_debug_output_never_echo_authenticated_content() {
    const PRIVATE_MARKER: &str = "private-engine-marker-do-not-log";
    let (router, mut companion) = UnixStream::pair().unwrap();
    let consumer = consumer_for(&router);
    let companion_task = tokio::spawn(async move {
        let challenge = read_hello(&mut companion).await;
        companion
            .write_all(&snapshot_response(
                challenge,
                GENERATION,
                &incarnation(PRIVATE_MARKER),
                Vec::new(),
            ))
            .await
            .unwrap();
    });
    let error = consumer
        .consume(router, CHALLENGE_A, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SnapshotConsumerError::Session(SnapshotSessionError::IncarnationMismatch)
    ));
    let display = error.to_string();
    let debug = format!("{error:?}");
    for forbidden in [
        PRIVATE_MARKER,
        "snapshot-session-secret-32-byte",
        "sha256:image-a",
    ] {
        assert!(!display.contains(forbidden));
        assert!(!debug.contains(forbidden));
    }
    assert_eq!(error.reason(), "incarnation_mismatch");
    companion_task.await.unwrap();
}
