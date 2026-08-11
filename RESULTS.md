# mini-dynamo benchmark results (2026-08-12)

Hardware: node06, 2× vLLM+DSpark TP4 instances (DeepSeek-V4-Flash-0731),
8× RTX PRO 6000. Baseline = `ds4-loadbalancer:1.0.1` (static 4KB-key hash
router). Candidate = `1.1.0-rc1` (overlap+load router, `alpha=4`).
Method: `bench/locality_bench.sh` + a concurrent-same-app harness.

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
