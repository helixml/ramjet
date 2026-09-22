#!/usr/bin/env python3
"""Run a small, synthetic TypeSafe System One latency benchmark through ramjet.

The script prints aggregate JSON lines only. It never prints states, answers,
credentials, or response bodies.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import statistics
import time
import urllib.request
from datetime import datetime, timezone


QUESTIONS = {
    "department": {
        "type": "choice",
        "instructions": "Which team should handle this case?",
        "criteria": {
            "returns": "Returns, exchanges, or damaged goods",
            "shipping": "Delivery status, delay, or loss",
            "billing": "Charges, invoices, or payments",
        },
    },
    "escalate": {
        "type": "noul",
        "instructions": "Does this case require prompt human review?",
        "criteria": {
            "true": "Prompt review is required",
            "false": "Normal queue is appropriate",
        },
    },
    "urgency": {
        "type": "score",
        "instructions": "How urgent is this case?",
        "criteria": ["can wait", "this week", "today"],
    },
}


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def fetch_metrics(metrics_url: str) -> dict[str, float]:
    with urllib.request.urlopen(metrics_url, timeout=5) as response:
        body = response.read().decode()
    totals: dict[str, float] = {}
    for line in body.splitlines():
        if not line.startswith("ramjet_upstream_requests_total{"):
            continue
        upstream = line.split('upstream="', 1)[1].split('"', 1)[0]
        totals[upstream] = totals.get(upstream, 0.0) + float(line.rsplit(" ", 1)[1])
    return totals


def request_one(url: str, state: str) -> dict[str, float | int]:
    payload = json.dumps(
        {"state": state, "model": "kev-latest", "questions": QUESTIONS}
    ).encode()
    request = urllib.request.Request(
        url,
        data=payload,
        headers={"content-type": "application/json"},
        method="POST",
    )
    started = time.perf_counter()
    with urllib.request.urlopen(request, timeout=120) as response:
        body = json.load(response)
        upstream = response.headers.get("x-ramjet-upstream")
    wall_ms = (time.perf_counter() - started) * 1000
    if upstream != "3":
        raise RuntimeError(f"request reached unexpected upstream {upstream!r}")
    if body.get("model") != "kev-latest":
        raise RuntimeError("response model did not match kev-latest")
    if set(body.get("answers", {})) != set(QUESTIONS):
        raise RuntimeError("response did not contain every question")
    answer_types = {answer.get("type") for answer in body["answers"].values()}
    if answer_types != {"choice", "noul", "score"}:
        raise RuntimeError("response did not contain every answer type")
    return {
        "wall_ms": wall_ms,
        "model_ms": float(body["latency_ms"]),
        "input_tokens": int(body["usage"]["input_tokens"]),
        "output_tokens": int(body["usage"]["output_tokens"]),
    }


def run_cell(
    url: str,
    name: str,
    count: int,
    concurrency: int,
    state_factory,
) -> None:
    started = time.perf_counter()
    if concurrency == 1:
        rows = [request_one(url, state_factory(index)) for index in range(count)]
    else:
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
            rows = list(
                pool.map(
                    lambda index: request_one(url, state_factory(index)), range(count)
                )
            )
    elapsed = time.perf_counter() - started
    model_ms = [float(row["model_ms"]) for row in rows]
    wall_ms = [float(row["wall_ms"]) for row in rows]
    print(
        json.dumps(
            {
                "cell": name,
                "concurrency": concurrency,
                "elapsed_seconds": round(elapsed, 3),
                "failures": 0,
                "input_tokens_median": round(
                    statistics.median(int(row["input_tokens"]) for row in rows)
                ),
                "model_ms_p50": round(statistics.median(model_ms), 1),
                "model_ms_p95": round(percentile(model_ms, 0.95), 1),
                "questions": count * len(QUESTIONS),
                "questions_per_second": round(
                    count * len(QUESTIONS) / elapsed, 2
                ),
                "requests": count,
                "requests_per_second": round(count / elapsed, 2),
                "wall_ms_p50": round(statistics.median(wall_ms), 1),
                "wall_ms_p95": round(percentile(wall_ms, 0.95), 1),
            },
            sort_keys=True,
        ),
        flush=True,
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default="http://127.0.0.1:8006")
    parser.add_argument("--short-requests", type=int, default=24)
    parser.add_argument("--cached-requests", type=int, default=12)
    parser.add_argument("--concurrent-requests", type=int, default=24)
    parser.add_argument("--concurrency", type=int, default=4)
    args = parser.parse_args()
    if min(args.short_requests, args.cached_requests, args.concurrent_requests) < 1:
        parser.error("request counts must be positive")
    if args.concurrency < 1:
        parser.error("concurrency must be positive")

    url = args.base_url.rstrip("/") + "/v1/systemone"
    metrics_url = args.base_url.rstrip("/").replace(":8006", ":8007") + "/metrics"
    namespace = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    short = lambda index: (
        f"Synthetic case {namespace}-{index}. A customer says an order arrived late, "
        "the item is the wrong size, and the card statement shows a duplicate charge. "
        "They ask for a replacement and a billing correction."
    )
    policy_line = (
        "Synthetic policy record. Returns are accepted within thirty days. "
        "Duplicate charges go to billing. Late delivery goes to shipping. "
        "Safety threats require prompt human review. "
    )
    long_state = (
        f"benchmark namespace {namespace}. "
        + policy_line * 180
        + "The case reports a duplicate charge and a late delivery."
    )

    before = fetch_metrics(metrics_url)
    run_cell(args.base_url.rstrip("/") + "/v1/systemone", "short-c1", args.short_requests, 1, short)
    run_cell(
        url,
        "short-c4",
        args.concurrent_requests,
        args.concurrency,
        lambda index: short(1000 + index),
    )
    cold = request_one(url, long_state)
    print(
        json.dumps(
            {
                "cell": "long-cold",
                "failures": 0,
                "input_tokens": cold["input_tokens"],
                "model_ms": cold["model_ms"],
                "questions": len(QUESTIONS),
                "requests": 1,
                "wall_ms": round(float(cold["wall_ms"]), 1),
            },
            sort_keys=True,
        ),
        flush=True,
    )
    run_cell(url, "long-cached-c1", args.cached_requests, 1, lambda _: long_state)
    run_cell(
        url,
        "long-cached-c4",
        args.concurrent_requests,
        args.concurrency,
        lambda _: long_state,
    )
    after = fetch_metrics(metrics_url)
    print(
        json.dumps(
            {
                "deltas": {
                    key: after.get(key, 0) - before.get(key, 0)
                    for key in sorted(set(before) | set(after))
                },
                "metric": "upstream_request_deltas",
            },
            sort_keys=True,
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
