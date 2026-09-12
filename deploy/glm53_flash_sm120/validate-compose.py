#!/usr/bin/env python3
"""Fail-closed semantic validation for the isolated node06 GLM TP2 canary."""

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
if set(services) != {"glm53sm120-b"}:
    fail("the file must define only the isolated candidate service")
service = services["glm53sm120-b"]
if service.get("image") != EXPECTED_IMAGE:
    fail("candidate image is not the reviewed immutable digest")
labels = service.get("labels", {})
if labels.get("ai.ramjet.runtime.base-digest") != EXPECTED_BASE:
    fail("candidate does not identify the reviewed base image")
if labels.get("ai.ramjet.runtime.glm47-parser-sha256") != EXPECTED_PARSER:
    fail("candidate does not identify the reviewed GLM47 parser")
if service.get("restart") not in ("no", None):
    fail("candidate restart policy must remain disabled")
ports = service.get("ports", [])
if len(ports) != 1 or ports[0].get("host_ip") != "127.0.0.1" or ports[0].get("published") != "8062":
    fail("candidate API must bind only 127.0.0.1:8062")
devices = service.get("deploy", {}).get("resources", {}).get("reservations", {}).get("devices", [])
if len(devices) != 1 or devices[0].get("device_ids") != ["4", "5"]:
    fail("candidate must own exactly host GPUs 4 and 5")
command = service.get("command", [])
required = {
    "--tp=2",
    "--quantization=modelopt_mixed",
    "--kv-cache-dtype=fp8_e4m3",
    "--max-running-requests=4",
    "--dsa-prefill-backend=flashinfer_sparse_mla",
    "--dsa-decode-backend=flashinfer_sparse_mla",
    "--tool-call-parser=glm47",
    "--reasoning-parser=glm45",
}
missing = sorted(required - set(command))
if missing:
    fail(f"missing required arguments: {', '.join(missing)}")
if any("hierarchical-cache" in arg for arg in command):
    fail("HiCache must stay off for the initial canary")
print("glm53 SM120 compose validation: ok")
