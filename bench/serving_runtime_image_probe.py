#!/usr/bin/env python3
"""Verify a pinned serving-image launch contract without allocating a GPU.

The probe renders the production Compose pair, lets the exact engine launcher
construct its final argv/environment, and replaces only the terminal vllm
executable with a read-only evidence collector. It can either check the pinned
manifest or atomically generate reviewed replacement bytes with ``--output``.
It never starts vLLM, opens a network, mounts model data, or prints raw
argv/environment values.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import tempfile
import time
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "dspark_0731"
BASE = DEPLOY / "docker-compose.yaml"
OVERLAY = DEPLOY / "docker-compose.compatibility-identity.yaml"
MANIFEST = ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json"
DEFAULT_SERVICE = "dspark-0731"
PROBE_PATH = "/opt/venv/bin/vllm"
MANIFEST_TARGET = "/probe/runtime.json"
MAX_CAPTURE_BYTES = 1 << 20
ENGINE_ARGS_SUCCESS = b"engine_args_preflight=passed\n"
MAX_RUNTIME_ARGUMENTS = 256
MAX_RUNTIME_ARGUMENT_BYTES = 4096
MAX_RUNTIME_ARGUMENT_TOTAL_BYTES = 64 << 10
_SHA256 = re.compile(r"[0-9a-f]{64}")
RUNTIME_ROOT_KEYS = {
    "schema_version",
    "compatibility_manifest_sha256",
    "engine",
    "process",
}
RUNTIME_PROCESS_KEYS = {
    "argv",
    "argv_sha256",
    "environment",
    "environment_sha256",
    "packages",
    "packages_sha256",
    "artifacts",
    "artifacts_sha256",
}
KV_EVENT_KEYS = {
    "enable_kv_cache_events",
    "publisher",
    "endpoint",
    "replay_endpoint",
    "buffer_steps",
    "hwm",
    "max_queue_size",
    "topic",
}
LAUNCH_ENVIRONMENT_KEYS = {
    "ALLREDUCE_MODE",
    "BACKEND",
    "BLOCK_SIZE",
    "CUDA_DEVICE_ORDER",
    "CUDA_VISIBLE_DEVICES",
    "DCP_SIZE",
    "DRAFT_SAMPLE_METHOD",
    "DSPARK_CAPACITY_LOG_INTERVAL",
    "DSPARK_DEPTH_MODE",
    "DSPARK_STS_LOG_INTERVAL",
    "DSPARK_TOKENS",
    "EXTRA_VLLM_ARGS",
    "GPU_MEMORY_UTILIZATION",
    "GRAPH",
    "INSTANTTENSOR_BACKEND",
    "KV_OFFLOADING_SIZE",
    "LMCACHE_MODE",
    "LOAD_FORMAT",
    "MAX_MODEL_LEN",
    "MAX_NUM_BATCHED_TOKENS",
    "MAX_NUM_SEQS",
    "MODE",
    "MODEL_PATH",
    "MODEL_REVISION",
    "MODEL_ROOT",
    "NCCL_P2P_DISABLE",
    "PORT",
    "REJECTION_SAMPLE_METHOD",
    "SERVED_MODEL_NAME",
    "TOKENIZER_REVISION",
    "TP_SIZE",
}

WRAPPER = r'''#!/opt/venv/bin/python
import hashlib
from importlib import metadata
import json
import os
import stat
import sys

MAX_ARTIFACT_BYTES = 1 << 30

def digest_file(path):
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags)
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= MAX_ARTIFACT_BYTES:
            raise RuntimeError("invalid artifact")
        digest = hashlib.sha256()
        total = 0
        while total <= MAX_ARTIFACT_BYTES:
            chunk = os.read(descriptor, min(1 << 20, MAX_ARTIFACT_BYTES + 1 - total))
            if not chunk:
                break
            total += len(chunk)
            digest.update(chunk)
        if total != info.st_size or total > MAX_ARTIFACT_BYTES:
            raise RuntimeError("artifact changed")
        return digest.hexdigest()
    finally:
        os.close(descriptor)

with open("/probe/runtime.json", "rb") as source:
    process = json.load(source)["process"]
environment = {
    key: os.environ.get(key) for key in sorted(process["environment"])
}
evidence = {
    "argv": sys.argv[1:],
    "environment": environment,
    "packages": {
        name: metadata.version(name) for name in sorted(process["packages"])
    },
    "artifacts": [
        {"path": item["path"], "sha256": digest_file(item["path"])}
        for item in process["artifacts"]
    ],
}
print(json.dumps(evidence, sort_keys=True, separators=(",", ":")))
'''

ENGINE_ARGS_PROBE = r'''import json
import vllm.platforms
from vllm.platforms.cpu import CpuPlatform

# Parser construction asks vLLM for a default device even though this probe
# never constructs an engine. Use the CPU platform only for those defaults;
# the exact serving argv and image-native EngineArgs class are still parsed.
vllm.platforms.current_platform = CpuPlatform()

from vllm import AsyncEngineArgs
from vllm.entrypoints.openai.cli_args import make_arg_parser
from vllm.utils.argparse_utils import FlexibleArgumentParser

with open("/probe/runtime.json", "rb") as source:
    argv = json.load(source)["process"]["argv"]
if not argv or argv[0] != "serve":
    raise RuntimeError("invalid serving argv")
parsed = make_arg_parser(FlexibleArgumentParser()).parse_args(argv[1:])
engine = AsyncEngineArgs.from_cli_args(parsed)
engine._check_feature_supported()
print("engine_args_preflight=passed")
'''


class ProbeError(RuntimeError):
    pass


def canonical_json_sha256(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def nul_joined_sha256(values: list[str]) -> str:
    return hashlib.sha256(b"\0".join(value.encode() for value in values)).hexdigest()


def manifest_errors(document: Any) -> list[str]:
    if not isinstance(document, dict) or not isinstance(document.get("process"), dict):
        return ["manifest.process"]
    process = document["process"]
    argv = process.get("argv")
    environment = process.get("environment")
    packages = process.get("packages")
    artifacts = process.get("artifacts")
    if (
        not isinstance(argv, list)
        or not all(isinstance(value, str) for value in argv)
        or not isinstance(environment, dict)
        or not isinstance(packages, dict)
        or not isinstance(artifacts, list)
    ):
        return ["manifest.process"]
    errors = []
    checks = {
        "argv_sha256": nul_joined_sha256(argv),
        "environment_sha256": canonical_json_sha256(environment),
        "packages_sha256": canonical_json_sha256(packages),
        "artifacts_sha256": canonical_json_sha256(artifacts),
    }
    for name, actual in checks.items():
        if process.get(name) != actual:
            errors.append(f"manifest.{name}")
    return errors


def validate_generation_template(document: Any) -> None:
    if (
        not isinstance(document, dict)
        or set(document) != RUNTIME_ROOT_KEYS
        or document.get("schema_version") != 2
        or not isinstance(document.get("compatibility_manifest_sha256"), str)
        or _SHA256.fullmatch(document["compatibility_manifest_sha256"]) is None
    ):
        raise ProbeError("runtime manifest generation template is invalid")
    engine = document.get("engine")
    if (
        not isinstance(engine, dict)
        or set(engine) != {"core_process_count", "kv_events"}
        or type(engine.get("core_process_count")) is not int
        or not 0 < engine["core_process_count"] <= 64
        or not isinstance(engine.get("kv_events"), dict)
        or set(engine["kv_events"]) != KV_EVENT_KEYS
    ):
        raise ProbeError("runtime manifest engine template is invalid")
    process = document.get("process")
    if not isinstance(process, dict) or set(process) != RUNTIME_PROCESS_KEYS:
        raise ProbeError("runtime manifest process template is invalid")


def comparison_errors(expected: dict[str, Any], actual: Any) -> list[str]:
    if not isinstance(actual, dict):
        return ["capture.document"]
    errors = []
    if actual.get("argv") != expected.get("argv"):
        errors.append("argv")
    for group in ("environment", "packages"):
        wanted = expected.get(group)
        observed = actual.get(group)
        if not isinstance(wanted, dict) or not isinstance(observed, dict):
            errors.append(group)
            continue
        for key in sorted(set(wanted) | set(observed)):
            if wanted.get(key) != observed.get(key):
                errors.append(f"{group}.{key}")
    wanted_artifacts = expected.get("artifacts")
    observed_artifacts = actual.get("artifacts")
    if not isinstance(wanted_artifacts, list) or not isinstance(
        observed_artifacts, list
    ):
        errors.append("artifacts")
    elif len(wanted_artifacts) != len(observed_artifacts):
        errors.append("artifacts.count")
    else:
        for index, (wanted, observed) in enumerate(
            zip(wanted_artifacts, observed_artifacts, strict=True)
        ):
            if wanted != observed:
                errors.append(f"artifacts.{index}")
    return errors


def option_json(argv: list[str], name: str) -> Any:
    values = []
    for index, argument in enumerate(argv):
        if argument == name:
            if index + 1 >= len(argv):
                raise ProbeError("generated serving option is incomplete")
            values.append(argv[index + 1])
        elif argument.startswith(f"{name}="):
            values.append(argument.partition("=")[2])
    if len(values) != 1:
        raise ProbeError("generated serving option is ambiguous")
    try:
        return json.loads(values[0])
    except json.JSONDecodeError as error:
        raise ProbeError("generated serving option is invalid") from error


def generated_manifest(template: dict[str, Any], captured: Any) -> dict[str, Any]:
    if not isinstance(template, dict) or not isinstance(template.get("process"), dict):
        raise ProbeError("runtime manifest template is invalid")
    if not isinstance(captured, dict):
        raise ProbeError("captured serving process is invalid")
    process = template["process"]
    argv = captured.get("argv")
    environment = captured.get("environment")
    packages = captured.get("packages")
    artifacts = captured.get("artifacts")
    if (
        not isinstance(argv, list)
        or not argv
        or not all(isinstance(value, str) and value for value in argv)
        or not isinstance(environment, dict)
        or not isinstance(packages, dict)
        or not isinstance(artifacts, list)
        or set(environment) != set(process.get("environment", {}))
        or set(packages) != set(process.get("packages", {}))
        or [item.get("path") for item in artifacts if isinstance(item, dict)]
        != [item.get("path") for item in process.get("artifacts", []) if isinstance(item, dict)]
    ):
        raise ProbeError("captured serving process shape changed")
    if (
        len(argv) > MAX_RUNTIME_ARGUMENTS
        or argv[0] != "serve"
        or sum(len(value.encode("utf-8")) for value in argv)
        > MAX_RUNTIME_ARGUMENT_TOTAL_BYTES
        or any(not generated_string(value) for value in argv)
    ):
        raise ProbeError("captured serving argv is invalid")
    if any(
        not isinstance(key, str)
        or not generated_string(key, 256)
        or not generated_environment_string(value)
        for key, value in environment.items()
    ):
        raise ProbeError("captured serving environment is invalid")
    if any(
        not isinstance(key, str)
        or not generated_string(key, 256)
        or not generated_string(value)
        for key, value in packages.items()
    ):
        raise ProbeError("captured serving packages are invalid")
    if any(
        not isinstance(item, dict)
        or set(item) != {"path", "sha256"}
        or not generated_string(item["path"])
        or not isinstance(item["sha256"], str)
        or _SHA256.fullmatch(item["sha256"]) is None
        for item in artifacts
    ):
        raise ProbeError("captured serving artifacts are invalid")
    if any(
        argument.split("=", 1)[0]
        in {"--api-key", "--token", "--hf-token", "--authorization"}
        for argument in argv
    ):
        raise ProbeError("captured serving argv contains a sensitive option")

    generated_process = {
        "argv": argv,
        "argv_sha256": nul_joined_sha256(argv),
        "environment": environment,
        "environment_sha256": canonical_json_sha256(environment),
        "packages": packages,
        "packages_sha256": canonical_json_sha256(packages),
        "artifacts": artifacts,
        "artifacts_sha256": canonical_json_sha256(artifacts),
    }
    document = copy.deepcopy(template)
    try:
        expected_kv_events = document["engine"]["kv_events"]
    except (KeyError, TypeError) as error:
        raise ProbeError("runtime manifest engine template is invalid") from error
    captured_kv_events = option_json(argv, "--kv-events-config")
    if (
        not isinstance(expected_kv_events, dict)
        or not isinstance(captured_kv_events, dict)
        or set(captured_kv_events) != set(expected_kv_events)
    ):
        raise ProbeError("captured KV-event schema changed")
    document["engine"]["kv_events"] = captured_kv_events
    document["process"] = generated_process
    return document


def generated_string(value: Any, limit: int = MAX_RUNTIME_ARGUMENT_BYTES) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and value.isascii()
        and "\0" not in value
        and len(value.encode()) <= limit
    )


def generated_environment_string(
    value: Any, limit: int = MAX_RUNTIME_ARGUMENT_BYTES
) -> bool:
    return (
        isinstance(value, str)
        and value.isascii()
        and "\0" not in value
        and len(value.encode()) <= limit
    )


def manifest_bytes(document: dict[str, Any]) -> bytes:
    return (json.dumps(document, indent=2) + "\n").encode("ascii")


def write_manifest(path: pathlib.Path, raw: bytes, *, replace: bool) -> None:
    parent = path.parent
    if not parent.is_dir() or parent.is_symlink():
        raise ProbeError("runtime manifest output parent is invalid")
    if parent.stat().st_mode & 0o022:
        raise ProbeError("runtime manifest output parent is writable")
    if path.exists() or path.is_symlink():
        if not replace:
            raise ProbeError("runtime manifest output already exists")
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise ProbeError("runtime manifest output is unsafe")
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=parent)
    temporary_path = pathlib.Path(temporary)
    try:
        os.fchmod(descriptor, 0o644)
        destination = os.fdopen(descriptor, "wb", closefd=True)
        descriptor = -1
        with destination:
            destination.write(raw)
            destination.flush()
            os.fsync(destination.fileno())
        if replace:
            os.replace(temporary_path, path)
        else:
            os.link(temporary_path, path)
            temporary_path.unlink()
        directory = os.open(parent, os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except Exception:
        if descriptor >= 0:
            os.close(descriptor)
        try:
            temporary_path.unlink()
        except FileNotFoundError:
            pass
        raise


def safe_environment(environment: Any) -> dict[str, str]:
    if not isinstance(environment, dict):
        raise ProbeError("rendered engine environment is invalid")
    result = {}
    for key, value in environment.items():
        if not isinstance(key, str):
            raise ProbeError("rendered engine environment is invalid")
        if key.startswith("RAMJET_") or sensitive_environment_name(key):
            continue
        if key not in LAUNCH_ENVIRONMENT_KEYS:
            raise ProbeError("rendered engine environment has an unreviewed setting")
        if not isinstance(value, (str, int, float, bool)):
            raise ProbeError("rendered engine environment is invalid")
        if key == "EXTRA_VLLM_ARGS" and any(
            option in str(value)
            for option in ("--api-key", "--token", "--hf-token", "--authorization")
        ):
            raise ProbeError("rendered engine arguments contain a sensitive option")
        result[key] = str(value)
    return result


def sensitive_environment_name(value: str) -> bool:
    return (
        "SECRET" in value
        or "PASSWORD" in value
        or "CREDENTIAL" in value
        or "ACCESS_KEY" in value
        or "PRIVATE_KEY" in value
        or "BEARER" in value
        or value.endswith("_TOKEN")
        or value.endswith("_API_KEY")
        or value.endswith("_AUTHORIZATION")
    )


def render(overlays: list[pathlib.Path] | None = None) -> dict[str, Any]:
    selected = [OVERLAY] if overlays is None else overlays
    command = ["docker", "compose", "-f", str(BASE)]
    for overlay in selected:
        command.extend(("-f", str(overlay)))
    command.extend(("config", "--format", "json"))
    try:
        completed = subprocess.run(
            command,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ProbeError("serving Compose render failed") from error
    if completed.returncode != 0:
        raise ProbeError("serving Compose render failed")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ProbeError("serving Compose render is invalid") from error


def container_process(service: Any) -> tuple[str, list[str]]:
    if not isinstance(service, dict):
        raise ProbeError("rendered engine service is invalid")
    entrypoint = service.get("entrypoint")
    if (
        not isinstance(entrypoint, list)
        or not entrypoint
        or len(entrypoint) > 8
        or not all(generated_string(value) for value in entrypoint)
    ):
        raise ProbeError("rendered engine entrypoint is invalid")
    arguments = list(entrypoint[1:])
    command = service.get("command")
    if command is not None:
        if (
            not isinstance(command, list)
            or len(command) > MAX_RUNTIME_ARGUMENTS
            or not all(generated_string(value) for value in command)
        ):
            raise ProbeError("rendered engine command is invalid")
        arguments.extend(command)
    return entrypoint[0], arguments


def run_probe(
    document: dict[str, Any],
    manifest_path: pathlib.Path,
    service_name: str,
    timeout_seconds: float,
) -> tuple[dict[str, Any], str, int]:
    try:
        service = document["services"][service_name]
        image = service["image"]
    except (KeyError, TypeError) as error:
        raise ProbeError("rendered engine service is unavailable") from error
    if (
        not isinstance(image, str)
        or "@sha256:" not in image
        or len(image.rpartition("@sha256:")[2]) != 64
    ):
        raise ProbeError("rendered engine image is not immutable")
    environment = safe_environment(service.get("environment"))
    entrypoint, process_arguments = container_process(service)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ramjet-runtime-probe-") as directory:
        wrapper = pathlib.Path(directory) / "vllm"
        wrapper.write_text(WRAPPER, encoding="ascii")
        wrapper.chmod(0o555)
        command = [
            "docker",
            "run",
            "--rm",
            "--pull",
            "never",
            "--runtime",
            "runc",
            "--network",
            "none",
            "--read-only",
            "--tmpfs",
            "/cache:rw,nosuid,nodev,noexec,size=64m",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,noexec,size=16m",
            "--entrypoint",
            entrypoint,
            "--volume",
            f"{wrapper}:{PROBE_PATH}:ro",
            "--volume",
            f"{manifest_path.resolve()}:{MANIFEST_TARGET}:ro",
        ]
        for key, value in sorted(environment.items()):
            command.extend(("--env", f"{key}={value}"))
        command.append(image)
        command.extend(process_arguments)
        try:
            completed = subprocess.run(
                command,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                timeout=timeout_seconds,
            )
        except (OSError, subprocess.SubprocessError) as error:
            raise ProbeError("serving runtime image probe failed") from error
    elapsed_ms = round((time.monotonic() - started) * 1000)
    if completed.returncode != 0 or len(completed.stdout) > MAX_CAPTURE_BYTES:
        raise ProbeError("serving runtime image probe failed")
    try:
        captured = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ProbeError("serving runtime image probe returned invalid evidence") from error
    return captured, image, elapsed_ms


def engine_args_command(
    document: dict[str, Any], manifest_path: pathlib.Path, service_name: str
) -> list[str]:
    try:
        service = document["services"][service_name]
        image = service["image"]
    except (KeyError, TypeError) as error:
        raise ProbeError("rendered engine service is unavailable") from error
    if (
        not isinstance(image, str)
        or "@sha256:" not in image
        or len(image.rpartition("@sha256:")[2]) != 64
    ):
        raise ProbeError("rendered engine image is not immutable")
    environment = safe_environment(service.get("environment"))
    command = [
        "docker",
        "run",
        "--rm",
        "--pull",
        "never",
        "--runtime",
        "runc",
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,noexec,size=16m",
        "--entrypoint",
        "/opt/venv/bin/python",
        "--volume",
        f"{manifest_path.resolve()}:{MANIFEST_TARGET}:ro",
    ]
    for key, value in sorted(environment.items()):
        command.extend(("--env", f"{key}={value}"))
    command.extend((image, "-c", ENGINE_ARGS_PROBE))
    return command


def run_engine_args_probe(
    document: dict[str, Any],
    manifest_path: pathlib.Path,
    service_name: str,
    timeout_seconds: float,
) -> int:
    started = time.monotonic()
    try:
        completed = subprocess.run(
            engine_args_command(document, manifest_path, service_name),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=timeout_seconds,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ProbeError("serving EngineArgs probe failed") from error
    if (
        completed.returncode != 0
        or len(completed.stdout) > MAX_CAPTURE_BYTES
        or not completed.stdout.endswith(ENGINE_ARGS_SUCCESS)
        or completed.stdout.count(ENGINE_ARGS_SUCCESS) != 1
    ):
        raise ProbeError("serving EngineArgs probe failed")
    return round((time.monotonic() - started) * 1000)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=pathlib.Path, default=MANIFEST)
    parser.add_argument("--service", default=DEFAULT_SERVICE)
    parser.add_argument(
        "--compose-overlay",
        action="append",
        type=pathlib.Path,
        help="Compose overlay to render; repeat for multiple overlays",
    )
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        help="atomically write image-derived manifest bytes instead of checking",
    )
    parser.add_argument(
        "--replace",
        action="store_true",
        help="replace one existing regular output file (requires --output)",
    )
    parser.add_argument(
        "--validate-engine-args",
        action="store_true",
        help="parse the exact receipt argv through image-native EngineArgs",
    )
    arguments = parser.parse_args()
    if not 1 <= arguments.timeout_seconds <= 120:
        raise SystemExit("timeout must be between 1 and 120 seconds")
    if arguments.replace and arguments.output is None:
        raise SystemExit("--replace requires --output")
    if arguments.validate_engine_args and arguments.output is not None:
        raise SystemExit("--validate-engine-args cannot be combined with --output")
    try:
        raw = arguments.manifest.read_bytes()
        manifest = json.loads(raw)
        validate_generation_template(manifest)
        errors = manifest_errors(manifest) if arguments.output is None else []
        rendered = render(arguments.compose_overlay)
        captured, image, elapsed_ms = run_probe(
            rendered,
            arguments.manifest,
            arguments.service,
            arguments.timeout_seconds,
        )
        generated = generated_manifest(manifest, captured)
        generated_raw = manifest_bytes(generated)
        if arguments.output is None:
            errors.extend(comparison_errors(manifest["process"], captured))
            if generated.get("engine") != manifest.get("engine"):
                errors.append("engine")
        else:
            write_manifest(arguments.output, generated_raw, replace=arguments.replace)
    except (OSError, json.JSONDecodeError):
        print(
            json.dumps(
                {"status": "failed", "reason": "serving runtime probe input failed"}
            )
        )
        return 1
    except ProbeError as error:
        print(json.dumps({"status": "failed", "reason": str(error)}))
        return 1
    if errors:
        print(
            json.dumps(
                {"status": "mismatch", "fields": sorted(set(errors))},
                sort_keys=True,
                separators=(",", ":"),
            )
        )
        return 1
    engine_args_elapsed_ms = None
    if arguments.validate_engine_args:
        try:
            engine_args_elapsed_ms = run_engine_args_probe(
                rendered,
                arguments.manifest,
                arguments.service,
                arguments.timeout_seconds,
            )
        except ProbeError as error:
            print(json.dumps({"status": "failed", "reason": str(error)}))
            return 1
    if arguments.output is not None:
        print(
            json.dumps(
                {
                    "status": "generated",
                    "service": arguments.service,
                    "image": image,
                    "manifest_sha256": hashlib.sha256(generated_raw).hexdigest(),
                    "template_match": generated_raw == raw,
                    "elapsed_ms": elapsed_ms,
                },
                sort_keys=True,
                separators=(",", ":"),
            )
        )
        return 0
    process = manifest["process"]
    report = {
        "status": "match",
        "service": arguments.service,
        "image": image,
        "manifest_sha256": hashlib.sha256(raw).hexdigest(),
        "argv_sha256": process["argv_sha256"],
        "environment_sha256": process["environment_sha256"],
        "packages_sha256": process["packages_sha256"],
        "artifacts_sha256": process["artifacts_sha256"],
        "elapsed_ms": elapsed_ms,
    }
    if engine_args_elapsed_ms is not None:
        report["engine_args_elapsed_ms"] = engine_args_elapsed_ms
    print(
        json.dumps(
            report,
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
