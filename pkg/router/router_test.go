package router

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

func chatBody(t *testing.T, system, user string) []byte {
	t.Helper()
	body, err := json.Marshal(map[string]any{
		"model": "m",
		"messages": []map[string]string{
			{"role": "system", "content": system},
			{"role": "user", "content": user},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	return body
}

func testConfig() Config {
	cfg := DefaultConfig([]string{"http://a:8000", "http://b:8000"})
	cfg.ChunkBytes = 64 // small blocks so short test prompts span many
	return cfg
}

func TestChainFingerprintsPrefixProperty(t *testing.T) {
	a := chainFingerprints([]byte(strings.Repeat("x", 300)+"SUFFIX-ONE"), 64)
	b := chainFingerprints([]byte(strings.Repeat("x", 300)+"different-tail-entirely"), 64)
	if len(a) < 3 || len(b) < 3 {
		t.Fatalf("want multiple blocks, got %d and %d", len(a), len(b))
	}
	// shared 300-byte prefix covers blocks 0-3 (last shared block is partial ->
	// diverges); first 4 full blocks are 256 bytes -> blocks 0..3 shared except
	// block 4 boundary. Check at least the fully-shared blocks match.
	shared := 300 / 64 // fully shared full blocks
	for i := 0; i < shared; i++ {
		if a[i] != b[i] {
			t.Fatalf("block %d should match on shared prefix", i)
		}
	}
	if a[len(a)-1] == b[len(b)-1] {
		t.Fatal("tails must differ")
	}
}

func TestRouteStickyConversation(t *testing.T) {
	r := New(testConfig())
	turn1 := chatBody(t, strings.Repeat("SYS", 100), "task Alpha, turn 1")
	d1 := r.Route(turn1)
	first := d1.Candidates[0]
	r.Observe(first, r.Fingerprints(turn1))

	// Next turn extends the same conversation -> deep overlap -> same upstream.
	turn2 := chatBody(t, strings.Repeat("SYS", 100), "task Alpha, turn 1 -- and now turn 2 content")
	d2 := r.Route(turn2)
	if d2.Candidates[0] != first {
		t.Fatalf("conversation should stick to %s, got %s", first, d2.Candidates[0])
	}
	if d2.Outcome != "overlap" || d2.OverlapBlocks == 0 {
		t.Fatalf("expected overlap outcome, got %+v", d2)
	}
}

func TestRouteTemplateSharingCoLocates(t *testing.T) {
	r := New(testConfig())
	sharedSystem := strings.Repeat("You are the Helix coding agent. ", 20) // ~640 bytes = ~10 blocks
	s1 := chatBody(t, sharedSystem, "session one unique task description")
	d1 := r.Route(s1)
	r.Observe(d1.Candidates[0], r.Fingerprints(s1))

	s2 := chatBody(t, sharedSystem, "session two entirely different task")
	d2 := r.Route(s2)
	if d2.Candidates[0] != d1.Candidates[0] {
		t.Fatalf("sessions sharing a system prompt should co-locate: %s vs %s", d1.Candidates[0], d2.Candidates[0])
	}
	if d2.OverlapBlocks == 0 {
		t.Fatal("expected shared-template overlap")
	}
}

func TestRouteLoadOverridesAffinity(t *testing.T) {
	cfg := testConfig()
	cfg.Alpha = 2
	r := New(cfg)
	body := chatBody(t, "sys", "hello")
	d := r.Route(body)
	home := d.Candidates[0]
	r.Observe(home, r.Fingerprints(body))
	// Small overlap (couple of blocks); pile in-flight onto home until load wins.
	for i := 0; i < 4; i++ {
		r.Acquire(home)
	}
	d2 := r.Route(body)
	if d2.Candidates[0] == home {
		t.Fatalf("4 inflight x alpha=2 should override %d-block overlap", d2.OverlapBlocks)
	}
}

func TestRouteColdBigPrefillPicksLeastLoaded(t *testing.T) {
	r := New(testConfig())
	releaseA := r.Acquire("http://a:8000")
	defer releaseA()
	// Brand-new large prompt: no overlap anywhere -> least-loaded (b) wins.
	cold := chatBody(t, strings.Repeat("N", 5000), "fresh")
	d := r.Route(cold)
	if d.Candidates[0] != "http://b:8000" {
		t.Fatalf("cold prefill should go to least-loaded, got %s", d.Candidates[0])
	}
	if d.Outcome != "load" {
		t.Fatalf("expected load outcome, got %s", d.Outcome)
	}
}

func TestUnhealthySortsLast(t *testing.T) {
	r := New(testConfig())
	body := chatBody(t, "sys", "hello")
	d := r.Route(body)
	winner := d.Candidates[0]
	r.Observe(winner, r.Fingerprints(body))
	r.SetHealthy(winner, false)
	d2 := r.Route(body)
	if d2.Candidates[0] == winner {
		t.Fatal("unhealthy upstream must not be first despite overlap")
	}
	if d2.Candidates[len(d2.Candidates)-1] != winner {
		t.Fatal("unhealthy upstream should be last (still a failover candidate)")
	}
}

func TestAffinityDisabled(t *testing.T) {
	cfg := testConfig()
	cfg.AffinityEnabled = false
	r := New(cfg)
	body := chatBody(t, "sys", "hello")
	d := r.Route(body)
	r.Observe(d.Candidates[0], r.Fingerprints(body))
	d2 := r.Route(body)
	if d2.Outcome == "overlap" || d2.OverlapBlocks != 0 {
		t.Fatalf("affinity disabled must not produce overlap decisions: %+v", d2)
	}
}

func TestIndexEviction(t *testing.T) {
	cfg := testConfig()
	cfg.IndexCapacity = 8
	r := New(cfg)
	for i := 0; i < 10; i++ {
		body := chatBody(t, fmt.Sprintf("unique-system-%d-%s", i, strings.Repeat("p", 200)), "u")
		r.Observe("http://a:8000", r.Fingerprints(body))
	}
	r.mu.Lock()
	size := r.states["http://a:8000"].lru.Len()
	indexed := len(r.states["http://a:8000"].index)
	r.mu.Unlock()
	if size != 8 || indexed != 8 {
		t.Fatalf("index must stay at capacity: lru=%d map=%d", size, indexed)
	}
}

func TestColdTrafficSpreads(t *testing.T) {
	r := New(testConfig())
	seen := map[string]int{}
	for i := 0; i < 10; i++ {
		d := r.Route(chatBody(t, fmt.Sprintf("cold-%d", i), "x"))
		seen[d.Candidates[0]]++
	}
	if len(seen) < 2 {
		t.Fatalf("cold round-robin should touch both upstreams: %v", seen)
	}
}
