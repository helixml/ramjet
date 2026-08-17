import copy
import importlib.util
import pathlib
import shutil
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = (
    ROOT / "deploy" / "dspark_0731" / "validate-serving-identity-compose.py"
)
SPEC = importlib.util.spec_from_file_location("serving_identity_validator", VALIDATOR)
validator = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(validator)


@unittest.skipUnless(shutil.which("docker"), "Docker Compose is validated in deployment CI")
class ServingIdentityComposeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.disabled = validator.render(enabled=False)
        cls.enabled = validator.render(enabled=True)

    def test_overlay_is_explicit_and_production_shaped(self):
        validator.validate_source_bind_policy()
        validator.validate_disabled(self.disabled)
        validator.validate_enabled(self.enabled)

    def test_overlay_cannot_enable_load_balancer_admission(self):
        document = validator.render(enabled=True)
        document["services"]["ds4-loadbalancer"]["environment"][
            "RJ_UPSTREAM_ADMISSION_MODE"
        ] = "compatibility"
        with self.assertRaisesRegex(validator.ValidationError, "may not opt"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        document["services"]["ds4-loadbalancer"]["environment"][
            "RJ_DSPARK_GUARD_MODE"
        ] = "quarantine"
        with self.assertRaisesRegex(validator.ValidationError, "DSpark enforcement"):
            validator.validate_enabled(document)

    def test_manifest_pin_and_mount_are_independent_gates(self):
        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        service["environment"]["RAMJET_SERVING_IDENTITY_MANIFEST_SHA256"] = "0" * 64
        with self.assertRaisesRegex(validator.ValidationError, "manifest pin"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        mount = validator.volume_by_target(
            document["services"][validator.ENGINES[0]], validator.MANIFEST_TARGET
        )
        mount["source"] = "/tmp/untrusted-manifest"
        with self.assertRaisesRegex(validator.ValidationError, "source is unavailable"):
            validator.validate_enabled(document)

    def test_serving_runtime_pin_paths_and_mounts_are_independent_gates(self):
        document = validator.render(enabled=True)
        engine = document["services"][validator.ENGINES[0]]
        engine["environment"][
            "RAMJET_SERVING_RUNTIME_MANIFEST_SHA256"
        ] = "0" * 64
        with self.assertRaisesRegex(validator.ValidationError, "runtime pin"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        load_balancer = document["services"]["ds4-loadbalancer"]
        load_balancer["environment"]["RJ_SERVING_RUNTIME_MANIFEST_PATH"] = (
            "/compat/wrong.json"
        )
        with self.assertRaisesRegex(validator.ValidationError, "runtime target"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        load_balancer = document["services"]["ds4-loadbalancer"]
        load_balancer["environment"]["RJ_SERVING_RUNTIME_MANIFEST_SHA256"] = (
            "0" * 64
        )
        with self.assertRaisesRegex(validator.ValidationError, "runtime pin"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        mount = validator.volume_by_target(
            document["services"]["ds4-loadbalancer"],
            validator.LB_RUNTIME_MANIFEST_TARGET,
        )
        mount["source"] = "/tmp/untrusted-serving-runtime"
        with self.assertRaisesRegex(validator.ValidationError, "source is unavailable"):
            validator.validate_enabled(document)

    def test_runtime_manifest_is_linked_to_unchanged_renderer_manifest(self):
        self.assertEqual(
            validator.MANIFEST_SHA256,
            "4ae2503554fa7089bc455e2ee89af0677c5cabec523d6b08d91a93d9ec9259aa",
        )
        runtime = validator.validate_runtime_manifest()
        self.assertEqual(
            runtime["compatibility_manifest_sha256"], validator.MANIFEST_SHA256
        )
        self.assertNotEqual(
            validator.RUNTIME_MANIFEST_SHA256, validator.MANIFEST_SHA256
        )

        mismatched = copy.deepcopy(runtime)
        mismatched["compatibility_manifest_sha256"] = "0" * 64
        with self.assertRaisesRegex(validator.ValidationError, "compatibility link"):
            validator.validate_enabled(validator.render(enabled=True), mismatched)

    def test_runtime_manifest_is_the_kv_publisher_authority(self):
        runtime = copy.deepcopy(validator.validate_runtime_manifest())
        runtime["engine"]["kv_events"]["hwm"] += 1
        with self.assertRaisesRegex(validator.ValidationError, "diverges"):
            validator.validate_enabled(validator.render(enabled=True), runtime)

        malformed = copy.deepcopy(validator.validate_runtime_manifest())
        malformed["engine"]["core_process_count"] = True
        with self.assertRaisesRegex(validator.ValidationError, "core process count"):
            validator.validate_enabled(validator.render(enabled=True), malformed)

        unicode_port = copy.deepcopy(validator.validate_runtime_manifest())
        unicode_port["engine"]["kv_events"]["endpoint"] = "tcp://*:٥٥٥٧"
        with self.assertRaisesRegex(validator.ValidationError, "port is invalid"):
            validator.validate_enabled(validator.render(enabled=True), unicode_port)

    def test_runtime_manifest_binds_launch_environment_packages_and_artifacts(self):
        runtime = validator.validate_runtime_manifest()
        self.assertEqual(runtime["schema_version"], 2)
        self.assertEqual(runtime["process"]["argv"][0], "serve")
        self.assertEqual(
            runtime["process"]["environment"]["VLLM_USE_B12X_FP8_GEMM"],
            "1",
        )
        for name in (
            "argv_sha256",
            "environment_sha256",
            "packages_sha256",
            "artifacts_sha256",
        ):
            malformed = copy.deepcopy(runtime)
            malformed["process"][name] = "0" * 64
            with self.subTest(name=name), self.assertRaisesRegex(
                validator.ValidationError, "digest"
            ):
                validator.validate_runtime_manifest(malformed)

        for key in ("PRIVATE_API_KEY", "AWS_ACCESS_KEY_ID", "TLS_PRIVATE_KEY"):
            secret = copy.deepcopy(runtime)
            secret["process"]["environment"][key] = "private"
            with self.subTest(key=key), self.assertRaisesRegex(
                validator.ValidationError, "mapping key"
            ):
                validator.validate_runtime_manifest(secret)

    def test_lb_and_engine_runtime_authorities_cannot_be_cross_wired(self):
        document = validator.render(enabled=True)
        document["services"]["ds4-loadbalancer"]["environment"][
            "RAMJET_SERVING_RUNTIME_MANIFEST_PATH"
        ] = validator.LB_RUNTIME_MANIFEST_TARGET
        with self.assertRaisesRegex(validator.ValidationError, "engine serving"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        document["services"][validator.ENGINES[0]]["environment"][
            "RJ_SERVING_RUNTIME_MANIFEST_PATH"
        ] = validator.ENGINE_RUNTIME_MANIFEST_TARGET
        with self.assertRaisesRegex(validator.ValidationError, "load balancer serving"):
            validator.validate_enabled(document)

    def test_base_compose_has_no_serving_runtime_authority(self):
        document = validator.render(enabled=False)
        document["services"]["ds4-loadbalancer"]["environment"][
            "RJ_SERVING_RUNTIME_MANIFEST_SHA256"
        ] = validator.RUNTIME_MANIFEST_SHA256
        with self.assertRaisesRegex(validator.ValidationError, "active in the base"):
            validator.validate_disabled(document)

    def test_overlay_pins_the_image_and_gilded_import_path(self):
        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        self.assertEqual(service["image"], validator.ENGINE_IMAGE)
        middleware = validator.volume_by_target(service, validator.MIDDLEWARE_TARGET)
        self.assertEqual(
            middleware["target"],
            "/opt/venv/lib/python3.12/site-packages/mini_dynamo_engine_identity.py",
        )

        service["image"] = "voipmonitor/vllm:mutable"
        with self.assertRaisesRegex(validator.ValidationError, "not pinned"):
            validator.validate_enabled(document)

    def test_live_verification_timeout_is_fixed_and_bounded_below_lb_timeout(self):
        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        service["environment"][
            "RAMJET_SERVING_IDENTITY_VERIFY_TIMEOUT_MS"
        ] = "5000"
        with self.assertRaisesRegex(validator.ValidationError, "timeout"):
            validator.validate_enabled(document)

    def test_qualified_engine_arguments_cannot_be_dropped(self):
        for argument, message in [
            ("--middleware", "middleware"),
            ("--kv-events-config", "KV publisher"),
            ("--override-generation-config", "sampling floor"),
        ]:
            document = validator.render(enabled=True)
            service = document["services"][validator.ENGINES[0]]
            arguments = service["environment"]["EXTRA_VLLM_ARGS"]
            value = validator.option_value(arguments, argument, message)
            service["environment"]["EXTRA_VLLM_ARGS"] = arguments.replace(
                f"{argument} {value}", "", 1
            )
            with self.assertRaisesRegex(validator.ValidationError, message):
                validator.validate_enabled(document)

    def test_qualified_engine_arguments_cannot_be_changed_or_duplicated(self):
        mutations = [
            (
                "--kv-events-config",
                '{"enable_kv_cache_events":false}',
                "KV publisher",
            ),
            ("--override-generation-config", '{"top_p":1.0}', "sampling floor"),
        ]
        for option, replacement, message in mutations:
            document = validator.render(enabled=True)
            service = document["services"][validator.ENGINES[0]]
            arguments = service["environment"]["EXTRA_VLLM_ARGS"]
            current = validator.option_value(arguments, option, message)
            service["environment"]["EXTRA_VLLM_ARGS"] = arguments.replace(
                f"{option} {current}", f"{option} {replacement}", 1
            )
            with self.assertRaisesRegex(validator.ValidationError, message):
                validator.validate_enabled(document)

        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        service["environment"]["EXTRA_VLLM_ARGS"] += " --kv-events-config={}"
        with self.assertRaisesRegex(validator.ValidationError, "cardinality"):
            validator.validate_enabled(document)

    def test_compose_cannot_assert_ignored_or_launcher_overwritten_settings(self):
        for key, value, message in [
            ("GPU_MEM_UTIL", "0.90", "ignored GPU memory"),
            (
                "VLLM_USE_B12X_FP8_GEMM",
                "0",
                "launcher-overwritten FP8 GEMM",
            ),
        ]:
            document = validator.render(enabled=True)
            service = document["services"][validator.ENGINES[0]]
            service["environment"][key] = value
            with self.subTest(key=key), self.assertRaisesRegex(
                validator.ValidationError, message
            ):
                validator.validate_enabled(document)

        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        service["environment"]["GPU_MEMORY_UTILIZATION"] = "0.90"
        with self.assertRaisesRegex(
            validator.ValidationError, "serving process environment"
        ):
            validator.validate_enabled(document)


if __name__ == "__main__":
    unittest.main()
