#!/usr/bin/env python3
"""Agentic coding benchmark: the real Claude Code CLI driving a served model.

Each task under ``coding_tasks/<id>/`` ships a small Python repository with a
TASK.md written like an issue (``repo/``), hidden grader tests the agent never
sees (``hidden/test_hidden.py``) and a reference solution (``solution/``).

``validate`` is the GPU-free self-check: for every task the hidden tests must
fail on the shipped repository and pass once the reference solution is applied.

``run`` copies each task into a fresh git workspace, runs ``claude -p`` in
headless mode against an Anthropic-compatible endpoint, then copies the hidden
tests in and grades the result. The bearer comes only from the CC_TASKS_TOKEN
environment variable. The results file holds one JSON line per attempt with
pass/fail, hidden-test counts, timings, turn and token counts, and a closed
failure class; model output and transcripts are never written to it. The
workspace diff can optionally be kept under ``--artifacts-dir`` for review.

``passed`` is decided by the hidden tests alone; an attempt that hit
``--max-turns`` but left a correct workspace still passes, with ``is_error``
and ``result_subtype`` recording how claude ended. ``failure_class`` is set
only for failed attempts: timeout, agent_error, no_changes, tests_failed.

claude runs with a clean environment and a throwaway HOME, so no user
settings, CLAUDE.md, memory or login leak in. ANTHROPIC_AUTH_TOKEN is sent as
a Bearer header; ANTHROPIC_API_KEY is never set. Every model alias (haiku,
sonnet, opus, sub-agents) is pointed at --model. Claude Code requests
max_tokens=32000 by default; use --max-output-tokens if the endpoint rejects it.

    CC_TASKS_TOKEN=... python3 bench/claude_code_tasks.py run \\
        --base-url http://127.0.0.1:8006 --model MODEL --label rNN \\
        --output /tmp/rNN-coding.jsonl --artifacts-dir /tmp/rNN-coding
"""

import argparse
import collections
import concurrent.futures
import datetime
import json
import os
import pathlib
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time


TASKS_DIR = pathlib.Path(__file__).with_name("coding_tasks")
MANIFEST = TASKS_DIR / "tasks.json"
PROMPT = (
    "Complete the task described in TASK.md in this repository. "
    "Run the tests with `python3 -m unittest` to check your work."
)
TOKEN_ENV = "CC_TASKS_TOKEN"
FAILURE_CLASSES = ("timeout", "agent_error", "no_changes", "tests_failed")
HIDDEN_MODULE = "test_hidden"
GRADE_TIMEOUT_SECONDS = 300
KILL_GRACE_SECONDS = 5
ARTIFACT_TAIL_BYTES = 64 * 1024
# The agent inherits only these from the caller. Everything else -- notably
# ANTHROPIC_API_KEY and the CLAUDECODE markers of an enclosing session -- is
# dropped so the run is reproducible and cannot pick up the operator's login.
PASSTHROUGH_ENV = ("PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ", "USER", "LOGNAME", "SHELL")
GIT_IDENTITY = {
    "GIT_AUTHOR_NAME": "coding-task",
    "GIT_AUTHOR_EMAIL": "coding-task@localhost",
    "GIT_COMMITTER_NAME": "coding-task",
    "GIT_COMMITTER_EMAIL": "coding-task@localhost",
}
WORKSPACE_GITIGNORE = "__pycache__/\n*.pyc\n"
LABEL_PATTERN = re.compile(r"[A-Za-z0-9._-]{1,80}")
SUBTYPE_PATTERN = re.compile(r"[a-z_]{1,40}")
USAGE_KEYS = (
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
)
MODEL_ENV_KEYS = (
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
)

# Runs inside the graded workspace with `python3 -c`, so the workspace is on
# sys.path. Counts go to a file outside the workspace instead of stdout, which
# the code under test can write to freely.
GRADER = r"""
import json, sys, unittest
summary = {"run": 0, "failed": 1}
try:
    suite = unittest.defaultTestLoader.loadTestsFromName(sys.argv[2])
    result = unittest.TextTestRunner(stream=sys.stderr, verbosity=2).run(suite)
    # A failing subTest is reported per subtest; count each test method once.
    ids = lambda tests: {getattr(test, "test_case", test).id() for test in tests}
    failed = ids(t for t, _ in result.failures + result.errors) | ids(result.unexpectedSuccesses)
    summary = {
        "run": result.testsRun,
        "failed": len(failed),
        "failed_tests": sorted(name.rsplit(".", 1)[-1] for name in failed),
    }
finally:
    with open(sys.argv[1], "w") as handle:
        json.dump(summary, handle)
"""


class TaskError(ValueError):
    """The committed task corpus is malformed."""


def load_manifest():
    entries = json.loads(MANIFEST.read_text())
    ids = [entry["id"] for entry in entries]
    if len(set(ids)) != len(ids):
        raise TaskError("duplicate task id in manifest")
    return {entry["id"]: entry for entry in entries}


def task_dirs():
    return sorted(path.name for path in TASKS_DIR.iterdir() if path.is_dir() and path.name != "__pycache__")


def check_layout(task_id):
    root = TASKS_DIR / task_id
    errors = []
    for relative in ("repo/TASK.md", f"hidden/{HIDDEN_MODULE}.py"):
        if not (root / relative).is_file():
            errors.append(f"missing {relative}")
    solution = root / "solution"
    if not solution.is_dir() or not any(path.is_file() for path in solution.rglob("*")):
        errors.append("missing solution files")
    return errors


def copy_tree(source, destination):
    shutil.copytree(
        source,
        destination,
        dirs_exist_ok=True,
        ignore=shutil.ignore_patterns("__pycache__", "*.pyc"),
    )


def clean_env(home, tmpdir):
    env = {key: os.environ[key] for key in PASSTHROUGH_ENV if key in os.environ}
    env.update(
        HOME=str(home),
        TMPDIR=str(tmpdir),
        XDG_CONFIG_HOME=str(home / ".config"),
        XDG_CACHE_HOME=str(home / ".cache"),
        XDG_DATA_HOME=str(home / ".local" / "share"),
        XDG_STATE_HOME=str(home / ".local" / "state"),
        PYTHONDONTWRITEBYTECODE="1",
        GIT_CONFIG_NOSYSTEM="1",
    )
    env.update(GIT_IDENTITY)
    return env


def git(workspace, *args, env):
    return subprocess.run(
        ["git", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
        cwd=workspace,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    ).stdout


class ProcessGroups:
    """Live process groups, so an interrupted run leaves nothing behind."""

    def __init__(self):
        self.lock = threading.Lock()
        self.groups = set()

    def add(self, pgid):
        with self.lock:
            self.groups.add(pgid)

    def discard(self, pgid):
        with self.lock:
            self.groups.discard(pgid)

    def kill_all(self):
        with self.lock:
            groups = list(self.groups)
        for pgid in groups:
            signal_group(pgid, signal.SIGKILL)


PROCESS_GROUPS = ProcessGroups()


def signal_group(pgid, signum):
    try:
        os.killpg(pgid, signum)
    except (ProcessLookupError, PermissionError):
        pass


def run_bounded(cmd, *, cwd, env, timeout):
    """Run ``cmd`` in its own session; kill the whole group on timeout.

    Returns (exit_code, stdout, stderr, timed_out). Any background process the
    command leaves behind is killed as well.
    """
    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    PROCESS_GROUPS.add(proc.pid)
    timed_out = False
    try:
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            # A finished command whose orphan still holds the pipe is not a timeout.
            timed_out = proc.poll() is None
            signal_group(proc.pid, signal.SIGTERM)
            try:
                proc.wait(KILL_GRACE_SECONDS)
            except subprocess.TimeoutExpired:
                pass
            signal_group(proc.pid, signal.SIGKILL)
            try:
                stdout, stderr = proc.communicate(timeout=KILL_GRACE_SECONDS)
            except subprocess.TimeoutExpired:
                stdout, stderr = b"", b""
    finally:
        signal_group(proc.pid, signal.SIGKILL)
        PROCESS_GROUPS.discard(proc.pid)
    return proc.returncode, stdout.decode(errors="replace"), stderr.decode(errors="replace"), timed_out


def grade(workspace, hidden_dir, env, timeout=GRADE_TIMEOUT_SECONDS):
    """Copy the hidden tests into ``workspace`` and run them."""
    shutil.copy2(hidden_dir / f"{HIDDEN_MODULE}.py", workspace / f"{HIDDEN_MODULE}.py")
    with tempfile.TemporaryDirectory(prefix="cc-grade-") as scratch:
        summary_path = pathlib.Path(scratch) / "summary.json"
        started = time.monotonic()
        code, _, stderr, timed_out = run_bounded(
            [sys.executable, "-c", GRADER, str(summary_path), HIDDEN_MODULE],
            cwd=workspace,
            env=env,
            timeout=timeout,
        )
        try:
            summary = json.loads(summary_path.read_text())
        except (OSError, ValueError):
            summary = {"run": 0, "failed": 1}
    run, failed = int(summary.get("run", 0)), int(summary.get("failed", 1))
    return {
        "run": run,
        "failed": failed,
        "failed_tests": [str(name)[:80] for name in summary.get("failed_tests", [])][:50],
        "passed": run > 0 and failed == 0 and not timed_out and code == 0,
        "timed_out": timed_out,
        "seconds": round(time.monotonic() - started, 3),
        "log": stderr,
    }


def run_visible(workspace, env, timeout=120):
    code, _, _, timed_out = run_bounded(
        [sys.executable, "-m", "unittest"], cwd=workspace, env=env, timeout=timeout
    )
    return code == 0 and not timed_out


def validate_task(task_id):
    """Hidden tests must fail on the shipped repo and pass with the solution."""
    root = TASKS_DIR / task_id
    record = {"type": "validate", "task": task_id, "errors": check_layout(task_id)}
    if record["errors"]:
        record["ok"] = False
        return record
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix=f"cc-validate-{task_id}-") as scratch:
        scratch = pathlib.Path(scratch)
        home = scratch / "home"
        home.mkdir()
        env = clean_env(home, scratch)

        original = scratch / "original"
        copy_tree(root / "repo", original)
        record["visible_fails_on_original"] = not run_visible(original, env)
        shipped = grade(original, root / "hidden", env)

        solved = scratch / "solved"
        copy_tree(root / "repo", solved)
        copy_tree(root / "solution", solved)
        record["visible_passes_on_solution"] = run_visible(solved, env)
        reference = grade(solved, root / "hidden", env)

    record.update(
        original_hidden_run=shipped["run"],
        original_hidden_failed=shipped["failed"],
        solution_hidden_run=reference["run"],
        solution_hidden_failed=reference["failed"],
        seconds=round(time.monotonic() - started, 2),
    )
    if shipped["passed"] or shipped["failed"] == 0:
        record["errors"].append("hidden tests pass on the shipped repository")
    if not reference["passed"]:
        record["errors"].append("hidden tests fail with the reference solution")
    if not record["visible_passes_on_solution"]:
        record["errors"].append("visible tests fail with the reference solution")
    record["ok"] = not record["errors"]
    return record


def validate_tasks(task_ids=None, workers=8):
    manifest = load_manifest()
    problems = []
    if set(manifest) != set(task_dirs()):
        problems.append(f"manifest {sorted(manifest)} does not match directories {task_dirs()}")
    task_ids = list(task_ids or manifest)
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, min(workers, len(task_ids)))) as pool:
        records = list(pool.map(validate_task, task_ids))
    return records, problems


def command_validate(args):
    task_ids = args.tasks.split(",") if args.tasks else None
    records, problems = validate_tasks(task_ids)
    for record in records:
        print(json.dumps(record, sort_keys=True))
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    ok = sum(record["ok"] for record in records)
    print(f"validated {ok}/{len(records)} tasks", file=sys.stderr)
    return 0 if ok == len(records) and not problems else 1


def parse_claude_result(stdout):
    """Find the final ``{"type": "result"}`` object in claude's JSON output."""
    text = stdout.strip()
    candidates = []
    try:
        candidates.append(json.loads(text))
    except ValueError:
        for line in reversed(text.splitlines()):
            try:
                candidates.append(json.loads(line))
            except ValueError:
                continue
            break
    for candidate in candidates:
        if isinstance(candidate, list):
            candidate = next(
                (item for item in reversed(candidate) if isinstance(item, dict) and item.get("type") == "result"),
                None,
            )
        if isinstance(candidate, dict) and candidate.get("type") == "result":
            return candidate
    return None


def as_int(value):
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def result_fields(result):
    """Reduce claude's result object to bounded numeric/enumerated fields."""
    if result is None:
        return {"num_turns": None, "duration_ms": None, "usage": None, "is_error": True, "result_subtype": None}
    usage = result.get("usage") if isinstance(result.get("usage"), dict) else None
    subtype = result.get("subtype")
    return {
        "num_turns": as_int(result.get("num_turns")),
        "duration_ms": as_int(result.get("duration_ms")),
        "usage": None if usage is None else {key: as_int(usage.get(key)) or 0 for key in USAGE_KEYS},
        "is_error": bool(result.get("is_error")),
        "result_subtype": subtype if isinstance(subtype, str) and SUBTYPE_PATTERN.fullmatch(subtype) else None,
    }


def failure_class(passed, timed_out, agent_error, changed):
    if passed:
        return None
    if timed_out:
        return "timeout"
    if agent_error:
        return "agent_error"
    if not changed:
        return "no_changes"
    return "tests_failed"


def claude_command(args):
    return [
        args.claude_bin,
        "-p",
        PROMPT,
        "--model",
        args.model,
        "--output-format",
        "json",
        "--dangerously-skip-permissions",
        "--max-turns",
        str(args.max_turns),
    ]


def claude_env(args, home, tmpdir, token):
    env = clean_env(home, tmpdir)
    env.update(
        ANTHROPIC_BASE_URL=args.base_url,
        ANTHROPIC_AUTH_TOKEN=token,
        DISABLE_TELEMETRY="1",
        DISABLE_ERROR_REPORTING="1",
        DISABLE_AUTOUPDATER="1",
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
    )
    # Background and sub-agent calls ask for haiku/sonnet aliases by default;
    # the served endpoint only knows the one model under test.
    env.update({key: args.model for key in MODEL_ENV_KEYS})
    if args.max_output_tokens:
        env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"] = str(args.max_output_tokens)
    return env


def prepare_home(home):
    """A throwaway HOME: no user settings, CLAUDE.md, memory, or login."""
    home.mkdir(mode=0o700)
    (home / ".claude").mkdir(mode=0o700)
    (home / ".claude.json").write_text(
        json.dumps({"hasCompletedOnboarding": True, "bypassPermissionsModeAccepted": True})
    )


def prepare_workspace(task_id, workspace, env):
    copy_tree(TASKS_DIR / task_id / "repo", workspace)
    (workspace / ".gitignore").write_text(WORKSPACE_GITIGNORE)
    git(workspace, "init", "-q", "-b", "main", env=env)
    git(workspace, "add", "-A", env=env)
    git(workspace, "commit", "-q", "-m", "Initial import", env=env)
    return git(workspace, "rev-parse", "HEAD", env=env).strip()


def workspace_changes(workspace, base, env):
    """Files changed since the initial commit (committed or not) and the diff."""
    git(workspace, "add", "-A", env=env)
    names = git(workspace, "diff", "--cached", "--name-only", base, env=env).split()
    diff = git(workspace, "diff", "--cached", base, env=env) if names else ""
    return names, diff


def write_artifacts(args, task_id, repetition, diff, grade_log, claude_stderr):
    if not args.artifacts_dir:
        return
    directory = pathlib.Path(args.artifacts_dir) / args.label
    directory.mkdir(parents=True, exist_ok=True)
    stem = f"{task_id}-r{repetition}"
    (directory / f"{stem}.diff").write_text(diff)
    (directory / f"{stem}.grade.log").write_text(grade_log[-ARTIFACT_TAIL_BYTES:])
    (directory / f"{stem}.claude-stderr.log").write_text(claude_stderr[-ARTIFACT_TAIL_BYTES:])


def run_attempt(args, task, repetition, token, claude_version):
    task_id = task["id"]
    scratch = pathlib.Path(tempfile.mkdtemp(prefix=f"cc-task-{task_id}-"))
    try:
        home = scratch / "home"
        prepare_home(home)
        workspace = scratch / "work"
        env = claude_env(args, home, scratch, token)
        base = prepare_workspace(task_id, workspace, clean_env(home, scratch))

        started_at = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        started = time.monotonic()
        exit_code, stdout, stderr, timed_out = run_bounded(
            claude_command(args), cwd=workspace, env=env, timeout=args.timeout_seconds
        )
        wall = time.monotonic() - started
        fields = result_fields(parse_claude_result(stdout))
        del stdout  # final assistant text; never persisted

        grader_env = clean_env(home, scratch)
        changed, diff = workspace_changes(workspace, base, grader_env)
        graded = grade(workspace, TASKS_DIR / task_id / "hidden", grader_env)
        write_artifacts(args, task_id, repetition, diff, graded["log"], stderr)

        agent_error = exit_code != 0 or fields["is_error"]
        passed = graded["passed"]
        return {
            "type": "attempt",
            "task": task_id,
            "category": task.get("category"),
            "difficulty": task.get("difficulty"),
            "repetition": repetition,
            "label": args.label,
            "model": args.model,
            "claude_version": claude_version,
            "started_at": started_at,
            "passed": passed,
            "failure_class": failure_class(passed, timed_out, agent_error, bool(changed)),
            "hidden_tests_run": graded["run"],
            "hidden_tests_failed": graded["failed"],
            "hidden_failed_tests": graded["failed_tests"],
            "grade_seconds": graded["seconds"],
            "wall_seconds": round(wall, 3),
            "timed_out": timed_out,
            "exit_code": exit_code,
            "changed_files": len(changed),
            **fields,
        }
    finally:
        if args.keep_workspaces:
            print(f"kept workspace {scratch}", file=sys.stderr)
        else:
            shutil.rmtree(scratch, ignore_errors=True)


def median(values):
    values = [value for value in values if value is not None]
    return statistics.median(values) if values else None


def summarize(records, args, claude_version):
    per_task = collections.OrderedDict()
    for record in records:
        entry = per_task.setdefault(record["task"], {"attempts": 0, "passed": 0})
        entry["attempts"] += 1
        entry["passed"] += bool(record["passed"])
    tokens = {key: 0 for key in USAGE_KEYS}
    for record in records:
        for key in USAGE_KEYS:
            tokens[key] += (record.get("usage") or {}).get(key) or 0
    passed = sum(bool(record["passed"]) for record in records)
    return {
        "type": "summary",
        "label": args.label,
        "model": args.model,
        "base_url": args.base_url,
        "claude_version": claude_version,
        "attempts": len(records),
        "passed": passed,
        "pass_rate": round(passed / len(records), 4) if records else None,
        "median_wall_seconds": median(record["wall_seconds"] for record in records),
        "median_turns": median(record.get("num_turns") for record in records),
        "tokens": tokens,
        "total_tokens": sum(tokens.values()),
        "failure_classes": dict(collections.Counter(r["failure_class"] for r in records if r["failure_class"])),
        "per_task": per_task,
        "max_turns": args.max_turns,
        "timeout_seconds": args.timeout_seconds,
    }


def print_summary(summary, stream=None):
    stream = stream or sys.stdout
    rate = "n/a" if summary["pass_rate"] is None else f"{100 * summary['pass_rate']:.1f}%"
    tokens = summary["tokens"]
    print(
        f"{summary['label']} model={summary['model']} passed {summary['passed']}/{summary['attempts']} ({rate})"
        f"  median wall {summary['median_wall_seconds']}s  median turns {summary['median_turns']}"
        f"  tokens total {summary['total_tokens']} (in {tokens['input_tokens']}, out {tokens['output_tokens']},"
        f" cache read {tokens['cache_read_input_tokens']})",
        file=stream,
    )
    for task_id, entry in summary["per_task"].items():
        print(f"  {task_id:<18} {entry['passed']}/{entry['attempts']}", file=stream)
    if summary["failure_classes"]:
        failures = ", ".join(f"{key}={value}" for key, value in sorted(summary["failure_classes"].items()))
        print(f"  failures: {failures}", file=stream)


def claude_version(args):
    with tempfile.TemporaryDirectory(prefix="cc-version-") as scratch:
        home = pathlib.Path(scratch) / "home"
        prepare_home(home)
        try:
            code, stdout, _, _ = run_bounded(
                [args.claude_bin, "--version"], cwd=scratch, env=clean_env(home, scratch), timeout=60
            )
        except OSError as error:
            raise SystemExit(f"cannot run {args.claude_bin}: {error.strerror}")
    match = re.search(r"\d+\.\d+\.\d+", stdout)
    if code != 0 or not match:
        raise SystemExit(f"{args.claude_bin} --version failed")
    return match.group(0)


def command_run(args):
    if not LABEL_PATTERN.fullmatch(args.label):
        raise SystemExit("label must match [A-Za-z0-9._-]{1,80}")
    token = os.environ.get(TOKEN_ENV)
    if not token:
        raise SystemExit(f"set {TOKEN_ENV} to the endpoint bearer (any non-empty value for an open endpoint)")
    if os.path.exists(args.output):
        raise SystemExit(f"refusing to overwrite {args.output}")
    manifest = load_manifest()
    task_ids = args.tasks.split(",") if args.tasks else list(manifest)
    unknown = [task_id for task_id in task_ids if task_id not in manifest]
    if unknown:
        raise SystemExit(f"unknown task(s): {', '.join(unknown)}")
    resolved = shutil.which(args.claude_bin, path=os.environ.get("PATH"))
    if resolved is None:
        raise SystemExit(f"claude executable not found: {args.claude_bin}")
    args.claude_bin = resolved
    version = claude_version(args)

    plan = [(manifest[task_id], repetition) for repetition in range(args.repetitions) for task_id in task_ids]
    records = []
    lock = threading.Lock()
    with open(args.output, "x", encoding="utf-8") as output:

        def attempt(item):
            task, repetition = item
            record = run_attempt(args, task, repetition, token, version)
            with lock:
                records.append(record)
                output.write(json.dumps(record, sort_keys=True) + "\n")
                output.flush()
                status = "PASS" if record["passed"] else f"FAIL ({record['failure_class']})"
                print(
                    f"[{len(records)}/{len(plan)}] {task['id']} r{repetition}: {status}"
                    f" {record['wall_seconds']:.1f}s turns={record['num_turns']}",
                    file=sys.stderr,
                    flush=True,
                )
            return record

        pool = concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency)
        try:
            for future in [pool.submit(attempt, item) for item in plan]:
                future.result()
        except BaseException:
            pool.shutdown(wait=False, cancel_futures=True)
            PROCESS_GROUPS.kill_all()
            raise
        pool.shutdown()

        summary = summarize(records, args, version)
        output.write(json.dumps(summary, sort_keys=True) + "\n")
    print_summary(summary)
    print(json.dumps(summary, sort_keys=True))
    return 0


def positive_int(text):
    value = int(text)
    if value < 1:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return value


def parser():
    root = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = root.add_subparsers(dest="command", required=True)

    validate = commands.add_parser("validate", help="self-check every task against its reference solution")
    validate.add_argument("--tasks", help="comma-separated task ids (default: all)")
    validate.set_defaults(handler=command_validate)

    run = commands.add_parser("run", help="run claude headless against an Anthropic-compatible endpoint")
    run.add_argument("--base-url", required=True, help="ANTHROPIC_BASE_URL for claude")
    run.add_argument("--model", required=True)
    run.add_argument("--label", required=True)
    run.add_argument("--output", required=True, help="results JSONL; must not exist")
    run.add_argument("--tasks", help="comma-separated task ids (default: all)")
    run.add_argument("--repetitions", type=positive_int, default=1)
    run.add_argument("--timeout-seconds", type=positive_int, default=900)
    run.add_argument("--max-turns", type=positive_int, default=60)
    run.add_argument("--concurrency", type=positive_int, default=1)
    run.add_argument("--max-output-tokens", type=positive_int, help="sets CLAUDE_CODE_MAX_OUTPUT_TOKENS")
    run.add_argument("--artifacts-dir", help="keep per-attempt diff and grader log here")
    run.add_argument("--keep-workspaces", action="store_true", help="do not delete attempt workspaces")
    run.add_argument("--claude-bin", default="claude", help="claude executable (default: claude on PATH)")
    run.set_defaults(handler=command_run)
    return root


def interrupt(signum, frame):
    raise KeyboardInterrupt


def main(argv=None):
    args = parser().parse_args(argv)
    # Background jobs of a non-interactive shell ignore SIGINT, so a wrapper
    # stops the run with SIGTERM; treat it like Ctrl-C and kill every attempt.
    signal.signal(signal.SIGTERM, interrupt)
    return args.handler(args)


if __name__ == "__main__":
    sys.exit(main())
