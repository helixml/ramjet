#!/usr/bin/env python3
"""Build a correctness-gated TTFT/TPOT SLO-goodput Pareto report.

Input is one bounded, privacy-safe campaign manifest. Direct-engine crossover
cells and serial LB/cache cells are separate comparison cohorts. Frontier
membership is explanatory: this tool never emits a promotion recommendation.
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import re
import sys


INPUT_SCHEMA_VERSION = 1
OUTPUT_SCHEMA_VERSION = 1
INPUT_TYPE = "slo_goodput_campaign"
OUTPUT_TYPE = "slo_goodput_pareto_report"
MAX_INPUT_BYTES = 16 << 20
MAX_CELLS = 10_000
MAX_REQUESTS_PER_CELL = 100_000
MAX_CONFIGURATIONS_PER_COHORT = 256
DOMAINS = {"direct_engine_crossover", "lb_serial"}
CORRECTNESS_VALUES = {
    "protocol": {"pass", "fail", "not_evaluated"},
    "task": {"pass", "fail", "not_evaluated"},
    "tool_use": {"pass", "fail", "not_applicable", "not_evaluated"},
}


class ReportError(ValueError):
    pass


def finite(value: object) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def positive(value: object) -> bool:
    return finite(value) and float(value) > 0


def nonnegative(value: object) -> bool:
    return finite(value) and float(value) >= 0


def bounded_name(value: object, field: str) -> str:
    if not isinstance(value, str) or re.fullmatch(
        r"[a-zA-Z0-9][a-zA-Z0-9._:/-]{0,127}", value
    ) is None:
        raise ReportError(f"{field} is invalid")
    return value


def digest(value: object, field: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ReportError(f"{field} is invalid")
    return value


def exact_keys(
    document: object, expected: set[str], field: str
) -> dict[str, object]:
    if not isinstance(document, dict) or set(document) != expected:
        raise ReportError(f"{field} schema is invalid")
    return document


def percentile(values: list[float], quantile: float) -> float:
    if not values:
        raise ReportError("a required sample is missing")
    ordered = sorted(values)
    position = (len(ordered) - 1) * quantile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] + fraction * (ordered[upper] - ordered[lower])


def median(values: list[float]) -> float:
    return percentile(values, 0.5)


def rounded(value: float) -> float:
    return round(value, 6)


def validate_correctness(raw: object, identity: str) -> dict[str, str]:
    correctness = exact_keys(
        raw, set(CORRECTNESS_VALUES), f"request {identity} correctness"
    )
    for field, allowed in CORRECTNESS_VALUES.items():
        if correctness[field] not in allowed:
            raise ReportError(f"request {identity} correctness is invalid")
    return {field: str(correctness[field]) for field in sorted(correctness)}


def validate_request(raw: object, cell_id: str, index: int) -> dict[str, object]:
    identity = f"{cell_id}/{index}"
    request = exact_keys(
        raw,
        {
            "completed",
            "http_status",
            "correctness",
            "ttft_ms",
            "tpot_ms",
            "prompt_tokens",
            "cached_tokens",
            "completion_tokens",
        },
        f"request {identity}",
    )
    if not isinstance(request["completed"], bool):
        raise ReportError(f"request {identity} completion is invalid")
    status = request["http_status"]
    if (
        not isinstance(status, int)
        or isinstance(status, bool)
        or not 100 <= status <= 599
    ):
        raise ReportError(f"request {identity} HTTP status is invalid")
    for field in ("ttft_ms", "tpot_ms"):
        if request[field] is not None and not nonnegative(request[field]):
            raise ReportError(f"request {identity} {field} is invalid")
    prompt = request["prompt_tokens"]
    cached = request["cached_tokens"]
    completion = request["completion_tokens"]
    if (
        not isinstance(prompt, int)
        or isinstance(prompt, bool)
        or prompt < 0
        or not isinstance(cached, int)
        or isinstance(cached, bool)
        or not 0 <= cached <= prompt
        or not isinstance(completion, int)
        or isinstance(completion, bool)
        or completion < 0
    ):
        raise ReportError(f"request {identity} token counts are invalid")
    return {
        "completed": request["completed"],
        "http_status": status,
        "correctness": validate_correctness(request["correctness"], identity),
        "ttft_ms": request["ttft_ms"],
        "tpot_ms": request["tpot_ms"],
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "completion_tokens": completion,
    }


def validate_native(raw: object, cell_id: str) -> dict[str, object] | None:
    if raw is None:
        return None
    native = exact_keys(
        raw,
        {"reconciled", "effective_tokens_per_step"},
        f"cell {cell_id} native metrics",
    )
    if not isinstance(native["reconciled"], bool):
        raise ReportError(f"cell {cell_id} native reconciliation is invalid")
    value = native["effective_tokens_per_step"]
    if native["reconciled"]:
        if not nonnegative(value):
            raise ReportError(f"cell {cell_id} native metric is invalid")
    elif value is not None:
        raise ReportError(f"cell {cell_id} unreconciled native value is forbidden")
    return {"reconciled": native["reconciled"], "effective_tokens_per_step": value}


def validate_cell(raw: object) -> dict[str, object]:
    cell = dict(
        exact_keys(
            raw,
            {
                "cell_id",
                "configuration",
                "configuration_digest",
                "workload_identity",
                "workload_digest",
                "comparison_domain",
                "repetition",
                "gpu_count",
                "observation_window_seconds",
                "crossover_round",
                "engine_ordinal",
                "native",
                "requests",
            },
            "cell",
        )
    )
    for field in (
        "cell_id",
        "configuration",
        "workload_identity",
    ):
        cell[field] = bounded_name(cell[field], field)
    cell["configuration_digest"] = digest(
        cell["configuration_digest"], "configuration_digest"
    )
    cell["workload_digest"] = digest(cell["workload_digest"], "workload_digest")
    domain = cell["comparison_domain"]
    if domain not in DOMAINS:
        raise ReportError(f"cell {cell['cell_id']} comparison_domain is invalid")
    repetition = cell["repetition"]
    gpu_count = cell["gpu_count"]
    if (
        not isinstance(repetition, int)
        or isinstance(repetition, bool)
        or repetition < 0
        or not isinstance(gpu_count, int)
        or isinstance(gpu_count, bool)
        or not 1 <= gpu_count <= 1024
        or not positive(cell["observation_window_seconds"])
    ):
        raise ReportError(f"cell {cell['cell_id']} numeric metadata is invalid")
    if domain == "direct_engine_crossover":
        if (
            not isinstance(cell["crossover_round"], int)
            or isinstance(cell["crossover_round"], bool)
            or cell["crossover_round"] < 1
            or cell["engine_ordinal"] not in {"a", "b"}
        ):
            raise ReportError(f"cell {cell['cell_id']} crossover provenance is invalid")
    elif cell["crossover_round"] is not None or cell["engine_ordinal"] is not None:
        raise ReportError(f"cell {cell['cell_id']} serial provenance is invalid")
    requests = cell["requests"]
    if not isinstance(requests, list) or not 1 <= len(requests) <= MAX_REQUESTS_PER_CELL:
        raise ReportError(f"cell {cell['cell_id']} request cardinality is invalid")
    return {
        **cell,
        "native": validate_native(cell["native"], str(cell["cell_id"])),
        "requests": [
            validate_request(request, str(cell["cell_id"]), index)
            for index, request in enumerate(requests)
        ],
    }


def validate_manifest(raw: object) -> dict[str, object]:
    manifest = exact_keys(
        raw,
        {
            "schema_version",
            "type",
            "campaign_id",
            "expected_repetitions",
            "slo_pairs",
            "cells",
        },
        "manifest",
    )
    if (
        manifest["schema_version"] != INPUT_SCHEMA_VERSION
        or manifest["type"] != INPUT_TYPE
    ):
        raise ReportError("manifest schema or type is unsupported")
    campaign_id = bounded_name(manifest["campaign_id"], "campaign_id")
    expected = manifest["expected_repetitions"]
    if (
        not isinstance(expected, int)
        or isinstance(expected, bool)
        or not 1 <= expected <= 1000
    ):
        raise ReportError("expected_repetitions is invalid")
    pairs = manifest["slo_pairs"]
    if not isinstance(pairs, list) or not 1 <= len(pairs) <= 64:
        raise ReportError("slo_pairs cardinality is invalid")
    validated_pairs = []
    names = set()
    for raw_pair in pairs:
        pair = exact_keys(raw_pair, {"name", "ttft_ms", "tpot_ms"}, "SLO pair")
        name = bounded_name(pair["name"], "SLO name")
        if name in names or not positive(pair["ttft_ms"]) or not positive(pair["tpot_ms"]):
            raise ReportError("SLO pair is duplicate or invalid")
        names.add(name)
        validated_pairs.append(
            {
                "name": name,
                "ttft_ms": float(pair["ttft_ms"]),
                "tpot_ms": float(pair["tpot_ms"]),
            }
        )
    validated_pairs.sort(key=lambda pair: pair["name"])
    cells = manifest["cells"]
    if not isinstance(cells, list) or not 1 <= len(cells) <= MAX_CELLS:
        raise ReportError("cells cardinality is invalid")
    validated_cells = [validate_cell(cell) for cell in cells]
    ids = [cell["cell_id"] for cell in validated_cells]
    if len(ids) != len(set(ids)):
        raise ReportError("cell_id is duplicated")
    configuration_labels = {}
    workload_labels = {}
    for cell in validated_cells:
        prior_configuration = configuration_labels.setdefault(
            cell["configuration_digest"], cell["configuration"]
        )
        prior_workload = workload_labels.setdefault(
            cell["workload_digest"], cell["workload_identity"]
        )
        if (
            prior_configuration != cell["configuration"]
            or prior_workload != cell["workload_identity"]
        ):
            raise ReportError("digest identity maps to conflicting labels")
    groups: dict[tuple[object, ...], list[dict[str, object]]] = {}
    for cell in validated_cells:
        key = (
            cell["configuration_digest"],
            cell["workload_digest"],
            cell["comparison_domain"],
        )
        groups.setdefault(key, []).append(cell)
    expected_repetitions = set(range(expected))
    for group in groups.values():
        observed = [cell["repetition"] for cell in group]
        if len(observed) != len(set(observed)) or set(observed) != expected_repetitions:
            raise ReportError("configuration repetitions are incomplete or duplicated")
        if len({cell["configuration"] for cell in group}) != 1 or len(
            {cell["workload_identity"] for cell in group}
        ) != 1:
            raise ReportError("digest identity maps to conflicting labels")
        if len({cell["gpu_count"] for cell in group}) != 1:
            raise ReportError("one configuration changed GPU count across repetitions")

    cohorts: dict[tuple[str, str], list[dict[str, object]]] = {}
    for cell in validated_cells:
        key = (str(cell["comparison_domain"]), str(cell["workload_digest"]))
        cohorts.setdefault(key, []).append(cell)
    for (domain, _workload), cohort in cohorts.items():
        configurations = {cell["configuration_digest"] for cell in cohort}
        if len(configurations) > MAX_CONFIGURATIONS_PER_COHORT:
            raise ReportError("comparison cohort exceeds its configuration bound")
        if len({len(cell["requests"]) for cell in cohort}) != 1:
            raise ReportError("comparison cohort changed offered request count")
        if domain != "direct_engine_crossover":
            continue
        if len(configurations) != 2 or expected < 2 or expected % 2 != 0:
            raise ReportError(
                "direct crossover requires two configurations and balanced round pairs"
            )
        round_ids = set()
        for repetition in range(expected):
            entries = [cell for cell in cohort if cell["repetition"] == repetition]
            if (
                len(entries) != 2
                or {cell["configuration_digest"] for cell in entries} != configurations
                or {cell["engine_ordinal"] for cell in entries} != {"a", "b"}
                or len({cell["crossover_round"] for cell in entries}) != 1
            ):
                raise ReportError("direct crossover round is incomplete or unbalanced")
            round_ids.add(entries[0]["crossover_round"])
        if round_ids != set(range(1, expected + 1)):
            raise ReportError("direct crossover round identity is incomplete or duplicated")
        for configuration in configurations:
            engines = [
                cell["engine_ordinal"]
                for cell in cohort
                if cell["configuration_digest"] == configuration
            ]
            if engines.count("a") != engines.count("b"):
                raise ReportError("direct crossover engine assignments are not balanced")
    validated_cells.sort(
        key=lambda cell: (
            cell["comparison_domain"],
            cell["workload_digest"],
            cell["gpu_count"],
            cell["configuration_digest"],
            cell["repetition"],
        )
    )
    return {
        "schema_version": INPUT_SCHEMA_VERSION,
        "type": INPUT_TYPE,
        "campaign_id": campaign_id,
        "expected_repetitions": expected,
        "slo_pairs": validated_pairs,
        "cells": validated_cells,
    }


def request_is_correct(request: dict[str, object]) -> bool:
    correctness = request["correctness"]
    return (
        request["completed"]
        and 200 <= request["http_status"] < 300
        and correctness["protocol"] == "pass"
        and correctness["task"] == "pass"
        and correctness["tool_use"] in {"pass", "not_applicable"}
        and positive(request["ttft_ms"])
        and positive(request["tpot_ms"])
        and request["prompt_tokens"] > 0
        and request["completion_tokens"] >= 2
    )


def repetition_metrics(
    cell: dict[str, object], slo_pairs: list[dict[str, object]]
) -> dict[str, object]:
    requests = cell["requests"]
    correct = [request_is_correct(request) for request in requests]
    eligible = all(correct)
    gpu_hours = (
        float(cell["observation_window_seconds"]) * int(cell["gpu_count"]) / 3600
    )
    prompt = sum(request["prompt_tokens"] for request in requests)
    cached = sum(request["cached_tokens"] for request in requests)
    completion = sum(request["completion_tokens"] for request in requests)
    per_slo = {}
    for slo in slo_pairs:
        qualified = sum(
            valid
            and request["ttft_ms"] <= slo["ttft_ms"]
            and request["tpot_ms"] <= slo["tpot_ms"]
            for valid, request in zip(correct, requests, strict=True)
        )
        per_slo[slo["name"]] = {
            "qualified_requests": qualified if eligible else 0,
            "attainment_pct": rounded(100 * qualified / len(requests)) if eligible else 0.0,
            "qualified_requests_per_gpu_hour": (
                qualified / gpu_hours if eligible else 0.0
            ),
        }
    native = cell["native"]
    return {
        "cell_id": cell["cell_id"],
        "repetition": cell["repetition"],
        "eligible": eligible,
        "attempted_requests": len(requests),
        "completed_requests": sum(request["completed"] for request in requests),
        "correct_requests": sum(correct),
        "completion_rate": rounded(
            sum(request["completed"] for request in requests) / len(requests)
        ),
        "correctness_rate": rounded(sum(correct) / len(requests)),
        "observation_window_seconds": float(cell["observation_window_seconds"]),
        "gpu_count": cell["gpu_count"],
        "aggregate_output_tok_s": rounded(
            completion / float(cell["observation_window_seconds"])
        ),
        "cache_hit_ratio": rounded(cached / prompt) if prompt else 0.0,
        "cache_outcomes": {
            "cold": sum(request["cached_tokens"] == 0 for request in requests),
            "partial": sum(
                0 < request["cached_tokens"] < request["prompt_tokens"]
                for request in requests
            ),
            "full": sum(
                request["prompt_tokens"] > 0
                and request["cached_tokens"] == request["prompt_tokens"]
                for request in requests
            ),
        },
        "ttft_ms_p95": (
            rounded(percentile([float(request["ttft_ms"]) for request in requests], 0.95))
            if all(request["ttft_ms"] is not None for request in requests)
            else None
        ),
        "tpot_ms_p95": (
            rounded(percentile([float(request["tpot_ms"]) for request in requests], 0.95))
            if all(request["tpot_ms"] is not None for request in requests)
            else None
        ),
        "effective_tokens_per_step": (
            native["effective_tokens_per_step"]
            if native is not None and native["reconciled"]
            else None
        ),
        "per_slo": per_slo,
    }


def configuration_summary(
    cells: list[dict[str, object]], slo_pairs: list[dict[str, object]]
) -> dict[str, object]:
    repetitions = [repetition_metrics(cell, slo_pairs) for cell in cells]
    eligible = all(item["eligible"] for item in repetitions)
    per_slo = {}
    objective_ranges = {}
    for slo in slo_pairs:
        values = [
            float(item["per_slo"][slo["name"]]["qualified_requests_per_gpu_hour"])
            for item in repetitions
        ]
        per_slo[slo["name"]] = {
            "ttft_ms": slo["ttft_ms"],
            "tpot_ms": slo["tpot_ms"],
            "goodput_per_gpu_hour": {
                "min": rounded(min(values)),
                "median": rounded(median(values)),
                "max": rounded(max(values)),
            },
            "qualified_requests": sum(
                item["per_slo"][slo["name"]]["qualified_requests"]
                for item in repetitions
            ),
        }
        objective_ranges[slo["name"]] = (min(values), max(values))
    return {
        "configuration": cells[0]["configuration"],
        "configuration_digest": cells[0]["configuration_digest"],
        "workload_identity": cells[0]["workload_identity"],
        "workload_digest": cells[0]["workload_digest"],
        "comparison_domain": cells[0]["comparison_domain"],
        "gpu_count": cells[0]["gpu_count"],
        "eligible": eligible,
        "pareto_status": "ineligible" if not eligible else "non_dominated",
        "dominated_by": [],
        "uncertainty_overlap": False,
        "per_slo": per_slo,
        "repetition_metrics": repetitions,
        "_objective_ranges": objective_ranges,
    }


def dominates(left: dict[str, object], right: dict[str, object]) -> bool:
    """Conservative range dominance: every left min beats every right max."""
    no_worse = True
    strictly_better = False
    for name in sorted(left["_objective_ranges"]):
        left_min = left["_objective_ranges"][name][0]
        right_max = right["_objective_ranges"][name][1]
        no_worse = no_worse and left_min >= right_max
        strictly_better = strictly_better or left_min > right_max
    return no_worse and strictly_better


def ranges_overlap(left: dict[str, object], right: dict[str, object]) -> bool:
    return any(
        left["_objective_ranges"][name][0]
        <= right["_objective_ranges"][name][1]
        and right["_objective_ranges"][name][0]
        <= left["_objective_ranges"][name][1]
        for name in left["_objective_ranges"]
    )


def build_report(raw: object) -> dict[str, object]:
    manifest = validate_manifest(raw)
    grouped: dict[tuple[object, ...], list[dict[str, object]]] = {}
    for cell in manifest["cells"]:
        key = (
            cell["comparison_domain"],
            cell["workload_digest"],
            cell["configuration_digest"],
        )
        grouped.setdefault(key, []).append(cell)
    summaries = [
        configuration_summary(
            sorted(cells, key=lambda cell: cell["repetition"]),
            manifest["slo_pairs"],
        )
        for _key, cells in sorted(grouped.items())
    ]
    for summary in summaries:
        if not summary["eligible"]:
            continue
        peers = [
            peer
            for peer in summaries
            if peer is not summary
            and peer["eligible"]
            and peer["comparison_domain"] == summary["comparison_domain"]
            and peer["workload_digest"] == summary["workload_digest"]
        ]
        summary["dominated_by"] = sorted(
            peer["configuration_digest"]
            for peer in peers
            if dominates(peer, summary)
        )
        summary["pareto_status"] = (
            "dominated" if summary["dominated_by"] else "non_dominated"
        )
        summary["uncertainty_overlap"] = any(
            ranges_overlap(summary, peer) for peer in peers
        )
    for summary in summaries:
        del summary["_objective_ranges"]
    return {
        "schema_version": OUTPUT_SCHEMA_VERSION,
        "type": OUTPUT_TYPE,
        "campaign_id": manifest["campaign_id"],
        "dominance_basis": "observed_repetition_range",
        "objective_contract": [
            {
                "field": f"per_slo.{slo['name']}.goodput_per_gpu_hour",
                "direction": "maximize",
                "comparison": "candidate_min_gte_peer_max",
            }
            for slo in manifest["slo_pairs"]
        ],
        "promotion": {"automatic": False, "decision": "not_evaluated"},
        "expected_repetitions": manifest["expected_repetitions"],
        "slo_pairs": manifest["slo_pairs"],
        "raw_cells": manifest["cells"],
        "classifications": summaries,
    }


def print_human(report: dict[str, object]) -> None:
    print(
        f"campaign={report['campaign_id']} automatic_promotion=false "
        f"cells={len(report['raw_cells'])} configurations={len(report['classifications'])}"
    )
    print("domain workload configuration status overlap goodput_ranges")
    for item in report["classifications"]:
        ranges = ",".join(
            f"{name}:{values['goodput_per_gpu_hour']['min']}/"
            f"{values['goodput_per_gpu_hour']['median']}/"
            f"{values['goodput_per_gpu_hour']['max']}"
            for name, values in sorted(item["per_slo"].items())
        )
        print(
            f"{item['comparison_domain']} {item['workload_identity']} "
            f"{item['configuration']} {item['pareto_status']} "
            f"{str(item['uncertainty_overlap']).lower()} {ranges}"
        )


def example_manifest() -> dict[str, object]:
    request = {
        "completed": True,
        "http_status": 200,
        "correctness": {
            "protocol": "pass",
            "task": "pass",
            "tool_use": "not_applicable",
        },
        "ttft_ms": 250.0,
        "tpot_ms": 20.0,
        "prompt_tokens": 1024,
        "cached_tokens": 512,
        "completion_tokens": 128,
    }
    cells = []
    for repetition in range(2):
        cells.append(
            {
                "cell_id": f"candidate-r{repetition}",
                "configuration": "candidate",
                "configuration_digest": "1" * 64,
                "workload_identity": "code-c8-max256",
                "workload_digest": "2" * 64,
                "comparison_domain": "lb_serial",
                "repetition": repetition,
                "gpu_count": 8,
                "observation_window_seconds": 30.0,
                "crossover_round": None,
                "engine_ordinal": None,
                "native": None,
                "requests": [dict(request)],
            }
        )
    return {
        "schema_version": INPUT_SCHEMA_VERSION,
        "type": INPUT_TYPE,
        "campaign_id": "example-offline-campaign",
        "expected_repetitions": 2,
        "slo_pairs": [
            {"name": "interactive", "ttft_ms": 500.0, "tpot_ms": 50.0}
        ],
        "cells": cells,
    }


def load_manifest(path: str) -> object:
    if path == "-":
        raw = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
    else:
        source = pathlib.Path(path)
        if not source.is_file() or source.is_symlink():
            raise ReportError("manifest path must be a regular file")
        if source.stat().st_size > MAX_INPUT_BYTES:
            raise ReportError("manifest exceeds its byte bound")
        raw = source.read_bytes()
    if not raw or len(raw) > MAX_INPUT_BYTES:
        raise ReportError("manifest is empty or exceeds its byte bound")
    try:
        return json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise ReportError("manifest JSON is malformed") from error


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", nargs="?", help="bounded campaign manifest, or - for stdin")
    parser.add_argument("--json", action="store_true", help="emit machine JSON")
    parser.add_argument(
        "--print-example",
        action="store_true",
        help="print a valid normalized campaign manifest and exit",
    )
    args = parser.parse_args(argv)
    if args.print_example:
        if args.manifest is not None:
            parser.error("--print-example does not accept a manifest")
        print(json.dumps(example_manifest(), sort_keys=True, indent=2))
        return 0
    if args.manifest is None:
        parser.error("a manifest is required unless --print-example is used")
    try:
        report = build_report(load_manifest(args.manifest))
    except ReportError as error:
        print(f"SLO Pareto report failed closed: {error}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    else:
        print_human(report)
    return 3 if any(not item["eligible"] for item in report["classifications"]) else 0


if __name__ == "__main__":
    raise SystemExit(main())
