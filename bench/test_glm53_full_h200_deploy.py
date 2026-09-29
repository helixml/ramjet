import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy/glm53_h200"
HAS_DOCKER = shutil.which("docker") is not None


def validate(directory):
    return subprocess.run(
        ["python3", str(directory / "validate-compose.py")],
        cwd=directory,
        check=False,
        capture_output=True,
        text=True,
    )


class Glm53FullH200DeployTests(unittest.TestCase):
    @unittest.skipUnless(HAS_DOCKER, "Docker Compose is validated in the deployment lane")
    def test_compose_semantic_validator_passes(self):
        result = validate(DEPLOY)
        self.assertEqual(result.returncode, 0, result.stderr)

    @unittest.skipUnless(HAS_DOCKER, "Docker Compose is validated in the deployment lane")
    def test_validator_fails_closed_on_measured_regressions(self):
        compose = (DEPLOY / "docker-compose.yaml").read_text()
        for name, old, new in (
            ("rank 0 on numa node 0", "${GLM_NUMA_NODES:-1 1 2 3 4 5 6 7}", "${GLM_NUMA_NODES:-0 1 2 3 4 5 6 7}"),
            ("no sys_nice", "cap_add: [SYS_NICE]", "cap_add: []"),
            ("bf16 kv", "--kv-cache-dtype=fp8_e4m3", "--kv-cache-dtype=bfloat16"),
            ("tp8 attention", "--tp-size=8 --dp-size=8 --enable-dp-attention", "--tp-size=8"),
            ("fcfs", "--schedule-policy=hrrn", "--schedule-policy=fcfs"),
            ("no deepep", "${GLM_MOE_A2A_BACKEND:-deepep}", "${GLM_MOE_A2A_BACKEND:-none}"),
            ("oom memory", "${GLM_MEM_FRACTION:-0.85}", "${GLM_MEM_FRACTION:-0.88}"),
            ("host tier overflows node 1", "--hicache-size 32", "--hicache-size 48"),
            ("marginal routing", "${RJ_ROUTE_AFFINITY_BASIS:-relative}", "${RJ_ROUTE_AFFINITY_BASIS:-marginal}"),
            ("no rank probe", "${RJ_UPSTREAM_RANK_PROBE:-on}", "${RJ_UPSTREAM_RANK_PROBE:-off}"),
            ("rank map", "RJ_UPSTREAM_DP_RANKS: 0,1,2,3,4,5,6,7", "RJ_UPSTREAM_DP_RANKS: 0,1,2,3,4,5,6,6"),
            ("single tokenizer", "${GLM_TOKENIZER_WORKERS:-4}", "${GLM_TOKENIZER_WORKERS:-1}"),
            ("generating health", 'SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION: "0"', 'SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION: "1"'),
        ):
            with self.subTest(name), tempfile.TemporaryDirectory() as temporary:
                self.assertEqual(compose.count(old), 1, old)
                directory = pathlib.Path(temporary)
                shutil.copy(DEPLOY / "validate-compose.py", directory)
                (directory / "docker-compose.yaml").write_text(compose.replace(old, new))
                result = validate(directory)
                self.assertNotEqual(result.returncode, 0, name)
                self.assertIn("validation failed", result.stderr)

    def test_recipe_documents_measured_decisions(self):
        readme = (DEPLOY / "README.md").read_text()
        for required in (
            "RJ_ROUTE_AFFINITY_BASIS=relative",
            "RJ_UPSTREAM_DP_RANKS",
            "GLM_MOE_A2A_BACKEND=none",
            "fp8_e4m3",
            "Xid 94",
            "agent_swarm_bench.py",
            "127.0.0.1:8073",
        ):
            self.assertIn(required, readme)


if __name__ == "__main__":
    unittest.main()
