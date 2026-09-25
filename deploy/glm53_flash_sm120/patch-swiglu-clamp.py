#!/usr/bin/env python3
"""Route GLM-5.3's routed-expert SwiGLU clamp into the SM120 W4A16 MoE kernel.

The checkpoint declares ``swiglu_limit = 10.0`` and SGLang builds the routed
experts' ``MoeRunnerConfig`` with it, but two hops drop it on the b12x W4A16
path: SGLang's ``_run_flashinfer_b12x_w4a16`` never passes it to
``launch_sm120_moe``, and FlashInfer 0.7.0's ``_launch_sm120_w4a16_moe`` never
forwards it to ``run_w4a16_moe``. The kernel itself already implements the
GLM semantics (gate <= L, -L <= up <= L) behind ``has_swiglu_limit``; this patch
only restores the plumbing. Shared experts and dense MLPs already clamp in
``glm5_next.swiglu_clamped`` and are untouched.
"""

from __future__ import annotations

import hashlib
from pathlib import Path


RUNNER = Path(
    "/opt/sglang-source/python/sglang/srt/layers/moe/moe_runner/flashinfer_cutlass.py"
)
RUNNER_SHA256 = "5e3cc610ecadf15312150244c668f9658bc3c67fc8a6e51bb0a442ee5698f321"

DISPATCH = Path(
    "/opt/sglang/lib/python3.12/site-packages/flashinfer/fused_moe/cute_dsl/"
    "blackwell_sm12x/moe_dispatch.py"
)
DISPATCH_SHA256 = "4574a1bfeacec80c23fa6e1886c1f6f704607f238bbac72bf53f305aa06ad486"


def replace_once(source: str, old: str, new: str, what: str) -> str:
    if source.count(old) != 1:
        raise SystemExit(f"pinned source shape changed: {what}")
    return source.replace(old, new, 1)


def patch_runner(source: str) -> str:
    return replace_once(
        source,
        """        activation="silu",
        quant_mode="w4a16",
        source_format="modelopt_e4m3_k32",
        _prepared_weights=quant_info.prepared_weights,
    )
""",
        """        activation="silu",
        swiglu_limit=(
            float(runner_config.gemm1_clamp_limit or runner_config.swiglu_limit)
            if (runner_config.gemm1_clamp_limit or runner_config.swiglu_limit)
            else None
        ),
        quant_mode="w4a16",
        source_format="modelopt_e4m3_k32",
        _prepared_weights=quant_info.prepared_weights,
    )
""",
        "sglang b12x W4A16 launch",
    )


def patch_dispatch(source: str) -> str:
    source = replace_once(
        source,
        """    fast_math: bool = True,
    activation: str = "silu",
    source_format: str = "modelopt",
    _workspace=None,
    _prepared_weights=None,
) -> torch.Tensor:
    prepared = (""",
        """    fast_math: bool = True,
    activation: str = "silu",
    swiglu_limit: float | None = None,
    source_format: str = "modelopt",
    _workspace=None,
    _prepared_weights=None,
) -> torch.Tensor:
    prepared = (""",
        "W4A16 launcher signature",
    )
    source = replace_once(
        source,
        """        expert_map=workspace.expert_map,
        fast_math=fast_math,
    )
""",
        """        expert_map=workspace.expert_map,
        fast_math=fast_math,
        swiglu_limit=swiglu_limit,
    )
""",
        "W4A16 kernel call",
    )
    return replace_once(
        source,
        """            fast_math=fast_math,
            activation=activation,
            source_format=source_format,
            _workspace=_workspace,
            _prepared_weights=_prepared_weights,
        )

    if quant_mode == "nvfp4\"""",
        """            fast_math=fast_math,
            activation=activation,
            swiglu_limit=swiglu_limit,
            source_format=source_format,
            _workspace=_workspace,
            _prepared_weights=_prepared_weights,
        )

    if quant_mode == "nvfp4\"""",
        "launch_sm120_moe W4A16 branch",
    )


def apply(path: Path, expected: str, patch) -> str:
    source = path.read_text()
    actual = hashlib.sha256(source.encode()).hexdigest()
    if actual != expected:
        raise SystemExit(f"{path}: expected SHA-256 {expected}, got {actual}")
    patched = patch(source)
    compile(patched, str(path), "exec")
    path.write_text(patched)
    return hashlib.sha256(patched.encode()).hexdigest()


def main() -> None:
    for path, expected, patch in (
        (RUNNER, RUNNER_SHA256, patch_runner),
        (DISPATCH, DISPATCH_SHA256, patch_dispatch),
    ):
        print(f"{path}: {apply(path, expected, patch)}")


if __name__ == "__main__":
    main()
