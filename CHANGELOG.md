# Changelog

## Unreleased

- `RJ_UPSTREAM_RANK_PROBE=all` extends the one-token generation probe from
  DP-rank upstreams to every upstream (DP ranks stay pinned with
  `routed_dp_rank`). Two SGLang replicas on one H200 host sharing `/dev/shm`
  (`--ipc host`) can cross-wire: when their main processes get the same PID,
  one overwrites the other's `multi_tokenizer_args_<pid>` segment, and that
  replica keeps answering `/health` and `/v1/models` while no request ever
  reaches its scheduler. Only a generation catches it. A recent real
  completion still overrides a probe timeout. System One upstreams and parked
  engines are never sent the generation.
- Listen addresses are configurable: `RJ_API_ADDR` (default `0.0.0.0:8000`)
  and `RJ_METRICS_ADDR` (default `0.0.0.0:9090`) let deployments whose
  containers share the host network move off ports the host already uses, for
  example `RJ_METRICS_ADDR=127.0.0.1:19090`. Values must be `IP:port` and are
  validated at startup.

## 0.7.0 — 2026-09-29

- Multi-node routing: one ramjet can front a fleet of nodes
  (`docs/multi-node.md`).
  - `RJ_ROUTE_AFFINITY_BASIS=relative` scores each replica against the
    warmest serving peer of its model. `marginal`'s floor is the least-warm
    peer, and beyond two replicas that peer is usually cold, so it fell back to
    `absolute`. In `tests/fleet_routing_simulation.rs` at 40 replicas,
    `relative` kept 90% of agent turns on the replica holding their session,
    against 51% for `absolute` and `marginal`. At two replicas it makes the
    same decisions as `marginal`. `route_replay.py --affinity-bases` replays
    it.
  - `RJ_TOPOLOGY_FILE` describes the fleet as named nodes and their replicas
    instead of index-aligned comma lists. `ramjet_upstream_info{upstream,node}`
    and the `/health` replica entries carry the node name.
  - Healthy replicas are probed concurrently under `http` admission, so a
    40-replica probe round no longer serializes 5s timeouts.
  - `RJ_ROUTE_MAX_ATTEMPTS` bounds failover, and
    `RJ_UPSTREAM_CONNECT_TIMEOUT_MS` (default 30000, unchanged) sets the
    connect budget for replicas on other machines.
  - SGLang data-parallel attention ranks can be upstreams:
    `RJ_UPSTREAM_DP_RANKS`, or `"dp_ranks": N` on a topology replica, pins
    each upstream's requests to one rank with `routed_dp_rank`. On GLM-5.3
    DP8 with 16 agent developers, prefix routing across ranks gave 71.9
    turns/min and 92.9% cached prompt, against 43.0 and 65.6% for SGLang's
    own round robin.
  - Failover prefers other nodes: after a failure, replicas sharing the
    failed one's node move behind the rest of the attempt budget, and a
    refused connection marks every DP rank of that engine down.
  - `RJ_UPSTREAM_RANK_PROBE=on` probes each DP-rank upstream with a one-token
    generation pinned to its rank, fencing a wedged rank that `/health` and
    `/v1/models` still report as up. Recent real completions override a probe
    timeout, so busy ranks stay routable.
  - `examples/route_scale_bench.rs` measures scoring cost: 0.3ms median at 40
    replicas in the fully warm worst case, against 2.7ms to fingerprint the
    same prompt outside the lock.
- Upstream connections now expire after 4s idle
  (`RJ_UPSTREAM_POOL_IDLE_TIMEOUT_MS`), below the 5s keep-alive of vLLM and
  SGLang. reqwest's 90s default reused sockets the engine was closing: under a
  96-developer agent load on two SGLang replicas that produced 20 failovers
  and a burst of 502s while both engines were healthy, and each failover
  marked a healthy replica down until its next 15s probe. Failovers are now
  counted in `ramjet_upstream_failovers_total{from,to,reason}` and logged with
  their reason.
- Adds `RJ_ROUTE_AFFINITY_BASIS=marginal` (default `absolute`, unchanged).
  With a shared system prompt longer than the 64KiB affinity cap, every
  replica scored full affinity and one load unit moved an agent session off
  the replica holding its history. `marginal` credits only the prefix beyond
  the least-warm serving peer of the same model and API profile, so session
  history outweighs small load differences while the cap still bounds the
  trade. Route journal v14 records the basis; `route_replay.py
  --affinity-bases` and the journal archive admit it.
- The Qwen/GLM/Kev deployment now defaults the long-prompt lane off
  (`RJ_ROUTE_LONG_PROMPT_BYTES=0`). Two concurrent ~290k-token GLM
  conversations do not fit one replica's KV pool, so confining both to one
  replica cost 23-30s per turn instead of ~4s. See EXPERIMENTS.md.
- `bench/snapshot_roi.py` replays archived route journals against
  hypothetical cache-retention horizons and sizes the snapshot tier they would
  need. On 13.6 days of GLM traffic infinite retention would have avoided only
  0.44% of prompt tokens, so no CPU/NVMe snapshot tier is planned
  (`docs/glm_snapshot_roi.md`).
- `serving_cost_audit.py` accepts journal v11/v12 output-limit telemetry, and
  a test now fails when any journal consumer rejects the version the LB emits.

## 0.6.2 — 2026-09-25

### GLM-5.3 prefix-cache capacity and SwiGLU clamp (node06)

- The GLM TP2 replicas cap cached linear-attention states per radix path
  (`--mamba-max-states-per-path=2`) and add a 4GB-per-rank HiCache host tier.
  One ~310k-token prompt used to evict every other session's prefix through
  the 28-slot state pool; six 20k-token sessions now stay 99.6% cached across
  it, and twelve fit on the device instead of none.
- A derived image (`Dockerfile.swiglu-clamp`) routes GLM-5.3's
  `swiglu_limit = 10.0` into the SM120 W4A16 routed-expert kernel, which the
  pinned SGLang and FlashInfer dropped. GSM8K 96.29% -> 96.44%.
- Adds `bench/gsm8k_check.py`, `bench/prefix_eviction_probe.py`, and the
  one-replica `node06-engine-rollout.sh` owner.

### Long-prompt lane

- Adds an opt-in long-prompt lane. `RJ_ROUTE_LONG_PROMPT_BYTES` sets a
  request-body threshold and `RJ_ROUTE_LONG_PROMPT_UPSTREAMS` (`lane` or `-`
  per upstream) names the replicas that may serve prompts at or above it.
  A long request is restricted to its model's serving lane members after
  model/API ownership is applied; if none is serving it routes normally.
  Shorter requests and models without a lane member are unchanged. Unset,
  or a threshold of `0`, is off.
- Adds `ramjet_route_long_prompt_total{upstream,outcome}` (`lane` or
  `fallback`) and route-journal v12's fixed-label `long_request_lane`
  outcome; `route_replay.py` and `route_journal_archive.py` admit v12.
- The Qwen/GLM/Kev deployment defaults the lane to `glm53sm120-c` at
  600,000 bytes (about 150k tokens), protecting `glm53sm120-b`'s prefix cache
  from ~310k-token prefills.

### TypeSafe System One upstreams

- Adds a dense `RJ_UPSTREAM_APIS` ownership map and first-class
  `/v1/systemone` routing. API and model ownership are intersected before
  dispatch, including retries and fail-open.
- Adds System One model-schema readiness probes while preserving the public
  OpenAI-compatible `/v1/models` response.
- Adds the pinned node06 Kev-0.8B runtime and Qwen/GLM/Kev deployment recipe.

## 0.6.1 — 2026-09-15

- Fixed machine view's rolling cache-hit chart producing impossible negative
  percentages after a busy interval aged into an idle window. Empty windows
  now clear floating-point accumulator residue and correctly render as absent.
- Added a focused frontend regression test and made the pull-request and
  release quality pipelines build and test the machine-view UI.

## 0.6.0 — 2026-09-13

### Heterogeneous multi-model serving

- Ramjet can assign an explicit served-model owner to every upstream. Requests
  are routed only within that model's replica set, unknown models fail before
  an upstream is dialed, and `GET /v1/models` returns one combined,
  deduplicated model list.
- Machine view reports prompt, cached-prompt, and completion-token usage per
  model. Its Topology tab renders the authoritative model, tensor-parallel
  size, and GPU group for every live engine, including multiple replicas of
  one model.
- Added the qualified node06 topology with Qwen3.8-Flash-Next TP4 on GPUs 0-3
  and two GLM-5.3-Flash TP2 replicas on GPUs 4-5 and 6-7. The guarded rollout
  preserves Qwen and the established GLM replica while adding the second GLM
  engine, then proves both GLM owners receive traffic before an LB-only
  promotion.

### GLM-5.3-Flash qualification

- Added immutable NVIDIA vLLM and SM120 SGLang NVFP4 deployment recipes,
  model verification, argument preflight, parser-contract validation, guarded
  canaries, rollback paths, and backend-neutral prefill/decode metrics.
- Qualified the SGLang W4A16 TP2 recipe with a patched nullable `glm47` tool
  parser and 6,144-token chunked prefill. The DFlash2 candidate remains
  explicitly rejected because it did not pass the model's correctness and
  serving gates.
- Added the optional Qwen NVFP4 steering plugin and its escape-vector recipe
  as a separate experimental surface.

### Routing and operations

- Added default-off time-decayed prefix affinity
  (`RJ_ROUTE_AFFINITY_HORIZON_MODE=observe|enforce`). Served fingerprint
  blocks older than a replica's estimated eviction horizon earn no routing
  credit; the horizon is a fixed age or an LRU fill model calibrated from the
  engine's KV capacity and the tokens ramjet has served. Route journal v11
  records per-candidate block ages and `bench/route_replay.py --horizons`
  sweeps horizons offline. New metrics: `ramjet_route_affinity_horizon_total`,
  `ramjet_route_affinity_horizon_seconds`, `ramjet_route_stale_overlap_blocks`.
- Fixed adaptive exact-route attestation scope and recorded the guarded node06
  placement qualification.
- Added privacy-bounded route-journal rotation, compression, retention, and
  offline archive analysis with hardened systemd installation units.
- LB rollouts preserve the exact previous container under Docker Compose v5,
  keeping rollback outside candidate reconciliation.

## 0.5.0 — 2026-09-02

- Added a dedicated dashboard login backed by signed, persistent HttpOnly
  sessions. Adaptive control no longer reuses or renders `RJ_UPSTREAM_TOKEN`,
  and authenticated machine-view/adaptive APIs share one browser session.
- Added an owner-only JSONL topology audit trail plus a dashboard Engine Change
  History view for controller, transition, rollback, and engine start/stop
  events.

### Adaptive engine topology

- An optional controller inside the Ramjet process can drain routing and
  switch between label-verified, pre-created Docker engine profiles. Its
  Docker authority is limited to inspect/start/stop, profile state is durable,
  manual/recommend/auto modes are explicit, and every configured transition
  publishes its downtime requirement and estimate. Target startup uses the
  ordinary health/warmup gates and failures attempt an automatic rollback.
- Transition intent and every destructive phase are durably journaled before
  Docker mutation. A restart with an unfinished journal fences all profiles,
  keeps the dashboard available, and exposes an authenticated retry-rollback
  action that can restore the exact previously committed profile.
- Machine view adds an animated SVG Topology screen with per-GPU engine
  grouping, token ingress/egress, GPU utilization, normalized serving load,
  profile controls, a persistent authenticated session, and change history.
  Adaptive policy can use
  input, output, or total token throughput plus live in-flight/load signals;
  temperature never participates in topology selection.
- GPU utilization uses a bounded 15-second trailing average in the overview
  chart and topology diagram. This aligns short NVML observations with the
  token counter window, while missing host-agent samples remain unavailable
  instead of being rendered as zero utilization.
- The node06 Flash-Next Compose defines its qualified TP4 pair and a
  default-stopped TP8 candidate as two named shapes. The controller and host
  deployment tools share the same filesystem lock; the initial rollout stays
  manual until the TP8 crossover and automatic thresholds are qualified.
- Exact placement now requires authority from the currently routable
  candidates, so a deliberately stopped adaptive profile cannot disable cache
  placement for the active shape or participate with stale inventory.

## 0.4.0 — 2026-08-20

### Idle drain grows an actuator

- `RJ_IDLE_DRAIN_ACTUATOR=sleep` lets the LB carry out its own park decision
  through vLLM sleep mode (`POST /sleep` / `POST /wake_up` with the upstream
  token). Actuation is gated on `drain` mode; `observe` remains
  consequence-free and `off` deployments are unaffected. A parked or waking
  replica stays fenced from routing by a single conjunction applied in both
  the publish and post-actuation paths, because a sleeping vLLM engine hangs
  rather than refuses.
- `RJ_IDLE_DRAIN_RELEASE=utilization` releases an individually quiet replica
  while its peers serve, keyed to load pressure rather than request arrival.
  `RJ_IDLE_DRAIN_MAX_PARKED` bounds host memory: level-1 sleep does not
  return offloaded weights on wake, so read it as parks-per-container-
  lifetime. The closed-loop `engine_park_simulation` test exercises burst
  arrival at a parked replica, failed sleeps, and slow wakes against the same
  fence function the proxy applies.

### Serving recipes

- `deploy/qwen38_27b/` documents two qualified stacks side by side: the vLLM
  FP8+MTP topology family (full feature surface: KV events, sleep actuator,
  guards) and a new SGLang NVFP4+DFlash2 overlay
  (`topology.8gpu-sglang-dflash2.yaml`, eight single-GPU engines, fastest
  single-stream decode). Both serve the same model name so clients never
  change. The SGLang tool-call parser must be `qwen3_coder`; the tempting
  `qwen` name is the Qwen2.5 JSON detector and silently swallows Qwen3.8's
  XML tool calls.

### Machine view

- The Gen tok/s tile shows a 30s mean instead of a 30s max. The proxy books a
  request's whole completion count in the sample where it finished, so the
  max read one big agent turn's completion tick as thousands of tok/s the
  fleet never sustained.
- Serving samples carry `stream_tps_p50`/`stream_tps_p05`: windowed
  per-request decode-rate quantiles from the existing
  `ramjet_decode_tokens_per_second` histogram. A new Stream tok/s tile shows
  the median with the slowest-5% tail — the number a user's stream actually
  runs at, which the throughput counters could never answer.
- Tile sparkline hover is confined to the chart's own bounds; the crosshair
  and hover line no longer appear (mispositioned) from anywhere on the card.

## 0.3.0 — 2026-08-18

### Breaking

- Metrics are exported under the `ramjet_` prefix. Every name that began
  `ds4proxy_` now begins `ramjet_`; nothing else about the names, labels, or
  types changed. The prefix had survived two project renames because it was
  held back for dashboard continuity.

  **Prometheus has no history under the new names.** A panel or alert whose
  window spans the switch shows a gap rather than a join, and anything querying
  `ds4proxy_*` stops returning data at the moment the new binary starts. The
  canonical Grafana dashboard is updated in the same change, but any external
  dashboard, alert rule, recording rule, or script that greps a metric name has
  to be updated separately.

  Update the canonical dashboard mirror with
  `python3 deploy/monitoring/rtx6000pro/sync-dashboards.py ../infra` after
  taking this.

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
