# Serving a model

mini-dynamo is a KV-cache-aware load balancer for vLLM. Most of it does not
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

A model with no profile still works. Set `MD_TOKENIZER_MODE=off` and
`MD_EXACT_ROUTE_MODE=off` and you get byte-fingerprint affinity routing, load
balancing, health, and metrics. A profile is what unlocks *attested local
tokenization*, which is a telemetry and placement optimisation, not a
requirement for serving.

## Choosing a profile

`MD_TOKENIZER_PROFILE` selects one. Unknown values are rejected at startup
rather than silently falling back, so a typo cannot quietly disable
attestation. Current profiles:

| Label | Model | Modality | Formatter |
|---|---|---|---|
| `deepseek-v4-r34` | DeepSeek-V4-Flash (vLLM r34) | text | native Rust |
| `qwen3.8-27b` | Qwen/Qwen3.8-27B | image + video | HF chat template |
| `qwen3.8-2.4t-a95b` | Qwen/Qwen3.8-2.4T-A95B | text | HF chat template |

Profiles using an HF chat template additionally require
`MD_CHAT_TEMPLATE_PATH` and `MD_CHAT_TEMPLATE_SHA256`. The template is
digest-pinned for the same reason the tokenizer is: editing it changes the
token IDs the renderer produces, silently invalidating every cached placement
decision derived from them. Setting one without the other is a startup error.

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
