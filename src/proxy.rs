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
    config::Config,
    exact_route_inventory::ExactRouteInventory,
    exact_shadow::ExactRouteSnapshot,
    journal::RouteJournal,
    kv_consumer::SharedFencedInventory,
    metrics::Metrics,
    prepare::PreparedRequest,
    router::{Decision, LoadGuard, Router},
    shims::{self, Endpoint},
    tokenizer::{CanarySession, ExactTokens, TokenizerObserver},
    usage::{Accumulator, feed_sse_chunk},
};

const MAX_REQUEST_BODY: usize = 64 << 20;
const MAX_PROBE_BODY: usize = 64 << 10;
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
    inventories: Arc<[SharedFencedInventory]>,
    journal: RouteJournal,
    tokenizer: TokenizerObserver,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    healthy_replicas: usize,
    total_replicas: usize,
    replicas: Vec<ReplicaHealth>,
}

#[derive(Serialize)]
struct ReplicaHealth {
    index: usize,
    healthy: bool,
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
            Arc::clone(&inventories),
            inventories
                .iter()
                .cloned()
                .map(ExactRouteInventory::direct)
                .collect(),
        )
    }

    /// Builds the proxy with an independent exact-route inventory backend.
    /// Raw direct inventories remain available for legacy health reporting.
    ///
    /// # Errors
    ///
    /// Returns an error when explicit local tokenizer initialization fails.
    pub fn new_with_exact_inventories(
        config: Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        router: Arc<Router>,
        inventories: Arc<[SharedFencedInventory]>,
        exact_inventories: Arc<[ExactRouteInventory]>,
    ) -> anyhow::Result<Self> {
        let journal = RouteJournal::new(config.route_journal);
        let tokenizer = TokenizerObserver::with_exact_inventories(
            &config,
            client.clone(),
            Arc::clone(&metrics),
            exact_inventories,
        )?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                client,
                metrics,
                router,
                inventories,
                journal,
                tokenizer,
            }),
        })
    }

    #[must_use]
    pub fn router(&self) -> &Arc<Router> {
        &self.inner.router
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
                        let exact_inventory = proxy.inner.inventories.get(index).map(|inventory| {
                            let inventory = inventory.read();
                            let stats = inventory.stats();
                            ExactInventoryHealth {
                                trusted: inventory.trusted(),
                                resident_blocks: stats.external_hashes,
                                resident_tokens: stats.token_ids,
                            }
                        });
                        ReplicaHealth {
                            index,
                            healthy,
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
            br#"{"status":"unhealthy","healthy_replicas":0,"total_replicas":0,"replicas":[]}"#
                .to_vec()
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
        let canary_session_id = exact_canary_session(&parts.headers);
        let endpoint = shims::endpoint(parts.uri.path());
        let endpoint_label = endpoint.label();
        let Ok(raw_body) = to_bytes(inbound_body, MAX_REQUEST_BODY).await else {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large or unreadable",
            );
        };
        let prepare_tokenizer_body = self.inner.tokenizer.wants_payload(endpoint, raw_body.len());
        let canary_assignment =
            self.inner
                .tokenizer
                .assign_canary(endpoint, prepare_tokenizer_body, canary_session_id);
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
        let mut decision = approximate_decision.clone();
        if let Some(tokens) = &pre_route_tokens {
            self.inner.tokenizer.route_pre_route(
                endpoint,
                tokens,
                canary_assignment,
                &mut decision,
            );
        }
        let pre_route_tokens = pre_route_tokens.map(|tokens| tokens.tokens);
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
            decision.candidate_state.first().map(|state| state.index),
            &self.inner.config,
            canary_assignment,
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
            for upstream in 0..self.inner.config.upstreams.len() {
                self.probe(upstream).await;
            }
        }
    }

    async fn probe(&self, upstream: usize) {
        let started = Instant::now();
        let label = self.upstream_label(upstream);
        let uri = Uri::from_static("/v1/models");
        let url = upstream_url(&self.inner.config.upstreams[upstream], &uri);
        let mut request = self.inner.client.get(url).timeout(Duration::from_secs(5));
        if let Some(token) = &self.inner.config.upstream_token {
            request = request.bearer_auth(token);
        }
        let result = request.send().await;
        let (healthy, reason, models_body) = match result {
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
            self.inner
                .tokenizer
                .attest_upstream(upstream, &models_body)
                .await;
        } else {
            self.inner.tokenizer.invalidate_attestation(upstream);
        }
        self.mark_probe(upstream, healthy, reason);
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
        if !hop_header(name) && name.as_str() != "x-session-id" {
            destination.append(name, value.clone());
        }
    }
    destination
}

fn exact_canary_session(headers: &HeaderMap) -> CanarySession<'_> {
    let mut values = headers.get_all("x-session-id").iter();
    let Some(session_id) = values.next().map(axum::http::HeaderValue::as_bytes) else {
        return CanarySession::Missing;
    };
    if values.next().is_some() || !(1..=256).contains(&session_id.len()) {
        return CanarySession::Invalid;
    }
    CanarySession::Valid(session_id)
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
        let result = filtered_headers(&source);
        assert!(result.contains_key("authorization"));
        assert!(!result.contains_key("connection"));
        assert!(!result.contains_key("x-mini-dynamo-upstream"));
        assert!(!result.contains_key("x-session-id"));
    }

    #[test]
    fn exact_canary_session_requires_one_bounded_nonempty_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(exact_canary_session(&headers), CanarySession::Missing);
        headers.insert("x-session-id", "session-a".parse().unwrap());
        assert_eq!(
            exact_canary_session(&headers),
            CanarySession::Valid(b"session-a")
        );
        headers.append("x-session-id", "session-b".parse().unwrap());
        assert_eq!(exact_canary_session(&headers), CanarySession::Invalid);

        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "".parse().unwrap());
        assert_eq!(exact_canary_session(&headers), CanarySession::Invalid);
        headers.insert("x-session-id", "x".repeat(257).parse().unwrap());
        assert_eq!(exact_canary_session(&headers), CanarySession::Invalid);
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
        assert_eq!(health["healthy_replicas"], 1);
        assert_eq!(health["total_replicas"], 2);
        assert_eq!(health["replicas"][0]["healthy"], true);
        assert_eq!(health["replicas"][1]["healthy"], false);
        assert!(health["replicas"][0].get("exact_inventory").is_none());

        proxy.router().set_healthy(0, false);
        let response = Proxy::health(State(proxy)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["status"], "unhealthy");
        assert_eq!(health["healthy_replicas"], 0);
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

    #[tokio::test]
    async fn dropping_downstream_body_immediately_cancels_a_silent_upstream() {
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
                    .header("content-type", "text/event-stream")
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
