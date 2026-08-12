import unittest

from engine_metrics import delta, metric_value


class EngineMetricsTest(unittest.TestCase):
    def test_metric_value_sums_labeled_series(self):
        body = """
# HELP vllm:num_requests_waiting waiting
vllm:num_requests_waiting{engine="0"} 2
vllm:num_requests_waiting{engine="1"} 3
vllm:num_requests_running 7
"""
        self.assertEqual(metric_value(body, "vllm:num_requests_waiting"), 5)
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


if __name__ == "__main__":
    unittest.main()
