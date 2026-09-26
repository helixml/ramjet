#!/usr/bin/env python3
"""Counterfactual snapshot-retention ROI from privacy-bounded route journals.

Usage:
  python3 bench/snapshot_roi.py journal.sqlite3 --upstreams 1,2 --compose-match glm53
  docker logs ds4-loadbalancer 2>&1 | python3 bench/snapshot_roi.py - --upstreams 1,2
  python3 bench/snapshot_roi.py segments/*/*.jsonl.gz --upstreams 1,2 --json

Question answered: if conversation state survived longer than the engine kept
it, how many prompt tokens would not have been re-prefilled, and how much state
would a retention tier have had to hold?

The journal holds no prompts, fingerprints, or session identity. Two recorded
facts make the counterfactual possible anyway:

* every start record carries, per candidate upstream, the age of each served
  leading prefix block (`overlap_ages_ms`, journal v11+). A block's age is the
  time since that upstream last completed a request containing it;
* every finish record carries the engine-reported `prompt_tokens` and
  `cached_tokens` and the completion instant.

The deepest overlapping block of a follow-up turn was therefore last served by
the finish whose instant is `start - age`. When that predecessor's whole prompt
is a prefix of this one (`overlap_blocks >= predecessor.total_blocks - 1`) the
request is a continuation, and the predecessor's engine-reported prompt tokens
are the prefix a longer-lived snapshot could have supplied. Requests without a
continuation are credited only the blocks the LB saw served, converted to
tokens at the request's own byte/token ratio. Both credits are bounded by the
actual prompt, and shortfalls smaller than the noise floor are not counted.

This is inference over one LB process's index: the index is lost when the LB
container is recreated, so gaps longer than the container's lifetime are
censored, not absent. Cross-container links are never made.
"""

import argparse
import bisect
import collections
import datetime as dt
import gzip
import json
import math
import sqlite3
import statistics
import sys
from dataclasses import dataclass, field

MARKER = "[route_journal] "
MIN_AGE_VERSION = 11
LINK_TOLERANCE_MS = 2
DEFAULT_HORIZONS = "300,900,1800,3600,10800,21600,43200,86400,259200,604800,inf"
GAP_BUCKETS = (
    ("<5m", 300),
    ("5-30m", 1800),
    ("30m-1h", 3600),
    ("1-6h", 21600),
    ("6-24h", 86400),
    ("1-3d", 259200),
    ("3-7d", 604800),
    (">7d", None),
)
PREFIX_BUCKETS = (
    ("<8k", 8192),
    ("8-32k", 32768),
    ("32-64k", 65536),
    ("64-128k", 131072),
    ("128-256k", 262144),
    (">=256k", None),
)


@dataclass
class Request:
    container: str
    seq: int
    start_ms: int
    finish_ms: int
    upstream: int
    prompt_tokens: float
    cached_tokens: float
    completion_tokens: float
    ttft_ms: float | None
    total_blocks: int
    request_bytes: int
    chunk_bytes: int
    candidates: dict  # upstream -> (overlap_blocks, [[blocks, age_ms], ...])
    link: "Link | None" = None
    chain_start_ms: int = 0
    consumed_ms: int | None = None
    miss: dict = field(default_factory=dict)


@dataclass
class Link:
    predecessor: int
    upstream: int
    gap_ms: int


def parse_horizons(raw):
    values = []
    for item in raw.split(","):
        label = item.strip().lower()
        if not label:
            continue
        if label == "inf":
            value = None
        else:
            seconds = float(label)
            if not math.isfinite(seconds) or seconds < 0:
                raise argparse.ArgumentTypeError("horizons must be inf or non-negative seconds")
            value = int(round(seconds * 1000))
        if value not in values:
            values.append(value)
    if not values:
        raise argparse.ArgumentTypeError("at least one horizon is required")
    return sorted(values, key=lambda value: math.inf if value is None else value)


def parse_upstreams(raw):
    values = {int(item) for item in raw.split(",") if item.strip()}
    if not values or min(values) < 0:
        raise argparse.ArgumentTypeError("upstreams must be non-negative ordinals")
    return values


def parse_instant(raw):
    """ISO-8601 (UTC when naive) or integer Unix milliseconds."""
    if raw.isdigit():
        return int(raw)
    moment = dt.datetime.fromisoformat(raw.replace("Z", "+00:00"))
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=dt.timezone.utc)
    return int(moment.timestamp() * 1000)


def parse_window(raw):
    """`START..END` in the forms accepted by parse_instant; END is exclusive."""
    start, separator, end = raw.partition("..")
    if not separator:
        raise argparse.ArgumentTypeError("windows are START..END")
    window = (parse_instant(start), parse_instant(end))
    if window[0] >= window[1]:
        raise argparse.ArgumentTypeError("window START must precede END")
    return window


def horizon_label(horizon_ms):
    if horizon_ms is None:
        return "inf"
    seconds = horizon_ms / 1000
    for unit, size in (("d", 86400), ("h", 3600), ("m", 60)):
        if seconds >= size and seconds % size == 0:
            return f"{seconds / size:g}{unit}"
    return f"{seconds:g}s"


def bucket(value, buckets):
    for label, limit in buckets:
        if limit is None or value < limit:
            return label
    return buckets[-1][0]


def _journal_lines(lines):
    for raw in lines:
        line = raw.strip()
        if MARKER in line:
            line = line.split(MARKER, 1)[1]
        elif not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict) and record.get("event") in ("start", "finish"):
            yield record


def read_sources(paths, compose_match=None):
    """Yield (container, record) in per-container source order."""
    for path in paths:
        if path.endswith((".sqlite3", ".sqlite", ".db")):
            connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
            try:
                query = (
                    "SELECT r.container_id, r.record_json FROM records r "
                    "JOIN containers c USING (container_id)"
                )
                params = ()
                if compose_match:
                    query += " WHERE c.compose_files LIKE ?"
                    params = (f"%{compose_match}%",)
                query += " ORDER BY r.container_id, r.unix_ms, r.event DESC"
                for container, record_json in connection.execute(query, params):
                    yield container, json.loads(record_json)
            finally:
                connection.close()
            continue
        if path == "-":
            source = sys.stdin
        elif path.endswith(".gz"):
            source = gzip.open(path, "rt", encoding="utf-8", errors="replace")
        else:
            source = open(path, encoding="utf-8", errors="replace")
        try:
            for record in _journal_lines(source):
                yield path, record
        finally:
            if source is not sys.stdin:
                source.close()


def pair_requests(stream, upstreams, since_ms=None, until_ms=None, exclude=()):
    """Pair starts and finishes; keep completed usage-bearing requests served by `upstreams`.

    `seq` restarts with the LB process, so a finish pairs with the most recent
    unmatched start of the same sequence in the same container. Excluded
    windows (synthetic experiments sharing the LB) are dropped after pairing,
    so they neither count nor serve as link predecessors.
    """
    pending = collections.defaultdict(dict)
    first_seen = {}
    requests = []
    skipped = collections.Counter()
    for container, record in stream:
        unix_ms = record.get("unix_ms")
        if type(unix_ms) is int:
            first_seen[container] = min(first_seen.get(container, unix_ms), unix_ms)
        if record["event"] == "start":
            pending[container][record.get("seq")] = record
            continue
        start = pending[container].pop(record.get("seq"), None)
        if start is None:
            skipped["unpaired_finish"] += 1
            continue
        if record.get("upstream") not in upstreams:
            continue
        if since_ms is not None and start["unix_ms"] < since_ms:
            continue
        if until_ms is not None and start["unix_ms"] >= until_ms:
            continue
        if any(low <= start["unix_ms"] < high for low, high in exclude):
            skipped["excluded_window"] += 1
            continue
        if record.get("result") != "complete":
            skipped[f"result_{record.get('result')}"] += 1
            continue
        if record.get("status") != 200:
            skipped[f"status_{record.get('status')}"] += 1
            continue
        if record.get("prompt_tokens") is None or record.get("cached_tokens") is None:
            skipped["missing_usage"] += 1
            continue
        if type(start.get("v")) is not int or start["v"] < MIN_AGE_VERSION:
            skipped["pre_v11_journal"] += 1
            continue
        candidates = {}
        for candidate in start.get("candidates", []):
            if candidate.get("upstream") in upstreams:
                overlap = candidate["overlap_blocks"]
                if (start.get("affinity_horizon") or {}).get("mode") == "enforce":
                    overlap += candidate.get("stale_blocks", 0)
                candidates[candidate["upstream"]] = (overlap, candidate.get("overlap_ages_ms") or [])
        if record["upstream"] not in candidates:
            skipped["served_candidate_missing"] += 1
            continue
        requests.append(
            Request(
                container=container,
                seq=record.get("seq"),
                start_ms=start["unix_ms"],
                finish_ms=record["unix_ms"],
                upstream=record["upstream"],
                prompt_tokens=float(record["prompt_tokens"]),
                cached_tokens=min(float(record["cached_tokens"]), float(record["prompt_tokens"])),
                completion_tokens=float(record.get("completion_tokens") or 0),
                ttft_ms=record.get("ttft_ms"),
                total_blocks=start["total_blocks"],
                request_bytes=start.get("request_bytes", 0),
                chunk_bytes=start.get("chunk_bytes", 2048),
                candidates=candidates,
            )
        )
    requests.sort(key=lambda request: (request.container, request.finish_ms))
    return requests, first_seen, skipped


def link_turns(requests):
    """Attach each continuation to the finish that last served its deepest block."""
    finishes = collections.defaultdict(list)
    for index, request in enumerate(requests):
        finishes[(request.container, request.upstream)].append((request.finish_ms, index))
    for entries in finishes.values():
        entries.sort()
    for index, request in enumerate(requests):
        best = None
        for upstream, (overlap, ages) in request.candidates.items():
            if not ages or overlap <= 0:
                continue
            gap_ms = ages[-1][1]
            target = request.start_ms - gap_ms
            entries = finishes.get((request.container, upstream), [])
            position = bisect.bisect_left(entries, (target - LINK_TOLERANCE_MS, -1))
            while position < len(entries) and entries[position][0] <= target + LINK_TOLERANCE_MS:
                other = entries[position][1]
                predecessor = requests[other]
                if (
                    other != index
                    and predecessor.finish_ms <= request.start_ms
                    and overlap >= predecessor.total_blocks - 1
                    and (best is None or overlap > best[0])
                ):
                    best = (overlap, Link(other, upstream, gap_ms))
                position += 1
        if best is not None:
            request.link = best[1]
    for request in sorted(requests, key=lambda item: item.start_ms):
        if request.link is None:
            request.chain_start_ms = request.start_ms
        else:
            predecessor = requests[request.link.predecessor]
            request.chain_start_ms = predecessor.chain_start_ms or predecessor.start_ms
            if predecessor.consumed_ms is None or request.start_ms < predecessor.consumed_ms:
                predecessor.consumed_ms = request.start_ms


def tokens_for_blocks(request, blocks):
    """Convert leading canonical blocks to prompt tokens at this request's ratio."""
    canonical = request.total_blocks * request.chunk_bytes
    if request.request_bytes > 0:
        canonical = min(canonical, request.request_bytes) if canonical < (2 << 20) else request.request_bytes
    if canonical <= 0:
        return 0.0
    return min(request.prompt_tokens, blocks * request.chunk_bytes / canonical * request.prompt_tokens)


def fresh_blocks(overlap, ages, horizon_ms):
    """Leading served blocks no older than the horizon.

    Blocks past the journal's 64-run cap inherit the last recorded age.
    """
    if horizon_ms is None:
        return overlap
    fresh = 0
    for blocks, age_ms in ages:
        if age_ms > horizon_ms:
            return min(fresh, overlap)
        fresh += blocks
    if ages and ages[-1][1] <= horizon_ms:
        return overlap
    return min(fresh, overlap)


def hypothetical_cached(requests, request, horizon_ms, any_replica):
    """Prompt tokens a retention tier with this horizon could have supplied."""
    credit = request.cached_tokens
    link = request.link
    if link is not None and (any_replica or link.upstream == request.upstream):
        if horizon_ms is None or link.gap_ms <= horizon_ms:
            predecessor = requests[link.predecessor]
            credit = max(credit, min(request.prompt_tokens, predecessor.prompt_tokens))
    for upstream, (overlap, ages) in request.candidates.items():
        if not any_replica and upstream != request.upstream:
            continue
        blocks = fresh_blocks(overlap, ages, horizon_ms)
        credit = max(credit, tokens_for_blocks(request, blocks))
    return credit


def avoidable(requests, request, horizon_ms, any_replica, min_tokens, min_fraction):
    gain = hypothetical_cached(requests, request, horizon_ms, any_replica) - request.cached_tokens
    if gain < max(min_tokens, min_fraction * request.prompt_tokens):
        return 0.0
    return gain


def required_horizon_ms(requests, request, any_replica, min_tokens, min_fraction):
    """Shortest retention that would have recovered this miss.

    Only recorded ages and the link gap can change the credit, so it suffices
    to try those instants in increasing order.
    """
    instants = set()
    if request.link is not None:
        instants.add(request.link.gap_ms)
    for upstream, (_, ages) in request.candidates.items():
        if any_replica or upstream == request.upstream:
            instants.update(age for _, age in ages)
    for horizon_ms in sorted(instants):
        if avoidable(requests, request, horizon_ms, any_replica, min_tokens, min_fraction) > 0:
            return horizon_ms
    return None


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, math.ceil(fraction * len(ordered)) - 1))]


def live_set(requests, horizon_ms, state_bytes, kv_bytes_per_token, useful_only):
    """Peak and time-weighted mean of a TTL tier holding each conversation tail.

    Each completed request leaves one tail (prompt + completion tokens). The
    tail leaves the tier when a continuation consumes it or when it outlives
    the horizon. `useful_only` keeps only tails a continuation consumed within
    the horizon, i.e. the snapshots that would actually have been read.
    """
    events = []
    for request in requests:
        if useful_only and (
            request.consumed_ms is None
            or (horizon_ms is not None and request.consumed_ms - request.finish_ms > horizon_ms)
        ):
            continue
        end = request.finish_ms + horizon_ms if horizon_ms is not None else math.inf
        if request.consumed_ms is not None:
            end = min(end, max(request.finish_ms, request.consumed_ms))
        if end <= request.finish_ms:
            continue
        tokens = request.prompt_tokens + request.completion_tokens
        events.append((request.finish_ms, 1, tokens))
        events.append((end, -1, -tokens))
    if not events:
        return {"peak_snapshots": 0, "peak_bytes": 0, "mean_snapshots": 0, "mean_bytes": 0}
    events.sort(key=lambda event: (event[0], event[1]))
    first = events[0][0]
    last = max(request.finish_ms for request in requests)
    count = tokens = 0
    peak_count = peak_bytes = 0
    area_count = area_bytes = 0.0
    previous = first
    for time, delta, token_delta in events:
        clipped = min(time, last)
        span = clipped - previous
        if span > 0:
            area_count += count * span
            area_bytes += (count * state_bytes + tokens * kv_bytes_per_token) * span
            previous = clipped
        count += delta
        tokens += token_delta
        size = count * state_bytes + tokens * kv_bytes_per_token
        peak_count = max(peak_count, count)
        peak_bytes = max(peak_bytes, size)
    duration = max(1, last - first)
    return {
        "peak_snapshots": peak_count,
        "peak_bytes": int(peak_bytes),
        "mean_snapshots": round(area_count / duration, 1),
        "mean_bytes": int(area_bytes / duration),
    }


def analyze(requests, first_seen, args):
    link_turns(requests)
    span_ms = max(1, max(r.finish_ms for r in requests) - min(r.start_ms for r in requests))
    days = span_ms / 86_400_000
    prompt = sum(r.prompt_tokens for r in requests)
    cached = sum(r.cached_tokens for r in requests)
    gpus = args.gpus_per_replica

    def restore_seconds(tokens):
        return args.restore_fixed_seconds + tokens / args.restore_tokens_per_second

    horizons = []
    reference = {}
    for scope in ("same_replica", "any_replica"):
        any_replica = scope == "any_replica"
        for horizon_ms in list(args.horizons) + ([None] if None not in args.horizons else []):
            gains = [
                avoidable(requests, r, horizon_ms, any_replica, args.miss_min_tokens, args.miss_min_fraction)
                for r in requests
            ]
            helped = [(r, g) for r, g in zip(requests, gains, strict=True) if g > 0]
            total = sum(gains)
            if horizon_ms is None:
                reference[scope] = total
            ttft_saved = [
                max(0.0, g / args.prefill_tokens_per_second - restore_seconds(g)) for _, g in helped
            ]
            censored = sum(
                1
                for r in requests
                if horizon_ms is not None and r.start_ms - first_seen.get(r.container, r.start_ms) < horizon_ms
            )
            horizons.append(
                {
                    "scope": scope,
                    "horizon": horizon_label(horizon_ms),
                    "horizon_ms": horizon_ms,
                    "requests_helped": len(helped),
                    "avoidable_tokens": round(total),
                    "avoidable_pct_of_prompt": round(100 * total / prompt, 3) if prompt else None,
                    "avoidable_pct_of_uncached": round(100 * total / (prompt - cached), 2)
                    if prompt > cached
                    else None,
                    "avoidable_tokens_per_day": round(total / days),
                    "prefill_gpu_seconds_per_day": round(total / args.prefill_tokens_per_second * gpus / days, 1),
                    "ttft_saved_seconds_total": round(sum(ttft_saved), 1),
                    "ttft_saved_seconds_p50": round(percentile(ttft_saved, 0.5), 2) if ttft_saved else None,
                    "ttft_saved_seconds_p90": round(percentile(ttft_saved, 0.9), 2) if ttft_saved else None,
                    "requests_with_censored_window": censored,
                }
            )
    for row in horizons:
        full = reference.get(row["scope"])
        row["capture_pct_of_infinite"] = round(100 * row["avoidable_tokens"] / full, 1) if full else None
    horizons = [row for row in horizons if row["horizon_ms"] in args.horizons]

    misses = collections.defaultdict(lambda: {"requests": 0, "avoidable_tokens": 0.0, "cross_replica": 0})
    miss_prefix = collections.defaultdict(lambda: {"requests": 0, "avoidable_tokens": 0.0})
    miss_ttft = []
    for r in requests:
        gain = avoidable(requests, r, None, True, args.miss_min_tokens, args.miss_min_fraction)
        r.miss = {"gain": gain}
        if gain <= 0:
            continue
        age_ms = required_horizon_ms(requests, r, True, args.miss_min_tokens, args.miss_min_fraction)
        label = bucket(age_ms / 1000, GAP_BUCKETS) if age_ms is not None else "unknown"
        cell = misses[label]
        cell["requests"] += 1
        cell["avoidable_tokens"] += gain
        cell["cross_replica"] += r.link is not None and r.link.upstream != r.upstream
        prefix = r.cached_tokens + gain
        prefix_cell = miss_prefix[bucket(prefix, PREFIX_BUCKETS)]
        prefix_cell["requests"] += 1
        prefix_cell["avoidable_tokens"] += gain
        if r.ttft_ms is not None:
            miss_ttft.append(r.ttft_ms / 1000)

    # Where the uncached tokens went. A continuation's growth beyond its
    # predecessor's prompt was never seen before; an unlinked request is a new
    # conversation, a fork, or a return whose history the LB index lost.
    decomposition = collections.Counter()
    for r in requests:
        uncached = r.prompt_tokens - r.cached_tokens
        gain = r.miss["gain"]
        decomposition["recoverable_by_retention"] += gain
        rest = uncached - gain
        if r.link is not None:
            predecessor = requests[r.link.predecessor]
            new = max(0.0, r.prompt_tokens - max(r.cached_tokens + gain, predecessor.prompt_tokens))
            decomposition["continuation_new_tokens"] += min(rest, new)
            decomposition["continuation_below_noise_floor"] += max(0.0, rest - new)
        elif r.start_ms - first_seen.get(r.container, r.start_ms) < args.censor_window_ms:
            decomposition["unlinked_near_lb_start"] += rest
        else:
            decomposition["unlinked_new_or_fork"] += rest

    continuation_gaps = collections.Counter()
    conversation_ages = collections.Counter()
    for r in requests:
        if r.link is not None:
            continuation_gaps[bucket(r.link.gap_ms / 1000, GAP_BUCKETS)] += 1
            conversation_ages[bucket((r.start_ms - r.chain_start_ms) / 1000, GAP_BUCKETS)] += 1

    cold_rates = [
        (r.prompt_tokens - r.cached_tokens) / (r.ttft_ms / 1000)
        for r in requests
        if r.ttft_ms and r.prompt_tokens - r.cached_tokens >= args.rate_min_uncached_tokens
    ]
    warm_ttft = [
        r.ttft_ms / 1000
        for r in requests
        if r.ttft_ms is not None and r.miss.get("gain", 0) == 0 and r.cached_tokens >= 0.9 * r.prompt_tokens
    ]

    capacity = []
    for horizon_ms in args.horizons:
        for useful in (False, True):
            row = live_set(requests, horizon_ms, args.state_bytes, args.kv_bytes_per_token, useful)
            row.update({"horizon": horizon_label(horizon_ms), "useful_only": useful})
            capacity.append(row)

    chains = collections.Counter(r.chain_start_ms for r in requests)
    return {
        "coverage": {
            "requests": len(requests),
            "containers": len({r.container for r in requests}),
            "first_start": dt.datetime.fromtimestamp(
                min(r.start_ms for r in requests) / 1000, dt.timezone.utc
            ).isoformat(timespec="seconds"),
            "last_finish": dt.datetime.fromtimestamp(
                max(r.finish_ms for r in requests) / 1000, dt.timezone.utc
            ).isoformat(timespec="seconds"),
            "days": round(days, 2),
            "max_lb_lifetime_hours": round(
                max(
                    max(r.finish_ms for r in requests if r.container == c) - first_seen[c]
                    for c in {r.container for r in requests}
                )
                / 3_600_000,
                1,
            ),
            "linked_continuations": sum(r.link is not None for r in requests),
            "cross_replica_continuations": sum(
                r.link is not None and r.link.upstream != r.upstream for r in requests
            ),
            "inferred_conversations": len(chains),
            "turns_per_conversation_p50": percentile(list(chains.values()), 0.5),
            "turns_per_conversation_p90": percentile(list(chains.values()), 0.9),
        },
        "baseline": {
            "prompt_tokens": round(prompt),
            "cached_tokens": round(cached),
            "uncached_tokens": round(prompt - cached),
            "cached_pct": round(100 * cached / prompt, 2) if prompt else None,
            "uncached_tokens_per_day": round((prompt - cached) / days),
            "uncached_prefill_gpu_seconds_per_day": round(
                (prompt - cached) / args.prefill_tokens_per_second * gpus / days, 1
            ),
            "observed_cold_prefill_tokens_per_second_p50": round(percentile(cold_rates, 0.5))
            if cold_rates
            else None,
            "observed_cold_prefill_samples": len(cold_rates),
            "warm_ttft_seconds_p50": round(percentile(warm_ttft, 0.5), 2) if warm_ttft else None,
            "miss_ttft_seconds_p50": round(percentile(miss_ttft, 0.5), 2) if miss_ttft else None,
            "miss_ttft_seconds_p90": round(percentile(miss_ttft, 0.9), 2) if miss_ttft else None,
        },
        "uncached_decomposition": {key: round(value) for key, value in sorted(decomposition.items())},
        "horizons": horizons,
        "miss_age_buckets": {
            label: {**cell, "avoidable_tokens": round(cell["avoidable_tokens"])}
            for label, cell in sorted(misses.items(), key=lambda item: _bucket_order(item[0], GAP_BUCKETS))
        },
        "miss_prefix_buckets": {
            label: {**cell, "avoidable_tokens": round(cell["avoidable_tokens"])}
            for label, cell in sorted(miss_prefix.items(), key=lambda item: _bucket_order(item[0], PREFIX_BUCKETS))
        },
        "continuation_gap_buckets": dict(
            sorted(continuation_gaps.items(), key=lambda item: _bucket_order(item[0], GAP_BUCKETS))
        ),
        "conversation_age_buckets": dict(
            sorted(conversation_ages.items(), key=lambda item: _bucket_order(item[0], GAP_BUCKETS))
        ),
        "capacity": capacity,
        "model": {
            "prefill_tokens_per_second": args.prefill_tokens_per_second,
            "restore_tokens_per_second": args.restore_tokens_per_second,
            "restore_fixed_seconds": args.restore_fixed_seconds,
            "state_bytes": args.state_bytes,
            "kv_bytes_per_token": args.kv_bytes_per_token,
            "gpus_per_replica": gpus,
            "miss_min_tokens": args.miss_min_tokens,
            "miss_min_fraction": args.miss_min_fraction,
        },
    }


def _bucket_order(label, buckets):
    labels = [name for name, _ in buckets]
    return labels.index(label) if label in labels else len(labels)


def _gb(value):
    return f"{value / 1e9:,.1f}GB"


def render(result, out):
    coverage, baseline = result["coverage"], result["baseline"]
    print(
        f"{coverage['requests']} requests over {coverage['days']} days "
        f"({coverage['first_start']} .. {coverage['last_finish']}), "
        f"{coverage['containers']} LB containers, longest LB lifetime "
        f"{coverage['max_lb_lifetime_hours']}h",
        file=out,
    )
    print(
        f"linked continuations {coverage['linked_continuations']} "
        f"(cross-replica {coverage['cross_replica_continuations']}), "
        f"inferred conversations {coverage['inferred_conversations']}",
        file=out,
    )
    print(
        f"prompt {baseline['prompt_tokens']:,} cached {baseline['cached_tokens']:,} "
        f"({baseline['cached_pct']}%) uncached {baseline['uncached_tokens']:,} "
        f"= {baseline['uncached_tokens_per_day']:,}/day, "
        f"{baseline['uncached_prefill_gpu_seconds_per_day']} GPU-s/day",
        file=out,
    )
    print(
        f"observed cold prefill p50 {baseline['observed_cold_prefill_tokens_per_second_p50']} tok/s "
        f"(n={baseline['observed_cold_prefill_samples']}); TTFT p50 warm "
        f"{baseline['warm_ttft_seconds_p50']}s, miss {baseline['miss_ttft_seconds_p50']}s "
        f"(p90 {baseline['miss_ttft_seconds_p90']}s)",
        file=out,
    )
    print(
        "uncached decomposition: "
        + ", ".join(f"{key} {value:,}" for key, value in result["uncached_decomposition"].items()),
        file=out,
    )
    print(file=out)
    print(
        f"{'scope':<13} {'horizon':>7} {'helped':>7} {'avoidable':>13} {'%prompt':>8} "
        f"{'%uncached':>9} {'capture%':>8} {'tok/day':>11} {'GPU-s/day':>9} {'TTFT-s':>8} {'censored':>8}",
        file=out,
    )
    for row in result["horizons"]:
        print(
            f"{row['scope']:<13} {row['horizon']:>7} {row['requests_helped']:>7} "
            f"{row['avoidable_tokens']:>13,} {row['avoidable_pct_of_prompt']:>8} "
            f"{row['avoidable_pct_of_uncached']:>9} {row['capture_pct_of_infinite']:>8} "
            f"{row['avoidable_tokens_per_day']:>11,} {row['prefill_gpu_seconds_per_day']:>9} "
            f"{row['ttft_saved_seconds_total']:>8} {row['requests_with_censored_window']:>8}",
            file=out,
        )
    print(file=out)
    print("misses by idle gap (any replica, infinite horizon):", file=out)
    for label, cell in result["miss_age_buckets"].items():
        print(
            f"  {label:>7} requests {cell['requests']:>5} tokens {cell['avoidable_tokens']:>12,} "
            f"cross-replica {cell['cross_replica']}",
            file=out,
        )
    print("misses by recoverable prefix length:", file=out)
    for label, cell in result["miss_prefix_buckets"].items():
        print(f"  {label:>8} requests {cell['requests']:>5} tokens {cell['avoidable_tokens']:>12,}", file=out)
    print(f"gap between linked turns: {result['continuation_gap_buckets']}", file=out)
    print(f"conversation age at turn: {result['conversation_age_buckets']}", file=out)
    print(file=out)
    print("TTL tier size (tail per conversation; state + KV):", file=out)
    for row in result["capacity"]:
        print(
            f"  {row['horizon']:>5} {'useful' if row['useful_only'] else 'all':>6} "
            f"peak {row['peak_snapshots']:>6} snapshots {_gb(row['peak_bytes']):>10}  "
            f"mean {row['mean_snapshots']:>8} snapshots {_gb(row['mean_bytes']):>10}",
            file=out,
        )


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("sources", nargs="+", help="archive .sqlite3, journal log/JSONL(.gz), or -")
    parser.add_argument("--upstreams", type=parse_upstreams, required=True, help="served upstream ordinals to study")
    parser.add_argument("--compose-match", help="archive only: containers whose compose files contain this")
    parser.add_argument("--since", type=parse_instant)
    parser.add_argument("--until", type=parse_instant)
    parser.add_argument(
        "--exclude",
        type=parse_window,
        action="append",
        default=[],
        help="drop requests starting in START..END (repeatable), e.g. synthetic replays",
    )
    parser.add_argument("--horizons", type=parse_horizons, default=parse_horizons(DEFAULT_HORIZONS))
    parser.add_argument("--miss-min-tokens", type=float, default=2048)
    parser.add_argument("--miss-min-fraction", type=float, default=0.02)
    # GLM-5.3-Flash SM120 TP2 defaults; see docs/glm_snapshot_roi.md for provenance.
    parser.add_argument("--prefill-tokens-per-second", type=float, default=5900)
    parser.add_argument("--restore-tokens-per-second", type=float, default=46500)
    parser.add_argument("--restore-fixed-seconds", type=float, default=0.0)
    parser.add_argument("--state-bytes", type=float, default=81.3e6)
    parser.add_argument("--kv-bytes-per-token", type=float, default=15850)
    parser.add_argument("--gpus-per-replica", type=int, default=2)
    parser.add_argument("--rate-min-uncached-tokens", type=float, default=16384)
    parser.add_argument(
        "--censor-window-seconds",
        type=float,
        default=3600,
        help="unlinked requests this soon after the LB container started may be lost returns",
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    args.censor_window_ms = int(args.censor_window_seconds * 1000)
    requests, first_seen, skipped = pair_requests(
        read_sources(args.sources, args.compose_match), args.upstreams, args.since, args.until, args.exclude
    )
    if not requests:
        raise SystemExit("no completed v11+ requests with usage for the selected upstreams")
    result = analyze(requests, first_seen, args)
    result["coverage"]["skipped"] = dict(sorted(skipped.items()))
    if args.json:
        print(json.dumps(result, sort_keys=True))
    else:
        render(result, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
