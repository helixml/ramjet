#!/usr/bin/env python3
"""Render and semantically validate the dual offline companion sandbox."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
OVERLAY = HERE / "docker-compose.snapshot-companion-offline.yaml"
PROFILE = "snapshot-companion-offline"
RESERVED_COMPANION_IMAGE = "snapshot-companion.invalid/mini-dynamo:not-built"
RESERVED_CLIENT_IMAGE = "snapshot-lb.invalid/mini-dynamo:not-built"

DOMAINS: dict[str, dict[str, Any]] = {
    "engine-a": {
        "companion": "snapshot-companion-offline-a",
        "client": "snapshot-lb-offline-a",
        "companion_uid": "12001",
        "runtime_source": "/run/mini-dynamo-snapshot-offline-a",
        "runtime_target": "/run/mini-dynamo-snapshot-a",
        "secret_source": "/run/secrets/mini-dynamo-snapshot-session-a",
        "secret_target": "/run/secrets/snapshot-session-a",
        "fixture_source": "/var/lib/mini-dynamo/snapshot-fixtures-a",
        "socket": "/run/mini-dynamo-snapshot-a/companion-a.sock",
    },
    "engine-b": {
        "companion": "snapshot-companion-offline-b",
        "client": "snapshot-lb-offline-b",
        "companion_uid": "12003",
        "runtime_source": "/run/mini-dynamo-snapshot-offline-b",
        "runtime_target": "/run/mini-dynamo-snapshot-b",
        "secret_source": "/run/secrets/mini-dynamo-snapshot-session-b",
        "secret_target": "/run/secrets/snapshot-session-b",
        "fixture_source": "/var/lib/mini-dynamo/snapshot-fixtures-b",
        "socket": "/run/mini-dynamo-snapshot-b/companion-b.sock",
    },
}


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(*, profile: bool) -> dict[str, Any]:
    environment = os.environ.copy()
    environment.update(
        {
            "SNAPSHOT_RUNTIME_DIR_A": DOMAINS["engine-a"]["runtime_source"],
            "SNAPSHOT_RUNTIME_DIR_B": DOMAINS["engine-b"]["runtime_source"],
            "SNAPSHOT_SESSION_SECRET_FILE_A": DOMAINS["engine-a"]["secret_source"],
            "SNAPSHOT_SESSION_SECRET_FILE_B": DOMAINS["engine-b"]["secret_source"],
            "SNAPSHOT_FIXTURE_DIR_A": DOMAINS["engine-a"]["fixture_source"],
            "SNAPSHOT_FIXTURE_DIR_B": DOMAINS["engine-b"]["fixture_source"],
        }
    )
    environment.pop("SNAPSHOT_COMPANION_IMAGE", None)
    environment.pop("SNAPSHOT_LB_IMAGE", None)
    command = ["docker", "compose", "-f", str(OVERLAY)]
    if profile:
        command.extend(["--profile", PROFILE])
    command.extend(["config", "--format", "json"])
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
    except json.JSONDecodeError as exc:
        raise ValidationError("docker compose did not produce JSON") from exc


def volume_by_target(service: dict[str, Any], target: str) -> dict[str, Any]:
    matches = [
        volume
        for volume in service.get("volumes", [])
        if volume.get("target") == target
    ]
    if len(matches) != 1:
        fail(f"expected exactly one mount at {target}")
    return matches[0]


def require_arg(command: list[str], expected: str, service: str) -> None:
    if command.count(expected) != 1:
        fail(f"{service} does not carry exactly one {expected!r} argument")


def validate_default(document: dict[str, Any]) -> None:
    if document.get("services"):
        fail("offline services are active without the explicit profile")


def validate_source_bind_policy(path: pathlib.Path = OVERLAY) -> int:
    """Require create_host_path:false in source, independent of Compose output.

    Older Compose renderers omit this bind option from normalized JSON. The
    security contract belongs to the committed YAML, so inspect each bind
    item at its list indentation and fail closed when the option is absent.
    """
    lines = path.read_text(encoding="utf-8").splitlines()
    bind_items = 0
    for index, line in enumerate(lines):
        stripped = line.lstrip()
        if stripped != "- type: bind":
            continue
        bind_items += 1
        indent = len(line) - len(stripped)
        block: list[str] = []
        for candidate in lines[index + 1 :]:
            candidate_stripped = candidate.lstrip()
            candidate_indent = len(candidate) - len(candidate_stripped)
            if candidate_stripped and candidate_indent <= indent:
                break
            block.append(candidate_stripped)
        if block.count("create_host_path: false") != 1:
            fail("a source bind may create its host path")
    if bind_items == 0:
        fail("offline profile contains no source bind mounts")
    return bind_items


def validate_sandbox(name: str, service: dict[str, Any]) -> None:
    if service.get("profiles") != [PROFILE]:
        fail(f"{name} is not guarded by the offline profile")
    if service.get("network_mode") != "none" or service.get("ports") or service.get("expose"):
        fail(f"{name} has network or published port access")
    if service.get("ipc") != "private" or service.get("pid") == "host":
        fail(f"{name} has host IPC or PID namespace access")
    if service.get("read_only") is not True:
        fail(f"{name} root filesystem is writable")
    if service.get("privileged") is True:
        fail(f"{name} is privileged")
    if service.get("cap_drop") != ["ALL"]:
        fail(f"{name} Linux capabilities are not fully dropped")
    if "no-new-privileges:true" not in service.get("security_opt", []):
        fail(f"{name} no-new-privileges is absent")
    if any(service.get(key) for key in ("devices", "device_requests", "gpus")):
        fail(f"{name} has GPU or host device access")
    if service.get("depends_on") or service.get("links"):
        fail(f"{name} health/lifecycle is coupled to another service")
    if service.get("environment"):
        fail(f"{name} claims a runtime environment contract")
    health = service.get("healthcheck", {}).get("test", [])
    if not health or health[0] != "CMD":
        fail(f"{name} exec-form healthcheck is absent")
    for volume in service.get("volumes", []):
        source = str(volume.get("source", ""))
        normalized_source = source.rstrip("/")
        if normalized_source.endswith("/docker.sock"):
            fail(f"{name} mounts the Docker socket")
        if source in {"/", "/proc", "/sys", "/dev", "/run"}:
            fail(f"{name} mounts a broad host path")
        if volume.get("type") != "bind":
            fail(f"{name} has a non-bind persistent mount")
        if volume.get("bind", {}).get("create_host_path") is True:
            fail(f"{name} may create its host bind source")


def validate_domain(
    engine: str, domain: dict[str, Any], services: dict[str, dict[str, Any]]
) -> None:
    companion_name = domain["companion"]
    client_name = domain["client"]
    companion = services[companion_name]
    client = services[client_name]

    if companion.get("user") != f"{domain['companion_uid']}:12000":
        fail(f"{companion_name} UID/GID contract changed")
    if client.get("user") != "12002:12000":
        fail(f"{client_name} UID/GID contract changed")
    if companion.get("pids_limit") != 128 or int(
        companion.get("mem_limit", 0)
    ) != 512 * 1024 * 1024:
        fail(f"{companion_name} resource bounds changed")
    if client.get("pids_limit") != 64 or int(client.get("mem_limit", 0)) != 256 * 1024 * 1024:
        fail(f"{client_name} resource bounds changed")
    if companion.get("image") != RESERVED_COMPANION_IMAGE:
        fail(f"{companion_name} default image is not reserved under .invalid")
    if client.get("image") != RESERVED_CLIENT_IMAGE:
        fail(f"{client_name} default image is not reserved under .invalid")

    companion_command = companion.get("command", [])
    client_command = client.get("command", [])
    if companion_command[:1] != ["snapshot-companion-fixture"]:
        fail(f"{companion_name} is not explicitly fixture-only")
    if client_command[:1] != ["snapshot-client-fixture"]:
        fail(f"{client_name} is not explicitly fixture-only")
    shared_args = (
        f"--engine-id={engine}",
        f"--socket={domain['socket']}",
        f"--secret={domain['secret_target']}",
        "--fixtures=/fixtures",
    )
    for expected in shared_args:
        require_arg(companion_command, expected, companion_name)
        require_arg(client_command, expected, client_name)
    require_arg(
        companion_command, "--expected-client-uid=12002", companion_name
    )
    require_arg(
        client_command,
        f"--expected-peer-uid={domain['companion_uid']}",
        client_name,
    )

    companion_runtime = volume_by_target(companion, domain["runtime_target"])
    client_runtime = volume_by_target(client, domain["runtime_target"])
    companion_secret = volume_by_target(companion, domain["secret_target"])
    client_secret = volume_by_target(client, domain["secret_target"])
    companion_fixtures = volume_by_target(companion, "/fixtures")
    client_fixtures = volume_by_target(client, "/fixtures")
    if len(companion.get("volumes", [])) != 3 or len(client.get("volumes", [])) != 3:
        fail(f"{engine} authority pair has an unexpected mount")
    if companion_runtime.get("source") != domain[
        "runtime_source"
    ] or companion_runtime.get("read_only", False):
        fail(f"{companion_name} does not exclusively own its writable runtime")
    if client_runtime.get("source") != domain[
        "runtime_source"
    ] or client_runtime.get("read_only") is not True:
        fail(f"{client_name} does not receive its runtime read-only")
    for secret in (companion_secret, client_secret):
        if secret.get("source") != domain["secret_source"] or secret.get("read_only") is not True:
            fail(f"{engine} session secret is not the exact shared read-only bind")
    for fixtures in (companion_fixtures, client_fixtures):
        if fixtures.get("source") != domain[
            "fixture_source"
        ] or fixtures.get("read_only") is not True:
            fail(f"{engine} fixture bind is not exact/read-only")

    expected_companion_health = [
        "CMD",
        "/mini-dynamo-snapshot-companion",
        "healthcheck",
        domain["socket"],
    ]
    expected_client_health = [
        "CMD",
        "/mini-dynamo",
        "snapshot-client-healthcheck",
        domain["socket"],
    ]
    if companion.get("healthcheck", {}).get("test") != expected_companion_health:
        fail(f"{companion_name} healthcheck is not isolated to its own socket")
    if client.get("healthcheck", {}).get("test") != expected_client_health:
        fail(f"{client_name} healthcheck is not isolated to its own socket")

    for name, service, role in (
        (companion_name, companion, "companion"),
        (client_name, client, "client"),
    ):
        labels = service.get("labels", {})
        if labels.get("org.helixml.mini-dynamo.engine") != engine:
            fail(f"{name} engine identity label changed")
        if labels.get("org.helixml.mini-dynamo.role") != role:
            fail(f"{name} role label changed")


def validate_authority_isolation(services: dict[str, dict[str, Any]]) -> None:
    runtime_sources = {domain["runtime_source"] for domain in DOMAINS.values()}
    secret_sources = {domain["secret_source"] for domain in DOMAINS.values()}
    fixture_sources = {domain["fixture_source"] for domain in DOMAINS.values()}
    if any(
        len(sources) != len(DOMAINS)
        for sources in (runtime_sources, secret_sources, fixture_sources)
    ):
        fail("per-engine runtime, secret, or fixture source is shared")
    for engine, domain in DOMAINS.items():
        allowed = {domain["companion"], domain["client"]}
        for source in (
            domain["runtime_source"],
            domain["secret_source"],
            domain["fixture_source"],
        ):
            holders = {
                name
                for name, service in services.items()
                for volume in service.get("volumes", [])
                if volume.get("source") == source
            }
            if holders != allowed:
                fail(f"{engine} authority source is visible outside its pair")
        peer = "engine-b" if engine == "engine-a" else "engine-a"
        peer_tokens = (
            DOMAINS[peer]["socket"],
            DOMAINS[peer]["secret_target"],
            f"--engine-id={peer}",
        )
        for name in allowed:
            flattened = "\0".join(services[name].get("command", []))
            health = "\0".join(services[name].get("healthcheck", {}).get("test", []))
            if any(token in flattened or token in health for token in peer_tokens):
                fail(f"{name} can address the peer authority domain")


def validate_profile(document: dict[str, Any]) -> None:
    services = document.get("services", {})
    expected = {
        name
        for domain in DOMAINS.values()
        for name in (domain["companion"], domain["client"])
    }
    if set(services) != expected:
        fail("offline profile does not contain exactly two isolated pairs")
    for name, service in services.items():
        validate_sandbox(name, service)
    for engine, domain in DOMAINS.items():
        validate_domain(engine, domain, services)
    validate_authority_isolation(services)


def authority_status(
    document: dict[str, Any], health: dict[str, bool]
) -> dict[str, dict[str, Any]]:
    """Project fixture health without allowing cross-engine substitution."""
    validate_profile(document)
    status = {}
    for engine, domain in DOMAINS.items():
        companion_ready = health.get(domain["companion"], False)
        client_ready = health.get(domain["client"], False)
        status[engine] = {
            "socket": domain["socket"],
            "authoritative": companion_ready and client_ready,
        }
    return status


def main() -> int:
    try:
        source_bind_items = validate_source_bind_policy()
        validate_default(render(profile=False))
        profile_document = render(profile=True)
        rendered_bind_items = sum(
            volume.get("type") == "bind"
            for service in profile_document.get("services", {}).values()
            for volume in service.get("volumes", [])
        )
        if rendered_bind_items != source_bind_items:
            fail("rendered bind mounts are not all explicit source bind items")
        validate_profile(profile_document)
    except ValidationError as exc:
        print(f"snapshot companion compose validation failed: {exc}", file=sys.stderr)
        return 1
    print("snapshot companion compose validation passed: two isolated authority domains")
    return 0


if __name__ == "__main__":
    sys.exit(main())
