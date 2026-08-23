#!/usr/bin/env python3
"""Sample LB dispatch against per-engine scheduler occupancy.

Answers "is this concurrency level actually running, or queueing?" — the LB
dispatches every request it accepts, so a request can be in flight from the
proxy's point of view while sitting in an engine's waiting queue. On the
hybrid Qwen3.8 stack the binding limit is the mamba state cache, which caps
each engine at `max_running_requests` well below the fleet's nominal
concurrency (12 per engine, 96 across eight, on the 2026-08-23 shape).

Generates no requests of its own: run it beside a benchmark cell. Writes CSV
to stdout, one row per poll.

Usage:
  slot_sampler.py --duration 300 > slots.csv
  slot_sampler.py --engine-metrics http://127.0.0.1:8030/metrics ... \
      --lb-metrics http://127.0.0.1:8007/metrics

Read the result as: lb_upstream_inflight is what the LB dispatched,
run_total is what the engines are actually running, and queue_total is the
difference sitting in scheduler queues. A run_total that plateaus below
lb_upstream_inflight while queue_total absorbs the remainder is a per-engine
capacity ceiling, not a throughput measurement.
"""

import argparse
import sys
import time
import urllib.request

DEFAULT_LB = "http://127.0.0.1:8007/metrics"
DEFAULT_ENGINES = ["http://127.0.0.1:%d/metrics" % (8030 + i) for i in range(8)]

RUNNING_METRIC = "sglang:num_running_reqs"
QUEUE_METRIC = "sglang:num_queue_reqs"
LB_INFLIGHT = "ramjet_requests_inflight"
LB_UPSTREAM_INFLIGHT = "ramjet_upstream_inflight"


def scrape(url, timeout):
    """Return the metrics body, or "" if the endpoint is unreachable.

    A failed scrape must not kill a sampling run: losing one poll of one
    engine is far cheaper than losing the whole cell it was observing.
    """
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return response.read().decode("utf-8", "replace")
    except Exception:
        return ""


def sum_metric(text, name):
    """Sum every labelled sample of `name`, or NaN if the metric is absent.

    Absent and zero are different facts here — a zero means the engine
    reported no running requests, while an absent metric means the scrape
    failed or the engine does not publish it, and averaging those together
    would silently understate occupancy.
    """
    total = 0.0
    found = False
    for line in text.splitlines():
        if line.startswith("#") or not line.startswith(name):
            continue
        # Guard against a longer metric name sharing this prefix.
        suffix = line[len(name):]
        if suffix and suffix[0] not in "{ ":
            continue
        try:
            total += float(line.rsplit(None, 1)[1])
            found = True
        except (ValueError, IndexError):
            continue
    return total if found else float("nan")


def fmt(value):
    return "nan" if value != value else "%.0f" % value


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--lb-metrics", default=DEFAULT_LB)
    parser.add_argument("--engine-metrics", action="append", default=None,
                        help="repeatable; defaults to the eight sglang engines")
    parser.add_argument("--interval", type=float, default=0.5)
    parser.add_argument("--duration", type=float, default=300.0)
    parser.add_argument("--timeout", type=float, default=2.0)
    args = parser.parse_args()

    engines = args.engine_metrics or DEFAULT_ENGINES
    names = ["e%d" % i for i in range(len(engines))]

    columns = ["ts", "lb_inflight", "lb_upstream_inflight", "run_total", "queue_total"]
    columns += ["run_" + n for n in names] + ["queue_" + n for n in names]
    print(",".join(columns), flush=True)

    end = time.monotonic() + args.duration
    while time.monotonic() < end:
        started = time.monotonic()
        lb = scrape(args.lb_metrics, args.timeout)
        runs, queues = [], []
        for url in engines:
            body = scrape(url, args.timeout)
            runs.append(sum_metric(body, RUNNING_METRIC))
            queues.append(sum_metric(body, QUEUE_METRIC))
        row = [
            "%.3f" % time.time(),
            fmt(sum_metric(lb, LB_INFLIGHT)),
            fmt(sum_metric(lb, LB_UPSTREAM_INFLIGHT)),
            fmt(sum(runs)),
            fmt(sum(queues)),
        ]
        row += [fmt(v) for v in runs] + [fmt(v) for v in queues]
        print(",".join(row), flush=True)
        time.sleep(max(0.0, args.interval - (time.monotonic() - started)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
