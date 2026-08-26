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
SPEC_COUNTERS = {
    "draft_steps": "vllm:spec_decode_num_drafts_total",
    "proposed_tokens": "vllm:spec_decode_num_draft_tokens_total",
    "accepted_tokens": "vllm:spec_decode_num_accepted_tokens_total",
    "generation_tokens": "vllm:generation_tokens_total",
    "finished_requests": "vllm:request_success_total",
}
SPEC_POSITION_COUNTER = "vllm:spec_decode_num_accepted_tokens_per_pos_total"


def metric_value(body, name, required_labels=None):
    matches = re.findall(
        r"^" + re.escape(name) + r"(?:\{([^\n}]*)\})?\s+([0-9.eE+-]+)$",
        body,
        re.MULTILINE,
    )
    selected = []
    for raw_labels, value in matches:
        labels = dict(re.findall(r'(\w+)="((?:\\.|[^"\\])*)"', raw_labels))
        if required_labels and any(labels.get(key) != wanted for key, wanted in required_labels.items()):
            continue
        selected.append(float(value))
    if not selected:
        return None
    return sum(selected)


def fetch(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        body = response.read().decode("utf-8", "replace")
    return {
        key: metric_value(body, name)
        for key, name in {**COUNTERS, **GAUGES}.items()
    }


def position_values(body, name=SPEC_POSITION_COUNTER):
    """Return summed speculative counters keyed by bounded integer position."""
    matches = re.findall(
        r"^" + re.escape(name) + r"\{([^\n}]*)\}\s+([0-9.eE+-]+)$",
        body,
        re.MULTILINE,
    )
    result = {}
    for raw_labels, value in matches:
        labels = dict(re.findall(r'(\w+)="((?:\\.|[^"\\])*)"', raw_labels))
        raw_position = labels.get("position")
        try:
            position = int(raw_position)
        except (TypeError, ValueError):
            continue
        if 0 <= position <= 64:
            result[position] = result.get(position, 0.0) + float(value)
    return result


def fetch_speculative(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        body = response.read().decode("utf-8", "replace")
    result = {key: metric_value(body, name) for key, name in SPEC_COUNTERS.items()}
    result["accepted_per_position"] = position_values(body)
    return result


def speculative_delta(
    before,
    after,
    client_completion_tokens,
    client_requests,
    expected_enabled=None,
):
    """Normalize speculative work and reject reset or contaminated intervals."""
    if before is None or after is None:
        return {"state": "unavailable", "reconciled": False}
    core = ("draft_steps", "proposed_tokens", "accepted_tokens")
    present = [before.get(key) is not None and after.get(key) is not None for key in core]
    if not any(present):
        state = "disabled" if expected_enabled is False else "unavailable"
        return {"state": state, "reconciled": state == "disabled"}
    if not all(present):
        return {"state": "incomplete", "reconciled": False}
    keys = (*core, "generation_tokens", "finished_requests")
    values = {}
    for key in keys:
        left, right = before.get(key), after.get(key)
        if left is None or right is None:
            return {"state": "incomplete", "reconciled": False}
        if right < left:
            return {"state": "counter_reset", "reconciled": False}
        values[key] = right - left
    positions = {}
    before_positions = before.get("accepted_per_position") or {}
    after_positions = after.get("accepted_per_position") or {}
    for position in sorted(set(before_positions) | set(after_positions)):
        left = before_positions.get(position, 0)
        right = after_positions.get(position, 0)
        if right < left:
            return {"state": "counter_reset", "reconciled": False}
        positions[str(position)] = int(right - left)
    draft_steps = values["draft_steps"]
    proposed = values["proposed_tokens"]
    accepted = values["accepted_tokens"]
    if draft_steps <= 0 or proposed <= 0:
        return {"state": "no_drafts", "reconciled": False}
    reconciled = (
        values["generation_tokens"] == client_completion_tokens
        and values["finished_requests"] == client_requests
    )
    return {
        "state": "enabled" if reconciled else "contaminated",
        "reconciled": reconciled,
        "client_completion_tokens": int(client_completion_tokens),
        "engine_generation_tokens": int(values["generation_tokens"]),
        "client_requests": int(client_requests),
        "engine_finished_requests": int(values["finished_requests"]),
        "draft_steps": int(draft_steps),
        "proposed_tokens": int(proposed),
        "accepted_tokens": int(accepted),
        "strict_acceptance_pct": round(100 * accepted / proposed, 2),
        "proposed_tokens_per_step": round(proposed / draft_steps, 3),
        "accepted_tokens_per_step": round(accepted / draft_steps, 3),
        "effective_tokens_per_step": round(1 + accepted / draft_steps, 3),
        "accepted_per_position": positions,
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


def aggregate_deltas(before, after):
    """Aggregate one counter interval across an exact set of engine snapshots."""
    if not before or not after or len(before) != len(after):
        return None
    cells = [delta(left, right) for left, right in zip(before, after, strict=True)]
    if any(cell is None for cell in cells):
        return None
    result = {}
    for key in COUNTERS:
        values = [cell[key] for cell in cells]
        result[key] = None if any(value is None for value in values) else sum(values)
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


def cache_usage(
    response_prompt_tokens, response_cached_tokens, engine, authority="response"
):
    """Select the declared cache authority without silently falling back.

    Some OpenAI-compatible responses, including Qwen3.8-Flash-Next on the
    current vLLM preview, report cached_tokens=0 even when vLLM's native prefix
    counters record reuse. Native mode therefore uses only the interval's
    prefix query/hit counters. Missing, reset, or impossible native counters
    make the result unavailable instead of promoting response usage again.
    """
    if authority not in {"response", "vllm-prefix"}:
        raise ValueError(f"unsupported cache authority: {authority}")
    source = "response_usage" if authority == "response" else "vllm_prefix_counters"
    prompt = response_prompt_tokens
    cached = response_cached_tokens
    if authority == "vllm-prefix":
        prompt = None if engine is None else engine.get("prefix_queries")
        cached = None if engine is None else engine.get("prefix_hits")
    available = (
        prompt is not None
        and cached is not None
        and prompt >= 0
        and 0 <= cached <= prompt
        and not (
            authority == "vllm-prefix"
            and response_prompt_tokens > 0
            and prompt == 0
        )
    )
    return {
        "source": source,
        "available": available,
        "prompt_tokens": prompt if available else None,
        "cached_tokens": cached if available else None,
        "hit_pct": (
            round(100 * cached / prompt, 2) if available and prompt else None
        ),
        "response_usage_authoritative": authority == "response",
        "response_prompt_tokens": response_prompt_tokens,
        "response_cached_tokens": response_cached_tokens,
    }


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
