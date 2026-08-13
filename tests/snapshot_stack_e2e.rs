use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mini_dynamo::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{
        AttentionKind, DigestAlgorithm, DigestRecord, DigestSpec, EngineIncarnation,
        GroupDisposition, GroupMetadata, ResetScope, SnapshotBlockHash, SnapshotBody,
        SnapshotCapacity, SnapshotLimits, encode_snapshot,
    },
    kv_wire::KvWireLimits,
    snapshot_actor::{SessionEpoch, SnapshotActorLimits},
    snapshot_consumer::{SharedSnapshotPublication, SnapshotConsumer, SnapshotConsumerConfig},
    snapshot_producer::{
        ProducerIdentity, ProducerSnapshot, ProducerTailEvent, SnapshotBuildFuture,
        SnapshotProducer, SnapshotProducerCancellation, SnapshotProducerConfig,
        SnapshotProducerSource, SnapshotProducerSourceError, SnapshotTailPublisher,
    },
    snapshot_reconnect::{
        SnapshotReconnectConfig, SnapshotReconnectHandle, SnapshotReconnectOwner,
        SnapshotReconnectReport,
    },
    snapshot_session::{
        SnapshotSessionChallenge, SnapshotSessionLimits, SnapshotSessionSecret, encode_client_hello,
    },
    snapshot_socket_path::{PublishedSocketPath, SocketParentPolicy, bind_and_publish},
    snapshot_supervisor::{
        SnapshotSupervisorConfig, SnapshotSupervisorReport, supervise_snapshot_sessions,
    },
    snapshot_tail_wire::TailWireLimits,
};
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
const DIGEST_SECRET: [u8; 32] = *b"0123456789abcdef0123456789abcdef";
const INITIAL_WATERMARK: u64 = 100;
const INITIAL_GENERATION: u64 = 7;
const LIVE_HASH: u64 = 90_001;
const LIVE_TOKENS: [u32; 2] = [90_001, 90_002];
const WAIT: Duration = Duration::from_secs(3);
static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    root: PathBuf,
    socket: PathBuf,
    policy: SocketParentPolicy,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "mini-dynamo-snapshot-e2e-{:x}-{sequence:x}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let policy = SocketParentPolicy {
            owner_uid: fs::metadata(&root).unwrap().uid(),
        };
        Self {
            socket: root.join("snapshot.sock"),
            root,
            policy,
        }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone)]
struct PublishedSession {
    id: u64,
    publisher: SnapshotTailPublisher,
}

struct CapturedState {
    identity: ProducerIdentity,
    watermark: u64,
    records: Vec<DigestRecord>,
    sessions: Vec<PublishedSession>,
}

struct CapturedSource {
    state: Arc<Mutex<CapturedState>>,
    next_session: AtomicU64,
}

impl CapturedSource {
    fn new(identity: ProducerIdentity, record_count: usize) -> Arc<Self> {
        let digester = BlockDigester::new(DIGEST_SECRET);
        let records = (0..record_count)
            .map(|index| {
                let token = u32::try_from(index + 1).unwrap();
                root_record(&digester, u64::from(token), &[token, token | 0x8000_0000])
            })
            .collect();
        Arc::new(Self {
            state: Arc::new(Mutex::new(CapturedState {
                identity,
                watermark: INITIAL_WATERMARK,
                records,
                sessions: Vec::new(),
            })),
            next_session: AtomicU64::new(1),
        })
    }

    fn active_sessions(&self) -> usize {
        self.state.lock().unwrap().sessions.len()
    }

    fn identity(&self) -> ProducerIdentity {
        self.state.lock().unwrap().identity.clone()
    }

    async fn store_live_root(&self) {
        let digester = BlockDigester::new(DIGEST_SECRET);
        let (event, sessions) = {
            let mut state = self.state.lock().unwrap();
            state
                .records
                .retain(|record| record.external_hash != SnapshotBlockHash::Unsigned(LIVE_HASH));
            state
                .records
                .push(root_record(&digester, LIVE_HASH, &LIVE_TOKENS));
            state.watermark += 1;
            let event = ProducerTailEvent::Batch {
                identity: state.identity.clone(),
                event_watermark: state.watermark,
                payload: store_payload(LIVE_HASH, &LIVE_TOKENS),
            };
            (event, state.sessions.clone())
        };
        self.broadcast(event, sessions).await;
    }

    async fn remove_live_root(&self) {
        let (event, sessions) = {
            let mut state = self.state.lock().unwrap();
            state
                .records
                .retain(|record| record.external_hash != SnapshotBlockHash::Unsigned(LIVE_HASH));
            state.watermark += 1;
            let event = ProducerTailEvent::Batch {
                identity: state.identity.clone(),
                event_watermark: state.watermark,
                payload: remove_payload(LIVE_HASH),
            };
            (event, state.sessions.clone())
        };
        self.broadcast(event, sessions).await;
    }

    async fn rollover(&self, identity: ProducerIdentity) {
        let sessions = {
            let mut state = self.state.lock().unwrap();
            state.identity = identity.clone();
            state.sessions.clone()
        };
        self.broadcast(ProducerTailEvent::IdentityChanged(identity), sessions)
            .await;
    }

    async fn broadcast(&self, event: ProducerTailEvent, sessions: Vec<PublishedSession>) {
        for session in sessions {
            if session.publisher.send(clone_event(&event)).await.is_err() {
                self.remove_session(session.id);
            }
        }
    }

    fn remove_session(&self, id: u64) {
        self.state
            .lock()
            .unwrap()
            .sessions
            .retain(|session| session.id != id);
    }
}

impl SnapshotProducerSource for CapturedSource {
    fn start(
        &self,
        publisher: SnapshotTailPublisher,
        mut cancellation: SnapshotProducerCancellation,
    ) -> Result<SnapshotBuildFuture, SnapshotProducerSourceError> {
        let session_id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let (snapshot, identity, watermark) = {
            let mut state = self.state.lock().unwrap();
            let snapshot = producer_snapshot(&state);
            state.sessions.push(PublishedSession {
                id: session_id,
                publisher: publisher.clone(),
            });
            (snapshot, state.identity.clone(), state.watermark)
        };
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let _ = publisher
                .send(ProducerTailEvent::CaughtUp {
                    identity,
                    event_watermark: watermark,
                })
                .await;
            cancellation.cancelled().await;
            state
                .lock()
                .unwrap()
                .sessions
                .retain(|session| session.id != session_id);
        });
        Ok(Box::pin(async move { Ok(snapshot) }))
    }
}

struct CompanionRun {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<
        Result<SnapshotSupervisorReport, mini_dynamo::snapshot_supervisor::SnapshotSupervisorError>,
    >,
}

impl CompanionRun {
    async fn stop(self) -> SnapshotSupervisorReport {
        self.shutdown.send(true).unwrap();
        timeout(WAIT, self.task).await.unwrap().unwrap().unwrap()
    }
}

struct OwnerRun {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<SnapshotReconnectReport>,
    replacements: SnapshotReconnectHandle,
}

impl OwnerRun {
    async fn stop(self) -> SnapshotReconnectReport {
        self.shutdown.send(true).unwrap();
        timeout(WAIT, self.task).await.unwrap().unwrap()
    }
}

fn start_companion(directory: &TestDirectory, source: Arc<CapturedSource>) -> CompanionRun {
    let published = bind_and_publish(&directory.socket, directory.policy).unwrap();
    let (listener, guard) = published.into_parts();
    listener.set_nonblocking(true).unwrap();
    let listener = UnixListener::from_std(listener).unwrap();
    let producer = Arc::new(
        SnapshotProducer::new(
            SnapshotProducerConfig {
                expected_peer_uid: directory.policy.owner_uid,
                session_limits: SnapshotSessionLimits::default(),
                tail_limits: TailWireLimits::default(),
                tail_queue_capacity: 16,
            },
            Arc::new(SnapshotSessionSecret::new(SESSION_SECRET)),
            source,
        )
        .unwrap(),
    );
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(run_supervisor(listener, guard, producer, receiver));
    CompanionRun { shutdown, task }
}

async fn run_supervisor(
    listener: UnixListener,
    guard: PublishedSocketPath,
    producer: Arc<SnapshotProducer>,
    shutdown: watch::Receiver<bool>,
) -> Result<SnapshotSupervisorReport, mini_dynamo::snapshot_supervisor::SnapshotSupervisorError> {
    let report = supervise_snapshot_sessions(
        listener,
        SnapshotSupervisorConfig::new(Duration::from_secs(30)).unwrap(),
        shutdown,
        move |stream, deadline| {
            let producer = Arc::clone(&producer);
            async move { producer.handle(stream, deadline).await }
        },
    )
    .await;
    drop(guard);
    report
}

fn consumer(identity: &ProducerIdentity, uid: u32) -> Arc<SnapshotConsumer> {
    Arc::new(
        SnapshotConsumer::new(
            SnapshotConsumerConfig {
                expected_peer_uid: uid,
                expected_engine_incarnation: identity.engine_incarnation.clone(),
                minimum_snapshot_watermark: INITIAL_WATERMARK,
                minimum_companion_generation: identity.companion_generation,
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

fn start_owner(directory: &TestDirectory, consumer: Arc<SnapshotConsumer>) -> OwnerRun {
    let config = SnapshotReconnectConfig::new(
        directory.socket.clone(),
        directory.policy,
        Duration::from_secs(30),
        Duration::from_millis(2),
        Duration::from_millis(20),
        64,
    )
    .unwrap();
    let (owner, replacements) = SnapshotReconnectOwner::new(config, consumer).unwrap();
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(owner.run(receiver));
    OwnerRun {
        shutdown,
        task,
        replacements,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_stack_recovers_and_fails_closed_across_every_lifecycle() {
    let directory = TestDirectory::new();
    let identity_v1 = identity("engine-a", INITIAL_GENERATION, 42);
    let source = CapturedSource::new(identity_v1.clone(), 256);
    let mut companion = start_companion(&directory, Arc::clone(&source));
    let consumer_v1 = consumer(&identity_v1, directory.policy.owner_uid);
    let publication_v1 = Arc::clone(consumer_v1.publication());
    let mut owner = start_owner(&directory, Arc::clone(&consumer_v1));

    let first_epoch = wait_for_published(&publication_v1, None).await;
    assert_index_match(&publication_v1, &[1, 0x8000_0001], 2);
    wait_for_sessions(&source, 1).await;

    source.store_live_root().await;
    wait_for_match(&publication_v1, &LIVE_TOKENS, 2).await;
    source.remove_live_root().await;
    wait_for_match(&publication_v1, &LIVE_TOKENS, 0).await;

    owner.replacements.request_replacement().unwrap();
    let replacement_epoch = wait_for_published(&publication_v1, Some(first_epoch)).await;
    wait_for_sessions(&source, 1).await;
    assert_ne!(replacement_epoch, first_epoch);

    let report = owner.stop().await;
    assert!(report.replacement_promotions >= 1);
    wait_for_unpublished(&publication_v1).await;
    wait_for_sessions(&source, 0).await;
    owner = start_owner(&directory, Arc::clone(&consumer_v1));
    let restarted_epoch = wait_for_published(&publication_v1, None).await;
    assert_ne!(restarted_epoch, replacement_epoch);

    let companion_report = companion.stop().await;
    assert!(companion_report.sessions_cancelled >= 1);
    wait_for_unpublished(&publication_v1).await;
    wait_for_sessions(&source, 0).await;
    assert!(!directory.socket.exists());
    companion = start_companion(&directory, Arc::clone(&source));
    let after_companion_restart = wait_for_published(&publication_v1, None).await;
    assert_ne!(after_companion_restart, restarted_epoch);

    let identity_v2 = identity("engine-a-rollover", INITIAL_GENERATION + 1, 84);
    source.rollover(identity_v2.clone()).await;
    wait_for_unpublished(&publication_v1).await;
    wait_for_sessions(&source, 0).await;
    let old_report = owner.stop().await;
    assert!(old_report.session_failures >= 1);

    let consumer_v2 = consumer(&identity_v2, directory.policy.owner_uid);
    let publication_v2 = Arc::clone(consumer_v2.publication());
    let owner_v2 = start_owner(&directory, consumer_v2);
    wait_for_published(&publication_v2, None).await;
    assert_eq!(source.identity(), identity_v2);
    assert_index_match(&publication_v2, &[1, 0x8000_0001], 2);

    let final_owner_report = owner_v2.stop().await;
    assert_eq!(final_owner_report.shutdown_cancellations, 1);
    wait_for_unpublished(&publication_v2).await;
    wait_for_sessions(&source, 0).await;
    let final_companion_report = companion.stop().await;
    assert!(final_companion_report.accepted_connections >= 1);
    assert!(!directory.socket.exists());
    assert_eq!(source.active_sessions(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_rejects_third_client_while_two_slow_readers_hold_capacity() {
    let directory = TestDirectory::new();
    let identity = identity("engine-capacity", INITIAL_GENERATION, 42);
    let source = CapturedSource::new(identity, 2_048);
    let companion = start_companion(&directory, Arc::clone(&source));
    let secret = SnapshotSessionSecret::new(SESSION_SECRET);

    let mut first = connect_slow_reader(&directory.socket, &secret, 0x31).await;
    wait_for_sessions(&source, 1).await;
    let mut second = connect_slow_reader(&directory.socket, &secret, 0x32).await;
    wait_for_sessions(&source, 2).await;
    let mut rejected = UnixStream::connect(&directory.socket).await.unwrap();
    let mut byte = [0_u8; 1];
    let read = timeout(WAIT, rejected.read(&mut byte)).await.unwrap();
    assert!(read.is_err() || read.unwrap() == 0);
    assert_eq!(source.active_sessions(), 2);

    let report = companion.stop().await;
    assert_eq!(report.sessions_started, 2);
    assert_eq!(report.connections_rejected_capacity, 1);
    assert_eq!(report.sessions_cancelled, 2);
    let _ = first.read(&mut byte).await;
    let _ = second.read(&mut byte).await;
    wait_for_sessions(&source, 0).await;
    assert!(!directory.socket.exists());
}

async fn connect_slow_reader(
    path: &Path,
    secret: &SnapshotSessionSecret,
    challenge: u8,
) -> UnixStream {
    use tokio::io::AsyncWriteExt;

    let mut stream = UnixStream::connect(path).await.unwrap();
    let hello = encode_client_hello(
        SnapshotSessionChallenge::new([challenge; 32]),
        secret,
        SnapshotSessionLimits::default(),
    )
    .unwrap();
    stream.write_all(&hello).await.unwrap();
    stream
}

async fn wait_for_published(
    publication: &SharedSnapshotPublication,
    different_from: Option<SessionEpoch>,
) -> SessionEpoch {
    wait_until(|| {
        publication
            .lock()
            .published_epoch()
            .filter(|epoch| Some(*epoch) != different_from)
    })
    .await
}

async fn wait_for_unpublished(publication: &SharedSnapshotPublication) {
    wait_until(|| {
        if publication.lock().published_index().is_none() {
            Some(())
        } else {
            None
        }
    })
    .await;
}

async fn wait_for_sessions(source: &CapturedSource, expected: usize) {
    wait_until(|| (source.active_sessions() == expected).then_some(())).await;
}

async fn wait_for_match(publication: &SharedSnapshotPublication, tokens: &[u32], expected: usize) {
    wait_until(|| {
        publication.lock().published_index().and_then(|index| {
            index
                .find_longest(tokens)
                .ok()
                .filter(|matched| matched.token_ids == expected)
                .map(|_| ())
        })
    })
    .await;
}

async fn wait_until<T>(mut check: impl FnMut() -> Option<T>) -> T {
    timeout(WAIT, async {
        loop {
            if let Some(value) = check() {
                return value;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap()
}

fn assert_index_match(publication: &SharedSnapshotPublication, tokens: &[u32], expected: usize) {
    let publication = publication.lock();
    let index: &DigestKvIndex = publication.published_index().unwrap();
    assert_eq!(index.find_longest(tokens).unwrap().token_ids, expected);
}

fn identity(name: &str, generation: u64, started: u64) -> ProducerIdentity {
    let digester = BlockDigester::new(DIGEST_SECRET);
    ProducerIdentity {
        engine_incarnation: EngineIncarnation {
            engine_id: name.to_owned(),
            model_revision: "captured-revision".to_owned(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            process_started_unix_ns: started,
            attestation_sha256: vec![0x11; 32],
        },
        digest_key_id: *digester.key_id().as_bytes(),
        companion_generation: generation,
    }
}

fn producer_snapshot(state: &CapturedState) -> ProducerSnapshot {
    let mut body = SnapshotBody {
        engine_incarnation: state.identity.engine_incarnation.clone(),
        watermark: state.watermark,
        reset_scope: ResetScope::full_engine(),
        digest: DigestSpec {
            algorithm: DigestAlgorithm::HmacSha256V1,
            key_id: state.identity.digest_key_id.to_vec(),
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
        records: state.records.clone(),
    };
    body.refresh_capacity().unwrap();
    ProducerSnapshot {
        identity: state.identity.clone(),
        watermark: state.watermark,
        frame: encode_snapshot(&body, SnapshotLimits::default()).unwrap(),
    }
}

fn root_record(digester: &BlockDigester, hash: u64, tokens: &[u32]) -> DigestRecord {
    DigestRecord {
        group_slot: 0,
        parent_record: None,
        external_hash: SnapshotBlockHash::Unsigned(hash),
        block_digest: digester.commit(tokens).unwrap().digest_bytes().to_vec(),
        block_token_ids: u32::try_from(tokens.len()).unwrap(),
        prefix_token_ids: u64::try_from(tokens.len()).unwrap(),
        present: true,
    }
}

fn clone_event(event: &ProducerTailEvent) -> ProducerTailEvent {
    match event {
        ProducerTailEvent::Batch {
            identity,
            event_watermark,
            payload,
        } => ProducerTailEvent::Batch {
            identity: identity.clone(),
            event_watermark: *event_watermark,
            payload: payload.clone(),
        },
        ProducerTailEvent::CaughtUp {
            identity,
            event_watermark,
        } => ProducerTailEvent::CaughtUp {
            identity: identity.clone(),
            event_watermark: *event_watermark,
        },
        ProducerTailEvent::IdentityChanged(identity) => {
            ProducerTailEvent::IdentityChanged(identity.clone())
        }
        ProducerTailEvent::Disconnect(identity) => ProducerTailEvent::Disconnect(identity.clone()),
    }
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
