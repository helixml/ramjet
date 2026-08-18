# Changelog

## 0.2.0 — 2026-08-18

Renames the project to ramjet, adds the machine-view dashboard and multi-model
serving, and makes the cache-hit number reportable against engines that do not
return cached-token usage. The `ds4proxy_` metric prefix is deliberately
unchanged for dashboard continuity.

### Breaking

- Environment prefix `MD_` is now `RJ_`, request headers are `X-Ramjet-*`, and
  the benchmark harness prefix `MINI_DYNAMO_` is now `RAMJET_`.
- Images publish as `ghcr.io/helixml/ramjet` and
  `ghcr.io/helixml/ramjet:companion-*`.
- `build_engine_sample` returns `EngineScrape` rather than `EngineSample`, so
  callers take `.sample` for the published shape.

### Machine view

- New observation-only dashboard on the loopback metrics listener: Overview,
  Serving, GPUs, and System tabs, Helix branding, and a token calendar.
- Per-GPU utilization, clocks, power, throttle reasons, and per-device rows;
  host CPU, memory, disk pressure, and network from the loopback host agent.
- Hourly token history with two heatmaps, persisted across restarts via
  `RJ_MACHINEVIEW_STATE_PATH`.
- Live serving metrics stream over a WebSocket; the REST series API remains.
- Cache-hit ratio now falls back to the engines' own
  `vllm:prefix_cache_{hits,queries}_total` when responses never populate
  `prompt_tokens_details.cached_tokens`. The fallback is token-weighted across
  engines rather than a mean of per-engine percentages, fills only an absent
  value, and publishes its provenance as `serving.cache_hit_source`. A quiet
  interval still reports absence instead of a fabricated 0%.

### Serving

- Multi-model support, and multimodal content is no longer dropped.
- Qwen3.8-27B-FP8 serving profile on node06, with generated topologies covering
  1 to 8 GPUs.
- Engine `top_p` defaults to 0.95 for tool-call safety.
- Idle-driven single-engine drain policy: publishes `desired_running` and
  `safe_to_stop` per upstream for a separately privileged actor to converge,
  and keeps the drain flag distinct from health so a parked replica is never
  read as a failing one.
- Phase-aware serving cost controls, bounded output-limit telemetry, and
  correctness-gated SLO Pareto reporting.
- Fail open instead of shedding when every readiness probe starves.
- Projected cold-residency telemetry, kept as an observation-only
  counterfactual separate from raw exact residency.

### Experimental and disabled by default

- Authenticated snapshot companion recovery gate, compact replay classification
  and orphaned-block filtering, and hardened host authority setup.
- Serving-runtime identity and admission: image-derived serving authority, live
  vLLM renderer identity, EngineCore runtime binding, a diagnostic identity
  endpoint, isolated persistent JIT caches, and the durable DSpark degeneration
  guard.
- Session-affinity shadow replay and bounded served-request shadow soak.

These paths still cannot affect ordinary routing or health unless an operator
explicitly enables their validated gates.

### Operations

- Benchmarks gate on chassis intake-air temperature rather than GPU
  temperature, with continuous inference capped per run. A GPU defends itself
  by throttling; facility cooling has no such backstop.
- Node06 cooling moratorium is enforced in the guard and P2P harness, and is
  lifted per named supervised window rather than globally.
- Release publishing uses a digest-pinned unprivileged Kaniko executor behind
  revision-bound markers, from the content-keyed release-tools image.

### Qualification

- 572 Rust tests across the crate and 7 integration/adversarial/E2E suites,
  plus 475 Python protocol, benchmark, and Compose tests.
- Node06 8× RTX PRO 6000 whole-box aggregate: 7,890.9 output tok/s at
  c256/max256 on Qwen3.8-27B, 1,891.2 tok/s at c24/max256 on DeepSeek-V4-Flash.

## 0.1.0 — 2026-08-13

First public Rust release.

### Stable serving surface

- OpenAI-compatible streaming reverse proxy with request sanitization and
  model-context rewriting.
- Prefix-locality plus weighted-load routing across healthy replicas.
- Health-gated failover and a replica-aware `/health` endpoint.
- Immediate upstream cancellation when the downstream client disconnects.
- Prometheus request, TTFT, usage, cache-outcome, route, load, and health
  metrics under the stable `ds4proxy_` prefix.
- Privacy-bounded decision journaling and offline policy replay.
- Bounded local/remote tokenizer observation that always falls back to the
  approximate router.

### Experimental and disabled by default

- Exact vLLM KV-event shadow inventories and placement canaries.
- Authenticated compact snapshot companions and hot engine-attestation
  rotation.
- Production snapshot Compose/Caddy admission artifacts.

These experimental paths cannot affect ordinary routing or health unless an
operator explicitly enables their validated gates.

### Qualification

- 330 Rust unit tests plus 38 integration/adversarial/E2E tests before the
  release metadata cut.
- Node06 8× RTX PRO 6000 serving control: 1,820–1,844 output tok/s at
  c24/max256, with 144/144 successful requests.
- Concurrent same-app throughput improved from 298 to 469 tok/s versus the
  original load-blind behavior, while request preparation is about 10× faster
  than the retired Go implementation at 256KiB–2MiB request sizes.
