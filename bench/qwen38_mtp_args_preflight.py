#!/usr/bin/env python3
"""Parser-only admission for the Qwen Flash-Next MTP-off candidate argv."""

import json

import vllm.platforms
from vllm.platforms.cpu import CpuPlatform


# Parser defaults query the platform even though this probe never constructs an
# engine. Keep the admission GPU-free while using the pinned image's real parser.
vllm.platforms.current_platform = CpuPlatform()

from vllm import AsyncEngineArgs  # noqa: E402
from vllm.entrypoints.openai.cli_args import make_arg_parser  # noqa: E402
from vllm.utils.argparse_utils import FlexibleArgumentParser  # noqa: E402


with open("/probe/candidate-argv.json", "rb") as source:
    argv = json.load(source)

if not isinstance(argv, list) or not all(isinstance(item, str) for item in argv):
    raise RuntimeError("candidate argv is not a string list")
if any("speculative" in item or item.startswith("--spec-") for item in argv):
    raise RuntimeError("candidate argv still enables speculative decoding")

parsed = make_arg_parser(FlexibleArgumentParser()).parse_args(argv)
engine = AsyncEngineArgs.from_cli_args(parsed)
if engine.speculative_config is not None:
    raise RuntimeError("pinned parser did not resolve the candidate to MTP off")
if engine.tensor_parallel_size != 4 or engine.max_num_seqs != 64:
    raise RuntimeError("candidate topology or sequence capacity changed")
if engine.kv_cache_memory_bytes != 40190174004:
    raise RuntimeError("candidate fixed KV allocation changed")

print("qwen_mtp_off_engine_args_preflight=passed")
