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
use tokio::{sync::mpsc, time::Instant};
use tokio_stream::wrappers::ReceiverStream;
use url::Url;

use crate::{
    config::Config,
    journal::RouteJournal,
    metrics::Metrics,
    prepare::PreparedRequest,
    router::{Decision, LoadGuard, Router},
    shims::{self, Endpoint},
    usage::{Accumulator, feed_sse_chunk},
};

const MAX_REQUEST_BODY: usize = 64 << 20;
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
    journal: RouteJournal,
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
    #[must_use]
    pub fn new(
        config: Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        router: Arc<Router>,
    ) -> Self {
        let journal = RouteJournal::new(config.route_journal);
        Self {
            inner: Arc::new(Inner {
                config,
                client,
                metrics,
                router,
                journal,
            }),
        }
    }

    #[must_use]
    pub fn router(&self) -> &Arc<Router> {
        &self.inner.router
    }

    pub async fn handle(State(proxy): State<Self>, request: Request<Body>) -> Response<Body> {
        proxy.serve(request).await
    }

    #[allow(clippy::too_many_lines)]
    async fn serve(&self, request: Request<Body>) -> Response<Body> {
        let started = Instant::now();
        let (parts, inbound_body) = request.into_parts();
        let endpoint = shims::endpoint(parts.uri.path());
        let endpoint_label = endpoint.label();
        let Ok(raw_body) = to_bytes(inbound_body, MAX_REQUEST_BODY).await else {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large or unreadable",
            );
        };
        let prepared = PreparedRequest::new(
            endpoint,
            &raw_body,
            self.inner.config.max_tokens_strip,
            &self.inner.router,
        );
        let decision = prepared.route(&self.inner.router);
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
        let journal_sequence =
            self.inner
                .journal
                .start(endpoint_label, body.len(), &decision, &self.inner.config);

        let mut last_error = None;
        let mut selected = None;
        for (rank, &candidate) in decision.candidates.iter().enumerate() {
            let url = upstream_url(&self.inner.config.upstreams[candidate], &parts.uri);
            let mut outbound = self
                .inner
                .client
                .request(parts.method.clone(), url)
                .body(body.clone());
            outbound = outbound.headers(filtered_headers(&parts.headers));
            let units = decision
                .candidate_state
                .get(rank)
                .map_or(decision.load_units, |state| state.request_load_units);
            let load = self.acquire(candidate, units);
            match outbound.send().await {
                Ok(response)
                    if matches!(
                        response.status(),
                        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
                    ) && rank + 1 < decision.candidates.len() =>
                {
                    self.inner.router.set_healthy(candidate, false);
                    self.record_upstream_request(candidate, response.status());
                    drop(load);
                }
                Ok(response) => {
                    if rank > 0 {
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
            let reason = last_error.unwrap_or("protocol");
            let status = if reason == "timeout" {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
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

    #[allow(clippy::too_many_arguments)]
    async fn relay(
        &self,
        sender: mpsc::Sender<Result<Bytes, io::Error>>,
        response: reqwest::Response,
        endpoint: Endpoint,
        upstream: usize,
        status: StatusCode,
        streaming: bool,
        fingerprints: Vec<u64>,
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
        while let Some(item) = stream.next().await {
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

    fn acquire(&self, upstream: usize, units: usize) -> RoutedLoad {
        let guard = self.inner.router.acquire(upstream, units);
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
        RoutedLoad {
            guard: Some(guard),
            router: Arc::clone(&self.inner.router),
            metrics: Arc::clone(&self.inner.metrics),
            upstream,
            label,
        }
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
        let (healthy, reason) = match result {
            Ok(response) if response.status() == StatusCode::OK => match response.bytes().await {
                Ok(_) => (true, ""),
                Err(error) => (false, upstream_error_reason(&error)),
            },
            Ok(_) => (false, "http"),
            Err(error) => (false, upstream_error_reason(&error)),
        };
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
        if !hop_header(name) {
            destination.append(name, value.clone());
        }
    }
    destination
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

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use axum::{Router as AxumRouter, routing::any};
    use prometheus::Registry;

    use super::*;

    async fn start_upstream(app: AxumRouter) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{address}")).unwrap(), task)
    }

    fn proxy_for(upstreams: &[Url]) -> Proxy {
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
        Proxy::new(config, reqwest::Client::new(), metrics, router)
    }

    #[test]
    fn strips_hop_and_route_headers() {
        let mut source = HeaderMap::new();
        source.insert("authorization", "Bearer okay".parse().unwrap());
        source.insert("connection", "close".parse().unwrap());
        source.insert("x-mini-dynamo-upstream", "secret".parse().unwrap());
        let result = filtered_headers(&source);
        assert!(result.contains_key("authorization"));
        assert!(!result.contains_key("connection"));
        assert!(!result.contains_key("x-mini-dynamo-upstream"));
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

    #[tokio::test]
    async fn proxies_sanitized_requests_and_usage_responses() {
        let upstream = AxumRouter::new().fallback(any(|request: Request<Body>| async move {
            let body = to_bytes(request.into_body(), MAX_REQUEST_BODY).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if request.get("max_tokens").is_some() || request.get("reasoning_effort").is_some() {
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
