#!/usr/bin/env python3
"""Concurrency sweep for the Qwen3.8 stack.

Measures aggregate output throughput, TTFT, and per-upstream split at a series
of concurrency levels. Each cell uses a fresh salt so no prompt is served from
a prior cell's prefix cache; warm reuse is measured separately and deliberately.

Usage: qwen_concurrency.py BASE MODEL [--concurrencies 1,8,16,32] [--tokens 256]

Reads the bearer from BENCH_TOKEN. Prints one JSON record per cell to stdout.
"""

import argparse
import json
import os
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request

PROMPT = (
    "You are a systems engineer. Answer concisely and concretely.\n\n"
    "Task {index} (salt {salt}): explain one distinct trade-off in "
    "distributed KV cache design, in about three sentences."
)

# One app's shared system prompt. Everything before the per-request tail is
# byte-identical across that app's requests, which is what makes the prefix
# cacheable and what gives the router something to route on.
APP_PREAMBLE = (
    "You are assistant {app} for salt {salt}. Operating manual section {line}: "
    "follow the escalation policy, cite the runbook, and never invent an "
    "identifier that is not present in the provided context.\n"
)


def shared_prefix(app, salt, kib):
    """Builds a deterministic, app-specific system prompt of about `kib` KiB."""

    target = kib * 1024
    parts = []
    size = 0
    line = 0
    while size < target:
        chunk = APP_PREAMBLE.format(app=app, salt=salt, line=line)
        parts.append(chunk)
        size += len(chunk)
        line += 1
    return "".join(parts)


def one_request(
    base, model, token, index, salt, max_tokens, timeout, deterministic,
    apps=0, prefix_kib=0,
):
    """Issues one streaming request and returns (ttft, wall, output_tokens)."""
    if apps > 0 and prefix_kib > 0:
        # Round-robin over apps so every app is exercised and its prefix is
        # reused, which is the shape of real agent traffic: a handful of large
        # system prompts, each shared by many short turns.
        app = index % apps
        messages = [
            {"role": "system", "content": shared_prefix(app, salt, prefix_kib)},
            {"role": "user", "content": f"Turn {index}: summarise the policy in one sentence."},
        ]
    else:
        messages = [{"role": "user", "content": PROMPT.format(index=index, salt=salt)}]
    body = {
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.0,
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    if deterministic:
        # Throughput A/B needs every request to emit exactly max_tokens.
        # Left to itself the model thinks for a variable number of tokens and
        # may stop early, so cell wall-time reflects prompt luck rather than
        # engine speed -- an early sweep read 3693 tok/s and 1496 tok/s at the
        # same concurrency for that reason. ignore_eos plus thinking off makes
        # the output length fixed and the comparison meaningful.
        body["chat_template_kwargs"] = {"enable_thinking": False}
        body["ignore_eos"] = True
        body["min_tokens"] = max_tokens
    request = urllib.request.Request(
        f"{base}/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "Authorization": f"Bearer {token}"},
    )
    started = time.time()
    ttft = None
    completion_tokens = 0
    upstream = None
    with urllib.request.urlopen(request, timeout=timeout) as response:
        upstream = response.headers.get("x-mini-dynamo-upstream")
        for raw in response:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data: "):
                continue
            payload = line[6:]
            if payload == "[DONE]":
                break
            chunk = json.loads(payload)
            if ttft is None and chunk.get("choices"):
                delta = chunk["choices"][0].get("delta") or {}
                if delta.get("content") or delta.get("reasoning_content"):
                    ttft = time.time() - started
            if chunk.get("usage"):
                completion_tokens = chunk["usage"].get("completion_tokens", 0)
    return ttft, time.time() - started, completion_tokens, upstream


def run_cell(
    base, model, token, concurrency, salt, max_tokens, timeout, deterministic,
    apps=0, prefix_kib=0,
):
    results = []
    errors = []
    lock = threading.Lock()

    def worker(index):
        try:
            result = one_request(
                base, model, token, index, salt, max_tokens, timeout,
                deterministic, apps, prefix_kib,
            )
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, OSError) as error:
            with lock:
                errors.append(repr(error)[:120])
            return
        with lock:
            results.append(result)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(concurrency)]
    started = time.time()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    wall = time.time() - started

    ttfts = sorted(t for t, _, _, _ in results if t is not None)
    tokens = sum(c for _, _, c, _ in results)
    split = {}
    for _, _, _, upstream in results:
        split[upstream] = split.get(upstream, 0) + 1
    return {
        "concurrency": concurrency,
        "salt": salt,
        "apps": apps,
        "prefix_kib": prefix_kib,
        "completed": len(results),
        "errors": errors[:3],
        "error_count": len(errors),
        "wall_s": round(wall, 3),
        "output_tokens": tokens,
        "output_tok_per_s": round(tokens / wall, 1) if wall > 0 else None,
        "ttft_p50_s": round(statistics.median(ttfts), 3) if ttfts else None,
        "ttft_p95_s": round(ttfts[int(len(ttfts) * 0.95) - 1], 3) if len(ttfts) >= 2 else None,
        "upstream_split": split,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("base")
    parser.add_argument("model")
    parser.add_argument("--concurrencies", default="1,8,16,32")
    parser.add_argument("--tokens", type=int, default=256)
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument("--salt", default=None)
    parser.add_argument(
        "--apps", type=int, default=0,
        help="distinct shared system prompts to round-robin over (0 disables)",
    )
    parser.add_argument(
        "--prefix-kib", type=int, default=0,
        help="approximate KiB of shared system prompt per app",
    )
    parser.add_argument(
        "--thinking", action="store_true",
        help="leave thinking on and let requests stop early; realistic but high variance",
    )
    args = parser.parse_args()

    token = os.environ.get("BENCH_TOKEN")
    if not token:
        raise SystemExit("set BENCH_TOKEN")
    base = args.base.rstrip("/")
    salt_base = args.salt or str(int(time.time()))

    for concurrency in [int(c) for c in args.concurrencies.split(",") if c.strip()]:
        # A distinct salt per cell keeps each cell's prompts unseen, so a later
        # cell cannot inherit an earlier one's prefix-cache residency.
        cell = run_cell(
            base, args.model, token, concurrency,
            f"{salt_base}-c{concurrency}", args.tokens, args.timeout,
            not args.thinking, args.apps, args.prefix_kib,
        )
        print(json.dumps(cell), flush=True)


if __name__ == "__main__":
    sys.exit(main())
