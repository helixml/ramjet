//! Standalone, off-by-default single-engine snapshot companion composition.

use std::{
    env, fmt, fs,
    net::{IpAddr, SocketAddr},
    os::unix::fs::MetadataExt,
    path::Path,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::{Router, body::Body, http::Response, routing::get};
use prometheus::{CounterVec, Encoder, Gauge, HistogramVec, Opts, Registry, TextEncoder};
use thiserror::Error;
use tokio::{
    net::{TcpListener, UnixListener},
    sync::watch,
    task::{JoinError, JoinHandle},
    time::{Instant, timeout_at},
};

use crate::{
    companion_attestation::{
        CompanionAttestationError, load_authenticated_engine_incarnation,
        load_companion_digest_secret, watch_authenticated_engine_incarnation,
    },
    companion_config::{
        SnapshotCompanionConfig, SnapshotCompanionConfigError, SnapshotCompanionMode,
    },
    companion_index_owner::{
        CompanionIndexOwner, CompanionIndexOwnerConfig, CompanionIndexOwnerError,
        CompanionIndexOwnerEvent, CompanionIndexOwnerObserver, CompanionIndexOwnerRebuildReason,
        CompanionIndexOwnerReplayInvalidPhase, CompanionIndexOwnerReplayKind,
        CompanionIndexOwnerReplayOutcome, CompanionIndexOwnerReport, ZmqCompanionKvEventConnector,
    },
    companion_index_source::{
        CompanionIndexSource, CompanionIndexSourceConfig, CompanionIndexSourceError,
    },
    companion_runtime::{
        SnapshotCompanionRunReport, SnapshotCompanionRuntimeError, run_snapshot_companion,
    },
    digest_index::DigestIndexLimits,
    kv_snapshot::{AttentionKind, GroupDisposition, GroupMetadata, SnapshotLimits},
    kv_transport::KvTransportConfig,
    kv_wire::KvWireLimits,
    snapshot_producer::SnapshotProducerSource,
    snapshot_secret_file::{SnapshotSecretFileError, SnapshotSecretFilePolicy},
    snapshot_socket_path::{
        PublishedSocketPath, SnapshotSocketPathError, SocketParentPolicy, bind_and_publish,
        validate_socket_parent,
    },
    snapshot_supervisor::MAX_ACTIVE_SNAPSHOT_CLIENTS,
};

const MAX_PATH_BYTES: usize = 4_096;
const MAX_METRICS_SOCKET_PATH_BYTES: usize = 64;
const MAX_REPLAY_BATCHES: usize = 100_000;
const MAX_BLOCK_SIZE: usize = 1_048_576;
const SETGID_BIT: u32 = 0o2_000;
const GROUP_EXECUTE_BIT: u32 = 0o010;

#[derive(Clone, Eq, PartialEq)]
enum CompanionMetricsEndpoint {
    Loopback(SocketAddr),
    Unix { path: PathBuf, group_gid: u32 },
}

impl fmt::Debug for CompanionMetricsEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Loopback(_) => "Loopback",
            Self::Unix { .. } => "Unix",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigAttentionKind {
    Full,
    Mla,
    SinkFull,
}

impl ConfigAttentionKind {
    const fn wire(self) -> AttentionKind {
        match self {
            Self::Full => AttentionKind::FullAttention,
            Self::Mla => AttentionKind::MlaAttention,
            Self::SinkFull => AttentionKind::SinkFullAttention,
        }
    }
}

#[derive(Clone)]
pub struct SingleEngineCompanionConfig {
    pub snapshot: SnapshotCompanionConfig,
    digest_secret_path: Option<PathBuf>,
    attestation_path: Option<PathBuf>,
    metrics_endpoint: CompanionMetricsEndpoint,
    attestation_refresh: Duration,
    transport: Option<KvTransportConfig>,
    owner: CompanionIndexOwnerConfig,
    group: Option<GroupMetadata>,
}

impl fmt::Debug for SingleEngineCompanionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SingleEngineCompanionConfig")
            .field("snapshot", &self.snapshot)
            .field(
                "digest_secret_path",
                &self.digest_secret_path.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "attestation_path",
                &self.attestation_path.as_ref().map(|_| "[REDACTED]"),
            )
            .field("metrics_endpoint", &self.metrics_endpoint)
            .field("attestation_refresh", &self.attestation_refresh)
            .field("transport_configured", &self.transport.is_some())
            .field("owner", &self.owner)
            .field("group_configured", &self.group.is_some())
            .finish()
    }
}

impl SingleEngineCompanionConfig {
    /// Load the standalone companion environment contract.
    ///
    /// # Errors
    ///
    /// Returns content-free typed configuration errors. Off mode does not read
    /// or require any companion executable setting.
    pub fn from_env() -> Result<Self, SingleEngineCompanionConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    #[allow(clippy::too_many_lines)]
    /// Deterministic lookup-based constructor for tests and embedders.
    ///
    /// # Errors
    ///
    /// Rejects invalid base settings, multi-engine sources, missing protected
    /// paths/geometry, non-loopback metrics, and out-of-bound runtime values.
    pub fn from_lookup(
        mut get: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, SingleEngineCompanionConfigError> {
        let snapshot = SnapshotCompanionConfig::from_lookup(&mut get)?;
        if snapshot.mode == SnapshotCompanionMode::Off {
            return Ok(Self {
                snapshot,
                digest_secret_path: None,
                attestation_path: None,
                metrics_endpoint: CompanionMetricsEndpoint::Loopback(parse_loopback(
                    "127.0.0.1:9091",
                )?),
                attestation_refresh: Duration::from_secs(1),
                transport: None,
                owner: CompanionIndexOwnerConfig {
                    replay_limit: 10_000,
                    reconnect_min: Duration::from_millis(250),
                    reconnect_max: Duration::from_secs(5),
                },
                group: None,
            });
        }
        let metrics_endpoint = metrics_endpoint(&mut get, &snapshot)?;
        let attestation_refresh = parse_duration(
            &mut get,
            "RJ_SNAPSHOT_ATTESTATION_REFRESH_MS",
            1_000,
            50,
            60_000,
        )?;
        let owner = CompanionIndexOwnerConfig {
            replay_limit: u64::try_from(parse_usize(
                &mut get,
                "RJ_SNAPSHOT_REPLAY_LIMIT",
                10_000,
                1,
                MAX_REPLAY_BATCHES,
            )?)
            .map_err(|_| invalid("RJ_SNAPSHOT_REPLAY_LIMIT", "a bounded batch count"))?,
            reconnect_min: parse_duration(
                &mut get,
                "RJ_SNAPSHOT_RECONNECT_MIN_MS",
                250,
                1,
                60_000,
            )?,
            reconnect_max: parse_duration(
                &mut get,
                "RJ_SNAPSHOT_RECONNECT_MAX_MS",
                5_000,
                1,
                60_000,
            )?,
        };
        if owner.reconnect_min > owner.reconnect_max {
            return Err(invalid(
                "RJ_SNAPSHOT_RECONNECT_MAX_MS",
                "at least the reconnect minimum",
            ));
        }
        if snapshot.sources.len() != 1 {
            return Err(invalid(
                "RJ_SNAPSHOT_LIVE_ENDPOINTS",
                "exactly one live/replay engine pair",
            ));
        }
        if snapshot.max_clients != MAX_ACTIVE_SNAPSHOT_CLIENTS {
            return Err(invalid(
                "RJ_SNAPSHOT_MAX_CLIENTS",
                "exactly two active clients",
            ));
        }
        let digest_secret_path = required_path(&mut get, "RJ_SNAPSHOT_DIGEST_SECRET_PATH")?;
        let attestation_path = required_path(&mut get, "RJ_SNAPSHOT_ATTESTATION_PATH")?;
        let session_path = snapshot
            .secret_path
            .as_ref()
            .ok_or_else(|| invalid("RJ_SNAPSHOT_SECRET_PATH", "a session secret path"))?;
        if &digest_secret_path == session_path
            || attestation_path == digest_secret_path
            || &attestation_path == session_path
        {
            return Err(invalid(
                "RJ_SNAPSHOT_ATTESTATION_PATH",
                "three distinct protected file paths",
            ));
        }
        let block_size = parse_usize(&mut get, "RJ_SNAPSHOT_BLOCK_SIZE", 0, 1, MAX_BLOCK_SIZE)?;
        let attention = match get("RJ_SNAPSHOT_ATTENTION_KIND")
            .as_deref()
            .unwrap_or("mla")
        {
            "full" => ConfigAttentionKind::Full,
            "mla" => ConfigAttentionKind::Mla,
            "sink_full" => ConfigAttentionKind::SinkFull,
            _ => {
                return Err(invalid(
                    "RJ_SNAPSHOT_ATTENTION_KIND",
                    "full, mla, or sink_full",
                ));
            }
        };
        let source = &snapshot.sources[0];
        let transport = KvTransportConfig {
            live_endpoint: source.live_endpoint.clone(),
            replay_endpoint: Some(source.replay_endpoint.clone()),
            topic: snapshot.event_topic.clone(),
            connect_timeout: parse_duration(
                &mut get,
                "RJ_SNAPSHOT_CONNECT_TIMEOUT_MS",
                2_000,
                1,
                60_000,
            )?,
            replay_timeout: parse_duration(
                &mut get,
                "RJ_SNAPSHOT_REPLAY_TIMEOUT_MS",
                30_000,
                1,
                900_000,
            )?,
            max_replay_batches: usize::try_from(owner.replay_limit)
                .map_err(|_| invalid("RJ_SNAPSHOT_REPLAY_LIMIT", "a bounded batch count"))?,
            max_replay_tail_batches: parse_usize(
                &mut get,
                "RJ_SNAPSHOT_REPLAY_TAIL_LIMIT",
                1_024,
                0,
                MAX_REPLAY_BATCHES,
            )?,
            wire_limits: KvWireLimits {
                max_payload_bytes: snapshot.max_batch_payload_bytes,
                max_events: snapshot.max_batch_events,
                ..KvWireLimits::default()
            },
        };
        let data_parallel_rank = parse_u32(&mut get, "RJ_SNAPSHOT_DATA_PARALLEL_RANK", 0)?;
        let group_idx = parse_u32(&mut get, "RJ_SNAPSHOT_GROUP_INDEX", 0)?;
        Ok(Self {
            snapshot,
            digest_secret_path: Some(digest_secret_path),
            attestation_path: Some(attestation_path),
            metrics_endpoint,
            attestation_refresh,
            transport: Some(transport),
            owner,
            group: Some(GroupMetadata {
                data_parallel_rank,
                group_idx,
                attention_kind: attention.wire(),
                disposition: GroupDisposition::Indexed,
                block_size: u32::try_from(block_size)
                    .map_err(|_| invalid("RJ_SNAPSHOT_BLOCK_SIZE", "a bounded block size"))?,
            }),
        })
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.snapshot.mode == SnapshotCompanionMode::Serve
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SingleEngineCompanionConfigError {
    #[error("snapshot companion base configuration failed")]
    Base(#[from] SnapshotCompanionConfigError),
    #[error("invalid standalone companion setting {key}: expected {reason}")]
    Invalid {
        key: &'static str,
        reason: &'static str,
    },
    #[error("missing standalone companion setting {key}: expected {reason}")]
    Missing {
        key: &'static str,
        reason: &'static str,
    },
}

impl SingleEngineCompanionConfigError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Base(_) => "base_config",
            Self::Invalid { .. } => "invalid_setting",
            Self::Missing { .. } => "missing_setting",
        }
    }
}

#[derive(Debug, Error)]
pub enum SingleEngineCompanionError {
    #[error("standalone companion session secret validation failed")]
    SessionSecret(#[from] SnapshotSecretFileError),
    #[error("standalone companion protected input failed")]
    Attestation(#[from] CompanionAttestationError),
    #[error("standalone companion source initialization failed")]
    Source(#[from] CompanionIndexSourceError),
    #[error("standalone companion owner failed")]
    Owner(#[from] CompanionIndexOwnerError),
    #[error("standalone companion snapshot server failed")]
    Runtime(#[from] SnapshotCompanionRuntimeError),
    #[error("standalone companion metric registration failed")]
    Metrics(#[from] prometheus::Error),
    #[error("standalone companion metrics listener failed")]
    MetricsIo,
    #[error("standalone companion metrics socket validation failed")]
    MetricsSocket(#[from] SnapshotSocketPathError),
    #[error("standalone companion metrics authority isolation failed")]
    MetricsIsolation,
    #[error("standalone companion configuration is incomplete")]
    InvalidConfig,
    #[error("standalone companion task failed")]
    TaskFailed,
    #[error("standalone companion shutdown timed out")]
    ShutdownTimeout,
}

impl SingleEngineCompanionError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::SessionSecret(error) => error.reason(),
            Self::Attestation(error) => error.reason(),
            Self::Source(_) => "source",
            Self::Owner(error) => error.reason(),
            Self::Runtime(error) => error.reason(),
            Self::Metrics(_) => "metrics",
            Self::MetricsIo => "metrics_io",
            Self::MetricsSocket(error) => error.reason(),
            Self::MetricsIsolation => "metrics_isolation",
            Self::InvalidConfig => "invalid_config",
            Self::TaskFailed => "task_failed",
            Self::ShutdownTimeout => "shutdown_timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SingleEngineCompanionReport {
    Off,
    Stopped {
        owner: CompanionIndexOwnerReport,
        snapshot: SnapshotCompanionRunReport,
    },
}

enum ServiceExit {
    Shutdown,
    Owner(Result<Result<CompanionIndexOwnerReport, CompanionIndexOwnerError>, JoinError>),
    Snapshot(Result<Result<SnapshotCompanionRunReport, SingleEngineCompanionError>, JoinError>),
    Metrics(Result<Result<(), SingleEngineCompanionError>, JoinError>),
    Watcher(Result<(), JoinError>),
}

enum BoundMetricsEndpoint {
    Tcp(TcpListener),
    Unix {
        listener: UnixListener,
        path_guard: PublishedSocketPath,
    },
}

/// Run the isolated single-engine companion until shutdown.
///
/// # Errors
///
/// Fails before socket publication for unsafe secrets, unauthenticated
/// incarnation input, invalid source geometry, or metrics bind failure.
#[allow(clippy::too_many_lines)]
pub async fn run_single_engine_companion(
    config: SingleEngineCompanionConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<SingleEngineCompanionReport, SingleEngineCompanionError> {
    if !config.enabled() {
        return Ok(SingleEngineCompanionReport::Off);
    }
    let digest_path = config
        .digest_secret_path
        .as_deref()
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    let attestation_path = config
        .attestation_path
        .as_deref()
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    let transport = config
        .transport
        .clone()
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    let group = config
        .group
        .clone()
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    let policy = SnapshotSecretFilePolicy {
        expected_owner_uid: config.snapshot.secret_owner_uid,
    };
    // Preflight every protected input before binding metrics, connecting to the
    // engine, or publishing the UDS. The snapshot runtime reopens this file at
    // publication time, closing the validation/use race rather than trusting
    // this first check.
    let session_path = config
        .snapshot
        .secret_path
        .as_deref()
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    drop(crate::snapshot_secret_file::load_snapshot_session_secret(
        session_path,
        policy,
    )?);
    let digest_secret = load_companion_digest_secret(digest_path, policy)?;
    let initial = load_authenticated_engine_incarnation(attestation_path, policy, &digest_secret)?;
    let source = Arc::new(CompanionIndexSource::new(
        CompanionIndexSourceConfig {
            group,
            index_limits: DigestIndexLimits::default(),
            snapshot_limits: SnapshotLimits {
                max_frame_bytes: config.snapshot.max_snapshot_frame_bytes,
                max_payload_bytes: config.snapshot.max_snapshot_frame_bytes,
                ..SnapshotLimits::default()
            },
            max_active_sessions: config.snapshot.max_clients,
        },
        initial.clone(),
        1,
        digest_secret.as_bytes(),
    )?);

    let registry = Arc::new(Registry::new());
    let observer = Arc::new(OwnerObserver::new(&registry)?);
    let connector = Arc::new(ZmqCompanionKvEventConnector::new(transport));
    let owner = CompanionIndexOwner::new(config.owner, Arc::clone(&source), connector, observer);
    let (authority_tx, authority_rx) = watch::channel(Some(initial));
    let (internal_tx, internal_rx) = watch::channel(false);

    let snapshot_socket_path = config
        .snapshot
        .socket_path
        .as_deref()
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    let companion_uid = config
        .snapshot
        .companion_uid
        .ok_or(SingleEngineCompanionError::InvalidConfig)?;
    let metrics_listener = bind_metrics_endpoint(
        &config.metrics_endpoint,
        snapshot_socket_path,
        companion_uid,
    )
    .await?;
    let metrics_registry = Arc::clone(&registry);
    let metrics_shutdown = internal_rx.clone();
    let mut metrics_task = tokio::spawn(async move {
        serve_metrics_endpoint(metrics_listener, metrics_registry, metrics_shutdown).await
    });
    let mut owner_task = tokio::spawn(owner.run(authority_rx, internal_rx.clone()));
    let producer_source: Arc<dyn SnapshotProducerSource> = source;
    let snapshot_config = config.snapshot.clone();
    let runtime_registry = Arc::clone(&registry);
    let mut snapshot_task = tokio::spawn(async move {
        run_snapshot_companion(
            snapshot_config,
            &runtime_registry,
            Some(producer_source),
            internal_rx,
        )
        .await
        .map_err(SingleEngineCompanionError::Runtime)
    });
    let watcher_path = attestation_path.to_path_buf();
    let watcher_shutdown = internal_tx.subscribe();
    let refresh = config.attestation_refresh;
    let mut watcher_task = tokio::spawn(async move {
        watch_authenticated_engine_incarnation(
            &watcher_path,
            policy,
            digest_secret,
            refresh,
            &authority_tx,
            watcher_shutdown,
        )
        .await;
    });

    let exit = tokio::select! {
        biased;
        () = wait_for_shutdown(&mut shutdown) => ServiceExit::Shutdown,
        result = &mut owner_task => ServiceExit::Owner(result),
        result = &mut snapshot_task => ServiceExit::Snapshot(result),
        result = &mut metrics_task => ServiceExit::Metrics(result),
        result = &mut watcher_task => ServiceExit::Watcher(result),
    };
    let _ = internal_tx.send(true);
    let deadline = Instant::now() + config.snapshot.shutdown_deadline;

    match exit {
        ServiceExit::Shutdown => {
            let (owner, snapshot, metrics, watcher) = tokio::join!(
                await_task(&mut owner_task, deadline),
                await_task(&mut snapshot_task, deadline),
                await_task(&mut metrics_task, deadline),
                await_task(&mut watcher_task, deadline),
            );
            let owner = owner??;
            let snapshot = snapshot??;
            metrics??;
            watcher?;
            Ok(SingleEngineCompanionReport::Stopped { owner, snapshot })
        }
        ServiceExit::Owner(result) => {
            finish_remaining(
                &mut snapshot_task,
                &mut metrics_task,
                &mut watcher_task,
                deadline,
            )
            .await?;
            Err(result
                .map_err(|_| SingleEngineCompanionError::TaskFailed)?
                .err()
                .map_or(SingleEngineCompanionError::InvalidConfig, Into::into))
        }
        ServiceExit::Snapshot(result) => {
            finish_owner_and_remaining(
                &mut owner_task,
                &mut metrics_task,
                &mut watcher_task,
                deadline,
            )
            .await?;
            Err(result
                .map_err(|_| SingleEngineCompanionError::TaskFailed)?
                .err()
                .unwrap_or(SingleEngineCompanionError::InvalidConfig))
        }
        ServiceExit::Metrics(result) => {
            finish_owner_and_snapshot(
                &mut owner_task,
                &mut snapshot_task,
                &mut watcher_task,
                deadline,
            )
            .await?;
            Err(result
                .map_err(|_| SingleEngineCompanionError::TaskFailed)?
                .err()
                .unwrap_or(SingleEngineCompanionError::MetricsIo))
        }
        ServiceExit::Watcher(result) => {
            finish_owner_and_snapshot(
                &mut owner_task,
                &mut snapshot_task,
                &mut metrics_task,
                deadline,
            )
            .await?;
            result.map_err(|_| SingleEngineCompanionError::TaskFailed)?;
            Err(SingleEngineCompanionError::InvalidConfig)
        }
    }
}

struct OwnerObserver {
    events: CounterVec,
    authority: Gauge,
    replay_duration: HistogramVec,
}

impl OwnerObserver {
    fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let observer = Self {
            events: CounterVec::new(
                Opts::new(
                    "ds4proxy_snapshot_companion_owner_events_total",
                    "Companion owner transitions by bounded event and reason",
                ),
                &["event", "reason"],
            )?,
            authority: Gauge::with_opts(Opts::new(
                "ds4proxy_snapshot_companion_authority",
                "Whether authenticated engine-incarnation authority is available",
            ))?,
            replay_duration: HistogramVec::new(
                prometheus::HistogramOpts::new(
                    "ds4proxy_snapshot_companion_owner_replay_duration_seconds",
                    "Transport replay duration by bounded kind and outcome",
                ),
                &["kind", "outcome"],
            )?,
        };
        registry.register(Box::new(observer.events.clone()))?;
        registry.register(Box::new(observer.authority.clone()))?;
        registry.register(Box::new(observer.replay_duration.clone()))?;
        Ok(observer)
    }
}

impl CompanionIndexOwnerObserver for OwnerObserver {
    fn observe(&self, event: CompanionIndexOwnerEvent) {
        let (name, reason) = owner_event_labels(&event);
        self.events.with_label_values(&[name, reason]).inc();
        match &event {
            CompanionIndexOwnerEvent::AuthorityAvailable => self.authority.set(1.0),
            CompanionIndexOwnerEvent::AuthorityLost | CompanionIndexOwnerEvent::Shutdown => {
                self.authority.set(0.0);
            }
            CompanionIndexOwnerEvent::Replay {
                kind,
                outcome,
                profile: Some(profile),
            } => self
                .replay_duration
                .with_label_values(&[replay_kind_label(*kind), replay_outcome_label(*outcome)])
                .observe(profile.elapsed.as_secs_f64()),
            _ => {}
        }
        tracing::debug!(event = name, reason, "snapshot companion owner transition");
    }
}

fn owner_event_labels(event: &CompanionIndexOwnerEvent) -> (&'static str, &'static str) {
    match event {
        CompanionIndexOwnerEvent::AuthorityAvailable => ("authority", "available"),
        CompanionIndexOwnerEvent::AuthorityLost => ("authority", "lost"),
        CompanionIndexOwnerEvent::ConnectAttempt => ("connect", "attempt"),
        CompanionIndexOwnerEvent::Connected => ("connect", "connected"),
        CompanionIndexOwnerEvent::Rebuild(reason) => ("rebuild", rebuild_label(*reason)),
        CompanionIndexOwnerEvent::Replay { outcome, .. } => {
            ("replay", replay_outcome_label(*outcome))
        }
        CompanionIndexOwnerEvent::ReplayInvalid(phase) => {
            ("replay_invalid", replay_invalid_phase_label(*phase))
        }
        CompanionIndexOwnerEvent::Ready => ("source", "ready"),
        CompanionIndexOwnerEvent::LiveApplied => ("live", "applied"),
        CompanionIndexOwnerEvent::LiveDuplicate => ("live", "duplicate"),
        CompanionIndexOwnerEvent::Shutdown => ("owner", "shutdown"),
    }
}

const fn rebuild_label(reason: CompanionIndexOwnerRebuildReason) -> &'static str {
    match reason {
        CompanionIndexOwnerRebuildReason::Startup => "startup",
        CompanionIndexOwnerRebuildReason::AuthorityChanged => "authority_changed",
        CompanionIndexOwnerRebuildReason::AuthorityLost => "authority_lost",
        CompanionIndexOwnerRebuildReason::Disconnected => "disconnected",
        CompanionIndexOwnerRebuildReason::Transport => "transport",
        CompanionIndexOwnerRebuildReason::Replay => "replay",
        CompanionIndexOwnerRebuildReason::ReplayInvalid => "replay_invalid",
        CompanionIndexOwnerRebuildReason::ReplayTooLarge => "replay_too_large",
        CompanionIndexOwnerRebuildReason::Apply => "apply",
    }
}

const fn replay_kind_label(kind: CompanionIndexOwnerReplayKind) -> &'static str {
    match kind {
        CompanionIndexOwnerReplayKind::Full => "full",
    }
}

const fn replay_outcome_label(outcome: CompanionIndexOwnerReplayOutcome) -> &'static str {
    match outcome {
        CompanionIndexOwnerReplayOutcome::Complete => "complete",
        CompanionIndexOwnerReplayOutcome::TransportFailed => "transport_failed",
        CompanionIndexOwnerReplayOutcome::Invalid => "invalid",
        CompanionIndexOwnerReplayOutcome::Cancelled => "cancelled",
    }
}

const fn replay_invalid_phase_label(phase: CompanionIndexOwnerReplayInvalidPhase) -> &'static str {
    match phase {
        CompanionIndexOwnerReplayInvalidPhase::Apply => "apply",
        CompanionIndexOwnerReplayInvalidPhase::Boundary => "boundary",
        CompanionIndexOwnerReplayInvalidPhase::Tail => "tail",
        CompanionIndexOwnerReplayInvalidPhase::Commit => "commit",
    }
}

async fn bind_metrics_endpoint(
    endpoint: &CompanionMetricsEndpoint,
    snapshot_socket_path: &Path,
    companion_uid: u32,
) -> Result<BoundMetricsEndpoint, SingleEngineCompanionError> {
    match endpoint {
        CompanionMetricsEndpoint::Loopback(address) => TcpListener::bind(address)
            .await
            .map(BoundMetricsEndpoint::Tcp)
            .map_err(|_| SingleEngineCompanionError::MetricsIo),
        CompanionMetricsEndpoint::Unix { path, group_gid } => {
            let snapshot_parent = snapshot_socket_path
                .parent()
                .ok_or(SingleEngineCompanionError::MetricsIsolation)?;
            let metrics_parent = path
                .parent()
                .ok_or(SingleEngineCompanionError::MetricsIsolation)?;
            let policy = SocketParentPolicy {
                owner_uid: companion_uid,
            };
            validate_socket_parent(snapshot_parent, policy)?;
            validate_socket_parent(metrics_parent, policy)?;

            let snapshot_metadata = fs::symlink_metadata(snapshot_parent)
                .map_err(|_| SingleEngineCompanionError::MetricsIsolation)?;
            let metrics_metadata = fs::symlink_metadata(metrics_parent)
                .map_err(|_| SingleEngineCompanionError::MetricsIsolation)?;
            let same_directory = snapshot_metadata.dev() == metrics_metadata.dev()
                && snapshot_metadata.ino() == metrics_metadata.ino();
            let snapshot_mode = snapshot_metadata.mode();
            let metrics_mode = metrics_metadata.mode();
            if same_directory
                || *group_gid == 0
                || snapshot_metadata.gid() == *group_gid
                || metrics_metadata.gid() != *group_gid
                || snapshot_mode & SETGID_BIT == 0
                || snapshot_mode & GROUP_EXECUTE_BIT == 0
                || metrics_mode & SETGID_BIT == 0
                || metrics_mode & GROUP_EXECUTE_BIT == 0
            {
                return Err(SingleEngineCompanionError::MetricsIsolation);
            }

            let published = bind_and_publish(path, policy)?;
            let socket_metadata = fs::symlink_metadata(path)
                .map_err(|_| SingleEngineCompanionError::MetricsIsolation)?;
            if socket_metadata.gid() != *group_gid {
                return Err(SingleEngineCompanionError::MetricsIsolation);
            }
            let (listener, path_guard) = published.into_parts();
            listener
                .set_nonblocking(true)
                .map_err(|_| SingleEngineCompanionError::MetricsIo)?;
            let listener = UnixListener::from_std(listener)
                .map_err(|_| SingleEngineCompanionError::MetricsIo)?;
            Ok(BoundMetricsEndpoint::Unix {
                listener,
                path_guard,
            })
        }
    }
}

async fn serve_metrics_endpoint(
    endpoint: BoundMetricsEndpoint,
    registry: Arc<Registry>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), SingleEngineCompanionError> {
    let router = Router::new().route(
        "/metrics",
        get(move || {
            let registry = Arc::clone(&registry);
            async move { metrics_response(&registry) }
        }),
    );
    match endpoint {
        BoundMetricsEndpoint::Tcp(listener) => axum::serve(listener, router)
            .with_graceful_shutdown(async move { wait_for_shutdown(&mut shutdown).await })
            .await
            .map_err(|_| SingleEngineCompanionError::MetricsIo),
        BoundMetricsEndpoint::Unix {
            listener,
            mut path_guard,
        } => {
            let serve_result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { wait_for_shutdown(&mut shutdown).await })
                .await
                .map_err(|_| SingleEngineCompanionError::MetricsIo);
            let cleanup_result = path_guard
                .cleanup()
                .map_err(|_| SingleEngineCompanionError::MetricsIo);
            serve_result.and(cleanup_result.map(|_| ()))
        }
    }
}

fn metrics_response(registry: &Registry) -> Response<Body> {
    let mut output = Vec::new();
    let encoder = TextEncoder::new();
    if encoder.encode(&registry.gather(), &mut output).is_err() {
        return Response::builder()
            .status(500)
            .body(Body::from("metrics encoding failed"))
            .expect("valid response");
    }
    Response::builder()
        .status(200)
        .header("content-type", encoder.format_type())
        .body(Body::from(output))
        .expect("valid response")
}

async fn await_task<T>(
    task: &mut JoinHandle<T>,
    deadline: Instant,
) -> Result<T, SingleEngineCompanionError> {
    if let Ok(result) = timeout_at(deadline, &mut *task).await {
        result.map_err(|_| SingleEngineCompanionError::TaskFailed)
    } else {
        task.abort();
        let _ = task.await;
        Err(SingleEngineCompanionError::ShutdownTimeout)
    }
}

async fn finish_remaining<A, B, C>(
    first: &mut JoinHandle<A>,
    second: &mut JoinHandle<B>,
    third: &mut JoinHandle<C>,
    deadline: Instant,
) -> Result<(), SingleEngineCompanionError> {
    let (first, second, third) = tokio::join!(
        await_task(first, deadline),
        await_task(second, deadline),
        await_task(third, deadline),
    );
    let _ = first?;
    let _ = second?;
    let _ = third?;
    Ok(())
}

async fn finish_owner_and_remaining<A, B, C>(
    first: &mut JoinHandle<A>,
    second: &mut JoinHandle<B>,
    third: &mut JoinHandle<C>,
    deadline: Instant,
) -> Result<(), SingleEngineCompanionError> {
    finish_remaining(first, second, third, deadline).await
}

async fn finish_owner_and_snapshot<A, B, C>(
    first: &mut JoinHandle<A>,
    second: &mut JoinHandle<B>,
    third: &mut JoinHandle<C>,
    deadline: Instant,
) -> Result<(), SingleEngineCompanionError> {
    finish_remaining(first, second, third, deadline).await
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn required_path(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<PathBuf, SingleEngineCompanionConfigError> {
    let raw = get(key).filter(|value| !value.is_empty()).ok_or(
        SingleEngineCompanionConfigError::Missing {
            key,
            reason: "an absolute normalized protected file path",
        },
    )?;
    let path = PathBuf::from(&raw);
    if raw.len() > MAX_PATH_BYTES
        || !path.is_absolute()
        || path.file_name().is_none()
        || raw.ends_with('/')
        || raw[1..]
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid(key, "an absolute normalized protected file path"));
    }
    Ok(path)
}

fn metrics_endpoint(
    get: &mut impl FnMut(&str) -> Option<String>,
    snapshot: &SnapshotCompanionConfig,
) -> Result<CompanionMetricsEndpoint, SingleEngineCompanionConfigError> {
    let tcp = get("RJ_SNAPSHOT_METRICS_BIND");
    let unix = get("RJ_SNAPSHOT_METRICS_SOCKET_PATH");
    let group_gid = get("RJ_SNAPSHOT_METRICS_GROUP_GID");
    if tcp.is_some() && unix.is_some() {
        return Err(invalid(
            "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
            "exactly one TCP or Unix metrics endpoint",
        ));
    }
    if let Some(raw) = unix {
        let path = normalized_path(
            &raw,
            "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
            MAX_METRICS_SOCKET_PATH_BYTES,
            "an absolute normalized Unix socket path",
        )?;
        let snapshot_path = snapshot
            .socket_path
            .as_deref()
            .ok_or_else(|| invalid("RJ_SNAPSHOT_SOCKET_PATH", "a snapshot socket path"))?;
        if path.parent() == snapshot_path.parent() {
            return Err(invalid(
                "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
                "a parent distinct from the snapshot authority directory",
            ));
        }
        let group_gid = group_gid
            .as_deref()
            .ok_or_else(|| {
                invalid(
                    "RJ_SNAPSHOT_METRICS_GROUP_GID",
                    "a dedicated non-root metrics group",
                )
            })?
            .parse::<u32>()
            .map_err(|_| {
                invalid(
                    "RJ_SNAPSHOT_METRICS_GROUP_GID",
                    "a dedicated non-root metrics group",
                )
            })?;
        if group_gid == 0 {
            return Err(invalid(
                "RJ_SNAPSHOT_METRICS_GROUP_GID",
                "a dedicated non-root metrics group",
            ));
        }
        Ok(CompanionMetricsEndpoint::Unix { path, group_gid })
    } else {
        if group_gid.is_some() {
            return Err(invalid(
                "RJ_SNAPSHOT_METRICS_GROUP_GID",
                "only with a Unix metrics endpoint",
            ));
        }
        Ok(CompanionMetricsEndpoint::Loopback(parse_loopback(
            tcp.as_deref().unwrap_or("127.0.0.1:9091"),
        )?))
    }
}

fn normalized_path(
    raw: &str,
    key: &'static str,
    maximum_bytes: usize,
    reason: &'static str,
) -> Result<PathBuf, SingleEngineCompanionConfigError> {
    let path = PathBuf::from(raw);
    if raw.is_empty()
        || raw.len() > maximum_bytes
        || !path.is_absolute()
        || path.file_name().is_none()
        || raw.ends_with('/')
        || raw[1..]
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid(key, reason));
    }
    Ok(path)
}

fn parse_loopback(raw: &str) -> Result<SocketAddr, SingleEngineCompanionConfigError> {
    let address = raw
        .parse::<SocketAddr>()
        .map_err(|_| invalid("RJ_SNAPSHOT_METRICS_BIND", "a loopback IP socket address"))?;
    if !matches!(address.ip(), IpAddr::V4(_) | IpAddr::V6(_)) || !address.ip().is_loopback() {
        return Err(invalid(
            "RJ_SNAPSHOT_METRICS_BIND",
            "a loopback IP socket address",
        ));
    }
    Ok(address)
}

fn parse_duration(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: u64,
    minimum: u64,
    maximum: u64,
) -> Result<Duration, SingleEngineCompanionConfigError> {
    let value = get(key)
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| invalid(key, "bounded milliseconds"))?
        .unwrap_or(fallback);
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(key, "bounded milliseconds"));
    }
    Ok(Duration::from_millis(value))
}

fn parse_usize(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, SingleEngineCompanionConfigError> {
    let value = get(key)
        .as_deref()
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| invalid(key, "a bounded integer"))?
        .unwrap_or(fallback);
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(key, "a bounded integer"));
    }
    Ok(value)
}

fn parse_u32(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: u32,
) -> Result<u32, SingleEngineCompanionConfigError> {
    get(key)
        .as_deref()
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|_| invalid(key, "a non-negative 32-bit integer"))
        .map(|value| value.unwrap_or(fallback))
}

const fn invalid(key: &'static str, reason: &'static str) -> SingleEngineCompanionConfigError {
    SingleEngineCompanionConfigError::Invalid { key, reason }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, chown},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        companion_attestation::{
            encode_authenticated_engine_incarnation, load_companion_digest_secret,
        },
        kv_snapshot::EngineIncarnation,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::timeout,
    };

    static TEST_ID: AtomicU64 = AtomicU64::new(1);
    // Drone runs this suite as root. Keep its protected files root-owned while
    // exercising the production contract with distinct non-root process UIDs.
    const ROOT_CONTAINER_COMPANION_UID: u32 = 12_001;
    const ROOT_CONTAINER_CLIENT_UID: u32 = 12_002;

    struct TestFiles {
        directory: PathBuf,
        socket: PathBuf,
        session: PathBuf,
        digest: PathBuf,
        attestation: PathBuf,
        owner: u32,
        companion_uid: u32,
        client_uid: u32,
    }

    impl TestFiles {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            // The production UDS contract deliberately caps paths at 64 bytes.
            // Do not inherit a potentially long Cargo `TMPDIR` in this test.
            let directory =
                PathBuf::from("/tmp").join(format!("md-svc-{}-{id}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let owner = fs::metadata(&directory).unwrap().uid();
            let (companion_uid, client_uid) = if owner == 0 {
                (ROOT_CONTAINER_COMPANION_UID, ROOT_CONTAINER_CLIENT_UID)
            } else {
                (owner, owner.saturating_add(1).max(1))
            };
            let session = directory.join("session");
            let digest = directory.join("digest");
            for (path, byte) in [(&session, 0x31), (&digest, 0x51)] {
                fs::write(path, [byte; 32]).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let attestation = directory.join("attestation");
            let secret = load_companion_digest_secret(
                &digest,
                SnapshotSecretFilePolicy {
                    expected_owner_uid: owner,
                },
            )
            .unwrap();
            let encoded = encode_authenticated_engine_incarnation(&incarnation(), &secret).unwrap();
            fs::write(&attestation, encoded).unwrap();
            fs::set_permissions(&attestation, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                socket: directory.join("companion.sock"),
                directory,
                session,
                digest,
                attestation,
                owner,
                companion_uid,
                client_uid,
            }
        }

        fn values(&self) -> HashMap<&'static str, String> {
            HashMap::from([
                ("RJ_SNAPSHOT_COMPANION_MODE", "serve".to_owned()),
                (
                    "RJ_SNAPSHOT_SOCKET_PATH",
                    self.socket.to_string_lossy().into_owned(),
                ),
                ("RJ_SNAPSHOT_COMPANION_UID", self.companion_uid.to_string()),
                ("RJ_SNAPSHOT_CLIENT_UID", self.client_uid.to_string()),
                (
                    "RJ_SNAPSHOT_SECRET_PATH",
                    self.session.to_string_lossy().into_owned(),
                ),
                ("RJ_SNAPSHOT_SECRET_OWNER_UID", self.owner.to_string()),
                (
                    "RJ_SNAPSHOT_LIVE_ENDPOINTS",
                    "tcp://127.0.0.1:45171".to_owned(),
                ),
                (
                    "RJ_SNAPSHOT_REPLAY_ENDPOINTS",
                    "tcp://127.0.0.1:45172".to_owned(),
                ),
                (
                    "RJ_SNAPSHOT_DIGEST_SECRET_PATH",
                    self.digest.to_string_lossy().into_owned(),
                ),
                (
                    "RJ_SNAPSHOT_ATTESTATION_PATH",
                    self.attestation.to_string_lossy().into_owned(),
                ),
                ("RJ_SNAPSHOT_BLOCK_SIZE", "64".to_owned()),
                ("RJ_SNAPSHOT_METRICS_BIND", "127.0.0.1:0".to_owned()),
                ("RJ_SNAPSHOT_RECONNECT_MIN_MS", "10".to_owned()),
                ("RJ_SNAPSHOT_RECONNECT_MAX_MS", "20".to_owned()),
                ("RJ_SNAPSHOT_ATTESTATION_REFRESH_MS", "50".to_owned()),
            ])
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn incarnation() -> EngineIncarnation {
        EngineIncarnation {
            engine_id: "engine-a".to_owned(),
            model_revision: "revision".to_owned(),
            image_digest: "sha256:image".to_owned(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![7; 32],
        }
    }

    fn load(values: &HashMap<&str, String>) -> SingleEngineCompanionConfig {
        SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).unwrap()
    }

    #[test]
    fn off_is_the_default_and_requires_no_files_or_engine() {
        let config = SingleEngineCompanionConfig::from_lookup(|key| match key {
            // Standalone-only settings are not parsed while disabled.
            "RJ_SNAPSHOT_METRICS_BIND" => Some("not-an-address".to_owned()),
            "RJ_SNAPSHOT_RECONNECT_MIN_MS" => Some("not-a-duration".to_owned()),
            _ => None,
        })
        .unwrap();
        assert!(!config.enabled());
        assert!(config.digest_secret_path.is_none());
        assert!(config.attestation_path.is_none());
        assert!(config.transport.is_none());
        assert!(config.group.is_none());
    }

    #[tokio::test]
    async fn off_runtime_has_no_listener_or_filesystem_side_effects() {
        let mut values = HashMap::new();
        values.insert("RJ_SNAPSHOT_METRICS_BIND", "127.0.0.1:1".to_owned());
        let config = load(&values);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        assert_eq!(
            run_single_engine_companion(config, shutdown_rx)
                .await
                .unwrap(),
            SingleEngineCompanionReport::Off
        );
    }

    #[tokio::test]
    async fn bounded_join_aborts_stalled_tasks_and_maps_panics() {
        let mut stalled = tokio::spawn(std::future::pending::<()>());
        assert!(matches!(
            await_task(&mut stalled, Instant::now() + Duration::from_millis(10)).await,
            Err(SingleEngineCompanionError::ShutdownTimeout)
        ));
        assert!(stalled.is_finished());

        let mut panicked = tokio::spawn(async { panic!("test task panic") });
        assert!(matches!(
            await_task(&mut panicked, Instant::now() + Duration::from_secs(1)).await,
            Err(SingleEngineCompanionError::TaskFailed)
        ));
    }

    #[test]
    fn serve_requires_one_engine_explicit_geometry_and_unambiguous_metrics() {
        let files = TestFiles::new();
        let mut values = files.values();
        let config = load(&values);
        assert!(config.enabled());
        assert_eq!(config.snapshot.sources.len(), 1);
        assert_eq!(config.group.as_ref().unwrap().block_size, 64);

        values.remove("RJ_SNAPSHOT_BLOCK_SIZE");
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());
        let mut values = files.values();
        values.insert(
            "RJ_SNAPSHOT_LIVE_ENDPOINTS",
            "tcp://127.0.0.1:1,tcp://127.0.0.1:2".to_owned(),
        );
        values.insert(
            "RJ_SNAPSHOT_REPLAY_ENDPOINTS",
            "tcp://127.0.0.1:3,tcp://127.0.0.1:4".to_owned(),
        );
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());
        let mut values = files.values();
        values.insert("RJ_SNAPSHOT_METRICS_BIND", "0.0.0.0:9091".to_owned());
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());

        let metrics_path = files
            .directory
            .with_extension("metrics")
            .join("metrics.sock");
        let mut values = files.values();
        values.remove("RJ_SNAPSHOT_METRICS_BIND");
        values.insert(
            "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
            metrics_path.to_string_lossy().into_owned(),
        );
        values.insert("RJ_SNAPSHOT_METRICS_GROUP_GID", "12004".to_owned());
        assert!(matches!(
            load(&values).metrics_endpoint,
            CompanionMetricsEndpoint::Unix {
                group_gid: 12004,
                ..
            }
        ));

        values.remove("RJ_SNAPSHOT_METRICS_GROUP_GID");
        assert!(matches!(
            SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()),
            Err(SingleEngineCompanionConfigError::Invalid {
                key: "RJ_SNAPSHOT_METRICS_GROUP_GID",
                reason: "a dedicated non-root metrics group",
            })
        ));
        values.insert("RJ_SNAPSHOT_METRICS_GROUP_GID", "12004".to_owned());
        values.insert("RJ_SNAPSHOT_METRICS_BIND", "127.0.0.1:9091".to_owned());
        assert!(matches!(
            SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()),
            Err(SingleEngineCompanionConfigError::Invalid {
                key: "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
                reason: "exactly one TCP or Unix metrics endpoint",
            })
        ));

        let mut values = files.values();
        values.remove("RJ_SNAPSHOT_METRICS_BIND");
        values.insert(
            "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
            files
                .directory
                .join("metrics.sock")
                .to_string_lossy()
                .into_owned(),
        );
        values.insert("RJ_SNAPSHOT_METRICS_GROUP_GID", "12004".to_owned());
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());

        let mut values = files.values();
        values.insert("RJ_SNAPSHOT_MAX_CLIENTS", "1".to_owned());
        assert!(matches!(
            SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()),
            Err(SingleEngineCompanionConfigError::Invalid {
                key: "RJ_SNAPSHOT_MAX_CLIENTS",
                reason: "exactly two active clients",
            })
        ));
    }

    fn alternate_metrics_gid(current_gid: u32) -> Option<u32> {
        if fs::metadata("/proc/self").ok()?.uid() == 0 {
            return Some(12_004);
        }
        fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("Groups:"))?
            .split_whitespace()
            .filter_map(|value| value.parse::<u32>().ok())
            .find(|group| *group != current_gid && *group != 0)
    }

    #[tokio::test]
    async fn permission_isolated_metrics_uds_serves_and_cleans_its_inode() {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from("/tmp").join(format!("md-met-{}-{id}", std::process::id()));
        let snapshot_parent = root.join("snapshot");
        let metrics_parent = root.join("metrics");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&snapshot_parent).unwrap();
        fs::create_dir(&metrics_parent).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&snapshot_parent, fs::Permissions::from_mode(0o2750)).unwrap();
        let owner = fs::metadata(&snapshot_parent).unwrap().uid();
        let snapshot_gid = fs::metadata(&snapshot_parent).unwrap().gid();
        let Some(metrics_gid) = alternate_metrics_gid(snapshot_gid) else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        chown(&metrics_parent, None, Some(metrics_gid)).unwrap();
        fs::set_permissions(&metrics_parent, fs::Permissions::from_mode(0o2750)).unwrap();

        let metrics_path = metrics_parent.join("metrics.sock");
        let endpoint = CompanionMetricsEndpoint::Unix {
            path: metrics_path.clone(),
            group_gid: metrics_gid,
        };
        fs::set_permissions(&snapshot_parent, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            bind_metrics_endpoint(&endpoint, &snapshot_parent.join("snapshot.sock"), owner).await,
            Err(SingleEngineCompanionError::MetricsIsolation)
        ));
        assert!(!metrics_path.exists());
        fs::set_permissions(&snapshot_parent, fs::Permissions::from_mode(0o2750)).unwrap();
        let bound = bind_metrics_endpoint(&endpoint, &snapshot_parent.join("snapshot.sock"), owner)
            .await
            .unwrap();
        let socket_metadata = fs::symlink_metadata(&metrics_path).unwrap();
        assert_eq!(socket_metadata.gid(), metrics_gid);
        assert_eq!(socket_metadata.mode() & 0o777, 0o660);

        let registry = Arc::new(Registry::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server =
            tokio::spawn(async move { serve_metrics_endpoint(bound, registry, shutdown_rx).await });
        let mut client = tokio::net::UnixStream::connect(&metrics_path)
            .await
            .unwrap();
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!metrics_path.exists());

        let old = bind_metrics_endpoint(&endpoint, &snapshot_parent.join("snapshot.sock"), owner)
            .await
            .unwrap();
        let old_inode = fs::symlink_metadata(&metrics_path).unwrap().ino();
        fs::remove_file(&metrics_path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&metrics_path).unwrap();
        let replacement_inode = fs::symlink_metadata(&metrics_path).unwrap().ino();
        assert_ne!(old_inode, replacement_inode);
        drop(old);
        assert_eq!(
            fs::symlink_metadata(&metrics_path).unwrap().ino(),
            replacement_inode
        );
        drop(replacement);
        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn metrics_uds_rejects_session_group_and_preserves_existing_target() {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from("/tmp").join(format!("md-mrej-{}-{id}", std::process::id()));
        let snapshot_parent = root.join("snapshot");
        let metrics_parent = root.join("metrics");
        fs::create_dir_all(&snapshot_parent).unwrap();
        fs::create_dir(&metrics_parent).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&snapshot_parent, fs::Permissions::from_mode(0o2750)).unwrap();
        fs::set_permissions(&metrics_parent, fs::Permissions::from_mode(0o2750)).unwrap();
        let owner = fs::metadata(&snapshot_parent).unwrap().uid();
        let shared_gid = fs::metadata(&snapshot_parent).unwrap().gid();
        let metrics_path = metrics_parent.join("metrics.sock");
        fs::write(&metrics_path, b"preserve").unwrap();
        let endpoint = CompanionMetricsEndpoint::Unix {
            path: metrics_path.clone(),
            group_gid: shared_gid,
        };
        assert!(matches!(
            bind_metrics_endpoint(&endpoint, &snapshot_parent.join("snapshot.sock"), owner,).await,
            Err(SingleEngineCompanionError::MetricsIsolation)
        ));
        assert_eq!(fs::read(&metrics_path).unwrap(), b"preserve");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stable_observe_only_fence_has_a_bounded_metric_reason() {
        let event =
            CompanionIndexOwnerEvent::Rebuild(CompanionIndexOwnerRebuildReason::ReplayTooLarge);
        assert_eq!(owner_event_labels(&event), ("rebuild", "replay_too_large"));

        let registry = Registry::new();
        let observer = OwnerObserver::new(&registry).unwrap();
        observer.observe(event);
        let family = registry
            .gather()
            .into_iter()
            .find(|family| family.name() == "ds4proxy_snapshot_companion_owner_events_total")
            .expect("owner metric family");
        assert_eq!(family.get_metric().len(), 1);
        let labels = family.get_metric()[0].get_label();
        assert!(
            labels
                .iter()
                .any(|label| { label.name() == "event" && label.value() == "rebuild" })
        );
        assert!(
            labels
                .iter()
                .any(|label| { label.name() == "reason" && label.value() == "replay_too_large" })
        );

        for phase in [
            CompanionIndexOwnerReplayInvalidPhase::Apply,
            CompanionIndexOwnerReplayInvalidPhase::Boundary,
            CompanionIndexOwnerReplayInvalidPhase::Tail,
            CompanionIndexOwnerReplayInvalidPhase::Commit,
        ] {
            assert_eq!(
                owner_event_labels(&CompanionIndexOwnerEvent::ReplayInvalid(phase)),
                ("replay_invalid", replay_invalid_phase_label(phase))
            );
        }
    }

    #[test]
    fn debug_and_errors_never_expose_protected_paths_or_endpoints() {
        let files = TestFiles::new();
        let config = load(&files.values());
        let debug = format!("{config:?}");
        for forbidden in [
            files.digest.to_string_lossy(),
            files.attestation.to_string_lossy(),
            std::borrow::Cow::Borrowed("45171"),
        ] {
            assert!(!debug.contains(forbidden.as_ref()));
        }

        let metrics_path = files
            .directory
            .with_extension("metrics")
            .join("metrics.sock");
        let mut values = files.values();
        values.remove("RJ_SNAPSHOT_METRICS_BIND");
        values.insert(
            "RJ_SNAPSHOT_METRICS_SOCKET_PATH",
            metrics_path.to_string_lossy().into_owned(),
        );
        values.insert("RJ_SNAPSHOT_METRICS_GROUP_GID", "12004".to_owned());
        let debug = format!("{:?}", load(&values));
        assert!(!debug.contains(metrics_path.to_string_lossy().as_ref()));
        assert!(!debug.contains("12004"));
    }

    #[tokio::test]
    async fn unauthenticated_startup_fails_before_socket_publication() {
        let files = TestFiles::new();
        fs::write(&files.attestation, b"malformed").unwrap();
        let config = load(&files.values());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        assert!(matches!(
            run_single_engine_companion(config, shutdown_rx).await,
            Err(SingleEngineCompanionError::Attestation(_))
        ));
        assert!(!files.socket.exists());
    }

    #[tokio::test]
    async fn unsafe_session_secret_fails_before_socket_publication() {
        let files = TestFiles::new();
        fs::set_permissions(&files.session, fs::Permissions::from_mode(0o622)).unwrap();
        let config = load(&files.values());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        assert!(matches!(
            run_single_engine_companion(config, shutdown_rx).await,
            Err(SingleEngineCompanionError::SessionSecret(
                SnapshotSecretFileError::UnsafePermissions
            ))
        ));
        assert!(!files.socket.exists());
    }

    #[tokio::test]
    async fn full_composition_publishes_socket_and_shuts_down_cleanly() {
        let files = TestFiles::new();
        if files.owner == 0 {
            // The production contract intentionally prohibits root service
            // identity; root-only test containers cannot exercise peer UIDs.
            return;
        }
        let config = load(&files.values());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_single_engine_companion(config, shutdown_rx));
        timeout(Duration::from_secs(2), async {
            while !files.socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("snapshot socket must be published");
        shutdown_tx.send(true).unwrap();
        let report = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            report,
            SingleEngineCompanionReport::Stopped { .. }
        ));
        assert!(!files.socket.exists());
    }
}
