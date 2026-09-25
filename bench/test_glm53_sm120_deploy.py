import pathlib
import shutil
import subprocess
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy/glm53_flash_sm120"


class Glm53Sm120DeployTests(unittest.TestCase):
    @unittest.skipUnless(
        shutil.which("docker"),
        "Docker Compose is validated in the deployment lane",
    )
    def test_compose_semantic_validator_passes(self):
        result = subprocess.run(
            ["python3", str(DEPLOY / "validate-compose.py")],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_operational_scripts_are_valid_bash(self):
        for script in sorted(DEPLOY.glob("*.sh")):
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)

    def test_recipe_documents_isolation_and_parser_authority(self):
        readme = (DEPLOY / "README.md").read_text()
        for required in (
            "127.0.0.1:8062",
            "127.0.0.1:8063",
            "Qwen A",
            "GPUs 6-7",
            "Dockerfile.nullable-parser",
            "[\"string\", \"null\"]",
            "1,500-second",
        ):
            self.assertIn(required, readme)

    def test_recipe_documents_multimodal_client_contract(self):
        readme = (DEPLOY / "README.md").read_text()
        for required in (
            "--enable-multimodal",
            "Multimodal client contract",
            '"input": ["text", "image"]',
            "this model does not support image input",
        ):
            self.assertIn(required, readme)

    def test_loader_defers_only_the_inference_runtime_budget(self):
        wrapper = (DEPLOY / "node06-guarded-rollout.sh").read_text()
        canary = (DEPLOY / "node06-canary.sh").read_text()
        self.assertIn("--runtime-start-signal", wrapper)
        self.assertIn("--runtime-start-timeout-seconds 2400", wrapper)
        self.assertLess(
            canary.index("start_inference_budget\n"),
            canary.index('glm_serve_probe || fail'),
        )

    def test_prefill_candidate_bounds_memory_and_defers_inference_budget(self):
        compose = (DEPLOY / "docker-compose.yaml").read_text()
        rollout = (DEPLOY / "node06-prefill-6144-rollout.sh").read_text()
        self.assertIn("${GLM53_CHUNKED_PREFILL_SIZE:-6144}", compose)
        self.assertIn("${GLM53_MAX_PREFILL_TOKENS:-6144}", compose)
        self.assertIn("${GLM53_MAX_TOTAL_TOKENS:-500000}", compose)
        self.assertIn("GLM53_CHUNKED_PREFILL_SIZE=6144", rollout)
        self.assertIn("GLM53_MAX_PREFILL_TOKENS=6144", rollout)
        self.assertIn("GLM53_MAX_TOTAL_TOKENS=500000", rollout)
        self.assertIn('.State.Status == "running"', rollout)
        self.assertIn("exact failed 8K warmup", rollout)
        self.assertLess(
            rollout.index("start_inference_budget\n"),
            rollout.index('glm_probe || fail'),
        )

    def test_second_replica_rollout_is_additive_and_defers_inference_budget(self):
        compose = (DEPLOY / "docker-compose.yaml").read_text()
        rollout = (DEPLOY / "node06-second-replica-rollout.sh").read_text()
        self.assertIn("glm53sm120-b:", compose)
        self.assertIn("glm53sm120-c:", compose)
        self.assertIn('device_ids: ["6", "7"]', compose)
        self.assertIn('"127.0.0.1:8063:8000"', compose)
        self.assertIn('compose_run up -d --no-deps "$glm_c"', rollout)
        self.assertNotIn('force-recreate "$glm_b"', rollout)
        self.assertNotIn('force-recreate "$qwen"', rollout)
        self.assertNotIn("docker.sock", rollout)
        restore = (DEPLOY / "node06-restore-qwen-b.sh").read_text()
        self.assertIn("refusing Qwen B restore", restore)
        self.assertLess(
            rollout.index("start_inference_budget\n"),
            rollout.index('glm_c_probe || fail'),
        )

    def test_engine_rollout_keeps_a_peer_serving_and_defers_inference_budget(self):
        rollout = (DEPLOY / "node06-engine-rollout.sh").read_text()
        self.assertIn("refusing to remove the last GLM replica", rollout)
        self.assertIn("ramjet-node06-deployment.lock", rollout)
        self.assertIn('up -d --no-deps --force-recreate "$service"', rollout)
        self.assertNotIn("ds4-loadbalancer", rollout)
        self.assertLess(
            rollout.index("start_inference_budget\n"),
            rollout.index("probe || fail"),
        )

    def test_recipe_documents_cache_capacity_and_clamp(self):
        readme = (DEPLOY / "README.md").read_text()
        compose = (DEPLOY / "docker-compose.yaml").read_text()
        for required in ("Prefix-cache capacity", "Routed-expert SwiGLU clamp", "Dockerfile.swiglu-clamp"):
            self.assertIn(required, readme)
        self.assertIn("${GLM53_MAMBA_MAX_STATES_PER_PATH:-2}", compose)
        self.assertIn("--enable-hierarchical-cache", compose)


if __name__ == "__main__":
    unittest.main()
