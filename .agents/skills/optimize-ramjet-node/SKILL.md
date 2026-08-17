---
name: optimize-ramjet-node
description: Measure and tune a ramjet GPU inference node using controlled, reproducible experiments. Use for GPU/NUMA placement, vLLM or DSpark scheduler settings, routing weights, cache locality, throughput, latency, concurrency, context-size, or image A/B optimization.
---

# Optimize a ramjet node

## Current node06 moratorium

For node06, do not benchmark, send inference requests, start/restart an engine,
load a model, run JIT/warmup, or apply a candidate deployment even if the host
returns. AC repair alone is insufficient. Until the user authorizes a specific
supervised window after the repair, work only from existing results and run
GPU-free local/CI tests, image/manifest inspection, receipt validation, and
dry-run Compose rendering. The experimental procedure below is future guidance,
not current node06 execution authority.

Optimize from evidence, with one attributable change at a time. Read
`AGENTS.md` for the current benchmark contract and `RESULTS.md` for the metric
definitions before running GPU work.

## Capture a clean baseline

1. Record the immutable engine and LB image digests, process/container start
   times, effective argv, model/tokenizer revision, GPU topology, CPU/NUMA
   placement, and current router configuration.
2. Check `nvidia-smi`, engine health, ramjet `/health`, queue/load gauges,
   and recent CUDA/NCCL/OOM/Xid/JIT markers. Do not benchmark an unhealthy or
   identity-ambiguous stack.
3. Keep unrelated production traffic off the engine whose native metrics will
   be interpreted. Reject intervals whose request/token counters do not
   reconcile.

## Choose the smallest useful experiment

- Routing policy: use `bench/route_replay.py` before a live A/B.
- Cache working sets: use `bench/cachebench.py` with a fresh salt for every
  cell; keep cells serial.
- Concurrent shared prompts: use `bench/concurrent_sameapp.sh`.
- Direct engine throughput: use `bench/codebench.py` or
  `bench/engine_matrix.sh` with the same engine's metrics endpoint.
- Long-prefill interference: use `bench/mixed_bench.py` in both request orders.
- Infernal r11 on B: use `bench/candidate_gate.py --profile infernal-r11-b`;
  stop at the first failed correctness, runtime, or scout gate. Add and review
  a pinned admission profile before using the gate for another engine image.

Start with a correctness smoke and one representative scout. Run a full matrix
only when the candidate passes. Always use fresh synthetic salts; never compare
one candidate's cold state with another candidate's warm state.

## Change one variable

State the hypothesis, primary success metric, regression guardrails, and
rollback value before mutation. Validate any new vLLM argument inside the
pinned image before restarting an engine. Roll one replica at a time and keep
production single-homed on its healthy peer.

Matched direct-engine work may use a two-round A/B crossover on independent GPU
pairs. Keep cache-locality tests, LB policy tests, aggregate box-capacity tests,
and exact-placement tests serial because concurrent work changes the state being
measured.

## Interpret the result

Compare end-to-end throughput, TTFT, request success, token reconciliation,
queue/prefill time, preemptions, and effective speculative tokens per step.
Never promote on cache-hit percentage or draft acceptance alone; either can
improve because its denominator shrank.

Record commands, fresh input identity, wall time, image/process identity,
metric deltas, variance, and contamination checks in `EXPERIMENTS.md`. Promote
only when the result beats the declared threshold without a correctness or
latency regression; otherwise restore the baseline and keep the negative result
as evidence.
