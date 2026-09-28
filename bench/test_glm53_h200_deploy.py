import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy/glm53_flash_h200"
HAS_DOCKER = shutil.which("docker") is not None


def validate(directory):
    return subprocess.run(
        ["python3", str(directory / "validate-compose.py")],
        cwd=directory,
        check=False,
        capture_output=True,
        text=True,
    )


class Glm53H200DeployTests(unittest.TestCase):
    @unittest.skipUnless(HAS_DOCKER, "Docker Compose is validated in the deployment lane")
    def test_compose_semantic_validator_passes(self):
        result = validate(DEPLOY)
        self.assertEqual(result.returncode, 0, result.stderr)

    @unittest.skipUnless(HAS_DOCKER, "Docker Compose is validated in the deployment lane")
    def test_validator_fails_closed_on_measured_regressions(self):
        compose = (DEPLOY / "docker-compose.yaml").read_text()
        for name, old, new in (
            ("fp8 kv", "${GLM_KV_DTYPE:-bfloat16}", "${GLM_KV_DTYPE:-fp8_e4m3}"),
            ("absolute routing", "${RJ_ROUTE_AFFINITY_BASIS:-marginal}", "${RJ_ROUTE_AFFINITY_BASIS:-absolute}"),
            ("mixed chunk", "${GLM_EXTRA_ARGS:-}", "${GLM_EXTRA_ARGS:---enable-mixed-chunk}"),
            ("shared gpu", 'device_ids: ["4", "5", "6", "7"]', 'device_ids: ["3", "4", "5", "6"]'),
            ("wrong socket", 'GLM_NUMA_NODES: "4 5 6 7"', 'GLM_NUMA_NODES: "0 1 2 3"'),
            ("no sys_nice", "cap_add: [SYS_NICE]", "cap_add: []"),
            ("single tokenizer", "${GLM_TOKENIZER_WORKERS:-4}", "${GLM_TOKENIZER_WORKERS:-1}"),
            ("10s worker healthcheck", "${SGLANG_UVICORN_WORKER_HEALTHCHECK_TIMEOUT:-60}", "${SGLANG_UVICORN_WORKER_HEALTHCHECK_TIMEOUT:-10}"),
            ("generating health", 'SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION: "0"', 'SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION: "1"'),
        ):
            with self.subTest(name), tempfile.TemporaryDirectory() as temporary:
                self.assertEqual(compose.count(old), 1, old)
                directory = pathlib.Path(temporary)
                shutil.copy(DEPLOY / "validate-compose.py", directory)
                (directory / "docker-compose.yaml").write_text(compose.replace(old, new))
                result = validate(directory)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("validation failed", result.stderr)

    def test_operational_scripts_are_valid_bash_and_host_neutral(self):
        for script in sorted(DEPLOY.glob("*.sh")):
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)
                # Paths derive from GLM_H200_ROOT/$HOME, never a host-specific root.
                self.assertNotRegex(script.read_text(), r"(?m)^\s*(?:export\s+)?\w+=/")

    def test_recipe_documents_measured_decisions(self):
        readme = (DEPLOY / "README.md").read_text()
        for required in (
            "RJ_ROUTE_AFFINITY_BASIS=marginal",
            "bfloat16",
            "--enable-mixed-chunk",
            "--speculative-adaptive",
            "agent_swarm_bench.py",
            "127.0.0.1:8070",
            "127.0.0.1:8071",
        ):
            self.assertIn(required, readme)


if __name__ == "__main__":
    unittest.main()
