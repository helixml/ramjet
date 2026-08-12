# mini-dynamo roadmap

Status legend: ✅ done · 🔨 in progress · ⬜ planned. Ordered by
value-per-effort given the current deployment (2 vLLM+DSpark TP4 instances on
node06). The design rationale for each lives in DESIGN.md.

## Rust rewrite (v1.2)

- ✅ **Parity routing kernel.** Rust 2024 implementation of typed config,
  canonical prompt preparation, chained fingerprints, bounded LRU indexes,
  overlap/load scoring, health ordering, exact-score tie policy, and RAII
  weighted-load accounting. Go-generated fingerprints are golden-tested.
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
  4.53ms at 2MiB on the development host, about 10× faster than the retained Go
  data path and 15% faster than the initial two-parse Rust implementation at
  2MiB. Keep `examples/preparation_bench.rs` as the pre-tokenizer baseline.
- ✅ **Rolling Go/Rust node06 qualification.** Build and publish immutable
  `rust-*` images, then run locality, concurrent same-app, c24 aggregate, route
  telemetry, and occasional Helix workflow acceptance before promotion. Go
  remains an LB-only rollback; neither engine is restarted for proxy trials.
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
  1,332/1,724 retained batches respectively, without engine restarts. Exact
  placement remains shadow-only while #13 collects organic gain/load evidence.
- ⬜ **Session-cached incremental preparation.** Bounded session state with
  deterministic invalidation so returning 80K conversations extend prior token
  vectors rather than restarting; benchmark memory, p99 preparation latency,
  and mismatch recovery before routing with it.
- ⬜ **P/D and KV-transfer seams.** Keep request preparation, cache inventory,
  placement policy, and transport independent so a future Dynamo/NIXL prefill
  pool does not require another proxy rewrite.

## Shipped (v1.1)

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

- 🔨 **Production-shaped DeepSeek-V4 agent/DSML gate (#10).** The versioned
  synthetic v1 JSONL corpus and privacy-safe runner now cover stream/non-stream,
  automatic/required/parallel tool calls, split deltas and DSML leaks, every
  JSON argument class plus `arguments`/`input`, and retained reasoning/tool
  history. Eighteen GPU-free parser/schema tests run in Drone. The first
  node06 gates passed 5/5 deterministic c1 and 10/10 deterministic c8 cases; a
  five-run official-agentic auto+stream probe also passed 5/5 with no DSML leak. The matrix records image,
  model/config/tokenizer/router provenance, TTFT, mean ITL, throughput, cache,
  protocol validity, and successful tasks/GPU-hour. Complete the 0/256KiB,
  cold/warm, c1/c8/c16 matrix with three qualified runs and add sovereign
  redacted trace-shape ingestion before closing the issue.
- 🔨 **Reproducible experiment journal + workload matrix.** Keep
  `EXPERIMENTS.md`; measure deterministic code, prose, shared-app, cold/warm
  prefill, and mixed prefill+decode separately. Never report speculative decode
  without workload, temperature, prompt/output lengths, and acceptance data.
- ✅ **node06 DSpark depth sweep (K5 vs K7).** K5 passed 10/10 gates and beat
  K7 by 6.5% on code and 12.2% on prose at c8; promoted in the infra compose.
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

- 🔨 **CI + package publishing.** GitHub Actions runs Rust fmt, strict Clippy,
  and 104 unit tests with a pruned dependency cache. Drone independently adds
  the release build plus retained Go tests/vet/gofmt and 18 GPU-free protocol
  tests; its Rust, Go, and protocol steps fan out in parallel. Main's Docker
  build is the second release-mode proof, avoiding a duplicate PR link. Both Drone push/PR builds
  and GitHub Actions passed on r21. The post-merge image also compiled, but GHCR
  denied its final push: because `mini-dynamo` was created by a manual push,
  repository linkage did not grant Actions access. Add `helixml/mini-dynamo`
  under the package's **Manage Actions access**, rerun the failed job, and
  verify an anonymous pull before marking complete. Do not replace this
  one-time ACL with a long-lived PAT secret.
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
  matched the resident index. Compare exact versus approximate decisions in
  telemetry before the router may consume this state; Dynamo's additional
  tree-dump recovery remains the scale-out reference.
- ✅ **True TTFT instrumentation.** rc6's journal and Prometheus histogram
  time the first SSE response byte, which may be a role-only chunk. Journal v3
  code now records both first byte and first generated token/tool-call delta;
  OpenAI and Anthropic shapes are unit-tested and replay does not mislabel
  legacy v1/v2 first-byte samples. rc7 is deployed after 144/144 c24 requests
  succeeded at 1,820–1,844 tok/s with balanced placement.
- ⬜ **Pinned/session-hinted routing.** Accept a stable session routing hint and
  mark long-lived orchestrator conversations so neither router migration nor
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
