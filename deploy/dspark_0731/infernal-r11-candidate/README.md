# Infernal Invocation r11 candidate

This directory pins the upstream r11 image as a direct, one-engine candidate.
It does not build or patch the image. `manifest.json` locks the registry image,
config, entrypoint, and a selected r4-to-r11 source/runtime label contract.

The declared CUDA 13.3, Torch 2.13, FlashInfer 0.6.18, InstantTensor 0.1.9,
NCCL 2.31.2, vLLM base commit, and LMCache base commit/version are unchanged.
The vLLM, B12X, and LMCache integration trees change. The named Kimi-K3 base
tag is the same but its recorded content ID also changes, so native binary
equivalence is not proven. Correctness and performance must both be
requalified; the metadata alone is not a throughput result.

## Fast GPU-free gate

This concurrently verifies both immutable r4/r11 registry manifests/configs
and does not download image layers:

```bash
python3 bench/infernal_registry_candidate.py
python3 deploy/dspark_0731/infernal-r11-candidate/validate-compose.py
```

Observed warm registry reads took 2.6-7.9 seconds. Run the check before a
large pull and again immediately before a live qualification.

## First run after the cooling repair

Do not start with a full-box matrix. Capture idle GPU/BMC/airflow evidence,
then keep the load balancer single-homed on A and qualify r11 only on B. Hold
`/run/lock/mini-dynamo-node06-deployment.lock` for every deployment mutation.
The first image pull is intentionally outside benchmark timing and happens
once.

Before assigning GPUs, use the pulled image for the pinned-image-specific
`EngineArgs` validation described in `AGENTS.md`. The overlay deliberately
restores r11's vendor wrapper entrypoint, single-homes every LB engine/KV
endpoint on A, and changes B only by image, entrypoint, and isolated cache
mounts. Under the common lock, first recreate only `ds4-loadbalancer` with the
base plus overlay, verify its effective environment contains no B endpoint,
then recreate exactly `--no-deps dspark-0731-b` from the same render. Never
start B first or run an unscoped `docker compose up`.

Watch facility/BMC cooling during model load and JIT. Do not generate requests
until B is healthy, its immutable metadata is captured, and the eight-GPU
thermal guard owns the workload. Rollback also holds the common lock: recreate
only B and the LB from base Compose, then verify the original images, render,
and 2/2 health before releasing it.

The decision ladder is deliberately fail-fast:

1. Five deterministic agent-protocol requests
   (`candidate_gate.py --through smoke`).
2. One code and one prose c8 scout (`--through scout --resume`).
3. The six-cell direct-engine matrix only if both scouts pass
   (`--through matrix --resume`).
4. A two-round r34/r11 TP4 crossover only if r11 is close enough to the
   promotion threshold to justify a second warm start.

Use fresh inputs in every performance cell and point `METRICS_URL` at B. Keep
production off B until correctness is green. Cache-locality, exact-placement,
and aggregate box-capacity tests remain serial and come after the direct-engine
decision; they are not part of the first iteration.
