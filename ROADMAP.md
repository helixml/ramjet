# mini-dynamo roadmap

Status legend: ✅ done · 🔨 in progress · ⬜ planned. Ordered by
value-per-effort given the current deployment (2 vLLM+DSpark TP4 instances on
node06). The design rationale for each lives in DESIGN.md.

## Shipped (v1.1)

- ✅ Overlap+load router (`score = prefixOverlapBlocks − alpha·inflight`),
  chain-fingerprint prefix index per upstream.
- ✅ Conversation stickiness + cross-session template co-location.
- ✅ Cold-prefill → least-loaded placement (emergent from scoring).
- ✅ Model-aware affinity toggle (`DS4_AFFINITY=prefix|load`) for K3/KDA-class
  linear-attention models.
- ✅ Health-aware failover, authenticated probes, `/v1/models` context-margin
  rewrite, request shims (max_tokens / content-parts / reasoning_effort).
- ✅ Prometheus surface incl. `route_decisions_total{outcome}`,
  `route_overlap_blocks`, `upstream_inflight`; engine-native passthrough.
- ✅ Measured: ties hash router on cache locality (82.9%), 1.57× under
  concurrent same-app load (RESULTS.md).

## Near term

- ⬜ **CI + package publishing.** GitHub Actions: `go test ./...`, `go vet`,
  build, and push `ghcr.io/helixml/ds4-loadbalancer:<tag>` on tag/main.
  Removes the current manual "build on node06, no ghcr push" gap (the
  interactive `gh` token lacks `write:packages`).
- ⬜ **Alpha auto-tuning / sweep.** The 12-concurrent split was 4/8 at
  `alpha=4`; expose a sweep in `bench/` and pick a default from data.
  Consider making alpha adaptive to observed queue depth.
- ⬜ **Decision journal + offline replay** (DwarfStar `dspark_trace_replay`
  idea). Log `(fingerprints, per-upstream inflight, choice, outcome)` per
  request; replay against alternative policies/alphas offline before
  changing production.
- ⬜ **Anthropic `/v1/messages` canonicalization.** Currently raw-body
  fallback for fingerprints; canonicalize system+messages like the chat path
  so cross-format prefix overlap is detected.

## Medium term

- ⬜ **KV-event ground truth.** Subscribe to vLLM `kv_events` (block
  stored/removed) and replace the approximate LRU index with the engine's
  actual block inventory. Removes drift from evictions we can't see. This is
  what NVIDIA Dynamo's router does.
- ⬜ **Pinned sessions.** Mark long-lived orchestrator conversations so
  neither router migration nor alpha pressure moves them off their warm
  engine (DwarfStar pinned-deep-trunk analogue).
- ⬜ **Load-aware tie-breaks under cache pressure.** With >2 instances or a
  KV pool too small to hold every template, eviction makes overlap routing
  matter much more than at current scale; validate + tune there.
- ⬜ **SLA planner-lite (advisory).** Watch queue depth + TTFT p95, emit a
  recommendation (not an action) for `MAX_NUM_SEQS` / instance count.
  Dynamo's planner, read-only.

## Longer term / speculative

- ⬜ **True disaggregated prefill/decode** once engines expose KV transfer
  (vLLM P/D + NIXL): route prefill to a prefill pool, stream KV to a decode
  engine. Modest single-box value; large multi-node value.
- ⬜ **KVBM-lite**: engine-side CPU-RAM KV offload (LMCache connector) so
  evicted agent sessions warm-restore instead of re-prefilling. ds4's disk
  KV banks proved the pattern on this exact workload.
- ⬜ **Multi-model / multi-pool routing.** Today one model, N identical
  engines. Extend to several models behind one endpoint with per-model
  affinity policy and pool sizing.

## Helix-side follow-ups (not mini-dynamo, but this LB masks them)

The shims exist because Helix's Zed config emits fields strict engines reject.
Proper fixes belong upstream: cap Zed `max_output_tokens`, retry-on-5xx for
ACP thread follow-ups, and drop/valid-map `reasoning_effort` on agent switch.
