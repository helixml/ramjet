#!/usr/bin/env python3
"""Run one short concurrent low-reasoning cell without printing model output."""

from __future__ import annotations

import argparse
import json
import math
import statistics
import threading
import time
import urllib.request

from metrics import reconcile, snapshot


PROMPT = (
    "Write a complete Python function with type hints that implements a "
    "thread-safe TTL cache, followed by concise pytest tests for expiry and "
    "concurrent access. Output only code. Request salt: "
)


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[math.ceil(fraction * len(ordered)) - 1]


def one(base: str, model: str, max_tokens: int, index: int, output: dict) -> None:
    body = {
        "model": model,
        "messages": [{"role": "user", "content": PROMPT + f"glm53-{index:02d}"}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "chat_template_kwargs": {"reasoning_effort": "low"},
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    call = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer scout"},
    )
    started = time.perf_counter()
    first = last = None
    prompt_tokens = completion_tokens = cached_tokens = 0
    try:
        with urllib.request.urlopen(call, timeout=300) as response:
            for raw in response:
                line = raw.decode("utf-8", "ignore").strip()
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if not data or data == "[DONE]":
                    continue
                event = json.loads(data)
                choices = event.get("choices") or []
                if choices:
                    delta = choices[0].get("delta") or {}
                    if any(
                        delta.get(key)
                        for key in ("content", "reasoning", "reasoning_content")
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
        valid = (
            first is not None
            and last is not None
            and last > first
            and prompt_tokens > 0
            and completion_tokens > 1
        )
        output[index] = {
            "ok": valid,
            "prompt_tokens": prompt_tokens,
            "cached_tokens": cached_tokens,
            "completion_tokens": completion_tokens,
            "ttft_seconds": first - started if first else None,
            "decode_seconds": last - first if first and last and last > first else None,
            "wall_seconds": ended - started,
        }
    except Exception as error:
        output[index] = {"ok": False, "error": f"{type(error).__name__}: {error}"}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("base")
    parser.add_argument("--model", default="glm-5.3-flash")
    parser.add_argument("--metrics")
    parser.add_argument("--max-tokens", type=int, default=128)
    parser.add_argument("--concurrency", type=int, choices=range(1, 25), default=1)
    args = parser.parse_args()
    if not 16 <= args.max_tokens <= 256:
        parser.error("--max-tokens must be between 16 and 256")

    native_before = snapshot(args.metrics) if args.metrics else None
    output: dict[int, dict] = {}
    threads = [
        threading.Thread(
            target=one,
            args=(args.base, args.model, args.max_tokens, index, output),
        )
        for index in range(args.concurrency)
    ]
    started = time.perf_counter()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    batch_seconds = time.perf_counter() - started
    observations = [output[index] for index in sorted(output)]
    good = [item for item in observations if item.get("ok")]
    if len(good) != args.concurrency:
        print(json.dumps({"ok": False, "observations": observations}, sort_keys=True))
        return 1

    ttfts = [item["ttft_seconds"] for item in good]
    per_stream = [
        item["completion_tokens"] / item["decode_seconds"] for item in good
    ]
    completed = sum(item["completion_tokens"] for item in good)
    prompted = sum(item["prompt_tokens"] for item in good)
    cached = sum(item["cached_tokens"] for item in good)
    native_delta = None
    if native_before is not None:
        native_delta = reconcile(
            native_before,
            snapshot(args.metrics),
            {
                "requests": len(good),
                "prompt_tokens": prompted,
                "completion_tokens": completed,
            },
        )
    print(
        json.dumps(
            {
                "ok": True,
                "concurrency": args.concurrency,
                "max_tokens": args.max_tokens,
                "native_delta": native_delta,
                "requests": len(good),
                "prompt_tokens": prompted,
                "cached_tokens": cached,
                "completion_tokens": completed,
                "batch_seconds": round(batch_seconds, 4),
                "aggregate_completion_tok_s": round(completed / batch_seconds, 1),
                "per_stream_decode_tok_s_median": round(statistics.median(per_stream), 1),
                "ttft_ms_median": round(1000 * statistics.median(ttfts), 1),
                "ttft_ms_p95": round(1000 * percentile(ttfts, 0.95), 1),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
