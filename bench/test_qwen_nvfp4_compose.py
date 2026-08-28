import hashlib
import importlib.util
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "bench" / "qwen38_nvfp4_compose.py"
SOURCE = ROOT / "deploy" / "qwen38_flash_next" / "docker-compose.yaml"
SPEC = importlib.util.spec_from_file_location("qwen38_nvfp4_compose", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load NVFP4 Compose renderer")
renderer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(renderer)


class QwenNvfp4ComposeTests(unittest.TestCase):
    def test_canonical_source_renders_exact_standalone_candidate(self):
        output = renderer.render(SOURCE.read_bytes())
        self.assertEqual(hashlib.sha256(output).hexdigest(), renderer.OUTPUT_SHA256)
        text = output.decode()
        self.assertIn("Inferact/Qwen3.8-Flash-Next-NVFP4", text)
        self.assertIn(f"--revision={renderer.REVISION}", text)
        self.assertIn("--moe-backend=marlin", text)
        self.assertIn("--max-num-seqs=16", text)
        self.assertIn("--gpu-memory-utilization=0.95", text)
        self.assertNotIn("--kv-cache-memory", text)
        self.assertNotIn("--speculative-config", text)
        self.assertIn('RJ_EXACT_ROUTE_CANARY_BPS: "0"', text)
        self.assertIn('RJ_EXACT_ROUTE_CANARY_KEY: ""', text)
        self.assertNotIn("RJ_EXACT_ROUTE_CANARY_BPS:-10000", text)
        self.assertNotIn("Qwen/Qwen3.8-Flash-Next-FP8", text)
        self.assertEqual(text.count("services:"), 1)
        self.assertEqual(text.count("\n  qwen38flashnext-a:\n"), 1)
        self.assertEqual(text.count("\n  qwen38flashnext-b:\n"), 1)

    def test_source_and_output_drift_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "canonical Qwen Compose bytes changed"):
            renderer.render(SOURCE.read_bytes() + b"\n")

        with tempfile.TemporaryDirectory() as temporary:
            output = pathlib.Path(temporary) / "candidate.yaml"
            output.write_text("existing")
            with self.assertRaises(FileExistsError):
                renderer.write_candidate(SOURCE, output)


if __name__ == "__main__":
    unittest.main()
