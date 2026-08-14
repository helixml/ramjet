#!/usr/bin/env python3
"""Capture real served decisions and wait for the bounded exact-route soak.

Marked requests follow the ordinary sanitize, tokenize, route, dispatch, usage,
and index-update path. The marker is authenticated and stripped upstream. Once
the bounded source target is reached, comparisons are in-memory and issue no
additional inference requests. Output is aggregate and content-free.
"""

import argparse
import json
import os
import time
import urllib.request
from types import SimpleNamespace

from cachebench import fetch_replica_inventory, replica_inventory_change, run_cell
from engine_metrics import metric_value


ATTEMPT_OUTCOMES = (
    "stable",
    "inventory_changed",
    "inventory_untrusted",
    "lookup_error",
    "candidate_mismatch",
    "attestation_wait",
    "attestation_changed",
    "cancelled",
    "attempt_limit",
    "timeout",
    "other",
)
COMPARISON_OUTCOMES = ("agree", "would_move", "tie", "all_zero")
PLACEMENT_OUTCOMES = (
    "would_move",
    "kept_agree",
    "kept_tie",
    "kept_all_zero",
    "kept_ambiguous",
    "kept_gain_gate",
    "kept_load_gate",
    "would_balance",
    "kept_balance_delta_gate",
    "kept_balance_load_gate",
    "fallback",
)
POLICY_OUTCOMES = (
    "not_cold",
    "kept_selected",
    "would_balance",
    "kept_delta_gate",
    "kept_load_gate",
    "fallback",
)
POLICY_LOAD_DELTAS = ("0", "1", "2", "4")
SOURCE_ATTEMPT_OUTCOMES = (
    "stable",
    "tokenizer_unavailable",
    "inventory_changed",
    "inventory_untrusted",
    "lookup_error",
    "candidate_mismatch",
    "attestation_changed",
    "other",
)
TRANSIENT_SOURCE_ATTEMPT_OUTCOMES = (
    "tokenizer_unavailable",
    "attestation_changed",
)


def fetch_text(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return response.read().decode("utf-8", "replace")


def soak_metrics(body):
    def value(name, labels=None):
        result = metric_value(body, name, labels)
        return 0.0 if result is None else result

    return {
        "enabled": value("ds4proxy_shadow_soak_enabled"),
        "complete": value("ds4proxy_shadow_soak_complete"),
        "sources": value("ds4proxy_shadow_soak_sources"),
        "source_token_bytes": value("ds4proxy_shadow_soak_source_token_bytes"),
        "duration_seconds": value("ds4proxy_shadow_soak_duration_seconds"),
        "phases": {
            phase: value("ds4proxy_shadow_soak_phase", {"phase": phase})
            for phase in (
                "off",
                "collecting",
                "ready",
                "running",
                "complete",
                "failed",
            )
        },
        "attempts": {
            outcome: value(
                "ds4proxy_shadow_soak_attempts_total", {"outcome": outcome}
            )
            for outcome in ATTEMPT_OUTCOMES
        },
        "comparisons": {
            outcome: value(
                "ds4proxy_shadow_soak_comparisons_total", {"outcome": outcome}
            )
            for outcome in COMPARISON_OUTCOMES
        },
        "source_comparisons": {
            outcome: value(
                "ds4proxy_shadow_soak_source_comparisons_total",
                {"outcome": outcome},
            )
            for outcome in COMPARISON_OUTCOMES
        },
        "source_attempts": {
            outcome: value(
                "ds4proxy_shadow_soak_source_attempts_total",
                {"outcome": outcome},
            )
            for outcome in SOURCE_ATTEMPT_OUTCOMES
        },
        "overlap_token_sums": {
            choice: value(
                "ds4proxy_shadow_soak_overlap_tokens_sum", {"choice": choice}
            )
            for choice in ("selected", "best")
        },
        "source_overlap_token_sums": {
            choice: value(
                "ds4proxy_shadow_soak_source_overlap_tokens_sum",
                {"choice": choice},
            )
            for choice in ("selected", "best")
        },
        "placement": {
            delta: {
                outcome: value(
                    "ds4proxy_shadow_soak_placement_total",
                    {"max_load_delta": delta, "outcome": outcome},
                )
                for outcome in PLACEMENT_OUTCOMES
            }
            for delta in POLICY_LOAD_DELTAS
        },
        "projected_balance": {
            delta: {
                outcome: value(
                    "ds4proxy_shadow_soak_projected_balance_total",
                    {"max_load_delta": delta, "outcome": outcome},
                )
                for outcome in POLICY_OUTCOMES
            }
            for delta in POLICY_LOAD_DELTAS
        },
    }


def exact_health_ready(inventory, expected_replicas=2):
    return (
        inventory is not None
        and len(inventory) == expected_replicas
        and all(replica["trusted"] for replica in inventory.values())
    )


def qualification_valid(metrics, expected_comparisons):
    forbidden_attempts = (
        "inventory_untrusted",
        "lookup_error",
        "candidate_mismatch",
        "attestation_changed",
        "cancelled",
        "attempt_limit",
        "timeout",
        "other",
    )
    return (
        metrics["attempts"]["stable"] == expected_comparisons
        and sum(metrics["comparisons"].values()) == expected_comparisons
        and metrics["overlap_token_sums"]["best"] > 0
        and all(metrics["attempts"][outcome] == 0 for outcome in forbidden_attempts)
    )


def source_attempts_valid(metrics, expected_sources, workload):
    retry_reasons = workload["retry_reasons"]
    retry_count = sum(retry_reasons.values())
    return (
        workload["client_attempts_total"] == expected_sources + retry_count
        and metrics["stable"] == expected_sources
        and all(
            metrics[outcome] == retry_reasons.get(outcome, 0)
            for outcome in TRANSIENT_SOURCE_ATTEMPT_OUTCOMES
        )
        and all(
            metrics[outcome] == 0
            for outcome in SOURCE_ATTEMPT_OUTCOMES
            if outcome not in ("stable", *TRANSIENT_SOURCE_ATTEMPT_OUTCOMES)
        )
    )
def start_soak(base, token, timeout=10):
    request = urllib.request.Request(
        base.rstrip("/") + "/diagnostics/shadow-soak/start",
        data=b"",
        headers={"Authorization": "Bearer " + token},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.status == 202


def metrics_listener_base(metrics_url):
    suffix = "/metrics"
    normalized = metrics_url.rstrip("/")
    if not normalized.endswith(suffix):
        raise ValueError("--metrics-url must end in /metrics")
    return normalized[: -len(suffix)]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", help="mini-dynamo API base URL")
    parser.add_argument("model")
    parser.add_argument("--apps", type=int, default=52)
    parser.add_argument("--sessions", type=int, default=1)
    parser.add_argument("--turns", type=int, default=2)
    parser.add_argument("--prefix-kib", type=int, default=529)
    parser.add_argument("--concurrency", type=int, default=2)
    parser.add_argument("--salt", required=True)
    parser.add_argument("--token", default=os.environ.get("BENCH_TOKEN"))
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--soak-timeout", type=float, default=300)
    parser.add_argument("--expected-comparisons", type=int, default=100000)
    parser.add_argument("--poll-seconds", type=float, default=0.25)
    parser.add_argument("--engine-metrics", action="append", default=[])
    parser.add_argument("--metrics-url", required=True)
    parser.add_argument("--settle-seconds", type=float, default=0.25)
    parser.add_argument("--progress-every", type=int, default=8)
    parser.add_argument("--capture-retries", type=int, default=20)
    parser.add_argument("--retry-delay-seconds", type=float, default=0.25)
    parser.add_argument("--capture-retry-timeout", type=float, default=330)
    args = parser.parse_args(argv)
    if not args.token:
        parser.error("--token or BENCH_TOKEN is required")
    if min(args.apps, args.sessions, args.turns, args.prefix_kib, args.concurrency) < 1:
        parser.error("workload dimensions and concurrency must be positive")
    if args.expected_comparisons < 1:
        parser.error("--expected-comparisons must be positive")
    if args.capture_retries < 0 or args.retry_delay_seconds < 0:
        parser.error("capture retry bounds must be non-negative")
    if args.capture_retries > 50 or args.capture_retries * args.retry_delay_seconds > 30:
        parser.error("capture retries must add at most 30 seconds and 50 attempts")
    if not args.timeout <= args.capture_retry_timeout <= args.timeout + 60:
        parser.error("capture retry timeout must be within 60 seconds of request timeout")
    if len(args.engine_metrics) != 2:
        parser.error("exactly two --engine-metrics URLs are required")
    metrics_url = args.metrics_url
    try:
        diagnostics_base = metrics_listener_base(metrics_url)
    except ValueError as error:
        parser.error(str(error))
    initial_body = fetch_text(metrics_url)
    initial = soak_metrics(initial_body)
    if initial["enabled"] != 1 or initial["phases"]["collecting"] != 1:
        raise SystemExit("shadow soak is not in the collecting phase")
    health_before = fetch_replica_inventory(args.base)
    if not exact_health_ready(health_before):
        raise SystemExit("both exact inventories must be trusted before capture")
    started = time.perf_counter()
    workload = run_cell(
        SimpleNamespace(
            base=args.base,
            model=args.model,
            sessions=args.sessions,
            turns=args.turns,
            prefix_kib=args.prefix_kib,
            max_tokens=8,
            concurrency=args.concurrency,
            salt=args.salt,
            metrics_url=args.metrics_url,
            engine_metrics=args.engine_metrics,
            timeout=args.timeout,
            settle_seconds=args.settle_seconds,
            reconcile_tolerance=0,
            emit_requests=False,
            progress_every=args.progress_every,
            request_retries=args.capture_retries,
            retry_delay_seconds=args.retry_delay_seconds,
            retry_timeout_seconds=args.capture_retry_timeout,
        ),
        args.apps,
        args.token,
        {"X-Mini-Dynamo-Shadow-Soak": "capture"},
    )
    expected_sources = args.apps * args.sessions * args.turns
    if (
        workload["successful"] != expected_sources
        or not workload["reconciliation"]["consistent"]
    ):
        print(
            json.dumps(
                {
                    "type": "shadow_soak_source_failure",
                    "source_concurrency": args.concurrency,
                    "source_workload": workload,
                    "soak": soak_metrics(fetch_text(metrics_url)),
                },
                sort_keys=True,
            )
        )
        raise SystemExit("source workload failed exact response/LB/engine reconciliation")
    ready = soak_metrics(fetch_text(metrics_url))
    if ready["phases"]["ready"] != 1 or ready["sources"] != expected_sources:
        raise SystemExit("shadow soak did not reach the exact source boundary")
    if not start_soak(diagnostics_base, args.token):
        raise SystemExit("shadow soak start was not accepted")
    deadline = time.monotonic() + args.soak_timeout
    final_body = None
    final = None
    while time.monotonic() < deadline:
        final_body = fetch_text(metrics_url)
        final = soak_metrics(final_body)
        if final["phases"]["complete"] == 1 or final["phases"]["failed"] == 1:
            break
        time.sleep(args.poll_seconds)
    if final is None or final["phases"]["complete"] != 1 or final["complete"] != 1:
        raise SystemExit("shadow soak did not complete successfully")
    health_after = fetch_replica_inventory(args.base)
    exact_trusted_before_after = exact_health_ready(health_after) and all(
        replica["trusted"] for replica in health_before.values()
    )
    qualification_passed = qualification_valid(final, args.expected_comparisons)
    source_bounds_valid = (
        final["sources"] == expected_sources
        and final["source_token_bytes"] > 0
        and sum(final["source_comparisons"].values()) == expected_sources
        and source_attempts_valid(
            final["source_attempts"],
            expected_sources,
            workload,
        )
        and sum(
            final["source_comparisons"][outcome]
            for outcome in ("agree", "would_move", "tie")
        )
        > 0
        and final["source_overlap_token_sums"]["best"] > 0
        and all(
            sum(outcomes.values()) == expected_sources
            for outcomes in final["placement"].values()
        )
        and all(
            sum(outcomes.values()) == expected_sources
            for outcomes in final["projected_balance"].values()
        )
    )
    output = {
        "type": "shadow_soak",
        "unique_sources": expected_sources,
        "source_concurrency": args.concurrency,
        "capture_retry_limit": args.capture_retries,
        "capture_retry_timeout_seconds": args.capture_retry_timeout,
        "capture_and_soak_wall_seconds": round(time.perf_counter() - started, 3),
        "source_workload": workload,
        "soak": final,
        "exact_trusted_before_after": exact_trusted_before_after,
        "qualification_valid": qualification_passed,
        "source_bounds_valid": source_bounds_valid,
        "replica_exact_inventory": replica_inventory_change(
            health_before, health_after
        ),
    }
    print(json.dumps(output, sort_keys=True))
    if (
        not exact_trusted_before_after
        or not qualification_passed
        or not source_bounds_valid
    ):
        raise SystemExit(2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
