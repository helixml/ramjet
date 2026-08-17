#!/usr/bin/env python3
"""Create a cache state invisible to request-derived approximate routing.

Each attempt warms a fresh long prompt directly on one engine, then sends the
same request through mini-dynamo. The script stops when the proxy selects a
different engine. It never prints the prompt or token IDs.

Usage: forced_exact_miss.py PROXY_BASE WARM_ENGINE_BASE MODEL [ATTEMPTS]
Set BENCH_TOKEN (or VLLM_API_KEY).
Set BENCH_SESSION_ID to exercise an exact-placement canary cohort; the header
is sent only through the proxy, never directly to an engine.
Set BENCH_EXPECT_ROUTE to stop when that opaque proxy route is selected;
without it, the historical shadow-mode behavior stops on any route other than
zero.
"""

import json
import os
import sys
import time
import urllib.request


if len(sys.argv) not in (4, 5):
    raise SystemExit(
        "usage: forced_exact_miss.py PROXY_BASE WARM_ENGINE_BASE MODEL [ATTEMPTS]"
    )

PROXY = sys.argv[1].rstrip("/")
WARM_ENGINE = sys.argv[2].rstrip("/")
MODEL = sys.argv[3]
ATTEMPTS = int(sys.argv[4]) if len(sys.argv) == 5 else 4
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
SESSION_ID = os.environ.get("BENCH_SESSION_ID")
EXPECTED_ROUTE = os.environ.get("BENCH_EXPECT_ROUTE")
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")

FILLER = (
    "The scheduler verifies every transition, preserves prefix identity, "
    "and records bounded operational evidence for later replay. "
)


def chat(base, payload, session_id=None):
    headers = {
        "Authorization": "Bearer " + TOKEN,
        "Content-Type": "application/json",
    }
    if session_id:
        headers["X-Session-ID"] = session_id
    request = urllib.request.Request(
        base + "/v1/chat/completions",
        data=payload,
        headers=headers,
    )
    with urllib.request.urlopen(request, timeout=300) as response:
        route = response.headers.get("X-Ramjet-Upstream")
        body = json.load(response)
        usage = body.get("usage") or {}
        cached = (usage.get("prompt_tokens_details") or {}).get("cached_tokens", 0)
        return response.status, route, cached


results = []
for attempt in range(ATTEMPTS):
    salt = f"{time.time_ns()}-{attempt}"
    body = {
        "model": MODEL,
        "messages": [
            {
                "role": "system",
                "content": f"[forced-exact-miss {salt}] " + FILLER * 1800,
            },
            {"role": "user", "content": "Return one word."},
        ],
        "max_tokens": 1,
        "temperature": 0,
    }
    payload = json.dumps(body, separators=(",", ":")).encode()
    warm_status, _, warm_cached = chat(WARM_ENGINE, payload)
    time.sleep(0.25)
    proxy_status, route, proxy_cached = chat(PROXY, payload, SESSION_ID)
    result = {
        "attempt": attempt + 1,
        "request_bytes": len(payload),
        "warm_status": warm_status,
        "warm_cached_tokens": warm_cached,
        "proxy_status": proxy_status,
        "proxy_route": route,
        "proxy_cached_tokens": proxy_cached,
    }
    results.append(result)
    matched = (
        route == EXPECTED_ROUTE
        if EXPECTED_ROUTE is not None
        else route not in (None, "0")
    )
    if matched:
        break

print(json.dumps({"attempts": results}, sort_keys=True))
matched = (
    results[-1]["proxy_route"] == EXPECTED_ROUTE
    if EXPECTED_ROUTE is not None
    else results[-1]["proxy_route"] not in (None, "0")
)
raise SystemExit(0 if matched else 1)
