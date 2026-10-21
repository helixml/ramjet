# Domyn Edge behind ramjet

[Domyn Edge](https://huggingface.co/domyn/Domyn-Edge-SFT) is a fully open
(weights, data, training code) 4B dense model from Domyn, Apache-2.0, with a
256k recommended context and first-class English, Italian, Spanish, French and
German. Two deployments live in this repository:

| file | hardware | shape |
| --- | --- | --- |
| [`deploy/domyn_edge/docker-compose.yaml`](docker-compose.yaml) | 4× RTX PRO 6000 Blackwell (96 GB) | four single-GPU vLLM replicas behind ramjet prefix routing |
| [`deploy/domyn_edge_4090/docker-compose.yaml`](../domyn_edge_4090/docker-compose.yaml) | 1× GeForce RTX 4090 (24 GB), also driving a desktop | one replica behind ramjet for tokens, usage and the dashboard |

Measurements and the experiments behind every default are in
[`EXPERIMENTS.md`](../../EXPERIMENTS.md) (2026-10-21 entries).

## Run it

```bash
hf download domyn/Domyn-Edge-SFT --local-dir /models/Domyn-Edge-SFT   # 15.7 GB (FP32 on disk)

cd deploy/domyn_edge            # or deploy/domyn_edge_4090
umask 077; cat > .env <<EOF
DOMYN_MODEL_DIR=/models/Domyn-Edge-SFT
DOMYN_API_KEY=$(openssl rand -hex 24)
RJ_UI_AUTH_TOKEN=$(openssl rand -hex 24)
LB_IMAGE=ghcr.io/helixml/ramjet:rust-f5b7f09@sha256:8a06f192bd13dce3baba429da2fd9a67a6f222760d0794ea996af90e4884e739
EOF
docker compose build            # vLLM nightly + Domyn PR overlay, no compile
docker compose up -d            # first start ~2-4 min per engine (compile + graph capture)
```

- API (OpenAI and Anthropic Messages): `http://127.0.0.1:8000`, with
  `Authorization: Bearer $DOMYN_API_KEY` or a minted `rjk_*` token
  (`x-api-key` also works for Anthropic-style clients).
- Dashboard: `http://127.0.0.1:9090/ui/` (log in with `RJ_UI_AUTH_TOKEN`). Mint
  one token per client in the Tokens tab; Usage shows requests, tokens, cache
  hits, TTFT and decode speed per token and per model.
- Coding harnesses: Anthropic-compatible clients take
  `ANTHROPIC_BASE_URL=http://127.0.0.1:8000`,
  `ANTHROPIC_AUTH_TOKEN=rjk_…` and model `domyn-edge`.

## Client settings that matter

- **Send `reasoning_effort: "none"` or omit it.** The engine defaults thinking
  off, but vLLM turns any other `reasoning_effort` value into
  `enable_thinking=true`, overriding that default. With thinking on, Domyn
  dropped parallel tool calls and multi-turn recall in our protocol suite;
  with it off every case passed, at 3-5x fewer tokens per task. To opt in,
  send `chat_template_kwargs: {"enable_thinking": true}` and allow thousands
  of output tokens.
- Sampling defaults come from the model (`temperature 1.0, top_p 0.95,
  top_k 20`); agent harnesses usually override them.

`LB_IMAGE` has no default: the token and usage features these files enable are
newer than the last tagged ramjet release, and the repository only defaults
Compose files to released images. Repin to the first release that contains
them (CHANGELOG "Unreleased" → minted API tokens) when it is published.

## Knobs

| variable | default | when to change |
| --- | --- | --- |
| `DOMYN_SPEC_METHOD` | `suffix` | `off` for chat/prose-heavy traffic. Suffix decoding gives 2-5x on code edits (8x single-stream on the 4090) but costs ~30% on open-ended prose on the RTX PRO 6000, because vLLM disables async scheduling with suffix/ngram speculation. `ngram` is the dependency-free alternative (3.3x single-stream on code). |
| `DOMYN_GPU_UTIL` | `0.92` / `0.80` (4090) | Raise to 0.90 on a headless 4090. On a display GPU 0.90 OOMs during speculative warm-up. |
| `DOMYN_MAX_MODEL_LEN` | `262144` | Beyond 256k quality degrades (RULER 74.5 at 256k, 60.8 at 512k for the base model). |
| `RJ_ROUTE_AFFINITY_BASIS` | `relative` | `absolute` scores raw prefix overlap, so a system prompt every replica holds makes them all look equally warm (agent fleet: 168 vs 221 turns/min for `marginal`). `marginal` scores overlap above the least-warm replica; `relative` scores each replica against the warmest one. At four replicas `relative` won back to back: 225.9 vs 203.9 turns/min, TTFT p90 2.9 vs 4.3 s. |
| `RJ_API_TOKENS_MODE` | `on` | `off` only behind another authenticating proxy; the engines check only the master key. |

## Why these defaults

| change | measured effect |
| --- | --- |
| FP8 weights + fp8 KV (vs BF16) | 2.2x throughput on RTX PRO 6000 (26.5k vs 12.2k tok/s at c256, 2.2k-in/256-out); +1.2-1.3% held-out perplexity |
| one replica per GPU | 1.7M-token KV pool per RTX PRO 6000, ~280k on a 4090; no TP communication |
| thinking off | protocol suite 100% vs 80-86%; 27-40 vs 87-179 completion tokens per task |
| suffix decoding | code edits: 165→876 tok/s single-stream (RTX PRO 6000), 114→927 (4090); prose: -20 to -31% (RTX PRO 6000) |
| `--enable-prompt-tokens-details` | without it vLLM omits cached tokens from response usage, so ramjet's per-token cache hits and Anthropic clients' `cache_read_input_tokens` read 0 |
| ramjet prefix routing (vs nginx `least_conn`) | domyn-swarm document-QA batch 1.46x faster (cache hits 36%→66%); coding-agent fleet +31% turns/min at 96 developers and +38% at 32 with `marginal` (median TTFT -44% / -54%); `relative` adds another +11% over `marginal` |

## Caveats

- The architecture is not in a vLLM release yet. `Dockerfile.vllm` copies
  vllm-project/vllm#62544 at a pinned commit onto a pinned nightly; delete it
  when a release ships the model. The base image is CUDA 13.0 and needs
  NVIDIA driver 580 or newer.
- Speculative decoding disables vLLM async scheduling in this build. Re-measure
  prose throughput when vLLM supports async scheduling with suffix decoding.
- Quality numbers here are protocol validity and perplexity, not task accuracy.
  Domyn's own card reports SWE-bench Verified 33.6 and BFCL v4 36.5 for the SFT
  model; it is a fast 4B model, not a frontier coder.
