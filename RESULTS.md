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
| r20 attested pre-route shadow | **15 agree / 3 cold; forced miss found 36,096 warm tokens before mutation** |
| r21 exact placement canary | **2 moves / 2 agrees; 32,768 cached tokens on all 4 forced-warm requests** |
| r32 session-stable exact canary | **2/4 constructed misses corrected, 2/4 exact agreements; all reused 32,768 tokens** |
| r21 production shadow policy | **71.6% locality; c12 566 tok/s; c16/max512 1,343 tok/s; 28/28 requests** |
| r22 production client cancellation | **2.000s disconnect; LB load + vLLM running zero by 2.019s** |
| r23 publisher-safe KV replay | **1,332/1,724 batches trusted first attempt; c16/max512 1,462.9 tok/s** |
| r24 cache-counter reconciliation | **52/52; zero spread across response/LB/native views; 1/4/8 apps** |

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

Public r22 is now the production LB with event publishers enabled on
container-only ports, manifest-attested exact routing, and the gated placement
policy evaluated in non-mutating shadow mode. The LB-only promotion left both
engine processes and KV caches intact. Both runtime identities attested and
late-subscriber replay restored both authoritative inventories. The
post-deploy gates passed at 565 tok/s for a 6/6 c12 same-app split and
1,354.0 tok/s for c16/max512, with all 28 requests successful.
Exact state remains telemetry-only and cannot change placement.

r22 watches downstream closure concurrently with upstream reads. In a
production forced-cancellation gate, a 4,096-token stream was active on engine
A before the client timed out at 2.000s; by the first 2.019s sample, proxy
inflight/load and both engines' running-request gauges were zero, and the
disconnect counter had incremented exactly once. This closes the prior gap
where a silent engine could retain work until its next response chunk.

r23 removes the replay-recovery blocker without changing request placement.
The node06 vLLM publisher streams retained replay synchronously from one
ROUTER thread. A framing-only pure-Rust ZMTP client handled small tails but
could stall before the end marker on a large response; the same full 1,292-
batch, 29.9MB response drained through libzmq in 77ms. mini-dynamo therefore
keeps async Rust for live SUB events and confines libzmq replay to a deadline-
bounded blocking worker with fresh identities and drain-through-validation.
An isolated start restored 1,293/1,684 batches on the two engines concurrently;
production then restored 1,332/1,724 on its first attempt. The post-promotion
c12 gate split 6/6 at 556 tok/s and c16/max512 reached 1,462.9 tok/s. Both
engine processes and caches were retained. Exact placement remains telemetry-
only.

r20 removes the concurrency ambiguity without enabling exact placement. A
SHA-pinned manifest replays ten local token-vector goldens at startup and
continuously attests both engines' model identity and r34 `/version`. With the
production 32KiB admission threshold and eight non-blocking CPU permits, the
final 3-app × 3-session × 2-turn gate passed 18/18 at 78.9% cache hit; the
pre-route scorer recorded 15 agreements and three cold decisions. c12 passed
12/12 with a 6/6 split at 564 tok/s. A prompt warmed directly only on A was
approximately routed cold to B, while the pre-route exact lookup found 36,096
tokens on A and emitted `would_move` before either inventory could be mutated.
A wrong-version negative control kept both attestation gauges at zero and
served normally through the approximate fallback. Two matched short-prompt
c16/max512 samples averaged 1,342 tok/s through r20 versus 1,350 through r19
(-0.6%, inside shared-box noise). Exact placement remains disabled.

r21's placement mode remains default-off; only its non-mutating policy shadow
is in production. A constructed four-request run warmed each fresh 228,791-byte
prompt only on engine A. Exact placement retained two approximate agreements
and corrected two approximate misses, sending all four requests to A where
usage reported 32,768 cached tokens. Its matched 2-app locality gate was
identical to r20 at 71.6%. c8 same-app was 395 versus 406 tok/s and c16/max256
was 1,110 versus 1,147 tok/s, differences of about 3% inside shared-box noise.
An independent negative-health canary reported one healthy replica as
`degraded` and sent 4/4 requests only to that replica. All 97 Rust tests, both
Drone triggers, and GitHub Actions passed; no production component changed.
An intentional canary-only restart then served the first long request through
the approximate fallback with `inventory_untrusted`. B replayed 943 retained
batches; A remained fenced until its next full-block event and replayed 885.
With both inventories trusted again, the same four-request gate reproduced two
exact moves, two agreements, and 32,768 cached tokens on every request.
The follow-up `718012c` shadow-policy canary then observed a constructed
8,192-token/load-gated `would_move` while leaving the request on B with zero
cached tokens, proving the counterfactual metric cannot affect placement. Two
reverse-order c16/max256 pairs averaged 1,192 tok/s through the canary versus
1,221 through r20 (-2.3%, within the shared-box noise band); all 64 requests
succeeded. The public r21 promotion then recorded two agreements and 12
all-zero decisions in its initial qualification gates; production has not yet
produced an organic `would_move` sample.
The 2,048-batch/20-second replay setting recovered authoritative inventories
of 1.53M and 1.27M token IDs. The initial simultaneous full replays exposed
publisher-side stale identity/backpressure behavior after aborted requests;
serving stayed healthy and exact routing remained fenced until later contiguous
replays restored trust.

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
