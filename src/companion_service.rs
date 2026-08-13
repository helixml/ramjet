//! Standalone, off-by-default single-engine snapshot companion composition.

use std::{
    env, fmt,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::{Router, body::Body, http::Response, routing::get};
use prometheus::{CounterVec, Encoder, Gauge, HistogramVec, Opts, Registry, TextEncoder};
use thiserror::Error;
use tokio::{
    net::TcpListener,
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
        CompanionIndexOwnerReplayKind, CompanionIndexOwnerReplayOutcome, CompanionIndexOwnerReport,
        ZmqCompanionKvEventConnector,
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
};

const MAX_PATH_BYTES: usize = 4_096;
const MAX_REPLAY_BATCHES: usize = 100_000;
const MAX_BLOCK_SIZE: usize = 1_048_576;

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
    metrics_bind: SocketAddr,
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
            .field("metrics_loopback", &self.metrics_bind.ip().is_loopback())
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
        let metrics_bind = parse_loopback(
            get("DS4_SNAPSHOT_METRICS_BIND")
                .as_deref()
                .unwrap_or("127.0.0.1:9091"),
        )?;
        let attestation_refresh = parse_duration(
            &mut get,
            "DS4_SNAPSHOT_ATTESTATION_REFRESH_MS",
            1_000,
            50,
            60_000,
        )?;
        let owner = CompanionIndexOwnerConfig {
            replay_limit: u64::try_from(parse_usize(
                &mut get,
                "DS4_SNAPSHOT_REPLAY_LIMIT",
                10_000,
                1,
                MAX_REPLAY_BATCHES,
            )?)
            .map_err(|_| invalid("DS4_SNAPSHOT_REPLAY_LIMIT", "a bounded batch count"))?,
            reconnect_min: parse_duration(
                &mut get,
                "DS4_SNAPSHOT_RECONNECT_MIN_MS",
                250,
                1,
                60_000,
            )?,
            reconnect_max: parse_duration(
                &mut get,
                "DS4_SNAPSHOT_RECONNECT_MAX_MS",
                5_000,
                1,
                60_000,
            )?,
        };
        if owner.reconnect_min > owner.reconnect_max {
            return Err(invalid(
                "DS4_SNAPSHOT_RECONNECT_MAX_MS",
                "at least the reconnect minimum",
            ));
        }
        if snapshot.mode == SnapshotCompanionMode::Off {
            return Ok(Self {
                snapshot,
                digest_secret_path: None,
                attestation_path: None,
                metrics_bind,
                attestation_refresh,
                transport: None,
                owner,
                group: None,
            });
        }
        if snapshot.sources.len() != 1 {
            return Err(invalid(
                "DS4_SNAPSHOT_LIVE_ENDPOINTS",
                "exactly one live/replay engine pair",
            ));
        }
        let digest_secret_path = required_path(&mut get, "DS4_SNAPSHOT_DIGEST_SECRET_PATH")?;
        let attestation_path = required_path(&mut get, "DS4_SNAPSHOT_ATTESTATION_PATH")?;
        let session_path = snapshot
            .secret_path
            .as_ref()
            .ok_or_else(|| invalid("DS4_SNAPSHOT_SECRET_PATH", "a session secret path"))?;
        if &digest_secret_path == session_path
            || attestation_path == digest_secret_path
            || &attestation_path == session_path
        {
            return Err(invalid(
                "DS4_SNAPSHOT_ATTESTATION_PATH",
                "three distinct protected file paths",
            ));
        }
        let block_size = parse_usize(&mut get, "DS4_SNAPSHOT_BLOCK_SIZE", 0, 1, MAX_BLOCK_SIZE)?;
        let attention = match get("DS4_SNAPSHOT_ATTENTION_KIND")
            .as_deref()
            .unwrap_or("mla")
        {
            "full" => ConfigAttentionKind::Full,
            "mla" => ConfigAttentionKind::Mla,
            "sink_full" => ConfigAttentionKind::SinkFull,
            _ => {
                return Err(invalid(
                    "DS4_SNAPSHOT_ATTENTION_KIND",
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
                "DS4_SNAPSHOT_CONNECT_TIMEOUT_MS",
                2_000,
                1,
                60_000,
            )?,
            replay_timeout: parse_duration(
                &mut get,
                "DS4_SNAPSHOT_REPLAY_TIMEOUT_MS",
                30_000,
                1,
                900_000,
            )?,
            max_replay_batches: usize::try_from(owner.replay_limit)
                .map_err(|_| invalid("DS4_SNAPSHOT_REPLAY_LIMIT", "a bounded batch count"))?,
            max_replay_tail_batches: parse_usize(
                &mut get,
                "DS4_SNAPSHOT_REPLAY_TAIL_LIMIT",
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
        let data_parallel_rank = parse_u32(&mut get, "DS4_SNAPSHOT_DATA_PARALLEL_RANK", 0)?;
        let group_idx = parse_u32(&mut get, "DS4_SNAPSHOT_GROUP_INDEX", 0)?;
        Ok(Self {
            snapshot,
            digest_secret_path: Some(digest_secret_path),
            attestation_path: Some(attestation_path),
            metrics_bind,
            attestation_refresh,
            transport: Some(transport),
            owner,
            group: Some(GroupMetadata {
                data_parallel_rank,
                group_idx,
                attention_kind: attention.wire(),
                disposition: GroupDisposition::Indexed,
                block_size: u32::try_from(block_size)
                    .map_err(|_| invalid("DS4_SNAPSHOT_BLOCK_SIZE", "a bounded block size"))?,
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
            Self::Attestation(error) => error.reason(),
            Self::Source(_) => "source",
            Self::Owner(error) => error.reason(),
            Self::Runtime(error) => error.reason(),
            Self::Metrics(_) => "metrics",
            Self::MetricsIo => "metrics_io",
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

    let metrics_listener = TcpListener::bind(config.metrics_bind)
        .await
        .map_err(|_| SingleEngineCompanionError::MetricsIo)?;
    let metrics_registry = Arc::clone(&registry);
    let mut metrics_shutdown = internal_rx.clone();
    let mut metrics_task = tokio::spawn(async move {
        axum::serve(
            metrics_listener,
            Router::new().route(
                "/metrics",
                get(move || {
                    let registry = Arc::clone(&metrics_registry);
                    async move { metrics_response(&registry) }
                }),
            ),
        )
        .with_graceful_shutdown(async move { wait_for_shutdown(&mut metrics_shutdown).await })
        .await
        .map_err(|_| SingleEngineCompanionError::MetricsIo)
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
            let owner = await_task(&mut owner_task, deadline).await??;
            let snapshot = await_task(&mut snapshot_task, deadline).await??;
            await_task(&mut metrics_task, deadline).await??;
            await_task(&mut watcher_task, deadline).await?;
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
    let _ = await_task(first, deadline).await?;
    let _ = await_task(second, deadline).await?;
    let _ = await_task(third, deadline).await?;
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

fn parse_loopback(raw: &str) -> Result<SocketAddr, SingleEngineCompanionConfigError> {
    let address = raw
        .parse::<SocketAddr>()
        .map_err(|_| invalid("DS4_SNAPSHOT_METRICS_BIND", "a loopback IP socket address"))?;
    if !matches!(address.ip(), IpAddr::V4(_) | IpAddr::V6(_)) || !address.ip().is_loopback() {
        return Err(invalid(
            "DS4_SNAPSHOT_METRICS_BIND",
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

// Keep the imported error type visible in this composition boundary's public
// documentation: both session and digest paths use exactly this file policy.
const _: Option<SnapshotSecretFileError> = None;

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        companion_attestation::{
            encode_authenticated_engine_incarnation, load_companion_digest_secret,
        },
        kv_snapshot::EngineIncarnation,
    };
    use tokio::time::timeout;

    static TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct TestFiles {
        directory: PathBuf,
        socket: PathBuf,
        session: PathBuf,
        digest: PathBuf,
        attestation: PathBuf,
        owner: u32,
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
            }
        }

        fn values(&self) -> HashMap<&'static str, String> {
            HashMap::from([
                ("DS4_SNAPSHOT_COMPANION_MODE", "serve".to_owned()),
                (
                    "DS4_SNAPSHOT_SOCKET_PATH",
                    self.socket.to_string_lossy().into_owned(),
                ),
                ("DS4_SNAPSHOT_COMPANION_UID", self.owner.to_string()),
                (
                    "DS4_SNAPSHOT_CLIENT_UID",
                    self.owner.saturating_add(1).max(1).to_string(),
                ),
                (
                    "DS4_SNAPSHOT_SECRET_PATH",
                    self.session.to_string_lossy().into_owned(),
                ),
                ("DS4_SNAPSHOT_SECRET_OWNER_UID", self.owner.to_string()),
                (
                    "DS4_SNAPSHOT_LIVE_ENDPOINTS",
                    "tcp://127.0.0.1:45171".to_owned(),
                ),
                (
                    "DS4_SNAPSHOT_REPLAY_ENDPOINTS",
                    "tcp://127.0.0.1:45172".to_owned(),
                ),
                (
                    "DS4_SNAPSHOT_DIGEST_SECRET_PATH",
                    self.digest.to_string_lossy().into_owned(),
                ),
                (
                    "DS4_SNAPSHOT_ATTESTATION_PATH",
                    self.attestation.to_string_lossy().into_owned(),
                ),
                ("DS4_SNAPSHOT_BLOCK_SIZE", "64".to_owned()),
                ("DS4_SNAPSHOT_METRICS_BIND", "127.0.0.1:0".to_owned()),
                ("DS4_SNAPSHOT_RECONNECT_MIN_MS", "10".to_owned()),
                ("DS4_SNAPSHOT_RECONNECT_MAX_MS", "20".to_owned()),
                ("DS4_SNAPSHOT_ATTESTATION_REFRESH_MS", "50".to_owned()),
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
        let config = SingleEngineCompanionConfig::from_lookup(|_| None).unwrap();
        assert!(!config.enabled());
        assert!(config.digest_secret_path.is_none());
        assert!(config.attestation_path.is_none());
        assert!(config.transport.is_none());
        assert!(config.group.is_none());
    }

    #[tokio::test]
    async fn off_runtime_has_no_listener_or_filesystem_side_effects() {
        let mut values = HashMap::new();
        values.insert("DS4_SNAPSHOT_METRICS_BIND", "127.0.0.1:1".to_owned());
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
    fn serve_requires_one_engine_explicit_geometry_and_loopback_metrics() {
        let files = TestFiles::new();
        let mut values = files.values();
        let config = load(&values);
        assert!(config.enabled());
        assert_eq!(config.snapshot.sources.len(), 1);
        assert_eq!(config.group.as_ref().unwrap().block_size, 64);

        values.remove("DS4_SNAPSHOT_BLOCK_SIZE");
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());
        let mut values = files.values();
        values.insert(
            "DS4_SNAPSHOT_LIVE_ENDPOINTS",
            "tcp://127.0.0.1:1,tcp://127.0.0.1:2".to_owned(),
        );
        values.insert(
            "DS4_SNAPSHOT_REPLAY_ENDPOINTS",
            "tcp://127.0.0.1:3,tcp://127.0.0.1:4".to_owned(),
        );
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());
        let mut values = files.values();
        values.insert("DS4_SNAPSHOT_METRICS_BIND", "0.0.0.0:9091".to_owned());
        assert!(SingleEngineCompanionConfig::from_lookup(|key| values.get(key).cloned()).is_err());
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
