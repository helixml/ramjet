# Infernal Invocation r11 candidate

This directory pins the upstream r11 image as a direct, one-engine candidate.
It does not build or patch the image. `manifest.json` locks the registry image,
config, entrypoint, the complete effective image-environment and
`local-inference.*` label deltas, and the unique compressed layer-blob shape.

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

Observed warm registry reads took 2.6-7.9 seconds. The 40-test local Infernal
gate takes about 0.12 seconds. Run both before a large pull and repeat the
registry check immediately before a live qualification.

After the immutable image is present locally, capture its real vendor-wrapper
chain without a network, model mount, vLLM process, or GPU allocation:

```bash
R11_RECEIPT_DIR=$(mktemp -d)
python3 bench/serving_runtime_image_probe.py \
  --compose-overlay deploy/dspark_0731/infernal-r11-candidate/docker-compose.overlay.yaml \
  --service dspark-0731-b \
  --manifest deploy/dspark_0731/infernal-r11-candidate/serving-runtime.template.json \
  --output "${R11_RECEIPT_DIR}/serving-runtime.json"
```

The template carries an explicit reviewed allowlist of 216 stable non-secret
container environment names; unknown names, secrets, and the hardware-derived
`_CUDA_COMPAT_STATUS` diagnostic are never captured. Review the
resulting argv, environment values, package versions,
and hashes for both launcher scripts and the exact NCCL library before
committing it. The committed receipt is then a single exact runtime and
image-native argument-parser gate:

```bash
python3 bench/serving_runtime_image_probe.py \
  --compose-overlay deploy/dspark_0731/infernal-r11-candidate/docker-compose.overlay.yaml \
  --service dspark-0731-b \
  --manifest deploy/dspark_0731/infernal-r11-candidate/serving-runtime.json \
  --validate-engine-args
```

Final hardened warm repeats put receipt generation/check at 0.73-1.05 seconds
and the image-native CLI/`AsyncEngineArgs` parse at 8.75-12.82 seconds, for
9.53-13.70 seconds combined. Both commands require the local immutable
image (`--pull=never`), force the ordinary `runc` runtime, and bind the evidence
collector over the image's actual vLLM executable without changing production
`PATH`. The parser uses CPU defaults solely because the container has no GPU;
it does not construct an engine or load model configuration, and does not
replace the guarded live smoke. Image download time is outside probe timing.

The r4/r11 manifests contain 95/96 layer descriptors and 78/79 unique layer
blobs respectively. r11 contains 12.79 GiB of unique compressed blobs and
shares 51 blobs (9.85 GiB) with immutable r4, leaving 2.94 GiB/28 blobs unique
to r11.
Do not prune the cached r4 image before the node06 pull; verify the exact r4
digest is still present, then pull r11 once outside startup/benchmark timing.
If r4 was pruned, budget the full 12.79 GiB cold transfer instead of treating
the incremental figure as guaranteed.

## First run after the cooling repair

Do not start with a full-box matrix. Capture idle GPU/BMC/airflow evidence,
then keep the load balancer single-homed on A and qualify r11 only on B. Hold
`/run/lock/mini-dynamo-node06-deployment.lock` for every deployment mutation.
The first image pull is intentionally outside benchmark timing and happens
once.

Before assigning GPUs, use the pulled image for the pinned-image-specific
`EngineArgs` validation described in `AGENTS.md`. The overlay deliberately
restores r11's vendor wrapper entrypoint, pins the qualified r4 model/tokenizer
revision, sampling, graph-96, InstantTensor, and LMCache-off launcher inputs,
single-homes every LB engine/KV endpoint on A, and otherwise changes B only by
image, entrypoint, and isolated cache mounts. Under the common lock, first
recreate only `ds4-loadbalancer` with the
base plus overlay, verify its effective environment contains no B endpoint,
then recreate exactly `--no-deps dspark-0731-b` from the same render. Never
start B first or run an unscoped `docker compose up`.

Watch facility/BMC cooling during model load and JIT. Do not generate requests
until B is healthy, its immutable metadata is captured, and the eight-GPU
thermal guard owns the workload. Rollback also holds the common lock: recreate
only B and the LB from base Compose, then verify the original images, render,
and 2/2 health before releasing it.

Before the guarded smoke, use `install -m 0600` to copy `manifest.json` and
`serving-runtime.json` from this directory into an owner-only mode-0700 node06
experiment directory; keep engine metadata, agent metadata, the journal, and
artifacts in that directory rather than `/tmp`. Pass both paths to
`candidate_gate.py --profile infernal-r11-b` together with
`--expected-gpu-count 4` and
`--engine-metrics http://127.0.0.1:8013/metrics`. The gate holds the common
deployment lock, pins the exact committed receipt bytes, and binds the live
image/config plus the current `vllm serve` child lifetime, argv, environment,
launcher/NCCL artifacts, and packages to them. It requires B's sole Docker GPU
request to select exactly 4-7, verifies all LB HTTP/KV endpoints remain A-only,
probes A and B health at every boundary, and requires native request/token
reconciliation. It does not supervise model startup or perform rollback.

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
