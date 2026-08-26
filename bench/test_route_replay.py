import unittest
from unittest import mock

from route_replay import choose, records, replay, session_affinity_choice


def start(chosen=0, rotation=0, left=(40, 0), right=(0, 0)):
    return {
        "v": 1,
        "event": "start",
        "seq": 1,
        "chosen": chosen,
        "rotation": rotation,
        "candidates": [
            {"upstream": 0, "rank": 0, "overlap_blocks": left[0], "affinity_blocks": min(left[0], 32), "load_units": left[1], "request_load_units": 1, "healthy": True},
            {"upstream": 1, "rank": 1, "overlap_blocks": right[0], "affinity_blocks": min(right[0], 32), "load_units": right[1], "request_load_units": 1, "healthy": True},
        ],
    }


class RouteReplayTest(unittest.TestCase):
    def test_affinity_cap_changes_choice_under_load(self):
        record = start(left=(100, 9), right=(0, 0))
        self.assertEqual(choose(record, alpha=4, cap=32), 1)
        self.assertEqual(choose(record, alpha=4, cap=64), 0)

    def test_deeper_overlap_breaks_equal_load_capped_tie(self):
        record = start(left=(100, 0), right=(40, 0), rotation=1)
        self.assertEqual(choose(record, alpha=4, cap=32), 0)

    def test_v2_overlap_tie_break_retains_warm_candidate_across_load(self):
        record = start(left=(100, 8), right=(0, 0), rotation=1)
        record["v"] = 2
        record["score_tie_break"] = "overlap"
        self.assertEqual(choose(record, alpha=4, cap=32), 0)
        self.assertEqual(choose(record, alpha=4, cap=32, tie_break="load-neutral"), 1)

    def test_rotation_breaks_cold_tie(self):
        self.assertEqual(choose(start(left=(0, 0), right=(0, 0), rotation=0), 4, 32), 0)
        self.assertEqual(choose(start(left=(0, 0), right=(0, 0), rotation=1), 4, 32), 1)

    def test_log_prefix_and_static_replay(self):
        lines = ['2026/08/12 [route_journal] ' + __import__('json').dumps(start())]
        parsed = list(records(lines))
        rows = replay(parsed, {}, [4], [32])
        self.assertEqual(rows[0]["agreement_pct"], 100.0)
        self.assertEqual(rows[0]["counterfactual_migrations"], 0)

    def test_v3_through_v9_journal_records_are_accepted(self):
        record = start()
        record["v"] = 3
        record["score_tie_break"] = "overlap"
        canary = {**record, "v": 4, "exact_canary": "treatment"}
        session = {
            **start(left=(0, 0), right=(0, 0)),
            "v": 5,
            "alpha": 4,
            "session_affinity": {
                "policy_version": 1,
                "bonus_blocks": 4,
                "max_load_delta": 0,
                "outcome": "would_prefer_primary",
                "primary": 1,
                "secondary": 0,
                "target": 1,
            },
        }
        phase_aware = {**record, "v": 6, "phase_aware_load": True}
        output_limit = {
            **record,
            "v": 7,
            "output_limit": {
                "policy_version": 1,
                "requested_bucket": "65_256",
                "requested_source": "max_completion_tokens",
                "effective_bucket": "65_256",
                "effective_source": "max_completion_tokens",
                "mutation": "unchanged",
                "stream_mode": "streaming",
            },
        }
        admitted_load = {**record, "v": 8}
        projected_load = {**record, "v": 9, "projected_load": True}
        parsed = list(
            records(
                [
                    __import__("json").dumps(record),
                    __import__("json").dumps(canary),
                    __import__("json").dumps(session),
                    __import__("json").dumps(phase_aware),
                    __import__("json").dumps(output_limit),
                    __import__("json").dumps(admitted_load),
                    __import__("json").dumps(projected_load),
                ]
            )
        )
        self.assertEqual(
            parsed,
            [
                record,
                canary,
                session,
                phase_aware,
                output_limit,
                admitted_load,
                projected_load,
            ],
        )
        row = replay([session], {}, [4], [32])[0]
        self.assertEqual(
            row["session_affinity_counts"], {"would_prefer_primary": 1}
        )
        self.assertEqual(
            row["session_affinity_replay_counts"], {"would_prefer_primary": 1}
        )
        self.assertEqual(row["session_affinity_record_mismatches"], 0)

    def test_boolean_and_future_journal_versions_are_not_accepted(self):
        boolean = {**start(), "v": True}
        future = {**start(), "v": 10}
        self.assertEqual(
            list(records([__import__("json").dumps(boolean), __import__("json").dumps(future)])),
            [],
        )
        self.assertEqual(list(records(["[]", "true", '"string"'])), [])

    def test_projected_load_replay_uses_candidate_request_cost(self):
        record = start(chosen=1, left=(512, 9), right=(0, 0))
        record.update(v=9, projected_load=True)
        record["candidates"][0]["request_load_units"] = 1
        record["candidates"][1]["request_load_units"] = 32

        self.assertEqual(choose(record, alpha=4, cap=32, projected_load=False), 1)
        self.assertEqual(choose(record, alpha=4, cap=32), 0)
        self.assertEqual(choose(record, alpha=4, cap=32, projected_load=True), 0)

        record["candidates"][1]["request_load_units"] = 0
        self.assertIsNone(choose(record, alpha=4, cap=32))

    def test_session_affinity_replay_matches_weight_overlap_and_rotation(self):
        record = start(chosen=0, rotation=0, left=(5, 0), right=(0, 0))
        record.update(v=5, alpha=4)
        record["candidates"][0]["affinity_blocks"] = 4
        record["session_affinity"] = {
            "policy_version": 1,
            "bonus_blocks": 4,
            "max_load_delta": 0,
            "outcome": "kept_score",
            "primary": 1,
            "secondary": 0,
            "target": 1,
        }
        self.assertEqual(session_affinity_choice(record), "kept_score")

        record["candidates"][0].update(overlap_blocks=4, affinity_blocks=4)
        record["candidates"][1].update(overlap_blocks=4, affinity_blocks=4, load_units=1)
        record["session_affinity"]["max_load_delta"] = 1
        record["rotation"] = 1
        self.assertEqual(session_affinity_choice(record), "would_prefer_primary")
        record["rotation"] = 0
        self.assertEqual(session_affinity_choice(record), "kept_score")

    def test_session_affinity_replay_exposes_policy_mismatch_and_sweep(self):
        record = start(chosen=0, left=(0, 0), right=(0, 0))
        record.update(v=5, alpha=4)
        record["session_affinity"] = {
            "policy_version": 1,
            "bonus_blocks": 4,
            "max_load_delta": 0,
            "outcome": "kept_score",
            "primary": 1,
            "secondary": 0,
            "target": 1,
        }
        row = replay([record], {}, [4], [32])[0]
        self.assertEqual(row["session_affinity_record_mismatches"], 1)
        self.assertEqual(
            row["session_affinity_replay_counts"], {"would_prefer_primary": 1}
        )
        record["session_affinity"].update(
            outcome="would_prefer_primary", target=0
        )
        self.assertEqual(
            replay([record], {}, [4], [32])[0][
                "session_affinity_record_mismatches"
            ],
            1,
        )
        swept = replay(
            [record], {}, [4], [32], session_bonus_blocks=0
        )[0]
        self.assertEqual(swept["session_affinity_replay_counts"], {"kept_score": 1})

    def test_v4_replays_approximate_choice_but_attributes_actual_warmth(self):
        record = start(chosen=1, left=(40, 0), right=(0, 0))
        record.update(v=4, exact_canary="treatment", served_chosen=0)
        finish = {
            "v": 4,
            "event": "finish",
            "seq": 1,
            "result": "complete",
            "ttft_ms": 100,
            "prompt_tokens": 100,
            "cached_tokens": 80,
        }
        row = replay([record], {1: finish}, [4], [32])[0]
        self.assertEqual(row["agreement_pct"], 0.0)
        self.assertEqual(row["exact_canary_counts"], {"treatment": 1})
        self.assertEqual(row["observed_warm_complete"], 1)
        self.assertEqual(row["observed_cold_complete"], 0)

    def test_finish_join_reports_observed_outcomes(self):
        record = start(chosen=0)
        finishes = {
            1: {
                "v": 3,
                "event": "finish",
                "seq": 1,
                "result": "complete",
                "first_byte_ms": 25,
                "ttft_ms": 125.5,
                "duration_ms": 500,
                "prompt_tokens": 100,
                "cached_tokens": 75,
            }
        }
        row = replay([record], finishes, [4], [32])[0]
        self.assertEqual(row["paired_finishes"], 1)
        self.assertEqual(row["observed_complete"], 1)
        self.assertEqual(row["observed_first_byte_ms_median"], 25)
        self.assertEqual(row["observed_ttft_ms_median"], 125.5)
        self.assertEqual(row["observed_ttft_samples"], 1)
        self.assertEqual(row["observed_cache_hit_pct"], 75.0)
        self.assertEqual(row["observed_warm_complete"], 1)
        self.assertEqual(row["observed_cold_complete"], 0)
        self.assertEqual(row["observed_warm_ttft_ms_median"], 125.5)
        self.assertEqual(row["observed_warm_cache_hit_pct"], 75.0)
        self.assertIsNone(row["observed_cold_ttft_ms_median"])

    def test_finish_join_splits_actual_warm_and_cold_outcomes(self):
        warm = start(chosen=0, left=(40, 0))
        cold = start(chosen=1, left=(40, 9), right=(0, 0))
        cold["seq"] = 2
        finishes = {
            1: {
                "v": 3,
                "event": "finish",
                "seq": 1,
                "result": "complete",
                "ttft_ms": 100,
                "prompt_tokens": 100,
                "cached_tokens": 80,
            },
            2: {
                "v": 3,
                "event": "finish",
                "seq": 2,
                "result": "complete",
                "ttft_ms": 900,
                "prompt_tokens": 100,
                "cached_tokens": 0,
            },
        }
        row = replay([warm, cold], finishes, [4], [32])[0]
        self.assertEqual(row["observed_warm_complete"], 1)
        self.assertEqual(row["observed_cold_complete"], 1)
        self.assertEqual(row["observed_warm_ttft_ms_median"], 100)
        self.assertEqual(row["observed_cold_ttft_ms_median"], 900)
        self.assertEqual(row["observed_warm_cache_hit_pct"], 80.0)
        self.assertEqual(row["observed_cold_cache_hit_pct"], 0.0)

    def test_legacy_ttft_is_reported_as_first_byte_only(self):
        record = start()
        finish = {
            "v": 2,
            "event": "finish",
            "seq": 1,
            "result": "complete",
            "ttft_ms": 80,
        }
        row = replay([record], {1: finish}, [4], [32])[0]
        self.assertEqual(row["observed_first_byte_ms_median"], 80)
        self.assertIsNone(row["observed_ttft_ms_median"])
        self.assertEqual(row["observed_ttft_samples"], 0)

    def test_cli_filters_to_joined_request_slice(self):
        first = start(chosen=0)
        first["request_bytes"] = 100
        second = start(chosen=1, rotation=1)
        second["seq"] = 2
        second["request_bytes"] = 200
        second["v"] = 5
        second["session_affinity"] = {
            "policy_version": 1,
            "bonus_blocks": 4,
            "max_load_delta": 0,
            "outcome": "would_prefer_secondary_primary_unhealthy",
        }
        lines = "\n".join(__import__("json").dumps(item) for item in (first, second))
        with mock.patch("sys.stdin", __import__("io").StringIO(lines)), mock.patch(
            "sys.stdout", new_callable=__import__("io").StringIO
        ) as output:
            from route_replay import main

            self.assertEqual(
                main(
                    [
                        "-",
                        "--alphas",
                        "4",
                        "--caps",
                        "32",
                        "--min-request-bytes",
                        "150",
                        "--session-affinity",
                        "would_prefer_secondary_primary_unhealthy",
                    ]
                ),
                0,
            )
        self.assertIn("       1", output.getvalue())


if __name__ == "__main__":
    unittest.main()
