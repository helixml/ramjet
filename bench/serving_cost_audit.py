#!/usr/bin/env python3
"""Audit delivered serving cost from privacy-bounded route-journal records.

This is an observational audit of the route that actually served each request,
not a counterfactual simulator.  It follows two systems-design ideas:

* MOSAIC makes delivered work architecture-dependent and calibrates it from
  measured performance instead of nominal FLOPs:
  https://arxiv.org/abs/2608.10605
* DistServe evaluates LLM goodput under separate TTFT and TPOT constraints:
  https://www.usenix.org/conference/osdi24/presentation/zhong-yinmin

The journal deliberately contains no prompt text, token IDs, request IDs, or
cache keys.  TTFT includes queueing, transport, and prefill; consequently the
reported TTFT-per-uncached-token ratio is a service-cost signal, not a claim
about isolated engine prefill throughput.

Usage:
  python3 bench/serving_cost_audit.py trace.log
  python3 bench/serving_cost_audit.py trace.log \
    --ttft-slo-ms 2000 --tpot-slo-ms 50 --gpu-count 8 --json
"""

import argparse
from collections import defaultdict
import json
import math
import sys

from route_replay import records


SCHEMA_VERSION = 1


def finite_number(value):
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def nonnegative_number(value):
    return finite_number(value) and value >= 0


def percentile(values, quantile):
    """Return a linearly interpolated percentile for any non-empty sample."""
    if not values:
        return None
    ordered = sorted(float(value) for value in values)
    position = (len(ordered) - 1) * quantile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return round(ordered[lower], 3)
    fraction = position - lower
    return round(ordered[lower] + fraction * (ordered[upper] - ordered[lower]), 3)


def served_candidate(start, finish):
    upstream = finish.get("upstream")
    if not isinstance(upstream, int) or isinstance(upstream, bool) or upstream < 0:
        upstream = start.get("served_chosen", start.get("chosen"))
    candidates = start.get("candidates")
    if not isinstance(upstream, int) or not isinstance(candidates, list):
        return None
    return next(
        (
            candidate
            for candidate in candidates
            if isinstance(candidate, dict) and candidate.get("upstream") == upstream
        ),
        None,
    )


def observation(start, finish):
    """Return one validated cost observation, or None when fields are unusable."""
    if finish.get("result") != "complete":
        return None
    status = finish.get("status")
    if not isinstance(status, int) or isinstance(status, bool) or not 200 <= status < 300:
        return None
    prompt = finish.get("prompt_tokens")
    cached = finish.get("cached_tokens")
    ttft = finish.get("ttft_ms")
    duration = finish.get("duration_ms")
    completion = finish.get("completion_tokens")
    if not all(nonnegative_number(value) for value in (prompt, cached, ttft, duration)):
        return None
    if cached > prompt or duration < ttft:
        return None
    candidate = served_candidate(start, finish)
    request_load_units = None if candidate is None else candidate.get("request_load_units")
    if (
        not isinstance(request_load_units, int)
        or isinstance(request_load_units, bool)
        or request_load_units < 1
    ):
        request_load_units = None
    uncached = float(prompt - cached)
    service_ms_per_uncached_token = float(ttft) / uncached if uncached > 0 else None
    tpot_ms = None
    if nonnegative_number(completion) and completion > 1:
        tpot_ms = float(duration - ttft) / float(completion - 1)
    if cached == 0:
        cache_outcome = "cold"
    elif cached >= prompt:
        cache_outcome = "full"
    else:
        cache_outcome = "partial"
    return {
        "request_load_units": request_load_units,
        "cache_outcome": cache_outcome,
        "prompt_tokens": float(prompt),
        "cached_tokens": float(cached),
        "uncached_tokens": uncached,
        "ttft_ms": float(ttft),
        "duration_ms": float(duration),
        "completion_tokens": float(completion) if nonnegative_number(completion) else None,
        "service_ms_per_uncached_token": service_ms_per_uncached_token,
        "tpot_ms": tpot_ms,
    }


def summarize(items):
    ttfts = [item["ttft_ms"] for item in items]
    service_costs = [
        item["service_ms_per_uncached_token"]
        for item in items
        if item["service_ms_per_uncached_token"] is not None
    ]
    tpots = [item["tpot_ms"] for item in items if item["tpot_ms"] is not None]
    prompt = sum(item["prompt_tokens"] for item in items)
    cached = sum(item["cached_tokens"] for item in items)
    return {
        "requests": len(items),
        "prompt_tokens": round(prompt, 3),
        "cached_tokens": round(cached, 3),
        "uncached_tokens": round(sum(item["uncached_tokens"] for item in items), 3),
        "cache_hit_pct": round(100 * cached / prompt, 3) if prompt else None,
        "ttft_ms_p50": percentile(ttfts, 0.50),
        "ttft_ms_p95": percentile(ttfts, 0.95),
        "ttft_per_uncached_token_samples": len(service_costs),
        "ttft_ms_per_uncached_token_p50": percentile(service_costs, 0.50),
        "ttft_ms_per_uncached_token_p95": percentile(service_costs, 0.95),
        "tpot_samples": len(tpots),
        "tpot_ms_p50": percentile(tpots, 0.50),
        "tpot_ms_p95": percentile(tpots, 0.95),
    }


def audit(parsed, ttft_slo_ms=None, tpot_slo_ms=None, gpu_count=None):
    # Sequence numbers restart with the process. Pair each finish with the most
    # recent unmatched start so concatenated container logs cannot silently
    # overwrite an earlier process lifetime.
    pending_starts = defaultdict(list)
    joined = []
    starts = []
    finishes = []
    unmatched_finishes = 0
    for record in parsed:
        sequence = record.get("seq")
        if not isinstance(sequence, int) or isinstance(sequence, bool):
            continue
        if record.get("event") == "start":
            starts.append(record)
            pending_starts[sequence].append(record)
        elif record.get("event") == "finish":
            finishes.append(record)
            if pending_starts[sequence]:
                joined.append((pending_starts[sequence].pop(), record))
            else:
                unmatched_finishes += 1
    unmatched_starts = sum(len(entries) for entries in pending_starts.values())
    observations = [
        item
        for start, finish in joined
        if (item := observation(start, finish)) is not None
    ]
    by_load = {
        str(units): summarize(
            [item for item in observations if item["request_load_units"] == units]
        )
        for units in sorted(
            {
                item["request_load_units"]
                for item in observations
                if item["request_load_units"] is not None
            }
        )
    }
    by_cache = {
        outcome: summarize(
            [item for item in observations if item["cache_outcome"] == outcome]
        )
        for outcome in ("cold", "partial", "full")
        if any(item["cache_outcome"] == outcome for item in observations)
    }
    report = {
        "schema_version": SCHEMA_VERSION,
        "records": {
            "starts": len(starts),
            "finishes": len(finishes),
            "joined": len(joined),
            "unmatched_starts": unmatched_starts,
            "unmatched_finishes": unmatched_finishes,
            "cost_observations": len(observations),
        },
        "overall": summarize(observations),
        "by_request_load_units": by_load,
        "by_cache_outcome": by_cache,
    }
    if ttft_slo_ms is not None and tpot_slo_ms is not None:
        eligible = [item for item in observations if item["tpot_ms"] is not None]
        qualified = [
            item
            for item in eligible
            if item["ttft_ms"] <= ttft_slo_ms and item["tpot_ms"] <= tpot_slo_ms
        ]
        start_times = [
            record.get("unix_ms")
            for record in starts
            if nonnegative_number(record.get("unix_ms"))
        ]
        finish_times = [
            record.get("unix_ms")
            for record in finishes
            if nonnegative_number(record.get("unix_ms"))
        ]
        window_seconds = (
            max(0.0, (max(finish_times) - min(start_times)) / 1000)
            if start_times and finish_times
            else None
        )
        per_gpu_hour = None
        if gpu_count is not None and window_seconds is not None and window_seconds > 0:
            per_gpu_hour = len(qualified) / (window_seconds / 3600 * gpu_count)
        report["slo"] = {
            "ttft_ms": ttft_slo_ms,
            "tpot_ms": tpot_slo_ms,
            "eligible_requests": len(eligible),
            "qualified_requests": len(qualified),
            "attainment_pct": (
                round(100 * len(qualified) / len(eligible), 3) if eligible else None
            ),
            "observation_window_seconds": (
                round(window_seconds, 3) if window_seconds is not None else None
            ),
            "gpu_count": gpu_count,
            "qualified_requests_per_gpu_hour": (
                round(per_gpu_hour, 3) if per_gpu_hour is not None else None
            ),
        }
    return report


def print_human(report):
    records_summary = report["records"]
    overall = report["overall"]
    print(
        "records "
        f"starts={records_summary['starts']} finishes={records_summary['finishes']} "
        f"joined={records_summary['joined']} cost_observations={records_summary['cost_observations']}"
    )
    print(
        "overall "
        f"requests={overall['requests']} cache_hit_pct={overall['cache_hit_pct']} "
        f"ttft_p50_ms={overall['ttft_ms_p50']} ttft_p95_ms={overall['ttft_ms_p95']} "
        f"tpot_p50_ms={overall['tpot_ms_p50']} tpot_p95_ms={overall['tpot_ms_p95']}"
    )
    print("load_units requests ttft_p50_ms ttft_p95_ms service_ms_per_uncached_tok_p50 tpot_p50_ms")
    for units, summary in report["by_request_load_units"].items():
        print(
            f"{units:>10} {summary['requests']:>8} {str(summary['ttft_ms_p50']):>11} "
            f"{str(summary['ttft_ms_p95']):>11} "
            f"{str(summary['ttft_ms_per_uncached_token_p50']):>36} "
            f"{str(summary['tpot_ms_p50']):>11}"
        )
    if "slo" in report:
        slo = report["slo"]
        print(
            "slo "
            f"ttft_ms={slo['ttft_ms']} tpot_ms={slo['tpot_ms']} "
            f"qualified={slo['qualified_requests']}/{slo['eligible_requests']} "
            f"attainment_pct={slo['attainment_pct']} "
            f"qualified_requests_per_gpu_hour={slo['qualified_requests_per_gpu_hour']}"
        )


def positive_float(raw):
    try:
        value = float(raw)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be a positive finite number") from error
    if not math.isfinite(value) or value <= 0:
        raise argparse.ArgumentTypeError("must be a positive finite number")
    return value


def positive_int(raw):
    try:
        value = int(raw)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be a positive integer") from error
    if value <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", help="journal/log file, or - for stdin")
    parser.add_argument("--ttft-slo-ms", type=positive_float)
    parser.add_argument("--tpot-slo-ms", type=positive_float)
    parser.add_argument("--gpu-count", type=positive_int)
    parser.add_argument("--json", action="store_true", help="emit one JSON report")
    args = parser.parse_args(argv)
    if (args.ttft_slo_ms is None) != (args.tpot_slo_ms is None):
        parser.error("--ttft-slo-ms and --tpot-slo-ms must be set together")
    if args.gpu_count is not None and args.ttft_slo_ms is None:
        parser.error("--gpu-count requires both SLO thresholds")
    source = sys.stdin if args.trace == "-" else open(args.trace, encoding="utf-8", errors="replace")
    try:
        parsed = list(records(source))
    finally:
        if source is not sys.stdin:
            source.close()
    if not parsed:
        raise SystemExit("no supported route-journal records found")
    report = audit(parsed, args.ttft_slo_ms, args.tpot_slo_ms, args.gpu_count)
    if args.json:
        print(json.dumps(report, sort_keys=True))
    else:
        print_human(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
