import unittest
from unittest import mock

import shadow_soak


class ShadowSoakTests(unittest.TestCase):
    def test_metrics_are_fixed_and_missing_children_are_zero(self):
        body = """
ds4proxy_shadow_soak_enabled 1
ds4proxy_shadow_soak_complete 0
ds4proxy_shadow_soak_sources 104
ds4proxy_shadow_soak_source_token_bytes 55000000
ds4proxy_shadow_soak_duration_seconds 0
ds4proxy_shadow_soak_phase{phase="running"} 1
ds4proxy_shadow_soak_attempts_total{outcome="stable"} 7
ds4proxy_shadow_soak_comparisons_total{outcome="agree"} 6
ds4proxy_shadow_soak_source_comparisons_total{outcome="would_move"} 2
ds4proxy_shadow_soak_source_attempts_total{outcome="stable"} 9
ds4proxy_shadow_soak_overlap_tokens_sum{choice="best"} 700
ds4proxy_shadow_soak_source_overlap_tokens_sum{choice="best"} 300
ds4proxy_shadow_soak_placement_total{max_load_delta="2",outcome="kept_gain_gate"} 4
ds4proxy_shadow_soak_projected_balance_total{max_load_delta="1",outcome="would_balance"} 3
"""
        parsed = shadow_soak.soak_metrics(body)
        self.assertEqual(parsed["enabled"], 1)
        self.assertEqual(parsed["sources"], 104)
        self.assertEqual(parsed["phases"]["running"], 1)
        self.assertEqual(parsed["phases"]["complete"], 0)
        self.assertEqual(parsed["attempts"]["stable"], 7)
        self.assertEqual(parsed["attempts"]["lookup_error"], 0)
        self.assertEqual(parsed["comparisons"]["agree"], 6)
        self.assertEqual(parsed["source_comparisons"]["would_move"], 2)
        self.assertEqual(parsed["source_attempts"]["stable"], 9)
        self.assertEqual(parsed["overlap_token_sums"]["best"], 700)
        self.assertEqual(parsed["source_overlap_token_sums"]["best"], 300)
        self.assertEqual(parsed["placement"]["2"]["kept_gain_gate"], 4)
        self.assertEqual(parsed["projected_balance"]["1"]["would_balance"], 3)

    def test_source_retries_require_exact_safe_reason_attribution(self):
        attempts = {
            outcome: 0 for outcome in shadow_soak.SOURCE_ATTEMPT_OUTCOMES
        }
        attempts.update(stable=104, tokenizer_unavailable=3, attestation_changed=2)
        workload = {
            "client_attempts_total": 109,
            "retry_reasons": {
                "tokenizer_unavailable": 3,
                "attestation_changed": 2,
            },
        }
        self.assertTrue(shadow_soak.source_attempts_valid(attempts, 104, workload))
        attempts["lookup_error"] = 1
        self.assertFalse(shadow_soak.source_attempts_valid(attempts, 104, workload))
        attempts["lookup_error"] = 0
        attempts["stable"] = 105
        self.assertFalse(shadow_soak.source_attempts_valid(attempts, 104, workload))
        attempts["stable"] = 104
        workload["client_attempts_total"] = 108
        self.assertFalse(shadow_soak.source_attempts_valid(attempts, 104, workload))

    def test_exact_health_requires_two_trusted_replicas(self):
        healthy = {
            "0": {"trusted": True},
            "1": {"trusted": True},
        }
        self.assertTrue(shadow_soak.exact_health_ready(healthy))
        healthy["1"]["trusted"] = False
        self.assertFalse(shadow_soak.exact_health_ready(healthy))
        self.assertFalse(shadow_soak.exact_health_ready({"0": {"trusted": True}}))
        self.assertFalse(shadow_soak.exact_health_ready(None))

    def test_qualification_requires_exact_target_and_zero_hard_failures(self):
        metrics = shadow_soak.soak_metrics("")
        metrics["attempts"]["stable"] = 10
        metrics["comparisons"]["agree"] = 10
        metrics["overlap_token_sums"]["best"] = 1
        for outcomes in metrics["placement"].values():
            outcomes["kept_agree"] = 10
        for outcomes in metrics["projected_balance"].values():
            outcomes["not_cold"] = 10
        self.assertTrue(shadow_soak.qualification_valid(metrics, 10))
        metrics["attempts"]["attestation_changed"] = 1
        self.assertFalse(shadow_soak.qualification_valid(metrics, 10))

    def test_start_uses_authenticated_exact_endpoint(self):
        response = mock.MagicMock()
        response.__enter__.return_value.status = 202
        with mock.patch.object(shadow_soak.urllib.request, "urlopen", return_value=response) as call:
            self.assertTrue(shadow_soak.start_soak("http://lb:8007", "secret"))
        request = call.call_args.args[0]
        self.assertEqual(
            request.full_url,
            "http://lb:8007/diagnostics/shadow-soak/start",
        )
        self.assertEqual(request.headers["Authorization"], "Bearer secret")

    def test_metrics_listener_base_removes_only_exact_metrics_suffix(self):
        self.assertEqual(
            shadow_soak.metrics_listener_base("http://lb:8007/metrics"),
            "http://lb:8007",
        )
        self.assertEqual(
            shadow_soak.metrics_listener_base("http://lb:8007/metrics/"),
            "http://lb:8007",
        )
        with self.assertRaises(ValueError):
            shadow_soak.metrics_listener_base("http://lb:8007")


if __name__ == "__main__":
    unittest.main()
