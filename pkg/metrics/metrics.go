// Package metrics defines the Prometheus surface. Metric names keep the
// ds4proxy_ prefix for dashboard continuity with the deployment this
// replaces.
package metrics

import "github.com/prometheus/client_golang/prometheus"

type Metrics struct {
	Requests            *prometheus.CounterVec
	Inflight            prometheus.Gauge
	Duration            *prometheus.HistogramVec
	TTFT                *prometheus.HistogramVec
	PromptTokens        *prometheus.CounterVec
	CachedTokens        *prometheus.CounterVec
	CompletionTokens    *prometheus.CounterVec
	ContextSize         *prometheus.HistogramVec
	OutputSize          *prometheus.HistogramVec
	DecodeTPS           *prometheus.HistogramVec
	TPOT                *prometheus.HistogramVec
	RequestBytes        *prometheus.HistogramVec
	ResponseBytes       *prometheus.HistogramVec
	ParseFailures       *prometheus.CounterVec
	FinishReasons       *prometheus.CounterVec
	UpstreamUp          *prometheus.GaugeVec
	UpstreamProbeTime   *prometheus.GaugeVec
	UpstreamProbeErrors *prometheus.CounterVec
	UpstreamErrors      *prometheus.CounterVec
	ClientDisconnects   *prometheus.CounterVec
	LastUpstreamSuccess *prometheus.GaugeVec
	UpstreamRequests    *prometheus.CounterVec

	// router observability
	RouteDecisions    *prometheus.CounterVec
	RouteOverlap      prometheus.Histogram
	RouteAffinity     prometheus.Histogram
	UpstreamInflight  *prometheus.GaugeVec
	UpstreamLoadUnits *prometheus.GaugeVec
}

func New(reg prometheus.Registerer) *Metrics {
	latencyBuckets := []float64{.01, .025, .05, .075, .1, .15, .25, .5, .75, 1, 2.5, 5, 10, 20, 40, 80, 160, 320, 640, 1280, 2560}
	tokenBuckets := []float64{256, 1024, 4096, 8192, 16384, 32768, 65536, 98304, 131072}
	tPSBuckets := []float64{1, 2, 5, 10, 15, 20, 30, 40, 60, 80, 120, 180, 240}
	m := &Metrics{
		Requests:            prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_requests_total", Help: "Completed proxied requests"}, []string{"endpoint", "code", "stream"}),
		Inflight:            prometheus.NewGauge(prometheus.GaugeOpts{Name: "ds4proxy_requests_inflight", Help: "Requests currently in flight"}),
		Duration:            prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_request_duration_seconds", Help: "Full request duration", Buckets: []float64{.5, 1, 2.5, 5, 10, 30, 60, 120, 300, 600, 1800}}, []string{"endpoint"}),
		TTFT:                prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_ttft_seconds", Help: "Time to first generated token or tool-call delta (streaming only)", Buckets: latencyBuckets}, []string{"endpoint"}),
		PromptTokens:        prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_prompt_tokens_total", Help: "Prompt tokens processed"}, []string{"endpoint"}),
		CachedTokens:        prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_cached_prompt_tokens_total", Help: "Prompt tokens served from KV prefix cache"}, []string{"endpoint"}),
		CompletionTokens:    prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_completion_tokens_total", Help: "Tokens generated"}, []string{"endpoint"}),
		ContextSize:         prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_context_tokens", Help: "Per-request prompt size (tokens)", Buckets: tokenBuckets}, []string{"endpoint"}),
		OutputSize:          prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_output_tokens", Help: "Per-request completion size (tokens)", Buckets: []float64{64, 256, 1024, 4096, 8192, 16384, 32768, 65536}}, []string{"endpoint"}),
		DecodeTPS:           prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_decode_tokens_per_second", Help: "Per-request decode throughput (completion tokens / time after first generated token)", Buckets: tPSBuckets}, []string{"endpoint"}),
		TPOT:                prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_time_per_output_token_seconds", Help: "Per-request mean time per output token after the first token", Buckets: []float64{.005, .01, .015, .02, .025, .03, .04, .05, .075, .1, .15, .2, .3, .5, .75, 1, 2.5, 5, 10, 20, 40}}, []string{"endpoint"}),
		RequestBytes:        prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_request_body_bytes", Help: "Request body size", Buckets: []float64{1024, 16384, 131072, 524288, 2097152, 8388608}}, []string{"endpoint"}),
		ResponseBytes:       prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "ds4proxy_response_body_bytes", Help: "Response body size", Buckets: []float64{1024, 16384, 131072, 524288, 2097152, 8388608}}, []string{"endpoint"}),
		ParseFailures:       prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_usage_parse_failures_total", Help: "Responses where no usage block could be extracted"}, []string{"endpoint"}),
		FinishReasons:       prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_finish_reasons_total", Help: "Successful responses by finish reason"}, []string{"endpoint", "reason"}),
		UpstreamUp:          prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "ds4proxy_upstream_up", Help: "Whether the upstream /v1/models readiness probe is succeeding"}, []string{"upstream"}),
		UpstreamProbeTime:   prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "ds4proxy_upstream_probe_duration_seconds", Help: "Duration of the latest upstream readiness probe"}, []string{"upstream"}),
		UpstreamProbeErrors: prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_upstream_probe_failures_total", Help: "Failed upstream readiness probes"}, []string{"upstream", "reason"}),
		UpstreamErrors:      prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_upstream_errors_total", Help: "Proxied requests that failed before receiving a complete upstream response"}, []string{"endpoint", "reason"}),
		ClientDisconnects:   prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_client_disconnects_total", Help: "Requests cancelled because the downstream client disconnected"}, []string{"endpoint"}),
		LastUpstreamSuccess: prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "ds4proxy_last_upstream_success_timestamp_seconds", Help: "Unix timestamp of the latest successful upstream readiness probe"}, []string{"upstream"}),
		UpstreamRequests:    prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_upstream_requests_total", Help: "Requests dispatched per upstream engine"}, []string{"upstream", "code"}),
		RouteDecisions:      prometheus.NewCounterVec(prometheus.CounterOpts{Name: "ds4proxy_route_decisions_total", Help: "Routing decisions by outcome (overlap|load|rr|single)"}, []string{"outcome"}),
		RouteOverlap:        prometheus.NewHistogram(prometheus.HistogramOpts{Name: "ds4proxy_route_overlap_blocks", Help: "Prefix-cache overlap depth of the chosen upstream, in fingerprint blocks", Buckets: []float64{0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024}}),
		RouteAffinity:       prometheus.NewHistogram(prometheus.HistogramOpts{Name: "ds4proxy_route_affinity_blocks", Help: "Bounded prefix overlap contribution used in the route score", Buckets: []float64{0, 1, 2, 4, 8, 16, 32, 64, 128}}),
		UpstreamInflight:    prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "ds4proxy_upstream_inflight", Help: "In-flight requests per upstream"}, []string{"upstream"}),
		UpstreamLoadUnits:   prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "ds4proxy_upstream_load_units", Help: "Size-weighted in-flight work used by the router"}, []string{"upstream"}),
	}
	reg.MustRegister(
		m.Requests, m.Inflight, m.Duration, m.TTFT, m.PromptTokens,
		m.CachedTokens, m.CompletionTokens, m.ContextSize, m.OutputSize,
		m.DecodeTPS, m.TPOT, m.RequestBytes, m.ResponseBytes, m.ParseFailures,
		m.FinishReasons, m.UpstreamUp, m.UpstreamProbeTime,
		m.UpstreamProbeErrors, m.UpstreamErrors, m.ClientDisconnects, m.LastUpstreamSuccess,
		m.UpstreamRequests, m.RouteDecisions, m.RouteOverlap, m.RouteAffinity, m.UpstreamInflight,
		m.UpstreamLoadUnits,
	)
	return m
}
