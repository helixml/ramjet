import copy
import importlib.util
import pathlib
import shutil
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = (
    ROOT
    / "deploy"
    / "dspark_0731"
    / "validate-persistent-jit-cache-compose.py"
)
SPEC = importlib.util.spec_from_file_location("jit_cache_validator", VALIDATOR)
validator = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(validator)


@unittest.skipUnless(shutil.which("docker"), "Docker Compose is validated in CI")
class PersistentJitCacheComposeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.disabled = validator.render(enabled=False)
        cls.enabled = validator.render(enabled=True)

    def test_overlay_is_explicit_isolated_and_image_bound(self):
        validator.validate_source_bind_policy()
        validator.validate_disabled(self.disabled)
        validator.validate_enabled(self.enabled, self.disabled)

    def test_mutable_or_different_image_is_rejected(self):
        for image in (
            "voipmonitor/vllm:r34",
            "voipmonitor/vllm@sha256:" + "0" * 64,
        ):
            document = copy.deepcopy(self.enabled)
            document["services"][validator.ENGINES[0]]["image"] = image
            with self.subTest(image=image), self.assertRaisesRegex(
                validator.ValidationError, "immutable digest"
            ):
                validator.validate_enabled(document, self.disabled)

    def test_writer_paths_are_distinct_fingerprinted_and_precreated(self):
        document = copy.deepcopy(self.enabled)
        first = validator.volume_by_target(
            document["services"][validator.ENGINES[0]], validator.TARGET
        )
        second = validator.volume_by_target(
            document["services"][validator.ENGINES[1]], validator.TARGET
        )
        second["source"] = first["source"]
        with self.assertRaisesRegex(validator.ValidationError, "host path|share"):
            validator.validate_enabled(document, self.disabled)

        document = copy.deepcopy(self.enabled)
        mount = validator.volume_by_target(
            document["services"][validator.ENGINES[0]], validator.TARGET
        )
        mount["bind"]["create_host_path"] = True
        with self.assertRaisesRegex(validator.ValidationError, "may create"):
            validator.validate_enabled(document, self.disabled)

    def test_overlay_cannot_change_engine_or_load_balancer_settings(self):
        document = copy.deepcopy(self.enabled)
        document["services"][validator.ENGINES[0]]["environment"][
            "MAX_NUM_SEQS"
        ] = "32"
        with self.assertRaisesRegex(validator.ValidationError, "changes more"):
            validator.validate_enabled(document, self.disabled)

        document = copy.deepcopy(self.enabled)
        document["services"]["ds4-loadbalancer"]["environment"][
            "RJ_MAX_BODY_BYTES"
        ] = "1"
        with self.assertRaisesRegex(validator.ValidationError, "load balancer"):
            validator.validate_enabled(document, self.disabled)

    def test_runtime_cache_paths_share_one_exact_namespace(self):
        selected = validator.cache_environment(validator.runtime_manifest())
        self.assertGreaterEqual(len(selected), 12)
        prefix = f"{validator.TARGET}/{validator.FINGERPRINT}"
        self.assertTrue(
            all(value == prefix or value.startswith(f"{prefix}/") for value in selected.values())
        )

        document = validator.runtime_manifest()
        document["process"]["environment"][
            "LOCAL_INFERENCE_CACHE_FINGERPRINT"
        ] = "other"
        with self.assertRaisesRegex(validator.ValidationError, "fingerprint"):
            validator.cache_environment(document)


if __name__ == "__main__":
    unittest.main()
