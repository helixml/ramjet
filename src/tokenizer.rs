use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Context;
use axum::body::Bytes;
use dynamo_protocols::types::CreateChatCompletionRequest;
use dynamo_renderer::{
    OAIChatLikeRequest, PromptFormatter, TextInput, deepseek_formatter_for,
    dynamo_tokenizers::{FastTokenizer, traits::Encoder},
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};
use url::Url;

use crate::{
    config::{Config, TokenizerMode},
    metrics::Metrics,
    shims::Endpoint,
};

const REMOTE_BACKEND: &str = "remote";
const LOCAL_BACKEND: &str = "fastokens";
const MAX_RESPONSE_BYTES: usize = 16 << 20;

#[derive(Clone)]
pub struct TokenizerObserver {
    sender: Option<mpsc::Sender<Job>>,
    backend_label: &'static str,
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
    LocalShadow {
        local: Arc<LocalTokenizer>,
        remote: RemoteTokenizer,
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
    tokenizer: FastTokenizer,
    formatter: Arc<dyn dynamo_renderer::OAIPromptFormatter>,
}

struct RenderRequest {
    inner: CreateChatCompletionRequest,
    args: HashMap<String, Value>,
    add_generation_prompt: bool,
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
    pub fn new(
        config: &Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<Self> {
        let sender = match config.tokenizer_mode {
            TokenizerMode::Off => None,
            TokenizerMode::RemoteShadow => {
                let (sender, receiver) = mpsc::channel(config.tokenizer_queue_capacity);
                let backend = Backend::Remote(remote_tokenizer(config, client));
                spawn_workers(receiver, config.tokenizer_workers, &backend, &metrics);
                Some(sender)
            }
            TokenizerMode::LocalShadow => {
                let path = config
                    .tokenizer_path
                    .as_deref()
                    .context("DS4_TOKENIZER_PATH is required in local-shadow mode")?;
                let tokenizer = FastTokenizer::from_file(path)
                    .with_context(|| format!("load fastokens tokenizer from {path}"))?;
                let PromptFormatter::OAI(formatter) =
                    deepseek_formatter_for(&Some("deepseek_v4".to_owned()), "deepseek-v4-flash")
                        .context("DeepSeek-V4 formatter unavailable")?;
                let (sender, receiver) = mpsc::channel(config.tokenizer_queue_capacity);
                let backend = Backend::LocalShadow {
                    local: Arc::new(LocalTokenizer {
                        tokenizer,
                        formatter,
                    }),
                    remote: remote_tokenizer(config, client),
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
            .with_label_values(&[self.backend_label, endpoint.label(), outcome])
            .inc();
    }
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
        Backend::Remote(remote) => {
            let started = Instant::now();
            let result = remote.tokenize(&job).await;
            record_remote(metrics, endpoint, started.elapsed(), &result);
        }
        Backend::LocalShadow { local, remote } => {
            let local = Arc::clone(local);
            let local_endpoint = job.endpoint;
            let local_body = job.body.clone();
            let local_future = async move {
                let started = Instant::now();
                let result = tokio::task::spawn_blocking(move || {
                    local.tokenize(local_endpoint, &local_body)
                })
                .await
                .map_err(|_| LocalFailure::Join)
                .and_then(std::convert::identity);
                (started.elapsed(), result)
            };
            let remote_future = async {
                let started = Instant::now();
                let result = remote.tokenize(&job).await;
                (started.elapsed(), result)
            };
            let ((local_duration, local_result), (remote_duration, remote_result)) =
                tokio::join!(local_future, remote_future);
            record_remote(metrics, endpoint, remote_duration, &remote_result);
            metrics
                .tokenizer_duration
                .with_label_values(&[LOCAL_BACKEND, endpoint])
                .observe(local_duration.as_secs_f64());
            match (&local_result, &remote_result) {
                (Ok(local), Ok(remote)) => {
                    metrics
                        .tokenizer_tokens
                        .with_label_values(&[LOCAL_BACKEND, endpoint])
                        .observe(usize_to_f64(local.token_ids.len()));
                    let outcome = if local == remote {
                        "parity_match"
                    } else {
                        "parity_mismatch"
                    };
                    metrics
                        .tokenizer_shadow
                        .with_label_values(&[LOCAL_BACKEND, endpoint, outcome])
                        .inc();
                }
                (Err(error), _) => metrics
                    .tokenizer_shadow
                    .with_label_values(&[LOCAL_BACKEND, endpoint, error.label()])
                    .inc(),
                (Ok(local), Err(_)) => {
                    metrics
                        .tokenizer_tokens
                        .with_label_values(&[LOCAL_BACKEND, endpoint])
                        .observe(usize_to_f64(local.token_ids.len()));
                    metrics
                        .tokenizer_shadow
                        .with_label_values(&[
                            LOCAL_BACKEND,
                            endpoint,
                            "remote_authority_unavailable",
                        ])
                        .inc();
                }
            }
        }
    }
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

    fn encode(&self, prompt: &str) -> Result<ExactTokens, LocalFailure> {
        let encoding = self
            .tokenizer
            .encode(prompt)
            .map_err(|_| LocalFailure::Encode)?;
        Ok(ExactTokens {
            token_ids: encoding.token_ids().to_vec(),
        })
    }
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
        let observer = TokenizerObserver::new(&config, reqwest::Client::new(), metrics).unwrap();
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
            TokenizerObserver::new(&config, reqwest::Client::new(), Arc::clone(&metrics)).unwrap();
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
}
