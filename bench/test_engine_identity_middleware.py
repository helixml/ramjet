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
from types import SimpleNamespace
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


def exact_object(module, name, **attributes):
    kind = type(name, (), {})
    kind.__module__ = module
    value = kind()
    for key, attribute in attributes.items():
        setattr(value, key, attribute)
    return value


class FakeProcess(middleware_module.BaseProcess):
    def __init__(self, pid=4242, alive=True, exitcode=None):
        self.fake_pid = pid
        self.fake_alive = alive
        self.fake_exitcode = exitcode

    @property
    def pid(self):
        return self.fake_pid

    @property
    def exitcode(self):
        return self.fake_exitcode

    def is_alive(self):
        return self.fake_alive


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
        self.runtime_path = Path(self.temporary.name) / "runtime.json"
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
        self.runtime_incarnations = ("boot:4242:100",)
        self.runtime_probe_error = None
        self.runtime_probe_calls = 0
        self.application = SimpleNamespace(state=SimpleNamespace())

    def tearDown(self):
        self.temporary.cleanup()

    def runtime_manifest(self, compatibility_digest):
        argv = ["serve", "model"]
        environment = {
            "MAX_MODEL_LEN": "4096",
            "SERVED_MODEL_NAME": "model",
        }
        packages = {"vllm": "v1"}
        artifacts = [
            {
                "path": str(self.tokenizer_path),
                "sha256": hashlib.sha256(
                    self.tokenizer_path.read_bytes()
                ).hexdigest(),
            }
        ]
        return {
            "schema_version": 2,
            "compatibility_manifest_sha256": compatibility_digest,
            "engine": {
                "core_process_count": 1,
                "kv_events": {
                    "enable_kv_cache_events": True,
                    "publisher": "zmq",
                    "endpoint": "tcp://*:5557",
                    "replay_endpoint": "tcp://*:5558",
                    "buffer_steps": 10000,
                    "hwm": 100000,
                    "max_queue_size": 100000,
                    "topic": "",
                },
            },
            "process": {
                "argv": argv,
                "argv_sha256": hashlib.sha256(
                    b"\0".join(value.encode() for value in argv)
                ).hexdigest(),
                "environment": environment,
                "environment_sha256": hashlib.sha256(
                    json.dumps(
                        environment, sort_keys=True, separators=(",", ":")
                    ).encode()
                ).hexdigest(),
                "packages": packages,
                "packages_sha256": hashlib.sha256(
                    json.dumps(
                        packages, sort_keys=True, separators=(",", ":")
                    ).encode()
                ).hexdigest(),
                "artifacts": artifacts,
                "artifacts_sha256": hashlib.sha256(
                    json.dumps(
                        artifacts, sort_keys=True, separators=(",", ":")
                    ).encode()
                ).hexdigest(),
            },
        }

    @staticmethod
    def refresh_process_digests(runtime):
        process = runtime["process"]
        process["argv_sha256"] = hashlib.sha256(
            b"\0".join(value.encode() for value in process["argv"])
        ).hexdigest()
        for name in ("environment", "packages", "artifacts"):
            process[f"{name}_sha256"] = hashlib.sha256(
                json.dumps(
                    process[name], sort_keys=True, separators=(",", ":")
                ).encode()
            ).hexdigest()
        return runtime

    def live_scope(self, processes=None, kv_events=None):
        if processes is None:
            processes = [FakeProcess()]
        if kv_events is None:
            kv_events = self.runtime_manifest("0" * 64)["engine"]["kv_events"]
        config = exact_object(
            "vllm.config.kv_events",
            "KVEventsConfig",
            **kv_events,
        )
        manager = exact_object(
            "vllm.v1.engine.utils",
            "CoreEngineProcManager",
            processes=processes,
        )
        core = exact_object(
            "vllm.v1.engine.core_client",
            "AsyncMPClient",
            resources=SimpleNamespace(engine_manager=manager),
            vllm_config=SimpleNamespace(kv_events_config=config),
        )
        client = exact_object(
            "vllm.v1.engine.async_llm",
            "AsyncLLM",
            engine_core=core,
        )
        return {
            "app": SimpleNamespace(state=SimpleNamespace(engine_client=client))
        }

    @staticmethod
    def proc_table(*rows):
        return (
            "sl local_address rem_address st tx_queue rx_queue tr tm->when "
            "retrnsmt uid timeout inode\n"
            + "".join(rows)
        ).encode()

    @staticmethod
    def proc_row(address, port, inode, state="0A"):
        return (
            f"0: {address}:{port:04X} 00000000:0000 {state} "
            f"00000000:00000000 00:00000000 00000000 0 0 {inode}\n"
        )

    def middleware(
        self,
        manifest=None,
        digest=None,
        runtime=None,
        runtime_digest=None,
        token="secret-token",
        timeout_ms="4000",
    ):
        selected_manifest = self.manifest if manifest is None else manifest
        raw = json.dumps(selected_manifest).encode()
        self.manifest_path.write_bytes(raw)
        compatibility_digest = hashlib.sha256(raw).hexdigest()
        selected_runtime = (
            self.runtime_manifest(compatibility_digest)
            if runtime is None
            else runtime
        )
        runtime_raw = json.dumps(selected_runtime).encode()
        self.runtime_path.write_bytes(runtime_raw)
        environment = {
            "RAMJET_SERVING_IDENTITY_MANIFEST_PATH": str(self.manifest_path),
            "RAMJET_SERVING_IDENTITY_MANIFEST_SHA256": digest
            or compatibility_digest,
            "RAMJET_SERVING_RUNTIME_MANIFEST_PATH": str(self.runtime_path),
            "RAMJET_SERVING_RUNTIME_MANIFEST_SHA256": runtime_digest
            or hashlib.sha256(runtime_raw).hexdigest(),
            "VLLM_API_KEY": token,
            "SERVED_MODEL_NAME": "model",
            "MAX_MODEL_LEN": "4096",
            "RAMJET_SERVING_IDENTITY_TOKENIZER_PATH": str(
                self.tokenizer_path
            ),
            "RAMJET_SERVING_IDENTITY_VERIFY_TIMEOUT_MS": timeout_ms,
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
        ), patch.object(
            middleware_module,
            "_normalized_process_argv",
            return_value=["serve", "model"],
        ):
            return middleware_module.ServingIdentityMiddleware(downstream)

    def runtime_probe(self, scope, expected_engine):
        self.runtime_probe_calls += 1
        self.assertIs(scope["app"], self.application)
        if self.runtime_probe_error is not None:
            raise self.runtime_probe_error
        return self.runtime_incarnations, dict(expected_engine["kv_events"])

    def invoke(self, middleware, path, authorization=None, method="GET", raw_path=None):
        headers = [] if authorization is None else [(b"authorization", authorization)]
        messages = []
        scope = {
            "type": "http",
            "app": self.application,
            "method": method,
            "path": path,
            "raw_path": path.encode() if raw_path is None else raw_path,
            "headers": headers,
        }

        async def receive():
            return {"type": "http.request", "body": b"", "more_body": False}

        async def send(message):
            messages.append(message)

        with patch.object(
            middleware_module,
            "_live_engine_runtime",
            side_effect=self.runtime_probe,
        ):
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
        self.assertEqual(identity["schema_version"], 3)
        self.assertEqual(identity["model"], self.manifest["model"])
        self.assertNotIn("goldens", identity)
        self.assertRegex(
            identity["incarnation"]["frontend"],
            r"^[A-Za-z0-9._:-]{1,256}$",
        )
        self.assertEqual(
            identity["incarnation"]["engine_core"],
            list(self.runtime_incarnations),
        )
        self.assertEqual(identity["engine"]["core_process_count"], 1)
        self.assertEqual(
            identity["engine"]["kv_events"],
            self.runtime_manifest("0" * 64)["engine"]["kv_events"],
        )
        self.assertEqual(
            identity["runtime"],
            {
                key: value
                for key, value in self.runtime_manifest("0" * 64)[
                    "process"
                ].items()
                if key.endswith("_sha256")
            },
        )
        self.assertEqual(
            self.internal_paths,
            ["/health", "/v1/models", "/tokenize", "/health"],
        )
        self.assertEqual(self.downstream_calls, 0)
        self.assertEqual(self.runtime_probe_calls, 2)

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
        self.assertEqual(self.runtime_probe_calls, 3)

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
        self.assertEqual(self.runtime_probe_calls, 3)

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
            (middleware_module.IDENTITY_PATH, b"/v1/ramjet/%69dentity"),
        ]:
            messages = self.invoke(middleware, path, raw_path=raw_path)
            self.assertEqual(messages[0]["status"], 204)
        self.assertEqual(self.downstream_calls, 3)
        self.assertEqual(self.runtime_probe_calls, 0)

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

    def test_runtime_manifest_pin_link_schema_and_config_are_fail_closed(self):
        compatibility_raw = json.dumps(self.manifest).encode()
        compatibility_digest = hashlib.sha256(compatibility_raw).hexdigest()

        def runtime():
            return json.loads(
                json.dumps(self.runtime_manifest(compatibility_digest))
            )

        wrong_link = runtime()
        wrong_link["compatibility_manifest_sha256"] = "c" * 64
        wrong_schema = runtime()
        wrong_schema["schema_version"] = 1
        extra_root = runtime()
        extra_root["private-content"] = "must-not-leak"
        boolean_count = runtime()
        boolean_count["engine"]["core_process_count"] = True
        wrong_publisher = runtime()
        wrong_publisher["engine"]["kv_events"]["publisher"] = "other"
        non_wildcard = runtime()
        non_wildcard["engine"]["kv_events"]["endpoint"] = (
            "tcp://127.0.0.1:5557"
        )
        unicode_port = runtime()
        unicode_port["engine"]["kv_events"]["endpoint"] = "tcp://*:٥٥٥٧"
        duplicate_endpoint = runtime()
        duplicate_endpoint["engine"]["kv_events"]["replay_endpoint"] = (
            "tcp://*:5557"
        )
        boolean_capacity = runtime()
        boolean_capacity["engine"]["kv_events"]["hwm"] = True
        oversized_topic = runtime()
        oversized_topic["engine"]["kv_events"]["topic"] = "x" * 4097
        bad_argv_digest = runtime()
        bad_argv_digest["process"]["argv_sha256"] = "0" * 64
        sensitive_argv = runtime()
        sensitive_argv["process"]["argv"].extend(["--api-key", "private"])
        secret_environment = runtime()
        secret_environment["process"]["environment"]["PRIVATE_API_KEY"] = (
            "private"
        )
        duplicate_artifact = runtime()
        duplicate_artifact["process"]["artifacts"].append(
            dict(duplicate_artifact["process"]["artifacts"][0])
        )
        cases = [
            {"runtime_digest": "0" * 64},
            {"runtime": wrong_link},
            {"runtime": wrong_schema},
            {"runtime": extra_root},
            {"runtime": boolean_count},
            {"runtime": wrong_publisher},
            {"runtime": non_wildcard},
            {"runtime": unicode_port},
            {"runtime": duplicate_endpoint},
            {"runtime": boolean_capacity},
            {"runtime": oversized_topic},
            {"runtime": bad_argv_digest},
            {"runtime": sensitive_argv},
            {"runtime": secret_environment},
            {"runtime": duplicate_artifact},
        ]
        for arguments in cases:
            with self.subTest(arguments=sorted(arguments)):
                with self.assertRaisesRegex(
                    RuntimeError, "^serving identity initialization failed$"
                ) as raised:
                    self.middleware(**arguments)
                self.assertNotIn("private-content", str(raised.exception))
                self.assertNotIn(str(self.runtime_path), str(raised.exception))

    def test_process_argv_environment_packages_and_artifacts_are_live(self):
        raw = json.dumps(self.manifest).encode()
        compatibility_digest = hashlib.sha256(raw).hexdigest()
        cases = []

        argv = self.runtime_manifest(compatibility_digest)
        argv["process"]["argv"].append("--changed")
        cases.append(self.refresh_process_digests(argv))

        environment = self.runtime_manifest(compatibility_digest)
        environment["process"]["environment"]["MAX_MODEL_LEN"] = "8192"
        cases.append(self.refresh_process_digests(environment))

        packages = self.runtime_manifest(compatibility_digest)
        packages["process"]["packages"]["vllm"] = "v2"
        cases.append(self.refresh_process_digests(packages))

        artifacts = self.runtime_manifest(compatibility_digest)
        artifacts["process"]["artifacts"][0]["sha256"] = "f" * 64
        cases.append(self.refresh_process_digests(artifacts))

        for runtime in cases:
            with self.subTest(
                process={
                    key: runtime["process"][key]
                    for key in (
                        "argv_sha256",
                        "environment_sha256",
                        "packages_sha256",
                        "artifacts_sha256",
                    )
                }
            ), self.assertRaisesRegex(
                RuntimeError, "^serving identity initialization failed$"
            ):
                self.middleware(runtime=runtime)

    def test_process_argv_normalization_is_exact_and_bounded(self):
        with patch.object(
            middleware_module,
            "_read_bounded",
            return_value=(
                b"/opt/venv/bin/python\0/opt/venv/bin/vllm\0"
                b"serve\0model\0"
            ),
        ):
            self.assertEqual(
                middleware_module._normalized_process_argv(),
                ["serve", "model"],
            )

        for raw in (
            b"/opt/venv/bin/python\0/opt/venv/bin/vllm\0model\0",
            b"python\0vllm\0serve\0serve\0",
            b"python\0vllm\0serve\0model",
            b"python\0vllm\0serve\0\xff\0",
        ):
            with self.subTest(raw=raw), patch.object(
                middleware_module, "_read_bounded", return_value=raw
            ), self.assertRaises(ValueError):
                middleware_module._normalized_process_argv()

    def test_runtime_environment_secret_names_are_conservative(self):
        for key in (
            "PRIVATE_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "TLS_PRIVATE_KEY",
            "INTERNAL_BEARER",
            "DATABASE_PASSWORD",
        ):
            with self.subTest(key=key):
                self.assertTrue(
                    middleware_module._sensitive_runtime_environment_key(key)
                )
        self.assertFalse(
            middleware_module._sensitive_runtime_environment_key(
                "LOCAL_INFERENCE_CACHE_FINGERPRINT"
            )
        )

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
        compatibility_digest = hashlib.sha256(raw).hexdigest()
        runtime_raw = json.dumps(
            self.runtime_manifest(compatibility_digest)
        ).encode()
        self.runtime_path.write_bytes(runtime_raw)
        base = {
            "RAMJET_SERVING_IDENTITY_MANIFEST_PATH": str(self.manifest_path),
            "RAMJET_SERVING_IDENTITY_MANIFEST_SHA256": compatibility_digest,
            "RAMJET_SERVING_RUNTIME_MANIFEST_PATH": str(self.runtime_path),
            "RAMJET_SERVING_RUNTIME_MANIFEST_SHA256": hashlib.sha256(
                runtime_raw
            ).hexdigest(),
            "RAMJET_SERVING_IDENTITY_TOKENIZER_PATH": str(self.tokenizer_path),
            "VLLM_API_KEY": "token",
            "SERVED_MODEL_NAME": "model",
            "MAX_MODEL_LEN": "4096",
            "RAMJET_SERVING_IDENTITY_VERIFY_TIMEOUT_MS": "4000",
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
            ), patch.object(
                middleware_module,
                "_normalized_process_argv",
                return_value=["serve", "model"],
            ), self.assertRaisesRegex(
                RuntimeError, "^serving identity initialization failed$"
            ):
                middleware_module.ServingIdentityMiddleware(downstream)

        self.tokenizer_path.write_bytes(b"changed")
        with patch.dict(os.environ, base, clear=True), patch.object(
            middleware_module.importlib_metadata, "version", return_value="v1"
        ), patch.object(
            middleware_module,
            "_normalized_process_argv",
            return_value=["serve", "model"],
        ), self.assertRaisesRegex(
            RuntimeError, "^serving identity initialization failed$"
        ):
            middleware_module.ServingIdentityMiddleware(downstream)

    def test_live_engine_runtime_requires_exact_vllm_shape_and_config(self):
        expected = self.runtime_manifest("0" * 64)["engine"]
        scope = self.live_scope()
        with patch.object(
            middleware_module,
            "_inspect_core_process",
            return_value="boot:4242:100",
        ) as inspect:
            incarnations, live_config = middleware_module._live_engine_runtime(
                scope, expected
            )
        self.assertEqual(incarnations, ("boot:4242:100",))
        self.assertEqual(live_config, expected["kv_events"])
        process = (
            scope["app"]
            .state.engine_client.engine_core.resources.engine_manager.processes[0]
        )
        inspect.assert_called_once_with(process, {5557, 5558})

        malformed = self.live_scope()
        malformed["app"].state.engine_client = object()
        wrong_client_core = self.live_scope()
        wrong_client_core["app"].state.engine_client.engine_core = object()
        wrong_manager = self.live_scope()
        wrong_manager["app"].state.engine_client.engine_core.resources.engine_manager = (
            object()
        )
        wrong_config = self.live_scope()
        core_config = wrong_config["app"].state.engine_client.engine_core.vllm_config
        core_config.kv_events_config = object()
        for name, candidate in [
            ("client", malformed),
            ("core", wrong_client_core),
            ("manager", wrong_manager),
            ("config", wrong_config),
        ]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                middleware_module._live_engine_runtime(candidate, expected)

        for processes in ([], [FakeProcess(), FakeProcess(pid=4243)]):
            with self.subTest(core_count=len(processes)), self.assertRaisesRegex(
                ValueError, "process count mismatch"
            ):
                middleware_module._live_engine_runtime(
                    self.live_scope(processes=processes), expected
                )

        changed = dict(expected["kv_events"])
        changed["hwm"] += 1
        with self.assertRaisesRegex(ValueError, "live KV event config mismatch"):
            middleware_module._live_engine_runtime(
                self.live_scope(kv_events=changed), expected
            )

    def test_engine_core_process_must_be_live_stable_and_parented(self):
        expected_ports = {5557, 5558}
        for name, process in [
            ("dead", FakeProcess(alive=False)),
            ("exited", FakeProcess(exitcode=1)),
            ("invalid", object()),
        ]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                middleware_module._inspect_core_process(process, expected_ports)

        process = FakeProcess()
        with patch.object(
            middleware_module,
            "_proc_stat",
            return_value=("S", os.getpid() + 1, 100),
        ), self.assertRaisesRegex(ValueError, "process unavailable"):
            middleware_module._inspect_core_process(process, expected_ports)

        with patch.object(
            middleware_module,
            "_proc_stat",
            side_effect=[
                ("S", os.getpid(), 100),
                ("S", os.getpid(), 101),
            ],
        ), patch.object(
            middleware_module, "_verify_owned_listeners"
        ), self.assertRaisesRegex(
            ValueError, "process changed"
        ):
            middleware_module._inspect_core_process(process, expected_ports)

        with patch.object(
            middleware_module,
            "_proc_stat",
            side_effect=[
                ("S", os.getpid(), 100),
                ("S", os.getpid(), 100),
            ],
        ), patch.object(
            middleware_module, "_verify_owned_listeners"
        ) as listeners, patch.object(
            middleware_module, "_boot_id", return_value="boot"
        ):
            incarnation = middleware_module._inspect_core_process(
                process, expected_ports
            )
        self.assertEqual(incarnation, "boot:4242:100")
        listeners.assert_called_once_with(4242, expected_ports)

    def test_listener_evidence_requires_owned_wildcard_sockets(self):
        network_namespace = SimpleNamespace(st_dev=1, st_ino=2)
        descriptors = ["9", "11", "12"]
        links = {
            "/proc/4242/fd/9": "socket:[100]",
            "/proc/4242/fd/11": "socket:[101]",
        }

        def readlink(path):
            if path.endswith("/12"):
                raise FileNotFoundError(path)
            return links[path]

        def verify(tcp):
            tables = {
                "tcp": tcp,
                "tcp6": self.proc_table(),
            }
            with patch.object(
                middleware_module.os,
                "stat",
                return_value=network_namespace,
            ), patch.object(
                middleware_module.os,
                "listdir",
                return_value=descriptors,
            ), patch.object(
                middleware_module.os,
                "readlink",
                side_effect=readlink,
            ), patch.object(
                middleware_module,
                "_read_bounded",
                side_effect=lambda path, limit: tables[path.rsplit("/", 1)[-1]],
            ):
                middleware_module._verify_owned_listeners(4242, {5557, 5558})

        verify(
            self.proc_table(
                self.proc_row("00000000", 5557, "100"),
                self.proc_row("00000000", 5558, "101"),
            )
        )

        failures = {
            "missing": self.proc_table(
                self.proc_row("00000000", 5557, "100")
            ),
            "wrong-owner": self.proc_table(
                self.proc_row("00000000", 5557, "100"),
                self.proc_row("00000000", 5558, "999"),
            ),
            "wrong-address": self.proc_table(
                self.proc_row("00000000", 5557, "100"),
                self.proc_row("0100007F", 5558, "101"),
            ),
            "not-listening": self.proc_table(
                self.proc_row("00000000", 5557, "100"),
                self.proc_row("00000000", 5558, "101", state="01"),
            ),
        }
        for name, table in failures.items():
            with self.subTest(name=name), self.assertRaisesRegex(
                ValueError, "listener ownership mismatch"
            ):
                verify(table)

        with patch.object(
            middleware_module.os,
            "stat",
            side_effect=[
                SimpleNamespace(st_dev=1, st_ino=2),
                SimpleNamespace(st_dev=1, st_ino=3),
            ],
        ), self.assertRaisesRegex(ValueError, "network namespace mismatch"):
            middleware_module._verify_owned_listeners(4242, {5557, 5558})

    def test_listener_proc_evidence_is_bounded_and_malformed_tables_fail(self):
        network_namespace = SimpleNamespace(st_dev=1, st_ino=2)

        def invoke(table, descriptors=("9",)):
            with patch.object(
                middleware_module.os,
                "stat",
                return_value=network_namespace,
            ), patch.object(
                middleware_module.os,
                "listdir",
                return_value=list(descriptors),
            ), patch.object(
                middleware_module.os,
                "readlink",
                return_value="socket:[100]",
            ), patch.object(
                middleware_module,
                "_read_bounded",
                return_value=table,
            ):
                middleware_module._verify_owned_listeners(4242, {5557, 5558})

        malformed_tables = [
            b"header\nbad\n",
            self.proc_table(
                "0: 00000000:ZZZZ 00000000:0000 0A "
                "00000000:00000000 00:00000000 00000000 0 0 100\n"
            ),
            b"\xff",
        ]
        for table in malformed_tables:
            with self.subTest(table=table[:16]), self.assertRaises(
                (UnicodeDecodeError, ValueError)
            ):
                invoke(table)

        with patch.object(
            middleware_module.os,
            "stat",
            return_value=network_namespace,
        ), patch.object(
            middleware_module.os,
            "listdir",
            return_value=["not-a-number"],
        ), self.assertRaisesRegex(
            ValueError, "invalid engine core file descriptor"
        ):
            middleware_module._verify_owned_listeners(4242, {5557, 5558})

        with patch.object(
            middleware_module.os,
            "stat",
            return_value=network_namespace,
        ), patch.object(
            middleware_module.os,
            "listdir",
            return_value=["9"],
        ), patch.object(
            middleware_module.os,
            "readlink",
            return_value="socket:[100]",
        ), patch.object(
            middleware_module,
            "_read_bounded",
            side_effect=ValueError("runtime evidence is oversized"),
        ), self.assertRaisesRegex(ValueError, "runtime evidence is oversized"):
            middleware_module._verify_owned_listeners(4242, {5557, 5558})

        oversized = self.proc_table(
            self.proc_row("00000000", 5557, "100"),
            self.proc_row("00000000", 5558, "100"),
        )
        with patch.object(
            middleware_module, "MAX_CORE_FDS", 1
        ), self.assertRaisesRegex(ValueError, "invalid process network table"):
            invoke(oversized)

    def test_cached_runtime_probe_loss_or_replacement_fails_closed(self):
        middleware = self.middleware()
        first = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(first[0]["status"], 200)

        self.runtime_probe_error = ValueError("private-runtime-detail")
        unavailable = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(unavailable[0]["status"], 503)
        self.assertEqual(
            unavailable[1]["body"], b'{"error":"identity_unavailable"}'
        )
        self.assertNotIn(b"private-runtime-detail", unavailable[1]["body"])

        self.runtime_probe_error = None
        self.runtime_incarnations = ("boot:4242:101",)
        replaced = self.invoke(
            middleware,
            middleware_module.IDENTITY_PATH,
            authorization=b"Bearer secret-token",
        )
        self.assertEqual(replaced[0]["status"], 503)
        self.assertEqual(
            replaced[1]["body"], b'{"error":"identity_unavailable"}'
        )

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
                "app": self.application,
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

        with patch.object(
            middleware_module,
            "_live_engine_runtime",
            side_effect=self.runtime_probe,
        ):
            results = asyncio.run(scenario())
        self.assertTrue(all(messages[0]["status"] == 200 for messages in results))
        self.assertEqual(self.internal_paths.count("/v1/models"), 1)
        self.assertEqual(self.internal_paths.count("/tokenize"), 1)
        self.assertEqual(self.internal_paths.count("/health"), 9)
        self.assertEqual(self.runtime_probe_calls, 9)

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
