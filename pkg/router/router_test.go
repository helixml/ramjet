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

func TestLargePrefillReservesCapacityForSmallRequests(t *testing.T) {
	cfg := testConfig()
	cfg.LoadUnitBytes = 128
	cfg.MaxLoadUnits = 4
	r := New(cfg)
	large := chatBody(t, strings.Repeat("large-cold-prefill-", 100), "fresh")
	prefillDecision := r.Route(large)
	if prefillDecision.LoadUnits != 4 {
		t.Fatalf("large prefill load units = %d, want 4", prefillDecision.LoadUnits)
	}
	prefillUpstream := prefillDecision.Candidates[0]
	releasePrefill := r.AcquireWeighted(prefillUpstream, prefillDecision.LoadUnits)
	defer releasePrefill()

	other := "http://a:8000"
	if prefillUpstream == other {
		other = "http://b:8000"
	}
	var releases []func()
	defer func() {
		for _, release := range releases {
			release()
		}
	}()
	for i := 0; i < 3; i++ {
		decision := r.Route(chatBody(t, fmt.Sprintf("small-%d", i), "x"))
		if decision.LoadUnits != 1 {
			t.Fatalf("small request load units = %d, want 1", decision.LoadUnits)
		}
		if decision.Candidates[0] != other {
			t.Fatalf("small request %d routed to reserved prefill engine %s", i, prefillUpstream)
		}
		releases = append(releases, r.AcquireWeighted(other, decision.LoadUnits))
	}
	if got := r.Inflight(prefillUpstream); got != 1 {
		t.Fatalf("inflight requests = %d, want 1", got)
	}
	if got := r.LoadUnits(prefillUpstream); got != 4 {
		t.Fatalf("weighted load = %d, want 4", got)
	}
}

func TestLoadWeightIncludesTopLevelToolSchemas(t *testing.T) {
	cfg := testConfig()
	cfg.LoadUnitBytes = 128
	cfg.MaxLoadUnits = 4
	r := New(cfg)
	body := []byte(fmt.Sprintf(
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"description":%q}]}`,
		strings.Repeat("large tool schema ", 100),
	))
	decision := r.Route(body)
	if decision.LoadUnits != 4 {
		t.Fatalf("tool-heavy body load units = %d, want 4", decision.LoadUnits)
	}
}

func TestFingerprintWindowCoversLateDivergence(t *testing.T) {
	cfg := testConfig()
	cfg.MaxPrefixBytes = 1 << 20
	cfg.MaxOverlapBlocks = 32
	r := New(cfg)
	shared := strings.Repeat("shared-long-prefix-", 20_000)
	first := chatBody(t, shared+"tail-a", "task")
	second := chatBody(t, shared+"tail-b", "task")
	d1 := r.Route(first)
	home := d1.Candidates[0]
	r.Observe(home, r.Fingerprints(first))
	d2 := r.Route(second)
	if d2.TotalBlocks <= (256<<10)/cfg.ChunkBytes {
		t.Fatalf("fingerprint window still stops at 256KB: %d blocks", d2.TotalBlocks)
	}
	if d2.OverlapBlocks == 0 || d2.OverlapBlocks >= d2.TotalBlocks {
		t.Fatalf("late divergence should produce a deep partial match: %+v", d2)
	}
	if d2.AffinityBlocks != cfg.MaxOverlapBlocks {
		t.Fatalf("affinity contribution = %d, want cap %d", d2.AffinityBlocks, cfg.MaxOverlapBlocks)
	}
}

func TestBoundedOverlapAllowsLoadOverride(t *testing.T) {
	cfg := testConfig()
	cfg.Alpha = 1
	cfg.MaxOverlapBlocks = 4
	r := New(cfg)
	body := chatBody(t, strings.Repeat("warm-trunk-", 100), "task")
	d1 := r.Route(body)
	home := d1.Candidates[0]
	r.Observe(home, r.Fingerprints(body))
	for range 5 {
		r.Acquire(home)
	}
	d2 := r.Route(body)
	if d2.Candidates[0] == home {
		t.Fatalf("bounded affinity should yield to five load units: %+v", d2)
	}
	if d2.Outcome != "load" {
		t.Fatalf("winner has no overlap and should report load, got %+v", d2)
	}
}

func TestExactScoreTiePrefersWarmPrefix(t *testing.T) {
	cfg := testConfig()
	cfg.Alpha = 1
	cfg.MaxOverlapBlocks = 4
	r := New(cfg)
	body := chatBody(t, strings.Repeat("warm-trunk-", 100), "task")
	home := r.Route(body).Candidates[0]
	r.Observe(home, r.Fingerprints(body))
	for range 4 {
		r.Acquire(home)
	}
	decision := r.Route(body)
	if decision.AffinityBlocks != 4 {
		t.Fatalf("affinity = %d, want 4", decision.AffinityBlocks)
	}
	if decision.Candidates[0] != home {
		t.Fatalf("exact score tie should retain warm prefix on %s: %+v", home, decision)
	}
}

func TestWarmLargeRequestUsesUncachedLoadEstimate(t *testing.T) {
	cfg := testConfig()
	cfg.LoadUnitBytes = 128
	cfg.MaxLoadUnits = 4
	r := New(cfg)
	body := chatBody(t, strings.Repeat("large-warm-prompt-", 200), "task")
	cold := r.Route(body)
	if cold.LoadUnits != 4 {
		t.Fatalf("cold request load units = %d, want 4", cold.LoadUnits)
	}
	home := cold.Candidates[0]
	r.Observe(home, r.Fingerprints(body))
	warm := r.Route(body)
	if warm.Candidates[0] != home || warm.OverlapBlocks == 0 {
		t.Fatalf("warm request should return to cached engine: %+v", warm)
	}
	if warm.LoadUnits != 1 {
		t.Fatalf("fully cached large request load units = %d, want 1", warm.LoadUnits)
	}
}

func TestDecisionCandidateStateIsStableAndComplete(t *testing.T) {
	cfg := testConfig()
	cfg.LoadUnitBytes = 128
	cfg.MaxLoadUnits = 4
	r := New(cfg)
	body := chatBody(t, strings.Repeat("state-snapshot-", 100), "task")
	r.Observe("http://a:8000", r.Fingerprints(body))
	release := r.AcquireWeighted("http://a:8000", 3)
	defer release()
	decision := r.Route(body)
	if len(decision.CandidateState) != len(cfg.Upstreams) {
		t.Fatalf("candidate states = %d, want %d", len(decision.CandidateState), len(cfg.Upstreams))
	}
	seen := map[int]bool{}
	for rank, state := range decision.CandidateState {
		if state.Rank != rank {
			t.Fatalf("rank = %d, want %d", state.Rank, rank)
		}
		seen[state.Index] = true
		if state.RequestLoadUnits < 1 || state.RequestLoadUnits > cfg.MaxLoadUnits {
			t.Fatalf("candidate request load units out of range: %+v", state)
		}
	}
	if !seen[0] || !seen[1] {
		t.Fatalf("stable upstream ordinals missing: %+v", decision.CandidateState)
	}
}

func TestCanonicalPromptIncludesPromptAffectingFields(t *testing.T) {
	cfg := testConfig()
	r := New(cfg)
	base := []byte(`{"messages":[{"role":"assistant","name":"worker","content":"","reasoning":"think","tool_calls":[{"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{}"}}]}],"tools":[{"type":"function","function":{"name":"lookup","description":"first"}}],"temperature":0}`)
	reordered := []byte(`{"temperature":1,"tools":[{"function":{"description":"first","name":"lookup"},"type":"function"}],"messages":[{"tool_calls":[{"function":{"arguments":"{}","name":"lookup"},"type":"function","id":"call-1"}],"reasoning":"think","content":"","name":"worker","role":"assistant"}]}`)
	if fmt.Sprint(r.Fingerprints(base)) != fmt.Sprint(r.Fingerprints(reordered)) {
		t.Fatal("JSON key order and generation-only temperature must not change fingerprints")
	}
	variants := [][]byte{
		[]byte(strings.Replace(string(base), `"description":"first"`, `"description":"second"`, 1)),
		[]byte(strings.Replace(string(base), `"reasoning":"think"`, `"reasoning":"different"`, 1)),
		[]byte(strings.Replace(string(base), `"name":"worker"`, `"name":"other"`, 1)),
		[]byte(strings.Replace(string(base), `"id":"call-1"`, `"id":"call-2"`, 1)),
	}
	want := fmt.Sprint(r.Fingerprints(base))
	for index, variant := range variants {
		if fmt.Sprint(r.Fingerprints(variant)) == want {
			t.Fatalf("prompt-affecting variant %d did not change fingerprints", index)
		}
	}
}

func TestAnthropicSystemCanonicalizesLikeOpenAI(t *testing.T) {
	cfg := testConfig()
	r := New(cfg)
	openAI := []byte(`{"messages":[{"role":"system","content":"shared system"},{"role":"user","content":"hello"}],"tools":[{"name":"lookup"}]}`)
	anthropic := []byte(`{"system":"shared system","tools":[{"name":"lookup"}],"messages":[{"role":"user","content":"hello"}]}`)
	if got, want := fmt.Sprint(r.Fingerprints(anthropic)), fmt.Sprint(r.Fingerprints(openAI)); got != want {
		t.Fatalf("equivalent Anthropic/OpenAI prompts differ:\n got %s\nwant %s", got, want)
	}
}

func TestLateDivergenceReplay(t *testing.T) {
	base := testConfig()
	base.ChunkBytes = 2048
	base.IndexCapacity = 100_000
	shared := strings.Repeat("shared-trunk-", 30_000) // diverges after the old 256KB window
	promptA := chatBody(t, shared+"tail-a", "task")
	promptB := chatBody(t, shared+"tail-b", "task")

	accuracy := func(cfg Config) int {
		r := New(cfg)
		r.Observe("http://a:8000", r.Fingerprints(promptA))
		r.Observe("http://b:8000", r.Fingerprints(promptB))
		correct := 0
		for _, item := range []struct {
			body []byte
			want string
		}{
			{promptA, "http://a:8000"}, {promptA, "http://a:8000"},
			{promptA, "http://a:8000"}, {promptA, "http://a:8000"},
			{promptB, "http://b:8000"}, {promptB, "http://b:8000"},
			{promptB, "http://b:8000"}, {promptB, "http://b:8000"},
		} {
			if r.Route(item.body).Candidates[0] == item.want {
				correct++
			}
		}
		return correct
	}

	legacy := base
	legacy.MaxPrefixBytes = 256 << 10
	legacy.MaxOverlapBlocks = 10_000 // effectively unbounded
	modern := base
	modern.MaxPrefixBytes = 2 << 20
	modern.MaxOverlapBlocks = 32
	if old, new := accuracy(legacy), accuracy(modern); old != 4 || new != 8 {
		t.Fatalf("late-divergence replay accuracy old=%d/8 new=%d/8, want 4/8 and 8/8", old, new)
	}
}

func BenchmarkLongPromptFingerprints(b *testing.B) {
	body, err := json.Marshal(map[string]any{
		"model": "m",
		"messages": []map[string]string{
			{"role": "system", "content": strings.Repeat("long-context-ledger-", 70_000)},
			{"role": "user", "content": "summarize"},
		},
	})
	if err != nil {
		b.Fatal(err)
	}
	for _, maxBytes := range []int{256 << 10, 2 << 20} {
		b.Run(fmt.Sprintf("max-%dKiB", maxBytes>>10), func(b *testing.B) {
			cfg := testConfig()
			cfg.ChunkBytes = 2048
			cfg.MaxPrefixBytes = maxBytes
			r := New(cfg)
			b.ReportAllocs()
			b.SetBytes(int64(len(body)))
			for range b.N {
				r.Fingerprints(body)
			}
		})
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
