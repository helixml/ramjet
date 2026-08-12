# mini-dynamo roadmap

Status legend: ✅ done · 🔨 in progress · ⬜ planned. Ordered by
value-per-effort given the current deployment (2 vLLM+DSpark TP4 instances on
node06). The design rationale for each lives in DESIGN.md.

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

- 🔨 **Reproducible experiment journal + workload matrix.** Keep
  `EXPERIMENTS.md`; measure deterministic code, prose, shared-app, cold/warm
  prefill, and mixed prefill+decode separately. Never report speculative decode
  without workload, temperature, prompt/output lengths, and acceptance data.
- ✅ **node06 DSpark depth sweep (K5 vs K7).** K5 passed 10/10 gates and beat
  K7 by 6.5% on code and 12.2% on prose at c8; promoted in the infra compose.
- ⬜ **DSpark dynamic-depth/capacity gate.** r34 exposes a supported
  `DSPARK_DEPTH_MODE=dynamic` path (dynamic draft depth, capacity verification,
  online STS, and B12X varlen verification) while fixed K5 is the current
  control. Qualify code/prose c1/c8/c16, draft acceptance, and the mixed
  prefill+decode tail as one rolling experiment; retain fixed K5 unless dynamic
  mode improves the workload mix rather than one isolated median.
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
- ⬜ **Explicit KV-cache-bytes qualification.** r34's post-capture profiler
  reports 0.87GiB estimated versus 0.16GiB actual CUDA-graph memory and suggests
  `--kv-cache-memory-bytes=53105596109` to remain inside the requested 0.975
  envelope. A rolling one-engine trial could reclaim about 0.57GiB / 1.2% KV
  capacity without changing scheduler shape; require first-use long-prefill,
  c16, and OOM/error gates before considering promotion. Do not jump directly
  to the profiler's full-memory 50.76GiB suggestion because transient headroom
  is more valuable than the extra ~2.6% cache.

- ⬜ **CI + package publishing.** GitHub Actions: `go test ./...`, `go vet`,
  build, and push `ghcr.io/helixml/ds4-loadbalancer:<tag>` on tag/main.
  Removes the current manual "build on node06, no ghcr push" gap (the
  interactive `gh` token lacks `write:packages`).
- ⬜ **Secure post-deploy Helix acceptance.** The retired plaintext key now
  returns 401 and has been removed from both node06 guides. Confirm revocation,
  clean Git history if policy requires it, and inject a current smoke-test key
  from the secret store so every rolling promotion can close the required
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
  were within -1.4% to +2.0%. Next build the privacy-bounded shadow consumer;
  Dynamo's additional tree-dump recovery is the reference for reconnects.
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
