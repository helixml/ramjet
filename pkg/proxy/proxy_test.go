package proxy

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"testing"
	"time"

	"github.com/helixml/mini-dynamo/pkg/config"
	"github.com/helixml/mini-dynamo/pkg/router"
	"github.com/helixml/mini-dynamo/pkg/shims"
	"github.com/helixml/mini-dynamo/pkg/usage"
)

var benchmarkDecision router.Decision
var benchmarkFingerprints []uint64

func BenchmarkPrepareLongPrompt(b *testing.B) {
	for _, targetBytes := range []int{256 << 10, 2 << 20} {
		b.Run(fmt.Sprintf("bytes-%d", targetBytes), func(b *testing.B) {
			body := []byte(fmt.Sprintf(
				`{"messages":[{"role":"system","content":%q},{"role":"user","content":"summarize"}],"max_tokens":256}`,
				strings.Repeat("long-context-ledger-", targetBytes/20),
			))
			cfg := router.DefaultConfig([]string{"http://engine:8000"})
			r := router.New(cfg)
			b.ReportAllocs()
			b.SetBytes(int64(len(body)))
			b.ResetTimer()
			for range b.N {
				sanitized := shims.SanitizeRequestBody("chat", body, 100_000)
				benchmarkDecision = r.Route(sanitized)
				benchmarkFingerprints = r.Fingerprints(sanitized)
			}
		})
	}
}

func TestUpstreamIndexIsOpaqueAndStable(t *testing.T) {
	p := &Proxy{cfg: config.Config{Upstreams: []string{"http://engine-a:8000", "http://engine-b:8000"}}}
	if got := p.upstreamIndex("http://engine-a:8000"); got != 0 {
		t.Fatalf("first upstream index = %d, want 0", got)
	}
	if got := p.upstreamIndex("http://engine-b:8000"); got != 1 {
		t.Fatalf("second upstream index = %d, want 1", got)
	}
	if got := p.upstreamIndex("http://unknown:8000"); got != -1 {
		t.Fatalf("unknown upstream index = %d, want -1", got)
	}
}

func TestRouteJournalFinishSeparatesFirstByteFromFirstToken(t *testing.T) {
	started := time.Unix(10, 0)
	firstByte := started.Add(100 * time.Millisecond)
	firstToken := started.Add(350 * time.Millisecond)
	prompt, cached, completion := 100.0, 75.0, 10.0
	record := makeRouteFinishRecord(
		7, started.Add(time.Second), started, firstByte, firstToken,
		"complete", 1, 200, 2048,
		&usage.Accumulator{Prompt: &prompt, Cached: &cached, Completion: &completion},
	)
	if routeJournalVersion != 3 {
		t.Fatal("journal v3 must be active")
	}
	if *record.FirstByteMS != 100 || *record.TTFTMS != 350 {
		t.Fatalf("journal timing fields collapsed: %+v", record)
	}
	if record.DurationMS != 1000 || record.PromptTokens == nil || *record.PromptTokens != 100 {
		t.Fatalf("journal finish record incomplete: %+v", record)
	}
}

func TestUpstreamOrdinalHeaderCannotBeSpoofed(t *testing.T) {
	destination := http.Header{"X-Mini-Dynamo-Upstream": []string{"1"}}
	source := http.Header{"X-Mini-Dynamo-Upstream": []string{"secret-engine"}}
	copyResponseHeaders(destination, source)
	if got := destination.Values("X-Mini-Dynamo-Upstream"); len(got) != 1 || got[0] != "1" {
		t.Fatalf("upstream ordinal header = %v, want only proxy value", got)
	}

	forwarded := http.Header{}
	copyRequestHeaders(forwarded, source)
	if got := forwarded.Values("X-Mini-Dynamo-Upstream"); len(got) != 0 {
		t.Fatalf("client route header was forwarded upstream: %v", got)
	}
}

func TestRouteJournalStartIsPrivacyBounded(t *testing.T) {
	cfg := config.Config{
		Upstreams:             []string{"http://secret-engine-a:8000", "http://secret-engine-b:8000"},
		RouteAlpha:            4,
		RouteChunkBytes:       2048,
		RouteMaxOverlapBlocks: 32,
		RouteLoadUnitBytes:    32 << 10,
		RouteMaxLoadUnits:     8,
	}
	decision := router.Decision{
		Candidates:  cfg.Upstreams,
		TotalBlocks: 760,
		Rotation:    1,
		Outcome:     "overlap",
		CandidateState: []router.CandidateState{
			{Index: 1, Rank: 0, OverlapBlocks: 760, AffinityBlocks: 32, LoadUnits: 0, RequestLoadUnits: 1, Healthy: true},
			{Index: 0, Rank: 1, OverlapBlocks: 128, AffinityBlocks: 32, LoadUnits: 2, RequestLoadUnits: 8, Healthy: true},
		},
	}
	record := makeRouteStartRecord(42, time.UnixMilli(1234), "chat", 1_555_943, decision, cfg)
	encoded, err := json.Marshal(record)
	if err != nil {
		t.Fatal(err)
	}
	text := string(encoded)
	for _, forbidden := range []string{"secret-engine", "http://", "prompt", "fingerprint"} {
		if strings.Contains(text, forbidden) {
			t.Fatalf("journal leaked %q: %s", forbidden, text)
		}
	}
	if record.Chosen != 1 || len(record.Candidates) != 2 || record.Candidates[0].OverlapBlocks != 760 {
		t.Fatalf("journal lost replay state: %+v", record)
	}
}
