#!/usr/bin/env python3
"""Measure the latency and response cost of vLLM's native /tokenize API.

Usage: tokenize_bench.py BASE MODEL [runs]
Set BENCH_TOKEN (or VLLM_API_KEY). TARGETS is a comma-separated list of
approximate prompt-token counts (default: 200,4000,20000,80000).
"""

import json
import os
import statistics
import sys
import time
import urllib.request


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
RUNS = int(sys.argv[3]) if len(sys.argv) > 3 else 5
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
TARGETS = [int(item) for item in os.environ.get("TARGETS", "200,4000,20000,80000").split(",")]
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")

FILLER = (
    "The subsystem records each transaction in an append-only ledger and "
    "reconciles the balance against the upstream snapshot on every commit. "
)


def one(target):
    content = f"[tokenize-bench {target}]\n" + FILLER * max(1, round(target / 22))
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [{"role": "user", "content": content}],
            "add_generation_prompt": True,
        }
    ).encode()
    request = urllib.request.Request(
        BASE + "/tokenize",
        data=body,
        headers={
            "Authorization": "Bearer " + TOKEN,
            "Content-Type": "application/json",
        },
    )
    started = time.perf_counter()
    with urllib.request.urlopen(request, timeout=120) as response:
        payload = response.read()
    elapsed = time.perf_counter() - started
    result = json.loads(payload)
    return elapsed, result["count"], len(body), len(payload)


for target in TARGETS:
    one(target)
    samples = [one(target) for _ in range(RUNS)]
    print(
        json.dumps(
            {
                "target_tokens": target,
                "actual_tokens": int(statistics.median(item[1] for item in samples)),
                "runs": RUNS,
                "latency_ms_median": round(statistics.median(item[0] for item in samples) * 1000, 2),
                "latency_ms_max": round(max(item[0] for item in samples) * 1000, 2),
                "request_bytes": int(statistics.median(item[2] for item in samples)),
                "response_bytes": int(statistics.median(item[3] for item in samples)),
            },
            sort_keys=True,
        )
    )
