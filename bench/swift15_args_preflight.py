#!/usr/bin/env python3
"""Check the isolated Swift canary argv with the pinned vLLM parser."""

import json

import vllm.platforms
from vllm.platforms.cpu import CpuPlatform


vllm.platforms.current_platform = CpuPlatform()

from vllm import AsyncEngineArgs  # noqa: E402
from vllm.entrypoints.cli.serve import make_arg_parser  # noqa: E402
from vllm.utils.argparse_utils import FlexibleArgumentParser  # noqa: E402


REVISION = "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38"

with open("/probe/candidate-argv.json", "rb") as source:
    argv = json.load(source)
if not isinstance(argv, list) or not all(isinstance(item, str) for item in argv):
    raise RuntimeError("candidate argv is not a string list")
if not argv or argv[0] != "/workspace/model":
    raise RuntimeError("candidate positional model changed")

parsed = make_arg_parser(FlexibleArgumentParser()).parse_args(
    ["--model", argv[0], *argv[1:]]
)
engine = AsyncEngineArgs.from_cli_args(parsed)
checks = {
    "model": engine.model == "/workspace/model",
    "revision": engine.revision == REVISION,
    "tokenizer_revision": engine.tokenizer_revision == REVISION,
    "served_model_name": engine.served_model_name == ["qwen3.8-flash-next"],
    "tensor_parallel_size": engine.tensor_parallel_size == 4,
    "enable_expert_parallel": engine.enable_expert_parallel is True,
    "quantization": engine.quantization == "modelopt",
    "trust_remote_code": engine.trust_remote_code is True,
    "gpu_memory_utilization": engine.gpu_memory_utilization == 0.90,
    "kv_cache_memory_bytes": engine.kv_cache_memory_bytes == 16_000_000_000,
    "max_num_seqs": engine.max_num_seqs == 16,
    "max_num_batched_tokens": engine.max_num_batched_tokens == 4096,
    "max_model_len": engine.max_model_len == 262144,
    "enable_prefix_caching": engine.enable_prefix_caching is True,
    "speculative_config": engine.speculative_config is None,
    "kv_events_config": engine.kv_events_config is None,
    "prompt_token_details": parsed.enable_prompt_tokens_details is True,
    "tool_parser": parsed.tool_call_parser == "qwen3_coder",
    "reasoning_parser": parsed.reasoning_parser == "qwen3",
}
failed = sorted(name for name, passed in checks.items() if not passed)
if failed:
    raise RuntimeError("candidate parser shape changed: " + ",".join(failed))

print("swift15_engine_args_preflight=passed")
