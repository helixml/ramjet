import io
import json
import tempfile
import unittest
from unittest import mock

from slo_pareto_report import ReportError, build_report, dominates, main


def hex_digest(value):
    return f"{value:064x}"


def correctness(protocol="pass", task="pass", tool_use="not_applicable"):
    return {"protocol": protocol, "task": task, "tool_use": tool_use}


def request(
    *,
    completed=True,
    status=200,
    correctness_value=None,
    ttft=100,
    tpot=10,
    prompt=100,
    cached=0,
    completion=100,
):
    return {
        "completed": completed,
        "http_status": status,
        "correctness": correctness_value or correctness(),
        "ttft_ms": ttft,
        "tpot_ms": tpot,
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "completion_tokens": completion,
    }


def cell(
    configuration,
    config_number,
    *,
    repetition=0,
    domain="lb_serial",
    workload="code-c8-max256",
    workload_number=100,
    gpu_count=4,
    window=10,
    requests=None,
    native=True,
):
    direct = domain == "direct_engine_crossover"
    return {
        "cell_id": f"{configuration}-r{repetition}-{domain}",
        "configuration": configuration,
        "configuration_digest": hex_digest(config_number),
        "workload_identity": workload,
        "workload_digest": hex_digest(workload_number),
        "comparison_domain": domain,
        "repetition": repetition,
        "gpu_count": gpu_count,
        "observation_window_seconds": window,
        "crossover_round": repetition + 1 if direct else None,
        "engine_ordinal": "a" if direct and repetition % 2 == 0 else ("b" if direct else None),
        "native": (
            {"reconciled": True, "effective_tokens_per_step": 1.5}
            if native
            else None
        ),
        "requests": requests or [request()],
    }


def manifest(cells, *, repetitions=1, slo_pairs=None):
    return {
        "schema_version": 1,
        "type": "slo_goodput_campaign",
        "campaign_id": "r127-fixture",
        "expected_repetitions": repetitions,
        "slo_pairs": slo_pairs
        or [{"name": "interactive", "ttft_ms": 500, "tpot_ms": 50}],
        "cells": cells,
    }


def by_configuration(report):
    return {item["configuration"]: item for item in report["classifications"]}


class SloParetoReportTests(unittest.TestCase):
    def test_one_repetition_strict_dominance_and_raw_retention(self):
        better = cell(
            "better",
            1,
            window=5,
            requests=[request(cached=50)],
        )
        worse = cell("worse", 2, window=10)
        report = build_report(manifest([worse, better]))
        summaries = by_configuration(report)
        self.assertEqual(summaries["better"]["pareto_status"], "non_dominated")
        self.assertEqual(summaries["worse"]["pareto_status"], "dominated")
        self.assertEqual(summaries["worse"]["dominated_by"], [hex_digest(1)])
        self.assertEqual(len(report["raw_cells"]), 2)
        self.assertEqual(
            report["promotion"], {"automatic": False, "decision": "not_evaluated"}
        )
        self.assertEqual(report["dominance_basis"], "observed_repetition_range")

    def test_exact_ties_are_non_dominated(self):
        report = build_report(manifest([cell("a", 1), cell("b", 2)]))
        self.assertEqual(
            [item["pareto_status"] for item in report["classifications"]],
            ["non_dominated", "non_dominated"],
        )
        self.assertTrue(all(item["uncertainty_overlap"] for item in report["classifications"]))

    def test_missing_latency_or_correctness_field_fails_closed(self):
        base = cell("candidate", 1)
        cases = []
        for field in ("ttft_ms", "tpot_ms", "correctness"):
            candidate = json.loads(json.dumps(base))
            del candidate["requests"][0][field]
            cases.append((field, candidate))
        for field, candidate in cases:
            with self.subTest(field=field), self.assertRaises(ReportError):
                build_report(manifest([candidate]))

    def test_null_or_not_evaluated_correctness_is_retained_but_ineligible(self):
        candidate = cell(
            "candidate",
            1,
            requests=[
                request(
                    correctness_value=correctness(task="not_evaluated"),
                    ttft=None,
                    tpot=None,
                    completion=0,
                )
            ],
        )
        report = build_report(manifest([candidate]))
        summary = report["classifications"][0]
        self.assertFalse(summary["eligible"])
        self.assertEqual(summary["pareto_status"], "ineligible")
        self.assertEqual(summary["per_slo"]["interactive"]["qualified_requests"], 0)
        self.assertIsNone(summary["repetition_metrics"][0]["ttft_ms_p95"])

    def test_protocol_task_tool_or_completion_failure_is_hard_gate(self):
        cases = (
            request(correctness_value=correctness(protocol="fail")),
            request(correctness_value=correctness(task="fail")),
            request(correctness_value=correctness(tool_use="fail")),
            request(completed=False, status=503),
            request(completion=1),
        )
        for index, failed in enumerate(cases):
            with self.subTest(index=index):
                report = build_report(manifest([cell("candidate", 1, requests=[failed])]))
                self.assertEqual(report["classifications"][0]["pareto_status"], "ineligible")

    def test_repetition_ranges_and_derived_audit_metrics_are_exact(self):
        cells = [
            cell(
                "candidate",
                1,
                repetition=0,
                window=10,
                requests=[request(cached=0, completion=100)],
            ),
            cell(
                "candidate",
                1,
                repetition=1,
                window=20,
                requests=[request(cached=100, completion=200)],
            ),
        ]
        summary = build_report(manifest(cells, repetitions=2))["classifications"][0]
        goodput = summary["per_slo"]["interactive"]["goodput_per_gpu_hour"]
        self.assertEqual(goodput, {"min": 45.0, "median": 67.5, "max": 90.0})
        self.assertEqual(len(summary["repetition_metrics"]), 2)
        self.assertEqual(summary["repetition_metrics"][0]["cache_hit_ratio"], 0)
        self.assertEqual(summary["repetition_metrics"][1]["cache_hit_ratio"], 1)
        self.assertEqual(summary["repetition_metrics"][0]["aggregate_output_tok_s"], 10)

        odd_cells = [
            cell("odd", 3, repetition=index, window=window)
            for index, window in enumerate((20, 10, 5))
        ]
        odd = build_report(manifest(odd_cells, repetitions=3))["classifications"][0]
        self.assertEqual(
            odd["per_slo"]["interactive"]["goodput_per_gpu_hour"]["median"],
            90.0,
        )

    def test_overlapping_ranges_do_not_declare_a_median_winner(self):
        cells = [
            cell("a", 1, repetition=0, window=9),
            cell("a", 1, repetition=1, window=4.5),
            cell("b", 2, repetition=0, window=10),
            cell("b", 2, repetition=1, window=6),
        ]
        summaries = by_configuration(build_report(manifest(cells, repetitions=2)))
        self.assertGreater(
            summaries["a"]["per_slo"]["interactive"]["goodput_per_gpu_hour"]["median"],
            summaries["b"]["per_slo"]["interactive"]["goodput_per_gpu_hour"]["median"],
        )
        self.assertEqual(summaries["a"]["pareto_status"], "non_dominated")
        self.assertEqual(summaries["b"]["pareto_status"], "non_dominated")
        self.assertTrue(summaries["a"]["uncertainty_overlap"])

    def test_slo_tradeoff_keeps_both_configurations_non_dominated(self):
        slos = [
            {"name": "tight", "ttft_ms": 100, "tpot_ms": 10},
            {"name": "loose", "ttft_ms": 500, "tpot_ms": 50},
        ]
        a = cell("a", 1, requests=[request(ttft=50, tpot=5)])
        a["requests"].append(request(ttft=600, tpot=60))
        b = cell(
            "b",
            2,
            requests=[request(ttft=200, tpot=20), request(ttft=200, tpot=20)],
        )
        summaries = by_configuration(build_report(manifest([a, b], slo_pairs=slos)))
        self.assertEqual(summaries["a"]["pareto_status"], "non_dominated")
        self.assertEqual(summaries["b"]["pareto_status"], "non_dominated")

    def test_slo_boundary_is_inclusive(self):
        slos = [{"name": "exact", "ttft_ms": 100, "tpot_ms": 10}]
        summary = build_report(
            manifest([cell("a", 1, requests=[request(ttft=100, tpot=10)])], slo_pairs=slos)
        )["classifications"][0]
        self.assertEqual(summary["per_slo"]["exact"]["qualified_requests"], 1)

    def test_direct_and_lb_serial_cohorts_are_isolated(self):
        direct_a = [
            cell("direct-a", 1, domain="direct_engine_crossover", repetition=index, window=100)
            for index in range(2)
        ]
        direct_b = [
            cell("direct-b", 2, domain="direct_engine_crossover", repetition=index, window=1)
            for index in range(2)
        ]
        direct_b[0]["engine_ordinal"] = "b"
        direct_b[1]["engine_ordinal"] = "a"
        serial = [
            cell("serial", 3, repetition=index, gpu_count=8, window=1, requests=[request(cached=100)])
            for index in range(2)
        ]
        report = build_report(manifest([*direct_a, *direct_b, *serial], repetitions=2))
        summaries = by_configuration(report)
        self.assertEqual(summaries["serial"]["pareto_status"], "non_dominated")
        self.assertEqual(summaries["direct-b"]["pareto_status"], "non_dominated")
        self.assertEqual(summaries["direct-a"]["pareto_status"], "dominated")

    def test_per_gpu_hour_frontier_compares_different_gpu_counts(self):
        tp2 = cell("tp2", 1, gpu_count=2, window=10)
        tp4 = cell("tp4", 2, gpu_count=4, window=10)
        summaries = by_configuration(build_report(manifest([tp2, tp4])))
        self.assertEqual(summaries["tp2"]["pareto_status"], "non_dominated")
        self.assertEqual(summaries["tp4"]["pareto_status"], "dominated")

    def test_incomplete_duplicate_or_cross_domain_provenance_fails(self):
        with self.assertRaisesRegex(ReportError, "repetitions"):
            build_report(manifest([cell("a", 1, repetition=0)], repetitions=2))
        duplicate = [cell("a", 1), cell("a", 1)]
        duplicate[1]["cell_id"] = "other-id"
        with self.assertRaisesRegex(ReportError, "repetitions"):
            build_report(manifest(duplicate))
        bad_serial = cell("serial", 2, domain="lb_serial")
        bad_serial["crossover_round"] = 1
        with self.assertRaisesRegex(ReportError, "serial provenance"):
            build_report(manifest([bad_serial]))
        conflict = [cell("a", 1), cell("b", 2)]
        conflict[1]["workload_digest"] = conflict[0]["workload_digest"]
        conflict[1]["workload_identity"] = "different-workload-label"
        with self.assertRaisesRegex(ReportError, "conflicting labels"):
            build_report(manifest(conflict))
        unbalanced = [
            cell("a", 1, domain="direct_engine_crossover", repetition=index)
            for index in range(2)
        ] + [
            cell("b", 2, domain="direct_engine_crossover", repetition=index)
            for index in range(2)
        ]
        with self.assertRaisesRegex(ReportError, "unbalanced"):
            build_report(manifest(unbalanced, repetitions=2))

        odd_rounds = [
            cell("a", 1, domain="direct_engine_crossover", repetition=index)
            for index in range(3)
        ] + [
            cell("b", 2, domain="direct_engine_crossover", repetition=index)
            for index in range(3)
        ]
        for candidate in odd_rounds:
            if candidate["configuration"] == "b":
                candidate["engine_ordinal"] = (
                    "b" if candidate["engine_ordinal"] == "a" else "a"
                )
        with self.assertRaisesRegex(ReportError, "balanced round pairs"):
            build_report(manifest(odd_rounds, repetitions=3))

    def test_cohort_bounds_and_fixed_offered_load_fail_closed(self):
        oversized = [
            cell(f"candidate-{index}", index + 1)
            for index in range(257)
        ]
        with self.assertRaisesRegex(ReportError, "configuration bound"):
            build_report(manifest(oversized))
        changed_load = [
            cell("a", 1, requests=[request()]),
            cell("b", 2, requests=[request(), request()]),
        ]
        with self.assertRaisesRegex(ReportError, "offered request count"):
            build_report(manifest(changed_load))

    def test_invalid_numbers_cache_and_native_reconciliation_fail(self):
        cases = []
        zero_window = cell("a", 1)
        zero_window["observation_window_seconds"] = 0
        cases.append(zero_window)
        bad_cache = cell("a", 1)
        bad_cache["requests"][0]["cached_tokens"] = 101
        cases.append(bad_cache)
        bad_native = cell("a", 1)
        bad_native["native"] = {"reconciled": False, "effective_tokens_per_step": 1}
        cases.append(bad_native)
        boolean_latency = cell("a", 1)
        boolean_latency["requests"][0]["ttft_ms"] = True
        cases.append(boolean_latency)
        infinite_latency = cell("a", 1)
        infinite_latency["requests"][0]["tpot_ms"] = float("inf")
        cases.append(infinite_latency)
        for candidate in cases:
            with self.assertRaises(ReportError):
                build_report(manifest([candidate]))

        for field in ("ttft_ms", "tpot_ms", "prompt_tokens"):
            zero = cell("zero", 9)
            zero["requests"][0][field] = 0
            with self.subTest(zero_field=field):
                summary = build_report(manifest([zero]))["classifications"][0]
                self.assertEqual(summary["pareto_status"], "ineligible")

    def test_optional_native_metric_absent_is_not_invented(self):
        report = build_report(manifest([cell("a", 1, native=False)]))
        self.assertIsNone(
            report["classifications"][0]["repetition_metrics"][0][
                "effective_tokens_per_step"
            ]
        )

    def test_shuffled_cell_and_slo_order_is_deterministic(self):
        cells = [cell("a", 1), cell("b", 2)]
        slos = [
            {"name": "z", "ttft_ms": 500, "tpot_ms": 50},
            {"name": "a", "ttft_ms": 100, "tpot_ms": 10},
        ]
        first = build_report(manifest(cells, slo_pairs=slos))
        second = build_report(manifest(list(reversed(cells)), slo_pairs=list(reversed(slos))))
        self.assertEqual(
            json.dumps(first, sort_keys=True, separators=(",", ":")),
            json.dumps(second, sort_keys=True, separators=(",", ":")),
        )

    def test_cli_human_json_and_ineligible_exit_codes(self):
        with tempfile.NamedTemporaryFile(mode="w+", encoding="utf-8") as source:
            json.dump(manifest([cell("a", 1)]), source)
            source.flush()
            with mock.patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(main([source.name]), 0)
            self.assertIn("automatic_promotion=false", output.getvalue())
            with mock.patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(main([source.name, "--json"]), 0)
            self.assertEqual(json.loads(output.getvalue())["type"], "slo_goodput_pareto_report")

        invalid = cell("a", 1, requests=[request(ttft=None)])
        with tempfile.NamedTemporaryFile(mode="w+", encoding="utf-8") as source:
            json.dump(manifest([invalid]), source)
            source.flush()
            with mock.patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(main([source.name, "--json"]), 3)

        with mock.patch("sys.stdout", new_callable=io.StringIO) as output:
            self.assertEqual(main(["--print-example"]), 0)
        example = json.loads(output.getvalue())
        self.assertEqual(build_report(example)["campaign_id"], "example-offline-campaign")

    def test_range_dominance_requires_all_objectives_and_one_strict(self):
        left = {"_objective_ranges": {"a": (3, 3), "b": (4, 5)}}
        right = {"_objective_ranges": {"a": (1, 2), "b": (2, 4)}}
        self.assertTrue(dominates(left, right))
        self.assertFalse(dominates(right, left))
        tied = {"_objective_ranges": {"a": (2, 2), "b": (4, 4)}}
        self.assertFalse(dominates(tied, tied))


if __name__ == "__main__":
    unittest.main()
