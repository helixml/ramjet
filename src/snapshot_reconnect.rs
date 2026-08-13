//! LB-side outbound ownership for authenticated snapshot consumer sessions.
//!
//! Normal reconnects are strictly serial: one engine has at most one active
//! consumer attempt. An explicit replacement request may temporarily overlap a
//! second attempt; the old session is dropped only after the actor publishes a
//! different epoch. Failed replacements leave the old session untouched.
//! Approximate serving is outside this module and is never stopped or gated.

use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio::{
    net::UnixStream,
    sync::{mpsc, watch},
    time::{Instant, interval, sleep, timeout_at},
};

use crate::{
    snapshot_consumer::{SnapshotConsumer, SnapshotConsumerError},
    snapshot_session::SnapshotSessionChallenge,
    snapshot_socket_path::{
        SnapshotSocketPathError, SocketParentPolicy, validate_socket_client_path,
    },
};

const CHALLENGE_ATTEMPTS: usize = 16;
pub const MAX_CHALLENGE_LEDGER_CAPACITY: usize = 65_536;
const REPLACEMENT_CHANNEL_CAPACITY: usize = 1;
const PUBLICATION_POLL_INTERVAL: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotReconnectConfig {
    socket_path: PathBuf,
    socket_policy: SocketParentPolicy,
    attempt_timeout: Duration,
    reconnect_min: Duration,
    reconnect_max: Duration,
    challenge_ledger_capacity: usize,
}

impl SnapshotReconnectConfig {
    /// Construct and validate an outbound reconnect policy.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe socket path, zero bounds/capacity, inverted backoff,
    /// or a backoff duration too large for bounded nanosecond jitter.
    pub fn new(
        socket_path: PathBuf,
        socket_policy: SocketParentPolicy,
        attempt_timeout: Duration,
        reconnect_min: Duration,
        reconnect_max: Duration,
        challenge_ledger_capacity: usize,
    ) -> Result<Self, SnapshotReconnectError> {
        validate_socket_client_path(&socket_path, socket_policy)
            .map_err(SnapshotReconnectError::SocketPath)?;
        if attempt_timeout.is_zero()
            || reconnect_min.is_zero()
            || reconnect_min > reconnect_max
            || challenge_ledger_capacity == 0
            || challenge_ledger_capacity > MAX_CHALLENGE_LEDGER_CAPACITY
            || reconnect_max.as_nanos() > u128::from(u64::MAX)
        {
            return Err(SnapshotReconnectError::InvalidConfig);
        }
        Ok(Self {
            socket_path,
            socket_policy,
            attempt_timeout,
            reconnect_min,
            reconnect_max,
            challenge_ledger_capacity,
        })
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotReconnectReport {
    pub attempts_started: u64,
    pub connect_failures: u64,
    pub session_failures: u64,
    pub attempts_timed_out: u64,
    pub random_failures: u64,
    pub replacement_requests: u64,
    pub replacement_promotions: u64,
    pub replacement_failures: u64,
    pub shutdown_cancellations: u64,
}

#[derive(Debug, Error)]
pub enum SnapshotReconnectError {
    #[error("snapshot reconnect configuration is invalid")]
    InvalidConfig,
    #[error("snapshot reconnect socket path is unsafe")]
    SocketPath(#[source] SnapshotSocketPathError),
}

impl SnapshotReconnectError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_config",
            Self::SocketPath(error) => error.reason(),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ReplacementRequestError {
    #[error("snapshot replacement request is already pending")]
    AlreadyPending,
    #[error("snapshot reconnect owner is not running")]
    OwnerStopped,
}

#[derive(Clone)]
pub struct SnapshotReconnectHandle {
    replacement: mpsc::Sender<()>,
}

impl SnapshotReconnectHandle {
    /// Explicitly request a rolling replacement. Repeated pending requests are
    /// coalesced rather than creating more than two concurrent sessions.
    ///
    /// # Errors
    ///
    /// Returns a bounded state error when a request is pending or the owner has
    /// stopped.
    pub fn request_replacement(&self) -> Result<(), ReplacementRequestError> {
        self.replacement.try_send(()).map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => ReplacementRequestError::AlreadyPending,
            mpsc::error::TrySendError::Closed(()) => ReplacementRequestError::OwnerStopped,
        })
    }
}

trait RandomSource: Send {
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), ()>;
}

struct OsRandomSource;

impl RandomSource for OsRandomSource {
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), ()> {
        getrandom::fill(destination).map_err(|_| ())
    }
}

pub struct SnapshotReconnectOwner {
    config: SnapshotReconnectConfig,
    consumer: Arc<SnapshotConsumer>,
    random: Box<dyn RandomSource>,
    challenges: ChallengeLedger,
    replacements: mpsc::Receiver<()>,
}

impl SnapshotReconnectOwner {
    /// Construct the production owner with operating-system randomness.
    ///
    /// # Errors
    ///
    /// Revalidates the configured socket path before retaining it.
    pub fn new(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
    ) -> Result<(Self, SnapshotReconnectHandle), SnapshotReconnectError> {
        Self::with_random(config, consumer, Box::new(OsRandomSource))
    }

    fn with_random(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        random: Box<dyn RandomSource>,
    ) -> Result<(Self, SnapshotReconnectHandle), SnapshotReconnectError> {
        validate_socket_client_path(&config.socket_path, config.socket_policy)
            .map_err(SnapshotReconnectError::SocketPath)?;
        let challenges = ChallengeLedger::new(config.challenge_ledger_capacity);
        let (replacement, replacements) = mpsc::channel(REPLACEMENT_CHANNEL_CAPACITY);
        Ok((
            Self {
                config,
                consumer,
                random,
                challenges,
                replacements,
            },
            SnapshotReconnectHandle { replacement },
        ))
    }

    /// Reconnect until shutdown. Failures affect only exact publication state;
    /// approximate serving is not owned or mutated here.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> SnapshotReconnectReport {
        let mut report = SnapshotReconnectReport::default();
        let mut backoff = self.config.reconnect_min;
        let mut delay_before_attempt = false;
        let mut replacements_open = true;

        loop {
            if delay_before_attempt {
                let delay = jittered_delay(backoff, self.random.as_mut()).unwrap_or_else(|()| {
                    report.random_failures = report.random_failures.saturating_add(1);
                    backoff
                });
                backoff = next_backoff(backoff, self.config.reconnect_max);
                if self
                    .wait_before_retry(delay, &mut shutdown, &mut report, &mut replacements_open)
                    .await
                {
                    return report;
                }
            }
            delay_before_attempt = true;

            let Some(mut active) = self.start_attempt(&mut report) else {
                continue;
            };
            let mut published = self.consumer.publication().lock().published_epoch();
            let mut publication_tick = interval(PUBLICATION_POLL_INTERVAL);

            loop {
                tokio::select! {
                    biased;
                    () = wait_for_shutdown(&mut shutdown) => {
                        drop(active);
                        report.shutdown_cancellations =
                            report.shutdown_cancellations.saturating_add(1);
                        return report;
                    }
                    replacement = self.replacements.recv(), if replacements_open => {
                        let Some(()) = replacement else {
                            replacements_open = false;
                            continue;
                        };
                        report.replacement_requests = report.replacement_requests.saturating_add(1);
                        let Some(replacement) = self.start_attempt(&mut report) else {
                            report.replacement_failures =
                                report.replacement_failures.saturating_add(1);
                            continue;
                        };
                        match self.roll_explicit_replacement(
                            active,
                            replacement,
                            published,
                            &mut shutdown,
                            &mut report,
                            &mut replacements_open,
                        ).await {
                            RollOutcome::Active(next) => {
                                active = next;
                                published = self.consumer.publication().lock().published_epoch();
                            }
                            RollOutcome::Shutdown => return report,
                        }
                    }
                    outcome = &mut active => {
                        record_attempt_end(&mut report, &outcome);
                        break;
                    }
                    _ = publication_tick.tick() => {
                        let current = self.consumer.publication().lock().published_epoch();
                        if current.is_some() && current != published {
                            published = current;
                            backoff = self.config.reconnect_min;
                        }
                    }
                }
            }
        }
    }

    fn start_attempt(&mut self, report: &mut SnapshotReconnectReport) -> Option<AttemptFuture> {
        let Some(deadline) = Instant::now().checked_add(self.config.attempt_timeout) else {
            report.session_failures = report.session_failures.saturating_add(1);
            return None;
        };
        let Ok(challenge) = self.challenges.generate(self.random.as_mut()) else {
            report.random_failures = report.random_failures.saturating_add(1);
            return None;
        };
        report.attempts_started = report.attempts_started.saturating_add(1);
        let path = self.config.socket_path.clone();
        let policy = self.config.socket_policy;
        let consumer = Arc::clone(&self.consumer);
        Some(Box::pin(async move {
            let attempt = async {
                validate_socket_client_path(&path, policy).map_err(AttemptEnd::SocketPath)?;
                let stream = UnixStream::connect(&path)
                    .await
                    .map_err(|_| AttemptEnd::Connect)?;
                consumer
                    .consume(stream, challenge, deadline)
                    .await
                    .map_err(AttemptEnd::Consumer)
            };
            timeout_at(deadline, attempt)
                .await
                .unwrap_or(Err(AttemptEnd::Deadline))
        }))
    }

    async fn wait_before_retry(
        &mut self,
        delay: Duration,
        shutdown: &mut watch::Receiver<bool>,
        report: &mut SnapshotReconnectReport,
        replacements_open: &mut bool,
    ) -> bool {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => true,
            replacement = self.replacements.recv(), if *replacements_open => {
                if replacement.is_some() {
                    report.replacement_requests = report.replacement_requests.saturating_add(1);
                } else {
                    *replacements_open = false;
                }
                false
            }
            () = sleep(delay) => false,
        }
    }

    async fn roll_explicit_replacement(
        &mut self,
        mut old: AttemptFuture,
        mut replacement: AttemptFuture,
        old_publication: Option<crate::snapshot_actor::SessionEpoch>,
        shutdown: &mut watch::Receiver<bool>,
        report: &mut SnapshotReconnectReport,
        replacements_open: &mut bool,
    ) -> RollOutcome {
        let mut publication_tick = interval(PUBLICATION_POLL_INTERVAL);
        loop {
            tokio::select! {
                biased;
                () = wait_for_shutdown(shutdown) => {
                    drop(replacement);
                    drop(old);
                    report.shutdown_cancellations =
                        report.shutdown_cancellations.saturating_add(2);
                    return RollOutcome::Shutdown;
                }
                outcome = &mut replacement => {
                    record_attempt_end(report, &outcome);
                    report.replacement_failures = report.replacement_failures.saturating_add(1);
                    return RollOutcome::Active(old);
                }
                outcome = &mut old => {
                    record_attempt_end(report, &outcome);
                    return RollOutcome::Active(replacement);
                }
                request = self.replacements.recv(), if *replacements_open => {
                    if request.is_some() {
                        report.replacement_requests = report.replacement_requests.saturating_add(1);
                        report.replacement_failures = report.replacement_failures.saturating_add(1);
                    } else {
                        *replacements_open = false;
                    }
                }
                _ = publication_tick.tick() => {
                    let current = self.consumer.publication().lock().published_epoch();
                    if current.is_some() && current != old_publication {
                        drop(old);
                        report.replacement_promotions =
                            report.replacement_promotions.saturating_add(1);
                        return RollOutcome::Active(replacement);
                    }
                }
            }
        }
    }
}

type AttemptFuture = Pin<Box<dyn Future<Output = Result<(), AttemptEnd>> + Send>>;

enum RollOutcome {
    Active(AttemptFuture),
    Shutdown,
}

#[derive(Debug)]
enum AttemptEnd {
    SocketPath(SnapshotSocketPathError),
    Connect,
    Consumer(SnapshotConsumerError),
    Deadline,
}

fn record_attempt_end(report: &mut SnapshotReconnectReport, outcome: &Result<(), AttemptEnd>) {
    match outcome {
        Err(AttemptEnd::SocketPath(error)) => {
            let _ = error.reason();
            report.connect_failures = report.connect_failures.saturating_add(1);
        }
        Err(AttemptEnd::Connect) => {
            report.connect_failures = report.connect_failures.saturating_add(1);
        }
        Err(AttemptEnd::Deadline) => {
            report.attempts_timed_out = report.attempts_timed_out.saturating_add(1);
        }
        Err(AttemptEnd::Consumer(error)) => {
            if matches!(error, SnapshotConsumerError::Timeout) {
                report.attempts_timed_out = report.attempts_timed_out.saturating_add(1);
            } else {
                report.session_failures = report.session_failures.saturating_add(1);
            }
        }
        Ok(()) => {
            report.session_failures = report.session_failures.saturating_add(1);
        }
    }
}

struct ChallengeLedger {
    capacity: usize,
    order: VecDeque<[u8; 32]>,
    seen: HashSet<[u8; 32]>,
}

impl ChallengeLedger {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
        }
    }

    fn generate(&mut self, random: &mut dyn RandomSource) -> Result<SnapshotSessionChallenge, ()> {
        for _ in 0..CHALLENGE_ATTEMPTS {
            let mut bytes = [0_u8; 32];
            random.fill(&mut bytes)?;
            if self.seen.insert(bytes) {
                self.order.push_back(bytes);
                if self.order.len() > self.capacity
                    && let Some(evicted) = self.order.pop_front()
                {
                    self.seen.remove(&evicted);
                }
                return Ok(SnapshotSessionChallenge::new(bytes));
            }
        }
        Err(())
    }
}

fn jittered_delay(base: Duration, random: &mut dyn RandomSource) -> Result<Duration, ()> {
    let nanos = u64::try_from(base.as_nanos()).map_err(|_| ())?;
    let lower = nanos.div_ceil(2);
    let span = nanos.saturating_sub(lower);
    let mut bytes = [0_u8; 8];
    random.fill(&mut bytes)?;
    let offset = u64::from_le_bytes(bytes) % span.saturating_add(1);
    Ok(Duration::from_nanos(lower.saturating_add(offset)))
}

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
        time::timeout,
    };

    use super::*;
    use crate::{
        block_digest::BlockDigester,
        digest_index::{DigestIndexLimits, SnapshotGroupKey},
        kv_snapshot::{
            AttentionKind, DigestAlgorithm, DigestSpec, EngineIncarnation, GroupDisposition,
            GroupMetadata, ResetScope, SnapshotBody, SnapshotCapacity, SnapshotLimits,
            encode_snapshot,
        },
        kv_wire::KvWireLimits,
        snapshot_actor::{SessionEpoch, SnapshotActorLimits},
        snapshot_consumer::{SharedSnapshotPublication, SnapshotConsumerConfig},
        snapshot_session::{
            SnapshotSessionBinding, SnapshotSessionLimits, SnapshotSessionSecret,
            decode_client_hello, encode_authenticated_snapshot, encode_client_hello,
        },
        snapshot_socket_path::{PublishedSocketPath, bind_and_publish},
        snapshot_tail_wire::{
            TailDirection, TailFrameBinding, TailFrameType, TailSessionKey, TailWireLimits,
            encode_tail_frame,
        },
    };

    const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const DIGEST_SECRET: [u8; 32] = [0x71; 32];
    const GENERATION: u64 = 7;
    static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mini-dynamo-reconnect-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("companion.sock")
        }

        fn policy(&self) -> SocketParentPolicy {
            SocketParentPolicy {
                owner_uid: fs::symlink_metadata(&self.0).unwrap().uid(),
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct CounterRandom(u8);

    impl RandomSource for CounterRandom {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), ()> {
            self.0 = self.0.wrapping_add(1);
            destination.fill(self.0);
            Ok(())
        }
    }

    struct ScriptedRandom(VecDeque<Vec<u8>>);

    impl RandomSource for ScriptedRandom {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), ()> {
            let bytes = self.0.pop_front().ok_or(())?;
            if bytes.len() != destination.len() {
                return Err(());
            }
            destination.copy_from_slice(&bytes);
            Ok(())
        }
    }

    fn incarnation() -> EngineIncarnation {
        EngineIncarnation {
            engine_id: "engine-a".into(),
            model_revision: "revision-a".into(),
            image_digest: "sha256:image-a".into(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![3; 32],
        }
    }

    fn consumer(uid: u32) -> Arc<SnapshotConsumer> {
        Arc::new(
            SnapshotConsumer::new(
                SnapshotConsumerConfig {
                    expected_peer_uid: uid,
                    expected_engine_incarnation: incarnation(),
                    minimum_snapshot_watermark: 100,
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

    fn config(directory: &TestDirectory, attempt_timeout: Duration) -> SnapshotReconnectConfig {
        SnapshotReconnectConfig::new(
            directory.socket(),
            directory.policy(),
            attempt_timeout,
            Duration::from_millis(5),
            Duration::from_millis(20),
            32,
        )
        .unwrap()
    }

    fn owner_with_counter(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
    ) -> (SnapshotReconnectOwner, SnapshotReconnectHandle) {
        SnapshotReconnectOwner::with_random(config, consumer, Box::new(CounterRandom(0))).unwrap()
    }

    fn tokio_listener(directory: &TestDirectory) -> (UnixListener, PublishedSocketPath) {
        let published = bind_and_publish(&directory.socket(), directory.policy()).unwrap();
        let (listener, guard) = published.into_parts();
        listener.set_nonblocking(true).unwrap();
        (UnixListener::from_std(listener).unwrap(), guard)
    }

    fn snapshot_response(challenge: SnapshotSessionChallenge, watermark: u64) -> Vec<u8> {
        let digester = BlockDigester::new(DIGEST_SECRET);
        let mut body = SnapshotBody {
            engine_incarnation: incarnation(),
            watermark,
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
                block_size: 64,
            }],
            records: Vec::new(),
        };
        body.refresh_capacity().unwrap();
        let snapshot = encode_snapshot(&body, SnapshotLimits::default()).unwrap();
        encode_authenticated_snapshot(
            &snapshot,
            SnapshotSessionBinding {
                challenge,
                engine_incarnation: &body.engine_incarnation,
                snapshot_watermark: watermark,
                digest_key_id: digester.key_id().as_bytes(),
                companion_generation: GENERATION,
            },
            &SnapshotSessionSecret::new(SESSION_SECRET),
            SnapshotSessionLimits::default(),
        )
        .unwrap()
    }

    async fn accept_and_publish(
        listener: &UnixListener,
        watermark: u64,
    ) -> (UnixStream, SnapshotSessionChallenge) {
        let (mut stream, _) = listener.accept().await.unwrap();
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
        let challenge =
            decode_client_hello(&hello, &secret, SnapshotSessionLimits::default()).unwrap();
        stream
            .write_all(&snapshot_response(challenge, watermark))
            .await
            .unwrap();
        let key = TailSessionKey::derive(
            &secret,
            challenge,
            GENERATION,
            TailDirection::CompanionToRouter,
        );
        let digester = BlockDigester::new(DIGEST_SECRET);
        let caught_up = encode_tail_frame(
            b"",
            TailFrameBinding {
                frame_type: TailFrameType::CaughtUp,
                direction: TailDirection::CompanionToRouter,
                session_id: challenge,
                message_sequence: 1,
                delivery_sequence: 0,
                event_watermark: watermark,
                engine_incarnation: &incarnation(),
                digest_key_id: digester.key_id().as_bytes(),
                companion_generation: GENERATION,
            },
            &key,
            TailWireLimits::default(),
        )
        .unwrap();
        stream.write_all(&caught_up).await.unwrap();
        (stream, challenge)
    }

    async fn wait_for_epoch(
        publication: &SharedSnapshotPublication,
        different_from: Option<SessionEpoch>,
    ) -> SessionEpoch {
        timeout(Duration::from_secs(1), async {
            loop {
                if let Some(epoch) = publication.lock().published_epoch()
                    && Some(epoch) != different_from
                {
                    return epoch;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[test]
    fn challenge_ledger_retries_collisions_and_bounds_history() {
        let mut random = ScriptedRandom(VecDeque::from([
            vec![1; 32],
            vec![1; 32],
            vec![2; 32],
            vec![3; 32],
            vec![1; 32],
        ]));
        let mut ledger = ChallengeLedger::new(2);
        let first = ledger.generate(&mut random).unwrap();
        let second = ledger.generate(&mut random).unwrap();
        let third = ledger.generate(&mut random).unwrap();
        let after_eviction = ledger.generate(&mut random).unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_eq!(first, after_eviction);
        assert_eq!(ledger.order.len(), 2);
    }

    #[test]
    fn jitter_and_exponential_backoff_are_bounded() {
        let base = Duration::from_millis(100);
        let mut low = ScriptedRandom(VecDeque::from([vec![0; 8]]));
        let mut high = ScriptedRandom(VecDeque::from([vec![u8::MAX; 8]]));
        assert_eq!(jittered_delay(base, &mut low).unwrap(), base / 2);
        let high = jittered_delay(base, &mut high).unwrap();
        assert!(high >= base / 2 && high <= base);
        assert_eq!(
            next_backoff(base, Duration::from_millis(150)),
            Duration::from_millis(150)
        );
    }

    #[tokio::test]
    async fn connect_failures_back_off_then_recover_with_fresh_challenge() {
        let directory = TestDirectory::new();
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let (owner, _) = owner_with_counter(config(&directory, Duration::from_secs(1)), consumer);
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        sleep(Duration::from_millis(25)).await;
        let (listener, _guard) = tokio_listener(&directory);
        let (_stream, challenge) = accept_and_publish(&listener, 100).await;
        wait_for_epoch(&publication, None).await;
        assert_ne!(challenge.as_bytes(), &[0; 32]);
        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        assert!(report.connect_failures >= 1);
        assert!(report.attempts_started >= 2);
        assert_eq!(report.shutdown_cancellations, 1);
        assert!(publication.lock().published_index().is_none());
    }

    #[tokio::test]
    async fn one_deadline_bounds_stalled_connected_session() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let (owner, _) =
            owner_with_counter(config(&directory, Duration::from_millis(20)), consumer);
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));
        let (stream, _) = listener.accept().await.unwrap();
        sleep(Duration::from_millis(35)).await;
        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(stream);
        assert!(report.attempts_timed_out >= 1);
        assert!(report.attempts_started >= 1);
    }

    #[tokio::test]
    async fn shutdown_cancels_stalled_attempt_without_waiting_for_deadline() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let (owner, _) = owner_with_counter(config(&directory, Duration::from_secs(30)), consumer);
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));
        let (_stream, _) = listener.accept().await.unwrap();
        let started = Instant::now();
        shutdown_sender.send(true).unwrap();
        let report = timeout(Duration::from_millis(100), owner_task)
            .await
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(report.shutdown_cancellations, 1);
    }

    #[tokio::test]
    async fn reconnect_uses_distinct_challenges_and_republishes() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let (owner, _) = owner_with_counter(config(&directory, Duration::from_secs(1)), consumer);
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (first_stream, first_challenge) = accept_and_publish(&listener, 100).await;
        let first_epoch = wait_for_epoch(&publication, None).await;
        drop(first_stream);
        let (second_stream, second_challenge) = accept_and_publish(&listener, 200).await;
        let _second_epoch = wait_for_epoch(&publication, Some(first_epoch)).await;
        assert_ne!(first_challenge, second_challenge);

        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(second_stream);
        assert!(report.attempts_started >= 2);
        assert!(report.session_failures >= 1);
    }

    #[tokio::test]
    async fn explicit_replacement_hands_off_before_old_session_is_dropped() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let (owner, handle) =
            owner_with_counter(config(&directory, Duration::from_secs(1)), consumer);
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (mut old_stream, _) = accept_and_publish(&listener, 100).await;
        let old_epoch = wait_for_epoch(&publication, None).await;
        handle.request_replacement().unwrap();
        let (replacement_stream, _) = accept_and_publish(&listener, 200).await;
        let new_epoch = wait_for_epoch(&publication, Some(old_epoch)).await;
        assert_ne!(old_epoch, new_epoch);

        let mut eof = [0_u8; 1];
        timeout(Duration::from_secs(1), old_stream.read(&mut eof))
            .await
            .unwrap()
            .unwrap();
        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(replacement_stream);
        assert_eq!(report.replacement_promotions, 1);
        assert!(publication.lock().published_index().is_none());
    }
}
