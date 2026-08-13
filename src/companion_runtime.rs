//! Off-by-default composition for the snapshot companion foundation.
//!
//! This module deliberately does not construct an engine event source. Serve
//! mode requires an injected [`SnapshotProducerSource`], and the ordinary load
//! balancer does not call this coordinator. A missing source therefore fails
//! before secret or socket filesystem state is touched.

use std::{future::Future, sync::Arc, time::Duration};

use prometheus::Registry;
use thiserror::Error;
use tokio::{
    net::UnixListener,
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

use crate::{
    companion_config::{SnapshotCompanionConfig, SnapshotCompanionMode},
    companion_metrics::{
        CompanionEngineSlot, CompanionMetrics, CompanionMetricsError, CompanionSessionResult,
    },
    snapshot_producer::{
        SnapshotProducer, SnapshotProducerConfig, SnapshotProducerError, SnapshotProducerSource,
    },
    snapshot_secret_file::{
        SnapshotSecretFileError, SnapshotSecretFilePolicy, load_snapshot_session_secret,
    },
    snapshot_session::SnapshotSessionLimits,
    snapshot_socket_path::{SnapshotSocketPathError, SocketParentPolicy, bind_and_publish},
    snapshot_supervisor::{
        MAX_ACTIVE_SNAPSHOT_CLIENTS, SnapshotSupervisorConfig, SnapshotSupervisorError,
        SnapshotSupervisorReport, supervise_snapshot_sessions,
    },
    snapshot_tail_wire::TailWireLimits,
};

const FRAME_OVERHEAD_BYTES: usize = 4 * 1024;
const METADATA_BYTES: usize = 4 * 1024;
const INCARNATION_COMPONENT_BYTES: usize = 512;
const SOURCE_STATUS_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotCompanionRunReport {
    Off,
    Served(SnapshotSupervisorReport),
}

#[derive(Debug, Error)]
pub enum SnapshotCompanionRuntimeError {
    #[error("snapshot companion runtime configuration is unsupported")]
    InvalidConfig,
    #[error("snapshot companion producer source is unavailable")]
    MissingSource,
    #[error("snapshot companion metrics initialization failed")]
    Metrics(#[from] CompanionMetricsError),
    #[error("snapshot companion secret loading failed")]
    Secret(#[from] SnapshotSecretFileError),
    #[error("snapshot companion socket publication failed")]
    Socket(#[from] SnapshotSocketPathError),
    #[error("snapshot companion producer initialization failed")]
    Producer(#[from] SnapshotProducerError),
    #[error("snapshot companion supervisor failed")]
    Supervisor(#[from] SnapshotSupervisorError),
    #[error("snapshot companion shutdown timed out")]
    ShutdownTimeout,
    #[error("snapshot companion listener conversion failed")]
    ListenerConversion,
    #[error("snapshot companion socket cleanup failed")]
    Cleanup,
}

impl SnapshotCompanionRuntimeError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_config",
            Self::MissingSource => "missing_source",
            Self::Metrics(error) => error.reason(),
            Self::Secret(error) => error.reason(),
            Self::Socket(error) => error.reason(),
            Self::Producer(error) => error.reason(),
            Self::Supervisor(error) => error.reason(),
            Self::ShutdownTimeout => "shutdown_timeout",
            Self::ListenerConversion => "listener_conversion_failed",
            Self::Cleanup => "socket_cleanup_failed",
        }
    }
}

/// Compose and run one companion source until shutdown.
///
/// Serve mode currently supports exactly one configured source because the
/// authenticated wire hello has no engine selector. Multi-engine publication
/// must remain off until that routing contract exists.
///
/// # Errors
///
/// Returns a content-free startup, supervisor, or cleanup error. A missing
/// source and unsupported source cardinality fail before opening the secret or
/// publishing the socket.
pub async fn run_snapshot_companion(
    config: SnapshotCompanionConfig,
    registry: &Registry,
    source: Option<Arc<dyn SnapshotProducerSource>>,
    shutdown: watch::Receiver<bool>,
) -> Result<SnapshotCompanionRunReport, SnapshotCompanionRuntimeError> {
    run_snapshot_companion_with(
        config,
        registry,
        source,
        shutdown,
        |listener, config, shutdown, producer| async move {
            supervise_snapshot_sessions(listener, config, shutdown, move |stream, deadline| {
                let producer = Arc::clone(&producer);
                async move { producer.handle(stream, deadline).await }
            })
            .await
        },
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn run_snapshot_companion_with<R, Fut>(
    config: SnapshotCompanionConfig,
    registry: &Registry,
    source: Option<Arc<dyn SnapshotProducerSource>>,
    shutdown: watch::Receiver<bool>,
    run_supervisor: R,
) -> Result<SnapshotCompanionRunReport, SnapshotCompanionRuntimeError>
where
    R: FnOnce(
        UnixListener,
        SnapshotSupervisorConfig,
        watch::Receiver<bool>,
        Arc<SnapshotProducer>,
    ) -> Fut,
    Fut: Future<Output = Result<SnapshotSupervisorReport, SnapshotSupervisorError>>,
{
    let metrics = Arc::new(CompanionMetrics::new(registry, &config)?);
    if config.mode == SnapshotCompanionMode::Off {
        return Ok(SnapshotCompanionRunReport::Off);
    }

    let source = source.ok_or(SnapshotCompanionRuntimeError::MissingSource)?;
    if config.sources.len() != 1 || config.max_clients != MAX_ACTIVE_SNAPSHOT_CLIENTS {
        return Err(SnapshotCompanionRuntimeError::InvalidConfig);
    }
    let socket_path = config
        .socket_path
        .as_deref()
        .ok_or(SnapshotCompanionRuntimeError::InvalidConfig)?;
    let companion_uid = config
        .companion_uid
        .ok_or(SnapshotCompanionRuntimeError::InvalidConfig)?;
    let client_uid = config
        .client_uid
        .ok_or(SnapshotCompanionRuntimeError::InvalidConfig)?;
    let secret_path = config
        .secret_path
        .as_deref()
        .ok_or(SnapshotCompanionRuntimeError::InvalidConfig)?;
    let engine = metrics.engine_slot(0)?;

    // The current producer accepts one absolute deadline rather than separate
    // snapshot-phase and resettable tail-idle deadlines. Keep the foundation
    // fail-closed by using the stricter configured bound; do not silently
    // relax either policy by adding them together.
    let session_timeout = config.snapshot_deadline.min(config.tail_idle_deadline);
    let supervisor_config = SnapshotSupervisorConfig::new(session_timeout)?;
    if config.shutdown_deadline.is_zero() {
        return Err(SnapshotCompanionRuntimeError::InvalidConfig);
    }
    let max_response_frame_bytes = config
        .max_snapshot_frame_bytes
        .checked_add(FRAME_OVERHEAD_BYTES)
        .ok_or(SnapshotCompanionRuntimeError::InvalidConfig)?;
    let secret = load_snapshot_session_secret(
        secret_path,
        SnapshotSecretFilePolicy {
            expected_owner_uid: config.secret_owner_uid,
        },
    )?;
    metrics.update_source_status(engine, source.status());
    let producer_source = Arc::clone(&source);
    let producer = Arc::new(SnapshotProducer::new(
        SnapshotProducerConfig {
            expected_peer_uid: client_uid,
            session_limits: SnapshotSessionLimits {
                max_hello_frame_bytes: SnapshotSessionLimits::default().max_hello_frame_bytes,
                max_response_frame_bytes,
                max_header_bytes: METADATA_BYTES,
                max_snapshot_frame_bytes: config.max_snapshot_frame_bytes,
                max_incarnation_component_bytes: INCARNATION_COMPONENT_BYTES,
            },
            tail_limits: TailWireLimits {
                max_frame_bytes: config.max_tail_frame_bytes,
                max_metadata_bytes: METADATA_BYTES,
                max_payload_bytes: config.max_batch_payload_bytes,
                max_incarnation_component_bytes: INCARNATION_COMPONENT_BYTES,
            },
            tail_queue_capacity: config.tail_queue_capacity,
            tail_queue_max_bytes: config.tail_queue_max_bytes,
        },
        Arc::new(secret),
        producer_source,
    )?);

    // Bind last so every startup error above is guaranteed socket-free.
    let published = bind_and_publish(
        socket_path,
        SocketParentPolicy {
            owner_uid: companion_uid,
        },
    )?;
    let (listener, mut socket_guard) = published.into_parts();
    listener
        .set_nonblocking(true)
        .map_err(|_| SnapshotCompanionRuntimeError::ListenerConversion)?;
    let listener = UnixListener::from_std(listener)
        .map_err(|_| SnapshotCompanionRuntimeError::ListenerConversion)?;
    metrics.set_listening(engine, true);

    let shutdown_monitor = shutdown.clone();
    let result = await_bounded_shutdown(
        observe_source_status(
            run_supervisor(listener, supervisor_config, shutdown, producer),
            source,
            Arc::clone(&metrics),
            engine,
        ),
        shutdown_monitor,
        config.shutdown_deadline,
    )
    .await;
    metrics.set_listening(engine, false);
    if let Ok(Ok(report)) = &result {
        record_supervisor_report(&metrics, engine, *report);
    }
    let cleanup_result = socket_guard.cleanup();
    if cleanup_result.is_err() {
        return Err(SnapshotCompanionRuntimeError::Cleanup);
    }
    let report = result??;
    Ok(SnapshotCompanionRunReport::Served(report))
}

async fn observe_source_status<F>(
    supervisor: F,
    source: Arc<dyn SnapshotProducerSource>,
    metrics: Arc<CompanionMetrics>,
    engine: CompanionEngineSlot,
) -> Result<SnapshotSupervisorReport, SnapshotSupervisorError>
where
    F: Future<Output = Result<SnapshotSupervisorReport, SnapshotSupervisorError>>,
{
    tokio::pin!(supervisor);
    let mut ticker = interval(SOURCE_STATUS_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            result = &mut supervisor => {
                metrics.update_source_status(engine, source.status());
                return result;
            }
            _ = ticker.tick() => {
                metrics.update_source_status(engine, source.status());
            }
        }
    }
}

async fn await_bounded_shutdown<F>(
    supervisor: F,
    mut shutdown: watch::Receiver<bool>,
    shutdown_deadline: std::time::Duration,
) -> Result<Result<SnapshotSupervisorReport, SnapshotSupervisorError>, SnapshotCompanionRuntimeError>
where
    F: Future<Output = Result<SnapshotSupervisorReport, SnapshotSupervisorError>>,
{
    tokio::pin!(supervisor);
    tokio::select! {
        biased;
        result = &mut supervisor => Ok(result),
        () = wait_for_shutdown(&mut shutdown) => {
            timeout(shutdown_deadline, &mut supervisor)
                .await
                .map_err(|_| SnapshotCompanionRuntimeError::ShutdownTimeout)
        }
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

/// Fold a content-free aggregate supervisor report into closed-label metrics.
pub fn record_supervisor_report(
    metrics: &CompanionMetrics,
    engine: CompanionEngineSlot,
    report: SnapshotSupervisorReport,
) {
    metrics.record_sessions(
        engine,
        CompanionSessionResult::Completed,
        report.sessions_completed,
    );
    metrics.record_sessions(
        engine,
        CompanionSessionResult::FailedApplication,
        report.sessions_failed,
    );
    metrics.record_sessions(
        engine,
        CompanionSessionResult::Timeout,
        report.sessions_timed_out,
    );
    metrics.record_sessions(
        engine,
        CompanionSessionResult::Cancelled,
        report.sessions_cancelled,
    );
    metrics.record_sessions(
        engine,
        CompanionSessionResult::RejectedCapacity,
        report.connections_rejected_capacity,
    );
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use prometheus::TextEncoder;
    use tokio::sync::oneshot;
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::{
        companion_config::SnapshotCompanionSource,
        snapshot_producer::{
            SnapshotBuildFuture, SnapshotProducerCancellation, SnapshotProducerSourceError,
            SnapshotProducerSourcePhase, SnapshotProducerSourceStatus, SnapshotTailPublisher,
        },
    };

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(1);

    struct TestFiles {
        directory: PathBuf,
        socket: PathBuf,
        secret: PathBuf,
        owner_uid: u32,
    }

    impl TestFiles {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("md-runtime-{:x}-{sequence:x}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let owner_uid = fs::metadata(&directory).unwrap().uid();
            let secret = directory.join("session.secret");
            fs::write(&secret, [7_u8; 32]).unwrap();
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
            let socket = directory.join("companion.sock");
            Self {
                directory,
                socket,
                secret,
                owner_uid,
            }
        }

        fn config(&self) -> SnapshotCompanionConfig {
            SnapshotCompanionConfig {
                mode: SnapshotCompanionMode::Serve,
                socket_path: Some(self.socket.clone()),
                companion_uid: Some(self.owner_uid),
                client_uid: Some(self.owner_uid.saturating_add(1)),
                secret_path: Some(self.secret.clone()),
                secret_owner_uid: self.owner_uid,
                max_clients: MAX_ACTIVE_SNAPSHOT_CLIENTS,
                tail_queue_capacity: 8,
                tail_queue_max_bytes: 1024 * 1024,
                snapshot_deadline: Duration::from_millis(100),
                tail_idle_deadline: Duration::from_secs(1),
                shutdown_deadline: Duration::from_secs(1),
                max_snapshot_frame_bytes: 1024 * 1024,
                max_tail_frame_bytes: 64 * 1024,
                max_batch_payload_bytes: 60 * 1024,
                max_batch_events: 64,
                event_topic: String::new(),
                sources: vec![SnapshotCompanionSource {
                    live_endpoint: "tcp://engine.invalid:1".to_owned(),
                    replay_endpoint: "tcp://engine.invalid:2".to_owned(),
                }],
            }
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.socket);
            let _ = fs::remove_file(&self.secret);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    struct UnavailableSource;

    impl SnapshotProducerSource for UnavailableSource {
        fn start(
            &self,
            _publisher: SnapshotTailPublisher,
            _cancellation: SnapshotProducerCancellation,
        ) -> Result<SnapshotBuildFuture, SnapshotProducerSourceError> {
            Err(SnapshotProducerSourceError::Failed)
        }
    }

    struct MutableStatusSource {
        phase: AtomicUsize,
    }

    impl SnapshotProducerSource for MutableStatusSource {
        fn status(&self) -> SnapshotProducerSourceStatus {
            match self.phase.load(Ordering::Acquire) {
                0 => SnapshotProducerSourceStatus {
                    phase: SnapshotProducerSourcePhase::Replay,
                    ..SnapshotProducerSourceStatus::default()
                },
                1 => SnapshotProducerSourceStatus {
                    phase: SnapshotProducerSourcePhase::Ready,
                    ready: true,
                    watermark_present: true,
                    active_sessions: 1,
                    indexed_blocks: 42,
                },
                _ => SnapshotProducerSourceStatus {
                    phase: SnapshotProducerSourcePhase::Fenced,
                    ..SnapshotProducerSourceStatus::default()
                },
            }
        }

        fn start(
            &self,
            _publisher: SnapshotTailPublisher,
            _cancellation: SnapshotProducerCancellation,
        ) -> Result<SnapshotBuildFuture, SnapshotProducerSourceError> {
            Err(SnapshotProducerSourceError::Failed)
        }
    }

    fn source() -> Arc<dyn SnapshotProducerSource> {
        Arc::new(UnavailableSource)
    }

    fn metric_text(registry: &Registry) -> String {
        TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap()
    }

    async fn wait_for_path(path: &Path) {
        timeout(Duration::from_secs(1), async {
            while !path.exists() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn off_mode_needs_no_source_or_filesystem_state() {
        let registry = Registry::new();
        let (_, shutdown) = watch::channel(false);
        let report = run_snapshot_companion(
            SnapshotCompanionConfig::from_lookup(|_| None).unwrap(),
            &registry,
            None,
            shutdown,
        )
        .await
        .unwrap();
        assert_eq!(report, SnapshotCompanionRunReport::Off);
        assert!(metric_text(&registry).contains("ds4proxy_snapshot_companion_enabled 0"));
    }

    #[tokio::test]
    async fn runtime_observer_tracks_live_source_authority_transitions() {
        let files = TestFiles::new();
        let registry = Registry::new();
        let metrics = Arc::new(CompanionMetrics::new(&registry, &files.config()).unwrap());
        let engine = metrics.engine_slot(0).unwrap();
        let source = Arc::new(MutableStatusSource {
            phase: AtomicUsize::new(0),
        });
        let source_dyn: Arc<dyn SnapshotProducerSource> = source.clone();
        let (finish, finished) = oneshot::channel();
        let observer = tokio::spawn(observe_source_status(
            async move {
                finished.await.unwrap();
                Ok(SnapshotSupervisorReport::default())
            },
            source_dyn,
            metrics,
            engine,
        ));

        sleep(SOURCE_STATUS_INTERVAL * 2).await;
        let replay = metric_text(&registry);
        assert!(replay.contains(
            "ds4proxy_snapshot_companion_source_phase{engine=\"engine-0\",phase=\"replay\"} 1"
        ));
        assert!(replay.contains("ds4proxy_snapshot_companion_ready{engine=\"engine-0\"} 0"));

        source.phase.store(1, Ordering::Release);
        sleep(SOURCE_STATUS_INTERVAL * 2).await;
        let ready = metric_text(&registry);
        assert!(ready.contains(
            "ds4proxy_snapshot_companion_source_phase{engine=\"engine-0\",phase=\"ready\"} 1"
        ));
        assert!(ready.contains("ds4proxy_snapshot_companion_ready{engine=\"engine-0\"} 1"));
        assert!(
            ready.contains(
                "ds4proxy_snapshot_companion_source_indexed_blocks{engine=\"engine-0\"} 42"
            )
        );

        source.phase.store(2, Ordering::Release);
        sleep(SOURCE_STATUS_INTERVAL * 2).await;
        let fenced = metric_text(&registry);
        assert!(fenced.contains(
            "ds4proxy_snapshot_companion_source_phase{engine=\"engine-0\",phase=\"fenced\"} 1"
        ));
        assert!(fenced.contains("ds4proxy_snapshot_companion_ready{engine=\"engine-0\"} 0"));
        finish.send(()).unwrap();
        observer.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn supervisor_start_failure_removes_published_socket() {
        let files = TestFiles::new();
        let socket = files.socket.clone();
        let registry = Registry::new();
        let registry_ref = &registry;
        let socket_ref = &socket;
        let (_, shutdown) = watch::channel(false);
        let result = run_snapshot_companion_with(
            files.config(),
            &registry,
            Some(source()),
            shutdown,
            move |listener, supervisor, _, _| async move {
                assert!(socket_ref.exists());
                assert_eq!(supervisor.session_timeout, Duration::from_millis(100));
                let text = metric_text(registry_ref);
                assert!(text.contains(
                    "ds4proxy_snapshot_companion_listening{engine=\"engine-0\"} 1"
                ));
                assert!(
                    text.contains("ds4proxy_snapshot_companion_ready{engine=\"engine-0\"} 0")
                );
                assert!(text.contains(
                    "ds4proxy_snapshot_companion_source_phase{engine=\"engine-0\",phase=\"unknown\"} 1"
                ));
                drop(listener);
                Err(SnapshotSupervisorError::Listener)
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(SnapshotCompanionRuntimeError::Supervisor(
                SnapshotSupervisorError::Listener
            ))
        ));
        assert!(!files.socket.exists());
        assert!(
            metric_text(&registry)
                .contains("ds4proxy_snapshot_companion_listening{engine=\"engine-0\"} 0")
        );
    }

    #[tokio::test]
    async fn shutdown_stops_supervisor_and_removes_socket() {
        let files = TestFiles::new();
        let socket = files.socket.clone();
        let config = files.config();
        let registry = Registry::new();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            run_snapshot_companion(config, &registry, Some(source()), shutdown).await
        });
        wait_for_path(&socket).await;
        shutdown_sender.send(true).unwrap();
        let result = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            result,
            SnapshotCompanionRunReport::Served(SnapshotSupervisorReport::default())
        );
        assert!(!socket.exists());
    }

    #[test]
    fn supervisor_report_maps_capacity_and_terminal_results() {
        let files = TestFiles::new();
        let config = files.config();
        let registry = Registry::new();
        let metrics = CompanionMetrics::new(&registry, &config).unwrap();
        let engine = metrics.engine_slot(0).unwrap();
        record_supervisor_report(
            &metrics,
            engine,
            SnapshotSupervisorReport {
                sessions_completed: 2,
                sessions_failed: 3,
                sessions_timed_out: 4,
                sessions_cancelled: 5,
                connections_rejected_capacity: 6,
                ..SnapshotSupervisorReport::default()
            },
        );
        let text = metric_text(&registry);
        for expected in [
            "outcome=\"completed\",reason=\"none\"} 2",
            "outcome=\"failed\",reason=\"application\"} 3",
            "outcome=\"failed\",reason=\"timeout\"} 4",
            "outcome=\"failed\",reason=\"cancelled\"} 5",
            "outcome=\"rejected\",reason=\"capacity\"} 6",
        ] {
            assert!(text.contains(expected), "missing metric: {expected}");
        }
    }
}
