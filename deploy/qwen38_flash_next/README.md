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
the recorded runtime/package capture, correctness, and performance qualification
establish this deployment without inventing missing image-source provenance.
Do not turn the recipe page into an authority claim.

Production now uses both TP4 engines: A on GPUs 0-3 and B on GPUs 4-7. The
load balancer is owned by this Compose project and reports 2/2 HTTP admission.
Both engines independently passed direct identity, deterministic agent/tool,
code-decode, prefix-cache, long-context, and multimodal gates before pair
admission. The temporary canary controller was removed after promotion so an
obsolete action cannot silently restore the retired baseline.

Every future mutation must hold `/run/lock/ramjet-node06-deployment.lock` and
retain an owner-only evidence journal below `.experiments/`. Roll back one TP4
half at a time: single-home the healthy Flash engine, stop the peer, recover the
old engines on that GPU half, move traffic only after they are healthy, then
repeat for the other half. Never start old single-GPU engines underneath a
running TP4 process.

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

## Qualified TP4 pair

Both active engines are healthy with restart count zero. With MTP3 each
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

The approximate routing shape is pinned explicitly in Compose: 2KiB chunks, a
2MiB fingerprint window, a 32-block affinity cap, 32KiB load units, and an
eight-unit request cap. Candidate-specific projected-load scoring remains
explicitly off pending a guarded warm-long-prefix A/B. Phase-aware load
accounting is enabled: once an
upstream emits its first semantic token, ramjet releases the size-weighted
prefill reservation and retains one decode unit. A guarded 3-run ABBA conflict
cell reduced returning-probe TTFT from 2,496ms to 287ms (88.5%) while retaining
99.1% of blocker throughput. A separate c32 ABBA improved aggregate throughput
from 2,640 to 2,712 tok/s (2.7%); TTFT p95 rose 7.3%, within the 10% guard. All
432 requests completed with exact native speculative reconciliation and zero
preemptions. A guarded alpha 4/2/2/4 crossover found that alpha 2 raised native
prefix hits while regressing returning-probe TTFT by 15%, blocker TTFT p95 by
25%, and completed output throughput by 7%; alpha 4 therefore remains the
qualified value.

The checked-in Compose default follows the repository-wide released-image
policy. The node06 production render supplies the separately qualified r133
override explicitly until these Flash-Next changes are included in a tagged
release. Every node06 `docker compose` invocation, including rollback and
cleanup traps, must therefore carry the exact `LB_IMAGE` override; restoring
the Compose file alone would select the older released default.

The load balancer joins both the Flash serving network and the existing
`qwen38_27b_default` bridge. The latter is observation-only: node06's host
machine-view agent listens on that bridge gateway, so dropping it would leave
serving healthy while silently losing machine telemetry. It can be retired
only after the host agent is deliberately rebound and the replacement URL is
qualified from inside the final load-balancer container.

## Checkpoint revision currency (checked 2026-08-27)

`Qwen/Qwen3.8-Flash-Next-FP8` mutable `main` has moved past our pin
`bcd9f01ddc9cff2316eb84281bebcd5b058bddce` to
`970c569adaca6b35532111fd6b27351b2baefe50`. The delta is `README.md` only;
`config.json`, `chat_template.jinja`, `generation_config.json`, both tokenizer
files, and all 131 safetensors shards are byte-identical across the two
revisions. Our pin is therefore still the latest substantive revision and no
re-download or re-qualification is warranted. Re-check the revision tree, not
just the `lastModified` timestamp, before concluding that a Hub change matters.

## Upstream recipe delta (vLLM recipes, 2026-08-27)

The recipe's Flash-Next guide switched `--tool-call-parser` from `qwen3_coder`
to `qwen3_xml`. In the pinned engine image both names resolve to the same class
object, `vllm.tool_parsers.qwen3_engine_tool_parser.Qwen3EngineToolParser`, so
our argv needs no change. Re-probe the parser registry on any future engine
image before assuming the alias still holds:

```bash
docker exec qwen38flashnext-a python3 -c "
from vllm.tool_parsers import ToolParserManager as M
print(M.get_tool_parser('qwen3_xml') is M.get_tool_parser('qwen3_coder'))"
```

The same commit marks `rtx_pro_6000_4x` verified and publishes an FP8 override
of `--max-num-seqs 16` at `--gpu-memory-utilization 0.95`. That is a different
trade from our measured 64 sequences at 0.90 with a pinned `--kv-cache-memory`,
and it is a candidate for a bounded A/B rather than a correction. The recipe
also raised `min_vllm_version` to `0.29.0`, which is unreleased; vLLM v0.28.0
(2026-08-26) is the newest tagged release and remains ungated here.
