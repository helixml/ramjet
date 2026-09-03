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
ADAPTIVE_CONFIG = HERE / "adaptive-config.json"
MODEL_REPOSITORY = "Qwen/Qwen3.8-Flash-Next-FP8"
MODEL_REVISION = "bcd9f01ddc9cff2316eb84281bebcd5b058bddce"
MODEL_SOURCE = "/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c"
ENGINE_IMAGE = (
    "vllm/vllm-openai@sha256:"
    "0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3"
)
LB_IMAGE = (
    "ghcr.io/helixml/ramjet:v0.5.0@sha256:"
    "c3fc5723a0dba51f9bb8eced77648cf0b05788039e90fc638fbd8c19adec70d8"
)
EXACT_CANARY_KEY = "validator-exact-route-key-000000"
ROUTING_SHAPE = {
    "RJ_AFFINITY": "prefix",
    "RJ_ROUTE_ALPHA": "4",
    "RJ_ROUTE_CHUNK_BYTES": "2048",
    "RJ_ROUTE_MAX_PREFIX_BYTES": "2097152",
    "RJ_ROUTE_MAX_OVERLAP_BLOCKS": "32",
    "RJ_ROUTE_LOAD_UNIT_BYTES": "32768",
    "RJ_ROUTE_MAX_LOAD_UNITS": "8",
    "RJ_ROUTE_PHASE_AWARE_LOAD": "true",
    "RJ_ROUTE_DECODE_LOAD_UNIT_TOKENS": "256",
    "RJ_ROUTE_DECODE_MAX_LOAD_UNITS": "4",
    "RJ_ROUTE_PROJECTED_LOAD": "false",
    "RJ_ROUTE_SPECULATION_MODE": "prefer",
    "RJ_ROUTE_SPECULATION_PROFILES": "mtp,standard,mtp",
    "RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MODE": "prefer",
    "RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MIN_BLOCKS": "8",
    "RJ_ROUTE_PREFIX_SINGLE_FLIGHT_CAPACITY": "1024",
    "RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MAX_LOAD_DELTA": "1",
    "RJ_UPSTREAM_WARMUP_MODE": "enforce",
    "RJ_UPSTREAM_WARMUP_CONSECUTIVE_SUCCESSES": "3",
    "RJ_UPSTREAM_WARMUP_STABLE_SECONDS": "30",
}
ENGINE_SHAPE = {
    "qwen38flashnext-a": {
        "cpuset": "0-11,24-35",
        "gpus": ["0", "1", "2", "3"],
        "port": "8040",
        "profile": "mtp",
        "adaptive_profile": "split-tp4",
        "upstream": "0",
        "tp": "4",
    },
    "qwen38flashnext-b": {
        "cpuset": "12-23,36-47",
        "gpus": ["4", "5", "6", "7"],
        "port": "8041",
        "profile": "standard",
        "adaptive_profile": "split-tp4",
        "upstream": "1",
        "tp": "4",
    },
    "qwen38flashnext-tp8": {
        "cpuset": "0-47",
        "gpus": [str(index) for index in range(8)],
        "port": "8042",
        "profile": "mtp",
        "adaptive_profile": "unified-tp8",
        "upstream": "2",
        "tp": "8",
    },
}
REQUIRED_ARGUMENTS = {
    "/workspace/model",
    "--served-model-name=qwen3.8-flash-next",
    "--revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
    "--tokenizer-revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
    "--enable-expert-parallel",
    "--gpu-memory-utilization=0.90",
    "--kv-cache-memory=40190174004",
    "--max-model-len=262144",
    "--max-num-seqs=64",
    "--max-num-batched-tokens=8192",
    '--kv-events-config={"enable_kv_cache_events":true,"publisher":"zmq",'
    '"endpoint":"tcp://*:5557","replay_endpoint":"tcp://*:5558",'
    '"buffer_steps":10000,"hwm":100000,"max_queue_size":100000,"topic":""}',
    "--enable-prefix-caching",
    "--enable-prompt-tokens-details",
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
            "RJ_UI_AUTH_TOKEN": "validator-ui-auth-token-that-is-long-enough",
            "ENGINE_RESTART_POLICY": "unless-stopped",
            "RJ_EXACT_ROUTE_CANARY_KEY": EXACT_CANARY_KEY,
        }
    )
    completed = subprocess.run(
        [
            "docker", "compose", "-f", str(COMPOSE),
            "--profile", "adaptive", "config", "--format", "json",
        ],
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
    if labels.get("com.helixml.ramjet.adaptive-profile") != expected["adaptive_profile"]:
        fail(f"{name} adaptive profile label changed")
    if labels.get("com.helixml.ramjet.adaptive-upstream") != expected["upstream"]:
        fail(f"{name} adaptive upstream label changed")

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
    if f'--tensor-parallel-size={expected["tp"]}' not in arguments:
        fail(f"{name} tensor-parallel shape changed")
    if any(argument.split("=", 1)[0] == "--api-key" for argument in arguments):
        fail(f"{name} exposes bearer authority in the serving argv")
    speculative = [argument for argument in arguments if "speculative" in argument]
    if expected["profile"] == "mtp":
        wanted_speculative = [
            '--speculative-config={"method":"mtp","num_speculative_tokens":3,'
            '"index_share_for_mtp_iteration":true}'
        ]
    else:
        wanted_speculative = []
    if speculative != wanted_speculative:
        fail(f"{name} speculative decoding differs from its admitted profile")
    environment = service.get("environment", {})
    if environment.get("VLLM_API_KEY") != "validator-token":
        fail(f"{name} bearer authority differs from the load balancer")
    if environment.get("VLLM_PLE_CPU_OFFLOAD") != "0":
        fail(f"{name} enables unqualified host-memory PLE offload")
    # NVIDIA remaps the requested physical devices to container-local ordinal
    # 0..N-1; the process-level visibility list names those local ordinals.
    expected_cuda = ",".join(str(index) for index in range(len(expected["gpus"])))
    if environment.get("CUDA_VISIBLE_DEVICES") != expected_cuda:
        fail(f"{name} CUDA visibility differs from its GPU assignment")
    if name == "qwen38flashnext-tp8" and service.get("profiles") != ["adaptive"]:
        fail("TP8 candidate is not isolated behind the adaptive Compose profile")


def validate(document: dict[str, Any]) -> None:
    services = document.get("services", {})
    expected_services = {"ds4-loadbalancer", *ENGINE_SHAPE}
    if set(services) != expected_services:
        fail("deployment service set changed")

    for name in ENGINE_SHAPE:
        validate_engine(name, services[name])
    left_command = services["qwen38flashnext-a"].get("command", [])
    right_command = services["qwen38flashnext-b"].get("command", [])
    without_speculation = lambda command: [
        argument for argument in command if "speculative" not in argument
    ]
    if without_speculation(left_command) != without_speculation(right_command):
        fail("the two engine commands differ beyond their admitted profiles")

    load_balancer = services["ds4-loadbalancer"]
    if load_balancer.get("image") != LB_IMAGE:
        fail("load-balancer rollback pin changed")
    environment = load_balancer.get("environment", {})
    if environment.get("RJ_UPSTREAM") != (
        "http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000,"
        "http://qwen38flashnext-tp8:8000"
    ):
        fail("load balancer does not target the two TP4 engines plus fenced TP8 candidate")
    if environment.get("RJ_UPSTREAM_TOKEN") != "validator-token":
        fail("engine and load-balancer bearer authority differ")
    if environment.get("RJ_UI_AUTH_TOKEN") != "validator-ui-auth-token-that-is-long-enough":
        fail("dedicated UI authentication authority changed")
    if environment.get("RJ_UI_AUTH_TOKEN") == environment.get("RJ_UPSTREAM_TOKEN"):
        fail("UI and engine bearer authority must remain separate")
    exact_shape = {
        "RJ_TOKENIZER_MODE": "local-shadow",
        "RJ_TOKENIZER_PATH": "/models/qwen38-flash-next/tokenizer.json",
        "RJ_TOKENIZER_SHA256": "0997f410c57a1f4e53b09e4be8f4a172d90edd9564368fb0847030937229b9f3",
        "RJ_CHAT_TEMPLATE_PATH": "/models/qwen38-flash-next/tokenizer_config.json",
        "RJ_CHAT_TEMPLATE_SHA256": "b11349aafa7cdc6a320767cf7ceb29ed82f7eda5d65e8e0819e76f0ce947bf27",
        "RJ_EXACT_ROUTE_MODE": "placement",
        "RJ_EXACT_ROUTE_MANIFEST_PATH": "/compat/qwen38-flash-next-r134.json",
        "RJ_EXACT_ROUTE_MANIFEST_SHA256": "a5efb2db66475b8a7c4f01bbb5d47b62387f251354bdebd2641b1f2d00a64a67",
        "RJ_EXACT_ROUTE_MIN_GAIN_TOKENS": "8192",
        "RJ_EXACT_ROUTE_MAX_LOAD_DELTA": "0",
        "RJ_EXACT_ROUTE_CANARY_BPS": "10000",
        "RJ_EXACT_ROUTE_CANARY_KEY": EXACT_CANARY_KEY,
        "RJ_KV_EVENT_MODE": "shadow",
        "RJ_KV_EVENT_LIVE_ENDPOINTS": "tcp://qwen38flashnext-a:5557,tcp://qwen38flashnext-b:5557,tcp://qwen38flashnext-tp8:5557",
        "RJ_KV_EVENT_REPLAY_ENDPOINTS": "tcp://qwen38flashnext-a:5558,tcp://qwen38flashnext-b:5558,tcp://qwen38flashnext-tp8:5558",
        "RJ_KV_EVENT_REPLAY_LIMIT": "8192",
        "RJ_KV_EVENT_REPLAY_TAIL_LIMIT": "64",
        "RJ_KV_EVENT_TIMEOUT_MS": "180000",
        "RJ_ADAPTIVE_CONFIG_PATH": "/etc/ramjet/adaptive-config.json",
    }
    for key, value in exact_shape.items():
        if environment.get(key) != value:
            fail(f"exact routing authority changed: {key}")
    if environment.get("RJ_UPSTREAM_ADMISSION_MODE") != "http":
        fail("unqualified compatibility admission is enabled")
    if environment.get("RJ_TOKENIZER_PROFILE") != "qwen3.8-flash-next":
        fail("load balancer does not select the Flash-Next renderer profile")
    if environment.get("RJ_MAX_TOKENS_STRIP") != "0":
        fail("load balancer can silently strip a valid Flash-Next output budget")
    mounts = {
        mount.get("target"): mount
        for mount in load_balancer.get("volumes", [])
    }
    for target in (
        "/models/qwen38-flash-next/tokenizer.json",
        "/models/qwen38-flash-next/tokenizer_config.json",
    ):
        mount = mounts.get(target, {})
        if mount.get("read_only") is not True:
            fail(f"exact routing artifact is not mounted read-only: {target}")
    adaptive_mounts = {
        "/etc/ramjet/adaptive-config.json": (str(ADAPTIVE_CONFIG), True),
        "/var/run/docker.sock": ("/var/run/docker.sock", False),
        "/run/lock/ramjet-node06-deployment.lock": (
            "/run/lock/ramjet-node06-deployment.lock", False
        ),
        "/var/lib/ramjet-adaptive": ("/var/lib/ramjet-adaptive", False),
    }
    for target, (source, read_only) in adaptive_mounts.items():
        mount = mounts.get(target, {})
        if mount.get("source") != source or bool(mount.get("read_only")) != read_only:
            fail(f"adaptive authority mount changed: {target}")
    for key, value in ROUTING_SHAPE.items():
        if environment.get(key) != value:
            fail(f"load balancer routing shape changed: {key}")
    adaptive = json.loads(ADAPTIVE_CONFIG.read_text(encoding="utf-8"))
    if adaptive.get("audit_path") != "/var/lib/ramjet-adaptive/audit.jsonl":
        fail("adaptive audit path changed")
    if adaptive.get("mode") != "manual" or adaptive.get("active_profile") != "split-tp4":
        fail("adaptive rollout must start manual on the qualified TP4 profile")
    profile_engines = {
        profile["id"]: profile["engines"] for profile in adaptive.get("profiles", [])
    }
    if set(profile_engines) != {"split-tp4", "unified-tp8"}:
        fail("adaptive profile set changed")
    configured = {
        engine["container"]: (str(engine["upstream"]), [str(gpu) for gpu in engine["gpus"]])
        for engines in profile_engines.values()
        for engine in engines
    }
    expected_configured = {
        name: (shape["upstream"], shape["gpus"]) for name, shape in ENGINE_SHAPE.items()
    }
    if configured != expected_configured:
        fail("adaptive config no longer matches Compose engine authority")

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
    print(
        "Qwen3.8-Flash-Next Compose validation passed: adaptive TP4-pair/TP8 "
        "profiles with pinned controller and live/replay KV-event authority"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
