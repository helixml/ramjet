//! Bounded-label Prometheus surface for the snapshot companion.
//!
//! Runtime code records only typed enum values and engine slots. Endpoint URLs,
//! socket paths, UIDs, keys, hashes, epochs, and generation values never become
//! labels. Every bounded series is instantiated at construction so dashboards
//! can distinguish zero activity from a missing collector.

use prometheus::{
    CounterVec, Gauge, GaugeVec, HistogramOpts, HistogramVec, Opts, Registry, core::Collector,
};
use thiserror::Error;

use crate::companion_config::SnapshotCompanionConfig;
use crate::snapshot_tail::SnapshotTailFenceReason;

const ENGINE_SLOTS: [CompanionEngineSlot; 2] = [
    CompanionEngineSlot { index: 0 },
    CompanionEngineSlot { index: 1 },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompanionEngineSlot {
    index: usize,
}

impl CompanionEngineSlot {
    #[must_use]
    pub const fn index(self) -> usize {
        self.index
    }

    const fn label(self) -> &'static str {
        match self.index {
            0 => "engine-0",
            1 => "engine-1",
            _ => "invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionSessionState {
    AwaitingSnapshot,
    BuildingSnapshot,
    CatchingUp,
    Published,
    Fenced,
}

impl CompanionSessionState {
    const ALL: [Self; 5] = [
        Self::AwaitingSnapshot,
        Self::BuildingSnapshot,
        Self::CatchingUp,
        Self::Published,
        Self::Fenced,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::AwaitingSnapshot => "awaiting_snapshot",
            Self::BuildingSnapshot => "building_snapshot",
            Self::CatchingUp => "catching_up",
            Self::Published => "published",
            Self::Fenced => "fenced",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionSnapshotPhase {
    Build,
    Encode,
    Transfer,
    CatchUp,
    Apply,
    CaughtUp,
}

impl CompanionSnapshotPhase {
    const ALL: [Self; 6] = [
        Self::Build,
        Self::Encode,
        Self::Transfer,
        Self::CatchUp,
        Self::Apply,
        Self::CaughtUp,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Encode => "encode",
            Self::Transfer => "transfer",
            Self::CatchUp => "catch_up",
            Self::Apply => "apply",
            Self::CaughtUp => "caught_up",
        }
    }
}

/// Closed terminal result set. Each variant maps to a fixed outcome/reason
/// pair, preventing callers from creating labels from arbitrary errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionSessionResult {
    Completed,
    RejectedCapacity,
    RejectedAuthentication,
    RejectedPeer,
    FailedProtocol,
    FailedTransport,
    FailedIdentity,
    FailedApplication,
    Cancelled,
    Timeout,
}

impl CompanionSessionResult {
    const ALL: [Self; 10] = [
        Self::Completed,
        Self::RejectedCapacity,
        Self::RejectedAuthentication,
        Self::RejectedPeer,
        Self::FailedProtocol,
        Self::FailedTransport,
        Self::FailedIdentity,
        Self::FailedApplication,
        Self::Cancelled,
        Self::Timeout,
    ];

    const fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Completed => ("completed", "none"),
            Self::RejectedCapacity => ("rejected", "capacity"),
            Self::RejectedAuthentication => ("rejected", "authentication"),
            Self::RejectedPeer => ("rejected", "peer"),
            Self::FailedProtocol => ("failed", "protocol"),
            Self::FailedTransport => ("failed", "transport"),
            Self::FailedIdentity => ("failed", "identity"),
            Self::FailedApplication => ("failed", "application"),
            Self::Cancelled => ("failed", "cancelled"),
            Self::Timeout => ("failed", "timeout"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionOutcome {
    Success,
    Failure,
    Cancelled,
    Timeout,
}

impl CompanionOutcome {
    const ALL: [Self; 4] = [Self::Success, Self::Failure, Self::Cancelled, Self::Timeout];

    const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionTailKind {
    Event,
    CaughtUp,
    Identity,
    Disconnect,
}

impl CompanionTailKind {
    const ALL: [Self; 4] = [
        Self::Event,
        Self::CaughtUp,
        Self::Identity,
        Self::Disconnect,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::CaughtUp => "caught_up",
            Self::Identity => "identity",
            Self::Disconnect => "disconnect",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionTailOutcome {
    Applied,
    Queued,
    Duplicate,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionDeltaEvent {
    Stored,
    Removed,
    Cleared,
    Filtered,
}

impl CompanionDeltaEvent {
    const ALL: [Self; 4] = [Self::Stored, Self::Removed, Self::Cleared, Self::Filtered];

    const fn label(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Removed => "removed",
            Self::Cleared => "cleared",
            Self::Filtered => "filtered",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionDiscardReason {
    Superseded,
    SnapshotFailed,
    TailFailed,
    BufferOverflow,
    Disconnected,
    Shutdown,
}

impl CompanionDiscardReason {
    const ALL: [Self; 6] = [
        Self::Superseded,
        Self::SnapshotFailed,
        Self::TailFailed,
        Self::BufferOverflow,
        Self::Disconnected,
        Self::Shutdown,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::SnapshotFailed => "snapshot_failed",
            Self::TailFailed => "tail_failed",
            Self::BufferOverflow => "buffer_overflow",
            Self::Disconnected => "disconnected",
            Self::Shutdown => "shutdown",
        }
    }
}

impl CompanionTailOutcome {
    const ALL: [Self; 4] = [Self::Applied, Self::Queued, Self::Duplicate, Self::Rejected];

    const fn label(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Queued => "queued",
            Self::Duplicate => "duplicate",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionIdentityChange {
    Incarnation,
    DigestKey,
    Generation,
}

impl CompanionIdentityChange {
    const ALL: [Self; 3] = [Self::Incarnation, Self::DigestKey, Self::Generation];

    const fn label(self) -> &'static str {
        match self {
            Self::Incarnation => "incarnation",
            Self::DigestKey => "digest_key",
            Self::Generation => "generation",
        }
    }
}

#[derive(Debug, Error)]
pub enum CompanionMetricsError {
    #[error("snapshot companion engine slot is not configured")]
    InvalidEngineSlot,
    #[error("snapshot companion metric registration failed")]
    Prometheus(#[from] prometheus::Error),
}

impl CompanionMetricsError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidEngineSlot => "invalid_engine_slot",
            Self::Prometheus(_) => "metric_registration_failed",
        }
    }
}

pub struct CompanionMetrics {
    source_count: usize,
    enabled: Gauge,
    ready: GaugeVec,
    sessions: GaugeVec,
    session_results: CounterVec,
    tail_queue_depth: GaugeVec,
    published_generation: GaugeVec,
    published_index_entries: GaugeVec,
    published_blocks: GaugeVec,
    published_tokens: GaugeVec,
    last_publish_timestamp: GaugeVec,
    snapshots: CounterVec,
    snapshot_duration: HistogramVec,
    snapshot_bytes: HistogramVec,
    tail_frames: CounterVec,
    tail_batches: CounterVec,
    tail_events: CounterVec,
    fences: CounterVec,
    discards: CounterVec,
    identity_changes: CounterVec,
}

impl CompanionMetrics {
    /// Register a stable, pre-initialized companion metric surface.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus registration error for invalid or duplicate
    /// descriptors. Configuration cardinality is already validated by the
    /// typed config constructor.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        registry: &Registry,
        config: &SnapshotCompanionConfig,
    ) -> Result<Self, CompanionMetricsError> {
        let counter = |name, help, labels| CounterVec::new(Opts::new(name, help), labels);
        let gauge = |name, help, labels| GaugeVec::new(Opts::new(name, help), labels);
        let histogram = |name, help, buckets, labels| {
            HistogramVec::new(HistogramOpts::new(name, help).buckets(buckets), labels)
        };
        let metrics = Self {
            source_count: config.sources.len(),
            enabled: Gauge::with_opts(Opts::new(
                "ds4proxy_snapshot_companion_enabled",
                "Whether snapshot companion serving is configured",
            ))?,
            ready: gauge(
                "ds4proxy_snapshot_companion_ready",
                "Whether an engine companion state is ready to serve snapshots",
                &["engine"],
            )?,
            sessions: gauge(
                "ds4proxy_snapshot_companion_sessions",
                "Snapshot companion sessions by bounded lifecycle state",
                &["engine", "state"],
            )?,
            session_results: counter(
                "ds4proxy_snapshot_companion_sessions_total",
                "Snapshot companion terminal sessions by bounded outcome and reason",
                &["engine", "outcome", "reason"],
            )?,
            tail_queue_depth: gauge(
                "ds4proxy_snapshot_companion_tail_queue_depth",
                "Authenticated tail frames queued by bounded engine and client slot",
                &["engine", "client_slot"],
            )?,
            published_generation: gauge(
                "ds4proxy_snapshot_companion_published_generation",
                "Current published companion generation",
                &["engine"],
            )?,
            published_index_entries: gauge(
                "ds4proxy_snapshot_companion_published_index_entries",
                "Entries in the current published digest index",
                &["engine"],
            )?,
            published_blocks: gauge(
                "ds4proxy_snapshot_companion_published_blocks",
                "Resident blocks in the published digest index",
                &["engine"],
            )?,
            published_tokens: gauge(
                "ds4proxy_snapshot_companion_published_tokens",
                "Logical prefix tokens represented by the published digest index",
                &["engine"],
            )?,
            last_publish_timestamp: gauge(
                "ds4proxy_snapshot_companion_last_publish_timestamp_seconds",
                "Unix timestamp of the latest successful snapshot publication",
                &["engine"],
            )?,
            snapshots: counter(
                "ds4proxy_snapshot_companion_snapshots_total",
                "Snapshot exchanges by bounded terminal outcome",
                &["engine", "outcome"],
            )?,
            snapshot_duration: histogram(
                "ds4proxy_snapshot_companion_snapshot_duration_seconds",
                "Snapshot work duration by bounded phase and terminal outcome",
                vec![
                    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 3.0, 10.0, 30.0,
                ],
                &["engine", "phase", "outcome"],
            )?,
            snapshot_bytes: histogram(
                "ds4proxy_snapshot_companion_snapshot_bytes",
                "Authenticated snapshot frame size by terminal outcome",
                vec![
                    1_024.0,
                    16_384.0,
                    262_144.0,
                    1_048_576.0,
                    4_194_304.0,
                    8_388_608.0,
                    33_554_432.0,
                ],
                &["engine", "outcome"],
            )?,
            tail_frames: counter(
                "ds4proxy_snapshot_companion_tail_frames_total",
                "Authenticated tail frames by bounded kind and outcome",
                &["engine", "kind", "outcome"],
            )?,
            tail_batches: counter(
                "ds4proxy_snapshot_companion_tail_batches_total",
                "Decoded tail batches by bounded outcome",
                &["engine", "outcome"],
            )?,
            tail_events: counter(
                "ds4proxy_snapshot_companion_tail_events_total",
                "Decoded tail events by bounded semantic kind",
                &["engine", "kind"],
            )?,
            fences: counter(
                "ds4proxy_snapshot_companion_fences_total",
                "Snapshot lifecycle fences by bounded reason",
                &["engine", "reason"],
            )?,
            discards: counter(
                "ds4proxy_snapshot_companion_discards_total",
                "Private or published generations discarded by bounded reason",
                &["engine", "reason"],
            )?,
            identity_changes: counter(
                "ds4proxy_snapshot_companion_identity_changes_total",
                "Authenticated companion identity changes by bounded component",
                &["engine", "component"],
            )?,
        };
        metrics.enabled.set(f64::from(config.enabled()));
        metrics.initialize_series();
        for collector in metrics.collectors() {
            registry.register(collector)?;
        }
        Ok(metrics)
    }

    /// Resolve one configured engine to its bounded metric slot.
    ///
    /// # Errors
    ///
    /// Returns [`CompanionMetricsError::InvalidEngineSlot`] when the companion
    /// is off or the index is outside the configured source cardinality.
    pub fn engine_slot(&self, index: usize) -> Result<CompanionEngineSlot, CompanionMetricsError> {
        if index >= self.source_count {
            return Err(CompanionMetricsError::InvalidEngineSlot);
        }
        Ok(CompanionEngineSlot { index })
    }

    pub fn set_ready(&self, engine: CompanionEngineSlot, ready: bool) {
        self.ready
            .with_label_values(&[engine.label()])
            .set(f64::from(ready));
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn set_session_state(
        &self,
        engine: CompanionEngineSlot,
        state: CompanionSessionState,
        count: usize,
    ) {
        self.sessions
            .with_label_values(&[engine.label(), state.label()])
            .set(count as f64);
    }

    pub fn record_session(&self, engine: CompanionEngineSlot, result: CompanionSessionResult) {
        let (outcome, reason) = result.labels();
        self.session_results
            .with_label_values(&[engine.label(), outcome, reason])
            .inc();
    }

    /// Update one of the two bounded per-client queue series.
    ///
    /// # Errors
    ///
    /// Returns [`CompanionMetricsError::InvalidEngineSlot`] for a client slot
    /// other than zero or one.
    #[allow(clippy::cast_precision_loss)]
    pub fn set_tail_queue_depth(
        &self,
        engine: CompanionEngineSlot,
        client_slot: usize,
        depth: usize,
    ) -> Result<(), CompanionMetricsError> {
        let slot = client_slot_label(client_slot)?;
        self.tail_queue_depth
            .with_label_values(&[engine.label(), slot])
            .set(depth as f64);
        Ok(())
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn published(
        &self,
        engine: CompanionEngineSlot,
        generation: u64,
        entries: usize,
        blocks: usize,
        tokens: usize,
        timestamp_seconds: f64,
    ) {
        let engine = engine.label();
        self.published_generation
            .with_label_values(&[engine])
            .set(generation as f64);
        self.published_index_entries
            .with_label_values(&[engine])
            .set(entries as f64);
        self.published_blocks
            .with_label_values(&[engine])
            .set(blocks as f64);
        self.published_tokens
            .with_label_values(&[engine])
            .set(tokens as f64);
        self.last_publish_timestamp
            .with_label_values(&[engine])
            .set(timestamp_seconds);
    }

    pub fn observe_snapshot(
        &self,
        engine: CompanionEngineSlot,
        phase: CompanionSnapshotPhase,
        outcome: CompanionOutcome,
        duration_seconds: f64,
    ) {
        self.snapshot_duration
            .with_label_values(&[engine.label(), phase.label(), outcome.label()])
            .observe(duration_seconds);
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn finish_snapshot(
        &self,
        engine: CompanionEngineSlot,
        outcome: CompanionOutcome,
        bytes: usize,
    ) {
        let labels = [engine.label(), outcome.label()];
        self.snapshots.with_label_values(&labels).inc();
        self.snapshot_bytes
            .with_label_values(&labels)
            .observe(bytes as f64);
    }

    pub fn record_tail(
        &self,
        engine: CompanionEngineSlot,
        kind: CompanionTailKind,
        outcome: CompanionTailOutcome,
    ) {
        self.tail_frames
            .with_label_values(&[engine.label(), kind.label(), outcome.label()])
            .inc();
    }

    pub fn record_tail_batch(&self, engine: CompanionEngineSlot, outcome: CompanionTailOutcome) {
        self.tail_batches
            .with_label_values(&[engine.label(), outcome.label()])
            .inc();
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn record_tail_events(
        &self,
        engine: CompanionEngineSlot,
        kind: CompanionDeltaEvent,
        count: usize,
    ) {
        self.tail_events
            .with_label_values(&[engine.label(), kind.label()])
            .inc_by(count as f64);
    }

    pub fn record_fence(&self, engine: CompanionEngineSlot, reason: SnapshotTailFenceReason) {
        self.fences
            .with_label_values(&[engine.label(), fence_label(reason)])
            .inc();
    }

    pub fn record_discard(&self, engine: CompanionEngineSlot, reason: CompanionDiscardReason) {
        self.discards
            .with_label_values(&[engine.label(), reason.label()])
            .inc();
    }

    pub fn record_identity_change(
        &self,
        engine: CompanionEngineSlot,
        component: CompanionIdentityChange,
    ) {
        self.identity_changes
            .with_label_values(&[engine.label(), component.label()])
            .inc();
    }

    fn initialize_series(&self) {
        for engine in ENGINE_SLOTS {
            let engine_label = engine.label();
            self.ready.with_label_values(&[engine_label]);
            self.published_generation.with_label_values(&[engine_label]);
            self.published_index_entries
                .with_label_values(&[engine_label]);
            self.published_blocks.with_label_values(&[engine_label]);
            self.published_tokens.with_label_values(&[engine_label]);
            self.last_publish_timestamp
                .with_label_values(&[engine_label]);
            for state in CompanionSessionState::ALL {
                self.sessions
                    .with_label_values(&[engine_label, state.label()]);
            }
            for result in CompanionSessionResult::ALL {
                let (outcome, reason) = result.labels();
                self.session_results
                    .with_label_values(&[engine_label, outcome, reason]);
            }
            for client_slot in ["client-0", "client-1"] {
                self.tail_queue_depth
                    .with_label_values(&[engine_label, client_slot]);
            }
            for outcome in CompanionOutcome::ALL {
                self.snapshots
                    .with_label_values(&[engine_label, outcome.label()]);
                self.snapshot_bytes
                    .with_label_values(&[engine_label, outcome.label()]);
                for phase in CompanionSnapshotPhase::ALL {
                    self.snapshot_duration.with_label_values(&[
                        engine_label,
                        phase.label(),
                        outcome.label(),
                    ]);
                }
            }
            for kind in CompanionTailKind::ALL {
                for outcome in CompanionTailOutcome::ALL {
                    self.tail_frames.with_label_values(&[
                        engine_label,
                        kind.label(),
                        outcome.label(),
                    ]);
                }
            }
            for outcome in CompanionTailOutcome::ALL {
                self.tail_batches
                    .with_label_values(&[engine_label, outcome.label()]);
            }
            for kind in CompanionDeltaEvent::ALL {
                self.tail_events
                    .with_label_values(&[engine_label, kind.label()]);
            }
            for reason in all_fence_reasons() {
                self.fences
                    .with_label_values(&[engine_label, fence_label(reason)]);
            }
            for reason in CompanionDiscardReason::ALL {
                self.discards
                    .with_label_values(&[engine_label, reason.label()]);
            }
            for component in CompanionIdentityChange::ALL {
                self.identity_changes
                    .with_label_values(&[engine_label, component.label()]);
            }
        }
    }

    fn collectors(&self) -> Vec<Box<dyn Collector>> {
        vec![
            Box::new(self.enabled.clone()),
            Box::new(self.ready.clone()),
            Box::new(self.sessions.clone()),
            Box::new(self.session_results.clone()),
            Box::new(self.tail_queue_depth.clone()),
            Box::new(self.published_generation.clone()),
            Box::new(self.published_index_entries.clone()),
            Box::new(self.published_blocks.clone()),
            Box::new(self.published_tokens.clone()),
            Box::new(self.last_publish_timestamp.clone()),
            Box::new(self.snapshots.clone()),
            Box::new(self.snapshot_duration.clone()),
            Box::new(self.snapshot_bytes.clone()),
            Box::new(self.tail_frames.clone()),
            Box::new(self.tail_batches.clone()),
            Box::new(self.tail_events.clone()),
            Box::new(self.fences.clone()),
            Box::new(self.discards.clone()),
            Box::new(self.identity_changes.clone()),
        ]
    }
}

fn client_slot_label(slot: usize) -> Result<&'static str, CompanionMetricsError> {
    match slot {
        0 => Ok("client-0"),
        1 => Ok("client-1"),
        _ => Err(CompanionMetricsError::InvalidEngineSlot),
    }
}

const fn fence_label(reason: SnapshotTailFenceReason) -> &'static str {
    reason.as_str()
}

const fn all_fence_reasons() -> [SnapshotTailFenceReason; 14] {
    use SnapshotTailFenceReason::{
        ApplicationFailed, BufferOverflow, Cancelled, CaughtUpMismatch, DigestKeyChanged,
        Disconnected, EventWatermarkRegression, GenerationChanged, IncarnationChanged,
        SequenceOverflow, StaleSnapshot, TailGap, UnexpectedState, UnsupportedResetScope,
    };
    [
        StaleSnapshot,
        UnsupportedResetScope,
        IncarnationChanged,
        DigestKeyChanged,
        GenerationChanged,
        TailGap,
        EventWatermarkRegression,
        SequenceOverflow,
        BufferOverflow,
        CaughtUpMismatch,
        Disconnected,
        Cancelled,
        ApplicationFailed,
        UnexpectedState,
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use prometheus::TextEncoder;

    use super::*;
    use crate::companion_config::SnapshotCompanionMode;

    fn config(mode: SnapshotCompanionMode) -> SnapshotCompanionConfig {
        let mut values = HashMap::new();
        if mode == SnapshotCompanionMode::Serve {
            values.extend([
                ("DS4_SNAPSHOT_COMPANION_MODE", "serve"),
                ("DS4_SNAPSHOT_SOCKET_PATH", "/run/mini-dynamo/snapshot.sock"),
                ("DS4_SNAPSHOT_COMPANION_UID", "12001"),
                ("DS4_SNAPSHOT_CLIENT_UID", "12002"),
                ("DS4_SNAPSHOT_SECRET_PATH", "/run/secrets/snapshot-session"),
                ("DS4_SNAPSHOT_LIVE_ENDPOINTS", "tcp://a:5557,tcp://b:5557"),
                ("DS4_SNAPSHOT_REPLAY_ENDPOINTS", "tcp://a:5558,tcp://b:5558"),
            ]);
        }
        SnapshotCompanionConfig::from_lookup(|key| values.get(key).map(ToString::to_string))
            .unwrap()
    }

    fn text(registry: &Registry) -> String {
        TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap()
    }

    #[test]
    fn all_metric_families_and_zero_series_exist_before_sessions() {
        let registry = Registry::new();
        CompanionMetrics::new(&registry, &config(SnapshotCompanionMode::Serve)).unwrap();
        let text = text(&registry);
        for expected in [
            "ds4proxy_snapshot_companion_enabled 1",
            "ds4proxy_snapshot_companion_ready{engine=\"engine-0\"} 0",
            "ds4proxy_snapshot_companion_sessions{engine=\"engine-1\",state=\"catching_up\"} 0",
            "ds4proxy_snapshot_companion_sessions_total{engine=\"engine-0\",outcome=\"rejected\",reason=\"authentication\"} 0",
            "ds4proxy_snapshot_companion_tail_queue_depth{client_slot=\"client-1\",engine=\"engine-0\"} 0",
            "ds4proxy_snapshot_companion_snapshots_total{engine=\"engine-0\",outcome=\"timeout\"} 0",
            "ds4proxy_snapshot_companion_snapshot_duration_seconds_count{engine=\"engine-1\",outcome=\"success\",phase=\"build\"} 0",
            "ds4proxy_snapshot_companion_snapshot_duration_seconds_count{engine=\"engine-1\",outcome=\"success\",phase=\"apply\"} 0",
            "ds4proxy_snapshot_companion_snapshot_duration_seconds_count{engine=\"engine-1\",outcome=\"success\",phase=\"caught_up\"} 0",
            "ds4proxy_snapshot_companion_tail_frames_total{engine=\"engine-0\",kind=\"caught_up\",outcome=\"queued\"} 0",
            "ds4proxy_snapshot_companion_tail_batches_total{engine=\"engine-0\",outcome=\"applied\"} 0",
            "ds4proxy_snapshot_companion_tail_events_total{engine=\"engine-0\",kind=\"stored\"} 0",
            "ds4proxy_snapshot_companion_fences_total{engine=\"engine-1\",reason=\"tail_gap\"} 0",
            "ds4proxy_snapshot_companion_discards_total{engine=\"engine-1\",reason=\"superseded\"} 0",
            "ds4proxy_snapshot_companion_identity_changes_total{component=\"digest_key\",engine=\"engine-0\"} 0",
        ] {
            assert!(text.contains(expected), "missing series: {expected}");
        }
    }

    #[test]
    fn off_mode_exports_disabled_without_accepting_engine_updates() {
        let registry = Registry::new();
        let metrics =
            CompanionMetrics::new(&registry, &config(SnapshotCompanionMode::Off)).unwrap();
        assert!(matches!(
            metrics.engine_slot(0),
            Err(CompanionMetricsError::InvalidEngineSlot)
        ));
        assert!(text(&registry).contains("ds4proxy_snapshot_companion_enabled 0"));
    }

    #[test]
    fn typed_updates_cannot_create_unbounded_labels() {
        let registry = Registry::new();
        let metrics =
            CompanionMetrics::new(&registry, &config(SnapshotCompanionMode::Serve)).unwrap();
        let engine = metrics.engine_slot(1).unwrap();
        metrics.set_ready(engine, true);
        metrics.set_session_state(engine, CompanionSessionState::Published, 1);
        metrics.record_session(engine, CompanionSessionResult::Completed);
        metrics.set_tail_queue_depth(engine, 0, 4).unwrap();
        metrics.finish_snapshot(engine, CompanionOutcome::Success, 5_000_000);
        metrics.record_tail(
            engine,
            CompanionTailKind::Event,
            CompanionTailOutcome::Applied,
        );
        metrics.record_tail_batch(engine, CompanionTailOutcome::Applied);
        metrics.record_tail_events(engine, CompanionDeltaEvent::Stored, 7);
        metrics.record_fence(engine, SnapshotTailFenceReason::TailGap);
        metrics.record_discard(engine, CompanionDiscardReason::Superseded);
        metrics.record_identity_change(engine, CompanionIdentityChange::Generation);
        metrics.published(engine, 8, 36_612, 36_000, 9_216_000, 123.0);
        assert!(metrics.set_tail_queue_depth(engine, 2, 0).is_err());
        assert!(metrics.engine_slot(2).is_err());

        let text = text(&registry);
        assert!(text.contains("ds4proxy_snapshot_companion_ready{engine=\"engine-1\"} 1"));
        assert!(text.contains(
            "ds4proxy_snapshot_companion_published_index_entries{engine=\"engine-1\"} 36612"
        ));
        assert!(
            text.contains(
                "ds4proxy_snapshot_companion_published_tokens{engine=\"engine-1\"} 9216000"
            )
        );
        for forbidden in [
            "/run/mini-dynamo/snapshot.sock",
            "/run/secrets/snapshot-session",
            "tcp://a:5557",
            "snapshot-session",
        ] {
            assert!(
                !text.contains(forbidden),
                "metric output leaked {forbidden}"
            );
        }
    }
}
