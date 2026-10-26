#!/usr/bin/env python3
"""Tests for the Claude Code agentic coding-task benchmark.

The reference check proves every hidden suite fails on the shipped repository
and passes with the committed solution. The run path is exercised with fake
``claude`` executables, so no model or network is needed.
"""

import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import textwrap
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import claude_code_tasks as cct  # noqa: E402

TOKEN = "cc-test-secret-7f3a"
TOKEN_ENV = cct.TOKEN_ENV
RESULT = {
    "type": "result",
    "subtype": "success",
    "is_error": False,
    "duration_ms": 1234,
    "num_turns": 7,
    "result": "MODEL TEXT THAT MUST NOT BE STORED",
    "usage": {
        "input_tokens": 1000,
        "output_tokens": 200,
        "cache_read_input_tokens": 300,
        "cache_creation_input_tokens": 50,
    },
}

FAKE_CLAUDE = """\
#!{python}
import json, os, pathlib, shutil, subprocess, sys, time

if sys.argv[1:] == ["--version"]:
    print("2.1.302 (Claude Code)")
    sys.exit(0)

env = os.environ
problems = []
expect = {{
    "ANTHROPIC_BASE_URL": {base_url!r},
    "ANTHROPIC_AUTH_TOKEN": {token!r},
    "ANTHROPIC_SMALL_FAST_MODEL": {model!r},
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": {model!r},
    "DISABLE_TELEMETRY": "1",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "DISABLE_AUTOUPDATER": "1",
}}
for key, value in expect.items():
    if env.get(key) != value:
        problems.append(key)
for key in ("ANTHROPIC_API_KEY", "CLAUDECODE"):
    if key in env:
        problems.append("leaked " + key)
if env.get("HOME") == {real_home!r} or not os.path.isfile(os.path.join(env["HOME"], ".claude.json")):
    problems.append("HOME")
if {token!r} in " ".join(sys.argv):
    problems.append("token in argv")
for flag in ("-p", "--dangerously-skip-permissions", "--output-format", "--max-turns"):
    if flag not in sys.argv:
        problems.append(flag)
if not os.path.isfile("TASK.md") or not os.path.isdir(".git"):
    problems.append("workspace")
if problems:
    print(json.dumps(problems), file=sys.stderr)
    sys.exit(3)

behaviour = {behaviour!r}
if behaviour == "solve":
    shutil.copytree({solution!r}, ".", dirs_exist_ok=True)
elif behaviour == "break":
    pathlib.Path("lrucache/cache.py").write_text("raise ImportError('broken')\\n")
elif behaviour == "hang":
    subprocess.Popen(["sleep", "60"])
    time.sleep(60)
elif behaviour == "garbage":
    print("not json")
    sys.exit(1)
print({result!r})
"""


class FakeClaude:
    def __init__(self, directory, behaviour, base_url, model, task="lru_cache"):
        self.path = pathlib.Path(directory) / f"claude-{behaviour}"
        self.path.write_text(
            FAKE_CLAUDE.format(
                python=sys.executable,
                base_url=base_url,
                token=TOKEN,
                model=model,
                real_home=os.path.expanduser("~"),
                behaviour=behaviour,
                solution=str(cct.TASKS_DIR / task / "solution"),
                result=json.dumps(RESULT),
            )
        )
        self.path.chmod(0o755)


class ReferenceValidationTest(unittest.TestCase):
    def test_manifest_matches_task_directories(self):
        self.assertEqual(sorted(cct.load_manifest()), cct.task_dirs())
        self.assertEqual(len(cct.task_dirs()), 10)

    def test_every_task_fails_shipped_and_passes_with_solution(self):
        records, problems = cct.validate_tasks()
        self.assertEqual(problems, [])
        for record in records:
            with self.subTest(task=record["task"]):
                self.assertEqual(record["errors"], [])
                self.assertGreater(record["original_hidden_failed"], 0)
                self.assertGreater(record["solution_hidden_run"], 0)
                self.assertEqual(record["solution_hidden_failed"], 0)
                self.assertTrue(record["visible_passes_on_solution"])


class ParsingTest(unittest.TestCase):
    def test_single_object(self):
        self.assertEqual(cct.parse_claude_result(json.dumps(RESULT))["num_turns"], 7)

    def test_array_and_trailing_line(self):
        stream = [{"type": "system"}, {"type": "assistant"}, RESULT]
        self.assertEqual(cct.parse_claude_result(json.dumps(stream))["num_turns"], 7)
        mixed = "warning: something\n" + json.dumps(RESULT) + "\n"
        self.assertEqual(cct.parse_claude_result(mixed)["num_turns"], 7)

    def test_garbage(self):
        self.assertIsNone(cct.parse_claude_result("oops"))
        self.assertIsNone(cct.parse_claude_result(json.dumps({"type": "assistant"})))

    def test_fields_drop_text_and_bound_subtype(self):
        fields = cct.result_fields(dict(RESULT, subtype="Free text! not allowed"))
        self.assertNotIn("result", fields)
        self.assertIsNone(fields["result_subtype"])
        self.assertEqual(fields["usage"]["cache_read_input_tokens"], 300)
        self.assertTrue(cct.result_fields(None)["is_error"])

    def test_failure_class_precedence(self):
        self.assertIsNone(cct.failure_class(True, True, True, False))
        self.assertEqual(cct.failure_class(False, True, True, True), "timeout")
        self.assertEqual(cct.failure_class(False, False, True, True), "agent_error")
        self.assertEqual(cct.failure_class(False, False, False, False), "no_changes")
        self.assertEqual(cct.failure_class(False, False, False, True), "tests_failed")


class RunPathTest(unittest.TestCase):
    base_url = "http://127.0.0.1:9/fake"
    model = "fake-model"

    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="cc-test-")
        self.root = pathlib.Path(self.scratch.name)

    def tearDown(self):
        self.scratch.cleanup()

    def run_cli(self, behaviour, *extra, tasks="lru_cache"):
        fake = FakeClaude(self.root, behaviour, self.base_url, self.model)
        output = self.root / f"{behaviour}.jsonl"
        argv = [
            "run",
            "--base-url", self.base_url,
            "--model", self.model,
            "--label", f"test-{behaviour}",
            "--output", str(output),
            "--tasks", tasks,
            "--claude-bin", str(fake.path),
            "--artifacts-dir", str(self.root / "artifacts"),
            *extra,
        ]
        leaked = {TOKEN_ENV: TOKEN, "ANTHROPIC_API_KEY": "must-not-leak", "CLAUDECODE": "1"}
        stdout = io.StringIO()
        with mock.patch.dict(os.environ, leaked), contextlib.redirect_stdout(stdout), \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(cct.main(argv), 0)
        text = output.read_text()
        self.assertNotIn(TOKEN, text)
        self.assertNotIn(TOKEN, stdout.getvalue())
        self.assertNotIn("MODEL TEXT", text)
        lines = [json.loads(line) for line in text.splitlines()]
        self.assertEqual(lines[-1]["type"], "summary")
        return [line for line in lines if line["type"] == "attempt"], lines[-1]

    def test_solved_attempt_passes(self):
        attempts, summary = self.run_cli("solve")
        (attempt,) = attempts
        self.assertTrue(attempt["passed"], attempt)
        self.assertIsNone(attempt["failure_class"])
        self.assertEqual(attempt["num_turns"], 7)
        self.assertEqual(attempt["duration_ms"], 1234)
        self.assertEqual(attempt["usage"]["input_tokens"], 1000)
        self.assertEqual(attempt["hidden_tests_failed"], 0)
        self.assertGreater(attempt["hidden_tests_run"], 0)
        self.assertEqual(attempt["changed_files"], 1)
        self.assertEqual(attempt["claude_version"], "2.1.302")
        self.assertEqual(summary["pass_rate"], 1.0)
        self.assertEqual(summary["total_tokens"], 1550)
        diff = (self.root / "artifacts" / "test-solve" / "lru_cache-r0.diff").read_text()
        self.assertIn("move_to_end", diff)

    def test_untouched_workspace_is_no_changes(self):
        (attempt,), summary = self.run_cli("noop")
        self.assertFalse(attempt["passed"])
        self.assertEqual(attempt["failure_class"], "no_changes")
        self.assertEqual(summary["failure_classes"], {"no_changes": 1})

    def test_broken_change_is_tests_failed(self):
        (attempt,), _ = self.run_cli("break")
        self.assertFalse(attempt["passed"])
        self.assertEqual(attempt["failure_class"], "tests_failed")
        self.assertGreater(attempt["hidden_tests_failed"], 0)

    def test_garbage_output_is_agent_error(self):
        (attempt,), _ = self.run_cli("garbage")
        self.assertEqual(attempt["failure_class"], "agent_error")
        self.assertIsNone(attempt["num_turns"])
        self.assertEqual(attempt["exit_code"], 1)

    def test_timeout_kills_process_group(self):
        started = time.monotonic()
        (attempt,), _ = self.run_cli("hang", "--timeout-seconds", "1")
        self.assertLess(time.monotonic() - started, 20)
        self.assertTrue(attempt["timed_out"])
        self.assertEqual(attempt["failure_class"], "timeout")

    def test_concurrent_repetitions(self):
        attempts, summary = self.run_cli(
            "solve", "--repetitions", "2", "--concurrency", "3", tasks="lru_cache,interval_merge"
        )
        self.assertEqual(len(attempts), 4)
        self.assertEqual(summary["attempts"], 4)
        by_task = {(a["task"], a["repetition"]): a["passed"] for a in attempts}
        self.assertTrue(by_task[("lru_cache", 0)] and by_task[("lru_cache", 1)])
        # The fake only knows the LRU solution.
        self.assertFalse(by_task[("interval_merge", 0)])

    def test_requires_token_env_and_fresh_output(self):
        output = self.root / "out.jsonl"
        argv = ["run", "--base-url", self.base_url, "--model", self.model, "--label", "x", "--output", str(output)]
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop(TOKEN_ENV, None)
            with self.assertRaises(SystemExit):
                cct.main(argv)
        self.assertFalse(output.exists())
        output.write_text("previous run\n")
        with mock.patch.dict(os.environ, {TOKEN_ENV: TOKEN}):
            with self.assertRaises(SystemExit):
                cct.main(argv)
        self.assertEqual(output.read_text(), "previous run\n")


if __name__ == "__main__":
    unittest.main()
