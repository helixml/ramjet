package config

import (
	"strings"
	"testing"
)

func TestRouterDefaults(t *testing.T) {
	t.Setenv("DS4_UPSTREAM", "http://engine:8000")
	cfg, err := Load()
	if err != nil {
		t.Fatal(err)
	}
	if cfg.RouteMaxPrefixBytes != 2<<20 {
		t.Fatalf("max prefix bytes = %d, want %d", cfg.RouteMaxPrefixBytes, 2<<20)
	}
	if cfg.RouteMaxOverlapBlocks != 32 {
		t.Fatalf("max overlap blocks = %d, want 32", cfg.RouteMaxOverlapBlocks)
	}
}

func TestRouterSettingsMustBePositive(t *testing.T) {
	t.Setenv("DS4_UPSTREAM", "http://engine:8000")
	for _, key := range []string{
		"DS4_ROUTE_CHUNK_BYTES",
		"DS4_ROUTE_MAX_PREFIX_BYTES",
		"DS4_ROUTE_MAX_OVERLAP_BLOCKS",
		"DS4_ROUTE_INDEX_CAPACITY",
		"DS4_ROUTE_LOAD_UNIT_BYTES",
		"DS4_ROUTE_MAX_LOAD_UNITS",
	} {
		t.Run(key, func(t *testing.T) {
			t.Setenv(key, "0")
			_, err := Load()
			if err == nil || !strings.Contains(err.Error(), "must be positive") {
				t.Fatalf("Load() error = %v, want positive-setting error", err)
			}
		})
	}
}

func TestRouterAlphaMustBeFiniteAndNonNegative(t *testing.T) {
	t.Setenv("DS4_UPSTREAM", "http://engine:8000")
	for _, value := range []string{"-1", "NaN", "+Inf"} {
		t.Run(value, func(t *testing.T) {
			t.Setenv("DS4_ROUTE_ALPHA", value)
			_, err := Load()
			if err == nil || !strings.Contains(err.Error(), "finite and non-negative") {
				t.Fatalf("Load() error = %v", err)
			}
		})
	}
}

func TestRouteJournalBoolean(t *testing.T) {
	t.Setenv("DS4_UPSTREAM", "http://engine:8000")
	t.Setenv("DS4_ROUTE_JOURNAL", "true")
	cfg, err := Load()
	if err != nil {
		t.Fatal(err)
	}
	if !cfg.RouteJournal {
		t.Fatal("route journal should be enabled")
	}
	t.Setenv("DS4_ROUTE_JOURNAL", "sometimes")
	if _, err := Load(); err == nil || !strings.Contains(err.Error(), "DS4_ROUTE_JOURNAL") {
		t.Fatalf("invalid journal boolean error = %v", err)
	}
}
