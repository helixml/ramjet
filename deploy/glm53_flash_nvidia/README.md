# GLM-5.3-Flash NVFP4 on the official vLLM runtime

One-file node06 deployment for two NUMA-local TP4 vLLM engines behind ramjet,
serving NVIDIA's ModelOpt NVFP4 quantisation of GLM-5.3-Flash. This is the
whole deployment; do not add an overlay.

**Status: unqualified admission artefact.** No GPU has run this configuration.
Every gate below is GPU-free and has passed; nothing beyond them has been
measured. Do not treat a green preflight as permission to serve traffic.

## Why this exists beside `deploy/glm53_flash`

`deploy/glm53_flash` serves the same model family from `LibertAIDAI`'s NVFP4
checkpoint on a locally built SGLang image patched from a third-party
repository that carries no detected licence. That recipe is explicitly not
promotable and must not be pushed to a registry, so its 2026-08-27
qualification can never become production.

This deployment removes that blocker. Both halves are first-party and already
present on the box:

- NVIDIA's own checkpoint, published under MIT from `zai-org/GLM-5.3-Flash`.
- The exact immutable `vllm/vllm-openai` digest already qualified and running
  for the NVIDIA Qwen3.8-Flash-Next NVFP4 deployment. No new image, no patched
  source, no local build.

The two deployments bind the same load-balancer ports, so only one may own
node06 at a time. Neither supersedes the other until this one is qualified.

## Immutable inputs

- Model: `nvidia/GLM-5.3-Flash-NVFP4` at
  `423acf37583782c51c142d145aef733d72943d93`. 33 shards, 204,439,103,396
  tensor bytes, 190.4GiB on disk.
- Engine image: `vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93`.
  It bundles `vllm.models.glm5next` (model, MTP, KDA, MLA+indexer, multimodal)
  and transformers 5.16.1, which is exactly the model card's stated minimum.
- Load balancer: `ghcr.io/helixml/ramjet:v0.5.0@sha256:c3fc5723a0dba51f9bb8eced77648cf0b05788039e90fc638fbd8c19adec70d8`.

## What the checkpoint is, and why it fits

320B total parameters with 18B active: 288 routed experts plus one shared
expert, top-8 routing, 45 layers. The attention stack is hybrid — 11 full
attention layers (MLA with a DeepSeek-style sparse indexer, `kv_lora_rank`
512) interleaved with 34 KDA linear-attention layers — plus
Manifold-Constrained Hyper-Connections and one MTP layer. It is natively
multimodal and declares 1,048,576 positions.

NVFP4 covers the weights and activations of transformer-block linear
operators at group size 16, with an FP8 KV cache. `lm_head`, the embeddings,
`self_attn`, many `shared_experts`/`mlp.gate` tensors, and the entire vision
tower stay in higher precision.

On node06's eight RTX PRO 6000 Blackwell Server Edition cards (97,887MiB
each), TP4 puts **47.6GiB of weights on each GPU**, leaving roughly 38GiB per
GPU for KV, KDA state, activations and graphs at `--gpu-memory-utilization
0.90`. Both TP4 engines resident is about 381GiB of the box's 764GiB.

The KV cache is unusually cheap for the context length because only 11 layers
cache per token, and they cache an MLA latent at FP8. The 34 KDA layers hold a
constant per-sequence state instead, which vLLM promotes to float32 for its
accelerated GDN backend. This is a structural observation from the config, not
a measurement: the real allocation comes from the engine's own profiling run
and must be read from live `cache_config_info`.

## GPU-free gates

Run all three before any container is created. None needs a GPU, and the first
two need no weights at all.

```bash
python3 deploy/glm53_flash_nvidia/validate-compose.py
python3 -m unittest bench.test_glm53_nvidia_compose \
  bench.test_glm53_nvidia_model_verify bench.test_glm53_nvidia_docs
```

After the checkpoint is on disk at the pinned revision:

```bash
python3 deploy/glm53_flash_nvidia/verify-model.py \
  /prod/models/nvidia/GLM-5.3-Flash-NVFP4-423acf37583782c51c142d145aef733d72943d93
```

The verifier checks the eight pinned metadata digests, the exact 33-shard index
and on-disk sets, absence of partial downloads, regular-file shape, and the
exact aggregate tensor byte count. The Hugging Face client remains responsible
for transport-level verification of each downloaded object.

Then admit the exact rendered argv through the pinned image's own CLI:

```bash
docker compose -f deploy/glm53_flash_nvidia/docker-compose.yaml config --format json \
  | jq -c '.services["glm53nvidia-a"].command' > "$experiment/candidate-argv.json"
install -m 0644 deploy/glm53_flash_nvidia/args-preflight.py "$experiment/"

docker run --rm --network none \
  -v /prod/models/nvidia/GLM-5.3-Flash-NVFP4-423acf37583782c51c142d145aef733d72943d93:/workspace/model:ro \
  -v "$experiment":/probe:ro \
  --entrypoint python3 \
  vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93 \
  /probe/args-preflight.py /probe/candidate-argv.json
```

This parses the argv through the image's real CLI and builds the complete
engine config, so an unsupported architecture, an unavailable parser, a
rejected flag combination, or a silently re-decided quantisation method fails
in about 33 seconds instead of inside a multi-minute load on eight shared GPUs.
`/workspace/model` needs only `config.json`, the safetensors index and the
tokenizer, so this runs before the 190GiB download finishes. It never builds an
engine or a model, so the live smoke stays mandatory.

## Canary rollout

Download the exact revision outside any benchmark timing, verify it, then hold
the common deployment lock and start **one** named engine:

```bash
flock --nonblock /run/lock/ramjet-node06-deployment.lock \
  docker compose -f docker-compose.yaml up -d --no-deps glm53nvidia-b
```

Model load, JIT and graph capture are GPU work that no request-process wrapper
covers. Isolate the one TP4 pair, watch intake air, GPU telemetry and driver
errors throughout, and start only at 46C intake or below. Every
request-generating command afterwards must be a child of
`bench/node06_gpu_guard.py`.

The checked-in defaults deliberately reduce the card's 1M context and its
recommended concurrency to a 262K, four-sequence, MTP-off loader canary. That
minimises the first GPU exposure; it is not a performance comparison. After the
canary passes, change one environment value at a time and journal each:

1. raise `MAX_NUM_SEQS` from 4 and re-read live `cache_config_info`;
2. flip `RJ_KV_EVENT_MODE` to `shadow` — a load-balancer-only recreate, because
   the engines already publish;
3. only then consider context above 262K, or MTP.

## What stays off, and why

| Setting | State | Why |
|---|---|---|
| `RJ_TOKENIZER_MODE` | `off` | No GLM profile is registered in `src/model`. Local rendering, and every authority derived from it, is unavailable by construction. |
| `RJ_EXACT_ROUTE_MODE` | `off` | Needs a renderer profile and an attested compatibility manifest. |
| `RJ_KV_EVENT_MODE` | `off` | The engines publish, but consuming inventory over a hybrid KDA/MLA allocator is unqualified. Promote to `shadow` with a recorded comparison, never straight to placement. |
| `RJ_SNAPSHOT_ROUTE_MODE` | `off` | Depends on the same unqualified inventory. |
| `RJ_IDLE_DRAIN_MODE` | `off` | Sleep-mode parking is unmeasured for this runtime and model. |
| `--speculative-config` | absent | MTP stays off on SM120 until qualified on its own, matching the Qwen posture. |
| `--trust-remote-code` | absent | The image implements this checkpoint natively; the preflight asserts remote code is not needed. |
| `--limit-mm-per-prompt` | `{"image":0,"video":0}` | The checkpoint is multimodal but ramjet's request path and tokenizer shadow are text-only, so image-token accounting would be wrong. |

Approximate prefix and load routing, health, cancellation, metrics and the
decision journal remain live. That is the whole of ramjet's contribution here
until the inventory path is qualified.

## Open questions this deployment does not answer

- Throughput and TTFT. Nothing has been measured; 18B active parameters
  predicts nothing on its own.
- Whether prefix-cache accounting behaves usefully over the hybrid allocator,
  which decides whether ramjet's affinity routing is worth anything for this
  model. `RJ_ROUTE_KV_CAPACITY_TOKENS` cannot be pinned until it is read from a
  live engine.
- Agent-protocol correctness. `glm47` is the registered parser whose
  `<tool_call>`/`<arg_key>`/`<arg_value>` and `<think>` markers match this
  checkpoint's chat template exactly, but a marker match is not a
  qualification: `bench/agentbench.py` decides, and the sibling SGLang recipe
  already found this model family violating a nullable tool-argument contract.
- Host-memory peak during a 190GiB load on a box whose RAM is tight.
