#!/usr/bin/env python3
"""Privacy-safe deterministic multimodal smoke for OpenAI-compatible APIs.

The runner creates a small PNG entirely in memory, asks one visual question,
and validates the answer without ever emitting response text or credentials.
Set BENCH_TOKEN (or VLLM_API_KEY). Example:

    python3 bench/multimodal_smoke.py http://127.0.0.1:8041 model \
      --engine-metrics http://127.0.0.1:8041/metrics \
      --require-reconciled-speculation
"""

import argparse
import base64
import binascii
import json
import os
import re
import struct
import sys
import time
import urllib.request
import zlib

from engine_metrics import fetch_speculative, speculative_delta


IMAGE_WIDTH = 96
IMAGE_HEIGHT = 96
SQUARE_MARGIN = 16
EXPECTED_ANSWER = "red"
QUESTION = (
    "What color is the large square in this image? Reply with exactly one "
    "lowercase color word and no punctuation."
)


def png_chunk(kind, payload):
    body = kind + payload
    return (
        struct.pack(">I", len(payload))
        + body
        + struct.pack(">I", binascii.crc32(body) & 0xFFFFFFFF)
    )


def deterministic_image():
    """Return stable PNG bytes for a red square on a white background."""
    rows = bytearray()
    for y in range(IMAGE_HEIGHT):
        rows.append(0)  # PNG filter: None
        for x in range(IMAGE_WIDTH):
            inside = (
                SQUARE_MARGIN <= x < IMAGE_WIDTH - SQUARE_MARGIN
                and SQUARE_MARGIN <= y < IMAGE_HEIGHT - SQUARE_MARGIN
            )
            rows.extend((220, 24, 24) if inside else (255, 255, 255))
    header = struct.pack(">IIBBBBB", IMAGE_WIDTH, IMAGE_HEIGHT, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + b"".join(
        (
            png_chunk(b"IHDR", header),
            png_chunk(b"IDAT", zlib.compress(bytes(rows), level=9)),
            png_chunk(b"IEND", b""),
        )
    )


def normalized_answer_words(content):
    return re.findall(r"[a-z]+", content.casefold())


def bounded_finish_reason(value):
    if value in {"stop", "length", "tool_calls", "content_filter"}:
        return value
    return "missing" if value is None else "other"


def safe_metric_snapshot(url):
    if not url:
        return None
    try:
        return fetch_speculative(url, timeout=30)
    except Exception:
        return None


def request_smoke(base, model, token, timeout, max_tokens):
    image = deterministic_image()
    image_url = "data:image/png;base64," + base64.b64encode(image).decode("ascii")
    body = {
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": QUESTION},
                    {
                        "type": "image_url",
                        "image_url": {"url": image_url, "detail": "low"},
                    },
                ],
            }
        ],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": True,
        "stream_options": {"include_usage": True},
        "chat_template_kwargs": {"enable_thinking": False},
    }
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers={
            "Authorization": "Bearer " + token,
            "Content-Type": "application/json",
        },
    )
    started = time.perf_counter()
    first_content = None
    content_fragments = []
    finish_reason = None
    usage = {}
    transport_ok = False
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
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
                    if choice.get("finish_reason") is not None:
                        finish_reason = choice.get("finish_reason")
                    delta = choice.get("delta") or {}
                    fragment = delta.get("content")
                    if isinstance(fragment, str) and fragment:
                        first_content = first_content or time.perf_counter()
                        content_fragments.append(fragment)
                if event.get("usage"):
                    usage = event["usage"]
        transport_ok = True
    except Exception:
        # Exception strings can contain URLs or response fragments. Keep only a
        # bounded structural failure label in the report.
        pass
    ended = time.perf_counter()
    content = "".join(content_fragments)
    details = usage.get("prompt_tokens_details") or {}
    prompt_tokens = usage.get("prompt_tokens")
    completion_tokens = usage.get("completion_tokens")
    total_tokens = usage.get("total_tokens")
    usage_ok = (
        isinstance(prompt_tokens, int)
        and prompt_tokens > 0
        and isinstance(completion_tokens, int)
        and completion_tokens > 0
    )
    answer_match = normalized_answer_words(content) == [EXPECTED_ANSWER]
    failures = []
    if not transport_ok:
        failures.append("transport_error")
    elif not content:
        failures.append("response_missing_content")
    elif not answer_match:
        failures.append("visual_answer_mismatch")
    if transport_ok and not usage_ok:
        failures.append("authoritative_usage_missing")
    return {
        "transport_ok": transport_ok,
        "answer_match": answer_match,
        "usage_ok": usage_ok,
        "finish_reason": bounded_finish_reason(finish_reason),
        "prompt_tokens": prompt_tokens,
        "cached_tokens": details.get("cached_tokens"),
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens,
        "ttft_ms": (
            round(1000 * (first_content - started), 3) if first_content else None
        ),
        "wall_ms": round(1000 * (ended - started), 3),
        "failures": failures,
        "image_bytes": len(image),
    }


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("model")
    parser.add_argument(
        "--engine-metrics",
        default=os.environ.get("METRICS_URL"),
        help="direct engine /metrics URL for native speculation reconciliation",
    )
    parser.add_argument(
        "--require-reconciled-speculation",
        action="store_true",
        default=os.environ.get("BENCH_REQUIRE_RECONCILED_SPECULATION") == "1",
    )
    parser.add_argument(
        "--spec-mode",
        choices=("enabled", "disabled"),
        default=os.environ.get("BENCH_SPEC_MODE", "enabled"),
    )
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--max-tokens", type=int, default=8)
    args = parser.parse_args(argv)
    if args.require_reconciled_speculation and not args.engine_metrics:
        parser.error("--require-reconciled-speculation requires --engine-metrics")
    if not 0 < args.timeout <= 900:
        parser.error("--timeout must be in (0, 900]")
    if not 1 <= args.max_tokens <= 32:
        parser.error("--max-tokens must be in [1, 32]")
    return args


def main(argv=None):
    args = parse_args(argv)
    token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
    if not token:
        raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")

    metrics_before = safe_metric_snapshot(args.engine_metrics)
    observation = request_smoke(
        args.base, args.model, token, args.timeout, args.max_tokens
    )
    metrics_after = safe_metric_snapshot(args.engine_metrics)
    completion_tokens = (
        observation["completion_tokens"]
        if isinstance(observation["completion_tokens"], int)
        else 0
    )
    speculation = speculative_delta(
        metrics_before,
        metrics_after,
        completion_tokens,
        1 if observation["transport_ok"] else 0,
        expected_enabled=args.spec_mode == "enabled",
    )
    if (
        args.require_reconciled_speculation
        and speculation.get("reconciled") is not True
    ):
        observation["failures"].append("speculation_not_reconciled")

    report = {
        "schema_version": 1,
        "type": "multimodal_smoke",
        "ok": not observation["failures"],
        "visual_answer_match": observation["answer_match"],
        "response_structure": {
            "transport_ok": observation["transport_ok"],
            "authoritative_usage_present": observation["usage_ok"],
            "finish_reason": observation["finish_reason"],
        },
        "image": {
            "format": "png",
            "width": IMAGE_WIDTH,
            "height": IMAGE_HEIGHT,
            "bytes": observation["image_bytes"],
        },
        "timing": {
            "ttft_ms": observation["ttft_ms"],
            "wall_ms": observation["wall_ms"],
        },
        "tokens": {
            "prompt": observation["prompt_tokens"],
            "cached_prompt": observation["cached_tokens"],
            "completion": observation["completion_tokens"],
            "total": observation["total_tokens"],
        },
        "speculation": speculation,
        "failures": observation["failures"],
    }
    print(json.dumps(report, sort_keys=True))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
