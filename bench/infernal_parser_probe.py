#!/usr/bin/env python3
"""Run synthetic DS4 parser regressions against a vLLM source tree.

The probe imports only vLLM's parser state machine and DeepSeek V4 config. It
stubs heavyweight serving modules, so it needs neither GPUs nor a vLLM Python
environment. This is intentionally a source-tree gate: point it at the exact
composed source used to build an image.
"""

from __future__ import annotations

import argparse
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
                if not isinstance(outcome.get("tool_calls"), int):
                    raise ProbeError(
                        f"{path}:{line_number}: expected.{profile}.tool_calls must be an integer"
                    )
                if not isinstance(outcome.get("dsml_content"), bool):
                    raise ProbeError(
                        f"{path}:{line_number}: expected.{profile}.dsml_content must be boolean"
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

    parser_engine = types.ModuleType("vllm.parser.engine.parser_engine")
    parser_engine.ParserEngine = object  # type: ignore[attr-defined]
    sys.modules[parser_engine.__name__] = parser_engine

    tool_utils = types.ModuleType("vllm.tool_parsers.utils")
    tool_utils.find_tool_properties = lambda *_args: {}  # type: ignore[attr-defined]
    sys.modules[tool_utils.__name__] = tool_utils

    deepseek = importlib.import_module("vllm.parser.deepseek_v4")
    streaming = importlib.import_module(
        "vllm.parser.engine.streaming_parser_engine"
    )
    return deepseek, streaming


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


def run_case(source_root: pathlib.Path, case: dict[str, Any]) -> dict[str, Any]:
    deepseek, streaming = load_parser(source_root)
    engine = streaming.StreamingParserEngine(
        deepseek.deepseek_v4_config(thinking=False), tokenizer=None
    )
    if hasattr(engine, "allowed_tool_names"):
        engine.allowed_tool_names = frozenset(case.get("allowed_tool_names", [])) or None
    if hasattr(engine, "suppress_tool_calls"):
        engine.suppress_tool_calls = bool(case.get("suppress_tool_calls", False))

    events = []
    for chunk in case["chunks"]:
        events.extend(engine.feed(chunk, ()))
    events.extend(engine.finish())

    content = "".join(
        event.value for event in events if event.type.name == "TEXT_CHUNK"
    )
    return {
        "tool_calls": sum(
            event.type.name == "TOOL_CALL_START" for event in events
        ),
        "dsml_content": "｜DSML｜" in content,
        "content": content,
    }


def compare(outcome: dict[str, Any], expected: dict[str, Any]) -> list[str]:
    errors = []
    for key in ("tool_calls", "dsml_content"):
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
                        "tool_calls": outcome["tool_calls"],
                        "dsml_content": outcome["dsml_content"],
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
