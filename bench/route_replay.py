#!/usr/bin/env python3
"""Static counterfactual replay for mini-dynamo route-journal JSONL.

Usage:
  docker logs ds4-loadbalancer 2>&1 | python3 route_replay.py -
  python3 route_replay.py trace.log --alphas 1,2,4,8 --caps 8,16,32,64

The journal deliberately excludes prompts and fingerprints. Replay therefore
holds each observed cache-overlap/load snapshot fixed and asks which upstream
another alpha/affinity cap would have chosen. It does not simulate how changed
placements would alter future cache contents or overlapping request lifetimes.
"""

import argparse
import functools
import json
import math
import statistics
import sys


MARKER = "[route_journal] "


def parse_numbers(raw, cast):
    values = [cast(item.strip()) for item in raw.split(",") if item.strip()]
    if not values or any(not math.isfinite(float(value)) or value < 0 for value in values):
        raise argparse.ArgumentTypeError("values must be finite and non-negative")
    return values


def records(lines):
    for line_number, raw in enumerate(lines, 1):
        line = raw.strip()
        if MARKER in line:
            line = line.split(MARKER, 1)[1]
        elif not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise ValueError(f"line {line_number}: invalid journal JSON: {error}") from error
        if record.get("v") in (1, 2, 3, 4) and record.get("event") in ("start", "finish"):
            yield record


def choose(record, alpha, cap, tie_break=None):
    candidates = record["candidates"]
    rotation = record.get("rotation", 0)
    count = len(candidates)
    if count == 0:
        return None
    tie_break = tie_break or record.get("score_tie_break", "load-neutral")

    def compare(left, right):
        left_healthy = bool(left["healthy"])
        right_healthy = bool(right["healthy"])
        if left_healthy != right_healthy:
            return -1 if left_healthy else 1
        left_score = min(left["overlap_blocks"], cap) - alpha * left["load_units"]
        right_score = min(right["overlap_blocks"], cap) - alpha * right["load_units"]
        if left_score != right_score:
            return -1 if left_score > right_score else 1
        if (
            left["overlap_blocks"] != right["overlap_blocks"]
            and (tie_break == "overlap" or left["load_units"] == right["load_units"])
        ):
            return -1 if left["overlap_blocks"] > right["overlap_blocks"] else 1
        left_rotation = (left["upstream"] + rotation) % count
        right_rotation = (right["upstream"] + rotation) % count
        if left_rotation == right_rotation:
            return 0
        return -1 if left_rotation < right_rotation else 1

    return sorted(candidates, key=functools.cmp_to_key(compare))[0]["upstream"]


def replay(starts, finishes, alphas, caps, tie_break=None):
    paired_records = [
        (record, finishes[record["seq"]])
        for record in starts
        if record["seq"] in finishes
    ]
    complete_records = [
        (start, finish)
        for start, finish in paired_records
        if finish.get("result") == "complete"
    ]

    def was_warm(start):
        served = start.get("served_chosen", start.get("chosen"))
        candidate = next(
            (item for item in start["candidates"] if item["upstream"] == served),
            None,
        )
        return candidate is not None and candidate["overlap_blocks"] > 0

    warm_finishes = [finish for start, finish in complete_records if was_warm(start)]
    cold_finishes = [finish for start, finish in complete_records if not was_warm(start)]

    def median_field(items, field):
        values = [item[field] for item in items if item.get(field) is not None]
        return round(statistics.median(values), 1) if values else None

    def true_ttft(items):
        return median_field([item for item in items if item.get("v", 1) >= 3], "ttft_ms")

    def first_byte(items):
        values = []
        for item in items:
            value = item.get("first_byte_ms")
            if value is None and item.get("v", 1) < 3:
                # v1/v2 called the first response byte "ttft_ms".
                value = item.get("ttft_ms")
            if value is not None:
                values.append(value)
        return round(statistics.median(values), 1) if values else None

    def cache_hit(items):
        prompt_tokens = sum(item.get("prompt_tokens") or 0 for item in items)
        cached_tokens = sum(item.get("cached_tokens") or 0 for item in items)
        return round(100 * cached_tokens / prompt_tokens, 1) if prompt_tokens else None

    rows = []
    for alpha in alphas:
        for cap in caps:
            choices = []
            agreements = 0
            overlaps = []
            loads = []
            for record in starts:
                selected = choose(record, alpha, cap, tie_break)
                if selected is None:
                    continue
                choices.append(selected)
                agreements += selected == record.get("chosen")
                candidate = next(item for item in record["candidates"] if item["upstream"] == selected)
                overlaps.append(candidate["overlap_blocks"])
                loads.append(candidate["load_units"])
            route_counts = {str(route): choices.count(route) for route in sorted(set(choices))}
            paired = [finish for _, finish in paired_records]
            complete = [finish for _, finish in complete_records]
            durations = [record["duration_ms"] for record in complete if record.get("duration_ms") is not None]
            prompt_tokens = sum(record.get("prompt_tokens") or 0 for record in complete)
            cached_tokens = sum(record.get("cached_tokens") or 0 for record in complete)
            rows.append(
                {
                    "alpha": alpha,
                    "cap": cap,
                    "tie_break": tie_break or "observed",
                    "requests": len(choices),
                    "agreement_pct": round(100 * agreements / len(choices), 1) if choices else None,
                    "counterfactual_migrations": len(choices) - agreements,
                    "route_counts": route_counts,
                    "exact_canary_counts": {
                        str(cohort): sum(
                            1 for record in starts if record.get("exact_canary", "legacy") == cohort
                        )
                        for cohort in sorted(
                            {record.get("exact_canary", "legacy") for record in starts}
                        )
                    },
                    "mean_overlap_blocks": round(sum(overlaps) / len(overlaps), 2) if overlaps else None,
                    "mean_observed_load_units": round(sum(loads) / len(loads), 2) if loads else None,
                    "paired_finishes": len(paired),
                    "observed_complete": len(complete),
                    "observed_first_byte_ms_median": first_byte(complete),
                    "observed_ttft_ms_median": true_ttft(complete),
                    "observed_ttft_samples": sum(
                        1
                        for record in complete
                        if record.get("v", 1) >= 3 and record.get("ttft_ms") is not None
                    ),
                    "observed_duration_ms_median": round(statistics.median(durations), 1) if durations else None,
                    "observed_cache_hit_pct": round(100 * cached_tokens / prompt_tokens, 1)
                    if prompt_tokens
                    else None,
                    "observed_warm_complete": len(warm_finishes),
                    "observed_cold_complete": len(cold_finishes),
                    "observed_warm_ttft_ms_median": true_ttft(warm_finishes),
                    "observed_cold_ttft_ms_median": true_ttft(cold_finishes),
                    "observed_warm_cache_hit_pct": cache_hit(warm_finishes),
                    "observed_cold_cache_hit_pct": cache_hit(cold_finishes),
                }
            )
    return rows


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", help="journal/log file, or - for stdin")
    parser.add_argument("--alphas", default="1,2,4,8")
    parser.add_argument("--caps", default="8,16,32,64")
    parser.add_argument("--endpoint", help="only replay starts with this endpoint label")
    parser.add_argument("--min-request-bytes", type=int, default=0)
    parser.add_argument("--max-request-bytes", type=int)
    parser.add_argument(
        "--exact-canary",
        choices=("disabled", "treatment", "control", "missing_session", "invalid_session", "legacy"),
        help="only replay one bounded exact-canary cohort; legacy selects records without one",
    )
    parser.add_argument(
        "--tie-break",
        choices=("observed", "overlap", "load-neutral"),
        default="observed",
        help="score-equality policy; observed follows each journal version/record",
    )
    parser.add_argument("--json", action="store_true", help="emit one JSON object per policy")
    args = parser.parse_args(argv)
    alphas = parse_numbers(args.alphas, float)
    caps = parse_numbers(args.caps, int)
    source = sys.stdin if args.trace == "-" else open(args.trace, encoding="utf-8", errors="replace")
    try:
        parsed = list(records(source))
    finally:
        if source is not sys.stdin:
            source.close()
    starts = [record for record in parsed if record["event"] == "start"]
    finishes = {record["seq"]: record for record in parsed if record["event"] == "finish"}
    starts = [
        record
        for record in starts
        if (args.endpoint is None or record.get("endpoint") == args.endpoint)
        and record.get("request_bytes", 0) >= args.min_request_bytes
        and (args.max_request_bytes is None or record.get("request_bytes", 0) <= args.max_request_bytes)
        and (
            args.exact_canary is None
            or record.get("exact_canary", "legacy") == args.exact_canary
        )
    ]
    sequences = {record["seq"] for record in starts}
    finishes = {sequence: record for sequence, record in finishes.items() if sequence in sequences}
    if not starts:
        raise SystemExit("no v1 route-journal start records found")
    tie_break = None if args.tie_break == "observed" else args.tie_break
    rows = replay(starts, finishes, alphas, caps, tie_break)
    if args.json:
        for row in rows:
            print(json.dumps(row, sort_keys=True))
    else:
        print(
            "alpha cap requests agree% moves routes mean_overlap mean_load paired "
            "first_byte ttft_ms cache% warm/cold warm_ttft cold_ttft"
        )
        for row in rows:
            routes = ",".join(f"{key}:{value}" for key, value in row["route_counts"].items())
            print(
                f"{row['alpha']:>5g} {row['cap']:>3} {row['requests']:>8} "
                f"{row['agreement_pct']:>6.1f} {row['counterfactual_migrations']:>5} {routes:>12} "
                f"{row['mean_overlap_blocks']:>12.2f} {row['mean_observed_load_units']:>9.2f} "
                f"{row['paired_finishes']:>6} "
                f"{row['observed_first_byte_ms_median'] if row['observed_first_byte_ms_median'] is not None else '-':>10} "
                f"{row['observed_ttft_ms_median'] if row['observed_ttft_ms_median'] is not None else '-':>7} "
                f"{row['observed_cache_hit_pct'] if row['observed_cache_hit_pct'] is not None else '-':>6} "
                f"{row['observed_warm_complete']}/{row['observed_cold_complete']:<5} "
                f"{row['observed_warm_ttft_ms_median'] if row['observed_warm_ttft_ms_median'] is not None else '-':>9} "
                f"{row['observed_cold_ttft_ms_median'] if row['observed_cold_ttft_ms_median'] is not None else '-':>9}"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
