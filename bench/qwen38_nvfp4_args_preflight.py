#!/usr/bin/env python3
"""Parser-only admission for the pinned Qwen Flash-Next NVFP4 recipe argv."""

import json

import vllm.platforms
from vllm.platforms.cpu import CpuPlatform


vllm.platforms.current_platform = CpuPlatform()

from vllm import AsyncEngineArgs  # noqa: E402
from vllm.entrypoints.openai.cli_args import make_arg_parser  # noqa: E402
from vllm.utils.argparse_utils import FlexibleArgumentParser  # noqa: E402


REVISION = "103a7608316173ca6edd49929544244de7ffda70"

with open("/probe/candidate-argv.json", "rb") as source:
    argv = json.load(source)
if not isinstance(argv, list) or not all(isinstance(item, str) for item in argv):
    raise RuntimeError("candidate argv is not a string list")
if not argv or argv[0] != "/workspace/model":
    raise RuntimeError("candidate positional model changed")

# `vllm serve` owns the positional model argument and normalizes it onto the
# engine parser's `--model` option before constructing AsyncEngineArgs. Rebuild
# that exact boundary here; passing the positional value directly to the inner
# parser leaves its unrelated Qwen3-0.6B help/default model in place.
normalized = ["--model", argv[0], *argv[1:]]
parsed = make_arg_parser(FlexibleArgumentParser()).parse_args(normalized)
engine = AsyncEngineArgs.from_cli_args(parsed)
checks = {
    "model": engine.model == "/workspace/model",
    "revision": engine.revision == REVISION,
    "tokenizer_revision": engine.tokenizer_revision == REVISION,
    "tensor_parallel_size": engine.tensor_parallel_size == 4,
    "enable_expert_parallel": engine.enable_expert_parallel is True,
    "moe_backend": engine.moe_backend == "marlin",
    "gpu_memory_utilization": engine.gpu_memory_utilization == 0.95,
    "kv_cache_memory_bytes": engine.kv_cache_memory_bytes is None,
    "max_num_seqs": engine.max_num_seqs == 16,
    "max_num_batched_tokens": engine.max_num_batched_tokens == 8192,
    "max_model_len": engine.max_model_len == 262144,
    "enable_prefix_caching": engine.enable_prefix_caching is True,
    "speculative_config": engine.speculative_config is None,
}
failed = sorted(name for name, passed in checks.items() if not passed)
if failed:
    raise RuntimeError("candidate parser shape changed: " + ",".join(failed))

print("qwen_nvfp4_engine_args_preflight=passed")
