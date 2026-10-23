# GLM-5.3-Flash TP2 kernel experiments (node06)

Harness used for the 2026-10-22 entries in `EXPERIMENTS.md`. These Compose
files start an **isolated** replica (`glm53-kexp`, GPUs 0-1, `127.0.0.1:8065`,
own network, never attached to the LB) from production's flags. They are an
experiment harness, not deployments; the deployment is
`deploy/glm53_flash_sm120/docker-compose.yaml`.

- `docker-compose.yaml` + `entry.sh`: v0.4.3 serving image; `entry.sh` applies
  `sglang-patches/` (chunked PCIe IPC all-reduce, TileLang W8A16 GEMM) before
  `sglang serve`. Every patch is gated by an env var (`KEXP_*`).
- `v061/`: ormandj v0.6.1 candidate, the chunked-IPC port
  (`patch-pcie-ipc-chunked-v061.py`, TP2-only), and `diag/sitecustomize.py`
  (times large all-reduces on the engine's tensor vs a fresh copy).
- `kbench.py`: direct-engine client. Decode c1/c4 reports `ms_per_iteration` and
  `tokens_per_verify` from SGLang counters, because tok/s moves with
  speculative acceptance. Prefill is cold and salted. Run it under
  `node06_gpu_guard.py`.
- `trace_*.py`: torch-profiler trace analysis (kernel categories, launch
  shapes, overlap, memcpys, cross-rank skew).
- `micro/`: all-reduce / P2P microbenchmarks, including the NCCL SHM NUMA
  reproduction (`ar_enginelike.py`).
- `dev/`: TileLang W8A16 kernel development and the cold-weight GEMM shootout.

GPUs 0-3 belong to `glm53-unc-fp8`; stop it only with the operator's OK and
restart it afterwards (~21 min).
