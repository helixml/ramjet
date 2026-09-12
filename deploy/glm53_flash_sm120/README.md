# GLM-5.3-Flash SM120 TP2 canary

This is the isolated node06 deployment contract for the community-qualified
SGLang v0.4.3 stack. It deliberately contains no Ramjet service. During the
canary, canonical Qwen B is stopped under the common deployment lock, Qwen A
continues serving through the unchanged load balancer, and GLM is reachable
only at `127.0.0.1:8062`.

Immutable inputs:

- Runtime: `ghcr.io/ormandj/sglang-glm53-flash-sm120` at manifest digest
  `sha256:ec4243f940a179a27fea21895077efd47cd050501f99a1d2a5fecf7df2e7be71`.
- Model: `ormandj/GLM-5.3-Flash-W4A16-NVFP4-K32-Experts-FP8-WO` at revision
  `ee0989a944b0e213589191d7fca63af825a0741e`.
- Runtime source revision: `386684975edf3cbce15c4b12df37908366e2aa8b`.
- Nullable-parser canary image on node06:
  `sha256:024a988fd0c0e15d80e382073c05657b2d57f52611c324599508cdb62b9debb8`.
  Its patched GLM47 parser is
  `8ed76f9da2aa782b3e9374d00687186624fd383e98acf9ba0d6ac9759e1425d7`.

The initial profile matches the release's qualified TP2/C4 settings, with
HiCache disabled. TP4 is not admitted: the upstream launcher and measurements
cover only TP2. Run `validate-compose.py`, verify the downloaded checkpoint
with `hf cache verify --fail-on-missing-files`. The downloader's own
`.cache/huggingface` metadata is expected local-only state, so the generic
`--fail-on-extra-files` switch is intentionally not used. Run the rollout only
beneath `bench/node06_gpu_guard.py`. Do not recreate or single-home the shared
load balancer.

`Dockerfile.nullable-parser` derives a candidate from that exact runtime and
fails closed unless the upstream GLM47 parser has the reviewed SHA-256. It
fixes streaming schemas whose type is exactly `["string", "null"]`: the parser
buffers that value until the XML argument closes, then emits either JSON null
or a quoted JSON string. This is a parser correction, not a GLM renderer
profile. Model loading and graph capture remain intake-monitored deployment
work. The rollout uses the guard's runtime-start signal immediately before its
post-readiness request, so loading does not consume the 1,500-second inference
budget. The load remains bounded separately at 2,400 seconds. Each later
request-generating qualification cell gets a fresh budget.

Build the parser layer on a host that already has the exact 14.7 GB base
image. Record the resulting image ID and repin the Compose file before using
a rebuilt artifact; the committed pin identifies the live node06 canary build:

```bash
docker build --network=none -f Dockerfile.nullable-parser \
  -t ramjet/glm53-sm120:nullable-parser-r1 .
docker image inspect ramjet/glm53-sm120:nullable-parser-r1 \
  --format '{{.Id}}'
python3 validate-compose.py
```

The current pin is a node06-local canary image, not a published registry
artifact. Do not promote it to a durable deployment until the same derived
image is published by immutable registry digest and the Compose pin is updated.

For a first canary, stage the complete directory plus the current benchmark
guard under `/home/luke/inference/glm53_flash_sm120`, create a root-owned
mode-0700 experiment directory, copy `node06-canary.sh` into it, and run
`node06-guarded-rollout.sh`. For an already-running exact base candidate,
`node06-parser-rollout.sh` rolls only the isolated GLM service and stops that
candidate on failure; a thermal abort never initiates another model load.
`node06-restore-qwen-b.sh` stops GLM and recreates the canonical Qwen B service;
it requires the exact Qwen Compose SHA-256 in
`EXPECTED_QWEN_COMPOSE_SHA256`.

The accepted node06 concurrency curve uses `bench/codebench.py` with
`METRICS_URL=http://127.0.0.1:8062/metrics` and
`BENCH_REQUIRE_RECONCILED_ENGINE_COUNTERS=1`. Every measured cell must run
under a fresh guard journal and must be rejected if SGLang request/generation
counters do not exactly match response usage or if a late JIT marker lands
inside the measured interval.
