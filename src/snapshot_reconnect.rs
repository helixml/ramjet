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
    companion_attestation::EngineIncarnationAuthority,
    kv_snapshot::EngineIncarnation,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotReconnectAttemptKind {
    Initial,
    Retry,
    Replacement,
}

impl SnapshotReconnectAttemptKind {
    pub const ALL: [Self; 3] = [Self::Initial, Self::Retry, Self::Replacement];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Retry => "retry",
            Self::Replacement => "replacement",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotReconnectAttemptResult {
    ConnectFailure,
    SessionFailure,
    Timeout,
    RandomFailure,
    Cancelled,
    UnexpectedEnd,
}

impl SnapshotReconnectAttemptResult {
    pub const ALL: [Self; 6] = [
        Self::ConnectFailure,
        Self::SessionFailure,
        Self::Timeout,
        Self::RandomFailure,
        Self::Cancelled,
        Self::UnexpectedEnd,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ConnectFailure => "connect_failure",
            Self::SessionFailure => "session_failure",
            Self::Timeout => "timeout",
            Self::RandomFailure => "random_failure",
            Self::Cancelled => "cancelled",
            Self::UnexpectedEnd => "unexpected_end",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotReconnectEvent {
    AttemptStarted(SnapshotReconnectAttemptKind),
    AttemptSetupFailed(SnapshotReconnectAttemptResult),
    ConnectionOpened,
    ConnectionClosed,
    AttemptFinished(SnapshotReconnectAttemptResult),
    Readiness(bool),
}

/// Typed, content-free observation boundary for one reconnect owner.
///
/// Implementations must keep work synchronous and bounded. Event variants and
/// their labels are closed enums: paths, peer identities, hosts, secrets, and
/// protocol error strings never cross this boundary.
pub trait SnapshotReconnectObserver: Send + Sync {
    fn observe(&self, event: SnapshotReconnectEvent);
}

struct NoopReconnectObserver;

impl SnapshotReconnectObserver for NoopReconnectObserver {
    fn observe(&self, _event: SnapshotReconnectEvent) {}
}

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
    pub authority_unchanged: u64,
    pub authority_losses: u64,
    pub authority_rotations: u64,
    pub authority_recoveries: u64,
    pub authority_cancellations: u64,
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
    observer: Arc<dyn SnapshotReconnectObserver>,
    expected_incarnation: Option<EngineIncarnation>,
    authority_revision: Option<u64>,
    authority: Option<watch::Receiver<EngineIncarnationAuthority>>,
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
        Self::with_observer(config, consumer, Arc::new(NoopReconnectObserver))
    }

    /// Construct the production owner with a typed, content-free observer.
    ///
    /// # Errors
    ///
    /// Revalidates the configured socket path before retaining it.
    pub fn with_observer(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        observer: Arc<dyn SnapshotReconnectObserver>,
    ) -> Result<(Self, SnapshotReconnectHandle), SnapshotReconnectError> {
        Self::with_random(config, consumer, Box::new(OsRandomSource), observer)
    }

    /// Construct an owner whose exact authority may be revoked or rotated at
    /// runtime. `None` suppresses connection attempts until authenticated
    /// authority is restored; a changed `Some` value revokes the old session
    /// before a fresh attempt is started.
    ///
    /// # Errors
    ///
    /// Revalidates the configured socket path before retaining it.
    pub fn new_with_authority(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        authority: watch::Receiver<EngineIncarnationAuthority>,
    ) -> Result<(Self, SnapshotReconnectHandle), SnapshotReconnectError> {
        Self::with_observer_and_authority(
            config,
            consumer,
            Arc::new(NoopReconnectObserver),
            authority,
        )
    }

    /// Construct a hot-authority owner with typed, content-free observation.
    ///
    /// # Errors
    ///
    /// Revalidates the configured socket path before retaining it.
    pub fn with_observer_and_authority(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        observer: Arc<dyn SnapshotReconnectObserver>,
        mut authority: watch::Receiver<EngineIncarnationAuthority>,
    ) -> Result<(Self, SnapshotReconnectHandle), SnapshotReconnectError> {
        let initial = authority.borrow_and_update().clone();
        Self::with_random_and_authority(
            config,
            consumer,
            Box::new(OsRandomSource),
            observer,
            initial.incarnation().cloned(),
            Some(initial.revision()),
            Some(authority),
        )
    }

    fn with_random(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        random: Box<dyn RandomSource>,
        observer: Arc<dyn SnapshotReconnectObserver>,
    ) -> Result<(Self, SnapshotReconnectHandle), SnapshotReconnectError> {
        let expected_incarnation = Some(consumer.expected_engine_incarnation().clone());
        Self::with_random_and_authority(
            config,
            consumer,
            random,
            observer,
            expected_incarnation,
            None,
            None,
        )
    }

    fn with_random_and_authority(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        random: Box<dyn RandomSource>,
        observer: Arc<dyn SnapshotReconnectObserver>,
        expected_incarnation: Option<EngineIncarnation>,
        authority_revision: Option<u64>,
        authority: Option<watch::Receiver<EngineIncarnationAuthority>>,
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
                observer,
                expected_incarnation,
                authority_revision,
                authority,
            },
            SnapshotReconnectHandle { replacement },
        ))
    }

    /// Reconnect until shutdown. Failures affect only exact publication state;
    /// approximate serving is not owned or mutated here.
    #[allow(clippy::too_many_lines)]
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> SnapshotReconnectReport {
        let mut report = SnapshotReconnectReport::default();
        let mut backoff = self.config.reconnect_min;
        let mut delay_before_attempt = false;
        let mut first_attempt = true;
        let mut replacements_open = true;
        self.observe_readiness();

        'owner: loop {
            if self.expected_incarnation.is_none() {
                tokio::select! {
                    biased;
                    () = wait_for_shutdown(&mut shutdown) => return report,
                    event = next_authority(&mut self.authority) => {
                        self.apply_authority_event(event, &mut report);
                        delay_before_attempt = false;
                    }
                    replacement = self.replacements.recv(), if replacements_open => {
                        if replacement.is_some() {
                            report.replacement_requests =
                                report.replacement_requests.saturating_add(1);
                            report.replacement_failures =
                                report.replacement_failures.saturating_add(1);
                        } else {
                            replacements_open = false;
                        }
                    }
                }
                continue;
            }
            if delay_before_attempt {
                let delay = jittered_delay(backoff, self.random.as_mut()).unwrap_or_else(|()| {
                    report.random_failures = report.random_failures.saturating_add(1);
                    backoff
                });
                backoff = next_backoff(backoff, self.config.reconnect_max);
                match self
                    .wait_before_retry(delay, &mut shutdown, &mut report, &mut replacements_open)
                    .await
                {
                    RetryWait::Ready => {}
                    RetryWait::Shutdown => return report,
                    RetryWait::Authority(event) => {
                        self.apply_authority_event(event, &mut report);
                        delay_before_attempt = false;
                        continue;
                    }
                }
            }
            delay_before_attempt = true;

            let kind = if first_attempt {
                first_attempt = false;
                SnapshotReconnectAttemptKind::Initial
            } else {
                SnapshotReconnectAttemptKind::Retry
            };
            let Some(expected) = self.expected_incarnation.clone() else {
                delay_before_attempt = false;
                continue;
            };
            let Some(mut active) = self.start_attempt(expected, &mut report, kind) else {
                continue;
            };
            let mut published = self.consumer.publication().lock().published_epoch();
            let mut publication_tick = interval(PUBLICATION_POLL_INTERVAL);

            loop {
                tokio::select! {
                    biased;
                    () = wait_for_shutdown(&mut shutdown) => {
                        drop(active);
                        self.observe_readiness();
                        report.shutdown_cancellations =
                            report.shutdown_cancellations.saturating_add(1);
                        return report;
                    }
                    event = next_authority(&mut self.authority) => {
                        if self.apply_authority_event(event, &mut report) {
                            drop(active);
                            report.authority_cancellations =
                                report.authority_cancellations.saturating_add(1);
                            delay_before_attempt = false;
                            continue 'owner;
                        }
                    }
                    replacement = self.replacements.recv(), if replacements_open => {
                        let Some(()) = replacement else {
                            replacements_open = false;
                            continue;
                        };
                        report.replacement_requests = report.replacement_requests.saturating_add(1);
                        let Some(expected) = self.expected_incarnation.clone() else {
                            report.replacement_failures =
                                report.replacement_failures.saturating_add(1);
                            continue;
                        };
                        let Some(replacement) = self.start_attempt(
                            expected,
                            &mut report,
                            SnapshotReconnectAttemptKind::Replacement,
                        ) else {
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
                                self.observe_readiness();
                            }
                            RollOutcome::Shutdown => return report,
                            RollOutcome::AuthorityChanged => {
                                report.authority_cancellations =
                                    report.authority_cancellations.saturating_add(2);
                                delay_before_attempt = false;
                                continue 'owner;
                            }
                        }
                    }
                    outcome = &mut active => {
                        record_attempt_end(&mut report, &outcome);
                        self.observe_readiness();
                        break;
                    }
                    _ = publication_tick.tick() => {
                        let current = self.consumer.publication().lock().published_epoch();
                        if current != published {
                            published = current;
                            if current.is_some() {
                                backoff = self.config.reconnect_min;
                            }
                            self.observer
                                .observe(SnapshotReconnectEvent::Readiness(current.is_some()));
                        }
                    }
                }
            }
        }
    }

    fn start_attempt(
        &mut self,
        expected_incarnation: EngineIncarnation,
        report: &mut SnapshotReconnectReport,
        kind: SnapshotReconnectAttemptKind,
    ) -> Option<AttemptFuture> {
        let Some(deadline) = Instant::now().checked_add(self.config.attempt_timeout) else {
            report.session_failures = report.session_failures.saturating_add(1);
            self.observer
                .observe(SnapshotReconnectEvent::AttemptSetupFailed(
                    SnapshotReconnectAttemptResult::SessionFailure,
                ));
            return None;
        };
        let Ok(challenge) = self.challenges.generate(self.random.as_mut()) else {
            report.random_failures = report.random_failures.saturating_add(1);
            self.observer
                .observe(SnapshotReconnectEvent::AttemptSetupFailed(
                    SnapshotReconnectAttemptResult::RandomFailure,
                ));
            return None;
        };
        report.attempts_started = report.attempts_started.saturating_add(1);
        let path = self.config.socket_path.clone();
        let policy = self.config.socket_policy;
        let consumer = Arc::clone(&self.consumer);
        let observer = Arc::clone(&self.observer);
        Some(Box::pin(async move {
            let mut observation = AttemptObservation::new(observer, kind);
            let attempt = async {
                validate_socket_client_path(&path, policy).map_err(AttemptEnd::SocketPath)?;
                let stream = UnixStream::connect(&path)
                    .await
                    .map_err(|_| AttemptEnd::Connect)?;
                observation.connected();
                consumer
                    .consume_with_expected_incarnation(
                        stream,
                        challenge,
                        deadline,
                        expected_incarnation,
                    )
                    .await
                    .map_err(AttemptEnd::Consumer)
            };
            let outcome = timeout_at(deadline, attempt)
                .await
                .unwrap_or(Err(AttemptEnd::Deadline));
            observation.finish(attempt_result(&outcome));
            outcome
        }))
    }

    fn observe_readiness(&self) {
        let ready = self
            .consumer
            .publication()
            .lock()
            .published_epoch()
            .is_some();
        self.observer
            .observe(SnapshotReconnectEvent::Readiness(ready));
    }

    async fn wait_before_retry(
        &mut self,
        delay: Duration,
        shutdown: &mut watch::Receiver<bool>,
        report: &mut SnapshotReconnectReport,
        replacements_open: &mut bool,
    ) -> RetryWait {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => RetryWait::Shutdown,
            event = next_authority(&mut self.authority) => RetryWait::Authority(event),
            replacement = self.replacements.recv(), if *replacements_open => {
                if replacement.is_some() {
                    report.replacement_requests = report.replacement_requests.saturating_add(1);
                } else {
                    *replacements_open = false;
                }
                RetryWait::Ready
            }
            () = sleep(delay) => RetryWait::Ready,
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
                    self.observe_readiness();
                    report.shutdown_cancellations =
                        report.shutdown_cancellations.saturating_add(2);
                    return RollOutcome::Shutdown;
                }
                event = next_authority(&mut self.authority) => {
                    if self.apply_authority_event(event, report) {
                        drop(replacement);
                        drop(old);
                        return RollOutcome::AuthorityChanged;
                    }
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
                        self.observe_readiness();
                        report.replacement_promotions =
                            report.replacement_promotions.saturating_add(1);
                        return RollOutcome::Active(replacement);
                    }
                }
            }
        }
    }

    fn apply_authority_event(
        &mut self,
        event: AuthorityEvent,
        report: &mut SnapshotReconnectReport,
    ) -> bool {
        let next = match event {
            AuthorityEvent::Updated(next) => {
                if self.authority_revision == Some(next.revision()) {
                    report.authority_unchanged = report.authority_unchanged.saturating_add(1);
                    return false;
                }
                self.authority_revision = Some(next.revision());
                next.incarnation().cloned()
            }
            AuthorityEvent::Closed => {
                self.authority = None;
                self.authority_revision = None;
                None
            }
        };
        if self.authority.is_none() && self.expected_incarnation == next {
            report.authority_unchanged = report.authority_unchanged.saturating_add(1);
            return false;
        }
        match (&self.expected_incarnation, &next) {
            (Some(_), Some(_)) => {
                report.authority_rotations = report.authority_rotations.saturating_add(1);
            }
            (Some(_), None) => {
                report.authority_losses = report.authority_losses.saturating_add(1);
            }
            (None, Some(_)) => {
                report.authority_recoveries = report.authority_recoveries.saturating_add(1);
            }
            // A watch receiver can observe only the final value of a rapid
            // `unavailable -> valid -> unavailable` sequence. The revision
            // still advances, but there is no active authority or session to
            // revoke and reconnect. Remain fail closed and wait for a later
            // valid authority instead of terminating the reconnect owner.
            (None, None) => {
                report.authority_unchanged = report.authority_unchanged.saturating_add(1);
                return false;
            }
        }
        self.consumer.publication().lock().revoke_authority();
        self.observer
            .observe(SnapshotReconnectEvent::Readiness(false));
        self.expected_incarnation = next;
        true
    }
}

type AttemptFuture = Pin<Box<dyn Future<Output = Result<(), AttemptEnd>> + Send>>;

struct AttemptObservation {
    observer: Arc<dyn SnapshotReconnectObserver>,
    connected: bool,
    finished: bool,
}

impl AttemptObservation {
    fn new(
        observer: Arc<dyn SnapshotReconnectObserver>,
        kind: SnapshotReconnectAttemptKind,
    ) -> Self {
        observer.observe(SnapshotReconnectEvent::AttemptStarted(kind));
        Self {
            observer,
            connected: false,
            finished: false,
        }
    }

    fn connected(&mut self) {
        debug_assert!(!self.connected);
        self.connected = true;
        self.observer
            .observe(SnapshotReconnectEvent::ConnectionOpened);
    }

    fn finish(&mut self, result: SnapshotReconnectAttemptResult) {
        self.close(result);
    }

    fn close(&mut self, result: SnapshotReconnectAttemptResult) {
        if self.finished {
            return;
        }
        if self.connected {
            self.observer
                .observe(SnapshotReconnectEvent::ConnectionClosed);
        }
        self.observer
            .observe(SnapshotReconnectEvent::AttemptFinished(result));
        self.finished = true;
    }
}

impl Drop for AttemptObservation {
    fn drop(&mut self) {
        self.close(SnapshotReconnectAttemptResult::Cancelled);
    }
}

enum RollOutcome {
    Active(AttemptFuture),
    Shutdown,
    AuthorityChanged,
}

enum RetryWait {
    Ready,
    Shutdown,
    Authority(AuthorityEvent),
}

enum AuthorityEvent {
    Updated(EngineIncarnationAuthority),
    Closed,
}

async fn next_authority(
    authority: &mut Option<watch::Receiver<EngineIncarnationAuthority>>,
) -> AuthorityEvent {
    let Some(authority) = authority else {
        return std::future::pending().await;
    };
    if authority.changed().await.is_err() {
        AuthorityEvent::Closed
    } else {
        AuthorityEvent::Updated(authority.borrow_and_update().clone())
    }
}

#[derive(Debug)]
enum AttemptEnd {
    SocketPath(SnapshotSocketPathError),
    Connect,
    Consumer(SnapshotConsumerError),
    Deadline,
}

const fn attempt_result(outcome: &Result<(), AttemptEnd>) -> SnapshotReconnectAttemptResult {
    match outcome {
        Err(AttemptEnd::SocketPath(_) | AttemptEnd::Connect) => {
            SnapshotReconnectAttemptResult::ConnectFailure
        }
        Err(AttemptEnd::Deadline) => SnapshotReconnectAttemptResult::Timeout,
        Err(AttemptEnd::Consumer(SnapshotConsumerError::Timeout)) => {
            SnapshotReconnectAttemptResult::Timeout
        }
        Err(AttemptEnd::Consumer(_)) => SnapshotReconnectAttemptResult::SessionFailure,
        Ok(()) => SnapshotReconnectAttemptResult::UnexpectedEnd,
    }
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
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicU64, Ordering},
        },
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
                "ramjet-reconnect-{}-{sequence}",
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

    #[derive(Default)]
    struct RecordingObserver(StdMutex<Vec<SnapshotReconnectEvent>>);

    impl RecordingObserver {
        fn events(&self) -> Vec<SnapshotReconnectEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl SnapshotReconnectObserver for RecordingObserver {
        fn observe(&self, event: SnapshotReconnectEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

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
        SnapshotReconnectOwner::with_random(
            config,
            consumer,
            Box::new(CounterRandom(0)),
            Arc::new(NoopReconnectObserver),
        )
        .unwrap()
    }

    fn owner_with_authority(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        authority: watch::Receiver<EngineIncarnationAuthority>,
    ) -> (SnapshotReconnectOwner, SnapshotReconnectHandle) {
        owner_with_authority_observer(config, consumer, authority, Arc::new(NoopReconnectObserver))
    }

    fn owner_with_authority_observer(
        config: SnapshotReconnectConfig,
        consumer: Arc<SnapshotConsumer>,
        mut authority: watch::Receiver<EngineIncarnationAuthority>,
        observer: Arc<dyn SnapshotReconnectObserver>,
    ) -> (SnapshotReconnectOwner, SnapshotReconnectHandle) {
        let initial = authority.borrow_and_update().clone();
        SnapshotReconnectOwner::with_random_and_authority(
            config,
            consumer,
            Box::new(CounterRandom(0)),
            observer,
            initial.incarnation().cloned(),
            Some(initial.revision()),
            Some(authority),
        )
        .unwrap()
    }

    fn tokio_listener(directory: &TestDirectory) -> (UnixListener, PublishedSocketPath) {
        let published = bind_and_publish(&directory.socket(), directory.policy()).unwrap();
        let (listener, guard) = published.into_parts();
        listener.set_nonblocking(true).unwrap();
        (UnixListener::from_std(listener).unwrap(), guard)
    }

    fn snapshot_response_for(
        challenge: SnapshotSessionChallenge,
        watermark: u64,
        engine_incarnation: &EngineIncarnation,
    ) -> Vec<u8> {
        let digester = BlockDigester::new(DIGEST_SECRET);
        let mut body = SnapshotBody {
            engine_incarnation: engine_incarnation.clone(),
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
        accept_and_publish_for(listener, watermark, &incarnation()).await
    }

    async fn accept_and_publish_for(
        listener: &UnixListener,
        watermark: u64,
        engine_incarnation: &EngineIncarnation,
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
            .write_all(&snapshot_response_for(
                challenge,
                watermark,
                engine_incarnation,
            ))
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
                engine_incarnation,
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

    async fn wait_for_readiness_count(
        observer: &RecordingObserver,
        expected: bool,
        minimum: usize,
    ) {
        timeout(Duration::from_secs(1), async {
            loop {
                let count = observer
                    .events()
                    .into_iter()
                    .filter(|event| *event == SnapshotReconnectEvent::Readiness(expected))
                    .count();
                if count >= minimum {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
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
    async fn observer_balances_connected_timeout_and_keeps_labels_typed() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let observer = Arc::new(RecordingObserver::default());
        let (owner, _) = SnapshotReconnectOwner::with_random(
            config(&directory, Duration::from_millis(20)),
            consumer,
            Box::new(CounterRandom(0)),
            observer.clone(),
        )
        .unwrap();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));
        let (stream, _) = listener.accept().await.unwrap();
        sleep(Duration::from_millis(35)).await;
        shutdown_sender.send(true).unwrap();
        owner_task.await.unwrap();
        drop(stream);

        let events = observer.events();
        assert!(events.contains(&SnapshotReconnectEvent::AttemptStarted(
            SnapshotReconnectAttemptKind::Initial
        )));
        assert!(events.contains(&SnapshotReconnectEvent::ConnectionOpened));
        assert!(events.contains(&SnapshotReconnectEvent::ConnectionClosed));
        assert!(events.contains(&SnapshotReconnectEvent::AttemptFinished(
            SnapshotReconnectAttemptResult::Timeout
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SnapshotReconnectEvent::ConnectionOpened))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event, SnapshotReconnectEvent::ConnectionClosed))
                .count()
        );
        assert_eq!(
            events.last(),
            Some(&SnapshotReconnectEvent::Readiness(false))
        );
    }

    #[tokio::test]
    async fn observer_readiness_tracks_publication_not_connection() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let observer = Arc::new(RecordingObserver::default());
        let (owner, _) = SnapshotReconnectOwner::with_random(
            config(&directory, Duration::from_secs(1)),
            consumer,
            Box::new(CounterRandom(0)),
            observer.clone(),
        )
        .unwrap();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (stream, _) = accept_and_publish(&listener, 100).await;
        wait_for_epoch(&publication, None).await;
        timeout(Duration::from_secs(1), async {
            loop {
                if observer
                    .events()
                    .contains(&SnapshotReconnectEvent::Readiness(true))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown_sender.send(true).unwrap();
        owner_task.await.unwrap();
        drop(stream);

        let events = observer.events();
        let ready_position = events
            .iter()
            .position(|event| *event == SnapshotReconnectEvent::Readiness(true))
            .unwrap();
        let connected_position = events
            .iter()
            .position(|event| *event == SnapshotReconnectEvent::ConnectionOpened)
            .unwrap();
        assert!(connected_position < ready_position);
        assert_eq!(
            events.last(),
            Some(&SnapshotReconnectEvent::Readiness(false))
        );
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

    #[tokio::test]
    async fn identical_authority_refresh_does_not_churn_active_session() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let current = incarnation();
        let initial = EngineIncarnationAuthority::new(1, Some(current.clone()));
        let (authority_tx, authority_rx) = watch::channel(initial.clone());
        let (owner, _) = owner_with_authority(
            config(&directory, Duration::from_secs(1)),
            consumer,
            authority_rx,
        );
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (active_stream, _) = accept_and_publish_for(&listener, 100, &current).await;
        let epoch = wait_for_epoch(&publication, None).await;
        authority_tx.send(initial).unwrap();
        sleep(Duration::from_millis(20)).await;
        assert_eq!(publication.lock().published_epoch(), Some(epoch));
        assert!(
            timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );

        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(active_stream);
        assert_eq!(report.attempts_started, 1);
        assert_eq!(report.authority_unchanged, 1);
        assert_eq!(report.authority_rotations, 0);
        assert_eq!(report.authority_cancellations, 0);
    }

    #[tokio::test]
    async fn authority_rotation_fences_stale_session_and_republishes_fresh_identity() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let old = incarnation();
        let mut rotated = old.clone();
        rotated.process_started_unix_ns += 1;
        let (authority_tx, authority_rx) =
            watch::channel(EngineIncarnationAuthority::new(1, Some(old.clone())));
        let observer = Arc::new(RecordingObserver::default());
        let (owner, _) = owner_with_authority_observer(
            config(&directory, Duration::from_secs(1)),
            consumer,
            authority_rx,
            observer.clone(),
        );
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (mut stale_stream, stale_challenge) =
            accept_and_publish_for(&listener, 100, &old).await;
        let stale_epoch = wait_for_epoch(&publication, None).await;
        wait_for_readiness_count(&observer, true, 1).await;
        authority_tx
            .send(EngineIncarnationAuthority::new(2, Some(rotated.clone())))
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while publication.lock().published_epoch().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        wait_for_readiness_count(&observer, false, 2).await;
        let mut eof = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), stale_stream.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        let (fresh_stream, fresh_challenge) =
            accept_and_publish_for(&listener, 200, &rotated).await;
        let fresh_epoch = wait_for_epoch(&publication, None).await;
        wait_for_readiness_count(&observer, true, 2).await;
        assert_ne!(stale_epoch, fresh_epoch);
        assert_ne!(stale_challenge, fresh_challenge);

        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(fresh_stream);
        assert_eq!(report.authority_rotations, 1);
        assert_eq!(report.authority_cancellations, 1);
        assert_eq!(report.attempts_started, 2);
        let readiness: Vec<_> = observer
            .events()
            .into_iter()
            .filter_map(|event| match event {
                SnapshotReconnectEvent::Readiness(ready) => Some(ready),
                _ => None,
            })
            .collect();
        let first_ready = readiness.iter().position(|ready| *ready).unwrap();
        let fenced = readiness[first_ready + 1..]
            .iter()
            .position(|ready| !*ready)
            .map(|index| first_ready + 1 + index)
            .unwrap();
        assert!(readiness[fenced + 1..].iter().any(|ready| *ready));
    }

    #[tokio::test]
    async fn coalesced_authority_gap_reconnects_even_when_identity_matches() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let current = incarnation();
        let (authority_tx, authority_rx) =
            watch::channel(EngineIncarnationAuthority::new(1, Some(current.clone())));
        let (owner, _) = owner_with_authority(
            config(&directory, Duration::from_secs(1)),
            consumer,
            authority_rx,
        );
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (mut stale_stream, stale_challenge) =
            accept_and_publish_for(&listener, 100, &current).await;
        wait_for_epoch(&publication, None).await;
        // Revision three represents a coalesced loss/recovery pair. The value
        // alone matches, but the old session crossed an untrusted interval.
        authority_tx
            .send(EngineIncarnationAuthority::new(3, Some(current.clone())))
            .unwrap();
        let mut eof = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), stale_stream.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(publication.lock().published_index().is_none());

        let (fresh_stream, fresh_challenge) =
            accept_and_publish_for(&listener, 200, &current).await;
        wait_for_epoch(&publication, None).await;
        assert_ne!(stale_challenge, fresh_challenge);

        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(fresh_stream);
        assert_eq!(report.authority_rotations, 1);
        assert_eq!(report.authority_cancellations, 1);
        assert_eq!(report.attempts_started, 2);
    }

    #[tokio::test]
    async fn authority_loss_stops_attempts_until_valid_recovery() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let current = incarnation();
        let mut recovered = current.clone();
        recovered.process_started_unix_ns += 1;
        let (authority_tx, authority_rx) =
            watch::channel(EngineIncarnationAuthority::new(1, Some(current.clone())));
        let (owner, _) = owner_with_authority(
            config(&directory, Duration::from_secs(1)),
            consumer,
            authority_rx,
        );
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        let (mut stale_stream, _) = accept_and_publish_for(&listener, 100, &current).await;
        wait_for_epoch(&publication, None).await;
        authority_tx
            .send(EngineIncarnationAuthority::new(2, None))
            .unwrap();
        let mut eof = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), stale_stream.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(publication.lock().published_index().is_none());
        assert!(
            timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );

        authority_tx
            .send(EngineIncarnationAuthority::new(3, Some(recovered.clone())))
            .unwrap();
        let (fresh_stream, _) = accept_and_publish_for(&listener, 200, &recovered).await;
        wait_for_epoch(&publication, None).await;

        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(fresh_stream);
        assert_eq!(report.authority_losses, 1);
        assert_eq!(report.authority_recoveries, 1);
        assert_eq!(report.authority_cancellations, 1);
        assert_eq!(report.attempts_started, 2);
    }

    #[tokio::test]
    async fn coalesced_recovery_gap_while_unavailable_remains_recoverable() {
        let directory = TestDirectory::new();
        let (listener, _guard) = tokio_listener(&directory);
        let consumer = consumer(directory.policy().owner_uid);
        let publication = Arc::clone(consumer.publication());
        let recovered = incarnation();
        let (authority_tx, authority_rx) = watch::channel(EngineIncarnationAuthority::new(1, None));
        let (owner, _) = owner_with_authority(
            config(&directory, Duration::from_secs(1)),
            consumer,
            authority_rx,
        );
        let (shutdown_sender, shutdown) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(shutdown));

        // Revision three represents a coalesced `unavailable -> valid ->
        // unavailable` sequence. There is no stale session to cancel, but the
        // owner must stay alive so a later valid authority can recover.
        authority_tx
            .send(EngineIncarnationAuthority::new(3, None))
            .unwrap();
        sleep(Duration::from_millis(20)).await;
        assert!(
            timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );

        authority_tx
            .send(EngineIncarnationAuthority::new(4, Some(recovered.clone())))
            .unwrap();
        let (fresh_stream, _) = accept_and_publish_for(&listener, 200, &recovered).await;
        wait_for_epoch(&publication, None).await;

        shutdown_sender.send(true).unwrap();
        let report = owner_task.await.unwrap();
        drop(fresh_stream);
        assert_eq!(report.authority_unchanged, 1);
        assert_eq!(report.authority_recoveries, 1);
        assert_eq!(report.authority_cancellations, 0);
        assert_eq!(report.attempts_started, 1);
    }
}
