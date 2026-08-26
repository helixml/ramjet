#!/usr/bin/env python3
"""Exercise the affinity-versus-load route boundary with bounded traffic.

Usage: route_conflict.py BASE MODEL [blockers] [blocker_tokens] [runs]

Each run warms a fresh long shared system prefix, starts several long decodes
that should stay on that warm upstream, then sends one short returning probe
while those decodes are active. Blockers run to completion so the result can
compare useful work as well as probe latency.

Set BENCH_TOKEN (or VLLM_API_KEY). METRICS_URLS may contain comma-separated
direct engine /metrics endpoints. BENCH_REQUIRE_RECONCILED_SPECULATION=1 makes
native prompt/request/completion/speculation reconciliation a hard gate.

BLOCKER_READY_MODE=first_token moves the probe boundary from upstream response
headers to the first semantic content/reasoning/tool delta. Combine it with a
positive BLOCKER_TAIL_KIB to give every blocker a distinct non-reusable tail;
that is the controlled shape for RJ_ROUTE_PHASE_AWARE_LOAD because a fully
warm request already reserves one unit and cannot be reduced further.

PROBE_BASE may point the probe at a different endpoint from BASE. This permits
an authoritative direct-engine oracle using the exact same workload: BASE is
the warm, busy engine and PROBE_BASE is either that engine or its cold, idle
peer. Leave it unset for the normal Ramjet routing experiment.

Pair the result with route-journal replay to see which alpha/cap values would
move the probe without changing production policy.
"""

from __future__ import annotations

import json
import math
import os
import statistics
import sys
import threading
import time
import urllib.request

from engine_metrics import (
    COUNTERS as ENGINE_COUNTERS,
    delta as engine_delta,
    fetch as fetch_engine_metrics,
    fetch_speculative,
    speculative_delta,
)


FILLER = (
    "The service validates every transition, records the result in an "
    "append-only ledger, and reconciles it against the durable snapshot. "
)


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return None
    return values[math.ceil(fraction * len(values)) - 1]


def stream_request(
    base,
    model,
    token,
    system,
    user,
    max_tokens,
    output,
    key,
    ready=None,
    ready_mode="headers",
):
    body = {
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    encoded_body = json.dumps(body).encode()
    request = urllib.request.Request(
        base + "/v1/chat/completions",
        data=encoded_body,
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
    )
    started = time.perf_counter()
    first = None
    route = None
    prompt_tokens = cached_tokens = completion_tokens = 0
    try:
        with urllib.request.urlopen(request, timeout=900) as response:
            route = response.headers.get("X-Ramjet-Upstream")
            if ready is not None and ready_mode == "headers":
                ready.set()
            for raw in response:
                line = raw.decode("utf-8", "ignore").strip()
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if not payload or payload == "[DONE]":
                    continue
                event = json.loads(payload)
                choices = event.get("choices") or []
                if choices and any(
                    (choices[0].get("delta") or {}).get(field)
                    for field in ("content", "reasoning", "reasoning_content", "tool_calls")
                ):
                    first = first or time.perf_counter()
                    if ready is not None and ready_mode == "first_token":
                        ready.set()
                usage = event.get("usage") or {}
                prompt_tokens = usage.get("prompt_tokens", prompt_tokens)
                completion_tokens = usage.get("completion_tokens", completion_tokens)
                # Qwen hybrid responses currently report this as structural
                # zero. Retain it for backward-compatible observation only;
                # reconciliation below never treats it as cache authority.
                cached_tokens = (usage.get("prompt_tokens_details") or {}).get(
                    "cached_tokens", cached_tokens
                )
        ended = time.perf_counter()
        ok = first is not None and prompt_tokens > 0 and completion_tokens > 0
        output[key] = {
            "ok": ok,
            "route": route,
            "ttft_ms": round((first - started) * 1000, 1) if first else None,
            "wall_ms": round((ended - started) * 1000, 1),
            "prompt_tokens": prompt_tokens,
            "cached_tokens": cached_tokens,
            "completion_tokens": completion_tokens,
            "request_bytes": len(encoded_body),
        }
        if not ok:
            output[key]["error"] = "missing_generated_output_or_usage"
    except Exception as error:  # output must never contain request or response material
        output[key] = {"ok": False, "route": route, "error": type(error).__name__}
    finally:
        if ready is not None:
            ready.set()


def blocker_user(config, run_index, blocker_index):
    instruction = (
        f"blocker {blocker_index}: write a long production-quality Python module with tests"
    )
    target = config["blocker_tail_kib"] * 1024
    if target == 0:
        return instruction
    header = f"[unique tail {config['salt']}/{run_index}/{blocker_index}] "
    unit = "Inspect this private synthetic module record and preserve its ordering. "
    tail = (header + unit * (target // len(unit) + 1))[:target]
    return f"{instruction}\n{tail}"


def route_state_snapshot(base):
    try:
        with urllib.request.urlopen(base + "/health", timeout=10) as response:
            document = json.load(response)
    except Exception:
        return None
    replicas = document.get("replicas") if isinstance(document, dict) else None
    if not isinstance(replicas, list):
        return None
    result = []
    for replica in replicas:
        if not isinstance(replica, dict):
            return None
        index = replica.get("index")
        inflight = replica.get("inflight")
        load_units = replica.get("load_units")
        if not all(type(value) is int and value >= 0 for value in (index, inflight, load_units)):
            return None
        result.append({"upstream": index, "inflight": inflight, "load_units": load_units})
    return result


def engine_gauge_snapshot(urls):
    if not urls:
        return None
    try:
        snapshots = [fetch_engine_metrics(url, timeout=10) for url in urls]
    except Exception:
        return None
    return [
        {
            "running": snapshot.get("running"),
            "waiting": snapshot.get("waiting"),
            "kv_cache_usage": snapshot.get("kv_cache_usage"),
        }
        for snapshot in snapshots
    ]


def run_once(index, config):
    system = (
        f"[route-conflict {config['salt']}/{index}] "
        + FILLER * max(1, round(config["context_tokens"] / 22))
    )
    output = {}
    stream_request(
        config["base"],
        config["model"],
        config["token"],
        system,
        "Warm this shared runbook. Reply OK.",
        16,
        output,
        "warm",
    )
    if not output["warm"].get("ok"):
        return output, 0.0, None

    ready = [threading.Event() for _ in range(config["blockers"])]
    threads = [
        threading.Thread(
            target=stream_request,
            args=(
                config["base"],
                config["model"],
                config["token"],
                system,
                blocker_user(config, index, blocker),
                config["blocker_tokens"],
                output,
                f"blocker-{blocker}",
                ready[blocker],
                config["blocker_ready_mode"],
            ),
        )
        for blocker in range(config["blockers"])
    ]
    blockers_started = time.perf_counter()
    for thread in threads:
        thread.start()
    deadline = time.monotonic() + 120
    for event in ready:
        event.wait(max(0, deadline - time.monotonic()))

    boundary = {
        "router": route_state_snapshot(config["base"]),
        "engines": engine_gauge_snapshot(config["metrics_urls"]),
    }

    stream_request(
        config["probe_base"],
        config["model"],
        config["token"],
        system,
        "returning probe: continue the analysis with precise implementation advice",
        config["probe_tokens"],
        output,
        "probe",
    )
    # Blockers are useful measured work, not disposable load. Each request has
    # its own bounded 900-second HTTP timeout; the outer node guard supplies
    # the stricter whole-cell deadline in production.
    for thread in threads:
        thread.join()
    blocker_window_ms = (time.perf_counter() - blockers_started) * 1000
    return output, blocker_window_ms, boundary


def combine_speculative_snapshots(snapshots):
    if not snapshots:
        return None
    result = {}
    for key in snapshots[0]:
        if key == "accepted_per_position":
            positions = {}
            for snapshot in snapshots:
                for position, value in (snapshot.get(key) or {}).items():
                    positions[position] = positions.get(position, 0) + value
            result[key] = positions
            continue
        values = [snapshot.get(key) for snapshot in snapshots]
        result[key] = None if any(value is None for value in values) else sum(values)
    return result


def aggregate_engine_delta(before, after):
    if not before or not after or len(before) != len(after):
        return None
    cells = [engine_delta(left, right) for left, right in zip(before, after, strict=True)]
    if any(cell is None for cell in cells):
        return None
    result = {}
    for key in ENGINE_COUNTERS:
        values = [cell.get(key) for cell in cells]
        result[key] = None if any(value is None for value in values) else sum(values)
    result["queue_ms_mean"] = (
        round(1000 * result["queue_seconds_sum"] / result["queue_samples"], 2)
        if result["queue_samples"] and result["queue_seconds_sum"] is not None
        else None
    )
    result["prefill_ms_mean"] = (
        round(1000 * result["prefill_seconds_sum"] / result["prefill_samples"], 2)
        if result["prefill_samples"] and result["prefill_seconds_sum"] is not None
        else None
    )
    result["prefix_hit_pct"] = (
        round(100 * result["prefix_hits"] / result["prefix_queries"], 2)
        if result["prefix_queries"] and result["prefix_hits"] is not None
        else None
    )
    return result


def metric_snapshot(urls):
    if not urls:
        return None
    try:
        ordinary = [fetch_engine_metrics(url, timeout=30) for url in urls]
        speculative = [fetch_speculative(url, timeout=30) for url in urls]
        return ordinary, speculative
    except Exception:
        return None


def build_reconciliation(records, engine, speculative):
    good = [record for record in records if record.get("ok")]
    client_requests = len(good)
    client_prompt_tokens = sum(record.get("prompt_tokens", 0) for record in good)
    client_completion_tokens = sum(record.get("completion_tokens", 0) for record in good)

    def matches(value, expected):
        return None if value is None else value == expected

    prompt_match = matches(None if engine is None else engine.get("prompt_tokens"), client_prompt_tokens)
    queue_match = matches(None if engine is None else engine.get("queue_samples"), client_requests)
    prefill_match = matches(None if engine is None else engine.get("prefill_samples"), client_requests)
    speculation_match = speculative.get("reconciled") if speculative else None
    comparisons = (prompt_match, queue_match, prefill_match, speculation_match)
    if any(value is False for value in comparisons):
        contaminated = True
    elif all(value is True for value in comparisons):
        contaminated = False
    else:
        contaminated = None
    return {
        "client_requests": client_requests,
        "client_prompt_tokens": client_prompt_tokens,
        "client_completion_tokens": client_completion_tokens,
        "engine_prompt_tokens": None if engine is None else engine.get("prompt_tokens"),
        "engine_queue_samples": None if engine is None else engine.get("queue_samples"),
        "engine_prefill_samples": None if engine is None else engine.get("prefill_samples"),
        "prompt_tokens_match": prompt_match,
        "queue_samples_match": queue_match,
        "prefill_samples_match": prefill_match,
        "speculation_match": speculation_match,
        "cached_tokens_authoritative": False,
        "contaminated": contaminated,
        "reconciled": contaminated is False,
    }


def summarize_runs(config, run_outputs, blocker_windows_ms, boundaries, snapshots):
    records = [record for output in run_outputs for record in output.values()]
    errors = [record["error"] for record in records if not record.get("ok")]
    probes = [output["probe"] for output in run_outputs if output.get("probe", {}).get("ok")]
    blockers = [
        record
        for output in run_outputs
        for key, record in output.items()
        if key.startswith("blocker-") and record.get("ok")
    ]
    blocker_ttfts = [record["ttft_ms"] for record in blockers if record.get("ttft_ms") is not None]
    blocker_prompt_tokens = sum(record.get("prompt_tokens", 0) for record in blockers)
    blocker_completion_tokens = sum(record.get("completion_tokens", 0) for record in blockers)
    blocker_window_seconds = sum(blocker_windows_ms) / 1000

    before, after = snapshots
    if before is None or after is None:
        engine = None
        speculative = {"state": "unavailable", "reconciled": False}
    else:
        engine = aggregate_engine_delta(before[0], after[0])
        speculative = speculative_delta(
            combine_speculative_snapshots(before[1]),
            combine_speculative_snapshots(after[1]),
            sum(record.get("completion_tokens", 0) for record in records if record.get("ok")),
            sum(record.get("ok") is True for record in records),
            expected_enabled=True,
        )
    reconciliation = build_reconciliation(records, engine, speculative)
    result = {
        "base": config["base"],
        "probe_base": config["probe_base"],
        "runs": config["runs"],
        "blockers": config["blockers"],
        "blocker_tokens": config["blocker_tokens"],
        "blocker_tail_kib": config["blocker_tail_kib"],
        "blocker_ready_mode": config["blocker_ready_mode"],
        "context_target_tokens": config["context_tokens"],
        "probe_tokens": config["probe_tokens"],
        "metrics_endpoints": len(config["metrics_urls"]),
        "warm_routes": [output.get("warm", {}).get("route") for output in run_outputs],
        "blocker_routes": [
            [output.get(f"blocker-{index}", {}).get("route") for index in range(config["blockers"])]
            for output in run_outputs
        ],
        "probe_routes": [probe.get("route") for probe in probes],
        "probe_boundaries": boundaries,
        # Backward-compatible observation only. See cached_tokens_authoritative.
        "probe_cached_tokens": [probe.get("cached_tokens") for probe in probes],
        "probe_request_bytes": [probe.get("request_bytes") for probe in probes],
        "probe_ttft_ms_median": (
            round(statistics.median(probe["ttft_ms"] for probe in probes), 1) if probes else None
        ),
        "blocker_requests_ok": len(blockers),
        "blocker_prompt_tokens": blocker_prompt_tokens,
        "blocker_completion_tokens": blocker_completion_tokens,
        "blocker_ttft_ms_p95": (
            round(percentile(blocker_ttfts, 0.95), 1) if blocker_ttfts else None
        ),
        "blocker_observation_window_seconds": round(blocker_window_seconds, 3),
        "blocker_aggregate_output_tok_s": (
            round(blocker_completion_tokens / blocker_window_seconds, 1)
            if blocker_window_seconds
            else None
        ),
        "usage": {
            "requests": sum(record.get("ok") is True for record in records),
            "prompt_tokens": sum(record.get("prompt_tokens", 0) for record in records if record.get("ok")),
            "completion_tokens": sum(
                record.get("completion_tokens", 0) for record in records if record.get("ok")
            ),
            "response_cached_tokens_untrusted": sum(
                record.get("cached_tokens", 0) for record in records if record.get("ok")
            ),
        },
        "engine_metrics_delta": engine,
        "speculative": speculative,
        "reconciliation": reconciliation,
        "requests_failed": len(errors),
    }
    if errors:
        result["errors"] = errors
    return result


def parse_config(argv):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) < 2 or len(argv) > 5:
        raise SystemExit("usage: route_conflict.py BASE MODEL [blockers] [blocker_tokens] [runs]")
    blockers = int(argv[2]) if len(argv) > 2 else 4
    blocker_tokens = int(argv[3]) if len(argv) > 3 else 1024
    runs = int(argv[4]) if len(argv) > 4 else 3
    token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
    if not token:
        raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
    context_tokens = int(os.environ.get("CONTEXT_TOKENS", "20000"))
    probe_tokens = int(os.environ.get("PROBE_TOKENS", "64"))
    blocker_tail_kib = int(os.environ.get("BLOCKER_TAIL_KIB", "0"))
    if (
        min(blockers, blocker_tokens, runs, context_tokens, probe_tokens) <= 0
        or not 0 <= blocker_tail_kib <= 8 * 1024
    ):
        raise SystemExit(
            "blockers, blocker_tokens, runs, context, and probe tokens must be positive; "
            "BLOCKER_TAIL_KIB must be from 0 through 8192"
        )
    blocker_ready_mode = os.environ.get("BLOCKER_READY_MODE", "headers")
    if blocker_ready_mode not in {"headers", "first_token"}:
        raise SystemExit("BLOCKER_READY_MODE must be headers or first_token")
    require = os.environ.get("BENCH_REQUIRE_RECONCILED_SPECULATION", "0")
    if require not in {"0", "1"}:
        raise SystemExit("BENCH_REQUIRE_RECONCILED_SPECULATION must be 0 or 1")
    metrics_urls = [
        value.strip()
        for value in os.environ.get("METRICS_URLS", os.environ.get("METRICS_URL", "")).split(",")
        if value.strip()
    ]
    if require == "1" and not metrics_urls:
        raise SystemExit("reconciled speculation requires METRICS_URLS")
    settle_seconds = float(os.environ.get("METRICS_SETTLE_SECONDS", "0.25"))
    if not math.isfinite(settle_seconds) or settle_seconds < 0:
        raise SystemExit("METRICS_SETTLE_SECONDS must be finite and non-negative")
    return {
        "base": argv[0].rstrip("/"),
        "probe_base": os.environ.get("PROBE_BASE", argv[0]).rstrip("/"),
        "model": argv[1],
        "blockers": blockers,
        "blocker_tokens": blocker_tokens,
        "runs": runs,
        "token": token,
        "salt": os.environ.get("SALT") or str(time.time_ns()),
        "context_tokens": context_tokens,
        "probe_tokens": probe_tokens,
        "blocker_tail_kib": blocker_tail_kib,
        "blocker_ready_mode": blocker_ready_mode,
        "metrics_urls": metrics_urls,
        "require_reconciled": require == "1",
        "settle_seconds": settle_seconds,
    }


def main(argv=None):
    config = parse_config(argv)
    before = metric_snapshot(config["metrics_urls"])
    run_results = [run_once(index, config) for index in range(config["runs"])]
    run_outputs = [output for output, _, _ in run_results]
    blocker_windows_ms = [window for _, window, _ in run_results]
    boundaries = [boundary for _, _, boundary in run_results]
    time.sleep(config["settle_seconds"])
    after = metric_snapshot(config["metrics_urls"])
    result = summarize_runs(config, run_outputs, blocker_windows_ms, boundaries, (before, after))
    measurement_ok = not config["require_reconciled"] or result["reconciliation"]["reconciled"]
    if not measurement_ok:
        result["measurement_error"] = "native_metrics_not_reconciled"
    print(json.dumps(result, sort_keys=True))
    return 0 if not result["requests_failed"] and measurement_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
