#!/usr/bin/env python3
"""Measure cold-prefill interference with active decoder streams.

Usage: mixed_bench.py BASE MODEL [prefill_tokens] [decoders] [decode_tokens] [runs]

Set BENCH_TOKEN (or VLLM_API_KEY). Each measured run starts one cache-busted
long prefill and DECODERS short-prompt streams. MIXED_ORDER is `prefill-first`
(default) or `decode-first`; the latter waits until every decoder has emitted a
token before admitting the prefill. MIXED_LEAD_MS (default 50) is the delay
after the leading class is ready/started. The JSON result reports prefill TTFT
and decoder TTFT/throughput separately; input and output tokens are deliberately
not combined into one misleading tokens/s number.
"""

import json
import math
import os
import statistics
import sys
import threading
import time
import urllib.request


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
PREFILL_TOKENS = int(sys.argv[3]) if len(sys.argv) > 3 else 32000
DECODERS = int(sys.argv[4]) if len(sys.argv) > 4 else 4
DECODE_TOKENS = int(sys.argv[5]) if len(sys.argv) > 5 else 512
RUNS = int(sys.argv[6]) if len(sys.argv) > 6 else 3
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
LEAD_SECONDS = float(os.environ.get("MIXED_LEAD_MS", "50")) / 1000
ORDER = os.environ.get("MIXED_ORDER", "prefill-first")
SALT = os.environ.get("SALT") or str(time.time_ns())
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
if min(PREFILL_TOKENS, DECODERS, DECODE_TOKENS, RUNS) <= 0:
    raise SystemExit("prefill_tokens, decoders, decode_tokens, and runs must be positive")
if ORDER not in ("prefill-first", "decode-first"):
    raise SystemExit("MIXED_ORDER must be prefill-first or decode-first")

CODE_PROMPT = (
    "Write a complete, production-quality Python module that implements a "
    "thread-safe LRU cache with TTL expiry. Include the full class with type "
    "hints, mapping methods, a background sweeper thread, explicit locking, "
    "statistics, and pytest tests. Output only code."
)
FILLER = (
    "The subsystem records each transaction in an append-only ledger and "
    "reconciles the balance against the upstream snapshot on every commit. "
)


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return None
    return values[math.ceil(fraction * len(values)) - 1]


def generated(delta):
    return bool(
        delta.get("content")
        or delta.get("reasoning")
        or delta.get("reasoning_content")
        or delta.get("tool_calls")
    )


def stream_request(kind, prompt, max_tokens, output, key, first_event=None):
    body = {
        "model": MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
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
            route = response.headers.get("X-Mini-Dynamo-Upstream")
            for raw in response:
                line = raw.decode("utf-8", "ignore").strip()
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if not payload or payload == "[DONE]":
                    continue
                event = json.loads(payload)
                choices = event.get("choices") or []
                if choices and generated(choices[0].get("delta") or {}):
                    now = time.perf_counter()
                    first = first or now
                    last = now
                    if first_event is not None:
                        first_event.set()
                usage = event.get("usage") or {}
                prompt_tokens = usage.get("prompt_tokens", prompt_tokens)
                completion_tokens = usage.get("completion_tokens", completion_tokens)
                details = usage.get("prompt_tokens_details") or {}
                cached_tokens = details.get("cached_tokens", cached_tokens)
        ended = time.perf_counter()
        output[key] = {
            "ok": True,
            "kind": kind,
            "started": started,
            "ended": ended,
            "ttft": first - started if first else None,
            "decode_seconds": last - first if first and last and last > first else None,
            "prompt_tokens": prompt_tokens,
            "cached_tokens": cached_tokens,
            "completion_tokens": completion_tokens,
            "route": route,
        }
    except Exception as error:
        output[key] = {
            "ok": False,
            "kind": kind,
            "error": f"{type(error).__name__}: {error}",
        }
    finally:
        if first_event is not None:
            first_event.set()


def prefill_prompt(namespace, target_tokens=PREFILL_TOKENS):
    return (
        f"[mixed trace {SALT}/{namespace}] Summarize this ledger in one sentence.\n\n"
        + FILLER * max(1, round(target_tokens / 22))
    )


def run_mixed(run):
    output = {}
    first_events = [threading.Event() for _ in range(DECODERS)]
    prefill = threading.Thread(
        target=stream_request,
        args=("prefill", prefill_prompt(f"run-{run}"), 32, output, "prefill"),
    )
    decoders = [
        threading.Thread(
            target=stream_request,
            args=(
                "decode",
                f"[mixed decoder {SALT}/{run}/{index}] {CODE_PROMPT}",
                DECODE_TOKENS,
                output,
                f"decode-{index}",
                first_events[index],
            ),
        )
        for index in range(DECODERS)
    ]
    if ORDER == "prefill-first":
        prefill.start()
        time.sleep(LEAD_SECONDS)
        for decoder in decoders:
            decoder.start()
    else:
        for decoder in decoders:
            decoder.start()
        deadline = time.monotonic() + 120
        for event in first_events:
            event.wait(max(0, deadline - time.monotonic()))
        time.sleep(LEAD_SECONDS)
        prefill.start()
    prefill.join()
    for decoder in decoders:
        decoder.join()
    return output


# Prime request handling and decoder kernels without warming a measured prefix.
warmup = {}
stream_request("warmup", "Return the number 1.", 16, warmup, "warmup")
if not warmup.get("warmup", {}).get("ok"):
    raise SystemExit("warmup failed: " + json.dumps(warmup, sort_keys=True))

runs = [run_mixed(run) for run in range(RUNS)]
requests = [item for run in runs for item in run.values()]
errors = [item["error"] for item in requests if not item.get("ok")]
prefills = [run["prefill"] for run in runs if run.get("prefill", {}).get("ok")]
decodes = [
    item
    for run in runs
    for key, item in run.items()
    if key.startswith("decode-") and item.get("ok")
]
prefill_ttfts = [item["ttft"] for item in prefills if item.get("ttft") is not None]
decode_ttfts = [item["ttft"] for item in decodes if item.get("ttft") is not None]
decode_rates = [
    item["completion_tokens"] / item["decode_seconds"]
    for item in decodes
    if item.get("decode_seconds") and item.get("completion_tokens")
]
aggregate_rates = []
for run in runs:
    good = [item for key, item in run.items() if key.startswith("decode-") and item.get("ok")]
    if good:
        wall = max(item["ended"] for item in good) - min(item["started"] for item in good)
        aggregate_rates.append(sum(item["completion_tokens"] for item in good) / wall)

result = {
    "label": os.environ.get("SWEEP_LABEL", "mixed"),
    "base": BASE,
    "runs": RUNS,
    "prefill_target_tokens": PREFILL_TOKENS,
    "prefill_prompt_tokens": prefills[0].get("prompt_tokens") if prefills else None,
    "prefill_cached_tokens_max": max((item["cached_tokens"] for item in prefills), default=None),
    "prefill_ttft_ms_median": round(statistics.median(prefill_ttfts) * 1000, 1) if prefill_ttfts else None,
    "prefill_ttft_ms_p95": round(percentile(prefill_ttfts, 0.95) * 1000, 1) if prefill_ttfts else None,
    "decoders": DECODERS,
    "decode_max_tokens": DECODE_TOKENS,
    "decoder_requests_ok": len(decodes),
    "decoder_requests_failed": DECODERS * RUNS - len(decodes),
    "decoder_ttft_ms_median": round(statistics.median(decode_ttfts) * 1000, 1) if decode_ttfts else None,
    "decoder_ttft_ms_p95": round(percentile(decode_ttfts, 0.95) * 1000, 1) if decode_ttfts else None,
    "decoder_per_stream_tok_s_median": round(statistics.median(decode_rates), 1) if decode_rates else None,
    "decoder_aggregate_tok_s_median": round(statistics.median(aggregate_rates), 1) if aggregate_rates else None,
    "lead_ms": round(LEAD_SECONDS * 1000, 1),
    "order": ORDER,
    "prefill_route_counts": {
        route: sum(1 for item in prefills if item.get("route") == route)
        for route in sorted({item.get("route") for item in prefills if item.get("route") is not None})
    },
    "decoder_route_counts": {
        route: sum(1 for item in decodes if item.get("route") == route)
        for route in sorted({item.get("route") for item in decodes if item.get("route") is not None})
    },
}
if errors:
    result["errors"] = errors
print(json.dumps(result, sort_keys=True))
raise SystemExit(0 if not errors else 1)
