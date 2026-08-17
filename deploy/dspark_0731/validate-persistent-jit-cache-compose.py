#!/usr/bin/env python3
"""Validate the optional image-specific persistent JIT-cache overlay."""

from __future__ import annotations

import copy
import json
import pathlib
import re
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
BASE = HERE / "docker-compose.yaml"
OVERLAY = HERE / "docker-compose.persistent-jit-cache.yaml"
RUNTIME_MANIFEST = ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json"
TARGET = "/cache/jit"
ENGINES = ("dspark-0731", "dspark-0731-b")
IMAGE = (
    "voipmonitor/vllm@"
    "sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b"
)
FINGERPRINT = "vllme2666d9a65-b12x7cecbb2c48-136ce64f2c43f0f8"
SOURCES = {
    "dspark-0731": f"/prod/ramjet/jit-cache/{FINGERPRINT}/engine-a",
    "dspark-0731-b": f"/prod/ramjet/jit-cache/{FINGERPRINT}/engine-b",
}
_FINGERPRINT = re.compile(r"[a-z0-9][a-z0-9._-]{0,127}")


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(*, enabled: bool) -> dict[str, Any]:
    command = ["docker", "compose", "-f", str(BASE)]
    if enabled:
        command.extend(("-f", str(OVERLAY)))
    command.extend(("config", "--format", "json"))
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        timeout=15,
    )
    if completed.returncode != 0:
        fail("persistent JIT-cache Compose render failed")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError("persistent JIT-cache Compose render is invalid") from error


def runtime_manifest() -> dict[str, Any]:
    try:
        document = json.loads(RUNTIME_MANIFEST.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        raise ValidationError("serving runtime manifest is unavailable") from error
    try:
        environment = document["process"]["environment"]
    except (KeyError, TypeError) as error:
        raise ValidationError("serving runtime environment is unavailable") from error
    if not isinstance(environment, dict):
        fail("serving runtime environment is invalid")
    return document


def volume_by_target(service: dict[str, Any], target: str) -> dict[str, Any]:
    matches = [
        volume
        for volume in service.get("volumes", [])
        if volume.get("target") == target
    ]
    if len(matches) != 1:
        fail(f"expected exactly one mount at {target}")
    return matches[0]


def cache_environment(document: dict[str, Any]) -> dict[str, str]:
    environment = document["process"]["environment"]
    selected = {
        key: value
        for key, value in environment.items()
        if key in {"FLASHINFER_WORKSPACE_BASE", "TILELANG_TMP_DIR", "XDG_CACHE_HOME"}
        or key.endswith(("_CACHE_DIR", "_CACHE_PATH", "_CACHE_ROOT"))
    }
    fingerprint = environment.get("LOCAL_INFERENCE_CACHE_FINGERPRINT")
    if (
        fingerprint != FINGERPRINT
        or _FINGERPRINT.fullmatch(str(fingerprint)) is None
        or len(selected) < 12
    ):
        fail("serving runtime cache fingerprint is invalid")
    prefix = f"{TARGET}/{FINGERPRINT}"
    if any(
        not isinstance(value, str)
        or (value != prefix and not value.startswith(f"{prefix}/"))
        for value in selected.values()
    ):
        fail("serving runtime cache path escapes its fingerprint")
    return selected


def validate_disabled(document: dict[str, Any]) -> None:
    for name in ENGINES:
        service = document["services"][name]
        if any(volume.get("target") == TARGET for volume in service.get("volumes", [])):
            fail("base Compose unexpectedly enables persistent JIT cache")


def validate_enabled(
    document: dict[str, Any], base: dict[str, Any] | None = None
) -> None:
    cache_environment(runtime_manifest())
    if base is None:
        base = render(enabled=False)
    if document.get("services", {}).get("ds4-loadbalancer") != base.get("services", {}).get(
        "ds4-loadbalancer"
    ):
        fail("persistent JIT-cache overlay changes the load balancer")

    seen_sources: set[str] = set()
    for name in ENGINES:
        service = document["services"][name]
        if service.get("image") != IMAGE:
            fail(f"{name} image is not the cache-qualified immutable digest")
        mount = volume_by_target(service, TARGET)
        if mount.get("type") != "bind" or mount.get("source") != SOURCES[name]:
            fail(f"{name} cache mount does not use its fingerprinted host path")
        if mount.get("read_only") is True:
            fail(f"{name} cache mount is not writable")
        if mount.get("bind", {}).get("create_host_path") is not False:
            fail(f"{name} cache mount may create its host path")
        if mount["source"] in seen_sources:
            fail("engine JIT-cache writers share a host directory")
        seen_sources.add(mount["source"])

        candidate = copy.deepcopy(service)
        baseline = copy.deepcopy(base["services"][name])
        candidate.pop("image", None)
        baseline.pop("image", None)
        candidate["volumes"] = [
            volume
            for volume in candidate.get("volumes", [])
            if volume.get("target") != TARGET
        ]
        if candidate != baseline:
            fail(f"{name} overlay changes more than image and JIT-cache mount")


def validate_source_bind_policy(path: pathlib.Path = OVERLAY) -> None:
    text = path.read_text(encoding="utf-8")
    if text.count("- type: bind") != len(ENGINES):
        fail("persistent JIT-cache overlay bind count changed")
    if text.count("create_host_path: false") != len(ENGINES):
        fail("persistent JIT-cache overlay may create a host path")


def main() -> int:
    try:
        disabled = render(enabled=False)
        enabled = render(enabled=True)
        validate_source_bind_policy()
        validate_disabled(disabled)
        validate_enabled(enabled, disabled)
    except ValidationError as error:
        print(f"persistent JIT-cache compose validation failed: {error}")
        return 1
    print(
        "persistent JIT-cache compose validation passed: "
        f"{FINGERPRINT}, isolated engine writers"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
