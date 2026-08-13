//! Process-level ownership of one engine's live KV-event authority.
//!
//! The owner is deliberately separate from the snapshot server. It subscribes
//! to live delivery before asking the engine for replay, qualifies replay with
//! [`KvEventFence`], and only then feeds the long-lived
//! [`CompanionIndexSource`]. Any transport, replay, application, or explicit
//! engine-authority failure fences the source before reconnecting.

use std::{sync::Arc, time::Duration};

use futures::future::BoxFuture;
use thiserror::Error;
use tokio::{sync::watch, time::sleep};

use crate::{
    companion_index_source::{CompanionIndexSource, CompanionIndexSourceError},
    kv_fence::{IngestAction, KvEventFence, ReplayAction},
    kv_snapshot::EngineIncarnation,
    kv_transport::{
        KvTransportConfig, KvTransportError, LiveActivity, ReplayProfile, SequencedBatch,
        ZmqKvEventSource,
    },
};

/// A connection created with its live subscription already installed.
///
/// The boxed async seam keeps transport tests deterministic without weakening
/// the production adapter's ZMQ framing and bounded replay validation.
pub trait CompanionKvEventTransport: Send {
    fn recv_live_activity(&mut self) -> BoxFuture<'_, Result<LiveActivity, KvTransportError>>;

    fn replay(
        &mut self,
        from: u64,
        through: u64,
    ) -> BoxFuture<'_, Result<Vec<SequencedBatch>, KvTransportError>>;

    /// Stream a full replay directly into the source's private rebuild stage.
    /// Implementations retain only sparse sequence metadata, never the raw
    /// decoded replay corpus.
    fn replay_full(
        &mut self,
        through: u64,
        source: Arc<CompanionIndexSource>,
    ) -> BoxFuture<'_, Result<CompanionFullReplay, KvTransportError>>;

    fn take_replay_profile(&mut self) -> Option<ReplayProfile>;
}

impl CompanionKvEventTransport for ZmqKvEventSource {
    fn recv_live_activity(&mut self) -> BoxFuture<'_, Result<LiveActivity, KvTransportError>> {
        Box::pin(ZmqKvEventSource::recv_live_activity(self))
    }

    fn replay(
        &mut self,
        from: u64,
        through: u64,
    ) -> BoxFuture<'_, Result<Vec<SequencedBatch>, KvTransportError>> {
        Box::pin(ZmqKvEventSource::replay(self, from, through))
    }

    fn replay_full(
        &mut self,
        through: u64,
        source: Arc<CompanionIndexSource>,
    ) -> BoxFuture<'_, Result<CompanionFullReplay, KvTransportError>> {
        Box::pin(async move {
            ZmqKvEventSource::replay_fold(
                self,
                0,
                through,
                CompanionFullReplay::new(source),
                |replay, batch| replay.apply(&batch),
            )
            .await
        })
    }

    fn take_replay_profile(&mut self) -> Option<ReplayProfile> {
        ZmqKvEventSource::take_replay_profile(self)
    }
}

/// Content-free result of streaming a replay into private source state.
pub struct CompanionFullReplay {
    sequences: Vec<u64>,
    establishes_boundary: bool,
    apply_error: Option<CompanionIndexSourceError>,
    expected_generation: u64,
    source: Arc<CompanionIndexSource>,
}

impl CompanionFullReplay {
    #[must_use]
    pub fn new(source: Arc<CompanionIndexSource>) -> Self {
        let expected_generation = source.status().companion_generation;
        Self {
            sequences: Vec::new(),
            establishes_boundary: false,
            apply_error: None,
            expected_generation,
            source,
        }
    }

    pub fn apply(&mut self, batch: &SequencedBatch) {
        self.sequences.push(batch.sequence);
        self.establishes_boundary |= batch.batch.clears_all();
        if self.apply_error.is_none()
            && let Err(error) = self
                .source
                .apply_replay_for_generation(self.expected_generation, batch)
        {
            self.apply_error = Some(error);
        }
    }
}

/// Factory for a fresh subscribed connection after every authority loss.
pub trait CompanionKvEventConnector: Send + Sync + 'static {
    fn connect(
        &self,
    ) -> BoxFuture<'_, Result<Box<dyn CompanionKvEventTransport>, KvTransportError>>;
}

#[derive(Clone, Debug)]
pub struct ZmqCompanionKvEventConnector {
    config: KvTransportConfig,
}

impl ZmqCompanionKvEventConnector {
    #[must_use]
    pub const fn new(config: KvTransportConfig) -> Self {
        Self { config }
    }
}

impl CompanionKvEventConnector for ZmqCompanionKvEventConnector {
    fn connect(
        &self,
    ) -> BoxFuture<'_, Result<Box<dyn CompanionKvEventTransport>, KvTransportError>> {
        Box::pin(async move {
            ZmqKvEventSource::connect(self.config.clone())
                .await
                .map(|source| Box::new(source) as Box<dyn CompanionKvEventTransport>)
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionIndexOwnerRebuildReason {
    Startup,
    AuthorityChanged,
    AuthorityLost,
    Disconnected,
    Transport,
    Replay,
    ReplayInvalid,
    ReplayTooLarge,
    Apply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionIndexOwnerReplayKind {
    Full,
    Gap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionIndexOwnerReplayOutcome {
    Complete,
    TransportFailed,
    Invalid,
    Cancelled,
}

/// Closed, content-free event surface suitable for metrics and diagnostics.
///
/// No endpoint, engine identity, sequence, payload, token, or hash is exposed.
#[derive(Clone, Debug, PartialEq)]
pub enum CompanionIndexOwnerEvent {
    AuthorityAvailable,
    AuthorityLost,
    ConnectAttempt,
    Connected,
    Rebuild(CompanionIndexOwnerRebuildReason),
    Replay {
        kind: CompanionIndexOwnerReplayKind,
        outcome: CompanionIndexOwnerReplayOutcome,
        profile: Option<ReplayProfile>,
    },
    Ready,
    LiveApplied,
    LiveDuplicate,
    Shutdown,
}

pub trait CompanionIndexOwnerObserver: Send + Sync + 'static {
    fn observe(&self, event: CompanionIndexOwnerEvent);
}

#[derive(Debug, Default)]
pub struct NoopCompanionIndexOwnerObserver;

impl CompanionIndexOwnerObserver for NoopCompanionIndexOwnerObserver {
    fn observe(&self, _event: CompanionIndexOwnerEvent) {}
}

#[derive(Clone, Copy, Debug)]
pub struct CompanionIndexOwnerConfig {
    pub replay_limit: u64,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl CompanionIndexOwnerConfig {
    fn valid(self) -> bool {
        self.replay_limit > 0
            && !self.reconnect_min.is_zero()
            && self.reconnect_min <= self.reconnect_max
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompanionIndexOwnerReport {
    pub connections: u64,
    pub rebuilds: u64,
    pub replay_batches: u64,
    pub live_batches: u64,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CompanionIndexOwnerError {
    #[error("companion index owner configuration is invalid")]
    InvalidConfig,
    #[error("explicit engine-incarnation authority channel closed")]
    AuthorityClosed,
    #[error("companion index generation was exhausted")]
    GenerationExhausted,
    #[error("companion index owner source transition failed")]
    Source,
}

impl CompanionIndexOwnerError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_config",
            Self::AuthorityClosed => "authority_closed",
            Self::GenerationExhausted => "generation_exhausted",
            Self::Source => "source_error",
        }
    }
}

/// Own one engine's transport/fence/source lifecycle until shutdown.
pub struct CompanionIndexOwner {
    config: CompanionIndexOwnerConfig,
    source: Arc<CompanionIndexSource>,
    connector: Arc<dyn CompanionKvEventConnector>,
    observer: Arc<dyn CompanionIndexOwnerObserver>,
}

impl CompanionIndexOwner {
    #[must_use]
    pub fn new(
        config: CompanionIndexOwnerConfig,
        source: Arc<CompanionIndexSource>,
        connector: Arc<dyn CompanionKvEventConnector>,
        observer: Arc<dyn CompanionIndexOwnerObserver>,
    ) -> Self {
        Self {
            config,
            source,
            connector,
            observer,
        }
    }

    /// Run until explicit shutdown or permanent authority/generation failure.
    ///
    /// `engine_authority` must be populated only from the authenticated engine
    /// metadata path. `None` is an explicit loss of authority and immediately
    /// fences the source. A changed incarnation also fences before reconnect.
    ///
    /// # Errors
    ///
    /// Returns only content-free configuration, authority-channel, or
    /// generation-exhaustion failures. Recoverable transport/data errors remain
    /// fenced and retry with bounded backoff.
    #[allow(clippy::too_many_lines)]
    pub async fn run(
        self,
        mut engine_authority: watch::Receiver<Option<EngineIncarnation>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<CompanionIndexOwnerReport, CompanionIndexOwnerError> {
        if !self.config.valid() {
            return Err(CompanionIndexOwnerError::InvalidConfig);
        }
        // Tokio task abort drops this guard synchronously. Authority therefore
        // fails closed even when a supervisor cannot signal graceful shutdown.
        let _cancellation_fence = OwnerCancellationFence {
            source: Arc::clone(&self.source),
            observer: Arc::clone(&self.observer),
        };

        let mut report = CompanionIndexOwnerReport::default();
        let mut fence = KvEventFence::new(self.config.replay_limit);
        let mut current_authority = engine_authority.borrow_and_update().clone();
        let mut backoff = self.config.reconnect_min;
        let mut full_replay_through = None;
        rebuild(
            &self.source,
            &mut fence,
            current_authority.clone(),
            CompanionIndexOwnerRebuildReason::Startup,
            &self.observer,
            &mut report,
        )?;

        loop {
            let Some(authority) =
                wait_for_authority(&mut engine_authority, &mut shutdown, &self.observer).await?
            else {
                return Ok(report);
            };

            if current_authority.as_ref() != Some(&authority) {
                // A watermark belongs only to the incarnation that produced
                // it; never carry reconnect recovery across authority change.
                full_replay_through = None;
                rebuild(
                    &self.source,
                    &mut fence,
                    Some(authority.clone()),
                    CompanionIndexOwnerRebuildReason::AuthorityChanged,
                    &self.observer,
                    &mut report,
                )?;
                current_authority = Some(authority.clone());
            }

            self.observer
                .observe(CompanionIndexOwnerEvent::ConnectAttempt);
            let connected = tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => {
                    return Ok(report);
                }
                changed = engine_authority.changed() => {
                    if changed.is_err() {
                        fence_authority_closed(&self.source, &mut fence, &self.observer, &mut report)?;
                        return Err(CompanionIndexOwnerError::AuthorityClosed);
                    }
                    continue;
                }
                result = self.connector.connect() => result,
            };
            let Ok(mut transport) = connected else {
                if !wait_backoff_or_change(backoff, &mut engine_authority, &mut shutdown).await? {
                    return Ok(report);
                }
                backoff = next_backoff(backoff, self.config.reconnect_max);
                continue;
            };
            report.connections = report.connections.saturating_add(1);
            self.observer.observe(CompanionIndexOwnerEvent::Connected);

            match consume_connection(
                &self.source,
                &mut fence,
                transport.as_mut(),
                &authority,
                &mut engine_authority,
                &mut shutdown,
                &self.observer,
                &mut report,
                full_replay_through.take(),
            )
            .await?
            {
                ConnectionExit::Shutdown => {
                    return Ok(report);
                }
                ConnectionExit::AuthorityChanged(refreshed) => {
                    current_authority = refreshed;
                }
                ConnectionExit::Retry { reason, was_ready } => {
                    full_replay_through = recovery_through(&self.source, &fence);
                    rebuild(
                        &self.source,
                        &mut fence,
                        None,
                        reason,
                        &self.observer,
                        &mut report,
                    )?;
                    if was_ready {
                        backoff = self.config.reconnect_min;
                    }
                    if !wait_backoff_or_change(backoff, &mut engine_authority, &mut shutdown)
                        .await?
                    {
                        return Ok(report);
                    }
                    backoff = next_backoff(backoff, self.config.reconnect_max);
                }
            }
        }
    }
}

struct OwnerCancellationFence {
    source: Arc<CompanionIndexSource>,
    observer: Arc<dyn CompanionIndexOwnerObserver>,
}

impl Drop for OwnerCancellationFence {
    fn drop(&mut self) {
        let _ = self.source.begin_rebuild(None);
        self.observer.observe(CompanionIndexOwnerEvent::Shutdown);
    }
}

enum ConnectionExit {
    Shutdown,
    AuthorityChanged(Option<EngineIncarnation>),
    Retry {
        reason: CompanionIndexOwnerRebuildReason,
        was_ready: bool,
    },
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn consume_connection(
    source: &Arc<CompanionIndexSource>,
    fence: &mut KvEventFence,
    transport: &mut dyn CompanionKvEventTransport,
    authority: &EngineIncarnation,
    engine_authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
    full_replay_through: Option<u64>,
) -> Result<ConnectionExit, CompanionIndexOwnerError> {
    let mut ready = false;
    if let Some(through) = full_replay_through {
        if !fence.prepare_full_replay(through) {
            return Ok(ConnectionExit::Retry {
                reason: CompanionIndexOwnerRebuildReason::ReplayTooLarge,
                was_ready: false,
            });
        }
        match recover_full(
            Arc::clone(source),
            fence,
            transport,
            through,
            engine_authority,
            shutdown,
            observer,
            report,
        )
        .await?
        {
            ReplayExit::Complete => {
                ready = true;
                observer.observe(CompanionIndexOwnerEvent::Ready);
            }
            ReplayExit::Shutdown => return Ok(ConnectionExit::Shutdown),
            ReplayExit::AuthorityChanged(refreshed) => {
                return Ok(ConnectionExit::AuthorityChanged(refreshed));
            }
            ReplayExit::Retry(reason) => {
                return Ok(ConnectionExit::Retry {
                    reason,
                    was_ready: false,
                });
            }
        }
    }
    loop {
        let activity = tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return Ok(ConnectionExit::Shutdown),
            changed = engine_authority.changed() => {
                if changed.is_err() {
                    fence_authority_closed(source, fence, observer, report)?;
                    return Err(CompanionIndexOwnerError::AuthorityClosed);
                }
                let refreshed = engine_authority.borrow_and_update().clone();
                if refreshed.as_ref() != Some(authority) {
                    fence_refreshed_authority(source, fence, refreshed.clone(), observer, report)?;
                    return Ok(ConnectionExit::AuthorityChanged(refreshed));
                }
                continue;
            }
            result = transport.recv_live_activity() => result,
        };
        let Ok(activity) = activity else {
            return Ok(ConnectionExit::Retry {
                reason: CompanionIndexOwnerRebuildReason::Transport,
                was_ready: ready,
            });
        };
        let batch = match activity {
            LiveActivity::Connected => continue,
            LiveActivity::Disconnected => {
                return Ok(ConnectionExit::Retry {
                    reason: CompanionIndexOwnerRebuildReason::Disconnected,
                    was_ready: ready,
                });
            }
            LiveActivity::Batch(batch) => batch,
        };

        if !ready {
            match bootstrap_from_live(
                source,
                fence,
                transport,
                batch,
                engine_authority,
                shutdown,
                observer,
                report,
            )
            .await?
            {
                ReplayExit::Complete => {
                    ready = true;
                    observer.observe(CompanionIndexOwnerEvent::Ready);
                }
                ReplayExit::Shutdown => return Ok(ConnectionExit::Shutdown),
                ReplayExit::AuthorityChanged(refreshed) => {
                    return Ok(ConnectionExit::AuthorityChanged(refreshed));
                }
                ReplayExit::Retry(reason) => {
                    return Ok(ConnectionExit::Retry {
                        reason,
                        was_ready: false,
                    });
                }
            }
            continue;
        }

        match fence.ingest(batch.sequence, batch.batch.clears_all()) {
            IngestAction::Apply | IngestAction::ResetAndApply => {
                if source.apply_live(&batch).is_err() {
                    return Ok(ConnectionExit::Retry {
                        reason: CompanionIndexOwnerRebuildReason::Apply,
                        was_ready: true,
                    });
                }
                report.live_batches = report.live_batches.saturating_add(1);
                observer.observe(CompanionIndexOwnerEvent::LiveApplied);
            }
            IngestAction::Duplicate => {
                observer.observe(CompanionIndexOwnerEvent::LiveDuplicate);
            }
            IngestAction::Replay { from, through } => {
                match recover_gap(
                    source,
                    fence,
                    transport,
                    from,
                    through,
                    engine_authority,
                    shutdown,
                    observer,
                    report,
                )
                .await?
                {
                    ReplayExit::Complete => {}
                    ReplayExit::Shutdown => return Ok(ConnectionExit::Shutdown),
                    ReplayExit::AuthorityChanged(refreshed) => {
                        return Ok(ConnectionExit::AuthorityChanged(refreshed));
                    }
                    ReplayExit::Retry(reason) => {
                        return Ok(ConnectionExit::Retry {
                            reason,
                            was_ready: true,
                        });
                    }
                }
            }
            IngestAction::ObserveOnly | IngestAction::UnrecoverableGap => {
                return Ok(ConnectionExit::Retry {
                    reason: CompanionIndexOwnerRebuildReason::ReplayTooLarge,
                    was_ready: true,
                });
            }
        }
    }
}

enum ReplayExit {
    Complete,
    Shutdown,
    AuthorityChanged(Option<EngineIncarnation>),
    Retry(CompanionIndexOwnerRebuildReason),
}

#[allow(clippy::too_many_arguments)]
async fn bootstrap_from_live(
    source: &Arc<CompanionIndexSource>,
    fence: &mut KvEventFence,
    transport: &mut dyn CompanionKvEventTransport,
    first: SequencedBatch,
    engine_authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
) -> Result<ReplayExit, CompanionIndexOwnerError> {
    match fence.ingest(first.sequence, first.batch.clears_all()) {
        IngestAction::Apply | IngestAction::ResetAndApply => {
            if source.apply_replay(&first).is_err() || source.finish_replay(first.sequence).is_err()
            {
                return Ok(ReplayExit::Retry(CompanionIndexOwnerRebuildReason::Apply));
            }
            report.replay_batches = report.replay_batches.saturating_add(1);
            Ok(ReplayExit::Complete)
        }
        IngestAction::Replay { from, through } => {
            debug_assert_eq!(from, 0);
            recover_full(
                Arc::clone(source),
                fence,
                transport,
                through,
                engine_authority,
                shutdown,
                observer,
                report,
            )
            .await
        }
        IngestAction::Duplicate => Ok(ReplayExit::Retry(
            CompanionIndexOwnerRebuildReason::ReplayInvalid,
        )),
        IngestAction::ObserveOnly | IngestAction::UnrecoverableGap => Ok(ReplayExit::Retry(
            CompanionIndexOwnerRebuildReason::ReplayTooLarge,
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn recover_full(
    source: Arc<CompanionIndexSource>,
    fence: &mut KvEventFence,
    transport: &mut dyn CompanionKvEventTransport,
    through: u64,
    engine_authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
) -> Result<ReplayExit, CompanionIndexOwnerError> {
    let replayed = match await_full_replay_or_control(
        transport,
        through,
        Arc::clone(&source),
        engine_authority,
        shutdown,
    )
    .await
    {
        FullReplayControl::Replay(replay) => replay,
        FullReplayControl::Failed => {
            record_replay(
                transport,
                observer,
                CompanionIndexOwnerReplayKind::Full,
                CompanionIndexOwnerReplayOutcome::TransportFailed,
            );
            return Ok(ReplayExit::Retry(CompanionIndexOwnerRebuildReason::Replay));
        }
        FullReplayControl::Shutdown => {
            record_replay(
                transport,
                observer,
                CompanionIndexOwnerReplayKind::Full,
                CompanionIndexOwnerReplayOutcome::Cancelled,
            );
            return Ok(ReplayExit::Shutdown);
        }
        FullReplayControl::AuthorityChanged(refreshed) => {
            record_replay(
                transport,
                observer,
                CompanionIndexOwnerReplayKind::Full,
                CompanionIndexOwnerReplayOutcome::Cancelled,
            );
            fence_refreshed_authority(&source, fence, refreshed.clone(), observer, report)?;
            return Ok(ReplayExit::AuthorityChanged(refreshed));
        }
    };
    if replayed.apply_error.is_some()
        || fence.accept_replay(&replayed.sequences, replayed.establishes_boundary)
            != ReplayAction::Recovered
    {
        record_replay(
            transport,
            observer,
            CompanionIndexOwnerReplayKind::Full,
            CompanionIndexOwnerReplayOutcome::Invalid,
        );
        return Ok(ReplayExit::Retry(
            CompanionIndexOwnerRebuildReason::ReplayInvalid,
        ));
    }
    if source.finish_replay(through).is_err() {
        record_replay(
            transport,
            observer,
            CompanionIndexOwnerReplayKind::Full,
            CompanionIndexOwnerReplayOutcome::Invalid,
        );
        return Ok(ReplayExit::Retry(CompanionIndexOwnerRebuildReason::Apply));
    }
    report.replay_batches = report
        .replay_batches
        .saturating_add(u64::try_from(replayed.sequences.len()).unwrap_or(u64::MAX));
    record_replay(
        transport,
        observer,
        CompanionIndexOwnerReplayKind::Full,
        CompanionIndexOwnerReplayOutcome::Complete,
    );
    Ok(ReplayExit::Complete)
}

#[allow(clippy::too_many_arguments)]
async fn recover_gap(
    source: &CompanionIndexSource,
    fence: &mut KvEventFence,
    transport: &mut dyn CompanionKvEventTransport,
    from: u64,
    through: u64,
    engine_authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
) -> Result<ReplayExit, CompanionIndexOwnerError> {
    let replayed =
        match await_replay_or_control(transport, from, through, engine_authority, shutdown).await {
            ReplayControl::Batches(batches) => batches,
            ReplayControl::Failed => {
                record_replay(
                    transport,
                    observer,
                    CompanionIndexOwnerReplayKind::Gap,
                    CompanionIndexOwnerReplayOutcome::TransportFailed,
                );
                return Ok(ReplayExit::Retry(CompanionIndexOwnerRebuildReason::Replay));
            }
            ReplayControl::Shutdown => {
                record_replay(
                    transport,
                    observer,
                    CompanionIndexOwnerReplayKind::Gap,
                    CompanionIndexOwnerReplayOutcome::Cancelled,
                );
                return Ok(ReplayExit::Shutdown);
            }
            ReplayControl::AuthorityChanged(refreshed) => {
                record_replay(
                    transport,
                    observer,
                    CompanionIndexOwnerReplayKind::Gap,
                    CompanionIndexOwnerReplayOutcome::Cancelled,
                );
                fence_refreshed_authority(source, fence, refreshed.clone(), observer, report)?;
                return Ok(ReplayExit::AuthorityChanged(refreshed));
            }
        };
    let sequences = replayed
        .iter()
        .map(|batch| batch.sequence)
        .collect::<Vec<_>>();
    if fence.accept_replay(
        &sequences,
        replayed.iter().any(|batch| batch.batch.clears_all()),
    ) == ReplayAction::Invalid
    {
        record_replay(
            transport,
            observer,
            CompanionIndexOwnerReplayKind::Gap,
            CompanionIndexOwnerReplayOutcome::Invalid,
        );
        return Ok(ReplayExit::Retry(
            CompanionIndexOwnerRebuildReason::ReplayInvalid,
        ));
    }
    for batch in &replayed {
        if source.apply_live(batch).is_err() {
            record_replay(
                transport,
                observer,
                CompanionIndexOwnerReplayKind::Gap,
                CompanionIndexOwnerReplayOutcome::Invalid,
            );
            return Ok(ReplayExit::Retry(CompanionIndexOwnerRebuildReason::Apply));
        }
    }
    report.replay_batches = report
        .replay_batches
        .saturating_add(u64::try_from(replayed.len()).unwrap_or(u64::MAX));
    record_replay(
        transport,
        observer,
        CompanionIndexOwnerReplayKind::Gap,
        CompanionIndexOwnerReplayOutcome::Complete,
    );
    Ok(ReplayExit::Complete)
}

enum ReplayControl {
    Batches(Vec<SequencedBatch>),
    Failed,
    Shutdown,
    AuthorityChanged(Option<EngineIncarnation>),
}

enum FullReplayControl {
    Replay(CompanionFullReplay),
    Failed,
    Shutdown,
    AuthorityChanged(Option<EngineIncarnation>),
}

async fn await_replay_or_control(
    transport: &mut dyn CompanionKvEventTransport,
    from: u64,
    through: u64,
    engine_authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
) -> ReplayControl {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => ReplayControl::Shutdown,
        changed = engine_authority.changed() => {
            if changed.is_err() {
                ReplayControl::AuthorityChanged(None)
            } else {
                ReplayControl::AuthorityChanged(engine_authority.borrow_and_update().clone())
            }
        }
        result = transport.replay(from, through) => match result {
            Ok(batches) => ReplayControl::Batches(batches),
            Err(_) => ReplayControl::Failed,
        }
    }
}

async fn await_full_replay_or_control(
    transport: &mut dyn CompanionKvEventTransport,
    through: u64,
    source: Arc<CompanionIndexSource>,
    engine_authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
) -> FullReplayControl {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => FullReplayControl::Shutdown,
        changed = engine_authority.changed() => {
            if changed.is_err() {
                FullReplayControl::AuthorityChanged(None)
            } else {
                FullReplayControl::AuthorityChanged(engine_authority.borrow_and_update().clone())
            }
        }
        result = transport.replay_full(through, source) => match result {
            Ok(replay) => FullReplayControl::Replay(replay),
            Err(_) => FullReplayControl::Failed,
        }
    }
}

fn record_replay(
    transport: &mut dyn CompanionKvEventTransport,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    kind: CompanionIndexOwnerReplayKind,
    outcome: CompanionIndexOwnerReplayOutcome,
) {
    observer.observe(CompanionIndexOwnerEvent::Replay {
        kind,
        outcome,
        profile: transport.take_replay_profile(),
    });
}

fn rebuild(
    source: &CompanionIndexSource,
    fence: &mut KvEventFence,
    authority: Option<EngineIncarnation>,
    reason: CompanionIndexOwnerRebuildReason,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
) -> Result<(), CompanionIndexOwnerError> {
    fence.generation_changed();
    source.begin_rebuild(authority).map_err(map_source_error)?;
    report.rebuilds = report.rebuilds.saturating_add(1);
    observer.observe(CompanionIndexOwnerEvent::Rebuild(reason));
    Ok(())
}

fn fence_authority_closed(
    source: &CompanionIndexSource,
    fence: &mut KvEventFence,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
) -> Result<(), CompanionIndexOwnerError> {
    observer.observe(CompanionIndexOwnerEvent::AuthorityLost);
    rebuild(
        source,
        fence,
        None,
        CompanionIndexOwnerRebuildReason::AuthorityLost,
        observer,
        report,
    )
}

fn fence_refreshed_authority(
    source: &CompanionIndexSource,
    fence: &mut KvEventFence,
    refreshed: Option<EngineIncarnation>,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
    report: &mut CompanionIndexOwnerReport,
) -> Result<(), CompanionIndexOwnerError> {
    let reason = if refreshed.is_some() {
        CompanionIndexOwnerRebuildReason::AuthorityChanged
    } else {
        observer.observe(CompanionIndexOwnerEvent::AuthorityLost);
        CompanionIndexOwnerRebuildReason::AuthorityLost
    };
    rebuild(source, fence, refreshed, reason, observer, report)
}

fn map_source_error(error: CompanionIndexSourceError) -> CompanionIndexOwnerError {
    if error == CompanionIndexSourceError::GenerationExhausted {
        CompanionIndexOwnerError::GenerationExhausted
    } else {
        CompanionIndexOwnerError::Source
    }
}

async fn wait_for_authority(
    authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
    observer: &Arc<dyn CompanionIndexOwnerObserver>,
) -> Result<Option<EngineIncarnation>, CompanionIndexOwnerError> {
    loop {
        if *shutdown.borrow() {
            return Ok(None);
        }
        if let Some(incarnation) = authority.borrow_and_update().clone() {
            observer.observe(CompanionIndexOwnerEvent::AuthorityAvailable);
            return Ok(Some(incarnation));
        }
        observer.observe(CompanionIndexOwnerEvent::AuthorityLost);
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return Ok(None),
            changed = authority.changed() => {
                if changed.is_err() {
                    return Err(CompanionIndexOwnerError::AuthorityClosed);
                }
            }
        }
    }
}

async fn wait_backoff_or_change(
    duration: Duration,
    authority: &mut watch::Receiver<Option<EngineIncarnation>>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<bool, CompanionIndexOwnerError> {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => Ok(false),
        changed = authority.changed() => {
            if changed.is_err() {
                Err(CompanionIndexOwnerError::AuthorityClosed)
            } else {
                Ok(true)
            }
        }
        () = sleep(duration) => Ok(true),
    }
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

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

fn recovery_through(source: &CompanionIndexSource, fence: &KvEventFence) -> Option<u64> {
    let fenced = fence.next_sequence().and_then(|next| next.checked_sub(1));
    match (source.status().watermark, fenced) {
        (Some(source), Some(fenced)) => Some(source.max(fenced)),
        (source, fenced) => source.or(fenced),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use bytes::Bytes;
    use futures::future;
    use tokio::{sync::mpsc, time::timeout};
    use zeromq::{PubSocket, RouterSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

    use super::*;
    use crate::{
        companion_index_source::CompanionIndexSourceConfig,
        digest_index::DigestIndexLimits,
        kv_snapshot::{AttentionKind, GroupDisposition, GroupMetadata, SnapshotLimits},
        kv_wire::{BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
    };

    const SECRET: [u8; 32] = *b"0123456789abcdef0123456789abcdef";

    fn incarnation(name: &str, started: u64) -> EngineIncarnation {
        EngineIncarnation {
            engine_id: name.to_owned(),
            model_revision: "revision".to_owned(),
            image_digest: "sha256:image".to_owned(),
            process_started_unix_ns: started,
            attestation_sha256: vec![7; 32],
        }
    }

    fn source() -> Arc<CompanionIndexSource> {
        Arc::new(
            CompanionIndexSource::new(
                CompanionIndexSourceConfig {
                    group: GroupMetadata {
                        data_parallel_rank: 0,
                        group_idx: 0,
                        attention_kind: AttentionKind::MlaAttention,
                        disposition: GroupDisposition::Indexed,
                        block_size: 1,
                    },
                    index_limits: DigestIndexLimits::default(),
                    snapshot_limits: SnapshotLimits::default(),
                    max_active_sessions: 2,
                },
                incarnation("engine-a", 1),
                1,
                &SECRET,
            )
            .unwrap(),
        )
    }

    fn batch(sequence: u64, hash: Option<u64>) -> SequencedBatch {
        let events = hash.map_or_else(Vec::new, |hash| {
            vec![KvEvent::BlockStored(BlockStored {
                block_hashes: vec![ExternalBlockHash::Unsigned(hash)],
                parent_block_hash: None,
                token_ids: vec![u32::try_from(hash).unwrap()],
                block_size: 1,
                group_idx: Some(0),
                kv_cache_spec_kind: Some("mla_attention".to_owned()),
                kv_cache_spec_sliding_window: None,
                medium: Some("GPU".to_owned()),
                locality: Some("LOCAL".to_owned()),
                lora_name: None,
                cache_namespace: None,
                has_extra_keys: false,
            })]
        });
        SequencedBatch {
            sequence,
            payload: Bytes::from_static(b"bounded"),
            batch: KvEventBatch {
                timestamp: 1.0,
                events,
                data_parallel_rank: Some(0),
            },
        }
    }

    enum ReplayPlan {
        Complete(Vec<SequencedBatch>),
        Pending(Arc<AtomicBool>),
    }

    struct CancelWitness(Arc<AtomicBool>);

    impl Drop for CancelWitness {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct MockTransport {
        activities: mpsc::UnboundedReceiver<Result<LiveActivity, KvTransportError>>,
        replays: VecDeque<ReplayPlan>,
        requests: Arc<Mutex<Vec<(u64, u64)>>>,
    }

    impl MockTransport {
        fn replay_plan(&mut self, from: u64, through: u64) -> ReplayPlan {
            self.requests.lock().unwrap().push((from, through));
            self.replays.pop_front().expect("unexpected replay")
        }
    }

    impl CompanionKvEventTransport for MockTransport {
        fn recv_live_activity(&mut self) -> BoxFuture<'_, Result<LiveActivity, KvTransportError>> {
            Box::pin(async move {
                self.activities
                    .recv()
                    .await
                    .unwrap_or(Err(KvTransportError::Socket))
            })
        }

        fn replay(
            &mut self,
            from: u64,
            through: u64,
        ) -> BoxFuture<'_, Result<Vec<SequencedBatch>, KvTransportError>> {
            let plan = self.replay_plan(from, through);
            Box::pin(async move {
                match plan {
                    ReplayPlan::Complete(batches) => Ok(batches),
                    ReplayPlan::Pending(cancelled) => {
                        let _witness = CancelWitness(cancelled);
                        future::pending().await
                    }
                }
            })
        }

        fn replay_full(
            &mut self,
            through: u64,
            source: Arc<CompanionIndexSource>,
        ) -> BoxFuture<'_, Result<CompanionFullReplay, KvTransportError>> {
            let plan = self.replay_plan(0, through);
            Box::pin(async move {
                match plan {
                    ReplayPlan::Complete(batches) => {
                        let mut replay = CompanionFullReplay::new(source);
                        for batch in batches {
                            replay.apply(&batch);
                        }
                        Ok(replay)
                    }
                    ReplayPlan::Pending(cancelled) => {
                        let _witness = CancelWitness(cancelled);
                        future::pending().await
                    }
                }
            })
        }

        fn take_replay_profile(&mut self) -> Option<ReplayProfile> {
            None
        }
    }

    struct MockConnector {
        transports: Mutex<VecDeque<Box<dyn CompanionKvEventTransport>>>,
    }

    impl CompanionKvEventConnector for MockConnector {
        fn connect(
            &self,
        ) -> BoxFuture<'_, Result<Box<dyn CompanionKvEventTransport>, KvTransportError>> {
            let result = self
                .transports
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(KvTransportError::Socket);
            Box::pin(future::ready(result))
        }
    }

    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<CompanionIndexOwnerEvent>>);

    impl CompanionIndexOwnerObserver for RecordingObserver {
        fn observe(&self, event: CompanionIndexOwnerEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn owner(
        source: Arc<CompanionIndexSource>,
        transports: Vec<Box<dyn CompanionKvEventTransport>>,
        observer: Arc<RecordingObserver>,
    ) -> CompanionIndexOwner {
        CompanionIndexOwner::new(
            CompanionIndexOwnerConfig {
                replay_limit: 16,
                reconnect_min: Duration::from_millis(5),
                reconnect_max: Duration::from_millis(20),
            },
            source,
            Arc::new(MockConnector {
                transports: Mutex::new(transports.into()),
            }),
            observer,
        )
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        timeout(Duration::from_secs(1), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn abandoned_full_replay_cannot_mutate_a_new_generation() {
        let source = source();
        let mut abandoned = CompanionFullReplay::new(Arc::clone(&source));
        source.begin_rebuild(None).unwrap();
        abandoned.apply(&batch(0, Some(10)));
        assert_eq!(
            abandoned.apply_error,
            Some(CompanionIndexSourceError::InvalidReplay)
        );

        source.apply_replay(&batch(0, Some(20))).unwrap();
        source.finish_replay(0).unwrap();
        assert_eq!(source.status().indexed_blocks, 1);
    }

    fn zmq_message(frames: Vec<Bytes>) -> ZmqMessage {
        ZmqMessage::try_from(frames).unwrap()
    }

    async fn send_replay(router: &mut RouterSocket, identity: Bytes, sequences: &[u64]) {
        const EMPTY_BATCH: &[u8] = &[
            0x93, 0xcb, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0x00,
        ];
        for sequence in sequences {
            router
                .send(zmq_message(vec![
                    identity.clone(),
                    Bytes::new(),
                    Bytes::from_static(b"kv"),
                    Bytes::copy_from_slice(&sequence.to_be_bytes()),
                    Bytes::from_static(EMPTY_BATCH),
                ]))
                .await
                .unwrap();
        }
        router
            .send(zmq_message(vec![
                identity,
                Bytes::new(),
                Bytes::new(),
                Bytes::copy_from_slice(&u64::MAX.to_be_bytes()),
                Bytes::new(),
            ]))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn subscribes_then_builds_sparse_full_replay_and_recovers_sparse_gap() {
        let source = source();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();
        let transport = MockTransport {
            activities: activity_rx,
            replays: VecDeque::from([
                ReplayPlan::Complete(vec![batch(0, Some(10)), batch(2, None), batch(4, None)]),
                ReplayPlan::Complete(vec![batch(5, Some(11)), batch(7, None)]),
            ]),
            requests: Arc::clone(&requests),
        };
        let observer = Arc::new(RecordingObserver::default());
        let (authority_tx, authority_rx) = watch::channel(Some(incarnation("engine-a", 1)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(
            owner(
                Arc::clone(&source),
                vec![Box::new(transport)],
                Arc::clone(&observer),
            )
            .run(authority_rx, shutdown_rx),
        );

        activity_tx.send(Ok(LiveActivity::Connected)).unwrap();
        activity_tx
            .send(Ok(LiveActivity::Batch(batch(4, None))))
            .unwrap();
        wait_until(|| source.status().ready && source.status().watermark == Some(4)).await;
        activity_tx
            .send(Ok(LiveActivity::Batch(batch(7, None))))
            .unwrap();
        wait_until(|| source.status().watermark == Some(7)).await;
        assert_eq!(source.status().indexed_blocks, 2);
        assert_eq!(*requests.lock().unwrap(), [(0, 4), (5, 7)]);

        shutdown_tx.send(true).unwrap();
        let report = task.await.unwrap().unwrap();
        assert_eq!(report.connections, 1);
        assert_eq!(report.replay_batches, 5);
        assert!(!source.status().ready);
        assert!(
            observer
                .0
                .lock()
                .unwrap()
                .contains(&CompanionIndexOwnerEvent::Ready)
        );
        drop(authority_tx);
    }

    #[tokio::test]
    async fn explicit_authority_loss_fences_before_waiting_for_refresh() {
        let source = source();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (first_tx, first_rx) = mpsc::unbounded_channel();
        let (second_tx, second_rx) = mpsc::unbounded_channel();
        let transports: Vec<Box<dyn CompanionKvEventTransport>> = vec![
            Box::new(MockTransport {
                activities: first_rx,
                replays: VecDeque::new(),
                requests: Arc::clone(&requests),
            }),
            Box::new(MockTransport {
                activities: second_rx,
                replays: VecDeque::new(),
                requests,
            }),
        ];
        let observer = Arc::new(RecordingObserver::default());
        let (authority_tx, authority_rx) = watch::channel(Some(incarnation("engine-a", 1)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(
            owner(Arc::clone(&source), transports, observer).run(authority_rx, shutdown_rx),
        );

        first_tx
            .send(Ok(LiveActivity::Batch(batch(0, Some(10)))))
            .unwrap();
        wait_until(|| source.status().ready).await;
        authority_tx.send(None).unwrap();
        wait_until(|| !source.status().ready).await;
        let fenced_generation = source.status().companion_generation;

        authority_tx.send(Some(incarnation("engine-a", 2))).unwrap();
        second_tx
            .send(Ok(LiveActivity::Batch(batch(0, Some(20)))))
            .unwrap();
        wait_until(|| source.status().ready).await;
        assert!(source.status().companion_generation > fenced_generation);
        assert_eq!(source.status().watermark, Some(0));

        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn transport_disconnect_fences_before_a_fresh_subscribed_connection() {
        let source = source();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (first_tx, first_rx) = mpsc::unbounded_channel();
        let (second_tx, second_rx) = mpsc::unbounded_channel();
        let transports: Vec<Box<dyn CompanionKvEventTransport>> = vec![
            Box::new(MockTransport {
                activities: first_rx,
                replays: VecDeque::new(),
                requests: Arc::clone(&requests),
            }),
            Box::new(MockTransport {
                activities: second_rx,
                replays: VecDeque::from([ReplayPlan::Complete(vec![batch(0, Some(20))])]),
                requests,
            }),
        ];
        let observer = Arc::new(RecordingObserver::default());
        let (_authority_tx, authority_rx) = watch::channel(Some(incarnation("engine-a", 1)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(
            owner(Arc::clone(&source), transports, observer).run(authority_rx, shutdown_rx),
        );

        first_tx
            .send(Ok(LiveActivity::Batch(batch(0, Some(10)))))
            .unwrap();
        wait_until(|| source.status().ready).await;
        let generation = source.status().companion_generation;
        first_tx.send(Ok(LiveActivity::Disconnected)).unwrap();
        wait_until(|| source.status().companion_generation > generation).await;

        wait_until(|| source.status().ready).await;
        assert_eq!(source.status().watermark, Some(0));
        assert_eq!(source.status().indexed_blocks, 1);

        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
        drop(second_tx);
    }

    #[tokio::test]
    async fn shutdown_cancels_stalled_replay_and_fences_source() {
        let source = source();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replay_cancelled = Arc::new(AtomicBool::new(false));
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();
        let transport = MockTransport {
            activities: activity_rx,
            replays: VecDeque::from([ReplayPlan::Pending(Arc::clone(&replay_cancelled))]),
            requests: Arc::clone(&requests),
        };
        let observer = Arc::new(RecordingObserver::default());
        let (_authority_tx, authority_rx) = watch::channel(Some(incarnation("engine-a", 1)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(
            owner(Arc::clone(&source), vec![Box::new(transport)], observer)
                .run(authority_rx, shutdown_rx),
        );

        activity_tx
            .send(Ok(LiveActivity::Batch(batch(4, None))))
            .unwrap();
        wait_until(|| *requests.lock().unwrap() == [(0, 4)]).await;
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_millis(250), task)
            .await
            .expect("owner shutdown must cancel replay promptly")
            .unwrap()
            .unwrap();
        assert!(replay_cancelled.load(Ordering::Acquire));
        assert!(!source.status().ready);
    }

    #[tokio::test]
    async fn aborting_owner_task_immediately_fences_ready_source() {
        let source = source();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();
        let transport = MockTransport {
            activities: activity_rx,
            replays: VecDeque::new(),
            requests,
        };
        let observer = Arc::new(RecordingObserver::default());
        let (_authority_tx, authority_rx) = watch::channel(Some(incarnation("engine-a", 1)));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(
            owner(
                Arc::clone(&source),
                vec![Box::new(transport)],
                Arc::clone(&observer),
            )
            .run(authority_rx, shutdown_rx),
        );

        activity_tx
            .send(Ok(LiveActivity::Batch(batch(0, Some(10)))))
            .unwrap();
        wait_until(|| source.status().ready).await;
        task.abort();
        let _ = task.await;
        assert!(!source.status().ready);
        assert_eq!(source.status().watermark, None);
        assert_eq!(source.status().indexed_blocks, 0);
        assert!(
            observer
                .0
                .lock()
                .unwrap()
                .contains(&CompanionIndexOwnerEvent::Shutdown)
        );
    }

    #[tokio::test]
    async fn real_zmq_owner_subscribes_before_replay_and_drains_buffered_live_tail() {
        const EMPTY_BATCH: &[u8] = &[
            0x93, 0xcb, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0x00,
        ];
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let source = source();
        let observer = Arc::new(RecordingObserver::default());
        let connector = Arc::new(ZmqCompanionKvEventConnector::new(KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(1),
            replay_timeout: Duration::from_secs(2),
            max_replay_batches: 16,
            max_replay_tail_batches: 4,
            wire_limits: crate::kv_wire::KvWireLimits::default(),
        }));
        let owner = CompanionIndexOwner::new(
            CompanionIndexOwnerConfig {
                replay_limit: 16,
                reconnect_min: Duration::from_millis(5),
                reconnect_max: Duration::from_millis(20),
            },
            Arc::clone(&source),
            connector,
            observer,
        );
        let (_authority_tx, authority_rx) = watch::channel(Some(incarnation("engine-a", 1)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let owner_task = tokio::spawn(owner.run(authority_rx, shutdown_rx));

        // Allow the SUB subscription command to cross the real TCP transport.
        tokio::time::sleep(Duration::from_millis(50)).await;
        publisher
            .send(zmq_message(vec![
                Bytes::from_static(b"kv"),
                Bytes::copy_from_slice(&4_u64.to_be_bytes()),
                Bytes::from_static(EMPTY_BATCH),
            ]))
            .await
            .unwrap();
        let first_request = timeout(Duration::from_secs(1), replay_server.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_request.get(2).unwrap().as_ref(), 0_u64.to_be_bytes());
        send_replay(
            &mut replay_server,
            first_request.get(0).unwrap().clone(),
            &[0, 2, 4],
        )
        .await;
        wait_until(|| source.status().ready && source.status().watermark == Some(4)).await;

        publisher
            .send(zmq_message(vec![
                Bytes::from_static(b"kv"),
                Bytes::copy_from_slice(&7_u64.to_be_bytes()),
                Bytes::from_static(EMPTY_BATCH),
            ]))
            .await
            .unwrap();
        let second_request = timeout(Duration::from_secs(1), replay_server.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_request.get(2).unwrap().as_ref(), 5_u64.to_be_bytes());
        send_replay(
            &mut replay_server,
            second_request.get(0).unwrap().clone(),
            &[5, 7],
        )
        .await;
        wait_until(|| source.status().watermark == Some(7)).await;

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_millis(500), owner_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
