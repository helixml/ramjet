# mini-dynamo roadmap

Status legend: ✅ done · 🔨 in progress · ⬜ planned. Ordered by
value-per-effort given the current deployment (2 vLLM+DSpark TP4 instances on
node06). The design rationale for each lives in DESIGN.md.

## v0.1.0 — first public Rust release

The release boundary is the production-qualified proxy: locality/load routing,
health-gated failover, immediate client-cancellation propagation, compatibility
shims, usage/TTFT/cache metrics, privacy-bounded decision journals, and bounded
tokenizer observation. Exact KV events, snapshot companions, and placement are
experimental, off by default, and not release blockers. Session-incremental
tokenization, P/D, Kimi K3, and future engine candidates remain post-v0.1 work.

- ✅ **Parity routing kernel.** Rust 2024 implementation of typed config,
  canonical prompt preparation, chained fingerprints, bounded LRU indexes,
  overlap/load scoring, health ordering, exact-score tie policy, and RAII
  weighted-load accounting. Frozen legacy fingerprints are golden-tested.
- ✅ **Async compatibility data plane.** Axum/Tokio/Reqwest streaming proxy,
  bounded request bodies, request shims, model metadata rewrite, health probes,
  retryable-status failover, opaque route headers, true generated-token TTFT,
  privacy-bounded journal v3, and the existing `ds4proxy_*` metric names.
- ✅ **Immediate downstream-cancellation propagation.** The relay now selects
  on downstream closure and upstream reads concurrently, so dropping a client
  immediately drops the reqwest response stream even while the engine is
  silent. A loopback test proves upstream-body destruction plus inflight/load
  release; the production gate drained LB load and vLLM running state by the
  first 2.019s sample after a forced 2.000s timeout.
- ✅ **Health-gated serving contract.** `/health` reports opaque per-replica
  health plus aggregate `ok|degraded|unhealthy` readiness, returning 503 only
  when no replica can serve. Known-unhealthy replicas are removed from every
  serving/failover attempt. Unit tests cover exclusion, zero-healthy behavior,
  retryable failover, and probe recovery; a node06 negative canary sent 4/4
  requests only to the healthy replica.
- ✅ **Single-parse approximate preparation.** One JSON parse now feeds both
  compatibility mutations and canonical route fingerprints; cache observation
  reuses the prepared vector. Release-mode preparation is 0.49ms at 256KiB and
  4.53ms at 2MiB on the development host, about 10× faster than the original Go
  data path and 15% faster than the initial two-parse Rust implementation at
  2MiB. Keep `examples/preparation_bench.rs` as the pre-tokenizer baseline.
- ✅ **Rolling Rust node06 qualification.** Build and publish immutable
  `rust-*` images, then run locality, concurrent same-app, c24 aggregate, route
  telemetry, and occasional Helix workflow acceptance before promotion. A
  prior immutable Rust image remains the LB-only rollback; neither engine is
  restarted for proxy trials.
  Public r22 is live with both r34 publishers, manifest-attested pre-route
  exact scoring, and the placement policy in observation-only shadow mode.
  Exact state is not exposed to placement.
- ✅ **Bounded remote tokenizer shadow.** The one-pass boundary selectively
  derives chat/completion `/tokenize` payloads, then submits them only after the
  user request completes. Authenticated calls use a bounded non-blocking queue,
  fixed workers/timeouts/response caps, controlled metrics, and unconditional
  approximate-routing fallback; raw token IDs and prompts never enter logs.
- ✅ **Remote chat-template parity matrix.** Active vLLM completion usage and
  `/tokenize` agreed exactly in 13/13 plain, multi-turn, tools, tool-history,
  all seven reasoning levels, thinking-disabled, and normalized-content cases;
  repeated token IDs were stable in memory and never printed.
- ✅ **Bounded local Rust tokenizer pool.** The read-only model artifact feeds
  Dynamo's native DeepSeek-V4 renderer and NVIDIA `fastokens` outside Tokio I/O
  workers. Authenticated `/tokenize` runs concurrently as parity authority;
  only controlled match/fallback metrics survive. Local IDs remain shadow-only.
- 🔨 **Chat-template/token-ID golden matrix.** Compare local token IDs with the
  active vLLM `/tokenize` across OpenAI/Anthropic messages, tools, reasoning,
  content parts, special tokens, and `add_generation_prompt`; fail closed to
  remote or approximate mode on any model/template mismatch. The node06 OpenAI
  matrix admits 10/10 exact classes; tool history and `max`/`xhigh` are fenced
  remote-only because Dynamo 5.0.1 and vLLM r34 render them differently.
- ✅ **Versioned renderer compatibility manifest.** Bind model/tokenizer hashes,
  renderer profile, engine image digest, admitted request classes, and golden
  results so an engine or template update cannot silently widen local routing.
  The SHA-pinned r34 manifest re-renders ten synthetic token-vector goldens at
  startup and continuously matches each engine's model ID/root/context and
  `/version`. Image digest is recorded as provenance because Docker does not
  expose it inside the proxy container. Any mismatch or identity change fences
  in-flight tokenization and falls back to approximate routing.
- 🔨 **Exact KV-event shadow index.** The transport-independent sequence fence
  now starts untrusted, requests bounded contiguous replay on gaps, increments
  generations on restart/unrecoverable recovery, and admits exact state after
  either publisher sequence zero or a complete bounded replay from zero. A
  bounded, privacy-safe
  MessagePack decoder now matches an exact synthetic fixture emitted by the
  node06 vLLM r34 classes and validates event, hash, token, group, and block
  shape limits. Release-mode decode on the development host sustains about
  54–58M token IDs/s (4.8µs at 256 IDs, 324µs at 18.9K, and 1.41ms at
  82.2K). The bounded per-engine exact block trie is also complete: it keeps
  opaque engine hashes for O(1) removals, exact token-slice keys for collision-
  free prefix lookup, atomic capacity failure, tombstone pruning, conservative
  main-attention/tier/namespace filtering, and generation-safe replay
  integration. A 3.883M-token synthetic inventory used 21.4MiB and served
  80.9K-token lookups in 50.3µs; eight readers reached 102K lookups/s. The
  pure-Rust ZMTP transport now validates exact SUB/DEALER frame shapes, applies
  one total replay deadline and bounded requested/tail batches, and rejects
  missing, duplicate, or out-of-order sequences. A CPU-only node06 probe passed
  against Python `pyzmq` using the r34 live/replay protocol without touching
  either engine. One default-off supervised consumer per engine is now wired
  into the binary with typed endpoint cardinality, reconnect monitoring,
  generation fencing, bounded replay, graceful shutdown, and controlled
  connection/trust/index metrics. A second CPU-only node06 lifecycle test
  proved reconnect, authoritative-clear trust, and immediate disconnect
  fencing. The first real r34 B-only feed then exposed two undocumented hybrid
  details and drove fail-closed fixes: non-main sliding-window groups may omit
  masked hashes, and 4-token partial MLA events can reference internal parent
  hashes that the publisher never emits. The decoder now defers geometry to
  semantic group filtering; the index conservatively filters only orphaned
  blocks smaller than that group's observed canonical root geometry. A fresh
  r17 consumer replayed sequences 0–37, became trusted, indexed 650 blocks /
  166,400 token IDs, and then applied 14 live batches under c8 load with no
  reconnect or index error. Exact inventories remain disconnected from the
  router. A matching A-only trial has now passed from sequence zero, and bounded
  per-filter metrics directly count non-main and unsupported-partial exclusions.
  A temporary 785K-token A cache then forced 2,442 real removals over 192
  contiguous batches: 882 group-0 removals reduced the exact MLA index from
  3,456 stored blocks to 2,574 resident blocks exactly, while trust remained
  one and no reconnect/index error occurred. Both engines and eviction are now
  qualified. r19 now compares approximate choices with exact state without
  changing placement: it snapshots per-engine inventory revisions at decision
  time, uses engine-reported pre-request cached tokens for the selected engine,
  rejects an alternative that changes during the request, and preserves the
  original load snapshot. The node06 gate recorded 15 agreements, three cold
  decisions, and one deliberately constructed 14,336-token `would_move`; all
  28 concurrent comparisons failed closed on changing alternative revisions.
  r20 now moves admitted exact IDs into the pre-route preparation boundary:
  eight non-blocking CPU permits observed all 12 requests at c12, exact lookup
  averaged tens of microseconds, and the 3×3×2 locality gate produced 15
  agreements plus three cold decisions without post-response revision
  ambiguity. A forced approximate miss found 36,096 warm tokens on the other
  engine before mutation. r21 adds an explicit default-off placement canary
  behind unique-winner, minimum-token-gain, and maximum-load-delta gates while
  retaining the attestation, health, event-trust, inventory-revision, CPU, and
  timeout fences. An isolated node06 trial corrected 2/2 constructed
  approximate misses and kept 2/2 existing exact agreements; all four requests
  reused 32,768 tokens on the deliberately warmed engine. An intentional
  canary-only restart then proved recovery:
  the first long request fell back as `inventory_untrusted`, B replayed 943
  batches, A stayed fenced until its own event and replayed 885 batches, and
  the recovered gate again corrected 2/2 forced misses. r21 now also exports
  the identical gated decision as `mode="shadow"` without mutating the route;
  a real forced miss stayed cold on B while telemetry reported `would_move`.
  r21 introduced this observation mode and production r22 retains it. Its
  initial qualification gates recorded two agreements and 12 cold/all-zero
  decisions; production has not yet produced an organic move. Collect
  representative organic
  move/gain/load distributions before
  considering a narrowly admitted placement rollout. Raw token IDs, block
  hashes, and prompts remain out of logs.
- ✅ **Replay cancellation and publisher-backpressure resilience.** r23 proved
  that vLLM's synchronous ROUTER can burst a 1,292-batch / 29.9MB replay in
  77ms through libzmq while the async pure-Rust ZMTP receiver can stall before
  the end marker on the same large stream. Replay now runs in a deadline-
  bounded blocking worker with a fresh DEALER identity, high receive HWM, and
  drain-through-validation semantics; live SUB delivery remains async Rust.
  Reconnect progress means authoritative inventory restoration, so exponential
  backoff cannot reset on a merely live-but-fenced event. Isolated and
  production starts restored both inventories in parallel at 1,293/1,684 and
  1,332/1,724 retained batches respectively, without engine restarts. After r27
  exposed a quiet publisher waiting for a second allocation following an
  invalid replay, the consumer learned to preserve only the known upper
  boundary, clear the old generation, and retry one bounded full `0..through`
  range immediately after reconnect. An in-process PUB/ROUTER test proves the
  second replay becomes authoritative without a second live batch. A node06
  probe then identified the original `invalid_replay` cause: the publisher
  retains only scheduler steps that emitted events, so large histories have
  legitimate sequence holes. Validation now accepts strictly increasing
  in-range events ending at the requested boundary while still rejecting
  duplicates, regressions, out-of-range data, and incomplete tails. Exact
  index replay also omits a bounded `orphaned_parent` store when its ancestor
  has already left the retained/indexable set; this can only under-estimate KV
  availability, while structural/path/capacity errors still fence all state.
  Exact placement remains shadow-only while #13 collects organic gain/load
  evidence.
- ⬜ **Session-cached incremental preparation.** Bounded session state with
  deterministic invalidation so returning 80K conversations extend prior token
  vectors rather than restarting; benchmark memory, p99 preparation latency,
  and mismatch recovery before routing with it.
- ⬜ **P/D and KV-transfer seams.** Keep request preparation, cache inventory,
  placement policy, and transport independent so a future Dynamo/NIXL prefill
  pool does not require another proxy rewrite.

## Earlier internal prototype lineage

- ✅ Overlap+load router (`score = prefixOverlapBlocks − alpha·loadUnits`),
  chain-fingerprint prefix index per upstream.
- ✅ Conversation stickiness + cross-session template co-location.
- ✅ Cold-prefill → least-loaded placement (emergent from scoring).
- ✅ Size-weighted cold-prefill reservation: 32KB/request load units keep
  short decoders off the prefill engine; measured +23% aggregate decode and
  -39% median decoder TTFT in the 33.6k-prefill + 8-decoder workload.
- ✅ Explicit affinity toggle (`DS4_AFFINITY=prefix|load`) for policy A/B tests
  and engines without reusable prefix state.
- ✅ Health-aware failover, authenticated probes, `/v1/models` context-margin
  rewrite, request shims (max_tokens / content-parts / reasoning_effort).
- ✅ Prometheus surface incl. `route_decisions_total{outcome}`,
  `route_overlap_blocks`, `upstream_inflight`, `upstream_load_units`;
  engine-native passthrough.
- ✅ Measured: ties hash router on cache locality (82.9%), 1.57× under
  concurrent same-app load (RESULTS.md).

## Near term

- ✅ **Targeted long-prefill isolation (#11).** The exact pinned r34 CLI
  supports `max_num_partial_prefills`, `max_long_partial_prefills`, and
  `long_prefill_token_threshold`; production is currently 1/1/0 (threshold
  disabled) with the global 4,096 budget unchanged. The mixed benchmark now
  records engine queue/prefill histogram deltas, preemptions, and 20ms peak
  running/waiting/KV gauges in addition to request TTFT and decode throughput.
  A matched B-only baseline quantified the arrival-order effect: prefill-first
  put eight decoders in the engine queue and raised median decoder TTFT from
  834ms to 5,083ms. The proposed 2/1 candidate is not a valid r34 configuration:
  vLLM exits during validation because this backend does not support concurrent
  partial prefill. Retain 1/1/disabled and reopen the matrix only after an
  engine version exposes the capability. Future trials must probe candidate
  argv in a disposable container before rolling a resident engine.
- ✅ **Finer prefix-match-unit audit (#17).** The pinned engine already uses a
  four-token logical match unit by default: a privacy-bounded live probe found
  hybrid group sizes 256/64/64/4/8, and r34 resolves an unset unit to their
  GCD. Explicit 1- or 2-token matching offers at most three additional boundary
  tokens while multiplying hash work; larger values are strictly coarser.
  Retain the default and treat the group layout/unit as compatibility identity
  to re-audit after engine upgrades.
- 🔨 **Workload-aware reasoning and output budgets (#14).** The qualified
  agent oracle now supports explicit, non-mutating low/high/max effort and
  output-cap overrides, reports valid and total-spent completion tokens per
  successful task, and keeps model protocol failures measurable while still
  stopping on transport failure. Three fresh node06 rounds compared 192 and
  256 tokens across deterministic and official-agentic sampling: 192 produced
  only 256/270 protocol-valid tasks, while 256 passed 270/270. The initial
  96-token scout passed only 54/90 because typed and parallel tool calls hit
  the cap. At 256, low/high/max task-rate and throughput ranges overlapped, so
  there is no evidence to replace the high-effort production default. Preserve
  caller settings in mini-dynamo. Next, put a versioned shadow policy in Helix:
  class simple text/auto-tool steps separately from structured or parallel
  tool calls, retain 256 for the latter, and require real workflow success plus
  a kill switch before enforcement.
- 🔨 **Cache-efficiency SLO and working-set scorecard (#16).** The first Rust
  slice adds bounded `cold`/`partial`/`full`/`unknown` request counters and
  cache-outcome TTFT histograms from authoritative response usage, alongside
  the existing token-weighted prompt/cache counters. Unit tests cover outcome
  boundaries and metric registration/recording. The new synthetic working-set
  runner reconciles response usage, LB counters, and summed native
  prompt/cache/query/hit and request-sample counters with a fail-closed
  zero-spread gate; its first 1/4/8-app sweep passed 52/52 and balanced larger
  cells exactly across both engines. Accepted live/replay store, removal, and
  clear counters now expose content-free churn; removals are deliberately not
  called evictions because publisher events do not state the cause. A
  wave-barrier `--concurrency 2` mode now uses both TP4 pairs without allowing
  reuse to race an unfinished cold request. At 52×512KiB, every second-wave
  request still hit despite 20.56% block churn; at 64×512KiB, a 30/34 app
  placement split produced 30 partial hits and 34 cold reuses, 46.81%
  reuse-wave token hit, and a 35.04s cold p95. Next make cold placement
  capacity-aware so a small balance error cannot strand reusable capacity.
  The first implementation is deliberately telemetry-only: exact all-zero
  requests compare fenced resident token counts, require at least one prompt's
  delta, and retain the load gate. `/health` and `cachebench` now expose
  content-free before/after exact residency per opaque replica index, making
  the next boundary runs directly attributable without parsing upstream URL
  labels. A second observation-only counterfactual now adds each replica's
  exact residency to current-request-equivalent bounded in-flight pressure.
  It distinguishes a genuinely less-loaded cache from a replica whose cold
  prefill has not emitted KV events yet, without changing placement or
  claiming that decode load will become resident KV. Its first node06 rollout
  was serving-safe, but both long-lived publishers were beyond the bounded
  replay window, so the 64-app result was correctly withheld while exact
  authority was fenced. Repeat the 52/64 boundary three times only after both
  inventories have an authenticated generation, compare raw and projected
  outcomes against second-wave survival, and define no 95%+ SLO before then.
- ✅ **Production-shaped DeepSeek-V4 agent/DSML gate (#10).** The versioned
  synthetic v1 JSONL corpus and privacy-safe runner now cover stream/non-stream,
  automatic/required/parallel tool calls, split deltas and DSML leaks, every
  JSON argument class plus `arguments`/`input`, and retained reasoning/tool
  history. Eighteen GPU-free parser/schema tests run in Drone. Source-locked
  response-shape fixtures now also cover forced-choice JSON fallback in
  streaming and non-streaming responses plus `n=2` choice-local call IDs. The
  harness keeps assembly state per choice, matching the OpenAI contract instead
  of falsely treating identical IDs in different choices as duplicates. These
  fixtures validate the northbound contract without a GPU; they do not
  substitute for executing a candidate vLLM parser. The first
  node06 gates passed 5/5 deterministic c1 and 10/10 deterministic c8 cases; a
  five-run official-agentic auto+stream probe also passed 5/5 with no DSML leak. The matrix records image,
  model/config/tokenizer/router provenance, TTFT, mean ITL, throughput, cache,
  protocol validity, and successful tasks/GPU-hour. The corrected 256KiB
  c8/c16 cold-first/warm matrix is now three-run qualified: all 180 requests
  were protocol-valid, route splits stayed bounded across both TP4 pairs, and
  median warm TTFT was 1.56s/2.14s versus 8.36s/8.92s in the corresponding
  initial shared-prefix waves. Median matrix wall was 42.4s. The high
  79.5%/89.7% first-wave cache rate is expected within each concurrent cell:
  only the first placement on each replica is cold while peers share the same
  prefix. Do not call it an independently-cold-request result or interpret
  cached prompt accounting as compute throughput. The remaining c1 and 0KiB
  cells are also three-run qualified after fixing a harness bug that ignored
  fresh salts at zero prefix. The complete corrected deterministic matrix is
  420/420 protocol-valid. At 0KiB, warm reuse reduced median TTFT p95 from
  1.49s to 0.84s at c8 and from 2.19s to 0.86s at c16; c1 showed no latency
  win, as expected for tiny serial prompts. The full deterministic iteration
  is now about 85s from the three independently measured slices. Sovereign
  trace-shape ingestion is also complete: a strict numeric/enumerated schema,
  private-file admission, synthetic nested prefixes, relative arrival replay,
  bounded per-structure `/tokenize` calibration, and separate token-density
  and protocol-validity gates are covered by 17 focused tests. A four-shape
  node06 smoke reproduced all target prompt sizes within 10 tokens and split
  2/2 across replicas; one stochastic auto-tool completion failed its typed-
  argument oracle, which remains visible as model-quality evidence rather than
  being retried away. No customer content or production-derived artifact was
  used or retained.
- 🔨 **Reproducible experiment journal + workload matrix.** Keep
  `EXPERIMENTS.md`; measure deterministic code, prose, shared-app, cold/warm
  prefill, and mixed prefill+decode separately. Never report speculative decode
  without workload, temperature, prompt/output lengths, and acceptance data.
- ✅ **Fail-fast resumable engine qualification.** The Infernal r4 correctness
  failure is now an ordering invariant rather than a post-hoc lesson:
  `candidate_gate.py` binds immutable receipt/process/plan identity to a
  five-request deterministic agent smoke, then an optional code/prose c8 scout
  and only then the full matrix. Every boundary rejects restart drift and late
  JIT/CUDA/NCCL/OOM/Xid/runtime markers; successful stages resume only under
  the identical hashed plan. A live r34 direct-engine smoke passed 5/5 in
  2.99s and resumed without GPU traffic in 0.09s. This would have rejected r4
  before its 204-request performance matrix.
- ✅ **node06 DSpark depth sweep (K5 vs K7).** K5 passed 10/10 gates and beat
  K7 by 6.5% on code and 12.2% on prose at c8; promoted in the infra compose.
- ✅ **DSpark K3–K5 and rejection-mode gate (retain K5/standard).** The pinned
  DeepSeek-V4-Flash checkpoint declares `dspark_block_size=5`; r34's own engine
  configuration rejects K3/K4 because depth below that geometry produces
  incorrect or garbled output. K5/block passed an initial clean 5/5 smoke, but
  emitted 267 runtime compile/warmup markers during its matrix and then failed
  an agentic c8 structural warmup in round three. Its two complete rounds were
  280/280 measured-valid and counter-reconciled, yet their observational median
  was only 157.1 output tok/s versus 321.0 for matched K5/standard. Higher draft
  acceptance did not become useful work. Retain probabilistic drafting with
  standard rejection; the block overlay remains only as a reproducible negative
  canary.
- ✅ **DSpark dynamic-depth/capacity gate (retain fixed K5).** r34's supported
  default regressed code c1/c8/c16 by 22.8%/9.5%/8.9% and prose c1/c8 by
  24.7%/7.9%; only prose c16 improved 2.5%. Mixed decode fell 1.8% and p95
  worsened 11.7%, KV capacity fell 1.1%, and startup grew about two minutes.
  Diagnostics showed frequent physical-depth oscillation and a launcher-forced
  activation threshold of one despite auto-profiling eight. Retest only after
  the launcher honors the profiled threshold and the controller gains enough
  hysteresis to avoid 3↔4↔5 churn; keep fixed K5 for unified serving.
- ✅ **Collective-path matrix.** With P2P disabled, B12X improved cache-busted
  prefill by 36% at 8.5k and 71% at 33.6k tokens, but regressed code decode by
  20% at c8 and 44% at c1. Keep NCCL for unified engines; retain B12X for a
  future prefill-only lane. This run used `B12X_PCIE_DMA=0`.
- ✅ **NCCL P2P qualification.** node06 reports peer read/write support across
  all GPUs despite its PCIe-only topology. A matched isolated engine A/B
  improved c16 16%, mixed decode 39%, and 209K cold prefill 36%, while warm
  TTFT improved 14%; c1/c8 improved 3%. Promote `NCCL_P2P_DISABLE=0` and retain
  `=1` as the one-variable rollback. Post-promotion box c24 reached 1,879 tok/s
  with 72/72 successes, a balanced 37/35 split, and 1.11s p95 TTFT.
- ✅ **Mixed prefill/decode latency sweep.** Tested 2048/4096/8192/16384 in
  prefill-first and decode-first order. Promoted 4096: median 33.6k-prefill
  performance is unchanged, ordinary decode is within ~2%, and KV capacity
  rises 71% to 3.88M tokens. Known tradeoff: its ten-run queued-prefill p95 was
  6.43s versus 4.91s at 8192; the weighted router mitigates downstream stalls.
- ✅ **Draft-slot scheduler micro-gate (retain 4,096).** Raising the ceiling to
  4,160 restores a true 4,096 scheduled tokens with K5/max-seqs16 and improved
  c16/mixed medians 4–11%, but cost 1.1% KV, worsened mixed p95 12%, and made
  209K warm TTFT 10% slower for only 1.6% cold-prefill gain. The current 4,032
  effective quantum is an intentional warm-context/tail/capacity tradeoff.
- ✅ **Context-frontier benchmark.** Measured 2K through 259K actual prompt
  tokens: 49/49 requests succeeded, cold prefill peaked near 7.0K tok/s and was
  6.1K at the advertised boundary, while warm median TTFT remained below 1.9s.
  Draft acceptance stayed near 52–58%; repeat the 200K warm tail before making
  an SLA claim.
- ✅ **Effective-config capture.** `bench/capture_node06.sh` records actual
  `vllm serve` argv, image digest, driver, KV capacity, NUMA topology, cpusets,
  and LB health without printing credentials. The first capture confirmed the
  r34 launcher overrides compose-facing GPU-memory and CUDA-graph values.
- ✅ **Explicit KV-cache-bytes qualification (retain automatic sizing).**
  r34's post-capture profiler reports 0.87GiB estimated versus 0.16GiB actual
  CUDA-graph memory and suggests
  `--kv-cache-memory-bytes=53105596109` to remain inside the requested 0.975
  envelope. A rolling B trial raised capacity 1.16% (3,838,897→3,883,559
  tokens) and passed 200K context and c16 gates without OOM. Decode repeated at
  1,120.5 tok/s versus the 1,130.2 control; cold prefill improved 4.8%, but the
  setting bypasses profiling and leaves allocation coupled to image/runtime
  transients. The 44.7K-token gain is not worth that operational fragility.
  Retain automatic sizing and do not jump to the profiler's full-memory value.

- ✅ **Drone-only CI + package publishing.** One PR pipeline fetches Cargo once,
  then runs strict Clippy, the complete Rust suite, GPU-free protocol tests, and
  Compose validation in parallel. Main publishes both the LB and companion
  images only after that quality gate. A shared, source-free dependency image
  is keyed by the complete SHA-256 of the pinned toolchain, Cargo manifests,
  lockfile, and dependency Dockerfile; both release builds inherit it explicitly
  and compile offline instead of trusting Docker 20.10 inline-cache import.
  Dependency inputs rebuild that image first. Because Drone 2.12 discards
  unsupported path conditions, `rust-fetch` creates an exact-range,
  revision-bound publisher plan while Git is available; all three main-push
  steps consume only their marker before Docker startup or registry login.
  CI/docs/bench/deploy-only changes execute only those cheap guards and perform
  zero image work. A separate `refs/tags/v*` pipeline validates the exact Cargo
  version, waits for the same full quality gate, validates the existing
  SHA-tagged images' OCI identity, and digest-preservingly promotes them to
  immutable semver LB/companion tags—never rebuilding, updating edge aliases,
  overwriting a different existing digest, or creating PR/push artifacts. The Rust
  cutover is complete and the obsolete Go lane and
  packages are removed. Preserve the measured ~58–59s PR quality budget and
  investigate cache or scheduling regressions rather than adding duplicate CI
  systems.
- ⬜ **Secure post-deploy Helix acceptance.** The retired plaintext key was
  removed from both node06 guides. The separately supplied internal-account
  credential is also unusable for the test app (HTTP 403; `/users/me` returns
  500), so r21's real Helix workflow gate remains open even though direct LB
  gates pass. Confirm revocation/history policy and inject a current scoped
  smoke-test key from the secret store so every rolling promotion can close the
  control-plane-to-node06 acceptance gate automatically.
- ✅ **Alpha/affinity auto-tuning gate.** rc5's first 114-request live replay
  reproduced the deployed `(alpha=4, cap=32)` policy exactly. All positive
  alphas chose identically in the sampled cold-concurrent and idle-warm
  workloads. A controlled 58-block-warm/4-load-unit conflict then proved the
  decision boundary: alpha 4 retained 21.5K cached tokens at 523ms median TTFT;
  alpha 16 migrated all probes, cached zero, and took 3.20s. Keep alpha 4/cap
  32. Consider engine queue depth only after broader conflict replay.
- ✅ **Decision journal + offline replay** (DwarfStar `dspark_trace_replay`
  idea). rc5 emits versioned, privacy-bounded start/finish JSONL with opaque
  upstream ordinals, overlap/affinity/load/health snapshots, status, latency,
  and aggregate usage. `bench/route_replay.py` statically sweeps alpha/caps;
  it never sees prompt text, request IDs, fingerprints, generated text, or
  hostnames. The first live capture paired 114/114 records and reproduced the
  current policy 100%.
- ✅ **Conflict-trace benchmark + outcome scoring.** `bench/route_conflict.py`
  prewarms a shared trunk on one engine, applies controlled weighted load there,
  and sends returning work at a bounded setting. The completed 4K/20K/80K ×
  1/2/4/8-blocker sweep exposed the exact-score miss; replay now joins finishes
  to starts and reports paired completion, TTFT, duration, cache hit, and
  counterfactual migrations by policy.
- ✅ **Exact-score warm tie-break.** A 4K/20K/80K × 1/2/4/8-blocker sweep
  exposed an 80K request at the exact `32 − 4×8 = 0` boundary: rc5 rotated it
  cold, lost 87,296 cached tokens, and raised TTFT to 8.34s. rc6 prefers raw
  overlap only on score equality; strict load wins are unchanged. Journal v2
  records the policy and replay remains compatible with v1 traces. Three live
  rc6 reproductions retained all 87,296 cached tokens at 854ms median TTFT; a
  c24 gate then completed 72/72 at 1,891.2 tok/s.
- ⬜ **Bounded full-state replay feasibility.** Static replay intentionally
  holds snapshots fixed. Investigate short-lived, process-keyed opaque prefix
  IDs plus an explicit retention/privacy review if simulating counterfactual
  cache evolution proves valuable; never persist raw fingerprints or text.
- ✅ **Anthropic `/v1/messages` canonicalization.** rc3 normalizes top-level
  Anthropic system prompts with OpenAI system messages and includes the same
  prompt-affecting message/tool fields.
- ✅ **Fingerprint fidelity + bounded overlap.** rc3 expands the window from
  256KB to 2MiB, canonicalizes tools/reasoning/tool-call/name fields and
  Anthropic system prompts, caps score affinity at 32 blocks, uses deeper raw
  overlap as an equal-load tie-break, and estimates load from uncached bytes.
  A late-divergence replay improved from 4/8 to 8/8 correct placements; live
  259K requests now expose 760 blocks instead of 128.
- ✅ **Per-request route correlation.** rc4 returns opaque
  `X-Mini-Dynamo-Upstream: 0|1`; `concurrent_sameapp.sh` attributes its own
  responses and fails on missing routes/errors. Ten trials completed 120/120
  with exact splits no worse than 5/7, invalidating contaminated metric deltas.
  Python code, mixed, and context benchmarks now report the same exact route
  counts; a c24 validation proved 60/60 placement despite live latency noise.

## Medium term

- ✅ **NUMA-affinity A/B.** Promoted local CPU sets for both TP4 engines.
  Per-engine c8 aggregate improved 3.9% and warm 33.6K TTFT improved 16%; box
  throughput stayed flat but median/p95 TTFT improved 5%/22%. Tradeoff: pinned
  warm startup took 475s per engine. Keep the mapping in effective captures.
- ✅ **Backend/concurrency matrix.** Promoted `b12x-a16`: code decode
  improved 5–16%, prose 7–10%, and box c16/c24/c32 improved 14%/6%/4% to a
  1,910 tok/s peak before P2P tuning. `b12x-a8-dglin` beats A8 1–4% and wins the mixed profile,
  but trails A16 decode 3–11%; retain DGLin for a future prefill-heavy pool.
- ✅ **MAX_NUM_SEQS 8 vs 16.** Promoted 16 after isolated A/B: one-engine c12
  improved 65% and c16 97%, with p95 TTFT down 66%/77%; KV fell only 1%.
  Box c24/c32 reached 1,625/1,836 tok/s. Tradeoffs: c8 was 2.8% slower in the
  paired sample and pinned startup rose to about nine minutes.
- ✅ **Native CPU KV offload qualification (rejected on node06).** A rolling
  1GiB-total r34 native-offload trial initialized correctly without reducing
  the 3.843M-token GPU cache, but c8 aggregate fell 17.7%, 200K cold/warm TTFT
  rose 4.7%/34.5%, and NUMA-node-1 physical free memory fell to ~0.4GiB. It was
  rolled back. Do not enable native offload on this 128GiB host; revisit only
  with materially more RAM or a backend whose inactive path is demonstrably
  zero-overhead.

- ✅ **Infernal Invocation r4 one-engine qualification rejected (#32).** The r2 target
  in the original issue was superseded during investigation by immutable r4
  (`sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6`),
  with DS4 response-integrity fixes and a CUDA 13.3 / Torch 2.13 / NCCL 2.31
  base. Published A8/MNS8/MBT8192/131K results are not comparable to node06's
  A16/MNS16/MBT4096/393K control or its direct-root dual-socket topology.
  The exact image passed driver 595.84 CUDA execution and receipt verification,
  started in 12m51s, and increased GPU KV capacity 9.3%. After rejecting one
  JIT-contaminated interval, a zero-JIT matched matrix was 1.7-18.7% slower on
  code and 5.6-14.9% slower on prose, despite 11-23% better c8/c16 TTFT. The
  hard stop was correctness: r4 leaked a DSML marker in the deterministic
  parallel-tool case (4/5), while adjacent r34 passed 5/5 cold and warm. B was
  rolled back. Do not test MBT8192, MTP0, offload, or custom all-reduce on this
  image. A source-locked thin successor overlay is now packaged but deliberately
  not built, pushed, or deployed. Its GPU-free source gate reconstructs the
  exact candidate tree, compiles an exact six-file V4-only allowlist, and passes
  both the C128A invariant and all seven retained parser fixtures. A built
  successor must still pass the same gates from its runtime source before any
  GPU work. A fixed candidate must first pass the
  retained malformed-wrapper/orphan-invoke fixtures. A stdlib-only source gate
  now runs seven synthetic cases against the actual composed parser in about
  0.05s, emits no response content, and pins every V4 parser file touched by
  the candidate. Upstream vLLM #49117 recovers missing wrappers but needs the
  proven conservative exact-`toolcalls` prefix extension to close #51914.
  Candidate mismatch is a hard stop before image/GPU work. The immutable r4
  tree also definitely lacks vLLM #51318's
  C128A FULL-graph capture-stable row stride: its active width depends on each
  batch while capture uses `max_model_len`. The content-safe
  `bench/infernal_c128a_preflight.py` gate now proves the exact r4 source
  identity, rejects that layout for a candidate, and accepts only the fixed
  preallocated-capacity stride. This separately reported concurrent-decode
  corruption does not explain node06's sequential parser failure. Run the
  source gate and retained parser fixtures before retesting from the committed
  new revision-specific JIT cache and immutable one-engine Compose overlay.
  That r5 overlay built in 16.25s and reached API-ready on isolated B in 12m32s,
  but its first deterministic five-case gate passed only 3/5: typed-required
  emitted an extra call with invalid JSON, and parallel-required emitted a third
  call with a duplicate `engine` argument. Late JIT also remained in the first
  measured interval. It was rejected and rolled back before any c8 scout,
  matrix, LB exposure, or Helix workflow. Do not retry this tree; the next
  successor needs goldens for actual model-emitted extra/malformed calls in
  addition to synthetic parser shapes.
  A source-locked public refresh found no newer Infernal artifact or targeted
  fix: r4 remains the latest digest, the Infernal branch remains at
  `ce5f50f6`, #49117 and #51318 remain unmerged, and #51914 has no patch. Track
  vLLM #51538 at `e468291b` for its RTX PRO 6000 sparse-MLA/DSpark hardening,
  but do not roll it: r4 already includes the relevant workspace and lifetime
  protections, while the remaining changes touch neither DSML nor the C128A
  stride and do not address r5's sequential extra-call failures. Reopen only
  for an immutable r5+ artifact or a narrow patch that passes the retained
  real-output goldens before any image build.
  The Phase-B direct-P2P prerequisite is now tool-ready and read-only-qualified:
  exact NVIDIA `nvbandwidth`/`nccl-tests` sources build against canonical r34,
  and node06 preflight discovers the isolated B reservation as GPUs 4–7 with
  1,830MiB free per GPU while preserving 2/2 health and every container
  identity. The remaining 1MiB SM-vs-CE scout is separately explicit because
  it temporarily single-homes production; run it only in a low-traffic window.

- 🔨 **KV-event ground truth.** Subscribe to vLLM `kv_events` (block
  stored/removed) and replace the approximate LRU index with the engine's
  actual block inventory. The r34 interface has native sequence numbers and a
  bounded replay socket, but no initial/full snapshot; design must detect gaps,
  degrade safely when replay is too old, and avoid persisting the exact token
  IDs carried by `BlockStored`. The isolated publisher gate is complete: 49
  consecutive batches had zero gaps, and same-engine publisher-on/off results
  were within -1.4% to +2.0%. The Rust shadow consumer has now passed a complete
  real replay plus live-update qualification on B. A then passed live startup
  and a forced-eviction soak whose exact group-0 store/removal arithmetic
  matched the resident index. The Infernal canary's LB roll later reproduced
  the remaining recovery boundary: fresh B re-armed from sequence zero, while
  long-lived A was beyond replay history and correctly remained fenced. Do not
  restart a healthy engine only to recover shadow telemetry. A read-only
  follow-up found that A still retained a complete sparse sequence 0–9,392
  replay: 8,380 event-bearing messages and 408.6MB serialized. r31 now folds a
  full replay batch-by-batch into a scratch index and atomically swaps it only
  after end/cursor validation. The live A generation recovered in about six
  seconds with 407MiB peak LB RSS and more than 8.2GiB host memory available;
  both exact inventories became trusted without restarting either engine. The
  node06 replay limit is therefore aligned with the publisher's 10,000-step
  window. A later 9,426-batch near-retention replay exceeded 20 seconds but
  recovered once inside a 60-second fail-closed window, but subsequent rolls
  needed roughly three minutes. Node06 temporarily uses 180 seconds while
  exact placement remains disabled by default; timeout stretching is not the
  durable solution. A read-only r32 audit attributed the wall time to vLLM's
  single synchronous Python replay publisher: the LB received about 483MB and
  rebuilt 9,506 batches while consuming only 6.45 CPU-seconds, with no CPU
  throttling, memory pressure, stale-client overlap, or socket backlog. r33
  adds content-free success/failure profiling for request-to-first-frame,
  receive wait, maximum receive gap, decode, fold, commit, wire/payload bytes,
  and partial batch progress. Its LB-only roll received 5,500 A batches /
  254.5MB, spent 2.12s decoding and 0.16s folding, then exposed one 177.52s
  receive gap before the 180.04s fail-closed timeout. B's 69-batch / 3.79MB
  replay completed in 137ms with 34ms decode and less than 1ms fold. This
  closes attribution: do not extend the timeout or run another full-history
  probe against the production publisher. Restore node06's advertised replay
  limit to 8,192: once a live sequence is newer than that, A must observe only
  instead of initiating a request that the publisher cannot reliably finish.

  Bare vLLM exposes no authoritative snapshot: its replay request is only an
  inclusive starting sequence followed by buffered events and an end marker.
  Dynamo's `LocalKvIndexer` instead serves `TreeDump` plus a real-event
  watermark when the requested event is absent. Its dump is keyed by block
  hashes, however, while mini-dynamo's current radix tree requires exact token
  slices. Implement the smallest compatible no-engine-restart seam as a
  long-lived per-engine snapshot companion: subscribe live first, continuously
  maintain bounded exact state, serve one atomic digest-index dump plus engine
  incarnation/watermark, drain the buffered live tail, validate continuity,
  and atomically swap. Keep dumps memory-only and fail closed on every schema,
  generation, gap, capacity, checksum, or compatibility mismatch. First prove
  a captured 36,612-block / 9.37M-token shape in under three seconds locally,
  then deploy shadow-only and compare decisions against the raw-token index.
  Issue #41's first GPU-free prototype clears the transfer gate by a wide
  margin: a versioned, bounded 36,612-record MessagePack snapshot is 5.71MB,
  encodes in 10.3ms, decodes and validates in 10.8ms (8.3ms repeated), and
  peaks at 27MiB standalone RSS. A separate digest radix prototype stores one
  256-bit commitment per block, detects/poisons compact-key conflicts, and
  traverses a 524,288-token chain in 1.36ms. The production digest module now
  uses a canonical domain-separated HMAC-SHA256 contract, retains tombstones,
  rejects ambiguous multi-group/partial-scope snapshots, and imports into
  private state atomically with cancellation. Thirty-two deterministic
  1,000-action differential traces plus geometry/hash/tombstone cases have
  exact raw-index parity. At the matched 80,896-token shape it looks up in
  235us versus raw exact's 50.5us (4.66x); 524,288 tokens take 1.53ms; the
  complete 36,612-block snapshot builds an index in 13.1ms. At 15,168 blocks,
  digest RSS grows 8.1MiB versus raw exact's 21.5MiB (37.7%). The authenticated
  exchange and lifecycle are now complete as transport-independent modules:
  they use a separate session-auth key (never the block-digest key), authenticate
  a client challenge before snapshot work, verify fixed framing before owned
  decode, bind independently observed incarnation/watermark/generation/key ID,
  and use a distinct dense delivery sequence because real vLLM event watermarks
  are legitimately sparse. Snapshot and tail tokens are opaque to ordinary
  callers. The real one-shot Unix transport additionally verifies Linux peer
  UIDs on both ends before protocol bytes, shares one absolute deadline across
  connect/production/I/O/decode, bounds reads with a max+1 sentinel, and drops
  the producer future when the client disconnects. Tail frames derive ephemeral
  session/generation/direction keys and bind lifecycle admission to the exact
  authenticated payload; downstream decode/apply failure is terminal. The
  filesystem and actor boundary is now implemented too: raw session secrets
  require trusted ancestors plus exact owner/mode/inode/link/32-byte checks;
  socket publication uses a companion-owned non-writable directory, unique
  private bind, mode 0660, atomic no-clobber publication, and same-inode-only
  cleanup. A hard two-session actor keeps replacement state private, bounds
  tail queues, preserves same-identity publication during catch-up, revokes on
  identity change, and atomically publishes only a verifier-constructed opaque
  generation after caught-up. Epochs make stale disconnects harmless. The
  accept-loop supervisor and KV delta adapter are now complete: exactly two
  client tasks run independently under either a configured absolute supervisor
  deadline or handler-owned phase deadlines, excess clients are closed
  immediately, shutdown drops stalled streams, and every exit frees its slot.
  Authenticated payloads decode under existing vLLM bounds, filter to
  the selected local-GPU main-attention group, and apply store/remove/clear to
  actor-owned digest state; any late batch error clears the private generation
  before the actor fences it. Typed companion configuration now defaults off,
  validates the fixed two-engine/two-client contract and every queue, deadline,
  frame, and decoded-batch bound before startup, and redacts paths and endpoint
  identities from debug output. Its pre-initialized Prometheus surface uses
  only bounded engine slots and enums for session outcomes, capacity rejects,
  build/apply/catch-up work, tail batches/events, fences/discards, and published
  inventory size; arbitrary errors and protocol identity never become labels.
  The LB-side consumer now verifies peer credentials, authenticates one framed
  snapshot, builds the digest index on a cancellation-aware blocking worker
  while queuing authenticated tail frames, then publishes only on exact caught-
  up. One absolute deadline encloses the session, and timeout, abort, EOF, MAC
  failure, delivery gap, or stale generation synchronously fences its epoch and
  revokes owned publication. Keep this separate from companion/server admission.
  That companion/server half is now an engine-neutral producer behind the
  existing two-client supervisor: it authenticates the hello before source
  work, subscribes live before snapshot construction, writes a bounded length-
  framed snapshot without EOF, then signs dense-sequenced tail events while
  preserving sparse real watermarks. One absolute snapshot budget covers hello,
  source construction, authentication, and the snapshot write; after that,
  received and successfully written tail frames reset bounded idle/write
  budgets, so healthy progress is not killed by the bootstrap deadline. Bounded
  queues apply backpressure; client EOF, tail idle/slow-write timeout, shutdown,
  source failure, or identity rollover cancels source work and ends the session
  without holding engine locks across serialization or I/O. The LB reconnect
  owner now revalidates the trusted socket parent on every connection,
  generates OS-random 256-bit challenges
  under a bounded nonreuse ledger, applies bounded half-to-full jittered
  exponential backoff, and carries one absolute deadline through connect and
  consumption. Normal attempts are serial; only an explicit capacity-one
  replacement command overlaps a second session, preserves the old same-
  identity publication until the new epoch is caught up, and drops the old
  future only after handoff. Shutdown cancels promptly. The concrete long-lived
  source now owns bounded digest state across LB sessions, stages sparse replay
  privately, atomically publishes a complete boundary, registers subscribers
  before cloning that boundary, and fans out the already-qualified MessagePack
  bytes. Index failures, transport authority loss, rebuilds, or attested
  incarnation changes fence every session and advance the companion generation;
  a slow or disconnected reader only loses its own subscription. Tail payloads
  are shared and each session has a 16MiB default / 64MiB maximum aggregate
  queued-byte budget in addition to its entry bound; overflow and rebuild use
  prioritized out-of-band revocation so stale tail frames are not drained. The
  library-only process owner now installs SUB before replay, streams full sparse
  replay into generation-guarded private state, fences on transport or attested
  incarnation loss, reconnects with bounded backoff, and cancels blocking
  libzmq work on shutdown. A reconnect stays fenced until a fresh live watermark
  defines the replay boundary; it never trusts the last pre-disconnect value.
  Incremental gaps currently trigger a streamed full private rebuild rather
  than retaining an adversarially large decoded replay vector. If an old engine
  or live gap is already beyond that bounded replay window, the same subscribed
  connection remains stably fenced and observe-only; ordinary subsequent events
  neither cause reconnect/generation churn nor restore authority, while a real
  all-blocks clear establishes a safe new boundary. The same rule now applies
  after a structurally invalid completed full replay or failed private
  apply/commit: repeating identical history against the synchronous publisher
  cannot establish authority, while transport failures remain retryable. A
  bounded apply/boundary/tail/commit event makes that sole attempt diagnostic
  without exposing sequences or content. The standalone
  offline Compose/security harness now models the current one-source runtime as
  two isolated processes and authority domains: distinct companions, UIDs,
  tmpfs directories, sockets, secrets, clients, and readiness checks per engine.
  Its default profile renders zero services, and static fault projection proves
  one failed pair cannot publish or substitute through its healthy peer. A
  dedicated off-by-default executable now composes exactly one authenticated
  incarnation watch, ZMQ owner, long-lived source, snapshot socket, and bounded
  metrics endpoint. The endpoint may now be either loopback TCP or a mutually
  exclusive metrics UDS. UDS startup requires a normalized path in a distinct
  companion-owned setgid parent, a dedicated non-root group different from the
  snapshot/session parent group, setgid isolation on both authority parents,
  verified group inheritance, atomic no-replace publication, and inode-checked
  cleanup. It is built separately from the LB so ordinary router edits do not
  relink both binaries. A host-side authenticated-attestation provisioner
  now consumes a fresh protected schema-v1 engine metadata capture, derives a
  canonical immutable identity, rejects rollback/conflict, and atomically
  publishes the companion envelope without Docker access or identity/secret
  argv. Production-shaped Compose, host-authority setup, semantic validation,
  and metrics-only Caddy wiring now exist. The current immutable companions are
  deployed with routing still off; run at least 100,000 revision-stable shadow
  comparisons before placement can consume their state.

  The off-by-default library runtime now composes typed config, hardened secret
  loading, bind-last safe socket publication, the bounded supervisor, producer,
  shutdown drain, cleanup, readiness, and closed-label aggregate metrics. It
  refuses missing sources and multi-source mode before filesystem mutation:
  the current authenticated hello does not identify an engine, so a multi-source
  executable would be ambiguous. The offline deployment contract therefore
  selects one process/socket/source per engine instead of multiplexing them.
  The coordinator delegates one absolute
  snapshot-phase timeout and a resettable tail-idle/write timeout to the
  producer; the supervisor retains the two-client cap and immediate shutdown
  cancellation without imposing a total lifetime on healthy progress. The
  standalone process now injects the completed owner and a refreshable,
  HMAC-authenticated engine-incarnation watch; any invalid refresh fences
  authority immediately. The completed one-shot host provisioner now generates
  that manifest from current protected engine metadata without exposing either
  digest key or identity. Both node06 domains now pass the production host and
  Compose validators and run as separate attested services. The independently
  justified r98 canonical engine rolls supplied fresh attestations; both compact
  sources now bootstrap, retain live authority, and expose bounded resident
  inventories without restarting an engine for the snapshot experiment.

  A true offline public-stack harness now proves initial publication, live
  store/remove, rolling handoff, LB owner restart, companion shutdown/socket
  cleanup/restart, identity rollover, and leak-free teardown; two authenticated
  slow readers also hold both supervisor slots while a third is rejected. Ten
  repeated runs passed 20/20. At the captured 36,612-block / 9.37M-token shape,
  the 6.04MB snapshot encoded in 11.5ms, authenticated wire encode/decode took
  7.0/7.5ms, private rebuild took 22.4ms, total process wall was 0.15s, and peak
  RSS was about 58MiB. This clears the sub-3s offline gate with wide margin.
  The remaining synchronous atomic index clone was measured separately: one/two
  captured-shape starts pause the source lock for about 7.7/23.0ms; at the
  131,072-record maximum this is about 28.5/82.3ms. Instrument this before shadow
  and move to immutable/COW generations if ingestion p99 must stay below 10ms.
  A captured-shape eviction replay has now exercised 3,840 apply calls and
  2,442 removals per shape: apply p50/p95/p99 were 0.38/0.95/1.55us, with a
  45.45us maximum and exact final inventory arithmetic. This clears the 10ms
  gate by more than two orders of magnitude, so immutable/COW work remains
  profile-triggered rather than roadmap-driven.
  Runtime telemetry now polls the source every 25ms and separately reports
  listening, exact authority, replay/building/ready/fenced phase, watermark
  presence, indexed blocks, and active sessions with fixed label cardinality.
  The existing operational ready gauge is true only when the socket is listening
  and the source is authoritative; either shutdown or a rebuild clears it.

  Deployment hardening is also part of the gate. The offline dual-domain Compose
  harness now enforces fixed LB UID 12002, companion UIDs 12001/12003, and shared
  GID 12000; separate companion-owned non-LB-writable socket directories
  and mode-0660 socket; Linux `SO_PEERCRED` on both ends before protocol work;
  separate root-owned read-only secret files; read-only roots, all capabilities
  dropped, no-new-privileges, no GPU/host IPC/Docker socket/companion port, and
  explicit PID/memory/file limits. Permit at most two clients for rolling LB
  handoff and one in-flight cached snapshot per client. Approximate serving and
  overall `/health` readiness remain independent of companion/session readiness,
  while the content-free exact-inventory fields report the same selected direct
  or snapshot authority used by counterfactual routing. Rollback first
  disables snapshot placement and verifies approximate fallback, then removes
  the companion and socket. A separate production-shaped overlay and semantic
  validator now add one companion and one explicitly profiled provisioner per
  engine, immutable images, exact authority mounts, distinct setgid metrics
  directories/GIDs 12004/12005, and Caddy UDS-only scrape routes. The host
  validator checks tmpfs ownership/modes and unique inodes before any start;
  Caddy is explicitly forbidden from session GID 12000. This is an admission
  artifact only and has not changed node06. A fixed, idempotent host-authority
  helper now creates and validates the three non-login service identities,
  six setgid tmpfs parents, and four independent secrets without overwriting or
  repairing unsafe state. Caddy metrics membership remains a separate explicit
  opt-in. Twelve mock/in-memory policy tests cover first-run/idempotent behavior,
  collisions, unsafe paths and outputs, secret reuse, and Caddy isolation.
  The LB can now select one snapshot consumer per upstream behind the typed
  `DS4_SNAPSHOT_ROUTE_MODE=shadow` gate. Startup validates every protected
  authority and exact upstream cardinality before spawning reconnect owners;
  direct raw-event and compact snapshot authority are mutually exclusive.
  Snapshot inventory is injected only into the exact counterfactual scorer,
  and configuration rejects placement mode, so approximate serving and
  `/health` remain independent. The LB reconnect owners now export
  pre-initialized, fixed-cardinality readiness, active-attempt, active-
  connection, attempt-kind, and bounded terminal-outcome metrics. Engine
  labels are configuration ordinals only, and `ready` follows authoritative
  actor publication rather than a merely connected Unix socket. Cancellation,
  timeout, reconnect, and rolling-overlap paths balance their gauges through a
  drop guard. The LB also hot-reloads each authenticated
  incarnation through the same hardened file and HMAC policy. A monotonic
  authority revision prevents watch coalescing from hiding an invalid interval:
  loss, rotation, channel closure, or a skipped revision revokes the old actor
  epoch before reconnect, while an unchanged identity causes no churn. Valid
  atomic engine rotation therefore no longer requires an LB restart. The
  production-shaped dual-engine Compose/Caddy contract, host-authority setup,
  and host/semantic validators are also complete. The overlay and validator now
  pin the qualified v0.1.0 LB and companion manifests by SHA tag and digest.
  The node06 host setup, fresh per-engine attestations, full preflight, and
  restricted-identity off-mode LB start now pass without an engine restart.
  The r97 orphan-parity companion is digest-pinned on both engines. A naturally
  justified B rollback supplied a fresh attested generation and exposed a
  concrete compact-index incompatibility:
  r34 may publish a short partial MLA block whose internal parent is absent
  from the public event stream. The raw exact index already filters every
  absent-parent store without overclaiming, while the compact adapter had
  treated `ParentNotFound` as fatal. r97 gives both paths the same conservative
  contract. The corrected companions survived the later canonical A/B rolls and
  now publish 36 and 173 blocks respectively with zero invalid replay events. A
  repository-owned
  qualification gate now turns the next natural generation into one bounded
  command: read-only validation refuses fenced sources without mutation, while
  explicit apply mode holds the common lock across five LB-only shadow
  recreates, immutable-engine checks, nearest-rank p95 at or below three
  seconds, and mandatory config-hash-verified rollback. r99 removes two false
  negatives before the full gate: it timestamps snapshot publication separately
  from the slower ordinary-health probe and makes `/health` report the selected
  compact authority rather than the unused raw-event inventory. A production-
  shaped candidate recovered both 36/173-block inventories and 2/2 serving in
  2.150s with a clean immutable rollback. Publish and pin r99, then run the
  five-cycle gate before capacity cells.
  Compare at least 100,000 exact versus approximate decisions before placement
  can consume this state; Dynamo's additional tree-dump recovery remains the
  scale-out reference.
- ✅ **True TTFT instrumentation.** rc6's journal and Prometheus histogram
  time the first SSE response byte, which may be a role-only chunk. Journal v3
  code now records both first byte and first generated token/tool-call delta;
  OpenAI and Anthropic shapes are unit-tested and replay does not mislabel
  legacy v1/v2 first-byte samples. rc7 is deployed after 144/144 c24 requests
  succeeded at 1,820–1,844 tok/s with balanced placement.
- 🔨 **Pinned/session-hinted routing.** r32 accepts exactly one bounded
  `X-Session-ID` and uses a secret-keyed, monotonic basis-point cohort for
  exact placement. Zero is an instant rollback; missing/invalid hints fail
  closed, the header is stripped upstream, and journal v4 retains only a typed
  cohort plus separate approximate/served choices. Helix now propagates its
  existing internal session ID into the inference request. Next, use the same
  hint to mark long-lived orchestrator conversations so neither router
  migration nor
  alpha pressure moves them off their warm engine (DwarfStar pinned-deep-trunk
  analogue). Follow Kimi K3's failure-bounded form: assign a deterministic
  primary plus a pre-assigned secondary, not an unbounded hard pin.
- ⬜ **Request-class budgets / admission control.** The weighted load score is
  reactive, but does not isolate short interactive traffic from a burst of
  200K–393K prefills. Add observable short/medium/long token-estimate classes,
  then A/B per-class in-flight budgets or queues before enforcing them. This
  applies Kimi K3's budget-based scheduling lesson without importing its
  fleet-scale machinery.
- ⬜ **Load-aware tie-breaks under cache pressure.** With >2 instances or a
  KV pool too small to hold every template, eviction makes overlap routing
  matter much more than at current scale; validate + tune there.
- ⬜ **SLA planner-lite (advisory).** Watch queue depth + TTFT p95, emit a
  recommendation (not an action) for `MAX_NUM_SEQS` / instance count.
  Dynamo's planner, read-only.

- 🔨 **Track current upstream contracts, not names.** The 2026-08-13 refresh
  finds Dynamo v1.3.1 as the newest stable release. Dynamo v1.3 made its
  standalone selection service, branch-sharded KV indexer, compressed radix
  tree, topology-aware routing, parser separation, and trace replay first-class;
  `best_worker_id` and `get_overlap_scores` expose selection and per-tier overlap
  without requiring Dynamo to proxy the request. Keep mini-dynamo's two-engine
  implementation smaller, but preserve those boundaries: engine-neutral exact
  inventory, read-only counterfactual selection/overlap, and replayable workload
  traces. Add an offline `get_overlap_scores`-equivalent diagnostic after the
  snapshot owner lands; do not import branch sharding until profiling shows one
  compact index or its lock is actually limiting node06.

  Dynamo's Kimi-K3 v1.4 preview is explicitly non-production and targets TP8
  GB300 / TP16 GB200. It is useful evidence for frontend parser/reasoning/tool
  separation and aggregated-versus-disaggregated recipes, not evidence that K3
  fits node06. DwarfStar's current native-agent contract reinforces the session
  direction already selected here: rendered conversation plus durable KV state
  is the session truth, tool syntax stays model-native, and a stripped session
  can rebuild from saved rendered text. For mini-dynamo, preserve the exact
  engine-rendered-token golden boundary and add an explicit session snapshot /
  rebuild experiment before any persistent L2 promotion.

## Longer term / speculative

- ⬜ **Kimi K3 feasibility gate, not a deployment promise.** K3 is 2.8T total
  parameters and current official recipes require at least 8× GB300, so it does
  not fit this 8×96GB box. Reuse its serving lessons now: separate fine prefix
  match granularity from physical cache allocation, preserve long-lived state,
  use cache-aware primary/secondary affinity, and isolate request classes with
  budgets. Revisit smaller KDA descendants or validated quantizations when
  they exist.

- ⬜ **True disaggregated prefill/decode** once engines expose KV transfer
  (vLLM P/D + NIXL): route prefill to a prefill pool, stream KV to a decode
  engine. Modest single-box value; large multi-node value.
- ⬜ **KVBM-lite**: engine-side CPU-RAM KV offload (LMCache connector) so
  evicted agent sessions warm-restore instead of re-prefilling. ds4's disk
  KV banks proved the pattern on this exact workload.
- ⬜ **Multi-model / multi-pool routing.** Today one model, N identical
  engines. Extend to several models behind one endpoint with per-model
  affinity policy and pool sizing.

## Helix-side follow-ups (not mini-dynamo, but this LB masks them)

The shims exist because Helix's Zed config emits fields strict engines reject.
Proper fixes belong upstream: cap Zed `max_output_tokens`, retry-on-5xx for
ACP thread follow-ups, and drop/valid-map `reasoning_effort` on agent switch.
