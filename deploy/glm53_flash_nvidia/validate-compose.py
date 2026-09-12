#!/usr/bin/env python3
"""Validate the single-file GLM-5.3-Flash NVFP4 node06 deployment.

`docker compose config` proves a file parses. This proves it still says what
was reviewed: the immutable checkpoint and runtime, the isolated NUMA/GPU
split, the canary's bounded first GPU exposure, and that no ramjet authority
derived from an unavailable GLM renderer profile has been switched on.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
COMPOSE = HERE / "docker-compose.yaml"
MODEL_REPOSITORY = "nvidia/GLM-5.3-Flash-NVFP4"
MODEL_REVISION = "423acf37583782c51c142d145aef733d72943d93"
MODEL_SOURCE = f"/prod/models/nvidia/GLM-5.3-Flash-NVFP4-{MODEL_REVISION}"
ENGINE_IMAGE = (
    "vllm/vllm-openai@sha256:"
    "5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93"
)
LB_IMAGE = (
    "ghcr.io/helixml/ramjet:v0.5.0@sha256:"
    "c3fc5723a0dba51f9bb8eced77648cf0b05788039e90fc638fbd8c19adec70d8"
)
ENGINE_SHAPE = {
    "glm53nvidia-a": {
        "cpuset": "0-11,24-35",
        "gpus": ["0", "1", "2", "3"],
        "port": "8060",
        "cache": "/prod/engine-cache-vllm-glm53nvidia-a",
    },
    "glm53nvidia-b": {
        "cpuset": "12-23,36-47",
        "gpus": ["4", "5", "6", "7"],
        "port": "8061",
        "cache": "/prod/engine-cache-vllm-glm53nvidia-b",
    },
}
REQUIRED_ARGUMENTS = {
    "/workspace/model",
    "--served-model-name=glm-5.3-flash",
    f"--revision={MODEL_REVISION}",
    f"--tokenizer-revision={MODEL_REVISION}",
    "--tensor-parallel-size=4",
    "--enable-expert-parallel",
    "--gpu-memory-utilization=0.90",
    "--quantization=modelopt_fp4",
    "--kv-cache-dtype=fp8",
    "--max-model-len=262144",
    "--max-num-seqs=4",
    "--max-num-batched-tokens=8192",
    '--limit-mm-per-prompt={"image":0,"video":0}',
    "--enable-prefix-caching",
    "--enable-prompt-tokens-details",
    "--no-enable-flashinfer-autotune",
    '--kv-events-config={"enable_kv_cache_events":true,"publisher":"zmq",'
    '"endpoint":"tcp://*:5557","replay_endpoint":"tcp://*:5558",'
    '"buffer_steps":10000,"hwm":100000,"max_queue_size":100000,"topic":""}',
    "--enable-auto-tool-choice",
    "--tool-call-parser=glm47",
    "--reasoning-parser=glm47",
}
# The image implements this checkpoint natively, so remote code is neither
# needed nor admitted; speculation stays off on SM120 until qualified alone.
FORBIDDEN_ARGUMENT_PREFIXES = ("--trust-remote-code", "--speculative-config")
OFFLINE_ENVIRONMENT = {
    "HF_HUB_OFFLINE": "1",
    "TRANSFORMERS_OFFLINE": "1",
    "XDG_CACHE_HOME": "/root/.cache",
    "CUDA_VISIBLE_DEVICES": "0,1,2,3",
}
# Every ramjet authority that would need a registered GLM renderer profile, an
# attested compatibility manifest, or a qualified hybrid KV inventory.
DISABLED_AUTHORITY = (
    "RJ_TOKENIZER_MODE",
    "RJ_EXACT_ROUTE_MODE",
    "RJ_KV_EVENT_MODE",
    "RJ_SNAPSHOT_ROUTE_MODE",
    "RJ_IDLE_DRAIN_MODE",
)


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render() -> dict[str, Any]:
    environment = os.environ.copy()
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
    shape = ENGINE_SHAPE[name]
    if service.get("image") != ENGINE_IMAGE:
        fail(f"{name} does not pin the immutable engine image")
    if service.get("entrypoint") != ["vllm", "serve"]:
        fail(f"{name} launcher changed")
    if service.get("restart") != "no" or service.get("ipc") != "host":
        fail(f"{name} canary restart/IPC policy changed")
    if service.get("cpuset") != shape["cpuset"]:
        fail(f"{name} NUMA placement changed")

    labels = service.get("labels", {})
    if labels.get("ai.ramjet.model.repository") != MODEL_REPOSITORY:
        fail(f"{name} model repository label changed")
    if labels.get("ai.ramjet.model.revision") != MODEL_REVISION:
        fail(f"{name} model revision label changed")

    model = mount_at(service, "/workspace/model")
    if model.get("source") != MODEL_SOURCE or model.get("read_only") is not True:
        fail(f"{name} model mount is not the immutable source")
    cache = mount_at(service, "/root/.cache")
    if cache.get("source") != shape["cache"]:
        fail(f"{name} does not own a private JIT cache")

    devices = (
        service.get("deploy", {})
        .get("resources", {})
        .get("reservations", {})
        .get("devices", [])
    )
    if len(devices) != 1 or devices[0].get("device_ids") != shape["gpus"]:
        fail(f"{name} GPU placement changed")

    ports = service.get("ports", [])
    if (
        len(ports) != 1
        or ports[0].get("host_ip") != "127.0.0.1"
        or str(ports[0].get("published")) != shape["port"]
    ):
        fail(f"{name} direct API is not loopback-only")

    environment = service.get("environment", {})
    for key, value in OFFLINE_ENVIRONMENT.items():
        if environment.get(key) != value:
            fail(f"{name} unsafe runtime default changed: {key}")

    command = service.get("command", [])
    missing = REQUIRED_ARGUMENTS - set(command)
    if missing:
        fail(f"{name} admitted argv changed: {sorted(missing)}")
    for argument in command:
        if argument.startswith(FORBIDDEN_ARGUMENT_PREFIXES):
            fail(f"{name} carries an unqualified argument: {argument}")


def validate(document: dict[str, Any]) -> None:
    services = document.get("services", {})
    if set(services) != {"ds4-loadbalancer", *ENGINE_SHAPE}:
        fail("deployment service set changed")

    for name in ENGINE_SHAPE:
        validate_engine(name, services[name])

    commands = [services[name].get("command") for name in ENGINE_SHAPE]
    if commands[0] != commands[1]:
        fail("the two engines no longer serve an identical profile")

    load_balancer = services["ds4-loadbalancer"]
    if load_balancer.get("image") != LB_IMAGE:
        fail("load balancer image is not the pinned immutable release")
    environment = load_balancer.get("environment", {})
    expected_upstream = "http://glm53nvidia-a:8000,http://glm53nvidia-b:8000"
    if environment.get("RJ_UPSTREAM") != expected_upstream:
        fail("load balancer upstream set changed")
    if not environment.get("RJ_UPSTREAM_TOKEN"):
        fail("load balancer cannot probe the authenticated engines")
    for key in DISABLED_AUTHORITY:
        if environment.get(key) != "off":
            fail(f"unqualified ramjet authority enabled: {key}")
    if environment.get("RJ_UPSTREAM_ADMISSION_MODE") != "http":
        fail("unqualified compatibility admission enabled")
    if environment.get("RJ_AFFINITY") != "prefix":
        fail("approximate prefix routing was disabled")
    if set(load_balancer.get("networks", {})) != {"default", "machineview-host"}:
        fail("load balancer network shape changed")


def main() -> int:
    try:
        validate(render())
    except (ValidationError, json.JSONDecodeError) as error:
        print(error)
        return 1
    print("GLM-5.3-Flash NVFP4 Compose validation passed: two isolated TP4 candidates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
