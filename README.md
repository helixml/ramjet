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

## Run

    DS4_UPSTREAM=http://engine-a:8000,http://engine-b:8000 \
    DS4_UPSTREAM_TOKEN=<bearer for engine probes> \
    ./mini-dynamo
    # API :8000, Prometheus :9090 (/metrics, /metrics/upstream/{i})

Successful proxied responses include `X-Mini-Dynamo-Upstream: 0|1` for
opaque per-request route correlation; internal upstream names are not exposed.

Key env (all optional): DS4_ADVERTISE_CTX_MARGIN (16384),
DS4_MAX_TOKENS_STRIP (100000), DS4_ROUTE_ALPHA (4), DS4_ROUTE_CHUNK_BYTES
(2048), DS4_ROUTE_MAX_PREFIX_BYTES (2097152), DS4_ROUTE_MAX_OVERLAP_BLOCKS
(32), DS4_ROUTE_INDEX_CAPACITY (100000), DS4_ROUTE_LOAD_UNIT_BYTES (32768),
DS4_ROUTE_MAX_LOAD_UNITS (8),
DS4_AFFINITY (prefix|load), DS4_ROUTE_JOURNAL (false),
DS4_TOKENIZER_MODE (off|remote-shadow), DS4_TOKENIZER_MIN_BYTES (32768),
DS4_TOKENIZER_MAX_BYTES (2097152), DS4_TOKENIZER_WORKERS (1),
DS4_TOKENIZER_QUEUE_CAPACITY (8), DS4_TOKENIZER_TIMEOUT_MS (2000). `load` is an explicit baseline or an
escape hatch for engines without reusable prefix state; hybrid KDA models such
as Kimi K3 still benefit from their engine's recurrent-state prefix cache.

`remote-shadow` derives a vLLM-compatible `/tokenize` payload from the same
parsed and sanitized request, but does not use its token IDs for routing. After
the client request completes, the payload enters a bounded, non-blocking queue
and is sent to the selected engine with `DS4_UPSTREAM_TOKEN`. Unsupported
endpoints, requests outside the configured byte window, a full queue, timeouts,
and malformed responses all fall back to the existing approximate router.
Shadow results expose only controlled outcome labels, duration, and token-count
histograms; prompt text and token IDs are neither logged nor retained.

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

The Go implementation remains in-tree as the cutover reference. Rust tests
include Go-generated fingerprint goldens and live HTTP tests for sanitization,
failover, route correlation, usage streaming, and model metadata rewriting.
During the rewrite, keep both suites green:

    go test ./... && go vet ./... && test -z "$(gofmt -l .)"

Measure the request-preparation hot path before and after tokenizer work:

    cargo run --release --locked --example preparation_bench
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

`bench/route_replay.py` sweeps router policies over privacy-bounded live
decision records and splits observed warm/cold outcome latency. For native
KV-event feasibility, `bench/tokenize_bench.py` measures the exact-tokenization
hot-path cost; `bench/kv_event_probe.py` runs only inside a trusted vLLM
environment and summarizes event continuity/volume without logging the token
IDs or hashes carried by raw events.
