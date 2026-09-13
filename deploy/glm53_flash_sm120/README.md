# GLM-5.3-Flash SM120 TP2 replicas

This is the independently managed node06 deployment contract for two
community-qualified SGLang v0.4.3 TP2 replicas. It deliberately contains no
Ramjet service. Qwen A continues serving on GPUs 0-3, GLM B owns GPUs 4-5 at
`127.0.0.1:8062`, and GLM C owns GPUs 6-7 at `127.0.0.1:8063`. Start or recreate
only the explicitly named service being managed.

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

Both replicas use the release's qualified TP2/C4 settings, with HiCache
disabled and distinct writable compilation caches. TP4 is not admitted: the
upstream launcher and measurements cover only TP2. Run `validate-compose.py`, verify the downloaded checkpoint
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

The current pin is a node06-local derived image rather than a published
registry artifact. Both replicas must use that exact image ID until the same
derived image is published by immutable registry digest and the Compose pin is
updated.

For a first canary, stage the complete directory plus the current benchmark
guard under `/home/luke/inference/glm53_flash_sm120`, create a root-owned
mode-0700 experiment directory, copy `node06-canary.sh` into it, and run
`node06-guarded-rollout.sh`. For an already-running exact base candidate,
`node06-parser-rollout.sh` rolls only the isolated GLM service and stops that
candidate on failure; a thermal abort never initiates another model load.
`node06-restore-qwen-b.sh` stops GLM B and recreates the canonical Qwen B
service only while GLM C is absent. Once the second GLM replica owns GPUs 6-7,
the legacy Qwen-B restore fails closed instead of creating an overlapping GPU
assignment. It requires the exact Qwen Compose SHA-256 in
`EXPECTED_QWEN_COMPOSE_SHA256`.

The accepted node06 concurrency curve uses `bench/codebench.py` with
`METRICS_URL=http://127.0.0.1:8062/metrics` and
`BENCH_REQUIRE_RECONCILED_ENGINE_COUNTERS=1`. Every measured cell must run
under a fresh guard journal and must be rejected if SGLang request/generation
counters do not exactly match response usage or if a late JIT marker lands
inside the measured interval.

Cold-prefill experiments use the same one-file deployment. The admitted
defaults are 6,144 for both `GLM53_CHUNKED_PREFILL_SIZE` and
`GLM53_MAX_PREFILL_TOKENS`, with `GLM53_MAX_TOTAL_TOKENS=500000`; set all three
together when reproducing the measured memory/capacity trade.
An 8,192-token candidate failed serving warmup with the full pool (125MiB free
for a 256MiB sparse-attention output) and again at 500,000 tokens (244.94MiB
free). `node06-prefill-6144-rollout.sh` is the one-time guarded, B-only
transition owner from the former 4,096/524,288 recipe. It keeps the
500,000-token pool to leave about 53MiB of margin
above the proportional 192MiB output allocation. It requires the 600W inference
ceiling on GPUs 4-5, verifies Qwen A before and after, and starts the guard's
1,500-second inference budget only after model load, graph capture, and
readiness. It fails immediately if the engine exits; a failed candidate is
stopped instead of initiating another model load during a thermal abort.
The admitted measurements reconciled native prompt, cached-prompt, generation,
and request counters. Against 4,096, the 6,144 setting improved 8K cold prefill
by 7.4%, c4 aggregate output by 4.7%, and c4 per-stream decode by 2.9%; 32K and
64K cold prefill improved only about 1%. The 24,288-token (4.6%) pool reduction
is therefore part of the accepted performance/capacity contract, not free
headroom.

## Add the second replica

Stage this directory on node06 without changing the existing Compose project,
create a fresh root-owned mode-0700 evidence directory below `.experiments`,
copy `node06-second-replica-rollout.sh` into it, and run only that staged script
under the thermal guard:

```bash
sudo python3 /home/luke/inference/glm53_flash_sm120/node06_gpu_guard.py \
  --label glm53-sm120-second-tp2 \
  --expected-gpus 6,7 \
  --output /protected/evidence/thermal.jsonl \
  --runtime-start-signal \
  --runtime-start-timeout-seconds 2400 \
  --max-runtime-seconds 300 \
  -- /protected/evidence/node06-second-replica-rollout.sh /protected/evidence
```

The 2,400-second bound covers model loading, JIT, and graph capture. The
five-minute inference budget begins only after readiness and covers one direct
acceptance request; it is not a 25-minute load window. The script requires
GPUs 6-7 to be empty at the 600W inference ceiling, leaves Qwen A, GLM B, and
Ramjet byte-identical, and removes only the new candidate container on failure.
After direct qualification succeeds, add GLM C to Ramjet with the reviewed
`deploy/qwen38_glm53_multimodel` recipe.
