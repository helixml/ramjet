use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, Method, Request, Response, StatusCode, Uri},
};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    sync::mpsc,
    time::{Instant, Interval, MissedTickBehavior},
};
use tokio_stream::wrappers::ReceiverStream;
use url::Url;

use crate::{
    config::{Config, DsparkGuardMode, UpstreamAdmissionMode},
    dspark_guard::{
        AttestedEngineCoreIncarnation, DsparkCounters, DsparkGuard, IncarnationOutcome,
        MAX_PROMETHEUS_BYTES, Observation, ParseFailure, WindowOutcome, parse_prometheus,
    },
    dspark_guard_store::{DsparkGuardStore, DsparkGuardStorePolicy},
    exact_route_inventory::ExactRouteInventory,
    exact_shadow::ExactRouteSnapshot,
    idle_drain::{IdleDrainDecision, IdleDrainPolicy, UpstreamDrainState, UpstreamObservation},
    journal::{RouteAnnotations, RouteJournal},
    kv_consumer::SharedFencedInventory,
    metrics::Metrics,
    prepare::PreparedRequest,
    router::{Decision, LoadGuard, Router},
    session::OpaqueSession,
    session_affinity::SessionAffinity,
    shims::{self, Endpoint},
    tokenizer::{CompatibilityAdmission, ExactTokens, TokenizerObserver},
    usage::{Accumulator, feed_sse_chunk},
};

const MAX_REQUEST_BODY: usize = 64 << 20;
const MAX_PROBE_BODY: usize = 64 << 10;
const MAX_CONCURRENT_UPSTREAM_PROBES: usize = 8;
/// How recently a real request must have completed on an upstream for a failed
/// readiness probe to be treated as evidence about the probe rather than about
/// the engine.
///
/// The probe competes with request traffic for the same engine capacity, so a
/// saturated engine stops answering `/v1/models` inside the five-second probe
/// budget long before it stops serving. Because a fleet saturates together,
/// believing those probes fences every replica at once. Thirty seconds is two
/// probe intervals plus one probe budget: long enough that a single starved
/// round cannot fence a replica that is demonstrably serving, short enough that
/// an engine which actually died stops being credited after two probe rounds.
/// The request path is unaffected — a failed *request* still fences its
/// upstream immediately, so this only delays fencing an engine that no longer
/// answers anything by at most this window.
const RECENT_SERVING_SUCCESS_WINDOW_MS: u64 = 30_000;
/// Upstreams a request may try while failing open. With every replica marked
/// down there is no evidence about which one is best, and retrying all of them
/// multiplies the latency of a genuinely dead fleet by the replica count. One
/// alternate keeps the "a busy engine still answers" property while bounding
/// the cost of the case where nothing answers.
const FAIL_OPEN_MAX_ATTEMPTS: usize = 2;
const STREAM_BUFFER_CHUNKS: usize = 8;
const DSPARK_METRICS_TIMEOUT: Duration = Duration::from_secs(2);
const DSPARK_RELIABILITY_STATES: &[&str] = &[
    "disabled",
    "baseline",
    "healthy",
    "idle",
    "suspect",
    "would_quarantine",
    "quarantined",
    "persistence_failure",
    "unavailable",
];
const DSPARK_WINDOW_OUTCOMES: &[&str] = &[
    "baseline",
    "clean",
    "zero_acceptance",
    "idle",
    "counter_reset",
    "inconsistent",
    "oversized",
    "invalid_utf8",
    "too_many_lines",
    "line_too_long",
    "invalid_expected_positions",
    "malformed",
    "missing",
    "partial",
    "non_finite",
    "duplicate",
    "multiple_series",
    "mismatched_labels",
    "unexpected_position",
    "capacity",
    "overflow",
];

fn dspark_poll_interval(period: Duration) -> Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

fn dspark_upstream_commitment(upstream: &Url) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"mini-dynamo-dspark-upstream-v1\0");
    digest.update(upstream.as_str().trim_end_matches('/').as_bytes());
    digest.finalize().into()
}

#[derive(Clone)]
pub struct Proxy {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    client: reqwest::Client,
    metrics: Arc<Metrics>,
    router: Arc<Router>,
    exact_inventories: Arc<[ExactRouteInventory]>,
    journal: RouteJournal,
    session_affinity: SessionAffinity,
    tokenizer: TokenizerObserver,
    dspark_guards: Arc<[DsparkGuardReplica]>,
    dspark_guard_store: Option<Arc<DsparkGuardStore>>,
    /// Idle-drain policy and the monotonic origin its millisecond clock is
    /// measured from. Absent unless the policy is enabled, so the default build
    /// carries no extra per-request work.
    idle_drain: Option<IdleDrainState>,
    /// Monotonic origin for the serving-liveness clock below. Separate from the
    /// idle-drain origin because that state is absent in the default build.
    origin: Instant,
    /// Per-upstream evidence that real serving traffic completed recently.
    upstream_liveness: Arc<[UpstreamLiveness]>,
    /// Whether the last routing decision had to fail open. Only used to publish
    /// and log the transition once instead of once per request.
    failing_open: AtomicBool,
}

/// Timestamp of the most recent completed response from one upstream, on the
/// `Inner::origin` monotonic clock and offset by one so that the zero slot can
/// mean "nothing has completed since this process started".
struct UpstreamLiveness {
    last_success_ms: AtomicU64,
}

impl UpstreamLiveness {
    fn record(&self, now_ms: u64) {
        self.last_success_ms
            .store(now_ms.saturating_add(1), AtomicOrdering::Relaxed);
    }

    fn served_within(&self, now_ms: u64, window_ms: u64) -> bool {
        match self.last_success_ms.load(AtomicOrdering::Relaxed) {
            0 => false,
            stamp => now_ms.saturating_sub(stamp - 1) <= window_ms,
        }
    }
}

impl Inner {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

struct IdleDrainState {
    policy: Mutex<IdleDrainPolicy>,
    origin: Instant,
}

impl IdleDrainState {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    admission_mode: &'static str,
    healthy_replicas: usize,
    total_replicas: usize,
    replicas: Vec<ReplicaHealth>,
}

#[derive(Serialize)]
struct ReplicaHealth {
    index: usize,
    healthy: bool,
    reliability_state: &'static str,
    quarantined: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility_attested: Option<bool>,
    inflight: usize,
    load_units: usize,
    approximate_index_entries: usize,
    /// Idle-drain state. Reported alongside, never folded into, `healthy`: a
    /// parked replica is reachable and healthy, it is simply not being routed
    /// to, and an operator has to be able to tell those apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_drain: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exact_inventory: Option<ExactInventoryHealth>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReliabilityState {
    Disabled,
    Baseline,
    Healthy,
    Idle,
    Suspect,
    WouldQuarantine,
    Quarantined,
    PersistenceFailure,
    Unavailable,
}

impl ReliabilityState {
    const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Baseline => "baseline",
            Self::Healthy => "healthy",
            Self::Idle => "idle",
            Self::Suspect => "suspect",
            Self::WouldQuarantine => "would_quarantine",
            Self::Quarantined => "quarantined",
            Self::PersistenceFailure => "persistence_failure",
            Self::Unavailable => "unavailable",
        }
    }
}

struct DsparkGuardReplica {
    mode: DsparkGuardMode,
    inner: Mutex<DsparkGuardReplicaInner>,
}

struct DsparkGuardReplicaInner {
    guard: DsparkGuard,
    probe_healthy: bool,
    state: ReliabilityState,
    durable_fence: DurableFenceState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurableFenceState {
    Ready,
    StartupUncertain,
    AwaitingAttestation,
    PendingQuarantine([u8; 32]),
    RearmPersistenceFailure,
}

impl DurableFenceState {
    const fn blocks_serving(self) -> bool {
        !matches!(self, Self::Ready)
    }

    const fn persistence_failed(self) -> bool {
        matches!(
            self,
            Self::StartupUncertain | Self::PendingQuarantine(_) | Self::RearmPersistenceFailure
        )
    }
}

struct GuardApplication {
    observation: Observation,
    state: ReliabilityState,
    quarantine_reason: Option<&'static str>,
    persistence_failure: Option<&'static str>,
}

#[derive(Clone, Copy)]
struct GuardPublication<'a> {
    router: &'a Router,
    metrics: &'a Metrics,
    upstream: usize,
    upstream_label: &'a str,
    store: Option<&'a DsparkGuardStore>,
}

#[derive(Clone, Copy, Default)]
struct GuardTransition {
    rearmed: bool,
    quarantine_reason: Option<&'static str>,
    persistence_failure: Option<&'static str>,
}

impl GuardTransition {
    fn merge(&mut self, next: Self) {
        self.rearmed |= next.rearmed;
        if next.quarantine_reason.is_some() {
            self.quarantine_reason = next.quarantine_reason;
        }
        if next.persistence_failure.is_some() {
            self.persistence_failure = next.persistence_failure;
        }
    }
}

#[derive(Clone, Copy)]
struct ReliabilitySnapshot {
    state: ReliabilityState,
    quarantined: bool,
    effective_healthy: bool,
}

impl DsparkGuardReplica {
    fn new(
        config: &Config,
        probe_healthy: bool,
        restored_quarantine: Option<[u8; 32]>,
        startup_uncertain: bool,
    ) -> anyhow::Result<Self> {
        let minimum_proposed_tokens = u64::try_from(config.dspark_guard_min_proposed_tokens)
            .context("DSpark proposed-token threshold exceeds u64")?;
        let mut guard = DsparkGuard::new(
            config.dspark_guard_expected_positions,
            config.dspark_guard_consecutive_windows,
            minimum_proposed_tokens,
        )
        .map_err(|error| anyhow::anyhow!("invalid DSpark guard configuration: {error:?}"))?;
        if let Some(commitment) = restored_quarantine {
            guard.restore_quarantine(AttestedEngineCoreIncarnation::from_commitment(commitment));
        }
        Ok(Self {
            mode: config.dspark_guard_mode,
            inner: Mutex::new(DsparkGuardReplicaInner {
                guard,
                probe_healthy,
                state: if startup_uncertain {
                    ReliabilityState::PersistenceFailure
                } else if restored_quarantine.is_some() {
                    ReliabilityState::Quarantined
                } else if config.dspark_guard_mode == DsparkGuardMode::Off {
                    ReliabilityState::Disabled
                } else {
                    ReliabilityState::Baseline
                },
                durable_fence: if startup_uncertain {
                    DurableFenceState::StartupUncertain
                } else {
                    DurableFenceState::Ready
                },
            }),
        })
    }

    fn status(&self) -> ReliabilitySnapshot {
        let inner = self.inner.lock();
        inner.snapshot()
    }

    fn publish_probe_health(
        &self,
        router: &Router,
        metrics: &Metrics,
        upstream: usize,
        upstream_label: &str,
        healthy: bool,
    ) -> bool {
        let mut inner = self.inner.lock();
        inner.probe_healthy = healthy;
        inner.publish_effective_health(router, metrics, upstream, upstream_label)
    }

    fn apply(
        &self,
        publication: GuardPublication<'_>,
        sample: Result<DsparkCounters, ParseFailure>,
        attested_engine_core: Option<[u8; 32]>,
    ) -> GuardApplication {
        let mut inner = self.inner.lock();
        let mut transition = inner.reconcile_engine_core(
            publication.upstream,
            attested_engine_core,
            publication.store,
        );
        let observation = inner.guard.observe(sample);
        if self.mode == DsparkGuardMode::Quarantine
            && observation.threshold_met
            && !inner.guard.quarantined()
            && inner.durable_fence == DurableFenceState::Ready
        {
            transition.merge(inner.enforce_quarantine(
                publication.upstream,
                attested_engine_core,
                publication.store,
            ));
        }
        inner.state = if inner.durable_fence.persistence_failed() {
            ReliabilityState::PersistenceFailure
        } else if inner.guard.quarantined() {
            ReliabilityState::Quarantined
        } else if transition.rearmed || observation.outcome == WindowOutcome::Baseline {
            ReliabilityState::Baseline
        } else {
            match observation.outcome {
                WindowOutcome::Clean => ReliabilityState::Healthy,
                WindowOutcome::Idle => ReliabilityState::Idle,
                WindowOutcome::ZeroAcceptance if observation.threshold_met => {
                    ReliabilityState::WouldQuarantine
                }
                WindowOutcome::ZeroAcceptance => ReliabilityState::Suspect,
                WindowOutcome::Baseline => ReliabilityState::Baseline,
                WindowOutcome::CounterReset
                | WindowOutcome::Inconsistent
                | WindowOutcome::Parse(_) => ReliabilityState::Unavailable,
            }
        };
        inner.publish_effective_health(
            publication.router,
            publication.metrics,
            publication.upstream,
            publication.upstream_label,
        );
        GuardApplication {
            observation,
            state: inner.state,
            quarantine_reason: transition.quarantine_reason,
            persistence_failure: transition.persistence_failure,
        }
    }
}

impl DsparkGuardReplicaInner {
    fn reconcile_engine_core(
        &mut self,
        upstream: usize,
        commitment: Option<[u8; 32]>,
        store: Option<&DsparkGuardStore>,
    ) -> GuardTransition {
        let Some(commitment) = commitment else {
            return GuardTransition::default();
        };
        let incarnation = AttestedEngineCoreIncarnation::from_commitment(commitment);
        if self.durable_fence == DurableFenceState::StartupUncertain {
            if store
                .is_some_and(|store| store.persist_uncertain_fence(upstream, commitment).is_ok())
            {
                self.guard.restore_quarantine(incarnation);
                self.durable_fence = DurableFenceState::Ready;
                return GuardTransition {
                    quarantine_reason: Some("unclean_restart"),
                    ..GuardTransition::default()
                };
            }
            return GuardTransition {
                persistence_failure: Some("quarantine"),
                ..GuardTransition::default()
            };
        }
        if self.durable_fence == DurableFenceState::AwaitingAttestation {
            self.durable_fence = DurableFenceState::Ready;
            return self.enforce_quarantine(upstream, Some(commitment), store);
        }
        if let DurableFenceState::PendingQuarantine(pending) = self.durable_fence {
            if pending != commitment {
                self.durable_fence = DurableFenceState::Ready;
                return GuardTransition {
                    rearmed: self.guard.attest_incarnation(incarnation)
                        == IncarnationOutcome::Rearmed,
                    ..GuardTransition::default()
                };
            }
            if store.is_some_and(|store| store.persist_quarantine(upstream, commitment).is_ok()) {
                self.guard.restore_quarantine(incarnation);
                self.durable_fence = DurableFenceState::Ready;
                return GuardTransition {
                    quarantine_reason: Some("zero_acceptance"),
                    ..GuardTransition::default()
                };
            }
            return GuardTransition {
                persistence_failure: Some("quarantine"),
                ..GuardTransition::default()
            };
        }
        if !self.guard.quarantined() {
            return GuardTransition {
                rearmed: self.guard.attest_incarnation(incarnation) == IncarnationOutcome::Rearmed,
                ..GuardTransition::default()
            };
        }
        let Some((store, quarantined)) = store.and_then(|store| {
            store
                .quarantined_engine_core(upstream)
                .map(|quarantined| (store, quarantined))
        }) else {
            self.durable_fence = DurableFenceState::RearmPersistenceFailure;
            return GuardTransition {
                persistence_failure: Some("rearm"),
                ..GuardTransition::default()
            };
        };
        if quarantined == commitment {
            let _ = self.guard.attest_incarnation(incarnation);
            self.durable_fence = DurableFenceState::Ready;
            return GuardTransition::default();
        }
        if store
            .persist_rearm(upstream, quarantined, commitment)
            .is_err()
        {
            self.durable_fence = DurableFenceState::RearmPersistenceFailure;
            return GuardTransition {
                persistence_failure: Some("rearm"),
                ..GuardTransition::default()
            };
        }
        self.durable_fence = DurableFenceState::Ready;
        GuardTransition {
            rearmed: self.guard.attest_incarnation(incarnation) == IncarnationOutcome::Rearmed,
            ..GuardTransition::default()
        }
    }

    fn enforce_quarantine(
        &mut self,
        upstream: usize,
        commitment: Option<[u8; 32]>,
        store: Option<&DsparkGuardStore>,
    ) -> GuardTransition {
        let Some(commitment) = commitment else {
            self.durable_fence = DurableFenceState::AwaitingAttestation;
            if let Some(store) = store {
                let _ = store.poison_runtime(upstream);
            }
            return GuardTransition {
                quarantine_reason: Some("unattested_threshold"),
                ..GuardTransition::default()
            };
        };
        let Some(store) = store else {
            self.durable_fence = DurableFenceState::PendingQuarantine(commitment);
            return GuardTransition {
                persistence_failure: Some("quarantine"),
                ..GuardTransition::default()
            };
        };
        if store.persist_quarantine(upstream, commitment).is_ok() {
            self.durable_fence = DurableFenceState::Ready;
            self.guard
                .restore_quarantine(AttestedEngineCoreIncarnation::from_commitment(commitment));
            return GuardTransition {
                quarantine_reason: Some("zero_acceptance"),
                ..GuardTransition::default()
            };
        }
        self.durable_fence = DurableFenceState::PendingQuarantine(commitment);
        GuardTransition {
            persistence_failure: Some("quarantine"),
            ..GuardTransition::default()
        }
    }

    fn snapshot(&self) -> ReliabilitySnapshot {
        let quarantined = self.guard.quarantined() || self.durable_fence.blocks_serving();
        ReliabilitySnapshot {
            state: self.state,
            quarantined,
            effective_healthy: self.probe_healthy && !quarantined,
        }
    }

    /// Publish both serving authority and its public gauge while the owning
    /// per-replica lock is held. This prevents an older guard/probe result from
    /// overwriting a newer health decision after it releases the lock.
    fn publish_effective_health(
        &self,
        router: &Router,
        metrics: &Metrics,
        upstream: usize,
        upstream_label: &str,
    ) -> bool {
        let effective =
            self.probe_healthy && !self.guard.quarantined() && !self.durable_fence.blocks_serving();
        router.set_healthy(upstream, effective);
        metrics
            .upstream_up
            .with_label_values(&[upstream_label])
            .set(f64::from(effective));
        effective
    }
}

#[derive(Serialize)]
struct ExactInventoryHealth {
    trusted: bool,
    resident_blocks: usize,
    resident_tokens: usize,
}

struct InflightGuard(GaugeHandle);

#[derive(Clone)]
struct GaugeHandle(prometheus::Gauge);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.0.dec();
    }
}

struct RoutedLoad {
    guard: Option<LoadGuard>,
    router: Arc<Router>,
    metrics: Arc<Metrics>,
    upstream: usize,
    label: String,
}

impl RoutedLoad {
    fn publish(&self) {
        if let Some((inflight, load, _, _)) = self.router.state(self.upstream) {
            self.metrics
                .upstream_inflight
                .with_label_values(&[&self.label])
                .set(usize_to_f64(inflight));
            self.metrics
                .upstream_load_units
                .with_label_values(&[&self.label])
                .set(usize_to_f64(load));
        }
    }

    fn reduce_to(&mut self, units: usize) {
        if self
            .guard
            .as_mut()
            .is_some_and(|guard| guard.reduce_to(units))
        {
            self.publish();
        }
    }
}

impl Drop for RoutedLoad {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.publish();
    }
}

fn initialize_dspark_guard_metrics(
    metrics: &Metrics,
    replica: &str,
    state: ReliabilityState,
    expected_positions: usize,
) {
    for label in DSPARK_RELIABILITY_STATES {
        metrics
            .dspark_guard_state
            .with_label_values(&[replica, label])
            .set(f64::from(*label == state.label()));
    }
    for outcome in DSPARK_WINDOW_OUTCOMES {
        metrics
            .dspark_guard_windows
            .with_label_values(&[replica, outcome])
            .inc_by(0.0);
    }
    for reason in ["zero_acceptance", "unclean_restart", "unattested_threshold"] {
        metrics
            .dspark_guard_quarantines
            .with_label_values(&[replica, reason])
            .inc_by(0.0);
    }
    for operation in ["quarantine", "rearm"] {
        metrics
            .dspark_guard_persistence_failures
            .with_label_values(&[replica, operation])
            .inc_by(0.0);
    }
    metrics
        .dspark_guard_measurement_available
        .with_label_values(&[replica])
        .set(0.0);
    metrics
        .dspark_guard_strict_acceptance
        .with_label_values(&[replica])
        .set(0.0);
    metrics
        .dspark_guard_effective_tokens_per_step
        .with_label_values(&[replica])
        .set(0.0);
    for position in 0..expected_positions {
        metrics
            .dspark_guard_position_acceptance
            .with_label_values(&[replica, &position.to_string()])
            .set(0.0);
    }
}

/// Publishes every replica's startup reliability and admission state. In
/// compatibility mode the replicas are fenced after their guards publish, so a
/// scrape taken during construction can never see an admitted replica that has
/// not yet attested.
fn publish_initial_replica_state(
    config: &Config,
    metrics: &Metrics,
    router: &Router,
    guards: &[DsparkGuardReplica],
    initial_probe_health: bool,
) {
    for (index, guard) in guards.iter().enumerate() {
        let replica = index.to_string();
        initialize_dspark_guard_metrics(
            metrics,
            &replica,
            guard.status().state,
            config.dspark_guard_expected_positions,
        );
        let upstream_label = config.upstreams[index].as_str().trim_end_matches('/');
        guard.publish_probe_health(router, metrics, index, upstream_label, initial_probe_health);
    }
    if config.upstream_admission_mode != UpstreamAdmissionMode::Compatibility {
        return;
    }
    for (index, upstream) in config.upstreams.iter().enumerate() {
        router.set_healthy(index, false);
        let label = upstream.as_str().trim_end_matches('/');
        metrics.upstream_up.with_label_values(&[label]).set(0.0);
        metrics
            .upstream_compatibility_admitted
            .with_label_values(&[label])
            .set(0.0);
    }
}

impl Proxy {
    /// Builds the proxy and its optional tokenizer workers.
    ///
    /// # Errors
    ///
    /// Returns an error when explicit local tokenizer initialization or the
    /// `DSpark` reliability configuration fails.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        config: Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        router: Arc<Router>,
        inventories: Arc<[SharedFencedInventory]>,
    ) -> anyhow::Result<Self> {
        Self::new_with_exact_inventories(
            config,
            client,
            metrics,
            router,
            inventories
                .iter()
                .cloned()
                .map(ExactRouteInventory::direct)
                .collect(),
        )
    }

    /// Builds the proxy with an independent exact-route inventory backend.
    /// Health and routing report the same selected direct or snapshot authority.
    ///
    /// # Errors
    ///
    /// Returns an error when explicit local tokenizer initialization or the
    /// `DSpark` reliability configuration fails.
    pub fn new_with_exact_inventories(
        config: Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        router: Arc<Router>,
        exact_inventories: Arc<[ExactRouteInventory]>,
    ) -> anyhow::Result<Self> {
        let journal = RouteJournal::new(config.route_journal);
        let session_affinity = SessionAffinity::new(&config, Arc::clone(&metrics));
        let tokenizer = TokenizerObserver::with_exact_inventories(
            &config,
            client.clone(),
            Arc::clone(&metrics),
            Arc::clone(&exact_inventories),
        )?;
        Self::from_parts(
            config,
            client,
            metrics,
            router,
            exact_inventories,
            journal,
            session_affinity,
            tokenizer,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        config: Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        router: Arc<Router>,
        exact_inventories: Arc<[ExactRouteInventory]>,
        journal: RouteJournal,
        session_affinity: SessionAffinity,
        tokenizer: TokenizerObserver,
    ) -> anyhow::Result<Self> {
        let initial_probe_health =
            config.upstream_admission_mode != UpstreamAdmissionMode::Compatibility;
        let upstream_commitments = config
            .upstreams
            .iter()
            .map(dspark_upstream_commitment)
            .collect::<Vec<_>>();
        let dspark_guard_store = if config.dspark_guard_mode == DsparkGuardMode::Quarantine {
            let path = config
                .dspark_guard_state_path
                .as_deref()
                .context("DSpark quarantine state path is missing")?;
            Some(Arc::new(
                DsparkGuardStore::open(
                    path,
                    DsparkGuardStorePolicy {
                        owner_uid: config.dspark_guard_state_owner_uid,
                        group_gid: config.dspark_guard_state_group_gid,
                    },
                    &upstream_commitments,
                )
                .context("open durable DSpark quarantine state")?,
            ))
        } else {
            None
        };
        let dspark_guards: Arc<[DsparkGuardReplica]> = (0..config.upstreams.len())
            .map(|index| {
                DsparkGuardReplica::new(
                    &config,
                    initial_probe_health,
                    dspark_guard_store
                        .as_ref()
                        .and_then(|store| store.quarantined_engine_core(index)),
                    dspark_guard_store
                        .as_ref()
                        .is_some_and(|store| store.requires_uncertain_fence(index)),
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into();
        publish_initial_replica_state(
            &config,
            metrics.as_ref(),
            &router,
            &dspark_guards,
            initial_probe_health,
        );
        let idle_drain = config.idle_drain.mode.enabled().then(|| {
            let origin = Instant::now();
            IdleDrainState {
                policy: Mutex::new(IdleDrainPolicy::new(
                    config.idle_drain,
                    config.upstreams.len(),
                    0,
                )),
                origin,
            }
        });
        let upstream_liveness = (0..config.upstreams.len())
            .map(|_| UpstreamLiveness {
                last_success_ms: AtomicU64::new(0),
            })
            .collect::<Vec<_>>()
            .into();
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                client,
                metrics,
                router,
                exact_inventories,
                journal,
                session_affinity,
                tokenizer,
                dspark_guards,
                dspark_guard_store,
                idle_drain,
                origin: Instant::now(),
                upstream_liveness,
                failing_open: AtomicBool::new(false),
            }),
        })
    }

    #[must_use]
    pub fn router(&self) -> &Arc<Router> {
        &self.inner.router
    }

    /// Start the bounded comparison phase after all marked serving requests
    /// have completed and their KV events have had the normal settle window.
    #[must_use]
    pub fn start_shadow_soak(&self, headers: &HeaderMap) -> Response<Body> {
        let Some(token) = self.inner.config.upstream_token.as_deref() else {
            return text_error(StatusCode::NOT_FOUND, "not found");
        };
        if !bearer_matches(headers, token) {
            return text_error(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        let result = self.inner.tokenizer.start_shadow_soak();
        let status = match result {
            crate::shadow_soak::StartResult::Started => StatusCode::ACCEPTED,
            crate::shadow_soak::StartResult::Disabled => StatusCode::NOT_FOUND,
            crate::shadow_soak::StartResult::NotReady => StatusCode::CONFLICT,
            crate::shadow_soak::StartResult::QueueClosed => StatusCode::SERVICE_UNAVAILABLE,
        };
        text_error(status, result.label())
    }

    pub async fn handle(State(proxy): State<Self>, request: Request<Body>) -> Response<Body> {
        proxy.serve(request).await
    }

    /// Reports aggregate readiness and every replica's serving state without
    /// exposing upstream hostnames. One healthy replica is sufficient to
    /// serve, but the response is marked degraded until all are healthy.
    #[allow(clippy::unused_async)] // Axum handlers return futures.
    pub async fn health(State(proxy): State<Self>) -> Response<Body> {
        let replicas = (0..proxy.inner.config.upstreams.len())
            .filter_map(|index| {
                proxy.inner.router.state(index).map(
                    |(inflight, load_units, approximate_index_entries, _router_healthy)| {
                        let exact_inventory =
                            proxy.inner.exact_inventories.get(index).map(|inventory| {
                                let status = inventory.status();
                                ExactInventoryHealth {
                                    trusted: status.trusted,
                                    resident_blocks: status.resident_blocks,
                                    resident_tokens: status.resident_tokens,
                                }
                            });
                        let compatibility_attested = (proxy.inner.config.upstream_admission_mode
                            == UpstreamAdmissionMode::Compatibility)
                            .then(|| {
                                proxy
                                    .inner
                                    .tokenizer
                                    .compatibility_attested(index)
                                    .unwrap_or(false)
                            });
                        let reliability = proxy.inner.dspark_guards.get(index).map_or(
                            ReliabilitySnapshot {
                                state: ReliabilityState::Unavailable,
                                quarantined: false,
                                effective_healthy: false,
                            },
                            DsparkGuardReplica::status,
                        );
                        let healthy =
                            reliability.effective_healthy && compatibility_attested.unwrap_or(true);
                        let idle_drain = proxy.inner.idle_drain.as_ref().and_then(|state| {
                            state
                                .policy
                                .lock()
                                .state(index)
                                .map(UpstreamDrainState::label)
                        });
                        ReplicaHealth {
                            index,
                            healthy,
                            reliability_state: reliability.state.label(),
                            quarantined: reliability.quarantined,
                            compatibility_attested,
                            inflight,
                            load_units,
                            approximate_index_entries,
                            idle_drain,
                            exact_inventory,
                        }
                    },
                )
            })
            .collect::<Vec<_>>();
        let healthy_replicas = replicas.iter().filter(|replica| replica.healthy).count();
        let status = match healthy_replicas {
            0 => "unhealthy",
            healthy if healthy == replicas.len() => "ok",
            _ => "degraded",
        };
        let response = HealthResponse {
            status,
            admission_mode: upstream_admission_label(proxy.inner.config.upstream_admission_mode),
            healthy_replicas,
            total_replicas: replicas.len(),
            replicas,
        };
        let code = if healthy_replicas == 0 {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        };
        let body = serde_json::to_vec(&response).unwrap_or_else(|_| {
            br#"{"status":"unhealthy","admission_mode":"unknown","healthy_replicas":0,"total_replicas":0,"replicas":[]}"#.to_vec()
        });
        Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| Response::new(Body::from("health response unavailable")))
    }

    #[allow(clippy::too_many_lines)]
    async fn serve(&self, request: Request<Body>) -> Response<Body> {
        let started = Instant::now();
        let (parts, inbound_body) = request.into_parts();
        if parts.uri.path() == "/v1/mini-dynamo/identity"
            || parts.uri.path().starts_with("/v1/mini-dynamo/identity/")
        {
            return text_error(StatusCode::NOT_FOUND, "not found");
        }
        let capture_shadow_soak = match shadow_soak_capture_requested(
            &parts.headers,
            self.inner.config.upstream_token.as_deref(),
        ) {
            Ok(capture) => capture,
            Err(status) => return text_error(status, "shadow soak capture unauthorized"),
        };
        if capture_shadow_soak && !self.inner.tokenizer.shadow_soak_status().enabled {
            return text_error(StatusCode::NOT_FOUND, "not found");
        }
        let opaque_session = opaque_session_id(&parts.headers);
        let endpoint = shims::endpoint(parts.uri.path());
        let endpoint_label = endpoint.label();
        let Ok(raw_body) = to_bytes(inbound_body, MAX_REQUEST_BODY).await else {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large or unreadable",
            );
        };
        let prepare_tokenizer_body = self.inner.tokenizer.wants_payload(endpoint, raw_body.len());
        if capture_shadow_soak && (!prepare_tokenizer_body || endpoint != Endpoint::Chat) {
            return json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "request is outside the exact-token capture window",
            );
        }
        let canary_assignment =
            self.inner
                .tokenizer
                .assign_canary(endpoint, prepare_tokenizer_body, opaque_session);
        let prepared = PreparedRequest::with_tokenizer(
            endpoint,
            &raw_body,
            self.inner.config.max_tokens_strip,
            &self.inner.router,
            prepare_tokenizer_body,
        );
        let output_limit = prepared.output_limit;
        let tokenizer_body = prepared.tokenizer_body.clone();
        let pre_route_tokens = if prepare_tokenizer_body {
            self.inner
                .tokenizer
                .prepare_pre_route(endpoint, tokenizer_body.as_ref())
                .await
        } else {
            None
        };
        let approximate_decision = prepared.route(&self.inner.router);
        let session_affinity =
            self.inner
                .session_affinity
                .observe(endpoint, opaque_session, &approximate_decision);
        let pending_shadow_source = if capture_shadow_soak {
            let Some(tokens) = &pre_route_tokens else {
                self.inner
                    .tokenizer
                    .record_shadow_soak_tokenizer_unavailable();
                return shadow_soak_retryable_error("tokenizer_unavailable");
            };
            match self
                .inner
                .tokenizer
                .prepare_shadow_soak_source(tokens, &approximate_decision)
            {
                Ok(source) => Some(source),
                Err(crate::shadow_soak::CaptureResult::AttestationChanged) => {
                    return shadow_soak_retryable_error("attestation_changed");
                }
                Err(result) => {
                    return json_error(StatusCode::SERVICE_UNAVAILABLE, result.label());
                }
            }
        } else {
            None
        };
        let mut decision = approximate_decision.clone();
        if let Some(tokens) = &pre_route_tokens {
            self.inner.tokenizer.route_pre_route(
                endpoint,
                tokens,
                prepared.body.len(),
                canary_assignment,
                &mut decision,
            );
        }
        let exact_route_snapshot =
            prepare_tokenizer_body.then(|| self.inner.tokenizer.capture_route(&decision));
        let fingerprints = if endpoint == Endpoint::Other {
            Vec::new()
        } else {
            prepared.fingerprints
        };
        let body = Bytes::from(prepared.body);
        self.inner
            .metrics
            .request_bytes
            .with_label_values(&[endpoint_label])
            .observe(usize_to_f64(body.len()));
        self.inner.metrics.inflight.inc();
        let inflight_guard = InflightGuard(GaugeHandle(self.inner.metrics.inflight.clone()));
        // Recorded before dispatch so a parked replica resumes on the request
        // that arrives during an idle window, not one round trip later.
        self.observe_idle_drain_activity();

        self.record_decision(&decision);
        let journal_sequence = self.inner.journal.start(
            endpoint_label,
            body.len(),
            &approximate_decision,
            &self.inner.config,
            RouteAnnotations {
                served_chosen: decision.candidate_state.first().map(|state| state.index),
                exact_canary: canary_assignment,
                session_affinity,
                output_limit,
            },
        );

        // Carry each candidate's own reservation rather than re-deriving it in
        // the retry loop: a failover target must acquire and journal its own
        // units, never the originally selected candidate's.
        let mut serving_candidates = decision
            .candidates
            .iter()
            .filter_map(|candidate| {
                decision
                    .candidate_state
                    .iter()
                    .find(|state| state.index == *candidate)
                    .filter(|state| state.healthy)
                    .map(|state| (*candidate, state.request_load_units))
            })
            .collect::<Vec<_>>();
        // Nothing is healthy. Shedding here converts a fleet that is merely
        // saturated — the state in which its readiness probes starve first —
        // into a total outage, so dispatch anyway: a busy engine still answers,
        // and a genuinely dead one fails this one request, which is no worse
        // than the 503 the caller would otherwise already have received.
        let failing_open = serving_candidates.is_empty();
        if failing_open {
            serving_candidates = self.fail_open_candidates(&decision);
        }
        self.publish_fail_open(failing_open && !serving_candidates.is_empty());
        let mut last_error = None;
        let mut selected = None;
        for (attempt, &(candidate, units)) in serving_candidates.iter().enumerate() {
            let url = upstream_url(&self.inner.config.upstreams[candidate], &parts.uri);
            let mut outbound = self
                .inner
                .client
                .request(parts.method.clone(), url)
                .body(body.clone());
            outbound = outbound.headers(filtered_headers(&parts.headers));
            let Some(load) = self.acquire_for_dispatch(candidate, units, failing_open) else {
                continue;
            };
            if failing_open {
                self.inner
                    .metrics
                    .route_fail_open_dispatches
                    .with_label_values(&[&self.upstream_label(candidate)])
                    .inc();
            }
            match outbound.send().await {
                Ok(response)
                    if matches!(
                        response.status(),
                        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
                    ) && attempt + 1 < serving_candidates.len() =>
                {
                    self.publish_upstream_health(candidate, false);
                    self.record_upstream_request(candidate, response.status());
                    drop(load);
                }
                Ok(response) => {
                    if attempt > 0 {
                        tracing::warn!(
                            from = decision.candidates[0],
                            to = candidate,
                            "upstream failover"
                        );
                    }
                    selected = Some((candidate, response, load, units));
                    break;
                }
                Err(error) => {
                    last_error = Some(upstream_error_reason(&error));
                    self.publish_upstream_health(candidate, false);
                    drop(load);
                }
            }
        }

        let Some((upstream, response, load_guard, request_load_units)) = selected else {
            let reason = last_error.unwrap_or("no_healthy_upstream");
            let status = match reason {
                "timeout" => StatusCode::GATEWAY_TIMEOUT,
                "no_healthy_upstream" => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::BAD_GATEWAY,
            };
            self.record_error(endpoint_label, reason, status, started.elapsed());
            self.inner.journal.finish(
                journal_sequence,
                started.elapsed(),
                None,
                None,
                "upstream_error",
                None,
                None,
                status.as_u16(),
                0,
                &Accumulator::default(),
            );
            drop(inflight_guard);
            return json_error(status, "upstream unavailable");
        };

        let status = response.status();
        if capture_shadow_soak
            && (upstream != approximate_decision.candidates[0] || !status.is_success())
        {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "capture source did not complete on its primary route",
            );
        }
        let pre_route_tokens = pre_route_tokens.map(|tokens| tokens.tokens);
        let is_models = parts.method == Method::GET
            && parts
                .uri
                .path()
                .trim_end_matches('/')
                .ends_with("/v1/models")
            && status == StatusCode::OK;
        if is_models {
            return self
                .serve_models(
                    endpoint_label,
                    upstream,
                    response,
                    request_load_units,
                    load_guard,
                    inflight_guard,
                    journal_sequence,
                    started,
                )
                .await;
        }

        let streaming = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        let mut builder = Response::builder().status(status);
        copy_response_headers(
            builder.headers_mut().expect("response headers"),
            response.headers(),
        );
        builder
            .headers_mut()
            .expect("response headers")
            .insert("x-mini-dynamo-upstream", upstream.into());

        let (sender, receiver) = mpsc::channel(STREAM_BUFFER_CHUNKS);
        let proxy = self.clone();
        tokio::spawn(async move {
            proxy
                .relay(
                    sender,
                    response,
                    endpoint,
                    upstream,
                    status,
                    streaming,
                    fingerprints,
                    tokenizer_body,
                    pre_route_tokens,
                    prepare_tokenizer_body,
                    exact_route_snapshot,
                    decision,
                    pending_shadow_source,
                    request_load_units,
                    load_guard,
                    inflight_guard,
                    journal_sequence,
                    started,
                )
                .await;
        });
        builder
            .body(Body::from_stream(ReceiverStream::new(receiver)))
            .expect("valid upstream response")
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn relay(
        &self,
        sender: mpsc::Sender<Result<Bytes, io::Error>>,
        response: reqwest::Response,
        endpoint: Endpoint,
        upstream: usize,
        status: StatusCode,
        streaming: bool,
        fingerprints: Vec<u64>,
        tokenizer_body: Option<Bytes>,
        pre_route_tokens: Option<ExactTokens>,
        tokenizer_selected: bool,
        exact_route_snapshot: Option<ExactRouteSnapshot>,
        decision: Decision,
        pending_shadow_source: Option<crate::shadow_soak::ShadowSoakSource>,
        request_load_units: usize,
        mut load_guard: RoutedLoad,
        _inflight_guard: InflightGuard,
        journal_sequence: Option<u64>,
        started: Instant,
    ) {
        let endpoint_label = endpoint.label();
        let mut usage = Accumulator::default();
        let mut parse_buffer = Vec::new();
        let mut bytes_out = 0_usize;
        let mut first_byte = None;
        let mut first_token = None;
        let mut result = "complete";
        let mut stream = response.bytes_stream();
        loop {
            let item = tokio::select! {
                biased;
                () = sender.closed() => {
                    result = "client_disconnect";
                    self.inner
                        .metrics
                        .client_disconnects
                        .with_label_values(&[endpoint_label])
                        .inc();
                    break;
                }
                item = stream.next() => item,
            };
            let Some(item) = item else {
                break;
            };
            match item {
                Ok(chunk) => {
                    let received = started.elapsed();
                    first_byte.get_or_insert(received);
                    bytes_out += chunk.len();
                    if streaming {
                        feed_sse_chunk(&mut usage, &mut parse_buffer, &chunk);
                        if usage.generated && first_token.is_none() {
                            first_token = Some(received);
                            if self.inner.config.route_phase_aware_load {
                                // A generated token is the local, protocol-visible proof that
                                // prompt prefill has completed. Retain one decode unit while
                                // releasing only the size-weighted prefill reservation. This is
                                // a colocated accounting analogue of the phase separation studied
                                // by DistServe: https://arxiv.org/abs/2401.09670
                                load_guard.reduce_to(1);
                            }
                        }
                    } else {
                        parse_buffer.extend_from_slice(&chunk);
                    }
                    if sender.send(Ok(chunk)).await.is_err() {
                        result = "client_disconnect";
                        self.inner
                            .metrics
                            .client_disconnects
                            .with_label_values(&[endpoint_label])
                            .inc();
                        break;
                    }
                }
                Err(error) => {
                    result = "upstream_read_error";
                    let reason = upstream_error_reason(&error);
                    self.inner
                        .metrics
                        .upstream_errors
                        .with_label_values(&[endpoint_label, reason])
                        .inc();
                    let _ = sender.send(Err(io::Error::other(error))).await;
                    break;
                }
            }
        }
        if result == "complete" {
            if streaming {
                if !parse_buffer.is_empty() {
                    usage.feed_sse_line(&parse_buffer);
                }
            } else if !parse_buffer.is_empty() {
                usage.feed_json(&parse_buffer);
            }
            self.record_request(endpoint_label, status, streaming, started.elapsed());
            self.inner
                .metrics
                .response_bytes
                .with_label_values(&[endpoint_label])
                .observe(usize_to_f64(bytes_out));
            if let Some(ttft) = first_token.filter(|_| streaming) {
                self.inner
                    .metrics
                    .ttft
                    .with_label_values(&[endpoint_label])
                    .observe(ttft.as_secs_f64());
            }
            if status == StatusCode::OK && endpoint != Endpoint::Other {
                self.record_usage(endpoint_label, &usage, started.elapsed(), first_token);
                self.inner.router.observe(upstream, &fingerprints);
                if tokenizer_selected && let Some(route_snapshot) = exact_route_snapshot {
                    self.inner.tokenizer.submit(
                        endpoint,
                        upstream,
                        tokenizer_body,
                        usage.cached.and_then(f64_to_usize),
                        route_snapshot,
                        pre_route_tokens,
                    );
                }
                if let Some(source) = pending_shadow_source {
                    let capture = self.inner.tokenizer.commit_shadow_soak(source);
                    if capture != crate::shadow_soak::CaptureResult::Accepted {
                        tracing::warn!(outcome = capture.label(), "shadow soak source rejected");
                    }
                }
            }
        }
        self.record_upstream_request(upstream, status);
        self.inner.journal.finish(
            journal_sequence,
            started.elapsed(),
            first_byte,
            first_token,
            result,
            Some(upstream),
            Some(request_load_units),
            status.as_u16(),
            bytes_out,
            &usage,
        );
        tracing::debug!(
            endpoint = endpoint_label,
            status = status.as_u16(),
            upstream,
            outcome = decision.outcome.label(),
            overlap = decision.overlap_blocks,
            total_blocks = decision.total_blocks,
            affinity = decision.affinity_blocks,
            load_units = decision.load_units,
            content_chars = usage.content_chars,
            reasoning_chars = usage.reasoning_chars,
            tool_call_deltas = usage.tool_call_deltas,
            result,
            "request complete"
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve_models(
        &self,
        endpoint: &str,
        upstream: usize,
        response: reqwest::Response,
        request_load_units: usize,
        _load_guard: RoutedLoad,
        _inflight_guard: InflightGuard,
        journal_sequence: Option<u64>,
        started: Instant,
    ) -> Response<Body> {
        match response.bytes().await {
            Ok(bytes) => {
                let result = shims::shrink_advertised_context(
                    &bytes,
                    self.inner.config.advertise_ctx_margin,
                );
                self.record_request(endpoint, StatusCode::OK, false, started.elapsed());
                self.record_upstream_request(upstream, StatusCode::OK);
                self.inner.journal.finish(
                    journal_sequence,
                    started.elapsed(),
                    Some(started.elapsed()),
                    None,
                    "complete",
                    Some(upstream),
                    Some(request_load_units),
                    200,
                    result.len(),
                    &Accumulator::default(),
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .header("x-mini-dynamo-upstream", upstream)
                    .body(Body::from(result))
                    .expect("valid model response")
            }
            Err(error) => {
                let reason = upstream_error_reason(&error);
                self.record_error(endpoint, reason, StatusCode::BAD_GATEWAY, started.elapsed());
                json_error(StatusCode::BAD_GATEWAY, "upstream unavailable")
            }
        }
    }

    /// Candidates a request may still be dispatched to once every upstream is
    /// marked down, in the router's own preference order and bounded by
    /// [`FAIL_OPEN_MAX_ATTEMPTS`].
    fn fail_open_candidates(&self, decision: &Decision) -> Vec<(usize, usize)> {
        decision
            .candidates
            .iter()
            .filter_map(|candidate| {
                decision
                    .candidate_state
                    .iter()
                    .find(|state| state.index == *candidate)
                    .filter(|state| self.may_fail_open(state.index))
                    .map(|state| (*candidate, state.request_load_units))
            })
            .take(FAIL_OPEN_MAX_ATTEMPTS)
            .collect()
    }

    /// Whether a replica that is currently marked down may nevertheless be
    /// dispatched to when nothing is healthy.
    ///
    /// Only probe-derived health may be overridden. A `DSpark` quarantine, an
    /// unmet compatibility admission, and an idle-drain park are deliberate
    /// fences backed by their own evidence: failing open past them would serve
    /// from a replica this process has already decided must not serve, which is
    /// a correctness or safety regression rather than an availability trade.
    fn may_fail_open(&self, upstream: usize) -> bool {
        if self.inner.router.drained(upstream) {
            return false;
        }
        if self
            .inner
            .dspark_guards
            .get(upstream)
            .is_none_or(|guard| guard.status().quarantined)
        {
            return false;
        }
        if self.inner.config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility
            && self.inner.tokenizer.compatibility_attested(upstream) != Some(true)
        {
            return false;
        }
        true
    }

    /// Publishes the fail-open gauge and logs only the transitions, so a
    /// saturated fleet cannot turn one incident into a request-rate log flood.
    ///
    /// This is deliberately a separate series from `ds4proxy_upstream_up`:
    /// dashboards must keep reading that gauge as "this upstream passed its
    /// admission probe", and a fail-open interval is exactly the case where it
    /// reads zero while traffic is still being served.
    fn publish_fail_open(&self, failing_open: bool) {
        if self
            .inner
            .failing_open
            .swap(failing_open, AtomicOrdering::Relaxed)
            == failing_open
        {
            return;
        }
        self.inner
            .metrics
            .route_fail_open
            .set(f64::from(failing_open));
        if failing_open {
            tracing::warn!(
                upstreams = self.inner.config.upstreams.len(),
                "no upstream is healthy; failing open and dispatching anyway"
            );
        } else {
            tracing::info!("a healthy upstream is available again; fail-open routing ended");
        }
    }

    fn acquire_for_dispatch(
        &self,
        upstream: usize,
        units: usize,
        failing_open: bool,
    ) -> Option<RoutedLoad> {
        let guard = if failing_open {
            self.inner.router.acquire_failing_open(upstream, units)
        } else {
            self.inner.router.acquire_if_healthy(upstream, units)
        }?;
        let label = self.upstream_label(upstream);
        if let Some((inflight, load, _, _)) = self.inner.router.state(upstream) {
            self.inner
                .metrics
                .upstream_inflight
                .with_label_values(&[&label])
                .set(usize_to_f64(inflight));
            self.inner
                .metrics
                .upstream_load_units
                .with_label_values(&[&label])
                .set(usize_to_f64(load));
        }
        Some(RoutedLoad {
            guard: Some(guard),
            router: Arc::clone(&self.inner.router),
            metrics: Arc::clone(&self.inner.metrics),
            upstream,
            label,
        })
    }

    fn record_decision(&self, decision: &Decision) {
        self.inner
            .metrics
            .route_decisions
            .with_label_values(&[decision.outcome.label()])
            .inc();
        if decision.total_blocks > 0 {
            self.inner
                .metrics
                .route_overlap
                .observe(usize_to_f64(decision.overlap_blocks));
            self.inner
                .metrics
                .route_affinity
                .observe(usize_to_f64(decision.affinity_blocks));
        }
    }

    fn record_request(&self, endpoint: &str, status: StatusCode, stream: bool, elapsed: Duration) {
        self.inner
            .metrics
            .requests
            .with_label_values(&[
                endpoint,
                status.as_str(),
                if stream { "true" } else { "false" },
            ])
            .inc();
        self.inner
            .metrics
            .duration
            .with_label_values(&[endpoint])
            .observe(elapsed.as_secs_f64());
    }

    fn record_error(&self, endpoint: &str, reason: &str, status: StatusCode, elapsed: Duration) {
        self.inner
            .metrics
            .upstream_errors
            .with_label_values(&[endpoint, reason])
            .inc();
        self.inner
            .metrics
            .requests
            .with_label_values(&[endpoint, status.as_str(), "unknown"])
            .inc();
        self.inner
            .metrics
            .duration
            .with_label_values(&[endpoint])
            .observe(elapsed.as_secs_f64());
    }

    fn record_usage(
        &self,
        endpoint: &str,
        usage: &Accumulator,
        elapsed: Duration,
        first_token: Option<Duration>,
    ) {
        let cache_outcome = usage.cache_outcome();
        self.inner
            .metrics
            .cache_requests
            .with_label_values(&[endpoint, cache_outcome])
            .inc();
        if let Some(first_token) = first_token {
            self.inner
                .metrics
                .cache_ttft
                .with_label_values(&[endpoint, cache_outcome])
                .observe(first_token.as_secs_f64());
        }
        if usage.prompt.is_none() && usage.completion.is_none() {
            self.inner
                .metrics
                .parse_failures
                .with_label_values(&[endpoint])
                .inc();
        }
        if let Some(value) = usage.prompt {
            self.inner
                .metrics
                .prompt_tokens
                .with_label_values(&[endpoint])
                .inc_by(value);
            self.inner
                .metrics
                .context_size
                .with_label_values(&[endpoint])
                .observe(value);
        }
        if let Some(value) = usage.cached {
            self.inner
                .metrics
                .cached_tokens
                .with_label_values(&[endpoint])
                .inc_by(value);
        }
        if let Some(value) = usage.completion {
            self.inner
                .metrics
                .completion_tokens
                .with_label_values(&[endpoint])
                .inc_by(value);
            self.inner
                .metrics
                .output_size
                .with_label_values(&[endpoint])
                .observe(value);
            let decode = first_token.map_or(elapsed, |first| elapsed.saturating_sub(first));
            if decode > Duration::from_millis(500) && value > 8.0 {
                self.inner
                    .metrics
                    .decode_tps
                    .with_label_values(&[endpoint])
                    .observe(value / decode.as_secs_f64());
                self.inner
                    .metrics
                    .tpot
                    .with_label_values(&[endpoint])
                    .observe(decode.as_secs_f64() / (value - 1.0).max(1.0));
            }
        }
        self.inner
            .metrics
            .finish_reasons
            .with_label_values(&[endpoint, shims::finish_reason(&usage.finish_reason)])
            .inc();
    }

    fn record_upstream_request(&self, upstream: usize, status: StatusCode) {
        let label = self.upstream_label(upstream);
        self.inner
            .metrics
            .upstream_requests
            .with_label_values(&[&label, status.as_str()])
            .inc();
        // A server error is how both a shedding engine and a broken one answer,
        // so it is not admitted as liveness evidence. Anything the engine
        // generated itself is.
        if !status.is_server_error() {
            self.note_upstream_serving_success(upstream);
        }
    }

    fn upstream_label(&self, upstream: usize) -> String {
        self.inner.config.upstreams[upstream]
            .as_str()
            .trim_end_matches('/')
            .to_owned()
    }

    pub async fn probe_loop(self) {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            self.probe_round().await;
        }
    }

    /// Periodically re-evaluates the idle-drain policy and publishes its
    /// conclusions. Returns immediately when the policy is disabled.
    pub async fn idle_drain_loop(self) {
        if self.inner.idle_drain.is_none() {
            return;
        }
        let mut interval = dspark_poll_interval(Duration::from_secs(
            u64::try_from(self.inner.config.idle_drain_interval_seconds).unwrap_or(u64::MAX),
        ));
        loop {
            interval.tick().await;
            self.idle_drain_round();
        }
    }

    /// Records that the fleet served a request, resuming any parked replica on
    /// the next evaluation.
    fn observe_idle_drain_activity(&self) {
        if let Some(state) = self.inner.idle_drain.as_ref() {
            let now_ms = state.now_ms();
            state.policy.lock().observe_activity(now_ms);
        }
    }

    /// One evaluation: sample every upstream, tick the policy, then apply the
    /// result to routing and telemetry.
    fn idle_drain_round(&self) {
        let Some(state) = self.inner.idle_drain.as_ref() else {
            return;
        };
        let observations = (0..self.inner.config.upstreams.len())
            .map(|upstream| {
                let (inflight, _, _, healthy) = self
                    .inner
                    .router
                    .state(upstream)
                    .unwrap_or((0, 0, 0, false));
                UpstreamObservation { healthy, inflight }
            })
            .collect::<Vec<_>>();
        let now_ms = state.now_ms();
        let decision = state.policy.lock().tick(now_ms, &observations);
        self.publish_idle_drain(&decision);
    }

    fn publish_idle_drain(&self, decision: &IdleDrainDecision) {
        let metrics = &self.inner.metrics;
        metrics.idle_drain_fleet_idle.set(f64::from(decision.idle));
        for (upstream, intent) in decision.upstreams.iter().enumerate() {
            let label = self.upstream_label(upstream);
            // Routing authority is applied before telemetry so a scrape can
            // never advertise a replica as parked while it is still routable.
            if self.inner.config.idle_drain.mode.fences_routing() {
                self.inner.router.set_drained(upstream, intent.fenced);
            }
            metrics
                .idle_drain_state
                .with_label_values(&[&label])
                .set(intent.state.code());
            metrics
                .idle_drain_desired_running
                .with_label_values(&[&label])
                .set(f64::from(intent.desired_running));
            metrics
                .idle_drain_safe_to_stop
                .with_label_values(&[&label])
                .set(f64::from(intent.safe_to_stop));
        }
        for (upstream, state) in &decision.transitions {
            let label = self.upstream_label(*upstream);
            metrics
                .idle_drain_transitions
                .with_label_values(&[&label, state.label()])
                .inc();
            tracing::info!(
                upstream = *upstream,
                state = state.label(),
                mode = self.inner.config.idle_drain.mode.label(),
                "idle drain transition"
            );
        }
    }

    pub async fn dspark_guard_loop(self) {
        if self.inner.config.dspark_guard_mode == DsparkGuardMode::Off {
            return;
        }
        let mut interval = dspark_poll_interval(Duration::from_millis(
            u64::try_from(self.inner.config.dspark_guard_interval_ms).unwrap_or(u64::MAX),
        ));
        loop {
            interval.tick().await;
            self.dspark_guard_round().await;
        }
    }

    async fn dspark_guard_round(&self) {
        futures_util::stream::iter(0..self.inner.config.upstreams.len())
            .for_each_concurrent(Some(MAX_CONCURRENT_UPSTREAM_PROBES), |upstream| {
                self.poll_dspark_guard(upstream)
            })
            .await;
    }

    async fn poll_dspark_guard(&self, upstream: usize) {
        let sample = match self.fetch_dspark_metrics(upstream).await {
            Ok(body) => parse_prometheus(&body, self.inner.config.dspark_guard_expected_positions),
            Err(failure) => Err(failure),
        };
        let attested_engine_core = self
            .inner
            .tokenizer
            .compatibility_attested(upstream)
            .is_some_and(|ready| ready)
            .then(|| {
                self.inner
                    .tokenizer
                    .compatibility_engine_core_incarnation_commitment(upstream)
            })
            .flatten();
        let Some(guard) = self.inner.dspark_guards.get(upstream) else {
            return;
        };
        let upstream_label = self.upstream_label(upstream);
        let application = guard.apply(
            GuardPublication {
                router: &self.inner.router,
                metrics: self.inner.metrics.as_ref(),
                upstream,
                upstream_label: &upstream_label,
                store: self.inner.dspark_guard_store.as_deref(),
            },
            sample,
            attested_engine_core,
        );
        self.record_dspark_guard(upstream, &application);
        if let Some(reason) = application.quarantine_reason {
            tracing::error!(
                replica = upstream,
                reason,
                "DSpark reliability guard quarantined replica"
            );
        }
        if let Some(operation) = application.persistence_failure {
            tracing::error!(
                replica = upstream,
                operation,
                "DSpark reliability guard persistence failed closed"
            );
        }
    }

    async fn fetch_dspark_metrics(&self, upstream: usize) -> Result<Vec<u8>, ParseFailure> {
        let uri = Uri::from_static("/metrics");
        let url = upstream_url(&self.inner.config.upstreams[upstream], &uri);
        let mut request = self.inner.client.get(url).timeout(DSPARK_METRICS_TIMEOUT);
        if let Some(token) = &self.inner.config.upstream_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|_| ParseFailure::Missing)?;
        if response.status() != StatusCode::OK {
            return Err(ParseFailure::Missing);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PROMETHEUS_BYTES as u64)
        {
            return Err(ParseFailure::Oversized);
        }
        let mut body = Vec::with_capacity(
            response
                .content_length()
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(64 << 10)
                .min(MAX_PROMETHEUS_BYTES),
        );
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ParseFailure::Missing)?;
            if body.len().saturating_add(chunk.len()) > MAX_PROMETHEUS_BYTES {
                return Err(ParseFailure::Oversized);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    fn record_dspark_guard(&self, upstream: usize, application: &GuardApplication) {
        let replica = upstream.to_string();
        self.inner
            .metrics
            .dspark_guard_windows
            .with_label_values(&[replica.as_str(), application.observation.outcome.label()])
            .inc();
        for state in DSPARK_RELIABILITY_STATES {
            self.inner
                .metrics
                .dspark_guard_state
                .with_label_values(&[replica.as_str(), state])
                .set(f64::from(*state == application.state.label()));
        }
        if let Some(reason) = application.quarantine_reason {
            self.inner
                .metrics
                .dspark_guard_quarantines
                .with_label_values(&[replica.as_str(), reason])
                .inc();
        }
        if let Some(operation) = application.persistence_failure {
            self.inner
                .metrics
                .dspark_guard_persistence_failures
                .with_label_values(&[replica.as_str(), operation])
                .inc();
        }
        let measurement_available = application.observation.measurement.is_some();
        self.inner
            .metrics
            .dspark_guard_measurement_available
            .with_label_values(&[replica.as_str()])
            .set(f64::from(measurement_available));
        if let Some(measurement) = &application.observation.measurement {
            self.inner
                .metrics
                .dspark_guard_strict_acceptance
                .with_label_values(&[replica.as_str()])
                .set(counter_ratio(
                    measurement.accepted_tokens,
                    measurement.proposed_tokens,
                ));
            self.inner
                .metrics
                .dspark_guard_effective_tokens_per_step
                .with_label_values(&[replica.as_str()])
                .set(if measurement.draft_steps == 0 {
                    0.0
                } else {
                    1.0 + counter_ratio(measurement.accepted_tokens, measurement.draft_steps)
                });
            for (position, accepted) in measurement.accepted_per_position.iter().enumerate() {
                self.inner
                    .metrics
                    .dspark_guard_position_acceptance
                    .with_label_values(&[replica.as_str(), &position.to_string()])
                    .set(counter_ratio(*accepted, measurement.draft_steps));
            }
        }
    }

    async fn probe_round(&self) {
        let mut unhealthy = Vec::new();
        let mut healthy = Vec::new();
        for upstream in 0..self.inner.config.upstreams.len() {
            if self
                .inner
                .router
                .state(upstream)
                .is_some_and(|state| state.3)
            {
                healthy.push(upstream);
            } else {
                unhealthy.push(upstream);
            }
        }
        let initially_all_fenced = self.inner.config.upstream_admission_mode
            == UpstreamAdmissionMode::Compatibility
            && healthy.is_empty();
        futures_util::stream::iter(unhealthy)
            .for_each_concurrent(Some(MAX_CONCURRENT_UPSTREAM_PROBES), |upstream| {
                self.probe(upstream)
            })
            .await;
        if initially_all_fenced {
            return;
        }
        for upstream in healthy {
            let healthy_count = (0..self.inner.config.upstreams.len())
                .filter(|index| self.inner.router.state(*index).is_some_and(|state| state.3))
                .count();
            let target_healthy = self
                .inner
                .router
                .state(upstream)
                .is_some_and(|state| state.3);
            if self.inner.config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility
                && healthy_count <= 1
                && target_healthy
            {
                self.probe_with_admission(upstream, false).await;
            } else {
                self.probe(upstream).await;
            }
        }
    }

    async fn probe(&self, upstream: usize) {
        self.probe_with_admission(upstream, true).await;
    }

    async fn probe_with_admission(&self, upstream: usize, fence_before_check: bool) {
        let started = Instant::now();
        let label = self.upstream_label(upstream);
        if self.inner.config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility
            && fence_before_check
        {
            self.publish_upstream_health(upstream, false);
            self.inner
                .metrics
                .upstream_up
                .with_label_values(&[&label])
                .set(0.0);
            self.inner.tokenizer.invalidate_admission(upstream);
        }
        let uri = Uri::from_static("/v1/models");
        let url = upstream_url(&self.inner.config.upstreams[upstream], &uri);
        let mut request = self.inner.client.get(url).timeout(Duration::from_secs(5));
        if let Some(token) = &self.inner.config.upstream_token {
            request = request.bearer_auth(token);
        }
        let result = request.send().await;
        let (mut healthy, mut reason, models_body) = match result {
            Ok(response) if response.status() == StatusCode::OK => {
                if response
                    .content_length()
                    .is_some_and(|size| size > MAX_PROBE_BODY as u64)
                {
                    (false, "response_too_large", None)
                } else {
                    match response.bytes().await {
                        Ok(body) if body.len() <= MAX_PROBE_BODY => (true, "", Some(body)),
                        Ok(_) => (false, "response_too_large", None),
                        Err(error) => (false, upstream_error_reason(&error), None),
                    }
                }
            }
            Ok(_) => (false, "http", None),
            Err(error) => (false, upstream_error_reason(&error), None),
        };
        if let Some(models_body) = models_body {
            if self.inner.config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility {
                let compatibility = self
                    .inner
                    .tokenizer
                    .evaluate_upstream_admission(upstream)
                    .await;
                match compatibility {
                    CompatibilityAdmission::Match => {
                        self.inner.tokenizer.publish_admission(upstream, true);
                    }
                    CompatibilityAdmission::Mismatch => {
                        healthy = false;
                        reason = "compatibility_mismatch";
                    }
                    CompatibilityAdmission::NotConfigured | CompatibilityAdmission::Unavailable => {
                        healthy = false;
                        reason = "compatibility_unavailable";
                    }
                }
                self.mark_probe(upstream, healthy, reason);
                if !healthy {
                    self.inner.tokenizer.invalidate_admission(upstream);
                }
                self.inner
                    .tokenizer
                    .attest_upstream(upstream, &models_body)
                    .await;
            } else {
                self.inner
                    .tokenizer
                    .attest_upstream(upstream, &models_body)
                    .await;
                self.mark_probe(upstream, healthy, reason);
            }
        } else {
            self.mark_probe(upstream, healthy, reason);
            self.inner.tokenizer.invalidate_attestation(upstream);
            if self.inner.config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility {
                self.inner.tokenizer.invalidate_admission(upstream);
            }
        }
        self.inner
            .metrics
            .upstream_probe_time
            .with_label_values(&[&label])
            .set(started.elapsed().as_secs_f64());
    }

    fn mark_probe(&self, upstream: usize, healthy: bool, reason: &str) {
        let suppressed = !healthy && self.probe_failure_is_starvation(upstream, reason);
        let effective_healthy = if suppressed {
            // The probe still failed and is still counted below; what it may not
            // do is fence a replica that has just finished serving a real
            // request, because that request is the stronger liveness evidence.
            self.inner
                .metrics
                .upstream_probe_suppressed
                .with_label_values(&[&self.upstream_label(upstream), reason])
                .inc();
            tracing::debug!(
                upstream,
                reason,
                "readiness probe failed but recent serving traffic proves liveness"
            );
            self.inner
                .router
                .state(upstream)
                .is_some_and(|state| state.3)
        } else {
            self.publish_upstream_health(upstream, healthy)
        };
        let label = self.upstream_label(upstream);
        if healthy && effective_healthy {
            self.inner
                .metrics
                .last_upstream_success
                .with_label_values(&[&label])
                .set(unix_seconds());
        } else {
            self.inner
                .metrics
                .upstream_probe_errors
                .with_label_values(&[
                    label.as_str(),
                    if healthy && !effective_healthy {
                        "dspark_quarantined"
                    } else {
                        reason
                    },
                ])
                .inc();
        }
    }

    /// Whether a failed probe says more about the probe than about the engine.
    ///
    /// Only a transport-level failure qualifies: a timeout or a refused/queued
    /// connection is exactly what a saturated engine produces while it is still
    /// draining its request queue. An answered probe that reported the wrong
    /// thing — a non-200 status, an oversized body, a compatibility mismatch —
    /// is a real statement by a live engine and always fences it. Compatibility
    /// admission never suppresses anything: that mode requires positive, fresh
    /// attestation to serve, so an unreachable engine must fail closed.
    fn probe_failure_is_starvation(&self, upstream: usize, reason: &str) -> bool {
        if self.inner.config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility
            || !matches!(reason, "timeout" | "connect")
        {
            return false;
        }
        self.inner
            .upstream_liveness
            .get(upstream)
            .is_some_and(|liveness| {
                liveness.served_within(self.inner.now_ms(), RECENT_SERVING_SUCCESS_WINDOW_MS)
            })
    }

    /// Records that a real request completed on this upstream.
    ///
    /// A completed response is the same class of liveness evidence as a passing
    /// probe and is strictly fresher, so it both feeds the starvation window
    /// above and restores probe health directly. Compatibility admission is
    /// excluded because there serving authority additionally requires a
    /// positive attestation that only the probe can renew.
    fn note_upstream_serving_success(&self, upstream: usize) {
        let Some(liveness) = self.inner.upstream_liveness.get(upstream) else {
            return;
        };
        liveness.record(self.inner.now_ms());
        if self.inner.config.upstream_admission_mode != UpstreamAdmissionMode::Compatibility
            && !self
                .inner
                .router
                .state(upstream)
                .is_some_and(|state| state.3)
        {
            self.publish_upstream_health(upstream, true);
        }
    }

    fn publish_upstream_health(&self, upstream: usize, probe_healthy: bool) -> bool {
        let upstream_label = self.upstream_label(upstream);
        self.inner.dspark_guards.get(upstream).is_some_and(|guard| {
            guard.publish_probe_health(
                &self.inner.router,
                self.inner.metrics.as_ref(),
                upstream,
                &upstream_label,
                probe_healthy,
            )
        })
    }

    /// Proxies a native Prometheus endpoint selected by opaque upstream ordinal.
    ///
    /// # Panics
    ///
    /// Panics only if constructing a response from constant headers fails.
    pub async fn upstream_metrics(
        State(proxy): State<Self>,
        Path(index): Path<usize>,
    ) -> Response<Body> {
        let Some(base) = proxy.inner.config.upstreams.get(index) else {
            return text_error(StatusCode::NOT_FOUND, "no such upstream");
        };
        let uri = Uri::from_static("/metrics");
        let url = upstream_url(base, &uri);
        match proxy
            .inner
            .client
            .get(url)
            .header("accept-encoding", "identity")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                match response.bytes().await {
                    Ok(body) => Response::builder()
                        .status(status)
                        .header("content-type", "text/plain")
                        .body(Body::from(body))
                        .expect("valid metrics response"),
                    Err(_) => text_error(StatusCode::BAD_GATEWAY, "upstream metrics unavailable"),
                }
            }
            Err(_) => text_error(StatusCode::BAD_GATEWAY, "upstream metrics unavailable"),
        }
    }
}

const fn upstream_admission_label(mode: UpstreamAdmissionMode) -> &'static str {
    match mode {
        UpstreamAdmissionMode::Http => "http",
        UpstreamAdmissionMode::Compatibility => "compatibility",
    }
}

fn upstream_url(base: &Url, uri: &Uri) -> Url {
    let mut url = base.clone();
    let base_path = base.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}{}", uri.path()));
    url.set_query(uri.query());
    url
}

fn filtered_headers(source: &HeaderMap) -> HeaderMap {
    let mut destination = HeaderMap::with_capacity(source.len());
    for (name, value) in source {
        if !hop_header(name)
            && !matches!(name.as_str(), "x-session-id" | "x-mini-dynamo-shadow-soak")
        {
            destination.append(name, value.clone());
        }
    }
    destination
}

fn opaque_session_id(headers: &HeaderMap) -> OpaqueSession<'_> {
    let mut values = headers.get_all("x-session-id").iter();
    let Some(session_id) = values.next().map(axum::http::HeaderValue::as_bytes) else {
        return OpaqueSession::Missing;
    };
    if values.next().is_some() || !(1..=256).contains(&session_id.len()) {
        return OpaqueSession::Invalid;
    }
    OpaqueSession::Valid(session_id)
}

fn copy_response_headers(destination: &mut HeaderMap, source: &HeaderMap) {
    for (name, value) in source {
        if !hop_header(name) {
            destination.append(name, value.clone());
        }
    }
}

fn hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "host"
            | "x-mini-dynamo-upstream"
    )
}

fn upstream_error_reason(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else {
        "protocol"
    }
}

fn json_error(status: StatusCode, message: &str) -> Response<Body> {
    let body = serde_json::json!({"error": {"message": message, "type": status.as_str()}});
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("valid error response")
}

fn shadow_soak_retryable_error(reason: &'static str) -> Response<Body> {
    let body = serde_json::json!({"error": {
        "message": "exact-token capture temporarily unavailable",
        "type": StatusCode::SERVICE_UNAVAILABLE.as_str(),
    }});
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .header("x-mini-dynamo-shadow-soak-retry", reason)
        .body(Body::from(body.to_string()))
        .expect("valid shadow soak retry response")
}

fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all("authorization").iter();
    let matches = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        == Some(expected);
    matches && values.next().is_none()
}

fn shadow_soak_capture_requested(
    headers: &HeaderMap,
    expected_token: Option<&str>,
) -> Result<bool, StatusCode> {
    let mut values = headers.get_all("x-mini-dynamo-shadow-soak").iter();
    let Some(value) = values.next() else {
        return Ok(false);
    };
    if values.next().is_some() || value.as_bytes() != b"capture" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let Some(expected_token) = expected_token else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !bearer_matches(headers, expected_token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(true)
}

fn text_error(status: StatusCode, message: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Body::from(message.to_owned()))
        .expect("valid error response")
}

#[allow(clippy::cast_precision_loss)]
fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

fn f64_to_usize(value: f64) -> Option<usize> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > f64::from(u32::MAX) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(value as usize)
}

#[allow(clippy::cast_precision_loss)]
fn counter_ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fmt::Write as _,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    use axum::{Router as AxumRouter, routing::any};
    use prometheus::Registry;

    use super::*;
    use crate::{
        compat::{
            CompatibilityManifest, EngineIdentity, ModelIdentity, RendererIdentity,
            ServingRuntimeManifest, TokenizerIdentity,
        },
        exact_index::{ExactIndexLimits, FencedExactKvInventory},
        kv_wire::{BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
    };

    struct DropSignal {
        dropped: Arc<AtomicBool>,
        notify: Arc<tokio::sync::Notify>,
    }

    static NEXT_DSPARK_STATE_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDsparkState {
        root: PathBuf,
    }

    impl TestDsparkState {
        fn configure(config: &mut Config) -> Self {
            let root = std::env::temp_dir().join(format!(
                "md-proxy-dspark-state-{}-{}",
                std::process::id(),
                NEXT_DSPARK_STATE_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let state = root.join("state.json");
            fs::write(
                &state,
                b"{\"schema_version\":1,\"runtime_dirty\":false,\"quarantines\":[]}",
            )
            .unwrap();
            fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
            let metadata = fs::symlink_metadata(&root).unwrap();
            config.dspark_guard_state_path = Some(state);
            config.dspark_guard_state_owner_uid = metadata.uid();
            config.dspark_guard_state_group_gid = metadata.gid();
            Self { root }
        }
    }

    impl Drop for TestDsparkState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }
    }

    async fn start_upstream(app: AxumRouter) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{address}")).unwrap(), task)
    }

    fn proxy_for(upstreams: &[Url]) -> Proxy {
        proxy_for_with_inventories(upstreams, Arc::from([]))
    }

    fn proxy_for_with_inventories(
        upstreams: &[Url],
        inventories: Arc<[SharedFencedInventory]>,
    ) -> Proxy {
        let joined = upstreams
            .iter()
            .map(Url::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let config =
            Config::from_lookup(|key| (key == "MD_UPSTREAM").then(|| joined.clone())).unwrap();
        proxy_for_config(config, inventories)
    }

    fn proxy_for_with_token(upstreams: &[Url], token: &str) -> Proxy {
        let joined = upstreams
            .iter()
            .map(Url::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let config = Config::from_lookup(|key| match key {
            "MD_UPSTREAM" => Some(joined.clone()),
            "MD_UPSTREAM_TOKEN" => Some(token.to_owned()),
            _ => None,
        })
        .unwrap();
        proxy_for_config(config, Arc::from([]))
    }

    fn proxy_for_config(config: Config, inventories: Arc<[SharedFencedInventory]>) -> Proxy {
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let router = Arc::new(Router::new(crate::router::RouterConfig {
            upstreams: config.upstreams.clone(),
            alpha: config.route_alpha,
            chunk_bytes: config.route_chunk_bytes,
            max_prefix_bytes: config.route_max_prefix_bytes,
            max_overlap_blocks: config.route_max_overlap_blocks,
            index_capacity: config.route_index_capacity,
            load_unit_bytes: config.route_load_unit_bytes,
            max_load_units: config.route_max_load_units,
            affinity: config.affinity,
        }));
        Proxy::new(config, reqwest::Client::new(), metrics, router, inventories).unwrap()
    }

    /// Builds a proxy with the idle-drain policy configured, using the
    /// shortest windows the config validator permits so tests stay fast.
    fn idle_drain_proxy(upstreams: &[Url], mode: &str) -> Proxy {
        let joined = upstreams
            .iter()
            .map(Url::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let config = Config::from_lookup(|key| match key {
            "MD_UPSTREAM" => Some(joined.clone()),
            "MD_IDLE_DRAIN_MODE" => Some(mode.to_owned()),
            // Shortest window the validator permits, so the injected clock
            // only has to step a little past it.
            "MD_IDLE_DRAIN_IDLE_AFTER_SECONDS" | "MD_IDLE_DRAIN_COOLDOWN_SECONDS" => {
                Some("60".to_owned())
            }
            "MD_IDLE_DRAIN_GRACE_SECONDS" => Some("1".to_owned()),
            _ => None,
        })
        .unwrap();
        proxy_for_config(config, Arc::from([]))
    }

    fn upstreams_pair() -> Vec<Url> {
        vec![
            Url::parse("http://a:8000").unwrap(),
            Url::parse("http://b:8000").unwrap(),
        ]
    }

    /// Advances the policy's own clock without waiting in real time.
    fn advance_idle_drain(proxy: &Proxy, ms: u64) {
        let state = proxy.inner.idle_drain.as_ref().expect("policy enabled");
        let observations = (0..proxy.inner.config.upstreams.len())
            .map(|upstream| {
                let (inflight, _, _, healthy) = proxy
                    .inner
                    .router
                    .state(upstream)
                    .unwrap_or((0, 0, 0, false));
                UpstreamObservation { healthy, inflight }
            })
            .collect::<Vec<_>>();
        let decision = state.policy.lock().tick(ms, &observations);
        proxy.publish_idle_drain(&decision);
    }

    #[tokio::test]
    async fn idle_drain_disabled_by_default_and_carries_no_state() {
        let proxy = proxy_for(&upstreams_pair());
        assert!(proxy.inner.idle_drain.is_none());
        // The loop must return immediately rather than spin when disabled.
        tokio::time::timeout(Duration::from_secs(1), proxy.clone().idle_drain_loop())
            .await
            .expect("disabled drain loop returns");
    }

    #[tokio::test]
    async fn idle_drain_fences_one_replica_from_routing_after_the_idle_window() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "drain");
        // Inside the window nothing is fenced.
        advance_idle_drain(&proxy, 30_000);
        assert!(!proxy.inner.router.drained(0));
        assert!(!proxy.inner.router.drained(1));

        advance_idle_drain(&proxy, 90_000);
        assert!(!proxy.inner.router.drained(0), "one replica stays warm");
        assert!(proxy.inner.router.drained(1));

        // The fenced replica must not be selectable for dispatch.
        assert!(proxy.inner.router.acquire_if_healthy(1, 1).is_none());
        assert!(proxy.inner.router.acquire_if_healthy(0, 1).is_some());
    }

    #[tokio::test]
    async fn idle_drain_observe_mode_never_fences_routing() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "observe");
        advance_idle_drain(&proxy, 90_000);
        advance_idle_drain(&proxy, 120_000);
        // State advanced, but both replicas remain routable.
        assert!(!proxy.inner.router.drained(0));
        assert!(!proxy.inner.router.drained(1));
        assert!(proxy.inner.router.acquire_if_healthy(1, 1).is_some());
    }

    /// Records activity on the policy's injected clock. Production calls
    /// [`Proxy::observe_idle_drain_activity`], which reads the same monotonic
    /// origin the loop ticks from; tests drive both explicitly so the two
    /// cannot disagree.
    fn observe_idle_drain_activity_at(proxy: &Proxy, ms: u64) {
        proxy
            .inner
            .idle_drain
            .as_ref()
            .expect("policy enabled")
            .policy
            .lock()
            .observe_activity(ms);
    }

    #[tokio::test]
    async fn request_activity_resumes_a_fenced_replica() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "drain");
        advance_idle_drain(&proxy, 90_000);
        assert!(proxy.inner.router.drained(1));

        observe_idle_drain_activity_at(&proxy, 90_050);
        advance_idle_drain(&proxy, 90_100);
        assert!(
            !proxy.inner.router.drained(1),
            "traffic must restore capacity immediately"
        );
        assert!(proxy.inner.router.acquire_if_healthy(1, 1).is_some());
    }

    #[tokio::test]
    async fn serving_a_request_records_idle_drain_activity() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "drain");
        let state = proxy.inner.idle_drain.as_ref().expect("policy enabled");
        // The real request path stamps activity from the shared monotonic
        // origin, which must be at or after the policy's construction point.
        proxy.observe_idle_drain_activity();
        let stamped = state.policy.lock().config().mode;
        assert_eq!(stamped, crate::idle_drain::IdleDrainMode::Drain);
        // A tick at that same origin sees a fleet that is not yet idle.
        let observations = vec![
            UpstreamObservation {
                healthy: true,
                inflight: 0,
            };
            2
        ];
        let decision = state.policy.lock().tick(state.now_ms(), &observations);
        assert!(!decision.idle, "activity just recorded cannot be idle");
        assert_eq!(decision.desired_running(), 2);
    }

    #[tokio::test]
    async fn idle_drain_publishes_state_and_stop_intent_metrics() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "drain");
        advance_idle_drain(&proxy, 90_000);
        let metrics = &proxy.inner.metrics;
        assert!((metrics.idle_drain_fleet_idle.get() - 1.0).abs() < f64::EPSILON);
        let parked = metrics
            .idle_drain_state
            .with_label_values(&["http://b:8000"])
            .get();
        assert!((parked - 1.0).abs() < f64::EPSILON, "draining");
        assert!(
            (metrics
                .idle_drain_desired_running
                .with_label_values(&["http://b:8000"])
                .get())
            .abs()
                < f64::EPSILON,
            "policy no longer wants the parked engine running"
        );
        // Still draining, so stopping it is not yet safe.
        assert!(
            (metrics
                .idle_drain_safe_to_stop
                .with_label_values(&["http://b:8000"])
                .get())
            .abs()
                < f64::EPSILON
        );

        // Past the grace period it becomes safe to stop.
        advance_idle_drain(&proxy, 92_000);
        assert!(
            (metrics
                .idle_drain_safe_to_stop
                .with_label_values(&["http://b:8000"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        // The warm replica is never reported stoppable.
        assert!(
            (metrics
                .idle_drain_safe_to_stop
                .with_label_values(&["http://a:8000"])
                .get())
            .abs()
                < f64::EPSILON
        );
    }

    /// The Grafana readiness panel combines `ds4proxy_upstream_up` with
    /// `ds4proxy_idle_drain_state` by matching on the `upstream` label. If the
    /// two ever disagree on the label value the join silently yields nothing
    /// and the panel goes blank, so the contract is pinned here.
    fn upstream_labels(collector: &dyn prometheus::core::Collector) -> Vec<String> {
        let mut values = collector
            .collect()
            .iter()
            .flat_map(prometheus::proto::MetricFamily::get_metric)
            .flat_map(prometheus::proto::Metric::get_label)
            .filter(|label| label.name() == "upstream")
            .map(|label| label.value().to_owned())
            .collect::<Vec<_>>();
        values.sort_unstable();
        values.dedup();
        values
    }

    #[tokio::test]
    async fn idle_drain_metrics_use_the_same_upstream_labels_as_upstream_up() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "drain");
        advance_idle_drain(&proxy, 90_000);
        proxy.publish_upstream_health(0, true);
        proxy.publish_upstream_health(1, true);

        let metrics = &proxy.inner.metrics;
        let up = upstream_labels(&metrics.upstream_up);
        assert_eq!(up, vec!["http://a:8000", "http://b:8000"]);
        assert_eq!(upstream_labels(&metrics.idle_drain_state), up);
        assert_eq!(upstream_labels(&metrics.idle_drain_desired_running), up);
        assert_eq!(upstream_labels(&metrics.idle_drain_safe_to_stop), up);
    }

    #[tokio::test]
    async fn health_reports_drain_state_without_marking_a_parked_replica_unhealthy() {
        let proxy = idle_drain_proxy(&upstreams_pair(), "drain");
        advance_idle_drain(&proxy, 90_000);
        let response = Proxy::health(State(proxy.clone())).await;
        let body = axum::body::to_bytes(response.into_body(), 64 << 10)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let replicas = value["replicas"].as_array().expect("replicas");
        assert_eq!(replicas[0]["idle_drain"], "warm");
        assert_eq!(replicas[1]["idle_drain"], "draining");
        // A parked replica is still healthy; only its routing is withheld.
        assert_eq!(replicas[1]["healthy"], true);
        assert_eq!(value["status"], "ok");
    }

    fn guarded_proxy(upstreams: &[Url]) -> (Proxy, TestDsparkState) {
        let joined = upstreams
            .iter()
            .map(Url::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let mut config = Config::from_lookup(|key| match key {
            "MD_UPSTREAM" => Some(joined.clone()),
            "MD_DSPARK_GUARD_MODE" => Some("observe".to_owned()),
            _ => None,
        })
        .unwrap();
        // Production config requires compatibility admission before this mode;
        // the integration test injects the already-attested commitment below.
        config.dspark_guard_mode = DsparkGuardMode::Quarantine;
        let state = TestDsparkState::configure(&mut config);
        (proxy_for_config(config, Arc::from([])), state)
    }

    fn dspark_counters(window: u64) -> DsparkCounters {
        DsparkCounters::synthetic(window, window * 256, 0, vec![0; 5].into_boxed_slice())
    }

    fn quarantine_replica(proxy: &Proxy, upstream: usize, commitment: u8) {
        let guard = &proxy.inner.dspark_guards[upstream];
        let incarnation = Some([commitment; 32]);
        let upstream_label = proxy.upstream_label(upstream);
        for window in 0..=3 {
            let application = guard.apply(
                GuardPublication {
                    router: &proxy.inner.router,
                    metrics: proxy.inner.metrics.as_ref(),
                    upstream,
                    upstream_label: &upstream_label,
                    store: proxy.inner.dspark_guard_store.as_deref(),
                },
                Ok(dspark_counters(window)),
                incarnation,
            );
            proxy.record_dspark_guard(upstream, &application);
        }
        let status = guard.status();
        assert_eq!(status.state, ReliabilityState::Quarantined);
        assert!(status.quarantined);
        assert!(!status.effective_healthy);
    }

    fn proxy_for_config_with_manifest(config: Config, manifest: CompatibilityManifest) -> Proxy {
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let router = Arc::new(Router::new(crate::router::RouterConfig {
            upstreams: config.upstreams.clone(),
            alpha: config.route_alpha,
            chunk_bytes: config.route_chunk_bytes,
            max_prefix_bytes: config.route_max_prefix_bytes,
            max_overlap_blocks: config.route_max_overlap_blocks,
            index_capacity: config.route_index_capacity,
            load_unit_bytes: config.route_load_unit_bytes,
            max_load_units: config.route_max_load_units,
            affinity: config.affinity,
        }));
        let client = reqwest::Client::new();
        let tokenizer = TokenizerObserver::with_test_attestation(
            &config,
            client.clone(),
            Arc::clone(&metrics),
            manifest,
            test_serving_runtime_manifest(),
        );
        let journal = RouteJournal::new(config.route_journal);
        let session_affinity = SessionAffinity::new(&config, Arc::clone(&metrics));
        Proxy::from_parts(
            config,
            client,
            metrics,
            router,
            Arc::from([]),
            journal,
            session_affinity,
            tokenizer,
        )
        .unwrap()
    }

    fn test_compatibility_manifest() -> CompatibilityManifest {
        CompatibilityManifest {
            schema_version: 1,
            model: ModelIdentity {
                id: "model".to_owned(),
                root: "root".to_owned(),
                max_model_len: 4096,
            },
            engine: EngineIdentity {
                version: "v1".to_owned(),
                image_digest: format!("sha256:{}", "a".repeat(64)),
            },
            tokenizer: TokenizerIdentity {
                sha256: "b".repeat(64),
            },
            renderer: RendererIdentity {
                profile: "profile".to_owned(),
            },
            admitted_request_classes: vec!["plain".to_owned()],
            goldens: Vec::new(),
        }
    }

    fn test_serving_runtime_manifest() -> ServingRuntimeManifest {
        let mut runtime: ServingRuntimeManifest = serde_json::from_slice(include_bytes!(
            "../compat/deepseek-v4-r34-serving-runtime.json"
        ))
        .unwrap();
        runtime.compatibility_manifest_sha256 = "c".repeat(64);
        runtime
    }

    fn test_serving_identity(digest: &str) -> serde_json::Value {
        let runtime = test_serving_runtime_manifest();
        serde_json::json!({
            "schema_version": 3,
            "incarnation": {
                "frontend": "boot-1:1:10",
                "engine_core": ["boot-1:2:20"],
            },
            "model": {"id": "model", "root": "root", "max_model_len": 4096},
            "engine": {
                "version": "v1",
                "image_digest": format!("sha256:{digest}"),
                "core_process_count": 1,
                "kv_events": {
                    "enable_kv_cache_events": true,
                    "publisher": "zmq",
                    "endpoint": "tcp://*:5557",
                    "replay_endpoint": "tcp://*:5558",
                    "buffer_steps": 10000,
                    "hwm": 100_000,
                    "max_queue_size": 100_000,
                    "topic": "",
                },
            },
            "tokenizer": {"sha256": "b".repeat(64)},
            "renderer": {"profile": "profile"},
            "runtime": {
                "argv_sha256": runtime.process.argv_sha256,
                "environment_sha256": runtime.process.environment_sha256,
                "packages_sha256": runtime.process.packages_sha256,
                "artifacts_sha256": runtime.process.artifacts_sha256,
            },
        })
    }

    fn trusted_inventory(tokens: &[u32]) -> SharedFencedInventory {
        let events = (!tokens.is_empty())
            .then(|| {
                KvEvent::BlockStored(BlockStored {
                    block_hashes: vec![ExternalBlockHash::Unsigned(1)],
                    parent_block_hash: None,
                    token_ids: tokens.to_vec(),
                    block_size: tokens.len(),
                    group_idx: Some(0),
                    kv_cache_spec_kind: None,
                    kv_cache_spec_sliding_window: None,
                    medium: Some("GPU".to_owned()),
                    locality: Some("LOCAL".to_owned()),
                    lora_name: None,
                    cache_namespace: None,
                    has_extra_keys: false,
                })
            })
            .into_iter()
            .collect();
        let mut inventory = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        inventory
            .ingest_live(
                0,
                &KvEventBatch {
                    timestamp: 1.0,
                    events,
                    data_parallel_rank: Some(0),
                },
            )
            .unwrap();
        Arc::new(parking_lot::RwLock::new(inventory))
    }

    #[test]
    fn strips_hop_and_route_headers() {
        let mut source = HeaderMap::new();
        source.insert("authorization", "Bearer okay".parse().unwrap());
        source.insert("connection", "close".parse().unwrap());
        source.insert("x-mini-dynamo-upstream", "secret".parse().unwrap());
        source.insert("x-session-id", "private-session".parse().unwrap());
        source.insert("x-mini-dynamo-shadow-soak", "capture".parse().unwrap());
        let result = filtered_headers(&source);
        assert!(result.contains_key("authorization"));
        assert!(!result.contains_key("connection"));
        assert!(!result.contains_key("x-mini-dynamo-upstream"));
        assert!(!result.contains_key("x-session-id"));
        assert!(!result.contains_key("x-mini-dynamo-shadow-soak"));
    }

    #[tokio::test]
    async fn atomic_identity_control_path_is_never_publicly_proxied() {
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let upstream = AxumRouter::new().fallback(any(move || {
            let observed = Arc::clone(&observed);
            async move {
                observed.fetch_add(1, Ordering::Relaxed);
                Response::new(Body::from("private identity"))
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        for uri in [
            "/v1/mini-dynamo/identity",
            "/v1/mini-dynamo/identity/",
            "/v1/mini-dynamo/identity/private",
        ] {
            let request = Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            assert_eq!(proxy.serve(request).await.status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        task.abort();
    }

    #[test]
    fn shadow_soak_bearer_requires_one_exact_authorization_value() {
        let mut headers = HeaderMap::new();
        assert!(!bearer_matches(&headers, "secret"));
        headers.insert("authorization", "Basic secret".parse().unwrap());
        assert!(!bearer_matches(&headers, "secret"));
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(!bearer_matches(&headers, "secret"));
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(bearer_matches(&headers, "secret"));
        headers.append("authorization", "Bearer secret".parse().unwrap());
        assert!(!bearer_matches(&headers, "secret"));
    }

    #[test]
    fn shadow_soak_marker_is_exact_single_and_bearer_authenticated() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            shadow_soak_capture_requested(&headers, Some("secret")),
            Ok(false)
        );
        headers.insert("x-mini-dynamo-shadow-soak", "wrong".parse().unwrap());
        assert_eq!(
            shadow_soak_capture_requested(&headers, Some("secret")),
            Err(StatusCode::BAD_REQUEST)
        );
        headers.insert("x-mini-dynamo-shadow-soak", "capture".parse().unwrap());
        assert_eq!(
            shadow_soak_capture_requested(&headers, Some("secret")),
            Err(StatusCode::UNAUTHORIZED)
        );
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert_eq!(
            shadow_soak_capture_requested(&headers, Some("secret")),
            Ok(true)
        );
        headers.append("x-mini-dynamo-shadow-soak", "capture".parse().unwrap());
        assert_eq!(
            shadow_soak_capture_requested(&headers, Some("secret")),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn only_explicit_shadow_admission_errors_carry_the_retry_header() {
        let retryable = shadow_soak_retryable_error("tokenizer_unavailable");
        assert_eq!(retryable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            retryable
                .headers()
                .get("x-mini-dynamo-shadow-soak-retry")
                .unwrap(),
            "tokenizer_unavailable"
        );
        let ordinary = json_error(StatusCode::SERVICE_UNAVAILABLE, "upstream unavailable");
        assert!(
            ordinary
                .headers()
                .get("x-mini-dynamo-shadow-soak-retry")
                .is_none()
        );
    }

    #[tokio::test]
    async fn shadow_soak_capture_header_authenticates_and_hides_off_mode() {
        let proxy = proxy_for_with_token(&[Url::parse("http://127.0.0.1:1").unwrap()], "secret");
        let unauthorized = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("x-mini-dynamo-shadow-soak", "capture")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            proxy.serve(unauthorized).await.status(),
            StatusCode::UNAUTHORIZED
        );

        let disabled = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer secret")
            .header("x-mini-dynamo-shadow-soak", "capture")
            .body(Body::empty())
            .unwrap();
        assert_eq!(proxy.serve(disabled).await.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn opaque_session_id_requires_one_bounded_nonempty_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(opaque_session_id(&headers), OpaqueSession::Missing);
        headers.insert("x-session-id", "session-a".parse().unwrap());
        assert_eq!(
            opaque_session_id(&headers),
            OpaqueSession::Valid(b"session-a")
        );
        headers.append("x-session-id", "session-b".parse().unwrap());
        assert_eq!(opaque_session_id(&headers), OpaqueSession::Invalid);

        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "".parse().unwrap());
        assert_eq!(opaque_session_id(&headers), OpaqueSession::Invalid);
        headers.insert("x-session-id", "x".repeat(257).parse().unwrap());
        assert_eq!(opaque_session_id(&headers), OpaqueSession::Invalid);
    }

    #[test]
    fn joins_paths_and_queries_without_losing_base_prefix() {
        let base = Url::parse("http://engine:8000/prefix/").unwrap();
        let uri: Uri = "/v1/chat/completions?x=1".parse().unwrap();
        assert_eq!(
            upstream_url(&base, &uri).as_str(),
            "http://engine:8000/prefix/v1/chat/completions?x=1"
        );
    }

    #[test]
    fn response_usage_records_bounded_cache_outcome_and_ttft() {
        let proxy = proxy_for(&[Url::parse("http://127.0.0.1:1").unwrap()]);
        let usage = Accumulator {
            prompt: Some(100.0),
            cached: Some(64.0),
            completion: Some(10.0),
            finish_reason: "stop".to_owned(),
            ..Accumulator::default()
        };

        proxy.record_usage(
            "chat",
            &usage,
            Duration::from_secs(2),
            Some(Duration::from_millis(500)),
        );

        let requests = proxy
            .inner
            .metrics
            .cache_requests
            .with_label_values(&["chat", "partial"])
            .get();
        assert!((requests - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            proxy
                .inner
                .metrics
                .cache_ttft
                .with_label_values(&["chat", "partial"])
                .get_sample_count(),
            1
        );
    }

    #[tokio::test]
    async fn proxies_sanitized_requests_and_usage_responses() {
        let upstream = AxumRouter::new().fallback(any(|request: Request<Body>| async move {
            let body = to_bytes(request.into_body(), MAX_REQUEST_BODY).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if request.get("max_tokens").is_some()
                || request.get("reasoning_effort").and_then(serde_json::Value::as_str)
                    != Some("none")
            {
                return json_error(StatusCode::BAD_REQUEST, "request was not sanitized");
            }
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
                ))
                .unwrap()
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hello"}],"max_tokens":100000,"reasoning_effort":"none"}"#,
            ))
            .unwrap();
        let response = proxy.serve(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-mini-dynamo-upstream"], "0");
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("prompt_tokens"));
        task.abort();
    }

    /// A vision request must reach the engine with its image intact.
    ///
    /// The sanitizer rewrites the body that is actually forwarded, so a
    /// regression here does not fail the request — it silently answers a
    /// question about an image the model never saw.
    #[tokio::test]
    async fn vision_content_parts_reach_the_upstream_unaltered() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let upstream = {
            let seen = Arc::clone(&seen);
            AxumRouter::new().fallback(any(move |request: Request<Body>| {
                let seen = Arc::clone(&seen);
                async move {
                    let body = to_bytes(request.into_body(), MAX_REQUEST_BODY).await.unwrap();
                    *seen.lock().unwrap() = serde_json::from_slice(&body).unwrap();
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(
                            r#"{"choices":[{"message":{"content":"a cat"},"finish_reason":"stop"}],"usage":{"prompt_tokens":800,"completion_tokens":3}}"#,
                        ))
                        .unwrap()
                }
            }))
        };
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"qwen3.8-27b","messages":[{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"https://example.invalid/cat.png"}}]}]}"#,
            ))
            .unwrap();
        assert_eq!(proxy.serve(request).await.status(), StatusCode::OK);

        let seen = seen.lock().unwrap().clone();
        let content = &seen["messages"][0]["content"];
        assert!(
            content.is_array(),
            "multimodal content was flattened into {content}"
        );
        assert_eq!(content[0]["text"], "what is this?");
        assert_eq!(
            content[1]["image_url"]["url"], "https://example.invalid/cat.png",
            "the image part must survive the proxy"
        );
        task.abort();
    }

    /// A message whose parts are all text is still normalized, so the existing
    /// text path and its fingerprints are unchanged by the vision fix.
    #[tokio::test]
    async fn all_text_content_parts_are_still_flattened() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let upstream = {
            let seen = Arc::clone(&seen);
            AxumRouter::new().fallback(any(move |request: Request<Body>| {
                let seen = Arc::clone(&seen);
                async move {
                    let body = to_bytes(request.into_body(), MAX_REQUEST_BODY)
                        .await
                        .unwrap();
                    *seen.lock().unwrap() = serde_json::from_slice(&body).unwrap();
                    (StatusCode::OK, r#"{"ok":true}"#)
                }
            }))
        };
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}"#,
            ))
            .unwrap();
        assert_eq!(proxy.serve(request).await.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap()["messages"][0]["content"], "ab");
        task.abort();
    }

    #[tokio::test]
    async fn session_affinity_shadow_is_header_private_and_does_not_change_dispatch() {
        let requests_a = Arc::new(AtomicUsize::new(0));
        let requests_b = Arc::new(AtomicUsize::new(0));
        let leaked_session = Arc::new(AtomicBool::new(false));
        let upstream = |requests: Arc<AtomicUsize>| {
            let leaked_session = Arc::clone(&leaked_session);
            AxumRouter::new().fallback(any(move |headers: HeaderMap| {
                let requests = Arc::clone(&requests);
                let leaked_session = Arc::clone(&leaked_session);
                async move {
                    requests.fetch_add(1, Ordering::Relaxed);
                    leaked_session
                        .fetch_or(headers.contains_key("x-session-id"), Ordering::Relaxed);
                    (StatusCode::OK, r#"{"ok":true}"#)
                }
            }))
        };
        let (url_a, task_a) = start_upstream(upstream(Arc::clone(&requests_a))).await;
        let (url_b, task_b) = start_upstream(upstream(Arc::clone(&requests_b))).await;
        let joined = format!("{url_a},{url_b}");
        let values = HashMap::from([
            ("MD_UPSTREAM", joined),
            ("MD_SESSION_AFFINITY_MODE", "shadow".to_owned()),
            (
                "MD_SESSION_AFFINITY_KEY",
                "0123456789abcdef0123456789abcdef".to_owned(),
            ),
        ]);
        let config = Config::from_lookup(|key| values.get(key).cloned()).unwrap();
        let proxy = proxy_for_config(config, Arc::from([]));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            // This golden maps to primary 0, while the first cold approximate
            // rotation selects 1. Shadow must observe but still dispatch to 1.
            .header("x-session-id", "session-c")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        let response = proxy.serve(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-mini-dynamo-upstream"], "1");
        let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(requests_a.load(Ordering::Relaxed), 0);
        assert_eq!(requests_b.load(Ordering::Relaxed), 1);
        assert!(!leaked_session.load(Ordering::Relaxed));
        assert!(
            (proxy
                .inner
                .metrics
                .session_affinity
                .with_label_values(&["chat", "would_prefer_primary"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        task_a.abort();
        task_b.abort();
    }

    #[tokio::test]
    async fn response_usage_drives_unbiased_exact_route_shadow() {
        let upstream = || {
            AxumRouter::new().fallback(any(|request: Request<Body>| async move {
                if request.uri().path() == "/tokenize" {
                    return Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"count":2,"tokens":[3,5]}"#))
                        .unwrap();
                }
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":0}}}"#,
                    ))
                    .unwrap()
            }))
        };
        let (url_a, task_a) = start_upstream(upstream()).await;
        let (url_b, task_b) = start_upstream(upstream()).await;
        let joined = format!("{url_a},{url_b}");
        let values = HashMap::from([
            ("MD_UPSTREAM", joined),
            ("MD_TOKENIZER_MODE", "remote-shadow".to_owned()),
            ("MD_TOKENIZER_MIN_BYTES", "0".to_owned()),
        ]);
        let config = Config::from_lookup(|key| values.get(key).cloned()).unwrap();
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let router = Arc::new(Router::new(crate::router::RouterConfig {
            upstreams: config.upstreams.clone(),
            alpha: config.route_alpha,
            chunk_bytes: config.route_chunk_bytes,
            max_prefix_bytes: config.route_max_prefix_bytes,
            max_overlap_blocks: config.route_max_overlap_blocks,
            index_capacity: config.route_index_capacity,
            load_unit_bytes: config.route_load_unit_bytes,
            max_load_units: config.route_max_load_units,
            affinity: config.affinity,
        }));
        let proxy = Proxy::new(
            config,
            reqwest::Client::new(),
            Arc::clone(&metrics),
            router,
            Arc::from([trusted_inventory(&[3, 5]), trusted_inventory(&[])]),
        )
        .unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hello"}]}"#,
            ))
            .unwrap();
        let response = proxy.serve(request).await;
        assert_eq!(response.headers()["x-mini-dynamo-upstream"], "1");
        let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if metrics
                    .exact_route_shadow
                    .with_label_values(&["remote", "chat", "would_move"])
                    .get()
                    >= 1.0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task_a.abort();
        task_b.abort();
    }

    #[tokio::test]
    async fn fails_over_on_retryable_status() {
        let failing = AxumRouter::new().fallback(any(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        let healthy = AxumRouter::new().fallback(any(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                "{\"ok\":true}",
            )
        }));
        let (healthy_url, healthy_task) = start_upstream(healthy).await;
        let (failing_url, failing_task) = start_upstream(failing).await;
        // The first cold decision starts at ordinal 1, so this exercises 1 -> 0 failover.
        let proxy = proxy_for(&[healthy_url, failing_url]);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        let response = proxy.serve(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-mini-dynamo-upstream"], "0");
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            Bytes::from_static(b"{\"ok\":true}")
        );
        healthy_task.abort();
        failing_task.abort();
    }

    #[tokio::test]
    async fn known_unhealthy_replica_never_receives_serving_traffic() {
        let unhealthy_requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&unhealthy_requests);
        let unhealthy = AxumRouter::new().fallback(any(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                (StatusCode::OK, "should not be called")
            }
        }));
        let healthy = AxumRouter::new().fallback(any(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"ok":true}"#,
            )
        }));
        let (healthy_url, healthy_task) = start_upstream(healthy).await;
        let (unhealthy_url, unhealthy_task) = start_upstream(unhealthy).await;
        let proxy = proxy_for(&[healthy_url, unhealthy_url]);
        proxy.publish_upstream_health(1, false);

        for _ in 0..4 {
            let request = Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .body(Body::from(
                    r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap();
            let response = proxy.serve(request).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-mini-dynamo-upstream"], "0");
            let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        }
        assert_eq!(unhealthy_requests.load(Ordering::Relaxed), 0);
        healthy_task.abort();
        unhealthy_task.abort();
    }

    #[tokio::test]
    async fn health_reports_each_replica_and_requires_one_healthy() {
        let proxy = proxy_for(&[
            Url::parse("http://127.0.0.1:1").unwrap(),
            Url::parse("http://127.0.0.1:2").unwrap(),
        ]);
        proxy.publish_upstream_health(1, false);
        let response = Proxy::health(State(proxy.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["admission_mode"], "http");
        assert_eq!(health["healthy_replicas"], 1);
        assert_eq!(health["total_replicas"], 2);
        assert_eq!(health["replicas"][0]["healthy"], true);
        assert_eq!(health["replicas"][1]["healthy"], false);
        assert_eq!(health["replicas"][0]["reliability_state"], "disabled");
        assert_eq!(health["replicas"][0]["quarantined"], false);
        assert!(health["replicas"][0].get("exact_inventory").is_none());
        assert!(
            health["replicas"][0]
                .get("compatibility_attested")
                .is_none()
        );

        proxy.publish_upstream_health(0, false);
        let response = Proxy::health(State(proxy)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["status"], "unhealthy");
        assert_eq!(health["healthy_replicas"], 0);
    }

    #[tokio::test]
    async fn dspark_quarantine_survives_health_probe_and_fails_over_without_dialing_replica() {
        let quarantined_requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&quarantined_requests);
        let quarantined = AxumRouter::new().fallback(any(move || {
            let observed = Arc::clone(&observed);
            async move {
                observed.fetch_add(1, Ordering::Relaxed);
                (StatusCode::OK, "must not be called")
            }
        }));
        let healthy = AxumRouter::new().fallback(any(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"ok":true}"#,
            )
        }));
        let (healthy_url, healthy_task) = start_upstream(healthy).await;
        let (quarantined_url, quarantined_task) = start_upstream(quarantined).await;
        let (proxy, _state) = guarded_proxy(&[healthy_url, quarantined_url]);
        quarantine_replica(&proxy, 1, 7);

        assert!(!proxy.publish_upstream_health(1, true));
        assert!(!proxy.router().state(1).unwrap().3);
        for _ in 0..4 {
            let response = proxy
                .serve(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/v1/chat/completions")
                        .body(Body::from(
                            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                        ))
                        .unwrap(),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-mini-dynamo-upstream"], "0");
            let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        }
        assert_eq!(quarantined_requests.load(Ordering::Relaxed), 0);

        let response = Proxy::health(State(proxy)).await;
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(!text.contains("127.0.0.1"));
        assert!(!text.contains("070707"));
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["replicas"][1]["reliability_state"], "quarantined");
        assert_eq!(health["replicas"][1]["quarantined"], true);
        healthy_task.abort();
        quarantined_task.abort();
    }

    #[tokio::test]
    async fn all_dspark_quarantined_returns_503_without_dialing_either_replica() {
        let requests = Arc::new(AtomicUsize::new(0));
        let upstream = |requests: Arc<AtomicUsize>| {
            AxumRouter::new().fallback(any(move || {
                let requests = Arc::clone(&requests);
                async move {
                    requests.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::OK, "must not be called")
                }
            }))
        };
        let (url_a, task_a) = start_upstream(upstream(Arc::clone(&requests))).await;
        let (url_b, task_b) = start_upstream(upstream(Arc::clone(&requests))).await;
        let (proxy, _state) = guarded_proxy(&[url_a, url_b]);
        quarantine_replica(&proxy, 0, 1);
        quarantine_replica(&proxy, 1, 2);
        let config = proxy.inner.config.clone();
        drop(proxy);
        let proxy = proxy_for_config(config, Arc::from([]));

        let response = proxy
            .serve(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/chat/completions")
                    .body(Body::from(
                        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        task_a.abort();
        task_b.abort();
    }

    #[test]
    fn durable_quarantine_survives_lb_restart_and_only_changed_core_rearms() {
        let upstreams = [
            Url::parse("http://127.0.0.1:1").unwrap(),
            Url::parse("http://127.0.0.1:2").unwrap(),
        ];
        let (proxy, _state) = guarded_proxy(&upstreams);
        quarantine_replica(&proxy, 0, 7);
        let config = proxy.inner.config.clone();
        drop(proxy);

        let restored = proxy_for_config(config, Arc::from([]));
        let status = restored.inner.dspark_guards[0].status();
        assert_eq!(status.state, ReliabilityState::Quarantined);
        assert!(status.quarantined);
        assert!(!restored.publish_upstream_health(0, true));

        let label = restored.upstream_label(0);
        let same = restored.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &restored.inner.router,
                metrics: restored.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: restored.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(4)),
            Some([7; 32]),
        );
        restored.record_dspark_guard(0, &same);
        assert!(restored.inner.dspark_guards[0].status().quarantined);
        assert!(!restored.router().state(0).unwrap().3);

        let changed = restored.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &restored.inner.router,
                metrics: restored.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: restored.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(5)),
            Some([8; 32]),
        );
        restored.record_dspark_guard(0, &changed);
        let status = restored.inner.dspark_guards[0].status();
        assert_eq!(status.state, ReliabilityState::Baseline);
        assert!(!status.quarantined);
        assert!(status.effective_healthy);

        let config = restored.inner.config.clone();
        drop(restored);
        let reloaded = proxy_for_config(config, Arc::from([]));
        assert!(!reloaded.inner.dspark_guards[0].status().quarantined);
    }

    #[test]
    fn failed_durable_rearm_keeps_the_replica_fenced() {
        let upstreams = [
            Url::parse("http://127.0.0.1:1").unwrap(),
            Url::parse("http://127.0.0.1:2").unwrap(),
        ];
        let (proxy, state) = guarded_proxy(&upstreams);
        quarantine_replica(&proxy, 0, 7);
        fs::set_permissions(&state.root, fs::Permissions::from_mode(0o500)).unwrap();

        let label = proxy.upstream_label(0);
        let application = proxy.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &proxy.inner.router,
                metrics: proxy.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: proxy.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(4)),
            Some([8; 32]),
        );
        proxy.record_dspark_guard(0, &application);
        let status = proxy.inner.dspark_guards[0].status();
        assert_eq!(status.state, ReliabilityState::PersistenceFailure);
        assert!(status.quarantined);
        assert!(!status.effective_healthy);
        assert!(!proxy.router().state(0).unwrap().3);
        let failures = proxy
            .inner
            .metrics
            .dspark_guard_persistence_failures
            .with_label_values(&["0", "rearm"])
            .get();
        assert!((failures - 1.0).abs() < f64::EPSILON);

        fs::set_permissions(&state.root, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn failed_quarantine_add_remains_fenced_across_lb_restart() {
        let upstreams = [Url::parse("http://127.0.0.1:1").unwrap()];
        let (proxy, state) = guarded_proxy(&upstreams);
        let label = proxy.upstream_label(0);
        for window in 0..3 {
            let application = proxy.inner.dspark_guards[0].apply(
                GuardPublication {
                    router: &proxy.inner.router,
                    metrics: proxy.inner.metrics.as_ref(),
                    upstream: 0,
                    upstream_label: &label,
                    store: proxy.inner.dspark_guard_store.as_deref(),
                },
                Ok(dspark_counters(window)),
                Some([7; 32]),
            );
            proxy.record_dspark_guard(0, &application);
        }
        fs::set_permissions(&state.root, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = proxy.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &proxy.inner.router,
                metrics: proxy.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: proxy.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(3)),
            Some([7; 32]),
        );
        proxy.record_dspark_guard(0, &failed);
        assert_eq!(failed.persistence_failure, Some("quarantine"));
        assert!(!proxy.router().state(0).unwrap().3);

        let config = proxy.inner.config.clone();
        fs::set_permissions(&state.root, fs::Permissions::from_mode(0o700)).unwrap();
        drop(proxy);
        let restarted = proxy_for_config(config, Arc::from([]));
        let uncertain = restarted.inner.dspark_guards[0].status();
        assert_eq!(uncertain.state, ReliabilityState::PersistenceFailure);
        assert!(uncertain.quarantined);
        assert!(!restarted.publish_upstream_health(0, true));

        let label = restarted.upstream_label(0);
        let recovered = restarted.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &restarted.inner.router,
                metrics: restarted.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: restarted.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(4)),
            Some([7; 32]),
        );
        restarted.record_dspark_guard(0, &recovered);
        assert_eq!(recovered.quarantine_reason, Some("unclean_restart"));
        assert_eq!(
            restarted.inner.dspark_guards[0].status().state,
            ReliabilityState::Quarantined
        );
        assert!(!restarted.router().state(0).unwrap().3);
    }

    #[test]
    fn threshold_without_current_attestation_fences_probe_race_immediately() {
        let upstreams = [Url::parse("http://127.0.0.1:1").unwrap()];
        let (proxy, _state) = guarded_proxy(&upstreams);
        let label = proxy.upstream_label(0);
        for window in 0..3 {
            let application = proxy.inner.dspark_guards[0].apply(
                GuardPublication {
                    router: &proxy.inner.router,
                    metrics: proxy.inner.metrics.as_ref(),
                    upstream: 0,
                    upstream_label: &label,
                    store: proxy.inner.dspark_guard_store.as_deref(),
                },
                Ok(dspark_counters(window)),
                Some([7; 32]),
            );
            proxy.record_dspark_guard(0, &application);
        }

        // Model the worst ordering: the guard poll read no current
        // attestation, then a successful compatibility probe published health
        // before the poll acquired the per-replica guard lock.
        assert!(proxy.publish_upstream_health(0, true));
        let unresolved = proxy.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &proxy.inner.router,
                metrics: proxy.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: proxy.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(3)),
            None,
        );
        proxy.record_dspark_guard(0, &unresolved);
        assert_eq!(unresolved.quarantine_reason, Some("unattested_threshold"));
        assert!(proxy.inner.dspark_guards[0].status().quarantined);
        assert!(!proxy.router().state(0).unwrap().3);
        assert!(!proxy.publish_upstream_health(0, true));

        let persisted = proxy.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &proxy.inner.router,
                metrics: proxy.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: proxy.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(4)),
            Some([8; 32]),
        );
        proxy.record_dspark_guard(0, &persisted);
        assert_eq!(persisted.quarantine_reason, Some("zero_acceptance"));
        assert_eq!(
            proxy.inner.dspark_guards[0].status().state,
            ReliabilityState::Quarantined
        );
        assert!(!proxy.router().state(0).unwrap().3);

        let same_current_core = proxy.inner.dspark_guards[0].apply(
            GuardPublication {
                router: &proxy.inner.router,
                metrics: proxy.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: proxy.inner.dspark_guard_store.as_deref(),
            },
            Ok(dspark_counters(5)),
            Some([8; 32]),
        );
        proxy.record_dspark_guard(0, &same_current_core);
        assert!(proxy.inner.dspark_guards[0].status().quarantined);
        assert!(!proxy.router().state(0).unwrap().3);
    }

    #[tokio::test(start_paused = true)]
    async fn dspark_poll_interval_delays_instead_of_bursting_after_a_slow_scrape() {
        let mut interval = dspark_poll_interval(Duration::from_secs(1));
        interval.tick().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        interval.tick().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(1), interval.tick())
                .await
                .is_err()
        );
    }

    #[test]
    fn stale_guard_observation_cannot_restore_upstream_metric_after_probe_failure() {
        let upstream = Url::parse("http://127.0.0.1:1").unwrap();
        let proxy = proxy_for(std::slice::from_ref(&upstream));
        let guard = &proxy.inner.dspark_guards[0];
        let label = proxy.upstream_label(0);
        let stale = guard.apply(
            GuardPublication {
                router: &proxy.inner.router,
                metrics: proxy.inner.metrics.as_ref(),
                upstream: 0,
                upstream_label: &label,
                store: None,
            },
            Ok(dspark_counters(0)),
            None,
        );
        assert!(!proxy.publish_upstream_health(0, false));
        proxy.record_dspark_guard(0, &stale);
        assert!(!proxy.router().state(0).unwrap().3);
        assert!(
            proxy
                .inner
                .metrics
                .upstream_up
                .with_label_values(&[upstream.as_str().trim_end_matches('/')])
                .get()
                .abs()
                < f64::EPSILON
        );
    }

    #[tokio::test]
    async fn observe_mode_polls_native_metrics_without_fencing_at_threshold() {
        let samples = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&samples);
        let upstream = AxumRouter::new().fallback(any(move |request: Request<Body>| {
            let observed = Arc::clone(&observed);
            async move {
                if request.uri().path() != "/metrics" {
                    return Response::new(Body::from("ok"));
                }
                let window = observed.fetch_add(1, Ordering::Relaxed);
                let proposed = window * 256;
                let mut body = format!(
                    "vllm:spec_decode_num_drafts_total {window}\n\
                     vllm:spec_decode_num_draft_tokens_total {proposed}\n\
                     vllm:spec_decode_num_accepted_tokens_total 0\n"
                );
                for position in 0..5 {
                    writeln!(
                        body,
                        "vllm:spec_decode_num_accepted_tokens_per_pos_total{{position=\"{position}\"}} 0"
                    )
                    .unwrap();
                }
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(Body::from(body))
                    .unwrap()
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let joined = url.to_string();
        let config = Config::from_lookup(|key| match key {
            "MD_UPSTREAM" => Some(joined.clone()),
            "MD_DSPARK_GUARD_MODE" => Some("observe".to_owned()),
            _ => None,
        })
        .unwrap();
        let proxy = proxy_for_config(config, Arc::from([]));

        for _ in 0..4 {
            proxy.poll_dspark_guard(0).await;
        }
        assert_eq!(samples.load(Ordering::Relaxed), 4);
        assert!(proxy.router().state(0).unwrap().3);
        let status = proxy.inner.dspark_guards[0].status();
        assert_eq!(status.state, ReliabilityState::WouldQuarantine);
        assert!(!status.quarantined);
        assert!(status.effective_healthy);
        let measurement_available = proxy
            .inner
            .metrics
            .dspark_guard_measurement_available
            .with_label_values(&["0"])
            .get();
        let effective = proxy
            .inner
            .metrics
            .dspark_guard_effective_tokens_per_step
            .with_label_values(&["0"])
            .get();
        let position = proxy
            .inner
            .metrics
            .dspark_guard_position_acceptance
            .with_label_values(&["0", "4"])
            .get();
        assert!((measurement_available - 1.0).abs() < f64::EPSILON);
        assert!((effective - 1.0).abs() < f64::EPSILON);
        assert!(position.abs() < f64::EPSILON);
        task.abort();
    }

    #[tokio::test]
    async fn compatibility_admission_starts_fenced_and_fails_closed_without_attestation() {
        let inference_requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&inference_requests);
        let upstream = AxumRouter::new().fallback(any(move |request: Request<Body>| {
            let observed = Arc::clone(&observed);
            async move {
                if request.uri().path() == "/v1/models" {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"data":[{"id":"model"}]}"#))
                        .unwrap();
                }
                observed.fetch_add(1, Ordering::Relaxed);
                Response::new(Body::from("must not be served"))
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let joined = url.as_str().to_owned();
        let mut config =
            Config::from_lookup(|key| (key == "MD_UPSTREAM").then(|| joined.clone())).unwrap();
        // Config parsing requires a real manifest before this mode can be set.
        // Mutating the public structure here proves the proxy still fails closed
        // if an embedding constructs an inconsistent Config directly.
        config.upstream_admission_mode = UpstreamAdmissionMode::Compatibility;
        let proxy = proxy_for_config(config, Arc::from([]));

        assert!(!proxy.router().state(0).unwrap().3);
        let health = Proxy::health(State(proxy.clone())).await;
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(health.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["admission_mode"], "compatibility");

        proxy.probe(0).await;
        assert!(!proxy.router().state(0).unwrap().3);
        assert!(
            (proxy
                .inner
                .metrics
                .upstream_probe_errors
                .with_label_values(&[
                    url.as_str().trim_end_matches('/'),
                    "compatibility_unavailable"
                ])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );

        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        assert_eq!(
            proxy.serve(request).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(inference_requests.load(Ordering::Relaxed), 0);
        task.abort();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One end-to-end admission/fencing/recovery lifecycle.
    async fn compatibility_admission_fences_during_check_mismatch_and_recovers() {
        let identity_matches = Arc::new(AtomicBool::new(true));
        let block_identity = Arc::new(AtomicBool::new(false));
        let identity_entered = Arc::new(tokio::sync::Notify::new());
        let identity_release = Arc::new(tokio::sync::Notify::new());
        let inference_requests = Arc::new(AtomicUsize::new(0));
        let upstream = {
            let identity_matches = Arc::clone(&identity_matches);
            let block_identity = Arc::clone(&block_identity);
            let identity_entered = Arc::clone(&identity_entered);
            let identity_release = Arc::clone(&identity_release);
            let inference_requests = Arc::clone(&inference_requests);
            AxumRouter::new().fallback(any(move |request: Request<Body>| {
                let identity_matches = Arc::clone(&identity_matches);
                let block_identity = Arc::clone(&block_identity);
                let identity_entered = Arc::clone(&identity_entered);
                let identity_release = Arc::clone(&identity_release);
                let inference_requests = Arc::clone(&inference_requests);
                async move {
                    match request.uri().path() {
                        "/v1/models" => Response::new(Body::from(
                            r#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#,
                        )),
                        "/version" => Response::new(Body::from(r#"{"version":"v1"}"#)),
                        "/v1/mini-dynamo/identity" => {
                            if block_identity.load(Ordering::Acquire) {
                                identity_entered.notify_one();
                                identity_release.notified().await;
                            }
                            let digest = if identity_matches.load(Ordering::Acquire) {
                                "a".repeat(64)
                            } else {
                                "c".repeat(64)
                            };
                            Response::new(Body::from(test_serving_identity(&digest).to_string()))
                        }
                        _ => {
                            inference_requests.fetch_add(1, Ordering::Relaxed);
                            Response::new(Body::from(r#"{"ok":true}"#))
                        }
                    }
                }
            }))
        };
        let (url, task) = start_upstream(upstream).await;
        let joined = url.as_str().to_owned();
        let mut config =
            Config::from_lookup(|key| (key == "MD_UPSTREAM").then(|| joined.clone())).unwrap();
        config.upstream_admission_mode = UpstreamAdmissionMode::Compatibility;
        let proxy = proxy_for_config_with_manifest(config, test_compatibility_manifest());

        assert!(!proxy.router().state(0).unwrap().3);
        proxy.probe(0).await;
        assert!(proxy.router().state(0).unwrap().3);
        let first_incarnation = proxy
            .inner
            .tokenizer
            .compatibility_engine_core_incarnation_commitment(0);
        assert!(first_incarnation.is_some());
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        assert_eq!(proxy.serve(request).await.status(), StatusCode::OK);
        assert_eq!(inference_requests.load(Ordering::Relaxed), 1);

        identity_matches.store(false, Ordering::Release);
        block_identity.store(true, Ordering::Release);
        let checking_proxy = proxy.clone();
        let checking = tokio::spawn(async move { checking_proxy.probe(0).await });
        tokio::time::timeout(Duration::from_secs(1), identity_entered.notified())
            .await
            .expect("identity check must start");
        assert!(!proxy.router().state(0).unwrap().3);
        assert_eq!(proxy.inner.tokenizer.compatibility_attested(0), Some(false));
        assert!(
            proxy
                .inner
                .tokenizer
                .compatibility_engine_core_incarnation_commitment(0)
                .is_none()
        );
        identity_release.notify_one();
        checking.await.unwrap();
        block_identity.store(false, Ordering::Release);
        assert!(!proxy.router().state(0).unwrap().3);
        assert!(
            proxy
                .inner
                .tokenizer
                .compatibility_engine_core_incarnation_commitment(0)
                .is_none()
        );

        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        assert_eq!(
            proxy.serve(request).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(inference_requests.load(Ordering::Relaxed), 1);

        identity_matches.store(true, Ordering::Release);
        proxy.probe(0).await;
        assert!(proxy.router().state(0).unwrap().3);
        assert_eq!(
            proxy
                .inner
                .tokenizer
                .compatibility_engine_core_incarnation_commitment(0),
            first_incarnation
        );
        task.abort();
    }

    #[tokio::test]
    async fn compatibility_health_never_reports_a_router_admission_mixed_state() {
        let upstream = AxumRouter::new().fallback(any(|request: Request<Body>| async move {
            match request.uri().path() {
                "/v1/models" => Response::new(Body::from(
                    r#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#,
                )),
                "/version" => Response::new(Body::from(r#"{"version":"v1"}"#)),
                "/v1/mini-dynamo/identity" => Response::new(Body::from(
                    test_serving_identity(&"a".repeat(64)).to_string(),
                )),
                _ => Response::new(Body::empty()),
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let joined = url.as_str().to_owned();
        let mut config =
            Config::from_lookup(|key| (key == "MD_UPSTREAM").then(|| joined.clone())).unwrap();
        config.upstream_admission_mode = UpstreamAdmissionMode::Compatibility;
        let proxy = proxy_for_config_with_manifest(config, test_compatibility_manifest());
        proxy.probe(0).await;
        assert!(proxy.router().state(0).unwrap().3);

        // Simulate the only cross-source snapshot that could otherwise expose
        // a stale router=true together with a newly published admission=false.
        proxy.inner.tokenizer.invalidate_admission(0);
        assert!(proxy.router().state(0).unwrap().3);
        let health = Proxy::health(State(proxy)).await;
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(health.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["replicas"][0]["healthy"], false);
        assert_eq!(health["replicas"][0]["compatibility_attested"], false);
        task.abort();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn compatibility_startup_probes_all_fenced_replicas_concurrently() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let slow_matches = Arc::new(AtomicBool::new(true));
        let fast_matches = Arc::new(AtomicBool::new(true));
        let slow_identity_calls = Arc::new(AtomicUsize::new(0));
        let fast_identity_calls = Arc::new(AtomicUsize::new(0));
        let fast_second_entered = Arc::new(tokio::sync::Notify::new());
        let fast_second_release = Arc::new(tokio::sync::Notify::new());
        let slow = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let slow_matches = Arc::clone(&slow_matches);
            let identity_calls = Arc::clone(&slow_identity_calls);
            AxumRouter::new().fallback(any(move |request: Request<Body>| {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                let slow_matches = Arc::clone(&slow_matches);
                let identity_calls = Arc::clone(&identity_calls);
                async move {
                    match request.uri().path() {
                        "/v1/models" => Response::new(Body::from(
                            r#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#,
                        )),
                        "/version" => Response::new(Body::from(r#"{"version":"v1"}"#)),
                        "/v1/mini-dynamo/identity" => {
                            identity_calls.fetch_add(1, Ordering::Relaxed);
                            entered.notify_one();
                            if identity_calls.load(Ordering::Relaxed) == 1 {
                                release.notified().await;
                            }
                            let digest = if slow_matches.load(Ordering::Acquire) {
                                "a".repeat(64)
                            } else {
                                "c".repeat(64)
                            };
                            Response::new(Body::from(test_serving_identity(&digest).to_string()))
                        }
                        _ => Response::new(Body::empty()),
                    }
                }
            }))
        };
        let fast_calls = Arc::clone(&fast_identity_calls);
        let fast_matches_for_handler = Arc::clone(&fast_matches);
        let fast_entered = Arc::clone(&fast_second_entered);
        let fast_release = Arc::clone(&fast_second_release);
        let fast = AxumRouter::new().fallback(any(move |request: Request<Body>| {
            let fast_calls = Arc::clone(&fast_calls);
            let fast_matches = Arc::clone(&fast_matches_for_handler);
            let fast_entered = Arc::clone(&fast_entered);
            let fast_release = Arc::clone(&fast_release);
            async move {
                match request.uri().path() {
                    "/v1/models" => Response::new(Body::from(
                        r#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#,
                    )),
                    "/version" => Response::new(Body::from(r#"{"version":"v1"}"#)),
                    "/v1/mini-dynamo/identity" => {
                        let call = fast_calls.fetch_add(1, Ordering::Relaxed) + 1;
                        if call == 2 {
                            fast_entered.notify_one();
                            fast_release.notified().await;
                        }
                        let digest = if fast_matches.load(Ordering::Acquire) {
                            "a".repeat(64)
                        } else {
                            "c".repeat(64)
                        };
                        Response::new(Body::from(test_serving_identity(&digest).to_string()))
                    }
                    _ => Response::new(Body::empty()),
                }
            }
        }));
        let (slow_url, slow_task) = start_upstream(slow).await;
        let (fast_url, fast_task) = start_upstream(fast).await;
        let joined = format!("{slow_url},{fast_url}");
        let mut config =
            Config::from_lookup(|key| (key == "MD_UPSTREAM").then(|| joined.clone())).unwrap();
        config.upstream_admission_mode = UpstreamAdmissionMode::Compatibility;
        let proxy = proxy_for_config_with_manifest(config, test_compatibility_manifest());

        let probing_proxy = proxy.clone();
        let probing = tokio::spawn(async move { probing_proxy.probe_round().await });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("slow identity probe must start");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !proxy.router().state(1).unwrap().3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fast peer must admit while the first peer remains blocked");
        assert!(!proxy.router().state(0).unwrap().3);
        release.notify_one();
        probing.await.unwrap();
        assert!(proxy.router().state(0).unwrap().3);
        assert_eq!(slow_identity_calls.load(Ordering::Relaxed), 1);
        assert_eq!(fast_identity_calls.load(Ordering::Relaxed), 1);

        slow_matches.store(false, Ordering::Release);
        proxy.router().set_healthy(0, false);
        let second_proxy = proxy.clone();
        let second_round = tokio::spawn(async move { second_proxy.probe_round().await });
        tokio::time::timeout(Duration::from_secs(1), fast_second_entered.notified())
            .await
            .expect("the sole admitted peer must still receive an atomic recheck");
        assert!(proxy.router().state(1).unwrap().3);
        assert_eq!(proxy.inner.tokenizer.compatibility_attested(1), Some(true));
        let health = Proxy::health(State(proxy.clone())).await;
        assert_eq!(health.status(), StatusCode::OK);
        let body = to_bytes(health.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["replicas"][1]["healthy"], true);
        assert_eq!(health["replicas"][1]["compatibility_attested"], true);
        assert!(
            (proxy
                .inner
                .metrics
                .upstream_compatibility_admitted
                .with_label_values(&[fast_url.as_str().trim_end_matches('/')])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        fast_matches.store(false, Ordering::Release);
        fast_second_release.notify_one();
        second_round.await.unwrap();
        assert!(!proxy.router().state(0).unwrap().3);
        assert!(!proxy.router().state(1).unwrap().3);
        assert_eq!(proxy.inner.tokenizer.compatibility_attested(1), Some(false));
        assert!(
            proxy
                .inner
                .metrics
                .upstream_compatibility_admitted
                .with_label_values(&[fast_url.as_str().trim_end_matches('/')])
                .get()
                .abs()
                < f64::EPSILON
        );
        assert_eq!(slow_identity_calls.load(Ordering::Relaxed), 2);
        assert_eq!(fast_identity_calls.load(Ordering::Relaxed), 2);
        slow_task.abort();
        fast_task.abort();
    }

    #[tokio::test]
    async fn all_fenced_probe_fanout_is_bounded() {
        let identity_calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::clone(&identity_calls);
        let permits = Arc::clone(&release);
        let upstream = AxumRouter::new().fallback(any(move |request: Request<Body>| {
            let calls = Arc::clone(&calls);
            let permits = Arc::clone(&permits);
            async move {
                match request.uri().path() {
                    "/v1/models" => Response::new(Body::from(
                        r#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#,
                    )),
                    "/version" => Response::new(Body::from(r#"{"version":"v1"}"#)),
                    "/v1/mini-dynamo/identity" => {
                        calls.fetch_add(1, Ordering::Relaxed);
                        permits.acquire().await.unwrap().forget();
                        Response::new(Body::from(
                            test_serving_identity(&"a".repeat(64)).to_string(),
                        ))
                    }
                    _ => Response::new(Body::empty()),
                }
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let upstream_count = MAX_CONCURRENT_UPSTREAM_PROBES + 1;
        let joined = (0..upstream_count)
            .map(|_| url.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let mut config =
            Config::from_lookup(|key| (key == "MD_UPSTREAM").then(|| joined.clone())).unwrap();
        config.upstream_admission_mode = UpstreamAdmissionMode::Compatibility;
        let proxy = proxy_for_config_with_manifest(config, test_compatibility_manifest());

        let probing_proxy = proxy.clone();
        let probing = tokio::spawn(async move { probing_proxy.probe_round().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while identity_calls.load(Ordering::Relaxed) < MAX_CONCURRENT_UPSTREAM_PROBES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the bounded probe group must fill");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            identity_calls.load(Ordering::Relaxed),
            MAX_CONCURRENT_UPSTREAM_PROBES
        );

        release.add_permits(upstream_count);
        probing.await.unwrap();
        assert_eq!(identity_calls.load(Ordering::Relaxed), upstream_count);
        assert!((0..upstream_count).all(|index| proxy.router().state(index).unwrap().3));
        task.abort();
    }

    #[tokio::test]
    async fn health_reports_content_free_exact_inventory_by_replica_index() {
        let proxy = proxy_for_with_inventories(
            &[
                Url::parse("http://127.0.0.1:1").unwrap(),
                Url::parse("http://127.0.0.1:2").unwrap(),
            ],
            Arc::from([trusted_inventory(&[1, 2, 3]), trusted_inventory(&[])]),
        );
        let response = Proxy::health(State(proxy)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["replicas"][0]["exact_inventory"]["trusted"], true);
        assert_eq!(
            health["replicas"][0]["exact_inventory"]["resident_blocks"],
            1
        );
        assert_eq!(
            health["replicas"][0]["exact_inventory"]["resident_tokens"],
            3
        );
        assert_eq!(
            health["replicas"][1]["exact_inventory"]["resident_tokens"],
            0
        );
        assert!(
            !String::from_utf8(body.to_vec())
                .unwrap()
                .contains("127.0.0.1")
        );
    }

    #[tokio::test]
    async fn successful_probe_restores_a_failed_replica() {
        let available = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = Arc::clone(&available);
        let upstream = AxumRouter::new().fallback(any(move || {
            let state = Arc::clone(&state);
            async move {
                if state.load(Ordering::Acquire) {
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        r#"{"data":[{"id":"model"}]}"#,
                    )
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("content-type", "application/json")],
                        r#"{"error":"unavailable"}"#,
                    )
                }
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        proxy.probe(0).await;
        assert!(!proxy.router().state(0).unwrap().3);
        available.store(true, Ordering::Release);
        proxy.probe(0).await;
        assert!(proxy.router().state(0).unwrap().3);
        let response = Proxy::health(State(proxy)).await;
        assert_eq!(response.status(), StatusCode::OK);
        task.abort();
    }

    /// Issue #170: at c512 every readiness probe starved at once, the balancer
    /// fenced all eight engines, and 512 of 512 requests were shed in 0.309s
    /// while every engine was still answering. With nothing healthy left to
    /// protect, dispatching is strictly better than shedding.
    #[tokio::test]
    async fn every_upstream_down_fails_open_instead_of_shedding_the_request() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let upstream = AxumRouter::new().fallback(any(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"ok":true}"#,
                )
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        let label = proxy.upstream_label(0);
        proxy.publish_upstream_health(0, false);

        // Failing open must not make the balancer claim the replica is up.
        let health = Proxy::health(State(proxy.clone())).await;
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            proxy
                .inner
                .metrics
                .upstream_up
                .with_label_values(&[&label])
                .get()
                .abs()
                < f64::EPSILON
        );

        let response = proxy
            .serve(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/chat/completions")
                    .body(Body::from(
                        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1 << 20).await.unwrap(),
            Bytes::from_static(br#"{"ok":true}"#)
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert!(
            (proxy
                .inner
                .metrics
                .route_fail_open_dispatches
                .with_label_values(&[&label])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!((proxy.inner.metrics.route_fail_open.get() - 1.0).abs() < f64::EPSILON);

        // The completed request is fresher liveness evidence than the failed
        // probe, so the replica returns to normal admission without waiting for
        // the next probe round, and the fail-open interval ends.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !proxy.router().state(0).unwrap().3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a completed request must restore serving admission");
        let response = proxy
            .serve(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/chat/completions")
                    .body(Body::from(
                        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(proxy.inner.metrics.route_fail_open.get().abs() < f64::EPSILON);
        assert!(
            (proxy
                .inner
                .metrics
                .route_fail_open_dispatches
                .with_label_values(&[&label])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON,
            "a healthy upstream must be routed to normally, not through fail-open"
        );
        task.abort();
    }

    #[tokio::test]
    async fn fail_open_never_dispatches_into_a_parked_replica() {
        let parked_requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&parked_requests);
        let parked = AxumRouter::new().fallback(any(move || {
            let observed = Arc::clone(&observed);
            async move {
                observed.fetch_add(1, Ordering::Relaxed);
                (StatusCode::OK, "must not be called")
            }
        }));
        let serving = AxumRouter::new().fallback(any(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"ok":true}"#,
            )
        }));
        let (serving_url, serving_task) = start_upstream(serving).await;
        let (parked_url, parked_task) = start_upstream(parked).await;
        let proxy = proxy_for(&[serving_url, parked_url]);
        // A parked replica may be stopped by the converging actor at any
        // moment, so availability pressure must not reach it.
        proxy.router().set_drained(1, true);
        proxy.publish_upstream_health(0, false);
        proxy.publish_upstream_health(1, false);

        for _ in 0..4 {
            let response = proxy
                .serve(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/v1/chat/completions")
                        .body(Body::from(
                            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                        ))
                        .unwrap(),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-mini-dynamo-upstream"], "0");
            let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        }
        assert_eq!(parked_requests.load(Ordering::Relaxed), 0);
        serving_task.abort();
        parked_task.abort();
    }

    /// The other half of issue #170: the probe loses its race with request
    /// traffic first, so a starved probe must not be allowed to fence a replica
    /// that is demonstrably still completing requests.
    #[tokio::test(start_paused = true)]
    async fn a_starved_probe_never_fences_a_recently_serving_upstream() {
        let proxy = proxy_for(&[Url::parse("http://127.0.0.1:1").unwrap()]);
        let label = proxy.upstream_label(0);
        proxy.record_upstream_request(0, StatusCode::OK);

        proxy.mark_probe(0, false, "timeout");
        assert!(
            proxy.router().state(0).unwrap().3,
            "a probe timeout alone must not fence a replica that just served"
        );
        proxy.mark_probe(0, false, "connect");
        assert!(proxy.router().state(0).unwrap().3);
        let suppressed = proxy.inner.metrics.upstream_probe_suppressed.clone();
        assert!(
            (suppressed.with_label_values(&[&label, "timeout"]).get() - 1.0).abs() < f64::EPSILON
        );
        assert!(
            (suppressed.with_label_values(&[&label, "connect"]).get() - 1.0).abs() < f64::EPSILON
        );
        // The probe failures themselves stay visible to operators.
        assert!(
            (proxy
                .inner
                .metrics
                .upstream_probe_errors
                .with_label_values(&[&label, "timeout"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );

        // An engine that answers the probe with a real error is stating
        // something about itself, and is fenced whatever traffic it served.
        proxy.mark_probe(0, false, "http");
        assert!(!proxy.router().state(0).unwrap().3);
    }

    #[tokio::test(start_paused = true)]
    async fn serving_evidence_expires_so_a_silent_upstream_is_still_fenced() {
        let proxy = proxy_for(&[Url::parse("http://127.0.0.1:1").unwrap()]);
        proxy.record_upstream_request(0, StatusCode::OK);
        tokio::time::advance(Duration::from_millis(RECENT_SERVING_SUCCESS_WINDOW_MS + 1)).await;
        proxy.mark_probe(0, false, "timeout");
        assert!(
            !proxy.router().state(0).unwrap().3,
            "stale traffic must not keep an engine that answers nothing admitted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shedding_upstream_does_not_earn_probe_immunity() {
        let proxy = proxy_for(&[Url::parse("http://127.0.0.1:1").unwrap()]);
        // A 5xx is how both an overloaded engine and a broken one answer, so it
        // is not liveness evidence.
        proxy.record_upstream_request(0, StatusCode::SERVICE_UNAVAILABLE);
        proxy.mark_probe(0, false, "timeout");
        assert!(!proxy.router().state(0).unwrap().3);
    }

    #[tokio::test]
    async fn phase_aware_load_releases_prefill_units_at_first_generated_token() {
        let release_first_token = Arc::new(tokio::sync::Notify::new());
        let release_finish = Arc::new(tokio::sync::Notify::new());
        let first_gate = Arc::clone(&release_first_token);
        let finish_gate = Arc::clone(&release_finish);
        let upstream = AxumRouter::new().fallback(any(move || {
            let first_gate = Arc::clone(&first_gate);
            let finish_gate = Arc::clone(&finish_gate);
            async move {
                let stream = futures_util::stream::unfold(
                    (0_u8, first_gate, finish_gate),
                    |(step, first_gate, finish_gate)| async move {
                        match step {
                            0 => {
                                first_gate.notified().await;
                                Some((
                                    Ok::<Bytes, io::Error>(Bytes::from_static(
                                        b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                                    )),
                                    (1, first_gate, finish_gate),
                                ))
                            }
                            1 => {
                                finish_gate.notified().await;
                                Some((
                                    Ok::<Bytes, io::Error>(Bytes::from_static(
                                        b"data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n\ndata: [DONE]\n\n",
                                    )),
                                    (2, first_gate, finish_gate),
                                ))
                            }
                            _ => None,
                        }
                    },
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let joined = url.as_str().to_owned();
        let config = Config::from_lookup(|key| match key {
            "MD_UPSTREAM" => Some(joined.clone()),
            "MD_ROUTE_PHASE_AWARE_LOAD" => Some("true".to_owned()),
            "MD_ROUTE_LOAD_UNIT_BYTES" => Some("32".to_owned()),
            "MD_ROUTE_MAX_LOAD_UNITS" => Some("4".to_owned()),
            _ => None,
        })
        .unwrap();
        let proxy = proxy_for_config(config, Arc::from([]));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(format!(
                "{{\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}],\"stream\":true}}",
                "x".repeat(256)
            )))
            .unwrap();

        let response = proxy.serve(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(proxy.router().state(0).unwrap().0, 1);
        assert_eq!(proxy.router().state(0).unwrap().1, 4);

        release_first_token.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.router().state(0).unwrap().1 != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first generated token must release the prefill reservation");
        assert_eq!(proxy.router().state(0).unwrap().0, 1);

        release_finish.notify_one();
        let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.router().state(0).unwrap().0 != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed stream must release the remaining decode reservation");
        assert_eq!(proxy.router().state(0).unwrap().1, 0);
        task.abort();
    }

    async fn assert_dropping_downstream_body_cancels_silent_upstream(content_type: &'static str) {
        let dropped = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let upstream_dropped = Arc::clone(&dropped);
        let upstream_notify = Arc::clone(&notify);
        let upstream = AxumRouter::new().fallback(any(move || {
            let signal = DropSignal {
                dropped: Arc::clone(&upstream_dropped),
                notify: Arc::clone(&upstream_notify),
            };
            async move {
                let silent = futures_util::stream::unfold(signal, |signal| async move {
                    std::future::pending::<()>().await;
                    Some((Ok::<Bytes, io::Error>(Bytes::new()), signal))
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", content_type)
                    .body(Body::from_stream(silent))
                    .unwrap()
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"cancel me"}]}"#,
            ))
            .unwrap();

        let response = proxy.serve(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(proxy.router().state(0).unwrap().0, 1);
        drop(response);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::Acquire) {
                notify.notified().await;
            }
        })
        .await
        .expect("dropping the downstream must promptly drop the upstream body");
        tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.router().state(0).unwrap().0 != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("route load must be released when the upstream request is cancelled");
        assert!(
            (proxy
                .inner
                .metrics
                .client_disconnects
                .with_label_values(&["chat"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert_eq!(proxy.router().state(0).unwrap().1, 0);
        task.abort();
    }

    #[tokio::test]
    async fn dropping_downstream_body_cancels_silent_sse_and_json_upstreams() {
        assert_dropping_downstream_body_cancels_silent_upstream("text/event-stream").await;
        assert_dropping_downstream_body_cancels_silent_upstream("application/json").await;
    }

    #[tokio::test]
    async fn tcp_disconnect_before_headers_cancels_upstream_and_releases_load() {
        use tokio::io::AsyncWriteExt as _;

        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_notify = Arc::new(tokio::sync::Notify::new());
        let upstream_entered = Arc::clone(&entered);
        let upstream_dropped = Arc::clone(&dropped);
        let upstream_dropped_notify = Arc::clone(&dropped_notify);
        let upstream = AxumRouter::new().fallback(any(move || {
            let signal = DropSignal {
                dropped: Arc::clone(&upstream_dropped),
                notify: Arc::clone(&upstream_dropped_notify),
            };
            let entered = Arc::clone(&upstream_entered);
            async move {
                entered.notify_one();
                let _signal = signal;
                std::future::pending::<Response<Body>>().await
            }
        }));
        let (upstream_url, upstream_task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[upstream_url]);
        let app = AxumRouter::new()
            .fallback(any(Proxy::handle))
            .with_state(proxy.clone());
        let (proxy_url, proxy_task) = start_upstream(app).await;
        let address = proxy_url
            .socket_addrs(|| None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let body = r#"{"messages":[{"role":"user","content":"disconnect TCP"}]}"#;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                format!(
                    "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("upstream must receive the request before TCP disconnect");
        assert_eq!(proxy.router().state(0).unwrap().0, 1);
        assert_eq!(proxy.router().state(0).unwrap().1, 1);

        drop(client);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::Acquire) {
                dropped_notify.notified().await;
            }
        })
        .await
        .expect("TCP disconnect before headers must promptly drop the upstream request");
        tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.router().state(0).unwrap().0 != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("TCP disconnect before headers must release route load");
        assert_eq!(proxy.router().state(0).unwrap().1, 0);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn rewrites_every_advertised_model_context() {
        let upstream = AxumRouter::new().fallback(any(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"data":[{"max_model_len":393216},{"context_length":262144}]}"#,
            )
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        let request = Request::builder()
            .method(Method::GET)
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let response = proxy.serve(request).await;
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let models: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(models["data"][0]["max_model_len"], 376_832);
        assert_eq!(models["data"][1]["context_length"], 245_760);
        task.abort();
    }
}
