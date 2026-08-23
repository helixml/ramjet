# Qwen3.8-27B serving on node06

The deployment is one Compose file. `docker compose up -d` is the whole
command; there is no `-f` list and no overlay. See AGENTS.md "One Compose file
per deployment" for why, and change the file on a branch rather than adding a
second one.

```bash
git -C /prod/src/sglang checkout 38b74d294   # see PINNING CAVEAT in the file
docker compose up -d
```

## What is deployed

Eight single-GPU SGLang engines with DFlash2 block-8 speculation, behind the
ramjet load balancer. Weights: `Inferact/Qwen3.8-27B-NVFP4` (24.2GB resident;
keeps `lm_head` dense, which the DFlash2 selector requires) plus the
`z-lab/Qwen3.8-27B-DFlash2` draft.

Measured on this hardware:

| | |
|---|---:|
| batch-1 decode, DFlash2 block 8 | 147-218 tok/s (57.4 unspeculated) |
| aggregate, c192 | **7,882.6 tok/s** |
| concurrent slots | 200 (25 per engine) |
| KV pool per engine | 342,647 tokens |

Two settings in the command carry most of that and both have journal entries:

* `--mamba-ssm-dtype=bfloat16` halves the linear-attention state, which is what
  actually caps concurrency on this hybrid model — 12 running per engine
  becomes 25, and the fleet's aggregate ceiling roughly doubled. Gated against
  long-context recall to 199,482 tokens and multi-turn tool sessions with no
  regression (EXPERIMENTS.md 2026-08-23). Do **not** reach for
  `--max-mamba-cache-size` instead: raising it alone collapsed the KV pool from
  349,284 to 45,033 tokens.
* `--enable-torch-compile` lifts the greedy batch-1 median +12% (169.3 tok/s
  measured through the LB fleet-wide) and 82K-deep decode +10-15%, neutral
  elsewhere. It costs ~215s of startup per roll rather than ~90s, so roll one
  or two engines at a time, and watch the first CUDA-graph capture — it can
  need a retry at an auto memory fraction, which is why `--mem-fraction-static`
  is pinned in the command.

Headline numbers published for this stack elsewhere are best-case cells, not
medians: with block 16 + FP8 KV this box measured a 334.7 tok/s best greedy
code cell against a 149 median (EXPERIMENTS.md 2026-08-22), and the 300-450+
claims additionally need either HBM-class bandwidth (H200) or a
workstation-SKU memory overclock plus a W4A4 target — the Server Edition
boards here lock memory offsets in vBIOS, and the W4A4 export's quantized
lm_head is rejected by the DFlash2 selector. FP8 KV (`--kv-cache-dtype
fp8_e4m3`) halves KV memory but is a wash-to-loss on speed here; treat it as
capacity headroom, not throughput.

## What this stack gives up

vLLM KV events are forced `off` (SGLang publishes no ZMQ stream), so the digest
index and snapshot companion do not apply. Idle drain is observation-only —
there is no `/sleep` endpoint here. The model also does not emit **parallel
tool calls** on this stack: asked for two, it returns one, and that is the
model rather than the parser (EXPERIMENTS.md 2026-08-23).
The previous vLLM FP8 + MTP recipe and the generated topology overlays were
removed on 2026-08-23. They remain in git history if the comparison is needed;
its measured class was ~120 tok/s single-stream with MTP and ~7,800 tok/s at
c256.

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
