# GLM-5.3-Flash on node06: FP8 and NVFP4 deployment study

Status: deployment qualification in progress, researched 2026-08-26 and
rechecked 2026-08-27. The exact NVFP4 checkpoint and experimental SGLang image
are staged under `deploy/glm53_flash`. MTP-off TP4 deterministic text/simple
tool smoke and one-batch c1/c8/c16/c24 scouts passed on GPUs 4-7, followed by a passing
adaptive-MTP 5/1/6 smoke+c1+c8 comparison. Both guarded intervals peaked at
42C intake; the engine was stopped afterward. These are short feasibility
results, not a sustained-capacity or production-qualification claim. A later
c1-only five-case agent corpus passed 4/5; its required streamed typed-tool case
deterministically supplied the wrong value/type for a requested JSON-null
argument. Full tool correctness and long-context recall remain open.

## Recommendation

Test two primary serving shapes:

1. **Official native FP8 as one TP8 engine.** This is the quality and native
   precision reference. It should fit comfortably across all eight GPUs, but
   it has one cache/failure domain and crosses the two CPU/PCIe NUMA domains.
2. **LibertAI NVFP4 as two independent TP4 engines.** This is the most
   promising Ramjet serving shape. Each replica remains NUMA-local, has its own
   KV cache, and leaves much more VRAM for cache and concurrency.

Also run NVFP4 as TP8 to separate the effect of precision from the effect of
topology. Do not infer that FP4 is faster merely because its checkpoint is
smaller: most non-expert paths remain BF16, so its main expected advantage is
enabling two TP4 replicas rather than reducing every active byte.

The existing 128 GB host-RAM configuration is a credible starting point for a
**dedicated, GPU-resident NVFP4 deployment**. The 181.29 GiB checkpoint does
not need to remain resident in CPU RAM: the normal safetensors loader can
stream or memory-map weights into VRAM, and the kernel may reclaim file-backed
page cache. The unknown is the exact day-zero loader's transient/private peak,
so one TP4 engine must be loaded and measured before starting the second.
Qwen must be fully stopped, CPU weight/KV offload must remain disabled, and
swap cannot be treated as capacity.

The first node06 TP4 load confirms the one-engine case: SGLang reported
48.88 GiB container memory and the host never fell below 77.25 GiB available.
It did temporarily push about 4 GiB of inactive memory to swap, which returned
to about 178 MiB after the engine stopped. That is enough evidence to retain
128 GB for one TP4 canary, not enough to admit the second engine without a
separate sequential loader measurement.

Upgrading to 256 GB is recommended headroom, not a prerequisite for trying or
serving NVFP4. It would also populate all 16 memory channels if implemented as
16 matching 16 GB RDIMMs. A 512 GB configuration is desirable only if loader
measurements or later CPU-assisted features justify it.

Initial Ramjet integration must use approximate prefix routing only. Keep local
tokenization, exact routing, direct KV events, and snapshot routing off until
GLM's hybrid KDA/sparse-attention cache groups and vLLM event/replay behavior
have been independently qualified.

SGLang is now the first **NVFP4 TP4 canary**, based on a new exact-RTX-PRO-6000
field report. Its compatibility image fixes the previously reproduced KDA
`q_proj` loader mismatch and adds ModelOpt expert-parallel slicing plus SM120
sparse-MLA/DSA changes. The six replacement files match the report's manifest,
but the source has no detected license, is based on moving upstream code, and
the committed Dockerfile needed a two-line syntax repair. Treat this as an
internal experimental feasibility probe, not a production or redistributable
runtime. Native vLLM FP8 remains the independent quality/reference cell.

## Scope and non-goals

This study covers:

- the existing node06-style 8-GPU, dual-socket hardware;
- official FP8 and the community NVFP4 checkpoint;
- vLLM and SGLang topology, memory, CPU/NUMA placement, and expected throughput;
- a safe future experiment sequence and acceptance gates;
- which ideas from Z.ai's production serving design are useful to Ramjet.

It deliberately does not:

- treat a successful model load as a quality or production admission;
- enable Ramjet's exact cache inventory on an unproven cache-event contract.

## Evidence snapshot

The release and integration artifacts were changing during this research. The
identities below are evidence for this document, not evergreen promotion pins.
Re-resolve and review every one after the repositories have been quiet and
immediately before staging a future experiment.

| Artifact | Evidence at 2026-08-27 | Meaning |
|---|---|---|
| Official FP8 model | `zai-org/GLM-5.3-Flash` at `3f1971b7b5f7a528c9c4ef6212c8785298a8c24a` | 62 safetensors, 328,337,455,672 bytes (305.79 GiB) |
| Official BF16 source | `zai-org/GLM-5.3-Flash-BF16` at `b1967181a3917ae70a437f4884748f6b8e3a1f4d` | 120 shards, about 598.52 GiB; not a node06 serving candidate |
| Community NVFP4 | `LibertAIDAI/GLM-5.3-Flash-NVFP4` at `11d73216cd636238e82e1d77fe1042ffab36e7fa` | 120 safetensors, 194,660,206,040 bytes (181.29 GiB) |
| vLLM recipe | recipes commit `c0a069335646e3ae0026bd00062cf0fc5f5432e3` | Dedicated image/nightly; declares 386 GB minimum VRAM for FP8; now documents reasoning effort and MI355X |
| x86 CUDA 13 image | `vllm/vllm-openai:glm53-flash-x86_64-cu130@sha256:2e771fa615452282cc331eb418b3ef21636fce355bea0491fca89e6d362ab703` | Current amd64 manifest; metadata inspected, image not pulled |
| vLLM integration | PR [#53906](https://github.com/vllm-project/vllm/pull/53906), head `54d298759f6a4ce7e9768726e327110056594b9d` | Open, mergeable but `unstable`; 76 files and no stable release contract yet |
| SGLang cookbook | commits through `e27a7fac772bccb9f867c86ef7c4cbcf13738cf0` | New deployment generator and GB300 measurements; only the exact GB300 cells are marked verified |
| SGLang CUDA 13 image | `lmsysorg/sglang:glm-5.3-flash@sha256:a0d14f16b10d1f71738f1c3d88f5922369b6f921e8e79d498b81b2542218d459` | Multi-arch tag captured 2026-08-27; amd64 manifest `sha256:d0ff8177caeba28300135c98a24dafb93b7d8b3c2cc7b2b3bf88cf3ec75bec41`; build labels do not identify a source commit |
| SGLang integration | PR [#36507](https://github.com/sgl-project/sglang/pull/36507), head `033446bb05f35c0943aed2750c443077ffc0b92c` | Open, dirty/non-mergeable; 145 files; later cookbook generation now emits the canonical `EAGLE` spelling rather than its `NEXTN` alias |
| RTX PRO 6000 NVFP4 compatibility source | `0xSero/glm53-flash-nvfp4-sm120-exact-docker` at `8370bb04335bb07b6ee85907dd83cd1d300fa462` | Claims a working TP4/EP4 ModelOpt path; six source replacements hash-verified locally, but no detected license and no upstream provenance label |

The NVFP4 repository changed while this study was being written. Its current
head includes a chat-template synchronization and multimodal token fixes, and
the model had no recorded downloads when queried. That is a strong reason to
stage a local immutable snapshot, inspect its artifact diff, and require a
quiet-period repin rather than serving the live repository ID.

Primary sources are collected in [Sources](#sources).

## 2026-08-27 recipe recheck: what changed since yesterday

### vLLM

The official recipe gained two commits on 2026-08-27:

- GLM now exposes `reasoning_effort=low|high|max`; the default is `max` and
  thinking remains enabled. The field can be passed at the top level or in
  `chat_template_kwargs`. Ramjet's protocol corpus must exercise all three
  modes and preserve this field through forwarding before local rendering is
  considered.
- An MI355X ROCm recipe was added with TP4, `--max-num-seqs 512`,
  `ROCM_AITER_MLA_SPARSE`, and `VLLM_ROCM_USE_AITER=1`. Its reported FP8 KV
  pool is 14.92M tokens at 128K. This is useful architecture evidence but not
  applicable to node06's NVIDIA SM120 GPUs.

The NVIDIA recommendation is otherwise unchanged: native FP8 weights, FP8 KV
on Blackwell, optional five-token MTP, `glm47` tools, `glm45` reasoning, and
NIXL for the official P/D topology. The dedicated-image tag and integration
PR are still moving.

Two new runtime reports strengthen the requirement for a node06-specific
probe:

- The vLLM APC report still shows zero hits over 22.8K queried tokens. A
  maintainer linked an in-progress four-file scheduler/coordinator fix, but it
  is not in the official PR or captured image.
- A DGX Spark SM121 NVFP4 report found that the dedicated image could not
  serve the NoPE sparse-MLA geometry out of the box. It needed FlashInfer
  0.6.18 and local backend/cache-shape patches before serving. SM121 is not
  node06's SM120 and its 14.3 single-stream tok/s is not a node06 estimate,
  but the shared compute-capability-12 backend path makes this a concrete
  warning against assuming that “Blackwell supported” includes RTX PRO 6000.

The recipe itself is internally inconsistent about FlashInfer: prerequisites
say 0.6.17 or newer while troubleshooting says 0.6.18 or newer. Pin and inspect
the actual package instead of inheriting either statement.

### SGLang

SGLang published a substantially richer cookbook. Its purpose-built image is
required because GLM support is not in a normal release/nightly yet. The
cookbook lists H100, H200, B200, B300, GB200, and GB300; RTX PRO 6000 is not
listed, and only the exact GB300 commands currently carry a verified badge.

The useful serving guidance is:

- low latency uses adaptive EAGLE MTP with 5 steps, top-k 1, and 6 draft
  tokens; high throughput disables speculation;
- GLM consumes both a paged attention KV pool and a separate KDA state pool;
  KDA capacity can cap `max_running_requests` before the KV pool is full;
- Blackwell defaults to FP8 KV plus TRT-LLM DSA, while Hopper defaults to BF16
  KV plus TileLang DSA; the dtype and both DSA backends must be switched as a
  unit;
- prefix caching stays enabled in the official strategy;
- L1+L2 HiCache adds a 32 GB host tier; L3 adds Mooncake through
  `SGLANG_HICACHE_MOONCAKE_CONFIG_PATH`;
- P/D is a preview using NIXL to transfer both paged DSA KV and KDA recurrent
  state. It has only dummy-weight mechanical validation, requires matching TP
  sizes for now, and does not start with MTP.

HiCache is not an initial node06 option. The current build crashes at startup
when HiCache and the low-latency MTP profile are combined, and the published
HiCache benchmarks used random prompts with no reuse. They show only 0.2-1.2%
overhead, not a cache-hit benefit. A 32 GB host tier would also consume too
much of a 128 GB loader budget.

The exact 4x RTX PRO 6000 SM120 field report is more important than the generic
Blackwell default. Official FP8 booted and served only after all of the
following changes:

- disable the DeepGEMM mHC prenorm optimization with
  `SGLANG_OPT_DEEPGEMM_HC_PRENORM=0`, because SM120 lacks the required
  TMEM/tcgen05 path;
- select BF16 KV with TileLang for both DSA phases instead of the automatically
  selected TRT-LLM backend;
- patch TileLang decode to use one pipeline stage so dynamic shared memory
  falls from about 151 KB to about 86 KB, below SM120's limit.

With those changes, TP4 and EAGLE 3/1/4 reportedly served with about 2.9 of 4
draft tokens accepted. This is valuable feasibility evidence, but it is a
community report against an open integration PR, not an immutable recipe.

Both the LibertAI and dealignai NVFP4 conversions initially failed during
SGLang weight loading. A BF16 KDA `q_proj` was allocated with an expert-packed
shape, producing a `[2048,2048]` versus `[2048,4096]` assertion. Explicit
`--quantization modelopt_fp4` did not fix it. The later exact-SM120 compatibility
source corrects the quantization-name matching that caused this allocation,
preserves GLM ModelOpt metadata in the EAGLE draft path, and slices activation scales
for EP ranks. Those changes remove the known loader blocker on paper; only the
isolated node06 canary can establish that this exact build really loads and
serves.

### Current node06 decision

| Question | vLLM | SGLang | Decision |
|---|---|---|---|
| Official FP8 on RTX SM120 | The recipe is not enough: stock vLLM currently fails the NoPE sparse-MLA backend/cache selection on SM120 | Exact RTX report serves with BF16 KV/TileLang plus two workarounds and a code patch | Defer both FP8 reference paths until pinned fixes pass a loader probe |
| LibertAI NVFP4 | Blocked by the same stock-vLLM SM120 sparse-MLA issue, plus an open generic GLM cache-slot bounds defect | The exact experimental image has now passed the node06 TP4 loader and bounded c1/c8/c16/c24 cells | Continue short SGLang TP4 optimization; do not switch to vLLM yet |
| Prefix reuse | Current dedicated image has a measured zero-hit APC defect; fix is unmerged | Cookbook requires prefix cache, but no repeat-prefix GLM measurement was published | Ramjet approximate affinity may be demonstrated, but no cache benefit may be claimed yet |
| MTP | Fixed five-token candidate, blocked with the runtime | Adaptive 5/1/6 passed c1/c8; exact RTX report used 3/1/4 | Compare fixed 3/1/4 with an MTP-off batch-8 control; never carry GB300 simulated-accept numbers into node06 forecasts |
| External cache | NIXL P/D is official; MooncakeStore partial-hit review remains open | HiCache supports a 32 GB host tier and Mooncake L3, but the combinations are unverified and incompatible with current MTP | Defer on 128 GB; treat each cache tier as a separate experiment and authority domain |

SGLang's measured GB300 FP8 results are informative about direction, not
node06 throughput: FP8 KV plus TRT-LLM DSA improved aggregate output throughput
2.3-5.5% and cache-token capacity about 1.8x over BF16 KV plus TileLang. At
concurrency 16/64/256 it reported 1,189.96/2,476.87/3,972.52 aggregate output
tok/s. Those figures are for 4x GB300 and the backend pairing that currently
does not run unchanged on RTX SM120, so they must not replace the node06
theoretical ranges later in this document.

### First node06 loader canary (2026-08-27)

The exact checkpoint verifier passed 120 shards, 194,660,206,040 tensor bytes,
and all four manifest-pinned metadata hashes. The local image ID was
`sha256:c3ded56b905cf9cc70b99ab75b3accabd591260ab373c7a3746de1cc715a748b`.
The conservative TP4/EP4 profile used 262,144 context, four running requests,
90% static allocation, FP8 KV, and no MTP.

The direct endpoint became healthy in 434 seconds with no restart, OOM, Xid,
or runtime exception. Native SGLang metrics reported, per rank:

- 44.9707 GiB of weights;
- 20.8026 GiB of FP8 KV and 3,076,608 available KV tokens;
- about 18.7 GiB of KDA/Mamba state and 543 available state slots;
- 0.4746 GiB of decode graphs; and
- 8.1552 GiB available GPU memory after startup.

The startup interval peaked at 42C intake, 61C GPU, and 502 W total across the
four active GPUs. Weight loading took 213.4 seconds; scheduler and tokenizer
readiness took 323.1 and 336.6 seconds respectively. The engine remained
healthy but idle because the request guard requires intake at or below 40C.
Its five-minute wait ended `preflight_too_hot` with 0% GPU utilization and did
not launch the smoke child.

A second admission attempt used one guard around the entire startup and
bounded-test sequence, with a 900-second cooldown window and 1,200-second
child limit. It ended `preflight_too_hot` after 900.497 seconds: FP_TEMP was
42C at the final trigger and reached 43C during the wait. All eight GPUs stayed
at 0% utilization and 0 MiB allocation, proving the child never launched.
The staged MTP-off sequence is ready to validate exact text, a typed tool call,
and one batch each at c1/c8/c16; c24 is additionally gated on at most 44C
intake and 70C GPU temperature. Every cell reconciles client usage against
native SGLang request, prompt-token, and generation-token counters. Do not
repeat the attempt until intake is at or below the admission threshold, which
was 40C at the time of these attempts and was raised to 46C on 2026-08-27.

The revised 46C admission policy then admitted two bounded live intervals. The
MTP-off baseline passed deterministic text and a typed tool call, followed by
one batch at each requested concurrency:

| cell | aggregate output | median per-stream decode | median / p95 TTFT |
|---|---:|---:|---:|
| c1, 128 tokens | 63.7 tok/s | 86.5 tok/s | 529.5 / 529.5 ms |
| c8, 64 tokens | 183.8 tok/s | 76.6 tok/s | 1,390.2 / 1,955.2 ms |
| c16, 64 tokens | 229.3 tok/s | 78.1 tok/s | 2,168.5 / 3,645.6 ms |
| c24, 64 tokens | 239.2 tok/s | 76.3 tok/s | 3,101.9 / 5,572.4 ms |

All 51 requests, 2,657 prompt tokens, and 3,225 completion tokens reconciled
exactly against native SGLang counters. The scheduler was deliberately limited
to four running requests, so c8/c16/c24 include queueing and describe this safe
profile rather than a final concurrency ceiling. Guard run
`cd86d4e5071023bb2b43e7835fc5128e` passed in 323.628 seconds with 42C maximum
intake, 65C maximum GPU temperature, and 1,174.1 W maximum total eight-GPU
power. The container had no runtime/CUDA/NCCL/OOM/Xid markers and stopped with
exit 137 but `OOMKilled=false`, confirming the shutdown defect remains.

The adaptive EAGLE (`NEXTN` alias at test time) 5-step/top-k-1/6-draft candidate then passed the same smoke
and only c1/c8. c1 rose to 82.8 aggregate tok/s with 154.3 ms TTFT; c8 was
182.1 aggregate tok/s with 1,357.7 / 2,148.3 ms median/p95 TTFT. Native logs
reported accept length/rate 2.65/0.55 at c1 and 2.88/0.63 at c8. MTP therefore
improved this short c1 cell but left c8 aggregate throughput effectively flat
and increased c8 p95 TTFT about 10%. It also lengthened scheduler readiness
from 255.78 to 347.25 seconds, raised peak allocation about 1.6 GiB per active
GPU, and reduced Mamba slots from 543 to 234 while increasing the main KV pool
from 3.08M to 3.91M tokens plus a separate 2.40 GiB draft KV allocation.
Run `ceabfc898fadfaed556c6106a054cd78` passed at 42C intake and 63C GPU maximum.
Keep MTP off by default until max-running/graph batch 8 is tested as a separate
variable with native speculation counters captured directly.

Two constraints shape the next decision. First, SGLang warned that the model
does not supply FP8 KV scaling factors and used 1.0. Short text/tool correctness
now passes, but longer recall correctness remains required before FP8 KV is
accepted broadly. Second, 90% already
provides far more cache capacity than the initial four-request profile needs;
the external 93% setting should not be copied until a measured concurrency or
context frontier requires it. The stop path exceeded a 60-second grace period
and Docker killed the container (exit 137, `OOMKilled=false`); graceful shutdown
also needs qualification before promotion.

### Late upstream recheck and next configuration (2026-08-27)

The post-release issue stream makes the runtime choice clearer. Stock vLLM's
dedicated GLM image cannot currently select a valid NoPE sparse-MLA cache/kernel
path on RTX PRO 6000 (SM120), with both FP8 and BF16 attempts failing before
serving. A separate open GLM issue reports an out-of-bounds cache-slot map in a
generic circular-buffer path. Until both defects are fixed in a pinned image,
vLLM is a blocked reference candidate rather than a useful node06 A/B.

SGLang remains experimental, but it is the only path that has actually served
this immutable NVFP4 checkpoint on node06. Two fresh upstream changes matter:

- generated GLM commands now use the canonical speculative algorithm name
  `EAGLE`; `NEXTN` remains only a compatibility alias;
- the adaptive-speculation startup path now shares its CUDA-graph capture
  stream and sizes draft rows more accurately, addressing a startup crash and
  reducing graph memory. This fix is newer than the pinned day-zero base and is
  a requirement for the next upstream-derived image, not a reason to overlay
  moving `main` onto the working image;
- a separate open EAGLE issue shows temporary BF16 draft embedding/head copies
  can still be resident when the KV pool is sized. The reporter doubled usable
  KV tokens by releasing them earlier on a different hybrid model. This is not
  a node06 measurement, but a newer pinned image must resolve or explicitly
  exclude that defect before MTP cache capacity is compared.

The most promising current configuration change is the separate KDA/Mamba
state-pool bound. The MTP-off four-request boot reserved 543 state slots and
about 18.7 GiB per rank even though only four requests could run. SGLang defines
`--max-mamba-cache-size` as the maximum number of state slots/requests, whereas
`--mamba-full-memory-ratio` partitions the remaining post-weight budget between
the state and paged-KV pools. The next allocation probe should therefore cap
the state pool explicitly before changing its precision or ratio.

The Mamba scheduler is another reason to measure prefix behavior rather than
assume it. Generic SGLang argument documentation describes `auto` as normally
resolving to `no_buffer`, but the exact GLM image resolved the node06 launch to
`mamba_radix_cache_strategy='extra_buffer'` and
`uses_mamba_radix_cache=True`. That supplies branch-state/overlap support at a
state-capacity cost. Preserve the observed `extra_buffer` behavior in the next
cells; do not force `no_buffer` or claim recurrent-state recomputation without
a separate cold/warm trace.

Run the next short cells in this order, each under a fresh thermal guard:

1. MTP off, running requests 8, graph batch 8, and
   `--max-mamba-cache-size 32`; retain static allocation 0.90 for the first
   allocation-only comparison.
2. If cache/state capacity and margin are healthy, repeat the boot at static
   allocation 0.80. This is a memory-margin experiment, not an expected TPS
   improvement.
3. Run only c1 and c8 against that MTP-off control.
4. Compare fixed EAGLE 3 steps/top-k 1/4 draft tokens with adaptive EAGLE
   5/1/6, capturing native accepted length/rate and graph/state memory.
5. Run one salted cold/warm repeated-prefix check, then one 32K and one 128K
   recall cell. The model supplied no FP8-KV scaling factors and this runtime
   defaulted them to 1.0, so short protocol success is not enough quality proof.

Keep the 262,144 served cap. An open SGLang report shows the first decode after
a prompt above roughly 262K can abort under CUDA graphs while eager execution
works. Do not try 501K/1M, disable graphs, or accept a new eager baseline merely
to make the advertised context length load.

Do not spend a thermal window on all-reduce fusion, data-parallel attention,
two-batch overlap, HiCache, Mooncake, or an alternative MoE backend yet.
FlashInfer's fused all-reduce currently targets SM90/SM100 rather than SM120;
adaptive EAGLE does not support DP attention; TBO with a draft model remains
unfinished; and the SM120 CuteDSL/B12X MoE path is still an open optimization.
`flashinfer_cutlass` remains the defensible routed-expert backend for the pinned
image.

## Model architecture and serving consequences

GLM-5.3-Flash is a roughly 321B-parameter, 18B-active multimodal MoE with 45
language-model layers, a 1,048,576-token declared context, and one MTP layer.
The language stack combines:

- 34 KDA linear-attention layers;
- 11 sparse MLA layers with an indexer;
- 288 routed experts, 8 selected per token, plus a shared expert;
- native image and video processing;
- Manifold-Constrained Hyper-Connections.

Z.ai reports that hybrid linear/sparse attention reduces long-context
attention compute and KV size substantially versus its larger GLM model. It
does not make the cache ordinary. The vLLM implementation has heterogeneous
state: sparse-attention KV, indexer state, linear-attention/SSM state, and tail
or scratch allocations do not all have the same prefix-cache behavior.

Consequences for this project:

- `--enable-prefix-caching` is intended to make Ramjet's byte-prefix affinity
  useful, but it is not yet qualified. One day-zero B200 field report against
  the dedicated image measured zero hits across 22.8K queried tokens. Treat
  APC as a dedicated on/off diagnostic, not an assumed capability.
- Exact inventory cannot assume that every cache group emits equivalent block
  events or participates in prefix caching.
- One advertised “KV cache size” is insufficient observability. Future engine
  telemetry should report capacity and use by cache group.
- Long-context admission must be measured at several lengths; a 1M model limit
  is not a promise that useful concurrency survives at 1M.
- Multimodal correctness is a first-class gate because the community
  checkpoint's vision weights may be unchanged while its processor and chat
  template can still regress input construction.

### What the community NVFP4 checkpoint changes

This is not an official Z.ai quantization. Its model card says it was produced
from the official BF16 checkpoint with NVIDIA ModelOpt 0.45.0 and quantizes
only the routed-expert FFNs: 311.65B parameters, 37,152 tensors, and about 97%
of total parameters. The KDA and sparse-attention paths, indexer, vision tower,
shared experts, routers, dense and MTP MLPs, mHC tensors, embeddings, LM head,
and norms remain BF16; activations also remain BF16.

The author reports an approximately 0.9967 per-expert round-trip cosine and
0.0925 relative error. No end-to-end generation, agent/tool, multimodal,
quality, or throughput result was published with the checkpoint at capture.
Those tensor-level measurements establish that packing completed; they do not
establish serving correctness or acceptable model quality.

## Node06 hardware constraints

The target shape is the known node06 platform, not a generic eight-GPU server.

| Resource | Node06 constraint | Planning implication |
|---|---|---|
| GPUs | 8 x RTX PRO 6000 Blackwell Server Edition, 97,887 MiB reported per GPU (95.59 GiB usable), SM120 | Native FP8 and NVFP4 hardware are present; GLM-specific kernels still require validation because the recipe lists H100, B200, and GB200 as verified, not RTX PRO 6000 |
| GPU memory bandwidth | 1,597 GB/s specified per card, 12.776 TB/s aggregate | Useful only as an optimistic decode roofline; PCIe collectives and kernels lower it materially |
| Interconnect | PCIe, no NVLink; GPUs 0-3 local to NUMA 0 and 4-7 local to NUMA 1 | Prefer independent TP4 replicas when weights fit; TP8 crosses the `SYS` boundary |
| CPUs | 2 x Intel Xeon 6505P, 12 cores/24 threads per socket; 24 cores/48 threads total | Enough orchestration CPU, but not enough excess CPU or RAM for a CPU-offload design; keep each TP4 engine local to its socket |
| Memory channels | 8 DDR5 channels per CPU, 16 total | The current 128 GB uses half the channels; a 256 GB upgrade would populate all channels and improve loading/CPU-side bandwidth, but this is not a hard FP4 serving condition |
| Current RAM | 128 GB installed as 8 x 16 GB, previously observed as about 125 GiB usable with swap already consumed under the active Qwen stack | Credible for dedicated GPU-resident NVFP4 after Qwen and its swap pressure are fully removed; loader peak must be measured |
| Recommended upgrade | 256 GB as 16 x matching 16 GB DDR5 RDIMMs, 1DPC | More loader/recovery headroom and full use of all 16 memory channels; not required for the first dedicated FP4 trial |
| Optional larger build | 512 GB as 16 x 32 GB RDIMMs, 1DPC | Consider only if measurements or future CPU-offload/cache features require it |

For two TP4 engines, retain the already-qualified locality map:

| Engine | GPUs | NUMA node | Logical CPUs |
|---|---|---:|---|
| A | 0,1,2,3 | 0 | `0-11,24-35` |
| B | 4,5,6,7 | 1 | `12-23,36-47` |

A TP8 engine necessarily uses both NUMA domains. Give it all logical CPUs and
avoid binding its parent process to only one socket. Record CPU utilization,
remote NUMA traffic, page faults, and NCCL collective time; TP8 performance on
this PCIe topology may be communication-bound even when GPU utilization looks
healthy.

## Weight and VRAM model

The calculations below use exact safetensor byte totals from the captured
model revisions and the 97,887 MiB reported per GPU. “Free at 90%” is what
remains inside a theoretical vLLM allocator budget after weight bytes only. It
must also hold activations, CUDA graphs, workspaces, multimodal allocations,
hybrid state, and KV cache. It also assumes perfectly even sharding; replicated
or rank-skewed embeddings, vision, and output heads can make the most-loaded
GPU worse than this average.

| Checkpoint and shape | Weight/GPU | Physical VRAM | Weight-free at 90% | Assessment |
|---|---:|---:|---:|---|
| FP8 TP4 | 76.45 GiB | 382.37 GiB | 38.35 GiB total / 9.59 GiB per GPU | Possible boot probe, but too tight to plan as a useful serving engine |
| FP8 TP8 | 38.22 GiB | 764.74 GiB | 382.48 GiB total / 47.81 GiB per GPU | Recommended FP8 baseline |
| NVFP4 TP4 | 45.32 GiB | 382.37 GiB | 162.84 GiB total / 40.71 GiB per GPU | Recommended per-replica shape |
| NVFP4 TP8 | 22.66 GiB | 764.74 GiB | 506.98 GiB total / 63.37 GiB per GPU | Controlled precision comparison, but only one cache domain |
| NVFP4 TP2 | 90.65 GiB | 191.19 GiB | about 10.47 GiB total before runtime | Reject; no credible runtime/cache margin |

At 95% GPU utilization, FP8 TP4 leaves 57.46 GiB across the replica, still
only 14.37 GiB per GPU before non-weight allocations. The official recipe's
386 GB minimum is also effectively at the edge of a four-card 96 GiB-class
box, depending on whether its unit is interpreted as decimal GB or GiB. A
successful low-context boot would not make this a sound production topology.

Two independent NVFP4 TP4 engines duplicate 362.58 GiB of weights across the
box. That duplication is intentional: it buys a second independently schedulable
KV cache, NUMA locality, rolling restart capability, and Ramjet placement.

## Host RAM and storage plan

### Why checkpoint size is not the host-RAM requirement

NVFP4's 181.29 GiB artifact is stored on disk and ends up sharded across GPU
VRAM. It does not establish a 181.29 GiB host-RAM floor. With vLLM's ordinary
safetensors path, each TP worker reads the model and transfers its assigned
weights; file-backed mappings and page cache are reclaimable. Two engines
opening the same immutable files also share the kernel's page cache rather
than requiring two permanent 181.29 GiB CPU copies.

The real risk is transient private memory. The exact GLM/ModelOpt loader may
materialize tensors or buffers while loading, and two engines starting at once
could overlap those peaks. vLLM exposes `--max-parallel-loading-workers` for
limiting load concurrency on large TP models. Avoid `runai_streamer`: an
upstream vLLM issue reports that it can retain close to the full checkpoint in
every TP worker, whereas the ordinary `auto` path kept host memory bounded in
the reporter's control.

Use these rules for the dedicated 128 GB trial:

- stop both Qwen engines and verify their processes, private RSS, and swap
  pressure are gone before starting GLM;
- disable CPU weight offload, CPU KV offload, LMCache, Mooncake, and vLLM sleep
  mode weight offload;
- use the ordinary `auto`/safetensors loader unless the pinned image proves a
  different loader has bounded host memory;
- limit parallel loading workers if the exact image supports the option;
- keep swap as an alarm signal, never as capacity;
- load one TP4 engine first and record baseline, peak-loading, post-load, and
  post-warmup RSS/PSS, `MemAvailable`, major faults, and swap activity;
- start the second TP4 engine only if the first engine's measured peak can be
  repeated with at least 16 GiB safety margin; start it sequentially;
- abort on sustained major faults, swap-in activity, an OOM kill, or shrinking
  `MemAvailable` after warm-up;
- capture peak host RSS/PSS and page-cache behavior, not just steady state.

KTransformers is explicitly out of scope at 128 or 256 GB: its GLM tutorial
asks for at least 350 GB of available system RAM for the roughly 306 GiB FP8
model. The node06 plan is GPU-resident vLLM, not CPU-assisted inference.

The same logic makes an FP8 TP8 trial on 128 GB possible in principle even
though its artifact is 305.79 GiB, but it is a higher-risk loader experiment.
Do not assume it will work merely because NVFP4 did: qualify it separately and
upgrade RAM if the ordinary loader cannot keep transient private memory
bounded.

The FP8 and NVFP4 checkpoints alone total 487.08 GiB. Before staging both,
require at least 750 GiB free **after** preserving the current Qwen model and
rollback artifacts; 1 TiB is preferable so a partial download, immutable local
snapshot, image, and cache do not turn deployment into a disk-pressure event.
Resolve actual filesystem and inode headroom during the future preflight.

## Topology candidates

### Native FP8 reference

```text
clients -> Ramjet -> one GLM FP8 TP8 engine -> GPUs 0-7
                              |             -> one hybrid KV/cache domain
                              +------------- crosses both NUMA domains
```

This uses every GPU for each decode step. It is not “wasting” the GPUs: the
weights and cache need the memory, and TP8 aggregates bandwidth. It does leave
Ramjet with only one placement target, so the demo covers proxying, OpenAI
compatibility, health, cancellation, metrics, and journaling—not multi-engine
cache placement or failover.

### NVFP4 Ramjet candidate

```text
                         +-> TP4 A, GPUs 0-3, NUMA 0 -> KV/cache A
clients -> Ramjet -------+
                         +-> TP4 B, GPUs 4-7, NUMA 1 -> KV/cache B
```

At concurrency one, only one TP4 replica performs useful work. At concurrent
agent load, Ramjet can keep related prefixes warm on one replica while using
both halves of the box. This is the shape most likely to convert the smaller
checkpoint into better aggregate throughput and lower warm-prefix TTFT.

### Why not start with prefill/decode disaggregation

The official recipe includes a TP4 prefill pool plus TP4 decode pool using
NIXL. Z.ai's large deployment goes further with separate encode, prefill, and
decode worker pools. Those designs address large-fleet utilization and
independent scaling. On one PCIe-only eight-GPU host they initially cost us:

- a new state-transfer and failure boundary;
- matched KDA/SSM and KV layouts on both pools;
- only one decode pool, hence no independent prefix-cache placement;
- less direct comparison with the known Ramjet two-engine topology.

First measure prefill/decode imbalance. Revisit TP4+TP4 disaggregation only if
long prompts leave decode GPUs underused or queueing shows a clear phase
imbalance that two replicas cannot solve.

## Proposed initial engine contracts

These are argument plans, not launch commands. Every option must be checked
against an immutable candidate image with a no-GPU launcher probe before a
future model load. SGLang NVFP4 is the current working experiment; vLLM remains
blocked on SM120 and must not silently inherit generic Blackwell defaults.

| Setting | vLLM status | SGLang NVFP4 node06 profile | Reason |
|---|---|---|---|
| Model path | Local immutable snapshot path | Same | Avoid live repository drift and the current repo-ID multimodal processor risk |
| Image | No candidate until the SM120 sparse-MLA and cache-slot defects are fixed in a pin | Current exact local experimental image; later compare one upstream-derived immutable candidate containing reviewed fixes | Do not replace a working image with moving `main` |
| Tensor/expert parallel | Defer | TP4/EP4 on one NUMA-local four-GPU group | This is the loader and performance shape that passed; topology changes come later |
| KV/DSA | Blocked before serving on stock SM120 | FP8 E4M3 KV plus `flashinfer_sparse_mla` prefill/decode | This exact patched path passed; BF16+TileLang is a later quality reference, not a flag-only swap |
| MoE backend | Defer | `flashinfer_cutlass`, shared-expert fusion disabled, DeepGEMM mHC prenorm disabled | Current stable SM120 path; fused all-reduce and CuteDSL/B12X are not ready here |
| GPU memory allocation | Defer | 0.90 control, then 0.80 only after `max-mamba-cache-size=32` is measured | Bound the oversized state pool before trading away safety margin |
| Model context | 262,144 initially | 262,144 initially | Useful agent window without pretending day-one 1M concurrency is known |
| Concurrency | Defer | Four-request qualified baseline; next control is running requests 8 plus graph batch 8 | Separates scheduler capacity from speculation without a sustained sweep |
| Batched tokens | 8,192 initially | 8,192 chunked prefill/max prefill starting point | Tune against prefill/decode balance |
| Prefix caching | Defer | Enabled per cookbook, but still an explicit salted cold/warm diagnostic | No useful GLM prefix-hit benefit has been established on this hardware |
| Tool/reasoning parsers | `glm47` / `glm45`, auto tool choice | `glm47` / `glm45` | Current official contracts |
| Reasoning effort | Preserve and test `low`, `high`, and `max`; default `max` | Also test SGLang's `thinking=false` behavior separately | Request behavior affects token cost, latency, and agent comparisons |
| Speculation | Off in baseline | Off in baseline | Measure MTP independently after correctness and memory are stable |
| MTP candidate | Defer | EAGLE 3 steps/top-k 1/4 draft tokens first; compare adaptive 5/1/6 against the batch-8 MTP-off control | Native acceptance was only 2.65-2.88 in the adaptive scout; a smaller fixed graph may be a better trade |
| Ready timeout | 3,600 seconds | Measured startup budget with an explicit upper bound | Large model initialization is expected to be slow; timeout is not a readiness waiver |

Context qualification should advance through 8K, 32K, 128K, and 262K. Keep the
served limit there until the long-context CUDA-graph defect is fixed in the
pinned runtime. It should never advance merely because allocation succeeds;
it advances when concurrency, TTFT, recall, cancellation, and cache accounting
all remain acceptable.

After the baseline, test one variable at a time:

1. cap the separate state pool and measure static allocation 0.90 versus 0.80;
2. running/graph batch 4 versus 8 with MTP off;
3. fixed EAGLE 3/1/4 versus adaptive 5/1/6;
4. repeated-prefix behavior and 32K/128K recall with FP8 KV;
5. only then, topology, backend, or prefill/decode disaggregation changes.

Do not translate SGLang flags into a vLLM Compose service or vice versa. They
have different cache accounting, scheduler controls, speculative-decoding
implementations, and startup identities. Ramjet should see them as separate
engine profiles with the same OpenAI-level correctness corpus.

## Cache-transfer options: vLLM, SGLang HiCache, and Mooncake

These are separate from ordinary GPU prefix caching. A connector either moves
request state between prefill and decode engines or adds an external cache
tier; it does not automatically repair or replace GLM's local hybrid APC.

| Option | What it does | Current GLM evidence | Node06 position |
|---|---|---|---|
| Local GPU APC | Reuses prefix blocks already resident in one engine's GPU cache | `--enable-prefix-caching` exists, but a day-zero dedicated-image report observed zero hits; the integration PR is still open | First cache experiment; prove repeat-prompt hits before measuring Ramjet locality |
| `NixlConnector` | Directly transfers state from a prefill engine to a decode engine | This is the connector selected by the official GLM TP4+TP4 P/D recipe | Only P/D candidate initially; test later if phase imbalance justifies it |
| `MooncakeConnector` | Direct point-to-point prefill-to-decode transfer | vLLM supports it experimentally in general, but the GLM recipe does not select or claim it | No initial reason to prefer it over the recipe's NIXL path |
| `MooncakeStoreConnector` | External shared KV pool with CPU/disk offload and hash-based cross-instance reuse | GLM's current PR touches its hybrid/group lookup path, and an unresolved review notes a partial-hit regression | Do not use in the first GLM deployment |
| `LMCacheConnectorV1` / `LMCacheMPConnector` | CPU/disk offload, shared cache, or P/D integration, commonly using NIXL underneath | General vLLM support; no GLM-5.3-Flash recipe qualification | Also defer until native GPU APC and hybrid state are understood |

There are two commonly conflated “Mooncake” products:

1. `MooncakeConnector` is an ephemeral P/D transport. A prefiller computes a
   request's state and pushes it to the decoder that will generate the answer.
2. `MooncakeStoreConnector` is a persistent external cache tier. Multiple
   engines can independently save and retrieve hash-addressed blocks from a
   Mooncake distributed store backed by CPU memory or disk.

For GLM, the transferred object is not a simple transformer KV tensor. The
official P/D recipe pins `VLLM_SSM_CONV_STATE_LAYOUT=DS` and
`VLLM_KV_CACHE_LAYOUT=HND`, keeps the hybrid KV manager enabled, uses UCX on
both sides, and requires matching MTP depth. That reflects KDA/SSM state plus
sparse MLA/indexer state. A connector must preserve all of those layouts and
cache-group semantics.

MooncakeStore is especially unattractive on the current 128 GB host: its CPU
offload tier would consume the same scarce memory whose loader headroom we are
protecting. A disk tier adds I/O latency and another authority/failure domain.
It could eventually share prefixes between the two TP4 engines, but it would
also change Ramjet's locality model from “which GPU owns this prefix?” to
“which engine can retrieve it most cheaply?” Exact Ramjet inventory cannot
claim either answer without new authenticated store and per-group telemetry.

The recommended order is therefore:

1. two ordinary GPU-resident TP4 engines with no KV connector;
2. prove local APC on/off with repeated prompts and native hit counters;
3. measure whether two independent engines meet TTFT/ITL goals;
4. if phase interference is material, test the official NIXL TP4+TP4 P/D
   recipe as a separate topology;
5. consider MooncakeStore or LMCache only for a later shared/offloaded-cache
   study with more host memory and explicit hybrid-cache correctness gates.

SGLang names a different stack under the same Mooncake brand:

| SGLang tier | Storage | GLM recipe state | Node06 position |
|---|---|---|---|
| RadixAttention/prefix cache | GPU-local reusable prefixes | Cookbook says to keep it enabled, but publishes no GLM repeated-prefix result | Run a cold/warm diagnostic and inspect native hit/eviction metrics before crediting Ramjet locality |
| HiCache L2 | 32 GB host-memory spill tier in the generated recipe | Selectable but unverified; current random-prompt benchmark measured 0.2-1.2% overhead and no reuse benefit | Off on the initial 128 GB deployment |
| HiCache L3 + Mooncake | Distributed external storage behind HiCache | Selectable with a Mooncake config file, but unverified | Defer until GPU-local reuse works and host memory is upgraded |

This does not make SGLang L3 interchangeable with vLLM's
`MooncakeStoreConnector`. Both add external cache state, but the framework's
hashing, ownership, eviction, state-group completeness, and telemetry are part
of Ramjet's trust boundary. A future adapter must identify the runtime and tier
explicitly rather than expose one generic “Mooncake hit” signal.

## Ramjet integration plan

### Phase 1: safe model-neutral features

For the first GLM experiments:

- use the normal approximate raw-prompt prefix affinity and load balancing;
- forward multimodal request parts unchanged;
- retain health, cancellation, shims, usage accounting, metrics, and the
  decision journal;
- set `RJ_TOKENIZER_MODE=off`;
- set `RJ_EXACT_ROUTE_MODE=off`;
- set `RJ_KV_EVENT_MODE=off`;
- set `RJ_SNAPSHOT_ROUTE_MODE=off`;
- keep serving admission at HTTP health rather than runtime/KV compatibility.

With FP8 TP8 there is one upstream, so cache-aware selection is observable but
cannot choose another engine. With NVFP4 2xTP4, approximate affinity, load
balancing, failure isolation, and the cache-routing decision shape are
demonstrable on a dedicated 128 GB host if the sequential loader gate passes.
Any TTFT or throughput claim from locality additionally requires the native
APC diagnostic to show real hits.

### Phase 2: local rendering shadow

Add a GLM model profile only after the exact tokenizer, processor config, and
chat template are pinned and reviewed. Text-only golden vectors must cover
reasoning settings, tool choice, parallel tool calls, assistant reasoning
history, and template kwargs. Multimodal requests must continue to defer to
the engine rather than pretending local text tokenization accounts for image
or video patches.

Local rendering starts in shadow. It must not change placement until its token
vectors match the exact engine renderer and the existing attestation contract
can bind the complete serving runtime.

### Phase 3: exact cache research, not rollout

Before direct events or snapshots can influence anything, prove all of the
following against the pinned vLLM fork:

- which cache groups participate in prefix caching;
- block geometry and group identity for sparse MLA, the indexer, KDA/SSM
  state, and any tail scratch;
- event identity, incarnation, replay-from-zero, eviction, and restart
  semantics for every authoritative group;
- whether omitted groups are a conservative miss or invalidate exact scoring;
- how multiple group inventories map to one upstream without silently
  overstating reusable tokens;
- exact-token reconciliation on cold, warm, partial-hit, eviction, gap,
  cancellation, and multimodal cases.

Until then, exact inventory stays off. Z.ai's `ReplaySSM` is an engine-level
state-recovery optimization; it is a useful design reference, not something
the load balancer should emulate without an authenticated engine contract.

## What to borrow from Z.ai's serving architecture

Z.ai describes intra-node TP for linear attention and the LM head, ReplaySSM,
W8A8 weights, hybrid INT8/FP8/BF16 cache quantization, Layer Split, and
encode-prefill-decode (EPD) disaggregation. The useful Ramjet lessons are:

1. **Treat cache/state types separately.** Add group-level capacity, hit,
   eviction, and replay telemetry before attempting exact placement for a
   hybrid model.
2. **Route by work class as well as prefix.** Text decode, long prefill, and
   multimodal encoding have different bottlenecks. First journal those classes;
   only propose work-class-aware placement after measurements show a stable
   benefit.
3. **Quantize cache deliberately.** FP8 cache on Blackwell is worth testing,
   but cache precision belongs in the serving-runtime identity and requires
   long-context recall gates.
4. **Measure phase imbalance before disaggregating.** EPD is compelling at
   fleet scale. On one node, independently scheduled replicas remain the
   simpler baseline and preserve cache/failure isolation.
5. **Prefer communication savings on this host.** The production cluster uses
   high-bandwidth interconnect; node06 does not. NUMA-local TP4 replicas may
   beat a theoretically wider TP8 engine because they avoid cross-socket
   collectives.
6. **Keep engine tricks in the engine.** Layer splitting, ReplaySSM, and
   compute-for-bandwidth kernels should be adopted through a pinned upstream
   runtime rather than reimplemented in Ramjet.

Potential Ramjet follow-up work should be driven by the experiment journal:
hybrid-cache metrics first, request work-class observation second, and only
then a shadow-only cost model combining prefix reuse, queue load, prompt
length, modality, and cache-group eligibility.

## Performance hypotheses

These are vLLM-oriented capacity-planning bands, not benchmark claims. They
assume warm engines, approximately 8K input tokens, 512 generated tokens,
prefix reuse where applicable, no MTP, and successful output token
reconciliation. Day-zero kernels, PCIe collectives, prompt length,
tool/reasoning behavior, cache state, and quantization quality can move results
by 35-50%. Do not apply them to the SGLang scout; establish a fresh c1/c8
baseline after its SM120 image passes correctness.

| Shape | c1 aggregate output tok/s | c8 | c16 | c32 | Main hypothesis |
|---|---:|---:|---:|---:|---|
| FP8 TP8 | 140-230 | 600-1,000 | 900-1,500 | 1,300-2,100 | Strong single-engine bandwidth, cross-NUMA collective cost |
| NVFP4 TP8 | 120-240 | 650-1,100 | 1,000-1,700 | 1,400-2,300 | Same-topology precision control; may expose FP4 kernel benefit or overhead |
| NVFP4 2xTP4 | 100-220 | 700-1,300 | 1,100-2,000 | 1,600-2,800 | Lower TP communication plus two schedulers/caches should win as concurrency rises |

At c16, those aggregate bands correspond roughly to 56-94 output tok/s per
active FP8 session and 69-125 per active NVFP4 2xTP4 session. Agent experience
will also depend on TTFT and time spent in tools, so output tok/s is not a
complete session-capacity metric.

An intentionally optimistic memory-bandwidth roofline helps bound, but not
predict, the result. Eight cards provide 12.776 TB/s specified aggregate
bandwidth. Treating the 18B active parameters as one byte each gives about 710
target tokens/s per unbatched FP8 weight pass before cache, communication, and
kernel costs. For NVFP4, eight routed experts represent about 8.66B of the
311.65B expert parameters; using the checkpoint's packed expert ratio and
treating the remaining active path as BF16 gives a rough 23.5 GB active-weight
traffic model and a 271 token/s TP4 roofline. That crude model can make mixed
FP4 look *more* bandwidth-heavy per active token than native FP8. Batching,
inactive vision weights, cache traffic, FP4 tensor-core kernels, and MTP all
break the simplification. Its purpose is to prevent “181 GiB checkpoint means
2x faster” from becoming an assumption.

## Future experiment matrix

Run the cells in this order. Each change gets a fresh configuration identity,
model revision, image digest, prompt namespace, and result directory.

| ID | Runtime | Checkpoint | Shape | MTP | Purpose |
|---|---|---|---|---|---|
| S-F4-A | SGLang | NVFP4 | one NUMA-local TP4/EP4 | off, running/graph 4 | Complete: loader, protocol, c1/c8/c16/c24, native accounting, and thermal guard passed |
| S-F4-B | SGLang | NVFP4 | same | adaptive EAGLE 5/1/6 | Complete: c1 improved, c8 stayed flat, memory/startup cost increased; not the default |
| S-F4-C | SGLang | NVFP4 | same | off, running/graph 8 | Next: cap Mamba slots at 32, compare static allocation 0.90 then 0.80, and run only c1/c8 |
| S-F4-D | SGLang | NVFP4 | same | fixed EAGLE 3/1/4 | Compare native acceptance and graph/state cost with S-F4-C and S-F4-B |
| S-F4-E | SGLang | NVFP4 | same | best prior setting | Salted cold/warm prefix diagnostic plus bounded 32K/128K FP8-KV recall |
| S-F4-F | SGLang | NVFP4 | two independent TP4/EP4 | off first | Ramjet locality/failover/full-box candidate only after one-engine memory and graceful-stop gates pass |
| S-F8 | SGLang | Official FP8 | one NUMA-local TP4 | off | Future quality reference after reviewed SM120 TileLang/shared-memory fixes are pinned |
| V-* | vLLM | FP8 or NVFP4 | unset | unset | Blocked until SM120 sparse-MLA and GLM cache-slot defects are fixed and an immutable image passes a loader probe |
| TEP/PD | Qualified future runtime | Best stable checkpoint | unset | off initially | Consider only if measured communication or phase imbalance justifies a topology experiment |

`S-F4` is explicitly experimental because the working image contains unlicensed
third-party whole-file replacements. It is useful for feasibility and internal
optimization, not promotion. Replace it with an upstreamed or independently
reviewed/licensed image before production.

S-F8 begins as a bounded boot/memory probe and becomes a quality reference only
after correctness passes. Stop if the no-load memory plan cannot reserve useful
runtime and KV headroom; do not “succeed” by raising allocation until the
engine has no safe cache margin.

### Per-cell workload ladder

1. Health, model listing, tokenize/render sanity, one text completion.
2. Streaming and non-streaming reasoning; required, optional, and parallel
   tool calls; cancellation during prefill and decode.
3. Image and video construction, including image-only, video-only, and mixed
   message history.
4. Direct c1 and c8 scout with short context. Stop on errors or obvious
   regression before a full sweep.
5. Cold and warm prefix cells at c1/4/8/16/32 with fresh salts and several app
   prefixes. Record TTFT, TPOT, aggregate/output tok/s, request success,
   preemption, KV use, and per-engine routing.
6. Context frontier at 8K/32K/128K/262K with recall probes and bounded
   concurrency; do not cross 262K while the graph defect is open.
7. Fixed agent/tool correctness corpus, then an adequate sampled quality set
   comparing NVFP4 directly with official FP8.
8. Failure drills for the two-TP4 shape: one unhealthy engine, cancellation,
   restart, cold cache, and recovery without exact routing.

All request-generating cells must use the node thermal guard and the repository
load-test result contract. Stop request-generating work when intake air reaches
50 C and admit a new cell only after it returns to 46 C or below. Also abort on
any Xid/NVRM error, OOM kill, engine restart, sustained swap-in, unexpected
non-2xx response, token-count mismatch, or queue growth that does not recover
after load stops.

## Acceptance gates

### Runtime and safety

- Exact model revision, complete shard hashes/index, image digest, normalized
  arguments, selected environment, packages, launcher, and NCCL artifacts are
  captured before traffic.
- Every candidate image passes its runtime-specific no-GPU launcher/processor
  probe using the local snapshot path. For SGLang, the image must also identify
  the reviewed source revision and include the exact SM120 fixes; a mutable tag
  plus `unknown` provenance labels is insufficient.
- Both NUMA-local FP4 engines start sequentially on the dedicated 128 GB host
  without swap-in, OOM, or unbounded RSS growth. If not, the result defines the
  measured reason to upgrade rather than a failed model-serving result.
- Zero engine restarts, GPU errors, or request failures in the accepted cells.
- Server and client generation-token totals reconcile exactly.

### Correctness and quality

- Text, reasoning extraction, streaming, auto tool choice, required tools,
  parallel tools, and cancellation are protocol-valid.
- Multimodal cases pass on the pinned processor/template artifacts.
- NVFP4 has no new deterministic agent-protocol failures versus official FP8.
- Before promotion, agree on a fixed quality sample and require at least 98%
  of FP8 task success with no more than a 2 percentage-point absolute loss on
  the primary score. Per-expert cosine similarity is artifact evidence, not an
  end-to-end quality gate.

### Performance and Ramjet value

Promote NVFP4 2xTP4 over FP8 TP8 only if it supplies a material operational
benefit. A reasonable initial gate is:

- at least 20% more aggregate output throughput at c16 or c32 **or** a clearly
  better measured warm-prefix capacity/TTFT frontier;
- c1 output throughput at least 80% of the FP8 reference;
- warm-prefix TTFT p95 no worse than 15% at matched load;
- 100% request success and no worse long-context recall;
- stable per-engine balance without destroying prefix locality.

MTP is promoted independently. Require a measured target-step reduction,
useful acceptance, no correctness regression, and a win at the intended
concurrency; a c1 gain does not compensate for a saturated regression.

### Exact cache features

There is no day-one promotion gate because these features remain off. A future
shadow gate must demonstrate authenticated identity, complete replay, live
tail, eviction contraction, group-aware conservative scoring, tokenizer parity,
and exact engine-usage reconciliation before placement can be discussed.

## Future staging and rollback sequence

When the Qwen experiment is complete and a maintenance window is explicitly
approved:

1. Freeze and record the working Qwen Compose, image/model identities, health,
   and rollback procedure.
2. Acquire the node deployment lock and pass intake-temperature, GPU, memory,
   disk, Docker, network, and port preflight.
3. Stage immutable model snapshots and the pinned image without changing the
   running stack. Verify every artifact before stopping anything.
4. Stop the complete Qwen stack, verify its host memory and swap pressure have
   been reclaimed, then start only the next named SGLang TP4 cell. The preserved
   Qwen artifacts and Compose contract are the rollback, not a co-resident
   engine.
5. Complete S-F4-C/D/E before scheduling S-F4-F as a deliberate two-engine
   Ramjet candidate. Start A and B sequentially and qualify direct endpoints
   before LB traffic.
6. Do not schedule a vLLM cell until the SM120 sparse-MLA and cache-slot defects
   are fixed in a reviewed immutable image that passes a loader probe.
7. Replace the third-party SGLang runtime with upstreamed or independently
   reviewed and licensed fixes before production; then run the official FP8
   quality reference on whichever runtime has a qualified SM120 path.
8. Restore Qwen immediately on any stop condition. Validate its exact image,
   model identity, health, one direct request per engine, and one request
   through Ramjet before declaring rollback complete.

No future operator should synchronize a whole repository to node06. Transfer
only the reviewed deployment file list, preserve secrets and authority files,
and keep every engine image and model revision immutable.

## Decisions still blocked on measurement

- Can an upstream-derived, licensed image reproduce the working experimental
  SGLang SM120 sparse-MLA and NVFP4 MoE path?
- What are the exact loader peak and steady host RSS for a second simultaneous
  NVFP4 TP4 engine on the dedicated 128 GB host?
- Does the exact FP8 loader stay within 128 GB host RAM while streaming a
  305.79 GiB checkpoint, or does that cell justify the 256 GB upgrade?
- How much VRAM and useful KV capacity are recovered by an explicit 32-slot
  Mamba cap, and can static allocation safely fall from 0.90 to 0.80?
- Does native FP8 or mixed NVFP4 move fewer active bytes for real text decode?
- Does TP8 bandwidth overcome cross-NUMA PCIe collectives on this chassis?
- Does FP4 preserve agentic, tool, reasoning, and multimodal quality?
- At what prompt/concurrency mix, if any, does TP4+TP4 prefill/decode
  disaggregation beat two independent replicas?
- What exact vLLM cache groups and event semantics can Ramjet authenticate?
- Does SGLang expose stable, group-complete RadixAttention, KDA-pool, and
  HiCache telemetry that can be authenticated without affecting routing?
- Will the SGLang integration merge the SM120 schedule and NVFP4 loader changes
  that preserve BF16 KDA tensor shapes, eliminating the third-party runtime?

Until those are answered, the preferred *experiment* is one SGLang NVFP4
TP4/EP4 engine, progressing to 2xTP4 only after the one-engine memory and stop
gates pass. The future quality reference remains official FP8. Neither is yet a
production choice.

## Sources

- [Z.ai release and serving architecture](https://z.ai/blog/glm-5.3-flash)
- [Official GLM-5.3-Flash model card](https://huggingface.co/zai-org/GLM-5.3-Flash)
- [Official model configuration](https://huggingface.co/zai-org/GLM-5.3-Flash/blob/main/config.json)
- [vLLM GLM-5.3-Flash recipe](https://github.com/vllm-project/recipes/blob/main/models/zai-org/GLM-5.3-Flash.yaml)
- [vLLM reasoning-effort recipe update](https://github.com/vllm-project/recipes/commit/9d86e084b104980c32cdf81e0dcf6dfb8de239d8)
- [vLLM MI355X recipe update](https://github.com/vllm-project/recipes/commit/c0a069335646e3ae0026bd00062cf0fc5f5432e3)
- [vLLM GLM-5.3-Flash integration PR](https://github.com/vllm-project/vllm/pull/53906)
- [vLLM RTX PRO 6000 NoPE sparse-MLA startup blocker](https://github.com/vllm-project/vllm/issues/53963)
- [vLLM GLM cache-slot map bounds defect](https://github.com/vllm-project/vllm/issues/53982)
- [vLLM compute-capability-12 NVFP4 field report](https://github.com/vllm-project/vllm/pull/53906#issuecomment-5433846029)
- [LibertAI NVFP4 checkpoint and quantization description](https://huggingface.co/LibertAIDAI/GLM-5.3-Flash-NVFP4)
- [vLLM ModelOpt quantization support](https://docs.vllm.ai/en/latest/features/quantization/modelopt/)
- [vLLM memory-conservation and TP loading guidance](https://docs.vllm.ai/en/latest/configuration/conserving_memory/)
- [vLLM `runai_streamer` host-memory issue and ordinary-loader control](https://github.com/vllm-project/vllm/issues/44430)
- [vLLM experimental disaggregated-prefill connectors](https://docs.vllm.ai/en/latest/features/disagg_prefill/)
- [vLLM MooncakeStore shared-cache and offload guide](https://docs.vllm.ai/en/stable/features/mooncake_store_connector_usage/)
- [Day-zero GLM automatic-prefix-cache field report](https://github.com/vllm-project/vllm/pull/53906#issuecomment-5428998512)
- [In-progress vLLM GLM prefix-cache fix](https://github.com/ZJY0516/vllm/pull/4)
- [Open MooncakeStore partial-hit review on the GLM integration](https://github.com/vllm-project/vllm/pull/53906#discussion_r3863681067)
- [SGLang GLM-5.3-Flash cookbook](https://docs.sglang.io/cookbook/autoregressive/GLM/GLM-5.3-Flash)
- [SGLang GLM-5.3-Flash integration PR](https://github.com/sgl-project/sglang/pull/36507)
- [SGLang canonical EAGLE recipe correction](https://github.com/sgl-project/sglang/commit/636a6f7dbad251f22a1c31d46b0b97ba8154a9a0)
- [SGLang adaptive speculation startup/graph-memory fix](https://github.com/sgl-project/sglang/commit/b8a6adadfe8c292fbb673291dbd2a5d23232f500)
- [SGLang EAGLE draft-weight KV-pool sizing issue](https://github.com/sgl-project/sglang/issues/36452)
- [SGLang server arguments, including Mamba and HiCache controls](https://github.com/sgl-project/sglang/blob/main/docs/advanced_features/server_arguments.md)
- [SGLang ModelOpt exclusion issue](https://github.com/sgl-project/sglang/issues/36596)
- [SGLang ModelOpt MTP draft issue](https://github.com/sgl-project/sglang/issues/36599)
- [SGLang NVFP4 EP activation-scale issue](https://github.com/sgl-project/sglang/issues/36597)
- [SGLang long-context CUDA-graph decode issue](https://github.com/sgl-project/sglang/issues/36550)
- [SGLang SM120 CuteDSL/B12X MoE optimization PR](https://github.com/sgl-project/sglang/pull/29190)
- [SGLang exact RTX PRO 6000 FP8/SM120 field report](https://github.com/sgl-project/sglang/pull/36507#issuecomment-5432203047)
- [SGLang RTX PRO 6000 NVFP4 loader failure](https://github.com/sgl-project/sglang/pull/36507#issuecomment-5433433658)
- [vLLM expert-parallel deployment](https://docs.vllm.ai/en/latest/serving/expert_parallel_deployment/)
- [vLLM data-parallel deployment](https://docs.vllm.ai/en/latest/serving/data_parallel_deployment/)
- [vLLM parallelism and scaling guidance](https://github.com/vllm-project/vllm/blob/main/docs/serving/parallelism_scaling.md)
- [KTransformers GLM-5.3-Flash tutorial](https://github.com/kvcache-ai/ktransformers/blob/main/doc/en/kt-kernel/GLM-5.3-Flash-Tutorial.md)
- [NVIDIA RTX PRO 6000 Blackwell Server Edition specifications](https://www.nvidia.com/en-us/data-center/rtx-pro-6000-blackwell-server-edition/)
- [Intel Xeon 6505P specifications](https://www.intel.com/content/www/us/en/products/sku/242667/intel-xeon-6505p-processor-48m-cache-2-20-ghz/specifications.html)
- [GIGABYTE G494-SB0-AAP2 specifications](https://www.gigabyte.com/Enterprise/GPU-Server/G494-SB0-AAP2)
