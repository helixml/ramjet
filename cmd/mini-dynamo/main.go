// mini-dynamo: a KV-cache-locality-aware load balancer for OpenAI-compatible
// inference engines. See DESIGN.md.
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"os/signal"
	"syscall"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/helixml/mini-dynamo/pkg/config"
	"github.com/helixml/mini-dynamo/pkg/metrics"
	"github.com/helixml/mini-dynamo/pkg/proxy"
	"github.com/helixml/mini-dynamo/pkg/router"
)

func run(ctx context.Context, cfg config.Config) error {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.MaxIdleConns = 512
	transport.MaxIdleConnsPerHost = 256
	transport.MaxConnsPerHost = 256
	transport.DisableCompression = true
	transport.DialContext = (&net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}).DialContext
	client := &http.Client{Transport: transport}

	registry := prometheus.NewRegistry()
	m := metrics.New(registry)
	routerConfig := router.Config{
		Upstreams:        cfg.Upstreams,
		Alpha:            cfg.RouteAlpha,
		ChunkBytes:       cfg.RouteChunkBytes,
		MaxPrefixBytes:   cfg.RouteMaxPrefixBytes,
		MaxOverlapBlocks: cfg.RouteMaxOverlapBlocks,
		IndexCapacity:    cfg.RouteIndexCapacity,
		LoadUnitBytes:    cfg.RouteLoadUnitBytes,
		MaxLoadUnits:     cfg.RouteMaxLoadUnits,
		AffinityEnabled:  cfg.Affinity == "prefix",
	}
	p := proxy.New(cfg, client, m, router.New(routerConfig))

	proxyServer := &http.Server{
		Addr:              ":8000",
		Handler:           p,
		ReadHeaderTimeout: 10 * time.Second,
		MaxHeaderBytes:    1 << 20,
	}
	metricsMux := http.NewServeMux()
	metricsMux.Handle("GET /metrics", promhttp.HandlerFor(registry, promhttp.HandlerOpts{}))
	metricsMux.HandleFunc("GET /metrics/upstream/{index}", p.UpstreamMetrics)
	metricsServer := &http.Server{
		Addr:              ":9090",
		Handler:           metricsMux,
		ReadHeaderTimeout: 10 * time.Second,
		MaxHeaderBytes:    1 << 20,
	}

	serverErrors := make(chan error, 2)
	go func() {
		if err := proxyServer.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			serverErrors <- fmt.Errorf("API listener: %w", err)
		}
	}()
	go func() {
		if err := metricsServer.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			serverErrors <- fmt.Errorf("metrics listener: %w", err)
		}
	}()
	go p.ProbeLoop(ctx)
	log.Printf("mini-dynamo up: :8000 -> %v (affinity=%s alpha=%.1f max_prefix_bytes=%d max_overlap_blocks=%d load_unit_bytes=%d max_load_units=%d journal=%t), /metrics on :9090",
		cfg.Upstreams, cfg.Affinity, cfg.RouteAlpha, cfg.RouteMaxPrefixBytes, cfg.RouteMaxOverlapBlocks, cfg.RouteLoadUnitBytes, cfg.RouteMaxLoadUnits, cfg.RouteJournal)

	select {
	case <-ctx.Done():
	case err := <-serverErrors:
		return err
	}
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := proxyServer.Shutdown(shutdownCtx); err != nil {
		return err
	}
	return metricsServer.Shutdown(shutdownCtx)
}

func main() {
	cfg, err := config.Load()
	if err != nil {
		log.Fatal(err)
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, cfg); err != nil {
		log.Fatal(err)
	}
}
