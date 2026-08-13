# Infernal parser fixtures

These are synthetic raw model-output fixtures for the DeepSeek V4 parser. No
production prompts, responses, tool arguments, or credentials are retained.

The profiles distinguish the immutable Infernal r4 parser, upstream vLLM
#49117 applied to r4, and the intended conservative behavior. The `complete`
profile is a qualification contract, not a description of the rejected r5
candidate: that unchanged source is expected to fail the mixed-mode and EOF
cases.

```bash
python3 bench/infernal_parser_probe.py validate
python3 bench/infernal_parser_probe.py run /path/to/composed/vllm \
  --profile pr49117 --expected-source-id sha256:...
```

The probe imports the actual `deepseek_v4_config`, `ParserEngine` argument
prefix logic, and `StreamingParserEngine` from the supplied tree while
stubbing heavyweight vLLM serving modules. It is a fast source-composition
gate, not a replacement for the image's full parser tests or the live
deterministic agent gate.

Every expectation records only structural results: tool-call starts and ends,
whether a call was open immediately before EOF handling, whether the assembled
client argument strings are JSON objects, whether canonical argument objects
repeat, and whether DSML reached content. Argument validity follows the same
stream-prefix algorithm as the supplied parser source. Canonical argument
values, names, content, and raw events are never printed; the committed inputs
are synthetic.

The fail-closed contract is deliberately narrow:

- EOF may not manufacture `TOOL_CALL_END` for an incomplete native invoke.
  The serving layer must treat the unmatched start as a protocol failure
  instead of presenting partial arguments as a completed call; the source
  probe covers the event-side half of that contract.
- Orphan recovery applies only when no completed native tool block preceded
  it. A trailing orphan after a wrapped block stays content. This prevents a
  recovered suffix from silently widening one/two intended calls into two/three
  calls, without deduplicating otherwise valid tool calls by argument value.
- A held orphan name cut off before validation remains content and emits no
  tool lifecycle events.

The probe prints only these booleans/counts and content length, never fixture
content or argument values.
