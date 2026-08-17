#!/usr/bin/env python3
"""Exercise the affinity-versus-load route boundary with bounded traffic.

Usage: route_conflict.py BASE MODEL [blockers] [blocker_tokens] [runs]

Each run warms a fresh long shared system prefix, starts several long decodes
that should stay on that warm upstream, then sends one short returning probe
while those decodes are active. Set BENCH_TOKEN (or VLLM_API_KEY). Pair the
result with rc5 route-journal replay to see which alpha/cap values would move
the probe without changing production policy.
"""

import json
import os
import statistics
import sys
import threading
import time
import urllib.request


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
BLOCKERS = int(sys.argv[3]) if len(sys.argv) > 3 else 4
BLOCKER_TOKENS = int(sys.argv[4]) if len(sys.argv) > 4 else 1024
RUNS = int(sys.argv[5]) if len(sys.argv) > 5 else 3
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
SALT = os.environ.get("SALT") or str(time.time_ns())
CONTEXT_TOKENS = int(os.environ.get("CONTEXT_TOKENS", "20000"))
PROBE_TOKENS = int(os.environ.get("PROBE_TOKENS", "64"))
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
if min(BLOCKERS, BLOCKER_TOKENS, RUNS, CONTEXT_TOKENS, PROBE_TOKENS) <= 0:
    raise SystemExit("blockers, blocker_tokens, runs, context, and probe tokens must be positive")

FILLER = (
    "The service validates every transition, records the result in an "
    "append-only ledger, and reconciles it against the durable snapshot. "
)


def stream_request(system, user, max_tokens, output, key, ready=None, stop=None):
    body = {
        "model": MODEL,
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
        BASE + "/v1/chat/completions",
        data=encoded_body,
        headers={"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"},
    )
    started = time.perf_counter()
    first = None
    route = None
    cached_tokens = 0
    completion_tokens = 0
    try:
        with urllib.request.urlopen(request, timeout=900) as response:
            route = response.headers.get("X-Ramjet-Upstream")
            if ready is not None:
                ready.set()
            for raw in response:
                line = raw.decode("utf-8", "ignore").strip()
                if line.startswith("data:"):
                    payload = line[5:].strip()
                    if payload and payload != "[DONE]":
                        event = json.loads(payload)
                        choices = event.get("choices") or []
                        if choices and any(
                            (choices[0].get("delta") or {}).get(field)
                            for field in ("content", "reasoning", "reasoning_content", "tool_calls")
                        ):
                            first = first or time.perf_counter()
                        usage = event.get("usage") or {}
                        completion_tokens = usage.get("completion_tokens", completion_tokens)
                        cached_tokens = (usage.get("prompt_tokens_details") or {}).get(
                            "cached_tokens", cached_tokens
                        )
                if stop is not None and stop.is_set() and first is not None:
                    break
        output[key] = {
            "ok": True,
            "route": route,
            "ttft_ms": round((first - started) * 1000, 1) if first else None,
            "wall_ms": round((time.perf_counter() - started) * 1000, 1),
            "cached_tokens": cached_tokens,
            "completion_tokens": completion_tokens,
            "request_bytes": len(encoded_body),
        }
    except Exception as error:
        output[key] = {"ok": False, "route": route, "error": f"{type(error).__name__}: {error}"}
    finally:
        if ready is not None:
            ready.set()


def run(index):
    system = (
        f"[route-conflict {SALT}/{index}] "
        + FILLER * max(1, round(CONTEXT_TOKENS / 22))
    )
    output = {}
    stream_request(system, "Warm this shared runbook. Reply OK.", 16, output, "warm")
    if not output["warm"].get("ok"):
        return output

    stop = threading.Event()
    ready = [threading.Event() for _ in range(BLOCKERS)]
    threads = [
        threading.Thread(
            target=stream_request,
            args=(
                system,
                f"blocker {blocker}: write a long production-quality Python module with tests",
                BLOCKER_TOKENS,
                output,
                f"blocker-{blocker}",
                ready[blocker],
                stop,
            ),
        )
        for blocker in range(BLOCKERS)
    ]
    for thread in threads:
        thread.start()
    deadline = time.monotonic() + 120
    for event in ready:
        event.wait(max(0, deadline - time.monotonic()))

    stream_request(
        system,
        "returning probe: continue the analysis with precise implementation advice",
        PROBE_TOKENS,
        output,
        "probe",
    )
    stop.set()
    for thread in threads:
        thread.join(timeout=30)
    return output


runs = [run(index) for index in range(RUNS)]
errors = [item["error"] for run_output in runs for item in run_output.values() if not item.get("ok")]
probes = [run_output["probe"] for run_output in runs if run_output.get("probe", {}).get("ok")]
result = {
    "base": BASE,
    "runs": RUNS,
    "blockers": BLOCKERS,
    "blocker_tokens": BLOCKER_TOKENS,
    "context_target_tokens": CONTEXT_TOKENS,
    "probe_tokens": PROBE_TOKENS,
    "warm_routes": [run_output.get("warm", {}).get("route") for run_output in runs],
    "blocker_routes": [
        [run_output.get(f"blocker-{index}", {}).get("route") for index in range(BLOCKERS)]
        for run_output in runs
    ],
    "probe_routes": [probe.get("route") for probe in probes],
    "probe_cached_tokens": [probe.get("cached_tokens") for probe in probes],
    "probe_request_bytes": [probe.get("request_bytes") for probe in probes],
    "probe_ttft_ms_median": round(statistics.median(probe["ttft_ms"] for probe in probes), 1)
    if probes
    else None,
    "requests_failed": len(errors),
}
if errors:
    result["errors"] = errors
print(json.dumps(result, sort_keys=True))
raise SystemExit(0 if not errors else 1)
