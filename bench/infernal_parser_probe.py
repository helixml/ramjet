#!/usr/bin/env python3
"""Run synthetic DS4 parser regressions against a vLLM source tree.

The probe imports vLLM's parser state machine, DeepSeek V4 config, and client
argument-prefix helper. It stubs heavyweight serving modules, so it needs
neither GPUs nor a vLLM Python environment. This is intentionally a source-tree
gate: point it at the exact composed source used to build an image.
"""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import importlib
import json
import pathlib
import re
import sys
import types
from typing import Any


DEFAULT_CASES = pathlib.Path(__file__).with_name("infernal_parser_cases") / "v1.jsonl"
PROFILES = frozenset({"r4", "pr49117", "complete"})
OUTCOME_FIELDS = (
    "tool_call_starts",
    "tool_call_ends",
    "open_at_eof",
    "args_json_valid",
    "duplicate_canonical_args",
    "dsml_content",
)
SOURCE_FILES = (
    "vllm/parser/deepseek_v4.py",
    "vllm/parser/engine/events.py",
    "vllm/parser/engine/incremental_lexer.py",
    "vllm/parser/engine/parser_engine.py",
    "vllm/parser/engine/parser_engine_config.py",
    "vllm/parser/engine/streaming_parser_engine.py",
    "vllm/parser/engine/token_id_scanner.py",
    "vllm/tool_parsers/utils.py",
)


class ProbeError(ValueError):
    pass


def load_cases(path: pathlib.Path = DEFAULT_CASES) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    seen: set[str] = set()
    with path.open(encoding="utf-8") as handle:
        for line_number, raw in enumerate(handle, 1):
            if not raw.strip():
                continue
            try:
                case = json.loads(raw)
            except json.JSONDecodeError as exc:
                raise ProbeError(f"{path}:{line_number}: invalid JSON: {exc}") from exc
            case_id = case.get("id")
            if case.get("schema_version") != 2:
                raise ProbeError(f"{path}:{line_number}: schema_version must be 2")
            if not isinstance(case_id, str) or not case_id:
                raise ProbeError(f"{path}:{line_number}: id must be a non-empty string")
            if case_id in seen:
                raise ProbeError(f"{path}:{line_number}: duplicate id {case_id!r}")
            seen.add(case_id)
            chunks = case.get("chunks")
            if not isinstance(chunks, list) or not chunks or not all(
                isinstance(chunk, str) for chunk in chunks
            ):
                raise ProbeError(f"{path}:{line_number}: chunks must be non-empty strings")
            string_arg_names = case.get("string_arg_names", [])
            if (
                not isinstance(string_arg_names, list)
                or len(string_arg_names) != len(set(string_arg_names))
                or not all(
                    isinstance(name, str) and name for name in string_arg_names
                )
            ):
                raise ProbeError(
                    f"{path}:{line_number}: string_arg_names must be unique non-empty strings"
                )
            expected = case.get("expected")
            if not isinstance(expected, dict) or set(expected) != PROFILES:
                raise ProbeError(
                    f"{path}:{line_number}: expected profiles must be {sorted(PROFILES)}"
                )
            for profile, outcome in expected.items():
                if not isinstance(outcome, dict):
                    raise ProbeError(
                        f"{path}:{line_number}: expected.{profile} must be an object"
                    )
                if set(outcome) - {*OUTCOME_FIELDS, "content"}:
                    raise ProbeError(
                        f"{path}:{line_number}: expected.{profile} has unknown fields"
                    )
                for field in OUTCOME_FIELDS:
                    expected_type = int if field in {
                        "tool_call_starts",
                        "tool_call_ends",
                    } else bool
                    if type(outcome.get(field)) is not expected_type:
                        raise ProbeError(
                            f"{path}:{line_number}: expected.{profile}.{field} "
                            f"must be {expected_type.__name__}"
                        )
                if "content" in outcome and not isinstance(outcome["content"], str):
                    raise ProbeError(
                        f"{path}:{line_number}: expected.{profile}.content must be a string"
                    )
            cases.append(case)
    if not cases:
        raise ProbeError(f"{path}: no cases")
    return cases


def _clear_vllm_modules() -> None:
    for name in tuple(sys.modules):
        if name == "vllm" or name.startswith("vllm."):
            del sys.modules[name]


def _package(name: str, path: pathlib.Path) -> None:
    package = types.ModuleType(name)
    package.__path__ = [str(path)]  # type: ignore[attr-defined]
    sys.modules[name] = package


def _module(name: str, **attributes: Any) -> None:
    module = types.ModuleType(name)
    for attribute, value in attributes.items():
        setattr(module, attribute, value)
    sys.modules[name] = module


class _ProtocolValue:
    def __init__(self, **kwargs: Any) -> None:
        self.__dict__.update(kwargs)


class _Logger:
    def debug(self, *_args: Any, **_kwargs: Any) -> None:
        pass


def load_parser(source_root: pathlib.Path):
    parser_dir = source_root / "vllm" / "parser"
    required = (
        parser_dir / "deepseek_v4.py",
        parser_dir / "engine" / "streaming_parser_engine.py",
    )
    missing = [str(path) for path in required if not path.is_file()]
    if missing:
        raise ProbeError(f"not a vLLM source root; missing: {', '.join(missing)}")

    _clear_vllm_modules()
    # The parser uses only regex APIs shared by Python's stdlib re module for
    # these literal DSML fixtures. Avoid making the probe install dependencies.
    sys.modules["regex"] = re
    _package("vllm", source_root / "vllm")
    _package("vllm.parser", parser_dir)
    _package("vllm.parser.engine", parser_dir / "engine")
    _package("vllm.tool_parsers", source_root / "vllm" / "tool_parsers")

    _package("vllm.entrypoints", source_root / "vllm" / "entrypoints")
    _package("vllm.entrypoints.openai", source_root / "vllm" / "entrypoints/openai")
    _package(
        "vllm.entrypoints.openai.engine",
        source_root / "vllm" / "entrypoints/openai/engine",
    )
    _module(
        "vllm.entrypoints.chat_utils",
        get_tool_call_id_type=lambda *_args, **_kwargs: "random",
        make_tool_call_id=lambda *_args, **_kwargs: "fixture",
    )
    _module(
        "vllm.entrypoints.openai.engine.protocol",
        **{
            name: _ProtocolValue
            for name in (
                "DeltaFunctionCall",
                "DeltaMessage",
                "DeltaToolCall",
                "ExtractedToolCallInformation",
                "FunctionCall",
                "ToolCall",
            )
        },
    )
    _module("vllm.logger", init_logger=lambda *_args, **_kwargs: _Logger())
    _module(
        "vllm.parser.abstract_parser",
        Parser=object,
        StreamState=_ProtocolValue,
    )
    _module(
        "vllm.tool_parsers.utils",
        coerce_to_schema_type=lambda value, *_args: value,
        collect_tool_names=lambda *_args: set(),
        extract_types_from_schema=lambda *_args: set(),
        find_tool_name=lambda *_args: None,
        find_tool_properties=lambda *_args: {},
    )

    parser_engine = importlib.import_module("vllm.parser.engine.parser_engine")
    deepseek = importlib.import_module("vllm.parser.deepseek_v4")
    streaming = importlib.import_module(
        "vllm.parser.engine.streaming_parser_engine"
    )
    return deepseek, streaming, parser_engine


def source_identity(source_root: pathlib.Path) -> str:
    digest = hashlib.sha256()
    for relative in SOURCE_FILES:
        path = source_root / relative
        if not path.is_file():
            raise ProbeError(f"not a complete parser source tree; missing: {path}")
        digest.update(relative.encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return "sha256:" + digest.hexdigest()


def _client_argument_shape(
    parser_engine,
    config,
    event_batches: list[list[Any]],
    string_arg_names: set[str],
) -> tuple[bool, bool]:
    """Reassemble client-visible argument deltas without retaining values."""
    slots: dict[int, dict[str, Any]] = {}

    def slot(index: int) -> dict[str, Any]:
        return slots.setdefault(
            index,
            {
                "name": "",
                "raw": "",
                "name_sent": False,
                "streamed": "",
                "client": "",
            },
        )

    def converted_delta(current: dict[str, Any], partial: bool) -> str | None:
        converter = config.arg_converter
        if converter is None:
            return None
        try:
            converted = converter(current["raw"], partial)
        except (json.JSONDecodeError, TypeError, ValueError):
            return None
        if not converted:
            return None
        previous = current["streamed"]
        if partial:
            converted = parser_engine.ParserEngine._safe_arg_prefix(
                converted, string_arg_names or None
            )
        if not converted or converted == previous:
            return None
        if previous and not converted.startswith(previous):
            return None
        delta = converted[len(previous) :]
        if delta:
            current["streamed"] = converted
            return delta
        return None

    for batch in event_batches:
        for event in batch:
            kind = event.type.name
            if kind == "TOOL_CALL_START":
                slot(event.tool_index)
            elif kind == "TOOL_NAME":
                slot(event.tool_index)["name"] += event.value
            elif kind == "ARG_VALUE_CHUNK":
                current = slot(event.tool_index)
                current["raw"] += event.value
                if not current["name_sent"]:
                    if current["name"]:
                        current["name_sent"] = True
                elif event.value:
                    structural = config.arg_structural_chars
                    if structural is None or not structural.isdisjoint(event.value):
                        delta = converted_delta(current, partial=True)
                        if delta:
                            current["client"] += delta
            elif kind == "TOOL_CALL_END":
                current = slot(event.tool_index)
                remaining = converted_delta(current, partial=False)
                if not current["name_sent"] and current["name"]:
                    current["name_sent"] = True
                if current["name_sent"] and remaining:
                    current["client"] += remaining

    canonical: list[str] = []
    valid = True
    for current in slots.values():
        if not current["name_sent"]:
            continue
        try:
            parsed = json.loads(current["client"])
        except (json.JSONDecodeError, TypeError):
            valid = False
            continue
        if not isinstance(parsed, dict):
            valid = False
            continue
        canonical.append(json.dumps(parsed, sort_keys=True, separators=(",", ":")))
    return valid, len(canonical) != len(set(canonical))


def run_case(source_root: pathlib.Path, case: dict[str, Any]) -> dict[str, Any]:
    deepseek, streaming, parser_engine = load_parser(source_root)
    config = deepseek.deepseek_v4_config(thinking=False)
    engine = streaming.StreamingParserEngine(
        config, tokenizer=None
    )
    if hasattr(engine, "allowed_tool_names"):
        engine.allowed_tool_names = frozenset(case.get("allowed_tool_names", [])) or None
    if hasattr(engine, "suppress_tool_calls"):
        engine.suppress_tool_calls = bool(case.get("suppress_tool_calls", False))

    event_batches = []
    for chunk in case["chunks"]:
        event_batches.append(engine.feed(chunk, ()))
    before_finish = [event for batch in event_batches for event in batch]
    starts_at_eof = Counter(
        event.tool_index
        for event in before_finish
        if event.type.name == "TOOL_CALL_START"
    )
    ends_at_eof = Counter(
        event.tool_index
        for event in before_finish
        if event.type.name == "TOOL_CALL_END"
    )
    open_at_eof = any(
        starts_at_eof[index] > ends_at_eof[index] for index in starts_at_eof
    )
    event_batches.append(engine.finish())
    events = [event for batch in event_batches for event in batch]

    content = "".join(
        event.value for event in events if event.type.name == "TEXT_CHUNK"
    )
    args_json_valid, duplicate_canonical_args = _client_argument_shape(
        parser_engine,
        config,
        event_batches,
        set(case.get("string_arg_names", [])),
    )
    return {
        "tool_call_starts": sum(
            event.type.name == "TOOL_CALL_START" for event in events
        ),
        "tool_call_ends": sum(
            event.type.name == "TOOL_CALL_END" for event in events
        ),
        "open_at_eof": open_at_eof,
        "args_json_valid": args_json_valid,
        "duplicate_canonical_args": duplicate_canonical_args,
        "dsml_content": "｜DSML｜" in content,
        "content": content,
    }


def compare(outcome: dict[str, Any], expected: dict[str, Any]) -> list[str]:
    errors = []
    for key in OUTCOME_FIELDS:
        if outcome[key] != expected[key]:
            errors.append(f"{key}: got {outcome[key]!r}, want {expected[key]!r}")
    if "content" in expected and outcome["content"] != expected["content"]:
        errors.append("content mismatch")
    return errors


def command_validate(cases_path: pathlib.Path) -> int:
    cases = load_cases(cases_path)
    print(json.dumps({"cases": len(cases), "profiles": sorted(PROFILES)}))
    return 0


def command_run(
    source_root: pathlib.Path,
    profile: str,
    cases_path: pathlib.Path,
    expected_source_id: str | None,
) -> int:
    identity = source_identity(source_root)
    if expected_source_id is not None and identity != expected_source_id:
        raise ProbeError(
            f"source identity mismatch: got {identity}, want {expected_source_id}"
        )
    failures = 0
    for case in load_cases(cases_path):
        outcome = run_case(source_root, case)
        errors = compare(outcome, case["expected"][profile])
        failures += bool(errors)
        print(
            json.dumps(
                {
                    "id": case["id"],
                    "profile": profile,
                    "passed": not errors,
                    "source_id": identity,
                    "outcome": {
                        **{field: outcome[field] for field in OUTCOME_FIELDS},
                        "content_chars": len(outcome["content"]),
                    },
                    "errors": errors,
                },
                ensure_ascii=False,
            )
        )
    return 1 if failures else 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=pathlib.Path, default=DEFAULT_CASES)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("validate")
    run = subparsers.add_parser("run")
    run.add_argument("source_root", type=pathlib.Path)
    run.add_argument("--profile", choices=sorted(PROFILES), required=True)
    run.add_argument("--expected-source-id")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.command == "validate":
            return command_validate(args.cases)
        return command_run(
            args.source_root, args.profile, args.cases, args.expected_source_id
        )
    except (OSError, ProbeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
