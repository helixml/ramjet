#!/usr/bin/env python3
"""Warm/cache-busted prefill sweep with fresh namespaces.

Usage: prefill_sweep.py BASE MODEL [runs]
Set BENCH_TOKEN (or VLLM_API_KEY) and optionally SALT. The cache-busted column,
not prompt_tokens/TTFT on a warm prefix, is the useful prefill measurement.
"""

import json
import os
import statistics
import sys
import time
import urllib.request


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
RUNS = int(sys.argv[3]) if len(sys.argv) > 3 else 3
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
SALT = os.environ.get("SALT") or str(time.time_ns())
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")

FILLER = (
    "The subsystem records each transaction in an append-only ledger and "
    "reconciles the balance against the upstream snapshot on every commit. "
)


def one(prompt):
    body = {
        "model": MODEL,
        "stream": True,
        "temperature": 0,
        "max_tokens": 32,
        "stream_options": {"include_usage": True},
        "messages": [{"role": "user", "content": prompt}],
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
    ttft = prompt_tokens = cached_tokens = None
    with urllib.request.urlopen(request, timeout=900) as response:
        for raw in response:
            line = raw.decode("utf-8", "ignore").strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload or payload == "[DONE]":
                continue
            event = json.loads(payload)
            usage = event.get("usage") or {}
            prompt_tokens = usage.get("prompt_tokens", prompt_tokens)
            details = usage.get("prompt_tokens_details") or {}
            cached_tokens = details.get("cached_tokens", cached_tokens)
            choices = event.get("choices") or []
            if ttft is None and choices:
                delta = choices[0].get("delta") or {}
                if (
                    delta.get("content")
                    or delta.get("reasoning")
                    or delta.get("reasoning_content")
                    or delta.get("tool_calls")
                ):
                    ttft = time.perf_counter() - started
    if ttft is None or prompt_tokens is None:
        raise RuntimeError("response contained no generated token or usage block")
    return ttft, prompt_tokens, cached_tokens or 0


def build(target_tokens, namespace):
    # Leading uniqueness forces divergence at block zero.
    return (
        f"[trace {SALT}/{namespace}] Summarize this log in one sentence.\n\n"
        + FILLER * max(1, round(target_tokens / 22))
    )


print(f"salt={SALT} runs={RUNS}")
print(f"{'prompt':>9} {'warm_ttft':>11} {'warm_tok/s':>12} {'warm_cached':>12} | "
      f"{'cold_ttft':>11} {'cold_tok/s':>12} {'cold_cached':>12}")
for target in (250, 750, 2000, 8000, 32000):
    warm_prompt = build(target, f"warm-{target}")
    one(warm_prompt)
    warm = [one(warm_prompt) for _ in range(RUNS)]
    cold = [one(build(target, f"cold-{target}-{run}")) for run in range(RUNS)]
    prompt_tokens = int(statistics.median(item[1] for item in cold))
    warm_ttft = statistics.median(item[0] for item in warm)
    cold_ttft = statistics.median(item[0] for item in cold)
    warm_cached = int(statistics.median(item[2] for item in warm))
    cold_cached = int(statistics.median(item[2] for item in cold))
    print(
        f"{prompt_tokens:>9} {warm_ttft * 1000:>9.1f}ms "
        f"{prompt_tokens / warm_ttft:>12.1f} {warm_cached:>12} | "
        f"{cold_ttft * 1000:>9.1f}ms {prompt_tokens / cold_ttft:>12.1f} "
        f"{cold_cached:>12}"
    )
