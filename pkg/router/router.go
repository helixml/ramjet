// Package router implements KV-cache-locality-aware upstream selection.
//
// Design (see DESIGN.md): instead of pinning a conversation to an upstream by
// a static hash, we fingerprint the request's prompt prefix in fixed-size
// chunks and keep a per-upstream index of recently-served fingerprints. A
// request routes to the upstream with the deepest cached-prefix overlap,
// discounted by that upstream's in-flight load:
//
//	score(u) = overlapBlocks(u) - alpha*inflight(u)   (healthy first)
//
// Consequences, all emergent from the one formula:
//   - conversation stickiness: a session's own turns share the whole prefix
//     with their predecessor -> deep overlap -> same upstream (radix hit);
//   - template sharing: sessions of the same Helix app share the system
//     prompt -> shallow-but-real overlap -> co-located, unlike hash routing
//     which splits them 50/50;
//   - cold big prefills: zero overlap everywhere -> pure least-loaded
//     placement, so a giant TTFT job lands on the quieter engine
//     (poor-man's prefill/decode disaggregation, per NVIDIA Dynamo);
//   - load pressure: enough in-flight difference overrides affinity
//     (alpha tunes the trade; K3/KDA-class models that don't reward prefix
//     affinity can set DS4_AFFINITY=load to zero the overlap term).
package router

import (
	"container/list"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"sort"
	"sync"
)

type Config struct {
	Upstreams       []string
	Alpha           float64 // in-flight discount, in blocks per request
	ChunkBytes      int     // fingerprint block size (~4 bytes/token heuristic)
	MaxPrefixBytes  int     // how much prompt prefix to fingerprint
	IndexCapacity   int     // per-upstream fingerprint LRU capacity
	AffinityEnabled bool    // false => pure load balancing (K3/KDA mode)
}

func DefaultConfig(upstreams []string) Config {
	return Config{
		Upstreams:       upstreams,
		Alpha:           4,
		ChunkBytes:      2048,
		MaxPrefixBytes:  256 << 10,
		IndexCapacity:   100_000,
		AffinityEnabled: true,
	}
}

// Decision describes one routing choice, for metrics and the journal.
type Decision struct {
	Candidates    []string // ordered: try first to last
	OverlapBlocks int      // winner's overlap depth
	TotalBlocks   int      // fingerprinted blocks in the request
	Outcome       string   // "overlap" | "load" | "rr" | "single"
}

type upstreamState struct {
	index    map[uint64]*list.Element // fingerprint -> LRU node
	lru      *list.List               // of uint64 fingerprints
	inflight int
}

type Router struct {
	cfg Config

	mu      sync.Mutex
	states  map[string]*upstreamState
	healthy map[string]bool
	rr      uint64
}

func New(cfg Config) *Router {
	states := make(map[string]*upstreamState, len(cfg.Upstreams))
	healthy := make(map[string]bool, len(cfg.Upstreams))
	for _, u := range cfg.Upstreams {
		states[u] = &upstreamState{index: make(map[uint64]*list.Element), lru: list.New()}
		healthy[u] = true
	}
	return &Router{cfg: cfg, states: states, healthy: healthy}
}

func (r *Router) SetHealthy(upstream string, healthy bool) {
	r.mu.Lock()
	r.healthy[upstream] = healthy
	r.mu.Unlock()
}

func (r *Router) IsHealthy(upstream string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.healthy[upstream]
}

// Acquire marks a request in flight on an upstream; returns the release func.
func (r *Router) Acquire(upstream string) func() {
	r.mu.Lock()
	if s, ok := r.states[upstream]; ok {
		s.inflight++
	}
	r.mu.Unlock()
	return func() {
		r.mu.Lock()
		if s, ok := r.states[upstream]; ok && s.inflight > 0 {
			s.inflight--
		}
		r.mu.Unlock()
	}
}

func (r *Router) Inflight(upstream string) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	if s, ok := r.states[upstream]; ok {
		return s.inflight
	}
	return 0
}

// Fingerprints derives the chain fingerprints for a request body. Chat bodies
// are canonicalized from their messages (role + content); anything else falls
// back to the raw body prefix. The chain property matters: block i's
// fingerprint commits to everything before it, so a match at depth d implies
// the whole d-block prefix matches.
func (r *Router) Fingerprints(body []byte) []uint64 {
	return chainFingerprints(canonicalPrompt(body, r.cfg.MaxPrefixBytes), r.cfg.ChunkBytes)
}

func canonicalPrompt(body []byte, maxBytes int) []byte {
	var object struct {
		Messages []struct {
			Role    string          `json:"role"`
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	if json.Unmarshal(body, &object) != nil || len(object.Messages) == 0 {
		if len(body) > maxBytes {
			body = body[:maxBytes]
		}
		return body
	}
	out := make([]byte, 0, min(maxBytes, 64<<10))
	for _, m := range object.Messages {
		out = append(out, m.Role...)
		out = append(out, 0)
		out = append(out, m.Content...)
		out = append(out, 0)
		if len(out) >= maxBytes {
			out = out[:maxBytes]
			break
		}
	}
	return out
}

func chainFingerprints(prompt []byte, chunk int) []uint64 {
	if len(prompt) == 0 || chunk <= 0 {
		return nil
	}
	count := (len(prompt) + chunk - 1) / chunk
	fps := make([]uint64, 0, count)
	var prev uint64
	for i := 0; i < count; i++ {
		end := min((i+1)*chunk, len(prompt))
		h := fnv.New64a()
		var carry [8]byte
		binary.LittleEndian.PutUint64(carry[:], prev)
		_, _ = h.Write(carry[:])
		_, _ = h.Write(prompt[i*chunk : end])
		prev = h.Sum64()
		fps = append(fps, prev)
	}
	return fps
}

// Route returns the ordered candidate list plus decision metadata.
func (r *Router) Route(body []byte) Decision {
	if len(r.cfg.Upstreams) == 1 {
		return Decision{Candidates: append([]string(nil), r.cfg.Upstreams...), Outcome: "single"}
	}
	fps := r.Fingerprints(body)

	r.mu.Lock()
	defer r.mu.Unlock()

	type scored struct {
		upstream string
		overlap  int
		score    float64
		healthy  bool
		tiebreak int
	}
	results := make([]scored, 0, len(r.cfg.Upstreams))
	bestOverlap := 0
	for i, u := range r.cfg.Upstreams {
		s := r.states[u]
		overlap := 0
		if r.cfg.AffinityEnabled {
			for _, fp := range fps {
				if _, ok := s.index[fp]; !ok {
					break
				}
				overlap++
			}
		}
		if overlap > bestOverlap {
			bestOverlap = overlap
		}
		results = append(results, scored{
			upstream: u,
			overlap:  overlap,
			score:    float64(overlap) - r.cfg.Alpha*float64(s.inflight),
			healthy:  r.healthy[u],
			tiebreak: i,
		})
	}

	// Stable deterministic tiebreak for all-equal scores: rotate by rr so cold
	// traffic spreads instead of always hitting upstream 0.
	r.rr++
	rotation := int(r.rr % uint64(len(results)))

	sort.SliceStable(results, func(i, j int) bool {
		if results[i].healthy != results[j].healthy {
			return results[i].healthy
		}
		if results[i].score != results[j].score {
			return results[i].score > results[j].score
		}
		return (results[i].tiebreak+rotation)%len(results) < (results[j].tiebreak+rotation)%len(results)
	})

	decision := Decision{
		Candidates:    make([]string, len(results)),
		OverlapBlocks: results[0].overlap,
		TotalBlocks:   len(fps),
	}
	for i, s := range results {
		decision.Candidates[i] = s.upstream
	}
	scoresDiffer := false
	for _, s := range results[1:] {
		if s.score != results[0].score {
			scoresDiffer = true
			break
		}
	}
	switch {
	case bestOverlap > 0:
		decision.Outcome = "overlap"
	case scoresDiffer:
		decision.Outcome = "load"
	default:
		decision.Outcome = "rr"
	}
	return decision
}

// Observe records that an upstream served (2xx) a request with the given
// fingerprints, so future overlapping requests prefer it.
func (r *Router) Observe(upstream string, fps []uint64) {
	if len(fps) == 0 {
		return
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	s, ok := r.states[upstream]
	if !ok {
		return
	}
	for _, fp := range fps {
		if node, exists := s.index[fp]; exists {
			s.lru.MoveToBack(node)
			continue
		}
		s.index[fp] = s.lru.PushBack(fp)
	}
	for s.lru.Len() > r.cfg.IndexCapacity {
		oldest := s.lru.Front()
		s.lru.Remove(oldest)
		delete(s.index, oldest.Value.(uint64))
	}
}

// Snapshot reports per-upstream state for metrics/debugging.
func (r *Router) Snapshot() map[string]string {
	r.mu.Lock()
	defer r.mu.Unlock()
	out := make(map[string]string, len(r.states))
	for u, s := range r.states {
		out[u] = fmt.Sprintf("inflight=%d indexed=%d healthy=%t", s.inflight, s.lru.Len(), r.healthy[u])
	}
	return out
}
