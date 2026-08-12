// Package router implements KV-cache-locality-aware upstream selection.
//
// Design (see DESIGN.md): instead of pinning a conversation to an upstream by
// a static hash, we fingerprint the request's prompt prefix in fixed-size
// chunks and keep a per-upstream index of recently-served fingerprints. A
// request routes to the upstream with the deepest cached-prefix overlap,
// discounted by that upstream's estimated in-flight load:
//
//	score(u) = overlapBlocks(u) - alpha*loadUnits(u)   (healthy first)
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
//   - load pressure: enough in-flight work overrides affinity; large cold
//     prefills reserve several units so short requests stay on the other engine
//     (alpha tunes the trade; DS4_AFFINITY=load provides an explicit
//     least-loaded baseline for engines without reusable prefix state).
package router

import (
	"bytes"
	"container/list"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"sort"
	"sync"
)

type Config struct {
	Upstreams        []string
	Alpha            float64 // in-flight discount, in blocks per request
	ChunkBytes       int     // fingerprint block size (~4 bytes/token heuristic)
	MaxPrefixBytes   int     // how much prompt prefix to fingerprint
	MaxOverlapBlocks int     // maximum overlap contribution to the route score
	IndexCapacity    int     // per-upstream fingerprint LRU capacity
	LoadUnitBytes    int     // request-body bytes represented by one load unit
	MaxLoadUnits     int     // maximum units charged to one request
	AffinityEnabled  bool    // false => pure load balancing baseline
}

func DefaultConfig(upstreams []string) Config {
	return Config{
		Upstreams:        upstreams,
		Alpha:            4,
		ChunkBytes:       2048,
		MaxPrefixBytes:   2 << 20,
		MaxOverlapBlocks: 32,
		IndexCapacity:    100_000,
		LoadUnitBytes:    32 << 10,
		MaxLoadUnits:     8,
		AffinityEnabled:  true,
	}
}

// Decision describes one routing choice, for metrics and the journal.
type Decision struct {
	Candidates     []string // ordered: try first to last
	CandidateState []CandidateState
	OverlapBlocks  int    // winner's overlap depth
	TotalBlocks    int    // fingerprinted blocks in the request
	AffinityBlocks int    // overlap contribution after score normalization
	LoadUnits      int    // estimated in-flight cost for this request
	Rotation       int    // rotating tiebreak used for this decision
	Outcome        string // "overlap" | "load" | "rr" | "single"
}

// CandidateState is a privacy-safe snapshot of one upstream at route time.
// Index is the stable ordinal from Config.Upstreams; no hostname is exposed.
type CandidateState struct {
	Index            int  `json:"upstream"`
	Rank             int  `json:"rank"`
	OverlapBlocks    int  `json:"overlap_blocks"`
	AffinityBlocks   int  `json:"affinity_blocks"`
	LoadUnits        int  `json:"load_units"`
	RequestLoadUnits int  `json:"request_load_units"`
	Healthy          bool `json:"healthy"`
}

type upstreamState struct {
	index    map[uint64]*list.Element // fingerprint -> LRU node
	lru      *list.List               // of uint64 fingerprints
	inflight int
	load     int
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
	return r.AcquireWeighted(upstream, 1)
}

// AcquireWeighted marks one request in flight and charges its estimated load.
// Inflight remains a request count for dashboard compatibility; routing uses
// LoadUnits so one large prefill is not treated like one tiny decode request.
func (r *Router) AcquireWeighted(upstream string, units int) func() {
	units = max(1, units)
	r.mu.Lock()
	if s, ok := r.states[upstream]; ok {
		s.inflight++
		s.load += units
	}
	r.mu.Unlock()
	return func() {
		r.mu.Lock()
		if s, ok := r.states[upstream]; ok {
			if s.inflight > 0 {
				s.inflight--
			}
			s.load = max(0, s.load-units)
		}
		r.mu.Unlock()
	}
}

func (r *Router) LoadUnits(upstream string) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	if s, ok := r.states[upstream]; ok {
		return s.load
	}
	return 0
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
// are canonicalized from prompt-affecting top-level and message fields;
// anything else falls back to the raw body prefix. The chain property matters: block i's
// fingerprint commits to everything before it, so a match at depth d implies
// the whole d-block prefix matches.
func (r *Router) Fingerprints(body []byte) []uint64 {
	return chainFingerprints(canonicalPrompt(body, r.cfg.MaxPrefixBytes), r.cfg.ChunkBytes)
}

func canonicalPrompt(body []byte, maxBytes int) []byte {
	var object map[string]json.RawMessage
	if json.Unmarshal(body, &object) != nil {
		if len(body) > maxBytes {
			body = body[:maxBytes]
		}
		return body
	}
	var messages []map[string]json.RawMessage
	if raw := object["messages"]; len(raw) > 0 && string(raw) != "null" {
		if json.Unmarshal(raw, &messages) != nil {
			if len(body) > maxBytes {
				body = body[:maxBytes]
			}
			return body
		}
	}
	if len(messages) == 0 && !present(object["system"]) {
		if len(body) > maxBytes {
			body = body[:maxBytes]
		}
		return body
	}

	out := make([]byte, 0, min(maxBytes, 64<<10))
	// Anthropic carries system separately; normalize it to the same synthetic
	// message representation as an OpenAI leading system message.
	if present(object["system"]) {
		out = appendMessage(out, map[string]json.RawMessage{
			"role":    json.RawMessage(`"system"`),
			"content": object["system"],
		})
	}
	leading := 0
	for leading < len(messages) {
		role := messageRole(messages[leading])
		if role != "system" && role != "developer" {
			break
		}
		out = appendMessage(out, messages[leading])
		leading++
	}
	// Tools are rendered near the system/template prefix by common chat
	// templates, so place them before non-system conversation messages.
	for _, key := range []string{
		"tools", "functions", "tool_choice", "function_call",
		"parallel_tool_calls", "reasoning_effort", "thinking", "response_format",
	} {
		out = appendCanonicalField(out, key, object[key])
	}
	for _, message := range messages[leading:] {
		out = appendMessage(out, message)
	}
	if len(out) > maxBytes {
		out = out[:maxBytes]
	}
	return out
}

func appendMessage(out []byte, message map[string]json.RawMessage) []byte {
	out = append(out, "message"...)
	out = append(out, 0)
	for _, key := range []string{
		"role", "name", "content", "reasoning_content", "reasoning",
		"tool_calls", "function_call", "tool_call_id",
	} {
		out = appendCanonicalField(out, key, message[key])
	}
	return out
}

func appendCanonicalField(out []byte, key string, raw json.RawMessage) []byte {
	if !present(raw) {
		return out
	}
	out = append(out, key...)
	out = append(out, 0)
	out = append(out, canonicalJSON(raw)...)
	out = append(out, 0)
	return out
}

func canonicalJSON(raw json.RawMessage) []byte {
	trimmed := bytes.TrimSpace(raw)
	if len(trimmed) == 0 || (trimmed[0] != '{' && trimmed[0] != '[') {
		// Scalar JSON has no map-order ambiguity. This is the common path for
		// multi-megabyte text content and avoids decoding/copying the string.
		return trimmed
	}
	decoder := json.NewDecoder(bytes.NewReader(trimmed))
	decoder.UseNumber()
	var value any
	if decoder.Decode(&value) != nil {
		return raw
	}
	canonical, err := json.Marshal(value)
	if err != nil {
		return raw
	}
	return canonical
}

func present(raw json.RawMessage) bool {
	return len(raw) > 0 && string(raw) != "null"
}

func messageRole(message map[string]json.RawMessage) string {
	var role string
	_ = json.Unmarshal(message["role"], &role)
	return role
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
	fps := r.Fingerprints(body)

	r.mu.Lock()
	defer r.mu.Unlock()

	type scored struct {
		upstream string
		overlap  int
		affinity int
		load     int
		score    float64
		healthy  bool
		tiebreak int
	}
	results := make([]scored, 0, len(r.cfg.Upstreams))
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
		affinity := overlap
		if r.cfg.MaxOverlapBlocks > 0 {
			affinity = min(affinity, r.cfg.MaxOverlapBlocks)
		}
		results = append(results, scored{
			upstream: u,
			overlap:  overlap,
			affinity: affinity,
			load:     s.load,
			score:    float64(affinity) - r.cfg.Alpha*float64(s.load),
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
		// Score equality is the exact affinity/load decision boundary. Prefer
		// deeper raw overlap there: load still wins whenever its score is strictly
		// better, while an equality no longer makes a costly warm/cold migration
		// depend on round-robin rotation.
		if results[i].overlap != results[j].overlap {
			return results[i].overlap > results[j].overlap
		}
		return (results[i].tiebreak+rotation)%len(results) < (results[j].tiebreak+rotation)%len(results)
	})

	decision := Decision{
		Candidates:     make([]string, len(results)),
		CandidateState: make([]CandidateState, len(results)),
		OverlapBlocks:  results[0].overlap,
		TotalBlocks:    len(fps),
		AffinityBlocks: results[0].affinity,
		LoadUnits:      r.estimatedLoadUnits(len(body), results[0].overlap),
		Rotation:       rotation,
	}
	for i, s := range results {
		decision.Candidates[i] = s.upstream
		decision.CandidateState[i] = CandidateState{
			Index:            s.tiebreak,
			Rank:             i,
			OverlapBlocks:    s.overlap,
			AffinityBlocks:   s.affinity,
			LoadUnits:        s.load,
			RequestLoadUnits: r.estimatedLoadUnits(len(body), s.overlap),
			Healthy:          s.healthy,
		}
	}
	scoresDiffer := false
	for _, s := range results[1:] {
		if s.score != results[0].score {
			scoresDiffer = true
			break
		}
	}
	switch {
	case len(results) == 1:
		decision.Outcome = "single"
	case results[0].overlap > 0:
		decision.Outcome = "overlap"
	case scoresDiffer:
		decision.Outcome = "load"
	default:
		decision.Outcome = "rr"
	}
	return decision
}

func (r *Router) estimatedLoadUnits(bodyBytes, overlapBlocks int) int {
	uncachedBytes := max(0, bodyBytes-overlapBlocks*r.cfg.ChunkBytes)
	units := 1
	if r.cfg.LoadUnitBytes > 0 {
		units = max(1, (uncachedBytes+r.cfg.LoadUnitBytes-1)/r.cfg.LoadUnitBytes)
	}
	if r.cfg.MaxLoadUnits > 0 {
		units = min(units, r.cfg.MaxLoadUnits)
	}
	return units
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
