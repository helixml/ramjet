import asyncio
import hashlib
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = (
    Path(__file__).parents[1]
    / "deploy"
    / "dspark_0731"
    / "engine_identity_middleware.py"
)
SPEC = importlib.util.spec_from_file_location("engine_identity_middleware", MODULE_PATH)
middleware_module = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(middleware_module)


class EngineIdentityMiddlewareTest(unittest.TestCase):
    def setUp(self):
        self.manifest = {
            "schema_version": 1,
            "model": {"id": "model", "root": "root", "max_model_len": 4096},
            "engine": {
                "version": "v1",
                "image_digest": "sha256:" + "a" * 64,
            },
            "tokenizer": {"sha256": "b" * 64},
            "renderer": {"profile": "profile"},
            "admitted_request_classes": ["plain"],
            "goldens": [],
        }
        self.temporary = tempfile.TemporaryDirectory()
        self.manifest_path = Path(self.temporary.name) / "manifest.json"
        self.tokenizer_path = Path(self.temporary.name) / "tokenizer.json"
        self.tokenizer_path.write_bytes(b"tokenizer-artifact")
        self.manifest["tokenizer"]["sha256"] = hashlib.sha256(
            self.tokenizer_path.read_bytes()
        ).hexdigest()
        self.downstream_calls = 0

    def tearDown(self):
        self.temporary.cleanup()

    def middleware(self, manifest=None, digest=None, token="secret-token"):
        raw = json.dumps(manifest or self.manifest).encode()
        self.manifest_path.write_bytes(raw)
        environment = {
            "MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH": str(self.manifest_path),
            "MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SHA256": digest
            or hashlib.sha256(raw).hexdigest(),
            "VLLM_API_KEY": token,
            "SERVED_MODEL_NAME": "model",
            "MAX_MODEL_LEN": "4096",
            "MINI_DYNAMO_SERVING_IDENTITY_TOKENIZER_PATH": str(
                self.tokenizer_path
            ),
        }

        async def downstream(scope, receive, send):
            del scope, receive
            self.downstream_calls += 1
            await send({"type": "http.response.start", "status": 204, "headers": []})
            await send({"type": "http.response.body", "body": b""})

        with patch.dict(os.environ, environment, clear=True), patch.object(
            middleware_module.importlib_metadata, "version", return_value="v1"
        ):
            return middleware_module.ServingIdentityMiddleware(downstream)

    @staticmethod
    def invoke(middleware, path, authorization=None, method="GET", raw_path=None):
        headers = [] if authorization is None else [(b"authorization", authorization)]
        messages = []
        scope = {
            "type": "http",
            "method": method,
            "path": path,
            "raw_path": path.encode() if raw_path is None else raw_path,
            "headers": headers,
        }

        async def receive():
            return {"type": "http.request", "body": b"", "more_body": False}

        async def send(message):
            messages.append(message)

        asyncio.run(middleware(scope, receive, send))
        return messages

    def test_authenticated_identity_is_bounded_and_process_specific(self):
        middleware = self.middleware()
        messages = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(messages[0]["status"], 200)
        self.assertIn((b"cache-control", b"no-store"), messages[0]["headers"])
        identity = json.loads(messages[1]["body"])
        self.assertEqual(identity["schema_version"], 1)
        self.assertEqual(identity["model"], self.manifest["model"])
        self.assertNotIn("goldens", identity)
        self.assertRegex(identity["incarnation"], r"^[A-Za-z0-9._:-]{1,256}$")
        self.assertEqual(self.downstream_calls, 0)

    def test_control_path_auth_and_method_fail_without_dispatch(self):
        middleware = self.middleware()
        for authorization, method, expected in [
            (None, "GET", 401),
            (b"Bearer wrong", "GET", 401),
            (b"Bearer secret-token", "POST", 405),
        ]:
            messages = self.invoke(
                middleware,
                middleware_module.IDENTITY_PATH,
                authorization=authorization,
                method=method,
            )
            self.assertEqual(messages[0]["status"], expected)
        self.assertEqual(self.downstream_calls, 0)

    def test_noncanonical_and_ordinary_paths_pass_through(self):
        middleware = self.middleware()
        for path, raw_path in [
            ("/v1/models", None),
            (middleware_module.IDENTITY_PATH + "/", None),
            (middleware_module.IDENTITY_PATH, b"/v1/mini-dynamo/%69dentity"),
        ]:
            messages = self.invoke(middleware, path, raw_path=raw_path)
            self.assertEqual(messages[0]["status"], 204)
        self.assertEqual(self.downstream_calls, 3)

    def test_manifest_pin_schema_and_token_are_fail_closed_and_content_free(self):
        cases = [
            {"digest": "0" * 64},
            {"manifest": {**self.manifest, "unexpected": "private-content"}},
            {"token": "bad token"},
        ]
        for arguments in cases:
            with self.subTest(arguments=sorted(arguments)):
                with self.assertRaisesRegex(
                    RuntimeError, "^serving identity initialization failed$"
                ) as raised:
                    self.middleware(**arguments)
                self.assertNotIn("private-content", str(raised.exception))
                self.assertNotIn(str(self.manifest_path), str(raised.exception))

    def test_duplicate_authorization_is_rejected(self):
        middleware = self.middleware()
        messages = []
        scope = {
            "type": "http",
            "method": "GET",
            "path": middleware_module.IDENTITY_PATH,
            "raw_path": middleware_module.IDENTITY_PATH.encode(),
            "headers": [
                (b"authorization", b"Bearer secret-token"),
                (b"Authorization", b"Bearer secret-token"),
            ],
        }

        async def receive():
            return {"type": "http.request", "body": b"", "more_body": False}

        async def send(message):
            messages.append(message)

        asyncio.run(middleware(scope, receive, send))
        self.assertEqual(messages[0]["status"], 401)
        self.assertEqual(self.downstream_calls, 0)

    def test_live_runtime_identity_must_match_the_manifest(self):
        raw = json.dumps(self.manifest).encode()
        self.manifest_path.write_bytes(raw)
        base = {
            "MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH": str(self.manifest_path),
            "MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SHA256": hashlib.sha256(raw).hexdigest(),
            "MINI_DYNAMO_SERVING_IDENTITY_TOKENIZER_PATH": str(self.tokenizer_path),
            "VLLM_API_KEY": "token",
            "SERVED_MODEL_NAME": "model",
            "MAX_MODEL_LEN": "4096",
        }

        async def downstream(scope, receive, send):
            del scope, receive, send

        cases = [
            ({"SERVED_MODEL_NAME": "other"}, "v1"),
            ({"MAX_MODEL_LEN": "8192"}, "v1"),
            ({}, "v2"),
        ]
        for changed, version in cases:
            environment = {**base, **changed}
            with self.subTest(changed=changed, version=version), patch.dict(
                os.environ, environment, clear=True
            ), patch.object(
                middleware_module.importlib_metadata,
                "version",
                return_value=version,
            ), self.assertRaisesRegex(
                RuntimeError, "^serving identity initialization failed$"
            ):
                middleware_module.ServingIdentityMiddleware(downstream)

        self.tokenizer_path.write_bytes(b"changed")
        with patch.dict(os.environ, base, clear=True), patch.object(
            middleware_module.importlib_metadata, "version", return_value="v1"
        ), self.assertRaisesRegex(
            RuntimeError, "^serving identity initialization failed$"
        ):
            middleware_module.ServingIdentityMiddleware(downstream)


if __name__ == "__main__":
    unittest.main()
