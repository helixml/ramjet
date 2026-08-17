#!/usr/bin/env python3
"""Deterministic code-decode benchmark with real token accounting.

Usage: codebench.py BASE MODEL [max_tokens] [concurrency] [runs]

Set BENCH_TOKEN (or VLLM_API_KEY). The benchmark streams responses and uses
the final usage block; SSE chunk counts are not token counts under speculative
decoding. The prompt intentionally matches the upstream RTX PRO 6000 recipe.
"""

import json
import math
import os
import statistics
import sys
import threading
import time
import urllib.request

from engine_metrics import fetch_speculative, speculative_delta


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
MAX_TOKENS = int(sys.argv[3]) if len(sys.argv) > 3 else 512
CONCURRENCY = int(sys.argv[4]) if len(sys.argv) > 4 else 1
RUNS = int(sys.argv[5]) if len(sys.argv) > 5 else 5
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")

CODE_PROMPT = (
    "Write a complete, production-quality Python module that implements a "
    "thread-safe LRU cache with TTL expiry. Include: the full class with type "
    "hints, __getitem__/__setitem__/__delitem__, a background sweeper thread, "
    "explicit locking, a stats() method returning hits/misses/evictions, and "
    "pytest unit tests covering eviction order, TTL expiry, and concurrent "
    "access. Output only code."
)
PROSE_PROMPT = (
    "Write a thoughtful, detailed essay about how a coastal city adapts its "
    "infrastructure, institutions, and culture to a changing climate. Use vivid "
    "examples, acknowledge tradeoffs, and avoid bullet points."
)
WORKLOAD = os.environ.get("BENCH_WORKLOAD", "code")
if WORKLOAD not in ("code", "prose"):
    raise SystemExit("BENCH_WORKLOAD must be code or prose")
PROMPT = os.environ.get("BENCH_PROMPT") or (CODE_PROMPT if WORKLOAD == "code" else PROSE_PROMPT)
TEMPERATURE = float(os.environ.get("BENCH_TEMPERATURE", "0" if WORKLOAD == "code" else "0.6"))
METRICS_URLS = [
    value.strip()
    for value in os.environ.get("METRICS_URLS", os.environ.get("METRICS_URL", "")).split(",")
    if value.strip()
]
REQUIRE_RECONCILED = os.environ.get(
    "BENCH_REQUIRE_RECONCILED_SPECULATION", "0"
)
if REQUIRE_RECONCILED not in {"0", "1"}:
    raise SystemExit("BENCH_REQUIRE_RECONCILED_SPECULATION must be 0 or 1")
REQUIRE_RECONCILED = REQUIRE_RECONCILED == "1"


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return None
    return values[math.ceil(fraction * len(values)) - 1]


def one(index, output):
    body = {
        "model": MODEL,
        "messages": [{"role": "user", "content": PROMPT}],
        "max_tokens": MAX_TOKENS,
        "temperature": TEMPERATURE,
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    request = urllib.request.Request(
        BASE + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": "Bearer " + TOKEN,
            "Content-Type": "application/json",
        },
    )
    started = time.perf_counter()
    first = last = None
    prompt_tokens = cached_tokens = completion_tokens = 0
    try:
        with urllib.request.urlopen(request, timeout=900) as response:
            route = response.headers.get("X-Ramjet-Upstream")
            for raw in response:
                line = raw.decode("utf-8", "ignore").strip()
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if not payload or payload == "[DONE]":
                    continue
                event = json.loads(payload)
                choices = event.get("choices") or []
                if choices:
                    delta = choices[0].get("delta") or {}
                    if (
                        delta.get("content")
                        or delta.get("reasoning")
                        or delta.get("reasoning_content")
                    ):
                        now = time.perf_counter()
                        first = first or now
                        last = now
                usage = event.get("usage") or {}
                prompt_tokens = usage.get("prompt_tokens", prompt_tokens)
                completion_tokens = usage.get("completion_tokens", completion_tokens)
                details = usage.get("prompt_tokens_details") or {}
                cached_tokens = details.get("cached_tokens", cached_tokens)
        ended = time.perf_counter()
        output[index] = {
            "ok": bool(first and prompt_tokens > 0 and completion_tokens > 0),
            "prompt_tokens": prompt_tokens,
            "cached_tokens": cached_tokens,
            "completion_tokens": completion_tokens,
            "ttft": first - started if first else None,
            "decode_seconds": last - first if first and last and last > first else None,
            "wall_seconds": ended - started,
            "route": route,
        }
        if completion_tokens > 1 and output[index]["decode_seconds"] is not None:
            output[index]["tpot_ms"] = round(
                1000 * output[index]["decode_seconds"] / (completion_tokens - 1),
                3,
            )
        else:
            output[index]["tpot_ms"] = None
        output[index]["ok"] = (
            output[index]["ok"]
            and output[index]["tpot_ms"] is not None
            and output[index]["tpot_ms"] > 0
        )
        if not output[index]["ok"]:
            output[index]["error"] = (
                "missing generated output, authoritative usage, or measurable TPOT"
            )
    except Exception as error:
        output[index] = {"ok": False, "error": f"{type(error).__name__}: {error}"}


def run_batch():
    output = {}
    threads = [threading.Thread(target=one, args=(i, output)) for i in range(CONCURRENCY)]
    started = time.perf_counter()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return output, time.perf_counter() - started


def metric_snapshot():
    if not METRICS_URLS:
        return None
    try:
        snapshots = []
        for metrics_url in METRICS_URLS:
            snapshots.append(fetch_speculative(metrics_url, timeout=30))
        if len(snapshots) == 1:
            return snapshots[0]
        combined = {}
        for key in snapshots[0]:
            if key == "accepted_per_position":
                combined[key] = {}
                for snapshot in snapshots:
                    for position, value in snapshot[key].items():
                        combined[key][position] = combined[key].get(position, 0) + value
            else:
                values = [snapshot[key] for snapshot in snapshots]
                combined[key] = None if any(value is None for value in values) else sum(values)
        return combined
    except Exception:
        return None


warmup, _ = run_batch()
if not warmup or not all(item.get("ok") for item in warmup.values()):
    raise SystemExit("warmup failed: " + json.dumps(warmup, sort_keys=True))

metrics_before = metric_snapshot()
batches = []
requests = []
measurement_started = time.perf_counter()
for repetition in range(RUNS):
    output, wall = run_batch()
    for index in sorted(output):
        output[index]["repetition"] = repetition
        requests.append(output[index])
    completed = sum(item.get("completion_tokens", 0) for item in output.values())
    batches.append(
        {
            "repetition": repetition,
            "wall_seconds": wall,
            "completion_tokens": completed,
        }
    )
measurement_wall_seconds = time.perf_counter() - measurement_started

good = [item for item in requests if item.get("ok")]
decode_rates = [
    item["completion_tokens"] / item["decode_seconds"]
    for item in good
    if item.get("decode_seconds") and item.get("completion_tokens")
]
ttfts = [item["ttft"] for item in good if item.get("ttft") is not None]
tpots = [item["tpot_ms"] for item in good if item.get("tpot_ms") is not None]
aggregate_rates = [item["completion_tokens"] / item["wall_seconds"] for item in batches]
dspark = speculative_delta(
    metrics_before,
    metric_snapshot(),
    sum(item.get("completion_tokens", 0) for item in good),
    len(good),
    expected_enabled=os.environ.get("BENCH_SPEC_MODE", "enabled") == "enabled",
)
result = {
    "schema_version": 2,
    "type": "engine_cell",
    "label": os.environ.get("SWEEP_LABEL", "run"),
    "base": BASE,
    "concurrency": CONCURRENCY,
    "runs": RUNS,
    "max_tokens": MAX_TOKENS,
    "workload": WORKLOAD,
    "temperature": TEMPERATURE,
    "requests_ok": len(good),
    "requests_failed": len(requests) - len(good),
    "completion_rate": round(len(good) / len(requests), 6) if requests else None,
    "observation_window_seconds": round(measurement_wall_seconds, 6),
    "per_stream_decode_tok_s_median": round(statistics.median(decode_rates), 1) if decode_rates else None,
    "aggregate_tok_s_median": round(statistics.median(aggregate_rates), 1) if aggregate_rates else None,
    "ttft_ms_median": round(statistics.median(ttfts) * 1000, 1) if ttfts else None,
    "ttft_ms_p95": round(percentile(ttfts, 0.95) * 1000, 1) if ttfts else None,
    "tpot_ms_median": round(statistics.median(tpots), 3) if tpots else None,
    "tpot_ms_p95": round(percentile(tpots, 0.95), 3) if tpots else None,
    "prompt_tokens": good[0].get("prompt_tokens") if good else None,
    "cached_tokens": good[0].get("cached_tokens") if good else None,
    "cache_hit_pct": (
        round(
            100
            * sum(item.get("cached_tokens", 0) for item in good)
            / sum(item.get("prompt_tokens", 0) for item in good),
            3,
        )
        if sum(item.get("prompt_tokens", 0) for item in good)
        else None
    ),
    "request_observations": [
        {
            "repetition": item["repetition"],
            "ok": item.get("ok") is True,
            "prompt_tokens": item.get("prompt_tokens"),
            "cached_tokens": item.get("cached_tokens"),
            "completion_tokens": item.get("completion_tokens"),
            "ttft_ms": (
                round(item["ttft"] * 1000, 3)
                if item.get("ttft") is not None
                else None
            ),
            "tpot_ms": item.get("tpot_ms"),
            "wall_ms": (
                round(item["wall_seconds"] * 1000, 3)
                if item.get("wall_seconds") is not None
                else None
            ),
        }
        for item in requests
    ],
    "repetition_observations": [
        {
            "repetition": item["repetition"],
            "observation_window_seconds": round(item["wall_seconds"], 6),
            "completion_tokens": item["completion_tokens"],
        }
        for item in batches
    ],
    "route_counts": {
        route: sum(1 for item in good if item.get("route") == route)
        for route in sorted({item.get("route") for item in good if item.get("route") is not None})
    },
    "dspark": dspark,
}
errors = [item["error"] for item in requests if not item.get("ok")]
if errors:
    result["errors"] = errors
measurement_ok = not REQUIRE_RECONCILED or dspark.get("reconciled") is True
if not measurement_ok:
    result["measurement_error"] = "speculation_not_reconciled"
print(json.dumps(result, sort_keys=True))
raise SystemExit(0 if not errors and measurement_ok else 1)
