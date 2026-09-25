#!/usr/bin/env python3
"""Measure whether one very long prompt evicts other sessions' cached prefixes.

Warms N distinct synthetic session prefixes, sends one long prompt, then
re-sends every session prefix and records the engine-reported cached tokens
(`usage.prompt_tokens_details.cached_tokens`, SGLang `--enable-cache-report`).
Every request uses max_tokens=1, so the cost is prefill only. Prompts are
salted synthetic filler: nothing here carries user data.
"""

from __future__ import annotations

import argparse
import json
import random
import sys
import time
import urllib.request

WORDS = (
    "amber basalt cedar delta ember fjord garnet harbor indigo juniper kelp "
    "lagoon marble nectar onyx pollen quartz river sierra tundra umber valley "
    "willow xenon yarrow zephyr"
).split()


def filler(salt: str, approx_tokens: int) -> str:
    rng = random.Random(salt)
    # Each word+number costs about 3.4 GLM-5.3 tokens (measured 3,378 tokens
    # for 1,000 words); the summary reports the engine's actual prompt tokens.
    words = [f"{rng.choice(WORDS)}{rng.randrange(1000)}" for _ in range(approx_tokens * 10 // 34)]
    return f"[{salt}] " + " ".join(words)


def complete(base: str, model: str, text: str, timeout: int) -> dict:
    body = {
        "model": model,
        "temperature": 0,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": text}],
    }
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=timeout) as response:
        usage = json.load(response)["usage"]
    details = usage.get("prompt_tokens_details") or {}
    return {
        "prompt_tokens": usage["prompt_tokens"],
        "cached_tokens": details.get("cached_tokens") or 0,
        "seconds": round(time.monotonic() - started, 2),
    }


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("model")
    parser.add_argument("--salt", required=True)
    parser.add_argument("--sessions", type=int, default=6)
    parser.add_argument("--session-tokens", type=int, default=20000)
    parser.add_argument("--long-tokens", type=int, default=310000)
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args(argv)

    sessions = [
        filler(f"{args.salt}-s{index}", args.session_tokens) for index in range(args.sessions)
    ]
    report = {"warm": [], "rewarm": [], "after_long": []}
    for text in sessions:
        report["warm"].append(complete(args.base, args.model, text, args.timeout))
    for text in sessions:
        report["rewarm"].append(complete(args.base, args.model, text, args.timeout))
    report["long"] = complete(
        args.base, args.model, filler(f"{args.salt}-long", args.long_tokens), args.timeout
    )
    for text in sessions:
        report["after_long"].append(complete(args.base, args.model, text, args.timeout))

    def hit_ratio(rows):
        prompt = sum(row["prompt_tokens"] for row in rows)
        return round(sum(row["cached_tokens"] for row in rows) / prompt, 4) if prompt else None

    report["summary"] = {
        "rewarm_hit_ratio": hit_ratio(report["rewarm"]),
        "after_long_hit_ratio": hit_ratio(report["after_long"]),
        "sessions_surviving": sum(
            1 for row in report["after_long"] if row["cached_tokens"] >= row["prompt_tokens"] // 2
        ),
        "sessions": args.sessions,
        "long_prompt_tokens": report["long"]["prompt_tokens"],
    }
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
