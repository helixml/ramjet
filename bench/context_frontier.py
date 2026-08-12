#!/usr/bin/env python3
"""Measure cold prefill, warm TTFT, decode, and DSpark acceptance by context.

Usage: context_frontier.py BASE MODEL [runs]

Set BENCH_TOKEN (or VLLM_API_KEY). CONTEXT_TOKENS is a comma-separated target
list (default: 2000,8000,32000,64000,128000), MAX_OUTPUT_TOKENS defaults to
256. METRICS_URLS optionally lists comma-separated engine /metrics endpoints;
METRICS_URL remains a supported single-endpoint alias.

Run this sequentially, preferably through the production router so its load
reservation protects unrelated traffic. Every cold prompt diverges at block
zero; every warm phase primes one distinct prompt before measurement.
The reported prefill rate is uncached prompt tokens / TTFT, so it includes
request/scheduler and first-token overhead and is intentionally labelled an
effective rate rather than kernel throughput.
"""

import json
import math
import os
import re
import statistics
import sys
import time
import urllib.request


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
RUNS = int(sys.argv[3]) if len(sys.argv) > 3 else 3
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
TARGETS = [
    int(value.strip())
    for value in os.environ.get(
        "CONTEXT_TOKENS", "2000,8000,32000,64000,128000"
    ).split(",")
    if value.strip()
]
MAX_OUTPUT_TOKENS = int(os.environ.get("MAX_OUTPUT_TOKENS", "256"))
METRICS_URLS = [
    value.strip()
    for value in os.environ.get("METRICS_URLS", os.environ.get("METRICS_URL", "")).split(",")
    if value.strip()
]
SALT = os.environ.get("SALT") or str(time.time_ns())
TIMEOUT = float(os.environ.get("BENCH_TIMEOUT", "900"))

if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
if RUNS <= 0 or MAX_OUTPUT_TOKENS <= 1 or not TARGETS or min(TARGETS) <= 0:
    raise SystemExit("runs and contexts must be positive; MAX_OUTPUT_TOKENS must exceed 1")

FILLER = (
    "The subsystem records each transaction in an append-only ledger and "
    "reconciles the balance against the upstream snapshot on every commit. "
)
METRIC_NAMES = {
    "drafts": "vllm:spec_decode_num_drafts_total",
    "draft_tokens": "vllm:spec_decode_num_draft_tokens_total",
    "accepted_tokens": "vllm:spec_decode_num_accepted_tokens_total",
}


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return None
    return values[math.ceil(fraction * len(values)) - 1]


def median(values, digits=1):
    values = [value for value in values if value is not None]
    return round(statistics.median(values), digits) if values else None


def generated(delta):
    return bool(
        delta.get("content")
        or delta.get("reasoning")
        or delta.get("reasoning_content")
        or delta.get("tool_calls")
    )


def build_prompt(target_tokens, namespace):
    # The target is approximate; the engine's usage block is authoritative.
    return (
        f"[context frontier {SALT}/{namespace}] Summarize this ledger in one sentence.\n\n"
        + FILLER * max(1, round(target_tokens / 22))
    )


def request(prompt, max_tokens=MAX_OUTPUT_TOKENS):
    body = {
        "model": MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    req = urllib.request.Request(
        BASE + "/chat/completions",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": "Bearer " + TOKEN,
            "Content-Type": "application/json",
        },
    )
    started = time.perf_counter()
    first = last = None
    prompt_tokens = cached_tokens = completion_tokens = 0
    finish_reason = None
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as response:
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
                if choices:
                    choice = choices[0]
                    finish_reason = choice.get("finish_reason") or finish_reason
                    if generated(choice.get("delta") or {}):
                        now = time.perf_counter()
                        first = first or now
                        last = now
                usage = event.get("usage") or {}
                prompt_tokens = usage.get("prompt_tokens", prompt_tokens)
                completion_tokens = usage.get("completion_tokens", completion_tokens)
                details = usage.get("prompt_tokens_details") or {}
                cached_tokens = details.get("cached_tokens", cached_tokens)
        ended = time.perf_counter()
        if first is None or prompt_tokens <= 0 or completion_tokens <= 0:
            raise RuntimeError("response omitted generated tokens or final usage")
        decode_tokens = max(0, completion_tokens - 1)
        decode_seconds = last - first if last and last > first else None
        return {
            "ok": True,
            "prompt_tokens": prompt_tokens,
            "cached_tokens": cached_tokens or 0,
            "completion_tokens": completion_tokens,
            "finish_reason": finish_reason,
            "route": route,
            "ttft_ms": (first - started) * 1000,
            "wall_ms": (ended - started) * 1000,
            "decode_tok_s": (
                decode_tokens / decode_seconds if decode_seconds and decode_tokens else None
            ),
        }
    except Exception as error:
        return {"ok": False, "error": f"{type(error).__name__}: {error}"}


def metric_snapshot():
    if not METRICS_URLS:
        return None
    try:
        snapshot = {key: 0.0 for key in METRIC_NAMES}
        for metrics_url in METRICS_URLS:
            with urllib.request.urlopen(metrics_url, timeout=30) as response:
                body = response.read().decode("utf-8", "replace")
            for key, name in METRIC_NAMES.items():
                match = re.search(
                    r"^" + re.escape(name) + r"\{[^\n]*\}\s+([0-9.eE+-]+)$",
                    body,
                    re.MULTILINE,
                )
                if not match:
                    return None
                snapshot[key] += float(match.group(1))
        return snapshot
    except Exception:
        return None


def acceptance(before, after):
    if before is None or after is None:
        return None
    delta = {key: after[key] - before[key] for key in before}
    if min(delta.values()) < 0 or delta["draft_tokens"] <= 0 or delta["drafts"] <= 0:
        return None
    return {
        "draft_steps": int(delta["drafts"]),
        "draft_tokens": int(delta["draft_tokens"]),
        "accepted_tokens": int(delta["accepted_tokens"]),
        "draft_acceptance_pct": round(100 * delta["accepted_tokens"] / delta["draft_tokens"], 1),
        "mean_accepted_per_draft": round(delta["accepted_tokens"] / delta["drafts"], 2),
        "effective_tokens_per_step": round(1 + delta["accepted_tokens"] / delta["drafts"], 2),
    }


def summarize(samples):
    good = [sample for sample in samples if sample.get("ok")]
    errors = [sample["error"] for sample in samples if not sample.get("ok")]
    uncached_rates = [
        1000 * (sample["prompt_tokens"] - sample["cached_tokens"]) / sample["ttft_ms"]
        for sample in good
    ]
    result = {
        "requests_ok": len(good),
        "requests_failed": len(samples) - len(good),
        "prompt_tokens_median": int(statistics.median([s["prompt_tokens"] for s in good])) if good else None,
        "cached_tokens_median": int(statistics.median([s["cached_tokens"] for s in good])) if good else None,
        "ttft_ms_median": median([s["ttft_ms"] for s in good]),
        "ttft_ms_p95": round(percentile([s["ttft_ms"] for s in good], 0.95), 1) if good else None,
        "effective_uncached_prefill_tok_s_median": median(uncached_rates),
        "decode_tok_s_median": median([s["decode_tok_s"] for s in good]),
        "completion_tokens_median": int(statistics.median([s["completion_tokens"] for s in good])) if good else None,
        "finish_reasons": sorted({s["finish_reason"] for s in good if s["finish_reason"]}),
        "route_counts": {
            route: sum(1 for sample in good if sample.get("route") == route)
            for route in sorted({sample.get("route") for sample in good if sample.get("route") is not None})
        },
    }
    if errors:
        result["errors"] = errors
    return result


print(json.dumps({
    "event": "start",
    "base": BASE,
    "model": MODEL,
    "runs": RUNS,
    "targets": TARGETS,
    "max_output_tokens": MAX_OUTPUT_TOKENS,
    "metrics_endpoints": len(METRICS_URLS),
    "salt": SALT,
}, sort_keys=True), flush=True)

# Prime request handling and decoder graphs without warming a measured prefix.
warmup = request("Return the integer 1 and nothing else.", 16)
if not warmup.get("ok"):
    raise SystemExit("warmup failed: " + json.dumps(warmup, sort_keys=True))

failed = False
for target in TARGETS:
    cold_before = metric_snapshot()
    cold = [request(build_prompt(target, f"cold-{target}-{run}")) for run in range(RUNS)]
    cold_acceptance = acceptance(cold_before, metric_snapshot())

    warm_prompt = build_prompt(target, f"warm-{target}")
    prime = request(warm_prompt, 16)
    warm_before = metric_snapshot()
    warm = [request(warm_prompt) for _ in range(RUNS)] if prime.get("ok") else []
    warm_acceptance = acceptance(warm_before, metric_snapshot())

    result = {
        "event": "context",
        "target_tokens": target,
        "cold": summarize(cold),
        "warm": summarize(warm),
        "prime_ok": prime.get("ok", False),
        "cold_dspark": cold_acceptance,
        "warm_dspark": warm_acceptance,
    }
    if not prime.get("ok"):
        result["prime_error"] = prime.get("error")
    print(json.dumps(result, sort_keys=True), flush=True)
    if result["cold"]["requests_failed"] or result["warm"]["requests_failed"] or not prime.get("ok"):
        failed = True

raise SystemExit(1 if failed else 0)
