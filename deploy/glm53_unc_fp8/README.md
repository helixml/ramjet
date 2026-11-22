# Uncensored GLM-5.3-Flash TP4 replica (node06 GPUs 0-3)

`glm53-unc-fp8` serves `glm-5.3-flash-uncensored` behind the `qwen38_glm53_kev`
Ramjet, beside the stock TP2 replicas from `deploy/glm53_flash_sm120`. It is the
only replica of this model, so a restart takes the model offline for the whole
cold start (21-22 minutes) and drops its cached prefixes. Pick a quiet window.

Immutable inputs:

- Model: `orcarouter/GLM-5.3-Flash-Uncensored-FP8` at revision
  `0480f690f80e8f4c4ee63edd10dcbea8b7d9e31b`.
- Runtime: `ramjet/glm53-sm120@sha256:899fe8eb…` (ormandj SGLang v0.4.3,
  source revision `386684975edf3cbce15c4b12df37908366e2aa8b`), loaded locally
  on node06 (`pull_policy: never`).

node06 runs this file from `/home/luke/inference/glm53-unc-fp8`. Edit it here,
copy it over, and compare SHA-256 before recreating:

```bash
scp deploy/glm53_unc_fp8/docker-compose.yaml \
  node06:/home/luke/inference/glm53-unc-fp8/docker-compose.yaml
ssh node06 'cd /home/luke/inference/glm53-unc-fp8 \
  && flock --nonblock /run/lock/ramjet-node06-deployment.lock \
    docker compose up -d glm53-unc-fp8'
```

The LB is not touched: it keeps `glm53-unc-fp8` in its upstream list and
marks it up again once `/health` answers.

## All-reduce: NCCL P2P, not chunked IPC

The four GPUs meet only at a PCIe host bridge (`nvidia-smi topo`: NODE), so
NCCL picks its host-memory SHM transport, and SGLang forces
`NCCL_CUMEM_ENABLE=0`, which puts those buffers on whichever NUMA node the page
cache leaves free. The TP2 replicas fix this with chunked PCIe IPC. At four
ranks NCCL P2P wins instead (`bench/glm53_kexp/micro/ar_tp4.py`, 4,096-wide
BF16):

| All-reduce, four ranks | 24 rows | 6,144 rows |
| --- | ---: | ---: |
| NCCL SHM, cuMem off (before) | 146 µs | 6.8-20.1 ms |
| NCCL SHM, cuMem on | — | 6.8 ms |
| FlashInfer IPC, chunked | 53 µs | 4.3 ms |
| **NCCL `P2P_LEVEL=SYS`, cuMem on** | **29 µs** | **2.4 ms** |

So the file sets `NCCL_CUMEM_ENABLE=1` and `NCCL_P2P_LEVEL=SYS`, and lowers
`SGLANG_PCIE_IPC_MAX_NUMEL` to 16,384 (4 rows) so IPC keeps only the MTP draft
steps. The KV pool stayed at 499,968 tokens.

| Measured 2026-11-22 (direct, uncached) | Before | After |
| --- | ---: | ---: |
| Prefill, 9.3k-token prompt | 6,204 tok/s | 9,885 tok/s |
| Prefill, 37k-token prompt | 6,161 tok/s | 10,181 tok/s |
| Prefill, 149k-token prompt | — | 10,116 tok/s |
| Decode, c4 aggregate | 388 tok/s | 415 tok/s |
| Decode, c1 | 198.5 tok/s (n=1) | 195.5 tok/s |

If a later interleaved A/B confirms a c1 decode loss, raise
`SGLANG_PCIE_IPC_MAX_NUMEL` to 24,576 so 6-row verify calls return to IPC.
Full entry: EXPERIMENTS.md 2026-11-22. Rollback is the pre-change file,
kept on node06 as `docker-compose.yaml.bak-20261122-pre-nccl-p2p`.

Do not copy these settings to the TP2 replicas: at two ranks chunked IPC
still beats NCCL P2P (1.46 vs 1.84 ms).
