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
runner emits only structural outcomes, timing/token counters, routing identity, and this
metadata; it never emits completion text, reasoning, or tool arguments.

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

The corpus follows the public DeepSeek-V4 encoding contract in which tools use
DSML internally but OpenAI-compatible endpoints must return structured
`tool_calls`. Raw DSML is therefore always a protocol failure at the client
boundary.
