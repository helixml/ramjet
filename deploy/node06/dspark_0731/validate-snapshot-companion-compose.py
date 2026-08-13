#!/usr/bin/env python3
"""Render and semantically validate the offline companion Compose sandbox."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys


HERE = pathlib.Path(__file__).resolve().parent
OVERLAY = HERE / "docker-compose.snapshot-companion-offline.yaml"
SERVICE = "snapshot-companion-offline"
CLIENT = "snapshot-lb-offline"


def fail(message: str) -> None:
    raise SystemExit(f"snapshot companion compose validation failed: {message}")


def render() -> dict:
    environment = os.environ.copy()
    environment.update(
        {
            "SNAPSHOT_RUNTIME_DIR": "/run/mini-dynamo-snapshot-offline",
            "SNAPSHOT_SESSION_SECRET_FILE": "/run/secrets/mini-dynamo-snapshot-session",
            "SNAPSHOT_FIXTURE_DIR": "/var/lib/mini-dynamo/snapshot-fixtures",
        }
    )
    command = [
        "docker",
        "compose",
        "-f",
        str(OVERLAY),
        "--profile",
        "snapshot-companion-offline",
        "config",
        "--format",
        "json",
    ]
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    if completed.returncode != 0:
        fail("docker compose could not render the offline profile")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        fail("docker compose did not produce JSON")


def require_profile_off_by_default() -> None:
    completed = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(OVERLAY),
            "config",
            "--services",
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0 or completed.stdout.strip():
        fail("offline services are active without the explicit profile")


def volume_by_target(service: dict, target: str) -> dict:
    matches = [volume for volume in service.get("volumes", []) if volume.get("target") == target]
    if len(matches) != 1:
        fail(f"expected exactly one mount at {target}")
    return matches[0]


def main() -> int:
    require_profile_off_by_default()
    document = render()
    services = document.get("services", {})
    companion = services.get(SERVICE)
    load_balancer = services.get(CLIENT)
    if companion is None or load_balancer is None:
        fail("required services are absent")
    if set(services) != {SERVICE, CLIENT}:
        fail("offline file contains an unexpected service")

    for name, service in services.items():
        if service.get("profiles") != ["snapshot-companion-offline"]:
            fail(f"{name} is not guarded by the offline profile")
        if service.get("network_mode") != "none" or service.get("ports"):
            fail(f"{name} has network or published port access")
        if service.get("read_only") is not True:
            fail(f"{name} root filesystem is writable")
        if service.get("cap_drop") != ["ALL"]:
            fail(f"{name} Linux capabilities are not fully dropped")
        if "no-new-privileges:true" not in service.get("security_opt", []):
            fail(f"{name} no-new-privileges is absent")
        if service.get("healthcheck", {}).get("test", [None])[0] != "CMD":
            fail(f"{name} exec-form healthcheck is absent")
    if companion.get("user") != "12001:12000":
        fail("companion UID/GID contract changed")
    if load_balancer.get("user") != "12002:12000":
        fail("LB fixture UID/GID contract changed")
    if companion.get("pids_limit") != 128:
        fail("PID limit changed")
    if int(companion.get("mem_limit", 0)) != 512 * 1024 * 1024:
        fail("memory limit changed")

    runtime = volume_by_target(companion, "/run/mini-dynamo-snapshot")
    secret = volume_by_target(companion, "/run/secrets/snapshot-session")
    fixtures = volume_by_target(companion, "/fixtures")
    lb_runtime = volume_by_target(load_balancer, "/run/mini-dynamo-snapshot")
    lb_secret = volume_by_target(load_balancer, "/run/secrets/snapshot-session")
    if runtime.get("type") != "bind" or runtime.get("read_only", False):
        fail("companion runtime mount is not its sole writable bind")
    if secret.get("type") != "bind" or secret.get("read_only") is not True:
        fail("secret mount is not read-only")
    if lb_secret.get("source") != secret.get("source") or lb_secret.get("read_only") is not True:
        fail("LB fixture does not receive the same read-only session secret")
    if fixtures.get("type") != "bind" or fixtures.get("read_only") is not True:
        fail("offline fixture mount is not read-only")
    if lb_runtime.get("source") != runtime.get("source") or lb_runtime.get("read_only") is not True:
        fail("LB socket mount is not the same read-only runtime directory")

    runtime_source = runtime.get("source")
    holders = []
    for name, service in services.items():
        for volume in service.get("volumes", []):
            if volume.get("source") == runtime_source:
                holders.append(name)
    if sorted(holders) != sorted([CLIENT, SERVICE]):
        fail("runtime directory is shared with an unexpected service")

    print("snapshot companion compose validation passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
