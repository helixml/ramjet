# GLM-5.3-Flash FP8 on 8x H200

One-file deployment of `zai-org/GLM-5.3-Flash` (the official MIT FP8 E4M3
checkpoint, revision `eb9eb208eb0d988989d07a6a12d0fdeb5f52574a`, 306GiB) on
one 8x H200 host (Hopper SM90, 141GB per GPU, full NVLink): two TP4/EP4
SGLang v0.5.20 replicas behind ramjet. This is a test and measurement
deployment, not a production owner; nothing here touches node06.

| service | GPUs | port | role |
|---|---|---|---|
| `glm53-a` | 0-3 | `127.0.0.1:8070` | TP4/EP4 replica |
| `glm53-b` | 4-7 | `127.0.0.1:8071` | TP4/EP4 replica |
| `ramjet` | — | `127.0.0.1:8006` API, `:8007` metrics | prefix routing, `RJ_ROUTE_AFFINITY_BASIS=marginal` |
| `glm53-tp8` (profile `tp8`) | 0-7 | `127.0.0.1:8072` | single-engine control; never beside A/B |

```bash
docker compose up -d                                   # 2x TP4 + ramjet
docker compose stop glm53-a glm53-b && \
  GLM_MAX_RUNNING=128 docker compose --profile tp8 up -d glm53-tp8   # TP8 control
python3 validate-compose.py
```

`MODEL_DIR` and `CACHE_ROOT` default below `$HOME`; the runner scripts use
`GLM_H200_ROOT` (default `$HOME`) for `models/`, `engine-cache/`, and
`results/`. Keep the per-replica JIT caches on persistent disk: a warm restart
is ~130-210s against ~460s cold.

## Engine configuration and why

- **BF16 KV** (`GLM_KV_DTYPE=bfloat16`) with TileLang DSA prefill/decode and
  `deep_gemm` MoE: FP8 KV is not a valid SM90 combination for this NoPE sparse
  MLA. Each TP4 replica gets a 2.05M-token KV pool and 633 KDA state slots
  (TP8: 3.70M and 2,295); weights take 75GB per GPU at TP4.
- **EAGLE MTP 3/1/4** (`GLM_SPEC_ARGS`, set `" "` to disable). Direct 8K/1K
  cells against identical prompts: +56% output at c1, +31% at c16, +15% at
  c64. On the agent swarm it adds 9% turns/min and cuts turn e2e p90/p99 by
  about 20%, but roughly doubles TTFT and costs 12% of KV (1.80M tokens) and
  half the KDA slots (331). Agents act on the whole tool call, so e2e wins.
- **HiCache, 48GB per rank** (`GLM_HICACHE_ARGS`, set `" "` to disable). At 96
  developers the working set outgrows the device pools; the host tier restored
  223 vs 150 turns/min and cut TTFT p90 from 17.6s to 11.8s. It passed the
  tool-call smoke and the 5-case agent protocol corpus on both replicas.
- **4 tokenizer workers with a 60s uvicorn worker health check.** Tokenizing a
  100k-token agent prompt costs ~0.2s of event-loop CPU, so one tokenizer
  process saturates: at 48 developers per replica, 1 worker gave 108 turns/min
  and TTFT p90 15.3s; 4 workers gave 212 and 3.2s. With the default 10s health
  check uvicorn killed busy workers and dropped their requests; 60s removed
  every kill. 8 workers measured the same as 4. A multi-worker start once hung
  in SGLang's own warm-up (503 until restart), so bound readiness waits.
- **`/health` does not generate** (`SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION=0`).
  By default it pushes a token through the scheduler, which under load exceeds
  ramjet's 5s probe and marks a healthy replica down. The cost: a hung
  scheduler now still answers 200. After a GPU fault (Xid 94) hung replica A,
  ramjet kept routing its sessions there until the client timed out. Watch for
  that until ramjet has a stall signal of its own.
- **NUMA binding** (`cap_add: [SYS_NICE]`, `--numa-node`). The VM exposes two
  sockets as eight NUMA nodes, GPU *i* on node *i*. Without `SYS_NICE` SGLang
  silently skipped its bind and each replica's ~48GB-per-rank pinned host tier
  landed on the opposite socket; now every rank's CPUs and memory are local.
  This is a placement fix; no throughput change was measurable past the cliff.
  The exception is rank 0, which binds to node 1. On this VM, GPU DMA against
  NUMA node 0's memory raises contained errors (Xid 94, then CUDA
  `unspecified launch failure`) under sustained host copies, on any GPU. A
  plain pinned-memory copy loop reproduces it in about 40s against node 0 and
  runs clean against nodes 1-7. HiCache's pinned host tier is the only serving
  path that copies enough to hit it. The fault is in the host, not the GPUs;
  drop the remap once the host is fixed.
- **Rejected, measured:** `--enable-mixed-chunk` OOM-crashed a replica under
  the swarm (sparse-attention indexer top-k buffer, 30MiB free at 0.88);
  `--speculative-adaptive` with SGLang's default table fails graph capture at
  0.88, and a {0,1,3}-step table measured no gain over fixed 3/1/4.

## Routing

`RJ_ROUTE_AFFINITY_BASIS=marginal` (#294). Harness system prompts larger than
the 64KiB affinity cap made every replica score full affinity under the
`absolute` basis, so agent sessions hopped replicas and re-prefilled history.
The engines publish no KV events, so tokenizer, exact, KV-event, and snapshot
routing stay off.

## Measuring

`serving-cell.sh` runs one `sglang.bench_serving` cell. `swarm-cell.sh` runs
the open-loop coding-agent fleet (`bench/agent_swarm_bench.py`) through
ramjet; `ab-sequence.sh` repeats it across routing configurations with the
LB recreated between arms; `pair-swarm.sh` runs one swarm per replica in
parallel for engine-flag comparisons (swap the candidate between replicas for
a second round); `direct-ab.sh` does the same with fixed-shape cells;
`capacity.sh` sweeps developer counts. Keep `--seed` fixed and the salt fresh
across the arms of one comparison.

The remote shell used during qualification was zsh, which does not
word-split `$var`; keep loops in these bash scripts rather than inline.
