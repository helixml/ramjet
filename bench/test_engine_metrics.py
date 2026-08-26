import unittest

from engine_metrics import (
    aggregate_deltas,
    cache_usage,
    delta,
    metric_value,
    position_values,
    speculative_delta,
)


class EngineMetricsTest(unittest.TestCase):
    def test_metric_value_sums_labeled_series(self):
        body = """
# HELP vllm:num_requests_waiting waiting
vllm:num_requests_waiting{engine="0"} 2
vllm:num_requests_waiting{engine="1",source="local"} 3
vllm:num_requests_running 7
"""
        self.assertEqual(metric_value(body, "vllm:num_requests_waiting"), 5)
        self.assertEqual(
            metric_value(body, "vllm:num_requests_waiting", {"source": "local"}), 3
        )
        self.assertIsNone(
            metric_value(body, "vllm:num_requests_waiting", {"source": "external"})
        )
        self.assertEqual(metric_value(body, "vllm:num_requests_running"), 7)
        self.assertIsNone(metric_value(body, "vllm:missing"))

    def test_delta_reports_mean_queue_and_prefill_time(self):
        before = {
            "preemptions": 2,
            "prompt_tokens": 100,
            "cached_prompt_tokens": 20,
            "prefix_queries": 100,
            "prefix_hits": 20,
            "queue_seconds_sum": 1,
            "queue_samples": 10,
            "prefill_seconds_sum": 3,
            "prefill_samples": 10,
        }
        after = {
            "preemptions": 3,
            "prompt_tokens": 500,
            "cached_prompt_tokens": 220,
            "prefix_queries": 500,
            "prefix_hits": 220,
            "queue_seconds_sum": 2.5,
            "queue_samples": 15,
            "prefill_seconds_sum": 5,
            "prefill_samples": 15,
        }
        result = delta(before, after)
        self.assertEqual(result["preemptions"], 1)
        self.assertEqual(result["prompt_tokens"], 400)
        self.assertEqual(result["cached_prompt_tokens"], 200)
        self.assertEqual(result["prefix_queries"], 400)
        self.assertEqual(result["prefix_hits"], 200)
        self.assertEqual(result["prefix_hit_pct"], 50)
        self.assertEqual(result["queue_ms_mean"], 300)
        self.assertEqual(result["prefill_ms_mean"], 400)

    def test_counter_reset_is_not_reported_as_negative_work(self):
        before = {key: 10 for key in (
            "preemptions",
            "prompt_tokens",
            "cached_prompt_tokens",
            "prefix_queries",
            "prefix_hits",
            "queue_seconds_sum",
            "queue_samples",
            "prefill_seconds_sum",
            "prefill_samples",
        )}
        after = {key: 0 for key in before}
        result = delta(before, after)
        self.assertTrue(all(result[key] is None for key in before))
        self.assertIsNone(result["queue_ms_mean"])

    def test_aggregate_deltas_sums_an_exact_multi_engine_interval(self):
        keys = (
            "preemptions",
            "prompt_tokens",
            "cached_prompt_tokens",
            "prefix_queries",
            "prefix_hits",
            "queue_seconds_sum",
            "queue_samples",
            "prefill_seconds_sum",
            "prefill_samples",
        )
        before = [{key: 10 for key in keys}, {key: 100 for key in keys}]
        after = [{key: 30 for key in keys}, {key: 150 for key in keys}]
        result = aggregate_deltas(before, after)
        self.assertEqual(result["prefix_queries"], 70)
        self.assertEqual(result["prefix_hits"], 70)
        self.assertEqual(result["prefix_hit_pct"], 100)
        self.assertEqual(result["queue_ms_mean"], 1000)
        self.assertIsNone(aggregate_deltas(before, after[:1]))
        reset = [dict(after[0]), dict(after[1])]
        reset[1]["prefix_hits"] = 0
        self.assertIsNone(aggregate_deltas(before, reset)["prefix_hits"])

    def test_cache_usage_defaults_to_response_for_compatibility(self):
        result = cache_usage(
            100,
            0,
            {"prefix_queries": 100, "prefix_hits": 80},
        )
        self.assertEqual(result["source"], "response_usage")
        self.assertTrue(result["response_usage_authoritative"])
        self.assertEqual(result["cached_tokens"], 0)
        self.assertEqual(result["hit_pct"], 0)

    def test_cache_usage_can_select_native_prefix_counters(self):
        result = cache_usage(
            100,
            0,
            {"prefix_queries": 300, "prefix_hits": 230},
            "vllm-prefix",
        )
        self.assertEqual(result["source"], "vllm_prefix_counters")
        self.assertFalse(result["response_usage_authoritative"])
        self.assertEqual(result["prompt_tokens"], 300)
        self.assertEqual(result["cached_tokens"], 230)
        self.assertEqual(result["hit_pct"], 76.67)
        self.assertEqual(result["response_cached_tokens"], 0)

    def test_native_cache_usage_fails_closed_without_valid_counters(self):
        for engine in (
            None,
            {"prefix_queries": None, "prefix_hits": None},
            {"prefix_queries": 10, "prefix_hits": 11},
            {"prefix_queries": -1, "prefix_hits": 0},
            {"prefix_queries": 0, "prefix_hits": 0},
        ):
            with self.subTest(engine=engine):
                result = cache_usage(100, 0, engine, "vllm-prefix")
                self.assertFalse(result["available"])
                self.assertIsNone(result["cached_tokens"])
                self.assertFalse(result["response_usage_authoritative"])
        with self.assertRaises(ValueError):
            cache_usage(1, 0, None, "invented")

    def test_speculative_delta_reports_fixed_k5_and_reconciles_client_work(self):
        before = {
            "draft_steps": 10,
            "proposed_tokens": 50,
            "accepted_tokens": 20,
            "generation_tokens": 100,
            "finished_requests": 2,
            "accepted_per_position": {0: 10, 1: 6, 2: 4},
        }
        after = {
            "draft_steps": 14,
            "proposed_tokens": 70,
            "accepted_tokens": 30,
            "generation_tokens": 124,
            "finished_requests": 4,
            "accepted_per_position": {0: 14, 1: 10, 2: 6},
        }
        result = speculative_delta(before, after, 24, 2, expected_enabled=True)
        self.assertEqual(result["state"], "enabled")
        self.assertTrue(result["reconciled"])
        self.assertEqual(result["strict_acceptance_pct"], 50)
        self.assertEqual(result["proposed_tokens_per_step"], 5)
        self.assertEqual(result["accepted_tokens_per_step"], 2.5)
        self.assertEqual(result["effective_tokens_per_step"], 3.5)
        self.assertEqual(result["accepted_per_position"], {"0": 4, "1": 4, "2": 2})

    def test_speculative_state_distinguishes_disabled_reset_and_contamination(self):
        absent = {
            "draft_steps": None,
            "proposed_tokens": None,
            "accepted_tokens": None,
        }
        self.assertEqual(
            speculative_delta(absent, absent, 0, 0, expected_enabled=False)["state"],
            "disabled",
        )
        before = {
            "draft_steps": 2,
            "proposed_tokens": 10,
            "accepted_tokens": 5,
            "generation_tokens": 20,
            "finished_requests": 1,
            "accepted_per_position": {0: 2},
        }
        reset = {key: 0 for key in before if key != "accepted_per_position"}
        reset["accepted_per_position"] = {0: 0}
        self.assertEqual(speculative_delta(before, reset, 0, 0)["state"], "counter_reset")
        after = dict(before)
        after.update(
            draft_steps=4,
            proposed_tokens=20,
            accepted_tokens=10,
            generation_tokens=30,
            finished_requests=2,
        )
        after["accepted_per_position"] = {0: 4}
        contaminated = speculative_delta(before, after, 9, 1)
        self.assertEqual(contaminated["state"], "contaminated")
        self.assertFalse(contaminated["reconciled"])

    def test_position_values_sums_engines_and_bounds_positions(self):
        body = '''
vllm:spec_decode_num_accepted_tokens_per_pos_total{engine="0",position="0"} 3
vllm:spec_decode_num_accepted_tokens_per_pos_total{engine="1",position="0"} 4
vllm:spec_decode_num_accepted_tokens_per_pos_total{engine="0",position="4"} 2
vllm:spec_decode_num_accepted_tokens_per_pos_total{engine="0",position="999"} 9
'''
        self.assertEqual(position_values(body), {0: 7, 4: 2})


if __name__ == "__main__":
    unittest.main()
