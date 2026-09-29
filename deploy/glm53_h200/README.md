# GLM-5.3 (753B FP8) on 8x H200

One-file deployment of `zai-org/GLM-5.3` (FP8 E4M3, revision
`aca966e4e02791568aa6a4ced368624b3d897f42`, 755.7GB) on one 8x H200 host
(Hopper SM90, 141GB per GPU, full NVLink). One SGLang v0.5.20 engine owns all
eight GPUs; ramjet routes to its eight data-parallel attention ranks. This is
a test and measurement deployment, not a production owner; nothing here
touches node06. The measurements are in `EXPERIMENTS.md`, 2026-09-29.

| service | GPUs | port | role |
|---|---|---|---|
| `glm53` | 0-7 | `127.0.0.1:8073` | TP8, DP8 attention, DeepEP EP8 |
| `ramjet` | — | `127.0.0.1:8006` API, `:8007` metrics | one upstream per DP rank, `RJ_ROUTE_AFFINITY_BASIS=relative` |

```bash
docker compose up -d
python3 validate-compose.py
```

`MODEL_DIR` and `CACHE_ROOT` default below `$HOME`. Keep the JIT cache on
persistent disk; measured starts took 7-17 minutes.

## Engine configuration and why

- **DP8 attention, FP8 KV** (`--enable-dp-attention --dp-size 8`,
  `--kv-cache-dtype fp8_e4m3`, FlashMLA `flashmla_sparse_q8`/`flashmla_kv`
  DSA backends). TP8 replicates MLA's latent cache on every GPU and leaves
  353k tokens after 94GB of weights per GPU; it collapsed to 29.6 turns/min at
  64 developers. DP8 gives each rank its own pool, 1.55M tokens in total. FP8
  KV is valid for this model on SM90, unlike GLM-5.3-Flash.
- **ramjet, one upstream per rank** (`RJ_UPSTREAM` repeated eight times with
  `RJ_UPSTREAM_DP_RANKS=0,...,7`). Left to itself, SGLang spreads a session's
  turns over ranks whose caches are private: 43.0 turns/min and 65.6% cached
  at 16 developers. Pinning each session to one rank gave 72.7 and 92.8%.
  `relative` affinity keeps sessions sticky however many ranks are listed.
- **Host KV tier, 32GB per rank** (`GLM_HICACHE_ARGS`; `" "` disables).
  Doubled the overflowing 64-developer cell (95.1 vs 45.9 turns/min, 91.8% vs
  71.8% cached). An 86k-token context reloads from host in ~1s against 11-16s
  cold, and it passed a recall + tool-call gate on host-reloaded prefixes.
- **DeepEP** (`GLM_MOE_A2A_BACKEND=deepep`). 17% more turns/min than the
  all-gather MoE path at 16 developers on the same VM; its 7-12% lead at
  32-48 is within the cross-VM spread. `GLM_MOE_A2A_BACKEND=none` gains 22%
  KV per rank and has the lower TTFT tail from 48 developers up (see the load
  curve). It was measured with `SGLANG_DP_USE_GATHERV=1`, the
  `SGLANG_ENABLE_DSA_Q8KV8_*` switches, `--enable-nccl-nvls` and
  `--cuda-graph-max-bs-decode 32`.
- **EAGLE MTP 1/1/2, mem 0.85, 32k prefill chunks, 128 running requests
  (16 per rank), `hrrn` scheduling** — the published H200 recipe. 0.88 with
  64k chunks OOM-crashed under load.
- **4 tokenizer workers, 60s worker health check, non-generating `/health`**,
  for the same reasons as `deploy/glm53_flash_h200`. Because `/health` no
  longer proves a rank can generate, ramjet's `RJ_UPSTREAM_RANK_PROBE=on`
  sends a one-token generation pinned to each rank. It was not active during
  the measured cells.
- **NUMA binding with rank 0 on node 1** (`cap_add: [SYS_NICE]`,
  `--numa-node 1 1 2 3 4 5 6 7`). On this VM, GPU DMA into NUMA node 0's
  sub-4GiB window faults with Xid 94: ACS is disabled on the host's PCIe
  switch ports and the guest has no vIOMMU, so those addresses are routed
  peer-to-peer inside the switch. HiCache's pinned host pool is the only
  serving path that copies enough to hit it. Node 1 then holds two ranks'
  host pools, which is why the tier is 32GB rather than 48GB per rank. Drop
  the remap only after the host passes a node-0 pinned-copy test whose buffer
  provably covers PFN `0x90000-0xBFFFF`.
- **Rejected, measured:** an IMEX fabric channel (no change at TP8); vLLM
  v0.30.0 with decode context parallelism (a single 4.8M-token pool and the
  tightest 64-developer tail, but saturated earlier); the SGLang DCP patch
  stack (needs a newer SGLang than v0.5.20).

## Load curve

Agent swarm through ramjet, HiCache on; TTFT p50/p90 in seconds.

| developers | DeepEP (default) turns/min | TTFT | `GLM_MOE_A2A_BACKEND=none` turns/min | TTFT |
|---|---|---|---|---|
| 16 | 73.2 | 0.84 / 1.7 | 62.7 | 1.06 / 2.0 |
| 32 | 98.2† | 1.27 / 7.0 | 87.6 | 1.45 / 4.9 |
| 48 | 108.5† | 2.77 / 17.5 | 101.2 | 1.69 / 10.5 |
| 64 | 95.1 / 104.6† | 11.4 / 46.1 | 101.3 | 6.6 / 30.8 |
| 96 | 101.7 | 18.6 / 80 | 106.3 | 14.4 / 60 |

† Measured on a second VM of the same type. It matched the first within 2.5%
at 16 developers but ran 10% faster at 64, so the 32-48 developer gap
between the two columns is not resolved. DeepEP's light-load lead is. Cache
hit was 91-93% in every cell, and no request failed.

Size admission at 32-48 developers per node: past that, throughput is flat
and only latency grows.

## Measuring

`bench/agent_swarm_bench.py` against `127.0.0.1:8006` with `--developers`,
`--duration` and `--warmup`, fixed `--seed` and a fresh salt per cell.
