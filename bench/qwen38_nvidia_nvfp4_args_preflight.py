#!/usr/bin/env python3
"""Parser-only admission for the NVIDIA NVFP4 TP4 serving argv."""

import json

import vllm.platforms
from vllm.platforms.cpu import CpuPlatform


vllm.platforms.current_platform = CpuPlatform()

from vllm import AsyncEngineArgs  # noqa: E402
from vllm.entrypoints.cli.serve import make_arg_parser  # noqa: E402
from vllm.utils.argparse_utils import FlexibleArgumentParser  # noqa: E402


REVISION = "fc694b54fb0174e0913e6adf86691ef85a4ead47"

with open("/probe/candidate-argv.json", "rb") as source:
    argv = json.load(source)
if not isinstance(argv, list) or not all(isinstance(item, str) for item in argv):
    raise RuntimeError("candidate argv is not a string list")
if not argv or argv[0] != "/workspace/model":
    raise RuntimeError("candidate positional model changed")

normalized = ["--model", argv[0], *argv[1:]]
parsed = make_arg_parser(FlexibleArgumentParser()).parse_args(normalized)
engine = AsyncEngineArgs.from_cli_args(parsed)
checks = {
    "model": engine.model == "/workspace/model",
    "revision": engine.revision == REVISION,
    "tokenizer_revision": engine.tokenizer_revision == REVISION,
    "tensor_parallel_size": engine.tensor_parallel_size == 4,
    "enable_expert_parallel": engine.enable_expert_parallel is True,
    "quantization": engine.quantization == "modelopt",
    "trust_remote_code": engine.trust_remote_code is True,
    "gpu_memory_utilization": engine.gpu_memory_utilization == 0.90,
    "kv_cache_memory_bytes": engine.kv_cache_memory_bytes == 40_190_174_004,
    "max_num_seqs": engine.max_num_seqs == 64,
    "max_num_batched_tokens": engine.max_num_batched_tokens == 8192,
    "max_model_len": engine.max_model_len == 262144,
    "enable_prefix_caching": engine.enable_prefix_caching is True,
    "speculative_config": engine.speculative_config is None,
    "prompt_token_details": parsed.enable_prompt_tokens_details is True,
    "tool_parser": parsed.tool_call_parser == "qwen3_coder",
    "reasoning_parser": parsed.reasoning_parser == "qwen3",
}
failed = sorted(name for name, passed in checks.items() if not passed)
if failed:
    raise RuntimeError("candidate parser shape changed: " + ",".join(failed))

print("qwen_nvidia_nvfp4_engine_args_preflight=passed")
