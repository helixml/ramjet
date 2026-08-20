#!/usr/bin/env python3
"""Batch-1 decode throughput probe for the sglang DFlash2 repro.

Sequential single-request cells against an OpenAI-compatible endpoint.
Decode tok/s = (completion_tokens - 1) / (last_token_time - first_token_time),
tokens taken from the server's own usage object, never client-side counting.
"""
import argparse, json, sys, time
import urllib.request

PROMPTS = [
    "Write a Python function that parses an ISO-8601 timestamp without using datetime, with full error handling and docstrings.",
    "Explain how speculative decoding works in LLM inference, covering draft models, verification, and acceptance rates.",
    "Write a detailed README for a small Rust CLI tool that tails a JSONL log file and pretty-prints matching records.",
    "Describe the tradeoffs between tensor parallelism and pipeline parallelism for serving a dense 27B model on 8 GPUs.",
    "Implement a thread-safe LRU cache in Go with generics, including tests and comments explaining the locking strategy.",
]

def run_cell(base, model, prompt, max_tokens, sampling, timeout):
    body = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "stream": True,
        "stream_options": {"include_usage": True},
        "chat_template_kwargs": {"enable_thinking": False},
    }
    body.update(sampling)
    req = urllib.request.Request(
        base + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.monotonic()
    t_first = None
    t_last = None
    usage = None
    finish = None
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        for raw in resp:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if payload == "[DONE]":
                break
            chunk = json.loads(payload)
            if chunk.get("usage"):
                usage = chunk["usage"]
            for ch in chunk.get("choices", []):
                delta = ch.get("delta") or {}
                if delta.get("content") or delta.get("reasoning_content"):
                    now = time.monotonic()
                    if t_first is None:
                        t_first = now
                    t_last = now
                if ch.get("finish_reason"):
                    finish = ch["finish_reason"]
    wall = time.monotonic() - t0
    ct = usage["completion_tokens"] if usage else None
    ttft = (t_first - t0) if t_first else None
    decode_s = (t_last - t_first) if (t_first and t_last and t_last > t_first) else None
    decode_tps = ((ct - 1) / decode_s) if (ct and decode_s) else None
    return {
        "completion_tokens": ct,
        "prompt_tokens": usage.get("prompt_tokens") if usage else None,
        "ttft_s": round(ttft, 3) if ttft else None,
        "decode_tok_s": round(decode_tps, 1) if decode_tps else None,
        "wall_s": round(wall, 2),
        "finish": finish,
    }

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base")
    ap.add_argument("model")
    ap.add_argument("--max-tokens", type=int, default=1024)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--repetitions", type=int, default=1)
    ap.add_argument("--sampling", choices=["greedy", "official"], default="official")
    args = ap.parse_args()

    sampling = ({"temperature": 0.0} if args.sampling == "greedy"
                else {"temperature": 1.0, "top_p": 0.95, "top_k": 20})

    # one discarded warmup (JIT/graph capture pollutes the first cell)
    warm = run_cell(args.base, args.model, PROMPTS[0], 64, sampling, args.timeout)
    print(json.dumps({"cell": "warmup-discarded", **warm}), flush=True)

    results = []
    for rep in range(args.repetitions):
        for i, p in enumerate(PROMPTS):
            r = run_cell(args.base, args.model, p, args.max_tokens, sampling, args.timeout)
            r["cell"] = f"rep{rep}-p{i}"
            r["sampling"] = args.sampling
            results.append(r)
            print(json.dumps(r), flush=True)

    tps = [r["decode_tok_s"] for r in results if r["decode_tok_s"]]
    if tps:
        tps.sort()
        summary = {
            "cells": len(tps),
            "decode_tok_s_min": tps[0],
            "decode_tok_s_median": tps[len(tps) // 2],
            "decode_tok_s_max": tps[-1],
            "sampling": args.sampling,
        }
        print(json.dumps({"summary": summary}), flush=True)

if __name__ == "__main__":
    main()
