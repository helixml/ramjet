# Nanbeige4.3-33B behind ramjet

[Nanbeige4.3-33B](https://huggingface.co/Nanbeige/Nanbeige4.3-33B) is a 33B
dense model (Llama architecture: 52 layers, GQA with 8 KV heads, 512K
context) from Nanbeige (BOSS Zhipin), Apache-2.0, released 2026-10-24 and
aimed at agentic coding. This directory serves it to coding agents on four RTX
PRO 6000 Blackwell GPUs (96 GB): four single-GPU vLLM replicas behind ramjet
prefix routing.

Measurements and the experiments behind every default are in
[`EXPERIMENTS.md`](../../EXPERIMENTS.md) (2026-10-25 entry). All of them were
taken with the GPUs power-limited to 450 W.

## Run it

```bash
# NVFP4 weights (21 GB), a community quantisation of the 66 GB BF16 checkpoint.
hf download primitive-ai/Nanbeige4.3-33B-NVFP4 --local-dir /models/Nanbeige4.3-33B-NVFP4

cd deploy/nanbeige43_33b
umask 077; cat > .env <<EOF
NANBEIGE_MODEL_DIR=/models/Nanbeige4.3-33B-NVFP4
NANBEIGE_API_KEY=$(openssl rand -hex 24)
RJ_UI_AUTH_TOKEN=$(openssl rand -hex 24)
LB_IMAGE=ghcr.io/helixml/ramjet:rust-f5b7f09@sha256:8a06f192bd13dce3baba429da2fd9a67a6f222760d0794ea996af90e4884e739
EOF
docker compose build            # vLLM nightly + arctic-inference, no compile
docker compose up -d            # first start ~3-5 min per engine (load + compile + graphs)
```

- API (OpenAI and Anthropic Messages): `http://127.0.0.1:8000`, with
  `Authorization: Bearer $NANBEIGE_API_KEY` or a minted `rjk_*` token
  (Anthropic-style clients may send a minted token as `x-api-key`; the master
  key only works as a Bearer token, because ramjet forwards it unchanged and vLLM
  checks only `Authorization`).
- Dashboard: `http://127.0.0.1:9090/ui/` (log in with `RJ_UI_AUTH_TOKEN`).
- Claude Code and other Anthropic-compatible harnesses:
  `ANTHROPIC_BASE_URL=http://127.0.0.1:8000`, `ANTHROPIC_AUTH_TOKEN=rjk_…`,
  model `nanbeige4.3-33b`.

To serve the BF16 checkpoint instead, download `Nanbeige/Nanbeige4.3-33B` and
set `NANBEIGE_EXTRA_ARGS=--quantization fp8` (online FP8: 0.6x the NVFP4
throughput, 499k-token KV pool per GPU).

## The chat template

[`chat_template.jinja`](chat_template.jinja) is the model's own template with
two changes. Renders of ordinary conversations are byte-identical.

1. **A system message after the first is rendered in place** instead of
   raising `System message must be at the beginning.` Claude Code inserts
   such messages every turn (an environment block, then a
   `<total_tokens>… tokens left</total_tokens>` note). When the template
   rejects them, vLLM's Anthropic endpoint merges them all into the leading
   system block, ahead of the tool schemas and the history, so each turn's
   prompt diverges from the previous one after ~3.5k tokens. Engine prefix-cache
   hits were 13-28% with the stock template and 92-96% with this one.
2. **`disable_thinking`**, a template variable only the server sets, backs
   `NANBEIGE_THINKING=never`.

## Thinking

The model thinks by default. vLLM turns any request `reasoning_effort` other
than `none` into `enable_thinking=true` and `none` into `false`, overriding the
server default. **Claude Code always sends an effort** (`output_config.effort`,
`high` by default; no setting sends `none`), so with Claude Code only `never`
turns thinking off.

| `NANBEIGE_THINKING` | behaviour | Claude Code tasks (10, NVFP4) |
| --- | --- | --- |
| `on` (default) | model default; requests decide | 9-10/10 passed, median 367-552 s, 20-22 turns |
| `off` | server default off; any `reasoning_effort` but `none` turns it back on | same as `on` for Claude Code |
| `never` | template ignores requests; no reasoning parser (it would file whole answers as reasoning) | 8/10 passed, median 272 s, 32 turns; the two hardest tasks ran out of turns |

## Knobs

| variable | default | when to change |
| --- | --- | --- |
| `NANBEIGE_SPEC_METHOD` | `suffix` | `off` for chat/prose-heavy traffic. Code edits: 8.3x single-stream, 2.3x at 32 concurrent; Claude Code tasks finished in a median 0.81x the time. Prose: -6% single-stream, -22% at 32 concurrent, because vLLM disables async scheduling with suffix speculation. |
| `NANBEIGE_THINKING` | `on` | See above. |
| `NANBEIGE_EXTRA_ARGS` | empty | `--quantization fp8` when serving the BF16 checkpoint. |
| `NANBEIGE_GPU_UTIL` | `0.92` | |
| `NANBEIGE_MAX_MODEL_LEN` | `262144` | Up to 524288; one NVFP4 replica's KV pool is 623k tokens. |
| `RJ_ROUTE_AFFINITY_BASIS` | `relative` | See the Domyn Edge deployment for the absolute/marginal/relative comparison. |

## Why these defaults

| change | measured effect (one RTX PRO 6000 unless noted) |
| --- | --- |
| NVFP4 W4A4 weights + fp8 KV | 9,143 tok/s at c128 (2,200 in / 256 out) vs 5,307 for online FP8; single-stream decode 50.6 vs 31.1 vs 17.6 (BF16) tok/s |
| quality of NVFP4 | GSM8K (1,319, thinking off) 94.2% vs 94.7% FP8 (BF16 96.0% vs FP8 95.8% on the first 500); held-out perplexity within 2.2% of BF16 (lower); agent protocol suite 100% thinking on and off; Claude Code tasks 9-10/10 vs FP8 7/7 |
| one replica per GPU (vs TP2) | TP2 decodes one stream at 73.5 tok/s (1.45x) but peaks at 8,029 tok/s on two GPUs, against 9,143 on one GPU for TP1 |
| ramjet prefix routing (vs nginx) | simulated coding-agent fleet, same four engines: +37% turns/min at 32 developers and +65% at 64 over nginx `least_conn` (+50% over round-robin at 32); median time to first token 1.3-1.5 s vs 3.8-3.9 s; 97-99.7% of turns stay on their replica vs 38-39% |
| `--enable-prompt-tokens-details` | without it vLLM omits cached tokens from response usage |

## Caveats

- The NVFP4 checkpoint is a community quantisation (`primitive-ai`,
  llm-compressor `nvfp4-pack-quantized`), not a Nanbeige release. The quality
  checks here are GSM8K, perplexity, the agent protocol suite and ten small
  Claude Code tasks (`bench/claude_code_tasks.py`), not the model card's
  benchmarks.
- Speculative decoding disables vLLM async scheduling in this build. Re-measure
  prose throughput when vLLM supports async scheduling with suffix decoding.
- The base image is CUDA 13.0 and needs NVIDIA driver 580 or newer.
