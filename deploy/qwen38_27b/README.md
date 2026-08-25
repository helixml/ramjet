# Qwen3.8-27B serving on node06

The deployment is one Compose file. `docker compose up -d` is the whole
command; there is no `-f` list and no overlay. See AGENTS.md "One Compose file
per deployment" for why, and change the file on a branch rather than adding a
second one.

```bash
git -C /prod/src/sglang checkout 38b74d294   # see PINNING CAVEAT in the file
docker compose up -d
```

## Canonical deployment

Eight single-GPU SGLang engines with DFlash2 block-8 speculation, behind the
ramjet load balancer. Target weights are
`RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead` at immutable Hugging Face revision
`009632fef96dd349150baa780c984e62e70e91fe`: a 23.75GB indexed NVFP4/FP8
export that keeps `lm_head` dense in BF16, as the DFlash2 selector requires on
the qualified SGLang pin. The draft remains `z-lab/Qwen3.8-27B-DFlash2`.

Stage the target under the revision-named path used by Compose; never refresh
that directory from mutable `main`:

```bash
hf download RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead \
  --revision 009632fef96dd349150baa780c984e62e70e91fe \
  --local-dir /prod/models/RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead-009632fef96d
```

The RadixArk checkpoint was promoted across all eight engines on 2026-08-25
after a matched isolated canary. The current and retained rollback landmarks
must stay distinct.

Measured on this hardware:

| Gate | Current RadixArk BF16 head |
|---|---:|
| batch-1 greedy decode, matched direct-engine canary | **153.3 tok/s** vs 142.6 Inferact (**+7.5%**) |
| objective answers / deterministic agent protocol | **7/8 and 20/25**, equal to Inferact |
| concurrent slots | **208 (26 per engine)** |
| KV pool per engine | **582,246 tokens** |
| aggregate c192/max256 | Not yet requalified; former Inferact target: **7,882.6 tok/s** |

The canary reconciled 25/25 native SGLang requests and exact server/client
generation-token totals on both targets, with DFlash verification active. It
is evidence of no regression on the committed corpus and a local decode gain,
not a general proof of the upstream accuracy claim. See the 2026-08-25 entry in
`EXPERIMENTS.md` for the immutable identities and evidence paths.

Two settings in the command carry most of that and both have journal entries:

* `--mamba-ssm-dtype=bfloat16` halves the linear-attention state, which is what
  actually caps concurrency on this hybrid model — 12 running per engine
  becomes 25, and the fleet's aggregate ceiling roughly doubled. Gated against
  long-context recall to 199,482 tokens and multi-turn tool sessions with no
  regression (EXPERIMENTS.md 2026-08-23). Do **not** reach for
  `--max-mamba-cache-size` instead: raising it alone collapsed the KV pool from
  349,284 to 45,033 tokens.
* `--enable-torch-compile` lifted the former Inferact target's greedy batch-1
  median +12% (169.3 tok/s measured through the LB fleet-wide) and 82K-deep
  decode +10-15%, neutral elsewhere. A fresh BF16-head graph capture took
  688–823s per engine during the production rollout, so roll exactly one
  engine at a time and watch the first capture. It can need a retry at an auto
  memory fraction, which is why `--mem-fraction-static` is pinned in the
  command.

Headline numbers published for this stack elsewhere are best-case cells, not
medians: with block 16 + FP8 KV this box measured a 334.7 tok/s best greedy
code cell against a 149 median (EXPERIMENTS.md 2026-08-22), and the 300-450+
claims additionally need either HBM-class bandwidth (H200) or a
workstation-SKU memory overclock plus the original fully-W4A4 target — the
Server Edition boards here lock memory offsets in vBIOS, and that export's
quantized `lm_head` is rejected by the DFlash2 selector. The current RadixArk
export instead keeps `lm_head` in BF16. FP8 KV (`--kv-cache-dtype
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
