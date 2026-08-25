<div align="center">

<h1>ramjet</h1>
<h3>Warm intake. Balanced burn.</h3>
<p>A compact Rust router that puts each OpenAI-compatible inference request on<br>the healthy GPU replica where it can do the least repeated work.</p>
<p>
  <!-- The release and license badges were API-backed and rendered "repo not
       found" because this repository is private: shields.io queries the GitHub
       API anonymously. The release and LICENSE themselves are fine. If the
       repository is ever made public, they can be restored as:
         https://img.shields.io/github/v/release/helixml/ramjet
         https://img.shields.io/github/license/helixml/ramjet -->
  <a href="rust-toolchain.toml"><img alt="Rust 1.95 or newer" src="https://img.shields.io/badge/Rust-1.95%2B-f06a35?style=flat-square"></a>
</p>

</div>

<p align="center">
  <img src="docs/assets/deployment.svg" alt="An incoming prompt is scored by ramjet and routed to the GPU replica with the best combination of reusable prefix and available capacity" width="1100">
</p>

ramjet sits between your clients and replicated model servers. It keeps
conversations and shared system prompts near warm cache state, then lets live
load override affinity before one replica becomes a hotspot. Clients keep the
same OpenAI API; engines need no ramjet-specific integration.

## Why it exists

| Reuse more | Queue less | Fail cleanly |
| --- | --- | --- |
| Bounded prefix fingerprints find the replica most likely to reuse prior work. | Size-weighted reservations spread cold prefills and concurrent decodes. | Active health probes, retryable failover, and immediate disconnect cancellation keep capacity honest. |

The ordinary router is stateless, privacy-bounded, and deliberately useful
without raw KV-cache events. Optional DSpark enforcement persists only opaque
quarantine commitments so an LB restart cannot forget a bad EngineCore. The
production path remains the proxy plus your existing OpenAI-compatible engines.

## Built-in dashboard

System overview to see how your node is doing:

<img width="1441" height="1221" alt="image" src="https://github.com/user-attachments/assets/05deee62-4fcf-4220-ac77-bb318b2ce8ba" />

And specific serving tab:

<img width="1419" height="1214" alt="image" src="https://github.com/user-attachments/assets/6703a7a9-53b4-4d9a-ab07-7389a96fc684" />

You can also just plug it into prometheus, `/metrics` API is available. 

## Measured on real hardware

Reference stack: DeepSeek-V4-Flash-0731, two TP4 replicas, 8× RTX PRO 6000.

| Result | Measured outcome |
| --- | ---: |
| Shared-app concurrency, load-blind → ramjet | **298 → 469 output tok/s · 1.57×** |
| Fresh 3-app × 4-session locality run | **82.5% cached prompt tokens** |
| Whole-box deterministic code, c24/max256 | **1,820–1,844 output tok/s** |

These are workload results, not theoretical peaks. Reproduce them from
[RESULTS.md](RESULTS.md); inspect every accepted and rejected experiment in
[EXPERIMENTS.md](EXPERIMENTS.md).

### Models with a validated stack

All measured on node06 — 8× RTX PRO 6000 Blackwell; the engine topology is
listed per row. The full-box column reports the best qualified saturation
point recorded for that stack, not a shared concurrency level.

| Model | Served as | Decode @ c1 | Best qualified full-box throughput | Measured shape | Compose |
| --- | --- | ---: | ---: | --- | --- |
| DeepSeek-V4-Flash (sparse MoE) | `deepseek-v4-flash` | 245.1 tok/s | 1,891.2 tok/s | 2× TP4, c24/max256 | [`deploy/dspark_0731`](deploy/dspark_0731/docker-compose.yaml) |
| Qwen3.8-27B FP8 (dense, vLLM) | `qwen3.8-27b` | 77 tok/s · 121 with MTP | 7,890.9 tok/s | 2× TP4, c256/max256, MTP off | [`deploy/qwen38_27b`](deploy/qwen38_27b/docker-compose.yaml) |
| Qwen3.8-27B NVFP4/BF16 head (dense, SGLang + DFlash2) | `qwen3.8-27b` | 153.3 tok/s greedy median · +7.5% matched A/B | Not yet requalified (former Inferact target: 7,882.6 tok/s) | 8× TP1, 208 slots, bf16 SSM, DFlash2 on | [`deploy/qwen38_27b`](deploy/qwen38_27b/docker-compose.yaml) |

Neither model — and neither Qwen stack — is simply better. Single-stream
decode is what an interactive user feels; the full-box figure is a capacity
landmark for a saturated agent fleet. These maxima come from separate
model-specific workloads, so they are not a matched head-to-head benchmark.
The vLLM row's saturation result has MTP off because speculation improves
low-concurrency latency but wastes rejected drafts once the batch saturates
the GPU. The current SGLang production target is RadixArk's immutable
BF16-`lm_head` checkpoint. Its matched one-engine canary measured 153.3 tok/s
against 142.6 for the former Inferact target (+7.5%), with the same 7/8
objective answers and 20/25 deterministic agent-protocol cases. The smaller
target exposes 26 running slots and 582,246 KV tokens per engine: 208 slots
across the fleet. Full-box saturation has not yet been requalified on these
weights; the former Inferact target reached 7,882.6 tok/s, within 0.1% of the
vLLM reference. On that earlier SGLang stack, bf16 SSM state reduced c128 TTFT
p95 from 3.99s to 0.221s. The same
3-app × 4-session × 2-turn locality run measured **87.3% cached prompt
tokens**, and 12 concurrent same-app requests spread across 7 of 8 engines
at 714 tok/s. Its cost is cold long-context prefill: a 196K-token first
turn pays ~57s of TTFT on one GPU, with prefix-cached follow-ups at 2–4s.
[Model profiles](docs/models.md) covers the sizing, sharding, and
speculative-decoding trade-offs behind these numbers.

## Start in one minute

For existing engines, the upstream list is normally the only setting you need:

```yaml
services:
  ramjet:
    image: ghcr.io/helixml/ramjet:v0.4.0@sha256:467e7edf40c8fcad29e741cbba52ca571cbae0261d94cff008aa6bcdb737ea1b
    restart: unless-stopped
    ports:
      - "8000:8000" # OpenAI API + /health
      - "9090:9090" # Prometheus
    environment:
      RJ_UPSTREAM: http://model-server-1:8000,http://model-server-2:8000
      # RJ_UPSTREAM_TOKEN: ${MODEL_SERVER_API_KEY} # if required
```

```bash
docker compose up -d
curl --fail http://localhost:8000/health
```

The example pins a released image by immutable digest; see
[`CHANGELOG.md`](CHANGELOG.md) for what each version contains.
Safe defaults enable locality/load routing and keep tokenizer, raw KV-event,
exact-placement, and snapshot paths off. See the complete
[configuration table](docs/configuration.md), or start from the
[eight-replica Compose stack](deploy/qwen38_27b/docker-compose.yaml) currently
running in production. The
[two-replica DeepSeek-V4-Flash stack](deploy/dspark_0731/docker-compose.yaml)
is the previous deployment, kept as a reviewed alternative and rollback
record.

> **Backend compatibility:** `model-server-1` and `model-server-2` are example
> Docker DNS names—replace them with your backends. The default router is not
> tied to vLLM: it forwards OpenAI-compatible APIs and health-checks each server
> with `GET /v1/models`. The opt-in `/tokenize`, KV-event, exact-routing, and
> snapshot research paths are currently designed for vLLM/DSpark.

## The routing rule

```text
score(replica) = min(prefix overlap, affinity cap) − α × live load
```

ramjet fingerprints only a bounded prefix, scores every healthy replica,
and reserves load before forwarding. Warm state wins when it is valuable; idle
capacity wins when reuse no longer pays for the queue. Score ties prefer the
deeper raw overlap.

## Production surface

- OpenAI-compatible chat/completions, streaming, reasoning, and tool calls.
- `ok`, `degraded`, and `unhealthy` readiness at `GET /health`.
- Optional SHA-pinned model/template compatibility admission for engines that
  expose the atomic identity contract, with fail-closed per-replica recovery;
  the node06 guide includes an opt-in, no-extra-hop vLLM middleware candidate.
- Optional DSpark reliability observation and sticky per-replica quarantine
  when active K5 acceptance collapses to zero across multiple complete metric
  windows; enforcement fsyncs an opaque EngineCore commitment and only a
  different compatibility-attested EngineCore can durably rearm it. A
  precommitted dirty marker keeps unresolved replicas fenced after an unclean
  LB exit or failed state mutation.
- Stable `ramjet_*` Prometheus metrics on port `9090`.
- Opaque `X-Ramjet-Upstream` route correlation without leaking hosts.
- Bounded memory, request sanitization, model metadata rewriting, and upstream
  cancellation when the client disappears.

Exact tokenization, fenced KV indexes, authenticated snapshot companions,
exact-placement canaries, and session-affinity shadow telemetry remain opt-in
research surfaces. The session path cannot change placement. These paths fail
closed and are not dependencies of ordinary serving.

> **Naming:** the project was renamed from ramjet to ramjet. Settings now
> use the `RJ_*` prefix and responses carry `X-Ramjet-*` headers; the retired
> `MD_*` prefix is refused at startup rather than silently ignored, so a stale
> overlay fails loudly instead of running a differently tuned proxy. The
> `ramjet_*` metric names are deliberately unchanged so existing Grafana
> history keeps resolving.

## Operate it

| Task | Start here |
| --- | --- |
| Deploy or roll back | [Docker Compose operator guide](deploy/dspark_0731/README.md) |
| Configure the router | [Environment reference](docs/configuration.md) |
| Serve a different model | [Model profiles](docs/models.md) |
| Understand the design | [Architecture and routing model](DESIGN.md) |
| Inspect current work | [Roadmap](ROADMAP.md) |

Codex-compatible repo skills are included for repeatable node operations:
[`$deploy-ramjet`](.agents/skills/deploy-ramjet/SKILL.md),
[`$optimize-ramjet-node`](.agents/skills/optimize-ramjet-node/SKILL.md),
[`$load-test-ramjet-node`](.agents/skills/load-test-ramjet-node/SKILL.md),
and
[`$troubleshoot-ramjet-node`](.agents/skills/troubleshoot-ramjet-node/SKILL.md).

## Develop

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

<details>
<summary>Privacy-safe production-shape replay</summary>

For privacy-safe production-shape validation, `bench/agent_trace.py` accepts
only numeric/enumerated trace shapes and synthesizes all request content. A
bounded `/tokenize` preflight adjusts for the active chat-template overhead;
authoritative response usage still enforces the token-density gate. See the
[sovereign trace replay contract](bench/agent_cases/README.md#sovereign-trace-shape-replay).

</details>

See [AGENTS.md](AGENTS.md) for the GPU-free inner loop, full release gate, and
node06 benchmark contract.

## License

[Apache-2.0](LICENSE).
