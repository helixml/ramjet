---
name: load-test-ramjet-node
description: Execute guarded, reproducible load tests against a ramjet GPU inference deployment, especially the node06 8x RTX PRO 6000 stack. Use when asked to benchmark TPS, TTFT, TPOT, concurrency, long context, cache locality, agent/tool correctness, full-box capacity, or an engine candidate under load, and when comparing two serving configurations without disrupting production.
---

# Load-test a ramjet node

## Node06 access

The 2026-08-14 cooling/AC moratorium was retired on 2026-08-25 after the
operator confirmed the repair. Request-generating work no longer needs a
separate authorization token. Every run must still use the intake-air thermal
guard, its 25-minute continuous-inference cap, fresh owner-only evidence,
the common deployment lock for mutations, isolated candidate traffic, and an
exact rollback path. A future cooling incident may re-arm the compatibility
stop before telemetry or workload startup.

Run the smallest load test that answers the question, preserve production, and
leave an identity-bound experiment record. Treat node06 as a live serving box,
not a disposable benchmark host.

## Load the current contract

1. Read `AGENTS.md`, especially its thermal-admission, benchmark, candidate,
   contamination, and two-TP4-pair rules.
2. Read the current snapshot in `RESULTS.md` and the latest relevant entries in
   `EXPERIMENTS.md`; do not compare against a remembered baseline.
3. For engine or Compose changes, also read `deploy/dspark_0731/README.md` and
   the candidate directory named by the request.
4. Run repository commands from `/home/luke/inference/dspark_0731` on node06.
   Treat `deploy/dspark_0731` in this repository as the canonical deployment
   source and the node copy as a synchronized execution mirror.

## Admit hardware before load

Before any request-generating benchmark, capture current state from the
development machine:

```bash
bash bench/capture_node06.sh node06
```

Before load, inspect all GPU temperatures, reported
slowdown/shutdown thresholds, power, utilization, memory, topology, container
identity/restarts, ramjet upstream health, driver throttling, and available
BMC/facility cooling evidence. The watchdog's 50C chassis-intake ceiling is an
operational abort policy, not proof that the chassis is safe. Stop before load
if telemetry is missing, identities are ambiguous, cooling is unverified, a
GPU is already hot/throttled, or the serving stack is unhealthy.

Never run a sustained GPU command naked. Create an owner-only journal and make
the thermal watchdog the parent of the complete workload process tree:

```bash
cd /home/luke/inference/dspark_0731
install -d -o root -g root -m 0700 .experiments
experiment_id=$(date -u +%Y%m%dT%H%M%SZ)-focused-load
python3 bench/node06_gpu_guard.py \
  --label "$experiment_id" \
  --output ".experiments/${experiment_id}-thermal.jsonl" \
  -- COMMAND ARGUMENTS
```

Use a fresh journal for every invocation. Do not raise the 46C cool-start or
50C intake-abort defaults to make a run proceed. A telemetry failure, thermal
abort, child failure, signal, or orphaned process is a failed interval.

Obtain the bearer inside the remote shell without printing it and never enable
shell tracing:

```bash
export BENCH_TOKEN="$(grep -o 'Bearer [A-Za-z0-9_-]*' /etc/caddy/Caddyfile \
  | head -1 | cut -d' ' -f2)"
```

## Select the test shape

Default to one direct TP4 engine while the healthy peer serves production.
Point native metrics at the same engine and require client/native request and
token reconciliation. Use fresh input namespaces for every cell.

- Infernal r11 on engine B: capture immutable engine and agent metadata in an
  owner-only experiment directory and run `bench/candidate_gate.py --profile
  infernal-r11-b` through `smoke`; resume through `scout` and `matrix` only
  while each prior stage is green. The gate is deliberately pinned to the
  exact committed r11 admission bytes; add and review a new profile before
  qualifying another image. It does not own engine startup or rollback.
- Direct decode TPS/TTFT: use `bench/engine_matrix.sh` or a focused
  `bench/codebench.py` cell with `METRICS_URL` set to that engine.
- Agent/tool correctness: use `bench/agentbench.py` with the direct engine's
  `--engine-metrics` and `--require-reconciled-speculation`.
- Long-prefill interference: run `bench/mixed_bench.py` in both prefill-first
  and decode-first order with fresh salts and the same engine's metrics.
- Cache working set/eviction: use `bench/cachebench.py --require-reconciled`;
  keep cells serial and use a fresh salt.
- Concurrent shared-app balance: use `bench/concurrent_sameapp.sh` through the
  load balancer.
- Context frontier: use `bench/context_frontier.py`; begin with one bounded
  cell after repair, not the historical 52/64-app cliff.

For a focused direct cell, wrap the command rather than the shell that launched
the agent:

```bash
python3 bench/node06_gpu_guard.py \
  --label direct-b-code-c8 \
  --output .experiments/direct-b-code-c8-thermal.jsonl \
  -- env METRICS_URL=http://127.0.0.1:8013/metrics \
    BENCH_WORKLOAD=code \
    BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
    python3 bench/codebench.py http://127.0.0.1:8013 \
      deepseek-v4-flash 256 8 1
```

Do not interpret a cell if production traffic reached its native counters, a
container restarted, JIT appeared during measurement, request/token counts did
not reconcile, or the guard journal did not finish with `status=passed`.

## Preserve serving availability

- For direct B tests, first single-home ramjet on A and verify A health.
  Keep B out of every HTTP and KV endpoint list until qualification passes.
- Hold `/run/lock/ramjet-node06-deployment.lock` across every deployment
  inspect/mutate/verify interval. Recreate only the named service; never run an
  unscoped Compose update.
- Capture the baseline image IDs, rendered service hashes, starts, restart
  counts, and health before mutation. Prepare the exact rollback command first.
- Engine startup/model load/JIT is GPU work even before benchmark requests.
  Until a container-aware rollout owner exists, keep it isolated to one TP4
  pair and monitor facility/BMC plus driver telemetry manually.
- On failure or interruption, stop further stages, restore the baseline under
  the common lock, and prove 2/2 health and original identities before leaving.

Use both TP4 pairs concurrently only for independent direct-engine cells after
an explicitly authorized supervised single-pair re-entry has passed. Use a two-round
crossover with fresh inputs to remove engine/time bias. Keep LB routing,
cache-residency, exact-placement, aggregate-capacity, and eviction experiments
serial because parallel work changes their measured state.

## Record and decide

Record the hypothesis, one changed variable, immutable engine/LB/model identity,
effective argv, workload shape, fresh input identity, request and token counts,
wall time, TPS, TTFT p50/p95, TPOT when available, queue/prefill time,
preemptions, KV capacity, cached-token ratio, effective speculative tokens per
step, temperature/power/throttle maxima, JIT/runtime markers, contamination
checks, and rollback state in `EXPERIMENTS.md`.

Correctness is a hard gate. Never promote from draft acceptance, cache-hit
percentage, or aggregate TPS alone. Compare useful successful work plus latency
against the declared threshold. Stop at the first failed gate, retain negative
results, and report facts separately from inference without prompts,
completions, tool arguments, credentials, raw fingerprints, or container
environment dumps.
