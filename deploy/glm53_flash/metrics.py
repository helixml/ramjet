#!/usr/bin/env python3
"""Small native SGLang metric snapshot/reconciliation helper."""

from __future__ import annotations

import math
import re
import urllib.request


METRICS = {
    "requests": "sglang:num_requests_total",
    "prompt_tokens": "sglang:prompt_tokens_total",
    "completion_tokens": "sglang:generation_tokens_total",
}


def _metric_total(exposition: str, metric: str) -> int:
    pattern = re.compile(
        rf"^{re.escape(metric)}(?:\{{[^}}]*\}})?\s+([^\s]+)(?:\s+\d+)?$"
    )
    values: list[float] = []
    for raw_line in exposition.splitlines():
        match = pattern.match(raw_line.strip())
        if match:
            value = float(match.group(1))
            if not math.isfinite(value) or value < 0:
                raise ValueError(f"invalid native metric value for {metric}")
            values.append(value)
    if not values:
        raise ValueError(f"native metric is absent: {metric}")
    total = sum(values)
    rounded = round(total)
    if not math.isclose(total, rounded, abs_tol=1e-6):
        raise ValueError(f"native counter is not integral: {metric}={total}")
    return rounded


def snapshot(url: str) -> dict[str, int]:
    call = urllib.request.Request(url, headers={"Accept": "text/plain"})
    with urllib.request.urlopen(call, timeout=15) as response:
        if response.status != 200:
            raise ValueError(f"metrics endpoint returned HTTP {response.status}")
        exposition = response.read().decode("utf-8", "replace")
    return {name: _metric_total(exposition, metric) for name, metric in METRICS.items()}


def reconcile(
    before: dict[str, int], after: dict[str, int], expected: dict[str, int]
) -> dict[str, int]:
    delta = {name: after[name] - before[name] for name in METRICS}
    if delta != expected:
        raise ValueError(f"native/client accounting mismatch: {delta=} {expected=}")
    return delta
