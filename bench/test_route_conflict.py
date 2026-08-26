import unittest
from unittest import mock

import route_conflict


def ordinary_snapshot(**changes):
    result = {key: 0.0 for key in route_conflict.ENGINE_COUNTERS}
    result.update(changes)
    return result


def speculative_snapshot(**changes):
    result = {
        "draft_steps": 0.0,
        "proposed_tokens": 0.0,
        "accepted_tokens": 0.0,
        "generation_tokens": 0.0,
        "finished_requests": 0.0,
        "accepted_per_position": {},
    }
    result.update(changes)
    return result


class RouteConflictTests(unittest.TestCase):
    def config(self):
        return {
            "base": "http://engine",
            "model": "model",
            "blockers": 2,
            "blocker_tokens": 32,
            "runs": 1,
            "token": "secret",
            "salt": "fresh",
            "context_tokens": 100,
            "probe_tokens": 8,
            "metrics_urls": ["http://a/metrics", "http://b/metrics"],
            "require_reconciled": True,
            "settle_seconds": 0,
        }

    def test_run_waits_for_every_blocker_and_retains_usage(self):
        calls = []

        def fake_request(base, model, token, system, user, max_tokens, output, key, ready=None):
            calls.append(key)
            output[key] = {
                "ok": True,
                "route": "0",
                "ttft_ms": 5.0,
                "wall_ms": 10.0,
                "prompt_tokens": 11,
                "cached_tokens": 0,
                "completion_tokens": max_tokens,
                "request_bytes": 100,
            }
            if ready is not None:
                ready.set()

        with mock.patch("route_conflict.stream_request", side_effect=fake_request):
            output, window = route_conflict.run_once(0, self.config())
        self.assertEqual({"warm", "blocker-0", "blocker-1", "probe"}, set(calls))
        self.assertEqual(32, output["blocker-0"]["completion_tokens"])
        self.assertEqual(32, output["blocker-1"]["completion_tokens"])
        self.assertGreater(window, 0)

    def test_aggregate_engine_delta_sums_two_engines_and_derived_metrics(self):
        before = [ordinary_snapshot(), ordinary_snapshot()]
        after = [
            ordinary_snapshot(
                preemptions=1,
                prompt_tokens=100,
                cached_prompt_tokens=25,
                prefix_queries=100,
                prefix_hits=25,
                queue_seconds_sum=0.2,
                queue_samples=2,
                prefill_seconds_sum=0.4,
                prefill_samples=2,
            ),
            ordinary_snapshot(
                preemptions=2,
                prompt_tokens=200,
                cached_prompt_tokens=50,
                prefix_queries=200,
                prefix_hits=50,
                queue_seconds_sum=0.4,
                queue_samples=4,
                prefill_seconds_sum=0.8,
                prefill_samples=4,
            ),
        ]
        result = route_conflict.aggregate_engine_delta(before, after)
        self.assertEqual(3, result["preemptions"])
        self.assertEqual(300, result["prompt_tokens"])
        self.assertEqual(300, result["prefix_queries"])
        self.assertEqual(75, result["prefix_hits"])
        self.assertEqual(25.0, result["prefix_hit_pct"])
        self.assertEqual(100.0, result["queue_ms_mean"])
        self.assertEqual(200.0, result["prefill_ms_mean"])

    def test_speculative_snapshots_combine_positions(self):
        result = route_conflict.combine_speculative_snapshots(
            [
                speculative_snapshot(draft_steps=2, accepted_per_position={0: 2, 1: 1}),
                speculative_snapshot(draft_steps=3, accepted_per_position={0: 3, 2: 1}),
            ]
        )
        self.assertEqual(5, result["draft_steps"])
        self.assertEqual({0: 5, 1: 1, 2: 1}, result["accepted_per_position"])

    def test_reconciliation_ignores_response_cached_tokens(self):
        records = [
            {
                "ok": True,
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cached_tokens": 99,
            },
            {
                "ok": True,
                "prompt_tokens": 200,
                "completion_tokens": 30,
                "cached_tokens": 199,
            },
        ]
        engine = {"prompt_tokens": 300, "queue_samples": 2, "prefill_samples": 2}
        result = route_conflict.build_reconciliation(
            records, engine, {"state": "enabled", "reconciled": True}
        )
        self.assertTrue(result["reconciled"])
        self.assertFalse(result["cached_tokens_authoritative"])
        self.assertNotIn("cached_tokens_match", result)

    def test_summary_reports_useful_blocker_work_and_contamination(self):
        config = self.config()
        outputs = [
            {
                "warm": {
                    "ok": True,
                    "route": "0",
                    "ttft_ms": 1,
                    "prompt_tokens": 10,
                    "completion_tokens": 2,
                    "cached_tokens": 0,
                    "request_bytes": 10,
                },
                "blocker-0": {
                    "ok": True,
                    "route": "0",
                    "ttft_ms": 10,
                    "prompt_tokens": 20,
                    "completion_tokens": 30,
                    "cached_tokens": 0,
                    "request_bytes": 20,
                },
                "blocker-1": {
                    "ok": True,
                    "route": "1",
                    "ttft_ms": 20,
                    "prompt_tokens": 20,
                    "completion_tokens": 30,
                    "cached_tokens": 0,
                    "request_bytes": 20,
                },
                "probe": {
                    "ok": True,
                    "route": "0",
                    "ttft_ms": 5,
                    "prompt_tokens": 10,
                    "completion_tokens": 8,
                    "cached_tokens": 0,
                    "request_bytes": 10,
                },
            }
        ]
        before = (
            [ordinary_snapshot(), ordinary_snapshot()],
            [speculative_snapshot(), speculative_snapshot()],
        )
        after = (
            [
                ordinary_snapshot(
                    prompt_tokens=30,
                    queue_samples=2,
                    prefill_samples=2,
                ),
                ordinary_snapshot(
                    prompt_tokens=31,  # one contaminating prompt token
                    queue_samples=2,
                    prefill_samples=2,
                ),
            ],
            [
                speculative_snapshot(
                    draft_steps=2,
                    proposed_tokens=6,
                    accepted_tokens=4,
                    generation_tokens=35,
                    finished_requests=2,
                ),
                speculative_snapshot(
                    draft_steps=2,
                    proposed_tokens=6,
                    accepted_tokens=4,
                    generation_tokens=35,
                    finished_requests=2,
                ),
            ],
        )
        result = route_conflict.summarize_runs(config, outputs, [1000], (before, after))
        self.assertEqual(60.0, result["blocker_aggregate_output_tok_s"])
        self.assertEqual(20, result["blocker_ttft_ms_p95"])
        self.assertEqual(40, result["blocker_prompt_tokens"])
        self.assertTrue(result["reconciliation"]["contaminated"])
        self.assertFalse(result["reconciliation"]["reconciled"])
        self.assertEqual("enabled", result["speculative"]["state"])


if __name__ == "__main__":
    unittest.main()
