#!/usr/bin/env python3
"""Deterministic GSM8K accuracy check against one OpenAI-compatible engine.

This is a numerics regression check, not a leaderboard score: it compares an
engine candidate with its baseline on the same questions, sampling, and prompt.
Use the public test split from openai/grade-school-math (1,319 lines, SHA-256
3730d312f6e3440559ace48831e51066acaca737f6eabec99bccb9e4b3c39d14).
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import pathlib
import re
import sys
import time
import urllib.request

INSTRUCTION = "Solve the problem. End your reply with: The answer is N"
ANSWER_RE = re.compile(r"answer is\s*\$?\s*(-?[\d,]*\.?\d+)", re.IGNORECASE)
NUMBER_RE = re.compile(r"-?[\d,]*\.?\d+")


def normalize(number: str) -> str:
    value = number.replace(",", "").rstrip(".")
    if "." in value:
        value = value.rstrip("0").rstrip(".")
    return value.lstrip("+") or "0"


def gold_answer(answer: str) -> str:
    return normalize(answer.rsplit("####", 1)[1].strip())


def extract_answer(text: str) -> str | None:
    matches = ANSWER_RE.findall(text or "")
    if matches:
        return normalize(matches[-1])
    numbers = NUMBER_RE.findall(text or "")
    return normalize(numbers[-1]) if numbers else None


def ask(base: str, model: str, question: str, args) -> dict:
    body = {
        "model": model,
        "temperature": 0,
        "seed": args.seed,
        "max_tokens": args.max_tokens,
        "messages": [{"role": "user", "content": f"{question}\n\n{INSTRUCTION}"}],
    }
    if args.reasoning_effort:
        body["reasoning_effort"] = args.reasoning_effort
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=args.timeout) as response:
        return json.load(response)


def run_one(index: int, item: dict, args) -> dict:
    gold = gold_answer(item["answer"])
    try:
        response = ask(args.base, args.model, item["question"], args)
    except Exception as error:  # noqa: BLE001 - recorded as a failed question
        return {"index": index, "gold": gold, "error": type(error).__name__}
    choice = response["choices"][0]
    predicted = extract_answer(choice["message"].get("content") or "")
    return {
        "index": index,
        "gold": gold,
        "predicted": predicted,
        "correct": predicted == gold,
        "finish_reason": choice.get("finish_reason"),
        "completion_tokens": response.get("usage", {}).get("completion_tokens"),
    }


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("model")
    parser.add_argument("--data", type=pathlib.Path, required=True)
    parser.add_argument("--offset", type=int, default=0)
    parser.add_argument("--limit", type=int, default=500)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--reasoning-effort", default="low")
    parser.add_argument("--max-tokens", type=int, default=1024)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args(argv)

    items = [json.loads(line) for line in args.data.read_text().splitlines()]
    selected = list(enumerate(items))[args.offset : args.offset + args.limit]
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
        results = list(pool.map(lambda pair: run_one(*pair, args), selected))
    wall = time.monotonic() - started

    with args.output.open("w") as handle:
        for result in results:
            handle.write(json.dumps(result, sort_keys=True) + "\n")
    errors = sum(1 for result in results if "error" in result)
    correct = sum(1 for result in results if result.get("correct"))
    truncated = sum(1 for result in results if result.get("finish_reason") == "length")
    summary = {
        "questions": len(results),
        "correct": correct,
        "accuracy": round(correct / len(results), 4) if results else None,
        "errors": errors,
        "truncated": truncated,
        "completion_tokens": sum(result.get("completion_tokens") or 0 for result in results),
        "wall_seconds": round(wall, 1),
    }
    print(json.dumps(summary, sort_keys=True))
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
