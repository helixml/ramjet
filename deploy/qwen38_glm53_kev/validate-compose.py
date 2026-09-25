#!/usr/bin/env python3
"""Validate the node06 Qwen/GLM/Kev TypeSafe deployment."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
COMPOSE = ROOT / "docker-compose.yaml"
RAMJET_IMAGE = "ghcr.io/helixml/ramjet:systemone-test@sha256:" + "a" * 64
KEV_IMAGE = "ghcr.io/helixml/ramjet-kev:0.8b-test@sha256:" + "b" * 64
IMMUTABLE_IMAGE = re.compile(r"^[a-z0-9./_-]+:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}$")


def fail(message: str) -> None:
    raise ValueError(message)


def render() -> dict:
    environment = os.environ.copy()
    environment.update(
        {
            "LB_IMAGE": environment.get("LB_IMAGE", RAMJET_IMAGE),
            "KEV_IMAGE": environment.get("KEV_IMAGE", KEV_IMAGE),
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


def validate_long_prompt_lane(environment: dict) -> None:
    """Mirror Ramjet's boot-time lane rules so a bad render fails here first."""
    raw_bytes = environment.get("RJ_ROUTE_LONG_PROMPT_BYTES")
    if raw_bytes is None or not re.fullmatch(r"[0-9]+", raw_bytes):
        fail("RJ_ROUTE_LONG_PROMPT_BYTES must be a non-negative integer (0 disables)")
    upstreams = environment.get("RJ_UPSTREAM", "").split(",")
    profiles = environment.get("RJ_UPSTREAM_APIS", "").split(",")
    lanes = [value.strip() for value in environment.get("RJ_ROUTE_LONG_PROMPT_UPSTREAMS", "").split(",")]
    if len(lanes) != len(upstreams) or any(value not in {"lane", "-"} for value in lanes):
        fail("RJ_ROUTE_LONG_PROMPT_UPSTREAMS needs exactly one lane or - per upstream")
    if "lane" not in lanes:
        fail("RJ_ROUTE_LONG_PROMPT_UPSTREAMS must name at least one lane member")
    if any(lane == "lane" and profile != "openai" for lane, profile in zip(lanes, profiles)):
        fail("long-prompt lane members must be OpenAI-profile upstreams")


def validate(document: dict) -> None:
    if document.get("name") != "qwen38_glm53_kev":
        fail("unexpected Compose project name")
    services = document.get("services", {})
    if set(services) != {"ds4-loadbalancer", "kev-small"}:
        fail("deployment must own exactly Ramjet and Kev-small")

    kev = services["kev-small"]
    if not IMMUTABLE_IMAGE.fullmatch(kev.get("image", "")):
        fail("Kev image must be an immutable tag@sha256 reference")
    if set(kev.get("networks", {})) != {"kev-engine"}:
        fail("Kev must join only the private System One network")
    if kev.get("read_only") is not True or kev.get("user") != "1000:1000":
        fail("Kev must use the read-only non-root runtime")
    if kev.get("command") != [
        "--run",
        "jaredpalmer/kev-0.8b@54f4f8777356cd5bbbb6c6919c657f26e6f2f6d8",
        "--port",
        "8009",
    ]:
        fail("Kev model, revision, or port drift")
    devices = kev["deploy"]["resources"]["reservations"]["devices"]
    if len(devices) != 1 or devices[0].get("device_ids") != ["3"]:
        fail("Kev-small must be confined to physical GPU 3")
    if set(kev.get("environment", {})) != {
        "HF_HOME",
        "HF_HUB_DISABLE_PROGRESS_BARS",
        "HF_HUB_OFFLINE",
        "KEV_DTYPE",
        "KEV_HOST",
        "KEV_PREFIX_CACHE",
        "KEV_PREFIX_MIN_TOKENS",
        "TOKENIZERS_PARALLELISM",
        "TRANSFORMERS_OFFLINE",
        "TRITON_CACHE_DIR",
    }:
        fail("unexpected Kev runtime environment")

    lb = services["ds4-loadbalancer"]
    if not IMMUTABLE_IMAGE.fullmatch(lb.get("image", "")):
        fail("Ramjet image must be an immutable tag@sha256 reference")
    environment = lb.get("environment", {})
    expected = {
        "RJ_UPSTREAM": "http://qwen38flashnext-a:8000,http://glm53sm120-b:8000,http://glm53sm120-c:8000,http://kev-small:8009",
        "RJ_UPSTREAM_MODELS": "qwen3.8-flash-next,glm-5.3-flash,glm-5.3-flash,kev-latest",
        "RJ_UPSTREAM_APIS": "openai,openai,openai,systemone",
    }
    for key, value in expected.items():
        if environment.get(key) != value:
            fail(f"{key} must be {value!r}")
    validate_long_prompt_lane(environment)
    if "RJ_MACHINEVIEW_UPSTREAM_GPUS" in environment:
        fail("exclusive machine-view GPU ownership cannot describe co-located Kev")
    for key in (
        "RJ_TOKENIZER_MODE",
        "RJ_EXACT_ROUTE_MODE",
        "RJ_KV_EVENT_MODE",
        "RJ_SNAPSHOT_ROUTE_MODE",
        "RJ_IDLE_DRAIN_MODE",
    ):
        if environment.get(key) != "off":
            fail(f"{key} must remain off for the heterogeneous deployment")
    if environment.get("RJ_UPSTREAM_TOKEN") != "validator-upstream-token":
        fail("protected upstream token did not render")
    if environment.get("RJ_UI_AUTH_TOKEN") != "validator-ui-token":
        fail("protected UI token did not render")
    if set(lb.get("networks", {})) != {
        "qwen-engine",
        "glm-engine",
        "machineview-host",
        "kev-engine",
    }:
        fail("Ramjet external network set drift")
    kev_network = document.get("networks", {}).get("kev-engine", {})
    if (
        kev_network.get("external") is not True
        or kev_network.get("name") != "ramjet_kev_systemone"
    ):
        fail("Kev network must be the fixed external private network")
    if any(
        volume.get("source") == "/var/run/docker.sock"
        for service in services.values()
        for volume in service.get("volumes", [])
    ):
        fail("serving containers must not hold the Docker socket")


def main() -> int:
    try:
        validate(render())
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError, ValueError) as error:
        print(f"qwen/glm/kev compose validation failed: {error}", file=sys.stderr)
        return 1
    print("qwen/glm/kev compose validation: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
