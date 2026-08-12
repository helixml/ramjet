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

The canonical node06 stack is
`deploy/node06/dspark_0731/docker-compose.yaml`. Its adjacent README documents
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
DS4_EXACT_ROUTE_MAX_LOAD_DELTA (0),
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
empty engine generation. When selective tokenization succeeds, the inventories
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

`DS4_EXACT_ROUTE_MODE=placement` is an explicit default-off canary mode. It may
promote only one unique exact-score winner, requires at least
`DS4_EXACT_ROUTE_MIN_GAIN_TOKENS` additional cached tokens, and by default will
not move to an engine with any more load than the approximate choice. All
manifest attestation, tokenizer admission, event trust, inventory revision,
health, CPU-permit, and timeout fences remain mandatory; any failure preserves
the approximate route. `ds4proxy_exact_route_placement_total{mode="shadow"}`
evaluates the same gain/load policy without changing placement, while
`mode="placement"` distinguishes actual moves from gates and fail-closed
fallbacks. Production remains in `shadow` until a representative
counterfactual distribution and isolated node06 canary justify promotion.

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

The Go implementation remains in-tree as the cutover reference. Rust tests
include Go-generated fingerprint goldens and live HTTP tests for sanitization,
failover, route correlation, usage streaming, and model metadata rewriting.
During the rewrite, keep both suites green:

    go test ./... && go vet ./... && test -z "$(gofmt -l .)"

GitHub Actions runs Rust format, strict Clippy, and tests with a pruned
dependency cache. Drone independently adds the release build and keeps the Go
parity oracle and GPU-free agent protocol suite green; its three language lanes
run in parallel. The post-merge Docker build is the second release-mode proof,
so GitHub does not duplicate a release link in the PR check. The image publisher
runs only after the post-merge Rust gate succeeds
and requires GHCR package write permission for this repository.

Measure the request-preparation hot path before and after tokenizer work:

    cargo run --release --locked --example preparation_bench
    cargo run --release --locked --example kv_wire_bench
    cargo run --release --locked --example exact_index_bench
    cargo run --release --locked --example kv_zmq_probe -- LIVE_ENDPOINT REPLAY_ENDPOINT [TOPIC]
    cargo run --release --locked --example local_tokenizer_probe -- /path/to/tokenizer.json
    go test ./pkg/proxy -run '^$' -bench BenchmarkPrepareLongPrompt -benchmem

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
time, preemptions, and cache outcomes. With both engine metric URLs and the LB
metric URL supplied, `--require-reconciled` fails unless response usage, LB
counters, native prompt/cache counters, native prefix query/hit counters, and
request sample counts all agree within the configured tolerance.
