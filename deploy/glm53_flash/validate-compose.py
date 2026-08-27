#!/usr/bin/env python3
"""Validate the single-file experimental GLM-5.3-Flash node06 deployment."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
COMPOSE = HERE / "docker-compose.yaml"
MODEL_SOURCE = "/prod/models/LibertAIDAI/GLM-5.3-Flash-NVFP4-9e0d74e3cef1"
MODEL_REVISION = "9e0d74e3cef17f634e84fb8e2223707e02616290"
TEST_IMAGE = "sha256:" + "1" * 64
ENGINE_SHAPE = {
    "glm53-a": ("0-11,24-35", ["0", "1", "2", "3"], "8050"),
    "glm53-b": ("12-23,36-47", ["4", "5", "6", "7"], "8051"),
}


def fail(message: str) -> None:
    raise ValueError(message)


def render() -> dict[str, Any]:
    environment = os.environ.copy()
    environment.update({"SGLANG_IMAGE": TEST_IMAGE, "ENGINE_RESTART_POLICY": "no"})
    result = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE), "config", "--format", "json"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    if result.returncode:
        fail("docker compose render failed")
    return json.loads(result.stdout)


def mount_at(service: dict[str, Any], target: str) -> dict[str, Any]:
    matches = [item for item in service.get("volumes", []) if item.get("target") == target]
    if len(matches) != 1:
        fail(f"expected one mount at {target}")
    return matches[0]


def validate_engine(name: str, service: dict[str, Any]) -> None:
    cpuset, gpus, port = ENGINE_SHAPE[name]
    if service.get("image") != TEST_IMAGE or service.get("pull_policy") != "never":
        fail(f"{name} does not require a local immutable image")
    if service.get("entrypoint") != ["/bin/bash", "/opt/ramjet/glm53-launch.sh"]:
        fail(f"{name} launcher changed")
    if service.get("restart") != "no" or service.get("ipc") != "host":
        fail(f"{name} canary restart/IPC policy changed")
    if service.get("cpuset") != cpuset:
        fail(f"{name} NUMA placement changed")
    labels = service.get("labels", {})
    if labels.get("ai.ramjet.model.revision") != MODEL_REVISION:
        fail(f"{name} model revision changed")
    model = mount_at(service, "/workspace/model")
    if model.get("source") != MODEL_SOURCE or model.get("read_only") is not True:
        fail(f"{name} model mount is not the immutable source")
    launcher = mount_at(service, "/opt/ramjet/glm53-launch.sh")
    if launcher.get("read_only") is not True:
        fail(f"{name} launcher is writable")
    devices = (
        service.get("deploy", {})
        .get("resources", {})
        .get("reservations", {})
        .get("devices", [])
    )
    if len(devices) != 1 or devices[0].get("device_ids") != gpus:
        fail(f"{name} GPU placement changed")
    ports = service.get("ports", [])
    if (
        len(ports) != 1
        or ports[0].get("host_ip") != "127.0.0.1"
        or str(ports[0].get("published")) != port
    ):
        fail(f"{name} direct API is not loopback-only")
    environment = service.get("environment", {})
    expected = {
        "GLM_CONTEXT_LENGTH": "262144",
        "GLM_MAX_RUNNING_REQUESTS": "4",
        "GLM_MEM_FRACTION_STATIC": "0.90",
        "GLM_CUDA_GRAPH_MAX_BS": "4",
        "GLM_MAX_MAMBA_CACHE_SIZE": "",
        "GLM_MTP_MODE": "off",
        "GLM_MTP_ADAPTIVE": "on",
        "HF_HUB_OFFLINE": "1",
        "TRANSFORMERS_OFFLINE": "1",
        "XDG_CACHE_HOME": "/root/.cache",
        "SGLANG_OPT_DEEPGEMM_HC_PRENORM": "0",
    }
    for key, value in expected.items():
        if environment.get(key) != value:
            fail(f"{name} unsafe initial default changed: {key}")


def validate(document: dict[str, Any]) -> None:
    services = document.get("services", {})
    if set(services) != {"ds4-loadbalancer", *ENGINE_SHAPE}:
        fail("deployment service set changed")
    for name in ENGINE_SHAPE:
        validate_engine(name, services[name])
    load_balancer = services["ds4-loadbalancer"]
    environment = load_balancer.get("environment", {})
    if environment.get("RJ_UPSTREAM") != "http://glm53-a:8000,http://glm53-b:8000":
        fail("load balancer upstream set changed")
    for key in (
        "RJ_TOKENIZER_MODE",
        "RJ_EXACT_ROUTE_MODE",
        "RJ_KV_EVENT_MODE",
        "RJ_SNAPSHOT_ROUTE_MODE",
    ):
        if environment.get(key) != "off":
            fail(f"unqualified Ramjet feature enabled: {key}")
    if environment.get("RJ_UPSTREAM_ADMISSION_MODE") != "http":
        fail("unqualified compatibility admission enabled")
    if environment.get("RJ_IDLE_DRAIN_MODE") != "off":
        fail("unqualified SGLang parking enabled")
    if "RJ_UPSTREAM_TOKEN" in environment:
        fail("candidate unexpectedly carries bearer authority")
    if set(load_balancer.get("networks", {})) != {"default", "machineview-host"}:
        fail("load balancer network shape changed")


def main() -> int:
    try:
        validate(render())
    except (ValueError, json.JSONDecodeError) as error:
        print(error)
        return 1
    print("GLM-5.3-Flash Compose validation passed: two isolated TP4 candidates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
