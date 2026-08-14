use std::{
    io,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, Method, Request, Response, StatusCode, Uri},
};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::{sync::mpsc, time::Instant};
use tokio_stream::wrappers::ReceiverStream;
use url::Url;

use crate::{
    config::{Config, UpstreamAdmissionMode},
    exact_route_inventory::ExactRouteInventory,
    exact_shadow::ExactRouteSnapshot,
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
const STREAM_BUFFER_CHUNKS: usize = 8;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility_attested: Option<bool>,
    inflight: usize,
    load_units: usize,
    approximate_index_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    exact_inventory: Option<ExactInventoryHealth>,
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

impl Drop for RoutedLoad {
    fn drop(&mut self) {
        drop(self.guard.take());
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
}

impl Proxy {
    /// Builds the proxy and its optional tokenizer workers.
    ///
    /// # Errors
    ///
    /// Returns an error when explicit local tokenizer initialization fails.
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
    /// Returns an error when explicit local tokenizer initialization fails.
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
        Ok(Self::from_parts(
            config,
            client,
            metrics,
            router,
            exact_inventories,
            journal,
            session_affinity,
            tokenizer,
        ))
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
    ) -> Self {
        if config.upstream_admission_mode == UpstreamAdmissionMode::Compatibility {
            for (index, upstream) in config.upstreams.iter().enumerate() {
                router.set_healthy(index, false);
                metrics
                    .upstream_up
                    .with_label_values(&[upstream.as_str().trim_end_matches('/')])
                    .set(0.0);
                metrics
                    .upstream_compatibility_admitted
                    .with_label_values(&[upstream.as_str().trim_end_matches('/')])
                    .set(0.0);
            }
        }
        Self {
            inner: Arc::new(Inner {
                config,
                client,
                metrics,
                router,
                exact_inventories,
                journal,
                session_affinity,
                tokenizer,
            }),
        }
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
                    |(inflight, load_units, approximate_index_entries, healthy)| {
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
                        let healthy = healthy && compatibility_attested.unwrap_or(true);
                        ReplicaHealth {
                            index,
                            healthy,
                            compatibility_attested,
                            inflight,
                            load_units,
                            approximate_index_entries,
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
            },
        );

        let serving_candidates = decision
            .candidates
            .iter()
            .copied()
            .filter(|candidate| {
                decision
                    .candidate_state
                    .iter()
                    .find(|state| state.index == *candidate)
                    .is_some_and(|state| state.healthy)
            })
            .collect::<Vec<_>>();
        let mut last_error = None;
        let mut selected = None;
        for (attempt, &candidate) in serving_candidates.iter().enumerate() {
            let url = upstream_url(&self.inner.config.upstreams[candidate], &parts.uri);
            let mut outbound = self
                .inner
                .client
                .request(parts.method.clone(), url)
                .body(body.clone());
            outbound = outbound.headers(filtered_headers(&parts.headers));
            let units = decision
                .candidate_state
                .iter()
                .find(|state| state.index == candidate)
                .map_or(decision.load_units, |state| state.request_load_units);
            let Some(load) = self.acquire_if_healthy(candidate, units) else {
                continue;
            };
            match outbound.send().await {
                Ok(response)
                    if matches!(
                        response.status(),
                        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
                    ) && attempt + 1 < serving_candidates.len() =>
                {
                    self.inner.router.set_healthy(candidate, false);
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
                    selected = Some((candidate, response, load));
                    break;
                }
                Err(error) => {
                    last_error = Some(upstream_error_reason(&error));
                    self.inner.router.set_healthy(candidate, false);
                    drop(load);
                }
            }
        }

        let Some((upstream, response, load_guard)) = selected else {
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
        _load_guard: RoutedLoad,
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
                        if usage.generated {
                            first_token.get_or_insert(received);
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

    fn acquire_if_healthy(&self, upstream: usize, units: usize) -> Option<RoutedLoad> {
        let guard = self.inner.router.acquire_if_healthy(upstream, units)?;
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
            self.inner.router.set_healthy(upstream, false);
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
        self.inner.router.set_healthy(upstream, healthy);
        let label = self.upstream_label(upstream);
        self.inner
            .metrics
            .upstream_up
            .with_label_values(&[&label])
            .set(if healthy { 1.0 } else { 0.0 });
        if healthy {
            self.inner
                .metrics
                .last_upstream_success
                .with_label_values(&[&label])
                .set(unix_seconds());
        } else {
            self.inner
                .metrics
                .upstream_probe_errors
                .with_label_values(&[&label, reason])
                .inc();
        }
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
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use axum::{Router as AxumRouter, routing::any};
    use prometheus::Registry;

    use super::*;
    use crate::{
        compat::{
            CompatibilityManifest, EngineIdentity, KvEventsIdentity, ModelIdentity,
            RendererIdentity, ServingRuntimeEngine, ServingRuntimeManifest, TokenizerIdentity,
        },
        exact_index::{ExactIndexLimits, FencedExactKvInventory},
        kv_wire::{BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
    };

    struct DropSignal {
        dropped: Arc<AtomicBool>,
        notify: Arc<tokio::sync::Notify>,
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
            Config::from_lookup(|key| (key == "DS4_UPSTREAM").then(|| joined.clone())).unwrap();
        proxy_for_config(config, inventories)
    }

    fn proxy_for_with_token(upstreams: &[Url], token: &str) -> Proxy {
        let joined = upstreams
            .iter()
            .map(Url::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let config = Config::from_lookup(|key| match key {
            "DS4_UPSTREAM" => Some(joined.clone()),
            "DS4_UPSTREAM_TOKEN" => Some(token.to_owned()),
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
        ServingRuntimeManifest {
            schema_version: 1,
            compatibility_manifest_sha256: "c".repeat(64),
            engine: ServingRuntimeEngine {
                core_process_count: 1,
                kv_events: KvEventsIdentity {
                    enable_kv_cache_events: true,
                    publisher: "zmq".to_owned(),
                    endpoint: "tcp://*:5557".to_owned(),
                    replay_endpoint: "tcp://*:5558".to_owned(),
                    buffer_steps: 10_000,
                    hwm: 100_000,
                    max_queue_size: 100_000,
                    topic: String::new(),
                },
            },
        }
    }

    fn test_serving_identity(digest: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2,
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
            ("DS4_UPSTREAM", joined),
            ("DS4_SESSION_AFFINITY_MODE", "shadow".to_owned()),
            (
                "DS4_SESSION_AFFINITY_KEY",
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
            ("DS4_UPSTREAM", joined),
            ("DS4_TOKENIZER_MODE", "remote-shadow".to_owned()),
            ("DS4_TOKENIZER_MIN_BYTES", "0".to_owned()),
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
        proxy.router().set_healthy(1, false);

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
        proxy.router().set_healthy(1, false);
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
        assert!(health["replicas"][0].get("exact_inventory").is_none());
        assert!(
            health["replicas"][0]
                .get("compatibility_attested")
                .is_none()
        );

        proxy.router().set_healthy(0, false);
        let response = Proxy::health(State(proxy)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["status"], "unhealthy");
        assert_eq!(health["healthy_replicas"], 0);
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
            Config::from_lookup(|key| (key == "DS4_UPSTREAM").then(|| joined.clone())).unwrap();
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
            Config::from_lookup(|key| (key == "DS4_UPSTREAM").then(|| joined.clone())).unwrap();
        config.upstream_admission_mode = UpstreamAdmissionMode::Compatibility;
        let proxy = proxy_for_config_with_manifest(config, test_compatibility_manifest());

        assert!(!proxy.router().state(0).unwrap().3);
        proxy.probe(0).await;
        assert!(proxy.router().state(0).unwrap().3);
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
        identity_release.notify_one();
        checking.await.unwrap();
        block_identity.store(false, Ordering::Release);
        assert!(!proxy.router().state(0).unwrap().3);

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
            Config::from_lookup(|key| (key == "DS4_UPSTREAM").then(|| joined.clone())).unwrap();
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
            Config::from_lookup(|key| (key == "DS4_UPSTREAM").then(|| joined.clone())).unwrap();
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
            Config::from_lookup(|key| (key == "DS4_UPSTREAM").then(|| joined.clone())).unwrap();
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

    #[tokio::test]
    async fn no_healthy_replica_returns_503_without_dialing_upstream() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let upstream = AxumRouter::new().fallback(any(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                StatusCode::OK
            }
        }));
        let (url, task) = start_upstream(upstream).await;
        let proxy = proxy_for(&[url]);
        proxy.router().set_healthy(0, false);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        let response = proxy.serve(request).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(requests.load(Ordering::Relaxed), 0);
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
