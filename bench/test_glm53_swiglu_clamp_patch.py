import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PATCH_PATH = ROOT / "deploy/glm53_flash_sm120/patch-swiglu-clamp.py"
SPEC = importlib.util.spec_from_file_location("patch_swiglu_clamp", PATCH_PATH)
assert SPEC and SPEC.loader
PATCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PATCH)

RUNNER = '''
    return launch_sm120_moe(
        a=x,
        activation="silu",
        quant_mode="w4a16",
        source_format="modelopt_e4m3_k32",
        _prepared_weights=quant_info.prepared_weights,
    )
'''

DISPATCH = '''
def _launch_sm120_w4a16_moe(
    *,
    fast_math: bool = True,
    activation: str = "silu",
    source_format: str = "modelopt",
    _workspace=None,
    _prepared_weights=None,
) -> torch.Tensor:
    prepared = (None)
    return run_w4a16_moe(
        expert_map=workspace.expert_map,
        fast_math=fast_math,
    )

    if quant_mode == "w4a16":
        return _launch_sm120_w4a16_moe(
            fast_math=fast_math,
            activation=activation,
            source_format=source_format,
            _workspace=_workspace,
            _prepared_weights=_prepared_weights,
        )

    if quant_mode == "nvfp4" and input_global_scale is not None:
        pass
'''


class Glm53SwigluClampPatchTests(unittest.TestCase):
    def test_runner_passes_the_model_clamp_to_the_sm120_launcher(self):
        patched = PATCH.patch_runner(RUNNER)
        self.assertIn("runner_config.gemm1_clamp_limit or runner_config.swiglu_limit", patched)
        self.assertLess(patched.index("swiglu_limit="), patched.index('quant_mode="w4a16"'))

    def test_dispatch_forwards_the_clamp_through_every_hop(self):
        patched = PATCH.patch_dispatch(DISPATCH)
        self.assertIn("    swiglu_limit: float | None = None,\n    source_format", patched)
        self.assertIn("fast_math=fast_math,\n        swiglu_limit=swiglu_limit,\n    )", patched)
        self.assertIn("activation=activation,\n            swiglu_limit=swiglu_limit,", patched)
        compile(patched, "dispatch", "exec")

    def test_patch_fails_closed_when_the_pinned_shape_changes(self):
        with self.assertRaises(SystemExit):
            PATCH.patch_runner("unexpected upstream source")
        with self.assertRaises(SystemExit):
            PATCH.patch_dispatch(RUNNER)


if __name__ == "__main__":
    unittest.main()
