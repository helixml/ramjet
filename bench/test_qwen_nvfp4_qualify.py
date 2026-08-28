import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
RUNNER = ROOT / "bench" / "qwen38_nvfp4_qualify.sh"
RECOVERY = ROOT / "bench" / "qwen38_restore_baseline.sh"


class QwenNvfp4QualifyTests(unittest.TestCase):
    def test_runner_is_guarded_single_engine_and_exact_rollback(self):
        text = RUNNER.read_text()
        self.assertIn("RAMJET_GPU_GUARD_ACTIVE", text)
        self.assertIn("ramjet-node06-deployment.lock", text)
        self.assertIn('engine=qwen38flashnext-b', text)
        self.assertIn('peer=qwen38flashnext-a', text)
        self.assertIn('single_upstream=', text)
        self.assertIn('trap rollback EXIT', text)
        self.assertIn('wait_engine check_baseline_shape check_baseline_kv', text)
        self.assertNotIn("docker compose down", text)

    def test_candidate_gates_recipe_and_correctness_surfaces(self):
        text = RUNNER.read_text()
        for required in (
            "qwen38_nvfp4_model_verify.py",
            "qwen38_nvfp4_args_preflight.py",
            "--moe-backend=marlin",
            "agent_cases_v1.jsonl",
            "agent_cases_v2_sessions.jsonl",
            "agent_cases_v2_deep_context.jsonl",
            "multimodal_smoke.py",
            "engine_greedy_ab.py",
            "codebench.py",
        ):
            self.assertIn(required, text)
        self.assertIn(".candidate_correct >= 7", text)
        self.assertIn(".baseline_correct >= 7", text)
        self.assertIn(".candidate_correct >= .baseline_correct", text)
        self.assertNotIn(".candidate_correct == 8", text)

    def test_recovery_is_guarded_and_restores_only_b_then_the_lb(self):
        text = RECOVERY.read_text()
        self.assertIn("RAMJET_GPU_GUARD_ACTIVE", text)
        self.assertIn("ramjet-node06-deployment.lock", text)
        self.assertIn('engine=qwen38flashnext-b', text)
        self.assertIn('peer=qwen38flashnext-a', text)
        self.assertIn('up -d --no-deps --force-recreate "$engine"', text)
        self.assertIn('up -d --no-deps --force-recreate ds4-loadbalancer', text)
        self.assertNotIn("docker compose down", text)


if __name__ == "__main__":
    unittest.main()
