import io
import json
import unittest
from unittest import mock

from serving_cost_audit import audit, main, observation, percentile


def start(sequence, units, unix_ms=1_000):
    return {
        "v": 5,
        "event": "start",
        "seq": sequence,
        "unix_ms": unix_ms,
        "chosen": 0,
        "served_chosen": 0,
        "candidates": [
            {
                "upstream": 0,
                "rank": 0,
                "overlap_blocks": 0,
                "affinity_blocks": 0,
                "load_units": 0,
                "request_load_units": units,
                "healthy": True,
            }
        ],
    }


def finish(
    sequence,
    *,
    prompt,
    cached,
    ttft,
    duration,
    completion,
    unix_ms=2_000,
    result="complete",
    status=200,
):
    return {
        "v": 5,
        "event": "finish",
        "seq": sequence,
        "unix_ms": unix_ms,
        "result": result,
        "status": status,
        "upstream": 0,
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "ttft_ms": ttft,
        "duration_ms": duration,
        "completion_tokens": completion,
    }


class ServingCostAuditTest(unittest.TestCase):
    def test_percentile_handles_one_and_interpolates(self):
        self.assertEqual(percentile([7], 0.95), 7)
        self.assertEqual(percentile([10, 20], 0.50), 15)

    def test_observation_derives_phase_cost_without_overclaiming_full_hits(self):
        item = observation(
            start(1, 4),
            finish(1, prompt=100, cached=25, ttft=150, duration=350, completion=5),
        )
        self.assertEqual(item["uncached_tokens"], 75)
        self.assertEqual(item["service_ms_per_uncached_token"], 2)
        self.assertEqual(item["tpot_ms"], 50)
        self.assertEqual(item["cache_outcome"], "partial")

        full = observation(
            start(2, 1),
            finish(2, prompt=100, cached=100, ttft=20, duration=40, completion=2),
        )
        self.assertIsNone(full["service_ms_per_uncached_token"])
        self.assertEqual(full["cache_outcome"], "full")

    def test_invalid_or_unsuccessful_records_are_excluded(self):
        base = start(1, 1)
        self.assertIsNone(
            observation(
                base,
                finish(1, prompt=10, cached=11, ttft=20, duration=30, completion=2),
            )
        )
        self.assertIsNone(
            observation(
                base,
                finish(
                    1,
                    prompt=10,
                    cached=0,
                    ttft=20,
                    duration=30,
                    completion=2,
                    status=500,
                ),
            )
        )

    def test_audit_groups_load_and_computes_slo_goodput(self):
        parsed = [
            start(1, 1, 1_000),
            finish(
                1,
                prompt=100,
                cached=100,
                ttft=20,
                duration=60,
                completion=3,
                unix_ms=2_000,
            ),
            start(2, 4, 2_000),
            finish(
                2,
                prompt=100,
                cached=0,
                ttft=200,
                duration=600,
                completion=5,
                unix_ms=5_000,
            ),
        ]
        report = audit(parsed, ttft_slo_ms=100, tpot_slo_ms=50, gpu_count=2)
        self.assertEqual(report["records"]["cost_observations"], 2)
        self.assertEqual(set(report["by_request_load_units"]), {"1", "4"})
        self.assertEqual(report["by_cache_outcome"]["cold"]["ttft_ms_p50"], 200)
        self.assertEqual(report["slo"]["eligible_requests"], 2)
        self.assertEqual(report["slo"]["qualified_requests"], 1)
        self.assertEqual(report["slo"]["attainment_pct"], 50)
        self.assertEqual(report["slo"]["observation_window_seconds"], 4)
        self.assertEqual(report["slo"]["qualified_requests_per_gpu_hour"], 450)

    def test_audit_pairs_reused_sequences_across_process_lifetimes(self):
        parsed = [
            start(1, 4, 1_000),
            finish(
                1,
                prompt=100,
                cached=0,
                ttft=200,
                duration=300,
                completion=2,
                unix_ms=2_000,
            ),
            start(1, 1, 3_000),
            finish(
                1,
                prompt=100,
                cached=100,
                ttft=20,
                duration=40,
                completion=2,
                unix_ms=4_000,
            ),
        ]
        report = audit(parsed)
        self.assertEqual(report["records"]["starts"], 2)
        self.assertEqual(report["records"]["joined"], 2)
        self.assertEqual(report["records"]["unmatched_starts"], 0)
        self.assertEqual(report["records"]["unmatched_finishes"], 0)
        self.assertEqual(set(report["by_request_load_units"]), {"1", "4"})

    def test_cli_accepts_prefixed_logs_and_emits_json(self):
        lines = "\n".join(
            "[route_journal] " + json.dumps(record)
            for record in (
                start(1, 2),
                finish(1, prompt=10, cached=0, ttft=20, duration=40, completion=2),
            )
        )
        with mock.patch("sys.stdin", io.StringIO(lines)), mock.patch(
            "sys.stdout", new_callable=io.StringIO
        ) as output:
            self.assertEqual(main(["-", "--json"]), 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["overall"]["requests"], 1)
        self.assertEqual(report["by_request_load_units"]["2"]["tpot_ms_p50"], 20)


if __name__ == "__main__":
    unittest.main()
