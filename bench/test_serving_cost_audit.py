import io
import json
import unittest
from unittest import mock

from serving_cost_audit import (
    admitted_load_units,
    audit,
    bounded_output_limit,
    decode_observation,
    main,
    observation,
    percentile,
)


def output_limit(
    requested_bucket="65_256",
    *,
    requested_source="max_completion_tokens",
    effective_bucket=None,
    effective_source=None,
    mutation="unchanged",
    stream_mode="streaming",
):
    return {
        "policy_version": 1,
        "requested_bucket": requested_bucket,
        "requested_source": requested_source,
        "effective_bucket": effective_bucket or requested_bucket,
        "effective_source": effective_source or requested_source,
        "mutation": mutation,
        "stream_mode": stream_mode,
    }


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

    def test_output_limit_analysis_joins_decode_endpoint_stream_and_load(self):
        first_start = start(1, 2, 1_000)
        first_start.update(
            v=7,
            endpoint="chat",
            output_limit=output_limit(
                "4097_plus",
                effective_bucket="65_256",
                effective_source="max_tokens",
                mutation="max_completion_tokens_stripped",
            ),
        )
        first_finish = finish(
            1,
            prompt=100,
            cached=0,
            ttft=20,
            duration=100,
            completion=5,
            unix_ms=2_000,
        )
        first_finish["v"] = 7
        second_start = start(2, 4, 2_100)
        second_start.update(
            v=7,
            endpoint="responses",
            output_limit=output_limit(
                "65_256",
                requested_source="max_output_tokens",
                effective_source="max_output_tokens",
                stream_mode="non_streaming",
            ),
        )
        second_finish = finish(
            2,
            prompt=None,
            cached=None,
            ttft=None,
            duration=50,
            completion=None,
            result="client_disconnect",
            unix_ms=2_150,
        )
        second_finish["v"] = 7

        report = audit([first_start, first_finish, second_start, second_finish])
        analysis = report["output_limit_analysis"]
        self.assertEqual(report["schema_version"], 2)
        self.assertEqual(
            analysis["requested_bucket_counts"], {"4097_plus": 1, "65_256": 1}
        )
        bounded = analysis["by_requested_bucket"]["4097_plus"]
        self.assertEqual(
            bounded["complete_measurements"]["completion_tokens_p50"], 5
        )
        self.assertEqual(bounded["complete_measurements"]["tpot_ms_p50"], 20)
        self.assertEqual(bounded["request_load_bucket_counts"], {"2_4": 1})
        bounded_response = analysis["by_requested_bucket"]["65_256"]
        self.assertEqual(bounded_response["client_disconnects"], 1)
        self.assertEqual(bounded_response["missing_completion_tokens"], 1)
        self.assertEqual(bounded_response["missing_ttft_ms"], 1)
        self.assertEqual(bounded_response["missing_tpot_ms"], 1)
        self.assertEqual(bounded["effective_bucket_counts"], {"65_256": 1})
        self.assertEqual(
            bounded["mutation_counts"], {"max_completion_tokens_stripped": 1}
        )
        self.assertEqual(
            analysis["by_endpoint"]["chat"]["by_requested_bucket"]["4097_plus"][
                "requests"
            ],
            1,
        )
        self.assertEqual(
            analysis["by_stream_mode"]["non_streaming"]["overall"][
                "client_disconnects"
            ],
            1,
        )
        self.assertEqual(
            analysis["overall"]["complete_measurements"]["duration_ms_p50"], 100
        )
        self.assertEqual(
            analysis["overall"]["by_outcome"]["client_disconnect"][
                "duration_ms_p50"
            ],
            50,
        )
        self.assertEqual(
            analysis["by_request_load_bucket"]["2_4"]["overall"][
                "complete_measurements"
            ]["decode_duration_ms_p50"],
            80,
        )
        self.assertEqual(
            analysis["by_request_load_bucket"]["2_4"]["overall"][
                "by_outcome"
            ]["complete"]["completion_tokens_p50"],
            5,
        )

    def test_output_limit_legacy_and_malformed_v7_are_bounded(self):
        legacy = bounded_output_limit(start(1, 1))
        self.assertEqual(legacy["telemetry_state"], "legacy")
        self.assertEqual(legacy["requested_bucket"], "legacy")
        forged_legacy = start(4, 1)
        forged_legacy["v"] = 6
        forged_legacy["endpoint"] = "chat"
        forged_legacy["output_limit"] = output_limit()
        self.assertEqual(
            bounded_output_limit(forged_legacy)["telemetry_state"], "legacy"
        )

        malformed = start(2, 1)
        malformed.update(
            v=7,
            endpoint="attacker-controlled-endpoint",
            output_limit={
                **output_limit(),
                "requested_bucket": "attacker-controlled-bucket",
            },
        )
        malformed_finish = finish(
            2,
            prompt=10,
            cached=0,
            ttft=10,
            duration=20,
            completion=2,
        )
        item = decode_observation(malformed, malformed_finish)
        self.assertEqual(item["telemetry_state"], "invalid")
        self.assertEqual(item["requested_bucket"], "invalid")
        self.assertEqual(item["endpoint"], "invalid")
        encoded = json.dumps(audit([malformed, malformed_finish]), sort_keys=True)
        self.assertNotIn("attacker-controlled", encoded)

        impossible = start(3, 1)
        impossible.update(
            v=7,
            endpoint="responses",
            output_limit=output_limit(
                requested_source="max_output_tokens",
                effective_source="max_output_tokens",
                mutation="max_tokens_stripped",
            ),
        )
        self.assertEqual(
            bounded_output_limit(impossible)["telemetry_state"], "invalid"
        )

        boolean_policy = start(5, 1)
        boolean_policy.update(v=7, endpoint="chat", output_limit=output_limit())
        boolean_policy["output_limit"]["policy_version"] = True
        self.assertEqual(
            bounded_output_limit(boolean_policy)["telemetry_state"], "invalid"
        )
        future = start(6, 1)
        future.update(v=10, endpoint="chat", output_limit=output_limit())
        self.assertEqual(bounded_output_limit(future)["telemetry_state"], "invalid")

    def test_output_limit_analysis_retains_unmatched_start_as_missing_finish(self):
        orphan = start(1, 1)
        orphan.update(
            v=7,
            endpoint="chat",
            output_limit=output_limit(
                "unset",
                requested_source="none",
                effective_source="none",
                stream_mode="unset",
            ),
        )
        report = audit([orphan])
        self.assertEqual(report["records"]["unmatched_starts"], 1)
        summary = report["output_limit_analysis"]["by_requested_bucket"]["unset"]
        self.assertEqual(summary["requests"], 1)
        self.assertEqual(summary["missing_finishes"], 1)
        self.assertEqual(summary["missing_completion_tokens"], 1)

    def test_fractional_and_overflowing_token_counts_never_emit_nonfinite_tpot(self):
        request_start = start(1, 1)
        request_start.update(v=7, endpoint="chat", output_limit=output_limit())
        fractional = finish(
            1,
            prompt=10,
            cached=0,
            ttft=10,
            duration=20,
            completion=1.0000000000000002,
        )
        item = decode_observation(request_start, fractional)
        self.assertIsNone(item["completion_tokens"])
        self.assertIsNone(item["tpot_ms"])
        self.assertIsNone(observation(request_start, fractional)["completion_tokens"])

        oversized = finish(
            1,
            prompt=10,
            cached=0,
            ttft=10,
            duration=20,
            completion=(1 << 53) + 1,
        )
        report = audit([request_start, oversized])
        encoded = json.dumps(report, sort_keys=True, allow_nan=False)
        self.assertNotIn("Infinity", encoded)
        complete = report["output_limit_analysis"]["overall"][
            "complete_measurements"
        ]
        self.assertEqual(complete["missing_completion_tokens"], 1)

        zero_timing = finish(
            1,
            prompt=10,
            cached=0,
            ttft=0,
            duration=20,
            completion=2,
        )
        self.assertIsNone(observation(request_start, zero_timing))
        self.assertIsNone(decode_observation(request_start, zero_timing)["ttft_ms"])

    def test_admitted_reservation_prefers_the_journal_v8_finish_value(self):
        # Under failover the served upstream is not the initially selected
        # candidate, so the acquired reservation is the authoritative cost.
        served = start(1, 2)
        served["candidates"].append(
            {
                "upstream": 1,
                "rank": 1,
                "overlap_blocks": 0,
                "affinity_blocks": 0,
                "load_units": 0,
                "request_load_units": 7,
                "healthy": True,
            }
        )
        failed_over = finish(
            1, prompt=10, cached=0, ttft=20, duration=40, completion=2
        )
        failed_over.update({"v": 8, "upstream": 1, "request_load_units": 5})

        self.assertEqual(admitted_load_units(served, failed_over), 5)
        item = observation(served, failed_over)
        self.assertEqual(item["request_load_units"], 5)
        self.assertEqual(
            decode_observation(served, failed_over)["request_load_bucket"], "5_8"
        )

    def test_admitted_reservation_falls_back_to_the_candidate_estimate(self):
        legacy = finish(1, prompt=10, cached=0, ttft=20, duration=40, completion=2)
        self.assertEqual(admitted_load_units(start(1, 3), legacy), 3)

        for invalid in (0, -1, True, "4", None):
            with self.subTest(invalid=invalid):
                record = dict(legacy, v=8, request_load_units=invalid)
                self.assertEqual(admitted_load_units(start(1, 3), record), 3)

        unknown = dict(legacy, v=8, upstream=9)
        self.assertIsNone(admitted_load_units(start(1, 3), unknown))

    def test_v8_records_keep_validated_output_limit_telemetry(self):
        record = dict(start(1, 2), v=8, endpoint="chat", output_limit=output_limit())
        self.assertEqual(bounded_output_limit(record)["telemetry_state"], "valid")
        self.assertEqual(bounded_output_limit(record)["requested_bucket"], "65_256")

        projected = dict(record, v=9, projected_load=True)
        self.assertEqual(bounded_output_limit(projected)["telemetry_state"], "valid")

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
