# node06 experiment journal

Append-only record of controlled serving experiments. Configuration changes use
rolling engine restarts; the other TP4 engine stays available. Every comparison
must use the same workload, fresh cache-busting salts where applicable, and a
correctness/health check after the run.

## 2026-08-12 — calibrated baseline (no configuration change)

Effective runtime (from the live `vllm serve` process, not compose labels):

- 2 × TP4 on 8 × RTX PRO 6000 Blackwell, driver 595.84.
- image `gilded-gnosis-v20-...-20260810-r34`.
- B12X A8 MoE/linear and B12X sparse MLA; NCCL all-reduce with P2P disabled.
- DSpark depth 7, probabilistic draft sampling; FP8 KV, block size 256.
- 393,216 model context, 2.266M KV tokens/engine, max sequences 8,
  max batched tokens 8,192, CUDA graph capture 64.

Workload: the deterministic code prompt from the upstream RTX PRO 6000 recipe,
streamed usage accounting, three measured runs after warm-up.

| Target | Concurrency | Max output | Per-stream decode | Aggregate |
|---|---:|---:|---:|---:|
| engine A direct | 1 | 512 | 245.1 tok/s | 199.0 tok/s end-to-end |
| engine A direct | 8 | 256 | 107.5 tok/s | 556.3 tok/s |
| mini-dynamo, both engines | 16 | 256 | 105.7 tok/s | **1,087–1,129 tok/s** |

The earlier ~947 tok/s box result used a different mixed/prose workload. The
new number is not a config win; it demonstrates why speculative-decoding results
must name the workload. Both engines remained healthy and had zero queued or
in-flight work after the run.

The comparable upstream recipe reports 345–358 tok/s single-stream and uses
DSpark depth 5. node06 uses depth 7, making K5 versus K7 the next controlled
engine experiment.

Fresh-salt prefix sweep through the LB (three samples/row):

| Prompt tokens | Warm TTFT | Cached | Cache-busted TTFT | Cache-busted prefill |
|---:|---:|---:|---:|---:|
| 362 | 456 ms | 256 | 475 ms | 763 tok/s |
| 891 | 459 ms | 768 | 467 ms | 1,907 tok/s |
| 2,203 | 450 ms | 2,048 | 471 ms | 4,679 tok/s |
| 8,482 | 494 ms | 8,192 | 1,395 ms | 6,079 tok/s |
| 33,575 | 614 ms | 32,768 | 4,858 ms | **6,911 tok/s** |

The long-prompt cache-busted rate is well below the matching recipe's ~11.2k
tok/s, strengthening the case for the NCCL versus B12X PCIe collective test.
The warm ratio is explicitly not hardware prefill throughput.

## 2026-08-12 — DSpark depth K5 versus K7

Method: rolling A/B on the same r34 image and NCCL configuration. Engine A ran
K5 and engine B retained K7. The prompts are shorter than the 256-token cache
block, so every measured request reports zero cached tokens. Five c1 runs and
three c8 batches followed a warm-up. K5 passed all 10 correctness gates.

| Workload | Concurrency | K5 aggregate | K7 aggregate | Delta |
|---|---:|---:|---:|---:|
| code, temperature 0 | 1 | 203.5 tok/s | 192.7 tok/s | **+5.6%** |
| code, temperature 0 | 8 | 613.6 tok/s | 576.0 tok/s | **+6.5%** |
| prose, temperature 0.6 | 1 | 141.5 tok/s | 138.5 tok/s | +2.2% |
| prose, temperature 0.6 | 8 | 445.8 tok/s | 397.4 tok/s | **+12.2%** |

K5 also reduced median/p95 TTFT at c8 in both workloads. Its CUDA graph maximum
is 48 rather than K7's 64 because r34 derives the graph size from
`max_num_seqs × (draft_tokens + 1)`; this is part of the depth configuration,
not a separately controlled compose knob. Decision: promote K5 to both engines.

After the rolling promotion, both engines were healthy and a fresh box-level
code run at c16/max256 completed 48/48 measured requests with zero failures:

| Configuration | Aggregate | Per-stream decode | Median TTFT | p95 TTFT |
|---|---:|---:|---:|---:|
| K7 baseline range | 1,087–1,129 tok/s | 105.7 tok/s | 1,087 ms | — |
| **K5 production** | **1,265.0 tok/s** | **119.0 tok/s** | **914 ms** | 1,632 ms |

That is a 12.0–16.4% aggregate improvement over the repeated K7 baseline.
Engine B also passed an authenticated direct streamed acceptance request after
its restart; engine A had already passed the full 10-gate API suite before the
promotion. The LB reported both upstreams healthy after the roll.

Startup emitted a useful follow-up: speculative decoding reduces the effective
scheduled-token ceiling to 8,160 under `MAX_NUM_BATCHED_TOKENS=8192`. Test a
small aligned increase and 16,384 in the mixed-prefill sweep; do not treat this
warning alone as evidence that a larger batch is faster.

## 2026-08-12 — NCCL versus B12X PCIe collective

Method: engine A used K5 + `ALLREDUCE_MODE=b12x`; engine B remained on the
promoted K5 + NCCL control. P2P stayed disabled and all other effective runtime
settings matched. Direct-endpoint runs avoided router placement effects. The
first A request raced the last seconds of API startup and was discarded; all
reported batches ran after authenticated readiness succeeded.

Decode results:

| Workload | B12X | NCCL | B12X delta |
|---|---:|---:|---:|
| code c1/max512 | 116.4 tok/s | 206.3 tok/s | **-43.6%** |
| code c8/max256 | 511.5 tok/s | 635.4 tok/s | **-19.5%** |
| prose c8/max256 | 456.5 tok/s | 463.8 tok/s | -1.6% |

Fresh-salt cache-busted prefill results (median of three):

| Prompt tokens | B12X | NCCL | B12X delta |
|---:|---:|---:|---:|
| ~362 | 575 tok/s | 829 tok/s | -30.6% |
| ~891 | 1,591 tok/s | 2,002 tok/s | -20.6% |
| ~2,203 | 3,922 tok/s | 4,854 tok/s | -19.2% |
| ~8,482 | 8,447 tok/s | 6,204 tok/s | **+36.1%** |
| ~33,575 | **11,835 tok/s** | 6,934 tok/s | **+70.7%** |

Decision: keep NCCL on both unified prefill+decode engines. B12X has a clear
long-prefill crossover between ~2k and ~8k tokens, but its decode regression is
too large for the mixed production path. Preserve it as a candidate for a
future prefill-only pool with KV transfer, or a router experiment that can
explicitly price the subsequent decode penalty. `B12X_PCIE_DMA=0` in this run.

## 2026-08-12 — scheduler quantum sweep

Added `bench/mixed_bench.py`. `prefill-first` starts one fresh 33.6k-token cold
prefill 50ms before decoder requests and measures queueing behind it.
`decode-first` waits until all decoders emit a token before admitting the
prefill, measuring interference with active generation. Input and output tokens
remain separate rather than being combined into a meaningless aggregate.

All runs used K5, NCCL, P2P disabled, max sequences 8, and the same r34 image.
Engine A swept the scheduler ceiling while engine B stayed on the 8,192
control. Effective scheduled tokens were 32 below the configured value for
2,048/4,096/8,192 because DSpark draft slots share the ceiling.

| Batched-token ceiling | KV tokens/engine | Full 393,216-token concurrency |
|---:|---:|---:|
| 2,048 | 5,930,470 | 15.08× |
| **4,096** | **3,880,329–3,880,487** | **9.87×** |
| 8,192 | 2,271,326 | 5.78× |
| 16,384 | 1,202,150 | 3.06× |

Three-run screening with one prefill + four decoders:

| Ceiling | Prefill-first prefill TTFT | Prefill-first decoder aggregate | Decode-first decoder aggregate |
|---:|---:|---:|---:|
| 2,048 | 7.67s | 180.0 tok/s | 243.0 tok/s |
| 4,096 | 4.95s | 238.8 tok/s | 297.5 tok/s |
| 8,192 | 4.97s | 238.3 tok/s | 308.5 tok/s |
| 16,384 | 5.01s | 235.0 tok/s | 297.0 tok/s |

The 2,048 quantum does not reproduce DwarfStar's decoder-protection result in
this vLLM scheduler: it slows both orderings. 16,384 provides no long-prefill
gain and halves KV capacity. 4,096 and 8,192 have the same median 33.6k prefill
time; 4,096 buys 71% more KV capacity and enough space for all eight configured
sequences at maximum context.

The 4,096/8,192 ten-run comparison found ordinary and active mixed throughput
within 1–2%. The cost is queueing variance: prefill-first p95 was 6.43s at
4,096 versus 4.91s at 8,192. Short code/prose c1/c4/c8 results were within
about ±3% except a cross-engine c1 sample; the same-engine historical c1 was
flat. Decision: promote 4,096 for the long-context agent workload and mitigate
the cold-prefill tail in the router.

After rolling both engines, box code c16/max256 completed 80/80 requests at a
1,224.9 tok/s median across five runs, 3.2% below the 8,192/K5 peak while
retaining most of K5's gain and increasing aggregate KV capacity from 4.54M to
7.76M tokens.

## 2026-08-12 — size-weighted prefill load

The unweighted router counts a 33.6k-token prefill and a 149-token decoder as
one in-flight request each. In a box-level prefill-first workload, it began
sending decoders back to the prefill engine after the other engine accepted one
request. Added configurable request-body load units: one per 32KB, capped at
eight. Literal request-count metrics remain unchanged; routing uses the weighted
value and exposes `ds4proxy_upstream_load_units`.

Ten-run A/B, both engines on K5/NCCL/4,096, one 33.6k prefill + eight decoders:

| Router | Decoder aggregate | Per-stream decode | Decoder TTFT median | Decoder TTFT p95 | Prefill TTFT median |
|---|---:|---:|---:|---:|---:|
| rc1, one request = one load | 485.5 tok/s | 145.2 tok/s | 1,523.8ms | 5,069.7ms | 4,925.0ms |
| **rc2, size weighted** | **598.5 tok/s** | 108.1 tok/s | **931.7ms** | **4,738.7ms** | **4,801.5ms** |

The weighted policy improved aggregate decode 23.3%, median decoder TTFT 38.9%,
and p95 6.5%; 80/80 decoder requests succeeded. Per-stream decode fell 25.6%
because more decoders intentionally share the non-prefill engine. At 108 tok/s
this is an acceptable exchange for avoiding multi-second first-token stalls.
Ordinary short-prompt c16 remains governed by one-unit request counts and
measured 1,224.9 tok/s across five runs after the change.

## 2026-08-12 — production acceptance after rc2 promotion

The local compose and `/home/luke/inference/dspark_0731/docker-compose.yaml`
on node06 had the same SHA-256 after deployment. Both TP4 engines were running
K5, NCCL with P2P disabled, and a 4,096 batched-token ceiling; the rc2 load
balancer reported both upstreams up with zero in-flight requests and zero load
units. An authenticated direct request to engine B and the final rc2 mixed
smoke both succeeded.

The required Helix control-plane smoke could not be completed: the only
credential documented in the infra checkout is retired and Helix returned
HTTP 401. This is an acceptance-credential gap, not evidence of an inference
failure. The plaintext copies were removed from the working tree; confirm the
old key is revoked, clean repository history if required, and provide the
current key through a secure environment before the next promotion.

## 2026-08-12 — 2K–259K context frontier

Added `bench/context_frontier.py` and ran it sequentially through the rc2 load
balancer. Each point used three cache-busted prompts, one warm-up of a distinct
prompt, then three cache-hit requests. Output was capped at 256 tokens and all
49 requests, including warm-up requests, succeeded. Effective cold prefill is
uncached prompt tokens divided by TTFT; it includes scheduler and first-token
overhead and is not a kernel-only rate. DSpark acceptance comes from deltas of
the two engines' vLLM speculative counters.

| Actual prompt | Cold TTFT | Effective cold prefill | Warm cached | Warm TTFT | Cold / warm decode | Cold / warm draft acceptance |
|---:|---:|---:|---:|---:|---:|---:|
| 2,202 | 0.47s | 4,641 tok/s | 2,048 | 0.45s | 305 / 283 tok/s | 54.1 / 52.6% |
| 8,481 | 1.58s | 5,376 tok/s | 8,192 | 0.46s | 297 / 293 tok/s | 52.0 / 55.0% |
| 33,574 | 4.82s | **6,966 tok/s** | 32,768 | 0.60s | 290 / 352 tok/s | 52.1 / 64.6% |
| 67,016 | 10.06s | 6,659 tok/s | 66,816 | 0.74s | 299 / 284 tok/s | 57.9 / 52.3% |
| 133,923 | 20.28s | 6,604 tok/s | 131,072 | 1.16s | 299 / 360 tok/s | 52.8 / 54.8% |
| 200,832 | 32.14s | 6,250 tok/s | 200,704 | 1.55s | 368 / 294 tok/s | 53.0 / 52.7% |
| 259,390 | 42.66s | 6,080 tok/s | 258,048 | 1.89s | 299 / 299 tok/s | 52.8 / 53.1% |

The engine remains useful at the 262,144-token advertised boundary: cold
prefill declines gradually after 33K rather than falling off a cliff, warm TTFT
stays below two seconds at the median, and draft acceptance stays near 52–58%
apart from one 64.6% sample. The 200K warm p95 was 2.58s, so long-context tail
latency still needs a larger repeated run before an SLA claim.

This run exposed a router limit before an engine limit. Request bodies grow
from 12.8KB at 2K tokens to 1.56MB at 259K, but the 256KB fingerprint window
saturates at 128 blocks starting at 67K. Warm routing still worked because the
leading prefix matched, but sessions that share the first ~43K tokens and
diverge later are indistinguishable. Do not simply raise the window: an
uncapped 700+ block overlap would permanently overwhelm the current load term.
The next router experiment must extend fingerprint fidelity while separately
normalizing the overlap contribution.

Effective runtime captured for the run: image ID
`sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b`,
model revision `9e165c30e2704aec5d9d593cce3eebd58bbef1cb`, K5, NCCL with P2P
disabled, 4,096 batched tokens, 0.975 GPU memory utilization, CUDA graph size
48, FP8 KV, 3.88M KV tokens per engine, driver 595.84. The host has 125GiB RAM
and two NUMA nodes; neither container is CPU- or memory-pinned. Both upstreams
were healthy and idle after the run, with no CUDA, OOM, traceback, or fatal
errors since the benchmark began. `bench/capture_node06.sh` makes this capture
repeatable for subsequent experiments.

## 2026-08-12 — NUMA-local CPU placement

`nvidia-smi topo -m` maps GPUs 0–3 to NUMA node 0 CPUs `0-11,24-35` and GPUs
4–7 to node 1 CPUs `12-23,36-47`; both containers previously ran on all 48
logical CPUs. Engine A was drained from the LB while direct A/B phases
alternated between all CPUs/memory nodes and its local CPU/memory node. No
engine restart occurred during measurement.

Two c8/max256 code phases per state, three measured batches per phase:

| Placement | Aggregate | Per-stream decode | Median TTFT | p95 TTFT |
|---|---:|---:|---:|---:|
| all CPUs, median of phases | 599.0 tok/s | 117.8 tok/s | 1,104ms | 1,160ms |
| **NUMA-local, median of phases** | **622.5 tok/s** | 116.5 tok/s | **941ms** | **1,124ms** |

NUMA locality improved aggregate completion throughput 3.9% by reducing batch
startup/TTFT; steady per-stream decode was flat within noise. A repeated 33.6K
context test corroborated the latency effect:

| Placement | Effective cold prefill | Cold TTFT | Warm TTFT | Warm p95 |
|---|---:|---:|---:|---:|
| all CPUs | 6,910 tok/s | 4.86s | 573ms | 582ms |
| **NUMA-local** | **7,040 tok/s** | **4.77s** | **481ms** | **485ms** |

Decision: promote CPU affinity to both engines in compose. Because workers can
only first-touch memory from their local CPUs after a fresh start, this also
provides local host-memory placement without a non-portable compose
`cpuset_mems` key. Each engine was recreated separately and passed an
authenticated chat request before the other engine rolled; one TP4 remained
available throughout. Pinned warm startup took 475 seconds per engine, slower
than the previous approximately five-minute observation.

The post-promotion box c16/max256 run completed 80/80 requests. Aggregate
throughput was essentially unchanged at 1,220.5 tok/s versus 1,224.9 before
pinning, while median TTFT improved 5.3% (925ms to 876ms), p95 improved 21.6%
(1,302ms to 1,021ms), and per-stream decode rose from 116.1 to 120.0 tok/s.
Both upstreams were healthy and idle after the run.

## 2026-08-12 — rc3 long-prefix fidelity and bounded affinity

The context frontier proved that rc2's 256KB fingerprint window flattened all
67K–259K prompts to 128 blocks. rc3 expands canonical fingerprint coverage to
2MiB while capping the score contribution at 32 blocks:

```
affinity = min(raw overlap blocks, 32)
score    = affinity - 4 * in-flight load units
```

Raw overlap still breaks ties between equally loaded engines, so two prompts
that share a trunk beyond 256KB but diverge later remain distinguishable. A
deterministic replay of two such 400KB trunks improved exact placement from
4/8 with the rc2 window to 8/8. The cap ensures a maximum eight-unit cold
prefill can neutralize even the deepest affinity instead of a 700-block match
becoming immovable.

Canonicalization now includes prompt-affecting OpenAI/Anthropic system, tools,
functions, names, reasoning history, tool calls, tool IDs, thinking, and
response-format fields while ignoring generation-only JSON ordering and
temperature. Equivalent Anthropic top-level and OpenAI system prompts produce
the same fingerprints. Load units use the request bytes remaining after the
chosen engine's cached overlap: live 259K requests now report 760/760 raw
blocks, bounded affinity 32, and one load unit when warm, versus 0/760 and
eight units when cold.

Local 1.4MB fingerprint profiling showed the expanded window costs about
11.5–12.6ms versus 10.5–10.7ms for 256KB, with the same 47 allocations. This
is negligible beside 1.3s warm and 42s cold long-context TTFT, but should move
to token/block IDs if the engine exposes them.

Paired rc2/rc3 live gates on the NUMA-pinned engines:

| Workload | rc2 | rc3 | Result |
|---|---:|---:|---:|
| code c16/max256, five-run median | 1,220.5 tok/s | 1,221.4 tok/s | flat; 80/80 |
| 33.6K cold + 8 decoders/max256 | 358.8 tok/s | 362.2 tok/s | +0.9%; 80/80 |
| mixed decoder median TTFT | 923ms | 880ms | -4.6% |
| 67K cold effective prefill | 6,816 tok/s | 6,989 tok/s | +2.5% |
| 259K cold effective prefill | 5,848 tok/s | 6,110 tok/s | +4.5% |
| 259K warm TTFT | 1,627ms | 1,282ms | -21.2% |
| fresh 3-app locality token hit | 74.5% | 75.5% | one cold request/app in both |

The same-app shell benchmark's per-upstream split is derived from global
Prometheus deltas and was contaminated by concurrent production requests (one
nominal 12-request run counted 13 upstream requests). Its ten-run throughput
medians, 527.5 tok/s for rc2 and 501.5 for rc3, are therefore retained as an
adverse signal but not treated as a clean regression result. Add per-request
route correlation before using that benchmark as a promotion gate.

Decision: promote rc3 with the 32-block cap. Both upstreams stayed healthy;
the change is LB-only and did not restart engines or discard their KV caches.

## 2026-08-12 — rc4 exact route correlation

rc4 preserves rc3 routing and adds the opaque response header
`X-Mini-Dynamo-Upstream: 0|1`. The chat log records the same ordinal. This
allows a benchmark to attribute only its own responses without exposing Docker
service names or subtracting global counters. `concurrent_sameapp.sh` now uses
per-run temporary storage, checks curl/JSON failures, and requires every
response to contain a route ordinal.

Ten correlated 12-way shared-app runs completed 120/120 requests with zero
failures. Every exact split was 5/7, 6/6, or 7/5; median aggregate completion
rate was 480.5 tok/s under concurrent production load. This invalidates the
previous apparent 3/9 split and nominal 13-request sample: both came from
global Prometheus traffic, not the benchmark. Throughput remains load- and
acceptance-sensitive, but routing balance now has authoritative per-request
evidence. Decision: promote rc4 as an LB-only observability release.

The final post-promotion box c16/max256 code gate completed 80/80 requests at
1,259.7 tok/s, per-stream decode 119.2 tok/s, median TTFT 906ms, and p95 TTFT
1,254ms. This is 3.1% above the earlier repeated NUMA-pinned 1,220.5 tok/s
sample and within 0.4% of the 1,265.0 K5 peak, consistent with run-to-run load
rather than an rc4 throughput cost.

## 2026-08-12 — MAX_NUM_SEQS 8 versus 16

Production was isolated on one TP4 engine while the other was measured
directly, restarted at the candidate setting, and re-measured. Both used K5,
NCCL/P2P-disabled, a 4,096 batched-token ceiling, NUMA-local CPUs, and the same
r34 image. Five measured batches followed each warm-up.

| One TP4 | max seqs 8 | max seqs 16 | Delta |
|---|---:|---:|---:|
| code c8 aggregate | 633.4 tok/s | 615.5 tok/s | -2.8% |
| code c8 p95 TTFT | 1.34s | 1.61s | +19.5% |
| code c12 aggregate | 499.7 tok/s | **822.1 tok/s** | **+64.5%** |
| code c12 p95 TTFT | 4.54s | **1.56s** | **-65.6%** |
| code c16 aggregate | 477.8 tok/s | **942.4 tok/s** | **+97.2%** |
| code c16 p95 TTFT | 6.14s | **1.42s** | **-76.8%** |
| 33.6K prefill + 12 decoders | 289.3 tok/s | **397.4 tok/s** | **+37.4%** |
| mixed decoder p95 TTFT | 9.06s | **5.84s** | **-35.6%** |

The eight-sequence scheduler queues requests abruptly beyond c8; doubling the
active sequence slots removes that cliff. Candidate costs are modest: c8 had a
2.8% throughput sample and 19.5% p95 regression, warm startup increased from
475s to 535–540s, max CUDA graph capture rose from 48 to 96, and KV capacity
fell only 1.0%, from 3,880,487 to 3,842,835 tokens per engine (9.87x to 9.77x
full-context concurrency).

Decision: promote max sequences 16. Engine A passed authenticated direct and
then proxied acceptance before production moved to it; engine B was recreated
afterward and also passed direct acceptance. One engine stayed available
throughout. Post-promotion box results, 80/120/160 successful requests:

| Box concurrency | Aggregate | Per-stream decode | Median TTFT | p95 TTFT |
|---:|---:|---:|---:|---:|
| 16 | 1,214.5 tok/s | 116.9 tok/s | 887ms | 1,391ms |
| 24 | **1,625.1 tok/s** | 103.4 tok/s | 991ms | 1,198ms |
| 32 | **1,835.5 tok/s** | 85.3 tok/s | 1,085ms | 1,778ms |

The new throughput ceiling is 1.94x the earlier 946.6 tok/s mixed-workload
historical figure, though workload differences still prevent a direct config
speedup claim. At c32 the extra aggregate throughput trades per-stream rate;
c24 is the better latency/throughput operating point for routine benchmarking.

## 2026-08-12 — B12X MoE A8 versus A16

With max sequences 16 promoted, production stayed on engine B while engine A
was measured on `b12x-a8`, rolled to `b12x-a16`, and re-measured directly.
Both variants retained NCCL, K5, the 4,096 scheduler quantum, and the explicit
`VLLM_USE_B12X_FP8_GEMM=0` drafter-safety override; the changed variable was
the B12X MoE A8/A16 kernel selection. Five measured batches followed warm-up.

| Workload | A8 | A16 | A16 delta |
|---|---:|---:|---:|
| code c1/max512 | 207.0 tok/s | **240.4 tok/s** | **+16.1%** |
| code c4/max256 | 410.5 tok/s | **467.3 tok/s** | **+13.8%** |
| code c8/max256 | 631.4 tok/s | **688.2 tok/s** | **+9.0%** |
| code c16/max256 | 938.1 tok/s | **984.1 tok/s** | **+4.9%** |
| prose c1/max512 | 149.5 tok/s | **163.7 tok/s** | **+9.5%** |
| prose c8/max256 | 485.4 tok/s | **517.4 tok/s** | **+6.6%** |

A16 median TTFT was slightly better at every point, but some c4/c8 p95 samples
were noisier. Its one repeatable downside was the one-prefill + 12-decoder
single-engine workload: aggregate decode fell 397.4 to 384.7 tok/s (-3.2%),
median decoder TTFT rose 4.94s to 5.23s (+5.9%), and prefill TTFT rose 4.99s to
5.28s (+5.8%). KV capacity was unchanged within noise at 3.843M tokens.

Decision: promote A16 for the decode-heavy Helix agent workload and retain A8
as a one-variable rollback for a future prefill-heavy mix. Candidate A passed
direct and proxied authenticated requests before engine B rolled; B then
passed direct acceptance. One TP4 remained available throughout.

Post-promotion box A16 versus the fresh A8/max-seqs-16 box run:

| Concurrency | A8 | A16 | A16 delta |
|---:|---:|---:|---:|
| 16 | 1,214.5 tok/s | **1,384.4 tok/s** | **+14.0%** |
| 24 | 1,625.1 tok/s | **1,726.1 tok/s** | **+6.2%** |
| 32 | 1,835.5 tok/s | **1,909.7 tok/s** | **+4.0%** |

All 360 box code requests succeeded. At c32, median/p95 TTFT was 1.04/1.27s
and per-stream decode 87.7 tok/s. A c16 p95 of 5.62s was a production-contention
outlier; c24/c32 p95 remained 1.07/1.27s. A box mixed run with 12 decoders
completed 60/60 at 480.3 aggregate tok/s and 911ms median decoder TTFT, but is
not a direct backend comparison because no matched box A8 mixed sample exists.

A later c24 validation initially hit another live-traffic tail (1,523 tok/s,
5.79s p95). The benchmark was extended to record rc4's response ordinal and
rerun: 120/120 requests split exactly 60/60, producing 1,685.3 tok/s and 1.38s
p95. This confirms the clean 1,625–1,726 tok/s c24 operating range and makes
route imbalance an evidence-backed non-cause of the transient tail.

### A8-DGLin completion

`b12x-a8-dglin` keeps B12X sparse attention and A8 MoE but removes
`--linear-backend b12x`, using the upstream linear path. It started in 595s,
passed authenticated acceptance, and retained 3.844M KV tokens.

| Workload | A8 | A8-DGLin | A16 |
|---|---:|---:|---:|
| code c1 | 207.0 | 213.2 | **240.4** |
| code c4 | 410.5 | 419.0 | **467.3** |
| code c8 | 631.4 | 659.3 | **688.2** |
| code c16 | 938.1 | 951.8 | **984.1** |
| mixed aggregate | 397.4 | **400.4** | 384.7 |
| mixed median decoder TTFT | 4.94s | **4.88s** | 5.23s |

DGLin is 1–4% faster than A8 and essentially matches its mixed behavior, but
it is 3–11% slower than A16 on decode. Decision: reject it as the unified
default; retain it as the best measured prefill/mixed profile if separate
engine pools become useful. Engine A was restored to A16 and passed direct
authenticated acceptance while production stayed on A16 engine B.

## 2026-08-12 — rc5 privacy-bounded decision journal and replay

rc5 added paired route `start`/`finish` JSONL records and a static offline
policy replay tool. Start records contain a process-local sequence, endpoint,
request size, route parameters, rotation, and per-candidate opaque ordinal,
rank, overlap, bounded affinity, current/request load, and health. Finish
records contain the same sequence, actual upstream ordinal, result/status,
duration, TTFT, response size, and aggregate token counts. Prompt text,
request IDs, fingerprints, generated text, and upstream hostnames are omitted.
The feature is opt-in in the binary and enabled in node06's compose.

Local acceptance passed Python replay tests, `go test ./...`, `go vet ./...`,
formatting, build, and race tests for router/proxy. The LB-only rc4→rc5 swap
left both engines and their KV caches running. Both probes remained healthy;
startup confirmed prefix affinity, alpha 4, cap 32, 2MiB fingerprinting,
32KiB load units capped at eight, and journaling enabled.

Live validation used a fresh 12-way same-app batch and a sequential one-app ×
three-session × two-turn locality sample. The former completed 12/12 with an
exact 5/7 split. The latter cached 92,160 of 111,723 prompt tokens (82.5%); its
five warm requests each reused 18,432 tokens. The initial trace paired 18/18
starts and finishes, used unique sequences, and a schema scan found zero
forbidden prompt/fingerprint/hostname fields.

After a c24 code gate, the complete trace paired **114/114** records. Replaying
the deployed `(alpha=4, cap=32)` policy reproduced **100%** of choices. Every
positive alpha from 1 through 16 and cap from 8 through 64 also agreed on this
sample, while alpha 0 agreed only 77.2%; the workloads exercised cold load
balancing and idle warm affinity but not their conflict boundary. Therefore no
alpha/cap change is justified yet. A controlled prewarm-plus-load conflict
trace is the next router experiment.

The post-deploy c24/max256 code gate completed **72/72** requests with a 35/37
split, **1,654.2 tok/s** aggregate, **105.9 tok/s** median per-stream decode,
949ms median TTFT, and 1,375ms p95. This sits inside the prior clean
1,625–1,726 tok/s A16 operating range, so no rc5 throughput regression was
observed. Helix control-plane E2E remains an explicit open gate because the
documented credential was retired and no replacement secret was available;
direct authenticated LB traffic and both engine health probes passed.

### Controlled affinity-versus-load A/B

`bench/route_conflict.py` warmed a fresh 59-block (~21.5K-token) shared trunk,
started four returning long decodes on that engine, and admitted a short
returning probe. In the alpha-4 trace every probe snapshot saw 58 overlapping
blocks and four load units on the warm engine versus no overlap/load on the
other engine. Replay predicted alpha 4/cap 32 would retain all probes while
alpha 16/cap 32 would migrate all three.

The LB-only live A/B confirmed that prediction. Both variants ran on the same
unchanged engines and rc5 image; only `DS4_ROUTE_ALPHA` changed. Each sample
used three fresh trunks with four blockers and 1,024-token blocker budgets.

| Policy | Probe routes | Probe cached tokens | Median probe TTFT |
|---|---|---:|---:|
| alpha 4, cap 32 | warm engine 3/3 | **21,504 each** | **523ms** |
| alpha 16, cap 32 | cold engine 3/3 | 0 each | 3,198ms |

Aggressive migration made the returning probe **6.1× slower**. Decision: keep
alpha 4/cap 32. Restore was an LB-only recreation; startup confirmed alpha 4,
both upstream probes were healthy, and an authenticated `/v1/models` request
passed through rc5 with the opaque route header. This small controlled result
does not close the whole alpha frontier; vary blocker counts, prompt sizes,
and response lengths before considering adaptive policy.

## 2026-08-12 — native CPU KV offload 0 versus 1 GiB

The r34 launcher supports vLLM's experimental `OffloadingConnector`; its
`KV_OFFLOADING_SIZE` is total host capacity across all TP ranks. node06 had
only ~8.4GiB globally available host memory before the trial. Production was
single-homed on engine A, then engine B was measured directly, rolled with
exactly 1GiB offload, and re-measured. K5, A16, NCCL, max sequences 16, the
4,096-token scheduler quantum, and all other engine settings were unchanged.

The candidate initialized a shared 1.07GB mmap-backed CPU region and retained
3,843,150 GPU KV tokens (9.77 full-context concurrency), so offload augments
rather than shrinks the GPU tier. It reached readiness normally. Host available
memory settled near 7.4GiB, but physical free memory on its NUMA node fell to
about 0.4GiB.

| Direct engine B workload | Offload 0 | Offload 1GiB | Delta |
|---|---:|---:|---:|
| code c8 aggregate | **693.9 tok/s** | 571.2 tok/s | **-17.7%** |
| code c8 median TTFT | **849ms** | 1,194ms | **+40.6%** |
| code c8 p95 TTFT | **857ms** | 1,592ms | **+85.8%** |
| code c8 per-stream decode | 127.6 tok/s | 128.8 tok/s | +0.9% |
| 209K cold TTFT | **34.60s** | 36.23s | **+4.7%** |
| 209K effective cold prefill | **6,046 tok/s** | 5,775 tok/s | **-4.5%** |
| 209K warm TTFT | **2.03s** | 2.73s | **+34.5%** |
| warm cached tokens | 208,896 | 208,896 | unchanged |

Because even cache-resident and short decode workloads regressed before any
eviction/restore benefit was needed, and local RAM margin was poor, the costly
3.84M-token eviction-fill phase was not justified. Decision: reject native CPU
KV offload on this 128GiB box and roll engine B back to an empty
`KV_OFFLOADING_SIZE`. Keep the compose knob disabled for future higher-RAM
hardware qualification.

## 2026-08-12 — NCCL PCIe peer access disabled versus enabled

`nvidia-smi topo -p2p r/w` reports peer read/write support for every GPU pair
on node06, even though each TP4 group traverses PCIe host bridges (`NODE`) and
there is no NVLink. Production remained on engine A while engine B was rolled
between the recipe default `NCCL_P2P_DISABLE=1` and candidate `=0`. Both used
A16, K5, NCCL, max sequences 16, the 4,096 scheduler ceiling, no CPU offload,
the same pinned NUMA-1 CPUs, and fresh salts for long-context/mixed prompts.
The enabled candidate passed startup and authenticated direct acceptance; GPU
KV capacity changed only 0.1% (3,838,897 versus ~3,843,150 tokens).

| Direct engine B workload | P2P disabled | P2P enabled | Enabled delta |
|---|---:|---:|---:|
| code c1 aggregate | 233.4 tok/s | **240.8 tok/s** | **+3.2%** |
| code c8 aggregate | 658.3 tok/s | **677.7 tok/s** | **+2.9%** |
| code c16 aggregate | 986.2 tok/s | **1,147.4 tok/s** | **+16.3%** |
| mixed decoder aggregate | 388.4 tok/s | **539.9 tok/s** | **+39.0%** |
| mixed median decoder TTFT | 5.20s | **3.36s** | **-35.4%** |
| mixed 33.6K-prefill TTFT | 5.25s | **3.42s** | **-34.9%** |
| 209K effective cold prefill | 5,892 tok/s | **8,004 tok/s** | **+35.8%** |
| 209K cold TTFT | 35.51s | **26.14s** | **-26.4%** |
| 209K warm TTFT | 1.69s | **1.45s** | **-14.0%** |
| 209K warm cached tokens | 208,896 | 208,896 | unchanged |

Each c1 sample has five requests; c8/c16 and mixed have 24/48/36 measured
requests respectively; the 209K sample has three cold and three warm requests
per variant. Decision: promote P2P enabled through a rolling B-then-A update,
keeping `NCCL_P2P_DISABLE=1` as the one-variable rollback.

Promotion rolled B first while the LB served exclusively from A, passed direct
authenticated generation, moved production exclusively to B, then rolled and
accepted A before restoring the two-upstream LB. Both engines report P2P
enabled and both health probes are up. The post-promotion box c24/max256 gate
completed **72/72** measured requests at **1,879.4 tok/s**, **126.0 tok/s**
median per-stream decode, 937ms median TTFT, and 1,108ms p95, split 37/35.
Including its warm-up batch, rc5 journal replay reproduced 96/96 choices under
the deployed alpha/cap. This is 8.9% above the prior A16 c24 peak of 1,726.1
tok/s and 13.6% above the immediate pre-P2P rc5 gate of 1,654.2 tok/s.

## 2026-08-12 — scheduler ceiling 4,096 versus 4,160

With K5 and max sequences 16, vLLM reserves 64 speculative draft slots from
the configured 4,096-token batch ceiling and reports an effective scheduled
quantum of 4,032. Production stayed exclusively on P2P-enabled engine A while
engine B was measured directly at 4,096, rolled to 4,160, and re-measured with
the same A16/K5/NCCL/P2P profile. At 4,160 the reported effective quantum is
4,096. The setting generated a distinct compile-cache key, proving it was not
ignored.

| Direct engine B workload | ceiling 4,096 | ceiling 4,160 | 4,160 delta |
|---|---:|---:|---:|
| effective scheduled tokens | 4,032 | 4,096 | +64 |
| GPU KV capacity | **3,838,897** | 3,796,724 | **-1.1%** |
| code c8 aggregate | 757.6 tok/s | 760.7 tok/s | +0.4% |
| code c16 aggregate | 1,086.3 tok/s | **1,128.9 tok/s** | +3.9% |
| mixed decoder aggregate | 510.1 tok/s | **541.5 tok/s** | +6.2% |
| mixed median decoder TTFT | 3.66s | **3.26s** | -11.0% |
| mixed p95 decoder TTFT | **4.91s** | 5.52s | +12.5% |
| 209K effective cold prefill | 8,004 tok/s | 8,131 tok/s | +1.6% |
| 209K warm TTFT | **1.45s** | 1.60s | +10.3% |
| 209K warm cached tokens | 208,896 | 208,896 | unchanged |

The 4,160 candidate improves median throughput/latency at high concurrency but
regresses the mixed tail and long returning-session TTFT while reducing cache
capacity. Decision: retain 4,096 (effective 4,032) for the warm-context agent
fleet and roll engine B back. Revisit only if the workload becomes materially
more cold-prefill/high-concurrency oriented.

## 2026-08-12 — affinity/load boundary sweep and rc6 tie-break

`bench/route_conflict.py` was extended with configurable context and probe
sizes, then run sequentially at 4K, 20K, and 80K target contexts with 1/2/4/8
active 512-token blockers. Each point used a fresh trunk. The deployed alpha 4
and cap 32 retained warm 4K probes through two blockers, migrated at four, and
had replicated cache available at eight. It retained every 20K probe through
eight blockers. At 80K it retained 1/2/4-blocker probes with 87,296 cached
tokens and 0.71–0.92s TTFT, but the eight-blocker probe migrated cold, cached
zero, and took **8.34s**.

The 80K/eight-blocker journal snapshot showed 236 raw overlap blocks and eight
load units on the warm engine versus zero/zero on the cold engine. Both scored
exactly zero after the 32-block cap, so rc5's rotating load-neutral tie-break
selected cold. Static replay with an overlap tie-break changes exactly that
one of the four 80K probe decisions and leaves the other three untouched.

Decision: rc6 prefers deeper raw overlap on exact score equality. A strictly
better load-adjusted score still overrides affinity, preserving the useful 4K
four-blocker migration. Journal schema v2 records `score_tie_break=overlap`;
the replay tool understands v1 as legacy load-neutral behavior and can override
either with `--tie-break`. Local Go/Python unit tests, router/proxy race tests,
vet, build, and formatting passed before the LB-only deployment.

The rc5→rc6 deployment replaced only the stateless load balancer; both engines
and their KV caches stayed online. Three fresh 80K/eight-blocker validation
runs all retained the warm engine and reused **87,296 tokens**. Median probe
TTFT was **854ms**, versus the rc5 boundary miss at 8.34s. The v2 trace paired
30/30 starts and finishes, and replay reproduced all three decisive choices.

The post-deploy c24/max256 regression gate completed **72/72** requests at
**1,891.2 tok/s**, **125.0 tok/s** median per-stream decode, 934ms median TTFT,
and 1,088ms p95, split 35/37. This slightly exceeds the 1,879.4 tok/s P2P rc5
gate, so the tie-break change introduced no observed throughput regression.

## 2026-08-12 — native vLLM KV-event feasibility

r34 exposes vLLM's native ZMQ KV-event publisher with monotonically increasing
sequence numbers and a bounded replay socket. Production stayed exclusively on
engine A while engine B was rolled with only `--kv-events-config` added. The
publisher bound inside B's container; no event port was exposed on the host.
Raw `BlockStored` events contain exact token IDs, so a purpose-built probe kept
payloads in memory and emitted aggregate counts only.

During a 21K cold/warm context sample and c8 code run, the probe received 49
consecutive batches (sequence 0–48) with **zero gaps**: 321 `BlockStored`
events, 538 reported blocks, and no removals. It also exposed an important
integration constraint: DSpark emits several cache-group block sizes (256, 64,
8, and 4), so an exact consumer must honor group/cache-spec metadata instead of
assuming the configured 256-token physical block applies to every event.

After warming the same shapes, engine B was rolled back and rerun without the
publisher. Same-engine medians:

| Direct engine B workload | KV events on | KV events off | On delta |
|---|---:|---:|---:|
| code c8 aggregate | 769.6 tok/s | 754.7 tok/s | +2.0% |
| 21K effective cold prefill | 10,220 tok/s | 10,369 tok/s | -1.4% |
| 21K warm TTFT | 428.6ms | 430.9ms | -0.5% |
| warm cached tokens | 20,480 | 20,480 | unchanged |

The first long-prefill request after each restart paid shape/JIT warm-up and is
not used for the matched comparison. All 48 code requests and all matched
context requests succeeded. No material publisher overhead is visible at this
sample size; the interface is qualified for shadow-mode development, not yet
for routing production decisions.

Exact lookup also requires the rendered request token IDs. Direct r34
`/tokenize` measurements (three runs after warm-up) show why it must be
selective:

| Actual tokens | Median latency | Max latency | Response bytes |
|---:|---:|---:|---:|
| 299 | 3.70ms | 4.75ms | 1,553 |
| 4,279 | 8.37ms | 8.39ms | 21,453 |
| 21,000 | 41.34ms | 45.17ms | 105,059 |
| 83,721 | 202.78ms | 214.53ms | 418,665 |

Decision: leave native events disabled until a privacy-reviewed consumer has
gap detection, bounded replay, unrecoverable-gap fallback to the approximate
index, cache-group filtering, and shadow metrics. Do not put unconditional
`/tokenize` calls on the hot path; use exact lookup only for high-value
ambiguous decisions and/or a session-cached incremental design. Both engines
were restored event-off and healthy behind the two-upstream rc6 LB. Docker Hub
still listed r34 as the latest gilded-gnosis image at 05:08 UTC.

## 2026-08-12 — rc7 true first-token instrumentation

The route journal and `ds4proxy_ttft_seconds` previously timed the first SSE
response byte. A role-only chunk can precede generated content, so that value
is time-to-first-byte rather than TTFT and can bias both replay outcomes and
derived decode rates. rc7 detects the first non-empty content/reasoning/tool-
call delta for OpenAI and Anthropic streams. Journal schema v3 retains
`first_byte_ms` and makes `ttft_ms` the true generated-output timestamp;
offline replay treats legacy v1/v2 `ttft_ms` honestly as first-byte data.

Local acceptance passed ten Python replay tests, all Go tests, router/proxy/
usage race tests, vet, build, shell syntax, formatting, and diff checks. The
rc6→rc7 LB-only deployment left both TP4 engines and their KV caches running.
The first two live requests paired v3 starts/finishes; both fields happened to
share a read timestamp because vLLM delivered the first generated delta in its
first received chunk. Replay reproduced 2/2 decisions, and the Prometheus help
now describes first generated output rather than first response byte.

Two post-deploy c24/max256 gates completed **144/144** measured requests:

| Run | Aggregate | Route split | Median TTFT | p95 TTFT |
|---|---:|---:|---:|---:|
| rc7 gate 1 | 1,819.8 tok/s | 37/35 | 948ms | 1,319ms |
| rc7 gate 2 | 1,843.9 tok/s | 34/38 | 960ms | 1,270ms |

The 1,820–1,844 tok/s range is 2–4% below the single rc6 peak but remains above
the pre-P2P A16 range and has balanced placement, no failures, and normal TTFT.
Given the instrumentation-only data-plane change and known live-traffic noise,
no material regression is observed. rc7 is the production/default LB image;
both upstream health probes are up.

## 2026-08-12 — explicit KV-cache memory trial (rejected)

Production stayed single-homed on engine A while engine B was rolled with the
r34 profiler's conservative `--kv-cache-memory-bytes=53105596109` suggestion.
The candidate reserved 49.46GiB per GPU and raised reported KV capacity from
**3,838,897 to 3,883,559 tokens**: +44,662 tokens / **+1.16%**. It completed a
first-use 209K-token prompt plus three cold and three warm measured requests
without OOM, restart, or API failure. All warm samples reused 208,896 tokens.

| Direct engine B gate | Automatic control | Explicit bytes |
|---|---:|---:|
| 209K cold prefill | 7,728.2 tok/s | 8,097.9 tok/s |
| 209K cold TTFT median | 27,070.6ms | 25,834.5ms |
| 209K warm TTFT median | 1,527.4ms | 1,541.4ms |
| code c16 aggregate | 1,130.2 tok/s | 1,058.8 / 1,120.5 tok/s |
| code c16 requests | 48/48 | 96/96 |

The c16 repeat recovered to within 0.9% of control, so the first low result is
treated as shared-box noise rather than a reproducible regression. The repeat
also measured 50.0% draft-token acceptance and 3.50 effective tokens per
speculative step. Despite passing the safety and performance gates, explicit
bytes bypass vLLM memory profiling and couple available headroom to future
image, graph, and runtime changes. A 1.16% capacity gain does not justify that
operational fragility. Decision: retain automatic KV sizing; engine B was
rolled back before returning it to the production upstream set.

## 2026-08-12 — DSpark dynamic depth/capacity (rejected)

Production remained single-homed on fixed-K5 engine A. On engine B, the fixed
control and r34's supported `DSPARK_DEPTH_MODE=dynamic` default used the same
image, A16 backend, NCCL/P2P path, max-seqs 16, and 4,096 scheduler ceiling.
The candidate enabled compact varlen capacity verification, online sequential
temperature scaling, auto SPS profiling, and dynamic physical draft depth. It
auto-profiled a 40-draft-token budget and the following TP4 curve: 106.44
steps/s at one token, 84.71 at four, 54.30 at 12, 34.32 at 48, and 25.20 at
96. The six-point matrix completed 308/308 fixed+dynamic requests.

| Workload | Concurrency | Fixed K5 | Dynamic default | Delta |
|---|---:|---:|---:|---:|
| code | 1 | 227.5 tok/s | 175.7 tok/s | **-22.8%** |
| code | 8 | 742.2 tok/s | 671.7 tok/s | **-9.5%** |
| code | 16 | 1,130.4 tok/s | 1,029.7 tok/s | **-8.9%** |
| prose | 1 | 173.9 tok/s | 130.9 tok/s | **-24.7%** |
| prose | 8 | 564.1 tok/s | 519.6 tok/s | **-7.9%** |
| prose | 16 | 824.5 tok/s | 845.1 tok/s | +2.5% |

Dynamic mode's higher reported draft-token acceptance is not a throughput win:
it verifies a pruned denominator. Effective accepted tokens per engine step
fell from 3.22/3.48/3.53 to 2.87/3.16/2.86 on code and from
2.20/2.34/2.37 to 1.98/2.12/1.96 on prose. Reported KV capacity also fell
**1.1%**, from 3,838,897 to 3,796,598 tokens. Startup was about 670 seconds,
roughly two minutes longer than the fixed roll, including auto SPS profiling.

The 33.6K-prefill + 12-decoder gate completed 36/36 decoders and 3/3 prefills
at 501.0 aggregate tok/s, 3.48s median decoder TTFT, and 5.48s p95. Against the
same-profile fixed reference (510.1 tok/s, 3.66s median, 4.91s p95), that is a
1.8% throughput loss and 11.7% tail regression for a small median gain.

Aggregate diagnostics exposed two actionable r34 issues. First, the launcher
forces capacity activation at batch one even though hardware profiling chose a
threshold of eight and logged a mismatch warning. Second, the physical draft
controller repeatedly oscillated among depths three, four, and five; at one
c16 snapshot it retained only 21/80 possible draft tokens. Decision: retain
fixed K5. Revisit dynamic capacity only after profiled-threshold activation and
controller hysteresis are fixed or explicitly exposed for a matched retest.

## 2026-08-12 — Rust rewrite r1/r2 first rolling qualification

The v1.1 Go work was merged before branching `agent/rust-rewrite`. The first
Rust checkpoint reproduces typed configuration, prompt canonicalization and
chain fingerprints, overlap/load routing, bounded per-engine LRU indexes,
request shims, health/failover, response streaming and usage parsing, true
generated-output TTFT, journal v3, native metric passthrough, and the existing
`ds4proxy_*` Prometheus surface. The Go implementation remains in-tree as the
cutover oracle; Go-generated fingerprint vectors are Rust golden tests.

Local gates passed strict fmt/clippy, 19 Rust unit/integration tests, release
build, the complete Go suite/vet/format checks, and a distroless container
smoke test. The optimized binary is 7.3MiB. Immutable public images were
published to GHCR. r2 fixed the journal protocol; r3 removes a duplicated
parse/fingerprint pass before cache observation. The current candidate is
`ghcr.io/helixml/ds4-loadbalancer:rust-r3-59a8f08` (digest
`sha256:134548ce0b06617347a56e0d87461a310c79527b969533630a57a301307bb51f`).

The Go→Rust deployment replaced only the stateless LB. Both TP4 engines and
their KV caches stayed online and both authenticated probes remained healthy.
Fresh-salt matched gates:

| Gate | Go rc7 control | Rust r1 |
|---|---:|---:|
| locality cache hit (2 apps × 2 sessions × 2 turns) | 74.1% | 74.5% |
| concurrent same-app split / failures | 6/6, 0 | 6/6, 0 |
| concurrent same-app aggregate | 626 tok/s | 676 tok/s |
| c16/max256 aggregate | 866.2 tok/s | 1,114.1 tok/s |
| idle LB RSS | 11.3MiB | 8.9MiB |

The throughput difference is treated as a non-regression rather than a Rust
speedup because GPU serving and live traffic dominate this small sample. Rust
completed every measured request and preserved exact route correlation.

The first live r1 review found one observability regression: JSON tracing had
escaped each journal record inside an outer JSON message, breaking the existing
replay parser. r2 emits the original literal `[route_journal] {json}` protocol.
After the LB-only r2 roll, a 4/4 request smoke paired all starts/finishes and
`route_replay.py` parsed and reproduced 4/4 decisions across the requested
alpha/cap sweep. This validates the experiment loop itself, not just serving.

The r3 post-roll repeat retained the 74.5% locality result and completed
c16/max256 at 1,086.0 tok/s, within 2.5% of r1 and still 25% above the adjacent
Go sample; every request succeeded and both probes stayed up.

An actual Helix control-plane request used the test account's authorized org,
explicit `ds4-flash-node06` provider, and `deepseek-v4-flash`; it returned HTTP
200, finish reason `stop`, and the requested exact response. The separately
documented unmanned-org test app correctly returned 403 for this account, so no
cross-org access was assumed. The current state is Rust r3 live on node06 with
both engines healthy; Go rc7 remains a one-command LB-only rollback.
