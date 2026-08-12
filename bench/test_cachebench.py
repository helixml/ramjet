import unittest

from cachebench import (
    aggregate_engine_delta,
    cache_outcome,
    latency_by_outcome,
    nonnegative_delta,
    parse_apps,
    reconcile,
    summarize,
    workload_coordinates,
)


class CacheBenchTest(unittest.TestCase):
    def test_workload_round_robins_apps_before_reuse(self):
        self.assertEqual(
            list(workload_coordinates(2, 2, 1)),
            [(0, 0, 1), (1, 0, 1), (0, 1, 1), (1, 1, 1)],
        )

    def test_app_list_and_cache_outcomes_are_bounded(self):
        self.assertEqual(parse_apps("1,4,8"), [1, 4, 8])
        for prompt, cached, expected in [
            (0, 0, "unknown"),
            (10, -1, "unknown"),
            (10, 0, "cold"),
            (10, 8, "partial"),
            (10, 10, "full"),
        ]:
            self.assertEqual(cache_outcome(prompt, cached), expected)

    def test_nonnegative_delta_fails_closed_on_reset_or_missing_series(self):
        self.assertEqual(
            nonnegative_delta({"a": 1, "b": 2}, {"a": 4, "b": 1}, ["a", "b"]),
            {"a": 3, "b": None},
        )
        self.assertIsNone(nonnegative_delta(None, {}, ["a"]))

    def test_engine_deltas_aggregate_both_replicas(self):
        keys = [
            "preemptions",
            "prompt_tokens",
            "cached_prompt_tokens",
            "prefix_queries",
            "prefix_hits",
            "queue_seconds_sum",
            "queue_samples",
            "prefill_seconds_sum",
            "prefill_samples",
        ]
        before = [{key: 0 for key in keys}, {key: 10 for key in keys}]
        after = [{key: 2 for key in keys}, {key: 13 for key in keys}]
        result = aggregate_engine_delta(before, after)
        self.assertEqual(result["prompt_tokens"], 5)
        self.assertEqual(result["cached_prompt_tokens"], 5)
        self.assertEqual(result["queue_ms_mean"], 1000)
        self.assertEqual(result["prefix_hit_pct"], 100)
        self.assertIsNone(aggregate_engine_delta([], []))

    def test_reconciliation_requires_every_authoritative_view(self):
        records = [
            {
                "ok": True,
                "prompt_tokens": 100,
                "cached_tokens": 64,
            }
        ]
        lb = {
            "prompt_tokens": 100,
            "cached_prompt_tokens": 64,
            "cache_requests": 1,
            "cache_ttft_samples": 1,
        }
        engine = {
            "prompt_tokens": 100,
            "cached_prompt_tokens": 64,
            "prefix_queries": 100,
            "prefix_hits": 64,
            "queue_samples": 1,
            "prefill_samples": 1,
        }
        self.assertTrue(reconcile(records, lb, engine)["consistent"])
        engine["prefix_hits"] = 63
        mismatch = reconcile(records, lb, engine)
        self.assertFalse(mismatch["consistent"])
        self.assertEqual(mismatch["max_spread"], 1)
        self.assertFalse(reconcile(records, None, engine)["consistent"])

    def test_latency_summary_stays_bounded_by_cache_outcome(self):
        records = [
            {"ok": True, "cache_outcome": "cold", "ttft_ms": 100},
            {"ok": True, "cache_outcome": "cold", "ttft_ms": 200},
            {"ok": True, "cache_outcome": "partial", "ttft_ms": 50},
            {"ok": False, "cache_outcome": "unknown", "ttft_ms": 999},
        ]
        self.assertEqual(
            latency_by_outcome(records, "ttft_ms"),
            {
                "cold": {"count": 2, "p50": 150.0, "p95": 200},
                "partial": {"count": 1, "p50": 50, "p95": 50},
            },
        )

    def test_summary_is_content_free_and_reports_reuse_distance(self):
        records = [
            {
                "app": app,
                "session": session,
                "turn": 1,
                "ok": True,
                "route": str(app),
                "prompt_tokens": 100,
                "cached_tokens": 0 if session == 0 else 80,
                "completion_tokens": 2,
                "cache_outcome": "cold" if session == 0 else "partial",
                "ttft_ms": 100 + session,
                "wall_ms": 200 + session,
            }
            for app in range(2)
            for session in range(2)
        ]
        summary = summarize(records, 2, 2, 1, 32, 1.0, None, None, 0)
        self.assertEqual(summary["cache_hit_pct"], 40)
        self.assertEqual(summary["completion_tokens"], 8)
        self.assertEqual(summary["total_tok_s"], 408)
        self.assertEqual(summary["request_reuse_pct"], 50)
        self.assertEqual(summary["reuse_distance_requests_max"], 0)
        self.assertEqual(summary["route_split"], {"0": 2, "1": 2})
        encoded = str(summary).lower()
        for forbidden in ("message", "content", "fingerprint", "token_ids"):
            self.assertNotIn(forbidden, encoded)


if __name__ == "__main__":
    unittest.main()
