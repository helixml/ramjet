# mini-dynamo benchmark results (2026-08-12)

Hardware: node06, 2× vLLM+DSpark TP4 instances (DeepSeek-V4-Flash-0731),
8× RTX PRO 6000. Baseline = `ds4-loadbalancer:1.0.1` (static 4KB-key hash
router). Candidate = `1.1.0-rc1` (overlap+load router, `alpha=4`).
Method: `bench/locality_bench.sh` + a concurrent-same-app harness.

## Current node06 production snapshot

The historical router study below remains reproducible, but the production
stack has since advanced to mini-dynamo rc7 and r34 engines with fixed K5,
A16 MoE kernels, NCCL PCIe P2P enabled, max-seqs 16, a 4,096 configured
(4,032 effective) scheduler quantum, automatic KV sizing, and NUMA-local CPU
placement. Current measured landmarks:

| Gate | Result |
|---|---:|
| box code c24/max256, rc7 | **1,820–1,844 tok/s**, 144/144 requests |
| box code c24 best matched gate, rc6 | **1,891 tok/s**, 72/72 requests |
| direct TP4 code c16/max256 | **1,130 tok/s**, 48/48 requests |
| direct TP4 prose c16/max256 | **824 tok/s**, 48/48 requests |
| KV capacity | **3,838,897 tokens/engine** |
| 209K cold prefill | **~7.7–8.1K effective tok/s** |
| 209K warm cached tokens | **208,896** |
| r34 KV shadow qualification | **both engines; replay + 2,442-removal eviction soak; trusted** |
| r19 exact-score shadow | **15 agree / 3 cold / 1 forced move; 14,336-token miss detected** |

Two r34 candidates were explicitly rejected after rolling B-only trials:
manual KV bytes gained just 1.16% capacity while bypassing runtime profiling;
dynamic DSpark depth regressed five of six code/prose concurrency points by
8–25%, lost 1.1% KV, and worsened the mixed tail. `EXPERIMENTS.md` is the
append-only source for configurations, comparisons, and rollback decisions.

The Rust KV-event path is still shadow-only, but its first real r34 feed is now
qualified end to end. A B-only rolling canary replayed the publisher from
sequence zero, filtered the engine's non-main masked geometries and two
unreconstructable 4-token partial entries conservatively, then applied 14 live
batches under c8 load without reconnecting or losing trust. No exact state was
used for placement, and B was returned to the event-off production recipe. A
then passed the symmetric live gate. With its KV allocation temporarily reduced
to 785,171 tokens, a 893K-token cold sweep produced 882 main-group removals;
the exact inventory contracted from 3,456 stored blocks to exactly 2,574
resident blocks while remaining trusted.

r19 then joined exact request IDs to those inventories without changing
placement. A sequential 3-app locality gate produced 18/18 local/remote token
parity matches, 15 exact/approximate agreements, three correctly cold
`all_zero` decisions, and zero missed tokens. A prompt warmed directly on A
but hidden from the approximate router was then sent to B; engine usage
reported zero cached tokens while exact A state found 14,336, producing the
expected single `would_move`. Under c12 same-app and c16 aggregate concurrency,
all 28 comparisons rejected changing alternative revisions rather than using a
post-decision cache state. At the production 32KiB tokenizer threshold, five
matched c16/max512 runs had a 1,343.4 tok/s r19 median versus 1,362.1 for r12
(-1.4%, within box noise); the matched long-prompt pair had identical 112,128
cached tokens and overlapping warm latency.

## Cache locality — TIE (no regression, no win at this scale)

Realistic ~18.5k-token system prompts, 2-3 apps × 4 sessions × 2-3 turns,
sequential:

| Router | cache-hit % | cold prefills |
|---|---|---|
| hash 1.0.1 | 82.9% | 3 (1/app) |
| mini-dynamo rc1 | 82.9% | 3 (1/app) |

**Honest finding:** the 1.0.1 hash router *already* co-locates same-app
sessions — its key is `marshal(messages[:2])` truncated to 4KB, and a big
system prompt fills those 4KB, so all sessions of an app hash identically.
At 2 instances with a KV pool that holds every template, steady-state cache
locality is already near-optimal; mini-dynamo matches it. No credit claimed.

## Concurrent same-app load — WIN (1.57× throughput)

12 concurrent sessions sharing one system prompt (the Helix "many agents on
one app" shape):

| Router | upstream split | wall | aggregate |
|---|---|---|---|
| hash 1.0.1 | **12 / 0** (sibling idle) | 7.5 s | 298 tok/s |
| mini-dynamo rc1 | **4 / 8** | 4.5 s | **469 tok/s** |

The exact truncation that gives the hash router free co-location makes it
**load-blind**: identical keys pin every concurrent session to one instance.
mini-dynamo keeps affinity until `alpha·inflight` exceeds prefix overlap,
then spreads — 1.57× aggregate, 40% lower wall clock, no cache-locality loss.
The 4/8 (not 6/6) split is correct: it prefers the warm instance early and
spreads only under pressure. Higher `alpha` spreads more aggressively.

## Aggregate regression — none

16-way mixed bench: 697.9 tok/s on both routers (within run-to-run noise of
the ~840 tok/s class; concurrent runs share the box with live traffic).

## Takeaways

1. Ship rc1: it strictly dominates — ties cache locality, fixes the hash
   router's load-blindness under concurrent same-app load. Rollback is one
   `LB_IMAGE` flip.
2. The bigger structural wins (cold-prefill placement across >2 instances,
   model-aware affinity for K3/KDA, KV-event ground truth, load-aware tie
   breaks under cache pressure) need higher instance count / eviction
   pressure than a 2-instance/2.5M-token-pool box exhibits — measure when we
   scale out. See DESIGN.md roadmap.
3. Benchmark honesty note: the first attempt used a ~330-token "system
   prompt" (a bug — the string wasn't actually 20KB) and showed a false 0%
   delta; fixed to a real ~18.5k-token prompt. Always check the prompt is the
   size you think it is.
