#!/usr/bin/env python3
"""Tests for the LB-dispatch-vs-engine-occupancy sampler."""

import unittest

import slot_sampler


ENGINE_BODY = """\
# HELP sglang:num_running_reqs The number of running requests.
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{model_name="qwen3.8-27b"} 12.0
# HELP sglang:num_queue_reqs The number of requests in the waiting queue.
sglang:num_queue_reqs{model_name="qwen3.8-27b"} 4.0
sglang:num_grammar_queue_reqs{model_name="qwen3.8-27b"} 7.0
"""

LB_BODY = """\
ramjet_requests_inflight 128
ramjet_upstream_inflight{upstream="http://qwen38-sg-e0:8000"} 16
ramjet_upstream_inflight{upstream="http://qwen38-sg-e1:8000"} 16
"""


class SumMetricTests(unittest.TestCase):
    def test_reads_a_labelled_gauge(self):
        self.assertEqual(
            slot_sampler.sum_metric(ENGINE_BODY, "sglang:num_running_reqs"), 12.0)

    def test_sums_every_labelled_sample(self):
        self.assertEqual(
            slot_sampler.sum_metric(LB_BODY, "ramjet_upstream_inflight"), 32.0)

    def test_unlabelled_sample(self):
        self.assertEqual(
            slot_sampler.sum_metric(LB_BODY, "ramjet_requests_inflight"), 128.0)

    def test_a_longer_metric_sharing_the_prefix_is_not_counted(self):
        # num_grammar_queue_reqs must not be folded into num_queue_reqs;
        # doing so would inflate apparent queueing and fake a ceiling.
        self.assertEqual(
            slot_sampler.sum_metric(ENGINE_BODY, "sglang:num_queue_reqs"), 4.0)

    def test_absent_metric_is_nan_not_zero(self):
        # A failed scrape must be distinguishable from a genuinely idle
        # engine, otherwise lost polls silently understate occupancy.
        value = slot_sampler.sum_metric(ENGINE_BODY, "sglang:num_absent")
        self.assertNotEqual(value, value)

    def test_failed_scrape_body_is_nan(self):
        value = slot_sampler.sum_metric("", "sglang:num_running_reqs")
        self.assertNotEqual(value, value)

    def test_help_and_type_lines_are_ignored(self):
        body = "# HELP sglang:num_running_reqs x\n# TYPE sglang:num_running_reqs gauge\n"
        value = slot_sampler.sum_metric(body, "sglang:num_running_reqs")
        self.assertNotEqual(value, value)


class FormatTests(unittest.TestCase):
    def test_nan_renders_as_nan(self):
        self.assertEqual(slot_sampler.fmt(float("nan")), "nan")

    def test_value_renders_without_decimals(self):
        self.assertEqual(slot_sampler.fmt(96.0), "96")


if __name__ == "__main__":
    unittest.main()
