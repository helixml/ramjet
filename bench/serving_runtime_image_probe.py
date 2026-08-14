#!/usr/bin/env python3
"""Verify the pinned r34 launch contract without allocating a GPU.

The probe renders the production Compose pair, lets the exact engine launcher
construct its final argv/environment, and replaces only the terminal vllm
executable with a read-only evidence collector. It never starts vLLM, opens a
network, mounts model data, or prints raw argv/environment values.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
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
PROBE_PATH = "/probe/vllm"
MANIFEST_TARGET = "/probe/runtime.json"
CONTAINER_PATH = (
    "/probe:/opt/venv/bin:/usr/local/sbin:/usr/local/bin:"
    "/usr/sbin:/usr/bin:/sbin:/bin"
)
MAX_CAPTURE_BYTES = 1 << 20

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
evidence = {
    "argv": sys.argv[1:],
    "environment": {
        key: os.environ.get(key) for key in sorted(process["environment"])
    },
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


def safe_environment(environment: Any) -> dict[str, str]:
    if not isinstance(environment, dict):
        raise ProbeError("rendered engine environment is invalid")
    result = {}
    for key, value in environment.items():
        if not isinstance(key, str):
            raise ProbeError("rendered engine environment is invalid")
        if (
            key.startswith("MINI_DYNAMO_")
            or "SECRET" in key
            or "PASSWORD" in key
            or "CREDENTIAL" in key
            or key.endswith("_TOKEN")
            or key.endswith("_API_KEY")
            or key.endswith("_AUTHORIZATION")
        ):
            continue
        if not isinstance(value, (str, int, float, bool)):
            raise ProbeError("rendered engine environment is invalid")
        if key == "EXTRA_VLLM_ARGS" and any(
            option in str(value)
            for option in ("--api-key", "--token", "--hf-token", "--authorization")
        ):
            raise ProbeError("rendered engine arguments contain a sensitive option")
        result[key] = str(value)
    result["PATH"] = CONTAINER_PATH
    return result


def render() -> dict[str, Any]:
    completed = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(BASE),
            "-f",
            str(OVERLAY),
            "config",
            "--format",
            "json",
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        timeout=15,
    )
    if completed.returncode != 0:
        raise ProbeError("serving Compose render failed")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ProbeError("serving Compose render is invalid") from error


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
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="mini-dynamo-runtime-probe-") as directory:
        wrapper = pathlib.Path(directory) / "vllm"
        wrapper.write_text(WRAPPER, encoding="ascii")
        wrapper.chmod(0o555)
        command = [
            "docker",
            "run",
            "--rm",
            "--network",
            "none",
            "--read-only",
            "--tmpfs",
            "/cache:rw,nosuid,nodev,noexec,size=64m",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,noexec,size=16m",
            "--entrypoint",
            "/usr/local/bin/serve-ds4-flash.sh",
            "--volume",
            f"{wrapper}:{PROBE_PATH}:ro",
            "--volume",
            f"{manifest_path.resolve()}:{MANIFEST_TARGET}:ro",
        ]
        for key, value in sorted(environment.items()):
            command.extend(("--env", f"{key}={value}"))
        command.append(image)
        completed = subprocess.run(
            command,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=timeout_seconds,
        )
    elapsed_ms = round((time.monotonic() - started) * 1000)
    if completed.returncode != 0 or len(completed.stdout) > MAX_CAPTURE_BYTES:
        raise ProbeError("serving runtime image probe failed")
    try:
        captured = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ProbeError("serving runtime image probe returned invalid evidence") from error
    return captured, image, elapsed_ms


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=pathlib.Path, default=MANIFEST)
    parser.add_argument("--service", default=DEFAULT_SERVICE)
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    arguments = parser.parse_args()
    if not 1 <= arguments.timeout_seconds <= 120:
        raise SystemExit("timeout must be between 1 and 120 seconds")
    try:
        raw = arguments.manifest.read_bytes()
        manifest = json.loads(raw)
        errors = manifest_errors(manifest)
        captured, image, elapsed_ms = run_probe(
            render(),
            arguments.manifest,
            arguments.service,
            arguments.timeout_seconds,
        )
        errors.extend(comparison_errors(manifest["process"], captured))
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
    process = manifest["process"]
    print(
        json.dumps(
            {
                "status": "match",
                "service": arguments.service,
                "image": image,
                "manifest_sha256": hashlib.sha256(raw).hexdigest(),
                "argv_sha256": process["argv_sha256"],
                "environment_sha256": process["environment_sha256"],
                "packages_sha256": process["packages_sha256"],
                "artifacts_sha256": process["artifacts_sha256"],
                "elapsed_ms": elapsed_ms,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
