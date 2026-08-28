import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "bench" / "qwen38_nvfp4_model_verify.py"
SPEC = importlib.util.spec_from_file_location("qwen38_nvfp4_model_verify", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load NVFP4 model verifier")
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)


class QwenNvfp4ModelVerifyTests(unittest.TestCase):
    def fixture(self):
        temporary = tempfile.TemporaryDirectory()
        root = pathlib.Path(temporary.name).resolve()
        metadata = root / ".cache" / "huggingface" / "download"
        metadata.mkdir(parents=True)
        config = json.dumps(
            {
                "architectures": ["Qwen4ExpForConditionalGeneration"],
                "quantization_config": {
                    "quant_method": "modelopt",
                    "quant_algo": "NVFP4",
                    "group_size": 16,
                    "calibration_applied": True,
                },
            }
        ).encode()
        weight = b"immutable-weight-fixture"
        for name, content, kind in (
            ("config.json", config, "git"),
            ("weights.safetensors", weight, "lfs"),
        ):
            path = root / name
            path.write_bytes(content)
            if kind == "git":
                digest = hashlib.sha1(f"blob {len(content)}\0".encode() + content).hexdigest()
            else:
                digest = hashlib.sha256(content).hexdigest()
            (metadata / f"{name}.metadata").write_text(
                f"{verifier.REVISION}\n{digest}\n1.0\n"
            )
        return temporary, root, len(config) + len(weight), len(weight)

    def test_exact_local_dir_authority_passes(self):
        temporary, root, total, safetensors = self.fixture()
        self.addCleanup(temporary.cleanup)
        with (
            mock.patch.object(verifier, "FILES", {"config.json", "weights.safetensors"}),
            mock.patch.object(verifier, "TOTAL_BYTES", total),
            mock.patch.object(verifier, "SAFETENSOR_BYTES", safetensors),
        ):
            result = verifier.verify(root)
        self.assertTrue(result["verified"])
        self.assertEqual(result["files"], 2)

    def test_content_and_file_set_drift_fail_closed(self):
        temporary, root, total, safetensors = self.fixture()
        self.addCleanup(temporary.cleanup)
        (root / "weights.safetensors").write_bytes(b"changed")
        with (
            mock.patch.object(verifier, "FILES", {"config.json", "weights.safetensors"}),
            mock.patch.object(verifier, "TOTAL_BYTES", total),
            mock.patch.object(verifier, "SAFETENSOR_BYTES", safetensors),
            self.assertRaisesRegex(SystemExit, "content digest"),
        ):
            verifier.verify(root)

        (root / "unexpected.txt").write_text("x")
        with (
            mock.patch.object(verifier, "FILES", {"config.json", "weights.safetensors"}),
            self.assertRaisesRegex(SystemExit, "file set"),
        ):
            verifier.verify(root)


if __name__ == "__main__":
    unittest.main()
