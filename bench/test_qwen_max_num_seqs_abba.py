import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
RUNNER = ROOT / "bench" / "qwen38_max_num_seqs_abba.sh"


class QwenMaxNumSeqsAbbaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = RUNNER.read_text()

    def test_candidate_is_exact_and_changes_only_the_sequence_limit(self):
        self.assertIn("expected_kv_bytes=40190174004", self.source)
        self.assertIn("expected_kv_tokens=2667258", self.source)
        self.assertIn('compose 16 "$single_upstream" up -d --no-deps --force-recreate', self.source)
        self.assertIn('wait_engine 16 || fail', self.source)
        self.assertIn('check_engine_shape 64 || fail', self.source)

    def test_correctness_authority_is_staged_before_mutation(self):
        self.assertIn("agent_cases_v1.jsonl", self.source)
        self.assertIn('"$experiment_dir/agent_cases_v1.jsonl"', self.source)
        preflight = self.source.index("run_agent seq64-preflight")
        candidate = self.source.index("candidate_started=$(date +%s)")
        candidate_agent = self.source.index("run_agent seq16-candidate")
        self.assertLess(preflight, candidate)
        self.assertGreater(candidate_agent, candidate)

    def test_workload_is_order_balanced_and_reconciled(self):
        for label in ("seq64-a1", "seq16-b1", "seq16-b2", "seq64-a2"):
            self.assertIn(f"run_code_round {label}", self.source)
        self.assertIn("for concurrency in 1 8 16 32", self.source)
        self.assertIn("BENCH_REQUIRE_RECONCILED_SPECULATION=1", self.source)
        self.assertIn("Experiment namespace: ${label}-c${concurrency}", self.source)

    def test_exit_path_restores_exact_engine_and_two_replica_lb(self):
        self.assertIn("trap rollback EXIT", self.source)
        rollback = self.source.index("rollback()")
        body = self.source[rollback : self.source.index("trap rollback EXIT")]
        self.assertIn('compose 64 "$single_upstream"', body)
        self.assertIn("wait_engine 64", body)
        self.assertIn('recreate_lb "$all_upstreams" 2', body)
        self.assertIn("record_engine final", body)


if __name__ == "__main__":
    unittest.main()
