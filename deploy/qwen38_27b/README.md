# Qwen3.8-27B serving recipes

Two qualified ways to serve Qwen3.8-27B behind the ramjet load balancer on an
8x RTX PRO 6000 Blackwell box. Both advertise the same model name
(`qwen3.8-27b`), so clients do not change when the engine underneath does.

## Recipe 1 — vLLM, FP8 weights, MTP speculation (the full-feature stack)

The base file plus a generated topology. `render_topology.py` produces the
overlay for any (engines x tensor-parallel) split; see `docs/models.md` for
the selection table.

```bash
./render_topology.py --gpus 8 --tensor-parallel 2 -o topology.8gpu-tp2.yaml
docker compose -f docker-compose.yaml -f topology.8gpu-tp2.yaml \
  -f machineview.override.yaml up -d
```

Weights: `Qwen/Qwen3.8-27B-FP8` (~28GiB). The checkpoint ships its own MTP
draft head, enabled in the topology files
(`--speculative-config={"method":"mtp","num_speculative_tokens":2}`).

This is the recipe that supports the whole ramjet feature surface: vLLM
KV-event streams (`RJ_KV_EVENT_MODE=shadow`, the digest index, the snapshot
companion), the idle-drain sleep actuator (`topology.8gpu-tp2.sleep.yaml`),
DSpark guards, and the identity/attestation stack.

Measured single-stream class on this hardware (EXPERIMENTS.md 2026-08-14):
~77 tok/s base, ~120 tok/s with MTP; aggregate ~7,800 tok/s at c256.

## Recipe 2 — SGLang, NVFP4 weights, DFlash2 speculation (fastest single-stream)

One overlay, eight single-GPU engines:

```bash
git -C /prod/src/sglang checkout 38b74d294   # see PINNING CAVEAT in the overlay
docker compose -f docker-compose.yaml -f topology.8gpu-sglang-dflash2.yaml \
  -f machineview.override.yaml up -d
```

Weights: `Inferact/Qwen3.8-27B-NVFP4` (24.2GB resident; keeps `lm_head`
dense, which the DFlash2 selector requires) plus the
`z-lab/Qwen3.8-27B-DFlash2` block-diffusion draft.

Measured batch-1 decode on this hardware (EXPERIMENTS.md 2026-08-20):
57.4 tok/s without speculation, 147-218 tok/s with DFlash2 block 8 —
roughly 1.5-2x the vLLM recipe where users feel it. The 300+ tok/s numbers
published for this stack need HBM-class memory bandwidth (H200); they do not
transfer to GDDR7 cards.

What this recipe gives up (details in the overlay header): vLLM KV events
(forced `off`), the sleep actuator (observe-only idle drain), and FP8 -> NVFP4
quantization headroom. Speculative decoding also inverts at high concurrency
(the MTP data shows +112% at c8 becoming -12.5% at c256); qualify aggregate
capacity for your own traffic before assuming both wins at once.

Single-GPU engines also pay for **cold long-context prefill**: a real Helix
agent session's 196K-token first turn measured 56.8s TTFT (about 3.5K tok/s
at that depth; ~8.9K tok/s at 48K), where the TP2 recipe spreads prefill
over two GPUs. Prefix-cache affinity makes the following turns cheap — the
same session's 200K-token turns 2 and 3 measured 4.1s and 2.6s TTFT — so
this hurts once per session, not per message. Raising
`--chunked-prefill-size` to 32768 was measured *slower* (7.5-8.3K vs 8.9K
tok/s); the default 8192 stands.

## Switching between recipes

The LB is stateless and the engines keep nothing worth preserving across a
planned swap, but a Compose service is defined by every `-f` file it was
created from. Read the active file list from the running container and follow
the render-diff discipline in AGENTS.md before any recreate:

```bash
docker inspect ds4-loadbalancer \
  --format '{{index .Config.Labels "com.docker.compose.project.config_files"}}'
```

For a zero-downtime switch on a shared box: bring the new engines up on free
GPUs first, recreate only the LB with `RJ_UPSTREAM` pointing at the healthy
subset, then convert the remaining GPUs and recreate the LB again with the
full default list. Engines from the two recipes must not share a GPU.
