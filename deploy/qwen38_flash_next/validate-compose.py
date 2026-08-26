#!/usr/bin/env python3
"""Validate the single-file Qwen3.8-Flash-Next node06 deployment."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
COMPOSE = HERE / "docker-compose.yaml"
MODEL_REPOSITORY = "Qwen/Qwen3.8-Flash-Next-FP8"
MODEL_REVISION = "bcd9f01ddc9cff2316eb84281bebcd5b058bddce"
MODEL_SOURCE = "/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c"
ENGINE_IMAGE = (
    "vllm/vllm-openai@sha256:"
    "0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3"
)
LB_IMAGE = (
    "ghcr.io/helixml/ramjet:v0.4.0@sha256:"
    "467e7edf40c8fcad29e741cbba52ca571cbae0261d94cff008aa6bcdb737ea1b"
)
ROUTING_SHAPE = {
    "RJ_AFFINITY": "prefix",
    "RJ_ROUTE_ALPHA": "4",
    "RJ_ROUTE_CHUNK_BYTES": "2048",
    "RJ_ROUTE_MAX_PREFIX_BYTES": "2097152",
    "RJ_ROUTE_MAX_OVERLAP_BLOCKS": "32",
    "RJ_ROUTE_LOAD_UNIT_BYTES": "32768",
    "RJ_ROUTE_MAX_LOAD_UNITS": "8",
    "RJ_ROUTE_PHASE_AWARE_LOAD": "true",
}
ENGINE_SHAPE = {
    "qwen38flashnext-a": {
        "cpuset": "0-11,24-35",
        "gpus": ["0", "1", "2", "3"],
        "port": "8040",
    },
    "qwen38flashnext-b": {
        "cpuset": "12-23,36-47",
        "gpus": ["4", "5", "6", "7"],
        "port": "8041",
    },
}
REQUIRED_ARGUMENTS = {
    "/workspace/model",
    "--served-model-name=qwen3.8-flash-next",
    "--revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
    "--tokenizer-revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
    "--tensor-parallel-size=4",
    "--enable-expert-parallel",
    "--gpu-memory-utilization=0.90",
    "--kv-cache-memory=40190174004",
    "--max-model-len=262144",
    "--max-num-seqs=64",
    "--max-num-batched-tokens=8192",
    "--enable-prefix-caching",
    "--no-enable-flashinfer-autotune",
    "--enable-auto-tool-choice",
    "--tool-call-parser=qwen3_coder",
    "--reasoning-parser=qwen3",
}


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render() -> dict[str, Any]:
    environment = os.environ.copy()
    environment.update(
        {
            "VLLM_API_KEY": "validator-token",
            "ENGINE_RESTART_POLICY": "unless-stopped",
        }
    )
    completed = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE), "config", "--format", "json"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    if completed.returncode != 0:
        fail("docker compose could not render Qwen3.8-Flash-Next")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError("docker compose did not produce JSON") from error


def only_mount(service: dict[str, Any], target: str) -> dict[str, Any]:
    matches = [
        mount for mount in service.get("volumes", []) if mount.get("target") == target
    ]
    if len(matches) != 1:
        fail(f"expected one engine mount at {target}")
    return matches[0]


def validate_engine(name: str, service: dict[str, Any]) -> None:
    expected = ENGINE_SHAPE[name]
    if service.get("container_name") != name:
        fail(f"{name} container identity changed")
    if service.get("image") != ENGINE_IMAGE:
        fail(f"{name} image is not the pinned linux/amd64 manifest")
    if service.get("entrypoint") != ["vllm", "serve"]:
        fail(f"{name} entrypoint changed")
    if service.get("restart") != "unless-stopped":
        fail(f"{name} production restart policy changed")
    if service.get("ipc") != "host" or service.get("cpuset") != expected["cpuset"]:
        fail(f"{name} IPC or NUMA placement changed")

    labels = service.get("labels", {})
    if labels.get("ai.ramjet.model.repository") != MODEL_REPOSITORY:
        fail(f"{name} model repository label changed")
    if labels.get("ai.ramjet.model.revision") != MODEL_REVISION:
        fail(f"{name} model revision label changed")

    model_mount = only_mount(service, "/workspace/model")
    if (
        model_mount.get("type") != "bind"
        or model_mount.get("source") != MODEL_SOURCE
        or model_mount.get("read_only") is not True
    ):
        fail(f"{name} model mount is not the immutable revision directory")

    devices = (
        service.get("deploy", {})
        .get("resources", {})
        .get("reservations", {})
        .get("devices", [])
    )
    if len(devices) != 1 or devices[0].get("device_ids") != expected["gpus"]:
        fail(f"{name} GPU placement changed")

    ports = service.get("ports", [])
    if (
        len(ports) != 1
        or ports[0].get("host_ip") != "127.0.0.1"
        or str(ports[0].get("published")) != expected["port"]
    ):
        fail(f"{name} direct API is not isolated on loopback")

    arguments = service.get("command", [])
    if not REQUIRED_ARGUMENTS.issubset(set(arguments)):
        fail(f"{name} required serving arguments changed")
    if any(argument.split("=", 1)[0] == "--api-key" for argument in arguments):
        fail(f"{name} exposes bearer authority in the serving argv")
    speculative = [argument for argument in arguments if "speculative" in argument]
    if speculative != [
        '--speculative-config={"method":"mtp","num_speculative_tokens":3,'
        '"index_share_for_mtp_iteration":true}'
    ]:
        fail(f"{name} speculative decoding differs from the index-reuse candidate")
    environment = service.get("environment", {})
    if environment.get("VLLM_API_KEY") != "validator-token":
        fail(f"{name} bearer authority differs from the load balancer")
    if environment.get("VLLM_PLE_CPU_OFFLOAD") != "0":
        fail(f"{name} enables unqualified host-memory PLE offload")


def validate(document: dict[str, Any]) -> None:
    services = document.get("services", {})
    expected_services = {"ds4-loadbalancer", *ENGINE_SHAPE}
    if set(services) != expected_services:
        fail("deployment service set changed")

    for name in ENGINE_SHAPE:
        validate_engine(name, services[name])
    left_command = services["qwen38flashnext-a"].get("command")
    right_command = services["qwen38flashnext-b"].get("command")
    if left_command != right_command:
        fail("the two engine commands differ")

    load_balancer = services["ds4-loadbalancer"]
    if load_balancer.get("image") != LB_IMAGE:
        fail("load-balancer rollback pin changed")
    environment = load_balancer.get("environment", {})
    if environment.get("RJ_UPSTREAM") != (
        "http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000"
    ):
        fail("load balancer does not target exactly the two TP4 engines")
    if environment.get("RJ_UPSTREAM_TOKEN") != "validator-token":
        fail("engine and load-balancer bearer authority differ")
    for key in ("RJ_TOKENIZER_MODE", "RJ_EXACT_ROUTE_MODE", "RJ_KV_EVENT_MODE"):
        if environment.get(key) != "off":
            fail(f"unqualified routing authority is enabled: {key}")
    if environment.get("RJ_UPSTREAM_ADMISSION_MODE") != "http":
        fail("unqualified compatibility admission is enabled")
    if environment.get("RJ_TOKENIZER_PROFILE") != "qwen3.8-flash-next":
        fail("load balancer does not select the Flash-Next renderer profile")
    if environment.get("RJ_MAX_TOKENS_STRIP") != "0":
        fail("load balancer can silently strip a valid Flash-Next output budget")
    for key, value in ROUTING_SHAPE.items():
        if environment.get(key) != value:
            fail(f"load balancer routing shape changed: {key}")

    if set(load_balancer.get("networks", {})) != {"default", "machineview-host"}:
        fail("load balancer lost its serving or host-telemetry network")
    networks = document.get("networks", {})
    machineview = networks.get("machineview-host", {})
    if (
        machineview.get("external") is not True
        or machineview.get("name") != "qwen38_27b_default"
    ):
        fail("machine-view host bridge is not the admitted node06 network")


def main() -> int:
    try:
        validate(render())
    except ValidationError as error:
        print(str(error))
        return 1
    print("Qwen3.8-Flash-Next Compose validation passed: two isolated TP4 engines")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
