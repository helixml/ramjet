"""Authenticated, fail-closed vLLM serving-identity ASGI middleware.

This module intentionally depends only on the Python standard library. vLLM
loads :class:`ServingIdentityMiddleware` through ``--middleware``. Ordinary
requests take one exact path comparison and are passed to the original ASGI
application; the control endpoint never adds a network hop to inference.
"""

from __future__ import annotations

import asyncio
import hashlib
import hmac
from importlib import metadata as importlib_metadata
import json
from multiprocessing.process import BaseProcess
import os
import re
import stat
import struct
from typing import Any, Awaitable, Callable


IDENTITY_PATH = "/v1/mini-dynamo/identity"
MAX_MANIFEST_BYTES = 1 << 20
MAX_TOKENIZER_BYTES = 512 << 20
MAX_TOKEN_BYTES = 4096
MAX_INTERNAL_RESPONSE_BYTES = 1 << 20
MAX_GOLDENS = 64
MAX_ADMITTED_CLASSES = 32
MAX_CORE_PROCESSES = 64
MAX_CORE_FDS = 16_384
MAX_PROC_NET_BYTES = 4 << 20
MAX_RUNTIME_ARGUMENTS = 256
MAX_RUNTIME_ARGUMENT_BYTES = 4096
MAX_RUNTIME_ARGUMENT_TOTAL_BYTES = 64 << 10
MAX_RUNTIME_ENVIRONMENT = 128
MAX_RUNTIME_PACKAGES = 64
MAX_RUNTIME_ARTIFACTS = 16
MAX_RUNTIME_ARTIFACT_BYTES = 1 << 30
MAX_RUNTIME_CMDLINE_BYTES = 128 << 10
DEFAULT_VERIFY_TIMEOUT_MS = 4000
INTERNAL_CANCELLATION_GRACE_SECONDS = 0.25
_HEX_SHA256 = re.compile(r"[0-9a-f]{64}")
_IMAGE_DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
_INCARNATION_COMPONENT = re.compile(r"[A-Za-z0-9._:-]{1,192}")
_ROOT_KEYS = frozenset(
    {
        "schema_version",
        "model",
        "engine",
        "tokenizer",
        "renderer",
        "admitted_request_classes",
        "goldens",
    }
)

ASGIApp = Callable[
    [dict[str, Any], Callable[[], Awaitable[dict[str, Any]]], Callable[[dict[str, Any]], Awaitable[None]]],
    Awaitable[None],
]


class ServingIdentityMiddleware:
    """Serve one authenticated identity document from the vLLM API process."""

    __slots__ = (
        "_app",
        "_goldens",
        "_identity",
        "_live_verified",
        "_runtime",
        "_runtime_evidence",
        "_token",
        "_verification_lock",
        "_verified_core_incarnations",
        "_verify_timeout_seconds",
    )

    def __init__(self, app: ASGIApp) -> None:
        self._app = app
        try:
            self._identity, self._goldens, self._runtime = _load_identity()
            _verify_runtime(self._identity)
            self._runtime_evidence = _verify_process_contract(
                self._runtime["process"]
            )
            self._token = _load_token()
            self._verify_timeout_seconds = _load_verify_timeout()
            self._live_verified = False
            self._verified_core_incarnations: tuple[str, ...] | None = None
            self._verification_lock = asyncio.Lock()
        except Exception:
            # Startup errors must never echo paths, tokens, or manifest content.
            raise RuntimeError("serving identity initialization failed") from None

    async def __call__(
        self,
        scope: dict[str, Any],
        receive: Callable[[], Awaitable[dict[str, Any]]],
        send: Callable[[dict[str, Any]], Awaitable[None]],
    ) -> None:
        if not _is_identity_path(scope):
            await self._app(scope, receive, send)
            return
        if scope.get("method") != "GET":
            await _json_response(send, 405, b'{"error":"method_not_allowed"}')
            return
        if not _authorized(scope, self._token):
            await _json_response(send, 401, b'{"error":"unauthorized"}')
            return

        try:
            core_incarnations, live_kv_events = await asyncio.wait_for(
                self._ensure_live_compatibility(scope),
                timeout=self._verify_timeout_seconds,
            )
            frontend_incarnation = _process_incarnation()
        except Exception:
            await _json_response(send, 503, b'{"error":"identity_unavailable"}')
            return

        identity = dict(self._identity)
        engine = dict(identity["engine"])
        engine["core_process_count"] = len(core_incarnations)
        engine["kv_events"] = live_kv_events
        identity["engine"] = engine
        identity["incarnation"] = {
            "frontend": frontend_incarnation,
            "engine_core": list(core_incarnations),
        }
        identity["runtime"] = self._runtime_evidence
        body = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
        await _json_response(send, 200, body)

    async def _ensure_live_compatibility(
        self, scope: dict[str, Any]
    ) -> tuple[tuple[str, ...], dict[str, Any]]:
        if self._live_verified:
            await _verify_live_health(self._app, scope, self._token)
            current = _live_engine_runtime(scope, self._runtime["engine"])
            if current[0] != self._verified_core_incarnations:
                raise ValueError("engine core incarnation changed")
            return current
        async with self._verification_lock:
            if self._live_verified:
                await _verify_live_health(self._app, scope, self._token)
                current = _live_engine_runtime(scope, self._runtime["engine"])
                if current[0] != self._verified_core_incarnations:
                    raise ValueError("engine core incarnation changed")
                return current
            await _verify_live_health(self._app, scope, self._token)
            before = _live_engine_runtime(scope, self._runtime["engine"])
            await _verify_live_model(
                self._app,
                scope,
                self._token,
                self._identity["model"],
            )
            for golden in self._goldens:
                await _verify_live_golden(
                    self._app,
                    scope,
                    self._token,
                    golden,
                    self._identity["model"]["max_model_len"],
                )
            # Bracket the frontend-only model/render checks with EngineClient
            # liveness. A core failure during verification must not publish.
            await _verify_live_health(self._app, scope, self._token)
            after = _live_engine_runtime(scope, self._runtime["engine"])
            if before != after:
                raise ValueError("engine core incarnation changed")
            self._verified_core_incarnations = after[0]
            self._live_verified = True
            return after


def _load_identity() -> tuple[
    dict[str, Any], tuple[dict[str, Any], ...], dict[str, Any]
]:
    path = os.environ["MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH"]
    expected = os.environ["MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SHA256"]
    if _HEX_SHA256.fullmatch(expected) is None:
        raise ValueError("invalid manifest digest")
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or not 0 < metadata.st_size <= MAX_MANIFEST_BYTES:
            raise ValueError("invalid manifest file")
        raw = bytearray()
        while len(raw) <= MAX_MANIFEST_BYTES:
            chunk = os.read(descriptor, min(65536, MAX_MANIFEST_BYTES + 1 - len(raw)))
            if not chunk:
                break
            raw.extend(chunk)
        if len(raw) != metadata.st_size or len(raw) > MAX_MANIFEST_BYTES:
            raise ValueError("manifest changed while loading")
        if not hmac.compare_digest(hashlib.sha256(raw).hexdigest(), expected):
            raise ValueError("manifest digest mismatch")
        manifest = json.loads(raw)
    finally:
        os.close(descriptor)

    if not isinstance(manifest, dict) or set(manifest) != _ROOT_KEYS:
        raise ValueError("invalid manifest schema")
    if manifest.get("schema_version") != 1:
        raise ValueError("unsupported manifest schema")
    model = _exact_object(manifest, "model", {"id", "root", "max_model_len"})
    engine = _exact_object(manifest, "engine", {"version", "image_digest"})
    tokenizer = _exact_object(manifest, "tokenizer", {"sha256"})
    renderer = _exact_object(manifest, "renderer", {"profile"})
    _bounded_string(model.get("id"))
    _bounded_string(model.get("root"))
    if (
        type(model.get("max_model_len")) is not int
        or not 0 < model["max_model_len"] <= 10_000_000
    ):
        raise ValueError("invalid model context")
    _bounded_string(engine.get("version"))
    if not isinstance(engine.get("image_digest"), str) or _IMAGE_DIGEST.fullmatch(engine["image_digest"]) is None:
        raise ValueError("invalid engine digest")
    if not isinstance(tokenizer.get("sha256"), str) or _HEX_SHA256.fullmatch(tokenizer["sha256"]) is None:
        raise ValueError("invalid tokenizer digest")
    _bounded_string(renderer.get("profile"))
    admitted = _string_set(
        manifest.get("admitted_request_classes"),
        MAX_ADMITTED_CLASSES,
    )
    goldens = _golden_contract(manifest.get("goldens"), model)
    if not admitted.issubset({golden["name"] for golden in goldens}):
        raise ValueError("missing admitted golden")
    identity = {
        "schema_version": 3,
        "model": model,
        "engine": engine,
        "tokenizer": tokenizer,
        "renderer": renderer,
    }
    runtime = _load_runtime_contract(expected)
    return identity, goldens, runtime


def _load_runtime_contract(compatibility_sha256: str) -> dict[str, Any]:
    path = os.environ["MINI_DYNAMO_SERVING_RUNTIME_MANIFEST_PATH"]
    expected = os.environ["MINI_DYNAMO_SERVING_RUNTIME_MANIFEST_SHA256"]
    if _HEX_SHA256.fullmatch(expected) is None:
        raise ValueError("invalid runtime manifest digest")
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or not 0 < metadata.st_size <= MAX_MANIFEST_BYTES:
            raise ValueError("invalid runtime manifest file")
        raw = bytearray()
        while len(raw) <= MAX_MANIFEST_BYTES:
            chunk = os.read(
                descriptor, min(65536, MAX_MANIFEST_BYTES + 1 - len(raw))
            )
            if not chunk:
                break
            raw.extend(chunk)
        if len(raw) != metadata.st_size or len(raw) > MAX_MANIFEST_BYTES:
            raise ValueError("runtime manifest changed while loading")
        if not hmac.compare_digest(hashlib.sha256(raw).hexdigest(), expected):
            raise ValueError("runtime manifest digest mismatch")
        runtime = json.loads(raw)
    finally:
        os.close(descriptor)
    if not isinstance(runtime, dict) or set(runtime) != {
        "schema_version",
        "compatibility_manifest_sha256",
        "engine",
        "process",
    }:
        raise ValueError("invalid runtime manifest schema")
    if (
        runtime.get("schema_version") != 2
        or runtime.get("compatibility_manifest_sha256") != compatibility_sha256
    ):
        raise ValueError("runtime manifest compatibility mismatch")
    engine = _exact_object(runtime, "engine", {"core_process_count", "kv_events"})
    count = engine.get("core_process_count")
    if type(count) is not int or not 0 < count <= MAX_CORE_PROCESSES:
        raise ValueError("invalid engine core process count")
    engine["kv_events"] = _kv_events_contract(engine.get("kv_events"))
    process = _process_contract(runtime.get("process"))
    return {
        "schema_version": 2,
        "compatibility_manifest_sha256": compatibility_sha256,
        "engine": engine,
        "process": process,
    }


def _process_contract(value: Any) -> dict[str, Any]:
    keys = {
        "argv",
        "argv_sha256",
        "environment",
        "environment_sha256",
        "packages",
        "packages_sha256",
        "artifacts",
        "artifacts_sha256",
    }
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError("invalid serving process schema")

    argv = value.get("argv")
    if (
        not isinstance(argv, list)
        or not 0 < len(argv) <= MAX_RUNTIME_ARGUMENTS
        or argv[0] != "serve"
    ):
        raise ValueError("invalid serving process argv")
    total = 0
    for argument in argv:
        _runtime_string(argument, MAX_RUNTIME_ARGUMENT_BYTES)
        total += len(argument.encode())
        if _sensitive_runtime_argument(argument):
            raise ValueError("sensitive serving process argv")
    if total > MAX_RUNTIME_ARGUMENT_TOTAL_BYTES:
        raise ValueError("oversized serving process argv")
    _runtime_digest(value.get("argv_sha256"), _nul_joined_sha256(argv))

    environment = _runtime_mapping(
        value.get("environment"), MAX_RUNTIME_ENVIRONMENT, environment=True
    )
    _runtime_digest(
        value.get("environment_sha256"), _canonical_json_sha256(environment)
    )
    packages = _runtime_mapping(
        value.get("packages"), MAX_RUNTIME_PACKAGES, environment=False
    )
    _runtime_digest(
        value.get("packages_sha256"), _canonical_json_sha256(packages)
    )

    artifacts = value.get("artifacts")
    if (
        not isinstance(artifacts, list)
        or not 0 < len(artifacts) <= MAX_RUNTIME_ARTIFACTS
    ):
        raise ValueError("invalid serving process artifacts")
    normalized_artifacts: list[dict[str, str]] = []
    paths: set[str] = set()
    for artifact in artifacts:
        if not isinstance(artifact, dict) or set(artifact) != {"path", "sha256"}:
            raise ValueError("invalid serving process artifact")
        path = artifact.get("path")
        digest = artifact.get("sha256")
        _runtime_string(path, MAX_RUNTIME_ARGUMENT_BYTES)
        if (
            not path.startswith("/")
            or ".." in path.split("/")
            or path in paths
            or not isinstance(digest, str)
            or _HEX_SHA256.fullmatch(digest) is None
        ):
            raise ValueError("invalid serving process artifact")
        paths.add(path)
        normalized_artifacts.append({"path": path, "sha256": digest})
    _runtime_digest(
        value.get("artifacts_sha256"),
        _canonical_json_sha256(normalized_artifacts),
    )
    return {
        "argv": list(argv),
        "argv_sha256": value["argv_sha256"],
        "environment": environment,
        "environment_sha256": value["environment_sha256"],
        "packages": packages,
        "packages_sha256": value["packages_sha256"],
        "artifacts": normalized_artifacts,
        "artifacts_sha256": value["artifacts_sha256"],
    }


def _runtime_mapping(
    value: Any, limit: int, *, environment: bool
) -> dict[str, str]:
    if not isinstance(value, dict) or not 0 < len(value) <= limit:
        raise ValueError("invalid serving process mapping")
    normalized: dict[str, str] = {}
    for key, item in value.items():
        if not isinstance(key, str) or not key or not key.isascii():
            raise ValueError("invalid serving process mapping key")
        if environment:
            valid_key = len(key.encode()) <= 128 and all(
                character.isupper() or character.isdigit() or character == "_"
                for character in key
            )
            if _sensitive_runtime_environment_key(key):
                valid_key = False
        else:
            valid_key = len(key.encode()) <= 256 and all(
                character.isalnum() or character in "._+-" for character in key
            )
        _runtime_string(item, MAX_RUNTIME_ARGUMENT_BYTES)
        if not valid_key:
            raise ValueError("invalid serving process mapping key")
        normalized[key] = item
    return normalized


def _runtime_string(value: Any, limit: int) -> None:
    if (
        not isinstance(value, str)
        or not value
        or not value.isascii()
        or len(value.encode()) > limit
        or "\0" in value
    ):
        raise ValueError("invalid serving process string")


def _sensitive_runtime_argument(value: str) -> bool:
    return value.split("=", 1)[0] in {
        "--api-key",
        "--token",
        "--hf-token",
        "--authorization",
    }


def _sensitive_runtime_environment_key(value: str) -> bool:
    return (
        "SECRET" in value
        or "PASSWORD" in value
        or "CREDENTIAL" in value
        or "ACCESS_KEY" in value
        or "PRIVATE_KEY" in value
        or "BEARER" in value
        or value.endswith("_TOKEN")
        or value.endswith("_API_KEY")
        or value.endswith("_AUTHORIZATION")
    )


def _runtime_digest(value: Any, expected: str) -> None:
    if (
        not isinstance(value, str)
        or _HEX_SHA256.fullmatch(value) is None
        or not hmac.compare_digest(value, expected)
    ):
        raise ValueError("serving process digest mismatch")


def _nul_joined_sha256(values: list[str]) -> str:
    return hashlib.sha256(b"\0".join(value.encode() for value in values)).hexdigest()


def _canonical_json_sha256(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def _kv_events_contract(value: Any) -> dict[str, Any]:
    keys = {
        "enable_kv_cache_events",
        "publisher",
        "endpoint",
        "replay_endpoint",
        "buffer_steps",
        "hwm",
        "max_queue_size",
        "topic",
    }
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError("invalid KV event schema")
    result = dict(value)
    if result["enable_kv_cache_events"] is not True or result["publisher"] != "zmq":
        raise ValueError("invalid KV event publisher")
    ports = [_wildcard_tcp_port(result[name]) for name in ("endpoint", "replay_endpoint")]
    if ports[0] == ports[1]:
        raise ValueError("duplicate KV event endpoint")
    for name in ("buffer_steps", "hwm", "max_queue_size"):
        if type(result[name]) is not int or not 0 < result[name] <= 1_000_000_000:
            raise ValueError("invalid KV event capacity")
    if not isinstance(result["topic"], str) or len(result["topic"].encode()) > 4096:
        raise ValueError("invalid KV event topic")
    return result


def _wildcard_tcp_port(endpoint: Any) -> int:
    if not isinstance(endpoint, str) or not endpoint.startswith("tcp://*:"):
        raise ValueError("invalid KV event endpoint")
    raw = endpoint.removeprefix("tcp://*:")
    if not raw.isascii() or not raw.isdigit():
        raise ValueError("invalid KV event endpoint")
    port = int(raw)
    if not 0 < port <= 65535:
        raise ValueError("invalid KV event endpoint")
    return port


def _exact_object(manifest: dict[str, Any], name: str, keys: set[str]) -> dict[str, Any]:
    value = manifest.get(name)
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError("invalid manifest object")
    return dict(value)


def _bounded_string(value: Any) -> None:
    if not isinstance(value, str) or not 0 < len(value.encode()) <= 4096:
        raise ValueError("invalid manifest string")


def _string_set(value: Any, limit: int) -> set[str]:
    if not isinstance(value, list) or not 0 < len(value) <= limit:
        raise ValueError("invalid manifest strings")
    for item in value:
        _bounded_string(item)
    unique = set(value)
    if len(unique) != len(value):
        raise ValueError("duplicate manifest string")
    return unique


def _golden_contract(value: Any, model: dict[str, Any]) -> tuple[dict[str, Any], ...]:
    if not isinstance(value, list) or not 0 < len(value) <= MAX_GOLDENS:
        raise ValueError("invalid golden set")
    goldens: list[dict[str, Any]] = []
    names: set[str] = set()
    for golden in value:
        if not isinstance(golden, dict) or set(golden) != {
            "name",
            "endpoint",
            "request",
            "token_count",
            "token_ids_sha256",
        }:
            raise ValueError("invalid golden schema")
        name = golden.get("name")
        _bounded_string(name)
        if name in names or golden.get("endpoint") != "chat":
            raise ValueError("invalid golden identity")
        request = golden.get("request")
        if not isinstance(request, dict) or request.get("model") != model["id"]:
            raise ValueError("invalid golden request")
        token_count = golden.get("token_count")
        if (
            type(token_count) is not int
            or not 0 < token_count <= model["max_model_len"]
        ):
            raise ValueError("invalid golden token count")
        digest = golden.get("token_ids_sha256")
        if not isinstance(digest, str) or _HEX_SHA256.fullmatch(digest) is None:
            raise ValueError("invalid golden digest")
        names.add(name)
        goldens.append(dict(golden))
    return tuple(goldens)


def _load_token() -> bytes:
    value = os.environ.get("MINI_DYNAMO_SERVING_IDENTITY_BEARER_TOKEN")
    if value is None:
        value = os.environ.get("VLLM_API_KEY")
    if value is None:
        raise ValueError("missing identity bearer")
    encoded = value.encode()
    if not 0 < len(encoded) <= MAX_TOKEN_BYTES or any(byte < 0x21 or byte > 0x7E for byte in encoded):
        raise ValueError("invalid identity bearer")
    return b"Bearer " + encoded


def _load_verify_timeout() -> float:
    raw = os.environ.get(
        "MINI_DYNAMO_SERVING_IDENTITY_VERIFY_TIMEOUT_MS",
        str(DEFAULT_VERIFY_TIMEOUT_MS),
    )
    try:
        milliseconds = int(raw)
    except ValueError as exc:
        raise ValueError("invalid identity verification timeout") from exc
    if not 100 <= milliseconds <= 30_000:
        raise ValueError("invalid identity verification timeout")
    return milliseconds / 1000


def _verify_runtime(identity: dict[str, Any]) -> None:
    model = identity["model"]
    engine = identity["engine"]
    tokenizer = identity["tokenizer"]
    if importlib_metadata.version("vllm") != engine["version"]:
        raise ValueError("vLLM version mismatch")
    if os.environ.get("SERVED_MODEL_NAME") != model["id"]:
        raise ValueError("served model mismatch")
    try:
        max_model_len = int(os.environ["MAX_MODEL_LEN"])
    except (KeyError, ValueError) as exc:
        raise ValueError("model context unavailable") from exc
    if max_model_len != model["max_model_len"]:
        raise ValueError("model context mismatch")
    tokenizer_path = os.environ["MINI_DYNAMO_SERVING_IDENTITY_TOKENIZER_PATH"]
    if _regular_file_sha256(tokenizer_path, MAX_TOKENIZER_BYTES) != tokenizer["sha256"]:
        raise ValueError("tokenizer digest mismatch")


def _verify_process_contract(expected: dict[str, Any]) -> dict[str, str]:
    if _normalized_process_argv() != expected["argv"]:
        raise ValueError("serving process argv mismatch")
    if any(os.environ.get(key) != value for key, value in expected["environment"].items()):
        raise ValueError("serving process environment mismatch")
    for package, version in expected["packages"].items():
        if importlib_metadata.version(package) != version:
            raise ValueError("serving process package mismatch")
    for artifact in expected["artifacts"]:
        if (
            _regular_file_sha256(
                artifact["path"], MAX_RUNTIME_ARTIFACT_BYTES
            )
            != artifact["sha256"]
        ):
            raise ValueError("serving process artifact mismatch")
    return {
        "argv_sha256": expected["argv_sha256"],
        "environment_sha256": expected["environment_sha256"],
        "packages_sha256": expected["packages_sha256"],
        "artifacts_sha256": expected["artifacts_sha256"],
    }


def _normalized_process_argv() -> list[str]:
    raw = _read_bounded("/proc/self/cmdline", MAX_RUNTIME_CMDLINE_BYTES)
    if not raw.endswith(b"\0"):
        raise ValueError("serving process argv unavailable")
    encoded = raw[:-1].split(b"\0")
    if not encoded or any(not value for value in encoded):
        raise ValueError("serving process argv unavailable")
    try:
        values = [value.decode("ascii") for value in encoded]
    except UnicodeDecodeError as error:
        raise ValueError("serving process argv unavailable") from error
    positions = [index for index, value in enumerate(values) if value == "serve"]
    if len(positions) != 1 or positions[0] not in {1, 2}:
        raise ValueError("serving process argv unavailable")
    normalized = values[positions[0] :]
    if len(normalized) > MAX_RUNTIME_ARGUMENTS:
        raise ValueError("serving process argv unavailable")
    return normalized


def _regular_file_sha256(path: str, limit: int) -> str:
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or not 0 < metadata.st_size <= limit:
            raise ValueError("invalid runtime artifact")
        digest = hashlib.sha256()
        total = 0
        while total <= limit:
            chunk = os.read(descriptor, min(1 << 20, limit + 1 - total))
            if not chunk:
                break
            total += len(chunk)
            digest.update(chunk)
        if total != metadata.st_size or total > limit:
            raise ValueError("runtime artifact changed while loading")
        return digest.hexdigest()
    finally:
        os.close(descriptor)


async def _verify_live_health(
    app: ASGIApp,
    scope: dict[str, Any],
    token: bytes,
) -> None:
    status, body = await _internal_request(app, scope, token, "GET", "/health")
    if status != 200 or body:
        raise ValueError("live health mismatch")


async def _verify_live_model(
    app: ASGIApp,
    scope: dict[str, Any],
    token: bytes,
    expected: dict[str, Any],
) -> None:
    status, body = await _internal_request(
        app,
        scope,
        token,
        "GET",
        "/v1/models",
    )
    if status != 200:
        raise ValueError("live model unavailable")
    document = json.loads(body)
    if not isinstance(document, dict) or not isinstance(document.get("data"), list):
        raise ValueError("live model malformed")
    matching = [
        item
        for item in document["data"]
        if isinstance(item, dict) and item.get("id") == expected["id"]
    ]
    if len(matching) != 1 or any(
        matching[0].get(key) != expected[key]
        for key in ("root", "max_model_len")
    ):
        raise ValueError("live model mismatch")


async def _verify_live_golden(
    app: ASGIApp,
    scope: dict[str, Any],
    token: bytes,
    golden: dict[str, Any],
    expected_max_model_len: int,
) -> None:
    request = json.dumps(
        golden["request"], sort_keys=True, separators=(",", ":")
    ).encode()
    status, body = await _internal_request(
        app,
        scope,
        token,
        "POST",
        "/tokenize",
        request,
    )
    if status != 200:
        raise ValueError("live golden unavailable")
    document = json.loads(body)
    if not isinstance(document, dict) or set(document).difference(
        {"count", "max_model_len", "tokens", "token_strs"}
    ):
        raise ValueError("live golden malformed")
    tokens = document.get("tokens")
    if (
        not isinstance(tokens, list)
        or len(tokens) != golden["token_count"]
        or document.get("count") != len(tokens)
        or document.get("max_model_len") != expected_max_model_len
        or any(
            type(token_id) is not int or not 0 <= token_id <= 0xFFFFFFFF
            for token_id in tokens
        )
        or not hmac.compare_digest(
            _token_ids_sha256(tokens), golden["token_ids_sha256"]
        )
    ):
        raise ValueError("live golden mismatch")


def _token_ids_sha256(tokens: list[int]) -> str:
    digest = hashlib.sha256()
    for token_id in tokens:
        digest.update(struct.pack(">I", token_id))
    return digest.hexdigest()


def _live_engine_runtime(
    scope: dict[str, Any], expected_engine: dict[str, Any]
) -> tuple[tuple[str, ...], dict[str, Any]]:
    app = scope.get("app")
    state = getattr(app, "state", None)
    client = getattr(state, "engine_client", None)
    if not _exact_type(client, "vllm.v1.engine.async_llm", "AsyncLLM"):
        raise ValueError("unexpected serving engine client")
    core = getattr(client, "engine_core", None)
    if not _exact_type(core, "vllm.v1.engine.core_client", "AsyncMPClient"):
        raise ValueError("unexpected engine core client")
    resources = getattr(core, "resources", None)
    manager = getattr(resources, "engine_manager", None)
    if not _exact_type(
        manager, "vllm.v1.engine.utils", "CoreEngineProcManager"
    ):
        raise ValueError("unexpected engine core manager")
    processes = getattr(manager, "processes", None)
    if (
        type(processes) is not list
        or len(processes) != expected_engine["core_process_count"]
    ):
        raise ValueError("engine core process count mismatch")

    config = getattr(getattr(core, "vllm_config", None), "kv_events_config", None)
    if not _exact_type(config, "vllm.config.kv_events", "KVEventsConfig"):
        raise ValueError("unexpected KV event config")
    live_config = {
        name: getattr(config, name, None)
        for name in expected_engine["kv_events"]
    }
    if live_config != expected_engine["kv_events"]:
        raise ValueError("live KV event config mismatch")
    expected_ports = {
        _wildcard_tcp_port(live_config["endpoint"]),
        _wildcard_tcp_port(live_config["replay_endpoint"]),
    }

    incarnations = tuple(
        sorted(
            _inspect_core_process(process, expected_ports)
            for process in processes
        )
    )
    if len(set(incarnations)) != len(incarnations):
        raise ValueError("duplicate engine core incarnation")
    return incarnations, live_config


def _exact_type(value: Any, module: str, name: str) -> bool:
    kind = type(value)
    return kind.__module__ == module and kind.__name__ == name


def _inspect_core_process(process: Any, expected_ports: set[int]) -> str:
    if not isinstance(process, BaseProcess):
        raise ValueError("invalid engine core process")
    pid = process.pid
    if (
        type(pid) is not int
        or pid <= 1
        or pid == os.getpid()
        or process.exitcode is not None
        or not process.is_alive()
    ):
        raise ValueError("engine core process unavailable")
    before = _proc_stat(pid)
    if before[0] in {"Z", "X"} or before[1] != os.getpid():
        raise ValueError("engine core process unavailable")
    _verify_owned_listeners(pid, expected_ports)
    after = _proc_stat(pid)
    if (
        after[0] in {"Z", "X"}
        or after[1:] != before[1:]
        or process.exitcode is not None
        or not process.is_alive()
    ):
        raise ValueError("engine core process changed")
    return f"{_boot_id()}:{pid}:{after[2]}"


def _proc_stat(pid: int) -> tuple[str, int, int]:
    raw = _read_small(f"/proc/{pid}/stat", 4096)
    end = raw.rfind(")")
    if end < 0:
        raise ValueError("invalid process stat")
    fields = raw[end + 2 :].split()
    if len(fields) <= 19 or len(fields[0]) != 1:
        raise ValueError("invalid process stat")
    try:
        parent_pid = int(fields[1])
        start_ticks = int(fields[19])
    except ValueError as error:
        raise ValueError("invalid process stat") from error
    if parent_pid <= 0 or start_ticks <= 0:
        raise ValueError("invalid process stat")
    return fields[0], parent_pid, start_ticks


def _verify_owned_listeners(pid: int, expected_ports: set[int]) -> None:
    frontend_net = os.stat("/proc/self/ns/net")
    core_net = os.stat(f"/proc/{pid}/ns/net")
    if (frontend_net.st_dev, frontend_net.st_ino) != (core_net.st_dev, core_net.st_ino):
        raise ValueError("engine core network namespace mismatch")
    entries = os.listdir(f"/proc/{pid}/fd")
    if len(entries) > MAX_CORE_FDS:
        raise ValueError("engine core file descriptor set is oversized")
    socket_inodes: set[str] = set()
    for entry in entries:
        if not entry.isdigit():
            raise ValueError("invalid engine core file descriptor")
        try:
            target = os.readlink(f"/proc/{pid}/fd/{entry}")
        except FileNotFoundError:
            # The descriptor used to enumerate this proc directory, or another
            # unrelated descriptor, may close between listdir and readlink.
            # Missing publisher descriptors still fail the exact ownership
            # check below.
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            inode = target[8:-1]
            if not inode.isdigit():
                raise ValueError("invalid engine core socket")
            socket_inodes.add(inode)
    if len(socket_inodes) > MAX_CORE_FDS:
        raise ValueError("engine core socket set is oversized")

    matches = {port: set() for port in expected_ports}
    for family, address_width in (("tcp", 8), ("tcp6", 32)):
        raw = _read_bounded(f"/proc/{pid}/net/{family}", MAX_PROC_NET_BYTES)
        lines = raw.decode("ascii").splitlines()
        if not lines or len(lines) > MAX_CORE_FDS + 1:
            raise ValueError("invalid process network table")
        for line in lines[1:]:
            fields = line.split()
            if len(fields) < 10:
                raise ValueError("invalid process network table")
            local = fields[1].split(":")
            if len(local) != 2 or fields[3] != "0A":
                continue
            address, raw_port = local
            try:
                port = int(raw_port, 16)
            except ValueError as error:
                raise ValueError("invalid process network table") from error
            inode = fields[9]
            if (
                port in matches
                and len(address) == address_width
                and set(address) == {"0"}
                and inode in socket_inodes
            ):
                matches[port].add(inode)
    if any(len(inodes) != 1 for inodes in matches.values()) or len(
        set().union(*matches.values())
    ) != len(matches):
        raise ValueError("KV event listener ownership mismatch")


def _read_bounded(path: str, limit: int) -> bytes:
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC)
    try:
        raw = bytearray()
        while len(raw) <= limit:
            chunk = os.read(descriptor, min(65536, limit + 1 - len(raw)))
            if not chunk:
                break
            raw.extend(chunk)
        if not raw or len(raw) > limit:
            raise ValueError("runtime evidence is oversized")
        return bytes(raw)
    finally:
        os.close(descriptor)


async def _internal_request(
    app: ASGIApp,
    base_scope: dict[str, Any],
    token: bytes,
    method: str,
    path: str,
    body: bytes = b"",
) -> tuple[int, bytes]:
    if base_scope.get("app") is None:
        raise ValueError("internal application unavailable")
    request_sent = False
    disconnect = asyncio.Event()
    status: int | None = None
    response = bytearray()
    finished = False

    async def receive() -> dict[str, Any]:
        nonlocal request_sent
        if not request_sent:
            request_sent = True
            return {"type": "http.request", "body": body, "more_body": False}
        # vLLM's tokenize endpoint races work against client disconnect. Stay
        # connected normally, but let outer cancellation drive its cooperative
        # cleanup path instead of orphaning the route's two child tasks.
        await disconnect.wait()
        return {"type": "http.disconnect"}

    async def send(message: dict[str, Any]) -> None:
        nonlocal finished, status
        message_type = message.get("type")
        if message_type == "http.response.start":
            if status is not None or finished:
                raise ValueError("invalid internal response")
            candidate = message.get("status")
            if type(candidate) is not int or not 100 <= candidate <= 599:
                raise ValueError("invalid internal response")
            status = candidate
            return
        if message_type != "http.response.body" or status is None or finished:
            raise ValueError("invalid internal response")
        chunk = message.get("body", b"")
        if (
            not isinstance(chunk, bytes)
            or len(response) + len(chunk) > MAX_INTERNAL_RESPONSE_BYTES
        ):
            raise ValueError("internal response too large")
        response.extend(chunk)
        finished = not bool(message.get("more_body", False))

    headers = [
        (b"host", b"mini-dynamo.internal"),
        (b"authorization", token),
    ]
    if body:
        headers.extend(
            [
                (b"content-type", b"application/json"),
                (b"content-length", str(len(body)).encode()),
            ]
        )
    scope = dict(base_scope)
    scope.update(
        {
            "type": "http",
            "asgi": {"version": "3.0", "spec_version": "2.3"},
            "http_version": "1.1",
            "method": method,
            "scheme": "http",
            "path": path,
            "raw_path": path.encode(),
            "query_string": b"",
            "root_path": base_scope.get("root_path", ""),
            "headers": headers,
            "client": ("127.0.0.1", 0),
            "server": ("mini-dynamo.internal", 80),
        }
    )
    app_task = asyncio.create_task(app(scope, receive, send))
    try:
        await asyncio.shield(app_task)
    except asyncio.CancelledError:
        current = asyncio.current_task()
        if current is None or current.cancelling() == 0:
            await asyncio.gather(app_task, return_exceptions=True)
            raise ValueError("internal request cancelled") from None
        disconnect.set()
        done, _ = await asyncio.wait(
            {app_task}, timeout=INTERNAL_CANCELLATION_GRACE_SECONDS
        )
        if not done:
            app_task.cancel()
        await asyncio.gather(app_task, return_exceptions=True)
        # The pinned vLLM cancellation wrapper cancels, but does not await, its
        # losing child. Give that cancellation one turn before returning.
        await asyncio.sleep(0)
        raise
    if status is None or not finished:
        raise ValueError("incomplete internal response")
    return status, bytes(response)


def _is_identity_path(scope: dict[str, Any]) -> bool:
    if scope.get("type") != "http" or scope.get("path") != IDENTITY_PATH:
        return False
    raw_path = scope.get("raw_path")
    return raw_path is None or raw_path == IDENTITY_PATH.encode()


def _authorized(scope: dict[str, Any], expected: bytes) -> bool:
    values = [
        value
        for name, value in scope.get("headers", ())
        if name.lower() == b"authorization"
    ]
    return len(values) == 1 and hmac.compare_digest(values[0], expected)


def _process_incarnation() -> str:
    boot_id = _boot_id()
    process_stat = _read_small("/proc/self/stat", 4096)
    end = process_stat.rfind(")")
    if end < 0:
        raise RuntimeError("process identity unavailable")
    fields = process_stat[end + 2 :].split()
    if len(fields) <= 19:
        raise RuntimeError("process identity unavailable")
    start_ticks = fields[19]
    if not start_ticks.isdigit() or start_ticks == "0":
        raise RuntimeError("process identity unavailable")
    incarnation = f"{boot_id}:{os.getpid()}:{start_ticks}"
    if len(incarnation) > 256:
        raise RuntimeError("process identity unavailable")
    return incarnation


def _boot_id() -> str:
    boot_id = _read_small("/proc/sys/kernel/random/boot_id", 128).strip()
    if _INCARNATION_COMPONENT.fullmatch(boot_id) is None:
        raise RuntimeError("process identity unavailable")
    return boot_id


def _read_small(path: str, limit: int) -> str:
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC)
    try:
        raw = os.read(descriptor, limit + 1)
    finally:
        os.close(descriptor)
    if not raw or len(raw) > limit:
        raise RuntimeError("process identity unavailable")
    return raw.decode("ascii")


async def _json_response(
    send: Callable[[dict[str, Any]], Awaitable[None]], status_code: int, body: bytes
) -> None:
    await send(
        {
            "type": "http.response.start",
            "status": status_code,
            "headers": [
                (b"content-type", b"application/json"),
                (b"cache-control", b"no-store"),
                (b"content-length", str(len(body)).encode()),
            ],
        }
    )
    await send({"type": "http.response.body", "body": body})
