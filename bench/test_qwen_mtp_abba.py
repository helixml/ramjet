import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
RUNNER = ROOT / "bench" / "qwen38_mtp_abba.sh"
PREFLIGHT = ROOT / "bench" / "qwen38_mtp_args_preflight.py"


class QwenMtpAbbaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = RUNNER.read_text()
        cls.preflight = PREFLIGHT.read_text()

    def test_candidate_is_a_hash_pinned_single_file_deletion(self):
        self.assertIn("candidate_compose_sha=7457bafc", self.source)
        self.assertIn("standalone Compose file, never an overlay", self.source)
        self.assertIn("awk '!/^[[:space:]]*- --speculative-config=/'", self.source)
        self.assertIn('"$candidate_compose_sha"', self.source)

    def test_parser_preflight_runs_before_any_engine_mutation(self):
        parser = self.source.index("/probe/qwen38_mtp_args_preflight.py")
        mutation = self.source.index("candidate_started=$(date +%s)")
        self.assertLess(parser, mutation)
        self.assertIn("engine.speculative_config is not None", self.preflight)
        self.assertIn("engine.kv_cache_memory_bytes != 40190174004", self.preflight)

    def test_correctness_preflight_and_abba_are_explicit(self):
        baseline_agent = self.source.index("run_agent mtp-on-preflight enabled")
        mutation = self.source.index("candidate_started=$(date +%s)")
        self.assertLess(baseline_agent, mutation)
        for cell in (
            "run_code_round mtp-on-a1 enabled",
            "run_code_round mtp-off-b1 disabled",
            "run_code_round mtp-off-b2 disabled",
            "run_code_round mtp-on-a2 enabled",
        ):
            self.assertIn(cell, self.source)
        self.assertIn("BENCH_REQUIRE_RECONCILED_SPECULATION=1", self.source)
        self.assertIn('--speculation-mode "$mode"', self.source)

    def test_candidate_and_rollback_require_exact_kv_shapes(self):
        self.assertIn("expected_mtp_kv_tokens=2667258", self.source)
        self.assertIn("expected_plain_kv_tokens=3033380", self.source)
        rollback = self.source[
            self.source.index("rollback()") : self.source.index("trap rollback EXIT")
        ]
        self.assertIn('compose "$compose_file" "$single_upstream"', rollback)
        self.assertIn("wait_engine enabled", rollback)
        self.assertIn('recreate_lb "$all_upstreams" 2', rollback)
        self.assertIn("record_engine final", rollback)


if __name__ == "__main__":
    unittest.main()
