#!/usr/bin/env python3
"""Fail-fast, resumable qualification gate for one immutable engine candidate.

The gate deliberately invokes only repository-owned benchmark programs. It
never records commands, environment variables, model output, container logs,
or credentials. Benchmark stdout is already privacy-bounded by the child
runners and is stored as a mode-0600 artifact; the journal stores only hashes,
sizes, timing, bounded failure classes, and immutable candidate identity.
"""

import argparse
import dataclasses
import datetime
import hashlib
import json
import os
import pathlib
import platform
import re
import subprocess
import sys
import time


SCHEMA_VERSION = 1
PLAN_VERSION = 1
SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
THROUGH_ORDER = {"smoke": 1, "scout": 2, "matrix": 3}
REQUIRED_AGENT_METADATA = {
    "engine_image",
    "model_revision",
    "tokenizer_sha256",
    "config_sha256",
    "router_version",
    "gpu_count",
}
CONTROLLED_ENV = {
    "BENCH_PROMPT",
    "BENCH_SPEC_MODE",
    "BENCH_TEMPERATURE",
    "BENCH_WORKLOAD",
    "ENGINE_C1_MAX_TOKENS",
    "ENGINE_CONCURRENCIES",
    "ENGINE_CONCURRENT_MAX_TOKENS",
    "ENGINE_RUNS",
    "ENGINE_WORKLOADS",
    "METRICS_URL",
    "METRICS_URLS",
    "SWEEP_LABEL",
}
RUNTIME_MARKERS = (
    (
        "jit_compilation",
        re.compile(
            r"(?i)(?:(?:triton|cutedsl|torchinductor|torch\.compile).{0,100}"
            r"(?:compil|generat|autotun)|(?:compil|generat).{0,100}"
            r"(?:triton|cutedsl|kernel))"
        ),
    ),
    ("cuda_oom", re.compile(r"(?i)(?:CUDA out of memory|CUDA error: out of memory)")),
    ("cuda_error", re.compile(r"(?i)(?:CUDA error|CUDA failure|device-side assert)")),
    ("nccl_error", re.compile(r"(?i)(?:NCCL[^\n]{0,120}(?:error|abort|failed))")),
    ("xid", re.compile(r"(?i)\bXid(?:\s*[:=]?\s*\d+)?\b")),
    ("traceback", re.compile(r"Traceback \(most recent call last\)")),
    (
        "fatal_runtime",
        re.compile(r"(?i)(?:Fatal Python error|segmentation fault|core dumped)"),
    ),
)


class GateError(RuntimeError):
    """A gate precondition or fail-closed invariant was violated."""


@dataclasses.dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: bytes = b""
    stderr: bytes = b""


@dataclasses.dataclass(frozen=True)
class ContainerIdentity:
    image_id: str
    configured_image: str
    started_at: str
    restart_count: int
    running: bool


@dataclasses.dataclass(frozen=True)
class Stage:
    name: str
    argv: tuple[str, ...]
    env: tuple[tuple[str, str], ...] = ()


class SubprocessRunner:
    def run(self, argv, env=None):
        child_env = dict(os.environ)
        for key in CONTROLLED_ENV:
            child_env.pop(key, None)
        child_env.update(env or {})
        try:
            completed = subprocess.run(
                argv,
                check=False,
                capture_output=True,
                env=child_env,
            )
        except OSError as error:
            return CommandResult(127, b"", type(error).__name__.encode())
        return CommandResult(completed.returncode, completed.stdout, completed.stderr)

    def inspect(self, container):
        template = (
            "{{.Image}}\t{{.Config.Image}}\t{{.State.StartedAt}}\t"
            "{{.RestartCount}}\t{{.State.Running}}"
        )
        result = self.run(("docker", "inspect", "--format", template, container))
        if result.returncode != 0:
            raise GateError("container identity inspection failed")
        fields = result.stdout.decode("utf-8", "strict").strip().split("\t")
        if len(fields) != 5:
            raise GateError("container identity inspection returned an invalid shape")
        try:
            restart_count = int(fields[3])
        except ValueError as error:
            raise GateError("container restart count is invalid") from error
        return ContainerIdentity(
            image_id=fields[0],
            configured_image=fields[1],
            started_at=fields[2],
            restart_count=restart_count,
            running=fields[4].casefold() == "true",
        )

    def logs(self, container, since):
        return self.run(("docker", "logs", "--since", since, container))


def canonical_digest(value):
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def bytes_digest(value):
    return hashlib.sha256(value).hexdigest()


def read_json(path):
    try:
        return json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateError(f"invalid JSON metadata: {path}") from error


def candidate_contract(metadata):
    if metadata.get("schema_version") != 1 or not isinstance(metadata.get("live"), dict):
        raise GateError("engine metadata does not use schema version 1")
    if metadata.get("verified") is False:
        raise GateError("engine metadata failed receipt verification")
    if metadata.get("receipt") is not None and metadata.get("verified") is not True:
        raise GateError("engine receipt is present but not verified")
    live = metadata["live"]
    required = (
        "configured_image",
        "image_id",
        "model_revision",
        "tokenizer_revision",
        "tokenizer_sha256",
        "config_sha256",
        "started_at",
        "restart_count",
        "argv_sha256",
    )
    missing = [key for key in required if live.get(key) in (None, "")]
    if missing:
        raise GateError("engine metadata is missing immutable fields: " + ", ".join(missing))
    return {
        "configured_image": live["configured_image"],
        "image_id": live["image_id"],
        "image_descriptor_digest": live.get("image_descriptor_digest"),
        "image_config_digest": live.get("image_config_digest"),
        "model_revision": live["model_revision"],
        "tokenizer_revision": live["tokenizer_revision"],
        "tokenizer_sha256": live["tokenizer_sha256"],
        "config_sha256": live["config_sha256"],
        "runtime_packages": live.get("runtime_packages") or {},
        "effective_contract": live.get("effective_contract") or {},
        "argv_sha256": live["argv_sha256"],
        "started_at": live["started_at"],
        "restart_count": live["restart_count"],
        "receipt_sha256": (metadata.get("receipt") or {}).get("receipt_sha256"),
        "receipt_verified": metadata.get("verified") is True,
    }


def validate_agent_metadata(metadata, candidate):
    missing = sorted(REQUIRED_AGENT_METADATA - metadata.keys())
    if missing:
        raise GateError("agent metadata is missing: " + ", ".join(missing))
    if type(metadata["gpu_count"]) is not int or metadata["gpu_count"] < 1:
        raise GateError("agent metadata gpu_count must be a positive integer")
    for key in ("model_revision", "tokenizer_sha256", "config_sha256"):
        if metadata[key] != candidate[key]:
            raise GateError(f"agent metadata does not match engine {key}")
    expected_images = {
        candidate["configured_image"],
        candidate["configured_image"] + "@" + candidate["image_id"],
    }
    if metadata["engine_image"] not in expected_images:
        raise GateError("agent metadata does not match engine image identity")


def expected_identity(candidate):
    return ContainerIdentity(
        image_id=candidate["image_id"],
        configured_image=candidate["configured_image"],
        started_at=candidate["started_at"],
        restart_count=int(candidate["restart_count"]),
        running=True,
    )


def assert_identity(actual, expected):
    mismatches = [
        field
        for field in dataclasses.asdict(expected)
        if getattr(actual, field) != getattr(expected, field)
    ]
    if mismatches:
        raise GateError("container identity changed: " + ", ".join(mismatches))


def script_digest(path):
    return bytes_digest(pathlib.Path(path).read_bytes())


def build_stages(args):
    python = sys.executable
    agent = Stage(
        "agent_correctness",
        (
            python,
            str(SCRIPT_DIR / "agentbench.py"),
            "run",
            args.base,
            args.model,
            "--metadata-json",
            str(args.agent_metadata),
            "--profile",
            "deterministic",
            "--label",
            "candidate-gate-agent",
            "--concurrency",
            "1",
            "--repetitions",
            "1",
        ),
    )
    scout = Stage(
        "c8_scout",
        (str(SCRIPT_DIR / "engine_matrix.sh"), args.base, args.model, "candidate-gate-scout"),
        (("ENGINE_CONCURRENCIES", "8"), ("ENGINE_RUNS", "1")),
    )
    matrix = Stage(
        "full_matrix",
        (str(SCRIPT_DIR / "engine_matrix.sh"), args.base, args.model, "candidate-gate-matrix"),
    )
    return (agent, scout, matrix)


def plan_contract(args, agent_metadata):
    return {
        "plan_version": PLAN_VERSION,
        "python": platform.python_version(),
        "base": args.base.rstrip("/"),
        "model": args.model,
        "container": args.container,
        "agent_metadata_sha256": canonical_digest(agent_metadata),
        "inputs": {
            name: script_digest(SCRIPT_DIR / name)
            for name in (
                "candidate_gate.py",
                "agentbench.py",
                "agent_cases/v1.jsonl",
                "engine_matrix.sh",
                "codebench.py",
                "engine_metrics.py",
            )
        },
        "stages": [
            {"name": stage.name, "env": dict(stage.env)} for stage in build_stages(args)
        ],
    }


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")


def runtime_findings(logs):
    body = logs.decode("utf-8", "replace")
    return [name for name, pattern in RUNTIME_MARKERS if pattern.search(body)]


def load_prior(path, candidate_sha256, plan_sha256, resume):
    path = pathlib.Path(path)
    if not path.exists():
        return set()
    if not resume:
        raise GateError(f"output already exists (use --resume): {path}")
    successful = set()
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw.strip():
            continue
        try:
            record = json.loads(raw)
        except json.JSONDecodeError as error:
            raise GateError(f"invalid prior gate journal line {line_number}") from error
        if record.get("candidate_sha256") != candidate_sha256:
            raise GateError("resume candidate identity does not match prior journal")
        if record.get("plan_sha256") != plan_sha256:
            raise GateError("resume plan does not match prior journal")
        if record.get("status") in {"passed", "resumed"}:
            successful.add(record.get("gate"))
    return successful


def append_record(path, record):
    path = pathlib.Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
    path.chmod(0o600)


def store_artifact(directory, stage, output):
    directory = pathlib.Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    directory.chmod(0o700)
    path = directory / f"{stage}.jsonl"
    path.write_bytes(output)
    path.chmod(0o600)
    return path


def record_base(candidate_sha256, plan_sha256, gate, started, ended):
    return {
        "schema_version": SCHEMA_VERSION,
        "candidate_sha256": candidate_sha256,
        "plan_sha256": plan_sha256,
        "gate": gate,
        "started_utc": started,
        "ended_utc": ended,
    }


def run_gate(args, runner=None):
    runner = runner or SubprocessRunner()
    engine_metadata = read_json(args.engine_metadata)
    agent_metadata = read_json(args.agent_metadata)
    candidate = candidate_contract(engine_metadata)
    validate_agent_metadata(agent_metadata, candidate)
    candidate_sha256 = canonical_digest(candidate)
    plan_sha256 = canonical_digest(plan_contract(args, agent_metadata))
    expected = expected_identity(candidate)
    successful = load_prior(args.output, candidate_sha256, plan_sha256, args.resume)

    started = utc_now()
    try:
        assert_identity(runner.inspect(args.container), expected)
        status = "passed"
        error = None
    except GateError as caught:
        status = "failed"
        error = str(caught)
    ended = utc_now()
    identity_record = record_base(candidate_sha256, plan_sha256, "identity", started, ended)
    identity_record.update(
        {
            "status": status,
            "receipt_verified": candidate["receipt_verified"],
            "error": error,
        }
    )
    append_record(args.output, identity_record)
    if status != "passed":
        return 1

    stages = build_stages(args)[: THROUGH_ORDER[args.through]]
    for stage in stages:
        if stage.name in successful:
            now = utc_now()
            record = record_base(candidate_sha256, plan_sha256, stage.name, now, now)
            record["status"] = "resumed"
            append_record(args.output, record)
            continue

        stage_started = utc_now()
        monotonic_started = time.monotonic()
        try:
            assert_identity(runner.inspect(args.container), expected)
            result = runner.run(stage.argv, env=dict(stage.env))
            assert_identity(runner.inspect(args.container), expected)
            logs = runner.logs(args.container, stage_started)
            log_payload = logs.stdout + b"\n" + logs.stderr
            markers = runtime_findings(log_payload)
            log_error = logs.returncode != 0
            artifact = store_artifact(args.artifacts_dir, stage.name, result.stdout)
            stage_status = (
                "passed"
                if result.returncode == 0 and not markers and not log_error
                else "failed"
            )
            error_class = None
            if result.returncode != 0:
                error_class = "benchmark_failed"
            elif log_error:
                error_class = "log_scan_failed"
            elif markers:
                error_class = "runtime_marker"
            stage_error = None
        except GateError as caught:
            result = CommandResult(1)
            logs = CommandResult(1)
            log_payload = b"\n"
            markers = []
            artifact = store_artifact(args.artifacts_dir, stage.name, b"")
            stage_status = "failed"
            error_class = "identity_changed"
            stage_error = str(caught)
        stage_ended = utc_now()
        record = record_base(
            candidate_sha256, plan_sha256, stage.name, stage_started, stage_ended
        )
        record.update(
            {
                "status": stage_status,
                "error_class": error_class,
                "error": stage_error,
                "exit_code": result.returncode,
                "wall_seconds": round(time.monotonic() - monotonic_started, 3),
                "artifact": artifact.name,
                "artifact_sha256": bytes_digest(result.stdout),
                "artifact_bytes": len(result.stdout),
                "stderr_sha256": bytes_digest(result.stderr),
                "stderr_bytes": len(result.stderr),
                "runtime_markers": markers,
                "runtime_log_sha256": bytes_digest(log_payload),
                "runtime_log_bytes": len(log_payload),
            }
        )
        append_record(args.output, record)
        if stage_status != "passed":
            return 1
    return 0


def parser():
    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument("--base", required=True)
    root.add_argument("--model", required=True)
    root.add_argument("--container", required=True)
    root.add_argument("--engine-metadata", required=True, type=pathlib.Path)
    root.add_argument("--agent-metadata", required=True, type=pathlib.Path)
    root.add_argument("--output", required=True, type=pathlib.Path)
    root.add_argument("--artifacts-dir", type=pathlib.Path)
    root.add_argument("--through", choices=tuple(THROUGH_ORDER), default="smoke")
    root.add_argument("--resume", action="store_true")
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    if args.artifacts_dir is None:
        args.artifacts_dir = pathlib.Path(str(args.output) + ".artifacts")
    try:
        return run_gate(args)
    except GateError as error:
        print(f"candidate gate: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
