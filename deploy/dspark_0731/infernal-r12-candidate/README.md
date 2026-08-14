# Infernal Invocation r12 — candidate admission artifacts

GPU-free admission material for
`voipmonitor/vllm:infernal-invocation-vllmdc2934e-b12xd48c62b-fi1ac6942-cu133-torch213-20260814-r12`
(`sha256:7bb6994afe2b9b2307afb87f926ffe2fdc938254dc98f45692f836bc85654849`).

Upstream notes: [ds4dspark-infernal-invocation-r12.md](https://github.com/local-inference-lab/rtx6kpro/blob/master/models/ds4dspark-infernal-invocation-r12.md).

**Nothing here has run on node06.** These are admission artifacts only, produced
entirely from registry metadata while the cooling moratorium is active.

## What r12 actually changes

r12 is **r11 plus exactly one vLLM PR**. Verified from the images' own
`local-inference.*.integration.prs` labels rather than from the release notes:

| Tree | r11 → r12 |
| --- | --- |
| vLLM PRs | **+1**: `308@053e6351d0b3b3e35c969c9e3933db64d30a7164`. Nothing removed. |
| b12x PRs | **identical** (`145,146,148,149,150`), though the tree/commit was rebuilt |
| LMCache | unchanged (`5fdf59cfa1` in both cache fingerprints) |

PR #308 records and zeroes recycled KV blocks for heterogeneous attention-cache
specifications, with bounded CUDA launch geometry. It is a **correctness fix
for token contamination across long-context requests**, not a throughput
change. The upstream notes state plainly that "performance was not swept".

Everything else is inert:

- **Environment: 0 added, 0 removed, 22 changed** — and all 22 are the versioned
  cache-path fingerprint moving from `vllm908522a320-b12x5d648d944a` to
  `vllmdc2934ef69-b12xd48c62bbbd`. No functional setting differs.
- **Versions unchanged**: CUDA 13.3, Torch 2.13.0, NCCL 2.31.2, FlashInfer
  0.6.18+cu133, InstantTensor 0.1.9, LMCache 0.5.2+glm52dcp.5, base image
  `kimi-k3-cu133-torch213-nccl2312-20260811-r2`.
- `CUTLASS_DSL_VERSION` is `4.6.2` on **both** sides, so the r4→r11
  effective-environment inconsistency recorded in AGENTS.md does not recur.
- **Transfer cost**: 18 candidate-only blobs, 2.47 GiB compressed, against 61
  shared blobs / 11.27 GiB. Keep r11 present so Docker reuses them.

## The blocker: r12 does not qualify TP4

The upstream notes qualify r12 on **TP2/DCP1 across two GPUs** and state
explicitly that r12 "does not qualify GLM-5.2, **TP4**, or alternate DeepSeek
checkpoints". Their reference compose defaults reflect that, and differ from our
production contract in ways that are not incidental:

| Setting | upstream r12 default | node06 production |
| --- | --- | --- |
| `TP_SIZE` | **2** | **4** |
| GPUs | `0,1` | `0-3` and `4-7` (two TP4 pairs) |
| `BACKEND` | `b12x-a8` | `b12x-a16` |
| `ALLREDUCE_MODE` | `auto` | `nccl` |
| `MAX_MODEL_LEN` | 131072 | 393216 |
| `MAX_NUM_BATCHED_TOKENS` | 8192 | 4096 |
| `GRAPH` | `auto` | `96` |
| networking | `network_mode: host`, `ipc: host`, `gpus: all` | bridge, explicit `device_ids` |

So the whole serving stack runs a topology r12's own qualification does not
cover. That is the decision to make before any pull, not after: the delta is one
correctness PR, and the cost is exercising an unqualified tensor-parallel width
on the production box.

The overlay here keeps our contract explicit — TP4, GPUs 4-7, port 8013, the r4
launcher settings, and its own JIT cache path — precisely so that a comparison
measures the engine version and not a settings change.

## GPU-free checks (safe now)

```bash
python3 bench/infernal_registry_candidate.py \
  --manifest deploy/dspark_0731/infernal-r12-candidate/manifest.json
python3 deploy/dspark_0731/infernal-r12-candidate/validate-compose.py
```

The first performs registry reads only — no layer downloads — and took 2.6s
warm. The second renders base+overlay and proves the LB is single-homed on
engine A, engine A is untouched, and the candidate stays on GPUs 4-7 at port
8013.

## What still needs a supervised window

Everything below requires the cooling moratorium to be lifted, and a sustained
run additionally has to survive the ~15-20s c24 thermal ceiling recorded in
EXPERIMENTS.md:

1. Pull r12 once, outside benchmark timing, with r11 still present.
2. `bench/serving_runtime_image_probe.py --validate-engine-args` against this
   overlay — the pinned image's own CLI must accept our TP4 argv. **This is the
   first place the TP4 gap would surface**, and it is GPU-free once the image is
   local.
3. A `serving-runtime.json` receipt generated from the real launcher, as r11 has.
4. `candidate_gate.py` with a new `infernal-r12-b` profile through `smoke`, then
   `scout`, then `matrix` — the gate is pinned to exact committed admission
   bytes, so it needs that profile added and reviewed first.
