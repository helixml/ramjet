import email.message
import io
import unittest
import urllib.error
from unittest import mock

from cachebench import (
    aggregate_engine_delta,
    cache_outcome,
    execute_waves,
    execute_request,
    execute_with_retries,
    fetch_lb_metrics,
    fetch_replica_inventory,
    latency_by_outcome,
    nonnegative_delta,
    parse_apps,
    reconcile,
    replica_inventory_change,
    summarize,
    synthetic_prefix,
    workload_coordinates,
    workload_waves,
)


class CacheBenchTest(unittest.TestCase):
    def test_synthetic_prefix_has_stable_size_without_repeating_salt(self):
        first = synthetic_prefix(1, 2, "short")
        second = synthetic_prefix(1, 2, "a-much-longer-salt")
        other_app = synthetic_prefix(2, 2, "short")
        self.assertEqual(len(first.encode()), 2048)
        self.assertEqual(len(second.encode()), 2048)
        self.assertNotEqual(first, second)
        self.assertNotEqual(first, other_app)
        self.assertNotIn("short", first)
        self.assertNotIn("a-much-longer-salt", second)
        self.assertEqual(first[64:], second[64:])

    def test_registered_but_unobserved_lb_metric_is_zero(self):
        body = b"""# HELP ramjet_prompt_tokens_total prompts
# TYPE ramjet_prompt_tokens_total counter
# HELP ramjet_cached_prompt_tokens_total cached
# TYPE ramjet_cached_prompt_tokens_total counter
# HELP ramjet_cache_requests_total requests
# TYPE ramjet_cache_requests_total counter
# HELP ramjet_cache_ttft_seconds TTFT
# TYPE ramjet_cache_ttft_seconds histogram
# HELP ramjet_kv_event_blocks_total blocks
# TYPE ramjet_kv_event_blocks_total counter
# HELP ramjet_kv_event_clears_total clears
# TYPE ramjet_kv_event_clears_total counter
# HELP ramjet_exact_route_placement_total exact route placement decisions
# TYPE ramjet_exact_route_placement_total counter
# HELP ramjet_exact_route_projected_balance_total projected balance decisions
# TYPE ramjet_exact_route_projected_balance_total counter
"""
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = body
        with mock.patch("urllib.request.urlopen", return_value=response):
            metrics = fetch_lb_metrics("http://metrics")
        self.assertTrue(all(value == 0 for value in metrics.values()))

    def test_shadow_counter_deltas_select_only_bounded_chat_outcomes(self):
        body = b'''# HELP ramjet_exact_route_placement_total decisions
# TYPE ramjet_exact_route_placement_total counter
ramjet_exact_route_placement_total{endpoint="chat",mode="shadow",outcome="would_balance"} 3
ramjet_exact_route_placement_total{endpoint="chat",mode="shadow",outcome="kept_balance_load_gate"} 5
ramjet_exact_route_placement_total{endpoint="chat",mode="placement",outcome="would_balance"} 99
# HELP ramjet_exact_route_projected_balance_total projected decisions
# TYPE ramjet_exact_route_projected_balance_total counter
ramjet_exact_route_projected_balance_total{endpoint="chat",outcome="kept_selected"} 7
ramjet_exact_route_projected_balance_total{endpoint="chat",outcome="would_balance"} 2
'''
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = body
        with mock.patch("urllib.request.urlopen", return_value=response):
            metrics = fetch_lb_metrics("http://metrics")
        self.assertEqual(metrics["shadow_cold_would_balance"], 3)
        self.assertEqual(metrics["shadow_cold_load_gate"], 5)
        self.assertEqual(metrics["shadow_cold_delta_gate"], 0)
        self.assertEqual(metrics["projected_cold_kept_selected"], 7)
        self.assertEqual(metrics["projected_cold_would_balance"], 2)
        self.assertEqual(metrics["projected_cold_load_gate"], 0)

    def test_workload_round_robins_apps_before_reuse(self):
        self.assertEqual(
            list(workload_coordinates(2, 2, 1)),
            [(0, 0, 1), (1, 0, 1), (0, 1, 1), (1, 1, 1)],
        )

    def test_replica_inventory_uses_only_valid_opaque_indices(self):
        body = b'''{
          "status":"ok",
          "replicas":[
            {"index":1,"exact_inventory":{"trusted":false,"resident_blocks":2,"resident_tokens":4}},
            {"index":0,"exact_inventory":{"trusted":true,"resident_blocks":10,"resident_tokens":2560}}
          ]
        }'''
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = body
        with mock.patch("urllib.request.urlopen", return_value=response):
            inventory = fetch_replica_inventory("http://proxy")
        self.assertEqual(list(inventory), ["0", "1"])
        self.assertEqual(inventory["0"]["resident_tokens"], 2560)
        self.assertNotIn("upstream", str(inventory).lower())

    def test_replica_inventory_fails_closed_and_reports_signed_change(self):
        invalid = b'{"replicas":[{"index":0,"exact_inventory":null}]}'
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = invalid
        with mock.patch("urllib.request.urlopen", return_value=response):
            self.assertIsNone(fetch_replica_inventory("http://proxy"))
        before = {
            "0": {"trusted": True, "resident_blocks": 10, "resident_tokens": 100}
        }
        after = {
            "0": {"trusted": True, "resident_blocks": 8, "resident_tokens": 80}
        }
        self.assertEqual(
            replica_inventory_change(before, after),
            {
                "0": {
                    "trusted_before": True,
                    "trusted_after": True,
                    "resident_blocks_before": 10,
                    "resident_blocks_after": 8,
                    "resident_blocks_change": -2,
                    "resident_tokens_before": 100,
                    "resident_tokens_after": 80,
                    "resident_tokens_change": -20,
                }
            },
        )
        self.assertIsNone(replica_inventory_change(before, {}))
        after["0"]["trusted"] = False
        untrusted = replica_inventory_change(before, after)["0"]
        self.assertIsNone(untrusted["resident_blocks_change"])
        self.assertIsNone(untrusted["resident_tokens_change"])

    def test_concurrent_waves_keep_cold_to_reuse_barrier(self):
        completed_cold = set()
        progress = []

        def execute(coordinate):
            app, session, _turn = coordinate
            if session == 0:
                completed_cold.add(app)
            else:
                self.assertEqual(completed_cold, {0, 1, 2})
            return {"ok": True}

        records = execute_waves(3, 2, 1, 2, execute, progress.append)
        self.assertEqual(
            [(record["app"], record["session"]) for record in records],
            [(0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1)],
        )
        self.assertEqual(
            list(workload_waves(2, 2, 1)),
            [[(0, 0, 1), (1, 0, 1)], [(0, 1, 1), (1, 1, 1)]],
        )
        self.assertEqual(
            [
                (item["completed"], item["wave"], item["wave_completed"])
                for item in progress
            ],
            [
                (1, 1, 1),
                (2, 1, 2),
                (3, 1, 3),
                (4, 2, 1),
                (5, 2, 2),
                (6, 2, 3),
            ],
        )
        self.assertTrue(all(item["total"] == 6 for item in progress))
        self.assertTrue(all(item["waves"] == 2 for item in progress))

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
            "live_stored_blocks": 1,
            "live_removed_blocks": 0,
            "live_clear_events": 0,
            "shadow_exact_agree": 0,
            "shadow_cold_all_zero": 0,
            "shadow_cold_would_balance": 0,
            "shadow_cold_delta_gate": 0,
            "shadow_cold_load_gate": 0,
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

    def test_native_reconciliation_ignores_zero_response_cache_observation(self):
        records = [{"ok": True, "prompt_tokens": 100, "cached_tokens": 0}]
        lb = {
            "prompt_tokens": 100,
            "cached_prompt_tokens": 0,
            "cache_requests": 1,
            "cache_ttft_samples": 1,
        }
        engine = {
            "prompt_tokens": 100,
            "cached_prompt_tokens": 0,
            "prefix_queries": 100,
            "prefix_hits": 64,
            "queue_samples": 1,
            "prefill_samples": 1,
        }
        result = reconcile(
            records, lb, engine, cache_authority="vllm-prefix"
        )
        self.assertTrue(result["consistent"])
        self.assertEqual(
            result["values"]["cached_prompt_tokens"],
            {"engine_prefix_hits": 64},
        )
        self.assertFalse(
            result["cache_authority"]["response_usage_authoritative"]
        )

    def test_native_summary_promotes_aggregate_engine_cache_not_response_zero(self):
        records = [
            {
                "app": 0,
                "session": 0,
                "turn": 1,
                "ok": True,
                "route": "0",
                "prompt_tokens": 100,
                "cached_tokens": 0,
                "completion_tokens": 2,
                "cache_outcome": "cold",
                "ttft_ms": 100,
                "wall_ms": 200,
            }
        ]
        engine = {
            "prompt_tokens": 100,
            "cached_prompt_tokens": 0,
            "prefix_queries": 100,
            "prefix_hits": 80,
            "queue_samples": 1,
            "prefill_samples": 1,
        }
        summary = summarize(
            records,
            1,
            1,
            1,
            32,
            1.0,
            None,
            engine,
            0,
            cache_authority="vllm-prefix",
        )
        self.assertEqual(summary["cached_tokens"], 80)
        self.assertEqual(summary["response_cached_tokens"], 0)
        self.assertEqual(summary["cache_hit_pct"], 80)
        self.assertFalse(summary["response_cache_observations_authoritative"])
        self.assertEqual(
            summary["cache_authority"]["source"], "vllm_prefix_counters"
        )

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
        self.assertEqual(summary["initial_wave_prompt_tokens"], 200)
        self.assertEqual(summary["initial_prompt_tokens_mean"], 100)
        self.assertEqual(summary["total_tok_s"], 408)
        self.assertEqual(summary["request_reuse_pct"], 50)
        self.assertEqual(summary["reuse_wave_requests"], 2)
        self.assertEqual(summary["reuse_wave_outcomes"], {"partial": 2})
        self.assertEqual(summary["reuse_wave_cache_hit_pct"], 80)
        self.assertEqual(summary["reuse_distance_requests_max"], 0)
        self.assertEqual(summary["route_split"], {"0": 2, "1": 2})
        self.assertIsNone(summary["replica_exact_inventory"])
        encoded = str(summary).lower()
        for forbidden in ("message", "content", "fingerprint", "token_ids"):
            self.assertNotIn(forbidden, encoded)

    def test_summary_reports_live_block_churn_without_calling_it_eviction(self):
        records = [
            {
                "app": 0,
                "ok": True,
                "route": "0",
                "prompt_tokens": 10,
                "cached_tokens": 0,
                "completion_tokens": 1,
                "cache_outcome": "cold",
                "ttft_ms": 100,
                "wall_ms": 110,
            }
        ]
        lb = {
            "prompt_tokens": 10,
            "cached_prompt_tokens": 0,
            "cache_requests": 1,
            "cache_ttft_samples": 1,
            "live_stored_blocks": 20,
            "live_removed_blocks": 5,
            "live_clear_events": 0,
        }
        summary = summarize(records, 1, 1, 1, 1, 1.0, lb, None, 0)
        self.assertEqual(summary["live_block_churn_pct"], 25)
        self.assertNotIn("eviction", str(summary).lower())

    def test_capture_retry_is_bounded_and_only_retries_503(self):
        outcomes = iter(
            [
                {
                    "ok": False,
                    "error": "HTTP 503",
                    "retryable": True,
                    "retry_reason": "tokenizer_unavailable",
                },
                {
                    "ok": False,
                    "error": "HTTP 503",
                    "retryable": True,
                    "retry_reason": "attestation_changed",
                },
                {"ok": True, "error": None},
            ]
        )
        result = execute_with_retries(lambda _remaining: next(outcomes), 2, 0, 10)
        self.assertTrue(result["ok"])
        self.assertEqual(result["client_attempts"], 3)
        self.assertEqual(
            result["retry_reasons"],
            {"attestation_changed": 1, "tokenizer_unavailable": 1},
        )
        result = execute_with_retries(
            lambda _remaining: {
                "ok": False,
                "error": "HTTP 503",
                "retryable": False,
            },
            10,
            0,
            10,
        )
        self.assertEqual(result["client_attempts"], 1)

    def test_only_proxy_marked_503_is_retryable(self):
        def response(headers):
            error = urllib.error.HTTPError(
                "http://lb/v1/chat/completions",
                503,
                "unavailable",
                headers,
                io.BytesIO(b"{}"),
            )
            with mock.patch("cachebench.urllib.request.urlopen", side_effect=error):
                return execute_request(
                    "http://lb", "model", "token", [], 1, 1
                )

        ordinary = response(email.message.Message())
        self.assertFalse(ordinary["retryable"])
        marked_headers = email.message.Message()
        marked_headers["X-Ramjet-Shadow-Soak-Retry"] = "tokenizer_unavailable"
        marked = response(marked_headers)
        self.assertTrue(marked["retryable"])
        self.assertEqual(marked["retry_reason"], "tokenizer_unavailable")

    def test_capture_retry_has_one_absolute_deadline(self):
        with mock.patch("cachebench.time.monotonic", side_effect=[10, 10.5, 11.5]):
            result = execute_with_retries(
                lambda _remaining: {
                    "ok": False,
                    "error": "HTTP 503",
                    "retryable": True,
                    "retry_reason": "tokenizer_unavailable",
                },
                20,
                0,
                1,
            )
        self.assertEqual(result["client_attempts"], 1)
        self.assertEqual(result["retry_reasons"], {"tokenizer_unavailable": 1})


if __name__ == "__main__":
    unittest.main()
