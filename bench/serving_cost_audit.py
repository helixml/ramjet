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


SCHEMA_VERSION = 2
OUTPUT_LIMIT_POLICY_VERSION = 1
OUTPUT_LIMIT_BUCKETS = {
    "unset",
    "invalid",
    "1_64",
    "65_256",
    "257_1024",
    "1025_4096",
    "4097_plus",
}
OUTPUT_LIMIT_SOURCES = {
    "none",
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
}
OUTPUT_LIMIT_MUTATIONS = {
    "unchanged",
    "max_tokens_stripped",
    "max_completion_tokens_stripped",
    "both_stripped",
}
STREAM_MODES = {"unset", "non_streaming", "streaming", "invalid"}
ENDPOINTS = {"chat", "messages", "responses", "completions"}
OUTPUT_LIMIT_FIELDS = {
    "policy_version",
    "requested_bucket",
    "requested_source",
    "effective_bucket",
    "effective_source",
    "mutation",
    "stream_mode",
}
MAX_SAFE_TOKEN_COUNT = (1 << 53) - 1


def finite_number(value):
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def nonnegative_number(value):
    return finite_number(value) and value >= 0


def positive_number(value):
    return finite_number(value) and value > 0


def nonnegative_integral(value):
    return (
        nonnegative_number(value)
        and float(value).is_integer()
        and value <= MAX_SAFE_TOKEN_COUNT
    )


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


def positive_load_units(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 1


def admitted_load_units(start, finish):
    """Return the reservation actually acquired for the served upstream.

    Journal v8 records the admitted reservation on the finish event, which is
    authoritative under failover: the serving loop acquires the reserving
    candidate's units, which need not be the initially selected candidate's
    estimate. Older records only carry the pre-route candidate estimate.
    """
    admitted = finish.get("request_load_units")
    if positive_load_units(admitted):
        return admitted
    candidate = served_candidate(start, finish)
    estimate = None if candidate is None else candidate.get("request_load_units")
    return estimate if positive_load_units(estimate) else None


def bounded_output_limit(start):
    """Return only fixed-cardinality output-limit telemetry labels."""
    version = start.get("v")
    raw = start.get("output_limit")
    if type(version) is int and version in range(1, 7):
        return {
            "telemetry_state": "legacy",
            "requested_bucket": "legacy",
            "requested_source": "legacy",
            "effective_bucket": "legacy",
            "effective_source": "legacy",
            "mutation": "legacy",
            "stream_mode": "legacy",
        }
    endpoint = start.get("endpoint")
    allowed_sources = {
        "chat": {"none", "max_tokens", "max_completion_tokens"},
        "messages": {"none", "max_tokens"},
        "responses": {"none", "max_output_tokens"},
        "completions": {"none", "max_tokens"},
    }.get(endpoint, set())
    valid = (
        type(version) is int
        and version in (7, 8, 9, 10)
        and isinstance(raw, dict)
        and set(raw) == OUTPUT_LIMIT_FIELDS
        and type(raw.get("policy_version")) is int
        and raw.get("policy_version") == OUTPUT_LIMIT_POLICY_VERSION
        and raw.get("requested_bucket") in OUTPUT_LIMIT_BUCKETS
        and raw.get("requested_source") in OUTPUT_LIMIT_SOURCES
        and raw.get("effective_bucket") in OUTPUT_LIMIT_BUCKETS
        and raw.get("effective_source") in OUTPUT_LIMIT_SOURCES
        and raw.get("mutation") in OUTPUT_LIMIT_MUTATIONS
        and raw.get("stream_mode") in STREAM_MODES
        and raw.get("requested_source") in allowed_sources
        and raw.get("effective_source") in allowed_sources
        and (
            raw.get("requested_source") != "none"
            or raw.get("requested_bucket") in {"unset", "invalid"}
        )
        and (
            raw.get("effective_source") != "none"
            or raw.get("effective_bucket") in {"unset", "invalid"}
        )
        and (
            raw.get("requested_source") == "none"
            or raw.get("requested_bucket") != "unset"
        )
        and (
            raw.get("effective_source") == "none"
            or raw.get("effective_bucket") != "unset"
        )
        and (
            raw.get("mutation") != "unchanged"
            or (
                raw.get("requested_source") == raw.get("effective_source")
                and raw.get("requested_bucket") == raw.get("effective_bucket")
            )
        )
        and (
            endpoint in {"chat", "completions"}
            or raw.get("mutation") == "unchanged"
        )
    )
    if not valid:
        return {
            "telemetry_state": "invalid",
            "requested_bucket": "invalid",
            "requested_source": "invalid",
            "effective_bucket": "invalid",
            "effective_source": "invalid",
            "mutation": "invalid",
            "stream_mode": "invalid",
        }
    return {
        "telemetry_state": "valid",
        "requested_bucket": raw["requested_bucket"],
        "requested_source": raw["requested_source"],
        "effective_bucket": raw["effective_bucket"],
        "effective_source": raw["effective_source"],
        "mutation": raw["mutation"],
        "stream_mode": raw["stream_mode"],
    }


def decode_observation(start, finish):
    """Join bounded requested policy to observed decode and cancellation state."""
    output_limit = bounded_output_limit(start)
    endpoint = start.get("endpoint")
    endpoint = endpoint if endpoint in ENDPOINTS else "invalid"
    result = finish.get("result")
    status = finish.get("status")
    complete = result == "complete" and type(status) is int and 200 <= status < 300
    if result is None:
        outcome = "missing_finish"
    elif complete:
        outcome = "complete"
    elif result == "client_disconnect":
        outcome = "client_disconnect"
    else:
        outcome = "other_failure"
    duration = finish.get("duration_ms")
    duration = float(duration) if positive_number(duration) else None
    ttft = finish.get("ttft_ms")
    ttft = float(ttft) if positive_number(ttft) else None
    if duration is not None and ttft is not None and ttft > duration:
        ttft = None
    completion = finish.get("completion_tokens")
    completion = float(completion) if nonnegative_integral(completion) else None
    tpot = None
    if completion is not None and completion > 1 and duration is not None and ttft is not None:
        candidate_tpot = (duration - ttft) / (completion - 1)
        if positive_number(candidate_tpot):
            tpot = candidate_tpot
    load_units = admitted_load_units(start, finish)
    if load_units is None:
        load_bucket = "missing"
    elif load_units == 1:
        load_bucket = "1"
    elif load_units <= 4:
        load_bucket = "2_4"
    elif load_units <= 8:
        load_bucket = "5_8"
    else:
        load_bucket = "9_plus"
    return {
        **output_limit,
        "endpoint": endpoint,
        "outcome": outcome,
        "completion_tokens": completion,
        "duration_ms": duration,
        "ttft_ms": ttft,
        "tpot_ms": tpot,
        "request_load_units": load_units,
        "request_load_bucket": load_bucket,
    }


def label_counts(items, field):
    return {
        value: sum(item[field] == value for item in items)
        for value in sorted({item[field] for item in items})
    }


def measurement_summary(items):
    completions = [
        item["completion_tokens"]
        for item in items
        if item["completion_tokens"] is not None
    ]
    durations = [
        item["duration_ms"] for item in items if item["duration_ms"] is not None
    ]
    ttfts = [item["ttft_ms"] for item in items if item["ttft_ms"] is not None]
    tpots = [item["tpot_ms"] for item in items if item["tpot_ms"] is not None]
    decode_durations = [
        item["duration_ms"] - item["ttft_ms"]
        for item in items
        if item["duration_ms"] is not None and item["ttft_ms"] is not None
    ]
    return {
        "requests": len(items),
        "missing_completion_tokens": len(items) - len(completions),
        "completion_token_samples": len(completions),
        "completion_tokens_p50": percentile(completions, 0.50),
        "completion_tokens_p95": percentile(completions, 0.95),
        "missing_duration_ms": len(items) - len(durations),
        "duration_samples": len(durations),
        "duration_ms_p50": percentile(durations, 0.50),
        "duration_ms_p95": percentile(durations, 0.95),
        "missing_ttft_ms": len(items) - len(ttfts),
        "ttft_samples": len(ttfts),
        "ttft_ms_p50": percentile(ttfts, 0.50),
        "ttft_ms_p95": percentile(ttfts, 0.95),
        "missing_decode_duration_ms": len(items) - len(decode_durations),
        "decode_duration_samples": len(decode_durations),
        "decode_duration_ms_p50": percentile(decode_durations, 0.50),
        "decode_duration_ms_p95": percentile(decode_durations, 0.95),
        "missing_tpot_ms": len(items) - len(tpots),
        "tpot_samples": len(tpots),
        "tpot_ms_p50": percentile(tpots, 0.50),
        "tpot_ms_p95": percentile(tpots, 0.95),
    }


def summarize_decode(items):
    complete = [item for item in items if item["outcome"] == "complete"]
    return {
        "requests": len(items),
        "complete_requests": sum(item["outcome"] == "complete" for item in items),
        "client_disconnects": sum(item["outcome"] == "client_disconnect" for item in items),
        "other_failures": sum(item["outcome"] == "other_failure" for item in items),
        "missing_finishes": sum(item["outcome"] == "missing_finish" for item in items),
        "missing_completion_tokens": sum(
            item["completion_tokens"] is None for item in items
        ),
        "missing_duration_ms": sum(item["duration_ms"] is None for item in items),
        "missing_ttft_ms": sum(item["ttft_ms"] is None for item in items),
        "missing_tpot_ms": sum(item["tpot_ms"] is None for item in items),
        "complete_measurements": measurement_summary(complete),
        "by_outcome": {
            outcome: measurement_summary(group)
            for outcome in sorted({item["outcome"] for item in items})
            if (group := [item for item in items if item["outcome"] == outcome])
        },
        "request_load_bucket_counts": label_counts(items, "request_load_bucket"),
        "telemetry_state_counts": label_counts(items, "telemetry_state"),
        "requested_source_counts": label_counts(items, "requested_source"),
        "effective_bucket_counts": label_counts(items, "effective_bucket"),
        "effective_source_counts": label_counts(items, "effective_source"),
        "mutation_counts": label_counts(items, "mutation"),
        "stream_mode_counts": label_counts(items, "stream_mode"),
        "endpoint_counts": label_counts(items, "endpoint"),
    }


def grouped_decode(items, field):
    return {
        value: summarize_decode([item for item in items if item[field] == value])
        for value in sorted({item[field] for item in items})
    }


def grouped_decode_with_buckets(items, field):
    return {
        value: {
            "overall": summarize_decode(group),
            "by_requested_bucket": grouped_decode(group, "requested_bucket"),
        }
        for value in sorted({item[field] for item in items})
        if (group := [item for item in items if item[field] == value])
    }


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
    if (
        not nonnegative_integral(prompt)
        or prompt <= 0
        or not nonnegative_integral(cached)
        or not positive_number(ttft)
        or not positive_number(duration)
    ):
        return None
    if cached > prompt or duration < ttft:
        return None
    request_load_units = admitted_load_units(start, finish)
    uncached = float(prompt - cached)
    service_ms_per_uncached_token = float(ttft) / uncached if uncached > 0 else None
    tpot_ms = None
    if nonnegative_integral(completion) and completion > 1:
        candidate_tpot = float(duration - ttft) / float(completion - 1)
        if positive_number(candidate_tpot):
            tpot_ms = candidate_tpot
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
        "completion_tokens": (
            float(completion) if nonnegative_integral(completion) else None
        ),
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
    unmatched_start_records = [
        start for entries in pending_starts.values() for start in entries
    ]
    unmatched_starts = len(unmatched_start_records)
    observations = [
        item
        for start, finish in joined
        if (item := observation(start, finish)) is not None
    ]
    decode_observations = [
        decode_observation(start, finish) for start, finish in joined
    ] + [decode_observation(start, {}) for start in unmatched_start_records]
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
        "output_limit_analysis": {
            "policy_version": OUTPUT_LIMIT_POLICY_VERSION,
            "overall": summarize_decode(decode_observations),
            "requested_bucket_counts": label_counts(
                decode_observations, "requested_bucket"
            ),
            "by_requested_bucket": grouped_decode(
                decode_observations, "requested_bucket"
            ),
            "by_endpoint": grouped_decode_with_buckets(
                decode_observations, "endpoint"
            ),
            "by_stream_mode": grouped_decode_with_buckets(
                decode_observations, "stream_mode"
            ),
            "by_request_load_bucket": grouped_decode_with_buckets(
                decode_observations, "request_load_bucket"
            ),
        },
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
    output_limits = report["output_limit_analysis"]
    print(
        "output_limits "
        f"requests={output_limits['overall']['requests']} "
        f"states={json.dumps(output_limits['overall']['telemetry_state_counts'], sort_keys=True)}"
    )
    print("requested_bucket requests complete disconnected completion_p50 duration_p50")
    for bucket, summary in output_limits["by_requested_bucket"].items():
        complete = summary["complete_measurements"]
        print(
            f"{bucket:>16} {summary['requests']:>8} "
            f"{summary['complete_requests']:>8} {summary['client_disconnects']:>12} "
            f"{str(complete['completion_tokens_p50']):>14} "
            f"{str(complete['duration_ms_p50']):>12}"
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
