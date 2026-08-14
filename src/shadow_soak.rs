//! Bounded, route-only exact/approximate comparison soak.
//!
//! Source token vectors remain process-memory-only. The soak never dispatches
//! upstream, mutates a route, teaches either cache index, or uses the ordinary
//! production shadow metrics.

use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::{
    exact_shadow::{ExactPlacementPolicy, ExactRouteEvaluation, ExactRouteShadow},
    metrics::Metrics,
    router::Decision,
};

const POLICY_LOAD_DELTAS: [usize; 4] = [0, 1, 2, 4];
const YIELD_EVERY_ATTEMPTS: usize = 256;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ShadowSoakConfig {
    pub source_target: usize,
    pub comparison_target: usize,
    pub attempt_limit: usize,
    pub max_token_bytes: usize,
    pub min_gain_tokens: usize,
    pub timeout: Duration,
}

pub(crate) trait ShadowSoakAttestation: Send + Sync {
    fn marker(&self) -> Option<u64>;
    fn unchanged(&self, marker: u64) -> bool;
}

pub(crate) struct ShadowSoakSource {
    tokens: Arc<[u32]>,
    decision: Decision,
    baseline: SourceBaseline,
}

#[derive(Clone, Copy)]
struct SourceBaseline {
    outcome: &'static str,
    placement: [&'static str; POLICY_LOAD_DELTAS.len()],
    projected_balance: [&'static str; POLICY_LOAD_DELTAS.len()],
    selected_tokens: usize,
    best_tokens: usize,
}

impl ShadowSoakSource {
    #[must_use]
    fn new(
        tokens: &[u32],
        decision: &Decision,
        evaluation: &ExactRouteEvaluation,
        min_gain_tokens: usize,
    ) -> Self {
        let placement = POLICY_LOAD_DELTAS.map(|max_load_delta| {
            evaluation.placement_label(
                decision,
                ExactPlacementPolicy {
                    min_gain_tokens,
                    max_load_delta,
                },
            )
        });
        let projected_balance = POLICY_LOAD_DELTAS.map(|max_load_delta| {
            evaluation.projected_balance_label(
                decision,
                ExactPlacementPolicy {
                    min_gain_tokens,
                    max_load_delta,
                },
            )
        });
        Self {
            tokens: Arc::from(tokens),
            decision: decision.clone(),
            baseline: SourceBaseline {
                outcome: evaluation.outcome_label(),
                placement,
                projected_balance,
                selected_tokens: evaluation.selected_tokens(),
                best_tokens: evaluation.best_tokens(),
            },
        }
    }

    fn token_bytes(&self) -> Option<usize> {
        self.tokens.len().checked_mul(size_of::<u32>())
    }

    #[cfg(test)]
    fn test(tokens: &[u32], decision: &Decision) -> Self {
        Self {
            tokens: Arc::from(tokens),
            decision: decision.clone(),
            baseline: SourceBaseline {
                outcome: "all_zero",
                placement: ["kept_all_zero"; POLICY_LOAD_DELTAS.len()],
                projected_balance: ["kept_selected"; POLICY_LOAD_DELTAS.len()],
                selected_tokens: 0,
                best_tokens: 0,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Collecting,
    Ready,
    Running,
    Complete,
    Failed,
}

impl Phase {
    const fn label(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

struct CaptureState {
    phase: Phase,
    sources: usize,
    token_bytes: usize,
}

struct Inner {
    config: ShadowSoakConfig,
    sender: mpsc::Sender<ShadowSoakSource>,
    state: Mutex<CaptureState>,
    metrics: Arc<Metrics>,
    start_sender: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Clone)]
pub(crate) struct ShadowSoak {
    inner: Option<Arc<Inner>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureResult {
    Accepted,
    Disabled,
    NotCollecting,
    TokenCapacity,
    AttestationChanged,
    ExactUnavailable,
    QueueClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartResult {
    Started,
    Disabled,
    NotReady,
    QueueClosed,
}

impl StartResult {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Disabled => "disabled",
            Self::NotReady => "not_ready",
            Self::QueueClosed => "queue_closed",
        }
    }
}

impl CaptureResult {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Disabled => "disabled",
            Self::NotCollecting => "not_collecting",
            Self::TokenCapacity => "token_capacity",
            Self::AttestationChanged => "attestation_changed",
            Self::ExactUnavailable => "exact_unavailable",
            Self::QueueClosed => "queue_closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShadowSoakStatus {
    pub enabled: bool,
    pub phase: &'static str,
    pub sources: usize,
    pub token_bytes: usize,
}

impl ShadowSoak {
    #[must_use]
    pub(crate) fn off(metrics: &Arc<Metrics>) -> Self {
        metrics.shadow_soak_enabled.set(0.0);
        set_phase(metrics, "off");
        Self { inner: None }
    }

    #[must_use]
    pub(crate) fn start(
        config: ShadowSoakConfig,
        exact_shadow: ExactRouteShadow,
        attestation: Arc<dyn ShadowSoakAttestation>,
        metrics: Arc<Metrics>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(config.source_target);
        let (start_sender, start_receiver) = oneshot::channel();
        let inner = Arc::new(Inner {
            config,
            sender,
            state: Mutex::new(CaptureState {
                phase: Phase::Collecting,
                sources: 0,
                token_bytes: 0,
            }),
            metrics,
            start_sender: Mutex::new(Some(start_sender)),
        });
        inner.metrics.shadow_soak_enabled.set(1.0);
        set_phase(&inner.metrics, Phase::Collecting.label());
        tokio::spawn(run(
            Arc::downgrade(&inner),
            receiver,
            start_receiver,
            exact_shadow,
            attestation,
        ));
        Self { inner: Some(inner) }
    }

    pub(crate) fn capture(&self, source: ShadowSoakSource) -> CaptureResult {
        let Some(inner) = &self.inner else {
            return CaptureResult::Disabled;
        };
        let Some(source_bytes) = source.token_bytes() else {
            return CaptureResult::TokenCapacity;
        };
        let mut state = inner.state.lock();
        if state.phase != Phase::Collecting || state.sources >= inner.config.source_target {
            return CaptureResult::NotCollecting;
        }
        let Some(next_bytes) = state.token_bytes.checked_add(source_bytes) else {
            return CaptureResult::TokenCapacity;
        };
        if next_bytes > inner.config.max_token_bytes {
            return CaptureResult::TokenCapacity;
        }
        let baseline = source.baseline;
        if inner.sender.try_send(source).is_err() {
            return CaptureResult::QueueClosed;
        }
        state.sources += 1;
        state.token_bytes = next_bytes;
        inner
            .metrics
            .shadow_soak_sources
            .set(usize_to_f64(state.sources));
        inner
            .metrics
            .shadow_soak_source_token_bytes
            .set(usize_to_f64(state.token_bytes));
        record_source_baseline(&inner.metrics, baseline);
        if state.sources == inner.config.source_target {
            state.phase = Phase::Ready;
            set_phase(&inner.metrics, Phase::Ready.label());
        }
        CaptureResult::Accepted
    }

    pub(crate) fn prepare_source(
        &self,
        tokens: &[u32],
        decision: &Decision,
        evaluation: &ExactRouteEvaluation,
    ) -> Result<ShadowSoakSource, CaptureResult> {
        let Some(inner) = &self.inner else {
            return Err(CaptureResult::Disabled);
        };
        if !evaluation.stable() {
            return Err(CaptureResult::ExactUnavailable);
        }
        Ok(ShadowSoakSource::new(
            tokens,
            decision,
            evaluation,
            inner.config.min_gain_tokens,
        ))
    }

    pub(crate) fn start_run(&self) -> StartResult {
        let Some(inner) = &self.inner else {
            return StartResult::Disabled;
        };
        let mut state = inner.state.lock();
        if state.phase != Phase::Ready {
            return StartResult::NotReady;
        }
        let Some(sender) = inner.start_sender.lock().take() else {
            state.phase = Phase::Failed;
            set_phase(&inner.metrics, Phase::Failed.label());
            return StartResult::QueueClosed;
        };
        if sender.send(()).is_err() {
            state.phase = Phase::Failed;
            set_phase(&inner.metrics, Phase::Failed.label());
            return StartResult::QueueClosed;
        }
        state.phase = Phase::Running;
        set_phase(&inner.metrics, Phase::Running.label());
        StartResult::Started
    }

    #[must_use]
    pub(crate) fn status(&self) -> ShadowSoakStatus {
        let Some(inner) = &self.inner else {
            return ShadowSoakStatus {
                enabled: false,
                phase: "off",
                sources: 0,
                token_bytes: 0,
            };
        };
        let state = inner.state.lock();
        ShadowSoakStatus {
            enabled: true,
            phase: state.phase.label(),
            sources: state.sources,
            token_bytes: state.token_bytes,
        }
    }
}

async fn run(
    owner: Weak<Inner>,
    mut receiver: mpsc::Receiver<ShadowSoakSource>,
    start_receiver: oneshot::Receiver<()>,
    exact_shadow: ExactRouteShadow,
    attestation: Arc<dyn ShadowSoakAttestation>,
) {
    let Some(inner) = owner.upgrade() else {
        return;
    };
    let source_target = inner.config.source_target;
    drop(inner);
    let mut sources = Vec::with_capacity(source_target);
    while sources.len() < source_target {
        let Some(source) = receiver.recv().await else {
            return;
        };
        sources.push(source);
    }
    if start_receiver.await.is_err() {
        return;
    }
    let Some(inner) = owner.upgrade() else {
        return;
    };
    let config = inner.config;
    let metrics = Arc::clone(&inner.metrics);
    drop(inner);
    let task_owner = owner.clone();
    let result = tokio::task::spawn_blocking(move || {
        run_blocking(
            &task_owner,
            &sources,
            &exact_shadow,
            attestation.as_ref(),
            &metrics,
            config,
        );
    })
    .await;
    if result.is_err()
        && let Some(inner) = owner.upgrade()
    {
        finish(&inner, Phase::Failed, 0.0);
    }
}

#[allow(clippy::too_many_lines)]
fn run_blocking(
    owner: &Weak<Inner>,
    sources: &[ShadowSoakSource],
    exact_shadow: &ExactRouteShadow,
    attestation: &dyn ShadowSoakAttestation,
    metrics: &Metrics,
    config: ShadowSoakConfig,
) {
    let started = Instant::now();
    let deadline = started + config.timeout;
    let mut stable = 0_usize;
    let mut attempt = 0_usize;
    while stable < config.comparison_target {
        if Instant::now() >= deadline {
            metrics
                .shadow_soak_attempts
                .with_label_values(&["timeout"])
                .inc();
            if let Some(inner) = owner.upgrade() {
                finish(&inner, Phase::Failed, started.elapsed().as_secs_f64());
            }
            return;
        }
        if attempt.is_multiple_of(YIELD_EVERY_ATTEMPTS) {
            if owner.upgrade().is_none() {
                metrics
                    .shadow_soak_attempts
                    .with_label_values(&["cancelled"])
                    .inc();
                return;
            }
            std::thread::yield_now();
        }
        let Some(attestation_marker) = attestation.marker() else {
            if owner.upgrade().is_none() {
                metrics
                    .shadow_soak_attempts
                    .with_label_values(&["cancelled"])
                    .inc();
                return;
            }
            metrics
                .shadow_soak_attempts
                .with_label_values(&["attestation_wait"])
                .inc();
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        if attempt == config.attempt_limit {
            metrics
                .shadow_soak_attempts
                .with_label_values(&["attempt_limit"])
                .inc();
            if let Some(inner) = owner.upgrade() {
                finish(&inner, Phase::Failed, started.elapsed().as_secs_f64());
            }
            return;
        }
        let source = &sources[attempt % sources.len()];
        attempt += 1;
        let evaluation =
            exact_shadow.evaluate_pre_route_diagnostic(&source.tokens, &source.decision);
        if !attestation.unchanged(attestation_marker) {
            metrics
                .shadow_soak_attempts
                .with_label_values(&["attestation_changed"])
                .inc();
            if let Some(inner) = owner.upgrade() {
                finish(&inner, Phase::Failed, started.elapsed().as_secs_f64());
            }
            return;
        }
        if !evaluation.stable() {
            let outcome = attempt_outcome(evaluation.outcome_label());
            metrics
                .shadow_soak_attempts
                .with_label_values(&[outcome])
                .inc();
            if outcome != "inventory_changed" {
                if let Some(inner) = owner.upgrade() {
                    finish(&inner, Phase::Failed, started.elapsed().as_secs_f64());
                }
                return;
            }
            continue;
        }
        metrics
            .shadow_soak_attempts
            .with_label_values(&["stable"])
            .inc();
        metrics
            .shadow_soak_comparisons
            .with_label_values(&[evaluation.outcome_label()])
            .inc();
        metrics
            .shadow_soak_overlap
            .with_label_values(&["selected"])
            .observe(usize_to_f64(evaluation.selected_tokens()));
        metrics
            .shadow_soak_overlap
            .with_label_values(&["best"])
            .observe(usize_to_f64(evaluation.best_tokens()));
        metrics.shadow_soak_gain.observe(usize_to_f64(
            evaluation
                .best_tokens()
                .saturating_sub(evaluation.selected_tokens()),
        ));
        stable += 1;
    }
    if let Some(inner) = owner.upgrade() {
        finish(&inner, Phase::Complete, started.elapsed().as_secs_f64());
    }
}

fn record_source_baseline(metrics: &Metrics, baseline: SourceBaseline) {
    metrics
        .shadow_soak_source_comparisons
        .with_label_values(&[baseline.outcome])
        .inc();
    metrics
        .shadow_soak_source_overlap
        .with_label_values(&["selected"])
        .observe(usize_to_f64(baseline.selected_tokens));
    metrics
        .shadow_soak_source_overlap
        .with_label_values(&["best"])
        .observe(usize_to_f64(baseline.best_tokens));
    metrics.shadow_soak_source_gain.observe(usize_to_f64(
        baseline
            .best_tokens
            .saturating_sub(baseline.selected_tokens),
    ));
    for (index, max_load_delta) in POLICY_LOAD_DELTAS.into_iter().enumerate() {
        metrics
            .shadow_soak_placement
            .with_label_values(&[policy_label(max_load_delta), baseline.placement[index]])
            .inc();
        metrics
            .shadow_soak_projected_balance
            .with_label_values(&[
                policy_label(max_load_delta),
                baseline.projected_balance[index],
            ])
            .inc();
    }
}

fn finish(inner: &Inner, phase: Phase, duration: f64) {
    inner.state.lock().phase = phase;
    inner
        .metrics
        .shadow_soak_complete
        .set(f64::from(phase == Phase::Complete));
    inner.metrics.shadow_soak_duration.set(duration);
    set_phase(&inner.metrics, phase.label());
}

fn set_phase(metrics: &Metrics, phase: &str) {
    for label in [
        "off",
        "collecting",
        "ready",
        "running",
        "complete",
        "failed",
    ] {
        metrics
            .shadow_soak_phase
            .with_label_values(&[label])
            .set(f64::from(label == phase));
    }
}

fn attempt_outcome(outcome: &str) -> &'static str {
    match outcome {
        "inventory_changed" => "inventory_changed",
        "inventory_untrusted" => "inventory_untrusted",
        "lookup_error" => "lookup_error",
        "candidate_mismatch" => "candidate_mismatch",
        _ => "other",
    }
}

const fn policy_label(value: usize) -> &'static str {
    match value {
        0 => "0",
        1 => "1",
        2 => "2",
        4 => "4",
        _ => "other",
    }
}

#[allow(clippy::cast_precision_loss)]
fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use prometheus::Registry;

    use super::*;
    use crate::{
        exact_index::{ExactIndexLimits, FencedExactKvInventory},
        kv_wire::KvEventBatch,
        router::{CandidateState, Outcome},
    };

    struct Attestation {
        unready_attempts: AtomicUsize,
    }

    impl ShadowSoakAttestation for Attestation {
        fn marker(&self) -> Option<u64> {
            let remaining = self.unready_attempts.load(Ordering::Acquire);
            if remaining == 0 {
                Some(7)
            } else {
                self.unready_attempts.fetch_sub(1, Ordering::AcqRel);
                None
            }
        }

        fn unchanged(&self, marker: u64) -> bool {
            marker == 7
        }
    }

    fn inventory(trusted: bool) -> crate::kv_consumer::SharedFencedInventory {
        let inventory = Arc::new(parking_lot::RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        if trusted {
            inventory
                .write()
                .ingest_live(
                    0,
                    &KvEventBatch {
                        timestamp: 1.0,
                        events: Vec::new(),
                        data_parallel_rank: Some(0),
                    },
                )
                .unwrap();
        }
        inventory
    }

    fn decision() -> Decision {
        Decision {
            candidates: vec![0, 1],
            candidate_state: vec![
                CandidateState {
                    index: 0,
                    rank: 0,
                    overlap_blocks: 0,
                    affinity_blocks: 0,
                    load_units: 0,
                    request_load_units: 1,
                    healthy: true,
                },
                CandidateState {
                    index: 1,
                    rank: 1,
                    overlap_blocks: 0,
                    affinity_blocks: 0,
                    load_units: 0,
                    request_load_units: 1,
                    healthy: true,
                },
            ],
            overlap_blocks: 0,
            total_blocks: 1,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::RoundRobin,
        }
    }

    fn metrics() -> Arc<Metrics> {
        Arc::new(Metrics::new(&Registry::new()).unwrap())
    }

    fn start(
        metrics: &Arc<Metrics>,
        trusted: bool,
        unready_attempts: usize,
        max_token_bytes: usize,
    ) -> ShadowSoak {
        ShadowSoak::start(
            ShadowSoakConfig {
                source_target: 2,
                comparison_target: 10,
                attempt_limit: 16,
                max_token_bytes,
                min_gain_tokens: 1,
                timeout: Duration::from_secs(1),
            },
            ExactRouteShadow::new(
                Arc::from([inventory(trusted), inventory(trusted)]),
                Arc::clone(metrics),
                4.0,
                32,
            ),
            Arc::new(Attestation {
                unready_attempts: AtomicUsize::new(unready_attempts),
            }),
            Arc::clone(metrics),
        )
    }

    async fn wait_phase(soak: &ShadowSoak, expected: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while soak.status().phase != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn off_mode_has_no_capture_or_task() {
        let metrics = metrics();
        let soak = ShadowSoak::off(&metrics);
        assert_eq!(soak.status().phase, "off");
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[1], &decision())),
            CaptureResult::Disabled
        );
        assert!((metrics.shadow_soak_enabled.get() - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn bounded_sources_complete_revision_stable_policy_sweep() {
        let metrics = metrics();
        let soak = start(&metrics, true, 0, 8);
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[1], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[2], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[3], &decision())),
            CaptureResult::NotCollecting
        );
        assert_eq!(soak.start_run(), StartResult::Started);
        wait_phase(&soak, "complete").await;
        assert_eq!(soak.status().sources, 2);
        assert_eq!(soak.status().token_bytes, 8);
        assert!(
            metrics
                .shadow_soak_attempts
                .with_label_values(&["attestation_wait"])
                .get()
                .abs()
                < f64::EPSILON
        );
        assert!(
            (metrics
                .shadow_soak_comparisons
                .with_label_values(&["all_zero"])
                .get()
                - 10.0)
                .abs()
                < f64::EPSILON
        );
        for delta in ["0", "1", "2", "4"] {
            assert!(
                (metrics
                    .shadow_soak_projected_balance
                    .with_label_values(&[delta, "kept_selected"])
                    .get()
                    - 2.0)
                    .abs()
                    < f64::EPSILON
            );
        }
        assert!((metrics.shadow_soak_complete.get() - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn attestation_unready_waits_without_consuming_attempt_slack() {
        let metrics = metrics();
        let soak = start(&metrics, true, 1, 8);
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[1], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[2], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(soak.start_run(), StartResult::Started);
        wait_phase(&soak, "complete").await;
        assert!(
            (metrics
                .shadow_soak_attempts
                .with_label_values(&["attestation_wait"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!((metrics.shadow_soak_complete.get() - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn absolute_timeout_bounds_an_unavailable_attestation() {
        let metrics = metrics();
        let soak = ShadowSoak::start(
            ShadowSoakConfig {
                source_target: 1,
                comparison_target: 1,
                attempt_limit: 1,
                max_token_bytes: 4,
                min_gain_tokens: 1,
                timeout: Duration::from_millis(2),
            },
            ExactRouteShadow::new(
                Arc::from([inventory(true), inventory(true)]),
                Arc::clone(&metrics),
                4.0,
                32,
            ),
            Arc::new(Attestation {
                unready_attempts: AtomicUsize::new(usize::MAX),
            }),
            Arc::clone(&metrics),
        );
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[1], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(soak.start_run(), StartResult::Started);
        wait_phase(&soak, "failed").await;
        assert!(
            (metrics
                .shadow_soak_attempts
                .with_label_values(&["timeout"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            metrics
                .shadow_soak_attempts
                .with_label_values(&["attestation_wait"])
                .get()
                > 0.0
        );
    }

    #[tokio::test]
    async fn token_capacity_rejects_without_partial_accounting() {
        let metrics = metrics();
        let soak = start(&metrics, true, 0, 4);
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[1], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[2], &decision())),
            CaptureResult::TokenCapacity
        );
        assert_eq!(soak.status().sources, 1);
        assert_eq!(soak.status().token_bytes, 4);
        assert_eq!(soak.status().phase, "collecting");
    }

    #[tokio::test]
    async fn untrusted_inventory_fails_without_counting_a_comparison() {
        let metrics = metrics();
        let soak = start(&metrics, false, 0, 8);
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[1], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(
            soak.capture(ShadowSoakSource::test(&[2], &decision())),
            CaptureResult::Accepted
        );
        assert_eq!(soak.start_run(), StartResult::Started);
        wait_phase(&soak, "failed").await;
        assert!(
            (metrics
                .shadow_soak_attempts
                .with_label_values(&["inventory_untrusted"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            metrics
                .shadow_soak_comparisons
                .with_label_values(&["all_zero"])
                .get()
                .abs()
                < f64::EPSILON
        );
    }
}
