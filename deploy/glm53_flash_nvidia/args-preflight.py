#!/usr/bin/env python3
"""GPU-free admission for the GLM-5.3-Flash NVFP4 serving argv.

Runs inside the pinned vLLM image with no GPU, no network, and no weights. It
parses the exact rendered Compose argv through the image's own CLI and builds
the full engine config, so an unsupported architecture, an unavailable parser,
a rejected flag combination, or a silently re-decided quantization method fails
here instead of inside a multi-minute engine start on eight shared GPUs.

  docker run --rm --network none \
    -v /prod/models/nvidia/GLM-5.3-Flash-NVFP4-<revision>:/workspace/model:ro \
    -v <experiment>:/probe:ro \
    --entrypoint python3 vllm/vllm-openai@sha256:<digest> \
    /probe/args-preflight.py /probe/candidate-argv.json

/workspace/model needs only the checkpoint metadata: config.json, the
safetensors index, and the tokenizer. It does not read a single weight, so this
runs before the 190GiB download completes.
"""

from __future__ import annotations

import json
import pathlib
import sys

import vllm.platforms
from vllm.platforms.cpu import CpuPlatform

# The engine config is built for its shape, not to run. A CPU platform is what
# lets a GPU-less container construct it at all; it never builds an engine or a
# model, so the live smoke on node06 remains mandatory.
vllm.platforms.current_platform = CpuPlatform()

from vllm import AsyncEngineArgs  # noqa: E402
from vllm.entrypoints.cli.serve import make_arg_parser  # noqa: E402
from vllm.utils.argparse_utils import FlexibleArgumentParser  # noqa: E402


MODEL_REVISION = "423acf37583782c51c142d145aef733d72943d93"
EXPECTED_ARCHITECTURE = "Glm5NextForConditionalGeneration"


class PreflightError(RuntimeError):
    pass


def load_argv(path: pathlib.Path) -> list[str]:
    argv = json.loads(path.read_bytes())
    if not isinstance(argv, list) or not all(isinstance(item, str) for item in argv):
        raise PreflightError("candidate argv is not a string list")
    if not argv or argv[0] != "/workspace/model":
        raise PreflightError("candidate positional model changed")
    return argv


def checks(parsed, engine, config) -> dict[str, bool]:
    model = config.model_config
    cache = config.cache_config
    return {
        # Identity: an immutable checkpoint at a pinned revision.
        "model": engine.model == "/workspace/model",
        "revision": engine.revision == MODEL_REVISION,
        "tokenizer_revision": engine.tokenizer_revision == MODEL_REVISION,
        # The image resolves the checkpoint to the in-image GLM-5.3 hybrid
        # implementation rather than a generic or remote-code fallback.
        "architecture": model.architectures == [EXPECTED_ARCHITECTURE],
        "no_remote_code": engine.trust_remote_code is False,
        "hybrid": getattr(model, "is_hybrid", False) is True,
        # NVFP4 weights with an FP8 KV cache, as the checkpoint declares.
        "quantization": model.quantization == "modelopt_fp4",
        "kv_cache_dtype": cache.cache_dtype == "fp8",
        # Topology and the canary's bounded first GPU exposure.
        "tensor_parallel_size": engine.tensor_parallel_size == 4,
        "enable_expert_parallel": engine.enable_expert_parallel is True,
        "gpu_memory_utilization": engine.gpu_memory_utilization == 0.90,
        "max_model_len": model.max_model_len == 262_144,
        "max_num_seqs": engine.max_num_seqs == 4,
        "max_num_batched_tokens": engine.max_num_batched_tokens == 8192,
        # Ramjet routes on prefix reuse; a build that silently dropped prefix
        # caching would make every routing measurement meaningless.
        "prefix_caching": cache.enable_prefix_caching is True,
        # Text-only admission: the checkpoint is multimodal but ramjet's
        # request path is not.
        "multimodal_disabled": all(
            model.multimodal_config.get_limit_per_prompt(modality) == 0
            for modality in ("image", "video")
        ),
        # Speculation stays off on SM120 until it is separately qualified.
        "no_speculation": config.speculative_config is None,
        # Parsers must resolve inside the image, not merely be spelled right.
        "tool_parser": parsed.tool_call_parser == "glm47",
        "reasoning_parser": parsed.reasoning_parser == "glm47",
        "auto_tool_choice": parsed.enable_auto_tool_choice is True,
        "prompt_token_details": parsed.enable_prompt_tokens_details is True,
    }


def preflight(path: pathlib.Path) -> None:
    argv = load_argv(path)
    parsed = make_arg_parser(FlexibleArgumentParser()).parse_args(
        ["--model", argv[0], *argv[1:]]
    )
    engine = AsyncEngineArgs.from_cli_args(parsed)
    config = engine.create_engine_config()

    failed = sorted(name for name, passed in checks(parsed, engine, config).items() if not passed)
    if failed:
        raise PreflightError("candidate engine shape changed: " + ",".join(failed))

    # Resolving the parsers proves the registered names exist in this image.
    from vllm.reasoning import ReasoningParserManager
    from vllm.tool_parsers import ToolParserManager

    ToolParserManager.get_tool_parser(parsed.tool_call_parser)
    ReasoningParserManager.get_reasoning_parser(parsed.reasoning_parser)


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {pathlib.Path(sys.argv[0]).name} CANDIDATE_ARGV_JSON", file=sys.stderr)
        return 2
    try:
        preflight(pathlib.Path(sys.argv[1]))
    except (OSError, ValueError, PreflightError) as error:
        print(f"args-preflight.py: {error}", file=sys.stderr)
        return 1
    print("glm53_flash_nvidia_engine_args_preflight=passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
