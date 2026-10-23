#!/usr/bin/env python3
"""Kernel A/B client for one directly addressed GLM-5.3 engine (stdlib only).

decode : fixed coding prompts, temperature 0, measures per-request decode tok/s
         (completion tokens after the first, over first->last token time) and
         aggregate output tok/s at a given concurrency.
prefill: unique-salted filler prompts of a target token length, max_tokens=1,
         measures cold TTFT (prefill throughput).

Prompts are deterministic so two engine builds see identical work and identical
EAGLE acceptance opportunities; the salt only defeats the prefix cache.
Never prints completion content.
"""
import argparse
import json
import statistics
import threading
import time
import urllib.request

TASKS = [
    "Implement an LRU cache class in Python with O(1) get and put, with type hints and docstrings.",
    "Write a Rust function that parses an HTTP/1.1 request line and headers from a byte slice without allocation.",
    "Write a Go HTTP middleware that rate-limits per client IP with a token bucket, including tests.",
    "Implement Dijkstra's shortest path in C++ using a binary heap, and explain its complexity.",
    "Write a TypeScript React hook that debounces a value and cancels on unmount, with an example component.",
    "Implement a thread-safe bounded blocking queue in Java using ReentrantLock and two Conditions.",
    "Write a Python asyncio web crawler limited to N concurrent requests with retry and backoff.",
    "Write a SQL schema and queries for a multi-tenant invoicing system with line items and taxes.",
]

FILLER = (
    "The quarterly operations review covered supply chain latency, warehouse throughput, "
    "and the migration of the billing service to the new event pipeline. "
)


def post(base, body, timeout=1800):
    req = urllib.request.Request(
        base + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.perf_counter()
    first = last = None
    usage = None
    with urllib.request.urlopen(req, timeout=timeout) as r:
        for raw in r:
            line = raw.strip()
            if not line.startswith(b"data:"):
                continue
            data = line[5:].strip()
            if data == b"[DONE]":
                break
            obj = json.loads(data)
            if obj.get("usage"):
                usage = obj["usage"]
            for ch in obj.get("choices", []):
                d = ch.get("delta", {})
                if d.get("content") or d.get("reasoning_content") or d.get("reasoning"):
                    now = time.perf_counter()
                    if first is None:
                        first = now
                    last = now
    end = time.perf_counter()
    return {"t0": t0, "first": first, "last": last, "end": end, "usage": usage or {}}


def scrape(base):
    """Sum sglang counters across TP ranks' exported series (rank 0 exports)."""
    vals = {}
    try:
        with urllib.request.urlopen(base + "/metrics", timeout=10) as r:
            for line in r.read().decode().splitlines():
                for name in ("sglang:spec_verify_calls_total", "sglang:generation_tokens_total"):
                    if line.startswith(name + "{") or line.startswith(name + " "):
                        vals[name] = vals.get(name, 0.0) + float(line.rsplit(" ", 1)[1])
    except Exception:  # noqa: BLE001
        pass
    return vals


def decode_cell(base, model, conc, max_tokens, rounds, salt):
    m0 = scrape(base)
    jobs = []
    for r in range(rounds):
        for i in range(conc):
            task = TASKS[(r * conc + i) % len(TASKS)]
            jobs.append(task)
    results = []
    lock = threading.Lock()
    idx = [0]

    def worker():
        while True:
            with lock:
                if idx[0] >= len(jobs):
                    return
                j = idx[0]
                idx[0] += 1
            body = {
                "model": model,
                "messages": [
                    {"role": "system", "content": f"run {salt}-{j}"},
                    {"role": "user", "content": jobs[j]},
                ],
                "max_tokens": max_tokens,
                "temperature": 0,
                "stream": True,
                "stream_options": {"include_usage": True},
                "ignore_eos": True,
            }
            res = post(base, body)
            with lock:
                results.append(res)

    t_start = time.perf_counter()
    threads = [threading.Thread(target=worker) for _ in range(conc)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - t_start
    m1 = scrape(base)
    dv = m1.get("sglang:spec_verify_calls_total", 0) - m0.get("sglang:spec_verify_calls_total", 0)
    dg = m1.get("sglang:generation_tokens_total", 0) - m0.get("sglang:generation_tokens_total", 0)
    per_req = []
    ttft = []
    total_out = 0
    for res in results:
        n = res["usage"].get("completion_tokens", 0)
        total_out += n
        ttft.append(res["first"] - res["t0"])
        span = res["last"] - res["first"]
        if n > 1 and span > 0:
            per_req.append((n - 1) / span)
    return {
        "cell": f"decode-c{conc}",
        "requests": len(results),
        "max_tokens": max_tokens,
        "decode_tps_per_req_mean": round(statistics.mean(per_req), 2),
        "decode_tps_per_req_median": round(statistics.median(per_req), 2),
        "aggregate_out_tps": round(total_out / wall, 2),
        "ttft_ms_p50": round(1000 * statistics.median(ttft), 1),
        "wall_s": round(wall, 2),
        # Kernel-speed view independent of EAGLE acceptance (which shifts when
        # numerics change the generated text): tokens per verify and decode
        # wall per verify iteration (c1: summed request decode spans).
        "tokens_per_verify": round(dg / dv, 3) if dv else None,
        "ms_per_iteration": round(
            1000 * (sum(r["last"] - r["first"] for r in results) if conc == 1 else wall) / dv, 3
        ) if dv else None,
    }


def prefill_cell(base, model, tokens, reps, salt):
    # ~22 tokens per filler sentence for this tokenizer; calibrated against usage.
    out = []
    for r in range(reps):
        n_sent = max(1, int(tokens / 22))
        body = {
            "model": model,
            "messages": [
                {"role": "user", "content": f"[{salt}-{tokens}-{r}] " + FILLER * n_sent + "\nSummarize in one word."}
            ],
            "max_tokens": 1,
            "temperature": 0,
            "stream": True,
            "stream_options": {"include_usage": True},
        }
        res = post(base, body)
        pt = res["usage"].get("prompt_tokens", 0)
        cached = (res["usage"].get("prompt_tokens_details") or {}).get("cached_tokens", 0)
        dt = res["end"] - res["t0"]
        out.append({"prompt_tokens": pt, "cached": cached, "ttft_s": dt})
    ttfts = [o["ttft_s"] for o in out]
    pt = out[0]["prompt_tokens"]
    return {
        "cell": f"prefill-{tokens // 1024}k",
        "prompt_tokens": pt,
        "cached_max": max(o["cached"] for o in out),
        "ttft_s_median": round(statistics.median(ttfts), 3),
        "prefill_tps": round(pt / statistics.median(ttfts), 1),
        "reps": reps,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:8065")
    ap.add_argument("--model", default="glm-5.3-flash")
    ap.add_argument("--label", required=True)
    ap.add_argument("--salt", default=str(time.time_ns()))
    ap.add_argument("--decode-conc", default="1,4")
    ap.add_argument("--max-tokens", type=int, default=512)
    ap.add_argument("--rounds", type=int, default=2)
    ap.add_argument("--prefill-k", default="8,32,128")
    ap.add_argument("--prefill-reps", type=int, default=2)
    ap.add_argument("--out")
    a = ap.parse_args()
    cells = []
    # Warm one request so lazy JIT is not charged to the first cell.
    post(a.base, {"model": a.model, "messages": [{"role": "user", "content": f"hi {a.salt}"}],
                  "max_tokens": 8, "stream": True})
    for c in [int(x) for x in a.decode_conc.split(",") if x and int(x) > 0]:
        cells.append(decode_cell(a.base, a.model, c, a.max_tokens, a.rounds, a.salt))
        print(json.dumps({"label": a.label, **cells[-1]}), flush=True)
    for k in [int(x) for x in a.prefill_k.split(",") if x and int(x) > 0]:
        cells.append(prefill_cell(a.base, a.model, k * 1024, a.prefill_reps, a.salt))
        print(json.dumps({"label": a.label, **cells[-1]}), flush=True)
    if a.out:
        with open(a.out, "a") as f:
            for c in cells:
                f.write(json.dumps({"label": a.label, "salt": a.salt, **c}) + "\n")


if __name__ == "__main__":
    main()
