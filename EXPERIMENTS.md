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

## 2026-08-12 — Rust rewrite r1-r4 rolling qualification

The v1.1 Go work was merged before branching `agent/rust-rewrite`. The first
Rust checkpoint reproduces typed configuration, prompt canonicalization and
chain fingerprints, overlap/load routing, bounded per-engine LRU indexes,
request shims, health/failover, response streaming and usage parsing, true
generated-output TTFT, journal v3, native metric passthrough, and the existing
`ds4proxy_*` Prometheus surface. The Go implementation remains in-tree as the
cutover oracle; Go-generated fingerprint vectors are Rust golden tests.

Local gates passed strict fmt/clippy, 22 Rust unit/integration tests, release
build, the complete Go suite/vet/format checks, and a distroless container
smoke test. The optimized binary is 7.3MiB. Immutable public images were
published to GHCR. r2 fixed the journal protocol; r3 removes a duplicated
parse/fingerprint pass before cache observation. r4 makes the compatibility
shim and router consume one parsed object and adds persistent Cargo caches to
the container build. The current candidate is
`ghcr.io/helixml/ds4-loadbalancer:rust-r4-ace17cd` (digest
`sha256:6519a0c1bad25007d9ecea83b8b60923c2f329466a2772d4d2aafef71b2a9f6f`).

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

r4's release-mode request-preparation microbenchmark measured 0.490ms for a
256KiB request and 4.531ms for 2MiB. That is 1.07× and 1.15× faster than the
initial two-parse Rust path, respectively. The retained Go shim + route +
fingerprint path measured 5.355ms and 44.347ms on the same development host,
about 10.9× and 9.8× slower. These are CPU preparation measurements, not claims
about GPU-serving throughput. BuildKit cache mounts reduced an unchanged local
container rebuild from 41.5s cold to 2.2s.

The r4 qualification used fresh salts and an adjacent LB-only Go rc7 control;
neither TP4 engine was restarted. The small locality gate matched exactly at
71.6%. Same-app requests completed without failures and split 6/6 on Rust at
682 tok/s versus 5/7 on Go at 667 tok/s. Three warmed c16/max256 runs produced
a 1,232.0 tok/s Rust median (`1267.3, 1232.0, 1222.3`) and 1,238.1 tok/s Go
median (`1164.8, 1238.1, 1286.0`), a -0.5% difference. The adjacent repeated
c24 points were 1,638.0 tok/s Rust and 1,636.7 tok/s Go. An initial post-roll
Rust c24 sample was only 1,420.3 tok/s, demonstrating why a single cold sample
must not decide an implementation comparison.

Every r4 measured request succeeded. A 36-request journal capture paired all
starts and finishes, reproduced 36/36 deployed decisions for the full
alpha/cap sweep, and showed an exact 18/18 engine split. Both upstream probes
remained up; idle LB RSS was 8.8MiB. Verdict: Rust r4 reproduces Go throughput
and routing behavior within run noise while materially reducing CPU-side
preparation cost. Leave r4 live; retain Go rc7 as the compose-default rollback.

An actual Helix control-plane request used the test account's authorized org,
explicit `ds4-flash-node06` provider, and `deepseek-v4-flash`; it returned HTTP
200, finish reason `stop`, and the requested exact response. The separately
documented unmanned-org test app correctly returned 403 for this account, so no
cross-org access was assumed. At the end of the r4 gate, both engines were
healthy and Go rc7 remained a one-command LB-only rollback.

## 2026-08-12 — Rust r5/r6 bounded tokenizer shadow

r5 added a selective remote exact-token adapter without putting tokenization on
the routing critical path. The one-pass preparation boundary derives a vLLM
`/tokenize` payload only for supported chat/completion requests in a configured
32KiB–2MiB window. After the client request completes, it uses a one-worker,
eight-slot non-blocking queue, a two-second deadline, authenticated direct
engine calls, and a 16MiB response cap. Any skip, queue pressure, timeout, HTTP
failure, or decoding failure leaves the approximate router unchanged. Token IDs
exist only for the lifetime of the background observation and are never logged,
journaled, or exported.

The first r5 shadow run found an observability-only bug: 48 short requests were
correctly labeled outside the size window but were also labeled
`invalid_payload` at completion. r6 made selection explicit across the relay
boundary so outcomes are exclusive. A post-roll short request then emitted only
`outside_size_window`. No client request or routing decision was affected by
the r5 accounting issue.

The live candidate is
`ghcr.io/helixml/ds4-loadbalancer:rust-r6-ed8e595` (digest
`sha256:49a04896fe22d1c29a962de1adb1d78f8df39fc84ea68ff871eac38d6ac8c1b4`).
The infra compose exposes all shadow controls with mode `off` by default; the
node06 experiment explicitly enables `remote-shadow`. Both TP4 engines remained
running through each LB-only roll.

Exactness and boundedness gates:

- The first eight long-prompt shadow requests succeeded 8/8. Their exact-token
  sum was **150,188**, exactly equal to the completion-usage prompt-token sum;
  aggregate background tokenization time was 373ms (46.6ms/request).
- `bench/tokenizer_parity.py` passed 7/7 direct-engine cases: plain chat,
  system/multi-turn, declared tools, tool-call history, reasoning effort,
  `think:false`, and normalized content. `/tokenize` count equaled real
  completion `prompt_tokens` in every case, and a repeated in-memory token-ID
  vector was identical. The harness prints neither prompts nor IDs.
- A 12-request eligible same-app burst completed 12/12 client requests, split
  6/6 at 646 tok/s, and completed all 12 shadow jobs. Queue depth returned to
  zero with no queue-full, timeout, HTTP, size, or decode failures.
- At capture, the r6 process had 73 intentional short-request skips and 13/13
  exact-token successes. Idle LB RSS was 10.6MiB versus 8.8MiB for r4.

Three r6 c24/max256 samples were 1,480.6, 1,672.8, and **1,637.4 tok/s median**.
The median matches r4 Rust (1,638.0) and the adjacent Go rc7 control (1,636.7)
to within 0.1%; the low first sample again shows cold/live-load variance. The
decision journal paired 86/86 requests, reproduced the deployed alpha-4/cap-32
policy exactly, and showed a balanced 43/43 split. Both readiness probes were
up and all inflight/load/queue gauges were zero after the run.

Verdict: leave r6 shadow mode live to accumulate cost and request-class data.
It validates the remote authority and backpressure seam, but exact IDs do not
influence routing until the KV-event index has sequence-gap replay, generation
fencing, and an automatic approximate fallback. Go rc7 remains the compose-
default rollback.

## 2026-08-12 — Rust r7-r9 local fastokens shadow

The local tokenizer gate used `dynamo-renderer` 5.0.1's native DeepSeek-V4
formatter and `dynamo-tokenizers` 1.8.0 / NVIDIA `fastokens` 0.3.1 against the
active node06 tokenizer artifact. A release-mode development-host probe found
identical Hugging Face and fastokens IDs at every size. Steady-state median
encoding cost crossed over strongly in fastokens' favor:

| Prompt tokens | Hugging Face | fastokens |
|---:|---:|---:|
| 4,180 | 3.03ms | 0.45ms |
| 20,564 | 16.46ms | 0.86ms |
| 82,004 | 90.28ms | 2.11ms |

The active vLLM r34 template has newer reasoning semantics than Dynamo 5.0.1.
Direct completion usage and `/tokenize` agreed in 13/13 cases and exposed four
effective classes: `none`/`low` render 9 tokens, default/`minimal`/`medium`/
`high` render 88, and `xhigh`/`max` render a newer 101-token "beyond maximum"
preamble. The local profile maps only equivalence-proven classes and fences
`xhigh`/`max` to remote authority. An initial r8 live matrix then found tool
history differed at the ID level despite matching counts; r9 also fences any
prior tool/function-call history while continuing to admit declared tools.

The first r7 distroless launch failed immediately because the newly linked
tokenizer stack required `libpcre2-8.so.0`. The restart loop was observed before
serving a request and the LB was rolled back to r6 within seconds; both engines
remained running and healthy. r8 added the exact runtime library and passed a
standalone container startup with the real read-only tokenizer mount before the
next LB-only swap.

r9's fresh live matrix produced **10/10 exact local-ID matches**, three explicit
remote-only fallbacks (tool history, `xhigh`, `max`), **zero mismatches**, and
13/13 remote-authority successes. A separate 18,762-token cold request matched
local IDs, remote IDs, and completion usage exactly. Across the subsequent
long-prompt and same-app gate, 14/14 admitted observations matched with equal
local/remote token sums of 254,844; local end-to-end worker time averaged
6.26ms versus 34.68ms remote, including JSON decode/render and first-use cost.
The queue returned to zero. Idle RSS is about 196MiB because the tokenizer is
resident, versus 10.6MiB for remote-only r6.

The 12-request same-app gate completed 12/12, split 6/6, and delivered 639
tok/s. Three c24/max256 samples were 1,523.9, 1,629.1, and 1,605.6 tok/s
(1,605.6 median), 1.9% below the r6 median and within the established shared-box
run noise; all 72 requests succeeded and were below the tokenization size
window. Both upstream probes remained up, all route load gauges returned to
zero, and the engines were never restarted.

r10 removes an unnecessary decoder from the resident tokenizer object by using
the underlying encode-only `fastokens::Tokenizer`. The full live matrix stayed
at 10/10 admitted matches, three remote-only fallbacks, and zero mismatches.
Idle/post-matrix RSS fell from r9's 196MiB to 108MiB; after two 18,762-token
observations it was 138MiB. The warmed second local worker observation took
6.53ms versus 34.59ms remote. This is a memory optimization, not a routing
change.

The live node06 image is locally built
`ghcr.io/helixml/ds4-loadbalancer:rust-r10-e32eae9`; GHCR rejected node06's
stored credential, so GitHub Actions owns public publishing. r10 remains
observation only: exact IDs do not influence routing. Next gates are an
Anthropic-input golden adapter, a versioned compatibility manifest, and the
fenced KV-event shadow index. Go rc7 remains the compose-default rollback.

r11 makes the first compatibility-manifest constraint executable: local mode
will not start unless the mounted tokenizer matches the configured SHA-256 and
the explicit `deepseek-v4-r34` profile is recognized. The node06 artifact hash
is `8f9f37ca37fdc4f5fd36d5cf4d3b0e8392edb4e894fd10cc0d70b4957c8633cf`.
A standalone startup and LB-only roll passed; a fresh 18,762-token request
again matched local IDs, remote IDs, and completion usage, with both probes up
and no restart. The live local image is
`ghcr.io/helixml/ds4-loadbalancer:rust-r11-8e38ec7`.

The legacy `ghcr.io/helixml/ds4-loadbalancer` package also denied the
repository Actions token despite job-level `packages: write`; its package ACL
is not inherited from mini-dynamo. The exact r11 image was retagged and
published publicly as `ghcr.io/helixml/mini-dynamo:rust-r11-8e38ec7`
(`sha256:e01f57188b87b80426bcf5a2e0b29964d27e4e78a272903705a4c303cdeda86b`).
An anonymous manifest read succeeded, node06 pulled that public tag, and the
post-swap 18,762-token observation matched local/remote/usage with both probes
up. CI now targets the repository-owned package.

## 2026-08-12 — r34 KV-event wire decoder and CI publish boundary

The exact installed vLLM r34 source on node06 defines a three-frame PUB feed:
topic, unsigned 8-byte big-endian sequence, and a MessagePack `KVEventBatch`
array. Events are tagged maps. `BlockStored` includes bytes-or-integer block
hashes, parent hash, exact token IDs, block size, and optional cache-group/spec
metadata; the replay ROUTER streams the same batches from an inclusive starting
sequence and terminates with sequence `-1`. No engine configuration changed
while inspecting this source.

`src/kv_wire.rs` now provides a transport-independent bounded decoder. A
synthetic fixture was encoded inside the active r34 container from its actual
`msgspec` classes, then checked into the Rust test without production hashes or
token IDs. Five tests cover exact decoding, payload and aggregate limits,
unknown-event fail-closed behavior, and block/token shape validation. Decoder
errors contain invariant names only. Strict Clippy, all **45 Rust tests**, all
retained Go tests, `go vet`, and both format gates pass. Sockets, event ports,
cache indexes, and routing remain untouched.

`examples/kv_wire_bench.rs` provides a reproducible release-mode allocation and
decode baseline using synthetic full-attention batches. On the development
host it measured 4.76µs for 256 token IDs (521 bytes), 324µs for 18,944 IDs
(56,660 bytes), and 1.408ms for 82,176 IDs (280,143 bytes): approximately
54–58M token IDs/s and 104–190MiB/s. Decode cost is far below the measured
local-render/tokenize cost at long context, so the initial consumer should
prioritize bounded queues, replay correctness, and index-update contention over
custom MessagePack parsing.

Manual workflow run
[`31579218509`](https://github.com/helixml/mini-dynamo/actions/runs/31579218509)
passed fmt, strict Clippy, tests, release compilation, and the complete
distroless image build. GHCR rejected only the final push with
`permission_denied: write_package`. GitHub's documented package model requires
the private repository to be added separately under **Manage Actions access**
when a granular package was created by a manual push; repository linkage alone
is insufficient. The package remains public and node06 remains on immutable
`rust-r11-8e38ec7`. No package was deleted/recreated and no long-lived personal
token was added as an Actions secret.

A final read-only node06 check found the Rust LB and both engines running with
zero restarts, both probe gauges at one, all inflight/load gauges at zero, and
the tokenizer queue at zero. The engines were not restarted.

## 2026-08-12 — bounded exact index and recovery integration

The next shadow-only layer follows Dynamo's correctness model without taking
its multi-thousand-worker concurrency machinery into this two-engine process.
`src/exact_index.rs` stores exact token blocks in a per-engine trie, retains
opaque vLLM hashes only for reverse removal/parent lookup, and verifies full
token-slice equality on every prefix step. Store batches preflight parent,
hash, path, and capacity invariants before mutation; removals are idempotent and
prune unreachable tombstones. Main-attention group metadata is learned exactly
as events arrive, while non-main, unknown, non-local, non-GPU, LoRA,
cache-salted, and extra-key state fails closed.

The sequence fence and index now compose as one state machine. Startup events
remain observation-only, live gaps suspend queries until a complete inclusive
replay is validated, a clear inside replay can establish an authoritative
generation, and invalid replay/index/capacity state clears the inventory and
increments the generation. Exact lookup returns no result whenever the fence
is untrusted. Eleven index/integration tests cover branching, duplicate stores,
eviction/re-add, atomic failures, mixed block geometry, group/namespace
filtering, concurrent reads, startup fencing, replay recovery, and failure
cleanup. Together with the replay-boundary test, the full Rust suite is now
**57 tests**.

`examples/exact_index_bench.rs` builds 48 synthetic 80.9K-token sequences: a
3,883,008-token inventory matching node06's measured engine KV capacity. On the
development host it built 15,168 blocks at 619K blocks/s and increased RSS by
21,936KiB. Exact lookup measured 2.55µs at 4,096 tokens, 11.54µs at 18,944,
and 50.33µs at 80,896. Store+remove pairs sustained 1.59M/s. Eight concurrent
long-context readers reached 101.8K lookups/s, about 5.1× single-thread
throughput. The simple per-engine `RwLock` therefore has substantial headroom;
ZMQ queueing/recovery and real event-shape qualification remain the next gate.

This code is not constructed by the running binary and exact IDs still never
influence placement. No node06 container or engine configuration changed.

## 2026-08-12 — bounded pure-Rust ZMQ transport interoperability

`src/kv_transport.rs` adds a pure-Rust vLLM event source: SUB receives the
three-frame live stream and DEALER consumes the ROUTER replay stream because
one replay request has multiple responses. It validates exact frame counts,
topic and unsigned big-endian sequence fields, applies the existing bounded
MessagePack decoder, and enforces one total replay deadline plus explicit
requested-batch and newer-tail limits. Missing, duplicate, out-of-order, early
tail, malformed, or incomplete replay fails closed without rendering payload,
hash, or token values in errors. The release probe links only the standard C,
math, and compiler runtime libraries; it has no native `libzmq` dependency.

A temporary node06 CPU-only container from the already-local r34 image exposed
Python `pyzmq` PUB and ROUTER sockets with the exact vLLM framing. The standalone
Rust release probe received live sequence 3, requested inclusive replay 1–3,
and validated all three batches plus the `-1` end marker. The Python peer
confirmed the DEALER request arrived as identity, empty delimiter, and starting
sequence. The probe binary and temporary container were removed after the run.

All **62 Rust tests**, Rust formatting, and strict all-target/all-feature
Clippy pass. Both production engines and the live Rust r11 load balancer stayed
up with zero restarts and both readiness gauges at one. This transport is not
yet constructed by the serving binary, so exact IDs still cannot affect
routing. The next gate is a supervised per-engine shadow consumer with bounded
reconnect backoff, trust/gap/replay metrics, and real-feed observation.

## 2026-08-12 — supervised shadow consumer lifecycle

The serving binary now has a default-off `off|shadow` KV-event mode. Shadow
startup requires exactly one validated TCP live/replay endpoint pair per
configured upstream. Each task owns an independent fenced exact inventory,
bounded transport/replay configuration, capped initial-connect backoff,
graceful shutdown, and `ds4proxy_kv_event_*` connection, trust, generation,
batch-outcome, replay-size, and resident-index metrics. The inventories are not
passed to the router, so this cannot change placement.

An initial standalone node06 lifecycle run exposed that the pure-Rust SUB
socket reconnects transparently: its message receive stays pending when TCP
disconnects, which made a naive connection gauge remain at one. The transport
now consumes the library's socket-monitor stream alongside messages. A repeat
CPU-only Python-peer run proved the corrected transitions:

- after reconnect, 22 startup batches remained observation-only;
- an explicit `AllBlocksCleared` advanced generation 2 and set
  `up=1`, `trusted=1`;
- 21 subsequent batches applied authoritatively;
- peer shutdown advanced generation 3, immediately set `up=0`, `trusted=0`,
  and cleared the zero-entry synthetic inventory.

The standalone binary and temporary containers were removed. Both production
engines and the live r11 LB remained at zero restarts with readiness one. All
**67 Rust tests**, strict all-target/all-feature Clippy, release compilation,
the retained Go tests/vet, and both format gates pass. The next gate is a
rolling one-engine real-feed observation; neither production engine has been
reconfigured in this experiment.

The immutable node06 build
`ghcr.io/helixml/ds4-loadbalancer:rust-r12-5a455fe` has manifest-list digest
`sha256:fe5d5c409988ea7d703a76389d945b07ffd8015af62b18b5348b31754d93ba58`.
The first LB-only recreate exposed that r11's `local-shadow` setting had been a
one-off compose override: r12 correctly defaulted to off from the checked-in
compose. It was restored within seconds, and infra commit `c5316b3` now makes
local shadow the persistent compose default while keeping KV events off.

A fresh 18,762-token request completed in 1.95s and produced exact equal local
and remote token sums of 18,762 with one `parity_match`; the first warmed r12
observation took 21.1ms locally and 38.0ms remotely. The 12-request same-app
gate completed 12/12, split 6/6, at 567 tok/s. Three r12 c24/max256 samples
were 1,504.8, 1,508.3, and 1,596.2 tok/s. Because these were below the historical
box class, an adjacent r11 rollback control measured 1,497.2 and 1,507.3 tok/s;
r12 then measured 1,572.0 after re-promotion. The matched control rules out an
r12 regression and attributes the low absolute run to current shared-engine
state/noise.

r12 remains live with `LocalShadow`, `KvEventMode Off`, both probes at one,
zero inflight/load/tokenizer queue, and zero engine or LB restarts. No engine
configuration or process changed during this rollout.

## 2026-08-12 — real r34 KV-event replay and exact-index qualification

Production was first single-homed on engine A through a brief r12 LB-only
recreate; engine A stayed up with zero restarts throughout. Engine B was then
rolled with r34's native ZMQ publisher enabled on container-only ports
5557/5558. The accepted r34 CLI keys use underscores
(`enable_kv_cache_events`, `replay_endpoint`, `buffer_steps`, `max_queue_size`);
JSON quoting through Compose and hyphenated nested keys were rejected before
model load. The corrected B process booted with publisher `zmq`, topic `kv`, a
10,000-step replay buffer, 100,000 HWM/queue bounds, and zero restarts. B was
never advertised by the production LB during this gate.

The first real 18.6K-token request correctly exposed a decoder incompatibility
instead of silently accepting incomplete state. A privacy-bounded live probe
showed that one request produced five KV groups: group 0 `mla_attention` at
256-token blocks and four `sliding_window_mla` groups at 64/64/4/8 tokens.
Only group 0 had one hash per token block; the masked non-main groups retained
the full token slice while omitting hashes. Decoding now accepts that wire
shape so the existing semantic group filter can discard it, while the exact
main-attention index still enforces one hash per token chunk.

A bounded replay probe then checked only in-memory hash membership and emitted
counts, never token or hash values. Replay from zero returned 37 contiguous
batches and 220 stores with 1,259 hashes. Of 579 main-attention hashes, exactly
two 4-token partial stores referenced parents absent from the entire event
stream (sequences 22 and 32); the normal 256-token MLA chain was complete.
This matches r34's partial-block implementation: it can reference an internal
fine-grained chain hash that is not itself emitted. The index now filters only
missing-parent stores whose block size is smaller than the cache group's
already observed root geometry. A missing canonical-size parent remains a
generation-fencing error.

Startup recovery was also corrected. Sequence zero directly establishes the
new process generation; a late subscriber requests a bounded replay from zero
through its first live sequence. Transport and index errors now expose only a
fixed reason label (`invalid_messagepack`, `invalid_replay`,
`index_parent_not_found`, and peers), retaining privacy and bounded metric
cardinality. These reason codes drove the real-feed fixes without rendering a
payload.

The final isolated r17 consumer requested and applied sequences 0–37, set
`trusted=1`, and built 650 exact blocks / 166,400 resident token IDs. Two
fresh locality turns raised the live applied count to five while preserving a
49.5% cache hit (18,432 cached on the returning turn). An eight-request direct
B same-app load completed 8/8 at 327 generated tok/s; afterward the consumer
had applied 14 live batches, remained trusted, grew to 728 blocks / 186,368
token IDs, and had exactly one initial connection with no reconnect, decode,
replay, or index errors. Exact state remained unreachable from route selection.

The canary was stopped, B was recreated with `EXTRA_VLLM_ARGS` explicitly
empty, and its normal warm boot completed in 545 seconds with zero restarts.
After model and health probes passed, r12 was restored to both engines with KV
events off and local tokenization shadow on. The post-restore gates were:

- locality: 8/8 requests, 74.2% aggregate cache hit, exactly two cold prefills
  for two fresh apps;
- same-app c12/max128: 12/12, exact 6/6 split, 397 tok/s;
- aggregate c16/max512: 16/16, 1,230.8 generated tok/s;
- both upstream readiness gauges one, zero residual inflight/load, and zero
  unexpected container restart counts after the intentional recreates.

All **71 Rust tests**, Rust formatting, strict all-target/all-feature Clippy,
the retained Go tests/vet/build, Go formatting, Python probe syntax checks, and
`git diff --check` pass. The locally built r17 canary is intentionally not the
production LB; production remains immutable r12. Next qualify the same feed on
A, add a longer removal/eviction soak and filter-reason counters, then compare
exact-score shadow choices with the approximate router before exact placement
is considered.

## 2026-08-12 — symmetric A feed and forced-removal qualification

The matching A-engine gate kept production single-homed on healthy B through
r12 while A was rolled with the same r34 ZMQ publisher configuration used on
B. A and the isolated r18 consumer both started with zero restarts. The
consumer connected before the first publisher batch, so sequence zero directly
established trust without replay. A fresh two-turn 18.6K locality request and
eight direct same-app requests produced 16 contiguous live batches. The exact
inventory remained trusted with 151 main MLA blocks / 38,656 token IDs and no
decode, replay, transport, or index error. B-only production returned HTTP 200
throughout.

r18 adds `ds4proxy_kv_event_filtered_total{upstream,source,reason}` with a fixed
seven-value reason vocabulary. It counted 92 non-main-attention events and nine
unreconstructable partial-block events in that first A workload, directly
confirming the two conservative exclusions inferred during the B probe. The
metric contains only bounded labels and counts—no prompt, token, or hash data.

r34 does not expose its internal prefix-cache reset method through this API
build; an authenticated `POST /reset_prefix_cache` returned 404 and changed no
state. To exercise real removals, A was therefore rolled once more with its KV
allocation temporarily constrained to 10GiB while all other engine settings
remained fixed. It initialized a 785,171-token pool, still 2.00× the configured
393,216-token maximum request context. A fresh 48-app × one-turn sweep then
processed 893,232 uncached prompt tokens successfully off the production path.

The retained replay was contiguous from sequence 0 through 191 and contained
1,200 store events plus 2,442 removal events. Per-group removed hashes were
group 0: 882, group 1: 195, group 2: 195, group 3: 130, and group 4: 1,040.
The main MLA stream contained 3,456 stored hashes, no orphan parents, and exact
256-token geometry. The live Rust consumer applied all 192 batches, filtered
2,520 non-main events, stayed trusted, and retained exactly 2,574 main blocks /
658,944 IDs: `3,456 stores − 882 group-0 removals = 2,574`. There was one
successful connection after bounded boot retries and no post-connect reconnect,
decode, replay, index, or generation error. This qualifies real eviction and
reverse-hash removal behavior, not only store/replay startup.

The canary was stopped and A was restored to automatic KV sizing, event mode
off, and 3,838,897 KV tokens with zero restart count. r12 was then restored to
both engines with local tokenizer shadow on and KV events off. Post-restore
gates completed as follows:

- locality: 8/8, 74.2% cache hit, exactly two cold prefills for two fresh apps;
- same-app c12/max128: 12/12, exact 6/6 split, 199 tok/s during shared-box noise;
- aggregate c16/max512: 16/16, 1,213.6 generated tok/s;
- both upstream gauges one, no residual inflight/load, and zero unexpected
  container restart counts after the intentional recreates.

All **72 Rust tests**, strict all-target/all-feature Clippy, Rust release build,
the retained Go tests/vet/build, both format gates, Python probe syntax, and
`git diff --check` pass locally. The r18 canary remains shadow-only and is not
the production LB. With both engines, sequence-zero/replay startup, hybrid
filtering, and real eviction now qualified, the next gate is exact-score shadow
telemetry against approximate production choices before request-side exact IDs
can influence placement.

## 2026-08-12 — r19 revision-fenced exact-score shadow

r19 connects exact tokenization and the fenced KV inventories only to
counterfactual telemetry. A naive post-response lookup would be self-biased:
the selected engine may publish the just-completed request before tokenization
finishes. The implementation instead captures each trusted inventory's
generation and monotonic revision at the approximate decision, then uses the
selected engine's response-reported pre-request `cached_tokens`. Alternative
engines are queried only if their generation and revision are unchanged. The
original candidate health/load snapshot, alpha, and overlap cap remain fixed;
exact overlap replaces only the cache term and never reaches placement. All
outcomes and overlap/gain histograms are bounded and contain no identifiers.

Both r34 engines were rolled publisher-on one at a time. Production stayed
single-homed on the opposite engine through immutable r12 during each 531s/541s
boot, and a direct authenticated short completion passed before traffic moved.
Both publisher threads started with zero container restarts. After the isolated
r19 subscriber connected late, one fresh 18.8K prefill per engine triggered
bounded replay from zero. Both inventories became trusted at generation zero,
each initially holding 73 main blocks / 18,688 token IDs, with one connection
and no reconnect or error.

The controlled exact-score gates were:

- 3 apps × 3 sessions × 2 turns: 18/18 responses, 82.3% engine cache hit,
  18/18 local-fastokens versus remote-vLLM parity, 15 `agree`, three
  `all_zero`, and zero exact token gain over the selected engine;
- forced miss: a fresh prompt was warmed directly on A without teaching the
  approximate router, then the identical request was cold-routed to B. Both
  calls returned HTTP 200; B reported zero cached tokens while unchanged A
  exact state held 14,336 tokens, yielding one `would_move` and a 14,336-token
  gain;
- c12 same-app/max128: 12/12, exact 6/6 split, 379 generated tok/s. All twelve
  post-response comparisons reported `inventory_changed`, correctly rejecting
  alternatives mutated by concurrent requests;
- c16/max512 with `TOKENIZER_MIN_BYTES=0`: 16/16 at 1,130.7 tok/s; all sixteen
  comparisons again failed closed on concurrent alternative mutation. This is
  a correctness stress, not a production-threshold performance comparison.

A second r19 canary used the production 32KiB tokenizer admission threshold.
Five interleaved c16/max512 runs per image produced a 1,343.4 tok/s r19 median
versus 1,362.1 for r12, a -1.4% difference inside the normal shared-box noise
band; all 160 requests succeeded. The r19 canary split its 32-request initial
pair exactly 16/16 and admitted no tokenizer jobs for the small payloads. A
reverse-order 2-app × 2-session × 2-turn locality pair then reused exactly
112,128 tokens through each LB (74.1% r19 versus 74.5% r12 because the fresh
prompt strings tokenized to different totals), with overlapping 0.59–0.69s
warm wall times. r19 recorded 12 `agree`, four `all_zero`, zero gain, full
parity, zero queue depth, and no KV reconnect.

All **76 Rust tests**, strict all-target/all-feature Clippy, Rust release build,
the retained Go tests/vet/build, both format gates, Python bench syntax, and
`git diff --check` pass locally and the GitHub Actions check passed. The public
r19 manifest-list digest is
`sha256:0e04dca9cc2f44733ccb31b09820cc96f81b70550be7228ca9021f4296aacc95`.

After CI, production was swapped LB-only from r12 to r19 with both engines and
their KV caches untouched. Late-subscriber replay recovered 348 A batches and
430 B batches; both generation-zero inventories became trusted with one
connection and no reconnect. Post-deploy gates were 8/8 locality at 64.8%
(six `agree`, two `all_zero`), c12 same-app 12/12 with a 6/6 split at 568
tok/s, and c16/max512 16/16 at 1,351.9 tok/s. One c12 tokenizer job exceeded
the bounded queue and was dropped without affecting any user request. Infra
now persists public r19, both container-only publishers, and event shadow mode
as the recreate-safe defaults. Exact placement remains disabled; r12 plus
event mode off is the LB-only rollback. The next architectural gate is
selective pre-route tokenization plus a versioned renderer/engine attestation;
pre-route lookup is required to observe useful counterfactuals under concurrent
cache mutation.

## 2026-08-12 — r20 manifest-attested pre-route exact shadow

r20 moves selective local tokenization ahead of the approximate decision, but
still does not allow exact state to change candidate order. The new
`off|shadow` exact-route mode requires local-shadow tokenization, KV-event
shadow, and a SHA-pinned compatibility manifest. The node06 r34 manifest binds:

- model `deepseek-v4-flash`, root
  `deepseek-ai/DeepSeek-V4-Flash-0731`, and context 393,216;
- runtime `/version`
  `0.11.2.dev280+gilded.gnosis.v20.vllm4d006a4.b12xcd3ce19.fi1ac6942.cu132.20260810.r34`;
- engine-image provenance
  `sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b`;
- tokenizer SHA-256
  `8f9f37ca37fdc4f5fd36d5cf4d3b0e8392edb4e894fd10cc0d70b4957c8633cf`,
  renderer profile `deepseek-v4-r34`, nine admitted request classes, and ten
  synthetic token-vector count/digest goldens.

The local tokenizer re-renders every golden at startup. Each 15-second health
probe also matches `/v1/models` and `/version` for every engine. Attestation is
cleared before the asynchronous version check; a monotonic attestation revision
rejects tokenization that overlaps an identity transition. Request admission
also rejects known template gaps and ungoldened combinations, including tool
history, `max`/`xhigh`, tools plus reasoning, custom templates/kwargs,
truncation, and non-generation-prompt rendering. Eight non-blocking CPU permits
are independent from the single post-response remote-parity worker. Permit
pressure, timeout, unsupported input, attestation change, untrusted KV state,
or an inventory revision change drops only the observation.

The committed manifest was generated twice from direct authenticated r34
`/tokenize` calls per case; all IDs were stable and only vector digests were
persisted. Loading the real node06 tokenizer artifact then passed all ten local
goldens before the test binary bound a port. The full local gate passed **84
Rust tests**, formatting, strict all-target/all-feature Clippy, release build,
the retained Go tests/vet/build and formatting, Python syntax, and
`git diff --check`.

Three isolated canary concurrency settings established the admission budget:

- one permit: c12/max128 passed 12/12, split 6/6, at 545 tok/s; 3 tokenized and
  9 immediately fell back as busy;
- four permits: matched c12 runs passed at 552–556 tok/s; 4–5 tokenized before
  the remainder fell back;
- eight permits: c12 passed at 561 tok/s with all 12 tokenized in one
  authoritative run. The final production-threshold build passed at 564 tok/s;
  eight tokenized, three were busy, and one deliberately conservative fallback
  overlapped the periodic identity recheck.

Sequential local tokenization averaged about 4.1ms across short and 18.8K-token
requests; revision-stable exact lookup averaged about 44µs. The final 32KiB-
threshold 3 apps × 3 sessions × 2 turns gate returned 18/18, reused 266,496 of
337,923 prompt tokens (78.9%), maintained 0.58–0.70s warm walls, and added 15
pre-route `agree` plus three cold `all_zero` decisions. All admitted jobs later
matched the remote r34 authority; short c16 aggregate inputs were correctly
outside the production admission window.

The decisive forced-miss control warmed a fresh 228,791-byte prompt directly
only on A, then sent the identical request through a cold approximate router.
The proxy chose B and B reported zero cached tokens, while the pre-route exact
inventory found **36,096** tokens on A and emitted one `would_move` before the
request could mutate either cache. A separate manifest with an intentionally
wrong runtime version kept both `ds4proxy_compat_attested` gauges at zero;
its request still returned HTTP 200 through approximate routing and recorded
only `unattested`.

Two reverse-order short c16/max512 pairs averaged 1,342.4 tok/s through r20 and
1,350.2 through production r19, a -0.6% difference inside shared-box noise.
All canaries had zero restarts and no proxy error/panic/fatal logs. A fresh
late subscriber stays connected but untrusted until a publisher emits its first
full-block event; a fresh long prefill then triggered bounded replay of roughly
436–523 retained batches per engine and restored trust. This is a safe readiness
property and should remain visible in rollout checks.

Production was then swapped LB-only to the exact commit image
`rust-r20-attested-shadow-195ea1f` (manifest-list digest
`sha256:53bdca913af8b48c76e5af76e5b938ad90a7efa003ea5baa29c9a4336150a08e`).
Both engine containers retained their original start times and zero restart
counts. The new LB started at 2026-08-12T13:54:46Z with both runtime identity
gauges attested and both event inventories connected and trusted. A fresh
2-app × 2-session × 2-turn locality gate returned 8/8 at 71.6% cache hit with
six exact/approximate agreements and two cold decisions. The c12 same-app gate
returned 12/12, split 6/6, at 578 tok/s; c16/max512 returned 16/16 at
1,370.3 tok/s. Across the startup trigger and gates, pre-route shadow admitted
17 requests, reported six `agree` and 11 `all_zero`, and fell back three times
under CPU-permit pressure. All containers remained at zero restarts and the LB
logged no error, panic, or fatal. A real internal-account Helix
`POST /api/v1/sessions/chat` through provider `ds4-flash-node06` also returned
HTTP 200 with the requested exact sentinel. The exact placement mode remains
absent; rollback is the stateless r19 LB image or
`DS4_EXACT_ROUTE_MODE=off`.

Verdict: r20 proves exact IDs and exact KV overlap can be joined before cache
mutation with bounded single-digit-millisecond frontend cost and independent
fail-closed fences. It was promoted only in `shadow` mode at the 32KiB
threshold with eight pre-route permits. Exact placement remains disabled until
production shadow distributions cover move gain, load conflict, attestation
transitions, and event recovery long enough to set a conservative route gate.

## 2026-08-12 — r21 health contract, Drone gate, and exact-placement canary

r21 makes replica health part of the serving contract and introduces an
explicit `DS4_EXACT_ROUTE_MODE=placement` canary without changing the
production default. `/health` returns opaque replica ordinals and aggregate
`ok`, `degraded`, or `unhealthy` readiness; zero healthy replicas returns 503.
The serving loop filters every known-unhealthy candidate before opening a
connection, while successful probes restore a replica to routing. Exact
placement is allowed only for a unique exact-score winner with at least 8,192
additional cached tokens and no more load than the approximate choice. The
existing renderer/runtime attestation, local-token admission, event trust,
inventory-revision, health, non-blocking CPU, and timeout fences all preserve
the approximate route on failure.

The local gate passed formatting, strict all-target/all-feature Clippy, release
build, all **96 Rust tests**, and the retained Go tests/vet/gofmt oracle. New
tests directly cover unhealthy exclusion, zero-healthy 503 without a dial,
retryable failover, probe recovery, `/health` aggregation, exact gain/load
gates, metric registration, Anthropic and Responses usage, malformed usage
preservation, and the existing tokenizer/attestation/fencing paths. A new Drone
pipeline runs the same Rust gates and Go oracle on push and pull request; both
Drone builds and GitHub Actions passed on draft PR #6.

The immutable node06-local image
`rust-r21-health-placement-0bdcb10` ran as an isolated canary on :8020/:8021.
Production remained on `rust-r20-attested-shadow-195ea1f`; neither engine nor
its cache was restarted. After one fresh full-block event triggered late-
subscriber replay, both canary inventories became generation-zero trusted and
both runtime identities attested. The controlled gates were:

- forced warm placement: four fresh 228,791-byte prompts were warmed directly
  only on A. All four proxy requests returned to A and reported 32,768 cached
  tokens; exact placement retained two approximate agreements and corrected
  two approximate misses (`moved=2`), with 4/4 fastokens/remote parity;
- locality, 2 apps × 2 sessions × 2 turns: r21 and the r20 control both reused
  107,520 / 150,188 prompt tokens (71.6%) with matching cold/warm structure;
- c8 same-app/max128: r21 split 4/4 with 8/8 success at 395 tok/s; r20 split
  4/4 at 406 tok/s;
- c16/max256: r21 completed 16/16 at 1,109.6 tok/s; r20 completed 16/16 at
  1,146.5 tok/s. The roughly 3% gaps are inside ordinary shared-box noise;
- degraded-health negative control: a disposable canary with replica zero set
  to a nonexistent host reported `degraded`, `0/1` health, and sent 4/4
  successful requests only to replica one. The disposable container was then
  removed.

Both the production r20 LB and the isolated r21 canary retained zero restarts
and both production upstream health gauges stayed one. r21 remains up only as
an isolated soak canary; production exact placement remains disabled pending
organic gain/load distributions and the recovery gate below.

The event-recovery gate then intentionally restarted only the isolated r21
container at 14:49:08Z. It returned to `ok` serving health with neither exact
trust gauge instantiated. The first fresh 18.8K-token request returned HTTP
200 cold through approximate routing and recorded one `inventory_untrusted`;
its B-side event triggered a 943-batch replay and trusted only B. A stayed
fenced until a direct full-block A event triggered an independent 885-batch
replay. Both generation-zero inventories then reported trusted, both runtime
attestations remained one, and the post-recovery four-request forced-warm gate
again produced two exact moves plus two agreements with 32,768 cached tokens
on every request. All five admitted local tokenizations matched remote vLLM.
There were no canary error/panic/fatal logs, no unexpected restart, and
production remained r20 with both upstream health gauges one throughout.

Verdict: startup and asymmetric per-engine replay fail closed without making
the inference path unavailable, and placement resumes automatically only after
both inventories are authoritative. The remaining promotion gate is an
organic distribution of exact gain versus load conflict, not another basic
recovery mechanism.

To make that distribution observable without enabling placement, commit
`718012c` splits the policy into an immutable evaluation and a separately
invoked mutation. `ds4proxy_exact_route_placement_total` now has a bounded
`mode="shadow|placement"` label. Shadow computes the same unique-winner,
8,192-token-gain, and zero-extra-load decision but never calls the candidate-
order mutation. Placement applies only a returned `Move(upstream)` outcome.
Unit tests compare the complete route before/after shadow `would_move` and
`kept_load_gate` evaluations; both remain identical. The full local gate passed
strict Clippy, release build, the Go oracle, and **97 Rust tests**.

The node06-local `rust-r21-shadow-policy-718012c` image replaced only the
isolated canary and ran with `DS4_EXACT_ROUTE_MODE=shadow`. A fresh A/B event
pair replayed 930/947 retained batches and made both inventories authoritative.
The controlled two-request proof then behaved as follows:

- an approximate agreement stayed on A and reused 32,768 tokens; policy
  telemetry reported `mode="shadow", outcome="kept_agree"`;
- the next prompt was again warmed only on A but approximately routed to B.
  It stayed on B, reused zero tokens, and telemetry reported
  `mode="shadow", outcome="would_move"` plus exact `would_move`.

Both tokenizations matched remote vLLM and both health, attestation, and trust
gauges remained one. A c8 same-app gate split 4/4 with 8/8 success at 388 tok/s
versus 401 for r20. Two reverse-order c16/max256 pairs measured 1,169.8 and
1,214.7 tok/s through r21 versus 1,252.5 and 1,189.2 through r20: 1,192.3
versus 1,220.9 tok/s averages (-2.3%, inside the established shared-box noise
band), with 64/64 successful responses. Production remained r20 and neither
engine restarted. One idle operational snapshot showed 234.2MiB RSS and 146
PIDs for r21 versus 235.0MiB and 146 for r20. The distroless image grew only
14,889 bytes (14,013,729 versus 13,998,840), so the additional controlled
metric dimension has no material deployment footprint.

The pre-existing r20 mixed production/qualification sample at this point held
120 routed requests and 33 admitted pre-route exact lookups: 12 agreements, 21
cold/all-zero decisions, zero `would_move`, and zero aggregate exact token
gain. Because that sample includes synthetic qualification traffic and lacks
the new gain/load-gate breakdown, it is not sufficient for placement
promotion; r21 shadow telemetry is the safe collection mechanism.

The post-merge public image workflow is a separate infrastructure blocker: the
image compiled successfully, then GHCR rejected the push with
`permission_denied: write_package`. Grant the repository package Actions
access and rerun; no source/build repair is indicated by that failure.

## 2026-08-12 — r21 production shadow promotion and replay-window recovery

The public multi-architecture image
`ghcr.io/helixml/ds4-loadbalancer:rust-r21-shadow-policy-718012c` (digest
`sha256:12bb463ad554099e856b3b5a8beb6a23002cdf2d3da96efea57b59f2834d49f3`)
replaced r20 in production with `DS4_EXACT_ROUTE_MODE=shadow`. This was an
LB-only swap: A and B retained their 12:11Z/12:21Z start times and zero restart
counts, so neither engine nor its KV cache was disturbed. `/health` returned
`ok` with 2/2 replicas, both runtime compatibility gauges attested, and the LB
had no error/panic/fatal logs.

The retained publisher histories had grown beyond the old 1,024-batch replay
limit. A 2,048-batch request with the old five-second deadline timed out on B;
raising the fail-closed deadline to 20 seconds recovered 1,051 batches in four
seconds in the isolated canary. During the production roll, overlapping full
replays from the old canary and the new LB initially timed out, and immediate
retries could remain queued behind stale ROUTER/DEALER identities. A direct
privacy-bounded protocol probe established that A's retained 0..1083 history
was intact and contiguous: 1,084 batches, 5,516 `BlockStored` events, and 5,946
indexable main hashes replayed in about one second with no gaps or removals.
After that drain, Rust recovered A's 1,076 retained batches in five seconds;
B independently recovered 1,084 batches in four seconds. Both inventories are
now generation-zero trusted at 5,985/4,963 nodes and
1,532,160/1,270,528 token IDs. The working diagnosis is stale replay-client
backpressure after aborted full requests, not corrupt publisher history. The
entire interval failed closed to approximate routing while `/health` remained
available.

Post-promotion qualification used fresh prompts:

- locality, 2 apps × 2 sessions × 2 turns: 8/8 successful, 71.6% cache hit;
- same-app c12/max128: 12/12, exact 6/6 split, 566 aggregate tok/s;
- aggregate c16/max512: 16/16, 1,343.2 aggregate tok/s;
- exact pre-route: 14 tokenizations, all remote parity matches; policy shadow
  recorded two `kept_agree` and 12 `kept_all_zero`, with no organic
  `would_move` in this small initial sample.

The populated LB used about 301MiB RSS. It retained zero restarts, both engine
health gauges stayed one, and both engine containers remained untouched. The
infra compose update pins r21 plus the qualified 2,048-batch/20-second replay
defaults; r20 is the LB-only rollback. Exact placement remains off until the
organic shadow distribution is large enough and replay cancellation/backpressure
has a deterministic recovery test.

The required real Helix workflow check did not reach inference. The supplied
internal-account credential received HTTP 403 for both the named test app and
`POST /api/v1/sessions/chat`; `/api/v1/users/me` returned 500. This is recorded
as an authentication/control-plane blocker rather than an r21 inference
failure. A current scoped smoke-test credential is still required to close the
end-to-end acceptance gate.

## 2026-08-12 — r22 immediate client-disconnect cancellation

r22 fixes a resource-lifetime gap in the Rust relay. Previously, the detached
upstream relay noticed a closed downstream only when its next chunk reached the
bounded channel. If an engine was silent during prefill or between chunks, its
request and the router's weighted load reservation could survive until that
next read. The relay now selects on `sender.closed()` and the reqwest byte
stream concurrently; downstream closure immediately drops the upstream stream,
which propagates transport cancellation to vLLM. Existing completion, usage,
journal, and error accounting remain unchanged, while
`ds4proxy_client_disconnects_total` increments exactly once.

The local gate passed formatting, strict all-target/all-feature Clippy, release
build, retained Go tests/vet/gofmt, and all **98 Rust tests**. The new loopback
test keeps an upstream response body permanently silent, drops the downstream
body, and proves within one second that the networked upstream body is dropped,
the disconnect metric increments once, and both inflight and weighted load
return to zero.

The isolated node06 image `rust-r22-client-cancel-16704db` then ran against
engine A only. A 4,096-token streaming request became active at 328ms; curl
closed at 2.000s, and the first 2.012s sample showed proxy inflight zero, route
load zero, vLLM running requests zero, and one disconnect. A normal
c8/max128 gate completed 8/8 at 338 tok/s. No production component changed
during the canary.

The public amd64 image (digest
`sha256:df6ff508caf54e09519a0106b6da7c131c76d4609500042644c55c41311e1fb2`)
was then promoted LB-only. Both engines retained their original start times,
container IDs, and zero restart counts. One fresh full-block event per engine
triggered clean 1,186/1,137-batch replays; both inventories became trusted at
6,529/5,071 nodes and 1,671,424/1,298,176 token IDs. The production
cancellation gate again activated engine A, closed the client at 2.000s, and
showed LB inflight/load plus both vLLM running gauges at zero by the first
2.019s sample, with exactly one new disconnect. Normal regressions passed:
c12/max128 completed 12/12 with a 6/6 split at 565 tok/s, and c16/max512
completed 16/16 at 1,354.0 tok/s. The LB and engines retained zero restarts and
`/health` remained `ok` with 2/2 replicas.

A separate, unpromoted replay-retry experiment changed reconnect progress to
mean "authoritative inventory restored" instead of merely "one live event
received," preventing a 250ms full-replay request storm after timeouts. Unit
tests passed, including fresh reconnect identities and bounded exponential
backoff, but live canaries encountered the known publisher-side backlog after
aborted full replays. That work remains isolated from r22 production until the
publisher supports cancellation/chunking or the recovery behavior is proven
deterministically.

## 2026-08-12 — r23 publisher-safe replay recovery and faster build loop

r23 resolves the replay-recovery blocker encountered after r21/r22 inventories
grew beyond 1,024 retained batches. vLLM r34 services replay synchronously on
its publisher thread: one DEALER request receives all retained batches followed
by an explicit end marker. Privacy-bounded framing probes showed that the
pure-Rust `zeromq` 0.6 client handled a 92-batch / 2.7MB tail in 11ms and a
102-batch / 3.0MB tail in 39ms, but a large replay could stop making progress
before the end marker. The same endpoint and request through the mature libzmq
implementation drained all 1,292 batches / 29.9MB in 77ms. This isolated the
failure below MessagePack decoding and exact-index construction.

Commit `c0c2874` retains the async pure-Rust SUB path for live events but moves
the exceptional replay burst to `spawn_blocking` with statically vendored
libzmq. Every attempt uses a fresh DEALER identity, a receive HWM sized for the
bounded response, one monotonic deadline, and zero linger. Framing, topic,
sequence, payload, and size errors are remembered while the worker continues
draining to the end marker, preventing a malformed response from stranding the
single vLLM publisher thread. A receive deadline or socket failure drops the
identity and fails closed. Reconnect backoff resets only after exact inventory
authority is restored, and an undrained timeout receives at least one full
replay-window delay before retry. The local gate passed formatting, strict
all-target/all-feature Clippy, release build, retained Go tests/vet/gofmt, and
all **104 Rust tests**.

The image
`ghcr.io/helixml/ds4-loadbalancer:rust-r23-replay-libzmq-c0c2874` has digest
`sha256:716dec709ffecc78b8b6ebf21ca984ab09dfa316d33bbd8996943cc9d13e53ee`
and a 14,245,839-byte runtime footprint. An isolated node06 canary connected to
both production engines. One 12.7K-token, one-output-token request per engine
was launched concurrently to generate the startup boundary. Both full replays
completed on the first attempt at 1,293 and 1,684 batches; both inventories
became generation-zero trusted at 7,384/6,063 blocks and
1,890,304/1,552,128 token IDs. Populated RSS was 270.3MiB versus 270.4MiB for
r22.

Canary serving gates then passed:

- downstream cancellation: curl closed at 2.013s, and 71ms later proxy
  inflight plus both vLLM running gauges were zero with exactly one new client
  disconnect;
- c12 same-app/max128: 12/12, exact 6/6 split, 562 aggregate tok/s;
- c16/max512: 16/16, 1,397.7 aggregate tok/s.

The public image replaced only the production LB. Engines A/B retained IDs
`2bc90280...` / `c8fd901e...`, their 12:11Z/12:21Z starts, and zero restarts.
One modest concurrent full-block trigger per engine restored 1,332/1,724
batches on the first poll, with both event and trust gauges one. Production
regressions completed 12/12 at a 6/6 split and 556 tok/s, then 16/16 at
1,462.9 tok/s for max512. `/health` remained `ok` with 2/2 replicas and all
three serving containers retained zero restarts. Exact placement remains
non-mutating shadow mode.

The development loop was measured as part of this run. The first local
Bookworm BuildKit image was cold—83.09s including base images, registry
downloads, and every dependency. The unchanged warm build took 2.31s, and the
14.2MB zstd-compressed Docker stream loaded on node06 in 3.80s. This replaces
source upload plus compilation on the live GPU host. `bench/build_transfer.sh`
prints build, transfer, and total wall time; `AGENTS.md` now separates the
2-5s focused Rust loop, the once-per-push full gate, and valid two-engine
crossover work from cache/routing tests that must remain serial.

Verdict: promote r23. It deterministically restores large retained inventories
without changing the serving policy or engine state and closes the replay-
backpressure prerequisite in issue #13. The next implementation priority is
issue #10's production-shaped DSML benchmark/correctness corpus, which becomes
the reusable oracle for the remaining P1 engine, routing, and effort-policy
experiments. The existing Helix credential blocker remains; no claim is made
for a new real-session E2E in this run.

## 2026-08-12 — issue #10 agent protocol corpus and fast CI loop

The first issue #10 slice adds a synthetic, versioned five-case JSONL corpus
and a standard-library-only runner. It covers deterministic non-stream text,
required streamed typed arguments, two parallel calls assembled by index,
automatic streamed tool selection with DSML-leak rejection, and a second pass
over preserved assistant reasoning plus tool history. The assembler accepts
both `arguments` and `input`, reconstructs arbitrary SSE and JSON delta
boundaries, parses typed values without printing them, and resets all state per
request. Eighteen GPU-free tests include deliberately broken split-marker,
typed-argument, and reasoning-history fixtures. Corpus validation plus the
complete Python test suite take about 0.2 seconds wall locally (the test body
itself reports 1-2ms).

Every live record carries the serving image, model revision/config digest,
tokenizer digest, router image, GPU count, official/deterministic sampling,
and controlled case identity. Per-request output is limited to structural
validity, route ordinal, finish/tool counts, usage, first response, streaming
TTFT, mean token interval, and wall time. Summaries add protocol-valid rate,
output/total token rate, cache hit ratio, and successful tasks per GPU-hour.
No completion, reasoning, tool argument, credential, fingerprint, or customer
content enters the artifact.

The first production-LB node06 gate used c1, zero synthetic prefix, and model
revision `9e165c30e2704aec5d9d593cce3eebd58bbef1cb`. The metadata helper now
extracts that exact `--revision` plus the local Docker content IDs for engines
and router, rather than relying on mutable image tags. All 5/5
deterministic cases passed, including two parallel calls, null/boolean/number/
array/object arguments, preserved reasoning history, and automatic streaming
with no DSML fragments. After a corpus warmup, the measured five cases finished
in 2.522s at 163.8 output tok/s and 737.9 total tok/s, with 53.0% aggregate
cache usage, 360.8ms streaming TTFT p95, 2.00ms median mean-ITL, and 892.1
successful tasks/GPU-hour across the eight-GPU box. A separate official
agentic (`temperature=1.0`, `top_p=0.95`) auto+stream regression passed 5/5 in
2.452s: no DSML leak, structured tool calls every time, 366.4ms TTFT p95,
2.00ms median mean-ITL, and 122.0 output tok/s. These are correctness-runner
qualification numbers, not a headline capacity comparison.

A warm c8 development cell then passed 10/10 protocol checks in 1.494s with a
balanced 5/5 upstream split, 841.4ms streaming TTFT p95, 3.73ms median
mean-ITL, 560.4 output tok/s, 2,499.3 total tok/s, 26.5% cache usage, and
3,012.8 successful tasks/GPU-hour. This validates concurrent stream/tool-call
assembly and the runner's aggregate accounting; it is still a one-run smoke,
not the three-run variance-qualified matrix.

Iteration timing was treated as a deliverable. The warm local image path is
already 2-3s plus about 4s to node06; this slice makes protocol failures local
and sub-second. Drone now fans Rust, Go, and Python protocol gates out in
parallel. A cold GitHub-hosted run took 6m08s, compiled test dependencies for
3m11s, then exposed a fixture race: the in-process async ROUTER could be
dropped after enqueueing its end marker but before the blocking libzmq client
drained it. Retaining the fixture until explicit client acknowledgement passed
100/100 focused and 10/10 full local suites. GitHub now uses a dependency-aware
Rust cache, including failed runs, so later no-lockfile iterations do not repay
that cold compile.

The local parallel pre-push gate found a second workflow trap: `/tmp` is a
31GiB memory filesystem and was already full, so a new worktree's separate
Cargo target failed after 4.74s with `ENOSPC` despite 283GiB remaining on the
disk-backed filesystem. Only this task's 298MiB of partial build artifacts was
cleaned. Reusing the canonical checkout target with compiler scratch under
`/home/karolis/.cache/mini-dynamo-tmp` completed strict Clippy, all 104 tests,
and the release build in 26.32s; Go and protocol lanes independently took
1.59s and 0.12s. `AGENTS.md` now forbids Rust worktrees/build scratch on this
shared tmpfs and documents the shared-target escape hatch.

The full 0/256KiB cold/warm c1/c8/c16 matrix and three-run variance requirement
remain intentionally deferred until the corpus/runner PR is green. This keeps
the shared production box available and avoids spending GPU time before the
correctness oracle and its CI gate are reviewed.

## 2026-08-12 — issue #11 targeted partial-prefill instrumentation

Before any rolling engine restart, the exact pinned r34 binary was queried for
its scheduler contract. It implements `--max-num-partial-prefills` (default 1),
`--max-long-partial-prefills` (default 1), and
`--long-prefill-token-threshold` (default 0, which disables the long-request
cap). The live argv confirms the global 4,096 batched-token budget, 16 sequence
limit, chunked prefill, and async scheduling remain the production baseline.

`mixed_bench.py` now optionally snapshots the direct engine's Prometheus
counters around a cell: request queue/prefill histogram sum+count, prompt
tokens, and preemptions. A 20ms background sampler captures peak running,
waiting, and KV usage. This makes the planned 2/1 partial-prefill experiment
mechanistically testable without relying only on end-to-end TTFT. A tiny
2,245-token prefill plus one 32-token decoder direct-engine smoke completed
2/2 requests, saw two requests running, zero waiting/preemptions, 0.01ms mean
engine queue time, 361.39ms mean prefill time, and 690.7ms decoder TTFT. The
collector's 21 GPU-free tests pass in about 0.2s locally.

The subsequent rolling B-only trial first single-homed production on healthy A
and held the global budget at 4,096. The 1/1/disabled baseline used a fresh
roughly 52K-token prefill beside eight 128-token decoders for three runs in
each arrival order. Prefill-first completed 24/24 decoders at 173.8 aggregate
output tok/s, with 5,083ms median decoder TTFT, 5,135ms median prefill TTFT,
eight requests waiting at peak, 3,573.68ms mean engine queue time, and zero
preemptions. Decode-first completed 24/24 at 146.0 aggregate output tok/s,
833.9ms median decoder TTFT, 5,740ms median prefill TTFT, no waiting, 0.01ms
mean queue time, and zero preemptions. This confirms arrival order creates a
large, directly observable queueing effect under the current scheduler.

The candidate `2/1/2048` cell could not start: the pinned vLLM/DSpark binary
raises `NotImplementedError: Concurrent Partial Prefill is not supported`
during argument validation. It restarted four times before the readiness check
surfaced the failure; no candidate request was served. A remained healthy and
served production throughout. B was immediately recreated from the unmodified
compose recipe, its argv was checked to contain none of the candidate flags,
and the normal two-engine LB configuration was restored after readiness. The
baseline B recreation took 9m28s from container start to `/health`, dominated
by weight load, compilation, KV profiling, and CUDA-graph capture; the LB swap
back to two healthy replicas took under 7s. The equivalent read-only
`EngineArgs._check_feature_supported()` preflight rejected the candidate in
16.5s.

Verdict: do not sweep 1,024/2,048/4,096 on r34—the threshold is inert unless
`max_num_partial_prefills > 1`, and that mode is explicitly unsupported by this
backend. Retain 1/1/disabled. Revisit only after an engine upgrade advertises
concurrent-partial-prefill support; add a fail-fast capability probe before any
future rolling restart so an unsupported flag combination never enters a
restart loop again.

## 2026-08-12 — issue #17 prefix-match-unit capability audit

The exact pinned r34 source and one privacy-bounded 15-second live-event probe
were inspected without restarting either engine. `CacheConfig` exposes
`prefix_match_unit`; for a multi-group cache, the runtime defaults its logical
hash/match unit to the GCD of all effective group block sizes when the flag is
unset. The live B probe observed hybrid group block sizes 256 (main MLA), 64,
64, 4, and 8 tokens. Their GCD is four, so production already matches at a
four-token logical boundary even though its main physical KV blocks are 256
tokens. The probe retained no token IDs or hashes and emitted aggregates only.

An explicit unit must divide every group size. Values 1 or 2 are therefore
syntactically possible, but can recover at most three extra boundary tokens
over the current default while increasing hash/index work by 4x or 2x. Values
above four make matching coarser. No plausible end-to-end benefit clears the
cost of a rolling trial, especially after the adjacent #11 work measured a
9m28s engine recovery cycle.

Verdict: retain the unset/default four-token logical unit and close #17. Reopen
only if an engine release changes the hybrid group layout/default resolution,
or profiling identifies hash matching—not physical KV storage—as a material
bottleneck. Any future audit should repeat the aggregate event probe first;
the unit and group layout are runtime compatibility identity, not an assumed
constant.

## 2026-08-12 — issue #16 response-grounded cache scorecard, first slice

The Rust LB now classifies completed responses from authoritative usage only:
`cold` means zero reported cached prompt tokens, `partial` is greater than zero
but below the reported prompt count, `full` covers the prompt count, and
missing or nonsensical usage is `unknown`. The bounded outcome labels feed a
request counter and, for streams where TTFT is observable, a cache-outcome TTFT
histogram. Existing prompt/cached-token counters remain the token-weighted
view; no prompt, fingerprint, ID, tenant, or session becomes a label.

The focused loop took 4.85s for the first compile, 0.16s for the next test, and
2.07s after adding the proxy recording assertion. The three-lane pre-push gate
ran Rust, Go, and protocol validation concurrently. Strict Clippy rejected an
exact float comparison after 2.04s; after the local fix, all 106 Rust tests,
Clippy, and the release build passed in 23.49s. The independent Go and Python
lanes had already passed in 0.55s and 0.10s, so they were not rerun. This keeps
the critical path at the single Rust release lane rather than summing all three.

This is instrumentation, not yet a cache SLO. The next #16 evidence must
reconcile response-derived totals with native engine counters and run a
fresh-salt working-set sweep before making a 95%+ cache-efficiency claim.

The cached publisher built and transferred r24
`rust-r24-cache-scorecard-bec4110` in 28.36s (22.31s build, 5.98s transfer).
The stateless LB swap reached 2/2 healthy in 1.761s; engines and their KV caches
were untouched. A fresh repeated 7.3K-token streaming prompt produced exactly
one `cold` and one `partial` observation. Cold wall/TTFT were 2.742s/741ms;
the repeated request was 405ms/380ms and reported 7,168 cached tokens. The
token-weighted counters reconciled to 7,168 cached of 14,590 prompt tokens
across the pair. Both requests returned 200, no upstream/client error counter
appeared, and both exact inventories were trusted after one bounded event
trigger per engine. r24 remains the node06 LB candidate/production image.

## 2026-08-12 — issue #16 reconciled working-set runner

`cachebench.py` turns the cache scorecard into a repeatable gate. It generates
only synthetic prefixes, schedules a cold wave across apps before reuse, and
emits content-free request coordinates and summaries. Each cell snapshots the
LB plus both engines and fails closed when response usage, LB prompt/cache and
cache-outcome counters, native prompt/cache counters, native prefix query/hit
counters, or engine request samples differ. `engine_metrics.py` now also
reports cached tokens, prefix queries/hits, and their token hit ratio.
Twenty-eight GPU-free Python tests pass in under 0.1s. The full parallel gate
then passed: Rust/Clippy/106 tests/release in 22.04s, Go in 0.44s, and the
corpus plus Python suite in 0.16s.

The first four-request 8KiB smoke reconciled exactly at 9,552 prompt and 6,912
cached tokens, with zero preemptions and 0.03ms mean queue time. An initial
broader run grouped same-app sessions and accidentally phase-locked every cold
request to round-robin engine A. That was a workload-order artifact: the
runner now round-robins apps before the next session/turn, and a unit test fixes
the coordinate order.

The corrected, fresh-salt 32KiB sweep completed 52/52 requests:

| apps | requests | route split A/B | reuse distance max | token hit | request reuse | TTFT p50/p95 | native queue mean |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 4 | 4/0 | 0 | 72.99% | 75% | 380/1,183ms | 0.04ms |
| 4 | 16 | 8/8 | 3 | 72.99% | 75% | 378/1,239ms | 0.05ms |
| 8 | 32 | 16/16 | 7 | 72.99% | 75% | 375/1,067ms | 0.05ms |

Every cell had zero reconciliation spread and zero preemptions. The aggregate
prompt/cached totals were respectively 36,828/26,880; 147,312/107,520; and
294,624/215,040 tokens. This qualifies counter semantics and the low working-
set regime only; the largest synthetic prefix set was 0.25MiB of source text,
far below either engine's 3.84M-token KV capacity. No 95%+ capacity/SLO claim
is made. The next capacity experiment must increase both app count and prefix
size until evictions or hit degradation become observable, while preserving
the zero-spread contamination gate.

The new per-outcome report was then checked with a second four-app cell. Cold
TTFT was 1,054/1,289ms p50/p95 and partial-hit TTFT was 383/396ms, with an 8/8
route split and zero reconciliation spread. The immediately preceding cell had
one transient 6.26s cold tail that did not repeat and had only 0.05ms native
queue time. Therefore even the low-working-set cold p95 is not yet an SLA;
retain the roadmap's three-run variance requirement.

Iteration follow-up: Cargo's implicit package file set had made Python and
operational-document edits invalidate the local Rust package fingerprint and
pay an unnecessary roughly 18s thin-LTO relink. The crate is not published, so
its manifest now explicitly packages only `src/`, `examples/`, `compat/`, and
the Cargo manifests, with the implicit README disabled. `cargo package
--allow-dirty --list` confirms `bench/` and operational Markdown are absent;
the warm release check after the manifest change took 0.16s. After changing
these experiment/agent documents again, the release check remained warm at
0.15s instead of relinking, proving the intended inner-loop effect.

## 2026-08-12 — issue #16 content-free KV churn telemetry

The fenced consumer already computes how many semantically accepted exact-
index blocks each batch stores and removes, plus accepted generation clears.
The LB now exports those totals by upstream and live/replay source. This adds
no decoding, retention, content-derived label, or routing work. “Removed” is
the metric action rather than “evicted”: vLLM's event proves that a block left
the index but does not encode whether capacity pressure, explicit release, or
another lifecycle event caused it. `cachebench.py` snapshots the live counters
and reports removed/stored block churn alongside native preemptions.

Rust tests cover store, remove, clear, metric registration, and bounded action
labels. The Python parser now supports exact label filtering, and its 29 tests
cover live-churn reporting without upgrading a removal into an eviction claim.
All expected live/replay/action series are initialized to zero at consumer
startup, so a quiet interval is distinguishable from an old or missing metric
family. The final warm gate passed strict Clippy, 109 Rust tests, and the
release build in 1.19s; 30 Python/corpus tests passed in 0.06s and the Go
parity oracle in 0.35s. Building the final r25 image still paid the expected
18.34s Rust relink inside BuildKit; build plus transfer to node06 took 27.71s.

The final `rust-r25-cache-churn-873a201` stateless LB swap took 1.586s and
returned 2/2 healthy replicas with zero restarts. A fresh 32KiB, four-request
smoke then reconciled exactly across response usage, LB counters, and both
native engines: 38,544 prompt tokens, 28,416 cached tokens (73.72%), four
request samples, and zero spread. It observed six accepted live stores, zero
removals, zero clears, zero preemptions, and 0.04ms mean native queue time.
The cold request took 1,049ms TTFT; the three partial hits had 392ms median
TTFT. This validates the counter plumbing and quiet-interval zero series; it
does not yet exercise capacity-driven removal.

## 2026-08-12 — issue #16 cache-capacity boundary and parallel iteration

A 512KiB canary established the experiment cost and scale: each request
rendered to roughly 145–154K prompt tokens, with 15.4s cold TTFT, 1.1s warm
TTFT, 571 accepted live stores, and exact cross-layer reconciliation. Running
the boundary sequentially would leave one TP4 pair idle and take roughly twice
as long, so `cachebench.py` gained a wave-barrier `--concurrency` option. It
runs apps within each session/turn wave concurrently but never starts a reuse
wave before every cold app has completed. A two-app smoke used both engines,
split 2/2, reconciled with zero spread, and completed in 1.50s. The Python
suite is now 31 tests and the release build remained warm at 0.20s after the
benchmark/docs-only edit.

The first fresh 52-app × 512KiB cell used concurrency two and finished in
8m23s. All 104 requests succeeded; usage, LB, and both native engines agreed
exactly at 15,155,148 prompt and 7,565,824 cached tokens, with zero preemptions
and 0.05ms mean queue time. It observed 29,554 stores and 6,077 removals
(20.56% churn), but every second-wave request was still a partial hit. Aggregate
token hit was 49.92%, as expected for one cold and one almost-fully-cached
request per app. This proves removals alone are not an eviction or survival
signal and validates the deliberately conservative metric name.

The next fresh 64-app × 512KiB cell crossed the actual residency cliff. It
completed 128/128 in 20m33s with exact zero-spread reconciliation at 18,644,316
prompt and 4,363,520 cached tokens. The cold wave placed 30 apps on A and 34 on
B (60/68 requests after reuse). All 30 A-side repeats survived as partial hits;
all 34 B-side repeats were cold. Reuse-wave token hit fell to 46.81%; overall
outcomes were 98 cold and 30 partial. Cold TTFT p50/p95 were
20,462/35,038ms versus 836/868ms for partial hits. The cell observed 55,671
stores, 51,429 removals (92.38% churn), zero preemptions, and 3.75s mean queue
time as the overloaded side serialized cold prefills.

Verdict: the cache cliff is sharp and placement-sensitive. A modest 30/34 app
imbalance preserved every reusable prefix on one TP4 pair while the other
thrashed completely. The next router experiment should price persistent KV
residency or cold-app placement balance, not merely instantaneous inflight
load. Repeat the 52/64 cells three times after that candidate; do not define a
95% cache SLO from these single boundary observations.

## 2026-08-12 — issue #13 cold-residency counterfactual

The 64-app cliff motivates a narrow extension to the already fenced exact
pre-route evaluator. For an all-zero lookup only, it snapshots each healthy,
trusted inventory's resident token-ID count under the same generation/revision
checks as exact prefix matching. It reports `would_balance` only when the
approximate choice holds at least one whole prompt more resident token IDs than
the least-occupied replica and that replica passes the existing load-delta
gate. Smaller deltas and excess target load receive explicit bounded outcomes.
The residency delta is a histogram, never an upstream or request identifier.

This path is telemetry-only even if warm-prefix exact placement mode is active:
the decision and candidate order are unchanged. Unit tests prove both shadow
and placement modes cannot move an all-zero request, the one-prompt threshold
holds, and the load gate fails closed. Strict Clippy and all 111 Rust tests pass;
the focused edit/compile/test loop took under four seconds after the first
incremental build.

Before the r26 LB-only roll, the live counters showed 2,075 applied batches on
A and 3,453 on B. The canonical 2,048 replay cap could no longer reconstruct
B from generation zero even though the publisher retains 10,000 batches. The
deployment cap is therefore raised to 8,192, preserving 18% publisher-history
headroom and the existing 20-second fail-closed deadline. This is an LB memory
and recovery-window change only; no engine restart or publisher mutation is
required. After one fresh request triggered each live stream, r26 replayed
3,546 B batches and 3,617 A batches from generation zero. Both inventories
became trusted in 2.923s including the 1.122s trigger requests, well inside the
deadline, while both engines kept their original start times.

Four subsequent fresh 128KiB cold requests remained on the existing
round-robin path and completed successfully. The new shadow evaluator reported
two `kept_all_zero` and two `would_balance` outcomes; the latter represented a
combined 508,672 resident-token delta. Both inventories stayed trusted and the
final exact-index residency was 4,853,248 token IDs on A versus 4,665,856 on B.
This proves the counterfactual sees a production-shaped capacity imbalance
without changing placement. The published r26 image is
`ghcr.io/helixml/mini-dynamo:rust-r26-cold-residency-b4b3b55` at digest
`sha256:ae7dc14c2d19579bb721e475c8a0936b61d49309ea0579ec760c287d9780df8f`;
the registry push reused all but one layer and took 4.21s.

The final public-digest LB-only swap took 1.67s. Both vLLM engine start times
and restart counts remained unchanged, `/health` reported 2/2 healthy, the
container resolved the 8,192 replay limit, and node06's Compose SHA matched
the canonical repository file. A fresh direct request triggered the quiet B
publisher after the first balanced LB trigger reached only A's live event
stream; the final public process replayed 3,637 A and 3,568 B batches and made
both exact inventories trusted. The two-request LB trigger itself succeeded
and split 1/1, but its native-metric reconciliation sampled only one engine's
eventual counters, so it is not claimed as a reconciled benchmark cell.

The post-deploy Helix correctness probe could not reach inference: the
provided internal account authenticated but received HTTP 403 for the
documented test app, and its visible app list was empty. This is an account/app
authorization blocker rather than an LB result; synthetic and direct-engine
gates remain green, but the Helix workflow gate must be repeated with an app
shared to that account.

## 2026-08-12 — issue #16 replica-residency scorecard

The 64-app result required a manual join between route ordinals and raw
upstream-labeled exact-index gauges. The next benchmark slice makes that join
native and content-free: `/health` now nests trust, resident block count, and
resident token count under each existing opaque replica index. It still bases
HTTP readiness only on serving health and never emits upstream addresses,
hashes, or token vectors. `cachebench.py` snapshots these values before and
after each cell, retains signed residency changes, and fails closed to `null`
for old/non-LB endpoints, malformed/missing inventory state, or a replica that
was not trusted at both snapshot boundaries.

The implementation adds one O(1) inventory-stats read per replica per health
call and no request-path work. Focused Rust health tests took 3.21s including
the incremental compile; all 33 Python tests took 0.07s. This should remove the
manual telemetry reconstruction from the three-run 52/64 boundary matrix.

The first live r27 smoke demonstrated why the trust gate matters. One replica
was still reconstructing from retained replay at the initial snapshot, so its
raw inventory grew from zero to 4,626,688 tokens during the cell. That is
recovery, not workload residency. The gate now preserves the start/end values
and trust flags but reports both changes as `null` in this case. The smoke
otherwise completed 4/4, split 2/2, reconciled every usage/counter view with
zero spread, observed zero preemptions, and reached 98.85% reuse-wave token
hits in 5.24s.

A second trusted-boundary smoke completed 2/2 on one sticky replica and again
reconciled with zero spread. It reported that replica's exact inventory moving
from 4,797,696 to 4,779,008 resident tokens (-18,688) while the other remained
at 4,626,688, alongside 37 live stores and 110 live removals. This is the
intended output: it reveals net per-replica residency independently from gross
publisher churn without claiming removals are evictions.

The public `rust-r27-residency-health-48ff0bd` image has digest
`sha256:eceb463dd63954b826076d3eda7b7e4cd2695597c037e2a495fe91d05247a90f`.
Its registry promotion reused all but the changed binary layer and took 4.08s;
the canonical node06 Compose now pins that digest.

The public-digest LB-only swap took 1.55s; both engines again retained their
start times and zero restart counts. A became authoritative after a 3,647-batch
replay. B's first 3,500-plus-batch attempt failed closed as `invalid_replay`,
reconnected, and—because this publisher is quiet until another allocation—
required one more direct cold request before replaying 3,577 batches and
becoming trusted. Serving health remained 2/2 throughout because exact state
is shadow-only. This is a recovery-latency opportunity: a failed replay should
be able to retry its known range after reconnect without waiting for a second
live event, while preserving the current publisher-backpressure protections.

## 2026-08-12 — replay-range retry without a second allocation

The r27 recovery observation is addressed narrowly. When a replay fails after
the consumer has learned its upper sequence, reconnect now discards the prior
generation and re-arms only one bounded complete `0..through` replay. It never
continues a partial nonzero range against cleared state. A range outside the
configured replay limit remains fenced and falls back to waiting for an
authoritative boundary; a failed retry does the same instead of looping.
Retries retain r23's fresh libzmq DEALER identity,
drain-through-validation, timeout floor, and exponential backoff; serving and
approximate routing remain independent.

A real in-process PUB/ROUTER regression sends exactly one live sequence, makes
the first startup replay incomplete, then serves a valid second replay. The
consumer reconnects, requests zero again, and becomes trusted without any
second live message. The focused test plus incremental compile completed in
4.84s. Node06 qualification should reproduce the final public-roll condition:
force one malformed/invalid replay in an isolated mock only, never mutate the
production publisher, then confirm ordinary retained replay still restores
both real inventories.

The first node06 r28 attempt was rolled back in 0.97s after B repeated
`invalid_replay` five times and then hit the 20-second drain deadline. Serving
remained 2/2 and both engines retained their start times and zero restart
counts, but automatic retries were amplifying a persistent validation error.
A read-only replay probe found the root cause: B returned 2,040 event-bearing
batches across sequence 0..3,581 with 1,542 legitimate holes. vLLM increments
the sequence per scheduler step but retains/publishes only steps with KV
events. The old transport incorrectly required every integer in the interval.
It now accepts a sparse replay only when events are strictly increasing,
in-range, and end exactly at `through`; this preserves authoritative no-op
steps while rejecting duplicates, regressions, early tails, and out-of-range
messages. The retry path is also single-shot: another failure returns to the
live-event gate rather than forming a replay storm.

The corrected r28 node-local image then rolled LB-only in 1.00s. One fresh
allocation per engine triggered first-attempt replays of 3,653 A event-bearing
batches and 3,583 B batches; both inventories became trusted with no
`invalid_replay`, reconnect, or retry. Neither engine restarted. A subsequent
2-app/2-session concurrent LB smoke completed 4/4 with a 2/2 split, exact
zero-spread response/LB/native reconciliation, zero preemptions, and 98.45%
reuse-wave token hits in 3.69s. Both exact inventories stayed trusted, and the
r27 scorecard reported signed net residency changes independently for each
replica. This qualifies sparse-sequence validation on the real retained
publisher histories; the retry branch remains a bounded fallback rather than
the normal path.

The public `rust-r28-sparse-replay-0f49a6d` image has digest
`sha256:f7d79cff932bc514b632188b97ab8b48b8495058a05028d80ca43fb793895f74`;
its registry promotion took 9.24s. The canonical Compose pins this tested
digest.

The first public-digest roll found B's sparse transport replay valid but its
index replay failed at the probe's single full-size missing-parent event
(`sequence=3542`, block size 256). Runtime was rolled back to r27 in 1.02s;
serving stayed 2/2 and engines again remained untouched. A child whose parent
was already removed or omitted from the retained/indexable generation cannot
be placed in the radix index. Omitting that child is nevertheless safe: exact
lookup under-estimates cache state and cannot manufacture a false hit. Such
stores now increment the bounded `orphaned_parent` filter reason. True shape,
duplicate-hash, conflicting-path, or capacity errors still fence and clear the
entire inventory. Unit tests distinguish this safe under-estimation from a
structurally inconsistent store.

The r28.1 node-local candidate built in 22.80s and transferred in 6.08s
(28.88s total); its LB-only swap took 0.87s. One fresh 32 KiB allocation per
engine restored both inventories on the first retained replay. A applied its
replay normally; B conservatively filtered two `orphaned_parent` stores and
eight unsupported partial-block stores. Both inventories remained trusted,
there were no invalid-replay retries or reconnects beyond the initial
connections, and both engines retained their start times and zero restart
counts.

The post-replay routed scorecard completed 4/4 requests with a 2/2 replica
split in 1.52s for the two-app cell. Response usage, LB counters, and native
vLLM counters reconciled with zero spread; preemptions stayed at zero; the
reuse wave achieved a 98.88% token hit rate. Both per-replica exact
inventories were trusted at both snapshots. The public
`rust-r28.1-sparse-orphan-7ffc1d0` image has digest
`sha256:1a6c56820991f5fcdf3f6af2bdd0ec867967e9b84f7ce8f61e0985453a7a428f`;
promotion reused all but the binary layer and took 2.84s. The canonical
node06 Compose pins this qualified digest.

The final public-digest LB-only roll took 0.99s and again left both engines'
start times and restart counts unchanged. B restored on its first replay. A's
first replay and the one permitted automatic retry both exceeded the
20-second drain deadline; the consumer then stopped retrying and remained
fail-closed as designed. One subsequent A allocation opened the live-event
gate, and its 3,666-batch replay completed successfully, restoring both
inventories to trusted state. A final public-image scorecard completed 4/4,
split 2/2, reconciled with zero spread and zero preemptions, preserved trusted
inventory boundaries, and reached 98.07% reuse-wave token hits in 1.56s. This
qualifies both the bounded failure path and live-event recovery, while also
leaving replay-drain throughput as a follow-up optimization rather than hiding
it behind unbounded retries.

## 2026-08-12 — repeated 52/64 residency boundary and faster observability

Before spending another long cell on the cache cliff, a matched four-app
512 KiB concurrency check tested whether more host parallelism would shorten
the loop. Concurrency two completed 8/8 in 37.14s with 0.05ms mean native
queue time and 18.15s cold p95. Concurrency four was 13% slower at 42.06s,
introduced 4.35s mean queue time, and raised cold p95 to 40.42s. The existing
c2 wave schedule is therefore the efficient setting: it keeps one long
prefill on each TP4 pair without queueing a second behind it.

The first fresh r28.1 52-app × 512 KiB attempt completed 104/104 in 594.58s,
split 50/54, and reconciled response, LB, and native vLLM counters with zero
spread. All 52 reuse requests remained partial hits with 99.91% reuse-wave
token hits. There were zero preemptions. Live block removals reached 106.02%
of stores, again confirming that gross publisher churn is not itself a
cache-survival measure. Both exact inventories were trusted at both snapshots:
replica 0 changed by -547,072 resident token IDs and replica 1 by +40,704
while reuse still survived.

This is a valid stress cell but not a matched repeat: its prompts averaged
161,818 tokens, 11.0% above the original cell's 145,723. The generator had
repeated the caller-provided salt throughout the entire 512 KiB prefix, so the
new `r29-*` spelling changed tokenizer density on every repetition. The runner
now hashes the salt/app into one fixed-size leading nonce and repeats a fixed
representative payload. Fresh salts still invalidate cache identity at the
first block, but salt spelling can no longer change the bulk token density.
Future boundary runs must first calibrate the corrected generator's reported
prompt count and must compare actual tokens, not the nominal byte size alone.

The corresponding oversized 64-app stress cell completed 128/128 in
1,715.73s with exact zero-spread reconciliation, zero preemptions, and a 60/68
request split. It averaged 161,742 prompt tokens per request, 10.99% above the
original 145,723-token cell. All 64 reuse requests were cold, reuse-wave token
hits were zero, and live removals were 100.19% of stores. Both exact
inventories remained trusted at both snapshots and changed by only -19,456
and -18,944 resident token IDs after the full churn cycle. This usefully
confirms complete thrash above the boundary, but it is explicitly excluded
from the matched 64-app repetition count because of the generator-density
confounder.

Corrected one-app calibration at 512 KiB produced 140,952 initial prompt
tokens, 3.27% below the historical target. Scaling only the fixed payload to
529 KiB produced 145,631 tokens, within 0.063% of the original 145,723-token
mean. The matched repetition therefore uses 529 KiB and records its actual
initial-wave mean; the nominal byte change compensates tokenizer density and
does not enlarge the token working set.

The corrected 52-app cell completed 104/104 in 654.55s with an actual
145,632-token initial mean, a 50/54 request split, zero-spread reconciliation,
and zero preemptions. All 52 reuse requests survived as partial hits with a
99.85% reuse-wave token-hit rate. Both inventories stayed trusted; their
signed changes were +2,560 and +55,296 resident token IDs. The now-joined cold
shadow deltas were 26 `kept_all_zero`, 26 `kept_balance_delta_gate`, zero
`kept_balance_load_gate`, and zero `would_balance`; all 52 reuse decisions
agreed with exact lookup. At this below-cliff working set there is no admitted
cold-residency move to evaluate, so the calibrated 64-app cell remains the
decision point.

Long cells previously emitted nothing until their final JSON record, making
iteration progress dependent on manual SSH and production-wide metric reads.
`cachebench.py --progress-every N` now emits only cell size, bounded completion
and success counts, wave ordinal, and elapsed time to stderr; stdout remains
clean JSONL. The same snapshot now includes zero-safe deltas for exact shadow
agreement and cold-residency `would_balance`, delta-gate, load-gate, and
all-zero outcomes. This directly joins the counterfactual with per-replica
residency without exposing prompts, fingerprints, token vectors, or upstream
addresses. All 35 cache/benchmark Python tests pass.

## 2026-08-13 — issue #32 Infernal Invocation r4 preflight boundary

Parallel upstream, node06, and benchmark-tooling reviews found that issue
#32's r2 target had already been superseded by Infernal Invocation r4. The
candidate is pinned as
`voipmonitor/vllm:infernal-invocation-vllm3226eb7-b12x1584743-fi1ac6942-cu133-torch213-20260812-r4`
at registry digest
`sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6`.
It retains the exact node06 model/tokenizer revision but moves to vLLM
0.26.1rc0, B12X 1.2.3, Torch 2.13, CUDA 13.3, and NCCL 2.31.2. The integration
trees remain the immutable identity because their constituent upstream PRs are
not all merged.

No engine or driver was changed. The image is not cached on node06; its
13.69GB compressed / 30.64GB unpacked footprint fits current disk headroom,
and the existing 152GB model snapshot is reusable. Driver 595.84 meets CUDA
13.x minor compatibility but is below CUDA 13.3's corresponding 610.43
driver. The image carries the forward-compat path, but CUDA/CuTe/FlashInfer/
B12X PTX/JIT startup remains an empirical hard gate. The first candidate must
therefore isolate B, keep A on r34, run a disposable GPU compatibility smoke,
force NCCL, and preserve current A16/K5/standard/MNS16/MBT4096/393K/GMU0.975
settings. Custom all-reduce is a later maintenance-window experiment because
node06 lacks the upstream direct-P2P registry configuration.

Benchmark provenance was the first implementation slice. The new one-engine
capture records configured image, local image ID and repo digests,
model/tokenizer revisions and artifact hashes, runtime packages, container
start/restart identity, CPU/NUMA placement, topology hash, an allow-listed
effective serving contract, and a secret-independent argv hash. An optional
upstream receipt is compacted to immutable source trees/packages and hard-fails
bounded image/digest/model/tokenizer/runtime mismatches. It never writes the
raw argv or credential values. Live r34 B capture succeeded and matched its
deployed digest/revisions; a synthetic live-r4 fixture verified against the
immutable upstream receipt. Thirty-eight Python tests pass across the full
benchmark suite.

The next local slice centralizes speculative accounting for direct decode
cells. It reports strict accepted/proposed tokens, proposed and accepted
tokens per speculative step, effective tokens per target step, and bounded
per-position deltas. Target-only, absent, partial, reset, no-draft, and
contaminated intervals remain distinct states. A cell is reconciled only when
native generation-token and finished-request deltas equal client completion
usage and successful request count; acceptance from production cross-traffic
can no longer appear valid silently. Direct requests also require generated
output and authoritative usage before counting as successful. The full Python
suite is now 41 tests.

The calibrated 64-app cell completed 128/128 in 1,634.03s with an actual
145,632-token initial mean, a 62/66 request split, zero-spread reconciliation,
zero preemptions, and trusted inventory boundaries. It reproduced the cliff:
31 reuse requests remained partial hits and 33 were cold, for 48.36%
reuse-wave token hits. Partial TTFT p50/p95 was 841/878ms; cold p50/p95 was
28.27/46.75s. Replica residency changed by +117,760 and -58,880 token IDs,
while live removals reached 99.58% of stores.

The joined counterfactual explains why the current cold-balancing proposal is
not yet a fix. It recorded 35 all-zero decisions, four below the one-prompt
residency-delta threshold, 26 blocked by the existing load-delta gate, and
zero `would_balance`. Under the efficient c2 schedule, the other TP4 pair is
already processing one full-load cold prefill when the residency imbalance is
observable. Removing that gate would trade cache capacity against known queue
isolation; keeping it means the candidate cannot affect this workload. Do not
promote cold-residency placement from this evidence. The next policy slice
must either model projected post-request residency under equal parallel load
or add an admission/capacity budget, and must remain shadow-only until it can
predict the 31/33 survival outcome without collapsing both requests onto one
engine.

A quiet direct r34 B smoke validated the new speculation reconciliation on
real native metrics. One measured 64-token code response reconciled exactly to
64 engine generation tokens and one finished request. Fixed K5 proposed five
tokens per speculative step, accepted 4.077, and delivered 5.077 effective
tokens per target step at 81.54% strict accepted/proposed tokens. Per-position
accepted deltas were 11/11/11/11/9. This is an accounting gate, not a new
performance result; it proves a clean interval is accepted before the r4 A/B.

## 2026-08-13 — Infernal Invocation r4 live canary rejected

The exact r4 registry manifest was pulled in 313.95s. Its manifest digest is
`sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6`;
the manifest config digest is the receipt's historical Docker image ID,
`sha256:b0cac4ef4037ed8880809df87c14ddc592ef234d59499864e1468448eb928cbf`.
Docker 29's containerd image store reports the manifest descriptor as `.Id`,
so identity capture now records and verifies descriptor and config digests
separately. The live process verified against the immutable upstream receipt,
including model/tokenizer revision and observed runtime packages.

The pull exposed a disk-safety boundary: root fell from 57GB free to 14GB
(97% used). Three exact, unused historical inference images were removed
(`vllm-openai:latest`, `ds4-flash:upstream-84cc882-sm120`, and the June DS4 v7
image), recovering 44GB and returning root to 58-59GB free / 85% used. Live
r34, r4, the Helix runner, and all load-balancer rollback images were retained.

Production was single-homed on r34 A before B changed. A first LB recreate
failed closed because one upstream was paired with two KV live/replay
endpoints; it was corrected in under a minute with matching A-only endpoint
lists. The reusable canary overlay now documents this cardinality requirement.
The exact image passed a one-GPU CUDA 13.3 / Torch 2.13 FP16 matmul on driver
595.84. The real B startup completed in 12m51s with NCCL 2.31.2/PyNCCL, A16,
fixed probabilistic K5, standard rejection, graph96, 393216 context, MBT4096,
GMU0.975, and no offload. It exposed 4,198,887 GPU KV tokens, 9.3% above
r34's approximately 3.843M.

The first smoke and first six-cell matrix were rejected because the inference
JIT monitor found ten late Triton/CuTeDSL compilations. With the persistent
75MB r4 cache populated, the repeated matrix had zero JIT, CUDA, NCCL, OOM,
Xid, or traceback markers. Every measured request and native engine interval
reconciled exactly:

| workload | c | r34 tok/s | r4 tok/s | delta | r34 TTFT ms | r4 TTFT ms |
|---|---:|---:|---:|---:|---:|---:|
| code | 1 | 236.4 | 232.4 | -1.7% | 342.4 | 345.0 |
| code | 8 | 736.4 | 641.1 | -12.9% | 851.7 | 759.0 |
| code | 16 | 1110.0 | 902.0 | -18.7% | 1031.8 | 820.0 |
| prose | 1 | 165.6 | 141.0 | -14.9% | 334.2 | 344.5 |
| prose | 8 | 547.1 | 471.9 | -13.7% | 858.3 | 754.2 |
| prose | 16 | 758.7 | 716.4 | -5.6% | 1055.1 | 814.0 |

The high-concurrency TTFT reduction and larger KV pool are useful, but matched
throughput regressed. More importantly, the production-shaped agent corpus
rejected r4 independently of performance. Its deterministic parallel-required
tool case emitted the requested two calls but leaked a DSML marker into
response content (4/5 valid). Its seeded temperature-1 profile also failed the
same case with three malformed/non-unique calls (4/5 valid). The adjacent r34
deterministic control passed 5/5 cold and 5/5 warm with the same model revision
and corpus. No response content or tool arguments were retained.

Verdict: reject r4 for node06 and do not spend another engine roll on MBT8192
or MTP0 until the deterministic DSML leak is fixed upstream. B was recreated
from the exact r34 control; production remained on A during its warm start.
The r4 image and versioned JIT cache remain available for a fixed successor.
The rollback smoke reconciled 64 client/engine tokens and one finished request;
the dual-homed LB then returned 2/2 healthy. B's fresh KV publisher became
authoritative after a direct 8K-token seed (32 blocks / 8,192 token IDs). A's
long-lived replay history was already too old for the new LB generation and
remained fenced, so exact routing safely stayed shadow-only. Do not restart A
for telemetry; this is a production reproduction of the snapshot/tree-dump
recovery gap already tracked in the KV-event roadmap.

## 2026-08-13 — fail-fast candidate gate and retained-replay boundary

The rejected r4 ordering was unnecessarily expensive: its deterministic
five-case agent corpus was already a hard correctness stop, but the first
qualification paid the 204-request code/prose matrix (154 measured requests
plus 50 warmups) before that oracle ran. `candidate_gate.py` now makes the
ordering executable. It binds the engine metadata/optional verified receipt,
container image and lifetime, agent provenance, model, and hashes of every
invoked runner into one plan. The ordered stages are a five-request
deterministic direct-engine correctness smoke, a one-run code/prose c8 scout,
and the existing full c1/c8/c16 matrix. A failed child, container restart,
identity drift, late JIT compilation, CUDA/NCCL/OOM/Xid/traceback/fatal marker,
or unreadable log interval stops the run before the next stage. Resume skips a
green stage only under identical candidate and plan hashes.

The journal persists no command, environment, credential, prompt, response,
reasoning, tool arguments, or container log. It records bounded status classes,
hashes and byte counts for privacy-safe child artifacts/log intervals, process
identity, and stage timing; benchmark JSONL artifacts retain mode 0600 beneath
a mode-0700 directory. `engine_matrix.sh` now accepts bounded workload,
concurrency, and run lists for scouts and emits per-cell plus total wall time
without changing its default matrix.

Eight focused unit tests cover ordering, agent fail-fast behavior, runtime
markers, process restart fencing, candidate/plan-bound resume, metadata
matching, journal privacy, and the real secret-free Docker inspect format. The
first live preflight found that Docker's Go template prints a literal `\\t`
unless the argument contains a real tab; identity therefore failed closed in
0.10s before any request. After the test-backed fix, an idle direct r34 B smoke
passed 5/5 protocol cases in 2.99s wall (2.731s child wall), with no runtime
marker, zero engine restart, and load back at zero. The same journal resumed in
0.09s without issuing GPU work. This is a workflow result, not a new engine
performance claim.

The same read-only node06 audit sharpened the remaining exact-KV recovery
boundary. A's r34 replay endpoint still retained a complete generation from
sequence 0 through 9,392: 8,380 sparse event-bearing messages and 408,572,558
serialized bytes, delivered by the publisher in roughly 0.2s. The current
8,192 fence correctly declines it. Raising the limit alone is unsafe on a host
with only about 8.5GiB available because the current transport collects every
decoded batch before applying it. The next router slice is a streaming full
replay into a scratch exact inventory followed by end/cursor validation and an
atomic swap. That recovers histories still inside vLLM's 10,000-step window;
Dynamo-style worker snapshot/tree-dump remains necessary after zero ages out.

## 2026-08-13 — r31 streaming full replay recovers long-lived A

Commit `23abc26` changes only full replay ranges that start at sequence zero.
The blocking ZMQ worker now validates and folds each decoded batch directly
into a private `FullReplayStage`; the trusted inventory is not visible or
mutated until the requested end/cursor is complete and the stage is committed
atomically. Invalid transport, index, capacity, or boundary state discards the
entire scratch generation and leaves the engine fenced. Nonzero gap replay
retains the existing bounded vector path. Unit tests cover invisibility before
commit, successful atomic replacement, failed-stage discard, transport fold,
and the consumer's failed-startup/reconnect path. Review then found and closed
two additional boundedness gaps: a dropped async replay now wakes its libzmq
worker within 50ms instead of retaining scratch state until the deadline, and
filtered cache-group metadata shares the exact index's node-count capacity.
The strict local Rust gate passed 119 tests, Clippy with warnings denied,
formatting, and a locked release build.

The LB-only image
`ghcr.io/helixml/ds4-loadbalancer:rust-r31-streaming-replay-23abc26`
(`sha256:f3a4a730f7c1bcd3d35382e81677bb39ed5943b15dac8e4a49f4e478ec6031a6`)
was built locally and loaded onto node06 in 29.38s total (23.42s build, 5.96s
transfer). The first diagnostic used the new 10,000-message limit but stopped
its observation after 60s and rolled the LB back automatically; both engines
remained untouched and healthy. A read-only tail probe then confirmed that the
publisher delivered sequences 9,300–9,392 in 83.9ms, narrowing the uncertainty
to full decode/index construction rather than replay availability.

The repeated canary recovered both exact inventories in roughly six seconds.
A applied 5,610 replay batches containing 69,262 stores and 32,220 removals,
ending trusted at 36,612 resident blocks / 9,372,672 token IDs. Fresh B applied
19 batches and ended at 59 blocks / 15,104 token IDs. Sampled LB RSS peaked at
427,184,128 bytes (407MiB) and host available memory never fell below
8,891,994,112 bytes (8.28GiB). Both engines retained restart count zero. A
post-recovery request through the LB returned HTTP 200 in 171ms and health
remained 2/2 with both exact generations trusted.

The review-hardened image
`ghcr.io/helixml/mini-dynamo:rust-r31-streaming-replay-99da044`
(`sha256:5c560e5b8a56c8ff40f43b17baff33edb53eab5b4610d00292e26967ed2b750b`)
was built and transferred under an equivalent staging tag in 28.30s total
(22.38s build, 5.92s transfer), then published byte-identically to the public
package above. Its LB-only final canary retained the qualified 10,000/20s
settings, re-established both exact inventories two seconds after the direct
event triggers, and left both engine restart counts at zero.

Verdict: promote the streaming replay path and set node06's deployment replay
limit to the publisher's 10,000-step retention. Keep the existing 20-second
timeout: the temporary 180-second value was diagnostic only and measured replay
does not justify slowing failure detection. Exact placement remains shadow-only.
Histories whose sequence zero has aged out still require a Dynamo-style
snapshot/tree-dump recovery protocol.

## 2026-08-13 — r32 session-stable exact-placement canary

r32 replaces the global placement switch with deterministic HMAC-SHA256
admission over one bounded `X-Session-ID`. The percentage is expressed in
basis points, zero is an instant rollback, and missing, duplicate, empty, or
oversized headers fail closed to shadow. The session header is removed before
both initial and failover dispatch. The secret has a redacted `Debug`
representation and its value is absent from logs, metrics, journals, and
Compose. Cohort
assignment happens at ingress before tokenization and inventory gates, avoiding
success-biased denominators. A current-health check is now atomic with load
reservation, closing the route-to-dispatch window for replicas already fenced
by probes.

Journal v4 records only a typed bounded cohort, the original approximate
choice, and the actually served choice. Offline replay remains valid for
approximate alpha/cap counterfactuals and can filter canary cohorts, while
observed cache/TTFT attribution follows the served engine. Review caught both
the pre-exact snapshot requirement and a previously free-form journal string
before deployment. Local gates passed 126 Rust tests, 52 benchmark tests,
strict all-feature Clippy, Go parity/vet/formatting, a locked release build,
and Compose validation. HMAC coverage includes standard short- and long-key
vectors plus a domain-separated cohort golden.

The LB-only staging image was built locally and transferred in 3.9 seconds;
no engine was recreated. With placement at 10,000 basis points, four fresh
32,768-token forced-warm requests produced two `moved` and two `kept_agree`
decisions. All four were served by warm A with 32,768 cached tokens. The
negative control omitted `X-Session-ID`: telemetry recorded
`missing_session` and `mode="shadow", outcome="would_move"`, while the request
remained on cold B with zero cached tokens. Recreating only the LB with
placement plus zero basis points recorded `disabled` and again preserved the
approximate cold route. This proves both useful correction and the rollback
path without admitting ordinary traffic.

Repeated LB canaries also exposed a new replay-scale boundary. Long-lived A
had reached sequence 9,485; its 9,426 event-bearing batch replay timed out
undrained at 20 seconds, then recovered authoritatively once inside a 60-second
window at 20,335 resident blocks / 5,205,760 token IDs. B recovered to 871
blocks / 222,976 token IDs. The timeout remains fail-closed and affects only
shadow inventory restoration. Later promotion rolls below show that this was
not a stable bound near the publisher's 10,000-step retention edge. Both
engines remained healthy with
restart count zero throughout. Production was returned to the public r31
image in shadow mode, 2/2 healthy and 2/2 exact trusted; the next LB promotion
can use r32 after its public package is available.

The byte-identical qualified image was published manually after the repository
`GITHUB_TOKEN` again received GHCR `write_package` denial despite an otherwise
green main quality job. The public artifact is
`ghcr.io/helixml/mini-dynamo:rust-r32-exact-canary-4c63eed` at
`sha256:77f287741188277825abddb9fa684e39fe54e91ca4bdbe86b17b8fb9e02ed0df`.
The Compose default is promoted to that immutable digest in shadow mode; the
percentage remains zero and no canary key is stored in either repository.

The public-image promotion reproduced an important limit: A's full replay
timed out undrained twice at 60 seconds before a final capped 180-second
attempt restored both inventories near the end of its window. A finished at
19,517 blocks / 4,996,352 token IDs and B at 909 / 232,704; health remained
2/2 and engine restart counts stayed zero. The default is temporarily 180
seconds because exact state remains fail-closed shadow telemetry, but this is
not considered a scalable fix. The next recovery work must use a publisher
snapshot/tree dump or another compact authoritative transfer rather than
replaying roughly 9,500 scheduler steps through the live ROUTER protocol.

## 2026-08-13 — r33 replay attribution and snapshot protocol decision

A read-only production audit resolved the main uncertainty left by r32. A's
successful startup reconstruction consumed 9,506 event-bearing batches and
about 483MB of LB network input, then committed 19,517 resident blocks /
4,996,352 token IDs. Across the eleven-minute observation the LB cgroup used
only 6.45 CPU-seconds, peaked near 610MB, and showed zero throttling, OOM,
memory pressure, or lingering socket queues. Docker events prove that the old
LB had exited before its replacement started. Rust decode/index work, final
inventory swap, host pressure, and overlapping consumers therefore cannot
explain the minute-scale replay wall time.

The pinned vLLM publisher services replay synchronously on its single Python
publisher thread with one blocking ROUTER send per retained batch. It cannot
publish live events while that loop runs and exposes no cancellation request.
This makes publisher delivery / ZeroMQ backpressure the leading diagnosis.
r33 adds privacy-safe replay profiles for total wall, request-to-first-frame,
post-first-frame stream wall, receive wait, maximum receive gap, decode, fold,
commit, wire/payload bytes, and requested/tail/message progress. Unlike the old
success-only batch histogram, partial progress is retained under a timeout.
The absolute deadline now begins at worker entry rather than after request
send, so the configured limit cannot silently become connect timeout plus
replay timeout. Unit tests cover delayed first frame, timed-out partial
progress, fold time, tail accounting, stale-profile clearing, and timeout
metric export. The strict local Rust gate passed 127 tests and all-feature
Clippy; Go and 52 Python benchmark tests also passed. The release build took
19.38s, Go gate 0.41s, and Python gate 0.13s.

Primary-source protocol review found that bare vLLM has only PUB live events
and bounded ROUTER replay from an eight-byte starting sequence through an end
marker; it has no initial-sync snapshot. Dynamo's `LocalKvIndexer` provides the
correct recovery shape: an event-range request can return an authoritative
`TreeDump`, real-event watermark, and reset scope when requested history is too
old. A direct wire implementation is not possible yet because Dynamo's tree
dump contains block hashes while mini-dynamo's forward radix index is keyed by
exact token slices. The selected incremental design is a long-lived per-engine
companion that consumes the existing r34 stream, keeps a bounded memory-only
block-digest index, and serves an atomic dump plus engine incarnation and
watermark. An LB subscribes live before loading the dump, drains and validates
the tail, then swaps atomically. This avoids engine restarts and removes LB
recovery time from vLLM's growing scheduler-event ring. The first offline gate
is the captured 36,612-block / 9.37M-token shape with a target below three
seconds and bounded RSS, followed by shadow comparison against the raw-token
index.

The LB-only node06 roll then produced decisive phase data. One bounded direct
seed per engine triggered late-subscriber recovery. B replayed 69 batches /
3,793,913 payload bytes in 137ms: 101ms to first frame, 34ms decode, 0.84ms
fold, and 0.04ms commit. A received 5,500 requested batches / 254,487,816
payload bytes, spent only 2.12s decoding and 0.16s folding, then stopped
receiving for one 177.52s maximum gap and failed closed at 180.04s. No tail
batches arrived. Both engines remained restart-zero and serving health stayed
2/2; B became exact-trusted and A remained correctly fenced in shadow mode.

Verdict: stop tuning replay timeouts and do not issue another full-history
probe against the production publisher. The measured 98.7% receive-wait share
and 177.52s gap prove synchronous publisher/HOL behavior strongly enough that
a no-op-fold replay would add production pressure without changing the design
decision. Build the snapshot companion behind shadow/fail-closed gates;
separately require a fixed Infernal successor to pass GPU-free DSML/parser and
C128A stride gates before spending another engine warm start on issue #32.

PR #40 merged as `afdd3ed`. The byte-identical node-qualified image was
published as `ghcr.io/helixml/mini-dynamo:rust-r33-replay-profile-afdd3ed` at
`sha256:26f7a30fb5523be5b8fdecc251545a33580eb9b4fb8c66eba4b512de7a32052f`.
The canonical and infra-mirror Compose defaults are pinned to that digest.
The already-running local candidate has the same manifest and was deliberately
not recreated after publication: doing so would trigger another synchronous
full-history replay against A without changing serving bytes or policy.

Operational follow-up found that A's one permitted automatic retry also timed
out after another 180.03s, with cumulative decode/fold still only 4.37s/0.32s
and cumulative maximum receive gaps 355.03s. The consumer then stopped the
immediate retry chain and returned to live observation, but any later live A
event could request `0..current` again while the configured 10,000-step limit
claimed that range was recoverable. The node06 default is therefore restored
to 8,192. A sequence beyond that bound now stays fenced/observe-only without
opening replay, while younger B histories remain recoverable. Exact placement
is shadow with zero canary basis points, so this reduces publisher pressure
without changing request routing.

The public r33 image was recreated once with the 8,192 limit and reached 2/2
serving health after the normal startup socket transition. One 256-repeat /
one-output-token direct seed per engine proved the boundary: A stayed fenced
and emitted no replay-duration series or replay reconnect, while B completed
its short replay and became trusted at 926 blocks / 237,056 token IDs. Both
engine restart counts remained zero. This is the final full-replay transport
experiment; further recovery work proceeds through issue #41's snapshot
companion and captured-shape mock gates.

## 2026-08-13 — issue #41 compact snapshot foundation

The first snapshot slice stays entirely GPU-free. A versioned named-MessagePack
contract carries an exact engine incarnation, real event watermark, reset
scope, digest algorithm/key identity, indexed and filtered group geometry, BFS
block records, redundant capacity declaration, and an opaque-body SHA-256
corruption check. Decode bounds the frame/payload, verifies the checksum before
the body, validates the expected incarnation/reset/digest before records, and
returns only a fully validated private body. Every error and reason label is
static and content-free. Cancellation is checked between phases and per record;
schema, checksum, capacity, group, parent order, cross-group, incarnation, and
contract failures never return partial state. SHA-256 is deliberately only a
corruption guard in this prototype. Production UDS sessions require an
authenticated, permission-restricted handshake and HMAC before accepting a
snapshot as authoritative.

The companion and LB use per-block commitments, not cumulative prefix hashes:
parent ordinals provide path scope and the LB hashes each request block once.
A separate arena-backed digest-index prototype retains no raw token vector,
uses a 256-bit SHA-256 commitment split into primary/guard halves, preserves
opaque engine hashes only for parent/removal identity, and poisons a compact
edge on any detected commitment/identity conflict. Seven tests cover exact
prefixes, branches, variable geometries, removal/reinsert, lookup budgets, and
forced primary/full collisions. This is computational exactness under a
cryptographic commitment; it is not information-theoretic equality after raw
tokens are discarded, so production remains shadow-only until golden parity
with the current raw-token index passes.

The captured node06 shape uses 36,612 resident blocks / 9,372,672 logical
source token IDs. Its snapshot is 5,710,914 bytes, encodes in 10.34ms, decodes
and fully validates in 10.79ms, averages 8.29ms across ten repeated decodes,
and peaks at 27,672KiB standalone RSS. This clears the issue's three-second
gate by roughly two orders of magnitude before index construction. The digest
prototype stores 65,536 commitment bytes instead of 2,097,152 raw token bytes
for a 2,048-block/524,288-token chain (32x smaller); build is 1.96ms and a full
path lookup averages 1.36ms across 100 runs, with 7,468KiB peak RSS.

Lifecycle decision: keep lookups LB-local. A companion UDS is lifecycle-only:
subscribe live, transfer snapshot plus watermark, stream strictly-newer deltas,
emit a caught-up fence, then let the LB atomically swap local digest state.
This adds no IPC/queueing failure mode to TTFT. One independently pinned
long-lived companion owns both per-engine states; missing companion state
fences exact routing while approximate serving continues. It binds engine
incarnation with the existing compatibility attestation plus engine
`process_start_time_seconds`, never mounts Docker's socket, and never retries
an unrecoverable same-incarnation bootstrap storm. Next: production digest
module/interface parity, snapshot-to-index benchmark, authenticated UDS
handshake, actor bootstrap, then a shadow-only node06 rollout with no engine
restart.

## 2026-08-13 — issue #41 production digest index and differential gate

The prototype is now a bounded production module, still disconnected from the
serving path. Its canonical commitment is HMAC-SHA256 over a versioned domain,
little-endian `u32` token count, and little-endian `u32` token IDs. The owned
32-byte secret is neither cloneable nor serializable, debug output is redacted,
and snapshots carry only its domain-separated key identity. RFC 4231 and fixed
wire goldens pin the implementation. Primary/guard halves preserve compact
radix lookup while every detected commitment or external-identity collision
poisons the edge until the whole generation is discarded.

Snapshot schema v2 adds `present` so a removed parent with live descendants
round-trips without losing the exact index's reinsert behavior. Import accepts
only a full-engine snapshot with exactly one indexed group until multi-group
overlap semantics are defined. It preserves the prior index on key, scope,
shape, capacity, cancellation, or record failure, bounds retained tombstone
hash memory, and preserves bytes/signed/unsigned external-hash identity.
Snapshot SHA-256 remains only corruption detection: authenticated UDS session
binding plus incarnation and monotonic-watermark freshness are still hard
preconditions for authority.

The public differential oracle ran 32 deterministic seeds x 1,000 mutations,
with two or more queries after every mutation, plus explicit variable-geometry,
opaque-hash, parent tombstone, and reinsert cases. Raw `ExactKvIndex` and
`DigestKvIndex` matches and normalized resident statistics were identical;
there were zero digest overclaims.

Matched release measurements on the development host:

- 316 blocks / 80,896 tokens: raw lookup 50.534us, digest lookup 235.350us
  (4.66x); raw build 0.254ms, digest build 0.331ms.
- 2,048 blocks / 524,288 tokens: digest lookup 1.529ms.
- 36,612-block / 9,372,672-token captured-shape snapshot: digest-index import
  13.063ms and 1,171,584 commitment bytes.
- Separate fresh-process 15,168-block / 3,883,008-token builds: raw exact RSS
  delta 21,548KiB, digest RSS delta 8,124KiB (37.7% of raw); build 13.022ms
  versus 17.066ms.

All CPU gates passed: 80K <=250us and <=5x raw, 524K <=2ms, snapshot import
<=100ms, and digest RSS <=60% of raw. This confirms the intended trade: much
smaller recoverable state with modest additional lookup CPU, not faster HTTP
or prefix lookup. No node06 process, container, or engine was changed. Next is
the authenticated companion session/tail lifecycle and a shadow-only gate of
at least 100,000 revision-stable comparisons before any placement use.

## 2026-08-13 — issue #41 authenticated snapshot exchange and tail fence

The next GPU-free slice closes the replay/mix-and-match boundary without
wiring any socket or serving behavior. A separate 32-byte session-auth key now
authenticates a fixed-width client hello before a companion may do snapshot
work. The response uses a fixed binary envelope; declared total, metadata, and
payload lengths are checked against aligned 32MiB/131,072-record limits, then
the exact borrowed prefix, named-MessagePack metadata, and snapshot bytes are
HMAC-authenticated before any response field is deserialized or payload copied.
This prevents attacker-controlled pre-auth owned-vector allocation. Response
metadata binds the fresh challenge, independently observed exact engine
incarnation, exact 32-byte block-digest key identity, monotonic real-event
watermark, companion generation, checksum, type, direction, version, and
lengths. The decoded snapshot is opaque and exposes read-only accessors; no
ordinary caller can manufacture authenticated snapshot state.

The lifecycle fence also separates the engine's sparse real-event watermark
from the companion's dense authenticated delivery sequence. vLLM advances its
sequence with scheduler steps but retains only event-producing steps, so strict
`+1` on the real sequence would incorrectly fence healthy sparse streams.
Instead the snapshot is delivery item zero; authenticated tail delivery must
be dense while real watermarks need only increase. Opaque tail, caught-up, and
identity frames have no public constructors until their authenticated decoder
lands. Incarnation/key/generation changes are checked before duplicate
handling and immediately revoke Ready; gaps, regressions, overflow, disconnect,
cancellation, unsupported reset scope, or caught-up mismatch are terminal
content-free fences.

Eight exchange tests cover authenticated hello/round-trip, wrong keys,
metadata/payload tamper, malicious u32/u64 lengths, stale floors, identity and
challenge mixups, truncation/trailing bytes, version/type/direction, domain
separation, and redaction. Eleven lifecycle tests cover sparse 1,000 -> 9,000
-> 100,000 real watermarks, dense delivery gaps/duplicates, watermark
regression, identity changes before duplicates, exact caught-up, resets,
disconnect/overflow/cancellation, and sequence overflow. An integration test
runs authenticated response -> bounded snapshot decode -> private digest-index
build -> lifecycle CatchingUp transition. Strict all-target Clippy is green.

This remains intentionally unwired. Production still requires UDS
`SO_PEERCRED`, fixed distinct UIDs/shared GID/socket permissions, separate
read-only secret files, authenticated tail/control decoding, absolute IO and
bootstrap deadlines, bounded clients/queues, atomic tail catch-up/swap,
metrics, and failure tests. No node06 process, container, or engine changed.

## 2026-08-13 — issue #41 authenticated Unix exchange and tail wire

The third GPU-free slice puts the authenticated snapshot protocol on a real
one-request Unix stream without taking ownership of pathname lifecycle. Both
client and server verify Linux peer UIDs through `SO_PEERCRED` before sending or
reading protocol bytes. One absolute timeout covers connect, authenticated
hello, snapshot production, I/O, and decode. Responses are bounded with a
max+1 read, malformed/truncated exchanges fail content-free, and dropping the
client drops the server's pending producer future immediately. The transport
does no path lookup, chmod, bind, rename, or unlink; those operations remain in
the future companion-owned listener where directory and inode invariants can be
enforced together.

Tail and control frames now use fixed authenticated binary envelopes. Their
ephemeral HMAC key is derived from the separate snapshot-session secret and is
bound to the fresh session challenge, companion generation, and direction.
Every MAC covers schema/type/direction, message and delivery sequences, sparse
real-event watermark, exact engine incarnation, digest-key identity,
generation, lengths, and payload. Authentication and bounds checks precede
MessagePack identity decode and payload copying. Replay, direction/session
mixups, tamper, gaps, and identity changes fail closed. An authenticated payload
is released only after the lifecycle accepts that exact delivery; any later
payload decode or private-index application error has an explicit terminal
`application_failed` fence and can never publish the partially updated
generation.

The warm focused gate completed in 3.4 seconds: seven Unix transport tests, nine
tail-wire tests, and twenty filtered tail/lifecycle tests all passed. Coverage
includes success, wrong peer UID/key, truncation, max+1 oversize detection,
single-deadline timeout, dropped-client cancellation, full authenticated-region
tamper, replay, sparse real watermarks, key derivation separation, redaction,
and application failure. This is still not connected to the proxy or node06.
The next slice is the safe socket/listener lifecycle plus bounded companion
actor and atomic private-index catch-up/swap; only after offline fault/load
tests pass should it be deployed shadow-only.

## 2026-08-13 — issue #41 filesystem boundary and atomic publication actor

Four isolated CPU tracks were integrated behind the authenticated exchange.
The verifier is now the sole production constructor of an opaque prepared
generation: it cross-checks outer and inner incarnation, digest identity,
watermark, and full-engine scope, builds the digest index privately, and returns
the already-accepted CatchingUp lifecycle with that exact index. The actor no
longer accepts a caller-supplied reset scope or arbitrary ready index. Three
integration tests reject outer/inner watermark mismatch, partial reset scope,
and a wrong digest secret before private state escapes.

Secret loading is synchronous startup-only and accepts exactly 32 raw bytes.
It walks an absolute normalized ancestor chain, rejects symlinks and unsafe
writers, requires the expected owner, regular file, one link, safe mode, and
matching pre-open/opened/post-open device+inode, reads at most 33 bytes, and
clears its temporary buffer. Seven tests cover valid loading, short/long/
newline/hex inputs, target/parent symlinks, unsafe modes, hard links,
non-regular files, owner policy, relative paths, and redaction.

Socket publication requires an absolute symlink-free companion-owned parent
that is not group/world writable. It binds a unique private Unix socket, sets
0660, publishes with an atomic no-clobber hard link, records device+inode, and
removes only that same socket inode. A pre-existing file, directory, symlink,
or socket is preserved; this deliberately requires a fresh runtime directory
after an unclean companion exit rather than guessing that a socket is stale.
Six real Linux tests cover connectivity/mode, all pre-existing target kinds,
unsafe parents, ownership/path policy, idempotent cleanup, and replacement
inode preservation.

The deterministic single-owner actor has a hard two-session ceiling and a
bounded tail queue per private replacement. Same-identity catch-up preserves
the published index; authenticated identity/key/generation changes revoke it
immediately. Tail application occurs only inside actor-owned state, application
failure and queue overflow fence only a private replacement, caught-up swaps
the whole index atomically, and monotonically allocated session epochs prevent
an old disconnect from revoking a newer publication. Thirteen focused tests
cover those race orderings, including prepared-identity mismatch and invalid
owner transitions. Focused actor/integration/Clippy validation completed in
5.8 seconds after the API review correction.

This is still offline: there is no runtime accept loop, long-lived tail stream,
KV delta decoder/application adapter, metrics, container service, or proxy
publication consumer yet. No node06 process changed. The next gate wires those
pieces under absolute deadlines and bounded cancellation, then exercises two
fast clients, a stalled third/slow reader, overflow, disconnect, and late CPU
completion before any shadow deployment.

The first PR #48 Drone attempt ran two identical cold Rust lanes concurrently
(`push` and `pull_request`) after the local 29.5-second and GitHub full Rust
gates had passed. An exact cold reproduction found one root-sensitive test
assertion: with a deliberately wrong expected UID, a non-root run rejects the
parent first as `UntrustedParent`, while Drone's root-owned directory remains a
trusted ancestor and the root-owned target is then rejected as
`UnexpectedOwner`. Both are the intended fail-closed result; the test now
accepts either content-free policy rejection. The reproduction also passed the
new Linux socket tests in 46.5 seconds and all Go/Python container gates.

To remove avoidable CI contention independent of that test correction, Drone
now runs feature work once for PRs targeting `main` and again only for the
post-merge `main` push. Its redundant release build was removed: the required
local pre-push gate still builds release, and GitHub builds the published
release container on `main`. Drone continues to enforce format, strict Clippy,
all Rust tests, Go parity/vet/format, and Python protocol tests.

## 2026-08-13 — issue #41 bounded runtime supervision and KV deltas

The first runtime slice remains transport-composable and offline. A Tokio Unix
listener supervisor owns a hard two-permit semaphore and never waits for a slot
inside the accept loop: a third accepted connection is dropped immediately, so
a slow client cannot stop admission or another client. Each handler receives
the same absolute deadline that encloses its future. Completion, failure,
panic, timeout, and watch-based shutdown all drop the owned stream and release
the permit. Four real Unix tests prove two-client isolation, stalled-hello
timeout and slot reuse, immediate third-client capacity rejection, and shutdown
cancellation; the focused supervisor plus strict Clippy gate took 10.2 seconds.

The actor callback now has a bounded digest-delta adapter. It decodes the exact
vLLM MessagePack batch contract, selects only the configured data-parallel rank
and cache group, local/GPU state, known main-attention stores, and unnamespaced
events, then applies store/remove/rank-scoped clear directly to actor-owned
digest state. Decode or index failure clears the whole index even when an
earlier event in the same batch succeeded; the actor then fences/discards that
generation, so partial overclaims cannot survive. Seven tests cover shaped
store/remove/clear, mixed and unsupported groups, malformed/wire bounds, a
capacity failure after partial mutation, wrong-rank clear, and content-free
errors. The warm focused delta gate is below one second after compilation.

This is not yet a runnable companion. The remaining runtime joins are distinct:
an LB-side authenticated consumer connecting the one-shot snapshot, long-lived
tail decoder, actor, and delta adapter; and companion-side production behind the
accepted-stream supervisor. Cancellation must reach bounded CPU work on both
sides. Metrics/config and Compose sandbox must land before an offline two-client
end-to-end fault test or node06 shadow trial. No production state changed.

## 2026-08-13 — issue #41 companion config and bounded observability

The companion's startup contract is now typed and off by default. Serve mode
requires distinct non-root companion/client UIDs, normalized absolute socket
and secret paths, and one matched live/replay endpoint pair per engine. Every
client, queue, deadline, frame, decoded-batch, and event count is bounded before
any listener or network work begins. Custom debug output reports only typed
state and presence/cardinality, never socket paths, secret paths, or endpoint
hosts.

The Prometheus surface is likewise content-free and pre-initialized. Its only
labels are fixed engine/client slots and closed enums. It records readiness,
active lifecycle states, terminal session outcomes and capacity rejection,
snapshot prepare/decode/apply/catch-up time and bytes, decoded tail batches and
events, fences/discards, identity changes, and published generation/index/token
counts. Protocol keys, hashes, incarnations, paths, endpoints, request/session
identities, and arbitrary error strings cannot create labels. Focused config
and metric tests cover safe defaults, all validation boundaries, debug
redaction, fixed cardinality, zero-valued series, and typed updates.

This remains library-only and off by default. It does not bind a listener,
export the collectors through the production metrics endpoint, change Compose,
or deploy to node06. The authenticated session join, sandbox, offline fault
matrix, and shadow comparison gate remain outstanding.

## 2026-08-13 — issue #41 authenticated LB snapshot consumer

The LB/client half of the runtime now consumes an already-connected Unix
stream under one absolute deadline. It verifies `SO_PEERCRED` before sending an
authenticated hello, reads a length-bounded authenticated snapshot, and builds
the private digest index on a cancellation-aware blocking worker. Authenticated
tail frames are read concurrently and queued by the actor during the build;
their dense delivery sequence is checked independently of sparse real vLLM
watermarks. The generation becomes visible only after exact caught-up.

A synchronous guard fences the actor epoch and signals blocking work whenever
the future returns or is dropped. Six real Unix-pair tests cover sparse happy
publication followed by authenticated disconnect, malformed tail MAC, delivery
gap, stale generation before actor admission, EOF revocation, and task-abort
revocation. The full Rust test suite and strict Clippy pass.

This is not deployed or connected to routing. The next runtime joins are an
outbound connect/reconnect owner with fresh challenge generation and reuse
prevention, plus companion/server snapshot and tail production behind the
separate accepted-stream supervisor. The offline fault matrix must still add
explicit deadline, oversized/truncated framing, slow-build cancellation,
two-session replacement, live store/remove after publication, and coordinated
incarnation/key rollover cases before node06 shadowing.

## 2026-08-13 — issue #41 companion snapshot/tail producer

The companion/server protocol half now fits directly behind the bounded Unix
accept supervisor. It checks peer credentials and authenticates the fixed hello
before source work, establishes bounded live-tail capture before starting the
snapshot build, emits one authenticated length-framed snapshot without relying
on EOF, and derives the companion-to-router tail key for the remaining stream.
Tail message/delivery sequencing is dense while real engine watermarks may be
sparse. Identity rollover sends an authenticated fencing control and ends the
session.

The engine-independent source interface returns owned snapshot work, exposes a
bounded publisher with async backpressure plus non-blocking `try_send`, and
receives cancellation. Split read/write halves detect client EOF while the
snapshot or tail source is pending. One supervisor-provided absolute deadline
also bounds slow writes; no engine or global lock is held across serialization
or I/O. Seven real Unix tests cover the full snapshot/event/caught-up/live-
event/disconnect sequence, bad hello and peer gating, immediate client-drop
cancellation, queue/payload bounds, slow-reader backpressure, pending-build
deadline, and identity rollover. Focused test runtime is 0.12 seconds.

This remains an offline library seam. A concrete long-lived vLLM index source,
outbound LB reconnect/challenge owner, runtime command, and shadow-only wiring
are still required before the sandbox can use real images or node06 can run it.

## 2026-08-13 — issue #41 adversarial LB consumer matrix

Nine public-API Unix-stream integration tests now cover absolute-deadline
revocation after publication; oversized snapshot and tail prefixes rejected
before body allocation/read; truncated snapshot and tail frames; abort during a
60,000-record, greater-than-8MiB private build; same-identity two-session
replacement with stale-disconnect isolation; generation rollover and republish;
authenticated vLLM-shaped live store/remove mutation; and content-safe
error/debug/reason output. No runtime bug was found.

The warm test body takes about 1.6 seconds. The intentionally large
cancellation fixture peaks around 1.1–1.3GiB RSS, so keep it in the full gate
rather than duplicating it across parallel local loops.

## 2026-08-13 — issue #41 outbound reconnect and rolling handoff

The LB now has a transport owner around the authenticated consumer. It
revalidates the normalized, trusted-parent socket path before every Unix
connect, generates 256-bit challenges from the OS random source, retries a
collision at most sixteen times, and retains a bounded 65,536-entry FIFO reuse
ledger. A process restart does not persist the ledger; 256-bit OS randomness is
the freshness guarantee across restarts. One absolute deadline spans path
validation, connect, and consumption. Failures use half-to-full jittered bounded
exponential backoff, and shutdown drops the consumer future immediately.

Normal reconnects are serial. A capacity-one explicit replacement command is
the only path that overlaps two sessions: same-identity publication remains
available while the new session catches up, then the owner observes a new
published actor epoch and drops the old future. A failed replacement retains
the old session. Seven focused tests cover connect failure/backoff/recovery,
connected-session deadline, prompt shutdown, distinct challenges across
reconnect, republish, explicit caught-up handoff, collision-ledger eviction,
and jitter bounds. Focused test runtime is 0.04 seconds.

This stays outside approximate serving and is not wired into proxy startup.
The 2ms publication poll should eventually become an actor notification, and
refreshed expected engine incarnation remains a control-plane responsibility.

## 2026-08-13 — issue #41 off-by-default companion runtime coordinator

The library runtime now composes typed companion configuration, Prometheus
collectors, hardened 32-byte secret loading, safe bind-last socket publication,
the two-client supervisor, authenticated producer, bounded shutdown drain, and
inode-safe cleanup. Off mode returns before requiring a source or touching the
filesystem. Serve-mode startup validates and constructs every fallible object
before publishing the socket; readiness clears and the exact inode is removed
after normal exit, supervisor error, or bounded shutdown.

The current hello does not carry an engine selector, so the coordinator accepts
exactly one configured source and fails closed otherwise. It also uses the
stricter of snapshot and tail-idle durations as the producer's single absolute
session deadline; separate phase semantics remain future work. The supervisor
currently erases typed handler failures, so aggregate failures map to one closed
`application` metric reason rather than an arbitrary error label.

Four focused tests cover off mode without source/filesystem state, post-bind
supervisor failure cleanup, bounded shutdown cleanup, and terminal/capacity
report mapping. The full Rust suite and strict Clippy pass. This is still not a
CLI command and changes neither ordinary LB startup nor node06.

## 2026-08-13 — issue #41 true offline stack and captured-shape gate

The public modules now run together in one offline lifecycle harness: hardened
socket bind/publication and inode guard, two-client supervisor, authenticated
producer, reconnect owner, consumer, actor, and digest index. It covers initial
publication, live store/remove, rolling replacement, LB owner restart, companion
shutdown/socket cleanup/restart, identity rollover, and leak-free teardown. Two
authenticated slow readers hold both slots while a third is rejected. Ten runs
passed 20/20; the focused body is about 0.07 seconds.

At the captured 36,612-record / 9,372,672-token shape, the snapshot is 6,040,438
bytes and authenticated response 6,040,952 bytes. Measured locally: encode
11.509ms, decode 12.543ms, repeated decode 8.750ms, wire encode 6.961ms, wire
decode 7.519ms, private rebuild 22.355ms, total process wall 0.15s, and about
58MiB RSS/HWM. This clears the sub-3s offline gate with wide margin.

## 2026-08-13 — issue #41 long-lived companion digest source

The concrete engine-neutral source owns one bounded digest index across LB
session churn. Full replay is staged privately and committed atomically;
sessions are registered under the same boundary lock before the index is cloned,
while breadth-first export and MessagePack encoding run off-lock. Live batches
reuse the already-qualified bounded vLLM payload for authenticated tails. The
source supports at most two subscribers, drops only a slow or cancelled
subscriber, and leaves the long-lived index running when an LB disconnects.
Payloads are shared `Bytes`; every session now has both an entry bound and an
aggregate queued-byte semaphore (16MiB default, 64MiB maximum), plus at most one
separately frame-bounded in-flight payload. Byte overflow and rebuild signal
out-of-band cancellation that the producer prioritizes over queued events and
socket writes. Concrete Unix tests prove both paths close within 250ms without
draining a queued stale frame, and reservation permits return on dequeue/drop.

Replay disorder, digest-index failure, transport authority loss, rebuild, or an
attested engine-incarnation change fences all sessions, clears authority, and
advances the generation. vLLM watermarks are sparse, so strictly increasing
forward jumps are valid here; the process-level replay fence must distinguish
omitted scheduler steps from lost event batches. Focused tests cover boundary
ordering, cancellation isolation, slow-reader backpressure, two-client capacity,
sparse watermarks with explicit rebuild/recovery, incarnation rollover, and a
full-engine clear.

This remains offline. The next seam is a process-level owner that constructs
`ZmqKvEventSource`, subscribes live before replay, drives complete sparse replay
and live ingestion across reconnects, and refreshes the authenticated engine
incarnation. It is not yet a CLI command, Compose source, shadow consumer, or
node06 deployment.

The remaining atomic `DigestKvIndex` clone is synchronous under the source lock.
Ten release repetitions measured a 36,612-record single clone at 7.66ms median
and two serialized starts at 22.99ms wall; 131,072 records measured 28.45ms and
82.27ms. Real authenticated one/two-session captured-shape paths measured
38.96/50.94ms median with roughly 58/83-89MiB peak RSS; maximum-shape paths took
140.94/192.67ms and roughly 196/286-312MiB. These clear the 3s recovery gate but
do not meet a sub-10ms ingestion pause target. Instrument clone duration before
shadow and prefer immutable/COW generation ownership if that p99 is required.

The runtime readiness surface now follows the exact source rather than socket
publication. A conservative trait default reports unknown/not-ready; the
concrete source exposes replay, building, ready, and fenced phases plus bounded
watermark-presence, indexed-block, and active-session gauges. Runtime polling is
25ms with missed ticks skipped. `listening` and `source_ready` remain distinct,
while the existing operational `ready` is their conjunction and returns to zero
when the listener exits even if the in-memory source remains authoritative.

## 2026-08-13 — Dynamo, Kimi-K3, and DwarfStar upstream refresh

Primary-source refresh found Dynamo v1.3.1 as the newest stable release and
`v1.4.0-kimi-k3-dev.1` as an explicitly non-QA-gated preview. Dynamo v1.3's
router direction now includes a standalone selection service, branch-sharded KV
indexing, a compressed concurrent radix tree, topology-aware routing, separated
chat/tool/reasoning parsers, and offline trace replay. Its standalone router
exposes `best_worker_id` plus `get_overlap_scores`, including device, pinned-host,
disk, and shared-cache tiers. The actionable node06 lesson is to keep exact
inventory and counterfactual overlap observable independently of request proxying;
branch sharding is not justified for two engines until measured lock/index cost
requires it.

The Kimi-K3 Dynamo preview targets aggregated and disaggregated TP8 GB300 / TP16
GB200 deployments and explicitly does not cover RTX PRO 6000. K3 itself is a
2.8T-parameter hybrid KDA/Gated-MLA model with a one-million-token context, so
this refresh does not change the existing node06 feasibility rejection. Its
applicable lessons remain model-aware cache geometry, request-class budgets,
failure-bounded affinity, and strict frontend parser/reasoning/tool parity.

DwarfStar's current native agent treats the rendered conversation and saved KV
state as one session truth, persists complete KV sessions to disk, keeps tool
syntax native to the model, and can rebuild a stripped session by prefilling its
saved rendered text. This supports the current companion/session direction but
does not make its local GGUF cache format compatible with vLLM. The next
persistent-tier experiment should therefore prove engine-rendered-token parity,
session snapshot/rebuild correctness, and output parity before measuring NVMe
recovery benefit. No node06 state changed during this refresh.

Sources inspected: [Dynamo v1.3.1 release](https://github.com/ai-dynamo/dynamo/releases/tag/v1.3.1),
[Dynamo v1.3.0 router release](https://github.com/ai-dynamo/dynamo/releases/tag/v1.3.0),
[standalone router contract](https://github.com/ai-dynamo/dynamo/blob/main/components/src/dynamo/router/README.md),
[Dynamo Kimi-K3 preview](https://github.com/ai-dynamo/dynamo/releases/tag/v1.4.0-kimi-k3-dev.1),
[Kimi-K3](https://github.com/MoonshotAI/Kimi-K3), and
[DwarfStar](https://github.com/antirez/ds4).

## 2026-08-13 — r43 companion phase and tail-idle deadlines

The temporary producer-wide absolute session deadline is split at the wire
phase boundary. One absolute snapshot timeout now includes authenticated hello,
source start/build, response authentication, and the complete snapshot write.
After the response, every dequeued tail event receives a fresh bounded write
budget and every successful write starts a fresh tail-wait budget. Healthy live
progress can continue beyond the snapshot deadline; silence and a client that
stops reading remain bounded. Client EOF, source revocation, and supervisor
shutdown are still selected ahead of timer and data work.

The companion runtime uses the supervisor's handler-managed mode. This removes
only the accidental total lifetime: admission remains capped at two clients,
tail queues remain bounded by entries and bytes, wire frames retain size caps,
and both bootstrap and each idle/write interval have finite validated budgets.
Tokio paused-time tests cover slow snapshot expiry, tail-idle expiry, and two
progress events spanning six snapshot budgets. Real Unix tests cover immediate
client disconnect, a blocked multi-megabyte tail write, byte-overflow
revocation, and shutdown cancellation. This is library-only; no node06 or
Compose state changed.

The integrated local gate passed 274 library tests plus 31 integration tests
and all doc-test targets in 25.56s, strict all-target/all-feature Clippy in 4.67s, Go
test/vet/format parity in 1.17s, and 52 Python protocol tests plus corpus
validation in 0.22s. The first release build took 67.03s because this isolated
disk-backed worktree had a cold release target; after keeping Tokio's paused-
time support dev-only, the narrower production-feature rebuild took 33.97s.
The focused warm producer suite took 5.9s including its crate rebuild.

## 2026-08-13 — r43 process-level per-engine KV owner

The library-only owner now constructs a fresh subscribed transport per engine,
uses the first live batch as the authoritative replay watermark, streams sparse
full replay directly into generation-guarded private digest state, and then
applies buffered/new live batches. Transport disconnect, replay/apply failure,
task abort, explicit authority loss, and an attested incarnation change fence
the source and every snapshot session before retry. Reconnect uses bounded
backoff; repeated connection failures do not churn an already-fenced source.
Observer events expose only closed reasons and bounded replay profiles.

Independent integration review rejected two initially green behaviors. First,
replaying only through the last pre-disconnect watermark could miss events
published before the new SUB socket existed and falsely restore authority. A
silent reconnect now remains fenced until a fresh live watermark arrives.
Second, incremental replay returned a `Vec<SequencedBatch>` whose theoretical
retention was `replay_limit * max_payload_bytes`. Until the source has a
transactional bounded gap stage, every detected gap now fences immediately and
streams a full private replay through the triggering live watermark. Existing
SUB delivery remains installed, while the replay ROUTER's bounded post-watermark
tail is folded into the same private generation before publication; later SUB
delivery deduplicates against that final watermark. This closes the interval in
which libzmq is draining and the async SUB receiver is not polled. A third guard
makes an already-created clean replay stage idempotent so a source apply failure
and its owner retry spend only one companion generation.

Focused tests use both injected transports and real TCP ZMQ. They cover sparse
startup/full replay, a live gap/full rebuild, live delivery buffered during
replay, disconnect remaining fenced without a new watermark, reconnect replay,
authority loss/refresh, stalled replay cancellation, task-abort fencing,
stale blocking-worker generation rejection, and connection-failure generation
stability. The full integrated gate passed 284 library tests plus 31
integration/E2E tests, strict Clippy, Go parity, 52 Python tests, the five-case
agent corpus, and a 25.72s warm release build. This remains off-path: there is
no CLI, Compose, runtime, or node06 change yet.

## 2026-08-13 — Infernal r4 C128A source-lock preflight

The immutable Infernal r4 vLLM source was reconstructed GPU-free from base
`ce5f50f6d01b02336c4207f11277fd7bedacb4d6` and its locked integration patch.
The computed tree exactly matched the public receipt's
`3226eb7ff642702908f502a2402f9d083d16511c`. The relevant source blobs are now
pinned by `bench/infernal_c128a_preflight.py`, together with the r4 image,
Docker recipe, integration-patch, and vLLM #51318 head identities. Reports are
limited to public hashes, booleans, and closed reason codes; source, paths, and
request data are never emitted.

The upstream #51318 patch does not apply verbatim because r4 PR 289 refactored
the same batch-dependent expression into `get_c128a_active_topk_width`. Its
semantic port is nevertheless one production assignment: use the already
preallocated `self.c128a_max_compressed` row capacity and pass that same value
to the build kernel. Against the reconstructed source, baseline mode accepted
the exact r4 blobs, candidate mode rejected r4 with
`layout_batch_dependent`, and candidate mode accepted the semantic port. The
gate also proves that CUDA-graph capture selects `max_model_len` and that the
capacity is allocated from `max_model_len`, so it fails closed if either side
of the invariant moves. Nine focused unit tests passed in 0.004s. No image was
built or pushed and node06 was not touched. A packaged successor still needs
the retained parser fixtures before any GPU smoke.
## 2026-08-13 — Infernal r4 parser source reconstruction and GPU-free gate

The Infernal r4 receipt was reconstructed without touching node06. The exact
composition is Docker source `0040f0af0670d0e5bb0f6bea6ee7cd2de2990b01`,
vLLM base `ce5f50f6d01b02336c4207f11277fd7bedacb4d6`, integration-patch SHA256
`dec8963846acbd52dd76500900286fa596da83cafbe1abbc55a8b190e16b8279`, and
result tree `3226eb7ff642702908f502a2402f9d083d16511c`. Applying the immutable
release patch to that base and running `git write-tree` reproduced the receipt's
result tree exactly. The probe's deterministic V4 candidate-surface identity
for r4 is
`sha256:149a52d9d606899a47adb55f460de22ab06eee1d00e4c1e68e5065efeab3ade3`;
it includes every V4 runtime file modified by #49117, including the shared
parser engine and tool-property helper even though the lightweight streaming
fixture stubs their heavyweight dependencies.
The image tested in issue #32 was
`voipmonitor/vllm:infernal-invocation-vllm3226eb7-b12x1584743-fi1ac6942-cu133-torch213-20260812-r4`
at registry digest
`sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6`.

Upstream vLLM PR #49117 head
`7ef0ae2480799e95fb7cb801a8105c1db2585164` was compared from its base
`34bb795ff3efee6cc08c9dd104deceefff2c4d55`. Its DeepSeek V4-only delta
applies cleanly to four source files and `deepseek_v4.py`. There is one small
source conflict in `streaming_parser_engine.py`: retain both r4's projected
skip-state cleanup and #49117's recovery-hold abort. The V4 test file conflicts
only because r4's duplicate-closer regression and #49117's new recovery suite
were inserted at the same location; retain both. R4's duplicate-closer
transition itself is orthogonal and applied cleanly.

A new stdlib-only probe imports the actual composed `deepseek_v4_config` and
`StreamingParserEngine`, without torch, a vLLM environment, or GPUs. Seven
synthetic cases cover wrapped and orphan parallel calls, split markers,
undeclared tools, `tool_choice=none`, and the malformed `toolcalls` opener from
vLLM #51914. The immutable r4 source matched its profile in 0.04 seconds:
orphan parallel output produced zero calls and leaked DSML. R4 plus #49117 also
matched its profile in 0.04 seconds: both orphan calls were recovered with no
content leak, including split markers, while undeclared and suppressed calls
remained content.

#49117 is necessary but not sufficient for #51914. Given
`<｜DSML｜toolcalls><｜DSML｜invoke ...>`, it recovers the valid declared invokes
but emits the malformed opener as content, so the agent correctness gate still
fails. The minimal adjacent patch should hold that exact malformed opener and
suppress it only after a following orphan invoke validates against a tool
declared by the request; otherwise it must replay the prefix and invoke
byte-for-byte as content. Two literal terminals combining the malformed opener
and following invoke prefix (inline and LF-separated) can reuse #49117's hold
without changing the engine: that three-constant/two-transition prototype made
all seven `complete` cases pass in 0.05 seconds, including split chunks. Its
V4 candidate-surface identity is
`sha256:29c4307be05c78bfde1fbc043cc9189de66b85817a19e33b17eb76104319bbef`.
The
committed `complete` fixture profile encodes this remaining gate.

Issue #48089 should not receive a parser workaround. Its concurrency-dependent,
no-tools, and non-streaming corruption is now attributed to the C128A FULL-graph
row-stride defect tracked by vLLM #51318. A parser can fail closed on corrupt
markup but cannot repair arbitrary KV/graph output. The exact issue #32 live
response cannot be replayed byte-for-byte because the privacy-bounded journal
correctly retained only structural outcomes; the synthetic fixtures reproduce
the parser shapes, not production content.

## 2026-08-13 — r44 stable observe-only owner for old engine history

The process owner now treats history older than its bounded replay window as a
stable fenced state on the already-installed live subscription. This matters
for node06 engine A: a newly started companion can encounter a current sequence
beyond its 8,192-event recovery budget. Retrying the transport for every new
event cannot recover missing history and would churn connections, companion
generations, and logs while remaining unable to route exactly.

On startup, over-limit events now advance only the private fence's observation
watermark. After an over-limit live gap, the source and all sessions are fenced
once, then the same behavior applies. Neither case requests replay, applies
partial index state, reconnects, or reports ready. A genuine
`AllBlocksCleared` event on that subscription resets and publishes an empty
authoritative boundary; an attested incarnation change still takes the normal
fresh-authority path. The replay-too-large rebuild reason remains visible as a
closed, content-free observer value.

Two async lifecycle tests cover startup at sequence 10 with a replay limit of
2 and a ready source receiving a gap from 0 to 4. In each case another ordinary
event causes zero reconnects, zero replay requests, and zero generation churn;
the following clear restores readiness on the original connection. The full
library suite passed 286/286 tests in 0.68 seconds. This is still library-only;
node06, images, and production Compose were not changed.

## 2026-08-13 — dual per-engine companion Compose/security harness

The standalone offline companion contract now follows the runtime's deliberate
one-source limit with one process and authority domain per engine. The explicit
profile renders four fixture-only services: companion/client A and
companion/client B. A normal render still selects zero. The two domains use
different companion UIDs (12001/12003), host-tmpfs runtime directories, Unix
socket paths, root-owned 32-byte session-secret inodes, fixture subpaths,
healthchecks, and engine labels. The future LB fixtures retain UID 12002 and
only the shared numeric GID 12000 crosses the domain boundary. Synthetic fixture
directories are per-engine and read-only.

The semantic validator rejects networking or ports, host IPC/PID namespaces,
GPU/device access, privileged mode/capabilities, Docker socket or broad host
mounts, writable client roots, implicit host-path creation, cross-engine
runtime/secret visibility, peer socket names in commands or healthchecks, and
service dependencies that would couple readiness. Its authority projection is
keyed to each validated companion/client/socket tuple: with A failed and B
healthy, only B remains authoritative; the reverse holds independently. It
cannot substitute one engine's state through the other socket.

Local Docker Compose 5.0.0 rendered the explicit profile and passed the
validator; the default `config --services` output contained zero lines. Six
GPU-free mutation tests passed in under 1ms. A real disposable `/run` tmpfs
preflight passed exact UID/GID/mode/link/size/inode checks for both domains, and
an intentionally aliased runtime directory failed closed before metadata
validation. The disposable files and directories were removed afterward. This
changes no production Compose, executable, image, or node06 state; reserved
`.invalid` images keep the harness non-startable by default.

## 2026-08-13 — source-locked Infernal r5 correctness overlay

A build-only successor overlay was prepared from the exact rejected r4 image
digest and source tree without building an image, contacting node06, or changing
the canonical Compose deployment. The full-index patch changes exactly six
allowlisted Python files: the DeepSeek V4 sparse-MLA metadata builder, its V4
parser, and four shared parser-engine/helper files required by the V4-only port.
It contains the semantic r4 adaptation of vLLM #51318, the V4 runtime subset of
#49117, and the conservative inline/LF malformed-wrapper extension for #51914;
it does not contain the V3.2 parser delta.

The resulting vLLM tree is
`0eb3d442a49b78d194903d37fbff6dd86140e420`, the overlay patch SHA256 is
`603673eb721df372cd5807097deea7d1605d100ee3fa88c87a52c45c002cf553`,
and the complete parser-source identity is
`sha256:29c4307be05c78bfde1fbc043cc9189de66b85817a19e33b17eb76104319bbef`.
The image recipe rejects any base whose staged source is not the exact r4 tree,
checks the patch and resulting tree, proves imports resolve from that staged
checkout, and moves every inherited JIT/autotune cache to revision-specific
fingerprint
`cu133-torch213-vllm0eb3d442a4-b12x1584743fd9-lmcacheccccdfc37f`.

The end-to-end source gate took 1.15s and passed the baseline and candidate
C128A invariants, all seven parser cases, and syntax compilation of all six
changed sources. The normal wrapped-parallel fixture has identical r4 and
complete-profile expectations. Six focused overlay tests took 0.032s; the
canonical+overlay Compose render also passed and retained the base mounts while
replacing only the candidate image and revision-specific cache targets.
Verdict: the immutable source/build input is ready for an explicitly authorized
local image build. It remains unqualified for node06 until a built digest passes
the existing runtime source, corruption smoke, deterministic agent, and c8 scout
boundaries.

## 2026-08-13 — Infernal r5 embedded-index build preflight fix

The first real thin-overlay build on node06 failed closed in 7.14 seconds
before producing an image or allocating a GPU. The r4 source checkout's six
candidate blobs exactly matched its staged index, but Docker layer extraction
left their Git stat-cache entries stale. `git diff-files` therefore reported
them modified with an unknown worktree object, and `git apply --index` refused
the otherwise valid full-index patch with `does not match index`.

The recipe now refreshes the index stat cache for exactly the six allowlisted
paths after verifying the base tree and patch digest, then retains the stronger
`apply --index` behavior and exact candidate-tree check. A disposable container
against the immutable r4 digest proved all six refreshes, applications, and the
resulting `0eb3d442...` tree. The focused contract test locks the refresh list to
the manifest allowlist. No production container, Compose service, or GPU state
changed during the rejected build.

After the refresh fix, the same node06 build completed in 16.25 seconds and
produced the local immutable image ID
`sha256:1f3a54246d5ecdb1bae53360b89881b629921b557c44e739c0034b373fa21d26`.
Its labels report base digest `21f048...`, candidate tree `0eb3d442...`, parser
identity `29c4307b...`, patch `603673eb...`, and the expected revision-specific
cache fingerprint. A separate network-isolated, no-GPU container rechecked the
Git tree and imported `vllm.parser.deepseek_v4` from the staged source path.
Root remained at 58GB free / 85% used, and the LB plus both r34 engines retained
their prior uptime. The image is local-only and still must pass the corruption
smoke and deterministic agent gate before B may serve a measured request.

## 2026-08-13 — Infernal r5 direct-engine correctness rejection

Production was first single-homed on unchanged r34 A and verified 1/1 healthy.
The exact local candidate image
`sha256:1f3a54246d5ecdb1bae53360b89881b629921b557c44e739c0034b373fa21d26`
then replaced only B. Its effective argv matched the prior r4 canary contract:
A16/K5 probabilistic-standard, TP4, graph96, MNS16, MBT4096, 393,216 context,
GMU0.975, and the native KV publisher. B reached API-ready in 12m32s with zero
restarts, OOM, CUDA, NCCL, Xid, traceback, or fatal markers during startup.
Startup populated only the new `vllm0eb3d442a4` JIT namespace.

The fail-fast direct candidate gate stopped in 7.70s after its first stage. Five
deterministic agent cases completed, but only three were protocol-valid:

| case | result | structural failure |
|---|---|---|
| text non-stream | pass | — |
| typed required stream | reject | two calls instead of one; one argument was invalid JSON |
| parallel required stream | reject | three calls instead of two; `engine` was duplicated |
| auto-tool DSML stream | pass | — |
| reasoning/tool history | pass | — |

The same bounded log interval also contained a late JIT marker, so performance
would have required a warm rerun even if correctness passed. Correctness is the
independent hard stop: the C128A and synthetic parser fixes do not make this
model/runtime composition agent-safe. No c8 scout, six-cell matrix, LB route,
or Helix workflow was attempted. B was immediately recreated from canonical
r34 while production remained on A; the local candidate image and privacy-safe
gate journal were retained for diagnosis. The next Infernal successor must add
goldens for real model-emitted extra calls and malformed arguments rather than
only wrapper/orphan parser fixtures.

The rollback B became API-ready on canonical r34 without a restart. Direct
`/health` and `/v1/models` probes returned 200, and a deterministic 32-token
smoke stopped normally with reconciled usage (88 prompt + 24 completion = 112
total tokens). Its rollback interval contained zero CUDA, NCCL, OOM, Xid,
traceback, or fatal markers. The load balancer was then recreated from the
canonical dual-upstream Compose: `/health` reported 2/2 healthy, both
`ds4proxy_upstream_up` gauges were one, and both A and B resolved to r34 image
ID `sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b`
with zero restarts. The candidate remained unserved throughout.

## 2026-08-13 — immutable upstream audit after Infernal r5 rejection

A read-only public refresh at `2026-08-13T07:04:44Z` found no newer Infernal
candidate. Docker Hub still exposed only r2, r3, and r4; r4 remained
`sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6`.
The [DS4 runbook at `999653a8`](https://github.com/local-inference-lab/rtx6kpro/blob/999653a839121bca58ef46ebdf9743fb3a9284f8/models/deepseek-v4-flash.md)
still selected r4, the [container repository at `1429cb30`](https://github.com/local-inference-lab/blackwell-llm-docker/commit/1429cb3010be38e68bcaa069322bc0a587db452f)
still ended at its r4 qualification receipt, and the Infernal vLLM branch was
unchanged at [`ce5f50f6`](https://github.com/local-inference-lab/vllm/commit/ce5f50f6d01b02336c4207f11277fd7bedacb4d6).

The two relevant upstream fixes also remained unmerged. vLLM
[#49117](https://github.com/vllm-project/vllm/pull/49117) was open/dirty at
`7ef0ae2480799e95fb7cb801a8105c1db2585164` (audited patch SHA-256
`5a56dfd4cf8d12a237a39df2993522ad9f5f2cd65603c1a0340a0eac7d585907`), and
[#51318](https://github.com/vllm-project/vllm/pull/51318) was open/blocked at
`b5a04d25e8e9f3b01a26b57ea6644b71ce44c414` (patch SHA-256
`ae80086e6de06712524c3f7b5060c958623e3fd7bbde9bbdd1d34594b5d2795a`).
Rejected r5 had already ported both changes. Issue
[#51914](https://github.com/vllm-project/vllm/issues/51914) still had no public
fix, and official vLLM main at [`373592ef`](https://github.com/vllm-project/vllm/commit/373592ef57d4a19b057237fb015b0bd1382daa03)
still lacked both the required DSML hardening and a capture-stable C128A row
stride.

Track, but do not roll, vLLM
[#51538](https://github.com/vllm-project/vllm/pull/51538): its audited head was
`e468291b196a9d6f6c8700c28f95916e02484dea` (patch SHA-256
`706a371ad72aebbc00428302722d684af60929f33a2c1e1dcc82da3b18c6db81`),
open/blocked with CI incomplete. Its RTX PRO 6000 sparse-MLA/DSpark hardening is
valuable, but r4 already contains the workspace, sample-mapping, output-lifetime,
and null-block protections relevant to this stack. The remaining changes harden
FlashInfer, padded MTP, and top-k hangs; they change neither DSML parsing nor the
C128A stride and do not explain r5's sequential extra calls and invalid JSON.

Verdict: **no-go** for another image or node06 experiment from current public
upstream. Reopen only for an immutable r5+ artifact or a narrow source-locked
patch that first passes the retained real-output and synthetic parser goldens.

## 2026-08-13 — direct-P2P tool build and read-only node06 preflight

The Phase-B prerequisite tooling was exercised through its zero-serving-impact
boundary. Its first exact-r34 cold build failed closed after the 181.9-second
base extraction plus compile because the Python NCCL package exposes only
versioned `libnccl.so.2`, while `nccl-tests` expects an unversioned development
link. A build-only link to that exact packaged library fixed the contract; the
warm rebuild completed in 26.8 seconds and exported 10.9MiB. The immutable
receipt is:

- manifest SHA-256 `ede7b69919c1346aae01c718a8c09e8546b684c6d92c08a7c0406933b70878e0`;
- `nvbandwidth` SHA-256 `ba0b486efb1a83ceb535243730fe51fb7104a21a7e3f90dd590b4bed465fb023`;
- `all_reduce_perf` SHA-256 `17f2cca118295e4d16d1a1adbe62c6e69b03d188de68f14fc3fa8427b2c74257`.

Two read-only attempts then exposed and locked node06 CLI compatibility: Docker
accepts `top -eo pid` but rejects the empty-header `pid=` form, and NVIDIA 595's
topology header contains a non-adjacency `GPU NUMA ID` column. Both attempts
failed before any LB or GPU action. The corrected preflight completed in 4.0
seconds and verified exact r34/driver/process ownership, the NUMA-1 reservation
on GPUs 4–7, directed `NODE` topology, directed P2P read/write support, and
1,830MiB free on each target GPU.

LB and engine container IDs, start times, image IDs, and zero restart counts
were identical before and after; mini-dynamo remained 2/2 healthy with both
`ds4proxy_upstream_up` gauges at one. Tools are staged root-owned and
non-writable under `/tmp` on node06, but no tool container, CUDA context, LB
recreate, or benchmark ran. The next gate remains the separately acknowledged
1MiB/two-GPU scout during a low-traffic window.

## 2026-08-13 — r46 standalone single-engine snapshot companion

The previously separate owner, source, producer, supervisor, and metrics pieces
now compose into the off-by-default `mini-dynamo-snapshot-companion` executable.
Serve mode accepts exactly one live/replay endpoint pair and one group geometry.
Before binding metrics, connecting ZMQ, or publishing its Unix socket, it
hardens and loads three distinct protected files: the session secret, digest
secret, and an HMAC-authenticated engine-incarnation envelope. The refresh
watch removes authority immediately on any filesystem, schema, field, or MAC
failure and restores it only after a valid changed envelope. Owner telemetry is
closed-label and content-free, including the stable `replay_too_large`
observe-only state required by long-lived node06 engine A.

One absolute shutdown deadline covers the owner, snapshot runtime, metrics
server, and attestation watcher; tasks that miss it are aborted. The ordinary
LB Dockerfile now names only `--bin mini-dynamo`, while the companion has a
dedicated `Dockerfile.companion`. With one shared BuildKit target cache, the
measured source builds were 42.3s for the LB followed by 9.7s for the companion;
no-op warm builds were 2.16s and 3.38s. Final images were 14.27MB and 11.50MB.
This preserves the fast LB-only loop instead of paying the observed extra
roughly 41-second companion relink for every router image.

Review then closed three rollout defects before merge: serve mode now rejects
any client cap other than exactly two before side effects; root-container tests
use explicit non-root service identities without weakening production UID
validation; and the binary implements a passive `healthcheck <socket>` that
checks parent/socket ownership, type, mode 0660, and link count without consuming
an authenticated client slot. The misleading container `EXPOSE 9091` metadata
was removed because a loopback listener in a bridge container is not externally
scrapable. Production metrics need a separate permission-isolated UDS and must
not share the snapshot/session authority group.

The final gate passed 299 library tests plus 35 integration/E2E tests (334
total), strict Clippy, formatting, release builds, off-mode executable/container
smoke, and the dual-domain offline Compose validator. Drone #207 completed this
gate in 59 seconds. No production Compose, image push, or node06 state changed.
The executable is intentionally not deployment-ready yet: a bounded host
provisioner must derive and atomically write the authenticated incarnation from
current engine metadata before either per-engine service can be enabled.

## 2026-08-13 — issue #41 permission-isolated companion metrics UDS

The standalone companion metrics server now has an explicit endpoint type:
loopback TCP remains available for local use, while
`DS4_SNAPSHOT_METRICS_SOCKET_PATH` selects a Unix listener only when a dedicated
non-root `DS4_SNAPSHOT_METRICS_GROUP_GID` is also supplied. TCP and UDS settings
are mutually exclusive. UDS configuration rejects non-normalized/oversized
paths and any parent shared with the snapshot socket before filesystem work.

Runtime validation goes further than path inequality. Both parents must be
symlink-free, companion-owned, and non-writable by group or other. The metrics
parent and snapshot parent must both be setgid and group-traversable, its
configured group must differ from the snapshot/session parent's actual group,
and the published mode-0660 metrics socket must inherit that group. Publication
reuses the hardened unique-private
bind plus hard-link no-replace protocol; shutdown removes only the same device
and inode, so a replacement pathname is never unlinked. Protected session,
digest, and attestation inputs still validate before either metrics endpoint is
bound. Endpoint debug and typed errors expose no path, address, or group value.

Focused tests cover TCP/UDS ambiguity, group requirements, parent separation,
live HTTP metrics over UDS, inherited mode/group, graceful cleanup, rejection of
the snapshot authority group, and preservation of a pre-existing target. The
first shared-target compile plus focused config test took 26.36s after the
module changed; the immediately warm two-test UDS loop took 0.82s. Because a
parallel branch legitimately owned the canonical cache, the widened gate used
an isolated disk-backed target: strict Clippy was green after a 53.96s cold
dependency build and 6.70s incremental rerun, all 336 Rust tests passed in
61.00s, and the cold thin-LTO release build took 115.21s. In parallel, the Go
gate took 1.12s, 101 Python tests took 0.68s, and offline Compose validation took
0.42s. This is a concurrency tradeoff, not a new normal inner-loop baseline;
the final warmed focused test and strict Clippy took 8.81s and 3.48s. The final
all-test rerun took 7.23s and the source-change thin-LTO relink took 34.63s in
that isolated cache. No Compose, Caddy, node06 process, container, engine, or
production route changed. The next deployment gate is to extend the offline
dual-domain fixture and validator with
per-engine metrics-only groups and scraper paths.

## 2026-08-13 — r52 offline host engine-attestation provisioner

Issue #41's remaining host writer is now implemented as the separate one-shot
`mini-dynamo-attestation-provisioner`. It accepts no arguments and receives only
three protected paths, numeric output ownership, and a bounded capture age from
the environment. Docker inspection stays in the independently privileged
`node06_engine_metadata.sh` capture step; the helper now records the actual
vLLM process start from `/proc/<pid>/stat` and host boot time rather than using
the earlier, weaker container start as the authoritative process epoch. The
provisioner has no Docker or network capability and rejects schema changes,
unknown identity fields, missing verification state, stale/future/pre-process
captures, malformed digests, and unqualified supplied receipts.

The derived incarnation hashes a canonical allow-listed immutable evidence set
while intentionally omitting capture time and normalizing repository-digest
order. Publication holds an exclusive lock on the trusted output parent, checks
the preexisting inode, writes a random `create_new` file in the same directory,
fsyncs content, assigns exact owner/group/mode `0440`, fsyncs again, atomically
renames, fsyncs the directory, and performs an authenticated read-back. An
authenticated same identity is a no-write success; an older process, different
evidence for the same process, corrupt existing envelope, unsafe output inode,
or competing publisher fails closed without replacement. CLI errors contain
only bounded reason labels; success emits nothing.

Focused tests covered create/idempotence, stable capture refresh, valid process
replacement, rollback, same-process conflict, stale/future/pre-start inputs,
receipt authority, unsafe metadata permissions, symlink output, corrupt output,
unknown fields, parent-lock contention, redacted config, no-argument CLI, and a
real subprocess round trip. Final local gates passed 309 Rust unit tests plus 38
integration/E2E tests (347 total), strict all-target/all-feature Clippy,
formatting, release build, Go parity/vet/format, 101 Python tests and agent
validation, shell syntax, and the offline dual-domain Compose validator. Warm
timings were 0.28s format, 3.70s Clippy, 7.45s Rust tests, 34.49s all-bin release,
0.89s Go, 0.50s Python, and 0.18s Compose; the first isolated provisioner release
build was 69.75s and produced a 561,736-byte binary. No node06, Compose, CI,
container, image, secret, or production state changed. Per-engine service
manager/Compose capture wiring remains the next deployment boundary.

## 2026-08-13 — Drone-only quality and release-cache iteration gate

All CI tests moved from GitHub Actions into one Drone pipeline. Cargo fetches
once, then strict Clippy and the full Rust suite use independent target
directories in parallel while Go parity, the Python/agent corpus, and canonical
plus offline Compose validation run alongside them. The first complete PR run,
Drone #205, took 58 seconds versus 69 seconds for the previous successful serial
Rust lane. The merged companion surface remained in the 59-second class on #207.

The image publisher exposed a separate cold-build problem: the legacy DinD
daemon is ephemeral, so `purge: false` did not retain its BuildKit cache (#208
still spent 100 seconds publishing). A max-mode registry exporter was rejected
after #210 proved the new Buildx plugin was not privileged on this runner; it
failed before Dockerfile or GHCR work, and no broader runner trust was granted.
The safe replacement keeps the allowed publisher, makes the Cargo dependency
build an ordinary Docker layer, embeds inline BuildKit cache metadata in the
private `rust-edge` image, and imports that image in the next fresh daemon.
Drone #212 successfully seeded the cache in 113 seconds. Documentation-only
main build #214 then imported it in a fresh DinD, published in 14 seconds, and
completed the entire pipeline in 72 seconds. Relative to #208's 100-second
publisher and 171-second pipeline, that is an 86% publisher reduction and 58%
end-to-end reduction for a non-Rust change. The measured PR quality loop remains
58–66 seconds. Retain this design and treat a no-Rust publisher materially above
14 seconds as a cache-regression investigation, not normal variance.

## 2026-08-13 — r54 GPU-free correctness, eviction, and image iteration gates

Three independent pre-deployment gates were integrated without touching
node06. The agent response harness now has source-locked forced-choice JSON
fallback fixtures for streaming and non-streaming output and an `n=2` case
that proves tool-call assembly is isolated per choice. The complete Python
suite passed 105 tests. This validates the northbound response shape and local
assembly contract; it deliberately does not claim to execute a candidate
vLLM parser.

The captured companion eviction shape was replayed for 20 iterations: 3,840
apply calls, 2,442 removals per shape (882 selected main-attention blocks and
1,560 filtered non-main blocks), and the exact expected final inventory of
2,574 blocks. Measured apply latency was 0.38us p50, 0.95us p95, 1.55us p99,
and 45.45us maximum. This is a conservative no-subscriber upper bound on the
source critical section and clears the 10ms optimization trigger by more than
two orders of magnitude; immutable/COW index generations are not justified by
the current profile.

The companion image now carries both the default snapshot-companion entrypoint
and the separately invoked attestation provisioner, so deployment does not
need a third publisher. A cold local Docker build after adding the `chrono`
dependency spent 58.25s on dependencies and 33.38s on the source/thin-LTO
relink; an unchanged rebuild completed in 2.3s with every build layer cached.
Container smoke proved default off mode exits successfully, while the
provisioner fails closed with content-free `missing_setting` and
`invalid_arguments` reasons. The integrated local gate passed 311 library
tests plus 38 integration tests and strict all-target/all-feature Clippy.

Drone's rolling edge tags are now path-gated to actual image inputs. Build
#223 showed why: a docs-only publish completed quickly but replaced reusable
intermediate metadata, so the next Rust build paid a 57s dependency rebuild.
Markdown, benchmark, and deployment-only changes now finish after the quality
gate and cannot rewrite either edge tag. The next source-changing main build
is the registry-cache seed/acceptance measurement for this combined image.

Drone PR #224 passed the integrated quality gate in 59 seconds: fetch 8s,
Clippy 32s, full Rust tests 48s, with Go, Python, and Compose running in
parallel. Main #225 then caught a timing-test flake before either publisher:
socket setup and executor scheduling share the replay's absolute 200ms budget,
so receive-only telemetry cannot deterministically retain a 150ms lower bound.
The end-to-end deadline contract was unchanged. After replacing that scheduler-
sensitive assertion with positive, internally consistent telemetry checks, 50
focused repetitions, the full suite, and Drone #226 passed. Main #227 passed in
186 seconds and published `rust-e20a928` plus `companion-rust-e20a928`; quality
remained 58 seconds and the two 125–128s one-time cache-seed publishers ran in
parallel. The companion digest is
`sha256:b0d403ad6fe294c071662dab42af2c32310fce8ffefa6418f3cef6997ca9a89f`.

## 2026-08-13 — r56 off-by-default LB snapshot shadow join

The LB now has a representation-independent exact-inventory boundary: existing
direct consumers retain raw-token indexes, while snapshot companions publish
compact digest indexes behind the same revision-stable lookup contract. Typed
configuration requires one socket, companion UID, session secret, digest
secret, authenticated incarnation, and selected KV group per upstream. Every
protected authority and socket parent is validated before reconnect owners are
spawned, and direct raw-event and snapshot authority cannot be enabled together.

Snapshot mode is deliberately limited to `DS4_EXACT_ROUTE_MODE=shadow`.
Configuration rejects placement and the scorer independently forces compact
inventories back to shadow, so they feed only the exact
counterfactual scorer, and approximate routing plus `/health` remain backed by
their existing independent state. Each owner uses a bounded absolute attempt,
fresh challenge, retry backoff, immediate shutdown signal, and two-second join
bound. Missing, stale, changed, or malformed authority leaves that inventory
unpublished and the scorer fails closed. Engine-incarnation rotation currently
requires a stateless LB restart because the expected attestation is pinned at
startup.

On current main, the integrated local gate passed 320 library tests plus 38
integration/E2E tests (358 total), formatting, and strict all-target/all-feature
Clippy. No Compose, Caddy, node06 process, container, image, secret, route, or
GPU state changed. The next boundary is fixed-cardinality LB reconnect/readiness
metrics and production-shaped dual-domain Compose/Caddy validation; shadow
deployment remains gated behind those pieces.

## 2026-08-13 — r57 Rust cutover cleanup and server-neutral deploy layout

The Rust implementation is now the sole source of truth. The obsolete Go
module and 12 implementation/test files were removed (14 files, 2,528 lines),
while frozen legacy fingerprint vectors remain covered by Rust tests. Drone no
longer pulls a Go toolchain or spends runner CPU on the duplicate parity lane;
strict Rust lint/tests, Python protocol tests, and Compose validation remain the
quality gate. Historical Go/Rust benchmark evidence in this journal is retained
unchanged.

The canonical serving bundle moved from `deploy/node06/dspark_0731` to the
server-neutral `deploy/dspark_0731` path for eventual open-source publication.
Drone, the candidate-overlay tooling, documentation, validation commands, and
the infra mirror helper now resolve the new location. This is a repository-only
move: the existing node06 runtime and infra mirror paths are unchanged, and no
container, process, route, secret, or GPU state changed.

## 2026-08-13 — r58 LB snapshot reconnect/readiness telemetry

The LB-side companion owners now export an operationally complete but bounded
metric surface. `ds4proxy_snapshot_route_enabled` is always present, and every
configured ordinal `engine-N` pre-creates readiness, active-attempt, active-
connection, three attempt-kind, and six terminal-outcome series before a task
starts. Labels are selected only from closed Rust enums and configuration
ordinals; URLs, hosts, socket paths, peer identities, keys, protocol content,
and free-form errors cannot enter the registry.

Readiness is intentionally stricter than connectivity. The gauge becomes one
only after the publication actor exposes an authoritative caught-up generation,
and returns to zero synchronously when its owning future fences or is dropped.
An attempt-local drop guard balances attempt and connection gauges across
normal failure, timeout, explicit rolling overlap, and shutdown cancellation.
The existing 2ms publication poll emits a metric update only on an epoch/
readiness transition, so the new telemetry adds no steady per-tick Prometheus
mutation. Approximate routing and `/health` remain independent, and snapshot
placement remains prohibited.

Focused tests exercise a connected timeout, publication-after-connection,
synchronous shutdown fencing, balanced gauges, zero-series initialization in
off mode, exact series cardinality, and absence of path/URL label material. The
final local gate passed formatting, strict all-target/all-feature Clippy, 323
library tests plus 38 integration/E2E tests (361 total), the release build,
agent validation, and 105 Python tests. The first focused
source compile/test was 11.14s; warm metric and route tests were 0.53s and
0.20s, `cargo check` was 3.55s, final Clippy was 3.89s, the full Rust gate was
16.01s, Python was 1.52s, and an unchanged release verification
was 0.13s. After rebasing onto the Rust-only cutover, strict Clippy was 3.99s,
all tests were 14.59s, Python was 0.59s, and the source release relink was
52.30s while another isolated Rust worktree was compiling concurrently. That
contended relink is recorded, not accepted as the warm-loop baseline. No
node06, container, engine, Compose, Caddy, secret, route, or GPU state changed.
Hot LB attestation refresh, production dual-domain Compose/Caddy wiring, and
100,000 revision-stable shadow comparisons remain deployment gates.

## 2026-08-13 — r59 LB hot engine-attestation rotation

The off-by-default snapshot route no longer pins its expected engine
incarnation for the lifetime of the stateless LB. Startup still preflights all
session secrets, digest secrets, attestation envelopes, socket parents, and
upstream cardinality before spawning anything. Each per-engine watcher then
reloads the attestation at the bounded
`DS4_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS` interval using the same symlink,
owner, permission, link-count, inode-stability, size, schema, field, and HMAC
checks as startup.

The LB channel carries a monotonic, redacted authority revision as well as the
optional incarnation. This closes the value-watch race where a rapid
`valid -> invalid -> same valid` sequence could otherwise coalesce back to the
same value: any new revision revokes the published actor state before the
active consumer is dropped. Missing, malformed, unsafe, unauthenticated, or
closed authority suppresses new exact-session attempts. A later valid envelope
recovers with a fresh non-reused challenge and the newly captured expected
identity. An identical refresh at the same revision does not reconnect.
Authority loss affects only the compact exact-shadow inventory; approximate
routing, upstream health, `/health`, and request serving remain independent.

Focused filesystem tests covered unchanged atomic replacement, valid atomic
identity rotation, unsafe mode, malformed content, loss, and recovery. Real
Unix-stream reconnect tests covered same-identity no-churn, immediate stale
publication/session fencing, new-identity recovery, no attempts while authority
is absent, fresh challenge use, and a deliberately coalesced loss/recovery
revision. The warm focused check/test loop was 3.43s/11.53s for the shared
module rebuild and 0.04s for each attestation and reconnect test set. This was
a GPU-free control-plane change. After rebasing onto r58's fixed-cardinality
readiness metrics, the widened gate passed strict Clippy in 6.87s, 329 library
plus 38 integration tests in 28.17s, and 105 Python tests plus agent validation
in 1.04s; the final release relink took 43.58s. The isolated release profile
had earlier paid a one-time 102.56s cold dependency build and then completed an
immediate no-op rebuild in 1.72s. No Compose, Caddy, image, node06 process,
container, route, engine, secret, or production state changed. Production
shadow rollout remains gated on the dual-domain Compose/Caddy wiring.

## 2026-08-13 — r60 explicit content-keyed Drone dependency image

Main build #233 exposed that the rolling inline-cache scheme was not durable on
the publisher's Docker 20.10 daemon. A `.drone.yml`-only change still matched
both publisher path guards; each imported its edge image but rebuilt the Cargo
dependency layer anyway. The LB publisher took 135s and the companion 129s.
This contradicted the 14s no-source acceptance from #214 and made the rolling
edge tag both an output and a fragile cache authority.

The replacement makes dependencies an explicit shared build input. A new
`Dockerfile.deps` contains no repository source or secret: it starts from the
pinned Rust 1.95 Bookworm manifest digest, compiles every locked dependency
against dummy package targets, removes the package's own artifacts, and retains
the registry plus dependency outputs. Its GHCR tag includes the full SHA-256 of
`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, and `Dockerfile.deps`.
`bench/rust_deps_image.py` derives, updates, and validates that key across the
committed key, both release Dockerfiles, and Drone. Both real images inherit the
base and use `cargo build --locked --offline`, so a missing, stale, or incomplete
base fails closed instead of silently downloading or rebuilding dependencies.

Drone builds the dependency image only when one of those inputs or its derived
key changes. Both release publishers depend on it; Drone's skipped-step graph
correction lets source-only publishers proceed against the existing immutable
tag. `.drone.yml` was removed from both release path filters, so a CI-only merge
now runs zero publishers and cannot retag either edge image. A dependency change
is safe in one merge: the dependency publisher completes before both release
publishers start.

Local measurements used the final locked graph and the same pinned Rust base.
The one-time seed was 918,503,269 uncompressed bytes. Docker 29 built that cold
seed in 132.19s, then built the LB fully offline in 49.57s and the companion in
35.30s. An isolated Docker 20.10.24 daemon—the Drone engine generation—built
the seed in 90.00s and pushed it to a disposable local registry in 27.52s. A
second fresh Docker 20.10.24 daemon pulled it in 15.34s and built the LB offline
in 50.18s: 65.52s pull-plus-build versus #233's 135s publisher, a local 51.5%
reduction. These local-registry timings are kept separate from the following
real GHCR acceptance result.

Main Drone #241 then passed the real GHCR acceptance. The one-time dependency
seed took 140s; after it completed, the LB and companion publishers ran in
parallel and finished in 90s and 85s respectively. That reduces the LB
publisher from #233's 135s by 33.3% and the companion from 129s by 34.1% on the
actual runner. The whole dependency-changing build took 294s because it paid
both seed and application publication; ordinary source-only changes skip the
seed, while deployment/CI-only changes now skip every publisher.

The dependency tag is content-derived and normal CI never overwrites an old
key, but GHCR tags remain mutable registry objects rather than cryptographic
deploy attestations. Only trusted `main` pushes receive package credentials,
the dependency image contains public dependency sources/artifacts only, and
release compilation has no network access. Image signing or a post-seed digest
pin would be a separate supply-chain improvement. No node06 process, container,
image, route, engine, secret, or GPU state changed in this experiment.

## 2026-08-13 — r59 production-shaped dual snapshot admission contract

The canonical deployment now has a separate, explicitly profiled snapshot
overlay instead of promoting the earlier fixture-only Compose file. It pins a
snapshot-capable LB and companion image, runs one companion per engine under
UIDs 12001/12003, gives the LB only client UID 12002, and keeps session GID
12000 separate from per-engine metrics GIDs 12004/12005. Companions have
read-only roots, dropped capabilities, no host IPC, no devices/GPU, no
published port, exact engine-local ZMQ endpoints, and five narrowly scoped
mounts. The one-shot root provisioners are behind their own profile, have no
network/Docker/device access, and receive only metadata, digest secret, and
attestation-output mounts.

The LB overlay forces raw KV events off, defaults snapshot routing to off, and
mounts exactly two read-only runtime/session/digest/attestation domains. Its
pinned snapshot build independently permits compact state only in exact shadow;
ordinary approximate routing and `/health` remain independent. A Caddy snippet
can scrape only the two dedicated metrics sockets at
`/metrics/snapshot/0|1`; it explicitly forbids adding Caddy to session GID
12000.

The semantic validator renders companion-only and companion+provisioner
profiles and rejects cross-engine mounts, mutable images, raw/snapshot dual
authority, TCP metrics, shared metrics/session groups, Docker sockets,
GPU/device grants, broad mounts, incorrect identities, and implicit privileged
provisioning. Ten focused Python tests exercise the real render plus negative
mutations. The separate host preflight requires distinct symlink-free tmpfs
parents, exact owner/group/mode contracts, 32-byte one-link secrets, bounded
root-only metadata, provisioned attestations, and unique authority inodes.

This slice is repository-only. It does not create host users, groups, files,
directories, Caddy routes, containers, or node06 processes. Fixed-cardinality
LB reconnect/readiness metrics and hot LB attestation refresh are now merged.
Repin current images, pass host preflight, and first start with snapshot routing
still off.

Independent pre-merge review caught two production-contract gaps. First, the
base Compose exact-shadow default survived the intended off-mode overlay and
would have made the LB reject startup when both exact authorities were off.
The overlay now derives both `DS4_EXACT_ROUTE_MODE` and snapshot authority from
the same bounded `DS4_SNAPSHOT_ROUTE_MODE=off|shadow` control, with positive
renders and a divergence rejection test. Second, the initial Caddy validator
required both metrics sockets but did not reject an additional session-socket
proxy. It now accepts exactly the two ordered metrics UDS upstreams and rejects
every extra reverse proxy, including the peer session socket.

## 2026-08-13 — r61 Drone publisher path guard correction

The real deployment-only main acceptance contradicted r60's final claim. Drone
#245 changed only `.drone.yml`, operational documentation, one benchmark test,
and `deploy/dspark_0731`; nevertheless all three publisher steps were compiled
without a condition. The dependency image rebuilt for 138s, then the LB and
companion publishers ran for 87s/84s. Drone 2.12.1's native condition schema
does not include `paths`; lint accepted the unknown field but the server dropped
it. Moving the field to a pipeline trigger would have repeated the same
unsupported assumption.

The corrected pipeline keeps main-push event guards for secret isolation and
adds `bench/drone_publish_guard.sh` as the first command in each publisher
container. The installed `plugins/docker` image already contains POSIX shell,
Git 2.47.2, and `/bin/drone-docker`; it contains no Python and downloads
nothing. The guard verifies both `DRONE_COMMIT_BEFORE` and the documented
`DRONE_COMMIT_SHA` are available commit objects, verifies the range is nonempty,
and queries it with explicit top-rooted Git pathspecs for each owned-input
matrix. This avoids parsing filenames while retaining rename/delete coverage.
Matching inputs `exec /bin/drone-docker`; nonmatches return a distinct
skip status that the same command block maps to success before plugin startup.
Missing revisions, diff errors, and empty ranges fail closed.
The dependency guard remains a graph predecessor of both app publishers, so a
manifest/toolchain/key change seeds the required content-addressed base first.

Eight subprocess tests cover the #245-equivalent zero-publisher matrix, source
fan-out, manifest fan-out and ordering inputs, LB-only and companion-only build
inputs, deletions, an owned filename containing a newline, and every revision
failure. Two structural tests reject any
future unsupported `paths:` field and require each publisher to retain the
guard/skip/error/exec wrapper, push-only condition, and dependency ordering.
Against the actual #245 range, all three guards returned `skip`; none invokes
Docker, uses registry credentials, or touches GHCR. The first main build after
merge remains the real compiled-environment acceptance and should finish each
publisher guard in the seconds class. No node06 process, container, image,
route, engine, secret, or GPU state changed.

## 2026-08-13 — r62 Drone shallow predecessor recovery

The first r61 main acceptance, Drone #247, failed safely before Docker startup:
the server's shallow clone contained `DRONE_COMMIT_SHA` but not
`DRONE_COMMIT_BEFORE`, so all downstream publishers were blocked by the
dependency guard's `unavailable_revision` result. The guard now requires both
environment values to be exact 40-character hexadecimal object IDs before any
Git operation. It still requires the after commit locally, but if the validated
predecessor is absent it performs one quiet, noninteractive exact-object fetch
from `origin` with depth one, no tags, no submodule recursion, and no
`FETCH_HEAD` write, then revalidates the commit object. Fetch and validation
errors remain closed.

A real local `file://` shallow clone proves that an uppercase predecessor and
after SHA recover the missing predecessor and correctly skip a deployment-only
range. A PATH-wrapped Git regression proves malformed, short, non-hex, and
oversized revisions never reach `git fetch`; unavailable after objects and a
failed exact predecessor fetch also remain errors. No node06 state changed.
An additional real GitHub `--depth=1` clone of `fc4aa91` fetched predecessor
`fa6cd2f` by exact object ID in 0.8s, left no `FETCH_HEAD`, and returned skip for
the r61 CI/docs/bench-only dependency publisher range.

## 2026-08-13 — r63 workspace-independent Drone publisher plan

Drone #249 showed that even after the clone step checked out `b652abd`, the
`plugins/docker` command workspace did not expose that revision as a usable Git
object and the guard failed safely with `unavailable_after`. Git decisions now
happen once in the ordinary `rust-fetch` step after Cargo fetch. The planner
verifies workspace HEAD against the exact after SHA, recovers only a missing
exact predecessor in a shallow clone, evaluates all three pathspec sets, and
atomically replaces a private `.drone-publish-plan` directory. Publisher
containers perform no Git operations: they require a regular, non-symlinked
plan and revision-bound marker, skip on an absent marker, or fail closed on
unsafe/stale state before starting Docker.

Tests cover the full publisher matrix, real shallow-clone recovery, pull-request
inertness, replacement of a malicious directory and directory symlink without
touching its target, stale and symlinked markers, unusual filenames, deletes,
and revision/range failures. No node06 state changed.
A real GitHub depth-one clone of `b652abd` recovered predecessor `fc4aa91`,
published the empty-marker plan in 0.85s, left no `FETCH_HEAD`, and all three
publisher consumers returned skip.

## 2026-08-13 — r64 fail-closed semver tag releases

A second Drone document now owns release tags independently of normal PR/main
publishing. Only a `tag` event whose ref matches `refs/tags/v*` can instantiate
it. Before fetching dependencies, the Rust step verifies an exact 40-hex
checkout identity, `DRONE_TAG`/`DRONE_COMMIT_REF` agreement, checkout HEAD, and
an exact `v<Cargo package version>` match. It atomically publishes private LB
and companion markers bound to both SHA and tag. The Docker steps revalidate
those markers and the tag event without Git, wait for the complete Rust lint,
Rust test, agent-protocol, and deployment-Compose gate, then publish only
`${DRONE_TAG}` and `companion-${DRONE_TAG}` by registry manifest copy. The
publisher verifies OCI source, semantic version, and exact Git revision on the
existing SHA-tagged candidates, and requires copied destination digest equality.
It performs no build and no edge alias is present.

Eight focused behavioral/static tests finished in 0.74s. They cover a valid
prerelease, push/PR rejection in both planner and publisher, tag/ref/version/HEAD
mismatches, invalid revisions, Docker-incompatible Cargo build metadata,
malicious plan symlink replacement, stale/symlink marker rejection, trigger
isolation, immutable tag names, and full-quality dependencies. Drone lint also
accepted both pipeline documents. No tag, release, image, registry write, or
node06 change was made.

The first `0.1.0` dependency seed exposed a separate normal-main issue in Drone
#256 and its single retry #258. Both passed all quality steps, selected the
dependency publisher, then failed in 16–17s because `/usr/local/bin/dockerd`
never became reachable; the dependent LB and companion publishers correctly
stayed skipped. Compared with successful pre-guard #245, the compiled step no
longer carried plugin schema metadata after `commands` was added, so automatic
privileged-plugin treatment was absent. The repository API reports
`trusted=false`; the supplied account's apparently successful trust update did
not change server state, and normal lint rejects explicit privilege. Main
publishers now use a digest-pinned unprivileged Kaniko executor with the same
fail-closed plan and a GHCR remote cache. The tag pipeline uses unprivileged
`crane` manifest copies. A third blind retry was not attempted.

Pre-merge review found that #256/#258 never published the referenced dependency
image, so a source-only main build could have skipped its seed and failed both
application builds. `Dockerfile.deps` now separates the locked registry fetch
into its own reusable Kaniko layer, intentionally rotates the content key, and
the merge diff selects dependency, LB, and companion publishers together. The
same review closed a release immutability gap: an existing destination with the
source digest is idempotent success, a different digest is a hard conflict, and
only an explicit registry not-found result permits a copy. Ambiguous lookup
failures remain closed.

## 2026-08-13 — v0.1.0 tag publisher shell failure and bounded recovery

The immutable `v0.1.0` tag correctly peeled to qualified commit
`b0e070073d4266018d2f907ff35a7ee88adfdcd4`. Drone release build #266 passed
clone, authority/version planning, Rust lint, all Rust tests, agent protocol,
and Compose validation, then both four-second publisher steps ended with runner
status 255. Their logs contained only the pinned Crane image pull. The image's
entrypoint is `/ko-app/crane` and it provides `/busybox/sh`, not the `/bin/sh`
that Drone attempted for commands, so neither authentication nor any registry
read/write ran.

An initial source fix attempted to set `/busybox/sh` explicitly. Main build #268
passed in 61s and all ordinary publisher guards skipped in 0-1s with no Kaniko
or registry action. The one authorized command
`drone build promote helixml/mini-dynamo 268 release-v0.1.0` created recovery
build #269. Its tag/commit/version validation passed in one second, but Drone
2.12 silently discarded the entrypoint field and both publishers again failed
before commands with no logs. No registry operation occurred.

The durable fix is a content-keyed release-tools image built from digest-pinned
Alpine with the Crane binary copied from the digest-pinned upstream image.
`Dockerfile.release-tools` executes `/bin/sh -c 'crane version'` while building,
and all tag/recovery publishers use its generated content tag. A fourth guarded
Kaniko main publisher owns that image; release-tools input changes select it and
only it. A local cold build and actual `/bin/sh` invocation passed in 5.97s.

The separate one-purpose Drone promotion recovery accepts only target
`release-v0.1.0`; its validation
step fetches and peels the existing tag, requires the exact qualified commit and
the tag's Cargo version `0.1.0`, and emits revision/target/tag-bound private
markers. Its two Crane steps reuse the ordinary label/digest validation and
missing/same/conflict semantics to copy only `rust-b0e0700` to `v0.1.0` and
`companion-rust-b0e0700` to `companion-v0.1.0`. It cannot build, run Docker or
Kaniko, move a Git tag, update edge aliases, or overwrite a conflicting digest.
This entry records the source correction and design only; no promotion or
registry mutation was performed while preparing it.

Main build #271 passed in 83s and published only the new content-keyed tools
image; dependency, LB, and companion guards skipped in 0-1s. The tools Kaniko
step took 24s and produced
`release-tools-sha256-244bfd3b...@sha256:1d0d9c383119f43b832008d2b2866c43472175bf0d814d27032b677e30dcac43`.
An independent pull of that exact tag and digest ran `/bin/sh`, resolved
`/usr/local/bin/crane`, reported Crane revision
`7b32099eb119a9fdd715d84ef18c088cbe434c7f`, and found the Alpine CA bundle.
All four then-active normal/recovery consumers were pinned to that digest while
the sole publisher destination remained its content tag. This digest-only
change is not an image input and had to select zero publishers. No second
recovery promotion was attempted before that pin reached a green main build.

Main build #273 then passed in 61s: clone 2s, dependency planning 6s, Clippy
33s, Rust tests 50s, agent protocol and Compose 3s each. The dependency, tools,
LB, and companion publisher guards all explicitly skipped in 0-1s, proving the
digest-only merge performed no image or registry work. The single final command
`drone build promote helixml/mini-dynamo 273 release-v0.1.0` created recovery
build #274, which passed in 10s: clone 2s, immutable tag/commit/version authority
validation 1s, and concurrent LB/companion Crane copies 6s each. Its logs contain
no Kaniko, Docker daemon, Docker build, Cargo build, or edge update.

Independent registry reads confirmed `v0.1.0` equals qualified `rust-b0e0700`
at `sha256:62d949e0e6b3880796fab6c12f148f24d3f76449cb8397da6e81fe6e57dd70a1`,
and `companion-v0.1.0` equals `companion-rust-b0e0700` at
`sha256:4af08be5c011ac56d1bde2463e525c1d57d9ddd21391a3565ec55183566d9f95`.
Both retain source `https://github.com/helixml/mini-dynamo`, version `0.1.0`,
and revision `b0e070073d4266018d2f907ff35a7ee88adfdcd4`. The GitHub release was
subsequently verified by the release owner. With recovery complete, the
one-purpose promote pipeline, its scripts/tests, and plan ignore are removed;
the permanent normal tag pipeline keeps its two digest-pinned release-tools
consumers. There is no remaining Drone promote trigger or version-specific
recovery authority.

## 2026-08-13 — v0.1.0 candidate publication and node06 acceptance

The first public-release boundary is commit `b0e0700`. Stable v0.1 scope is the
OpenAI-compatible Rust proxy, approximate prefix/load routing, health-gated
failover, immediate downstream cancellation, compatibility shims, and the
existing `ds4proxy_*` serving telemetry. Local/remote tokenization, raw KV
events, compact snapshot companions, and exact placement remain observation
only, default-off where applicable, and outside the release contract.

Drone #263 passed the complete quality gate and the new daemonless main
publishers. The one-time cold content-keyed dependency seed took 286s; LB and
companion Kaniko publishers then ran concurrently for 142s/141s. Total wall was
503s. Dependency compilation itself was 52.19s and the two application compiles
were 45.53s/41.54s; snapshot, cache export, and registry upload dominated the
remaining cold wall. The published identities were independently checked with
the pinned registry client:

- dependency: `rust-deps-sha256-7da447db...` at
  `sha256:bdde49027da3c71761f7b86371d98cdfb5231b63f6779cd3424fc1944cdacdcd`;
- LB: `rust-b0e0700` at
  `sha256:62d949e0e6b3880796fab6c12f148f24d3f76449cb8397da6e81fe6e57dd70a1`;
- companion: `companion-rust-b0e0700` at
  `sha256:4af08be5c011ac56d1bde2463e525c1d57d9ddd21391a3565ec55183566d9f95`.

Both application configs carry source
`https://github.com/helixml/mini-dynamo`, semantic version `0.1.0`, and full
revision `b0e070073d4266018d2f907ff35a7ee88adfdcd4`. Edge aliases resolved to
the same SHA-tag digests. At qualification time no release tag existed; the
subsequent immutable tag points to this exact commit, and promotion may only
copy these already-qualified manifests while rejecting a conflicting destination.

The LB-only node06 swap completed in 6.9s under the shared deployment lock.
Both TP4 engines retained their containers and zero restart counts. Startup
reported version 0.1.0 with approximate prefix routing active, local tokenizer
and raw KV routing in shadow, exact placement canary zero, and snapshot routing
off. `/health` remained `ok` with 2/2 replicas.

A label-isolated cold cancellation gate deliberately used otherwise-idle engine
A while live Helix affinity traffic occupied B. The request entered vLLM at
631ms, curl closed at 2.016s with no response bytes, the LB's A-specific
inflight/load reservation reached zero 46ms later, vLLM A reached zero running
requests 269ms later, and the disconnect counter advanced exactly once. The
privacy-bounded journal recorded `client_disconnect` at 2.002s.

Fresh serving gates then measured:

- locality 3 apps x 4 sessions x 2 turns: 24/24 success, 371,712 / 450,564
  cached prompt tokens (**82.5%**), with exactly three cold app prefills;
- concurrent same-app c12/max256: 12/12, exact 6/6 split, **452 tok/s**;
- direct idle-engine-A c12/max256 while production remained on B: 12/12,
  **794.5 tok/s**, within 3.4% of the 822.1 tok/s matched control;
- the first box c24/max256 cell completed 24/24 at 1,306 tok/s but is rejected
  as a regression comparison: a live 1,495-token Helix turn occupied B for
  roughly 4.5-5.4s of every 5.4s throughout the cell.

The live workload continued rather than yielding a homogeneous c24 window. It
therefore became the production soak instead of being interrupted for a
synthetic number. At the final acceptance sample the candidate had served 206
chat requests (84 non-streaming and 122 streaming), all HTTP 200, plus ten
successful compatibility/other requests. There were no upstream-error series,
warning/error logs, or container restarts; both probes remained up. B's exact
shadow inventory stayed trusted at 6,178 blocks / 1,580,560 tokens while A's
old retained replay stayed conservatively untrusted, which did not affect
ordinary serving. The uncontaminated A-pair capacity control and balanced
same-app gate close the performance check; the contaminated c24 number is not
used as a release claim.

The previous immutable LB digest
`sha256:26f7a30fb5523be5b8fdecc251545a33580eb9b4fb8c66eba4b512de7a32052f`
remains the rollback. The candidate is accepted and stays live: correctness,
locality, balanced routing, isolated capacity, cancellation, health, restart,
and real-workflow soak gates all passed. A future idle-box c24 sample may update
the operating-range record but is not needed to manufacture a release result.

## 2026-08-13 — r70 snapshot host-authority setup

The remaining production-admission blocker for issue #41 was reproducible host
authority. A fixed, idempotent helper now owns the exact service identities,
six tmpfs authority directories, and four independent create-once secrets used
by the dual-engine snapshot overlay. It has no production path or numeric-ID
overrides, rejects name/number collisions and unsafe existing material before
ordinary setup mutation, never repairs or overwrites secrets, and keeps Caddy
membership behind a separate explicit metrics-only opt-in. Metadata and signed
attestation outputs remain owned by their existing provisioners.

Twelve in-memory policy tests passed in 0.001s, covering first apply and rerun,
read-only completeness, identity collisions, unsafe paths/filesystems, hard
links, invalid or reused secrets, bounded metadata, non-login accounts, and
Caddy session-group exclusion. The existing Drone Python-discovery lane picks
them up without a CI-pipeline change.

A read-only node06 audit under the deployment lock found no conflicting names,
UIDs, GIDs, paths, or secrets. `/run` is tmpfs and `/run/secrets` is currently
absent, which is the expected safe first-run shape. The LB and both TP4 engines
were running with zero restarts after the audit. No identity, group, directory,
secret, Caddy membership, container, route, image, engine, or GPU state changed.
The helper must land and pass Drone before an operator applies it; the first
companion deployment remains snapshot-routing-off and engine-restart-free.
