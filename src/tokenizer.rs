use std::{sync::Arc, time::Duration};

use axum::body::Bytes;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::{sync::mpsc, time::Instant};
use url::Url;

use crate::{
    config::{Config, TokenizerMode},
    metrics::Metrics,
    shims::Endpoint,
};

const REMOTE_BACKEND: &str = "remote";
const MAX_RESPONSE_BYTES: usize = 16 << 20;

#[derive(Clone)]
pub struct TokenizerObserver {
    sender: Option<mpsc::Sender<Job>>,
    min_bytes: usize,
    max_bytes: usize,
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
struct Job {
    endpoint: Endpoint,
    upstream: usize,
    body: Bytes,
}

#[derive(Clone)]
enum Backend {
    Remote(RemoteTokenizer),
}

#[derive(Clone)]
struct RemoteTokenizer {
    client: reqwest::Client,
    upstreams: Vec<Url>,
    token: Option<String>,
    timeout: Duration,
}

#[derive(Debug, PartialEq)]
struct ExactTokens {
    token_ids: Vec<u32>,
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

impl TokenizerObserver {
    #[must_use]
    pub fn new(config: &Config, client: reqwest::Client, metrics: Arc<Metrics>) -> Self {
        let sender = match config.tokenizer_mode {
            TokenizerMode::Off => None,
            TokenizerMode::RemoteShadow => {
                let (sender, receiver) = mpsc::channel(config.tokenizer_queue_capacity);
                let timeout_ms = u64::try_from(config.tokenizer_timeout_ms).unwrap_or(u64::MAX);
                let backend = Backend::Remote(RemoteTokenizer {
                    client,
                    upstreams: config.upstreams.clone(),
                    token: config.upstream_token.clone(),
                    timeout: Duration::from_millis(timeout_ms),
                });
                spawn_workers(receiver, config.tokenizer_workers, &backend, &metrics);
                Some(sender)
            }
        };
        Self {
            sender,
            min_bytes: config.tokenizer_min_bytes,
            max_bytes: config.tokenizer_max_bytes,
            metrics,
        }
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

    /// Enqueues a post-request shadow observation without waiting for capacity.
    pub fn submit(&self, endpoint: Endpoint, upstream: usize, body: Option<Vec<u8>>) {
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
            body: Bytes::from(body),
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
            .with_label_values(&[REMOTE_BACKEND, endpoint.label(), outcome])
            .inc();
    }
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
    let started = Instant::now();
    let endpoint = job.endpoint.label();
    let result = match backend {
        Backend::Remote(remote) => remote.tokenize(&job).await,
    };
    metrics
        .tokenizer_duration
        .with_label_values(&[REMOTE_BACKEND, endpoint])
        .observe(started.elapsed().as_secs_f64());
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

impl RemoteTokenizer {
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
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(Failure::ResponseTooLarge);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| classify_request_error(&error))?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(Failure::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
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

    use axum::{Router, body::to_bytes, http::Request, routing::post};
    use prometheus::Registry;

    use super::*;

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
            })
            .await
            .unwrap();
        assert_eq!(result.token_ids, [7, 11, 13]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[test]
    fn off_mode_does_not_prepare_or_enqueue() {
        let config = Config::from_lookup(|_| None).unwrap();
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let observer = TokenizerObserver::new(&config, reqwest::Client::new(), metrics);
        assert!(!observer.wants_payload(Endpoint::Chat, 100_000));
        observer.submit(Endpoint::Chat, 0, None);
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
        let observer =
            TokenizerObserver::new(&config, reqwest::Client::new(), Arc::clone(&metrics));
        assert!(observer.wants_payload(Endpoint::Chat, 16));
        observer.submit(Endpoint::Chat, 0, Some(br#"{"messages":[]}"#.to_vec()));
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
}
