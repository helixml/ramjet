#!/usr/bin/env python3
"""Privacy-bounded prefix-cache working-set and counter reconciliation bench.

The workload is synthetic. Output contains only ordinal workload coordinates,
token counts, timings, route ordinals, and aggregate Prometheus deltas.
"""

import argparse
import collections
import concurrent.futures
import hashlib
import json
import os
import re
import statistics
import sys
import time
import urllib.error
import urllib.request

from agentbench import Assembly, SSEDecoder, percentile, token_counts
from engine_metrics import COUNTERS as ENGINE_COUNTERS
from engine_metrics import delta as engine_delta
from engine_metrics import fetch as fetch_engine_metrics
from engine_metrics import metric_value


LB_COUNTERS = {
    "prompt_tokens": ("ds4proxy_prompt_tokens_total", None),
    "cached_prompt_tokens": ("ds4proxy_cached_prompt_tokens_total", None),
    "cache_requests": ("ds4proxy_cache_requests_total", None),
    "cache_ttft_samples": ("ds4proxy_cache_ttft_seconds_count", None),
    "live_stored_blocks": (
        "ds4proxy_kv_event_blocks_total",
        {"source": "live", "action": "stored"},
    ),
    "live_removed_blocks": (
        "ds4proxy_kv_event_blocks_total",
        {"source": "live", "action": "removed"},
    ),
    "live_clear_events": ("ds4proxy_kv_event_clears_total", {"source": "live"}),
    "shadow_exact_agree": (
        "ds4proxy_exact_route_placement_total",
        {"mode": "shadow", "endpoint": "chat", "outcome": "kept_agree"},
    ),
    "shadow_cold_all_zero": (
        "ds4proxy_exact_route_placement_total",
        {"mode": "shadow", "endpoint": "chat", "outcome": "kept_all_zero"},
    ),
    "shadow_cold_would_balance": (
        "ds4proxy_exact_route_placement_total",
        {"mode": "shadow", "endpoint": "chat", "outcome": "would_balance"},
    ),
    "shadow_cold_delta_gate": (
        "ds4proxy_exact_route_placement_total",
        {
            "mode": "shadow",
            "endpoint": "chat",
            "outcome": "kept_balance_delta_gate",
        },
    ),
    "shadow_cold_load_gate": (
        "ds4proxy_exact_route_placement_total",
        {
            "mode": "shadow",
            "endpoint": "chat",
            "outcome": "kept_balance_load_gate",
        },
    ),
    "projected_cold_kept_selected": (
        "ds4proxy_exact_route_projected_balance_total",
        {"endpoint": "chat", "outcome": "kept_selected"},
    ),
    "projected_cold_would_balance": (
        "ds4proxy_exact_route_projected_balance_total",
        {"endpoint": "chat", "outcome": "would_balance"},
    ),
    "projected_cold_delta_gate": (
        "ds4proxy_exact_route_projected_balance_total",
        {"endpoint": "chat", "outcome": "kept_delta_gate"},
    ),
    "projected_cold_load_gate": (
        "ds4proxy_exact_route_projected_balance_total",
        {"endpoint": "chat", "outcome": "kept_load_gate"},
    ),
    "projected_cold_fallback": (
        "ds4proxy_exact_route_projected_balance_total",
        {"endpoint": "chat", "outcome": "fallback"},
    ),
}


def parse_apps(value):
    try:
        apps = [int(item) for item in value.split(",")]
    except ValueError as error:
        raise argparse.ArgumentTypeError("apps must be comma-separated integers") from error
    if not apps or any(item < 1 for item in apps) or len(set(apps)) != len(apps):
        raise argparse.ArgumentTypeError("apps must be unique positive integers")
    return apps


def fetch_lb_metrics(url, timeout=10):
    if not url:
        return None
    with urllib.request.urlopen(url, timeout=timeout) as response:
        body = response.read().decode("utf-8", "replace")
    result = {}
    for key, (name, required_labels) in LB_COUNTERS.items():
        value = metric_value(body, name, required_labels)
        # prometheus client families do not emit a labeled child until its
        # first observation. A registered HELP descriptor proves a missing
        # child is an authoritative zero; a missing family stays fail-closed.
        family = re.sub(r"_(?:count|sum|bucket)$", "", name)
        if value is None and f"# HELP {family} " in body:
            value = 0.0
        result[key] = value
    return result


def fetch_replica_inventory(base, timeout=10):
    """Read content-free exact-index state keyed only by replica ordinal."""
    if not base:
        return None
    request = urllib.request.Request(base.rstrip("/") + "/health")
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = json.loads(response.read().decode("utf-8", "replace"))
    except (OSError, ValueError, urllib.error.URLError):
        return None
    replicas = payload.get("replicas") if isinstance(payload, dict) else None
    if not isinstance(replicas, list):
        return None
    result = {}
    for replica in replicas:
        if not isinstance(replica, dict):
            return None
        index = replica.get("index")
        exact = replica.get("exact_inventory")
        if (
            not isinstance(index, int)
            or isinstance(index, bool)
            or index < 0
            or str(index) in result
            or not isinstance(exact, dict)
        ):
            return None
        trusted = exact.get("trusted")
        blocks = exact.get("resident_blocks")
        tokens = exact.get("resident_tokens")
        if (
            not isinstance(trusted, bool)
            or not isinstance(blocks, int)
            or isinstance(blocks, bool)
            or blocks < 0
            or not isinstance(tokens, int)
            or isinstance(tokens, bool)
            or tokens < 0
        ):
            return None
        result[str(index)] = {
            "trusted": trusted,
            "resident_blocks": blocks,
            "resident_tokens": tokens,
        }
    return dict(sorted(result.items(), key=lambda item: int(item[0])))


def replica_inventory_change(before, after):
    if before is None or after is None or before.keys() != after.keys():
        return None
    result = {}
    for index in before:
        trusted_throughout = before[index]["trusted"] and after[index]["trusted"]
        result[index] = {
            "trusted_before": before[index]["trusted"],
            "trusted_after": after[index]["trusted"],
            "resident_blocks_before": before[index]["resident_blocks"],
            "resident_blocks_after": after[index]["resident_blocks"],
            "resident_blocks_change": (
                after[index]["resident_blocks"] - before[index]["resident_blocks"]
                if trusted_throughout
                else None
            ),
            "resident_tokens_before": before[index]["resident_tokens"],
            "resident_tokens_after": after[index]["resident_tokens"],
            "resident_tokens_change": (
                after[index]["resident_tokens"] - before[index]["resident_tokens"]
                if trusted_throughout
                else None
            ),
        }
    return result


def nonnegative_delta(before, after, keys):
    if before is None or after is None:
        return None
    result = {}
    for key in keys:
        left = before.get(key)
        right = after.get(key)
        result[key] = (
            None if left is None or right is None or right < left else right - left
        )
    return result


def aggregate_engine_delta(before, after):
    if not before or not after or len(before) != len(after):
        return None
    cells = [engine_delta(left, right) for left, right in zip(before, after)]
    if any(cell is None for cell in cells):
        return None
    aggregate = {}
    for key in ENGINE_COUNTERS:
        values = [cell[key] for cell in cells]
        aggregate[key] = None if any(value is None for value in values) else sum(values)
    queue_samples = aggregate["queue_samples"]
    prefill_samples = aggregate["prefill_samples"]
    prefix_queries = aggregate["prefix_queries"]
    prefix_hits = aggregate["prefix_hits"]
    aggregate["queue_ms_mean"] = (
        round(1000 * aggregate["queue_seconds_sum"] / queue_samples, 2)
        if queue_samples and aggregate["queue_seconds_sum"] is not None
        else None
    )
    aggregate["prefill_ms_mean"] = (
        round(1000 * aggregate["prefill_seconds_sum"] / prefill_samples, 2)
        if prefill_samples and aggregate["prefill_seconds_sum"] is not None
        else None
    )
    aggregate["prefix_hit_pct"] = (
        round(100 * prefix_hits / prefix_queries, 2)
        if prefix_queries and prefix_hits is not None
        else None
    )
    return aggregate


def cache_outcome(prompt, cached):
    if prompt <= 0 or cached < 0:
        return "unknown"
    if cached == 0:
        return "cold"
    if cached >= prompt:
        return "full"
    return "partial"


def reconcile(records, lb, engine, tolerance=0):
    good = [record for record in records if record["ok"]]
    response_prompt = sum(record["prompt_tokens"] for record in good)
    response_cached = sum(record["cached_tokens"] for record in good)
    expected = {
        "prompt_tokens": {
            "response_usage": response_prompt,
            "load_balancer": None if lb is None else lb["prompt_tokens"],
            "engine_prompt": None if engine is None else engine["prompt_tokens"],
            "engine_prefix_queries": None if engine is None else engine["prefix_queries"],
        },
        "cached_prompt_tokens": {
            "response_usage": response_cached,
            "load_balancer": None if lb is None else lb["cached_prompt_tokens"],
            "engine_cached": None if engine is None else engine["cached_prompt_tokens"],
            "engine_prefix_hits": None if engine is None else engine["prefix_hits"],
        },
        "requests": {
            "responses": len(good),
            "load_balancer": None if lb is None else lb["cache_requests"],
            "load_balancer_ttft": None if lb is None else lb["cache_ttft_samples"],
            "engine_queue_samples": None if engine is None else engine["queue_samples"],
            "engine_prefill_samples": None if engine is None else engine["prefill_samples"],
        },
    }
    consistent = True
    max_spread = 0
    for values in expected.values():
        present = [value for value in values.values() if value is not None]
        if len(present) != len(values):
            consistent = False
            continue
        spread = max(present) - min(present)
        max_spread = max(max_spread, spread)
        consistent &= spread <= tolerance
    return {
        "consistent": consistent,
        "tolerance": tolerance,
        "max_spread": max_spread,
        "values": expected,
    }


def synthetic_prefix(app, prefix_kib, salt):
    target = prefix_kib * 1024
    nonce = hashlib.blake2b(f"{salt}:{app}".encode(), digest_size=16).hexdigest()
    header = f"Synthetic cache nonce {nonce}. "
    # Keep the repeated portion representative of the original capacity
    # workload while making it independent from the fresh-salt spelling.
    unit = "Synthetic cache app 1786561234567890123-07; stable shared context. "
    prefix = header + unit * (target // len(unit.encode()) + 1)
    return prefix.encode()[:target].decode("utf-8", "strict")


def messages_for(app, session, turn, prefix_kib, salt):
    messages = [
        {"role": "system", "content": synthetic_prefix(app, prefix_kib, salt)}
    ]
    for prior in range(1, turn):
        messages.extend(
            [
                {
                    "role": "user",
                    "content": f"session {session} synthetic step {prior}",
                },
                {
                    "role": "assistant",
                    "content": f"synthetic answer {prior} for session {session}",
                },
            ]
        )
    messages.append(
        {
            "role": "user",
            "content": f"session {session} synthetic step {turn}; answer briefly",
        }
    )
    return messages


def workload_waves(apps, sessions, turns):
    """Yield phase-barrier waves so reuse never races an unfinished cold app."""
    for turn in range(1, turns + 1):
        for session in range(sessions):
            yield [(app, session, turn) for app in range(apps)]


def workload_coordinates(apps, sessions, turns):
    """Round-robin apps so cold placement and reuse distance are not phase-biased."""
    for wave in workload_waves(apps, sessions, turns):
        yield from wave


def execute_waves(apps, sessions, turns, concurrency, execute, progress=None):
    """Execute each app wave concurrently, preserving barriers and output order."""
    records = []
    total_requests = apps * sessions * turns
    total_waves = sessions * turns
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        for wave_index, wave in enumerate(
            workload_waves(apps, sessions, turns), start=1
        ):
            for wave_completed, ((app, session, turn), result) in enumerate(
                zip(wave, pool.map(execute, wave)), start=1
            ):
                record = {"app": app, "session": session, "turn": turn, **result}
                records.append(record)
                if progress:
                    progress(
                        {
                            "completed": len(records),
                            "total": total_requests,
                            "wave": wave_index,
                            "waves": total_waves,
                            "wave_completed": wave_completed,
                            "wave_size": len(wave),
                            "ok": bool(record.get("ok")),
                        }
                    )
    return records


def execute_request(
    base, model, token, messages, max_tokens, timeout, extra_headers=None
):
    body = {
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    headers = {
        "Authorization": "Bearer " + token,
        "Content-Type": "application/json",
    }
    if extra_headers:
        headers.update(extra_headers)
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers=headers,
    )
    started = time.perf_counter()
    assembly = Assembly()
    route = None
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            route = response.headers.get("X-Mini-Dynamo-Upstream")
            decoder = SSEDecoder(assembly)
            for line in response:
                decoder.feed(line, time.perf_counter())
            decoder.finish()
    except urllib.error.HTTPError as error:
        error.read(4096)
        retry_reason = (
            error.headers.get("X-Mini-Dynamo-Shadow-Soak-Retry")
            if error.headers is not None
            else None
        )
        retryable = error.code == 503 and retry_reason in {
            "tokenizer_unavailable",
            "attestation_changed",
        }
        return {
            "ok": False,
            "error": f"HTTP {error.code}",
            "retryable": retryable,
            "retry_reason": retry_reason if retryable else None,
        }
    except Exception as error:  # benchmark failures are structured, never payload dumps
        return {"ok": False, "error": type(error).__name__}
    ended = time.perf_counter()
    result = assembly.result()
    prompt, cached, completion = token_counts(result["usage"])
    ttft = assembly.generated_at[0] - started if assembly.generated_at else None
    return {
        "ok": prompt > 0,
        "error": None if prompt > 0 else "missing_usage",
        "route": route,
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "completion_tokens": completion,
        "cache_outcome": cache_outcome(prompt, cached),
        "ttft_ms": None if ttft is None else round(1000 * ttft, 1),
        "wall_ms": round(1000 * (ended - started), 1),
    }


def execute_with_retries(execute, retries, retry_delay_seconds, timeout_seconds):
    """Retry only explicit pre-serving admissions within one absolute deadline."""
    deadline = time.monotonic() + timeout_seconds
    retry_reasons = collections.Counter()
    for attempt in range(1, retries + 2):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return {
                "ok": False,
                "error": "retry_deadline",
                "retryable": False,
                "retry_reason": None,
                "client_attempts": attempt - 1,
                "retry_reasons": dict(sorted(retry_reasons.items())),
            }
        result = execute(remaining)
        result["client_attempts"] = attempt
        if not result.get("retryable") or attempt == retries + 1:
            result["retry_reasons"] = dict(sorted(retry_reasons.items()))
            return result
        retry_reasons[result["retry_reason"]] += 1
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            result["retry_reasons"] = dict(sorted(retry_reasons.items()))
            return result
        time.sleep(min(retry_delay_seconds, remaining))
    raise AssertionError("bounded request attempt loop returned no result")


def latency_by_outcome(records, field):
    grouped = collections.defaultdict(list)
    for record in records:
        value = record.get(field)
        if record.get("ok") and value is not None:
            grouped[record["cache_outcome"]].append(value)
    return {
        outcome: {
            "count": len(values),
            "p50": round(statistics.median(values), 1),
            "p95": percentile(values, 0.95),
        }
        for outcome, values in sorted(grouped.items())
    }


def summarize(
    records,
    apps,
    sessions,
    turns,
    prefix_kib,
    elapsed,
    lb,
    engine,
    tolerance,
    replica_inventory=None,
):
    good = [record for record in records if record["ok"]]
    prompt = sum(record["prompt_tokens"] for record in good)
    cached = sum(record["cached_tokens"] for record in good)
    completion = sum(record["completion_tokens"] for record in good)
    outcomes = collections.Counter(record["cache_outcome"] for record in good)
    errors = collections.Counter(
        record["error"] for record in records if not record["ok"]
    )
    routes = collections.Counter(record["route"] or "unknown" for record in good)
    ttfts = [record["ttft_ms"] for record in good if record["ttft_ms"] is not None]
    walls = [record["wall_ms"] for record in good]
    last_seen = {}
    reuse_distances = []
    reuse_records = []
    initial_records = []
    for position, record in enumerate(records):
        app = record["app"]
        if app in last_seen:
            reuse_distances.append(position - last_seen[app] - 1)
            if record["ok"]:
                reuse_records.append(record)
        elif record["ok"]:
            initial_records.append(record)
        last_seen[app] = position
    initial_prompt = sum(record["prompt_tokens"] for record in initial_records)
    reuse_prompt = sum(record["prompt_tokens"] for record in reuse_records)
    reuse_cached = sum(record["cached_tokens"] for record in reuse_records)
    reuse_outcomes = collections.Counter(
        record["cache_outcome"] for record in reuse_records
    )
    return {
        "type": "cache_working_set",
        "apps": apps,
        "sessions": sessions,
        "turns": turns,
        "prefix_kib": prefix_kib,
        "synthetic_working_set_mib": round(apps * prefix_kib / 1024, 2),
        "requests": len(records),
        "successful": len(good),
        "outcomes": dict(sorted(outcomes.items())),
        "errors": dict(sorted(errors.items())),
        "route_split": dict(sorted(routes.items())),
        "prompt_tokens": prompt,
        "initial_wave_prompt_tokens": initial_prompt,
        "initial_prompt_tokens_mean": (
            round(initial_prompt / len(initial_records), 1) if initial_records else None
        ),
        "cached_tokens": cached,
        "completion_tokens": completion,
        "cache_hit_pct": round(100 * cached / prompt, 2) if prompt else None,
        "request_reuse_pct": round(100 * (len(good) - outcomes["cold"]) / len(good), 2)
        if good
        else None,
        "reuse_wave_requests": len(reuse_records),
        "reuse_wave_outcomes": dict(sorted(reuse_outcomes.items())),
        "reuse_wave_cache_hit_pct": (
            round(100 * reuse_cached / reuse_prompt, 2) if reuse_prompt else None
        ),
        "ttft_ms_p50": round(statistics.median(ttfts), 1) if ttfts else None,
        "ttft_ms_p95": percentile(ttfts, 0.95),
        "ttft_ms_by_outcome": latency_by_outcome(good, "ttft_ms"),
        "wall_ms_p50": round(statistics.median(walls), 1) if walls else None,
        "wall_ms_p95": percentile(walls, 0.95),
        "wall_ms_by_outcome": latency_by_outcome(good, "wall_ms"),
        "elapsed_seconds": round(elapsed, 3),
        "output_tok_s": round(completion / elapsed, 1) if elapsed else None,
        "total_tok_s": round((prompt + completion) / elapsed, 1) if elapsed else None,
        "reuse_distance_requests_p50": round(statistics.median(reuse_distances), 1)
        if reuse_distances
        else None,
        "reuse_distance_requests_max": max(reuse_distances) if reuse_distances else None,
        "lb_metrics_delta": lb,
        "live_block_churn_pct": (
            round(100 * lb["live_removed_blocks"] / lb["live_stored_blocks"], 2)
            if lb
            and lb["live_stored_blocks"]
            and lb["live_removed_blocks"] is not None
            else None
        ),
        "engine_metrics_delta": engine,
        "replica_exact_inventory": replica_inventory,
        "reconciliation": reconcile(records, lb, engine, tolerance),
    }


def run_cell(args, apps, token, extra_headers=None):
    salt = f"{args.salt}-a{apps}"
    lb_before = fetch_lb_metrics(args.metrics_url)
    inventory_before = fetch_replica_inventory(args.base)
    engine_before = [fetch_engine_metrics(url) for url in args.engine_metrics]
    started = time.perf_counter()
    progress_successful = 0

    def execute(coordinate):
        app, session, turn = coordinate
        messages = messages_for(app, session, turn, args.prefix_kib, salt)
        retry_timeout = getattr(args, "retry_timeout_seconds", None) or args.timeout
        return execute_with_retries(
            lambda remaining: execute_request(
                args.base,
                args.model,
                token,
                messages,
                args.max_tokens,
                min(args.timeout, remaining),
                extra_headers,
            ),
            getattr(args, "request_retries", 0),
            getattr(args, "retry_delay_seconds", 0.1),
            retry_timeout,
        )

    def progress(state):
        nonlocal progress_successful
        progress_successful += int(state["ok"])
        if not args.progress_every or (
            state["completed"] % args.progress_every
            and state["wave_completed"] != state["wave_size"]
        ):
            return
        print(
            json.dumps(
                {
                    "type": "cache_progress",
                    "apps": apps,
                    "completed": state["completed"],
                    "total": state["total"],
                    "successful": progress_successful,
                    "wave": state["wave"],
                    "waves": state["waves"],
                    "elapsed_seconds": round(time.perf_counter() - started, 3),
                },
                sort_keys=True,
            ),
            file=sys.stderr,
            flush=True,
        )

    records = execute_waves(
        apps,
        args.sessions,
        args.turns,
        args.concurrency,
        execute,
        progress if args.progress_every else None,
    )
    if args.emit_requests:
        for record in records:
            print(json.dumps({"type": "cache_request", **record}, sort_keys=True))
    elapsed = time.perf_counter() - started
    time.sleep(args.settle_seconds)
    lb_after = fetch_lb_metrics(args.metrics_url)
    inventory_after = fetch_replica_inventory(args.base)
    engine_after = [fetch_engine_metrics(url) for url in args.engine_metrics]
    lb = nonnegative_delta(lb_before, lb_after, LB_COUNTERS)
    engine = aggregate_engine_delta(engine_before, engine_after)
    summary = summarize(
        records,
        apps,
        args.sessions,
        args.turns,
        args.prefix_kib,
        elapsed,
        lb,
        engine,
        args.reconcile_tolerance,
        replica_inventory_change(inventory_before, inventory_after),
    )
    summary["concurrency"] = args.concurrency
    summary["client_attempts_total"] = sum(
        record.get("client_attempts", 1) for record in records
    )
    summary["retried_requests"] = sum(
        record.get("client_attempts", 1) > 1 for record in records
    )
    retry_reasons = collections.Counter()
    for record in records:
        retry_reasons.update(record.get("retry_reasons", {}))
    summary["retry_reasons"] = dict(sorted(retry_reasons.items()))
    return summary


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("base")
    result.add_argument("model")
    result.add_argument("--apps", type=parse_apps, default=parse_apps("1,4,8"))
    result.add_argument("--sessions", type=int, default=2)
    result.add_argument("--turns", type=int, default=2)
    result.add_argument("--prefix-kib", type=int, default=32)
    result.add_argument("--max-tokens", type=int, default=8)
    result.add_argument(
        "--concurrency",
        type=int,
        default=1,
        help="requests per app wave; each session/turn remains a phase barrier",
    )
    result.add_argument("--salt", default=str(time.time_ns()))
    result.add_argument("--metrics-url")
    result.add_argument("--engine-metrics", action="append", default=[])
    result.add_argument("--timeout", type=float, default=300)
    result.add_argument("--request-retries", type=int, default=0)
    result.add_argument("--retry-delay-seconds", type=float, default=0.1)
    result.add_argument("--retry-timeout-seconds", type=float)
    result.add_argument("--settle-seconds", type=float, default=0.25)
    result.add_argument("--reconcile-tolerance", type=float, default=0)
    result.add_argument("--require-reconciled", action="store_true")
    result.add_argument("--emit-requests", action="store_true")
    result.add_argument(
        "--progress-every",
        type=int,
        default=0,
        help="emit a content-free stderr progress record every N completions",
    )
    return result


def main():
    args = parser().parse_args()
    if (
        args.sessions < 1
        or args.turns < 1
        or args.prefix_kib < 1
        or args.max_tokens < 1
        or args.concurrency < 1
        or args.progress_every < 0
        or args.request_retries < 0
        or args.retry_delay_seconds < 0
        or (args.retry_timeout_seconds is not None and args.retry_timeout_seconds <= 0)
    ):
        raise SystemExit(
            "sessions, turns, prefix-kib, max-tokens, and concurrency must be positive; "
            "retry-timeout-seconds must be positive; other retry/progress bounds "
            "must be nonnegative"
        )
    token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
    if not token:
        raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
    failed = False
    for apps in args.apps:
        summary = run_cell(args, apps, token)
        print(json.dumps(summary, sort_keys=True))
        failed |= summary["successful"] != summary["requests"]
        if args.require_reconciled:
            failed |= not summary["reconciliation"]["consistent"]
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
