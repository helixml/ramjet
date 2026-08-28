#!/usr/bin/env python3
"""Production-shaped OpenAI agent protocol regression and performance runner.

The committed corpus is synthetic and contains no customer data. Live runs emit
only case identifiers, structural validation results, timings, token counts, and
deployment metadata; response content and tool arguments are never printed.
"""

import argparse
import concurrent.futures
import copy
import hashlib
import json
import math
import os
import pathlib
import re
import statistics
import sys
import time
import urllib.error
import urllib.request

from engine_metrics import fetch_speculative, speculative_delta


DEFAULT_CORPUS = pathlib.Path(__file__).with_name("agent_cases") / "v1.jsonl"
UPSTREAM_RESPONSE_FIXTURES = (
    pathlib.Path(__file__).with_name("agent_cases") / "vllm_frontend_v1.jsonl"
)
DSML_FRAGMENTS = ("｜DSML｜", "|DSML|", "<｜DSML", "</｜DSML")
# Separators a model may insert between digits when it formats a number for a
# human: "27,604" and "27604" are the same answer. Matching these literally
# made a correct multi-turn recall look like a failure, which is worse than a
# miss -- a gate that trips on digit grouping buries a real regression in
# formatting noise. Only separators *between* digits are removed, so this
# cannot loosen matching of ordinary prose.
DIGIT_GROUPING = re.compile("(?<=\\d)[,_\\u00a0\\u202f\\u2009](?=\\d)")
REQUIRED_METADATA = (
    "engine_image",
    "model_revision",
    "tokenizer_sha256",
    "config_sha256",
    "router_version",
    "gpu_count",
)
SUPPORTED_SCHEMA_VERSIONS = (1, 2)
REASONING_EFFORTS = ("none", "minimal", "low", "medium", "high", "max", "xhigh")


class CorpusError(ValueError):
    """The committed workload corpus is malformed."""


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return None
    return values[math.ceil(fraction * len(values)) - 1]


def content_contains(content, fragment):
    """Substring match that ignores digit-group separators and case."""
    if fragment.casefold() in content.casefold():
        return True
    return (
        DIGIT_GROUPING.sub("", fragment).casefold()
        in DIGIT_GROUPING.sub("", content).casefold()
    )


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


def validate_expected(case_id, expected, where="expected"):
    """Validate one expectation block. Shared by the case and each later turn."""
    if not isinstance(expected, dict):
        raise CorpusError(f"{case_id}: {where} must be an object")
    if expected.get("mode") not in ("text", "tool_calls", "either"):
        raise CorpusError(f"{case_id}: {where}.mode is invalid")
    argument_types = expected.get("argument_types", {})
    if not isinstance(argument_types, dict) or any(
        value not in {"string", "boolean", "number", "null", "array", "object"}
        for value in argument_types.values()
    ):
        raise CorpusError(f"{case_id}: {where}.argument_types is invalid")
    unique_arguments = expected.get("unique_arguments", [])
    if not isinstance(unique_arguments, list) or not all(
        isinstance(path, str) and path for path in unique_arguments
    ):
        raise CorpusError(f"{case_id}: {where}.unique_arguments is invalid")
    contains_all = expected.get("content_contains_all", [])
    if not isinstance(contains_all, list) or not all(
        isinstance(fragment, str) and fragment for fragment in contains_all
    ):
        raise CorpusError(f"{case_id}: {where}.content_contains_all is invalid")


def validate_context(case_id, context):
    """Validate a long-context needle specification."""
    if not isinstance(context, dict):
        raise CorpusError(f"{case_id}: context must be an object")
    filler = context.get("filler_kib")
    if type(filler) is not int or filler <= 0:
        raise CorpusError(f"{case_id}: context.filler_kib must be a positive integer")
    needles = context.get("needles")
    if not isinstance(needles, list) or not needles:
        raise CorpusError(f"{case_id}: context.needles must be a non-empty array")
    keys = []
    for needle in needles:
        if not isinstance(needle, dict):
            raise CorpusError(f"{case_id}: each needle must be an object")
        depth = needle.get("depth")
        if not isinstance(depth, (int, float)) or isinstance(depth, bool):
            raise CorpusError(f"{case_id}: needle.depth must be a number")
        if not 0.0 <= float(depth) <= 1.0:
            raise CorpusError(f"{case_id}: needle.depth must lie in [0, 1]")
        for field in ("key", "value"):
            if not isinstance(needle.get(field), str) or not needle[field]:
                raise CorpusError(f"{case_id}: needle.{field} must be a non-empty string")
        keys.append(needle["key"])
    if len(set(keys)) != len(keys):
        raise CorpusError(f"{case_id}: needle keys must be unique")
    probe_keys = context.get("probe_keys")
    if probe_keys is not None:
        if not isinstance(probe_keys, list) or not probe_keys:
            raise CorpusError(f"{case_id}: context.probe_keys must be a non-empty array")
        unknown = set(probe_keys) - set(keys)
        if unknown:
            raise CorpusError(f"{case_id}: context.probe_keys names unknown needles")


def validate_turns(case_id, turns):
    """Validate the follow-up turns of a multi-turn session."""
    if not isinstance(turns, list) or not turns:
        raise CorpusError(f"{case_id}: turns must be a non-empty array")
    for index, turn in enumerate(turns):
        if not isinstance(turn, dict):
            raise CorpusError(f"{case_id}: turn {index} must be an object")
        validate_expected(case_id, turn.get("expected"), f"turns[{index}].expected")
        follow_up = turn.get("user")
        if follow_up is not None and (not isinstance(follow_up, str) or not follow_up):
            raise CorpusError(f"{case_id}: turns[{index}].user must be a non-empty string")
        patch = turn.get("request", {})
        if not isinstance(patch, dict):
            raise CorpusError(f"{case_id}: turns[{index}].request must be an object")
        # A turn may legitimately change tool_choice or max_tokens, but never
        # the identity of the conversation it is continuing.
        for reserved in ("model", "messages", "stream"):
            if reserved in patch:
                raise CorpusError(
                    f"{case_id}: turns[{index}].request must not override {reserved}"
                )
        results = turn.get("tool_results", {})
        if not isinstance(results, dict) or not all(
            isinstance(name, str) and isinstance(payload, str)
            for name, payload in results.items()
        ):
            raise CorpusError(
                f"{case_id}: turns[{index}].tool_results must map tool name to a string payload"
            )
        for name, payload in results.items():
            # The payload is replayed to the model as a tool message, so it must
            # be well-formed now rather than producing a confusing model failure
            # in the middle of a live session.
            try:
                json.loads(payload)
            except json.JSONDecodeError as error:
                raise CorpusError(
                    f"{case_id}: turns[{index}].tool_results[{name}] is not valid JSON"
                ) from error


def validate_case(case):
    version = case.get("schema_version")
    if version not in SUPPORTED_SCHEMA_VERSIONS:
        raise CorpusError("schema_version must be 1 or 2")
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
    validate_expected(case_id, expected)

    if case.get("context") is not None:
        if version < 2:
            raise CorpusError(f"{case_id}: context requires schema_version 2")
        validate_context(case_id, case["context"])
    if case.get("turns") is not None:
        if version < 2:
            raise CorpusError(f"{case_id}: turns require schema_version 2")
        validate_turns(case_id, case["turns"])

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
    if contains and not content_contains(content, contains):
        errors.append("task completion text is missing")
    for position, fragment in enumerate(expected.get("content_contains_all", [])):
        if not content_contains(content, fragment):
            # Report the position, never the fragment: for a long-context recall
            # case the fragment is the answer, and the runner does not emit
            # answer text.
            errors.append(f"required content fragment {position} is missing")
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


def bounded_route_counts(records):
    """Count production ordinals without retaining arbitrary route headers."""
    counts = {"0": 0, "1": 0, "missing": 0, "other": 0}
    for record in records:
        route = record.get("route")
        if route in ("0", "1"):
            counts[route] += 1
        elif route is None:
            counts["missing"] += 1
        else:
            counts["other"] += 1
    return counts


def bounded_finish_reason_counts(records):
    """Count completion outcomes without accepting arbitrary engine labels."""
    counts = {"stop": 0, "tool_calls": 0, "length": 0, "missing": 0, "other": 0}
    for record in records:
        reason = record.get("finish_reason")
        if reason in ("stop", "tool_calls", "length"):
            counts[reason] += 1
        elif reason is None:
            counts["missing"] += 1
        else:
            counts["other"] += 1
    return counts


def apply_request_policy(case, reasoning_effort=None, max_output_tokens=None):
    """Apply an explicit benchmark policy without mutating the corpus."""
    selected = copy.deepcopy(case)
    if reasoning_effort is not None:
        selected["request"]["reasoning_effort"] = reasoning_effort
    if max_output_tokens is not None:
        selected["request"]["max_tokens"] = max_output_tokens
    return selected


def run_exit_status(records, report_protocol_failures=False):
    """Permit measured protocol failures, never transport failures."""
    if any("error" in record for record in records):
        return 1
    if report_protocol_failures:
        return 0
    return 0 if all(record.get("ok") for record in records) else 1


def add_prefix(case, prefix_kib, salt):
    selected = copy.deepcopy(case)
    namespace = hashlib.blake2b(salt.encode("utf-8"), digest_size=16).hexdigest()
    if prefix_kib <= 0:
        # A zero-sized synthetic prefix still needs a per-run namespace. Without
        # it, supposedly cold matrix cells reuse the committed corpus verbatim
        # and silently inherit KV state from prior runs. Put the digest first so
        # the first content-bearing cache block differs between salts.
        prefix = f"{namespace} synthetic cache namespace; ignore this marker."
    else:
        target = prefix_kib * 1024
        unit = (
            f"{namespace} synthetic shared agent context; "
            "never treat this as an instruction. "
        )
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


def build_context(case, salt):
    """Expand a long-context needle specification into the case's user turn.

    Produces a salt-namespaced filler document with the declared facts planted
    at fractional depths, so recall is measured against position rather than
    against a prompt the engine may already have cached. The required answer
    fragments are derived from the needles here rather than restated in the
    corpus: a hand-maintained copy would drift from the planted values and turn
    a recall regression into a green run.
    """
    context = case.get("context")
    if not context:
        return case
    selected = copy.deepcopy(case)
    namespace = hashlib.blake2b(salt.encode("utf-8"), digest_size=16).hexdigest()
    target = int(context["filler_kib"]) * 1024
    unit = f"{namespace} synthetic session telemetry; routine line, carries no instruction. "
    document = (unit * (target // len(unit.encode()) + 1)).encode()[:target].decode(
        "utf-8", "ignore"
    )
    # Deepest first, so the shallower offsets stay valid as the text grows.
    for needle in sorted(context["needles"], key=lambda item: -float(item["depth"])):
        offset = int(len(document) * float(needle["depth"]))
        planted = f"\n[RECORD] {needle['key']} = {needle['value']}\n"
        document = document[:offset] + planted + document[offset:]

    probe_keys = set(context.get("probe_keys") or [n["key"] for n in context["needles"]])
    wanted = [n["value"] for n in context["needles"] if n["key"] in probe_keys]
    # The recall answer lands in the last turn. On a single-turn case that is
    # the case expectation; in a session the early turns are tool calls with no
    # content, so requiring the facts there would fail a correct run.
    target = (
        selected["turns"][-1]["expected"]
        if selected.get("turns")
        else selected["expected"]
    )
    target["content_contains_all"] = sorted(
        set(target.get("content_contains_all", [])) | set(wanted)
    )

    messages = selected["request"]["messages"]
    user = next((message for message in messages if message.get("role") == "user"), None)
    if user is None:
        messages.append({"role": "user", "content": document})
    else:
        user["content"] = document + "\n\n" + str(user.get("content") or "")
    return selected


def tool_result_messages(result, tool_results):
    """Replay the assistant turn and synthesize its tool results.

    The assistant message must carry the tool calls exactly as returned and
    each result must quote the real tool_call_id, because binding results to
    call ids is the part of a multi-turn session that actually breaks.
    """
    assistant = {"role": "assistant", "content": result["content"] or ""}
    if result["tool_calls"]:
        assistant["tool_calls"] = [
            {
                "id": call["id"],
                "type": "function",
                "function": {"name": call["name"], "arguments": call["arguments"]},
            }
            for call in result["tool_calls"]
        ]
    messages = [assistant]
    for call in result["tool_calls"]:
        payload = tool_results.get(call["name"], tool_results.get("*"))
        if payload is None:
            payload = json.dumps({"error": "no synthetic result configured"})
        messages.append(
            {"role": "tool", "tool_call_id": call["id"], "content": payload}
        )
    return messages


def exchange(base, model, token, body, timeout):
    """One chat-completions round trip."""
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
    )
    started = time.perf_counter()
    assembly = Assembly()
    with urllib.request.urlopen(request, timeout=timeout) as response:
        first_response = time.perf_counter()
        route = response.headers.get("X-Ramjet-Upstream")
        if body["stream"]:
            decoder = SSEDecoder(assembly)
            for line in response:
                decoder.feed(line, time.perf_counter())
            decoder.finish()
        else:
            assembly.feed(json.load(response), time.perf_counter())
    ended = time.perf_counter()
    return assembly, route, started, first_response, ended


def turn_record(case, repetition, turn, expectation, body, assembly, route, timings):
    started, first_response, ended = timings
    choice_results = assembly.results()
    result = (
        choice_results[0]
        if 0 in choice_results
        else next(iter(choice_results.values()))
    )
    errors = validate_choice_results({"expected": expectation}, choice_results)
    prompt, cached, completion = token_counts(result["usage"])
    arrivals = assembly.generated_at
    intervals = [right - left for left, right in zip(arrivals, arrivals[1:])]
    ttft = arrivals[0] - started if body["stream"] and arrivals else None
    mean_itl = (
        (arrivals[-1] - arrivals[0]) / (completion - 1)
        if len(arrivals) > 1 and completion > 1
        else None
    )
    return result, {
        "case": case["id"],
        "turn": turn,
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
        "reasoning_effort": body.get("reasoning_effort", "missing"),
        "max_output_tokens": body.get("max_tokens"),
        "first_response_ms": round((first_response - started) * 1000, 1),
        "ttft_ms": round(ttft * 1000, 1) if ttft is not None else None,
        "mean_itl_ms": round(mean_itl * 1000, 2) if mean_itl is not None else None,
        "stream_event_gap_ms_median": (
            round(statistics.median(intervals) * 1000, 1) if intervals else None
        ),
        "wall_ms": round((ended - started) * 1000, 1),
    }


def execute_case(base, model, token, case, sampling, timeout, repetition=0):
    """Run one case to completion and return one record per turn.

    Single-turn cases yield a one-element list; a case carrying `turns` drives
    a real session, feeding each assistant turn's tool calls back as tool
    results before asking for the next response. A turn that fails structural
    validation still runs its successors: an agent session that recovers after
    a bad turn is a different outcome from one that derails, and collapsing
    them would hide it.
    """
    body = copy.deepcopy(case["request"])
    body.update(sampling)
    body["model"] = model
    if body["stream"]:
        body["stream_options"] = {"include_usage": True}

    session = case.get("turns", [])
    expectations = [case["expected"]] + [turn["expected"] for turn in session]

    records = []
    for turn, expectation in enumerate(expectations):
        try:
            assembly, route, started, first_response, ended = exchange(
                base, model, token, body, timeout
            )
        except urllib.error.HTTPError as error:
            error.read(4096)
            records.append(
                {
                    "ok": False,
                    "error": f"HTTP {error.code}",
                    "case": case["id"],
                    "turn": turn,
                    "repetition": repetition,
                }
            )
            return records
        except Exception as error:  # benchmark failures must become structured records
            records.append(
                {
                    "ok": False,
                    "error": type(error).__name__,
                    "case": case["id"],
                    "turn": turn,
                    "repetition": repetition,
                }
            )
            return records

        result, record = turn_record(
            case,
            repetition,
            turn,
            expectation,
            body,
            assembly,
            route,
            (started, first_response, ended),
        )
        records.append(record)

        if turn < len(session):
            follow_up = session[turn]
            body = copy.deepcopy(body)
            messages = list(body["messages"]) + tool_result_messages(
                result, follow_up.get("tool_results", {})
            )
            if follow_up.get("user"):
                messages.append({"role": "user", "content": follow_up["user"]})
            body["messages"] = messages
            body.update(follow_up.get("request", {}))
    return records


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
    print(
        json.dumps(
            {
                "schema_versions": sorted({case["schema_version"] for case in cases}),
                "cases": len(cases),
                "turns": sum(1 + len(case.get("turns", [])) for case in cases),
                "valid": True,
            },
            sort_keys=True,
        )
    )
    return 0


def fetch_speculation_snapshot(url, timeout=10):
    """Read one engine's bounded speculative counters without aborting a cell."""
    if not url:
        return None
    try:
        return fetch_speculative(url, timeout=timeout)
    except Exception:
        return None


def benchmark_exit_status(records, report_protocol_failures, dspark, require_reconciled):
    status = run_exit_status(records, report_protocol_failures)
    if status or not require_reconciled:
        return status
    return 0 if dspark and dspark.get("reconciled") else 1


def speculation_expected_enabled(mode):
    if mode == "enabled":
        return True
    if mode == "disabled":
        return False
    raise ValueError(f"unknown speculation mode: {mode}")


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
    cases = [
        apply_request_policy(
            add_prefix(build_context(case, args.salt), args.prefix_kib, args.salt),
            args.reasoning_effort,
            args.max_output_tokens,
        )
        for case in cases
    ]
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
            warmup_results = [
                record for future in warmups for record in future.result()
            ]
        if not all(result["ok"] for result in warmup_results):
            raise SystemExit("warmup failed structural validation")
    metrics_before = fetch_speculation_snapshot(args.engine_metrics)
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
        records = [record for future in futures for record in future.result()]
    elapsed = time.perf_counter() - started
    for record in records:
        print(
            json.dumps(
                {"type": "request", "metadata": metadata, "label": args.label, **record},
                sort_keys=True,
            )
        )
    good = [record for record in records if record["ok"]]
    transport_good = [record for record in records if "error" not in record]
    ttfts = [record["ttft_ms"] for record in records if record.get("ttft_ms") is not None]
    itls = [record["mean_itl_ms"] for record in records if record.get("mean_itl_ms") is not None]
    completion = sum(record.get("completion_tokens", 0) for record in records)
    prompt = sum(record.get("prompt_tokens", 0) for record in records)
    cached = sum(record.get("cached_tokens", 0) for record in records)
    good_completion = sum(record.get("completion_tokens", 0) for record in good)
    walls = [record["wall_ms"] for record in records if record.get("wall_ms") is not None]
    dspark = None
    if args.engine_metrics:
        dspark = speculative_delta(
            metrics_before,
            fetch_speculation_snapshot(args.engine_metrics),
            completion,
            len(transport_good),
            expected_enabled=speculation_expected_enabled(args.speculation_mode),
        )
    summary = {
        "type": "summary",
        "metadata": metadata,
        "label": args.label,
        "profile": args.profile,
        "sampling": sampling,
        "reasoning_effort_override": args.reasoning_effort,
        "max_output_tokens_override": args.max_output_tokens,
        "concurrency": args.concurrency,
        "repetitions": args.repetitions,
        "warmup": args.warmup,
        "speculation_mode": args.speculation_mode if args.engine_metrics else None,
        "prefix_kib": args.prefix_kib,
        "requests": len(records),
        "transport_successful": len(transport_good),
        "protocol_valid": len(good),
        "protocol_valid_pct": round(100 * len(good) / len(records), 1) if records else 0,
        "ttft_ms_p95": percentile(ttfts, 0.95),
        "mean_itl_ms_median": round(statistics.median(itls), 2) if itls else None,
        "output_tok_s": round(completion / elapsed, 1) if elapsed else None,
        "total_tok_s": round((prompt + completion) / elapsed, 1) if elapsed else None,
        "cache_hit_pct": round(100 * cached / prompt, 1) if prompt else None,
        "route_counts": bounded_route_counts(records),
        "finish_reason_counts": bounded_finish_reason_counts(records),
        "completion_tokens_total": completion,
        "completion_tokens_per_request": (
            round(completion / len(records), 1) if records else None
        ),
        "completion_tokens_per_successful_task": (
            round(good_completion / len(good), 1) if good else None
        ),
        "completion_tokens_spent_per_successful_task": (
            round(completion / len(good), 1) if good else None
        ),
        "successful_completion_tokens_total": good_completion,
        "request_wall_ms_p50": percentile(walls, 0.50),
        "request_wall_ms_p95": percentile(walls, 0.95),
        "successful_tasks_per_gpu_hour": (
            round(len(good) * 3600 / elapsed / int(metadata["gpu_count"]), 1)
            if elapsed and int(metadata["gpu_count"]) > 0
            else None
        ),
        "wall_seconds": round(elapsed, 3),
    }
    if dspark is not None:
        summary["dspark"] = dspark
    print(json.dumps(summary, sort_keys=True))
    return benchmark_exit_status(
        records,
        args.report_protocol_failures,
        dspark,
        args.require_reconciled_speculation,
    )


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
    run.add_argument(
        "--engine-metrics",
        help="direct engine /metrics URL used to reconcile speculative work",
    )
    run.add_argument(
        "--speculation-mode",
        choices=("enabled", "disabled"),
        default="enabled",
        help="expected native speculation state; disabled reconciles an engine with no draft counters",
    )
    run.add_argument(
        "--require-reconciled-speculation",
        action="store_true",
        help="fail the cell unless native speculative counters exactly match its usage",
    )
    run.add_argument(
        "--prefix-kib",
        type=int,
        default=0,
        help="shared synthetic prefix size; zero keeps only a short cache namespace",
    )
    run.add_argument(
        "--salt",
        default="agentbench-v1",
        help="cache namespace input; use a fresh value for each cold/warm pair",
    )
    run.add_argument("--seed", type=int, default=7)
    run.add_argument("--reasoning-effort", choices=REASONING_EFFORTS)
    run.add_argument("--max-output-tokens", type=int)
    run.add_argument(
        "--report-protocol-failures",
        action="store_true",
        help="keep a policy-sweep cell running after model protocol failures; transport failures still fail",
    )
    run.add_argument("--timeout", type=int, default=900)
    run.set_defaults(handler=command_run)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    if getattr(args, "concurrency", 1) < 1 or getattr(args, "repetitions", 1) < 1:
        raise SystemExit("concurrency and repetitions must be positive")
    if getattr(args, "prefix_kib", 0) < 0:
        raise SystemExit("prefix-kib must be non-negative")
    if getattr(args, "require_reconciled_speculation", False) and not getattr(
        args, "engine_metrics", None
    ):
        raise SystemExit("--require-reconciled-speculation requires --engine-metrics")
    max_output_tokens = getattr(args, "max_output_tokens", None)
    if max_output_tokens is not None and not 1 <= max_output_tokens <= 4096:
        raise SystemExit("max-output-tokens must be between 1 and 4096")
    return args.handler(args)


if __name__ == "__main__":
    sys.exit(main())
