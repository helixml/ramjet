# Experimental GLM-5.3-Flash NVFP4 deployment

This is the one-file node06 deployment candidate for two NUMA-local SGLang
TP4 engines behind Ramjet. Start only one named engine until its loader,
SM120 kernels, host-memory peak, protocol behavior, and thermal state pass.

The authenticated OpenAI-compatible client base is
`http://100.89.187.17/v1`. Model-named ingress aliases are maintained by the
node infrastructure configuration for compatibility and are not part of this
deployment contract.

Current qualification: the MTP-off `glm53-b` loader, deterministic text and a
simple typed tool smoke, and one-batch c1/c8/c16/c24 scouts passed on
2026-08-27 with exact native
request/token reconciliation. The guard peaked at 42C intake and 65C GPU; the
engine was stopped afterward. A second smoke+c1+c8 interval with adaptive MTP
5/1/6 also passed; it improved c1 but left c8 aggregate throughput flat, so
MTP remains an experiment rather than the deployment default. See
`EXPERIMENTS.md` for measured memory, cache, startup, thermal, and shutdown
evidence. A later c1-only five-case agent corpus passed 4/5: the model
deterministically violated a nullable tool-argument contract. Full tool
correctness and long-context recall are therefore still open. Do not start both
engines or enable MTP based on loader success alone.

## Immutable inputs

- Model: `LibertAIDAI/GLM-5.3-Flash-NVFP4` at
  `9e0d74e3cef17f634e84fb8e2223707e02616290`. This is the revision in the
  working third-party report. The later Hub revision changes the chat template,
  while this runtime supplies its own reviewed multimodal template.
- Base image: `lmsysorg/sglang:glm-5.3-flash` multi-arch digest
  `sha256:3a97bd50034ca60c6e6c86b8e36a73675d261f6a5eb71197796aee5175409290`;
  linux/amd64 manifest
  `sha256:a3003a95c4eb352b4b659a677f4906f23f8d510b249ab9651bc15769d703b141`.
- Compatibility source:
  `0xSero/glm53-flash-nvfp4-sm120-exact-docker` at
  `8370bb04335bb07b6ee85907dd83cd1d300fa462`.

The third-party repository carries no detected license and replaces six whole
SGLang source files. Its image is therefore an internal experimental candidate,
not a redistributable or promotable production artifact. Do not push it to a
registry. Upstream or independently reviewed/licensed fixes are required before
promotion.

The referenced third-party Dockerfile also has two missing line-continuation
characters and does not parse as committed. `third-party-dockerfile.patch`
fixes only that Dockerfile syntax. The build helper archives the exact reviewed
commit, verifies all six published patch hashes, parses their Python syntax,
applies the two-line Dockerfile fix in a temporary directory, and builds a
local image.

```bash
SOURCE_DIR=/prod/src/glm53-flash-nvfp4-sm120-exact-docker-8370bb0 \
  deploy/glm53_flash/build-experimental-image.sh
```

Record the resulting `sha256:` image ID in node06's mode-`0600` `.env`; Compose
requires it and uses `pull_policy: never`.

The engine is forced into Hugging Face/Transformers offline mode. CUDA, Triton,
TorchInductor, FlashInfer, and related runtime caches are directed below each
engine's `/root/.cache` bind on `/prod`; this prevents JIT artifacts from
consuming the nearly full Docker/root filesystem and keeps A/B caches separate.

Download the exact model revision to the immutable model path, then run the
fail-closed checkpoint verifier before starting a container:

```bash
python3 verify-model.py \
  /prod/models/LibertAIDAI/GLM-5.3-Flash-NVFP4-9e0d74e3cef1
```

The verifier checks the four manifest-pinned metadata digests, exact 120-shard
index and on-disk sets, absence of partial downloads, regular-file shape, and
the exact aggregate tensor byte count. The Hugging Face client remains
responsible for transport-level verification of each downloaded object.

## Initial canary

The checked-in defaults intentionally reduce the working report's 1M context,
eight running requests, 93% static allocation, and adaptive EAGLE MTP profile to a
262K-context, four-request, 90%, MTP-off loader/correctness canary. This changes
several values only to minimize the first GPU exposure; it is not a performance
comparison. After the canary passes, change one environment value at a time:

1. enable the report's EAGLE 5-step/6-draft adaptive MTP profile;
2. compare one short c1 and one short c8 decode scout with exact token
   reconciliation;
3. first cap the separate KDA/Mamba state pool with
   `GLM_MAX_MAMBA_CACHE_SIZE=32`, then measure whether static allocation can fall
   from 0.90 toward 0.80 without reducing the useful KV/cache frontier;
4. raise running requests and graph capture from 4 to 8, with MTP off as the
   control and fixed EAGLE 3/1/4 (`GLM_MTP_ADAPTIVE=off`) versus adaptive 5/1/6
   as separate candidates;
5. keep the served limit at 262K until the open long-context CUDA-graph defect is
   fixed and the FP8-KV path passes 32K/128K recall checks;
6. do not copy the external report's 0.93 static allocation without a measured
   cache or concurrency requirement.

Do not run a sustained matrix. Every request-generating command must be the
child of `bench/node06_gpu_guard.py`, start only at 46C intake or below, and
abort at 50C. Model load and graph capture are also GPU work: isolate one TP4
pair and manually monitor intake, GPU telemetry, and driver errors throughout.

Render without mutation:

```bash
SGLANG_IMAGE=sha256:$(printf '1%.0s' {1..64}) \
  docker compose -f deploy/glm53_flash/docker-compose.yaml config --quiet
python3 deploy/glm53_flash/validate-compose.py
```

On node06, hold `/run/lock/ramjet-node06-deployment.lock`, pass the exact local
image ID, and name only the canary service:

```bash
docker compose up -d --no-deps glm53-b
```

After direct health is green, run `smoke.py` only as a child of the node06 GPU
guard. It performs two bounded 128-token requests, validates deterministic text
and a typed tool call, requires authoritative token usage, and prints no model
output. A fresh mode-`0700` experiment directory and fresh guard journal are
required for every invocation.

`brief-scout.py` is the only initial throughput probe. It runs one batch, caps
each request at 256 tokens and concurrency at 24, selects low reasoning effort,
reconciles client usage with native SGLang request/prompt/generation counters,
and prints no completions. Run c1 before higher concurrency; do not turn it
into a sustained matrix. `bounded-test-sequence.sh` is the reproducible MTP-off
sequence: it starts only B, requires direct correctness and tool use, runs one
batch each at c1/c8/c16, admits one 64-token c24 batch only with at most 44C
intake and 70C GPU temperature, then stops B. The whole script must itself be
the child of one fresh thermal guard invocation. Set
`GLM_TEST_MAX_CONCURRENCY=8` for the bounded MTP comparison so it stops after
c8; accepted values are 1, 8, 16, and 24.

Only after both direct engines independently pass may Ramjet be started with
both default upstreams. SGLang publishes no vLLM ZMQ KV-event feed, so exact KV
inventory and snapshot routing remain off. Ramjet's approximate prefix/load
routing, health, cancellation, metrics, and decision journal remain available.
The engine enables SGLang's response-level cache report so repeated-prefix
tests can reconcile `cached_tokens`; this is observability, not authenticated
exact cache-placement authority.
