use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use axum::body::Bytes;
use dynamo_protocols::types::CreateChatCompletionRequest;
use dynamo_renderer::{OAIChatLikeRequest, PromptFormatter, TextInput, deepseek_formatter_for};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Semaphore, mpsc},
    time::Instant,
};
use url::Url;

use crate::{
    compat::{CompatibilityManifest, RuntimeOutcome, sha256_hex, token_ids_sha256},
    config::{Config, ExactRouteMode, TokenizerMode, TokenizerProfile},
    exact_route_inventory::ExactRouteInventory,
    exact_shadow::{
        ExactPlacementMode, ExactPlacementPolicy, ExactRouteShadow, ExactRouteSnapshot,
    },
    kv_consumer::SharedFencedInventory,
    metrics::Metrics,
    router::Decision,
    shims::Endpoint,
};

const REMOTE_BACKEND: &str = "remote";
const LOCAL_BACKEND: &str = "fastokens";
const MAX_RESPONSE_BYTES: usize = 16 << 20;
const MAX_IDENTITY_BYTES: usize = 64 << 10;
const CANARY_BPS_SCALE: usize = 10_000;
const CANARY_DOMAIN: &[u8] = b"mini-dynamo exact placement canary v1\0";

#[derive(Clone)]
pub struct TokenizerObserver {
    sender: Option<mpsc::Sender<Job>>,
    backend_label: &'static str,
    min_bytes: usize,
    max_bytes: usize,
    metrics: Arc<Metrics>,
    exact_shadow: ExactRouteShadow,
    pre_route: Option<PreRouteTokenizer>,
    attestation: Option<RuntimeAttestation>,
    exact_route_mode: ExactRouteMode,
    exact_route_canary_bps: usize,
    exact_route_canary_key: Option<Arc<[u8]>>,
    exact_placement: ExactPlacementPolicy,
}

#[derive(Debug)]
struct Job {
    endpoint: Endpoint,
    upstream: usize,
    body: Bytes,
    cached_tokens: Option<usize>,
    route_snapshot: Option<ExactRouteSnapshot>,
    local_tokens: Option<ExactTokens>,
}

#[derive(Clone)]
enum Backend {
    Remote {
        tokenizer: RemoteTokenizer,
        exact_shadow: ExactRouteShadow,
    },
    LocalShadow {
        local: Arc<LocalTokenizer>,
        remote: RemoteTokenizer,
        exact_shadow: ExactRouteShadow,
    },
}

#[derive(Clone)]
struct RemoteTokenizer {
    client: reqwest::Client,
    upstreams: Vec<Url>,
    token: Option<String>,
    timeout: Duration,
}

struct LocalTokenizer {
    tokenizer: fastokens::Tokenizer,
    formatter: Arc<dyn dynamo_renderer::OAIPromptFormatter>,
}

#[derive(Clone)]
struct PreRouteTokenizer {
    local: Arc<LocalTokenizer>,
    manifest: Arc<CompatibilityManifest>,
    permits: Arc<Semaphore>,
    timeout: Duration,
}

#[derive(Clone)]
struct RuntimeAttestation {
    manifest: Arc<CompatibilityManifest>,
    remote: RemoteTokenizer,
    ready: Arc<Vec<AtomicBool>>,
    revision: Arc<AtomicU64>,
    metrics: Arc<Metrics>,
}

struct RenderRequest {
    inner: CreateChatCompletionRequest,
    args: HashMap<String, Value>,
    add_generation_prompt: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExactTokens {
    token_ids: Vec<u32>,
}

pub(crate) struct PreRouteTokens {
    pub(crate) tokens: ExactTokens,
    attestation_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CanarySession<'a> {
    Missing,
    Invalid,
    Valid(&'a [u8]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CanaryAssignment {
    NotApplicable,
    Disabled,
    Treatment,
    Control,
    MissingSession,
    InvalidSession,
}

impl CanaryAssignment {
    pub(crate) const fn label(self) -> Option<&'static str> {
        match self {
            Self::NotApplicable => None,
            Self::Disabled => Some("disabled"),
            Self::Treatment => Some("treatment"),
            Self::Control => Some("control"),
            Self::MissingSession => Some("missing_session"),
            Self::InvalidSession => Some("invalid_session"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenizeResponse {
    count: usize,
    tokens: Vec<u32>,
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    Timeout,
    Connect,
    Http,
    ResponseTooLarge,
    Decode,
}

#[derive(Clone, Copy, Debug)]
enum LocalFailure {
    Unsupported,
    Decode,
    Render,
    Encode,
    Join,
}

impl Failure {
    const fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect_error",
            Self::Http => "http_error",
            Self::ResponseTooLarge => "response_too_large",
            Self::Decode => "decode_error",
        }
    }
}

impl LocalFailure {
    const fn label(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported_request",
            Self::Decode => "decode_error",
            Self::Render => "render_error",
            Self::Encode => "encode_error",
            Self::Join => "join_error",
        }
    }
}

impl TokenizerObserver {
    /// Creates the bounded tokenizer observer.
    ///
    /// # Errors
    ///
    /// Returns an error when explicit local-shadow mode cannot load its
    /// tokenizer or DeepSeek-V4 formatter. Off and remote-shadow cannot fail.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        config: &Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        inventories: Arc<[SharedFencedInventory]>,
    ) -> anyhow::Result<Self> {
        Self::with_exact_inventories(
            config,
            client,
            metrics,
            inventories
                .iter()
                .cloned()
                .map(ExactRouteInventory::direct)
                .collect(),
        )
    }

    /// Creates the observer over representation-independent exact inventories.
    ///
    /// # Errors
    ///
    /// Returns the same initialization failures as [`Self::new`].
    #[allow(clippy::too_many_lines)]
    pub fn with_exact_inventories(
        config: &Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        inventories: Arc<[ExactRouteInventory]>,
    ) -> anyhow::Result<Self> {
        let exact_shadow = ExactRouteShadow::with_inventories(
            inventories,
            Arc::clone(&metrics),
            config.route_alpha,
            config.route_max_overlap_blocks,
        );
        let mut pre_route = None;
        let mut attestation = None;
        let sender = match config.tokenizer_mode {
            TokenizerMode::Off => None,
            TokenizerMode::RemoteShadow => {
                let (sender, receiver) = mpsc::channel(config.tokenizer_queue_capacity);
                let backend = Backend::Remote {
                    tokenizer: remote_tokenizer(config, client),
                    exact_shadow: exact_shadow.clone(),
                };
                spawn_workers(receiver, config.tokenizer_workers, &backend, &metrics);
                Some(sender)
            }
            TokenizerMode::LocalShadow => {
                let path = config
                    .tokenizer_path
                    .as_deref()
                    .context("DS4_TOKENIZER_PATH is required in local-shadow mode")?;
                let expected_sha256 = config
                    .tokenizer_sha256
                    .as_deref()
                    .context("DS4_TOKENIZER_SHA256 is required in local-shadow mode")?;
                validate_tokenizer_sha256(path, expected_sha256)?;
                let tokenizer = fastokens::Tokenizer::from_file(Path::new(path))
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
                    .with_context(|| format!("load fastokens tokenizer from {path}"))?;
                let PromptFormatter::OAI(formatter) =
                    deepseek_formatter_for(&Some("deepseek_v4".to_owned()), "deepseek-v4-flash")
                        .context("DeepSeek-V4 formatter unavailable")?;
                let local = Arc::new(LocalTokenizer {
                    tokenizer,
                    formatter,
                });
                if config.exact_route_mode != ExactRouteMode::Off {
                    let manifest_path = config
                        .exact_route_manifest_path
                        .as_deref()
                        .context("DS4_EXACT_ROUTE_MANIFEST_PATH is required")?;
                    let manifest_sha256 = config
                        .exact_route_manifest_sha256
                        .as_deref()
                        .context("DS4_EXACT_ROUTE_MANIFEST_SHA256 is required")?;
                    let tokenizer_sha256 = config
                        .tokenizer_sha256
                        .as_deref()
                        .context("DS4_TOKENIZER_SHA256 is required")?;
                    let manifest = Arc::new(CompatibilityManifest::load(
                        Path::new(manifest_path),
                        manifest_sha256,
                        tokenizer_sha256,
                        tokenizer_profile_label(config.tokenizer_profile),
                    )?);
                    validate_golden_tokens(&local, &manifest)?;
                    let remote = remote_tokenizer(config, client.clone());
                    attestation = Some(RuntimeAttestation::new(
                        Arc::clone(&manifest),
                        remote,
                        Arc::clone(&metrics),
                    ));
                    pre_route = Some(PreRouteTokenizer {
                        local: Arc::clone(&local),
                        manifest,
                        permits: Arc::new(Semaphore::new(config.exact_route_workers)),
                        timeout: Duration::from_millis(config.exact_route_timeout_ms as u64),
                    });
                }
                let (sender, receiver) = mpsc::channel(config.tokenizer_queue_capacity);
                let backend = Backend::LocalShadow {
                    local,
                    remote: remote_tokenizer(config, client),
                    exact_shadow: exact_shadow.clone(),
                };
                spawn_workers(receiver, config.tokenizer_workers, &backend, &metrics);
                Some(sender)
            }
        };
        let backend_label = match config.tokenizer_mode {
            TokenizerMode::LocalShadow => LOCAL_BACKEND,
            TokenizerMode::Off | TokenizerMode::RemoteShadow => REMOTE_BACKEND,
        };
        Ok(Self {
            sender,
            backend_label,
            min_bytes: config.tokenizer_min_bytes,
            max_bytes: config.tokenizer_max_bytes,
            metrics,
            exact_shadow,
            pre_route,
            attestation,
            exact_route_mode: config.exact_route_mode,
            exact_route_canary_bps: config.exact_route_canary_bps,
            exact_route_canary_key: config
                .exact_route_canary_key
                .as_ref()
                .map(|key| Arc::from(key.as_bytes())),
            exact_placement: ExactPlacementPolicy {
                min_gain_tokens: config.exact_route_min_gain_tokens,
                max_load_delta: config.exact_route_max_load_delta,
            },
        })
    }

    /// Decides whether request preparation should derive an exact-token payload.
    /// The default/off mode performs no extra serialization or allocation.
    #[must_use]
    pub fn wants_payload(&self, endpoint: Endpoint, body_bytes: usize) -> bool {
        if self.sender.is_none() {
            return false;
        }
        if !matches!(endpoint, Endpoint::Chat | Endpoint::Completions) {
            self.record(endpoint, "unsupported_endpoint");
            return false;
        }
        if !(self.min_bytes..=self.max_bytes).contains(&body_bytes) {
            self.record(endpoint, "outside_size_window");
            return false;
        }
        true
    }

    /// Captures the fenced inventory versions visible to the approximate route.
    #[must_use]
    pub fn capture_route(&self, decision: &Decision) -> ExactRouteSnapshot {
        self.exact_shadow.capture(decision)
    }

    /// Run admitted local tokenization before the approximate route without
    /// waiting for CPU capacity. Every failure is a telemetry-only fallback.
    pub(crate) async fn prepare_pre_route(
        &self,
        endpoint: Endpoint,
        body: Option<&Bytes>,
    ) -> Option<PreRouteTokens> {
        let Some(pre_route) = &self.pre_route else {
            return None;
        };
        let Some(attestation) = &self.attestation else {
            return None;
        };
        let Some(attestation_revision) = attestation.marker() else {
            self.record_pre_route(endpoint, "unattested");
            return None;
        };
        if !self.exact_shadow.ready() {
            self.record_pre_route(endpoint, "inventory_untrusted");
            return None;
        }
        let Some(body) = body else {
            self.record_pre_route(endpoint, "invalid_payload");
            return None;
        };
        let Ok(permit) = Arc::clone(&pre_route.permits).try_acquire_owned() else {
            self.record_pre_route(endpoint, "busy");
            return None;
        };
        let local = Arc::clone(&pre_route.local);
        let manifest = Arc::clone(&pre_route.manifest);
        let body = body.clone();
        let started = Instant::now();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            local.tokenize_attested(endpoint, &body, &manifest)
        });
        let result = match tokio::time::timeout(pre_route.timeout, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LocalFailure::Join),
            Err(_) => {
                self.metrics
                    .exact_route_preroute_duration
                    .with_label_values(&[endpoint.label(), "tokenize"])
                    .observe(started.elapsed().as_secs_f64());
                self.record_pre_route(endpoint, "timeout");
                return None;
            }
        };
        self.metrics
            .exact_route_preroute_duration
            .with_label_values(&[endpoint.label(), "tokenize"])
            .observe(started.elapsed().as_secs_f64());
        match result {
            Ok(tokens) => {
                if !attestation.still_ready(attestation_revision) {
                    self.record_pre_route(endpoint, "attestation_changed");
                    return None;
                }
                self.metrics
                    .tokenizer_tokens
                    .with_label_values(&[LOCAL_BACKEND, endpoint.label()])
                    .observe(usize_to_f64(tokens.token_ids.len()));
                self.record_pre_route(endpoint, "tokenized");
                Some(PreRouteTokens {
                    tokens,
                    attestation_revision,
                })
            }
            Err(error) => {
                self.record_pre_route(endpoint, error.label());
                None
            }
        }
    }

    pub(crate) fn route_pre_route(
        &self,
        endpoint: Endpoint,
        tokens: &PreRouteTokens,
        assignment: CanaryAssignment,
        decision: &mut Decision,
    ) {
        let Some(attestation) = &self.attestation else {
            return;
        };
        if !attestation.still_ready(tokens.attestation_revision) {
            self.record_pre_route(endpoint, "attestation_changed");
            return;
        }
        let mode = match self.exact_route_mode {
            ExactRouteMode::Placement if assignment == CanaryAssignment::Treatment => {
                ExactPlacementMode::Placement
            }
            ExactRouteMode::Placement if assignment == CanaryAssignment::Control => {
                ExactPlacementMode::Control
            }
            ExactRouteMode::Placement | ExactRouteMode::Shadow | ExactRouteMode::Off => {
                ExactPlacementMode::Shadow
            }
        };
        self.exact_shadow.route_pre_route(
            endpoint,
            &tokens.tokens.token_ids,
            decision,
            self.exact_placement,
            mode,
        );
    }

    pub(crate) fn assign_canary(
        &self,
        endpoint: Endpoint,
        eligible: bool,
        session: CanarySession<'_>,
    ) -> CanaryAssignment {
        if self.exact_route_mode != ExactRouteMode::Placement || !eligible {
            return CanaryAssignment::NotApplicable;
        }
        let assignment = exact_canary_assignment(
            session,
            self.exact_route_canary_key.as_deref(),
            self.exact_route_canary_bps,
        );
        self.metrics
            .exact_route_canary
            .with_label_values(&[endpoint.label(), assignment.label().expect("applicable")])
            .inc();
        assignment
    }

    pub async fn attest_upstream(&self, upstream: usize, models_body: &[u8]) {
        if let Some(attestation) = &self.attestation {
            attestation.check(upstream, models_body).await;
        }
    }

    pub fn invalidate_attestation(&self, upstream: usize) {
        if let Some(attestation) = &self.attestation {
            attestation.invalidate(upstream, "probe_unhealthy");
        }
    }

    /// Enqueues a post-request shadow observation without waiting for capacity.
    pub(crate) fn submit(
        &self,
        endpoint: Endpoint,
        upstream: usize,
        body: Option<Bytes>,
        cached_tokens: Option<usize>,
        route_snapshot: ExactRouteSnapshot,
        local_tokens: Option<ExactTokens>,
    ) {
        let Some(sender) = &self.sender else {
            return;
        };
        let Some(body) = body else {
            self.record(endpoint, "invalid_payload");
            return;
        };
        self.metrics.tokenizer_queue_depth.inc();
        let job = Job {
            endpoint,
            upstream,
            body,
            cached_tokens,
            route_snapshot: Some(route_snapshot),
            local_tokens,
        };
        match sender.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.tokenizer_queue_depth.dec();
                self.record(endpoint, "queue_full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.tokenizer_queue_depth.dec();
                self.record(endpoint, "queue_closed");
            }
        }
    }

    fn record(&self, endpoint: Endpoint, outcome: &str) {
        self.metrics
            .tokenizer_shadow
            .with_label_values(&[self.backend_label, endpoint.label(), outcome])
            .inc();
    }

    fn record_pre_route(&self, endpoint: Endpoint, outcome: &str) {
        self.metrics
            .exact_route_preroute
            .with_label_values(&[endpoint.label(), outcome])
            .inc();
    }
}

fn exact_canary_assignment(
    session: CanarySession<'_>,
    key: Option<&[u8]>,
    canary_bps: usize,
) -> CanaryAssignment {
    if canary_bps == 0 {
        return CanaryAssignment::Disabled;
    }
    match session {
        CanarySession::Missing => CanaryAssignment::MissingSession,
        CanarySession::Invalid => CanaryAssignment::InvalidSession,
        CanarySession::Valid(session_id) => {
            if exact_canary_enrolled(session_id, key, canary_bps) {
                CanaryAssignment::Treatment
            } else {
                CanaryAssignment::Control
            }
        }
    }
}

fn exact_canary_enrolled(session_id: &[u8], key: Option<&[u8]>, canary_bps: usize) -> bool {
    let Some(key) = key else {
        return false;
    };
    if canary_bps == 0 || session_id.is_empty() {
        return false;
    }
    if canary_bps >= CANARY_BPS_SCALE {
        return true;
    }
    let digest = hmac_sha256(key, &[CANARY_DOMAIN, session_id]);
    let value = u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 has eight bytes"));
    u128::from(value) * (CANARY_BPS_SCALE as u128)
        < (canary_bps as u128) * (u128::from(u64::MAX) + 1)
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut normalized = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for ((inner, outer), key_byte) in inner_pad.iter_mut().zip(&mut outer_pad).zip(normalized) {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in parts {
        inner.update(part);
    }
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn validate_tokenizer_sha256(path: &str, expected: &str) -> anyhow::Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read tokenizer artifact {path}"))?;
    let actual = sha256_hex(&bytes);
    anyhow::ensure!(
        actual == expected,
        "tokenizer artifact SHA-256 does not match DS4_TOKENIZER_SHA256"
    );
    Ok(())
}

fn remote_tokenizer(config: &Config, client: reqwest::Client) -> RemoteTokenizer {
    let timeout_ms = u64::try_from(config.tokenizer_timeout_ms).unwrap_or(u64::MAX);
    RemoteTokenizer {
        client,
        upstreams: config.upstreams.clone(),
        token: config.upstream_token.clone(),
        timeout: Duration::from_millis(timeout_ms),
    }
}

impl RuntimeAttestation {
    fn new(
        manifest: Arc<CompatibilityManifest>,
        remote: RemoteTokenizer,
        metrics: Arc<Metrics>,
    ) -> Self {
        let ready = (0..remote.upstreams.len())
            .map(|_| AtomicBool::new(false))
            .collect();
        Self {
            manifest,
            remote,
            ready: Arc::new(ready),
            revision: Arc::new(AtomicU64::new(0)),
            metrics,
        }
    }

    fn all_ready(&self) -> bool {
        !self.ready.is_empty() && self.ready.iter().all(|ready| ready.load(Ordering::Acquire))
    }

    fn marker(&self) -> Option<u64> {
        let before = self.revision.load(Ordering::Acquire);
        if !self.all_ready() {
            return None;
        }
        let after = self.revision.load(Ordering::Acquire);
        (before == after).then_some(after)
    }

    fn still_ready(&self, revision: u64) -> bool {
        self.revision.load(Ordering::Acquire) == revision && self.all_ready()
    }

    async fn check(&self, upstream: usize, models_body: &[u8]) {
        if !self.begin_check(upstream) {
            return;
        }
        let outcome = match self.remote.version(upstream).await {
            Ok(version) => self.manifest.runtime_outcome(models_body, &version),
            Err(error) => {
                self.set(upstream, false, &format!("version_{}", error.label()));
                return;
            }
        };
        self.set(upstream, outcome == RuntimeOutcome::Match, outcome.label());
    }

    fn invalidate(&self, upstream: usize, outcome: &str) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.set(upstream, false, outcome);
    }

    fn begin_check(&self, upstream: usize) -> bool {
        let Some(state) = self.ready.get(upstream) else {
            return false;
        };
        self.revision.fetch_add(1, Ordering::AcqRel);
        state.store(false, Ordering::Release);
        let Some(url) = self.remote.upstreams.get(upstream) else {
            return false;
        };
        self.metrics
            .compat_attested
            .with_label_values(&[url.as_str().trim_end_matches('/')])
            .set(0.0);
        true
    }

    fn set(&self, upstream: usize, ready: bool, outcome: &str) {
        let Some(state) = self.ready.get(upstream) else {
            return;
        };
        state.store(ready, Ordering::Release);
        let Some(url) = self.remote.upstreams.get(upstream) else {
            return;
        };
        let label = url.as_str().trim_end_matches('/');
        self.metrics
            .compat_attested
            .with_label_values(&[label])
            .set(if ready { 1.0 } else { 0.0 });
        self.metrics
            .compat_attestation_checks
            .with_label_values(&[label, outcome])
            .inc();
    }
}

fn tokenizer_profile_label(profile: TokenizerProfile) -> &'static str {
    match profile {
        TokenizerProfile::DeepSeekV4R34 => "deepseek-v4-r34",
    }
}

fn validate_golden_tokens(
    local: &LocalTokenizer,
    manifest: &CompatibilityManifest,
) -> anyhow::Result<()> {
    for golden in &manifest.goldens {
        let body = serde_json::to_vec(&golden.request)
            .with_context(|| format!("serialize tokenizer golden {}", golden.name))?;
        let tokens = local
            .tokenize_attested(Endpoint::Chat, &body, manifest)
            .map_err(|error| anyhow::anyhow!(error.label()))
            .with_context(|| format!("render tokenizer golden {}", golden.name))?;
        anyhow::ensure!(
            tokens.token_ids.len() == golden.token_count,
            "tokenizer golden {} count mismatch",
            golden.name
        );
        anyhow::ensure!(
            token_ids_sha256(&tokens.token_ids) == golden.token_ids_sha256,
            "tokenizer golden {} token-ID mismatch",
            golden.name
        );
    }
    Ok(())
}

fn spawn_workers(
    receiver: mpsc::Receiver<Job>,
    worker_count: usize,
    backend: &Backend,
    metrics: &Arc<Metrics>,
) {
    let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
    for _ in 0..worker_count {
        let receiver = Arc::clone(&receiver);
        let backend = backend.clone();
        let metrics = Arc::clone(metrics);
        tokio::spawn(async move {
            loop {
                let job = {
                    let mut receiver = receiver.lock().await;
                    receiver.recv().await
                };
                let Some(job) = job else {
                    break;
                };
                metrics.tokenizer_queue_depth.dec();
                observe(&backend, &metrics, job).await;
            }
        });
    }
}

async fn observe(backend: &Backend, metrics: &Metrics, job: Job) {
    let endpoint = job.endpoint.label();
    match backend {
        Backend::Remote {
            tokenizer: remote,
            exact_shadow,
        } => {
            let started = Instant::now();
            let result = remote.tokenize(&job).await;
            record_remote(metrics, endpoint, started.elapsed(), &result);
            if let Ok(tokens) = &result {
                observe_exact(exact_shadow, &job, REMOTE_BACKEND, tokens);
            }
        }
        Backend::LocalShadow {
            local,
            remote,
            exact_shadow,
        } => observe_local(local, remote, exact_shadow, metrics, job).await,
    }
}

async fn observe_local(
    local: &Arc<LocalTokenizer>,
    remote: &RemoteTokenizer,
    exact_shadow: &ExactRouteShadow,
    metrics: &Metrics,
    job: Job,
) {
    let endpoint = job.endpoint.label();
    let local = Arc::clone(local);
    let local_endpoint = job.endpoint;
    let local_body = job.body.clone();
    let pretokenized = job.local_tokens.clone();
    let local_future = async move {
        if let Some(tokens) = pretokenized {
            return (Duration::ZERO, Ok(tokens), true);
        }
        let started = Instant::now();
        let result =
            tokio::task::spawn_blocking(move || local.tokenize(local_endpoint, &local_body))
                .await
                .map_err(|_| LocalFailure::Join)
                .and_then(std::convert::identity);
        (started.elapsed(), result, false)
    };
    let remote_future = async {
        let started = Instant::now();
        let result = remote.tokenize(&job).await;
        (started.elapsed(), result)
    };
    let ((local_duration, local_result, local_reused), (remote_duration, remote_result)) =
        tokio::join!(local_future, remote_future);
    record_remote(metrics, endpoint, remote_duration, &remote_result);
    if !local_reused {
        metrics
            .tokenizer_duration
            .with_label_values(&[LOCAL_BACKEND, endpoint])
            .observe(local_duration.as_secs_f64());
    }
    match (&local_result, &remote_result) {
        (Ok(local), Ok(remote)) => {
            record_local_tokens(metrics, endpoint, local, local_reused);
            let parity = local == remote;
            metrics
                .tokenizer_shadow
                .with_label_values(&[
                    LOCAL_BACKEND,
                    endpoint,
                    if parity {
                        "parity_match"
                    } else {
                        "parity_mismatch"
                    },
                ])
                .inc();
            observe_exact(
                exact_shadow,
                &job,
                if parity {
                    LOCAL_BACKEND
                } else {
                    REMOTE_BACKEND
                },
                if parity { local } else { remote },
            );
        }
        (Err(error), remote) => {
            metrics
                .tokenizer_shadow
                .with_label_values(&[LOCAL_BACKEND, endpoint, error.label()])
                .inc();
            if let Ok(remote) = remote {
                observe_exact(exact_shadow, &job, REMOTE_BACKEND, remote);
            }
        }
        (Ok(local), Err(_)) => {
            record_local_tokens(metrics, endpoint, local, local_reused);
            metrics
                .tokenizer_shadow
                .with_label_values(&[LOCAL_BACKEND, endpoint, "remote_authority_unavailable"])
                .inc();
        }
    }
}

fn record_local_tokens(metrics: &Metrics, endpoint: &str, tokens: &ExactTokens, reused: bool) {
    if !reused {
        metrics
            .tokenizer_tokens
            .with_label_values(&[LOCAL_BACKEND, endpoint])
            .observe(usize_to_f64(tokens.token_ids.len()));
    }
}

fn observe_exact(shadow: &ExactRouteShadow, job: &Job, token_backend: &str, tokens: &ExactTokens) {
    let Some(route_snapshot) = &job.route_snapshot else {
        return;
    };
    shadow.observe(
        token_backend,
        job.endpoint,
        job.upstream,
        job.cached_tokens,
        &tokens.token_ids,
        route_snapshot,
    );
}

fn record_remote(
    metrics: &Metrics,
    endpoint: &str,
    duration: Duration,
    result: &Result<ExactTokens, Failure>,
) {
    metrics
        .tokenizer_duration
        .with_label_values(&[REMOTE_BACKEND, endpoint])
        .observe(duration.as_secs_f64());
    match result {
        Ok(tokens) => {
            metrics
                .tokenizer_tokens
                .with_label_values(&[REMOTE_BACKEND, endpoint])
                .observe(usize_to_f64(tokens.token_ids.len()));
            metrics
                .tokenizer_shadow
                .with_label_values(&[REMOTE_BACKEND, endpoint, "success"])
                .inc();
        }
        Err(error) => {
            metrics
                .tokenizer_shadow
                .with_label_values(&[REMOTE_BACKEND, endpoint, error.label()])
                .inc();
        }
    }
}

impl LocalTokenizer {
    fn tokenize(&self, endpoint: Endpoint, body: &[u8]) -> Result<ExactTokens, LocalFailure> {
        let request: Value = serde_json::from_slice(body).map_err(|_| LocalFailure::Decode)?;
        match endpoint {
            Endpoint::Chat => self.tokenize_chat(request),
            Endpoint::Completions => {
                let prompt = request
                    .get("prompt")
                    .and_then(Value::as_str)
                    .ok_or(LocalFailure::Unsupported)?;
                self.encode(prompt)
            }
            _ => Err(LocalFailure::Unsupported),
        }
    }

    fn tokenize_chat(&self, request: Value) -> Result<ExactTokens, LocalFailure> {
        let object = request.as_object().ok_or(LocalFailure::Decode)?;
        if object
            .get("documents")
            .is_some_and(|value| !value.is_null())
            || has_tool_history(object)
        {
            return Err(LocalFailure::Unsupported);
        }
        let add_generation_prompt = object
            .get("add_generation_prompt")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !add_generation_prompt {
            return Err(LocalFailure::Unsupported);
        }
        let mut args = object
            .get("chat_template_kwargs")
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        apply_node06_vllm_profile(&mut args)?;
        let inner = serde_json::from_value::<CreateChatCompletionRequest>(request)
            .map_err(|_| LocalFailure::Decode)?;
        let request = RenderRequest {
            inner,
            args,
            add_generation_prompt,
        };
        let prompt = self
            .formatter
            .render(&request)
            .map_err(|_| LocalFailure::Render)?;
        self.encode(&prompt)
    }

    fn tokenize_attested(
        &self,
        endpoint: Endpoint,
        body: &[u8],
        manifest: &CompatibilityManifest,
    ) -> Result<ExactTokens, LocalFailure> {
        if endpoint != Endpoint::Chat {
            return Err(LocalFailure::Unsupported);
        }
        let request: Value = serde_json::from_slice(body).map_err(|_| LocalFailure::Decode)?;
        let object = request.as_object().ok_or(LocalFailure::Decode)?;
        if object.get("model").and_then(Value::as_str) != Some(manifest.model.id.as_str()) {
            return Err(LocalFailure::Unsupported);
        }
        let class = attested_request_class(object)?;
        if !manifest.admitted(class) {
            return Err(LocalFailure::Unsupported);
        }
        self.tokenize_chat(request)
    }

    fn encode(&self, prompt: &str) -> Result<ExactTokens, LocalFailure> {
        let token_ids = self
            .tokenizer
            .encode(prompt)
            .map_err(|_| LocalFailure::Encode)?;
        Ok(ExactTokens { token_ids })
    }
}

fn attested_request_class(
    object: &serde_json::Map<String, Value>,
) -> Result<&'static str, LocalFailure> {
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(LocalFailure::Unsupported)?;
    if messages.is_empty()
        || object
            .get("documents")
            .is_some_and(|value| !value.is_null())
        || has_tool_history(object)
        || !object
            .get("add_generation_prompt")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        || [
            "response_format",
            "tool_choice",
            "function_call",
            "parallel_tool_calls",
            "chat_template",
            "add_special_tokens",
            "truncate_prompt_tokens",
            "think",
            "thinking",
        ]
        .iter()
        .any(|key| object.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(LocalFailure::Unsupported);
    }
    let args = object
        .get("chat_template_kwargs")
        .and_then(Value::as_object);
    if args.is_some_and(|args| {
        args.keys().any(|key| {
            !matches!(
                key.as_str(),
                "enable_thinking" | "thinking" | "reasoning_effort"
            )
        })
    }) {
        return Err(LocalFailure::Unsupported);
    }
    let top_effort = object.get("reasoning_effort").and_then(Value::as_str);
    let arg_effort = args
        .and_then(|args| args.get("reasoning_effort"))
        .and_then(Value::as_str);
    if top_effort.is_some() && arg_effort.is_some() && top_effort != arg_effort {
        return Err(LocalFailure::Unsupported);
    }
    let effort = top_effort.or(arg_effort);
    let thinking = args
        .and_then(|args| args.get("enable_thinking").or_else(|| args.get("thinking")))
        .and_then(Value::as_bool);
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if tools {
        let one_user_message =
            messages.len() == 1 && messages[0].get("role").and_then(Value::as_str) == Some("user");
        if !one_user_message || effort.is_some() || thinking.is_some() {
            return Err(LocalFailure::Unsupported);
        }
        return Ok("tools_declared");
    }
    if let Some(effort) = effort {
        return match effort {
            "high" => Ok("reasoning_high"),
            "none" => Ok("reasoning_none"),
            "minimal" => Ok("reasoning_minimal"),
            "low" => Ok("reasoning_low"),
            "medium" => Ok("reasoning_medium"),
            _ => Err(LocalFailure::Unsupported),
        };
    }
    if thinking == Some(false) {
        return Ok("thinking_disabled");
    }
    if messages.len() > 1
        || messages.iter().any(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
    {
        return Ok("system_multiturn");
    }
    Ok("plain")
}

fn has_tool_history(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("tool")
                    || message
                        .get("tool_calls")
                        .is_some_and(|value| !value.is_null())
                    || message
                        .get("function_call")
                        .is_some_and(|value| !value.is_null())
            })
        })
}

/// Translate the active node06 vLLM r34 DeepSeek-V4 template profile into
/// Dynamo renderer semantics. `max`/`xhigh` deliberately fall back to the
/// remote authority because this renderer release lacks vLLM's newer
/// "Beyond maximum" preamble.
fn apply_node06_vllm_profile(args: &mut HashMap<String, Value>) -> Result<(), LocalFailure> {
    let thinking = args
        .get("enable_thinking")
        .or_else(|| args.get("thinking"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !thinking {
        args.insert("thinking".to_owned(), Value::Bool(false));
        args.insert(
            "reasoning_effort".to_owned(),
            Value::String("none".to_owned()),
        );
        return Ok(());
    }
    let effort = args
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .unwrap_or("high");
    let mapped = match effort {
        "none" | "low" => "none",
        "minimal" | "medium" | "high" | "auto" => "max",
        _ => return Err(LocalFailure::Unsupported),
    };
    args.insert(
        "reasoning_effort".to_owned(),
        Value::String(mapped.to_owned()),
    );
    Ok(())
}

impl OAIChatLikeRequest for RenderRequest {
    fn model(&self) -> String {
        OAIChatLikeRequest::model(&self.inner)
    }

    fn messages(&self) -> minijinja::Value {
        OAIChatLikeRequest::messages(&self.inner)
    }

    fn typed_messages(&self) -> Option<&[dynamo_protocols::types::ChatCompletionRequestMessage]> {
        Some(self.inner.messages.as_slice())
    }

    fn tools(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::tools(&self.inner)
    }

    fn tool_choice(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::tool_choice(&self.inner)
    }

    fn response_format(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::response_format(&self.inner)
    }

    fn reasoning_effort(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::reasoning_effort(&self.inner)
    }

    fn should_add_generation_prompt(&self) -> bool {
        self.add_generation_prompt
    }

    fn chat_template_args(&self) -> Option<&HashMap<String, Value>> {
        Some(&self.args)
    }

    fn extract_text(&self) -> Option<TextInput> {
        Some(TextInput::Single(String::new()))
    }
}

impl RemoteTokenizer {
    async fn version(&self, upstream: usize) -> Result<Vec<u8>, Failure> {
        let Some(upstream) = self.upstreams.get(upstream) else {
            return Err(Failure::Connect);
        };
        let mut url = upstream.clone();
        let base_path = upstream.path().trim_end_matches('/');
        url.set_path(&format!("{base_path}/version"));
        url.set_query(None);
        let mut request = self.client.get(url).timeout(self.timeout);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| classify_request_error(&error))?;
        if !response.status().is_success() {
            return Err(Failure::Http);
        }
        bounded_response_body(response, MAX_IDENTITY_BYTES).await
    }

    async fn tokenize(&self, job: &Job) -> Result<ExactTokens, Failure> {
        let Some(upstream) = self.upstreams.get(job.upstream) else {
            return Err(Failure::Connect);
        };
        let mut url = upstream.clone();
        let base_path = upstream.path().trim_end_matches('/');
        url.set_path(&format!("{base_path}/tokenize"));
        url.set_query(None);
        let mut request = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .timeout(self.timeout)
            .body(job.body.clone());
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| classify_request_error(&error))?;
        if !response.status().is_success() {
            return Err(Failure::Http);
        }
        let body = bounded_response_body(response, MAX_RESPONSE_BYTES).await?;
        let response: TokenizeResponse =
            serde_json::from_slice(&body).map_err(|_| Failure::Decode)?;
        if response.count != response.tokens.len() {
            return Err(Failure::Decode);
        }
        Ok(ExactTokens {
            token_ids: response.tokens,
        })
    }
}

async fn bounded_response_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, Failure> {
    if response
        .content_length()
        .is_some_and(|size| size > max_bytes as u64)
    {
        return Err(Failure::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| classify_request_error(&error))?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(Failure::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_request_error(error: &reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::Timeout
    } else if error.is_connect() {
        Failure::Connect
    } else {
        Failure::Http
    }
}

#[allow(clippy::cast_precision_loss)]
fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, body::to_bytes, http::Request, routing::get, routing::post};
    use prometheus::Registry;

    use super::*;
    use crate::compat::{EngineIdentity, ModelIdentity, RendererIdentity, TokenizerIdentity};

    fn route_decision() -> crate::router::Decision {
        crate::router::Decision {
            candidates: vec![0],
            candidate_state: vec![crate::router::CandidateState {
                index: 0,
                rank: 0,
                overlap_blocks: 0,
                affinity_blocks: 0,
                load_units: 0,
                request_load_units: 1,
                healthy: true,
            }],
            overlap_blocks: 0,
            total_blocks: 1,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: crate::router::Outcome::Single,
        }
    }

    fn test_manifest(version: &str) -> CompatibilityManifest {
        CompatibilityManifest {
            schema_version: 1,
            model: ModelIdentity {
                id: "model".to_owned(),
                root: "root".to_owned(),
                max_model_len: 4096,
            },
            engine: EngineIdentity {
                version: version.to_owned(),
                image_digest: format!("sha256:{}", "a".repeat(64)),
            },
            tokenizer: TokenizerIdentity {
                sha256: "b".repeat(64),
            },
            renderer: RendererIdentity {
                profile: "profile".to_owned(),
            },
            admitted_request_classes: vec![
                "plain".to_owned(),
                "system_multiturn".to_owned(),
                "tools_declared".to_owned(),
                "reasoning_high".to_owned(),
                "thinking_disabled".to_owned(),
            ],
            goldens: Vec::new(),
        }
    }

    #[tokio::test]
    async fn remote_backend_returns_exact_ids_and_authenticates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let app = Router::new().route(
            "/tokenize",
            post(move |request: Request<axum::body::Body>| {
                let observed = Arc::clone(&observed);
                async move {
                    assert_eq!(request.headers()["authorization"], "Bearer secret");
                    let body = to_bytes(request.into_body(), 4096).await.unwrap();
                    assert!(String::from_utf8_lossy(&body).contains("messages"));
                    observed.fetch_add(1, Ordering::Relaxed);
                    axum::Json(serde_json::json!({"count": 3, "tokens": [7, 11, 13]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let backend = RemoteTokenizer {
            client: reqwest::Client::new(),
            upstreams: vec![url],
            token: Some("secret".to_owned()),
            timeout: Duration::from_secs(1),
        };
        let result = backend
            .tokenize(&Job {
                endpoint: Endpoint::Chat,
                upstream: 0,
                body: Bytes::from_static(br#"{"messages":[]}"#),
                cached_tokens: None,
                route_snapshot: None,
                local_tokens: None,
            })
            .await
            .unwrap();
        assert_eq!(result.token_ids, [7, 11, 13]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn runtime_attestation_requires_matching_models_and_version() {
        let app = Router::new().route(
            "/version",
            get(|| async { axum::Json(serde_json::json!({"version": "v1"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let attestation = RuntimeAttestation::new(
            Arc::new(test_manifest("v1")),
            RemoteTokenizer {
                client: reqwest::Client::new(),
                upstreams: vec![url],
                token: None,
                timeout: Duration::from_secs(1),
            },
            Arc::clone(&metrics),
        );
        attestation
            .check(
                0,
                br#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#,
            )
            .await;
        assert!(attestation.all_ready());
        let revision = attestation.marker().unwrap();
        assert!(attestation.still_ready(revision));
        assert!(
            (metrics
                .compat_attested
                .with_label_values(&[attestation.remote.upstreams[0]
                    .as_str()
                    .trim_end_matches('/'),])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        attestation.invalidate(0, "test");
        assert!(!attestation.still_ready(revision));
        assert!(attestation.marker().is_none());
        server.abort();
    }

    #[test]
    fn off_mode_does_not_prepare_or_enqueue() {
        let config = Config::from_lookup(|_| None).unwrap();
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let observer =
            TokenizerObserver::new(&config, reqwest::Client::new(), metrics, Arc::from([]))
                .unwrap();
        assert!(!observer.wants_payload(Endpoint::Chat, 100_000));
        observer.submit(
            Endpoint::Chat,
            0,
            None,
            None,
            observer.capture_route(&route_decision()),
            None,
        );
    }

    #[tokio::test]
    async fn selective_shadow_records_success_without_exposing_ids() {
        let app = Router::new().route(
            "/tokenize",
            post(|| async { axum::Json(serde_json::json!({"count": 2, "tokens": [3, 5]})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let values = HashMap::from([
            ("DS4_UPSTREAM", url),
            ("DS4_TOKENIZER_MODE", "remote-shadow".to_owned()),
            ("DS4_TOKENIZER_MIN_BYTES", "0".to_owned()),
        ]);
        let config = Config::from_lookup(|key| values.get(key).cloned()).unwrap();
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let observer = TokenizerObserver::new(
            &config,
            reqwest::Client::new(),
            Arc::clone(&metrics),
            Arc::from([]),
        )
        .unwrap();
        assert!(observer.wants_payload(Endpoint::Chat, 16));
        observer.submit(
            Endpoint::Chat,
            0,
            Some(Bytes::from_static(br#"{"messages":[]}"#)),
            None,
            observer.capture_route(&route_decision()),
            None,
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let successes = metrics
                    .tokenizer_shadow
                    .with_label_values(&[REMOTE_BACKEND, "chat", "success"])
                    .get();
                if successes >= 1.0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(metrics.tokenizer_queue_depth.get().abs() < f64::EPSILON);
        server.abort();
    }

    #[test]
    fn node06_profile_matches_observed_vllm_effort_classes() {
        for (effort, expected) in [
            ("none", "none"),
            ("low", "none"),
            ("minimal", "max"),
            ("medium", "max"),
            ("high", "max"),
        ] {
            let mut args = HashMap::from([(
                "reasoning_effort".to_owned(),
                Value::String(effort.to_owned()),
            )]);
            apply_node06_vllm_profile(&mut args).unwrap();
            assert_eq!(args["reasoning_effort"], expected);
        }
        for effort in ["xhigh", "max"] {
            let mut args = HashMap::from([(
                "reasoning_effort".to_owned(),
                Value::String(effort.to_owned()),
            )]);
            assert!(apply_node06_vllm_profile(&mut args).is_err());
        }
    }

    #[test]
    fn tool_history_stays_on_remote_authority() {
        let declared = serde_json::json!({
            "messages": [{"role": "user", "content": "use a tool"}],
            "tools": [{"type": "function", "function": {"name": "lookup"}}]
        });
        assert!(!has_tool_history(declared.as_object().unwrap()));

        let history = serde_json::json!({
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call-1"}]},
                {"role": "tool", "tool_call_id": "call-1", "content": "ok"}
            ]
        });
        assert!(has_tool_history(history.as_object().unwrap()));
    }

    #[test]
    fn pre_route_admission_rejects_ungoldened_feature_combinations() {
        let plain = serde_json::json!({
            "model": "model",
            "messages": [{"role": "user", "content": "hello"}],
            "add_generation_prompt": true
        });
        assert_eq!(
            attested_request_class(plain.as_object().unwrap()).unwrap(),
            "plain"
        );
        let custom_template = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "chat_template_kwargs": {"custom": true}
        });
        assert!(attested_request_class(custom_template.as_object().unwrap()).is_err());
        let tools_and_reasoning = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"type": "function", "function": {"name": "lookup"}}],
            "reasoning_effort": "high"
        });
        assert!(attested_request_class(tools_and_reasoning.as_object().unwrap()).is_err());
        let custom_template = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "chat_template": "{{ messages }}"
        });
        assert!(attested_request_class(custom_template.as_object().unwrap()).is_err());
        let truncated = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "truncate_prompt_tokens": 1024
        });
        assert!(attested_request_class(truncated.as_object().unwrap()).is_err());
    }

    #[test]
    fn artifact_digest_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn exact_canary_is_session_stable_bounded_and_keyed() {
        let key = b"0123456789abcdef0123456789abcdef";
        assert!(!exact_canary_enrolled(b"session-a", Some(key), 0));
        assert!(!exact_canary_enrolled(b"session-a", None, 10_000));
        assert!(!exact_canary_enrolled(b"", Some(key), 10_000));
        assert!(exact_canary_enrolled(b"session-a", Some(key), 10_000));

        let enrolled = exact_canary_enrolled(b"session-a", Some(key), 5_000);
        for _ in 0..100 {
            assert_eq!(
                exact_canary_enrolled(b"session-a", Some(key), 5_000),
                enrolled
            );
        }
        let other_key = b"fedcba9876543210fedcba9876543210";
        let changed = (0..100_u32).any(|session| {
            let session = session.to_be_bytes();
            exact_canary_enrolled(&session, Some(key), 5_000)
                != exact_canary_enrolled(&session, Some(other_key), 5_000)
        });
        assert!(changed, "rotating the HMAC key must change some cohorts");

        let admitted = (0..10_000_u32)
            .filter(|session| exact_canary_enrolled(&session.to_be_bytes(), Some(key), 1_000))
            .count();
        assert!((850..=1_150).contains(&admitted), "admitted={admitted}");

        let lower = (0..10_000_u32)
            .filter(|session| exact_canary_enrolled(&session.to_be_bytes(), Some(key), 100))
            .collect::<std::collections::HashSet<_>>();
        let higher = (0..10_000_u32)
            .filter(|session| exact_canary_enrolled(&session.to_be_bytes(), Some(key), 500))
            .collect::<std::collections::HashSet<_>>();
        assert!(lower.is_subset(&higher));
    }

    #[test]
    fn exact_canary_assignment_has_an_instant_fail_closed_zero() {
        let key = b"0123456789abcdef0123456789abcdef";
        assert_eq!(
            exact_canary_assignment(CanarySession::Valid(b"session"), Some(key), 0),
            CanaryAssignment::Disabled
        );
        assert_eq!(
            exact_canary_assignment(CanarySession::Missing, Some(key), 10_000),
            CanaryAssignment::MissingSession
        );
        assert_eq!(
            exact_canary_assignment(CanarySession::Invalid, Some(key), 10_000),
            CanaryAssignment::InvalidSession
        );
        assert_eq!(
            exact_canary_assignment(CanarySession::Valid(b"session"), Some(key), 10_000),
            CanaryAssignment::Treatment
        );
    }

    #[test]
    fn hmac_sha256_matches_the_standard_known_vector() {
        let digest = hmac_sha256(b"key", &[b"The quick brown fox jumps over the lazy dog"]);
        assert_eq!(
            digest,
            [
                0xf7, 0xbc, 0x83, 0xf4, 0x30, 0x53, 0x84, 0x24, 0xb1, 0x32, 0x98, 0xe6, 0xaa, 0x6f,
                0xb1, 0x43, 0xef, 0x4d, 0x59, 0xa1, 0x49, 0x46, 0x17, 0x59, 0x97, 0x47, 0x9d, 0xbc,
                0x2d, 0x1a, 0x3c, 0xd8,
            ]
        );

        let long_key = [0xaa; 131];
        let digest = hmac_sha256(
            &long_key,
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"],
        );
        assert_eq!(
            digest,
            [
                0x60, 0xe4, 0x31, 0x59, 0x1e, 0xe0, 0xb6, 0x7f, 0x0d, 0x8a, 0x26, 0xaa, 0xcb, 0xf5,
                0xb7, 0x7f, 0x8e, 0x0b, 0xc6, 0x21, 0x37, 0x28, 0xc5, 0x14, 0x05, 0x46, 0x04, 0x0f,
                0x0e, 0xe3, 0x7f, 0x54,
            ]
        );
    }

    #[test]
    fn exact_canary_domain_and_big_endian_threshold_are_golden() {
        let key = b"0123456789abcdef0123456789abcdef";
        let digest = hmac_sha256(key, &[CANARY_DOMAIN, b"session-a"]);
        assert_eq!(
            digest,
            [
                0xa9, 0xd2, 0xc4, 0x0e, 0x41, 0x5a, 0x52, 0xc6, 0x11, 0xf6, 0xc4, 0x3e, 0x8f, 0x35,
                0xc0, 0x73, 0x62, 0x2b, 0xcb, 0x2d, 0xd9, 0x6e, 0x8b, 0x5d, 0x36, 0x3c, 0xab, 0x43,
                0x58, 0xa3, 0x23, 0xd9,
            ]
        );
        assert!(!exact_canary_enrolled(b"session-a", Some(key), 5_000));
    }
}
