---
name: deploy-mini-dynamo
description: Deploy or upgrade mini-dynamo and OpenAI-compatible GPU engines on a Linux node with Docker Compose. Use for first-time node setup, creating a Compose stack, validating GPU/container prerequisites, promoting an immutable image, performing an LB-only rollout, or planning a rollback.
---

# Deploy mini-dynamo

For node06, the 2026-08-14 cooling/AC moratorium permits planning and GPU-free
image/manifest/Compose validation only. Do not mutate its deployment, send a
verification request, start/restart an engine, load a model, or run JIT/warmup
even if the host returns. Resume only when the user authorizes a specific
supervised window after the AC repair. Other explicitly authorized nodes retain
the generic procedure below.

Deploy a reproducible stack without assuming that an arbitrary node has the
reference node06 topology.

## Establish the target

1. Read `docs/configuration.md`. For the full DeepSeek-V4 stack, also read
   `deploy/dspark_0731/README.md` and its Compose file.
2. Determine whether the user wants only mini-dynamo in front of existing
   engines or the complete two-engine stack.
3. Inspect the node before changing it:

   ```bash
   docker version
   docker compose version
   nvidia-smi -L
   nvidia-smi topo -m
   lscpu
   docker info | sed -n '/Runtimes/,+3p'
   ```

4. Record GPU count/topology, CPU NUMA layout, available RAM/disk, current
   containers, occupied ports, and the existing Compose project. Never copy
   node06 GPU IDs, CPU sets, model paths, or credentials onto another host
   without matching this discovery.

## Prepare missing prerequisites

Require a working NVIDIA driver, NVIDIA Container Toolkit, Docker Engine, and
the Compose plugin. If any are missing, use the current official instructions
for the detected distribution; do not invent pinned package versions or pipe a
download directly into a shell. Package, driver, daemon, firewall, and user-
group changes need the user's deployment authority and an explicit maintenance
window. Verify the result with `nvidia-smi` on the host and a small
GPU-enumeration container before starting a model server.

## Build the Compose contract

- Pin production images by immutable digest. Do not introduce `latest`.
- Keep credentials in an uncommitted `.env` or secret store and set its mode to
  `0600`. Never print token values or persist them in command history.
- For existing engines, mini-dynamo normally needs only `MD_UPSTREAM`; add
  `MD_UPSTREAM_TOKEN` only when the engines require it. Leave tokenizer, KV
  event, exact route, and snapshot route modes off unless the requested stack
  includes every documented authority and validator.
- Bind public access through the node's authenticated reverse proxy. Bind
  direct engine and metrics ports to loopback unless the user provides a
  protected network design.
- Preserve a known-good image digest and rendered Compose file for rollback.

For the reference stack, edit `deploy/dspark_0731/docker-compose.yaml` in this
repository first. Treat the adjacent snapshot overlay as an admission artifact,
not a default feature.

## Validate before mutation

Run the narrow, read-only gates first:

```bash
docker compose -f COMPOSE_FILE config --quiet
docker compose -f COMPOSE_FILE config --images
docker compose -f COMPOSE_FILE ps
```

Pull images before the maintenance window. When adding or changing a vLLM flag,
exercise the pinned image's argument validation in the healthy peer or a
disposable container before assigning GPUs. Do not discover an unsupported
flag during a resident engine restart.

## Deploy and verify

Use the exact Compose project and explicitly name services. For an LB-only
change, recreate only mini-dynamo; do not restart engines or discard their KV
caches. On node06, hold the repository-documented deployment lock for the
complete inspect/mutate/verify interval.

After startup, verify:

```bash
docker compose -f COMPOSE_FILE ps
curl --fail http://127.0.0.1:API_PORT/health
curl --fail http://127.0.0.1:METRICS_PORT/metrics
nvidia-smi
```

Then send one small synthetic OpenAI request, confirm a success status and
usage, inspect bounded recent logs, and confirm `ds4proxy_upstream_up` for every
replica. Do not print completions, prompts, tokens, or secrets in an operational
report.

If verification fails, stop further rollout, preserve logs and identity, and
restore the recorded image digest with the same Compose/locking path. Report
what changed, health/metric evidence, interruption duration, and the rollback
state.
