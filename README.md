# mini-dynamo

KV-cache-locality-aware load balancer for OpenAI-compatible inference
engines. Compact Rust binary; drop-in replacement for `ds4-loadbalancer`
(same env vars, same `ds4proxy_*` metrics), plus an overlap-scored router:

    score(upstream) = min(prefixOverlapBlocks, maxAffinityBlocks) − alpha × loadUnits

Conversations stick to their warm engine, sessions that share prompt
templates co-locate, and cold big prefills reserve size-weighted load so short
requests remain on the other engine. See DESIGN.md for the full story and
roadmap (NVIDIA Dynamo, Kimi K3/KDA, and DwarfStar are the acknowledged
influences).

The canonical deployment stack is
`deploy/dspark_0731/docker-compose.yaml`. Its adjacent README documents
validation, the generated infra mirror, and safe LB-only deployment. The infra
copy is operational convenience only; do not edit it independently.

## Run

    DS4_UPSTREAM=http://engine-a:8000,http://engine-b:8000 \
    DS4_UPSTREAM_TOKEN=<bearer for engine probes> \
    ./mini-dynamo
    # API :8000 (/health), Prometheus :9090 (/metrics, /metrics/upstream/{i})

`GET /health` returns every opaque replica ordinal with its serving health,
inflight count, load units, and approximate-index size. It is `200 ok` when all
replicas are healthy, `200 degraded` when at least one can still serve, and
`503 unhealthy` when none can serve. Known-unhealthy replicas are excluded
from request attempts, including the final failover slot.

Successful proxied responses include `X-Mini-Dynamo-Upstream: 0|1` for
opaque per-request route correlation; internal upstream names are not exposed.

Key env (all optional): DS4_ADVERTISE_CTX_MARGIN (16384),
DS4_MAX_TOKENS_STRIP (100000), DS4_ROUTE_ALPHA (4), DS4_ROUTE_CHUNK_BYTES
(2048), DS4_ROUTE_MAX_PREFIX_BYTES (2097152), DS4_ROUTE_MAX_OVERLAP_BLOCKS
(32), DS4_ROUTE_INDEX_CAPACITY (100000), DS4_ROUTE_LOAD_UNIT_BYTES (32768),
DS4_ROUTE_MAX_LOAD_UNITS (8),
DS4_AFFINITY (prefix|load), DS4_ROUTE_JOURNAL (false),
DS4_TOKENIZER_MODE (off|remote-shadow|local-shadow),
DS4_TOKENIZER_PATH, DS4_TOKENIZER_SHA256 (both required by local-shadow),
DS4_TOKENIZER_PROFILE (deepseek-v4-r34), DS4_TOKENIZER_MIN_BYTES (32768),
DS4_TOKENIZER_MAX_BYTES (2097152), DS4_TOKENIZER_WORKERS (1),
DS4_TOKENIZER_QUEUE_CAPACITY (8), DS4_TOKENIZER_TIMEOUT_MS (2000),
DS4_EXACT_ROUTE_MODE (off|shadow|placement), DS4_EXACT_ROUTE_MANIFEST_PATH,
DS4_EXACT_ROUTE_MANIFEST_SHA256, DS4_EXACT_ROUTE_WORKERS (4),
DS4_EXACT_ROUTE_TIMEOUT_MS (250), DS4_EXACT_ROUTE_MIN_GAIN_TOKENS (8192),
DS4_EXACT_ROUTE_MAX_LOAD_DELTA (0), DS4_EXACT_ROUTE_CANARY_BPS (0),
DS4_EXACT_ROUTE_CANARY_KEY,
DS4_KV_EVENT_MODE (off|shadow), DS4_KV_EVENT_LIVE_ENDPOINTS,
DS4_KV_EVENT_REPLAY_ENDPOINTS, DS4_KV_EVENT_TOPIC (empty),
DS4_KV_EVENT_REPLAY_LIMIT (1024), DS4_KV_EVENT_REPLAY_TAIL_LIMIT (64), and
DS4_KV_EVENT_TIMEOUT_MS (5000). Shadow mode requires one internal TCP live and
replay endpoint per upstream. `load` is an explicit baseline or an escape hatch
for engines without reusable prefix state; hybrid KDA models such as Kimi K3
still benefit from their engine's recurrent-state prefix cache.

`remote-shadow` derives a vLLM-compatible `/tokenize` payload from the same
parsed and sanitized request, but does not use its token IDs for routing. After
the client request completes, the payload enters a bounded, non-blocking queue
and is sent to the selected engine with `DS4_UPSTREAM_TOKEN`. Unsupported
endpoints, requests outside the configured byte window, a full queue, timeouts,
and malformed responses all fall back to the existing approximate router.
Shadow results expose only controlled outcome labels, duration, and token-count
histograms; prompt text and token IDs are neither logged nor retained.

`local-shadow` additionally renders DeepSeek-V4 prompts with Dynamo's native
Rust formatter and encodes them with NVIDIA `fastokens` on bounded blocking CPU
workers. The selected engine's authenticated `/tokenize` runs concurrently as
the authority. Exact IDs are compared in memory and discarded; a template
mismatch, unsupported tool-history or reasoning variant, worker failure, or
missing remote authority cannot affect the approximate routing decision. The
configured profile and expected SHA-256 must match the mounted tokenizer at
startup, preventing silent artifact drift from inheriting old golden results.

`DS4_KV_EVENT_MODE=shadow` constructs one supervised, fenced exact inventory
per upstream. It observes bounded vLLM live/replay events and exports only
controlled connection, trust, generation, batch/filter-outcome, and
resident-size metrics. Startup and every disconnect are untrusted; publisher
sequence zero or a complete bounded replay from zero establishes the initially
empty engine generation. If a requested replay fails after its range is known,
the consumer clears the old generation and retries one bounded full range from
zero after reconnecting; it does not wait for another live allocation merely
to rediscover the same boundary. A failed retry falls back to the live-event
gate instead of looping. Every retry still uses a fresh DEALER identity,
deadline, drain-through-validation, and exponential backoff. When selective
tokenization succeeds, the inventories
also feed an observation-only counterfactual: the selected engine's pre-request
cache hit comes from response usage, while every alternative lookup requires
the same trusted generation and inventory revision captured at approximate
decision time. This avoids counting KV blocks created by the request itself and
rejects a moving alternative under concurrent traffic. The existing
approximate decision and load snapshot remain authoritative unless the
separate `placement` mode is explicitly enabled.
`ds4proxy_exact_route_shadow_total` reports bounded `agree`, `would_move`,
`tie`, `all_zero`, and fail-closed outcomes, while the overlap/gain histograms
contain counts only. Raw token IDs and hashes never enter logs, journals, or
metrics.

The alternative `DS4_SNAPSHOT_ROUTE_MODE=shadow` path consumes authenticated
compact indexes from one local companion per upstream. Its LB-side telemetry
uses ordinal-only `engine-N` labels:
`ds4proxy_snapshot_route_enabled`, `ds4proxy_snapshot_route_ready`,
`ds4proxy_snapshot_route_attempts_active`,
`ds4proxy_snapshot_route_connections_active`,
`ds4proxy_snapshot_route_attempts_total`, and
`ds4proxy_snapshot_route_attempt_results_total`. Readiness means the actor has
published an authoritative inventory, not merely that its Unix socket is
connected. Every attempt kind and result series exists at zero before the
owner starts, and paths, endpoints, identities, secrets, and free-form errors
are excluded from labels.

Full replay is folded batch-by-batch into a private scratch inventory. The live
inventory remains fenced and unchanged until the requested end/cursor is
validated, then the completed generation is swapped in atomically. Invalid,
incomplete, or capacity-exceeded replay discards the scratch state. This keeps
memory bounded by the resulting index plus the transport's current batch
instead of retaining every decoded token vector until replay completes. Group
metadata shares the index's node-count capacity, and cancellation wakes the
blocking replay worker within 50ms so shutdown promptly releases scratch and
native receive buffers.

Replay sequence numbers are monotonic scheduler-step positions, not a promise
that every number has a published KV event. A replay is valid when its retained
events are strictly increasing, stay inside the requested range, and include
the requested upper boundary; absent intermediate numbers are authoritative
no-op steps. Duplicate, decreasing, out-of-range, or incomplete-tail responses
remain invalid and fail closed.
An otherwise valid main-attention store whose parent is no longer present is
counted as `orphaned_parent` and omitted. This is conservative: the exact index
can under-estimate reusable KV but cannot claim a child path it cannot prove.
Structural shape conflicts, duplicate hashes, path conflicts, and capacity
overflow still fence the complete inventory.

The cache scorecard uses upstream response usage as its authority.
`ds4proxy_cache_requests_total{endpoint,outcome}` classifies each completed
response as `cold` (reported cached tokens are zero), `partial` (greater than
zero but less than prompt tokens), `full` (at least the reported prompt-token
count), or `unknown` (missing/invalid usage). For streaming responses,
`ds4proxy_cache_ttft_seconds` records TTFT under the same bounded labels. The
existing prompt/cached-token counters remain the token-weighted view; compute
their ratio in PromQL rather than averaging per-request percentages. These are
observed response outcomes, not claims that every prompt token was cache
eligible.

`ds4proxy_kv_event_blocks_total{upstream,source,action}` counts accepted
`stored` and `removed` exact-index block mutations; replay and live traffic are
separate. `ds4proxy_kv_event_clears_total{upstream,source}` counts accepted
generation clears. A live removal is observable cache churn, but is not labeled
an eviction because the publisher event does not state why the block left.
Together with the resident-index gauges and native preemption counter, these
make capacity sweeps explainable without logging hashes or token IDs.
`GET /health` exposes the same content-free exact inventory as
`exact_inventory.{trusted,resident_blocks,resident_tokens}` under each opaque
replica index. It never returns an upstream address or cache key and does not
change the endpoint's serving-readiness status semantics.

`DS4_EXACT_ROUTE_MODE=shadow` moves admitted local tokenization before the
approximate decision, then immediately scores the same load snapshot against
all trusted exact inventories without changing candidate order. It requires
local-shadow tokenization, KV-event shadow, and a SHA-pinned compatibility
manifest. The manifest binds the tokenizer hash, renderer profile, model
ID/root/context, runtime `/version`, engine-image provenance, admitted request
classes, and synthetic token-vector goldens. Goldens are re-rendered at
startup; `/v1/models` and `/version` are continuously re-attested for every
engine. An identity change during tokenization, an ungoldened request shape,
an unavailable CPU permit, timeout, event gap, or inventory revision change
drops only the observation. The live approximate route remains authoritative.
`ds4proxy_exact_route_preroute_total`, duration histograms, and
`ds4proxy_compat_attested` expose controlled results. Generate a fresh manifest
after an engine/template update with `bench/tokenizer_manifest.py`; it never
prints or persists raw token IDs.

`DS4_EXACT_ROUTE_MODE=placement` is an explicit default-off canary mode. A
request is eligible to change placement only when its single, non-empty,
bounded `X-Session-ID` is selected by the keyed
`DS4_EXACT_ROUTE_CANARY_BPS` cohort (0 through 10,000 basis points). The HMAC
key is required only above zero and is never logged, journaled, exposed in
metrics, or forwarded upstream. Missing, duplicate, empty, or oversized
session IDs fail closed to shadow routing. Keep the key fixed while changing
the percentage so cohorts remain monotonic; set the percentage to zero before
rotating it. Zero is the instant rollback and still permits exact shadow
evaluation. An admitted treatment may promote only one unique exact-score
winner, requires at least
`DS4_EXACT_ROUTE_MIN_GAIN_TOKENS` additional cached tokens, and by default will
not move to an engine with any more load than the approximate choice. All
manifest attestation, tokenizer admission, event trust, inventory revision,
health, CPU-permit, and timeout fences remain mandatory; any failure preserves
the approximate route. `ds4proxy_exact_route_canary_total` reports only
bounded admission outcomes. `ds4proxy_exact_route_placement_total{mode="shadow"}`
evaluates the same gain/load policy without changing placement, while
`mode="control"` measures a valid non-treatment cohort and `mode="placement"`
distinguishes actual moves from gates and fail-closed fallbacks. Production
remains in `shadow` until a representative
counterfactual distribution and isolated node06 canary justify promotion.

Exact all-zero lookups also evaluate a separate cold-capacity counterfactual.
When every healthy inventory is trusted and revision-stable, the policy asks
whether the approximate choice holds at least one full prompt more resident
exact-index token IDs than the least-occupied replica, while retaining the
existing load gate. It emits only `would_balance`, `kept_balance_delta_gate`,
or `kept_balance_load_gate` outcomes plus a residency-delta histogram. This
path is shadow-only even when exact warm-prefix placement is enabled; it cannot
change candidate order until repeated capacity-boundary experiments qualify
the signal.

Set `DS4_ROUTE_JOURNAL=true` to emit privacy-bounded versioned `start`/`finish`
records to the process log. Records contain only process-local sequence IDs,
opaque upstream ordinals, sizes, route-state snapshots, latency, status, and
aggregate usage—never prompt text, fingerprints, request IDs, generated text, or
upstream hostnames. Replay alternative alpha/cap choices against the observed
snapshots without affecting live traffic:

    docker logs ds4-loadbalancer 2>&1 | \
      python3 bench/route_replay.py - --alphas 1,2,4,8 --caps 8,16,32,64

This is a static counterfactual: it holds cache and load snapshots fixed. It
does not simulate the future cache contents that alternative earlier choices
would have produced.

Journal v3 records `first_byte_ms` separately and defines `ttft_ms` as the
first non-empty generated content, reasoning, or tool-call delta. Replay keeps
v1/v2 compatibility but labels their historical `ttft_ms` values as first-byte
latency rather than silently mixing the two timing semantics.

Score equality prefers deeper raw overlap. Load still overrides affinity when
its score is strictly higher, but the exact boundary no longer turns a warm-to-
cold migration into a round-robin coin flip. Replay accepts `--tie-break`
to compare this rule with the legacy load-neutral equality behavior.

## Develop

    cargo fmt --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked
    cargo build --release --locked
    python3 bench/agentbench.py validate
    python3 -m unittest discover -s bench -p 'test_*.py'

The Rust cutover is complete. Frozen legacy fingerprint vectors and live HTTP
tests cover sanitization, failover, route correlation, usage streaming, and
model metadata rewriting. Drone is the only CI system: strict Rust lint/tests,
the GPU-free agent protocol suite, and Compose validation run in parallel. The
post-merge publishers build and push the LB and companion images only after the
quality gate succeeds.

Release builds share a public, content-keyed Rust dependency base. Ordinary
source changes therefore compile only this crate, fully offline, even on
Drone's fresh Docker 20.10 daemons. If `Cargo.toml`, `Cargo.lock`,
`rust-toolchain.toml`, or `Dockerfile.deps` changes, refresh the committed key
and its Docker/Drone references first:

    python3 bench/rust_deps_image.py --update
    python3 bench/rust_deps_image.py

Drone publishes the new dependency image before either release image on the
same `main` build. To bootstrap that key locally before it exists in GHCR:

    deps_ref=$(python3 bench/rust_deps_image.py --print-reference)
    docker build -f Dockerfile.deps -t "$deps_ref" .
    docker build --build-arg RUST_DEPS_IMAGE="$deps_ref" .

This Drone server does not support path conditions. Each main-push publisher
therefore runs `bench/drone_publish_guard.sh` against the exact
push plan before starting Docker or logging in to GHCR. The Git-capable
`rust-fetch` step generates that plan atomically from the exact
`DRONE_COMMIT_BEFORE..DRONE_COMMIT_SHA` range; publisher containers consume only
revision-bound marker files because their command workspace may not expose
`.git`. CI/docs/benchmark/deployment-only changes create no markers and publish
nothing. Invalid, unfetchable, mismatched, or empty ranges fail closed.

Measure the request-preparation hot path before and after tokenizer work:

    cargo run --release --locked --example preparation_bench
    cargo run --release --locked --example kv_wire_bench
    cargo run --release --locked --example exact_index_bench
    cargo run --release --locked --example kv_zmq_probe -- LIVE_ENDPOINT REPLAY_ENDPOINT [TOPIC]
    cargo run --release --locked --example local_tokenizer_probe -- /path/to/tokenizer.json

See [ROADMAP.md](ROADMAP.md), [EXPERIMENTS.md](EXPERIMENTS.md), and
[AGENTS.md](AGENTS.md) (node06 test/bench workflow).

For reproducible node06 work, `bench/context_frontier.py` measures cold
prefill, warm TTFT, decode throughput, and DSpark acceptance from 2K through
the advertised context boundary. `bash bench/capture_node06.sh` records the
effective runtime configuration and topology without emitting credentials.
`bench/codebench.py` runs deterministic code or prose decode gates with true
usage-token accounting. Point `METRICS_URL` at a direct engine metrics endpoint
(or provide comma-separated `METRICS_URLS`) to record draft steps, draft-token
acceptance, mean accepted tokens, and effective tokens per speculative step in
the same result. Compare acceptance together with effective tokens/step and
throughput: capacity pruning can raise the percentage simply by shrinking the
draft-token denominator.

    METRICS_URL=http://127.0.0.1:8013/metrics \
      python3 bench/codebench.py http://127.0.0.1:8013 deepseek-v4-flash 256 16 3

For a matched engine/image A/B, run the standard code+prose c1/c8/c16 matrix
against a direct engine endpoint and capture its JSONL output:

    bench/engine_matrix.sh http://127.0.0.1:8013 deepseek-v4-flash fixed \
      | tee fixed.jsonl

`bench/agentbench.py` validates a committed synthetic DeepSeek-V4 corpus for
streaming, DSML leakage, typed/parallel tool calls, and reasoning/tool history.
Live results contain only structural outcomes, timings, usage, and deployment
provenance. On node06, `bench/node06_agent_metadata.sh` produces that
provenance; `bench/agent_matrix.sh` runs deterministic/official-agentic,
short/long-prefix, cold/warm, and c1/c8/c16 cells. Narrow its environment lists
for development and reserve `AGENT_RUNS=3` for final qualification.

`bench/candidate_gate.py` turns engine qualification into a fail-fast,
resumable state machine. It binds a five-case deterministic agent correctness
smoke, a code/prose c8 scout, and the full direct-engine matrix to one immutable
image/process/plan identity. Each boundary rejects a restart, receipt mismatch,
late JIT compilation, or CUDA/NCCL/OOM/Xid/runtime marker before more GPU work
is scheduled. Its JSONL journal is content-free; existing privacy-safe child
results are stored as hashed mode-0600 artifacts. `bench/engine_matrix.sh`
accepts `ENGINE_WORKLOADS`, `ENGINE_CONCURRENCIES`, and `ENGINE_RUNS` for the
scout while preserving the original six-cell defaults.

`bench/route_replay.py` sweeps router policies over privacy-bounded live
decision records and splits observed warm/cold outcome latency. For native
KV-event feasibility, `bench/tokenize_bench.py` measures the exact-tokenization
hot-path cost; `bench/tokenizer_parity.py` checks `/tokenize` counts and
in-memory ID stability against real chat prompt usage without printing prompts
or IDs; `bench/kv_event_probe.py` runs only inside a trusted vLLM environment
and summarizes event continuity/volume without logging the token IDs or hashes
carried by raw events. `bench/kv_event_replay_probe.py` requests retained replay
and reports only bounded sequence, geometry, parent-order, and per-group
removal counts while keeping all identifiers process-local.
`bench/forced_exact_miss.py` warms a synthetic long prompt directly on one
engine and sends it through the proxy, creating a reproducible exact-versus-
approximate disagreement without printing the prompt or token IDs.
For scheduler isolation trials, `bench/mixed_bench.py` accepts `METRICS_URL`
and records engine-native queue/prefill histogram deltas, preemptions, and peak
running/waiting/KV gauges alongside per-request TTFT and decode throughput.
Point it at the same direct engine and keep unrelated production traffic off
that engine while interpreting the deltas.
`bench/cachebench.py` generates fresh-salt synthetic app working sets and
reports request/token reuse, TTFT, route split, reuse distance, prefill/queue
time, preemptions, cache outcomes, reuse-wave survival, and per-replica exact
inventory before/after snapshots keyed only by route ordinal. Negative
resident changes are preserved so a capacity cliff cannot be mistaken for a
counter reset; changes are `null` unless that inventory was trusted at both
snapshot boundaries. Initial-wave prompt-token total and mean make byte-shape
drift visible before comparing capacity cells. The same cell reports zero-safe
shadow deltas for exact agreement and cold-residency `would_balance`,
delta-gate, load-gate, and
all-zero decisions, avoiding manual production-wide counter joins.
`--concurrency 2`
uses both TP4 engine pairs while retaining a barrier between each cold/reuse
wave, so a reuse cannot race its unfinished cold request. With both engine
metric URLs and the LB metric URL supplied, `--require-reconciled` fails unless
response usage, LB counters, native prompt/cache counters, native prefix
query/hit counters, and request sample counts all agree within the configured
tolerance. Long cells can add `--progress-every 2` to emit content-free
completion counts and elapsed time to stderr without contaminating final JSONL
summaries on stdout.

For direct engine A/Bs, generate one identity file per engine before accepting
benchmark output. The capture includes immutable image/repository digests,
model/tokenizer revisions and artifact hashes, runtime package versions,
container lifetime, CPU/NUMA placement, a topology hash, an allow-listed
effective serving contract, and a secret-independent argv hash. Supplying an
upstream qualification receipt hard-fails mismatched image, digest,
model/tokenizer, or observed runtime package identity:

```bash
bench/node06_engine_metadata.sh /tmp/engine-b.json dspark-0731-b \
  /tmp/infernal-invocation-r4-receipt.json
```

The raw serving command, API keys, hostnames, prompts, and token IDs are never
written to the output. On Docker's containerd image store, the local image ID
can be the manifest descriptor while an upstream receipt calls the manifest
config digest its image ID; the capture records and verifies both explicitly.

Direct decode cells use `bench/engine_metrics.py` for one normalized
speculation record. It distinguishes enabled, target-only, unavailable,
incomplete, reset, no-draft, and contaminated intervals; reports the strict
accepted/proposed denominator, proposals and accepts per speculative step,
effective tokens per target step, and bounded per-position counts; and marks
the interval reconciled only when native generation-token/request deltas equal
the benchmark client's authoritative usage and success counts. Never compare
acceptance from a contaminated interval.
