# Infernal parser fixtures

These are synthetic raw model-output fixtures for the DeepSeek V4 parser. No
production prompts, responses, tool arguments, or credentials are retained.

The profiles distinguish the immutable Infernal r4 parser, upstream vLLM
#49117 applied to r4, and the intended complete behavior after a conservative
fix for the malformed `toolcalls` opener from vLLM #51914.

```bash
python3 bench/infernal_parser_probe.py validate
python3 bench/infernal_parser_probe.py run /path/to/composed/vllm \
  --profile pr49117 --expected-source-id sha256:...
```

The probe imports the actual `deepseek_v4_config` and
`StreamingParserEngine` from the supplied tree while stubbing heavyweight vLLM
serving modules. It is a fast source-composition gate, not a replacement for
the image's full parser tests or the live deterministic agent gate.
It prints only structural outcomes and content length, never fixture content.
