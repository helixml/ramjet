use prometheus::{
    CounterVec, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, Opts, Registry,
};

pub struct Metrics {
    pub requests: CounterVec,
    pub inflight: Gauge,
    pub duration: HistogramVec,
    pub ttft: HistogramVec,
    pub prompt_tokens: CounterVec,
    pub cached_tokens: CounterVec,
    pub completion_tokens: CounterVec,
    pub context_size: HistogramVec,
    pub output_size: HistogramVec,
    pub decode_tps: HistogramVec,
    pub tpot: HistogramVec,
    pub request_bytes: HistogramVec,
    pub response_bytes: HistogramVec,
    pub parse_failures: CounterVec,
    pub finish_reasons: CounterVec,
    pub upstream_up: GaugeVec,
    pub upstream_probe_time: GaugeVec,
    pub upstream_probe_errors: CounterVec,
    pub upstream_errors: CounterVec,
    pub client_disconnects: CounterVec,
    pub last_upstream_success: GaugeVec,
    pub upstream_requests: CounterVec,
    pub route_decisions: CounterVec,
    pub route_overlap: Histogram,
    pub route_affinity: Histogram,
    pub upstream_inflight: GaugeVec,
    pub upstream_load_units: GaugeVec,
    pub tokenizer_shadow: CounterVec,
    pub tokenizer_duration: HistogramVec,
    pub tokenizer_tokens: HistogramVec,
    pub tokenizer_queue_depth: Gauge,
    pub kv_event_up: GaugeVec,
    pub kv_event_trusted: GaugeVec,
    pub kv_event_generation: GaugeVec,
    pub kv_event_index_entries: GaugeVec,
    pub kv_event_batches: CounterVec,
    pub kv_event_filtered: CounterVec,
    pub kv_event_reconnects: CounterVec,
    pub kv_event_replay_batches: HistogramVec,
}

impl Metrics {
    /// Creates and registers the dashboard-compatible metric surface.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error if a descriptor is invalid or already registered.
    #[allow(clippy::too_many_lines)]
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let latency = vec![
            0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0,
            80.0, 160.0, 320.0, 640.0, 1280.0, 2560.0,
        ];
        let tokens = vec![
            256.0, 1_024.0, 4_096.0, 8_192.0, 16_384.0, 32_768.0, 65_536.0, 98_304.0, 131_072.0,
        ];
        let tps = vec![
            1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 30.0, 40.0, 60.0, 80.0, 120.0, 180.0, 240.0,
        ];
        let counter = |name, help, labels| CounterVec::new(Opts::new(name, help), labels);
        let histogram = |name, help, buckets, labels| {
            HistogramVec::new(HistogramOpts::new(name, help).buckets(buckets), labels)
        };
        let gauge = |name, help, labels| GaugeVec::new(Opts::new(name, help), labels);

        let metrics = Self {
            requests: counter(
                "ds4proxy_requests_total",
                "Completed proxied requests",
                &["endpoint", "code", "stream"],
            )?,
            inflight: Gauge::with_opts(Opts::new(
                "ds4proxy_requests_inflight",
                "Requests currently in flight",
            ))?,
            duration: histogram(
                "ds4proxy_request_duration_seconds",
                "Full request duration",
                vec![
                    0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
                ],
                &["endpoint"],
            )?,
            ttft: histogram(
                "ds4proxy_ttft_seconds",
                "Time to first generated token or tool-call delta (streaming only)",
                latency,
                &["endpoint"],
            )?,
            prompt_tokens: counter(
                "ds4proxy_prompt_tokens_total",
                "Prompt tokens processed",
                &["endpoint"],
            )?,
            cached_tokens: counter(
                "ds4proxy_cached_prompt_tokens_total",
                "Prompt tokens served from KV prefix cache",
                &["endpoint"],
            )?,
            completion_tokens: counter(
                "ds4proxy_completion_tokens_total",
                "Tokens generated",
                &["endpoint"],
            )?,
            context_size: histogram(
                "ds4proxy_context_tokens",
                "Per-request prompt size (tokens)",
                tokens,
                &["endpoint"],
            )?,
            output_size: histogram(
                "ds4proxy_output_tokens",
                "Per-request completion size (tokens)",
                vec![
                    64.0, 256.0, 1024.0, 4096.0, 8192.0, 16384.0, 32768.0, 65536.0,
                ],
                &["endpoint"],
            )?,
            decode_tps: histogram(
                "ds4proxy_decode_tokens_per_second",
                "Per-request decode throughput (completion tokens / time after first generated token)",
                tps,
                &["endpoint"],
            )?,
            tpot: histogram(
                "ds4proxy_time_per_output_token_seconds",
                "Per-request mean time per output token after the first token",
                vec![
                    0.005, 0.01, 0.015, 0.02, 0.025, 0.03, 0.04, 0.05, 0.075, 0.1, 0.15, 0.2, 0.3,
                    0.5, 0.75, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0,
                ],
                &["endpoint"],
            )?,
            request_bytes: histogram(
                "ds4proxy_request_body_bytes",
                "Request body size",
                vec![
                    1_024.0,
                    16_384.0,
                    131_072.0,
                    524_288.0,
                    2_097_152.0,
                    8_388_608.0,
                ],
                &["endpoint"],
            )?,
            response_bytes: histogram(
                "ds4proxy_response_body_bytes",
                "Response body size",
                vec![
                    1_024.0,
                    16_384.0,
                    131_072.0,
                    524_288.0,
                    2_097_152.0,
                    8_388_608.0,
                ],
                &["endpoint"],
            )?,
            parse_failures: counter(
                "ds4proxy_usage_parse_failures_total",
                "Responses where no usage block could be extracted",
                &["endpoint"],
            )?,
            finish_reasons: counter(
                "ds4proxy_finish_reasons_total",
                "Successful responses by finish reason",
                &["endpoint", "reason"],
            )?,
            upstream_up: gauge(
                "ds4proxy_upstream_up",
                "Whether the upstream /v1/models readiness probe is succeeding",
                &["upstream"],
            )?,
            upstream_probe_time: gauge(
                "ds4proxy_upstream_probe_duration_seconds",
                "Duration of the latest upstream readiness probe",
                &["upstream"],
            )?,
            upstream_probe_errors: counter(
                "ds4proxy_upstream_probe_failures_total",
                "Failed upstream readiness probes",
                &["upstream", "reason"],
            )?,
            upstream_errors: counter(
                "ds4proxy_upstream_errors_total",
                "Proxied requests that failed before receiving a complete upstream response",
                &["endpoint", "reason"],
            )?,
            client_disconnects: counter(
                "ds4proxy_client_disconnects_total",
                "Requests cancelled because the downstream client disconnected",
                &["endpoint"],
            )?,
            last_upstream_success: gauge(
                "ds4proxy_last_upstream_success_timestamp_seconds",
                "Unix timestamp of the latest successful upstream readiness probe",
                &["upstream"],
            )?,
            upstream_requests: counter(
                "ds4proxy_upstream_requests_total",
                "Requests dispatched per upstream engine",
                &["upstream", "code"],
            )?,
            route_decisions: counter(
                "ds4proxy_route_decisions_total",
                "Routing decisions by outcome (overlap|load|rr|single)",
                &["outcome"],
            )?,
            route_overlap: Histogram::with_opts(
                HistogramOpts::new(
                    "ds4proxy_route_overlap_blocks",
                    "Prefix-cache overlap depth of the chosen upstream, in fingerprint blocks",
                )
                .buckets(vec![
                    0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0,
                ]),
            )?,
            route_affinity: Histogram::with_opts(
                HistogramOpts::new(
                    "ds4proxy_route_affinity_blocks",
                    "Bounded prefix overlap contribution used in the route score",
                )
                .buckets(vec![0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0]),
            )?,
            upstream_inflight: gauge(
                "ds4proxy_upstream_inflight",
                "In-flight requests per upstream",
                &["upstream"],
            )?,
            upstream_load_units: gauge(
                "ds4proxy_upstream_load_units",
                "Size-weighted in-flight work used by the router",
                &["upstream"],
            )?,
            tokenizer_shadow: counter(
                "ds4proxy_tokenizer_shadow_total",
                "Selective tokenizer observations by backend, endpoint, and outcome",
                &["backend", "endpoint", "outcome"],
            )?,
            tokenizer_duration: histogram(
                "ds4proxy_tokenizer_duration_seconds",
                "Background tokenizer request duration",
                vec![
                    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0,
                ],
                &["backend", "endpoint"],
            )?,
            tokenizer_tokens: histogram(
                "ds4proxy_tokenizer_tokens",
                "Exact tokens returned by a background tokenizer observation",
                vec![
                    256.0, 1_024.0, 4_096.0, 8_192.0, 16_384.0, 32_768.0, 65_536.0, 98_304.0,
                    131_072.0, 262_144.0, 393_216.0,
                ],
                &["backend", "endpoint"],
            )?,
            tokenizer_queue_depth: Gauge::with_opts(Opts::new(
                "ds4proxy_tokenizer_queue_depth",
                "Background tokenizer jobs waiting for a bounded worker",
            ))?,
            kv_event_up: gauge(
                "ds4proxy_kv_event_up",
                "Whether the per-upstream KV-event shadow consumer is connected",
                &["upstream"],
            )?,
            kv_event_trusted: gauge(
                "ds4proxy_kv_event_trusted",
                "Whether the per-upstream exact KV inventory has an authoritative generation",
                &["upstream"],
            )?,
            kv_event_generation: gauge(
                "ds4proxy_kv_event_generation",
                "Current fenced KV-event generation per upstream",
                &["upstream"],
            )?,
            kv_event_index_entries: gauge(
                "ds4proxy_kv_event_index_entries",
                "Resident exact KV index entries by bounded kind",
                &["upstream", "kind"],
            )?,
            kv_event_batches: counter(
                "ds4proxy_kv_event_batches_total",
                "KV-event batches by source and bounded processing outcome",
                &["upstream", "source", "outcome"],
            )?,
            kv_event_filtered: counter(
                "ds4proxy_kv_event_filtered_total",
                "KV events conservatively excluded from the exact index by bounded reason",
                &["upstream", "source", "reason"],
            )?,
            kv_event_reconnects: counter(
                "ds4proxy_kv_event_reconnects_total",
                "KV-event consumer reconnect attempts by bounded reason",
                &["upstream", "reason"],
            )?,
            kv_event_replay_batches: histogram(
                "ds4proxy_kv_event_replay_batches",
                "Number of batches in a bounded KV-event replay response",
                vec![
                    1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0,
                ],
                &["upstream"],
            )?,
        };
        for collector in metrics.collectors() {
            registry.register(collector)?;
        }
        Ok(metrics)
    }

    fn collectors(&self) -> Vec<Box<dyn prometheus::core::Collector>> {
        vec![
            Box::new(self.requests.clone()),
            Box::new(self.inflight.clone()),
            Box::new(self.duration.clone()),
            Box::new(self.ttft.clone()),
            Box::new(self.prompt_tokens.clone()),
            Box::new(self.cached_tokens.clone()),
            Box::new(self.completion_tokens.clone()),
            Box::new(self.context_size.clone()),
            Box::new(self.output_size.clone()),
            Box::new(self.decode_tps.clone()),
            Box::new(self.tpot.clone()),
            Box::new(self.request_bytes.clone()),
            Box::new(self.response_bytes.clone()),
            Box::new(self.parse_failures.clone()),
            Box::new(self.finish_reasons.clone()),
            Box::new(self.upstream_up.clone()),
            Box::new(self.upstream_probe_time.clone()),
            Box::new(self.upstream_probe_errors.clone()),
            Box::new(self.upstream_errors.clone()),
            Box::new(self.client_disconnects.clone()),
            Box::new(self.last_upstream_success.clone()),
            Box::new(self.upstream_requests.clone()),
            Box::new(self.route_decisions.clone()),
            Box::new(self.route_overlap.clone()),
            Box::new(self.route_affinity.clone()),
            Box::new(self.upstream_inflight.clone()),
            Box::new(self.upstream_load_units.clone()),
            Box::new(self.tokenizer_shadow.clone()),
            Box::new(self.tokenizer_duration.clone()),
            Box::new(self.tokenizer_tokens.clone()),
            Box::new(self.tokenizer_queue_depth.clone()),
            Box::new(self.kv_event_up.clone()),
            Box::new(self.kv_event_trusted.clone()),
            Box::new(self.kv_event_generation.clone()),
            Box::new(self.kv_event_index_entries.clone()),
            Box::new(self.kv_event_batches.clone()),
            Box::new(self.kv_event_filtered.clone()),
            Box::new(self.kv_event_reconnects.clone()),
            Box::new(self.kv_event_replay_batches.clone()),
        ]
    }
}
