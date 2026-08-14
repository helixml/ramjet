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
            "DS4_UPSTREAM_ADMISSION_MODE"
        ] = "compatibility"
        with self.assertRaisesRegex(validator.ValidationError, "may not opt"):
            validator.validate_enabled(document)

    def test_manifest_pin_and_mount_are_independent_gates(self):
        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        service["environment"]["MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SHA256"] = "0" * 64
        with self.assertRaisesRegex(validator.ValidationError, "manifest pin"):
            validator.validate_enabled(document)

        document = validator.render(enabled=True)
        mount = validator.volume_by_target(
            document["services"][validator.ENGINES[0]], validator.MANIFEST_TARGET
        )
        mount["source"] = "/tmp/untrusted-manifest"
        with self.assertRaisesRegex(validator.ValidationError, "source is unavailable"):
            validator.validate_enabled(document)

    def test_overlay_pins_the_image_and_gilded_import_path(self):
        document = validator.render(enabled=True)
        service = document["services"][validator.ENGINES[0]]
        self.assertEqual(service["image"], validator.ENGINE_IMAGE)
        middleware = validator.volume_by_target(service, validator.MIDDLEWARE_TARGET)
        self.assertEqual(
            middleware["target"],
            "/opt/vllm/vllm/mini_dynamo_engine_identity.py",
        )

        service["image"] = "voipmonitor/vllm:mutable"
        with self.assertRaisesRegex(validator.ValidationError, "not pinned"):
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


if __name__ == "__main__":
    unittest.main()
