//! Client authentication and per-principal accounting at the ingress.
//!
//! `RJ_API_TOKENS_MODE=off` (the default) authenticates nothing: requests are
//! forwarded exactly as before and only attributed to `master` or
//! `anonymous`. `on` makes ramjet the authority: a request must carry the
//! master token or a valid minted token, and a minted token is exchanged for
//! the master bearer before dispatch so engines never see it.
//!
//! See `docs/api-tokens.md` for the HTTP contract.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use prometheus::{CounterVec, GaugeVec, Opts, Registry};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::broadcast;

use crate::{
    api_tokens::{MutationError, NewToken, TOKEN_PREFIX, TokenPatch, TokenStore, TokenView},
    ui_auth::constant_time_equal,
    usage::Accumulator,
    usage_ledger::{
        Completion, LedgerSettings, RecentQuery, RequestRecord, SeriesQuery, UsageLedger, unix_ms,
    },
};

pub const PRINCIPAL_MASTER: &str = "master";
pub const PRINCIPAL_ANONYMOUS: &str = "anonymous";
pub const PRINCIPAL_REJECTED: &str = "rejected";
/// Usage deltas reach `SQLite` this often; a crash loses at most one interval.
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// Token last-use times are coarse; rewrite that file once a minute at most.
const TOKEN_FLUSH_EVERY: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenMode {
    Off,
    On,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub mode: TokenMode,
    pub state_dir: Option<PathBuf>,
    pub ledger: LedgerSettings,
}

impl Settings {
    /// # Errors
    ///
    /// Invalid or out-of-range values.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// # Errors
    ///
    /// Invalid or out-of-range values.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mode = match get("RJ_API_TOKENS_MODE").as_deref().map(str::trim) {
            None | Some("" | "off") => TokenMode::Off,
            Some("on") => TokenMode::On,
            Some(other) => anyhow::bail!("RJ_API_TOKENS_MODE must be off or on, got {other:?}"),
        };
        let state_dir = get("RJ_API_STATE_DIR")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        let defaults = LedgerSettings::default();
        let bounded = |key: &str, default: u64, min: u64, max: u64| -> anyhow::Result<u64> {
            match get(key).filter(|value| !value.trim().is_empty()) {
                None => Ok(default),
                Some(raw) => {
                    let value = raw
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("{key} must be an integer, got {raw:?}"))?;
                    anyhow::ensure!(
                        (min..=max).contains(&value),
                        "{key} must be within {min}-{max}, got {value}"
                    );
                    Ok(value)
                }
            }
        };
        let ledger = LedgerSettings {
            hour_retention_days: bounded(
                "RJ_USAGE_RETENTION_DAYS",
                defaults.hour_retention_days,
                1,
                400,
            )?,
            minute_retention_hours: bounded(
                "RJ_USAGE_MINUTE_RETENTION_HOURS",
                defaults.minute_retention_hours,
                1,
                168,
            )?,
            recent_capacity: usize::try_from(bounded(
                "RJ_USAGE_RECENT_REQUESTS",
                defaults.recent_capacity as u64,
                0,
                5_000_000,
            )?)?,
        };
        if mode == TokenMode::On {
            anyhow::ensure!(
                state_dir.is_some(),
                "RJ_API_TOKENS_MODE=on requires RJ_API_STATE_DIR so minted tokens survive restarts"
            );
        }
        Ok(Self {
            mode,
            state_dir,
            ledger,
        })
    }
}

/// The ingress decision for one request.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Forward the request. `exchange` means the presented credential was a
    /// minted token that must be replaced by the master bearer.
    Allow {
        principal: Arc<str>,
        exchange: bool,
    },
    Reject {
        reason: &'static str,
    },
}

/// Per-request accounting context carried from ingress to completion.
#[derive(Clone, Debug)]
pub struct RequestAccount {
    pub principal: Arc<str>,
    pub model: Option<String>,
    pub endpoint: &'static str,
    pub request_bytes: usize,
    pub route_outcome: Option<&'static str>,
}

impl RequestAccount {
    #[must_use]
    pub fn new(principal: Arc<str>, endpoint: &'static str) -> Self {
        Self {
            principal,
            model: None,
            endpoint,
            request_bytes: 0,
            route_outcome: None,
        }
    }
}

/// What happened after dispatch, for [`Accounting::finish`].
pub struct Outcome<'a> {
    pub status: StatusCode,
    pub result: &'a str,
    pub upstream: Option<usize>,
    pub stream: bool,
    pub usage: Option<&'a Accumulator>,
    pub first_token: Option<Duration>,
    pub elapsed: Duration,
    pub response_bytes: usize,
}

impl Outcome<'_> {
    /// A request ramjet answered itself without reaching an engine.
    #[must_use]
    pub fn local(status: StatusCode, result: &'static str, elapsed: Duration) -> Self {
        Outcome {
            status,
            result,
            upstream: None,
            stream: false,
            usage: None,
            first_token: None,
            elapsed,
            response_bytes: 0,
        }
    }
}

pub struct Accounting {
    mode: TokenMode,
    master: Option<Arc<str>>,
    master_bearer: Option<HeaderValue>,
    tokens: Option<Arc<TokenStore>>,
    ledger: Arc<UsageLedger>,
    upstreams: Vec<String>,
    auth_failures: Option<CounterVec>,
    token_gauge: Option<GaugeVec>,
}

impl Accounting {
    /// Attribution-only accounting with an in-memory ledger and no metrics.
    /// This is what a proxy built without explicit accounting uses.
    ///
    /// # Panics
    ///
    /// Never: a ledger without a registry has nothing that can fail.
    #[must_use]
    pub fn passive(master: Option<&str>, upstreams: Vec<String>) -> Self {
        Self {
            mode: TokenMode::Off,
            master: master.map(Arc::from),
            master_bearer: None,
            tokens: None,
            ledger: Arc::new(
                UsageLedger::new(LedgerSettings::default(), None, None)
                    .expect("an unregistered ledger cannot fail"),
            ),
            upstreams,
            auth_failures: None,
            token_gauge: None,
        }
    }

    /// Builds the configured accounting layer and restores persisted state.
    ///
    /// # Errors
    ///
    /// Missing master token in `on` mode, unreadable token state, or metric
    /// registration failure.
    pub fn new(
        settings: &Settings,
        master: Option<&str>,
        upstreams: Vec<String>,
        registry: Option<&Registry>,
    ) -> anyhow::Result<Self> {
        let master_bearer = match (settings.mode, master) {
            (TokenMode::On, None) => anyhow::bail!(
                "RJ_API_TOKENS_MODE=on requires RJ_UPSTREAM_TOKEN: it is the master token \
                 that minted tokens are exchanged for"
            ),
            (TokenMode::On, Some(master)) => Some(
                HeaderValue::from_str(&format!("Bearer {master}"))
                    .map_err(|_| anyhow::anyhow!("RJ_UPSTREAM_TOKEN is not a valid header"))?,
            ),
            (TokenMode::Off, _) => None,
        };
        let tokens = match settings.mode {
            TokenMode::On => Some(Arc::new(TokenStore::open(
                settings
                    .state_dir
                    .as_deref()
                    .context("RJ_API_TOKENS_MODE=on requires RJ_API_STATE_DIR")?,
            )?)),
            TokenMode::Off => None,
        };
        // Usage history is an observation, not an authority: an unopenable
        // store degrades to memory instead of keeping the router down.
        let ledger = match UsageLedger::new(
            settings.ledger.clone(),
            registry,
            settings.state_dir.as_deref(),
        ) {
            Ok(ledger) => ledger,
            Err(error) if settings.state_dir.is_some() => {
                tracing::error!(
                    error = %format!("{error:#}"),
                    "usage store unavailable; accounting falls back to memory"
                );
                UsageLedger::new(settings.ledger.clone(), registry, None)?
            }
            Err(error) => return Err(error),
        };
        let ledger = Arc::new(ledger);
        let (auth_failures, token_gauge) = match registry {
            Some(registry) => {
                let failures = CounterVec::new(
                    Opts::new(
                        "ramjet_api_token_auth_failures_total",
                        "Requests refused at ingress by closed reason",
                    ),
                    &["reason"],
                )?;
                let gauge = GaugeVec::new(
                    Opts::new("ramjet_api_tokens", "Minted API tokens by status"),
                    &["status"],
                )?;
                registry.register(Box::new(failures.clone()))?;
                registry.register(Box::new(gauge.clone()))?;
                (Some(failures), Some(gauge))
            }
            None => (None, None),
        };
        let accounting = Self {
            mode: settings.mode,
            master: master.map(Arc::from),
            master_bearer,
            tokens,
            ledger,
            upstreams,
            auth_failures,
            token_gauge,
        };
        accounting.publish_token_gauge();
        Ok(accounting)
    }

    #[must_use]
    pub fn tokens_enabled(&self) -> bool {
        self.tokens.is_some()
    }

    #[must_use]
    pub fn ledger(&self) -> &Arc<UsageLedger> {
        &self.ledger
    }

    /// Decides whether a request may proceed and whom it is attributed to.
    #[must_use]
    pub fn authenticate(&self, headers: &HeaderMap) -> Admission {
        self.authenticate_at(headers, unix_ms())
    }

    fn authenticate_at(&self, headers: &HeaderMap, now_ms: u64) -> Admission {
        let presented = presented_credential(headers);
        let is_master = |credential: &str| {
            self.master
                .as_deref()
                .is_some_and(|master| constant_time_equal(master.as_bytes(), credential.as_bytes()))
        };
        let admission = match (self.mode, presented) {
            (TokenMode::Off, Ok(Some(credential))) if is_master(credential) => Admission::Allow {
                principal: Arc::from(PRINCIPAL_MASTER),
                exchange: false,
            },
            (TokenMode::Off, _) => Admission::Allow {
                principal: Arc::from(PRINCIPAL_ANONYMOUS),
                exchange: false,
            },
            (TokenMode::On, Err(reason)) => Admission::Reject { reason },
            (TokenMode::On, Ok(None)) => Admission::Reject { reason: "missing" },
            (TokenMode::On, Ok(Some(credential))) if is_master(credential) => Admission::Allow {
                principal: Arc::from(PRINCIPAL_MASTER),
                exchange: false,
            },
            (TokenMode::On, Ok(Some(credential))) if credential.starts_with(TOKEN_PREFIX) => {
                match self
                    .tokens
                    .as_ref()
                    .expect("on mode always holds a token store")
                    .verify(credential, now_ms)
                {
                    Ok(id) => Admission::Allow {
                        principal: id,
                        exchange: true,
                    },
                    Err(error) => Admission::Reject {
                        reason: error.label(),
                    },
                }
            }
            (TokenMode::On, Ok(Some(_))) => Admission::Reject { reason: "invalid" },
        };
        if let (Admission::Reject { reason }, Some(failures)) = (&admission, &self.auth_failures) {
            failures.with_label_values(&[reason]).inc();
        }
        admission
    }

    /// Replaces a minted credential with the master bearer. Removes every
    /// presented credential header so the minted secret is never forwarded.
    pub fn exchange_credentials(&self, headers: &mut HeaderMap) {
        headers.remove(header::AUTHORIZATION);
        headers.remove("x-api-key");
        if let Some(bearer) = &self.master_bearer {
            headers.insert(header::AUTHORIZATION, bearer.clone());
        }
    }

    /// The OpenAI-shaped `401` returned for a refused request.
    #[must_use]
    pub fn rejection(reason: &'static str) -> Response {
        let message = match reason {
            "missing" => "missing API key",
            "expired" => "API key has expired",
            "revoked" => "API key has been revoked",
            _ => "invalid API key",
        };
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": {
                    "message": message,
                    "type": "authentication_error",
                    "code": "invalid_api_key",
                }
            })),
        )
            .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"ramjet\""),
        );
        response
    }

    /// Records a finished request.
    pub fn finish(&self, account: &RequestAccount, outcome: &Outcome<'_>) {
        self.ledger.record(Completion {
            principal: &account.principal,
            model: account.model.as_deref(),
            endpoint: account.endpoint,
            upstream: outcome.upstream,
            status: outcome.status.as_u16(),
            stream: outcome.stream,
            result: outcome.result,
            usage: outcome.usage,
            first_token: outcome.first_token,
            elapsed: outcome.elapsed,
            route_outcome: account.route_outcome,
            request_bytes: account.request_bytes,
            response_bytes: outcome.response_bytes,
        });
    }

    fn publish_token_gauge(&self) {
        if let (Some(gauge), Some(tokens)) = (&self.token_gauge, &self.tokens) {
            for (status, count) in tokens.status_counts(unix_ms()) {
                #[allow(clippy::cast_precision_loss)]
                gauge.with_label_values(&[status.label()]).set(count as f64);
            }
        }
    }

    async fn flush(&self, tokens_too: bool) {
        self.ledger.flush_async().await;
        if !tokens_too {
            return;
        }
        if let Some(tokens) = self.tokens.clone() {
            match tokio::task::spawn_blocking(move || tokens.flush_last_used()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "API token last-use flush failed"),
                Err(error) => tracing::warn!(%error, "API token flush task failed"),
            }
        }
        self.publish_token_gauge();
    }

    /// Periodically persists the ledger and token last-use times, with a final
    /// flush on shutdown.
    pub async fn run(self: Arc<Self>, mut shutdown: broadcast::Receiver<()>) {
        let mut interval = tokio::time::interval(FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        let mut ticks = 0_u32;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    ticks = ticks.wrapping_add(1);
                    self.flush(ticks.is_multiple_of(TOKEN_FLUSH_EVERY)).await;
                }
                _ = shutdown.recv() => {
                    self.flush(true).await;
                    return;
                }
            }
        }
    }

    /// `/api/tokens*`. Mount behind the dashboard session gate.
    pub fn tokens_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/tokens", get(list_tokens).post(create_token))
            .route(
                "/api/tokens/{id}",
                get(get_token).patch(update_token).delete(delete_token),
            )
            .route("/api/tokens/{id}/revoke", post(revoke_token))
            .with_state(Arc::clone(self))
    }

    /// `/api/usage/*`.
    pub fn usage_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/usage/meta", get(usage_meta))
            .route("/api/usage/series", get(usage_series))
            .route("/api/usage/requests", get(usage_requests))
            .with_state(Arc::clone(self))
    }
}

/// Reads the single presented credential from `Authorization: Bearer` or
/// `x-api-key`. Duplicate headers or two different credentials are refused
/// rather than guessed between.
fn presented_credential(headers: &HeaderMap) -> Result<Option<&str>, &'static str> {
    let single = |name: header::HeaderName| -> Result<Option<&str>, &'static str> {
        let mut values = headers.get_all(name).iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err("ambiguous");
        }
        value.to_str().map(Some).map_err(|_| "malformed")
    };
    let bearer = single(header::AUTHORIZATION)?
        .map(|value| {
            let (scheme, credential) = value.split_once(' ').ok_or("malformed")?;
            if scheme.eq_ignore_ascii_case("bearer") {
                Ok(credential.trim())
            } else {
                Err("malformed")
            }
        })
        .transpose()?;
    let api_key = single(header::HeaderName::from_static("x-api-key"))?.map(str::trim);
    match (bearer, api_key) {
        (Some(bearer), Some(api_key)) if bearer != api_key => Err("ambiguous"),
        (Some(credential), _) | (None, Some(credential)) => {
            Ok(Some(credential).filter(|credential| !credential.is_empty()))
        }
        (None, None) => Ok(None),
    }
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let mut response =
        (status, Json(serde_json::json!({ "error": message.into() }))).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json_no_store<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn mutation_error(error: &MutationError) -> Response {
    let status = match error {
        MutationError::Invalid(_) => StatusCode::BAD_REQUEST,
        MutationError::NotFound => StatusCode::NOT_FOUND,
        MutationError::Full => StatusCode::CONFLICT,
        MutationError::Persist(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if let MutationError::Persist(detail) = error {
        tracing::error!(%detail, "API token state persistence failed");
    }
    error_response(status, error.to_string())
}

fn disabled() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "token management is disabled (RJ_API_TOKENS_MODE=off)",
    )
}

/// Runs a blocking store mutation (it fsyncs) off the async workers.
async fn blocking<T: Send + 'static>(
    accounting: &Arc<Accounting>,
    operation: impl FnOnce(&TokenStore) -> Result<T, MutationError> + Send + 'static,
) -> Result<T, Response> {
    if !accounting.tokens_enabled() {
        return Err(disabled());
    }
    let accounting = Arc::clone(accounting);
    let result = tokio::task::spawn_blocking(move || {
        let store = accounting
            .tokens
            .as_deref()
            .expect("store presence checked above");
        let result = operation(store);
        accounting.publish_token_gauge();
        result
    })
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "token task failed"))?;
    result.map_err(|error| mutation_error(&error))
}

#[derive(Serialize)]
struct TokenList {
    enabled: bool,
    tokens: Vec<TokenView>,
}

#[allow(clippy::unused_async)] // Axum handlers return futures.
async fn list_tokens(State(accounting): State<Arc<Accounting>>) -> Response {
    let tokens = accounting
        .tokens
        .as_ref()
        .map(|store| store.list(unix_ms()))
        .unwrap_or_default();
    json_no_store(
        StatusCode::OK,
        &TokenList {
            enabled: accounting.tokens_enabled(),
            tokens,
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    expires_at_ms: Option<u64>,
}

#[derive(Serialize)]
struct Created {
    token: TokenView,
    secret: String,
}

#[derive(Serialize)]
struct Single {
    token: TokenView,
}

async fn create_token(
    State(accounting): State<Arc<Accounting>>,
    Json(request): Json<CreateRequest>,
) -> Response {
    let result = blocking(&accounting, move |store| {
        store.create(
            &NewToken {
                name: request.name,
                description: request.description,
                expires_at_ms: request.expires_at_ms,
            },
            unix_ms(),
        )
    })
    .await;
    match result {
        Ok((token, secret)) => {
            tracing::info!(token = %token.id, name = %token.name, "API token minted");
            json_no_store(StatusCode::CREATED, &Created { token, secret })
        }
        Err(response) => response,
    }
}

#[allow(clippy::option_option)] // Absent, null, and a value are three states.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<u64>>, D::Error> {
    Option::<u64>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "present")]
    #[allow(clippy::option_option)]
    expires_at_ms: Option<Option<u64>>,
}

async fn update_token(
    State(accounting): State<Arc<Accounting>>,
    Path(id): Path<String>,
    Json(request): Json<PatchRequest>,
) -> Response {
    let result = blocking(&accounting, move |store| {
        store.update(
            &id,
            &TokenPatch {
                name: request.name,
                description: request.description,
                expires_at_ms: request.expires_at_ms,
            },
            unix_ms(),
        )
    })
    .await;
    match result {
        Ok(token) => json_no_store(StatusCode::OK, &Single { token }),
        Err(response) => response,
    }
}

async fn revoke_token(
    State(accounting): State<Arc<Accounting>>,
    Path(id): Path<String>,
) -> Response {
    match blocking(&accounting, move |store| store.revoke(&id, unix_ms())).await {
        Ok(token) => {
            tracing::info!(token = %token.id, "API token revoked");
            json_no_store(StatusCode::OK, &Single { token })
        }
        Err(response) => response,
    }
}

async fn delete_token(
    State(accounting): State<Arc<Accounting>>,
    Path(id): Path<String>,
) -> Response {
    let logged = id.clone();
    match blocking(&accounting, move |store| store.delete(&id)).await {
        Ok(()) => {
            tracing::info!(token = %logged, "API token deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(response) => response,
    }
}

#[derive(Serialize)]
struct Principal {
    key: String,
    name: String,
    kind: &'static str,
    /// `active`, `expired`, or `revoked` for existing tokens; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
}

#[derive(Serialize)]
struct UsageMeta {
    principals: Vec<Principal>,
    models: Vec<String>,
    upstreams: Vec<String>,
    minute_retention_hours: u64,
    hour_retention_days: u64,
    recent_capacity: usize,
    tokens_enabled: bool,
}

#[allow(clippy::unused_async)] // Axum handlers return futures.
async fn get_token(State(accounting): State<Arc<Accounting>>, Path(id): Path<String>) -> Response {
    let Some(store) = accounting.tokens.as_ref() else {
        return disabled();
    };
    match store.get(&id, unix_ms()) {
        Some(token) => json_no_store(StatusCode::OK, &Single { token }),
        None => mutation_error(&MutationError::NotFound),
    }
}

/// Runs a ledger read off the async workers: it may scan `SQLite`.
async fn read_ledger<T: Serialize + Send + 'static>(
    accounting: &Arc<Accounting>,
    read: impl FnOnce(&Accounting) -> Result<T, crate::usage_ledger::QueryError> + Send + 'static,
    invalid: StatusCode,
) -> Response {
    let accounting = Arc::clone(accounting);
    match tokio::task::spawn_blocking(move || read(&accounting)).await {
        Ok(Ok(value)) => json_no_store(StatusCode::OK, &value),
        Ok(Err(error)) => error_response(invalid, error.0),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "usage task failed"),
    }
}

async fn usage_meta(State(accounting): State<Arc<Accounting>>) -> Response {
    read_ledger(&accounting, build_meta, StatusCode::INTERNAL_SERVER_ERROR).await
}

fn build_meta(accounting: &Accounting) -> Result<UsageMeta, crate::usage_ledger::QueryError> {
    let tokens = accounting
        .tokens
        .as_ref()
        .map(|store| store.list(unix_ms()))
        .unwrap_or_default();
    let mut principals = BTreeMap::new();
    for token in tokens {
        principals.insert(token.id, (token.name, "token", Some(token.status.label())));
    }
    for key in accounting.ledger.principals()? {
        principals
            .entry(key.clone())
            .or_insert_with(|| match key.as_str() {
                PRINCIPAL_MASTER => ("master token".to_owned(), "master", None),
                PRINCIPAL_ANONYMOUS => ("anonymous".to_owned(), "anonymous", None),
                PRINCIPAL_REJECTED => ("rejected".to_owned(), "rejected", None),
                _ => (format!("{TOKEN_PREFIX}{key}"), "deleted_token", None),
            });
    }
    let settings = accounting.ledger.settings();
    Ok(UsageMeta {
        principals: principals
            .into_iter()
            .map(|(key, (name, kind, status))| Principal {
                key,
                name,
                kind,
                status,
            })
            .collect(),
        models: accounting.ledger.models(),
        upstreams: accounting.upstreams.clone(),
        minute_retention_hours: settings.minute_retention_hours,
        hour_retention_days: settings.hour_retention_days,
        recent_capacity: settings.recent_capacity,
        tokens_enabled: accounting.tokens_enabled(),
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

async fn usage_series(
    State(accounting): State<Arc<Accounting>>,
    Query(mut query): Query<SeriesQuery>,
) -> Response {
    query.token = non_empty(query.token);
    query.model = non_empty(query.model);
    read_ledger(
        &accounting,
        move |accounting| accounting.ledger.series(&query, unix_ms()),
        StatusCode::BAD_REQUEST,
    )
    .await
}

#[derive(Serialize)]
struct RecentResponse {
    capacity: usize,
    requests: Vec<RequestRecord>,
}

async fn usage_requests(
    State(accounting): State<Arc<Accounting>>,
    Query(mut query): Query<RecentQuery>,
) -> Response {
    query.token = non_empty(query.token);
    query.model = non_empty(query.model);
    read_ledger(
        &accounting,
        move |accounting| {
            Ok(RecentResponse {
                capacity: accounting.ledger.settings().recent_capacity,
                requests: accounting.ledger.recent(&query)?,
            })
        },
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "engine-master-token";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn enforcing() -> (Accounting, std::path::PathBuf) {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let dir = std::env::temp_dir().join(format!("rj-acct-{}", crate::ui_auth::hex(&nonce)));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = Settings::from_lookup(|key| match key {
            "RJ_API_TOKENS_MODE" => Some("on".to_owned()),
            "RJ_API_STATE_DIR" => Some(dir.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap();
        let accounting =
            Accounting::new(&settings, Some(MASTER), vec!["http://a".to_owned()], None).unwrap();
        (accounting, dir)
    }

    #[test]
    fn accounting_survives_a_corrupt_usage_store() {
        let (_, dir) = enforcing();
        std::fs::remove_file(dir.join(crate::usage_ledger::DB_FILE)).ok();
        std::fs::write(
            dir.join(crate::usage_ledger::DB_FILE),
            b"definitely not a sqlite database, just bytes",
        )
        .unwrap();
        let settings = Settings::from_lookup(|key| match key {
            "RJ_API_STATE_DIR" => Some(dir.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap();
        let registry = Registry::new();
        let accounting =
            Accounting::new(&settings, Some(MASTER), Vec::new(), Some(&registry)).unwrap();
        accounting.finish(
            &RequestAccount::new(Arc::from(PRINCIPAL_MASTER), "chat"),
            &Outcome::local(StatusCode::OK, "complete", Duration::from_millis(1)),
        );
        assert_eq!(
            accounting.ledger().principals().unwrap(),
            [PRINCIPAL_MASTER]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accounting_settings_validate() {
        assert!(Settings::from_lookup(|_| None).is_ok());
        assert!(
            Settings::from_lookup(|key| (key == "RJ_API_TOKENS_MODE").then(|| "on".to_owned()))
                .is_err()
        );
        assert!(
            Settings::from_lookup(|key| (key == "RJ_API_TOKENS_MODE").then(|| "yes".to_owned()))
                .is_err()
        );
        assert!(
            Settings::from_lookup(|key| (key == "RJ_USAGE_RETENTION_DAYS").then(|| "0".to_owned()))
                .is_err()
        );
        let on = Settings::from_lookup(|key| match key {
            "RJ_API_TOKENS_MODE" => Some("on".to_owned()),
            "RJ_API_STATE_DIR" => Some("/tmp".to_owned()),
            _ => None,
        })
        .unwrap();
        assert!(Accounting::new(&on, None, Vec::new(), None).is_err());
    }

    #[test]
    fn accounting_off_mode_attributes_without_enforcing() {
        let accounting = Accounting::passive(Some(MASTER), Vec::new());
        assert_eq!(
            accounting.authenticate(&headers(&[("authorization", "Bearer engine-master-token")])),
            Admission::Allow {
                principal: Arc::from(PRINCIPAL_MASTER),
                exchange: false
            }
        );
        for presented in [
            headers(&[]),
            headers(&[("authorization", "Bearer something-else")]),
            headers(&[("authorization", "Basic x"), ("authorization", "Basic y")]),
        ] {
            assert_eq!(
                accounting.authenticate(&presented),
                Admission::Allow {
                    principal: Arc::from(PRINCIPAL_ANONYMOUS),
                    exchange: false
                }
            );
        }
    }

    #[test]
    fn accounting_on_mode_enforces_and_exchanges_minted_tokens() {
        let (accounting, dir) = enforcing();
        let store = accounting.tokens.as_ref().unwrap();
        let (view, secret) = store
            .create(
                &NewToken {
                    name: "fleet".to_owned(),
                    ..NewToken::default()
                },
                unix_ms(),
            )
            .unwrap();
        assert_eq!(
            accounting.authenticate(&headers(&[("authorization", &format!("Bearer {secret}"))])),
            Admission::Allow {
                principal: Arc::from(view.id.as_str()),
                exchange: true
            }
        );
        assert_eq!(
            accounting.authenticate(&headers(&[("x-api-key", &secret)])),
            Admission::Allow {
                principal: Arc::from(view.id.as_str()),
                exchange: true
            }
        );
        assert_eq!(
            accounting.authenticate(&headers(&[("x-api-key", MASTER)])),
            Admission::Allow {
                principal: Arc::from(PRINCIPAL_MASTER),
                exchange: false
            }
        );
        for (presented, reason) in [
            (headers(&[]), "missing"),
            (headers(&[("authorization", "Bearer nope")]), "invalid"),
            (headers(&[("authorization", "Basic abc")]), "malformed"),
            (
                headers(&[
                    ("authorization", &format!("Bearer {secret}")),
                    ("x-api-key", MASTER),
                ]),
                "ambiguous",
            ),
            (
                headers(&[("authorization", "Bearer rjk_000000000000_00")]),
                "malformed",
            ),
        ] {
            assert_eq!(
                accounting.authenticate(&presented),
                Admission::Reject { reason }
            );
        }
        store.revoke(&view.id, unix_ms()).unwrap();
        assert_eq!(
            accounting.authenticate(&headers(&[("authorization", &format!("Bearer {secret}"))])),
            Admission::Reject { reason: "revoked" }
        );

        let mut outbound = headers(&[
            ("authorization", &format!("Bearer {secret}")),
            ("x-api-key", &secret),
            ("content-type", "application/json"),
        ]);
        accounting.exchange_credentials(&mut outbound);
        assert_eq!(outbound["authorization"], "Bearer engine-master-token");
        assert!(outbound.get("x-api-key").is_none());
        assert_eq!(outbound["content-type"], "application/json");
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    async fn call(base: &str, method: &str, path: &str, body: &str) -> (StatusCode, String) {
        let response = reqwest::Client::new()
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        (status, response.text().await.unwrap())
    }

    #[tokio::test]
    async fn accounting_token_api_round_trip() {
        let (accounting, dir) = enforcing();
        let accounting = Arc::new(accounting);
        let router = serve(accounting.tokens_router().merge(accounting.usage_router())).await;
        let (status, body) = call(
            &router,
            "POST",
            "/api/tokens",
            r#"{"name":"helix","description":"fleet"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = created["token"]["id"].as_str().unwrap().to_owned();
        let secret = created["secret"].as_str().unwrap().to_owned();
        assert!(secret.starts_with("rjk_"));

        let (status, body) = call(&router, "GET", "/api/tokens", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&id) && !body.contains(&secret[17..]));

        let (status, body) = call(
            &router,
            "PATCH",
            &format!("/api/tokens/{id}"),
            r#"{"name":"helix-prod","expires_at_ms":null}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("helix-prod"));

        let (status, _) = call(&router, "POST", "/api/tokens", r#"{"name":""}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = call(&router, "POST", &format!("/api/tokens/{id}/revoke"), "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""status":"revoked""#));

        accounting.finish(
            &RequestAccount {
                model: Some("qwen".to_owned()),
                ..RequestAccount::new(Arc::from(id.as_str()), "chat")
            },
            &Outcome::local(StatusCode::OK, "complete", Duration::from_millis(5)),
        );
        let (status, body) = call(&router, "GET", "/api/usage/meta", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("helix-prod") && body.contains("qwen"));
        let (status, body) = call(
            &router,
            "GET",
            "/api/usage/series?granularity=minute&group_by=model&token=",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(r#""key":"qwen""#));
        let (status, body) = call(
            &router,
            "GET",
            "/api/usage/requests?limit=5&errors_only=false",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&id));

        let (status, _) = call(&router, "DELETE", &format!("/api/tokens/{id}"), "").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(&router, "GET", "/api/usage/meta", "").await;
        assert!(body.contains("deleted_token"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
