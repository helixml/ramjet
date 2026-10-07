import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy/dsv41_flash_h200"
HAS_DOCKER = shutil.which("docker") is not None


def validate(directory):
    return subprocess.run(
        ["python3", str(directory / "validate-compose.py")],
        cwd=directory,
        check=False,
        capture_output=True,
        text=True,
    )


class Dsv41H200DeployTests(unittest.TestCase):
    @unittest.skipUnless(HAS_DOCKER, "Docker Compose is validated in the deployment lane")
    def test_compose_semantic_validator_passes(self):
        result = validate(DEPLOY)
        self.assertEqual(result.returncode, 0, result.stderr)

    @unittest.skipUnless(HAS_DOCKER, "Docker Compose is validated in the deployment lane")
    def test_validator_fails_closed_on_measured_regressions(self):
        compose = (DEPLOY / "docker-compose.yaml").read_text()
        for name, old, new in (
            ("fp8 dense layers", "${SGLANG_BLOCK_FP8_DEQUANT_BF16:-1}", "${SGLANG_BLOCK_FP8_DEQUANT_BF16:-0}"),
            ("hard numa membind", "${SGLANG_NUMA_MEM_PREFERRED:-1}", "${SGLANG_NUMA_MEM_PREFERRED:-0}"),
            ("shared /dev/shm", "  shm_size: 64gb\n", "  shm_size: 64gb\n  ipc: host\n"),
            ("1M context with DSpark", "    DS_CONTEXT_LENGTH: ${DS_CONTEXT_LENGTH:-262144}", "    DS_CONTEXT_LENGTH: ${DS_CONTEXT_LENGTH:-1048576}"),
            ("no DSpark", "${DS_SPEC_ARGS:---speculative-algorithm DSPARK --speculative-dspark-block-size 5}", "${DS_SPEC_ARGS:- }"),
            ("fp4 activations", "${DS_MOE_PRECISION:-fp8}", "${DS_MOE_PRECISION:-bf16}"),
            ("no patches", "${DS_PATCHES:-patch_bf16_dense.py patch_numa_preferred.py}", "${DS_PATCHES:-}"),
            ("absolute routing", "${RJ_ROUTE_AFFINITY_BASIS:-marginal}", "${RJ_ROUTE_AFFINITY_BASIS:-absolute}"),
            ("lane on by default", "${RJ_ROUTE_LONG_PROMPT_BYTES:-}", "${RJ_ROUTE_LONG_PROMPT_BYTES:-1000000}"),
            ("dp attention", "${DS_EXTRA_ARGS:-}", "${DS_EXTRA_ARGS:---enable-dp-attention}"),
            ("shared gpu", 'device_ids: ["4", "5", "6", "7"]', 'device_ids: ["3", "4", "5", "6"]'),
            ("wrong socket", 'DS_NUMA_NODES: "4 5 6 7"', 'DS_NUMA_NODES: "0 1 2 3"'),
            ("no sys_nice", "cap_add: [SYS_NICE]", "cap_add: []"),
            ("no patch loop", "for p in $$DS_PATCHES; do python3 /opt/dsv41-patches/$$p /sgl-workspace/sglang/python/sglang; done;", ""),
            ("no context flag", "--context-length=$$DS_CONTEXT_LENGTH", ""),
            ("hard-coded tp", "--tp-size=$$DS_TP --ep-size=$$DS_EP", "--tp-size=4 --ep-size=4"),
            ("memory fraction", "${DS_MEM_FRACTION:-0.75}}", "${DS_MEM_FRACTION:-0.95}}"),
            ("generating health", 'SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION: "0"', 'SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION: "1"'),
        ):
            with self.subTest(name), tempfile.TemporaryDirectory() as temporary:
                self.assertEqual(compose.count(old), 1, old)
                directory = pathlib.Path(temporary)
                shutil.copy(DEPLOY / "validate-compose.py", directory)
                shutil.copytree(DEPLOY / "patches", directory / "patches")
                (directory / "docker-compose.yaml").write_text(compose.replace(old, new))
                result = validate(directory)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("validation failed", result.stderr)

    def test_patches_are_opt_in_and_fail_on_a_missing_anchor(self):
        for patch in sorted((DEPLOY / "patches").glob("*.py")):
            with self.subTest(patch=patch.name):
                text = patch.read_text()
                compile(text, str(patch), "exec")
                self.assertIn("os.environ.get(", text)
                self.assertIn("raise SystemExit", text)
                with tempfile.TemporaryDirectory() as temporary:
                    root = pathlib.Path(temporary)
                    for rel in ("srt/layers/quantization/fp8.py", "srt/utils/numa_utils.py"):
                        (root / rel).parent.mkdir(parents=True, exist_ok=True)
                        (root / rel).write_text("# an SGLang release without the anchor\n")
                    result = subprocess.run(
                        ["python3", str(patch), temporary], check=False, capture_output=True, text=True
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("anchor", result.stdout + result.stderr)

    def test_operational_scripts_are_valid_bash_and_host_neutral(self):
        for script in sorted(DEPLOY.glob("*.sh")):
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)
                self.assertNotRegex(script.read_text(), r"(?m)^\s*(?:export\s+)?\w+=/")

    def test_recipe_documents_measured_decisions(self):
        readme = (DEPLOY / "README.md").read_text()
        for required in (
            "RJ_ROUTE_AFFINITY_BASIS=marginal",
            "SGLANG_BLOCK_FP8_DEQUANT_BF16=1",
            "SGLANG_NUMA_MEM_PREFERRED=1",
            "multi_tokenizer_args_<pid>",
            "shmem_enabled",
            "agent_swarm_bench.py",
            "127.0.0.1:8070",
            "127.0.0.1:8071",
        ):
            self.assertIn(required, readme)


if __name__ == "__main__":
    unittest.main()
