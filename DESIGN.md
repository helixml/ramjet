# mini-dynamo — design & roadmap

A single-binary, KV-cache-locality-aware load balancer for OpenAI-compatible
inference engines (vLLM, SGLang, ds4, …). Grew out of the node06 DeepSeek
V4 Flash serving stack (`helixml/infra:node06/inference/`); the name is an
honest nod to [NVIDIA Dynamo](https://github.com/ai-dynamo/dynamo), whose
ideas we scale down to "a handful of engines on one or a few boxes".

## Why it exists

Serving agent fleets means: huge prompts resent every turn, engines whose
prefix caches make those prompts nearly free — *if* requests land where the
cache is — and harness clients that emit requests strict engines reject.
Generic LBs (round-robin, least-conn) shred prefix caches; engine-specific
routers lock you to one engine. mini-dynamo sits in front of any
OpenAI-compatible engine and does three jobs:

1. **Route for cache locality and load** (the router).
2. **Absorb client/engine incompatibilities** (the shims).
3. **Measure everything** (Prometheus, engine-agnostic + native passthrough).

## Architecture

```
client ──▶ :8000 proxy ──▶ router.Route(body) ──▶ engine[i] (stream through)
                │                                     │
                ├─ shims: sanitize request,           └─ router.Observe(fps)
                │         /v1/models ctx rewrite            on 2xx
                └─ usage: tokens/TTFT/finish ──▶ :9090 /metrics
                                                :9090 /metrics/upstream/{i}
```

Packages: `pkg/router` (selection), `pkg/shims` (request/metadata rewrites),
`pkg/usage` (response parsing), `pkg/metrics` (Prometheus), `pkg/proxy`
(data plane + probes), `pkg/config`, `cmd/mini-dynamo`.

## The router

Chain fingerprints over the canonicalized prompt prefix (role+content per
message, `DS4_ROUTE_CHUNK_BYTES`=2048-byte blocks ≈ 512 tokens, up to
`DS4_ROUTE_MAX_PREFIX_BYTES`=256KB). Block *i*'s fingerprint hashes block
*i−1*'s fingerprint too, so depth-d match ⇒ whole d-block prefix matches —
the same prefix-tree property engine radix caches key on, approximated
without engine cooperation.

Per upstream: LRU fingerprint index (`DS4_ROUTE_INDEX_CAPACITY`=100k ≈
covers ~200MB of distinct prompt text) populated on every 2xx response.

```
score(u) = overlapBlocks(u) − alpha × inflight(u)      alpha = DS4_ROUTE_ALPHA (4)
order    = healthy first, score desc, rotating tiebreak
```

Emergent behaviors (tested in `router_test.go`):
- conversation stickiness (deep overlap on every follow-up turn);
- **template co-location** — sessions of one Helix app share the system
  prompt and now share an engine's cache (static hashing split them 50/50);
- cold big prefills → least-loaded engine (poor-man's prefill/decode
  disaggregation);
- load spikes override affinity once `alpha×Δinflight` exceeds overlap;
- unhealthy engines sort last but remain failover candidates;
- `DS4_AFFINITY=load` zeroes the overlap term — for **K3/KDA-class models**
  whose recurrent-state attention can't snapshot arbitrary prefixes, prefix
  affinity buys nothing and pure load balancing is correct.

## Shims (accumulated harness-compat fixes, all battle-earned)

| Shim | Why |
|---|---|
| strip `max_tokens`/`max_completion_tokens` ≥ 100k | Helix/Zed send full-context budgets; strict engines reject prompt+budget > ctx |
| flatten content-parts arrays (incl. `{"type":"text"}` with no text) | Zed sends assistant history as parts; SGLang-class engines require strings |
| drop invalid `reasoning_effort` (`"none"`) | Helix agent-switch flow emits it; engines 400 |
| `/v1/models` context shrink (`DS4_ADVERTISE_CTX_MARGIN`) | clients undercount rendered prompts; and a thread over the engine limit can't even run compaction — advertised window MUST be below the engine ceiling |

## Metrics

`ds4proxy_*` (compat with existing dashboards): requests/duration/TTFT/TPOT,
prompt/cached/completion tokens, context & output size histograms, finish
reasons, upstream up/probes/errors/requests, client disconnects — plus new:
`route_decisions_total{outcome}`, `route_overlap_blocks`,
`upstream_inflight{upstream}`. Native engine metrics pass through at
`/metrics/upstream/{i}`.

## Learnings adopted (and their sources)

| Source | Idea | Status |
|---|---|---|
| NVIDIA Dynamo | KV-aware routing (overlap + load) | **v1.1 (this repo)** |
| NVIDIA Dynamo | conditional disaggregation (cold prefill placement) | **v1.1** (emergent from scoring) |
| Kimi K3 / KDA | model-aware affinity (linear-attn ⇒ affinity off) | **v1.1** (`DS4_AFFINITY`) |
| DwarfStar/ds4 | per-request timings surfaced to ops | v1.0 (chat log line + histograms) |
| SGLang router | radix-tree-approximate LB | v1.1 (chain fingerprints) |

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the tracked list. Summary below.

1. **KV-event ground truth**: vLLM exposes `kv_events` (block stored/removed).
   Subscribe → replace the approximate index with the engine's actual block
   inventory (Dynamo does exactly this). Removes drift from evictions we
   can't see today.
2. **Decision journal + offline replay** (DwarfStar's
   `dspark_trace_replay.py` idea): log (fingerprints, inflight, choice) per
   request; replay against alternative alphas/policies offline before
   changing production. Cheap and high-leverage for tuning.
3. **Pinned sessions** (DwarfStar's pinned deep-trunk KV banks): mark
   long-lived orchestrator conversations so neither the router (migration)
   nor alpha pressure moves them off their warm engine.
4. **True disaggregated prefill** once engines expose KV transfer (vLLM P/D
   + NIXL): route prefill to a prefill pool, stream KV to a decode engine.
   Single-box value is modest; multi-node value is large.
5. **SLA planner-lite**: watch queue depth + TTFT p95, recommend (not
   enact) MAX_NUM_SEQS / instance-count changes. Dynamo's planner, advisory.
6. **KVBM-lite**: engine-side CPU-RAM KV offload (LMCache connector) so
   evicted agent sessions warm-restore; ds4's disk KV banks proved the
   pattern on this exact workload.
7. Anthropic `/v1/messages` fingerprint canonicalization (currently raw-body
   fallback — works, but misses cross-format overlap).

## Benchmarks

Method: `bench/locality_bench.sh` — N simulated "apps" (shared ~20KB system
prompt) × M sessions × T turns against the LB; measure cached-token %, TTFT,
and upstream split vs the static-hash baseline. Plus the standard aggregate
sweep (8/16/32-way) to prove no regression. Results land in RESULTS.md.
