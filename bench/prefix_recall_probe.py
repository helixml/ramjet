#!/usr/bin/env python3
"""Check that reused (device- or host-restored) prefixes still carry their content.

Warms N salted synthetic sessions, each opening with a random 8-digit code,
then asks every session for its code in a follow-up turn whose prefix is the
warmed prompt. Reports cached tokens, latency, and whether the code came back
exactly. Size N above the device cache to exercise the HiCache host tier.
"""

from __future__ import annotations

import argparse
import json
import random
import sys
import time
import urllib.request

from prefix_eviction_probe import filler


def chat(base: str, model: str, messages: list, max_tokens: int, timeout: int) -> dict:
    body = {
        "model": model,
        "temperature": 0,
        "max_tokens": max_tokens,
        "reasoning_effort": "low",
        "messages": messages,
    }
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=timeout) as response:
        data = json.load(response)
    usage = data["usage"]
    return {
        "cached": (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0,
        "prompt": usage["prompt_tokens"],
        "seconds": round(time.monotonic() - started, 2),
        "text": data["choices"][0]["message"].get("content") or "",
    }


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("model")
    parser.add_argument("--salt", required=True)
    parser.add_argument("--sessions", type=int, default=14)
    parser.add_argument("--session-tokens", type=int, default=20000)
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args(argv)

    sessions = []
    for index in range(args.sessions):
        code = str(random.Random(f"{args.salt}-{index}").randrange(10**7, 10**8))
        text = (
            f"SECRET CODE FOR THIS DOCUMENT: {code}.\n"
            + filler(f"{args.salt}-r{index}", args.session_tokens)
            + "\nReply OK."
        )
        sessions.append((code, text))
    for _, text in sessions:
        chat(args.base, args.model, [{"role": "user", "content": text}], 1, args.timeout)
    results = []
    for code, text in sessions:
        reply = chat(
            args.base,
            args.model,
            [
                {"role": "user", "content": text},
                {"role": "assistant", "content": "OK"},
                {
                    "role": "user",
                    "content": "What is the secret code stated at the start of the document? "
                    "Reply with only the number.",
                },
            ],
            400,
            args.timeout,
        )
        results.append(
            {
                "cached": reply["cached"],
                "prompt": reply["prompt"],
                "seconds": reply["seconds"],
                "exact": code in reply["text"],
            }
        )
    print(
        json.dumps(
            {
                "sessions": args.sessions,
                "hit_ratio": round(
                    sum(row["cached"] for row in results) / sum(row["prompt"] for row in results), 4
                ),
                "recalled_exact": sum(row["exact"] for row in results),
                "results": results,
            }
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
