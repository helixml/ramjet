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
        "_token",
        "_verification_lock",
        "_verify_timeout_seconds",
    )

    def __init__(self, app: ASGIApp) -> None:
        self._app = app
        try:
            self._identity, self._goldens = _load_identity()
            _verify_runtime(self._identity)
            self._token = _load_token()
            self._verify_timeout_seconds = _load_verify_timeout()
            self._live_verified = False
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
            await asyncio.wait_for(
                self._ensure_live_compatibility(scope),
                timeout=self._verify_timeout_seconds,
            )
        except Exception:
            await _json_response(send, 503, b'{"error":"identity_unavailable"}')
            return

        identity = dict(self._identity)
        identity["incarnation"] = _process_incarnation()
        body = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
        await _json_response(send, 200, body)

    async def _ensure_live_compatibility(self, scope: dict[str, Any]) -> None:
        if self._live_verified:
            await _verify_live_health(self._app, scope, self._token)
            return
        async with self._verification_lock:
            if self._live_verified:
                await _verify_live_health(self._app, scope, self._token)
                return
            await _verify_live_health(self._app, scope, self._token)
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
            self._live_verified = True


def _load_identity() -> tuple[dict[str, Any], tuple[dict[str, Any], ...]]:
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
        "schema_version": 1,
        "model": model,
        "engine": engine,
        "tokenizer": tokenizer,
        "renderer": renderer,
    }
    return identity, goldens


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
    boot_id = _read_small("/proc/sys/kernel/random/boot_id", 128).strip()
    process_stat = _read_small("/proc/self/stat", 4096)
    end = process_stat.rfind(")")
    if end < 0:
        raise RuntimeError("process identity unavailable")
    fields = process_stat[end + 2 :].split()
    if len(fields) <= 19:
        raise RuntimeError("process identity unavailable")
    start_ticks = fields[19]
    if _INCARNATION_COMPONENT.fullmatch(boot_id) is None or not start_ticks.isdigit() or start_ticks == "0":
        raise RuntimeError("process identity unavailable")
    incarnation = f"{boot_id}:{start_ticks}"
    if len(incarnation) > 256:
        raise RuntimeError("process identity unavailable")
    return incarnation


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
