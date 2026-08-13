# Agent protocol corpus v1

`v1.jsonl` is a synthetic, versioned OpenAI chat-completions workload for
DeepSeek-V4 protocol regressions. It covers streaming and non-streaming text,
required and automatic tool selection, split tool-call deltas, all JSON value
classes, parallel calls, and a multi-turn reasoning/tool history. It contains
no customer prompts, credentials, real tool payloads, or request fingerprints.

Validate the schema and local parser fixtures without a model or GPU:

```bash
python3 bench/agentbench.py validate
python3 -m unittest discover -s bench -p 'test_*.py'
```

Live runs require a metadata JSON object so results cannot lose deployment
provenance. The required fields are `engine_image`, `model_revision`,
`tokenizer_sha256`, `config_sha256`, `router_version`, and `gpu_count`. The
runner emits only structural outcomes, timing/token counters, routing identity,
and this metadata; it never emits completion text, reasoning, or tool arguments.
Summaries reduce route headers to fixed `0`, `1`, `missing`, and `other`
buckets so placement balance is visible without retaining arbitrary values.

```bash
export BENCH_TOKEN=...
python3 bench/agentbench.py run http://127.0.0.1:8006 deepseek-v4-flash \
  --metadata-json /tmp/node06-agent-metadata.json \
  --profile deterministic --concurrency 1 --repetitions 1
```

Use `--profile agentic` for the model-recommended `temperature=1.0,
top_p=0.95` distribution. Deterministic runs use `temperature=0`, `top_p=1`,
and a fixed seed. A result fails if DSML fragments appear in normal content,
tool names/JSON/types do not match, reasoning is missing where required, or
the task-completion expectation is absent.

The typed required-tool case deliberately reserves 256 output tokens. A live
256KiB/c8 run used 184-206 tokens for valid nested JSON, while its former
192-token cap produced an intermittent structurally incomplete call. Keep
correctness budgets above measured valid output instead of treating benchmark-
induced truncation as a frontend-parser failure.

The corpus follows the public DeepSeek-V4 encoding contract in which tools use
DSML internally but OpenAI-compatible endpoints must return structured
`tool_calls`. Raw DSML is therefore always a protocol failure at the client
boundary.

`vllm_frontend_v1.jsonl` is a GPU-free response-shape fixture, not a live
workload. It locks the generic named-tool JSON fallback to the vLLM merge that
unified required/named streaming (`8eb401134e750781a202c0b6dc4059616cdb4954`)
and the independent-choice state contract to the `n > 1` isolation fix
(`3683fe6c0651fe54a0201552ae7dfb7acb1e0cea`). The fixtures prove local
assembly/validation for both streaming and non-streaming forced choices, and
prove that identical choice-scoped tool-call IDs do not merge alternative
choices' names or arguments. They deliberately do not join `v1.jsonl`:
replaying already-shaped frontend responses should not spend GPU time in the
candidate gate.
