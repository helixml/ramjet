#!/usr/bin/env python3
"""Render and validate the opt-in in-process serving-identity overlay."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import re
import subprocess
from typing import Any


HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
BASE = HERE / "docker-compose.yaml"
OVERLAY = HERE / "docker-compose.compatibility-identity.yaml"
MIDDLEWARE = HERE / "engine_identity_middleware.py"
MANIFEST = ROOT / "compat" / "deepseek-v4-r34.json"
ENGINES = ("dspark-0731", "dspark-0731-b")
MIDDLEWARE_TARGET = (
    "/opt/venv/lib/python3.12/site-packages/mini_dynamo_engine_identity.py"
)
MANIFEST_TARGET = "/opt/mini-dynamo/compatibility.json"
MIDDLEWARE_IMPORT = "mini_dynamo_engine_identity.ServingIdentityMiddleware"
ENGINE_IMAGE = (
    "voipmonitor/vllm@sha256:"
    "820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b"
)
KV_EVENTS_CONFIG = {
    "enable_kv_cache_events": True,
    "publisher": "zmq",
    "endpoint": "tcp://*:5557",
    "replay_endpoint": "tcp://*:5558",
    "buffer_steps": 10000,
    "hwm": 100000,
    "max_queue_size": 100000,
    "topic": "",
}
GENERATION_CONFIG = {"top_p": 0.95}
MANIFEST_SHA256 = hashlib.sha256(MANIFEST.read_bytes()).hexdigest()
MIDDLEWARE_SHA256 = hashlib.sha256(MIDDLEWARE.read_bytes()).hexdigest()


class ValidationError(ValueError):
    pass


def fail(message: str) -> None:
    raise ValidationError(message)


def render(*, enabled: bool) -> dict[str, Any]:
    command = ["docker", "compose", "-f", str(BASE)]
    if enabled:
        command.extend(["-f", str(OVERLAY)])
    command.extend(["config", "--format", "json"])
    environment = os.environ.copy()
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    if completed.returncode != 0:
        fail("docker compose could not render serving identity")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise ValidationError("docker compose did not produce JSON") from exc


def volume_by_target(service: dict[str, Any], target: str) -> dict[str, Any]:
    matches = [
        volume
        for volume in service.get("volumes", [])
        if volume.get("target") == target
    ]
    if len(matches) != 1:
        fail(f"expected exactly one serving-identity mount at {target}")
    return matches[0]


def validate_mount(
    service: dict[str, Any],
    target: str,
    expected_sha256: str,
) -> None:
    volume = volume_by_target(service, target)
    if volume.get("type") != "bind" or volume.get("read_only") is not True:
        fail(f"{target} is not a read-only bind")
    if volume.get("bind", {}).get("create_host_path") is not False:
        fail(f"{target} may create a missing host path")
    source = pathlib.Path(volume.get("source", ""))
    try:
        actual_sha256 = hashlib.sha256(source.read_bytes()).hexdigest()
    except OSError as exc:
        raise ValidationError(f"{target} source is unavailable") from exc
    if actual_sha256 != expected_sha256:
        fail(f"{target} source bytes changed")


def validate_disabled(document: dict[str, Any]) -> None:
    for name in ENGINES:
        service = document["services"][name]
        environment = service.get("environment", {})
        if any(key.startswith("MINI_DYNAMO_SERVING_IDENTITY_") for key in environment):
            fail("serving identity is active in the base Compose")
        if MIDDLEWARE_IMPORT in environment.get("EXTRA_VLLM_ARGS", ""):
            fail("serving identity middleware is active in the base Compose")
        if any(
            volume.get("target") in {MIDDLEWARE_TARGET, MANIFEST_TARGET}
            for volume in service.get("volumes", [])
        ):
            fail("serving identity mount is active in the base Compose")


def option_value(arguments: str, option: str, description: str) -> str:
    # The image's entrypoint consumes EXTRA_VLLM_ARGS as shell text. shlex
    # would remove the JSON member quotes, so retain each compact JSON token
    # byte-for-byte. Whitespace inside these policy objects is intentionally
    # rejected; Compose emits the canonical compact form.
    pattern = re.compile(
        rf"(?<!\S){re.escape(option)}(?:=|[ \t]+)(\{{[^ \t\r\n]*\}}|[^ \t\r\n]+)"
    )
    values = pattern.findall(arguments)
    if len(values) != 1:
        fail(f"{description} argument cardinality changed")
    return values[0]


def json_option(arguments: str, option: str, description: str) -> Any:
    try:
        return json.loads(option_value(arguments, option, description))
    except json.JSONDecodeError as exc:
        raise ValidationError(f"{description} is not valid JSON") from exc


def validate_enabled(document: dict[str, Any]) -> None:
    load_balancer = document["services"]["ds4-loadbalancer"]
    if load_balancer.get("environment", {}).get("DS4_UPSTREAM_ADMISSION_MODE") != "http":
        fail("identity overlay may not opt the load balancer into admission")
    for name in ENGINES:
        service = document["services"][name]
        if service.get("image") != ENGINE_IMAGE:
            fail(f"{name} is not pinned to the manifest engine image")
        environment = service.get("environment", {})
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH") != MANIFEST_TARGET:
            fail(f"{name} manifest target changed")
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SHA256") != MANIFEST_SHA256:
            fail(f"{name} manifest pin changed")
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_TOKENIZER_PATH") != "/workspace/model/tokenizer.json":
            fail(f"{name} tokenizer verification path changed")
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_VERIFY_TIMEOUT_MS") != "4000":
            fail(f"{name} live verification timeout changed")
        if not environment.get("VLLM_API_KEY"):
            fail(f"{name} has no endpoint bearer authority")
        arguments = environment.get("EXTRA_VLLM_ARGS", "")
        if option_value(arguments, "--middleware", f"{name} middleware") != MIDDLEWARE_IMPORT:
            fail(f"{name} middleware import changed")
        if json_option(arguments, "--kv-events-config", f"{name} KV publisher") != KV_EVENTS_CONFIG:
            fail(f"{name} changed the qualified KV publisher")
        if json_option(
            arguments,
            "--override-generation-config",
            f"{name} sampling floor",
        ) != GENERATION_CONFIG:
            fail(f"{name} changed the qualified sampling floor")
        validate_mount(service, MIDDLEWARE_TARGET, MIDDLEWARE_SHA256)
        validate_mount(service, MANIFEST_TARGET, MANIFEST_SHA256)


def validate_source_bind_policy(path: pathlib.Path = OVERLAY) -> None:
    text = path.read_text(encoding="utf-8")
    if text.count("- type: bind") != 4 or text.count("create_host_path: false") != 4:
        fail("serving identity source binds are not fail-closed")


def main() -> int:
    validate_source_bind_policy()
    validate_disabled(render(enabled=False))
    validate_enabled(render(enabled=True))
    print("serving identity compose validation passed: explicit in-process overlay")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
