"""Small Prometheus helpers shared by GPU benchmark scripts."""

import re
import threading
import time
import urllib.request


COUNTERS = {
    "preemptions": "vllm:num_preemptions_total",
    "prompt_tokens": "vllm:prompt_tokens_total",
    "cached_prompt_tokens": "vllm:prompt_tokens_cached_total",
    "prefix_queries": "vllm:prefix_cache_queries_total",
    "prefix_hits": "vllm:prefix_cache_hits_total",
    "queue_seconds_sum": "vllm:request_queue_time_seconds_sum",
    "queue_samples": "vllm:request_queue_time_seconds_count",
    "prefill_seconds_sum": "vllm:request_prefill_time_seconds_sum",
    "prefill_samples": "vllm:request_prefill_time_seconds_count",
}
GAUGES = {
    "running": "vllm:num_requests_running",
    "waiting": "vllm:num_requests_waiting",
    "kv_cache_usage": "vllm:kv_cache_usage_perc",
}


def metric_value(body, name):
    matches = re.findall(
        r"^" + re.escape(name) + r"(?:\{[^\n]*\})?\s+([0-9.eE+-]+)$",
        body,
        re.MULTILINE,
    )
    if not matches:
        return None
    return sum(float(value) for value in matches)


def fetch(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        body = response.read().decode("utf-8", "replace")
    return {
        key: metric_value(body, name)
        for key, name in {**COUNTERS, **GAUGES}.items()
    }


def delta(before, after):
    if before is None or after is None:
        return None
    result = {}
    for key in COUNTERS:
        left = before.get(key)
        right = after.get(key)
        if left is None or right is None or right < left:
            result[key] = None
        else:
            result[key] = right - left
    queue_samples = result["queue_samples"]
    prefill_samples = result["prefill_samples"]
    result["queue_ms_mean"] = (
        round(1000 * result["queue_seconds_sum"] / queue_samples, 2)
        if queue_samples and result["queue_seconds_sum"] is not None
        else None
    )
    result["prefill_ms_mean"] = (
        round(1000 * result["prefill_seconds_sum"] / prefill_samples, 2)
        if prefill_samples and result["prefill_seconds_sum"] is not None
        else None
    )
    prefix_queries = result["prefix_queries"]
    prefix_hits = result["prefix_hits"]
    result["prefix_hit_pct"] = (
        round(100 * prefix_hits / prefix_queries, 2)
        if prefix_queries and prefix_hits is not None
        else None
    )
    return result


class PeakSampler:
    """Poll low-cardinality engine gauges during one benchmark cell."""

    def __init__(self, url, interval=0.02):
        self.url = url
        self.interval = interval
        self.peaks = {key: None for key in GAUGES}
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        if not self.url:
            return
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=max(1.0, 5 * self.interval))

    def _run(self):
        while not self._stop.is_set():
            try:
                sample = fetch(self.url, timeout=max(1.0, 5 * self.interval))
                for key in GAUGES:
                    value = sample[key]
                    if value is not None:
                        self.peaks[key] = (
                            value if self.peaks[key] is None else max(self.peaks[key], value)
                        )
            except Exception:
                pass
            self._stop.wait(self.interval)
