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

The active implementation is Rust: `src/router.rs` (selection),
`src/shims.rs` (request/metadata rewrites), `src/usage.rs` (response parsing),
`src/metrics.rs` (Prometheus), `src/proxy.rs` (async data plane + probes), and
`src/journal.rs`, and `src/tokenizer.rs` (bounded exact-token shadow adapter).
The original Go packages remain temporarily as a parity oracle during the
rolling cutover.

The Rust boundary is deliberate: one parsed request-preparation pass applies
compatibility mutations and derives route fingerprints before feeding the async
streaming proxy. The prepared fingerprint vector is reused after a successful
response instead of reparsing the prompt for cache observation. In selective
remote-shadow mode the same parsed object also derives a vLLM `/tokenize`
payload. It enters a bounded non-blocking worker queue only after the client
request completes, so queue pressure or tokenizer failure cannot delay or
change placement. Local-shadow mode feeds Dynamo's native DeepSeek-V4 renderer
and NVIDIA `fastokens` through bounded blocking CPU workers while the remote
engine remains the exact-ID authority. Local tokenization work never runs on
Tokio I/O workers. Admitted request classes must match remote IDs; known
template-version gaps fail closed to remote-only observation. Next, this
boundary feeds exact per-engine KV indexes.
Exact state is fenced on event gaps and routing falls back automatically to
the approximate chain-fingerprint index.

## The router

Chain fingerprints over the canonicalized prompt prefix (prompt-affecting
system, tool, message, reasoning, name, and tool-call fields;
`DS4_ROUTE_CHUNK_BYTES`=2048-byte blocks ≈ 512 tokens, up to
`DS4_ROUTE_MAX_PREFIX_BYTES`=2MB). Block *i*'s fingerprint hashes block
*i−1*'s fingerprint too, so depth-d match ⇒ whole d-block prefix matches —
the same prefix-tree property engine radix caches key on, approximated
without engine cooperation.

Per upstream: LRU fingerprint index (`DS4_ROUTE_INDEX_CAPACITY`=100k ≈
covers ~200MB of distinct prompt text) populated on every 2xx response.

```
affinity(u) = min(overlapBlocks(u), DS4_ROUTE_MAX_OVERLAP_BLOCKS)  # default 32
score(u)    = affinity(u) − alpha × loadUnits(u)                    # alpha 4
order    = healthy first, score desc, raw overlap desc, rotating tiebreak
```

Every request costs at least one load unit. The estimate uses request-body
bytes remaining after the chosen upstream's overlap, at one unit per
`DS4_ROUTE_LOAD_UNIT_BYTES` (32KB), capped at
`DS4_ROUTE_MAX_LOAD_UNITS` (8). A fully cached large prompt is therefore cheap;
a cold one still reserves the engine. Raw overlap is retained for observability
and breaks exact score ties, while capped affinity ensures a multi-megabyte
trunk can still yield whenever load has a strictly better score. This avoids
making the warm/cold choice at the precise decision boundary depend on round-
robin rotation. The request-count metric
remains a literal count; only placement uses weighted load.

Emergent behaviors (tested in `router_test.go`):
- conversation stickiness (deep overlap on every follow-up turn);
- **template co-location** — sessions of one Helix app share the system
  prompt and share an engine's cache. The previous 4KB hash happened to do
  this too for large system prompts, but could not override affinity under
  concurrent load;
- cold big prefills → least-loaded engine, then temporarily reserve several
  load units so small decoders stay on the other engine (poor-man's
  prefill/decode disaggregation);
- load spikes override affinity once `alpha×ΔloadUnits` exceeds bounded
  affinity;
- unhealthy engines sort last but remain failover candidates;
- `DS4_AFFINITY=load` zeroes the overlap term for an explicit least-loaded
  baseline or engines without reusable prefix state. Do not infer this from
  linear attention alone: current vLLM implements fine-grained, copy-on-write
  recurrent-state prefix caching for Kimi K3's hybrid KDA/MLA stack.

## Shims (accumulated harness-compat fixes, all battle-earned)

| Shim | Why |
|---|---|
| strip `max_tokens`/`max_completion_tokens` ≥ 100k | Helix/Zed send full-context budgets; strict engines reject prompt+budget > ctx |
| flatten content-parts arrays (incl. `{"type":"text"}` with no text) | Zed sends assistant history as parts; SGLang-class engines require strings |
| drop unsupported `reasoning_effort` values | Preserve the current vLLM schema (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`); reject client-only values that engines would 400 |
| `/v1/models` context shrink (`DS4_ADVERTISE_CTX_MARGIN`) | clients undercount rendered prompts; and a thread over the engine limit can't even run compaction — advertised window MUST be below the engine ceiling |

## Metrics

`ds4proxy_*` (compat with existing dashboards): requests/duration/TTFT/TPOT,
prompt/cached/completion tokens, context & output size histograms, finish
reasons, upstream up/probes/errors/requests, client disconnects — plus new:
`route_decisions_total{outcome}`, `route_overlap_blocks`, `route_affinity_blocks`,
`upstream_inflight{upstream}`, `upstream_load_units{upstream}`,
`tokenizer_shadow_total{backend,endpoint,outcome}`, tokenizer duration/token
histograms, and bounded queue depth. Native engine metrics pass through at
`/metrics/upstream/{i}`.

Successful proxy responses and chat logs include an opaque upstream ordinal,
allowing benchmark traffic to correlate exact route choices without exposing
internal service names or subtracting production-wide counters.

With `DS4_ROUTE_JOURNAL=true`, the proxy also emits versioned start/finish
JSONL for static counterfactual replay. It records request/response sizes,
opaque upstream ordinals, route-state snapshots, status, timing, and aggregate
usage. It deliberately excludes prompt text, request IDs, fingerprints,
generated text, and hostnames. Because no prefix identity is retained, replay
holds every observed cache/load snapshot fixed and does not claim to simulate
the cache state caused by earlier counterfactual choices.

## Engine KV-event boundary (under qualification)

r34's vLLM exposes a ZMQ `KVEventBatch` feed with monotonically increasing
sequence numbers, `BlockStored`/`BlockRemoved`/clear events, and a bounded
replay socket. A one-engine probe received 49 consecutive batches without a
gap. This can correct the request-derived index after cache replication and
eviction, but it is not a drop-in replacement. NVIDIA Dynamo's
[replay/recovery comparison](https://github.com/ai-dynamo/dynamo/blob/main/docs/fern/pages/developer-guide/knowledge-base/modular-components/router/kv-event-replay-comparison.md)
is the reference for the failure semantics below:

- `BlockStored` carries exact token IDs. The feed must stay on the trusted
  compose network, raw events must never enter logs/journals, and the in-memory
  retention/privacy boundary needs explicit review.
- The observed DSpark feed contains several cache-group block geometries, not
  only the configured 256-token physical block. A consumer must honor group and
  cache-spec metadata rather than merging every emitted hash into one index.
- vLLM replay covers only its retained event window and has no full current-
  state snapshot. Sequence gaps must trigger bounded replay; an unrecoverable
  gap clears/fences that engine's exact index and falls back to the current
  approximate router. Dynamo's worker-side radix-tree dump is the stronger
  recovery model to copy if this limitation matters operationally.
- `src/kv_fence.rs` encodes this independently of ZMQ and the index: subscriber
  startup is observation-only, a full clear/snapshot begins a trusted
  generation, gaps request an inclusive bounded replay, and invalid or
  oversized recovery increments the generation and disables exact placement.
- `src/kv_wire.rs` decodes only the MessagePack payload frame behind explicit
  byte/event/hash/token/block bounds. Its fixture was emitted by the exact r34
  `msgspec` classes on node06 and covers bytes/integer hashes, cache-group/spec
  metadata, removals, and a full clear. Unknown event types and malformed
  shapes fail closed; errors never render token IDs, hashes, or payload bytes.
- `src/exact_index.rs` holds one bounded inventory per engine. Engine block
  hashes remain opaque reverse-removal keys; trie edges own exact token slices,
  so lookup does not depend on reproducing vLLM's rolling hash and verifies
  equality rather than trusting a fingerprint. A coarse per-engine `RwLock`
  keeps reads concurrent and writes serialized, which the node06 capacity-scale
  benchmark shows is ample for two engines. Capacity, parent/path, replay, or
  lookup-budget failure clears and fences the generation. Non-local, non-GPU,
  non-main-attention, LoRA, cache-salted, and extra-key events stay out of the
  exact inventory until request-side namespace parity exists.
- Exact request lookup requires the rendered token sequence. Calling r34's
  `/tokenize` for every request costs 3.7ms at 299 tokens, 8.4ms at 4.3K,
  41ms at 21K, and 203ms at 83.7K, while returning up to 419KB of token IDs.
  The viable design is shadow mode first, then selective exact lookup for
  high-value ambiguous decisions and/or a session-cached incremental path;
  unconditional hot-path tokenization would tax returning long sessions.

## Learnings adopted (and their sources)

| Source | Idea | Status |
|---|---|---|
| NVIDIA Dynamo | KV-aware routing (overlap + load) | **v1.1 (this repo)** |
| NVIDIA Dynamo | conditional disaggregation (cold prefill placement) | **v1.1** (size-weighted load reservation) |
| NVIDIA Dynamo | event gaps, replay, and snapshot recovery | native feed qualification / shadow-mode design |
| [Kimi K3 / KDA](https://github.com/MoonshotAI/Kimi-K3/blob/main/k3_tech_report.pdf) | model-aware cache geometry; recurrent state remains reusable | research / benchmark |
| Kimi K3 | primary/secondary affinity and request-class budgets | planned, scaled down to two engines |
| DwarfStar/ds4 | per-request timings surfaced to ops | v1.0 (chat log line + histograms) |
| DwarfStar/ds4 | decision traces and policy replay | **v1.1 rc5** (privacy-bounded static replay) |
| SGLang router | radix-tree-approximate LB | v1.1 (chain fingerprints) |

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the tracked list. Summary below.

1. **KV-event ground truth**: qualify vLLM `kv_events` in privacy-safe shadow
   mode, add gap/replay/fallback semantics, then use exact inventory only where
   tokenization cost is justified. Dynamo's event index and snapshot recovery
   are the reference; never persist raw event token IDs.
2. **Decision journal + offline replay** (shipped in rc5, inspired by
   DwarfStar's `dspark_trace_replay.py`): privacy-bounded route snapshots and
   outcomes can be replayed against alternative alphas/caps before production
   A/B changes. Next, add a controlled affinity-versus-load conflict workload
   because routine traces do not exercise every policy boundary.
3. **Pinned sessions** (DwarfStar's pinned deep-trunk KV banks): mark
   long-lived orchestrator conversations so neither the router (migration)
   nor alpha pressure moves them off their warm engine. Use Kimi K3's bounded
   primary/secondary assignment so failure recovery spreads cold re-prefill.
4. **True disaggregated prefill** once engines expose KV transfer (vLLM P/D
   + NIXL): route prefill to a prefill pool, stream KV to a decode engine.
   Single-box value is modest; multi-node value is large.
5. **SLA planner-lite**: watch queue depth + TTFT p95, recommend (not
   enact) MAX_NUM_SEQS / instance-count changes. Dynamo's planner, advisory.
6. **KVBM-lite**: engine-side CPU-RAM KV offload (LMCache connector) so
   evicted agent sessions warm-restore; ds4's disk KV banks proved the
   pattern on this exact workload.
7. Anthropic `/v1/messages` fingerprint canonicalization (shipped in rc3),
   including top-level system prompts and prompt-affecting tool/reasoning
   fields shared with OpenAI requests.

## Benchmarks

Method: `bench/locality_bench.sh` — N simulated "apps" (shared ~20KB system
prompt) × M sessions × T turns against the LB; measure cached-token %, TTFT,
and upstream split vs the static-hash baseline. Plus the standard aggregate
sweep (8/16/32-way) to prove no regression. Results land in RESULTS.md.
