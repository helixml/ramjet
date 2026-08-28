#!/usr/bin/env python3
"""Static counterfactual replay for ramjet route-journal JSONL.

Usage:
  docker logs ds4-loadbalancer 2>&1 | python3 route_replay.py -
  python3 route_replay.py trace.log --alphas 1,2,4,8 --caps 8,16,32,64
  python3 route_replay.py trace.log --projected-loads off,on

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


def parse_projected_loads(raw):
    labels = {"off": False, "on": True}
    values = []
    for item in raw.split(","):
        label = item.strip().lower()
        if not label:
            continue
        if label not in labels:
            raise argparse.ArgumentTypeError(
                "projected loads must be a comma-separated subset of off,on"
            )
        value = labels[label]
        if value not in values:
            values.append(value)
    if not values:
        raise argparse.ArgumentTypeError("projected loads must include off or on")
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
        if not isinstance(record, dict):
            continue
        version = record.get("v")
        if (
            type(version) is int
            and version in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10)
            and record.get("event") in ("start", "finish")
        ):
            yield record


def session_affinity_outcome(record):
    value = record.get("session_affinity")
    if value is None:
        return "legacy"
    if isinstance(value, dict) and isinstance(value.get("outcome"), str):
        return value["outcome"]
    return "invalid"


def _session_affinity_decision(record, alpha=None, bonus_blocks=None, max_load_delta=None):
    """Independently reproduce a journal-v5 session-affinity observation."""
    observation = record.get("session_affinity")
    if observation is None:
        return "legacy", None
    if not isinstance(observation, dict) or observation.get("policy_version") != 1:
        return "invalid_decision", None
    observed = observation.get("outcome")
    if observed in ("missing_session", "invalid_session"):
        if any(observation.get(field) is not None for field in ("primary", "secondary", "target")):
            return "invalid_decision", None
        return observed, None

    def nonnegative_int(value):
        return type(value) is int and value >= 0

    candidates = record.get("candidates")
    primary = observation.get("primary")
    secondary = observation.get("secondary")
    chosen = record.get("chosen")
    if (
        not isinstance(candidates, list)
        or len(candidates) < 2
        or not all(isinstance(candidate, dict) for candidate in candidates)
        or not all(nonnegative_int(value) for value in (primary, secondary, chosen))
        or primary == secondary
    ):
        return "invalid_decision", None
    states = {}
    for candidate in candidates:
        index = candidate.get("upstream")
        if (
            not nonnegative_int(index)
            or index in states
            or type(candidate.get("healthy")) is not bool
            or not all(
                nonnegative_int(candidate.get(field))
                for field in ("overlap_blocks", "affinity_blocks", "load_units")
            )
        ):
            return "invalid_decision", None
        states[index] = candidate
    if (
        set(states) != set(range(len(states)))
        or primary not in states
        or secondary not in states
        or chosen not in states
    ):
        return "invalid_decision", None

    alpha = record.get("alpha") if alpha is None else alpha
    bonus_blocks = observation.get("bonus_blocks") if bonus_blocks is None else bonus_blocks
    max_load_delta = (
        observation.get("max_load_delta") if max_load_delta is None else max_load_delta
    )
    if (
        not isinstance(alpha, (int, float))
        or not math.isfinite(float(alpha))
        or alpha < 0
        or not nonnegative_int(bonus_blocks)
        or not nonnegative_int(max_load_delta)
    ):
        return "invalid_decision", None

    healthy_loads = [state["load_units"] for state in states.values() if state["healthy"]]
    if not healthy_loads:
        return "no_healthy_upstream", None
    admitted_load = min(healthy_loads) + max_load_delta
    if states[primary]["healthy"] and states[primary]["load_units"] <= admitted_load:
        target = primary
        target_kind = "primary"
    elif states[secondary]["healthy"] and states[secondary]["load_units"] <= admitted_load:
        target = secondary
        target_kind = "secondary_load" if states[primary]["healthy"] else "secondary_health"
    elif not states[primary]["healthy"] and not states[secondary]["healthy"]:
        return "no_healthy_assigned_pair", None
    else:
        return "kept_assigned_pair_load_gated", None

    already = {
        "primary": "approximate_already_primary",
        "secondary_health": "approximate_already_secondary_primary_unhealthy",
        "secondary_load": "approximate_already_secondary_primary_load_gated",
    }
    preferred = {
        "primary": "would_prefer_primary",
        "secondary_health": "would_prefer_secondary_primary_unhealthy",
        "secondary_load": "would_prefer_secondary_primary_load_gated",
    }
    if chosen == target:
        return already[target_kind], target

    target_state = states[target]
    chosen_state = states[chosen]
    target_score = (
        target_state["affinity_blocks"] - float(alpha) * target_state["load_units"] + bonus_blocks
    )
    chosen_score = chosen_state["affinity_blocks"] - float(alpha) * chosen_state["load_units"]
    if target_state["healthy"] != chosen_state["healthy"]:
        target_wins = target_state["healthy"]
    elif target_score != chosen_score:
        target_wins = target_score > chosen_score
    elif target_state["overlap_blocks"] != chosen_state["overlap_blocks"]:
        target_wins = target_state["overlap_blocks"] > chosen_state["overlap_blocks"]
    else:
        rotation = record.get("rotation")
        if not nonnegative_int(rotation):
            return "invalid_decision", None
        count = len(states)
        target_wins = (target + rotation) % count < (chosen + rotation) % count
    return (preferred[target_kind] if target_wins else "kept_score"), target


def session_affinity_choice(record, alpha=None, bonus_blocks=None, max_load_delta=None):
    return _session_affinity_decision(record, alpha, bonus_blocks, max_load_delta)[0]


def choose(record, alpha, cap, tie_break=None, projected_load=False):
    candidates = record["candidates"]
    rotation = record.get("rotation", 0)
    count = len(candidates)
    if count == 0:
        return None
    tie_break = tie_break or record.get("score_tie_break", "load-neutral")
    scored_loads = {}
    for candidate in candidates:
        load = candidate["load_units"]
        if projected_load:
            request_load = candidate.get("request_load_units")
            if type(request_load) is not int or request_load <= 0:
                raise ValueError(
                    f"route-journal seq {record.get('seq', 'unknown')} candidate "
                    f"{candidate.get('upstream', 'unknown')}: projected-load replay "
                    "requires a positive integer request_load_units"
                )
            load += request_load - 1
        scored_loads[candidate["upstream"]] = load

    def compare(left, right):
        left_healthy = bool(left["healthy"])
        right_healthy = bool(right["healthy"])
        if left_healthy != right_healthy:
            return -1 if left_healthy else 1
        left_load = scored_loads[left["upstream"]]
        right_load = scored_loads[right["upstream"]]
        left_score = min(left["overlap_blocks"], cap) - alpha * left_load
        right_score = min(right["overlap_blocks"], cap) - alpha * right_load
        if left_score != right_score:
            return -1 if left_score > right_score else 1
        if (
            left["overlap_blocks"] != right["overlap_blocks"]
            and (tie_break == "overlap" or left_load == right_load)
        ):
            return -1 if left["overlap_blocks"] > right["overlap_blocks"] else 1
        left_rotation = (left["upstream"] + rotation) % count
        right_rotation = (right["upstream"] + rotation) % count
        if left_rotation == right_rotation:
            return 0
        return -1 if left_rotation < right_rotation else 1

    return sorted(candidates, key=functools.cmp_to_key(compare))[0]["upstream"]


def replay(
    starts,
    finishes,
    alphas,
    caps,
    tie_break=None,
    session_bonus_blocks=None,
    session_max_load_delta=None,
    projected_loads=None,
):
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
    policies = [None] if projected_loads is None else projected_loads
    for alpha in alphas:
        for cap in caps:
            for projected_load in policies:
                choices = []
                agreements = 0
                overlaps = []
                loads = []
                for record in starts:
                    selected = choose(
                        record,
                        alpha,
                        cap,
                        tie_break,
                        projected_load=bool(projected_load),
                    )
                    if selected is None:
                        continue
                    choices.append(selected)
                    agreements += selected == record.get("chosen")
                    candidate = next(item for item in record["candidates"] if item["upstream"] == selected)
                    overlaps.append(candidate["overlap_blocks"])
                    loads.append(candidate["load_units"])
                route_counts = {str(route): choices.count(route) for route in sorted(set(choices))}
                session_replayed = [
                    session_affinity_choice(
                        record,
                        alpha=alpha,
                        bonus_blocks=session_bonus_blocks,
                        max_load_delta=session_max_load_delta,
                    )
                    for record in starts
                ]
                session_record_replayed = [_session_affinity_decision(record) for record in starts]
                paired = [finish for _, finish in paired_records]
                complete = [finish for _, finish in complete_records]
                durations = [record["duration_ms"] for record in complete if record.get("duration_ms") is not None]
                prompt_tokens = sum(record.get("prompt_tokens") or 0 for record in complete)
                cached_tokens = sum(record.get("cached_tokens") or 0 for record in complete)
                row = {
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
                    "session_affinity_counts": {
                        str(outcome): sum(
                            1
                            for record in starts
                            if session_affinity_outcome(record) == outcome
                        )
                        for outcome in sorted(
                            {session_affinity_outcome(record) for record in starts}
                        )
                    },
                    "session_affinity_replay_counts": {
                        str(outcome): session_replayed.count(outcome)
                        for outcome in sorted(set(session_replayed))
                    },
                    "session_affinity_record_mismatches": sum(
                        (
                            session_affinity_outcome(record),
                            (record.get("session_affinity") or {}).get("target"),
                        )
                        != reproduced
                        for record, reproduced in zip(starts, session_record_replayed, strict=True)
                    ),
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
                if projected_load is not None:
                    row["projected_load"] = projected_load
                rows.append(row)
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
        "--session-affinity",
        choices=(
            "missing_session",
            "invalid_session",
            "approximate_already_primary",
            "approximate_already_secondary_primary_unhealthy",
            "approximate_already_secondary_primary_load_gated",
            "would_prefer_primary",
            "would_prefer_secondary_primary_unhealthy",
            "would_prefer_secondary_primary_load_gated",
            "kept_assigned_pair_load_gated",
            "kept_score",
            "no_healthy_assigned_pair",
            "no_healthy_upstream",
            "invalid_decision",
            "legacy",
            "invalid",
        ),
        help="only replay one bounded session-affinity outcome; legacy selects pre-v5 records",
    )
    parser.add_argument(
        "--session-bonus-blocks",
        type=int,
        help="override the recorded session bonus for static counterfactual replay",
    )
    parser.add_argument(
        "--session-max-load-delta",
        type=int,
        help="override the recorded session load delta for static counterfactual replay",
    )
    parser.add_argument(
        "--tie-break",
        choices=("observed", "overlap", "load-neutral"),
        default="observed",
        help="score-equality policy; observed follows each journal version/record",
    )
    parser.add_argument(
        "--projected-loads",
        type=parse_projected_loads,
        help=(
            "explicitly sweep off,on projected-load scoring; when on, score each "
            "candidate with load_units + request_load_units - 1"
        ),
    )
    parser.add_argument("--json", action="store_true", help="emit one JSON object per policy")
    args = parser.parse_args(argv)
    if args.session_bonus_blocks is not None and args.session_bonus_blocks < 0:
        parser.error("--session-bonus-blocks must be non-negative")
    if args.session_max_load_delta is not None and args.session_max_load_delta < 0:
        parser.error("--session-max-load-delta must be non-negative")
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
        and (
            args.session_affinity is None
            or session_affinity_outcome(record) == args.session_affinity
        )
    ]
    sequences = {record["seq"] for record in starts}
    finishes = {sequence: record for sequence, record in finishes.items() if sequence in sequences}
    if not starts:
        raise SystemExit("no v1 route-journal start records found")
    tie_break = None if args.tie_break == "observed" else args.tie_break
    try:
        rows = replay(
            starts,
            finishes,
            alphas,
            caps,
            tie_break,
            args.session_bonus_blocks,
            args.session_max_load_delta,
            args.projected_loads,
        )
    except ValueError as error:
        parser.error(str(error))
    if args.json:
        for row in rows:
            print(json.dumps(row, sort_keys=True))
    else:
        projected_header = " projected" if args.projected_loads is not None else ""
        print(
            f"alpha cap{projected_header} requests agree% moves routes mean_overlap "
            "mean_load paired first_byte ttft_ms cache% warm/cold warm_ttft cold_ttft"
        )
        for row in rows:
            routes = ",".join(f"{key}:{value}" for key, value in row["route_counts"].items())
            projected_value = (
                f" {'on' if row['projected_load'] else 'off':>9}"
                if args.projected_loads is not None
                else ""
            )
            print(
                f"{row['alpha']:>5g} {row['cap']:>3}{projected_value} {row['requests']:>8} "
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
