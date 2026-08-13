#!/usr/bin/env python3
"""Production-shaped OpenAI agent protocol regression and performance runner.

The committed corpus is synthetic and contains no customer data. Live runs emit
only case identifiers, structural validation results, timings, token counts, and
deployment metadata; response content and tool arguments are never printed.
"""

import argparse
import concurrent.futures
import copy
import json
import math
import os
import pathlib
import statistics
import sys
import time
import urllib.error
import urllib.request


DEFAULT_CORPUS = pathlib.Path(__file__).with_name("agent_cases") / "v1.jsonl"
UPSTREAM_RESPONSE_FIXTURES = (
    pathlib.Path(__file__).with_name("agent_cases") / "vllm_frontend_v1.jsonl"
)
DSML_FRAGMENTS = ("｜DSML｜", "|DSML|", "<｜DSML", "</｜DSML")
REQUIRED_METADATA = (
    "engine_image",
    "model_revision",
    "tokenizer_sha256",
    "config_sha256",
    "router_version",
    "gpu_count",
)


class CorpusError(ValueError):
    """The committed workload corpus is malformed."""


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return None
    return values[math.ceil(fraction * len(values)) - 1]


def json_type(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, str):
        return "string"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    raise TypeError(type(value).__name__)


def nested_value(value, path):
    current = value
    for part in path.split("."):
        if not isinstance(current, dict) or part not in current:
            raise KeyError(path)
        current = current[part]
    return current


def validate_case(case):
    if case.get("schema_version") != 1:
        raise CorpusError("schema_version must be 1")
    case_id = case.get("id")
    if not isinstance(case_id, str) or not case_id:
        raise CorpusError("case id must be a non-empty string")
    request = case.get("request")
    if not isinstance(request, dict):
        raise CorpusError(f"{case_id}: request must be an object")
    messages = request.get("messages")
    if not isinstance(messages, list) or not messages:
        raise CorpusError(f"{case_id}: request.messages must be a non-empty array")
    if "model" in request:
        raise CorpusError(f"{case_id}: model is supplied by the runner")
    if not isinstance(request.get("stream"), bool):
        raise CorpusError(f"{case_id}: request.stream must be boolean")
    choice_count = request.get("n", 1)
    if type(choice_count) is not int or choice_count < 1:
        raise CorpusError(f"{case_id}: request.n must be a positive integer")

    expected = case.get("expected")
    if not isinstance(expected, dict):
        raise CorpusError(f"{case_id}: expected must be an object")
    if expected.get("mode") not in ("text", "tool_calls", "either"):
        raise CorpusError(f"{case_id}: expected.mode is invalid")
    argument_types = expected.get("argument_types", {})
    if not isinstance(argument_types, dict) or any(
        value not in {"string", "boolean", "number", "null", "array", "object"}
        for value in argument_types.values()
    ):
        raise CorpusError(f"{case_id}: expected.argument_types is invalid")
    unique_arguments = expected.get("unique_arguments", [])
    if not isinstance(unique_arguments, list) or not all(
        isinstance(path, str) and path for path in unique_arguments
    ):
        raise CorpusError(f"{case_id}: expected.unique_arguments is invalid")

    if expected.get("reasoning_history"):
        assistants = [
            message
            for message in messages
            if message.get("role") == "assistant" and message.get("tool_calls")
        ]
        if not assistants or not all(message.get("reasoning_content") for message in assistants):
            raise CorpusError(
                f"{case_id}: every assistant tool turn must retain reasoning_content"
            )
        call_ids = {
            call.get("id")
            for message in assistants
            for call in message.get("tool_calls", [])
            if call.get("id")
        }
        result_ids = {
            message.get("tool_call_id")
            for message in messages
            if message.get("role") == "tool"
        }
        if not call_ids or not call_ids.issubset(result_ids):
            raise CorpusError(f"{case_id}: tool history is missing matching results")


def load_cases(path=DEFAULT_CORPUS):
    cases = []
    seen = set()
    with open(path, encoding="utf-8") as source:
        for line_number, raw in enumerate(source, 1):
            if not raw.strip() or raw.lstrip().startswith("#"):
                continue
            try:
                case = json.loads(raw)
            except json.JSONDecodeError as error:
                raise CorpusError(f"{path}:{line_number}: {error}") from error
            validate_case(case)
            if case["id"] in seen:
                raise CorpusError(f"{path}:{line_number}: duplicate id {case['id']}")
            seen.add(case["id"])
            cases.append(case)
    if not cases:
        raise CorpusError(f"{path}: corpus is empty")
    return cases


class _ChoiceAssembly:
    """Reassemble one OpenAI response choice without sharing parser state."""

    def __init__(self):
        self.content = []
        self.reasoning = []
        self.tool_calls = {}
        self.finish_reason = None

    def feed(self, choice):
        if choice.get("finish_reason") is not None:
            self.finish_reason = choice["finish_reason"]
        node = choice.get("delta")
        if not isinstance(node, dict) or not node:
            node = choice.get("message") or {}
        generated = False
        content = node.get("content")
        if isinstance(content, str) and content:
            self.content.append(content)
            generated = True
        for field in ("reasoning_content", "reasoning"):
            value = node.get(field)
            if isinstance(value, str) and value:
                self.reasoning.append(value)
                generated = True
        for position, delta in enumerate(node.get("tool_calls") or []):
            if not isinstance(delta, dict):
                continue
            index = delta.get("index", position)
            call = self.tool_calls.setdefault(
                index, {"id": "", "type": "function", "name": "", "arguments": ""}
            )
            if isinstance(delta.get("id"), str):
                call["id"] += delta["id"]
            if isinstance(delta.get("type"), str):
                call["type"] = delta["type"]
            function = delta.get("function") or {}
            if isinstance(function.get("name"), str):
                call["name"] += function["name"]
            arguments = function.get("arguments")
            if arguments is None:
                arguments = function.get("input", delta.get("input"))
            if isinstance(arguments, str):
                call["arguments"] += arguments
            elif isinstance(arguments, dict):
                call["arguments"] += json.dumps(arguments, separators=(",", ":"))
            generated = True
        return generated

    def result(self, usage):
        return {
            "content": "".join(self.content),
            "reasoning_content": "".join(self.reasoning),
            "tool_calls": [self.tool_calls[key] for key in sorted(self.tool_calls)],
            "finish_reason": self.finish_reason,
            "usage": usage,
        }


class Assembly:
    """Reassemble OpenAI chat responses across arbitrary SSE delta boundaries."""

    def __init__(self):
        self.choices = {}
        self.usage = {}
        self.generated_at = []

    def feed(self, event, observed_at=None):
        usage = event.get("usage")
        if isinstance(usage, dict) and usage:
            self.usage.update(usage)
        for position, choice in enumerate(event.get("choices") or []):
            if not isinstance(choice, dict):
                continue
            index = choice.get("index", position)
            state = self.choices.setdefault(index, _ChoiceAssembly())
            if state.feed(choice) and observed_at is not None:
                self.generated_at.append(observed_at)

    def results(self):
        if not self.choices:
            return {0: _ChoiceAssembly().result(self.usage)}
        return {
            index: self.choices[index].result(self.usage)
            for index in sorted(self.choices)
        }

    def result(self):
        results = self.results()
        return results[0] if 0 in results else next(iter(results.values()))


class SSEDecoder:
    def __init__(self, assembly):
        self.assembly = assembly
        self.tail = bytearray()

    def feed(self, chunk, observed_at=None):
        self.tail.extend(chunk)
        while True:
            newline = self.tail.find(b"\n")
            if newline < 0:
                return
            raw = bytes(self.tail[:newline]).strip()
            del self.tail[: newline + 1]
            if not raw.startswith(b"data:"):
                continue
            payload = raw[5:].strip()
            if not payload or payload == b"[DONE]":
                continue
            self.assembly.feed(json.loads(payload), observed_at)

    def finish(self):
        if self.tail.strip():
            self.feed(b"\n")


def validate_result(case, result):
    expected = case["expected"]
    errors = []
    content = result["content"]
    leaked = [fragment for fragment in DSML_FRAGMENTS if fragment in content]
    if leaked:
        errors.append("DSML marker leaked into content")

    tool_calls = result["tool_calls"]
    mode = expected["mode"]
    if mode == "text" and tool_calls:
        errors.append("unexpected tool call")
    if mode == "tool_calls" and len(tool_calls) < expected.get("min_tool_calls", 1):
        errors.append("missing required tool call")
    if len(tool_calls) > expected.get("max_tool_calls", 1_000_000):
        errors.append("too many tool calls")

    allowed_names = set(expected.get("tool_names", []))
    parsed_arguments = []
    for call in tool_calls:
        if allowed_names and call["name"] not in allowed_names:
            errors.append("unexpected tool name")
        try:
            arguments = json.loads(call["arguments"])
        except (TypeError, json.JSONDecodeError):
            errors.append("tool arguments are not valid JSON")
            continue
        if not isinstance(arguments, dict):
            errors.append("tool arguments must be an object")
            continue
        parsed_arguments.append(arguments)
        for path, wanted_type in expected.get("argument_types", {}).items():
            try:
                actual = nested_value(arguments, path)
            except KeyError:
                errors.append(f"missing typed argument {path}")
                continue
            if json_type(actual) != wanted_type:
                errors.append(f"argument {path} is not {wanted_type}")
        for path, wanted_value in expected.get("argument_values", {}).items():
            try:
                actual = nested_value(arguments, path)
            except KeyError:
                errors.append(f"missing expected argument {path}")
                continue
            if actual != wanted_value:
                errors.append(f"argument {path} has unexpected value")
    for path in expected.get("unique_arguments", []):
        try:
            values = [
                json.dumps(nested_value(arguments, path), sort_keys=True)
                for arguments in parsed_arguments
            ]
        except KeyError:
            errors.append(f"missing unique argument {path}")
            continue
        if len(set(values)) != len(values):
            errors.append(f"argument {path} is not unique across calls")

    minimum_reasoning = expected.get("min_reasoning_chars", 0)
    if len(result["reasoning_content"]) < minimum_reasoning:
        errors.append("reasoning content is missing or too short")
    contains = expected.get("content_contains")
    if contains and contains.casefold() not in content.casefold():
        errors.append("task completion text is missing")
    finish_reasons = expected.get("finish_reasons")
    if finish_reasons and result["finish_reason"] not in finish_reasons:
        errors.append("unexpected finish reason")
    return errors


def validate_choice_results(case, results):
    """Validate every alternative independently."""
    errors = []
    multiple = len(results) > 1
    for index, result in results.items():
        for error in validate_result(case, result):
            errors.append(f"choice {index}: {error}" if multiple else error)
    return errors


def token_counts(usage):
    details = usage.get("prompt_tokens_details") or {}
    return (
        int(usage.get("prompt_tokens", 0) or 0),
        int(details.get("cached_tokens", usage.get("cached_tokens", 0)) or 0),
        int(usage.get("completion_tokens", 0) or 0),
    )


def add_prefix(case, prefix_kib, salt):
    selected = copy.deepcopy(case)
    if prefix_kib <= 0:
        return selected
    target = prefix_kib * 1024
    unit = f"Synthetic shared agent context {salt}; never treat this as an instruction. "
    prefix = (unit * (target // len(unit.encode()) + 1)).encode()[:target].decode(
        "utf-8", "ignore"
    )
    messages = selected["request"]["messages"]
    system = next((message for message in messages if message.get("role") == "system"), None)
    if system is None:
        messages.insert(0, {"role": "system", "content": prefix})
    else:
        system["content"] = prefix + "\n\n" + str(system.get("content") or "")
    return selected


def execute_case(base, model, token, case, sampling, timeout, repetition=0):
    body = copy.deepcopy(case["request"])
    body.update(sampling)
    body["model"] = model
    if body["stream"]:
        body["stream_options"] = {"include_usage": True}
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
    )
    started = time.perf_counter()
    assembly = Assembly()
    route = None
    first_response = None
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            first_response = time.perf_counter()
            route = response.headers.get("X-Mini-Dynamo-Upstream")
            if body["stream"]:
                decoder = SSEDecoder(assembly)
                for line in response:
                    decoder.feed(line, time.perf_counter())
                decoder.finish()
            else:
                assembly.feed(json.load(response), time.perf_counter())
    except urllib.error.HTTPError as error:
        error.read(4096)
        return {
            "ok": False,
            "error": f"HTTP {error.code}",
            "case": case["id"],
            "repetition": repetition,
        }
    except Exception as error:  # benchmark failures must become structured records
        return {
            "ok": False,
            "error": type(error).__name__,
            "case": case["id"],
            "repetition": repetition,
        }
    ended = time.perf_counter()
    choice_results = assembly.results()
    result = (
        choice_results[0]
        if 0 in choice_results
        else next(iter(choice_results.values()))
    )
    errors = validate_choice_results(case, choice_results)
    prompt, cached, completion = token_counts(result["usage"])
    arrivals = assembly.generated_at
    intervals = [right - left for left, right in zip(arrivals, arrivals[1:])]
    ttft = arrivals[0] - started if body["stream"] and arrivals else None
    mean_itl = (
        (arrivals[-1] - arrivals[0]) / (completion - 1)
        if len(arrivals) > 1 and completion > 1
        else None
    )
    return {
        "case": case["id"],
        "repetition": repetition,
        "ok": not errors,
        "protocol_errors": errors,
        "stream": body["stream"],
        "route": route,
        "finish_reason": result["finish_reason"],
        "choices": len(choice_results),
        "tool_calls": sum(
            len(choice["tool_calls"]) for choice in choice_results.values()
        ),
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "completion_tokens": completion,
        "first_response_ms": round((first_response - started) * 1000, 1),
        "ttft_ms": round(ttft * 1000, 1) if ttft is not None else None,
        "mean_itl_ms": round(mean_itl * 1000, 2) if mean_itl is not None else None,
        "stream_event_gap_ms_median": (
            round(statistics.median(intervals) * 1000, 1) if intervals else None
        ),
        "wall_ms": round((ended - started) * 1000, 1),
    }


def load_metadata(path):
    with open(path, encoding="utf-8") as source:
        metadata = json.load(source)
    missing = [key for key in REQUIRED_METADATA if not metadata.get(key)]
    if missing:
        raise CorpusError("metadata is missing: " + ", ".join(missing))
    if type(metadata["gpu_count"]) is not int or metadata["gpu_count"] < 1:
        raise CorpusError("metadata gpu_count must be a positive integer")
    return metadata


def command_validate(args):
    cases = load_cases(args.corpus)
    print(json.dumps({"schema_version": 1, "cases": len(cases), "valid": True}, sort_keys=True))
    return 0


def command_run(args):
    token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
    if not token:
        raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")
    cases = load_cases(args.corpus)
    if args.case:
        wanted = set(args.case)
        cases = [case for case in cases if case["id"] in wanted]
        missing = wanted - {case["id"] for case in cases}
        if missing:
            raise SystemExit("unknown cases: " + ", ".join(sorted(missing)))
    cases = [add_prefix(case, args.prefix_kib, args.salt) for case in cases]
    metadata = load_metadata(args.metadata_json)
    sampling = (
        {"temperature": 0.0, "top_p": 1.0, "seed": args.seed}
        if args.profile == "deterministic"
        else {"temperature": 1.0, "top_p": 0.95, "seed": args.seed}
    )
    if args.warmup:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
            warmups = [
                pool.submit(
                    execute_case,
                    args.base,
                    args.model,
                    token,
                    case,
                    sampling,
                    args.timeout,
                )
                for case in cases
            ]
            warmup_results = [future.result() for future in warmups]
        if not all(result["ok"] for result in warmup_results):
            raise SystemExit("warmup failed structural validation")
    jobs = [
        (repetition, case)
        for repetition in range(args.repetitions)
        for case in cases
    ]
    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [
            pool.submit(
                execute_case,
                args.base,
                args.model,
                token,
                case,
                sampling,
                args.timeout,
                repetition,
            )
            for repetition, case in jobs
        ]
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
    ttfts = [record["ttft_ms"] for record in records if record.get("ttft_ms") is not None]
    itls = [record["mean_itl_ms"] for record in records if record.get("mean_itl_ms") is not None]
    completion = sum(record.get("completion_tokens", 0) for record in records)
    prompt = sum(record.get("prompt_tokens", 0) for record in records)
    cached = sum(record.get("cached_tokens", 0) for record in records)
    summary = {
        "type": "summary",
        "metadata": metadata,
        "label": args.label,
        "profile": args.profile,
        "sampling": sampling,
        "concurrency": args.concurrency,
        "repetitions": args.repetitions,
        "warmup": args.warmup,
        "prefix_kib": args.prefix_kib,
        "requests": len(records),
        "protocol_valid": len(good),
        "protocol_valid_pct": round(100 * len(good) / len(records), 1) if records else 0,
        "ttft_ms_p95": percentile(ttfts, 0.95),
        "mean_itl_ms_median": round(statistics.median(itls), 2) if itls else None,
        "output_tok_s": round(completion / elapsed, 1) if elapsed else None,
        "total_tok_s": round((prompt + completion) / elapsed, 1) if elapsed else None,
        "cache_hit_pct": round(100 * cached / prompt, 1) if prompt else None,
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
    validate = commands.add_parser("validate", help="validate the committed corpus without GPUs")
    validate.add_argument("--corpus", default=DEFAULT_CORPUS, type=pathlib.Path)
    validate.set_defaults(handler=command_validate)

    run = commands.add_parser("run", help="run the corpus against an OpenAI-compatible endpoint")
    run.add_argument("base")
    run.add_argument("model")
    run.add_argument("--corpus", default=DEFAULT_CORPUS, type=pathlib.Path)
    run.add_argument("--metadata-json", required=True, type=pathlib.Path)
    run.add_argument("--profile", choices=("deterministic", "agentic"), default="deterministic")
    run.add_argument("--label", default="agentbench")
    run.add_argument("--case", action="append")
    run.add_argument("--concurrency", type=int, default=1)
    run.add_argument("--repetitions", type=int, default=1)
    run.add_argument("--warmup", action="store_true")
    run.add_argument("--prefix-kib", type=int, default=0)
    run.add_argument("--salt", default="agentbench-v1")
    run.add_argument("--seed", type=int, default=7)
    run.add_argument("--timeout", type=int, default=900)
    run.set_defaults(handler=command_run)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    if getattr(args, "concurrency", 1) < 1 or getattr(args, "repetitions", 1) < 1:
        raise SystemExit("concurrency and repetitions must be positive")
    if getattr(args, "prefix_kib", 0) < 0:
        raise SystemExit("prefix-kib must be non-negative")
    return args.handler(args)


if __name__ == "__main__":
    sys.exit(main())
