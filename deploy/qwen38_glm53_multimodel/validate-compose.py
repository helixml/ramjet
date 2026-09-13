#!/usr/bin/env python3
"""Validate the node06 Qwen/GLM multi-model Ramjet deployment."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
COMPOSE = ROOT / "docker-compose.yaml"
CANDIDATE_IMAGE = (
    "ghcr.io/helixml/ramjet:rust-deadbee@sha256:"
    + "a" * 64
)
EXPECTED_ENV = {
    "RJ_UPSTREAM": "http://qwen38flashnext-a:8000,http://glm53sm120-b:8000,http://glm53sm120-c:8000",
    "RJ_UPSTREAM_MODELS": "qwen3.8-flash-next,glm-5.3-flash,glm-5.3-flash",
    "RJ_MACHINEVIEW_UPSTREAM_GPUS": "0,1,2,3;4,5;6,7",
    "RJ_TOKENIZER_MODE": "off",
    "RJ_EXACT_ROUTE_MODE": "off",
    "RJ_KV_EVENT_MODE": "off",
    "RJ_SNAPSHOT_ROUTE_MODE": "off",
    "RJ_IDLE_DRAIN_MODE": "off",
    "RJ_ROUTE_SPECULATION_MODE": "off",
    "RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MODE": "off",
    "RJ_ROUTE_AFFINITY_HORIZON_MODE": "off",
    "RJ_UPSTREAM_ADMISSION_MODE": "http",
    "RJ_UPSTREAM_WARMUP_MODE": "enforce",
}
EXPECTED_NETWORKS = {
    "qwen-engine": "qwen38_flash_next_default",
    "glm-engine": "glm53_flash_sm120_default",
    "machineview-host": "qwen38_27b_default",
}
IMAGE_RE = re.compile(r"^ghcr\.io/helixml/ramjet:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}$")


def fail(message: str) -> None:
    raise ValueError(message)


def render() -> dict:
    environment = os.environ.copy()
    environment.update(
        {
            "LB_IMAGE": environment.get("LB_IMAGE", CANDIDATE_IMAGE),
            "VLLM_API_KEY": "validator-upstream-token",
            "RJ_UI_AUTH_TOKEN": "validator-ui-token",
        }
    )
    completed = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE), "config", "--format", "json"],
        check=True,
        capture_output=True,
        text=True,
        env=environment,
    )
    return json.loads(completed.stdout)


def validate(document: dict) -> None:
    if document.get("name") != "qwen38_glm53_multimodel":
        fail("unexpected Compose project name")
    services = document.get("services", {})
    if set(services) != {"ds4-loadbalancer"}:
        fail("the multi-model deployment may own only the load balancer")
    service = services["ds4-loadbalancer"]
    image = service.get("image", "")
    if not IMAGE_RE.fullmatch(image):
        fail("LB image must be an immutable GHCR tag@sha256 reference")
    if service.get("container_name") != "ds4-loadbalancer":
        fail("canonical rollout must own the established LB container name")
    if service.get("restart") != "unless-stopped":
        fail("LB restart policy drift")

    environment = service.get("environment", {})
    for key, expected in EXPECTED_ENV.items():
        if environment.get(key) != expected:
            fail(f"{key} must be {expected!r}")
    if environment.get("RJ_UPSTREAM_TOKEN") != "validator-upstream-token":
        fail("upstream bearer was not rendered from the protected environment")
    if environment.get("RJ_UI_AUTH_TOKEN") != "validator-ui-token":
        fail("UI bearer was not rendered from the protected environment")
    if environment["RJ_UPSTREAM_TOKEN"] == environment["RJ_UI_AUTH_TOKEN"]:
        fail("serving and UI credentials must be distinct")
    forbidden = {
        "RJ_ADAPTIVE_CONFIG_PATH",
        "RJ_TOKENIZER_PATH",
        "RJ_TOKENIZER_SHA256",
        "RJ_CHAT_TEMPLATE_PATH",
        "RJ_CHAT_TEMPLATE_SHA256",
        "RJ_KV_EVENT_LIVE_ENDPOINTS",
        "RJ_KV_EVENT_REPLAY_ENDPOINTS",
        "RJ_EXACT_ROUTE_MANIFEST_PATH",
        "RJ_EXACT_ROUTE_MANIFEST_SHA256",
    }
    present = sorted(forbidden.intersection(environment))
    if present:
        fail(f"heterogeneous deployment carries incompatible authority: {present}")

    service_networks = set(service.get("networks", {}))
    if service_networks != set(EXPECTED_NETWORKS):
        fail("LB must join exactly the Qwen, GLM, and machine-view networks")
    networks = document.get("networks", {})
    for key, name in EXPECTED_NETWORKS.items():
        network = networks.get(key, {})
        if network.get("name") != name or network.get("external") is not True:
            fail(f"network {key} must bind external network {name}")

    ports = {
        (port.get("host_ip"), str(port.get("published")), port.get("target"))
        for port in service.get("ports", [])
    }
    expected_ports = {
        ("127.0.0.1", "8006", 8000),
        ("127.0.0.1", "8007", 9090),
        ("100.89.187.17", "8007", 9090),
    }
    if ports != expected_ports:
        fail("public/metrics bind shape drift")
    volumes = service.get("volumes", [])
    if len(volumes) != 1 or volumes[0].get("source") != "/var/lib/ramjet-machineview":
        fail("LB may mount only the machine-view state directory")
    if any(volume.get("source") == "/var/run/docker.sock" for volume in volumes):
        fail("static multi-model LB must not hold the Docker socket")


def main() -> int:
    try:
        validate(render())
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError, ValueError) as error:
        print(f"qwen/glm multi-model compose validation failed: {error}", file=sys.stderr)
        return 1
    print("qwen/glm multi-model compose validation: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
