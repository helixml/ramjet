#!/usr/bin/env python3
"""Content-free end-to-end reasoning A/B for a pinned question subset."""

import argparse
import hashlib
import json
import os
import pathlib
import re
import statistics
import sys
import time
import urllib.request


def exchange(base, model, token, prompt, max_tokens, timeout):
    body = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "reasoning_effort": "xhigh",
        "temperature": 0,
        "top_p": 1,
        "seed": 7,
        "max_tokens": max_tokens,
        "stream": False,
    }
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    start = time.monotonic()
    with urllib.request.urlopen(request, timeout=timeout) as response:
        result = json.load(response)
    return result, time.monotonic() - start


def prompt_for(case):
    if "options" not in case:
        return (
            "Solve the following mathematics problem. Think carefully, then "
            "end with exactly 'Final answer: <integer>'.\n\n"
            f"{case['question']}"
        )
    options = "\n".join(
        f"{chr(65 + index)}. {option}"
        for index, option in enumerate(case["options"])
    )
    return (
        "Answer this multiple-choice question. Think carefully, then end "
        "with exactly 'Final answer: <letter>'.\n\n"
        f"{case['question']}\n\n{options}"
    )


def answer_from(content, numeric):
    if numeric:
        matches = re.findall(r"Final answer:\s*\\?\(?([0-9]{1,3})\)?\b", content, re.I)
        return int(matches[-1]) if matches else None
    matches = re.findall(r"Final answer:\s*\(?([A-J])\)?\b", content, re.I)
    return matches[-1].upper() if matches else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("model")
    parser.add_argument("cases", type=pathlib.Path)
    parser.add_argument("--label", required=True)
    parser.add_argument("--limit", type=int, default=4)
    parser.add_argument("--max-tokens", type=int, default=16000)
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args()
    if not 1 <= args.limit <= 4 or not 1 <= args.max_tokens <= 32768:
        parser.error("limit or max-tokens is outside the reviewed bound")
    token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
    if not token:
        parser.error("BENCH_TOKEN or VLLM_API_KEY is required")
    raw = args.cases.read_bytes()
    corpus = json.loads(raw)
    rows = corpus["cases"][: args.limit]
    if len(rows) != args.limit:
        parser.error("insufficient cases")
    prefix = {
        "label": args.label,
        "dataset": corpus["dataset"],
        "dataset_revision": corpus["revision"],
        "cases_sha256": hashlib.sha256(raw).hexdigest(),
        "effort": "xhigh",
        "temperature": 0,
        "seed": 7,
        "max_tokens": args.max_tokens,
    }
    records = []
    for case in rows:
        result, wall = exchange(
            args.base, args.model, token, prompt_for(case),
            args.max_tokens, args.timeout,
        )
        choice = result["choices"][0]
        usage = result["usage"]
        message = choice["message"]
        content = message.get("content") or ""
        details = usage.get("completion_tokens_details") or {}
        answer = answer_from(content, "options" not in case)
        record = {
            "type": "case",
            **prefix,
            "case_id": case["question_id"],
            "category": case["category"],
            "answer": answer,
            "correct": answer == case["answer"],
            "finish_reason": choice.get("finish_reason"),
            "wall_seconds": round(wall, 3),
            "prompt_tokens": usage.get("prompt_tokens"),
            "completion_tokens": usage.get("completion_tokens"),
            "reasoning_tokens": details.get("reasoning_tokens"),
            "final_content_chars": len(content),
        }
        records.append(record)
        print(json.dumps(record, sort_keys=True), flush=True)
    complete = [r for r in records if r["finish_reason"] == "stop"]
    summary = {
        "type": "summary",
        **prefix,
        "requests": len(records),
        "completed": len(complete),
        "correct": sum(r["correct"] for r in records),
        "completion_tokens_total": sum(r["completion_tokens"] for r in records),
        "reasoning_tokens_total": (
            sum(r["reasoning_tokens"] for r in records)
            if all(r["reasoning_tokens"] is not None for r in records)
            else None
        ),
        "wall_seconds_total": round(sum(r["wall_seconds"] for r in records), 3),
        "wall_seconds_median": round(
            statistics.median(r["wall_seconds"] for r in records), 3
        ),
    }
    print(json.dumps(summary, sort_keys=True), flush=True)
    # A length-capped answer is a measured task outcome, not a harness failure.
    return 0


if __name__ == "__main__":
    sys.exit(main())
