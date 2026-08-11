# mini-dynamo

KV-cache-locality-aware load balancer for OpenAI-compatible inference
engines. Single static binary; drop-in replacement for `ds4-loadbalancer`
(same env vars, same `ds4proxy_*` metrics), plus an overlap-scored router:

    score(upstream) = prefixOverlapBlocks − alpha × inflight

Conversations stick to their warm engine, sessions that share prompt
templates co-locate, cold big prefills go to the quietest engine, and load
overrides affinity when it matters. See DESIGN.md for the full story and
roadmap (NVIDIA Dynamo, Kimi K3/KDA, and DwarfStar are the acknowledged
influences).

## Run

    DS4_UPSTREAM=http://engine-a:8000,http://engine-b:8000 \
    DS4_UPSTREAM_TOKEN=<bearer for engine probes> \
    ./mini-dynamo
    # API :8000, Prometheus :9090 (/metrics, /metrics/upstream/{i})

Key env (all optional): DS4_ADVERTISE_CTX_MARGIN (16384),
DS4_MAX_TOKENS_STRIP (100000), DS4_ROUTE_ALPHA (4), DS4_ROUTE_CHUNK_BYTES
(2048), DS4_ROUTE_MAX_PREFIX_BYTES (262144), DS4_ROUTE_INDEX_CAPACITY
(100000), DS4_AFFINITY (prefix|load).

## Develop

    go test ./...
    go build ./cmd/mini-dynamo

See [ROADMAP.md](ROADMAP.md) and [AGENTS.md](AGENTS.md) (node06 test/bench workflow).
