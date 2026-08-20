#!/usr/bin/env python3
"""Render and semantically validate the production snapshot overlay."""

from __future__ import annotations

import json
import argparse
import os
import pathlib
import subprocess
import sys
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
BASE = HERE / "docker-compose.yaml"
OVERLAY = HERE / "docker-compose.snapshot-companion.yaml"
LB_OVERLAY = HERE / "docker-compose.snapshot-lb.yaml"
CADDY = HERE / "Caddyfile.snapshot-companion"
SYSTEMD = HERE / "systemd"
TMPFILES = SYSTEMD / "tmpfiles.d" / "ramjet-snapshot.conf"
AUTHORITY_UNIT = SYSTEMD / "ramjet-snapshot-authority.service"
SERVING_SERVICE = "ds4-loadbalancer"
COMPANION_PROFILE = "snapshot-companion"
ATTESTATION_PROFILE = "snapshot-attestation"
SESSION_GID = "12000"
LB_UID = "12002"
EXPECTED_LB_IMAGE = (
    "ghcr.io/helixml/ramjet:v0.4.0@"
    "sha256:467e7edf40c8fcad29e741cbba52ca571cbae0261d94cff008aa6bcdb737ea1b"
)
EXPECTED_COMPANION_IMAGE = (
    "ghcr.io/helixml/ramjet:companion-v0.4.0@"
    "sha256:6d00646e40c0a3fed78b8a33d8136e52a0c46f0d5287c84bca00e61f22474d34"
)

DOMAINS: dict[str, dict[str, str]] = {
    "engine-a": {
        "companion": "snapshot-companion-a",
        "provisioner": "snapshot-attestation-a",
        "companion_uid": "12001",
        "metrics_gid": "12004",
        "engine": "dspark-0731",
        "runtime_source": "/run/ramjet-snapshot-a",
        "metrics_source": "/run/ramjet-snapshot-metrics-a",
        "session_source": "/run/secrets/ramjet-snapshot-session-a",
        "digest_source": "/run/secrets/ramjet-snapshot-digest-a",
        "attestation_source": "/run/ramjet-snapshot-attestation-a",
        "metadata_source": "/run/ramjet-engine-metadata-a.json",
        "lb_runtime_target": "/run/ramjet-snapshot-a",
        "lb_session_target": "/run/secrets/snapshot-session-a",
        "lb_digest_target": "/run/secrets/snapshot-digest-a",
        "lb_attestation_target": "/run/attestation-a",
        "caddy_path": "/run/ramjet-snapshot-metrics-a/metrics.sock",
    },
    "engine-b": {
        "companion": "snapshot-companion-b",
        "provisioner": "snapshot-attestation-b",
        "companion_uid": "12003",
        "metrics_gid": "12005",
        "engine": "dspark-0731-b",
        "runtime_source": "/run/ramjet-snapshot-b",
        "metrics_source": "/run/ramjet-snapshot-metrics-b",
        "session_source": "/run/secrets/ramjet-snapshot-session-b",
        "digest_source": "/run/secrets/ramjet-snapshot-digest-b",
        "attestation_source": "/run/ramjet-snapshot-attestation-b",
        "metadata_source": "/run/ramjet-engine-metadata-b.json",
        "lb_runtime_target": "/run/ramjet-snapshot-b",
        "lb_session_target": "/run/secrets/snapshot-session-b",
        "lb_digest_target": "/run/secrets/snapshot-digest-b",
        "lb_attestation_target": "/run/attestation-b",
        "caddy_path": "/run/ramjet-snapshot-metrics-b/metrics.sock",
    },
}


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(
    *,
    companion: bool,
    attestation: bool,
    route_mode: str = "off",
    soak_mode: str = "off",
    lb_image: str | None = None,
    lb_overlay: bool = True,
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
    environment["RJ_SNAPSHOT_ROUTE_MODE"] = route_mode
    environment["RJ_SHADOW_SOAK_MODE"] = soak_mode
    if lb_image is not None:
        environment["SNAPSHOT_LB_IMAGE"] = lb_image
    command = ["docker", "compose", "-f", str(BASE), "-f", str(OVERLAY)]
    if lb_overlay:
        command.extend(["-f", str(LB_OVERLAY)])
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


def tmpfs_bind_sources(service: dict[str, Any]) -> list[str]:
    """Bind-mount sources that live on a filesystem wiped by reboot.

    /run is the one that bit us in #156, and it is the only tmpfs the snapshot
    authority uses. Treat the classic volatile roots the same way so a future
    overlay cannot reintroduce the trap under a different name.
    """
    volatile = ("/run/", "/var/run/", "/dev/shm/", "/tmp/")
    sources: list[str] = []
    for volume in service.get("volumes", []):
        if volume.get("type") != "bind":
            continue
        source = str(volume.get("source", ""))
        if source.startswith(volatile) or source in {"/run", "/var/run", "/tmp"}:
            sources.append(source)
    return sources


def boot_provisioned_paths(path: pathlib.Path = TMPFILES) -> set[str]:
    """Directory paths the tmpfiles.d fragment guarantees to exist at boot."""
    if not path.is_file():
        fail("no boot-time tmpfiles fragment provisions the /run authority")
    provisioned: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        fields = stripped.split()
        # Only directory-creating types establish a mountable parent at boot.
        if len(fields) < 2 or fields[0] not in {"d", "D", "v", "q", "Q"}:
            continue
        provisioned.add(fields[1].rstrip("/"))
    if not provisioned:
        fail("the tmpfiles fragment provisions no directories")
    return provisioned


def unit_directives(path: pathlib.Path = AUTHORITY_UNIT) -> dict[str, set[str]]:
    """Active `Key=value` directives in a systemd unit, ignoring comments.

    Comments matter here: this unit documents why it deliberately avoids
    RequiredBy=, and a raw substring search would read that prose as the
    directive it warns against.
    """
    if not path.is_file():
        fail("no boot-time unit rebuilds the /run authority")
    directives: dict[str, set[str]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith(("#", ";", "[")) or "=" not in stripped:
            continue
        key, _, value = stripped.partition("=")
        directives.setdefault(key.strip(), set()).update(value.split())
    return directives


def validate_serving_path_isolation() -> None:
    """The serving LB must start with no /run state present at all (#157).

    This is the recurrence guard for #156. Rendering the base stack, and the
    base stack plus the companion overlay, must both produce a load balancer
    with zero volatile bind mounts -- so a reboot that wipes /run can never
    stop the container from being created.
    """
    for description, document in (
        ("the base stack", render(companion=False, attestation=False, lb_overlay=False)),
        (
            "the companion overlay",
            render(companion=True, attestation=True, lb_overlay=False),
        ),
    ):
        service = document.get("services", {}).get(SERVING_SERVICE)
        if service is None:
            fail(f"{description} does not define {SERVING_SERVICE}")
        sources = tmpfs_bind_sources(service)
        if sources:
            fail(
                f"{description} gives {SERVING_SERVICE} a tmpfs bind mount "
                f"({sources[0]}); the serving path must survive a reboot"
            )
        if service.get("user"):
            fail(f"{description} changes the {SERVING_SERVICE} runtime identity")


def validate_boot_authority(document: dict[str, Any]) -> None:
    """Every LB /run mount needs a boot-time provisioner behind it (#157).

    Once docker-compose.snapshot-lb.yaml is applied the serving container does
    depend on tmpfs state, so that state has to be recreated before Docker
    restores containers. An unguarded mount -- one whose parent no boot unit
    creates -- is rejected here rather than discovered at the next reboot.
    """
    service = document.get("services", {}).get(SERVING_SERVICE, {})
    sources = tmpfs_bind_sources(service)
    if not sources:
        fail("the LB overlay render carries no snapshot authority mounts")
    provisioned = boot_provisioned_paths()
    for source in sources:
        parent = str(pathlib.PurePosixPath(source).parent)
        if source.rstrip("/") not in provisioned and parent not in provisioned:
            fail(f"no boot-time unit provisions the LB authority mount {source}")
    directives = unit_directives()
    if "Before" not in directives or "docker.service" not in directives["Before"]:
        fail("the authority unit is not ordered before docker.service")
    if "WantedBy" not in directives or "docker.service" not in directives["WantedBy"]:
        fail("docker.service does not pull in the authority unit")
    # A hard requirement would make provisioner failure block serving, which is
    # the exact coupling #157 exists to prevent.
    if "docker.service" in directives.get("RequiredBy", set()):
        fail("the authority unit blocks docker.service on its own failure")


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


def validate_lb(
    service: dict[str, Any], route_mode: str, soak_mode: str, expected_image: str
) -> None:
    if service.get("user") != f"{LB_UID}:{SESSION_GID}":
        fail("LB does not use the authenticated snapshot client identity")
    image = str(service.get("image", ""))
    if image != expected_image:
        fail("LB image is not the qualified immutable release build")
    environment = service.get("environment", {})
    expected = {
        "RJ_KV_EVENT_MODE": "off",
        "RJ_EXACT_ROUTE_MODE": route_mode,
        "RJ_SNAPSHOT_ROUTE_MODE": route_mode,
        "RJ_SNAPSHOT_ROUTE_COMPANION_UIDS": "12001,12003",
        "RJ_SNAPSHOT_ROUTE_GROUPS": "0:0,0:0",
        "RJ_SNAPSHOT_ROUTE_SECRET_OWNER_UID": "0",
        "RJ_SHADOW_SOAK_MODE": soak_mode,
        "RJ_SHADOW_SOAK_SOURCE_TARGET": "104",
        "RJ_SHADOW_SOAK_COMPARISON_TARGET": "100000",
        "RJ_SHADOW_SOAK_ATTEMPT_LIMIT": "110000",
        "RJ_SHADOW_SOAK_MAX_TOKEN_BYTES": "100663296",
        "RJ_SHADOW_SOAK_TIMEOUT_MS": "300000",
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
        "RJ_SNAPSHOT_COMPANION_MODE": "serve",
        "RJ_SNAPSHOT_COMPANION_UID": domain["companion_uid"],
        "RJ_SNAPSHOT_CLIENT_UID": LB_UID,
        "RJ_SNAPSHOT_SECRET_OWNER_UID": "0",
        "RJ_SNAPSHOT_MAX_CLIENTS": "2",
        "RJ_SNAPSHOT_BLOCK_SIZE": "256",
        "RJ_SNAPSHOT_ATTENTION_KIND": "mla",
        "RJ_SNAPSHOT_LIVE_ENDPOINTS": f"tcp://{domain['engine']}:5557",
        "RJ_SNAPSHOT_REPLAY_ENDPOINTS": f"tcp://{domain['engine']}:5558",
        "RJ_SNAPSHOT_METRICS_GROUP_GID": domain["metrics_gid"],
    }
    for key, value in exact.items():
        if str(environment.get(key)) != value:
            fail(f"{name} setting {key} changed")
    if environment.get("RJ_SNAPSHOT_METRICS_BIND") is not None:
        fail(f"{name} enables TCP metrics")
    if environment.get("RJ_SNAPSHOT_METRICS_SOCKET_PATH") != "/run/ramjet-metrics/metrics.sock":
        fail(f"{name} metrics UDS changed")
    require_bind(service, "/run/ramjet-snapshot", domain["runtime_source"], read_only=False)
    require_bind(service, "/run/ramjet-metrics", domain["metrics_source"], read_only=False)
    require_bind(service, "/run/secrets/snapshot-session", domain["session_source"], read_only=True)
    require_bind(service, "/run/secrets/snapshot-digest", domain["digest_source"], read_only=True)
    require_bind(service, "/run/attestation", domain["attestation_source"], read_only=True)
    if len(service.get("volumes", [])) != 5:
        fail(f"{name} has an unexpected authority mount")
    expected_health = [
        "CMD",
        "/ramjet-snapshot-companion",
        "healthcheck",
        "/run/ramjet-snapshot/companion.sock",
    ]
    if service.get("healthcheck", {}).get("test") != expected_health:
        fail(f"{name} healthcheck changed")
    labels = service.get("labels", {})
    if labels.get("org.helixml.ramjet.engine") != engine:
        fail(f"{name} engine label changed")


def validate_provisioner(engine: str, domain: dict[str, str], service: dict[str, Any]) -> None:
    name = domain["provisioner"]
    validate_sandbox(name, service, networked=False)
    if service.get("profiles") != [ATTESTATION_PROFILE]:
        fail(f"{name} is not explicitly profile-gated")
    if service.get("user") != f"0:{SESSION_GID}":
        fail(f"{name} identity changed")
    if service.get("entrypoint") != ["/ramjet-attestation-provisioner"]:
        fail(f"{name} does not run the bounded provisioner")
    image = str(service.get("image", ""))
    if image != EXPECTED_COMPANION_IMAGE:
        fail(f"{name} image is not immutable")
    environment = service.get("environment", {})
    if str(environment.get("RJ_SNAPSHOT_SECRET_OWNER_UID")) != "0" or str(
        environment.get("RJ_SNAPSHOT_SECRET_GROUP_GID")
    ) != SESSION_GID:
        fail(f"{name} output ownership changed")
    require_bind(service, "/run/metadata/engine.json", domain["metadata_source"], read_only=True)
    require_bind(service, "/run/secrets/snapshot-digest", domain["digest_source"], read_only=True)
    require_bind(service, "/run/attestation", domain["attestation_source"], read_only=False)
    if len(service.get("volumes", [])) != 3:
        fail(f"{name} has an unexpected mount")
    labels = service.get("labels", {})
    if labels.get("org.helixml.ramjet.engine") != engine:
        fail(f"{name} engine label changed")


def validate_caddy(path: pathlib.Path = CADDY) -> None:
    text = path.read_text(encoding="utf-8")
    if SESSION_GID not in text or "must never be added" not in text:
        fail("Caddy snippet does not preserve session-group isolation")
    if "/run/secrets/" in text or "ramjet-snapshot-a/companion.sock" in text:
        fail("Caddy snippet exposes snapshot authority")
    expected_proxies: list[str] = []
    for index, domain in enumerate(DOMAINS.values()):
        route = f"handle /metrics/snapshot/{index}"
        upstream = f"reverse_proxy unix/{domain['caddy_path']}"
        expected_proxies.append(upstream)
        if text.count(route) != 1 or text.count(upstream) != 1:
            fail("Caddy metrics routes are incomplete or ambiguous")
    actual_proxies = [
        line.strip()
        for line in text.splitlines()
        if line.strip().startswith("reverse_proxy")
    ]
    if actual_proxies != expected_proxies:
        fail("Caddy snippet contains a non-metrics upstream")
    if text.count("rewrite * /metrics") != len(DOMAINS):
        fail("Caddy metrics routes do not rewrite to the companion endpoint")


def validate_documents(
    companion_document: dict[str, Any],
    full_document: dict[str, Any],
    *,
    route_mode: str = "off",
    soak_mode: str = "off",
    expected_lb_image: str = EXPECTED_LB_IMAGE,
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
    if soak_mode not in {"off", "capture"}:
        fail("shadow soak mode is not off or capture")
    if soak_mode == "capture" and route_mode != "shadow":
        fail("shadow soak capture is not paired with snapshot shadow")
    validate_lb(
        companion_services.get(SERVING_SERVICE, {}),
        route_mode,
        soak_mode,
        expected_lb_image,
    )
    validate_boot_authority(companion_document)
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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--shadow-soak-capture",
        action="store_true",
        help="validate the exact bounded capture experiment render",
    )
    parser.add_argument(
        "--candidate-lb-image",
        help="exact local immutable image ID for the bounded capture render",
    )
    args = parser.parse_args()
    if args.candidate_lb_image and not args.shadow_soak_capture:
        parser.error("--candidate-lb-image requires --shadow-soak-capture")
    if args.candidate_lb_image and not args.candidate_lb_image.startswith("sha256:"):
        parser.error("--candidate-lb-image must be an immutable sha256 image ID")
    try:
        validate_source_bind_policy()
        validate_source_bind_policy(LB_OVERLAY)
        validate_serving_path_isolation()
        if args.shadow_soak_capture:
            capture_companion = render(
                companion=True,
                attestation=False,
                route_mode="shadow",
                soak_mode="capture",
                lb_image=args.candidate_lb_image,
            )
            capture_full = render(
                companion=True,
                attestation=True,
                route_mode="shadow",
                soak_mode="capture",
                lb_image=args.candidate_lb_image,
            )
            validate_documents(
                capture_companion,
                capture_full,
                route_mode="shadow",
                soak_mode="capture",
                expected_lb_image=args.candidate_lb_image or EXPECTED_LB_IMAGE,
            )
            validate_caddy()
            print("snapshot production compose validation passed: bounded capture profile")
            return 0
        companion = render(companion=True, attestation=False)
        full = render(companion=True, attestation=True)
        validate_documents(companion, full)
        shadow_companion = render(
            companion=True, attestation=False, route_mode="shadow"
        )
        shadow_full = render(companion=True, attestation=True, route_mode="shadow")
        validate_documents(shadow_companion, shadow_full, route_mode="shadow")
        capture_companion = render(
            companion=True,
            attestation=False,
            route_mode="shadow",
            soak_mode="capture",
        )
        capture_full = render(
            companion=True,
            attestation=True,
            route_mode="shadow",
            soak_mode="capture",
        )
        validate_documents(
            capture_companion,
            capture_full,
            route_mode="shadow",
            soak_mode="capture",
        )
        validate_caddy()
    except ValidationError as exc:
        print(f"snapshot production compose validation failed: {exc}", file=sys.stderr)
        return 1
    print(
        "snapshot production compose validation passed: two isolated shadow "
        "domains, serving path free of tmpfs mounts"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
