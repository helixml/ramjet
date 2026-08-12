// Package config loads mini-dynamo configuration from the environment.
// Env names keep the DS4_ prefix for drop-in compatibility with the
// ds4-loadbalancer deployment it replaces.
package config

import (
	"errors"
	"fmt"
	"math"
	"net/url"
	"os"
	"strconv"
	"strings"
)

type Config struct {
	Upstreams          []string
	UpstreamToken      string
	MaxTokensStrip     int64
	AdvertiseCtxMargin int64

	// Router tuning — see internal/router.
	RouteAlpha            float64
	RouteChunkBytes       int
	RouteMaxPrefixBytes   int
	RouteMaxOverlapBlocks int
	RouteIndexCapacity    int
	RouteLoadUnitBytes    int
	RouteMaxLoadUnits     int
	Affinity              string // "prefix" (default) | "load"
	RouteJournal          bool
}

func Load() (Config, error) {
	raw := envOr("DS4_UPSTREAM", "http://ds4-flash:8000")
	var upstreams []string
	for _, item := range strings.Split(raw, ",") {
		item = strings.TrimRight(strings.TrimSpace(item), "/")
		if item == "" {
			continue
		}
		u, err := url.Parse(item)
		if err != nil || u.Scheme == "" || u.Host == "" {
			return Config{}, fmt.Errorf("invalid DS4_UPSTREAM entry %q", item)
		}
		upstreams = append(upstreams, item)
	}
	if len(upstreams) == 0 {
		return Config{}, errors.New("DS4_UPSTREAM contains no upstreams")
	}

	strip, err := envInt64("DS4_MAX_TOKENS_STRIP", 100000)
	if err != nil {
		return Config{}, err
	}
	margin, err := envInt64("DS4_ADVERTISE_CTX_MARGIN", 16384)
	if err != nil {
		return Config{}, err
	}
	alpha, err := envFloat("DS4_ROUTE_ALPHA", 4)
	if err != nil {
		return Config{}, err
	}
	chunk, err := envInt64("DS4_ROUTE_CHUNK_BYTES", 2048)
	if err != nil {
		return Config{}, err
	}
	maxPrefix, err := envInt64("DS4_ROUTE_MAX_PREFIX_BYTES", 2<<20)
	if err != nil {
		return Config{}, err
	}
	maxOverlap, err := envInt64("DS4_ROUTE_MAX_OVERLAP_BLOCKS", 32)
	if err != nil {
		return Config{}, err
	}
	capacity, err := envInt64("DS4_ROUTE_INDEX_CAPACITY", 100_000)
	if err != nil {
		return Config{}, err
	}
	loadUnitBytes, err := envInt64("DS4_ROUTE_LOAD_UNIT_BYTES", 32<<10)
	if err != nil {
		return Config{}, err
	}
	maxLoadUnits, err := envInt64("DS4_ROUTE_MAX_LOAD_UNITS", 8)
	if err != nil {
		return Config{}, err
	}
	if alpha < 0 || math.IsNaN(alpha) || math.IsInf(alpha, 0) {
		return Config{}, errors.New("DS4_ROUTE_ALPHA must be finite and non-negative")
	}
	if chunk <= 0 || maxPrefix <= 0 || maxOverlap <= 0 || capacity <= 0 || loadUnitBytes <= 0 || maxLoadUnits <= 0 {
		return Config{}, errors.New("DS4 route byte, block, capacity, and load settings must be positive")
	}
	affinity := envOr("DS4_AFFINITY", "prefix")
	if affinity != "prefix" && affinity != "load" {
		return Config{}, fmt.Errorf("invalid DS4_AFFINITY %q (want prefix|load)", affinity)
	}
	journal, err := envBool("DS4_ROUTE_JOURNAL", false)
	if err != nil {
		return Config{}, err
	}

	return Config{
		Upstreams:             upstreams,
		UpstreamToken:         os.Getenv("DS4_UPSTREAM_TOKEN"),
		MaxTokensStrip:        strip,
		AdvertiseCtxMargin:    margin,
		RouteAlpha:            alpha,
		RouteChunkBytes:       int(chunk),
		RouteMaxPrefixBytes:   int(maxPrefix),
		RouteMaxOverlapBlocks: int(maxOverlap),
		RouteIndexCapacity:    int(capacity),
		RouteLoadUnitBytes:    int(loadUnitBytes),
		RouteMaxLoadUnits:     int(maxLoadUnits),
		Affinity:              affinity,
		RouteJournal:          journal,
	}, nil
}

func envBool(key string, fallback bool) (bool, error) {
	raw := os.Getenv(key)
	if raw == "" {
		return fallback, nil
	}
	value, err := strconv.ParseBool(raw)
	if err != nil {
		return false, fmt.Errorf("invalid %s=%q: %w", key, raw, err)
	}
	return value, nil
}

func envOr(key, fallback string) string {
	if value := os.Getenv(key); value != "" {
		return value
	}
	return fallback
}

func envInt64(key string, fallback int64) (int64, error) {
	raw := os.Getenv(key)
	if raw == "" {
		return fallback, nil
	}
	value, err := strconv.ParseInt(raw, 10, 64)
	if err != nil {
		return 0, fmt.Errorf("invalid %s=%q: %w", key, raw, err)
	}
	return value, nil
}

func envFloat(key string, fallback float64) (float64, error) {
	raw := os.Getenv(key)
	if raw == "" {
		return fallback, nil
	}
	value, err := strconv.ParseFloat(raw, 64)
	if err != nil {
		return 0, fmt.Errorf("invalid %s=%q: %w", key, raw, err)
	}
	return value, nil
}
