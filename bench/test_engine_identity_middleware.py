import asyncio
import hashlib
import importlib.util
import json
import os
import struct
import tempfile
import time
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
        self.golden_tokens = [1, 7, 65537]
        golden_digest = hashlib.sha256(
            b"".join(struct.pack(">I", token) for token in self.golden_tokens)
        ).hexdigest()
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
            "goldens": [
                {
                    "name": "plain",
                    "endpoint": "chat",
                    "request": {
                        "model": "model",
                        "messages": [{"role": "user", "content": "hello"}],
                        "add_generation_prompt": True,
                        "return_token_strs": False,
                    },
                    "token_count": len(self.golden_tokens),
                    "token_ids_sha256": golden_digest,
                }
            ],
        }
        self.temporary = tempfile.TemporaryDirectory()
        self.manifest_path = Path(self.temporary.name) / "manifest.json"
        self.tokenizer_path = Path(self.temporary.name) / "tokenizer.json"
        self.tokenizer_path.write_bytes(b"tokenizer-artifact")
        self.manifest["tokenizer"]["sha256"] = hashlib.sha256(
            self.tokenizer_path.read_bytes()
        ).hexdigest()
        self.downstream_calls = 0
        self.internal_paths = []
        self.live_health_status = 200
        self.live_model_root = "root"
        self.live_tokens = list(self.golden_tokens)
        self.stall_internal = False

    def tearDown(self):
        self.temporary.cleanup()

    def middleware(
        self,
        manifest=None,
        digest=None,
        token="secret-token",
        timeout_ms="4000",
    ):
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
            "MINI_DYNAMO_SERVING_IDENTITY_VERIFY_TIMEOUT_MS": timeout_ms,
        }

        async def downstream(scope, receive, send):
            path = scope["path"]
            if path in {"/health", "/v1/models", "/tokenize"}:
                self.internal_paths.append(path)
                if self.stall_internal:
                    await asyncio.Event().wait()
                if path == "/health":
                    status, body = self.live_health_status, b""
                elif path == "/v1/models":
                    status = 200
                    body = json.dumps(
                        {
                            "data": [
                                {
                                    "id": "model",
                                    "root": self.live_model_root,
                                    "max_model_len": 4096,
                                }
                            ]
                        }
                    ).encode()
                else:
                    request = await receive()
                    self.assertEqual(request["type"], "http.request")
                    self.assertFalse(request["more_body"])
                    self.assertEqual(
                        json.loads(request["body"]),
                        self.manifest["goldens"][0]["request"],
                    )
                    status = 200
                    body = json.dumps(
                        {
                            "count": len(self.live_tokens),
                            "max_model_len": 4096,
                            "tokens": self.live_tokens,
                            "token_strs": None,
                        }
                    ).encode()
                await send(
                    {"type": "http.response.start", "status": status, "headers": []}
                )
                await send(
                    {"type": "http.response.body", "body": body, "more_body": False}
                )
                return
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
            "app": object(),
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
        self.assertEqual(
            self.internal_paths,
            ["/health", "/v1/models", "/tokenize", "/health"],
        )
        self.assertEqual(self.downstream_calls, 0)

        # The expensive live renderer proof is process-local and cached only
        # after every check succeeds.
        again = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(again[0]["status"], 200)
        self.assertEqual(
            self.internal_paths,
            ["/health", "/v1/models", "/tokenize", "/health", "/health"],
        )

        # Cached renderer evidence never masks a subsequent EngineCore health
        # failure: publication remains conditional on current liveness.
        self.live_health_status = 503
        unhealthy = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(unhealthy[0]["status"], 503)
        self.assertEqual(
            unhealthy[1]["body"], b'{"error":"identity_unavailable"}'
        )

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
            ("/v1/chat/completions", None),
            (middleware_module.IDENTITY_PATH + "/", None),
            (middleware_module.IDENTITY_PATH, b"/v1/mini-dynamo/%69dentity"),
        ]:
            messages = self.invoke(middleware, path, raw_path=raw_path)
            self.assertEqual(messages[0]["status"], 204)
        self.assertEqual(self.downstream_calls, 3)

    def test_manifest_pin_schema_and_token_are_fail_closed_and_content_free(self):
        duplicate_golden = json.loads(json.dumps(self.manifest))
        duplicate_golden["goldens"].append(
            json.loads(json.dumps(duplicate_golden["goldens"][0]))
        )
        missing_admitted = json.loads(json.dumps(self.manifest))
        missing_admitted["admitted_request_classes"] = ["tools"]
        boolean_context = json.loads(json.dumps(self.manifest))
        boolean_context["model"]["max_model_len"] = True
        cases = [
            {"digest": "0" * 64},
            {"manifest": {**self.manifest, "unexpected": "private-content"}},
            {"manifest": duplicate_golden},
            {"manifest": missing_admitted},
            {"manifest": boolean_context},
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
            "app": object(),
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
            "MINI_DYNAMO_SERVING_IDENTITY_VERIFY_TIMEOUT_MS": "4000",
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

    def test_live_model_renderer_and_health_must_match_before_publication(self):
        cases = [
            ("model", lambda: setattr(self, "live_model_root", "other")),
            ("renderer", lambda: setattr(self, "live_tokens", [1, 2, 3])),
            ("health", lambda: setattr(self, "live_health_status", 503)),
        ]
        for name, mutate in cases:
            with self.subTest(name=name):
                self.live_health_status = 200
                self.live_model_root = "root"
                self.live_tokens = list(self.golden_tokens)
                middleware = self.middleware()
                mutate()
                messages = self.invoke(
                    middleware,
                    middleware_module.IDENTITY_PATH,
                    authorization=b"Bearer secret-token",
                )
                self.assertEqual(messages[0]["status"], 503)
                self.assertEqual(
                    messages[1]["body"], b'{"error":"identity_unavailable"}'
                )

    def test_concurrent_first_probes_share_one_renderer_proof(self):
        middleware = self.middleware()

        async def invoke_once():
            messages = []
            scope = {
                "type": "http",
                "app": object(),
                "method": "GET",
                "path": middleware_module.IDENTITY_PATH,
                "raw_path": middleware_module.IDENTITY_PATH.encode(),
                "headers": [(b"authorization", b"Bearer secret-token")],
            }

            async def receive():
                return {"type": "http.request", "body": b"", "more_body": False}

            async def send(message):
                messages.append(message)

            await middleware(scope, receive, send)
            return messages

        async def scenario():
            return await asyncio.gather(*(invoke_once() for _ in range(8)))

        results = asyncio.run(scenario())
        self.assertTrue(all(messages[0]["status"] == 200 for messages in results))
        self.assertEqual(self.internal_paths.count("/v1/models"), 1)
        self.assertEqual(self.internal_paths.count("/tokenize"), 1)
        self.assertEqual(self.internal_paths.count("/health"), 9)

    def test_failed_or_timed_out_live_proof_is_not_cached(self):
        middleware = self.middleware()
        self.live_tokens = [1, 2, 3]
        failed = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(failed[0]["status"], 503)

        self.live_tokens = list(self.golden_tokens)
        recovered = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(recovered[0]["status"], 200)

        stalled = self.middleware(timeout_ms="100")
        self.stall_internal = True
        started = time.monotonic()
        timed_out = self.invoke(
            stalled,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(timed_out[0]["status"], 503)
        self.assertLess(time.monotonic() - started, 0.5)

    def test_internal_receive_stays_connected_until_route_completion(self):
        observed = []

        async def downstream(scope, receive, send):
            observed.append(scope["app"] is application)
            observed.append(scope["root_path"])
            first = await receive()
            observed.append(first["type"])
            disconnect = asyncio.create_task(receive())
            await asyncio.sleep(0)
            self.assertFalse(disconnect.done())
            await send({"type": "http.response.start", "status": 200})
            await send({"type": "http.response.body", "body": b"{}"})
            disconnect.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await disconnect

        application = object()
        status, body = asyncio.run(
            middleware_module._internal_request(
                downstream,
                {"app": application, "root_path": "/root"},
                b"Bearer token",
                "POST",
                "/tokenize",
                b"{}",
            )
        )
        self.assertEqual((status, body), (200, b"{}"))
        self.assertEqual(observed, [True, "/root", "http.request"])

    def test_internal_response_is_bounded(self):
        async def downstream(scope, receive, send):
            del scope, receive
            await send({"type": "http.response.start", "status": 200})
            await send({"type": "http.response.body", "body": b"12345"})

        async def scenario():
            with patch.object(
                middleware_module, "MAX_INTERNAL_RESPONSE_BYTES", 4
            ), self.assertRaisesRegex(ValueError, "internal response too large"):
                await middleware_module._internal_request(
                    downstream,
                    {"app": object()},
                    b"Bearer token",
                    "GET",
                    "/v1/models",
                )

        asyncio.run(scenario())

    def test_timeout_drives_decorated_route_disconnect_without_orphan_tasks(self):
        async def scenario():
            children = []

            async def downstream(scope, receive, send):
                del scope
                await receive()

                async def handler():
                    await asyncio.Event().wait()

                async def listen_for_disconnect():
                    while True:
                        if (await receive())["type"] == "http.disconnect":
                            return

                handler_task = asyncio.create_task(handler())
                cancellation_task = asyncio.create_task(listen_for_disconnect())
                children.extend([handler_task, cancellation_task])
                done, pending = await asyncio.wait(
                    [handler_task, cancellation_task],
                    return_when=asyncio.FIRST_COMPLETED,
                )
                for task in pending:
                    task.cancel()
                if handler_task in done:
                    handler_task.result()
                await send({"type": "http.response.start", "status": 200})
                await send({"type": "http.response.body", "body": b"null"})

            with self.assertRaises(TimeoutError):
                await asyncio.wait_for(
                    middleware_module._internal_request(
                        downstream,
                        {"app": object()},
                        b"Bearer token",
                        "POST",
                        "/tokenize",
                        b"{}",
                    ),
                    timeout=0.01,
                )
            await asyncio.sleep(0)
            self.assertTrue(children)
            self.assertTrue(all(task.done() for task in children))

        asyncio.run(scenario())


if __name__ == "__main__":
    unittest.main()
