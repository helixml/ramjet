---
name: troubleshoot-ramjet-node
description: Diagnose a ramjet GPU inference node by checking NVIDIA GPUs, Docker Compose services, OpenAI endpoints, router health, Prometheus metrics, logs, networking, and basic synthetic requests. Use for outages, degraded health, low throughput, cache misses, stuck requests, GPU errors, or post-deploy verification.
---

# Troubleshoot a ramjet node

For node06, the 2026-08-14 cooling/AC moratorium overrides the synthetic-test
and mutation steps below. Read-only health, metrics, bounded logs, inventory,
and image metadata checks are permitted when actually needed for diagnosis, but
do not poll merely because the host may have returned. Do not send inference
requests or start/restart an engine until the user authorizes a specific
supervised window after AC repair.

Diagnose first. Do not restart, recreate, reconfigure, or delete anything unless
the user also asks for a fix. Keep all output bounded and redact credentials,
prompts, generated text, upstream addresses, and cache identifiers.

## Establish scope and time

Record the reported symptom, first observed time, affected endpoint/model, and
the Compose project. Capture image digests and container start times so a
silent restart or mixed version cannot be mistaken for a performance issue.

## Check from hardware upward

Run read-only checks in this order:

```bash
nvidia-smi -L
nvidia-smi --query-gpu=index,uuid,temperature.gpu,power.draw,memory.used,memory.total,utilization.gpu,ecc.errors.uncorrected.volatile --format=csv,noheader
nvidia-smi topo -m
df -h / /tmp
free -h
docker compose -f COMPOSE_FILE ps
docker stats --no-stream
```

Look for missing GPUs, Xid/ECC faults, thermal or power limits, exhausted VRAM,
host memory/disk pressure, restart loops, unhealthy containers, and unexpected
GPU assignments. Use `docker inspect` for exact image, start time, health, and
device bindings; do not dump the full environment because it may contain
secrets.

## Check the serving path

```bash
curl --silent --show-error --fail http://127.0.0.1:API_PORT/health
curl --silent --show-error --fail http://127.0.0.1:METRICS_PORT/metrics \
  | grep -E '^(ds4proxy_upstream_up|ds4proxy_route_decisions_total|ds4proxy_cache_requests_total|ds4proxy_snapshot_route_ready)'
curl --silent --show-error --fail http://127.0.0.1:ENGINE_PORT/health
```

Interpret `/health` as `ok`, `degraded`, or `unhealthy`; use opaque replica
ordinals in reports. Compare router upstream health, inflight/load, route split,
cache outcomes, TTFT, native queue/prefill gauges, preemptions, and GPU
utilization. A healthy HTTP endpoint with a growing queue is a capacity or
scheduler symptom, not a network outage.

Inspect only bounded recent logs around the incident:

```bash
docker compose -f COMPOSE_FILE logs --since 15m --tail 300 ramjet
docker compose -f COMPOSE_FILE logs --since 15m --tail 300 ENGINE_SERVICE \
  | grep -Ei 'error|fatal|panic|traceback|CUDA|NCCL|OOM|Xid|JIT'
```

## Run basic synthetic tests

Skip this entire section on node06 while the operational moratorium is active.
Healthy control-plane reads do not authorize inference traffic.

Use a synthetic prompt and obtain the bearer token without printing it. First
query `/v1/models`, then send a deterministic request with at most eight output
tokens. Confirm HTTP success, response usage, and
`X-Mini-Dynamo-Upstream`. Run one request at a time before any concurrency
test.

If basic requests pass, choose only the focused test that matches the symptom:

- cache/locality: `python3 bench/cachebench.py ... --require-reconciled`
- route distribution: `bench/concurrent_sameapp.sh`
- agent/tool protocol: `python3 bench/agentbench.py run ...`
- direct engine decode: `python3 bench/codebench.py ...`
- long prefill: `python3 bench/mixed_bench.py ...`

Use a fresh salt and point collectors at the same engine under test. Stop if
native/client token counts do not reconcile or live traffic contaminates the
interval.

## Report the diagnosis

Separate observed facts from inference. Report the failing layer, first bad
timestamp, affected replica ordinal, image/process identity, GPU and container
state, decisive metrics/log markers, and the smallest next action. If the root
cause is not proven, give ranked hypotheses and the read-only check that would
distinguish them; do not present a restart as a diagnosis.
