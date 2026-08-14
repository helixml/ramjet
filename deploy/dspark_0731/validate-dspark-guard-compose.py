#!/usr/bin/env python3
"""Render and validate the explicit durable DSpark quarantine overlay."""

from __future__ import annotations

import copy
import json
import os
import pathlib
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
BASE = HERE / "docker-compose.yaml"
IDENTITY = HERE / "docker-compose.compatibility-identity.yaml"
GUARD = HERE / "docker-compose.dspark-guard-quarantine.yaml"
SOURCE = "/run/mini-dynamo-dspark-guard"
TARGET = "/run/mini-dynamo-dspark-guard"
STATE_PATH = f"{TARGET}/state.json"
ENGINES = ("dspark-0731", "dspark-0731-b")
EXPECTED_ENVIRONMENT = {
    "DS4_UPSTREAM_ADMISSION_MODE": "compatibility",
    "DS4_DSPARK_GUARD_MODE": "quarantine",
    "DS4_DSPARK_GUARD_INTERVAL_MS": "5000",
    "DS4_DSPARK_GUARD_CONSECUTIVE_WINDOWS": "3",
    "DS4_DSPARK_GUARD_MIN_PROPOSED_TOKENS": "256",
    "DS4_DSPARK_GUARD_EXPECTED_POSITIONS": "5",
    "DS4_DSPARK_GUARD_STATE_PATH": STATE_PATH,
    "DS4_DSPARK_GUARD_STATE_OWNER_UID": "0",
    "DS4_DSPARK_GUARD_STATE_GROUP_GID": "0",
}


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(*, enabled: bool) -> dict[str, Any]:
    command = ["docker", "compose", "-f", str(BASE), "-f", str(IDENTITY)]
    if enabled:
        command.extend(["-f", str(GUARD)])
    command.extend(["config", "--format", "json"])
    environment = os.environ.copy()
    environment["DSPARK_GUARD_STATE_DIR"] = SOURCE
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    if completed.returncode != 0:
        fail("docker compose could not render DSpark guard profile")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError("docker compose did not produce JSON") from error


def volume_by_target(service: dict[str, Any], target: str) -> dict[str, Any] | None:
    matches = [
        volume
        for volume in service.get("volumes", [])
        if volume.get("target") == target
    ]
    if len(matches) > 1:
        fail("DSpark guard authority is mounted more than once")
    return None if not matches else matches[0]


def validate_disabled(document: dict[str, Any]) -> None:
    load_balancer = document["services"]["ds4-loadbalancer"]
    environment = load_balancer.get("environment", {})
    if environment.get("DS4_DSPARK_GUARD_MODE") != "off":
        fail("identity-only profile enables DSpark enforcement")
    if environment.get("DS4_UPSTREAM_ADMISSION_MODE") != "http":
        fail("identity-only profile enables compatibility admission")
    if volume_by_target(load_balancer, TARGET) is not None:
        fail("identity-only profile mounts DSpark guard authority")


def validate_enabled(document: dict[str, Any], disabled: dict[str, Any]) -> None:
    for engine in ENGINES:
        if document["services"][engine] != disabled["services"][engine]:
            fail("DSpark guard overlay changes an engine")
    load_balancer = document["services"]["ds4-loadbalancer"]
    environment = load_balancer.get("environment", {})
    for key, expected in EXPECTED_ENVIRONMENT.items():
        if str(environment.get(key)) != expected:
            fail(f"DSpark guard environment changed: {key}")
    mount = volume_by_target(load_balancer, TARGET)
    if mount is None:
        fail("DSpark guard authority mount is missing")
    if (
        mount.get("type") != "bind"
        or mount.get("source") != SOURCE
        or mount.get("read_only") is True
    ):
        fail("DSpark guard authority is not the exact read-write bind")
    if mount.get("bind", {}).get("create_host_path") is not False:
        fail("DSpark guard authority may create a missing host path")

    expected = copy.deepcopy(disabled["services"]["ds4-loadbalancer"])
    expected_environment = expected.setdefault("environment", {})
    expected_environment.update(EXPECTED_ENVIRONMENT)
    expected.setdefault("volumes", []).append(mount)
    if load_balancer != expected:
        fail("DSpark guard overlay changes unrelated load-balancer settings")


def main() -> int:
    try:
        disabled = render(enabled=False)
        enabled = render(enabled=True)
        validate_disabled(disabled)
        validate_enabled(enabled, disabled)
    except ValidationError as error:
        print(str(error))
        return 1
    print("DSpark guard Compose validation passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
