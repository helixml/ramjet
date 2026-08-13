#!/usr/bin/env python3
"""Validate and replay content-free sovereign agent workload shapes.

The input schema accepts only bounded numeric fields and fixed enums. It has no
field capable of carrying prompt text, source identifiers, credentials, tool
payloads, absolute timestamps, or request fingerprints. Live output is limited
to structural validation, usage/timing counters, and bounded categories.
"""

import argparse
import concurrent.futures
import copy
import dataclasses
import hashlib
import json
import math
import os
import pathlib
import stat
import statistics
import sys
import time
import urllib.error
import urllib.request

from agentbench import (
    bounded_route_counts,
    execute_case,
    load_metadata,
    percentile,
)


MAX_TRACE_BYTES = 4 << 20
MAX_TRACE_RECORDS = 1024
MAX_PROMPT_TOKENS = 300_000
MAX_OUTPUT_TOKENS = 4096
MAX_TOTAL_PROMPT_TOKENS = 16_000_000
MAX_TOTAL_OUTPUT_TOKENS = 1_000_000
MAX_ARRIVAL_OFFSET_MS = 600_000
MAX_CALIBRATION_RESPONSE_BYTES = 1 << 20
ARRIVAL_BUCKET_MS = 100
PROTOCOLS = ("text", "required_tool", "auto_tool", "parallel_tool")
REASONING_EFFORTS = ("none", "minimal", "low", "medium", "high", "max", "xhigh")
PROMPT_BUCKETS = (1024, 4096, 16_384, 65_536, 131_072, 262_144, 300_000)
ARRIVAL_BUCKETS = (0, 1000, 10_000, 60_000, 300_000, 600_000)


class TraceShapeError(ValueError):
    """A trace shape or its private file envelope is invalid."""


@dataclasses.dataclass(frozen=True)
class SamplingShape:
    temperature: float
    top_p: float
    seed: int
    reasoning_effort: str


@dataclasses.dataclass(frozen=True)
class TraceShape:
    arrival_offset_ms: int
    prefix_group: int
    shared_prefix_tokens: int
    prompt_tokens: int
    history_turns: int
    history_tool_rounds: int
    history_parallel_calls: int
    protocol: str
    stream: bool
    expected_tool_calls: int
    max_output_tokens: int
    observed_completion_tokens: int
    sampling: SamplingShape


def _exact_fields(value, allowed, context):
    if not isinstance(value, dict):
        raise TraceShapeError(f"{context} must be an object")
    if set(value) != set(allowed):
        raise TraceShapeError(f"{context} fields do not match schema")


def _integer(value, minimum, maximum, context):
    if type(value) is not int or not minimum <= value <= maximum:
        raise TraceShapeError(f"{context} is out of range")
    return value


def _number(value, minimum, maximum, context, *, exclusive_minimum=False):
    if type(value) not in (int, float) or not math.isfinite(value):
        raise TraceShapeError(f"{context} is not a finite number")
    if (value <= minimum if exclusive_minimum else value < minimum) or value > maximum:
        raise TraceShapeError(f"{context} is out of range")
    return float(value)


def parse_shape(value, line_number):
    context = f"trace line {line_number}"
    _exact_fields(
        value,
        (
            "schema_version",
            "arrival_offset_ms",
            "prefix_group",
            "shared_prefix_tokens",
            "prompt_tokens",
            "history_turns",
            "history_tool_rounds",
            "history_parallel_calls",
            "protocol",
            "stream",
            "expected_tool_calls",
            "max_output_tokens",
            "observed_completion_tokens",
            "sampling",
        ),
        context,
    )
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise TraceShapeError(f"{context} schema_version must be 1")
    arrival = _integer(
        value["arrival_offset_ms"], 0, MAX_ARRIVAL_OFFSET_MS, f"{context} arrival"
    )
    if arrival % ARRIVAL_BUCKET_MS:
        raise TraceShapeError(f"{context} arrival is not privacy bucketed")
    prefix_group = _integer(value["prefix_group"], 0, 1023, f"{context} prefix group")
    shared = _integer(
        value["shared_prefix_tokens"], 0, MAX_PROMPT_TOKENS, f"{context} shared prefix"
    )
    prompt = _integer(
        value["prompt_tokens"], 1, MAX_PROMPT_TOKENS, f"{context} prompt tokens"
    )
    if shared > prompt:
        raise TraceShapeError(f"{context} shared prefix exceeds prompt")
    history_turns = _integer(value["history_turns"], 0, 32, f"{context} history turns")
    tool_rounds = _integer(
        value["history_tool_rounds"], 0, 16, f"{context} history tool rounds"
    )
    if tool_rounds > history_turns:
        raise TraceShapeError(f"{context} tool rounds exceed history")
    history_calls = _integer(
        value["history_parallel_calls"], 0, 8, f"{context} history parallel calls"
    )
    if (tool_rounds == 0) != (history_calls == 0):
        raise TraceShapeError(f"{context} history tool shape is inconsistent")
    protocol = value["protocol"]
    if protocol not in PROTOCOLS:
        raise TraceShapeError(f"{context} protocol is invalid")
    if type(value["stream"]) is not bool:
        raise TraceShapeError(f"{context} stream must be boolean")
    expected_calls = _integer(
        value["expected_tool_calls"], 0, 8, f"{context} expected tool calls"
    )
    if protocol == "text" and expected_calls != 0:
        raise TraceShapeError(f"{context} text protocol cannot expect tools")
    if protocol in ("required_tool", "auto_tool") and expected_calls < 1:
        raise TraceShapeError(f"{context} tool protocol requires a call")
    if protocol == "parallel_tool" and expected_calls < 2:
        raise TraceShapeError(f"{context} parallel protocol requires two calls")
    maximum = _integer(
        value["max_output_tokens"], 1, MAX_OUTPUT_TOKENS, f"{context} output maximum"
    )
    observed = _integer(
        value["observed_completion_tokens"],
        0,
        MAX_OUTPUT_TOKENS,
        f"{context} observed completion",
    )
    if observed > maximum:
        raise TraceShapeError(f"{context} observed completion exceeds maximum")

    sampling = value["sampling"]
    _exact_fields(
        sampling,
        ("temperature", "top_p", "seed", "reasoning_effort"),
        f"{context} sampling",
    )
    effort = sampling["reasoning_effort"]
    if effort not in REASONING_EFFORTS:
        raise TraceShapeError(f"{context} reasoning effort is invalid")
    parsed_sampling = SamplingShape(
        temperature=_number(sampling["temperature"], 0, 2, f"{context} temperature"),
        top_p=_number(
            sampling["top_p"], 0, 1, f"{context} top_p", exclusive_minimum=True
        ),
        seed=_integer(sampling["seed"], 0, (1 << 31) - 1, f"{context} seed"),
        reasoning_effort=effort,
    )
    return TraceShape(
        arrival_offset_ms=arrival,
        prefix_group=prefix_group,
        shared_prefix_tokens=shared,
        prompt_tokens=prompt,
        history_turns=history_turns,
        history_tool_rounds=tool_rounds,
        history_parallel_calls=history_calls,
        protocol=protocol,
        stream=value["stream"],
        expected_tool_calls=expected_calls,
        max_output_tokens=maximum,
        observed_completion_tokens=observed,
        sampling=parsed_sampling,
    )


def _reject_json_constant(_value):
    raise TraceShapeError("trace contains a non-finite JSON number")


def _reject_duplicate_fields(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise TraceShapeError("trace contains duplicate JSON fields")
        value[key] = item
    return value


def _open_private_trace(path):
    path = pathlib.Path(path)
    try:
        parent = path.parent
        parent_stat = parent.stat(follow_symlinks=False)
        if (
            not stat.S_ISDIR(parent_stat.st_mode)
            or parent_stat.st_uid != os.getuid()
            or stat.S_IMODE(parent_stat.st_mode) != 0o700
        ):
            raise TraceShapeError("trace parent must be owner-only mode 0700")
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except TraceShapeError:
        raise
    except OSError as error:
        raise TraceShapeError("trace file cannot be opened safely") from error
    try:
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.getuid()
            or stat.S_IMODE(metadata.st_mode) != 0o600
            or metadata.st_nlink != 1
            or not 0 < metadata.st_size <= MAX_TRACE_BYTES
        ):
            raise TraceShapeError("trace file envelope is unsafe")
        return os.fdopen(descriptor, encoding="utf-8")
    except Exception:
        os.close(descriptor)
        raise


def load_trace_shapes(path):
    shapes = []
    try:
        with _open_private_trace(path) as source:
            for line_number, raw in enumerate(source, 1):
                if len(raw.encode("utf-8")) > 65_536:
                    raise TraceShapeError(f"trace line {line_number} is too large")
                if not raw.strip():
                    continue
                if len(shapes) >= MAX_TRACE_RECORDS:
                    raise TraceShapeError("trace has too many records")
                try:
                    value = json.loads(
                        raw,
                        parse_constant=_reject_json_constant,
                        object_pairs_hook=_reject_duplicate_fields,
                    )
                except TraceShapeError:
                    raise
                except (json.JSONDecodeError, RecursionError) as error:
                    raise TraceShapeError(
                        f"trace line {line_number} is invalid JSON"
                    ) from error
                shapes.append(parse_shape(value, line_number))
    except UnicodeError as error:
        raise TraceShapeError("trace is not valid UTF-8") from error
    if not shapes:
        raise TraceShapeError("trace is empty")
    if shapes[0].arrival_offset_ms != 0:
        raise TraceShapeError("first trace arrival must be zero")
    if any(
        right.arrival_offset_ms < left.arrival_offset_ms
        for left, right in zip(shapes, shapes[1:])
    ):
        raise TraceShapeError("trace arrivals must be nondecreasing")
    first_seen = []
    for shape in shapes:
        if shape.prefix_group not in first_seen:
            first_seen.append(shape.prefix_group)
    if first_seen != list(range(len(first_seen))):
        raise TraceShapeError("prefix groups must be densely renumbered by first appearance")
    if sum(shape.prompt_tokens for shape in shapes) > MAX_TOTAL_PROMPT_TOKENS:
        raise TraceShapeError("trace prompt-token budget is too large")
    if sum(shape.max_output_tokens for shape in shapes) > MAX_TOTAL_OUTPUT_TOKENS:
        raise TraceShapeError("trace output-token budget is too large")
    return shapes


def _digest_namespace(salt, prefix_group):
    material = f"agent-trace-v1:{salt}:{prefix_group}".encode()
    return hashlib.blake2b(material, digest_size=16).hexdigest()


def synthetic_prefix(salt, prefix_group, target_tokens):
    namespace = _digest_namespace(salt, prefix_group)
    marker = f"{namespace} synthetic sovereign cache namespace; ignore."
    return marker + " x" * target_tokens


def _tools(count):
    return [
        {
            "type": "function",
            "function": {
                "name": f"shape_tool_{index}",
                "description": "Record one synthetic trace-shape value.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": False,
                    "properties": {
                        "value": {"type": "string"},
                        "index": {"type": "number"},
                    },
                    "required": ["value", "index"],
                },
            },
        }
        for index in range(count)
    ]


def build_case(shape, ordinal, salt, tail_adjustment=0):
    messages = [
        {
            "role": "system",
            "content": synthetic_prefix(salt, shape.prefix_group, shape.shared_prefix_tokens),
        }
    ]
    declared_tools = max(shape.expected_tool_calls, shape.history_parallel_calls)
    for turn in range(shape.history_turns):
        messages.append(
            {"role": "user", "content": f"Synthetic historical request {turn}."}
        )
        if turn < shape.history_tool_rounds:
            calls = []
            for index in range(shape.history_parallel_calls):
                call_id = f"shape_history_{turn}_{index}"
                calls.append(
                    {
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": f"shape_tool_{index}",
                            "arguments": json.dumps(
                                {"value": f"synthetic-{turn}-{index}", "index": index},
                                separators=(",", ":"),
                            ),
                        },
                    }
                )
            messages.append(
                {
                    "role": "assistant",
                    "content": None,
                    "reasoning_content": "Synthetic reasoning retained for replay shape.",
                    "tool_calls": calls,
                }
            )
            for index in range(shape.history_parallel_calls):
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": f"shape_history_{turn}_{index}",
                        "name": f"shape_tool_{index}",
                        "content": '{"status":"synthetic-ok"}',
                    }
                )
        else:
            messages.append(
                {"role": "assistant", "content": "Synthetic historical acknowledgement."}
            )

    fixed_estimate = shape.shared_prefix_tokens + 16 + 24 * shape.history_turns
    tail_tokens = max(0, shape.prompt_tokens - fixed_estimate + tail_adjustment)
    if shape.protocol == "text":
        instruction = "Reply with exactly: trace replay complete"
    else:
        instruction = (
            "Call exactly these synthetic tools once each: "
            + ", ".join(
                f"shape_tool_{index}" for index in range(shape.expected_tool_calls)
            )
            + ". Give each a distinct value and its numeric index. Do not answer in prose."
        )
    messages.append({"role": "user", "content": " y" * tail_tokens + "\n" + instruction})

    request = {
        "stream": shape.stream,
        "max_tokens": shape.max_output_tokens,
        "reasoning_effort": shape.sampling.reasoning_effort,
        "messages": messages,
    }
    expected = {
        "mode": "text" if shape.protocol == "text" else "tool_calls",
        "finish_reasons": ["stop", "length", "tool_calls"],
    }
    if shape.protocol == "text":
        expected.update({"content_contains": "trace replay complete", "max_tool_calls": 0})
        if declared_tools:
            request["tools"] = _tools(declared_tools)
            request["tool_choice"] = "none"
    else:
        request["tools"] = _tools(declared_tools)
        request["tool_choice"] = "auto" if shape.protocol == "auto_tool" else "required"
        request["parallel_tool_calls"] = shape.expected_tool_calls > 1
        expected.update(
            {
                "min_tool_calls": shape.expected_tool_calls,
                "max_tool_calls": shape.expected_tool_calls,
                "tool_names": [
                    f"shape_tool_{index}" for index in range(shape.expected_tool_calls)
                ],
                "argument_types": {"value": "string", "index": "number"},
                "unique_arguments": ["index"],
            }
        )
    return {
        "schema_version": 1,
        "id": f"shape-{ordinal:04d}",
        "request": request,
        "expected": expected,
    }


def _structural_key(shape):
    return (
        shape.history_turns,
        shape.history_tool_rounds,
        shape.history_parallel_calls,
        shape.protocol,
        shape.stream,
        shape.expected_tool_calls,
        shape.sampling.reasoning_effort,
    )


def _calibration_probe(shape):
    fixed_estimate = 16 + 24 * shape.history_turns
    return dataclasses.replace(
        shape,
        prefix_group=0,
        shared_prefix_tokens=0,
        prompt_tokens=max(1, fixed_estimate),
    )


def tokenize_count(base, model, token, case, timeout):
    payload = copy.deepcopy(case["request"])
    payload.pop("stream", None)
    payload.pop("max_tokens", None)
    effort = payload.get("reasoning_effort")
    template_kwargs = dict(payload.get("chat_template_kwargs") or {})
    if effort is not None:
        template_kwargs["reasoning_effort"] = effort
        template_kwargs.setdefault("enable_thinking", effort != "none")
    if template_kwargs:
        payload["chat_template_kwargs"] = template_kwargs
    payload.update(
        {"model": model, "add_generation_prompt": True, "return_token_strs": False}
    )
    request = urllib.request.Request(
        base.rstrip("/") + "/tokenize",
        data=json.dumps(payload, separators=(",", ":")).encode(),
        headers={
            "Authorization": "Bearer " + token,
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read(MAX_CALIBRATION_RESPONSE_BYTES + 1)
    except urllib.error.HTTPError as error:
        error.read(4096)
        raise TraceShapeError("tokenization calibration returned HTTP error") from None
    except Exception as error:
        raise TraceShapeError("tokenization calibration failed") from error
    if len(body) > MAX_CALIBRATION_RESPONSE_BYTES:
        raise TraceShapeError("tokenization calibration response is too large")
    try:
        result = json.loads(body)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise TraceShapeError("tokenization calibration response is invalid") from error
    count = result.get("count") if isinstance(result, dict) else None
    if type(count) is not int or not 1 <= count <= MAX_PROMPT_TOKENS:
        raise TraceShapeError("tokenization calibration count is invalid")
    return count


def calibrate_cases(args, shapes):
    adjustments = {}
    maximum_delta = 0
    for shape in shapes:
        key = _structural_key(shape)
        if key in adjustments:
            continue
        probe = _calibration_probe(shape)
        probe_case = build_case(probe, 0, args.salt)
        actual = tokenize_count(
            args.base,
            args.model,
            args.token,
            probe_case,
            args.tokenize_timeout,
        )
        delta = actual - probe.prompt_tokens
        if not 0 <= delta <= 4096:
            raise TraceShapeError("tokenization calibration delta is out of range")
        adjustments[key] = -delta
        maximum_delta = max(maximum_delta, delta)
    cases = [
        build_case(shape, ordinal, args.salt, adjustments[_structural_key(shape)])
        for ordinal, shape in enumerate(shapes)
    ]
    return cases, {
        "calibration_profiles": len(adjustments),
        "calibration_max_overhead_tokens": maximum_delta,
    }


def _sampling_class(sampling):
    if sampling.temperature == 0 and sampling.top_p == 1:
        return "deterministic"
    if sampling.temperature == 1 and sampling.top_p == 0.95:
        return "agentic"
    return "other"


def _bucket(value, boundaries):
    for boundary in boundaries:
        if value <= boundary:
            return str(boundary)
    return "overflow"


def summarize_shapes(shapes):
    protocol_counts = {protocol: 0 for protocol in PROTOCOLS}
    sampling_counts = {kind: 0 for kind in ("deterministic", "agentic", "other")}
    reasoning_counts = {effort: 0 for effort in REASONING_EFFORTS}
    prompt_buckets = {str(boundary): 0 for boundary in PROMPT_BUCKETS}
    prompt_buckets["overflow"] = 0
    for shape in shapes:
        protocol_counts[shape.protocol] += 1
        sampling_counts[_sampling_class(shape.sampling)] += 1
        reasoning_counts[shape.sampling.reasoning_effort] += 1
        prompt_buckets[_bucket(shape.prompt_tokens, PROMPT_BUCKETS)] += 1
    return {
        "records": len(shapes),
        "prefix_groups": len({shape.prefix_group for shape in shapes}),
        "protocol_counts": protocol_counts,
        "sampling_counts": sampling_counts,
        "reasoning_counts": reasoning_counts,
        "prompt_token_buckets": prompt_buckets,
        "arrival_span_bucket_ms": _bucket(shapes[-1].arrival_offset_ms, ARRIVAL_BUCKETS),
    }


def _execute_shape(args, shape, ordinal, case, queued_at):
    began = time.perf_counter()
    sampling = {
        "temperature": shape.sampling.temperature,
        "top_p": shape.sampling.top_p,
        "seed": shape.sampling.seed,
        "reasoning_effort": shape.sampling.reasoning_effort,
    }
    result = execute_case(
        args.base,
        args.model,
        args.token,
        case,
        sampling,
        args.timeout,
        ordinal,
    )
    protocol_valid = result.get("ok", False)
    actual = result.get("prompt_tokens")
    allowed_delta = max(
        args.prompt_token_tolerance_min,
        math.ceil(shape.prompt_tokens * args.prompt_token_tolerance_pct / 100),
    )
    delta = actual - shape.prompt_tokens if type(actual) is int else None
    shape_valid = delta is not None and abs(delta) <= allowed_delta
    result.update(
        {
            "shape_ordinal": ordinal,
            "protocol_valid": protocol_valid,
            "shape_valid": shape_valid,
            "ok": protocol_valid and shape_valid,
            "target_prompt_tokens": shape.prompt_tokens,
            "prompt_token_delta": delta,
            "target_completion_tokens": shape.observed_completion_tokens,
            "client_queue_ms": round((began - queued_at) * 1000, 1),
        }
    )
    result.pop("case", None)
    result.pop("repetition", None)
    return result


def command_validate(args):
    shapes = load_trace_shapes(args.trace)
    print(json.dumps({"schema_version": 1, "valid": True, **summarize_shapes(shapes)}, sort_keys=True))
    return 0


def command_run(args):
    token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
    if not token:
        raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
    args.token = token
    shapes = load_trace_shapes(args.trace)
    metadata = load_metadata(args.metadata_json)
    cases, calibration = calibrate_cases(args, shapes)
    started = time.perf_counter()
    futures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        for ordinal, (shape, case) in enumerate(zip(shapes, cases)):
            due = started + shape.arrival_offset_ms / 1000
            remaining = due - time.perf_counter()
            if remaining > 0:
                time.sleep(remaining)
            queued_at = time.perf_counter()
            futures.append(
                pool.submit(_execute_shape, args, shape, ordinal, case, queued_at)
            )
        records = [future.result() for future in futures]
    elapsed = time.perf_counter() - started

    for record in records:
        print(
            json.dumps(
                {"type": "request", "metadata": metadata, "label": args.label, **record},
                sort_keys=True,
            )
        )
    good = [record for record in records if record["ok"]]
    protocol_good = [record for record in records if record["protocol_valid"]]
    shape_good = [record for record in records if record["shape_valid"]]
    ttfts = [record["ttft_ms"] for record in records if record.get("ttft_ms") is not None]
    itls = [record["mean_itl_ms"] for record in records if record.get("mean_itl_ms") is not None]
    completion = sum(record.get("completion_tokens", 0) for record in records)
    prompt = sum(record.get("prompt_tokens", 0) for record in records)
    cached = sum(record.get("cached_tokens", 0) for record in records)
    summary = {
        "type": "summary",
        "schema_version": 1,
        "metadata": metadata,
        "label": args.label,
        **summarize_shapes(shapes),
        **calibration,
        "concurrency": args.concurrency,
        "requests": len(records),
        "protocol_valid": len(protocol_good),
        "shape_valid": len(shape_good),
        "successful": len(good),
        "ttft_ms_p95": percentile(ttfts, 0.95),
        "mean_itl_ms_median": (
            round(statistics.median(itls), 2) if itls else None
        ),
        "output_tok_s": round(completion / elapsed, 1) if elapsed else None,
        "cache_hit_pct": round(100 * cached / prompt, 1) if prompt else None,
        "route_counts": bounded_route_counts(records),
        "successful_tasks_per_gpu_hour": (
            round(len(good) * 3600 / elapsed / int(metadata["gpu_count"]), 1)
            if elapsed and int(metadata["gpu_count"]) > 0
            else None
        ),
        "wall_seconds": round(elapsed, 3),
    }
    print(json.dumps(summary, sort_keys=True))
    return 0 if len(good) == len(records) else 1


def parser():
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate", help="validate a private content-free shape file")
    validate.add_argument("trace", type=pathlib.Path)
    validate.set_defaults(handler=command_validate)

    run = commands.add_parser("run", help="replay a private shape file synthetically")
    run.add_argument("base")
    run.add_argument("model")
    run.add_argument("trace", type=pathlib.Path)
    run.add_argument("--metadata-json", required=True, type=pathlib.Path)
    run.add_argument("--salt", required=True)
    run.add_argument("--label", default="agent-trace")
    run.add_argument("--concurrency", type=int, default=32)
    run.add_argument("--timeout", type=int, default=900)
    run.add_argument("--tokenize-timeout", type=int, default=30)
    run.add_argument("--prompt-token-tolerance-pct", type=float, default=5.0)
    run.add_argument("--prompt-token-tolerance-min", type=int, default=256)
    run.set_defaults(handler=command_run)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    if not 1 <= getattr(args, "concurrency", 1) <= 128:
        raise SystemExit("concurrency must be between 1 and 128")
    if not 0 <= getattr(args, "prompt_token_tolerance_pct", 0) <= 25:
        raise SystemExit("prompt-token-tolerance-pct must be between 0 and 25")
    if not 0 <= getattr(args, "prompt_token_tolerance_min", 0) <= 4096:
        raise SystemExit("prompt-token-tolerance-min must be between 0 and 4096")
    if not 1 <= getattr(args, "tokenize_timeout", 1) <= 120:
        raise SystemExit("tokenize-timeout must be between 1 and 120")
    return args.handler(args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except TraceShapeError as error:
        raise SystemExit(str(error)) from None
