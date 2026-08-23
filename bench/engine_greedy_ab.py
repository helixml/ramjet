#!/usr/bin/env python3
"""Greedy A/B two engines on identical prompts to detect a numerics change.

Built for the `--mamba-ssm-dtype=bfloat16` candidate (2026-08-23), where the
question is not "is it faster" but "does halving the linear-attention state
dtype change what the model says". Applies to any candidate that alters
numerics without altering weights: KV dtype, state dtype, attention backend,
GEMM runner.

Sends the same deterministic prompt to both engines at temperature 0 with
thinking disabled, then reports whether the two completions were byte
identical and whether each contained the expected answer. Completion text is
never printed — only the comparison facts — so this is safe to run against a
production-adjacent engine and paste into a journal.

This is a signal, not a gate. With one sample per prompt it cannot separate a
real quality regression from sampling noise on a borderline token. A candidate
that diverges here needs the full agent correctness matrix before promotion;
a candidate that matches on every prompt has still only been shown to match on
this corpus.

Usage:
  engine_greedy_ab.py http://127.0.0.1:8037 http://127.0.0.1:8030 \
      --a-name candidate --b-name baseline --model qwen3.8-27b

Reads the bearer from BENCH_TOKEN. For a direct engine that is the engine's
own --api-key (deployment .env VLLM_API_KEY), not the Caddy bearer.
"""

import argparse
import hashlib
import json
import os
import sys
import urllib.request

# Each case is (prompt, substring that must appear in a correct answer).
# Deliberately short, objectively checkable, and free of anything resembling
# real traffic: this corpus is committed, so it must carry no user content.
CASES = [
    ("What is 17 * 23? Reply with only the number.", "391"),
    ("List the first 8 prime numbers, comma separated, nothing else.",
     "2, 3, 5, 7, 11, 13, 17, 19"),
    ("Spell the word 'necessary' backwards. Only the reversed letters.",
     "yrassecen"),
    ("If a train leaves at 14:35 and arrives 2h47m later, what time does it "
     "arrive? Only HH:MM.", "17:22"),
    ("Complete: The capital of Australia is", "Canberra"),
    ("Sort these numbers ascending, comma separated: 42, 7, 19, 3, 88, 15",
     "3, 7, 15, 19, 42, 88"),
    ("How many letters are in the word 'refrigerator'? Only the number.", "12"),
    ("What is the 10th Fibonacci number if F(1)=1, F(2)=1? Only the number.",
     "55"),
]


def ask(base, key, model, prompt, max_tokens, timeout):
    body = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.0,
        "top_p": 1.0,
        "max_tokens": max_tokens,
        # Thinking would dominate the output and make identity meaningless.
        "chat_template_kwargs": {"enable_thinking": False},
    }
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json",
                 "Authorization": "Bearer " + key},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        payload = json.loads(response.read())
    return payload["choices"][0]["message"]["content"].strip()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("a_base")
    parser.add_argument("b_base")
    parser.add_argument("--a-name", default="a")
    parser.add_argument("--b-name", default="b")
    parser.add_argument("--model", default="qwen3.8-27b")
    parser.add_argument("--max-tokens", type=int, default=96)
    parser.add_argument("--timeout", type=float, default=120.0)
    args = parser.parse_args()

    key = os.environ.get("BENCH_TOKEN")
    if not key:
        raise SystemExit("set BENCH_TOKEN")

    identical = 0
    a_correct = 0
    b_correct = 0
    for prompt, expected in CASES:
        a_text = ask(args.a_base, key, args.model, prompt, args.max_tokens, args.timeout)
        b_text = ask(args.b_base, key, args.model, prompt, args.max_tokens, args.timeout)
        same = a_text == b_text
        a_hit = expected.lower() in a_text.lower()
        b_hit = expected.lower() in b_text.lower()
        identical += same
        a_correct += a_hit
        b_correct += b_hit
        print(json.dumps({
            "prompt_sha": hashlib.sha256(prompt.encode()).hexdigest()[:8],
            "identical": same,
            args.a_name + "_correct": a_hit,
            args.b_name + "_correct": b_hit,
            args.a_name + "_len": len(a_text),
            args.b_name + "_len": len(b_text),
        }), flush=True)

    print(json.dumps({
        "summary": True,
        "n": len(CASES),
        "identical": identical,
        args.a_name + "_correct": a_correct,
        args.b_name + "_correct": b_correct,
    }), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
