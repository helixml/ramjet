// Package proxy is the HTTP data plane: it streams requests to the routed
// engine, applies request shims, extracts usage into metrics, rewrites
// advertised model metadata, probes upstream health, and passes through
// engine-native metrics.
package proxy

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/helixml/mini-dynamo/pkg/config"
	"github.com/helixml/mini-dynamo/pkg/metrics"
	"github.com/helixml/mini-dynamo/pkg/router"
	"github.com/helixml/mini-dynamo/pkg/shims"
	"github.com/helixml/mini-dynamo/pkg/usage"
)

const maxRequestBody = 64 << 20

var hopHeaders = map[string]struct{}{
	"connection": {}, "keep-alive": {}, "proxy-authenticate": {},
	"proxy-authorization": {}, "te": {}, "trailers": {},
	"transfer-encoding": {}, "upgrade": {}, "content-length": {}, "host": {},
	"x-mini-dynamo-upstream": {},
}

type Proxy struct {
	cfg     config.Config
	client  *http.Client
	metrics *metrics.Metrics
	router  *router.Router
	journal *routeJournal
}

func New(cfg config.Config, client *http.Client, m *metrics.Metrics, r *router.Router) *Proxy {
	return &Proxy{cfg: cfg, client: client, metrics: m, router: r, journal: &routeJournal{enabled: cfg.RouteJournal}}
}

func (p *Proxy) Router() *router.Router { return p.router }

func (p *Proxy) ServeHTTP(writer http.ResponseWriter, request *http.Request) {
	started := time.Now()
	endpoint := shims.EndpointLabel(request.URL.Path)
	body, err := io.ReadAll(http.MaxBytesReader(writer, request.Body, maxRequestBody))
	if err != nil {
		if request.Context().Err() != nil {
			p.recordClientDisconnect(endpoint, "", 0)
			return
		}
		http.Error(writer, "request body too large or unreadable", http.StatusRequestEntityTooLarge)
		return
	}
	body = shims.SanitizeRequestBody(endpoint, body, p.cfg.MaxTokensStrip)
	p.metrics.RequestBytes.WithLabelValues(endpoint).Observe(float64(len(body)))
	p.metrics.Inflight.Inc()
	defer p.metrics.Inflight.Dec()

	decision := p.router.Route(body)
	p.metrics.RouteDecisions.WithLabelValues(decision.Outcome).Inc()
	if decision.TotalBlocks > 0 {
		p.metrics.RouteOverlap.Observe(float64(decision.OverlapBlocks))
		p.metrics.RouteAffinity.Observe(float64(decision.AffinityBlocks))
	}
	var fingerprints []uint64
	if endpoint != "other" {
		fingerprints = p.router.Fingerprints(body)
	}

	target := decision.Candidates[0]
	accumulator := &usage.Accumulator{}
	var firstByteAt, firstTokenAt time.Time
	var bytesOut int64
	journalResult := "incomplete"
	journalStatus := 0
	journalUpstream := p.upstreamIndex(target)
	journalID := p.journal.start(endpoint, len(body), decision, p.cfg)
	defer func() {
		p.journal.finish(journalID, started, firstByteAt, firstTokenAt, journalResult, journalUpstream, journalStatus, bytesOut, accumulator)
	}()
	var response *http.Response
	var release func()
	var lastError error
	for idx, candidate := range decision.Candidates {
		outbound, err := p.outboundRequest(request, candidate, body)
		if err != nil {
			lastError = err
			p.router.SetHealthy(candidate, false)
			continue
		}
		requestLoadUnits := decision.LoadUnits
		if idx < len(decision.CandidateState) {
			requestLoadUnits = decision.CandidateState[idx].RequestLoadUnits
		}
		candidateRelease := p.acquire(candidate, requestLoadUnits)
		response, err = p.client.Do(outbound)
		if err != nil {
			candidateRelease()
			if request.Context().Err() != nil {
				journalResult = "client_disconnect"
				p.recordClientDisconnect(endpoint, "", 0)
				return
			}
			lastError = err
			p.router.SetHealthy(candidate, false)
			continue
		}
		if (response.StatusCode == http.StatusBadGateway || response.StatusCode == http.StatusServiceUnavailable) && idx+1 < len(decision.Candidates) {
			response.Body.Close()
			candidateRelease()
			p.router.SetHealthy(candidate, false)
			p.metrics.UpstreamRequests.WithLabelValues(candidate, strconv.Itoa(response.StatusCode)).Inc()
			response = nil
			continue
		}
		target = candidate
		journalUpstream = p.upstreamIndex(target)
		journalStatus = response.StatusCode
		release = candidateRelease
		if idx > 0 {
			log.Printf("[failover] %s unavailable, using %s", decision.Candidates[0], candidate)
		}
		break
	}
	if response == nil {
		journalResult = "upstream_error"
		journalStatus = http.StatusBadGateway
		if upstreamErrorReason(lastError) == "timeout" {
			journalStatus = http.StatusGatewayTimeout
		}
		p.writeUpstreamError(writer, endpoint, target, started, lastError, false)
		return
	}
	defer response.Body.Close()
	defer release()
	// An opaque ordinal lets benchmarks correlate their own requests with route
	// decisions without scraping global metrics or exposing internal hostnames.
	writer.Header().Set("X-Mini-Dynamo-Upstream", strconv.Itoa(p.upstreamIndex(target)))

	if request.Method == http.MethodGet && strings.HasSuffix(strings.TrimRight(request.URL.Path, "/"), "/v1/models") && response.StatusCode == http.StatusOK {
		raw, err := io.ReadAll(response.Body)
		if err != nil {
			if request.Context().Err() != nil {
				journalResult = "client_disconnect"
				p.recordClientDisconnect(endpoint, target, response.StatusCode)
				return
			}
			journalResult = "upstream_read_error"
			p.writeUpstreamError(writer, endpoint, target, started, err, false)
			return
		}
		result := shims.ShrinkAdvertisedContext(raw, p.cfg.AdvertiseCtxMargin)
		writer.Header().Set("Content-Type", "application/json")
		writer.WriteHeader(http.StatusOK)
		_, _ = writer.Write(result)
		journalResult = "complete"
		p.recordRequest(endpoint, http.StatusOK, "false", started)
		p.metrics.UpstreamRequests.WithLabelValues(target, "200").Inc()
		return
	}

	copyResponseHeaders(writer.Header(), response.Header)
	writer.WriteHeader(response.StatusCode)
	stream := strings.Contains(response.Header.Get("Content-Type"), "text/event-stream")
	streamLabel := strconv.FormatBool(stream)
	var parseBuffer []byte
	buffer := make([]byte, 32<<10)
	for {
		count, readError := response.Body.Read(buffer)
		if count > 0 {
			receivedAt := time.Now()
			if firstByteAt.IsZero() {
				firstByteAt = receivedAt
			}
			chunk := buffer[:count]
			written, writeError := writer.Write(chunk)
			bytesOut += int64(written)
			if stream {
				parseBuffer = usage.FeedSSEChunk(accumulator, parseBuffer, chunk)
				if firstTokenAt.IsZero() && accumulator.Generated {
					firstTokenAt = receivedAt
				}
			} else {
				parseBuffer = append(parseBuffer, chunk...)
			}
			if writeError != nil {
				journalResult = "client_disconnect"
				p.recordClientDisconnect(endpoint, target, response.StatusCode)
				return
			}
			if stream {
				if flushError := http.NewResponseController(writer).Flush(); flushError != nil {
					journalResult = "client_disconnect"
					p.recordClientDisconnect(endpoint, target, response.StatusCode)
					return
				}
			}
		}
		if readError == io.EOF {
			break
		}
		if readError != nil {
			if request.Context().Err() != nil {
				journalResult = "client_disconnect"
				p.recordClientDisconnect(endpoint, target, response.StatusCode)
				return
			}
			journalResult = "upstream_read_error"
			p.writeUpstreamError(writer, endpoint, target, started, readError, true)
			return
		}
	}
	if stream && len(parseBuffer) > 0 {
		accumulator.FeedSSELine(parseBuffer)
	} else if !stream && len(parseBuffer) > 0 {
		accumulator.FeedJSON(parseBuffer)
	}

	elapsed := time.Since(started)
	if response.StatusCode >= 400 {
		log.Printf("[upstream %d] %s %s\n  request body: %.2000q\n  response body: %.2000q", response.StatusCode, request.Method, request.URL.RequestURI(), body, parseBuffer)
	} else if endpoint == "chat" {
		log.Printf("[chat %d] stream=%t %.1fs upstream=%d finish=%s overlap=%d/%d affinity=%d load_units=%d outcome=%s content_chars=%d reasoning_chars=%d tool_call_deltas=%d prompt_toks=%s completion_toks=%s req_bytes=%d",
			response.StatusCode, stream, elapsed.Seconds(), p.upstreamIndex(target), accumulator.FinishReason,
			decision.OverlapBlocks, decision.TotalBlocks, decision.AffinityBlocks, decision.LoadUnits, decision.Outcome,
			accumulator.ContentChars, accumulator.ReasoningChars, accumulator.ToolCallDeltas,
			formatOptionalNumber(accumulator.Prompt), formatOptionalNumber(accumulator.Completion), len(body))
	}
	p.recordRequest(endpoint, response.StatusCode, streamLabel, started)
	p.metrics.ResponseBytes.WithLabelValues(endpoint).Observe(float64(bytesOut))
	if stream && !firstTokenAt.IsZero() {
		p.metrics.TTFT.WithLabelValues(endpoint).Observe(firstTokenAt.Sub(started).Seconds())
	}
	if response.StatusCode == http.StatusOK && endpoint != "other" {
		p.recordUsage(endpoint, accumulator, elapsed, started, firstTokenAt)
		p.router.Observe(target, fingerprints)
	}
	p.metrics.UpstreamRequests.WithLabelValues(target, strconv.Itoa(response.StatusCode)).Inc()
	journalResult = "complete"
}

func (p *Proxy) upstreamIndex(target string) int {
	for index, upstream := range p.cfg.Upstreams {
		if upstream == target {
			return index
		}
	}
	return -1
}

func (p *Proxy) acquire(upstream string, loadUnits int) func() {
	release := p.router.AcquireWeighted(upstream, loadUnits)
	p.metrics.UpstreamInflight.WithLabelValues(upstream).Set(float64(p.router.Inflight(upstream)))
	p.metrics.UpstreamLoadUnits.WithLabelValues(upstream).Set(float64(p.router.LoadUnits(upstream)))
	return func() {
		release()
		p.metrics.UpstreamInflight.WithLabelValues(upstream).Set(float64(p.router.Inflight(upstream)))
		p.metrics.UpstreamLoadUnits.WithLabelValues(upstream).Set(float64(p.router.LoadUnits(upstream)))
	}
}

func (p *Proxy) recordClientDisconnect(endpoint, target string, upstreamStatus int) {
	p.metrics.ClientDisconnects.WithLabelValues(endpoint).Inc()
	if target != "" && upstreamStatus != 0 {
		p.metrics.UpstreamRequests.WithLabelValues(target, strconv.Itoa(upstreamStatus)).Inc()
	}
}

func (p *Proxy) outboundRequest(inbound *http.Request, upstream string, body []byte) (*http.Request, error) {
	request, err := http.NewRequestWithContext(inbound.Context(), inbound.Method, upstream+inbound.URL.RequestURI(), bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	copyRequestHeaders(request.Header, inbound.Header)
	return request, nil
}

func copyRequestHeaders(destination, source http.Header) {
	for key, values := range source {
		if _, skip := hopHeaders[strings.ToLower(key)]; skip {
			continue
		}
		for _, value := range values {
			destination.Add(key, value)
		}
	}
}

func copyResponseHeaders(destination, source http.Header) {
	for key, values := range source {
		if _, skip := hopHeaders[strings.ToLower(key)]; skip {
			continue
		}
		for _, value := range values {
			destination.Add(key, value)
		}
	}
}

func (p *Proxy) recordRequest(endpoint string, code int, stream string, started time.Time) {
	p.metrics.Requests.WithLabelValues(endpoint, strconv.Itoa(code), stream).Inc()
	p.metrics.Duration.WithLabelValues(endpoint).Observe(time.Since(started).Seconds())
}

func (p *Proxy) recordUsage(endpoint string, accumulator *usage.Accumulator, elapsed time.Duration, started, firstTokenAt time.Time) {
	if accumulator.Prompt == nil && accumulator.Completion == nil {
		p.metrics.ParseFailures.WithLabelValues(endpoint).Inc()
	}
	if accumulator.Prompt != nil {
		p.metrics.PromptTokens.WithLabelValues(endpoint).Add(*accumulator.Prompt)
		p.metrics.ContextSize.WithLabelValues(endpoint).Observe(*accumulator.Prompt)
	}
	if accumulator.Cached != nil {
		p.metrics.CachedTokens.WithLabelValues(endpoint).Add(*accumulator.Cached)
	}
	if accumulator.Completion != nil {
		p.metrics.CompletionTokens.WithLabelValues(endpoint).Add(*accumulator.Completion)
		p.metrics.OutputSize.WithLabelValues(endpoint).Observe(*accumulator.Completion)
		decodeTime := elapsed
		if !firstTokenAt.IsZero() {
			decodeTime -= firstTokenAt.Sub(started)
		}
		if decodeTime > 500*time.Millisecond && *accumulator.Completion > 8 {
			p.metrics.DecodeTPS.WithLabelValues(endpoint).Observe(*accumulator.Completion / decodeTime.Seconds())
			p.metrics.TPOT.WithLabelValues(endpoint).Observe(decodeTime.Seconds() / max(*accumulator.Completion-1, 1))
		}
	}
	p.metrics.FinishReasons.WithLabelValues(endpoint, shims.FinishReasonLabel(accumulator.FinishReason)).Inc()
}

func (p *Proxy) writeUpstreamError(writer http.ResponseWriter, endpoint, target string, started time.Time, err error, responseStarted bool) {
	reason := upstreamErrorReason(err)
	code := http.StatusBadGateway
	if reason == "timeout" {
		code = http.StatusGatewayTimeout
	}
	p.metrics.UpstreamErrors.WithLabelValues(endpoint, reason).Inc()
	p.metrics.UpstreamRequests.WithLabelValues(target, strconv.Itoa(code)).Inc()
	p.recordRequest(endpoint, code, "unknown", started)
	if responseStarted {
		return
	}
	writer.Header().Set("Content-Type", "application/json")
	writer.WriteHeader(code)
	_, _ = fmt.Fprintf(writer, `{"error":{"message":"upstream unavailable","type":%q}}`, reason)
}

func upstreamErrorReason(err error) string {
	if err == nil {
		return "protocol"
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return "timeout"
	}
	var netError net.Error
	if errors.As(err, &netError) && netError.Timeout() {
		return "timeout"
	}
	var dnsError *net.DNSError
	var operationError *net.OpError
	if errors.As(err, &dnsError) || errors.As(err, &operationError) {
		return "connect"
	}
	return "protocol"
}

func formatOptionalNumber(value *float64) string {
	if value == nil {
		return "None"
	}
	return strconv.FormatFloat(*value, 'f', -1, 64)
}

// ProbeLoop keeps upstream health fresh even without traffic.
func (p *Proxy) ProbeLoop(ctx context.Context) {
	ticker := time.NewTicker(15 * time.Second)
	defer ticker.Stop()
	for {
		p.probeAll(ctx)
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

func (p *Proxy) probeAll(ctx context.Context) {
	for _, upstream := range p.cfg.Upstreams {
		started := time.Now()
		probeCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
		request, err := http.NewRequestWithContext(probeCtx, http.MethodGet, upstream+"/v1/models", nil)
		if err == nil && p.cfg.UpstreamToken != "" {
			request.Header.Set("Authorization", "Bearer "+p.cfg.UpstreamToken)
		}
		var response *http.Response
		if err == nil {
			response, err = p.client.Do(request)
		}
		if err != nil {
			p.markProbe(upstream, false, upstreamErrorReason(err))
		} else {
			_, readErr := io.Copy(io.Discard, response.Body)
			response.Body.Close()
			switch {
			case response.StatusCode != http.StatusOK:
				p.markProbe(upstream, false, "http")
			case readErr != nil:
				p.markProbe(upstream, false, upstreamErrorReason(readErr))
			default:
				p.markProbe(upstream, true, "")
			}
		}
		cancel()
		p.metrics.UpstreamProbeTime.WithLabelValues(upstream).Set(time.Since(started).Seconds())
	}
}

func (p *Proxy) markProbe(upstream string, healthy bool, reason string) {
	p.router.SetHealthy(upstream, healthy)
	if healthy {
		p.metrics.UpstreamUp.WithLabelValues(upstream).Set(1)
		p.metrics.LastUpstreamSuccess.WithLabelValues(upstream).SetToCurrentTime()
		return
	}
	p.metrics.UpstreamUp.WithLabelValues(upstream).Set(0)
	p.metrics.UpstreamProbeErrors.WithLabelValues(upstream, reason).Inc()
}

// UpstreamMetrics passes through an engine's native Prometheus text.
func (p *Proxy) UpstreamMetrics(writer http.ResponseWriter, request *http.Request) {
	rawIndex := strings.TrimPrefix(request.URL.Path, "/metrics/upstream/")
	index, err := strconv.Atoi(rawIndex)
	if err != nil || index < 0 || index >= len(p.cfg.Upstreams) {
		http.Error(writer, "no such upstream", http.StatusNotFound)
		return
	}
	ctx, cancel := context.WithTimeout(request.Context(), 10*time.Second)
	defer cancel()
	upstreamRequest, err := http.NewRequestWithContext(ctx, http.MethodGet, p.cfg.Upstreams[index]+"/metrics", nil)
	if err == nil {
		upstreamRequest.Header.Set("Accept-Encoding", "identity")
		var response *http.Response
		response, err = p.client.Do(upstreamRequest)
		if err == nil {
			defer response.Body.Close()
			writer.Header().Set("Content-Type", "text/plain")
			writer.WriteHeader(response.StatusCode)
			_, _ = io.Copy(writer, response.Body)
			return
		}
	}
	http.Error(writer, "upstream metrics unavailable", http.StatusBadGateway)
}
