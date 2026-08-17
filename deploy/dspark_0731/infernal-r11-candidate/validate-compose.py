#!/usr/bin/env python3
"""Prove the r11 qualification render isolates production from candidate B."""

from __future__ import annotations

import copy
import json
import pathlib
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
BASE = HERE.parent / "docker-compose.yaml"
OVERLAY = HERE / "docker-compose.overlay.yaml"
CANDIDATE_IMAGE = (
    "voipmonitor/vllm:infernal-invocation-vllm908522a-"
    "b12x5d648d9-fi1ac6942-cu133-torch213-20260813-r11@"
    "sha256:01b973d1ae132882bcc1bf62ea232f6aabe649dd4a89b961d81f3c41cc53f971"
)
ENTRYPOINT = [
    "/usr/local/bin/lmcache-mp-wrapper.sh",
    "/usr/local/bin/serve-ds4-flash.sh",
]
SINGLE_HOME = {
    "RJ_UPSTREAM": "http://dspark-0731:8000",
    "RJ_KV_EVENT_LIVE_ENDPOINTS": "tcp://dspark-0731:5557",
    "RJ_KV_EVENT_REPLAY_ENDPOINTS": "tcp://dspark-0731:5558",
}
MATCHED_ENGINE_ENVIRONMENT = {
    "MODEL_PATH": "/workspace/model",
    "MODEL_REVISION": "9e165c30e2704aec5d9d593cce3eebd58bbef1cb",
    "TOKENIZER_REVISION": "9e165c30e2704aec5d9d593cce3eebd58bbef1cb",
    "DRAFT_SAMPLE_METHOD": "probabilistic",
    "REJECTION_SAMPLE_METHOD": "standard",
    "GRAPH": "96",
    "LOAD_FORMAT": "instanttensor",
    "INSTANTTENSOR_BACKEND": "BUFFERED",
    "LMCACHE_MODE": "off",
}
EXTRA_VOLUMES = [
    {
        "type": "bind",
        "source": "/prod/engine-cache/infernal-invocation-cu133-r11",
        "target": "/cache",
        "bind": {},
    },
    {
        "type": "bind",
        "source": "/prod/engine-cache/infernal-invocation-cu133-r11/tmp",
        "target": "/container-tmp",
        "bind": {},
    },
]


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(*, candidate: bool) -> dict[str, Any]:
    command = ["docker", "compose", "-f", str(BASE)]
    if candidate:
        command.extend(["-f", str(OVERLAY)])
    command.extend(["config", "--format", "json"])
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        fail("docker compose could not render Infernal r11 profile")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError("docker compose did not produce JSON") from error


def validate(base: dict[str, Any], candidate: dict[str, Any]) -> None:
    base_services = base.get("services", {})
    services = candidate.get("services", {})
    if set(services) != set(base_services):
        fail("r11 overlay changes the service set")
    if services.get("dspark-0731") != base_services.get("dspark-0731"):
        fail("r11 overlay changes engine A")

    expected_lb = copy.deepcopy(base_services["ds4-loadbalancer"])
    expected_lb.setdefault("environment", {}).update(SINGLE_HOME)
    if services.get("ds4-loadbalancer") != expected_lb:
        fail("r11 overlay does not exactly single-home the load balancer")

    expected_b = copy.deepcopy(base_services["dspark-0731-b"])
    expected_b["image"] = CANDIDATE_IMAGE
    expected_b["entrypoint"] = ENTRYPOINT
    expected_b.setdefault("environment", {}).update(MATCHED_ENGINE_ENVIRONMENT)
    expected_b.setdefault("volumes", []).extend(copy.deepcopy(EXTRA_VOLUMES))
    if services.get("dspark-0731-b") != expected_b:
        fail("r11 overlay changes unrelated engine-B settings")

    devices = expected_b["deploy"]["resources"]["reservations"]["devices"]
    if len(devices) != 1 or devices[0].get("device_ids") != ["4", "5", "6", "7"]:
        fail("r11 candidate is not isolated to GPUs 4-7")
    ports = expected_b.get("ports", [])
    if len(ports) != 1 or str(ports[0].get("published")) != "8013":
        fail("r11 candidate does not retain the isolated engine-B port")

    expected_document = copy.deepcopy(base)
    expected_document["services"]["ds4-loadbalancer"] = expected_lb
    expected_document["services"]["dspark-0731-b"] = expected_b
    if candidate != expected_document:
        fail("r11 overlay changes unrelated top-level Compose settings")


def main() -> int:
    try:
        base = render(candidate=False)
        merged = render(candidate=True)
        validate(base, merged)
    except ValidationError as error:
        print(str(error))
        return 1
    print("Infernal r11 Compose validation passed: LB on A, candidate on B GPUs 4-7")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
