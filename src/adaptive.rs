//! Embedded, default-off engine topology controller.
//!
//! The controller has a deliberately tiny Docker authority: configured,
//! pre-created containers may be inspected, started, and stopped. It never
//! creates, removes, pulls, execs, or accepts a container name from an API
//! request. Routing membership remains in-process with the proxy, so draining
//! and admission share one authority boundary.

use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use parking_lot::Mutex;
use prometheus::{CounterVec, core::Collector};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::proxy::Proxy;

const CONFIG_ENV: &str = "RJ_ADAPTIVE_CONFIG_PATH";
const MAX_CONFIG_BYTES: u64 = 1 << 20;
const MIN_POLL_SECONDS: u64 = 2;
const MAX_POLL_SECONDS: u64 = 300;
const MAX_WAIT_SECONDS: u64 = 30 * 60;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Off,
    Manual,
    Recommend,
    Auto,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    RequestsPerSecond,
    PromptTokensPerSecond,
    CompletionTokensPerSecond,
    TokensPerSecond,
    Inflight,
    LoadPerEngine,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Above,
    Below,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    version: u32,
    mode: Mode,
    active_profile: String,
    state_path: PathBuf,
    docker_socket: PathBuf,
    deployment_lock_path: PathBuf,
    #[serde(default = "default_poll_seconds")]
    poll_seconds: u64,
    #[serde(default = "default_drain_timeout_seconds")]
    drain_timeout_seconds: u64,
    #[serde(default = "default_start_timeout_seconds")]
    start_timeout_seconds: u64,
    #[serde(default = "default_stable_seconds")]
    stable_seconds: u64,
    profiles: Vec<Profile>,
    transitions: Vec<Transition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub label: String,
    pub description: String,
    pub engines: Vec<Engine>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub upstream: usize,
    pub label: String,
    pub container: String,
    pub image: String,
    pub gpus: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transition {
    from: String,
    to: String,
    #[serde(default)]
    automatic: bool,
    allow_downtime: bool,
    estimated_downtime_seconds: u64,
    #[serde(default)]
    condition: Option<Condition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Condition {
    metric: Metric,
    comparison: Comparison,
    threshold: f64,
    for_seconds: u64,
}

const fn default_poll_seconds() -> u64 {
    5
}

const fn default_drain_timeout_seconds() -> u64 {
    120
}

const fn default_start_timeout_seconds() -> u64 {
    15 * 60
}

const fn default_stable_seconds() -> u64 {
    15
}

impl Config {
    fn load(path: &Path, upstreams: usize) -> anyhow::Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect adaptive config {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file(),
            "adaptive config must be a regular file"
        );
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "adaptive config must not be group/world writable"
        );
        ensure!(
            metadata.len() <= MAX_CONFIG_BYTES,
            "adaptive config exceeds 1 MiB"
        );
        let bytes =
            fs::read(path).with_context(|| format!("read adaptive config {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes).context("parse adaptive config JSON")?;
        config.validate(upstreams)?;
        Ok(config)
    }

    #[allow(clippy::too_many_lines)]
    fn validate(&self, upstreams: usize) -> anyhow::Result<()> {
        ensure!(self.version == 1, "adaptive config version must be 1");
        ensure!(
            !self.profiles.is_empty(),
            "adaptive config needs at least one profile"
        );
        ensure!(
            (MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&self.poll_seconds),
            "adaptive poll_seconds must be between {MIN_POLL_SECONDS} and {MAX_POLL_SECONDS}"
        );
        for (name, value) in [
            ("drain_timeout_seconds", self.drain_timeout_seconds),
            ("start_timeout_seconds", self.start_timeout_seconds),
            ("stable_seconds", self.stable_seconds),
        ] {
            ensure!(
                value > 0 && value <= MAX_WAIT_SECONDS,
                "adaptive {name} is out of range"
            );
        }
        for (name, path) in [
            ("state_path", &self.state_path),
            ("docker_socket", &self.docker_socket),
            ("deployment_lock_path", &self.deployment_lock_path),
        ] {
            ensure!(path.is_absolute(), "adaptive {name} must be absolute");
        }

        let mut ids = HashSet::new();
        let mut containers = HashMap::<&str, &str>::new();
        for profile in &self.profiles {
            validate_id("profile id", &profile.id)?;
            ensure!(
                ids.insert(profile.id.as_str()),
                "duplicate adaptive profile {}",
                profile.id
            );
            ensure!(
                !profile.label.trim().is_empty(),
                "adaptive profile {} has no label",
                profile.id
            );
            ensure!(
                !profile.engines.is_empty(),
                "adaptive profile {} has no engines",
                profile.id
            );
            let mut profile_upstreams = HashSet::new();
            let mut profile_gpus = HashSet::new();
            for engine in &profile.engines {
                ensure!(
                    engine.upstream < upstreams,
                    "adaptive profile {} upstream {} is out of range",
                    profile.id,
                    engine.upstream
                );
                ensure!(
                    profile_upstreams.insert(engine.upstream),
                    "adaptive profile {} repeats upstream {}",
                    profile.id,
                    engine.upstream
                );
                validate_container(&engine.container)?;
                ensure!(
                    engine.image.starts_with("sha256:") && engine.image.len() == 71,
                    "adaptive engine {} image must be an immutable sha256 ID",
                    engine.container
                );
                ensure!(
                    !engine.gpus.is_empty(),
                    "adaptive engine {} has no GPUs",
                    engine.container
                );
                for gpu in &engine.gpus {
                    ensure!(
                        profile_gpus.insert(*gpu),
                        "adaptive profile {} assigns GPU {} twice",
                        profile.id,
                        gpu
                    );
                }
                ensure!(
                    containers.insert(&engine.container, &profile.id).is_none(),
                    "adaptive container {} is configured more than once",
                    engine.container
                );
            }
        }
        ensure!(
            ids.contains(self.active_profile.as_str()),
            "adaptive active_profile does not exist"
        );

        let mut edges = HashSet::new();
        for transition in &self.transitions {
            ensure!(
                ids.contains(transition.from.as_str()),
                "adaptive transition source {} does not exist",
                transition.from
            );
            ensure!(
                ids.contains(transition.to.as_str()),
                "adaptive transition target {} does not exist",
                transition.to
            );
            ensure!(
                transition.from != transition.to,
                "adaptive transition cannot target itself"
            );
            ensure!(
                edges.insert((transition.from.as_str(), transition.to.as_str())),
                "duplicate adaptive transition {} -> {}",
                transition.from,
                transition.to
            );
            ensure!(
                transition.estimated_downtime_seconds <= MAX_WAIT_SECONDS,
                "adaptive transition downtime estimate is out of range"
            );
            if transition.automatic {
                ensure!(
                    transition.condition.is_some(),
                    "automatic adaptive transition {} -> {} needs a condition",
                    transition.from,
                    transition.to
                );
                ensure!(
                    transition.allow_downtime,
                    "automatic adaptive transition {} -> {} must explicitly allow downtime",
                    transition.from,
                    transition.to
                );
            }
            if let Some(condition) = &transition.condition {
                ensure!(
                    condition.threshold.is_finite() && condition.threshold >= 0.0,
                    "adaptive transition threshold must be finite and non-negative"
                );
                ensure!(
                    condition.for_seconds >= self.poll_seconds
                        && condition.for_seconds <= 24 * 60 * 60,
                    "adaptive condition for_seconds is out of range"
                );
            }
        }
        Ok(())
    }

    fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    fn transition(&self, from: &str, to: &str) -> Option<&Transition> {
        self.transitions
            .iter()
            .find(|transition| transition.from == from && transition.to == to)
    }
}

fn validate_id(what: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "adaptive {what} contains unsafe characters"
    );
    Ok(())
}

fn validate_container(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') }),
        "adaptive container name contains unsafe characters"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Idle,
    Draining,
    Stopping,
    Starting,
    Stabilizing,
    RollingBack,
    Failed,
}

impl Phase {
    const fn busy(self) -> bool {
        matches!(
            self,
            Self::Draining
                | Self::Stopping
                | Self::Starting
                | Self::Stabilizing
                | Self::RollingBack
        )
    }
}

#[derive(Clone, Debug, Serialize)]
struct Runtime {
    mode: Mode,
    active_profile: String,
    phase: Phase,
    target_profile: Option<String>,
    phase_started_at: u64,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    enabled: bool,
    mode: Mode,
    active_profile: String,
    phase: Phase,
    target_profile: Option<String>,
    phase_started_at: u64,
    last_error: Option<String>,
    signal: Signal,
    recommendation: Option<String>,
    profiles: Vec<ProfileStatus>,
    transitions: Vec<TransitionStatus>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Signal {
    requests_per_second: f64,
    prompt_tokens_per_second: f64,
    completion_tokens_per_second: f64,
    tokens_per_second: f64,
    inflight: usize,
    load_per_engine: f64,
}

#[derive(Clone, Debug, Serialize)]
struct ProfileStatus {
    #[serde(flatten)]
    profile: Profile,
    active: bool,
}

#[derive(Clone, Debug, Serialize)]
struct TransitionStatus {
    from: String,
    to: String,
    automatic: bool,
    allow_downtime: bool,
    requires_downtime: bool,
    estimated_downtime_seconds: u64,
    condition: Option<ConditionStatus>,
}

#[derive(Clone, Debug, Serialize)]
struct ConditionStatus {
    metric: Metric,
    comparison: Comparison,
    threshold: f64,
    for_seconds: u64,
}

impl From<&Condition> for ConditionStatus {
    fn from(value: &Condition) -> Self {
        Self {
            metric: value.metric,
            comparison: value.comparison,
            threshold: value.threshold,
            for_seconds: value.for_seconds,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedState {
    version: u32,
    mode: Mode,
    active_profile: String,
}

struct AutoSample {
    at: Instant,
    requests: f64,
    prompt_tokens: f64,
    completion_tokens: f64,
    rps: f64,
    prompt_tps: f64,
    completion_tps: f64,
    candidate: Option<(String, Instant)>,
    recommendation: Option<String>,
}

impl Default for AutoSample {
    fn default() -> Self {
        Self {
            at: Instant::now(),
            requests: 0.0,
            prompt_tokens: 0.0,
            completion_tokens: 0.0,
            rps: 0.0,
            prompt_tps: 0.0,
            completion_tps: 0.0,
            candidate: None,
            recommendation: None,
        }
    }
}

#[derive(Clone)]
pub struct Adaptive {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    proxy: Proxy,
    docker: Docker,
    admin_token: String,
    requests: CounterVec,
    prompt_tokens: CounterVec,
    completion_tokens: CounterVec,
    runtime: Mutex<Runtime>,
    auto: Mutex<AutoSample>,
    operation: AsyncMutex<()>,
}

impl Adaptive {
    /// Loads and reconciles the optional controller. Absence of the environment
    /// variable is the entire default-off contract: no socket is opened and no
    /// route membership changes.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid authority, unavailable Docker, or a
    /// running-container shape that disagrees with the configured profile.
    pub async fn from_env(
        proxy: Proxy,
        requests: CounterVec,
        prompt_tokens: CounterVec,
        completion_tokens: CounterVec,
        admin_token: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(path) = env::var_os(CONFIG_ENV) else {
            return Ok(None);
        };
        let admin_token = admin_token
            .filter(|token| !token.is_empty())
            .context("adaptive control requires RJ_UPSTREAM_TOKEN")?;
        let config = Config::load(Path::new(&path), proxy.upstream_count())?;
        let docker = Docker::new(&config.docker_socket)?;
        docker
            .ping()
            .await
            .context("connect adaptive Docker socket")?;
        for profile in &config.profiles {
            for engine in &profile.engines {
                docker
                    .verify(engine, &profile.id)
                    .await
                    .with_context(|| format!("verify adaptive engine {}", engine.container))?;
            }
        }

        let persisted = load_state(&config.state_path)?;
        let active_profile = persisted.as_ref().map_or_else(
            || config.active_profile.clone(),
            |state| state.active_profile.clone(),
        );
        ensure!(
            config.profile(&active_profile).is_some(),
            "persisted adaptive profile no longer exists"
        );
        let mode = persisted.as_ref().map_or(config.mode, |state| state.mode);
        let profile = config
            .profile(&active_profile)
            .context("validated adaptive profile disappeared")?;
        proxy.set_topology_active(&profile_upstreams(profile))?;
        let initial_requests = counter_sum(&requests);
        let initial_prompt_tokens = counter_sum(&prompt_tokens);
        let initial_completion_tokens = counter_sum(&completion_tokens);

        let controller = Self {
            inner: Arc::new(Inner {
                config,
                proxy,
                docker,
                admin_token,
                requests,
                prompt_tokens,
                completion_tokens,
                runtime: Mutex::new(Runtime {
                    mode,
                    active_profile,
                    phase: Phase::Idle,
                    target_profile: None,
                    phase_started_at: unix_seconds(),
                    last_error: None,
                }),
                auto: Mutex::new(AutoSample {
                    requests: initial_requests,
                    prompt_tokens: initial_prompt_tokens,
                    completion_tokens: initial_completion_tokens,
                    ..AutoSample::default()
                }),
                operation: AsyncMutex::new(()),
            }),
        };
        controller.reconcile_running_profile().await?;
        controller.persist()?;
        Ok(Some(controller))
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/api/adaptive/status", get(Self::get_status))
            .route("/api/adaptive/mode", post(Self::set_mode))
            .route("/api/adaptive/transition", post(Self::transition))
            .with_state(self.clone())
    }

    pub async fn run(self) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(self.inner.config.poll_seconds));
        loop {
            interval.tick().await;
            self.auto_tick();
        }
    }

    fn status(&self) -> Status {
        let runtime = self.inner.runtime.lock().clone();
        let auto = self.inner.auto.lock();
        let active_engines = self
            .inner
            .config
            .profile(&runtime.active_profile)
            .map_or(1, |profile| profile.engines.len())
            .max(1);
        let signal = Signal {
            requests_per_second: auto.rps,
            prompt_tokens_per_second: auto.prompt_tps,
            completion_tokens_per_second: auto.completion_tps,
            tokens_per_second: auto.prompt_tps + auto.completion_tps,
            inflight: self.inner.proxy.total_inflight(),
            load_per_engine: usize_f64(self.inner.proxy.total_load_units())
                / usize_f64(active_engines),
        };
        Status {
            enabled: true,
            mode: runtime.mode,
            active_profile: runtime.active_profile.clone(),
            phase: runtime.phase,
            target_profile: runtime.target_profile,
            phase_started_at: runtime.phase_started_at,
            last_error: runtime.last_error,
            signal,
            recommendation: auto.recommendation.clone(),
            profiles: self
                .inner
                .config
                .profiles
                .iter()
                .cloned()
                .map(|profile| ProfileStatus {
                    active: profile.id == runtime.active_profile,
                    profile,
                })
                .collect(),
            transitions: self
                .inner
                .config
                .transitions
                .iter()
                .map(|transition| TransitionStatus {
                    from: transition.from.clone(),
                    to: transition.to.clone(),
                    automatic: transition.automatic,
                    allow_downtime: transition.allow_downtime,
                    requires_downtime: self.requires_downtime(transition),
                    estimated_downtime_seconds: transition.estimated_downtime_seconds,
                    condition: transition.condition.as_ref().map(ConditionStatus::from),
                })
                .collect(),
        }
    }

    #[allow(clippy::unused_async)] // Axum handlers return futures.
    async fn get_status(State(controller): State<Self>) -> Json<Status> {
        Json(controller.status())
    }

    #[allow(clippy::unused_async)] // Axum handlers return futures.
    async fn set_mode(
        State(controller): State<Self>,
        headers: HeaderMap,
        Json(request): Json<ModeRequest>,
    ) -> Response {
        if !controller.authorized(&headers) {
            return api_error(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        if controller.inner.runtime.lock().phase.busy() {
            return api_error(StatusCode::CONFLICT, "a topology transition is in progress");
        }
        controller.inner.runtime.lock().mode = request.mode;
        if let Err(error) = controller.persist() {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
        }
        (StatusCode::OK, Json(controller.status())).into_response()
    }

    #[allow(clippy::unused_async)] // Axum handlers return futures.
    async fn transition(
        State(controller): State<Self>,
        headers: HeaderMap,
        Json(request): Json<TransitionRequest>,
    ) -> Response {
        if !controller.authorized(&headers) {
            return api_error(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        match controller.begin_transition(request.profile, "manual") {
            Ok(()) => (StatusCode::ACCEPTED, Json(controller.status())).into_response(),
            Err(error) => api_error(StatusCode::CONFLICT, &error.to_string()),
        }
    }

    fn begin_transition(&self, target: String, source: &'static str) -> anyhow::Result<()> {
        validate_id("target profile", &target)?;
        {
            let mut runtime = self.inner.runtime.lock();
            ensure!(
                !runtime.phase.busy(),
                "a topology transition is already in progress"
            );
            ensure!(runtime.mode != Mode::Off, "adaptive control is off");
            ensure!(
                runtime.active_profile != target,
                "profile {target} is already active"
            );
            let transition = self
                .inner
                .config
                .transition(&runtime.active_profile, &target)
                .context("this topology transition is not configured")?;
            if self.requires_downtime(transition) {
                ensure!(
                    transition.allow_downtime,
                    "transition requires downtime but allow_downtime is false"
                );
            }
            runtime.phase = Phase::Draining;
            runtime.target_profile = Some(target.clone());
            runtime.phase_started_at = unix_seconds();
            runtime.last_error = None;
        }
        tracing::info!(target_profile = %target, source, "adaptive topology transition accepted");
        let controller = self.clone();
        tokio::spawn(async move {
            controller.execute_transition(target).await;
        });
        Ok(())
    }

    async fn execute_transition(&self, target: String) {
        let _operation = self.inner.operation.lock().await;
        let previous = self.inner.runtime.lock().active_profile.clone();
        let result = self.transition_once(&previous, &target).await;
        match result {
            Ok(()) => {
                let mut runtime = self.inner.runtime.lock();
                runtime.active_profile.clone_from(&target);
                runtime.phase = Phase::Idle;
                runtime.target_profile = None;
                runtime.phase_started_at = unix_seconds();
                runtime.last_error = None;
                drop(runtime);
                if let Err(error) = self.persist() {
                    self.fail(format!("persist completed transition: {error:#}"));
                }
                tracing::info!(active_profile = %target, "adaptive topology transition completed");
            }
            Err(error) => {
                tracing::error!(target_profile = %target, error = %error, "adaptive topology transition failed; rolling back");
                self.set_phase(Phase::RollingBack);
                let rollback = self.rollback(&target, &previous).await;
                let detail = match rollback {
                    Ok(()) => format!("transition failed and was rolled back: {error:#}"),
                    Err(rollback) => {
                        format!("transition failed: {error:#}; rollback failed: {rollback:#}")
                    }
                };
                self.fail(detail);
            }
        }
    }

    async fn transition_once(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let lock = acquire_deployment_lock(&self.inner.config.deployment_lock_path)?;
        let source = self
            .inner
            .config
            .profile(from)
            .context("source profile disappeared")?;
        let target = self
            .inner
            .config
            .profile(to)
            .context("target profile disappeared")?;
        self.inner.proxy.set_topology_active(&[])?;
        wait_until(
            Duration::from_secs(self.inner.config.drain_timeout_seconds),
            || self.inner.proxy.total_inflight() == 0,
        )
        .await
        .context("timed out draining active requests")?;

        self.set_phase(Phase::Stopping);
        for engine in &source.engines {
            self.inner.proxy.mark_upstream_unavailable(engine.upstream);
            self.inner.docker.stop(&engine.container).await?;
        }
        self.set_phase(Phase::Starting);
        for engine in &target.engines {
            self.inner.proxy.mark_upstream_unavailable(engine.upstream);
            self.inner.docker.start(&engine.container).await?;
        }
        self.set_phase(Phase::Stabilizing);
        self.wait_profile_ready(target).await?;
        self.inner
            .proxy
            .set_topology_active(&profile_upstreams(target))?;
        drop(lock);
        Ok(())
    }

    async fn rollback(&self, failed: &str, previous: &str) -> anyhow::Result<()> {
        let _lock = acquire_deployment_lock(&self.inner.config.deployment_lock_path)?;
        self.inner.proxy.set_topology_active(&[])?;
        let failed = self
            .inner
            .config
            .profile(failed)
            .context("failed profile disappeared")?;
        let previous = self
            .inner
            .config
            .profile(previous)
            .context("previous profile disappeared")?;
        for engine in &failed.engines {
            self.inner.proxy.mark_upstream_unavailable(engine.upstream);
            self.inner.docker.stop(&engine.container).await?;
        }
        for engine in &previous.engines {
            self.inner.proxy.mark_upstream_unavailable(engine.upstream);
            self.inner.docker.start(&engine.container).await?;
        }
        self.wait_profile_ready(previous).await?;
        self.inner
            .proxy
            .set_topology_active(&profile_upstreams(previous))?;
        Ok(())
    }

    async fn wait_profile_ready(&self, profile: &Profile) -> anyhow::Result<()> {
        let deadline =
            Instant::now() + Duration::from_secs(self.inner.config.start_timeout_seconds);
        let stable = Duration::from_secs(self.inner.config.stable_seconds);
        let mut ready_since = None;
        loop {
            let ready = profile
                .engines
                .iter()
                .all(|engine| self.inner.proxy.upstream_ready(engine.upstream));
            if ready {
                let since = ready_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= stable {
                    return Ok(());
                }
            } else {
                ready_since = None;
            }
            ensure!(
                Instant::now() < deadline,
                "profile {} did not become ready",
                profile.id
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn reconcile_running_profile(&self) -> anyhow::Result<()> {
        let active = self.inner.runtime.lock().active_profile.clone();
        for profile in &self.inner.config.profiles {
            for engine in &profile.engines {
                let running = self.inner.docker.running(&engine.container).await?;
                if profile.id == active {
                    ensure!(
                        running,
                        "adaptive active engine {} is not running",
                        engine.container
                    );
                } else {
                    ensure!(
                        !running,
                        "adaptive inactive engine {} is unexpectedly running",
                        engine.container
                    );
                }
            }
        }
        Ok(())
    }

    fn auto_tick(&self) {
        let requests = counter_sum(&self.inner.requests);
        let prompt_tokens = counter_sum(&self.inner.prompt_tokens);
        let completion_tokens = counter_sum(&self.inner.completion_tokens);
        let now = Instant::now();
        let (rps, prompt_tps, completion_tps, current, mode) = {
            let mut auto = self.inner.auto.lock();
            let elapsed = now.duration_since(auto.at).as_secs_f64();
            auto.rps = counter_rate(requests, auto.requests, elapsed);
            auto.prompt_tps = counter_rate(prompt_tokens, auto.prompt_tokens, elapsed);
            auto.completion_tps = counter_rate(completion_tokens, auto.completion_tokens, elapsed);
            auto.at = now;
            auto.requests = requests;
            auto.prompt_tokens = prompt_tokens;
            auto.completion_tokens = completion_tokens;
            let runtime = self.inner.runtime.lock();
            (
                auto.rps,
                auto.prompt_tps,
                auto.completion_tps,
                runtime.active_profile.clone(),
                runtime.mode,
            )
        };
        if !matches!(mode, Mode::Recommend | Mode::Auto) || self.inner.runtime.lock().phase.busy() {
            self.inner.auto.lock().candidate = None;
            return;
        }
        let active_engines = self
            .inner
            .config
            .profile(&current)
            .map_or(1, |p| p.engines.len())
            .max(1);
        let signal = Signal {
            requests_per_second: rps,
            prompt_tokens_per_second: prompt_tps,
            completion_tokens_per_second: completion_tps,
            tokens_per_second: prompt_tps + completion_tps,
            inflight: self.inner.proxy.total_inflight(),
            load_per_engine: usize_f64(self.inner.proxy.total_load_units())
                / usize_f64(active_engines),
        };
        let matched = self.inner.config.transitions.iter().find(|transition| {
            transition.from == current
                && transition.automatic
                && transition
                    .condition
                    .as_ref()
                    .is_some_and(|condition| condition.matches(&signal))
        });
        let Some(transition) = matched else {
            let mut auto = self.inner.auto.lock();
            auto.candidate = None;
            auto.recommendation = None;
            return;
        };
        let condition = transition
            .condition
            .as_ref()
            .expect("automatic condition validated");
        let ready = {
            let mut auto = self.inner.auto.lock();
            let since = match &auto.candidate {
                Some((target, since)) if target == &transition.to => *since,
                _ => {
                    auto.candidate = Some((transition.to.clone(), now));
                    now
                }
            };
            auto.recommendation = Some(transition.to.clone());
            now.duration_since(since) >= Duration::from_secs(condition.for_seconds)
        };
        if ready && mode == Mode::Auto {
            let target = transition.to.clone();
            if let Err(error) = self.begin_transition(target, "auto") {
                tracing::warn!(error = %error, "automatic topology transition was not accepted");
            }
        }
    }

    fn requires_downtime(&self, transition: &Transition) -> bool {
        let Some(from) = self.inner.config.profile(&transition.from) else {
            return true;
        };
        let Some(to) = self.inner.config.profile(&transition.to) else {
            return true;
        };
        let from_names = from
            .engines
            .iter()
            .map(|engine| &engine.container)
            .collect::<HashSet<_>>();
        let to_names = to
            .engines
            .iter()
            .map(|engine| &engine.container)
            .collect::<HashSet<_>>();
        from_names != to_names
    }

    fn set_phase(&self, phase: Phase) {
        let mut runtime = self.inner.runtime.lock();
        runtime.phase = phase;
        runtime.phase_started_at = unix_seconds();
    }

    fn fail(&self, error: String) {
        let mut runtime = self.inner.runtime.lock();
        runtime.phase = Phase::Failed;
        runtime.target_profile = None;
        runtime.phase_started_at = unix_seconds();
        runtime.last_error = Some(error);
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        let Some(token) = value.strip_prefix("Bearer ") else {
            return false;
        };
        digest(token.as_bytes()) == digest(self.inner.admin_token.as_bytes())
    }

    fn persist(&self) -> anyhow::Result<()> {
        let runtime = self.inner.runtime.lock();
        let state = PersistedState {
            version: 1,
            mode: runtime.mode,
            active_profile: runtime.active_profile.clone(),
        };
        drop(runtime);
        write_state(&self.inner.config.state_path, &state)
    }
}

impl Condition {
    fn matches(&self, signal: &Signal) -> bool {
        let value = match self.metric {
            Metric::RequestsPerSecond => signal.requests_per_second,
            Metric::PromptTokensPerSecond => signal.prompt_tokens_per_second,
            Metric::CompletionTokensPerSecond => signal.completion_tokens_per_second,
            Metric::TokensPerSecond => signal.tokens_per_second,
            Metric::Inflight => usize_f64(signal.inflight),
            Metric::LoadPerEngine => signal.load_per_engine,
        };
        match self.comparison {
            Comparison::Above => value > self.threshold,
            Comparison::Below => value < self.threshold,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModeRequest {
    mode: Mode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionRequest {
    profile: String,
}

fn api_error(status: StatusCode, detail: &str) -> Response {
    (status, Json(serde_json::json!({ "error": detail }))).into_response()
}

fn profile_upstreams(profile: &Profile) -> Vec<usize> {
    profile
        .engines
        .iter()
        .map(|engine| engine.upstream)
        .collect()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn digest(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn usize_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn counter_sum(counter: &CounterVec) -> f64 {
    counter
        .collect()
        .into_iter()
        .map(|family| {
            family
                .get_metric()
                .iter()
                .map(|metric| metric.get_counter().get_value())
                .sum::<f64>()
        })
        .sum()
}

fn counter_rate(current: f64, previous: f64, elapsed_seconds: f64) -> f64 {
    if elapsed_seconds.is_finite() && elapsed_seconds > 0.0 {
        (current - previous).max(0.0) / elapsed_seconds
    } else {
        0.0
    }
}

async fn wait_until(mut timeout: Duration, predicate: impl Fn() -> bool) -> anyhow::Result<()> {
    while !predicate() {
        ensure!(!timeout.is_zero(), "deadline elapsed");
        let step = timeout.min(Duration::from_millis(100));
        tokio::time::sleep(step).await;
        timeout = timeout.saturating_sub(step);
    }
    Ok(())
}

fn load_state(path: &Path) -> anyhow::Result<Option<PersistedState>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "adaptive state must be a regular file"
            );
            ensure!(
                metadata.permissions().mode().trailing_zeros() >= 6,
                "adaptive state must be owner-only"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect adaptive state {}", path.display()));
        }
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Err(error).with_context(|| format!("read adaptive state {}", path.display()));
        }
    };
    let state: PersistedState = serde_json::from_slice(&bytes).context("parse adaptive state")?;
    ensure!(state.version == 1, "adaptive state version must be 1");
    Ok(Some(state))
}

fn write_state(path: &Path, state: &PersistedState) -> anyhow::Result<()> {
    let parent = path.parent().context("adaptive state path has no parent")?;
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspect adaptive state parent {}", parent.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "adaptive state parent must be a directory"
    );
    ensure!(
        metadata.permissions().mode() & 0o022 == 0,
        "adaptive state parent must not be group/world writable"
    );
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).context("generate adaptive state nonce")?;
    let temporary = parent.join(format!(
        ".ramjet-adaptive-{}-{}.tmp",
        std::process::id(),
        u64::from_ne_bytes(nonce)
    ));
    let bytes = serde_json::to_vec(state).context("serialize adaptive state")?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let file = options
        .open(&temporary)
        .with_context(|| format!("open adaptive state temporary {}", temporary.display()))?;
    let result = (|| -> anyhow::Result<()> {
        io::Write::write_all(&mut &file, &bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path).context("publish adaptive state")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn acquire_deployment_lock(path: &Path) -> anyhow::Result<File> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect deployment lock {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "deployment lock must be a regular file"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open deployment lock {}", path.display()))?;
    file.try_lock()
        .with_context(|| format!("deployment lock {} is busy", path.display()))?;
    Ok(file)
}

#[derive(Clone)]
struct Docker {
    client: Client,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspect {
    image: String,
    config: InspectConfig,
    host_config: HostConfig,
    state: ContainerState,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectConfig {
    image: String,
    labels: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HostConfig {
    #[serde(default)]
    device_requests: Vec<DeviceRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DeviceRequest {
    #[serde(default)]
    driver: String,
    #[serde(default, rename = "DeviceIDs")]
    device_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerState {
    running: bool,
}

impl Docker {
    fn new(socket: &Path) -> anyhow::Result<Self> {
        let client = Client::builder()
            .unix_socket(socket)
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(125))
            .build()
            .context("build adaptive Docker client")?;
        Ok(Self { client })
    }

    async fn ping(&self) -> anyhow::Result<()> {
        let response = self.client.get("http://localhost/_ping").send().await?;
        ensure!(
            response.status().is_success(),
            "Docker ping returned {}",
            response.status()
        );
        Ok(())
    }

    async fn inspect(&self, container: &str) -> anyhow::Result<Inspect> {
        validate_container(container)?;
        let response = self
            .client
            .get(format!("http://localhost/containers/{container}/json"))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "Docker inspect {container} returned {}",
            response.status()
        );
        let body = response
            .bytes()
            .await
            .context("read Docker inspect response")?;
        serde_json::from_slice(&body).context("decode Docker inspect response")
    }

    async fn verify(&self, engine: &Engine, profile: &str) -> anyhow::Result<()> {
        let inspect = self.inspect(&engine.container).await?;
        ensure!(
            inspect.image == engine.image,
            "container image ID does not match adaptive config"
        );
        ensure!(
            inspect.config.image.starts_with("sha256:") || inspect.config.image.contains('@'),
            "container was not created from an immutable image reference"
        );
        ensure!(
            inspect
                .config
                .labels
                .get("com.helixml.ramjet.adaptive-profile")
                .map(String::as_str)
                == Some(profile),
            "container adaptive-profile label does not match"
        );
        let expected_upstream = engine.upstream.to_string();
        ensure!(
            inspect
                .config
                .labels
                .get("com.helixml.ramjet.adaptive-upstream")
                .map(String::as_str)
                == Some(expected_upstream.as_str()),
            "container adaptive-upstream label does not match"
        );
        let expected = engine
            .gpus
            .iter()
            .map(u32::to_string)
            .collect::<HashSet<_>>();
        let actual = inspect
            .host_config
            .device_requests
            .iter()
            .filter(|request| request.driver.is_empty() || request.driver == "nvidia")
            .flat_map(|request| request.device_ids.iter().cloned())
            .collect::<HashSet<_>>();
        ensure!(
            actual == expected,
            "container GPU assignment does not match adaptive config"
        );
        Ok(())
    }

    async fn running(&self, container: &str) -> anyhow::Result<bool> {
        Ok(self.inspect(container).await?.state.running)
    }

    async fn start(&self, container: &str) -> anyhow::Result<()> {
        self.action(container, "start", None).await
    }

    async fn stop(&self, container: &str) -> anyhow::Result<()> {
        self.action(container, "stop", Some("t=120")).await
    }

    async fn action(
        &self,
        container: &str,
        action: &str,
        query: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_container(container)?;
        let mut url = format!("http://localhost/containers/{container}/{action}");
        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }
        let response = self.client.post(url).send().await?;
        ensure!(
            response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED,
            "Docker {action} {container} returned {}",
            response.status()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

    struct TestSocket(PathBuf);

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "mode": "manual",
            "active_profile": "split",
            "state_path": "/var/lib/ramjet-adaptive/state.json",
            "docker_socket": "/var/run/docker.sock",
            "deployment_lock_path": "/run/lock/ramjet-node06-deployment.lock",
            "poll_seconds": 5,
            "drain_timeout_seconds": 120,
            "start_timeout_seconds": 900,
            "stable_seconds": 15,
            "profiles": [
                {"id":"split","label":"Twin TP4","description":"throughput", "engines":[
                    {"upstream":0,"label":"A","container":"engine-a","image":format!("sha256:{}", "a".repeat(64)),"gpus":[0,1,2,3]},
                    {"upstream":1,"label":"B","container":"engine-b","image":format!("sha256:{}", "b".repeat(64)),"gpus":[4,5,6,7]}
                ]},
                {"id":"unified","label":"Unified TP8","description":"latency", "engines":[
                    {"upstream":2,"label":"Aero","container":"engine-tp8","image":format!("sha256:{}", "c".repeat(64)),"gpus":[0,1,2,3,4,5,6,7]}
                ]}
            ],
            "transitions": [
                {"from":"split","to":"unified","automatic":true,"allow_downtime":true,"estimated_downtime_seconds":540,
                 "condition":{"metric":"completion_tokens_per_second","comparison":"above","threshold":400,"for_seconds":30}},
                {"from":"unified","to":"split","automatic":true,"allow_downtime":true,"estimated_downtime_seconds":540,
                 "condition":{"metric":"load_per_engine","comparison":"above","threshold":4,"for_seconds":300}}
            ]
        }))
        .expect("config")
    }

    #[test]
    fn validates_named_profiles_and_explicit_downtime() {
        config().validate(3).expect("valid");
        let mut invalid = config();
        invalid.transitions[0].allow_downtime = false;
        assert!(
            invalid
                .validate(3)
                .unwrap_err()
                .to_string()
                .contains("allow downtime")
        );
    }

    #[test]
    fn rejects_duplicate_gpu_and_unsafe_container() {
        let mut duplicate = config();
        duplicate.profiles[0].engines[1].gpus[0] = 3;
        assert!(
            duplicate
                .validate(3)
                .unwrap_err()
                .to_string()
                .contains("GPU 3")
        );
        let mut unsafe_name = config();
        unsafe_name.profiles[0].engines[0].container = "../../docker".to_owned();
        assert!(unsafe_name.validate(3).is_err());
    }

    #[test]
    fn automatic_conditions_are_directional() {
        let candidate = config();
        let condition = candidate.transitions[0]
            .condition
            .as_ref()
            .expect("condition");
        assert!(condition.matches(&Signal {
            completion_tokens_per_second: 401.0,
            inflight: 0,
            load_per_engine: 0.0,
            ..Signal::default()
        }));
        assert!(!condition.matches(&Signal {
            completion_tokens_per_second: 399.0,
            inflight: 99,
            load_per_engine: 99.0,
            ..Signal::default()
        }));
    }

    #[test]
    fn token_counter_rates_are_reset_safe() {
        assert!((counter_rate(1_250.0, 1_000.0, 5.0) - 50.0).abs() < f64::EPSILON);
        assert!(counter_rate(10.0, 1_000.0, 5.0).abs() < f64::EPSILON);
        assert!(counter_rate(10.0, 1.0, 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn docker_authority_is_inspect_start_stop_only() {
        let path = PathBuf::from(format!(
            "/tmp/rj-adaptive-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = TestSocket(path.clone());
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = Arc::clone(&seen);
        let image = format!("sha256:{}", "a".repeat(64));
        let inspect = serde_json::json!({
            "Image": image,
            "Config": {
                "Image": format!("example.invalid/engine@sha256:{}", "b".repeat(64)),
                "Labels": {
                    "com.helixml.ramjet.adaptive-profile": "split",
                    "com.helixml.ramjet.adaptive-upstream": "0"
                }
            },
            "HostConfig": {"DeviceRequests":[{"Driver":"nvidia","DeviceIDs":["0","1","2","3"]}]},
            "State": {"Running": true}
        })
        .to_string();
        let task = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 16 << 10];
                let read = stream.read(&mut request).await.unwrap();
                let head = String::from_utf8_lossy(&request[..read]);
                let first = head.lines().next().unwrap_or_default().to_owned();
                server_seen.lock().push(first.clone());
                let body = if first.contains("/containers/engine-a/json") {
                    inspect.as_str()
                } else {
                    "OK"
                };
                let status = if first.contains("/start") || first.contains("/stop") {
                    "204 No Content"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    if status.starts_with("204") {
                        0
                    } else {
                        body.len()
                    }
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let docker = Docker::new(&socket.0).unwrap();
        let engine = &config().profiles[0].engines[0];
        docker.ping().await.unwrap();
        docker.verify(engine, "split").await.unwrap();
        assert!(docker.running("engine-a").await.unwrap());
        docker.start("engine-a").await.unwrap();
        docker.stop("engine-a").await.unwrap();
        task.await.unwrap();
        let seen = seen.lock();
        assert_eq!(seen.len(), 5);
        assert!(seen.iter().any(|request| request == "GET /_ping HTTP/1.1"));
        assert!(
            seen.iter()
                .any(|request| request.starts_with("POST /containers/engine-a/start "))
        );
        assert!(
            seen.iter()
                .any(|request| request.starts_with("POST /containers/engine-a/stop?t=120 "))
        );
        assert!(seen.iter().all(|request| !request.contains("/create")
            && !request.contains("/delete")
            && !request.contains("/exec")));
    }
}
