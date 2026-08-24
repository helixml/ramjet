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

Every live run derives a fixed-size cache namespace from `--salt`, including
when `--prefix-kib=0`; use one fresh salt for each matched cold/warm pair. The
zero-prefix case adds only the sub-KiB namespace, while positive sizes include
it inside the requested byte count. This prevents nominally cold corpus runs
from inheriting identical prompt state from an earlier experiment.

The typed required-tool case deliberately reserves 256 output tokens. A live
256KiB/c8 run used 184-206 tokens for valid nested JSON, while its former
192-token cap produced an intermittent structurally incomplete call. Keep
correctness budgets above measured valid output instead of treating benchmark-
induced truncation as a frontend-parser failure.

## Reasoning-effort and output-budget matrix

Issue #14 uses this same fixed correctness oracle to compare explicit
`reasoning_effort` and `max_tokens` policies. `agentbench.py` preserves the
corpus's caller settings unless `--reasoning-effort` or
`--max-output-tokens` is supplied. Each request record includes the effective
bounded policy; summaries add valid and total-spent completion tokens per
successful task, request-latency p50/p95, and fixed-cardinality finish-reason
counts.

Run the bounded low/high/max × 96/192/256 matrix with fresh namespaces:

```bash
bench/reasoning_matrix.sh http://127.0.0.1:8006 deepseek-v4-flash issue14-r1
```

Defaults run both deterministic and official-agentic sampling, three repeats
of the five-case corpus per cell, and concurrency five. Narrow development runs
with `REASONING_PROFILES`, `REASONING_EFFORTS`, `REASONING_OUTPUT_CAPS`,
`REASONING_RUNS`, or `REASONING_CONCURRENCY`. `REASONING_START_CELL` resumes at
a numbered cell after retaining the earlier JSONL. Progress goes to stderr and
results to stdout. Policy cells deliberately report protocol failures without
aborting later cells, but any transport failure still stops the matrix.

A lower cap is promotable only if protocol validity and task completion remain
equal to the control. Never infer quality from fewer completion tokens alone.
ramjet does not classify content or enforce this policy; it verifies
pass-through and supplies evidence for the Helix control plane, where an
explicit per-step policy and kill switch belong.

## Sovereign trace-shape replay

`bench/agent_trace.py` accepts an optional content-free workload shape exported
inside the sovereign environment. It does not accept OpenAI requests or logs.
Every JSONL record has exactly these fields:

```json
{"schema_version":1,"arrival_offset_ms":0,"prefix_group":0,"shared_prefix_tokens":32768,"prompt_tokens":33024,"history_turns":2,"history_tool_rounds":1,"history_parallel_calls":2,"protocol":"parallel_tool","stream":true,"expected_tool_calls":2,"max_output_tokens":256,"observed_completion_tokens":190,"sampling":{"temperature":1.0,"top_p":0.95,"seed":7,"reasoning_effort":"high"}}
```

The trusted exporter must:

- replace source app/session identifiers with prefix groups densely numbered
  from zero in first-seen order;
- use relative arrival offsets bucketed to 100ms, never absolute timestamps;
- retain only prompt/shared-prefix/completion token counts, turn/tool counts,
  fixed protocol enums, and sampling settings;
- emit no prompt/generated text, tool schemas/names/arguments/results, request
  or user/session IDs, URLs, credentials, fingerprints, or source hashes.

The ingester rejects unknown or missing fields, strings outside fixed enums,
non-finite/unbounded numbers, non-bucketed or regressing timing, sparse prefix
groups, inconsistent tool shapes, more than a ten-minute arrival window,
16M prompt tokens / 1M maximum output tokens in aggregate, and more than 1,024
records or 4MiB. The
input must be a singly linked regular mode-`0600` file owned by the runner in a
non-symlink mode-`0700` parent. This makes accidental raw request/log ingestion
fail before any network call.

Validate locally, then replay on the sovereign node with a fresh cache salt:

```bash
install -d -m 0700 /run/user/$(id -u)/ramjet-traces
umask 077
chmod 0600 /run/user/$(id -u)/ramjet-traces/shape.jsonl
python3 bench/agent_trace.py validate \
  /run/user/$(id -u)/ramjet-traces/shape.jsonl

python3 bench/agent_trace.py run http://127.0.0.1:8006 deepseek-v4-flash \
  /run/user/$(id -u)/ramjet-traces/shape.jsonl \
  --metadata-json /tmp/node06-agent-metadata.json \
  --salt "$(date +%s%N)" --concurrency 32 \
  > /run/user/$(id -u)/ramjet-traces/result.jsonl
chmod 0600 /run/user/$(id -u)/ramjet-traces/result.jsonl
```

Replay builds synthetic messages in memory. Same-group prefixes are nested;
different groups and salts are unlinkable. Before GPU execution, one small
synthetic `/tokenize` probe per unique protocol/history/tool/reasoning shape
measures the active engine chat-template overhead and adjusts only the repeated
tail filler. Calibration has a 30s timeout and 1MiB response cap; returned token
IDs are discarded in memory and never logged. A missing, malformed, oversized,
or implausible calibration fails before inference.

The recorded target prompt-token count is still checked against authoritative
completion response usage with a bounded default tolerance (5% or 256 tokens),
so tokenizer/template drift or a synthetic density mismatch fails the shape
gate instead of silently becoming performance evidence. Arrival timing is
scheduled from relative buckets and client queueing remains measurable.

Output contains only shape ordinals, bounded protocol errors/categories,
usage/timing counts, opaque route ordinals, deployment provenance, and aggregate
shape buckets. It never prints the input line, prefix group, raw salt, generated
content, reasoning, or tool arguments. Keep both input and output only under
the organization's normal private benchmark retention policy; never commit,
upload as a public CI artifact, or copy either outside the sovereign boundary.

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

## Schema v2: long-context recall and multi-turn sessions

`v2_sessions.jsonl` is a separate corpus so `v1.jsonl` and its goldens stay
frozen; run it with `--corpus`. Schema v2 adds two optional case fields. A v1
case that uses either is rejected, and v1 cases load unchanged.

### `context` — needle-in-a-haystack recall

```json
"context": {
  "filler_kib": 128,
  "needles": [{"depth": 0.02, "key": "ALPHA", "value": "51907"}],
  "probe_keys": ["ALPHA"]
}
```

The runner builds a salt-namespaced filler document, plants each `[RECORD]`
line at its fractional `depth`, and prepends the document to the case's user
message. `expected.content_contains_all` is **derived** from the needles
(narrowed by `probe_keys` when present) rather than restated in the corpus: a
hand-maintained copy drifts from the planted values and turns a recall
regression into a green run.

Depth matters more than size here. This model is a hybrid stack whose linear
attention carries long-range information in a fixed-size state, so a change to
that state's dtype is far likelier to cost recall at 98% depth than at 2%.
Keep needles at both ends and the middle.

The filler is salt-namespaced for the same reason as `add_prefix`: two runs
sharing a prompt prefix let the second be served from the first's radix cache.

### `turns` — real multi-turn sessions

```json
"turns": [
  {"tool_results": {"read_metric": "{\"value\":27604}"},
   "user": "Now read queue_depth for engine B.",
   "request": {"tool_choice": "none"},
   "expected": {"mode": "text", "content_contains_all": ["27604"]}}
]
```

Turn 0 uses the case's `request`/`expected`. Each later turn replays the
previous assistant message with its tool calls intact, appends one `tool`
message per call quoting the real `tool_call_id`, optionally appends a `user`
message, applies a `request` patch, and validates the new response against that
turn's `expected`. Binding results to call ids is the part of a session that
actually breaks, so the ids are taken from the response rather than invented.

A `request` patch may change `tool_choice` or `max_tokens` but never `model`,
`messages`, or `stream` — those are the identity of the conversation being
continued. `tool_results` payloads are parsed at corpus-validation time so a
malformed one fails locally instead of becoming a confusing model failure
mid-session. `"*"` matches any tool name.

Each turn emits its own record carrying a `turn` index, and a failing turn
still runs its successors: a session that recovers after a bad turn is a
different outcome from one that derails, and collapsing them hides it.
