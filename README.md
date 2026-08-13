# mini-dynamo

**Make every GPU feel warmer.** mini-dynamo is a fast, cache-aware router for
OpenAI-compatible inference servers. It sends each request to the healthy
replica with the best reusable prompt state—without piling work onto a busy
GPU.

<p align="center">
  <img src="docs/assets/deployment.svg" alt="mini-dynamo routing clients across two GPU inference engines" width="960">
</p>

```text
score(replica) = min(prefix overlap, affinity cap) - alpha × live load
```

## Why mini-dynamo?

- **More useful throughput.** Keep conversations and shared system prompts on
  warm replicas while spilling concurrent work to idle capacity.
- **Production-safe routing.** Health-aware failover, immediate cancellation,
  bounded memory, and a stateless Rust proxy.
- **Drop-in operations.** OpenAI-compatible streaming, Prometheus
  `ds4proxy_*` metrics, `/health`, and opaque per-request route correlation.

On the reference 8× RTX PRO 6000 stack, the overlap/load router improved a
shared-app concurrency test from **298 to 469 output tok/s (1.57×)** and reached
**82.5% cached prompt tokens** in a fresh multi-app locality run. See
[RESULTS.md](RESULTS.md) for workloads and [EXPERIMENTS.md](EXPERIMENTS.md) for
the evidence log.

## Run it

mini-dynamo has good defaults. For existing engines, set the upstream list and
start the container:

```yaml
services:
  mini-dynamo:
    image: ghcr.io/helixml/mini-dynamo:v0.1.0@sha256:62d949e0e6b3880796fab6c12f148f24d3f76449cb8397da6e81fe6e57dd70a1
    restart: unless-stopped
    ports:
      - "8000:8000" # OpenAI API + /health
      - "9090:9090" # Prometheus
    environment:
      DS4_UPSTREAM: http://engine-a:8000,http://engine-b:8000
      # DS4_UPSTREAM_TOKEN: ${VLLM_API_KEY} # only for protected engines
```

```bash
docker compose up -d
curl --fail http://localhost:8000/health
```

All tuning and experimental features are optional and off by default. See the
[configuration reference](docs/configuration.md) for every variable. For the
complete two-replica DeepSeek-V4/DSpark stack, use the canonical
[Docker Compose deployment](deploy/dspark_0731/docker-compose.yaml) and its
[operator guide](deploy/dspark_0731/README.md).

## How it works

mini-dynamo builds a bounded fingerprint of each reusable prompt prefix and
scores it against every healthy replica. A size-weighted reservation subtracts
live prefill/decode pressure, so affinity wins when it is useful and load wins
before a warm replica becomes a hotspot. If an engine fails, requests move to a
healthy peer; if a client disconnects, upstream work is cancelled promptly.

The v0.1 serving path uses approximate locality and does not depend on raw KV
state. Exact tokenization, KV-event indexes, authenticated snapshot companions,
and placement canaries are available for fail-closed shadow research and stay
off unless explicitly configured.

## Operate with an agent

Repo-scoped agent skills capture the safe workflows:

- [`$deploy-mini-dynamo`](.agents/skills/deploy-mini-dynamo/SKILL.md) — deploy
  Docker Compose on a GPU node.
- [`$optimize-mini-dynamo-node`](.agents/skills/optimize-mini-dynamo-node/SKILL.md)
  — benchmark and tune one variable at a time.
- [`$troubleshoot-mini-dynamo-node`](.agents/skills/troubleshoot-mini-dynamo-node/SKILL.md)
  — check GPUs, containers, health, metrics, logs, and basic requests.

## Develop

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Read [DESIGN.md](DESIGN.md) for internals, [ROADMAP.md](ROADMAP.md) for current
work, and [AGENTS.md](AGENTS.md) for the full development and node06 benchmark
contract. mini-dynamo is Apache-2.0 licensed.
