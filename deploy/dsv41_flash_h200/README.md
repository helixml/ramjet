# DeepSeek-V4.1-Flash on 8x H200

One-file deployment of `deepseek-ai/DeepSeek-V4.1-Flash` (MIT, revision
`2cba9e42aa026125f3ed06c6d98c1db82f7ca027`, 510GB) on one 8x H200 host
(Hopper SM90, 141GB per GPU, full NVLink): two TP4/EP4 SGLang v0.5.21
replicas with DSpark behind ramjet. This is a test and measurement
deployment.

| service | GPUs | port | role |
|---|---|---|---|
| `dsv41-a` | 0-3 | `127.0.0.1:8070` | TP4/EP4 replica |
| `dsv41-b` | 4-7 | `127.0.0.1:8071` | TP4/EP4 replica, or the 1M-context lane |
| `ramjet` | — | `127.0.0.1:8006` API, `:8007` metrics | prefix routing, `RJ_ROUTE_AFFINITY_BASIS=marginal` |
| `dsv41-tp8` (profile `tp8`) | 0-7 | `127.0.0.1:8072` | single engine, Engram in HBM; never beside A/B |

```bash
# host, once per boot: let the shared-memory Engram table use huge pages
echo advise | sudo tee /sys/kernel/mm/transparent_hugepage/shmem_enabled
docker pull lmsysorg/sglang@sha256:b1259f3ea3275f66237c498ea388919729018bc9f01c3d638391e06e2cf3f469
./prewarm.sh                                   # page-cache the weights
docker compose up -d                           # 2x TP4 + ramjet, ~15 min to serve
python3 validate-compose.py
```

`MODEL_DIR` and `CACHE_ROOT` default below `$HOME`; the scripts use
`DS_H200_ROOT` (default `$HOME`) for `models/` and `results/`. Keep the
per-replica JIT caches on persistent disk.

## Measured

ramjet `bench/agent_swarm_bench.py` (seed `swarm-v1`, fresh salt per cell),
300s cells after a 60s warm-up, through ramjet with `marginal` affinity. No
request failed in any cell.

| | 16 devs | 32 devs | 64 devs | 96 devs |
|---|---:|---:|---:|---:|
| agent turns/min | 140.1 | 216.8 | 275.8 | 286.9 |
| prompt tokens from cache | 94% | 93% | 92% | 90% |
| TTFT p50 / p90 (s) | 0.50 / 1.16 | 0.66 / 1.65 | 0.88 / 2.29 | 1.05 / 2.90 |
| turn e2e p50 / p90 (s) | 1.7 / 6.0 | 3.1 / 11.6 | 6.7 / 25.8 | — / 42 |

Decode per stream (natural coding prompts, 1,024 output tokens): 354 tok/s
at one stream, 237 at 8, 189 at 16 (2,632 aggregate). The stock H200 recipe
(TP8, 1M context, no speculation) does 76 tok/s and 62 turns/min at 16
developers. GSM8K is 94.8% against 95.2% on the stock stack (noise), and the
agent protocol corpora are 100% valid on both.

## Engine configuration and why

In the order they mattered:

- **BF16 dense layers** (`patches/patch_bf16_dense.py`,
  `SGLANG_BLOCK_FP8_DEQUANT_BF16=1`). Everything but the experts is FP8 with
  32x32 ue8m0 block scales, which on Hopper only SGLang's Triton block-FP8
  kernel runs; at decode it took half of all GPU time. FP8 times a
  power-of-two scale is exact in BF16, so dequantizing once at load and using
  cuBLAS is lossless. One stream: 234 to 392 tok/s (TP8).
- **DSpark** (`DS_SPEC_ARGS`, block 5, set `" "` to disable): about 3x one
  stream's decode speed. It needs `DS_CONTEXT_LENGTH=262144` on H200; at the
  native 1M the verify graphs OOM in capture (a 12GB `req_to_token` gather in
  the decode indexer).
- **W4A8 MoE** (`DS_MOE_PRECISION=fp8`): FP4 experts with FP8 activations,
  +10% decode under load. Hopper has no FP4 tensor cores.
- **Two TP4 replicas with the Engram tables in host memory**
  (`SGLANG_ENABLE_DSV41_ENGRAM_HOST_TABLE=1`): +23% turns/min at 16 developers
  and +59% at 64 over one TP8 engine. The two 101.5GB tables are shared by a
  replica's ranks, so two replicas cost 378GB of host RAM. With
  `shmem_enabled=advise` they sit on huge pages (+6% at 16 developers). Leave
  host memory for them: systemd-oomd killed unrelated processes while they
  were built.
- **Soft NUMA memory binding** (`patches/patch_numa_preferred.py`,
  `SGLANG_NUMA_MEM_PREFERRED=1`). SGLang's NUMA bind confines a rank's memory
  to one ~110GB node, and the 190GB Engram table is then OOM-killed
  (`CONSTRAINT_MEMORY_POLICY`). Turning NUMA off avoids that but loses ~5%;
  the patch keeps the CPU bind and makes memory a preference.
- **A private `/dev/shm` per replica** (no `ipc: host`). SGLang hands its
  tokenizer workers their channels through `/dev/shm/multi_tokenizer_args_<pid>`.
  Two containers sharing the host's `/dev/shm` sometimes drew the same PID,
  and the second overwrote the first's segment: that replica answered
  `/health` and `/v1/models` for an hour and ran no request.
- **Context length is a decode-speed dial.** The decode indexer scores the
  whole configured window, so `DS_CONTEXT_LENGTH=131072` decodes 10-12%
  faster per stream under concurrency (212 vs 193 tok/s at 16 streams).
  Agents that compact before 128k can take it; 262k is the default.
- **KV is never the limit**: 4.9M tokens per TP4 replica, peak use 22% at 64
  developers, so no host KV tier.
- **`/health` does not generate** and **4 tokenizer workers with a 60s
  worker health check**, for the same reasons as `glm53_flash_h200`.

Thinking is off unless a request sets `reasoning_effort` (`low`, `high` or
`max`); image input costs 201 prompt tokens per 448x448 image.

## Routing

`RJ_ROUTE_AFFINITY_BASIS=marginal`. KV never fills, yet a turn that changes
replica re-reads a ~40k-token conversation. Coding-harness system prompts
exceed the affinity cap, so under `absolute` both replicas score the same
and sessions hop. At 64 developers:

| ramjet routing | turns/min | cache | stayed on replica |
|---|---:|---:|---:|
| least loaded | 222.8 | 88.6% | 50% |
| prefix, `absolute` (default) | 222.7 | 88.5% | 60% |
| prefix, `marginal` | **263.8** | 91.6% | 88% |
| prefix, `relative` | 263.5 | 91.5% | 83% |
| `marginal` + prefix single-flight | 263.6 | 92.0% | 87% |

Use `relative` beyond two replicas. A replica that answers `/health` but
cannot generate is caught by `RJ_UPSTREAM_RANK_PROBE=all` (ramjet after
v0.7.0).

## A 1M-context lane

Replica B can serve the native 1M context with DSpark at a graph cap of 32
and a memory fraction of 0.73 (0.70 leaves a 446k-token pool; 0.82 OOMs in
graph capture):

```bash
DS_B_CONTEXT_LENGTH=1048576 DS_B_MEM_FRACTION=0.73 DS_B_CUDA_GRAPH_MAX_BS=32 \
RJ_ROUTE_LONG_PROMPT_BYTES=1000000 RJ_ROUTE_LONG_PROMPT_UPSTREAMS=-,lane \
docker compose up -d
```

It recalled needles in 463k- and 835k-token prompts (74s and 207s). It
decodes more slowly under concurrency (75 vs 189 tok/s per stream at 16),
and offering it cost 6% of turns/min at 16 developers and ~16% at 64. Keep
short prompts shared with it (`RJ_ROUTE_LONG_PROMPT_SHORT=shared`, the
default): `exclusive` measured 148 turns/min at 64 developers against 231.

## Rejected, measured

| config | result |
|---|---|
| DSpark at the default 1M context | OOM in verify-graph capture |
| DP8 attention + DSpark | refused without `--enable-dp-lm-head`; with it, hangs after weight load |
| `--numa-node` with a hard memory bind and host Engram | rank 0 OOM-killed |
| `--schedule-policy hrrn --chunked-prefill-size 16384` | 260.2 vs 276.2 turns/min at 64 developers, worse TTFT p90 at 16 |
| DeepGEMM MXFP8 for the dense layers | DeepGEMM 0.2.0 has no 1x32 scale path on SM90 |

## Measuring

`swarm-cell.sh` runs one coding-agent swarm through ramjet; keep `--seed`
fixed and the salt fresh across the arms of one comparison. The remote shell
used during qualification was zsh, which does not word-split `$var`; keep
loops in these bash scripts rather than inline.
