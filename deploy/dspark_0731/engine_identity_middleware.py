"""Authenticated, fail-closed vLLM serving-identity ASGI middleware.

This module intentionally depends only on the Python standard library. vLLM
loads :class:`ServingIdentityMiddleware` through ``--middleware``. Ordinary
requests take one exact path comparison and are passed to the original ASGI
application; the control endpoint never adds a network hop to inference.
"""

from __future__ import annotations

import hashlib
import hmac
from importlib import metadata as importlib_metadata
import json
import os
import re
import stat
from typing import Any, Awaitable, Callable


IDENTITY_PATH = "/v1/mini-dynamo/identity"
MAX_MANIFEST_BYTES = 1 << 20
MAX_TOKENIZER_BYTES = 512 << 20
MAX_TOKEN_BYTES = 4096
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

    __slots__ = ("_app", "_identity", "_token")

    def __init__(self, app: ASGIApp) -> None:
        self._app = app
        try:
            self._identity = _load_identity()
            _verify_runtime(self._identity)
            self._token = _load_token()
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

        identity = dict(self._identity)
        identity["incarnation"] = _process_incarnation()
        body = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
        await _json_response(send, 200, body)


def _load_identity() -> dict[str, Any]:
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

    if not isinstance(manifest, dict) or not set(manifest).issubset(_ROOT_KEYS):
        raise ValueError("invalid manifest schema")
    if manifest.get("schema_version") != 1:
        raise ValueError("unsupported manifest schema")
    model = _exact_object(manifest, "model", {"id", "root", "max_model_len"})
    engine = _exact_object(manifest, "engine", {"version", "image_digest"})
    tokenizer = _exact_object(manifest, "tokenizer", {"sha256"})
    renderer = _exact_object(manifest, "renderer", {"profile"})
    _bounded_string(model.get("id"))
    _bounded_string(model.get("root"))
    if not isinstance(model.get("max_model_len"), int) or not 0 < model["max_model_len"] <= 10_000_000:
        raise ValueError("invalid model context")
    _bounded_string(engine.get("version"))
    if not isinstance(engine.get("image_digest"), str) or _IMAGE_DIGEST.fullmatch(engine["image_digest"]) is None:
        raise ValueError("invalid engine digest")
    if not isinstance(tokenizer.get("sha256"), str) or _HEX_SHA256.fullmatch(tokenizer["sha256"]) is None:
        raise ValueError("invalid tokenizer digest")
    _bounded_string(renderer.get("profile"))
    return {
        "schema_version": 1,
        "model": model,
        "engine": engine,
        "tokenizer": tokenizer,
        "renderer": renderer,
    }


def _exact_object(manifest: dict[str, Any], name: str, keys: set[str]) -> dict[str, Any]:
    value = manifest.get(name)
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError("invalid manifest object")
    return dict(value)


def _bounded_string(value: Any) -> None:
    if not isinstance(value, str) or not 0 < len(value.encode()) <= 4096:
        raise ValueError("invalid manifest string")


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
