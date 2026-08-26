# Qwen3.8-Flash-Next deployment

This directory is the canonical one-file deployment for the official
`Qwen/Qwen3.8-Flash-Next-FP8` checkpoint on node06. It defines two NUMA-local
TP4 engines and the existing ramjet load balancer. Do not add Compose overlays.

The checkpoint and image are immutable inputs:

- model revision: `bcd9f01ddc9cff2316eb84281bebcd5b058bddce`
- model payload: 185,502,232,570 bytes across 131 safetensors shards
- linux/amd64 vLLM image: `sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3`
- released ramjet Compose default: `v0.4.0@sha256:467e7edf40c8fcad29e741cbba52ca571cbae0261d94cff008aa6bcdb737ea1b`
- node06-qualified profile override: `rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385`

The day-zero vLLM image config labels its source/build revision as `unknown`.
The digest makes the bytes immutable, but it does not supply source provenance;
keep it a candidate until runtime/package capture, correctness, and performance
qualification are recorded. Do not turn the recipe page into an authority claim.

The qualified candidate uses GPUs 4-7 while production remains single-homed on
the current Qwen3.8-27B engines on GPUs 0-3. `node06-canary.sh` holds
`/run/lock/ramjet-node06-deployment.lock` across the complete routing change,
engine stop/start and verification interval. The conservative mode starts only
the named candidate service and restores the eight-engine baseline on failure:

```bash
./node06-canary.sh start-b
```

The active fast-iteration lane keeps production fixed at 4/4 on GPUs 0-3 and
leaves the retired engines on GPUs 4-7 stopped. It starts the candidate without
restoring those retired engines after a failure:

```bash
./node06-canary.sh iterate-b
```

The startup path samples all GPU telemetry plus the intake-air sensor and
retains an owner-only evidence file below `.experiments/`.

Do not start this file's load balancer until both TP4 engines have independently
passed direct health, model identity, deterministic correctness, tool calling,
and a guarded performance scout. In the fast-iteration lane a failed candidate
is removed while production remains single-homed at 4/4.

The initial configuration deliberately keeps PLE CPU offload disabled because
node06 does not have 51 GiB of uncommitted host RAM while the production stack
is resident. First optimization cells should change one variable at a time:
expert parallelism, MTP, scheduler concurrency, then PLE offload only if host
memory admission becomes available. Every request-generating cell runs under
`bench/node06_gpu_guard.py` and is recorded in `EXPERIMENTS.md`.

## Day-zero option review

The vLLM recipe updated on 2026-08-26 exposes three weight variants: Inferact
NVFP4 (130 GB stated minimum), official FP8 (265 GB), and official BF16
(423 GB). The initial candidate deliberately uses the official FP8 weights:
TP4 is validated by the recipe and four 96 GB GPUs provide enough aggregate
capacity while avoiding an unqualified community quantization change. The
recipe's supported single-node strategies are TP, tensor+expert parallel
(TEP), and data+expert parallel (DEP). This deployment starts with TEP4; DEP
requires PLE CPU offload and therefore is not admissible with node06's present
host-memory headroom.

The cache-related controls are distinct:

- `--enable-prefix-caching` is enabled and retains reusable KV blocks in the
  on-device cache.
- The recipe UI's `SimpleCPUOffloadConnector` default asks for 236,223,201,280
  host bytes *per rank*. A TP4 engine would reserve about 880 GiB, so neither
  one nor two engines can use it on this 125 GiB host. Mooncake and LMCache
  likewise require a meaningful host-memory pool or additional nodes; they do
  not create capacity on this box.
- `VLLM_PLE_CPU_OFFLOAD=1` offloads the separate 51B N-gram embedding table,
  not KV cache. It needs at least 51 GB plus runtime headroom and remains off.
- GPU KV dtype is left at the model/runtime default for the correctness
  baseline. FP8 KV, including scale handling and quality, is a later isolated
  A/B rather than an assumption in the first boot.

At 90% utilization, the first successful boot reported 38.32 GiB available KV
and recommended `--kv-cache-memory=40190174004` to fit the requested budget.
An otherwise identical warm boot charged a transient 34.91 GiB warmup peak as
activation and auto-sized KV down to 4.48 GiB. The canonical command therefore
pins the engine-recommended 40,190,174,004-byte allocation; guarded load must
still prove that the explicit allocation survives the admitted batch limits.

The other recipe features are tool calling and reasoning (enabled), MTP3
speculative decoding (enabled after the non-speculative baseline passed), text-only
mode (rejected for the multimodal production target), and static YaRN to one
million tokens (deferred until native 262K behavior is qualified). The official
recipe uses 256 sequences and 90% GPU utilization. The first RTX PRO candidate
uses 64 sequences, an 8,192-token batch cap, and the recipe's 90% utilization.
An attempted 85% boot measured -0.27 GiB available for KV blocks after warmup
and correctly failed before serving. Each scheduler limit is raised
independently only after observed cache and memory telemetry shows room.

## Qualified TP4 cell

The active direct candidate is healthy with restart count zero. With MTP3 it
reports a 2,667,258-token GPU KV pool, enough for 10.17 native 262K contexts;
the non-speculative baseline reported 3,033,380 tokens. A guarded request with
251,009 actual prompt tokens completed, and the identical-prefix warm TTFT was
1.58s versus 32.25s cold. The hybrid runtime did not populate the response's
`cached_tokens` field, so that field is not treated as cache authority.
The synthetic long-context correctness gate also passed 4/4 requests: five
needles spanning 1–99% depth at 99,875 and 199,482 prompt tokens, followed by a
two-turn tool session over a 50K-token prompt.

MTP3 is a low-batch choice. Against the same direct TP4 engine it improved
512-token code decode by 72% at c1, 38% at c8, and 17% at c16, then regressed
aggregate throughput by 4.6% at c32. Native speculative counters reconciled
exactly in every cell. Tool calling, reasoning, and a real image request all
passed; the latter also confirms that the multimodal target works when vLLM
warns that its MTP draft receives text-only inputs. See the 2026-08-26 entry in
`EXPERIMENTS.md` for exact measurements and guard evidence.

The retained MTP3 configuration also sets
`index_share_for_mtp_iteration=true`. The pinned Qwen runtime implements
step-zero QSA top-k selection plus per-request row compaction before later
draft steps reuse those indices. A paired A/B crossover found a modest average
gain of about 3.7% at c8 and 0.9% at c32, while c1/c16 improved 6.2%/1.7% in
the first full matrix. The five-case agent gate, 4/4 deep-context corpus, and a
33.5K repeated-prefix cell all passed with native speculative reconciliation.
This flag is retained as a measured low/mid-batch improvement, not as a fix for
the MTP3 c32 crossover.

Ramjet's approximate route is the only admitted routing mode for this model.
The deployment selects the dedicated `qwen3.8-flash-next` renderer profile but
keeps tokenizer and exact KV routing off pending separate live attestation.
It disables the legacy 100K max-token strip so valid long-output budgets reach
vLLM unchanged. Qwen template controls such as `chat_template_kwargs`,
`preserve_thinking`, and multimodal processor kwargs participate in prefix
fingerprints, preventing requests with different rendered prefixes from
claiming the same warm route.

The checked-in Compose default follows the repository-wide released-image
policy. `node06-canary.sh` supplies the separately qualified r133 override
explicitly until these Flash-Next changes are included in a tagged release.
