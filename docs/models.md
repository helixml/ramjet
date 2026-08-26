# Serving a model

ramjet is a KV-cache-aware load balancer for vLLM. Most of it does not
know or care which model is behind it: routing, health, metrics, the decision
journal, and the KV-event index are all model-neutral. Only *local request
rendering* — turning an OpenAI request into token IDs the engine would produce
— is model-specific, and that lives entirely in `src/model/`.

## What is and is not model-specific

| Component | Model-specific? |
|---|---|
| Approximate routing (`src/router.rs`) | No — FNV-1a over raw prompt bytes |
| Proxy, shims, usage accounting | No — OpenAI protocol only |
| KV-event ingestion, exact index | No — a vLLM contract, not a model one |
| Snapshot companion, digest index | No |
| Prompt rendering, reasoning translation, attestation | **Yes** — `src/model/` |

A model with no profile still works. Set `RJ_TOKENIZER_MODE=off` and
`RJ_EXACT_ROUTE_MODE=off` and you get byte-fingerprint affinity routing, load
balancing, health, and metrics. A profile is what unlocks *attested local
tokenization*, which is a telemetry and placement optimisation, not a
requirement for serving.

## Choosing a profile

`RJ_TOKENIZER_PROFILE` selects one. Unknown values are rejected at startup
rather than silently falling back, so a typo cannot quietly disable
attestation. Current profiles:

| Label | Model | Modality | Formatter |
|---|---|---|---|
| `deepseek-v4-r34` | DeepSeek-V4-Flash (vLLM r34) | text | native Rust |
| `qwen3.8-27b` | Qwen/Qwen3.8-27B | image + video | HF chat template |
| `qwen3.8-flash-next` | Qwen/Qwen3.8-Flash-Next | image + video | HF chat template |
| `qwen3.8-2.4t-a95b` | Qwen/Qwen3.8-2.4T-A95B | text | HF chat template |

Profiles using an HF chat template additionally require
`RJ_CHAT_TEMPLATE_PATH` and `RJ_CHAT_TEMPLATE_SHA256`. The template is
digest-pinned for the same reason the tokenizer is: editing it changes the
token IDs the renderer produces, silently invalidating every cached placement
decision derived from them. Setting one without the other is a startup error.

The Flash-Next profile deliberately shares the Qwen3.8-27B rendering
implementation. At the Flash-Next revision qualified by this repository, its
`tokenizer.json` and `tokenizer_config.json` are byte-identical to the reviewed
Qwen3.8-27B artifacts, including the chat template and the
`low`/`medium`/`xhigh` reasoning controls. That equivalence justifies sharing
renderer behavior; it is not a model or runtime attestation. Local shadow mode
still requires the exact tokenizer and template digests plus its own reviewed
goldens, while model weights, engine arguments, packages, KV-event behavior,
and live process identity remain separate deployment authorities.

## Adding a model

Adding a model is a new file in `src/model/` plus one line in `PROFILES`.
Nothing outside that module is allowed to match on a model identity; if a
change seems to need that, the abstraction is in the wrong place.

Implement `ModelProfile`:

```rust
impl ModelProfile for MyModel {
    fn label(&self) -> &'static str { "my-model-v1" }
    fn family(&self) -> &'static str { "my-model" }
    fn formatter_source(&self) -> FormatterSource { FormatterSource::HfChatTemplate }
    fn modality(&self) -> Modality { Modality::Text }
    fn template_kwarg_keys(&self) -> &'static [&'static str] {
        &["enable_thinking", "reasoning_effort"]
    }
    fn apply_reasoning(&self, args: &mut HashMap<String, Value>) -> Result<(), LocalFailure> { ... }
    fn reasoning_class(&self, effort: &str) -> Option<&'static str> { ... }
    fn thinking_can_be_disabled(&self) -> bool { true }
}
```

Then register it in `PROFILES` in `src/model/mod.rs`. The registry tests will
immediately exercise the new profile: label uniqueness, lookup round-tripping,
refusal to locally tokenize a vision request, and rejection of unknown
`chat_template_kwargs`.

### Two rules that are easy to get wrong

**Do not remap a reasoning effort you do not support.** If a caller sends
`reasoning_effort: "high"` and your model only has `low`/`medium`/`xhigh`,
return `LocalFailure::Unsupported` so the request falls back to the engine.
Promoting `high` to `xhigh` silently changes the token budget the caller asked
for. The load balancer reports; it does not rewrite caller intent.

**List every template kwarg the model actually understands.** A request
carrying an unlisted key is not locally rendered, which is safe but means it
never gets attested. Qwen3.8's `preserve_thinking` defaults to on, so omitting
it would push essentially all agent traffic onto the remote authority and make
attestation coverage near zero.

## Multimodal requests

Requests with non-text content parts (`image_url`, `video_url`, and anything
added later) are forwarded to the engine **byte for byte**. The request
sanitizer flattens array-valued `content` into a string only when every part is
text.

This matters more than it sounds. The sanitized body is the body that reaches
the engine, so a sanitizer that dropped an image would not fail the request —
it would answer a question about an image the model never saw. There is an
end-to-end proxy test asserting the image survives; keep it.

No profile attempts to locally tokenize a request containing a non-text part,
including the multimodal ones. Counting image tokens requires reproducing the
engine's patch/preprocessing pipeline exactly, which this process deliberately
does not do. Those requests defer to the remote authority.

## Choosing and sizing a model

The load balancer is model-neutral, but which model you put behind it and how
you shard it change the answer by more than any router tuning does. These are
measured on node06 (8x RTX PRO 6000 Blackwell) and are meant as a worked
example of what to look at, not as universal constants.

### Sparse MoE and dense models fail differently

A sparse mixture-of-experts activates a fraction of its parameters per token; a
dense model activates all of them. That difference dominates single-stream
decode, while full-box capacity also depends heavily on batching, speculation,
and the concurrency at which each stack saturates.

| | DeepSeek-V4-Flash (sparse MoE) | Qwen3.8-27B (dense) |
|---|---|---|
| per-stream decode @ c1 | 245.1 tok/s | 77, or 121 with MTP |
| best qualified full-box throughput | 1,891.2 tok/s | 7,890.9 tok/s |
| measured shape | c24/max256 | c256/max256, MTP off |

Neither is "better". Pick against the workload: a single interactive user feels
the c1 row, while a saturated fleet cares about the full-box row. The capacity
figures are each model's best qualified point on the same two-TP4/eight-GPU
topology, but they come from separate model-specific workloads and are not a
matched head-to-head benchmark. DeepSeek's c24 result completed 72/72 requests;
Qwen's c256 result uses deterministic 256-token outputs and disables MTP,
which measured 12.5% slower once the box was saturated.

Two consequences worth knowing before tuning:

- **Speculative-decoding depth does not transfer between them.** DeepSeek ran
  DSpark at depth 5-7. On Qwen3.8, depth 4 measured *worse* than depth 2 (91.2
  vs 117.9 tok/s at c1) because acceptance falls from 61% to 38% and the extra
  draft compute is not repaid. Draft tokens are cheap relative to the target
  step on a sparse MoE and expensive on a dense one.
- **Being far from the bandwidth roofline means the bottleneck is elsewhere.**
  Qwen3.8-27B at TP4 realises about 29% of its weight-bandwidth roofline, so
  the missing time is kernel and tensor-parallel communication across 64
  layers, not memory. That is a signal to change the sharding, not the
  scheduler.

### Shard count is a cache decision, not just a parallelism decision

This is the part that is easy to miss. ramjet routes on prefix overlap, so
the number of engines determines how many distinct system prompts can each own
a warm engine. N engines partition M apps N ways.

Measured with four apps, 24KiB of shared system prompt each, warm:

| concurrency | TP4 x2 | TP2 x4 | TTFT p50 |
|---|---|---|---|
| 8 | 636 tok/s | **659** | 0.65s -> **0.31s** |
| 32 | 1805 tok/s | **2069** | 1.74s -> **1.03s** |
| 64 | 2183 tok/s | **2726** | 2.50s -> **1.83s** |

Four two-GPU engines beat two four-GPU engines by up to 25% on throughput and
cut TTFT roughly in half, on identical hardware, for two compounding reasons:
each app can own an engine rather than sharing one, and TP2 pays less
allreduce than TP4. Halving the shard size did not cost context -- both serve
the full 253,952-token window, because the KV pool bounds concurrency rather
than request length.

The corollary is that **benchmarking with unique prompts will mislead you
here**. A sweep where every request is unique cannot see cache partitioning at
all; it measures the one thing this router is not for. `bench/qwen_concurrency.py`
takes `--apps` and `--prefix-kib` for this reason, and the difference between
the two modes is large: the same TP4 pair reads 817 tok/s at c8 on unique
prompts and 636 tok/s at c8 on shared prefixes, because the shared-prefix
requests carry ~6000 prompt tokens each even when cached.

### Running on fewer GPUs

The topology is (number of engines) x (tensor-parallel size), and both come
out of how many GPUs you have. The node06 deployment is a single Compose file
carrying the qualified eight-engine shape
(`deploy/qwen38_27b/docker-compose.yaml`); change it on a branch to try another
topology rather than generating an overlay. The `render_topology.py` generator
and its committed topology files were removed on 2026-08-23 — see AGENTS.md
"One Compose file per deployment".

```bash
docker compose up -d
```

`--json` prints the plan without writing anything, including how much VRAM each
GPU needs. Renders for the common cases are committed beside it, and a test
asserts they still match the generator so a hand-edit cannot drift.

Qwen3.8-27B-FP8 is about 28GiB of weights, so the tensor-parallel size is
bounded from below by what fits and chosen from above by the trade-offs already
described. Allow roughly 1.4x the weight share per card for KV cache,
activations, and CUDA graphs:

| GPUs | TP | Engines | Weights/GPU | Needs about | Notes |
|---|---|---|---|---|---|
| 1 | 1 | 1 | 28GiB | 40GiB | Smallest workable box. No load balancing to do, but the shims, metrics, and journal still apply. |
| 2 | 2 | 1 | 14GiB | 20GiB | Fits 24GiB cards. One engine, so no cache partitioning. |
| 2 | 1 | 2 | 28GiB | 40GiB | Prefer this over 2xTP2 if the weights fit: two engines can hold two warm prefixes. |
| 4 | 2 | 2 | 14GiB | 20GiB | Good middle ground on 24GiB cards. |
| 4 | 1 | 4 | 28GiB | 40GiB | Best cache partitioning at this size when the weights fit on one card. |
| 8 | 2 | 4 | 14GiB | 20GiB | **Measured best** on node06 for shared-prefix traffic. |
| 8 | 4 | 2 | 7GiB | 10GiB | The base file. Largest per-engine KV pool; fewest engines. |

Two rules of thumb, both following from the measurements above rather than from
theory:

1. **Use the smallest tensor-parallel size the weights fit into.** TP costs an
   allreduce per layer and buys nothing the prefix cache cares about. Going
   from TP4 to TP2 on the same eight GPUs was worth up to 25%.
2. **More engines is better until you run out of distinct system prompts.**
   Engines partition the prefix cache, so the gain flattens once engines
   outnumber the apps in your workload. If you serve one system prompt to
   everyone, extra engines only split your KV pool.

The exception to both is long-context serving. Each engine gets its own KV
pool, so eight small pools hold fewer simultaneous long conversations than two
large ones even though the total is similar. If your workload is a handful of
very long sessions rather than many short ones, prefer the larger shard.

### Thermal envelope can be the real capacity limit

On node06 the binding constraint is not the GPUs' compute. Sustained c64 with
large shared prefixes drove GPU1 -- consistently ~5C hotter than its
neighbours -- to 85C in 92.7 seconds, and the thermal guard terminated the
run. c32 sustains fine. Measure how long a configuration can hold a load, not
only how fast it goes while it does; see AGENTS.md for the guard's ceiling and
continuous-inference cap.

## Engine-side settings that are not our concern but bite anyway

These live in the deployment, not in a profile, but a model will appear broken
without them:

- **A reasoning parser.** Without `--reasoning-parser`, a thinking model emits
  its entire `<think>` block inside `content` and every client sees raw
  chain-of-thought. For Qwen3.8 the parser registers as `qwen3`.
- **Block size.** It is not always 256. Qwen3.8's hybrid linear-attention
  layers force the attention block to **784 tokens** so the attention page is
  at least the mamba page. A request therefore needs more than 784 prompt
  tokens — not 256 — before it produces a single KV event, and prefix sharing
  granularity is correspondingly coarser.
- **`max_num_seqs`.** This caps concurrent sequences per engine and is a direct
  throughput ceiling. Raising Qwen3.8's from 64 to 256 measured +32% aggregate
  throughput at c256.
- **Speculative decoding, if the checkpoint ships a draft head.** Qwen3.8-27B
  carries a trained MTP head, and enabling it measured +112% at c8 and +55% at
  c1 -- but **-12.5% at c256**. Speculative decoding buys fewer sequential
  steps with more compute per step, so it wins while the device waits on
  decode and loses once the batch saturates it. Measure both ends: a good
  acceptance rate is not sufficient justification, because at saturation the
  rejected fraction is pure waste.

See `deploy/qwen38_27b/docker-compose.yaml` for a worked example and
`EXPERIMENTS.md` for the measurements behind these numbers.
