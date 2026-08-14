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
RUNTIME_MANIFEST = ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json"
ENGINES = ("dspark-0731", "dspark-0731-b")
MIDDLEWARE_TARGET = (
    "/opt/venv/lib/python3.12/site-packages/mini_dynamo_engine_identity.py"
)
MANIFEST_TARGET = "/opt/mini-dynamo/compatibility.json"
ENGINE_RUNTIME_MANIFEST_TARGET = "/opt/mini-dynamo/serving-runtime.json"
LB_RUNTIME_MANIFEST_TARGET = "/compat/serving-runtime.json"
LB_RENDERER_MANIFEST_TARGET = "/compat/deepseek-v4-r34.json"
MIDDLEWARE_IMPORT = "mini_dynamo_engine_identity.ServingIdentityMiddleware"
ENGINE_IMAGE = (
    "voipmonitor/vllm@sha256:"
    "820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b"
)
KV_EVENTS_KEYS = {
    "enable_kv_cache_events",
    "publisher",
    "endpoint",
    "replay_endpoint",
    "buffer_steps",
    "hwm",
    "max_queue_size",
    "topic",
}
GENERATION_CONFIG = {"top_p": 0.95}
MANIFEST_SHA256 = hashlib.sha256(MANIFEST.read_bytes()).hexdigest()
RUNTIME_MANIFEST_SHA256 = hashlib.sha256(RUNTIME_MANIFEST.read_bytes()).hexdigest()
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


def wildcard_tcp_port(endpoint: Any) -> int:
    prefix = "tcp://*:"
    if not isinstance(endpoint, str) or not endpoint.startswith(prefix):
        fail("serving runtime KV endpoint is not wildcard TCP")
    raw = endpoint.removeprefix(prefix)
    if not raw.isascii() or not raw.isdigit():
        fail("serving runtime KV endpoint port is invalid")
    port = int(raw)
    if not 0 < port <= 65535:
        fail("serving runtime KV endpoint port is invalid")
    return port


def validate_runtime_manifest(document: Any | None = None) -> dict[str, Any]:
    if document is None:
        try:
            document = json.loads(RUNTIME_MANIFEST.read_bytes())
        except (OSError, json.JSONDecodeError) as exc:
            raise ValidationError("serving runtime manifest is unavailable") from exc
    if not isinstance(document, dict) or set(document) != {
        "schema_version",
        "compatibility_manifest_sha256",
        "engine",
    }:
        fail("serving runtime manifest schema changed")
    if document.get("schema_version") != 1:
        fail("serving runtime manifest schema is unsupported")
    if document.get("compatibility_manifest_sha256") != MANIFEST_SHA256:
        fail("serving runtime compatibility link changed")
    engine = document.get("engine")
    if not isinstance(engine, dict) or set(engine) != {
        "core_process_count",
        "kv_events",
    }:
        fail("serving runtime engine schema changed")
    core_process_count = engine.get("core_process_count")
    if type(core_process_count) is not int or not 0 < core_process_count <= 64:
        fail("serving runtime core process count is invalid")
    kv_events = engine.get("kv_events")
    if not isinstance(kv_events, dict) or set(kv_events) != KV_EVENTS_KEYS:
        fail("serving runtime KV publisher schema changed")
    if (
        kv_events.get("enable_kv_cache_events") is not True
        or kv_events.get("publisher") != "zmq"
    ):
        fail("serving runtime KV publisher is disabled or unsupported")
    ports = {
        wildcard_tcp_port(kv_events.get("endpoint")),
        wildcard_tcp_port(kv_events.get("replay_endpoint")),
    }
    if len(ports) != 2:
        fail("serving runtime KV endpoints are not distinct")
    for name in ("buffer_steps", "hwm", "max_queue_size"):
        value = kv_events.get(name)
        if type(value) is not int or not 0 < value <= 1_000_000_000:
            fail("serving runtime KV publisher capacity is invalid")
    topic = kv_events.get("topic")
    if not isinstance(topic, str) or len(topic.encode()) > 4096:
        fail("serving runtime KV topic is invalid")
    return document


def validate_disabled(document: dict[str, Any]) -> None:
    load_balancer = document["services"]["ds4-loadbalancer"]
    lb_environment = load_balancer.get("environment", {})
    if any(key.startswith("DS4_SERVING_RUNTIME_") for key in lb_environment):
        fail("serving runtime is active in the base load balancer")
    if any(
        volume.get("target") == LB_RUNTIME_MANIFEST_TARGET
        for volume in load_balancer.get("volumes", [])
    ):
        fail("serving runtime mount is active in the base load balancer")
    if (
        lb_environment.get("DS4_EXACT_ROUTE_MANIFEST_PATH")
        != LB_RENDERER_MANIFEST_TARGET
        or lb_environment.get("DS4_EXACT_ROUTE_MANIFEST_SHA256")
        != MANIFEST_SHA256
    ):
        fail("base renderer manifest pin changed")
    for name in ENGINES:
        service = document["services"][name]
        environment = service.get("environment", {})
        if any(
            key.startswith("MINI_DYNAMO_SERVING_IDENTITY_")
            or key.startswith("MINI_DYNAMO_SERVING_RUNTIME_")
            for key in environment
        ):
            fail("serving identity is active in the base Compose")
        if MIDDLEWARE_IMPORT in environment.get("EXTRA_VLLM_ARGS", ""):
            fail("serving identity middleware is active in the base Compose")
        if any(
            volume.get("target")
            in {MIDDLEWARE_TARGET, MANIFEST_TARGET, ENGINE_RUNTIME_MANIFEST_TARGET}
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


def validate_enabled(
    document: dict[str, Any], runtime_document: Any | None = None
) -> None:
    runtime = validate_runtime_manifest(runtime_document)
    expected_kv_events = runtime["engine"]["kv_events"]
    load_balancer = document["services"]["ds4-loadbalancer"]
    lb_environment = load_balancer.get("environment", {})
    if lb_environment.get("DS4_UPSTREAM_ADMISSION_MODE") != "http":
        fail("identity overlay may not opt the load balancer into admission")
    if (
        lb_environment.get("DS4_EXACT_ROUTE_MANIFEST_PATH")
        != LB_RENDERER_MANIFEST_TARGET
        or lb_environment.get("DS4_EXACT_ROUTE_MANIFEST_SHA256")
        != MANIFEST_SHA256
    ):
        fail("identity overlay changed the renderer manifest pin")
    if (
        lb_environment.get("DS4_SERVING_RUNTIME_MANIFEST_PATH")
        != LB_RUNTIME_MANIFEST_TARGET
    ):
        fail("load balancer serving runtime target changed")
    if (
        lb_environment.get("DS4_SERVING_RUNTIME_MANIFEST_SHA256")
        != RUNTIME_MANIFEST_SHA256
    ):
        fail("load balancer serving runtime pin changed")
    if any(key.startswith("MINI_DYNAMO_SERVING_RUNTIME_") for key in lb_environment):
        fail("load balancer uses engine serving runtime authority")
    validate_mount(
        load_balancer,
        LB_RUNTIME_MANIFEST_TARGET,
        RUNTIME_MANIFEST_SHA256,
    )
    for name in ENGINES:
        service = document["services"][name]
        if service.get("image") != ENGINE_IMAGE:
            fail(f"{name} is not pinned to the manifest engine image")
        environment = service.get("environment", {})
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH") != MANIFEST_TARGET:
            fail(f"{name} manifest target changed")
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SHA256") != MANIFEST_SHA256:
            fail(f"{name} manifest pin changed")
        if (
            environment.get("MINI_DYNAMO_SERVING_RUNTIME_MANIFEST_PATH")
            != ENGINE_RUNTIME_MANIFEST_TARGET
        ):
            fail(f"{name} serving runtime target changed")
        if (
            environment.get("MINI_DYNAMO_SERVING_RUNTIME_MANIFEST_SHA256")
            != RUNTIME_MANIFEST_SHA256
        ):
            fail(f"{name} serving runtime pin changed")
        if any(key.startswith("DS4_SERVING_RUNTIME_") for key in environment):
            fail(f"{name} uses load balancer serving runtime authority")
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_TOKENIZER_PATH") != "/workspace/model/tokenizer.json":
            fail(f"{name} tokenizer verification path changed")
        if environment.get("MINI_DYNAMO_SERVING_IDENTITY_VERIFY_TIMEOUT_MS") != "4000":
            fail(f"{name} live verification timeout changed")
        if not environment.get("VLLM_API_KEY"):
            fail(f"{name} has no endpoint bearer authority")
        arguments = environment.get("EXTRA_VLLM_ARGS", "")
        if option_value(arguments, "--middleware", f"{name} middleware") != MIDDLEWARE_IMPORT:
            fail(f"{name} middleware import changed")
        if (
            json_option(arguments, "--kv-events-config", f"{name} KV publisher")
            != expected_kv_events
        ):
            fail(f"{name} KV publisher diverges from serving runtime manifest")
        if json_option(
            arguments,
            "--override-generation-config",
            f"{name} sampling floor",
        ) != GENERATION_CONFIG:
            fail(f"{name} changed the qualified sampling floor")
        validate_mount(service, MIDDLEWARE_TARGET, MIDDLEWARE_SHA256)
        validate_mount(service, MANIFEST_TARGET, MANIFEST_SHA256)
        validate_mount(
            service,
            ENGINE_RUNTIME_MANIFEST_TARGET,
            RUNTIME_MANIFEST_SHA256,
        )


def validate_source_bind_policy(path: pathlib.Path = OVERLAY) -> None:
    text = path.read_text(encoding="utf-8")
    if text.count("- type: bind") != 7 or text.count("create_host_path: false") != 7:
        fail("serving identity source binds are not fail-closed")


def main() -> int:
    validate_source_bind_policy()
    validate_disabled(render(enabled=False))
    validate_enabled(render(enabled=True))
    print("serving identity compose validation passed: explicit in-process overlay")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
