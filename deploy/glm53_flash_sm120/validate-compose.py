#!/usr/bin/env python3
"""Fail-closed semantic validation for the node06 GLM TP2 replicas."""

import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parent
COMPOSE = ROOT / "docker-compose.yaml"
EXPECTED_IMAGE = "sha256:024a988fd0c0e15d80e382073c05657b2d57f52611c324599508cdb62b9debb8"
EXPECTED_BASE = "sha256:ec4243f940a179a27fea21895077efd47cd050501f99a1d2a5fecf7df2e7be71"
EXPECTED_PARSER = "8ed76f9da2aa782b3e9374d00687186624fd383e98acf9ba0d6ac9759e1425d7"


def fail(message: str) -> None:
    raise SystemExit(f"glm53 SM120 compose validation failed: {message}")


rendered = subprocess.run(
    ["docker", "compose", "-f", str(COMPOSE), "config", "--format", "json"],
    cwd=ROOT,
    check=True,
    capture_output=True,
    text=True,
)
config = json.loads(rendered.stdout)
services = config.get("services", {})
EXPECTED_ENGINES = {
    "glm53sm120-b": {
        "port": "8062",
        "devices": ["4", "5"],
        "cache": "/prod/engine-cache-sglang-glm53-sm120-v84-b",
    },
    "glm53sm120-c": {
        "port": "8063",
        "devices": ["6", "7"],
        "cache": "/prod/engine-cache-sglang-glm53-sm120-v84-c",
    },
}
if set(services) != set(EXPECTED_ENGINES):
    fail("the file must define exactly the two reviewed TP2 replicas")

required = {
    "--tp=2",
    "--quantization=modelopt_mixed",
    "--kv-cache-dtype=fp8_e4m3",
    "--max-total-tokens=500000",
    "--max-running-requests=4",
    "--chunked-prefill-size=6144",
    "--max-prefill-tokens=6144",
    "--dsa-prefill-backend=flashinfer_sparse_mla",
    "--dsa-decode-backend=flashinfer_sparse_mla",
    "--tool-call-parser=glm47",
    "--reasoning-parser=glm45",
}
seen_devices = set()
seen_caches = set()
for name, expected in EXPECTED_ENGINES.items():
    service = services[name]
    if service.get("image") != EXPECTED_IMAGE:
        fail(f"{name} image is not the reviewed immutable digest")
    labels = service.get("labels", {})
    if labels.get("ai.ramjet.runtime.base-digest") != EXPECTED_BASE:
        fail(f"{name} does not identify the reviewed base image")
    if labels.get("ai.ramjet.runtime.glm47-parser-sha256") != EXPECTED_PARSER:
        fail(f"{name} does not identify the reviewed GLM47 parser")
    if service.get("restart") not in ("no", None):
        fail(f"{name} restart policy must remain disabled")
    if service.get("cpuset") != "12-23,36-47":
        fail(f"{name} must remain NUMA-local to GPUs 4-7")
    if service.get("environment", {}).get("CUDA_VISIBLE_DEVICES") != "0,1":
        fail(f"{name} must expose exactly two container-local GPU ordinals")

    ports = service.get("ports", [])
    if (
        len(ports) != 1
        or ports[0].get("host_ip") != "127.0.0.1"
        or str(ports[0].get("published")) != expected["port"]
        or ports[0].get("target") != 8000
    ):
        fail(f"{name} API must bind only 127.0.0.1:{expected['port']}")
    devices = (
        service.get("deploy", {})
        .get("resources", {})
        .get("reservations", {})
        .get("devices", [])
    )
    if len(devices) != 1 or devices[0].get("device_ids") != expected["devices"]:
        fail(f"{name} must own exactly host GPUs {expected['devices']}")
    for device in expected["devices"]:
        if device in seen_devices:
            fail(f"host GPU {device} is assigned to more than one replica")
        seen_devices.add(device)

    volumes = service.get("volumes", [])
    caches = [
        volume.get("source")
        for volume in volumes
        if volume.get("target") == "/root/.cache"
    ]
    if caches != [expected["cache"]]:
        fail(f"{name} must use its reviewed private compilation cache")
    if caches[0] in seen_caches:
        fail("the two replicas must not share a writable compilation cache")
    seen_caches.add(caches[0])

    command = service.get("command", [])
    missing = sorted(required - set(command))
    if missing:
        fail(f"{name} missing required arguments: {', '.join(missing)}")
    if any("hierarchical-cache" in arg for arg in command):
        fail(f"{name} must keep HiCache disabled")

if seen_devices != {"4", "5", "6", "7"}:
    fail("the GLM replica set must own exactly host GPUs 4-7")
print("glm53 SM120 compose validation: two TP2 replicas ok")
