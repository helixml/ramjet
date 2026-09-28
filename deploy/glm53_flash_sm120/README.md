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
- Nullable-parser image on node06:
  `sha256:024a988fd0c0e15d80e382073c05657b2d57f52611c324599508cdb62b9debb8`.
  Its patched GLM47 parser is
  `8ed76f9da2aa782b3e9374d00687186624fd383e98acf9ba0d6ac9759e1425d7`.
- Serving image on node06 (`Dockerfile.swiglu-clamp` over the nullable-parser
  image): `sha256:899fe8eb0f563b6654125f7b1c3ac7ad497e5c2e02256b30dc8fd7059e957a64`.

Both replicas use the release's qualified TP2/C4 settings plus the prefix-cache
settings below, with distinct writable compilation caches. TP4 is not admitted: the
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

## Routed-expert SwiGLU clamp

GLM-5.3 declares `swiglu_limit = 10.0`. Shared experts and dense MLPs apply it in
`glm5_next.swiglu_clamped`, but on the SM120 W4A16 routed-expert path the limit
was dropped twice: SGLang's `_run_flashinfer_b12x_w4a16` never passed it to
`launch_sm120_moe`, and FlashInfer 0.7.0's `_launch_sm120_w4a16_moe` never
forwarded it to `run_w4a16_moe`. The kernel already implements GLM's clamp
(gate <= L, -L <= up <= L) behind `has_swiglu_limit`, so `patch-swiglu-clamp.py`
restores only the plumbing. It fails closed unless both files have their
reviewed SHA-256. Build it where the nullable-parser tag resolves to its pinned ID:

```bash
docker build --network=none -f Dockerfile.swiglu-clamp \
  -t ramjet/glm53-sm120:swiglu-clamp-r1 .
```

Full GSM8K (1,319, temperature 0, `reasoning_effort=low`, `bench/gsm8k_check.py`):
96.29% before, 96.44% after, 10 fixed and 8 broken questions, same wall time.
732 of 1,319 completions changed length, so the clamp is live. The agent
protocol corpus passes 5/5.

## Prefix-cache capacity

GLM-5.3 is hybrid: resuming a cached prefix needs a saved linear-attention
state as well as its KV. SGLang's `UnifiedRadixCache` stores one state per
6,144-token prefill chunk in a 28-slot pool (`--max-mamba-cache-size`, about
38MB per slot per GPU) and evicts LRU. Running requests hold about four slots
each. Before this change one ~310k-token prompt (about 51 chunks) evicted every
other session's prefix while the 500k-token KV pool was far from full.

- `--mamba-max-states-per-path=2` keeps each path's tail and the state before
  it, plus every fork (for example a shared system prompt). Agent loops resume
  from the tail, so this costs them nothing and doubles session capacity.
- `--enable-hierarchical-cache --hicache-size=4` adds a write-through host tier
  per TP rank: 308,288 KV tokens (1.95GB) and 1.57GB of states. It pins about
  8GB of host memory per engine.
- `--enable-int8-mamba-checkpoint` is rejected: this build wires it only into
  the older `MambaRadixCache`, it is incompatible with HiCache, and its pool is
  carved from headroom that does not exist here.
- The KV token pool stays at 500,000: running requests alone reached 0.99 of it.

Measured on an isolated C (`bench/prefix_eviction_probe.py`, 20k-token sessions):

| probe | before | cap 4 | cap 2 + HiCache |
|---|---|---|---|
| 6 sessions, then one 308k-token prompt | 0/6 cached, 3.35s each | 6/6 (99.6%), 0.15-0.30s | - |
| 12 sessions, cyclic re-query | - | 0/12 | 12/12 (99.2%), 0.15-0.30s |
| 14 sessions, cyclic recall | - | - | 11/14 from host, 0.43s; 14/14 recalled exactly |

c4 decode throughput was unchanged (300 GSM8K questions: 87.2s cap 4, 88.5s
with HiCache). The shared load balancer additionally confines prompts above
600,000 request bytes to C (`RJ_ROUTE_LONG_PROMPT_*`), so B's cache never sees
them.

Roll one replica at a time with `node06-engine-rollout.sh` beneath the thermal
guard while the peer serves. For a candidate, pass a full experiment copy of
this file whose only difference is `networks.default.name`, so the shared load
balancer cannot reach it until it is qualified; then recreate it from this
canonical file.

## Multimodal client contract

Image input is part of this deployment: both replicas launch with
`--enable-multimodal` and the torchvision image processor, and the shared load
balancer passes image content parts through unchanged. Verified 2026-09-16
under a thermal guard (run `98cc4610d771ff28454ab61e3231f262`): a 32x32 PNG
`image_url` data-URI chat completion returned 200 with correct image content
and identical prompt-token counts on the direct engine `127.0.0.1:8062`, the
load balancer's `glm-5.3-flash` alias, and the public Caddy TLS ingress.

Agent harnesses that pre-check model capabilities before sending a request
(opencode/T3 Code) gate attachments on each model's declared modalities. A
client recipe for this deployment must declare the GLM entry multimodal, for
example in `~/.config/opencode/opencode.jsonc`:

```json
"glm-5.3-flash": {
  "modalities": {
    "input": ["text", "image"],
    "output": ["text"]
  }
}
```

A client that still declares `input: ["text"]` refuses image attachments
locally ("this model does not support image input") before any request reaches
node06; that error is a stale client recipe, not an engine capability. Keep
client-side modality declarations in sync with this section.

## Add the second replica

Stage this directory on node06 without changing the existing Compose project,
create a fresh root-owned mode-0700 evidence directory below `.experiments`,
copy `node06-second-replica-rollout.sh` into it, and run only that staged script
under the thermal guard:

```bash
sudo python3 /home/luke/inference/glm53_flash_sm120/node06_gpu_guard.py \
  --label glm53-sm120-second-tp2 \
  --expected-gpus 8 \
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
