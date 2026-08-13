#!/usr/bin/env python3
"""Render and semantically validate the production snapshot overlay."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
BASE = HERE / "docker-compose.yaml"
OVERLAY = HERE / "docker-compose.snapshot-companion.yaml"
CADDY = HERE / "Caddyfile.snapshot-companion"
COMPANION_PROFILE = "snapshot-companion"
ATTESTATION_PROFILE = "snapshot-attestation"
SESSION_GID = "12000"
LB_UID = "12002"
EXPECTED_LB_IMAGE = (
    "ghcr.io/helixml/mini-dynamo:rust-53359d5@"
    "sha256:ce108ce45cfdffc2ccb2869b3f526e714e010208c4fed196292911a7d836bd69"
)
EXPECTED_COMPANION_IMAGE = (
    "ghcr.io/helixml/mini-dynamo:companion-rust-53359d5@"
    "sha256:0b031b592acf4c9eea788da8ae20920354f414774e83d08b28c922f1fdadcc03"
)

DOMAINS: dict[str, dict[str, str]] = {
    "engine-a": {
        "companion": "snapshot-companion-a",
        "provisioner": "snapshot-attestation-a",
        "companion_uid": "12001",
        "metrics_gid": "12004",
        "engine": "dspark-0731",
        "runtime_source": "/run/mini-dynamo-snapshot-a",
        "metrics_source": "/run/mini-dynamo-snapshot-metrics-a",
        "session_source": "/run/secrets/mini-dynamo-snapshot-session-a",
        "digest_source": "/run/secrets/mini-dynamo-snapshot-digest-a",
        "attestation_source": "/run/mini-dynamo-snapshot-attestation-a",
        "metadata_source": "/run/mini-dynamo-engine-metadata-a.json",
        "lb_runtime_target": "/run/mini-dynamo-snapshot-a",
        "lb_session_target": "/run/secrets/snapshot-session-a",
        "lb_digest_target": "/run/secrets/snapshot-digest-a",
        "lb_attestation_target": "/run/attestation-a",
        "caddy_path": "/run/mini-dynamo-snapshot-metrics-a/metrics.sock",
    },
    "engine-b": {
        "companion": "snapshot-companion-b",
        "provisioner": "snapshot-attestation-b",
        "companion_uid": "12003",
        "metrics_gid": "12005",
        "engine": "dspark-0731-b",
        "runtime_source": "/run/mini-dynamo-snapshot-b",
        "metrics_source": "/run/mini-dynamo-snapshot-metrics-b",
        "session_source": "/run/secrets/mini-dynamo-snapshot-session-b",
        "digest_source": "/run/secrets/mini-dynamo-snapshot-digest-b",
        "attestation_source": "/run/mini-dynamo-snapshot-attestation-b",
        "metadata_source": "/run/mini-dynamo-engine-metadata-b.json",
        "lb_runtime_target": "/run/mini-dynamo-snapshot-b",
        "lb_session_target": "/run/secrets/snapshot-session-b",
        "lb_digest_target": "/run/secrets/snapshot-digest-b",
        "lb_attestation_target": "/run/attestation-b",
        "caddy_path": "/run/mini-dynamo-snapshot-metrics-b/metrics.sock",
    },
}


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(
    *, companion: bool, attestation: bool, route_mode: str = "off"
) -> dict[str, Any]:
    environment = os.environ.copy()
    for suffix, domain in zip(("A", "B"), DOMAINS.values(), strict=True):
        environment.update(
            {
                f"SNAPSHOT_RUNTIME_DIR_{suffix}": domain["runtime_source"],
                f"SNAPSHOT_METRICS_DIR_{suffix}": domain["metrics_source"],
                f"SNAPSHOT_SESSION_SECRET_FILE_{suffix}": domain["session_source"],
                f"SNAPSHOT_DIGEST_SECRET_FILE_{suffix}": domain["digest_source"],
                f"SNAPSHOT_ATTESTATION_DIR_{suffix}": domain["attestation_source"],
                f"SNAPSHOT_ENGINE_METADATA_FILE_{suffix}": domain["metadata_source"],
            }
        )
    environment["DS4_SNAPSHOT_ROUTE_MODE"] = route_mode
    command = ["docker", "compose", "-f", str(BASE), "-f", str(OVERLAY)]
    if companion:
        command.extend(["--profile", COMPANION_PROFILE])
    if attestation:
        command.extend(["--profile", ATTESTATION_PROFILE])
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
        fail("docker compose could not render the production overlay")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise ValidationError("docker compose did not produce JSON") from exc


def volume_by_target(service: dict[str, Any], target: str) -> dict[str, Any]:
    matches = [item for item in service.get("volumes", []) if item.get("target") == target]
    if len(matches) != 1:
        fail(f"expected exactly one mount at {target}")
    return matches[0]


def require_bind(
    service: dict[str, Any], target: str, source: str, *, read_only: bool
) -> None:
    volume = volume_by_target(service, target)
    if volume.get("type") != "bind" or volume.get("source") != source:
        fail(f"mount at {target} does not use its exact host authority")
    if bool(volume.get("read_only", False)) is not read_only:
        fail(f"mount at {target} has the wrong write policy")
    if volume.get("bind", {}).get("create_host_path") is True:
        fail(f"mount at {target} may create its host authority")


def validate_source_bind_policy(path: pathlib.Path = OVERLAY) -> int:
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
            fail("a production bind may create its host path")
    if bind_items == 0:
        fail("production overlay contains no bind mounts")
    return bind_items


def validate_sandbox(name: str, service: dict[str, Any], *, networked: bool) -> None:
    if service.get("read_only") is not True or service.get("privileged") is True:
        fail(f"{name} does not have a read-only unprivileged root")
    if service.get("cap_drop") != ["ALL"] or service.get("cap_add"):
        fail(f"{name} retains Linux capabilities")
    if "no-new-privileges:true" not in service.get("security_opt", []):
        fail(f"{name} lacks no-new-privileges")
    if service.get("ipc") != "private" or service.get("pid") == "host":
        fail(f"{name} has unsafe IPC/PID sharing")
    if service.get("ports") or service.get("expose"):
        fail(f"{name} publishes a network port")
    if any(service.get(key) for key in ("devices", "device_requests", "gpus")):
        fail(f"{name} receives a GPU or host device")
    if not networked and service.get("network_mode") != "none":
        fail(f"{name} has network access")
    for volume in service.get("volumes", []):
        source = str(volume.get("source", "")).rstrip("/")
        if source.endswith("/docker.sock") or source in {"", "/", "/run", "/proc", "/sys", "/dev"}:
            fail(f"{name} has a broad or privileged host mount")


def validate_lb(service: dict[str, Any], route_mode: str) -> None:
    if service.get("user") != f"{LB_UID}:{SESSION_GID}":
        fail("LB does not use the authenticated snapshot client identity")
    image = str(service.get("image", ""))
    if image != EXPECTED_LB_IMAGE:
        fail("LB image is not the immutable snapshot-shadow build")
    environment = service.get("environment", {})
    expected = {
        "DS4_KV_EVENT_MODE": "off",
        "DS4_SNAPSHOT_ROUTE_MODE": route_mode,
        "DS4_SNAPSHOT_ROUTE_COMPANION_UIDS": "12001,12003",
        "DS4_SNAPSHOT_ROUTE_GROUPS": "0:0,0:0",
        "DS4_SNAPSHOT_ROUTE_SECRET_OWNER_UID": "0",
    }
    for key, value in expected.items():
        if str(environment.get(key)) != value:
            fail(f"LB authority setting {key} changed")
    sources: set[str] = set()
    for domain in DOMAINS.values():
        for target_key, source_key in (
            ("lb_runtime_target", "runtime_source"),
            ("lb_session_target", "session_source"),
            ("lb_digest_target", "digest_source"),
            ("lb_attestation_target", "attestation_source"),
        ):
            require_bind(
                service,
                domain[target_key],
                domain[source_key],
                read_only=True,
            )
            if domain[source_key] in sources:
                fail("LB authority sources are shared across engines")
            sources.add(domain[source_key])


def validate_companion(engine: str, domain: dict[str, str], service: dict[str, Any]) -> None:
    name = domain["companion"]
    validate_sandbox(name, service, networked=True)
    if service.get("profiles") != [COMPANION_PROFILE]:
        fail(f"{name} is not explicitly profile-gated")
    if service.get("user") != f"{domain['companion_uid']}:{SESSION_GID}":
        fail(f"{name} identity changed")
    if service.get("group_add") != [domain["metrics_gid"]]:
        fail(f"{name} metrics-only group changed")
    if domain["metrics_gid"] == SESSION_GID:
        fail(f"{name} metrics and session groups are shared")
    image = str(service.get("image", ""))
    if image != EXPECTED_COMPANION_IMAGE:
        fail(f"{name} image is not immutable")
    environment = service.get("environment", {})
    exact = {
        "DS4_SNAPSHOT_COMPANION_MODE": "serve",
        "DS4_SNAPSHOT_COMPANION_UID": domain["companion_uid"],
        "DS4_SNAPSHOT_CLIENT_UID": LB_UID,
        "DS4_SNAPSHOT_SECRET_OWNER_UID": "0",
        "DS4_SNAPSHOT_MAX_CLIENTS": "2",
        "DS4_SNAPSHOT_BLOCK_SIZE": "256",
        "DS4_SNAPSHOT_ATTENTION_KIND": "mla",
        "DS4_SNAPSHOT_LIVE_ENDPOINTS": f"tcp://{domain['engine']}:5557",
        "DS4_SNAPSHOT_REPLAY_ENDPOINTS": f"tcp://{domain['engine']}:5558",
        "DS4_SNAPSHOT_METRICS_GROUP_GID": domain["metrics_gid"],
    }
    for key, value in exact.items():
        if str(environment.get(key)) != value:
            fail(f"{name} setting {key} changed")
    if environment.get("DS4_SNAPSHOT_METRICS_BIND") is not None:
        fail(f"{name} enables TCP metrics")
    if environment.get("DS4_SNAPSHOT_METRICS_SOCKET_PATH") != "/run/mini-dynamo-metrics/metrics.sock":
        fail(f"{name} metrics UDS changed")
    require_bind(service, "/run/mini-dynamo-snapshot", domain["runtime_source"], read_only=False)
    require_bind(service, "/run/mini-dynamo-metrics", domain["metrics_source"], read_only=False)
    require_bind(service, "/run/secrets/snapshot-session", domain["session_source"], read_only=True)
    require_bind(service, "/run/secrets/snapshot-digest", domain["digest_source"], read_only=True)
    require_bind(service, "/run/attestation", domain["attestation_source"], read_only=True)
    if len(service.get("volumes", [])) != 5:
        fail(f"{name} has an unexpected authority mount")
    expected_health = [
        "CMD",
        "/mini-dynamo-snapshot-companion",
        "healthcheck",
        "/run/mini-dynamo-snapshot/companion.sock",
    ]
    if service.get("healthcheck", {}).get("test") != expected_health:
        fail(f"{name} healthcheck changed")
    labels = service.get("labels", {})
    if labels.get("org.helixml.mini-dynamo.engine") != engine:
        fail(f"{name} engine label changed")


def validate_provisioner(engine: str, domain: dict[str, str], service: dict[str, Any]) -> None:
    name = domain["provisioner"]
    validate_sandbox(name, service, networked=False)
    if service.get("profiles") != [ATTESTATION_PROFILE]:
        fail(f"{name} is not explicitly profile-gated")
    if service.get("user") != f"0:{SESSION_GID}":
        fail(f"{name} identity changed")
    if service.get("entrypoint") != ["/mini-dynamo-attestation-provisioner"]:
        fail(f"{name} does not run the bounded provisioner")
    image = str(service.get("image", ""))
    if image != EXPECTED_COMPANION_IMAGE:
        fail(f"{name} image is not immutable")
    environment = service.get("environment", {})
    if str(environment.get("DS4_SNAPSHOT_SECRET_OWNER_UID")) != "0" or str(
        environment.get("DS4_SNAPSHOT_SECRET_GROUP_GID")
    ) != SESSION_GID:
        fail(f"{name} output ownership changed")
    require_bind(service, "/run/metadata/engine.json", domain["metadata_source"], read_only=True)
    require_bind(service, "/run/secrets/snapshot-digest", domain["digest_source"], read_only=True)
    require_bind(service, "/run/attestation", domain["attestation_source"], read_only=False)
    if len(service.get("volumes", [])) != 3:
        fail(f"{name} has an unexpected mount")
    labels = service.get("labels", {})
    if labels.get("org.helixml.mini-dynamo.engine") != engine:
        fail(f"{name} engine label changed")


def validate_caddy(path: pathlib.Path = CADDY) -> None:
    text = path.read_text(encoding="utf-8")
    if SESSION_GID not in text or "must never be added" not in text:
        fail("Caddy snippet does not preserve session-group isolation")
    if "/run/secrets/" in text or "mini-dynamo-snapshot-a/companion.sock" in text:
        fail("Caddy snippet exposes snapshot authority")
    for index, domain in enumerate(DOMAINS.values()):
        route = f"handle /metrics/snapshot/{index}"
        upstream = f"reverse_proxy unix/{domain['caddy_path']}"
        if text.count(route) != 1 or text.count(upstream) != 1:
            fail("Caddy metrics routes are incomplete or ambiguous")
    if text.count("rewrite * /metrics") != len(DOMAINS):
        fail("Caddy metrics routes do not rewrite to the companion endpoint")


def validate_documents(
    companion_document: dict[str, Any],
    full_document: dict[str, Any],
    *,
    route_mode: str = "off",
) -> None:
    companion_services = companion_document.get("services", {})
    full_services = full_document.get("services", {})
    for domain in DOMAINS.values():
        if domain["companion"] not in companion_services:
            fail("companion profile does not contain both engine services")
        if domain["provisioner"] in companion_services:
            fail("companion profile implicitly runs an attestation provisioner")
        if domain["provisioner"] not in full_services:
            fail("attestation profile does not contain both provisioners")
    if route_mode not in {"off", "shadow"}:
        fail("snapshot route mode is not off or shadow")
    validate_lb(companion_services.get("ds4-loadbalancer", {}), route_mode)
    for engine, domain in DOMAINS.items():
        validate_companion(engine, domain, companion_services[domain["companion"]])
        validate_provisioner(engine, domain, full_services[domain["provisioner"]])

    authority_holders: dict[str, set[str]] = {}
    managed = {
        "ds4-loadbalancer",
        *(domain["companion"] for domain in DOMAINS.values()),
        *(domain["provisioner"] for domain in DOMAINS.values()),
    }
    for name in managed:
        service = full_services[name]
        for volume in service.get("volumes", []):
            authority_holders.setdefault(str(volume.get("source")), set()).add(name)
    for domain in DOMAINS.values():
        peer_names = managed - {
            "ds4-loadbalancer",
            domain["companion"],
            domain["provisioner"],
        }
        for source_key in (
            "runtime_source",
            "metrics_source",
            "session_source",
            "digest_source",
            "attestation_source",
            "metadata_source",
        ):
            holders = authority_holders.get(domain[source_key], set())
            if holders & peer_names:
                fail("one engine authority is mounted by the peer domain")


def main() -> int:
    try:
        validate_source_bind_policy()
        companion = render(companion=True, attestation=False)
        full = render(companion=True, attestation=True)
        validate_documents(companion, full)
        shadow_companion = render(
            companion=True, attestation=False, route_mode="shadow"
        )
        shadow_full = render(companion=True, attestation=True, route_mode="shadow")
        validate_documents(shadow_companion, shadow_full, route_mode="shadow")
        validate_caddy()
    except ValidationError as exc:
        print(f"snapshot production compose validation failed: {exc}", file=sys.stderr)
        return 1
    print("snapshot production compose validation passed: two isolated shadow domains")
    return 0


if __name__ == "__main__":
    sys.exit(main())
