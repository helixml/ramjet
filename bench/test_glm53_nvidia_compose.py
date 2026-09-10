"""The GLM-5.3-Flash NVFP4 deployment must keep the reviewed admission shape."""

import copy
import importlib.util
import pathlib
import shutil
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = ROOT / "deploy" / "glm53_flash_nvidia" / "validate-compose.py"
SPEC = importlib.util.spec_from_file_location("glm53_nvidia_compose", VALIDATOR)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load GLM-5.3-Flash NVFP4 Compose validator")
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


@unittest.skipUnless(
    shutil.which("docker"), "Docker Compose is validated in the deployment lane"
)
class Glm53NvidiaComposeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = validator.render()

    def changed(self):
        return copy.deepcopy(self.document)

    def test_canonical_render_passes(self):
        validator.validate(self.changed())

    def test_model_revision_drift_fails(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-b"]["labels"][
            "ai.ramjet.model.revision"
        ] = "main"
        with self.assertRaisesRegex(validator.ValidationError, "revision label"):
            validator.validate(changed)

    def test_mutable_engine_image_fails(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-a"]["image"] = "vllm/vllm-openai:latest"
        with self.assertRaisesRegex(validator.ValidationError, "immutable engine image"):
            validator.validate(changed)

    def test_cross_numa_gpu_placement_fails(self):
        changed = self.changed()
        devices = changed["services"]["glm53nvidia-b"]["deploy"]["resources"][
            "reservations"
        ]["devices"]
        devices[0]["device_ids"] = ["0", "1", "2", "3"]
        with self.assertRaisesRegex(validator.ValidationError, "GPU placement"):
            validator.validate(changed)

    def test_shared_jit_cache_fails(self):
        changed = self.changed()
        for mount in changed["services"]["glm53nvidia-b"]["volumes"]:
            if mount.get("target") == "/root/.cache":
                mount["source"] = "/prod/engine-cache-vllm-glm53nvidia-a"
        with self.assertRaisesRegex(validator.ValidationError, "private JIT cache"):
            validator.validate(changed)

    def test_writable_model_mount_fails(self):
        changed = self.changed()
        for mount in changed["services"]["glm53nvidia-a"]["volumes"]:
            if mount.get("target") == "/workspace/model":
                mount["read_only"] = False
        with self.assertRaisesRegex(validator.ValidationError, "immutable source"):
            validator.validate(changed)

    def test_publicly_published_engine_port_fails(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-a"]["ports"][0]["host_ip"] = "0.0.0.0"
        with self.assertRaisesRegex(validator.ValidationError, "loopback-only"):
            validator.validate(changed)

    def test_unqualified_speculation_fails(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-a"]["command"].append(
            '--speculative-config={"method":"mtp","num_speculative_tokens":3}'
        )
        with self.assertRaisesRegex(validator.ValidationError, "unqualified argument"):
            validator.validate(changed)

    def test_remote_code_fails(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-b"]["command"].append("--trust-remote-code")
        with self.assertRaisesRegex(validator.ValidationError, "unqualified argument"):
            validator.validate(changed)

    def test_dropping_prefix_caching_fails(self):
        changed = self.changed()
        command = changed["services"]["glm53nvidia-a"]["command"]
        command.remove("--enable-prefix-caching")
        with self.assertRaisesRegex(validator.ValidationError, "admitted argv"):
            validator.validate(changed)

    def test_admitting_images_fails(self):
        changed = self.changed()
        command = changed["services"]["glm53nvidia-a"]["command"]
        index = command.index('--limit-mm-per-prompt={"image":0,"video":0}')
        command[index] = '--limit-mm-per-prompt={"image":1,"video":0}'
        with self.assertRaisesRegex(validator.ValidationError, "admitted argv"):
            validator.validate(changed)

    def test_divergent_engine_profiles_fail(self):
        changed = self.changed()
        command = changed["services"]["glm53nvidia-b"]["command"]
        index = command.index("--gpu-memory-utilization=0.90")
        command[index] = "--gpu-memory-utilization=0.93"
        with self.assertRaisesRegex(validator.ValidationError, "admitted argv"):
            validator.validate(changed)

    def test_engine_argv_reordering_is_admitted_but_divergence_is_not(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-b"]["command"].reverse()
        with self.assertRaisesRegex(validator.ValidationError, "identical profile"):
            validator.validate(changed)

    def test_enabling_unqualified_ramjet_authority_fails(self):
        for key in validator.DISABLED_AUTHORITY:
            with self.subTest(key=key):
                changed = self.changed()
                changed["services"]["ds4-loadbalancer"]["environment"][key] = "shadow"
                with self.assertRaisesRegex(
                    validator.ValidationError, "unqualified ramjet authority"
                ):
                    validator.validate(changed)

    def test_missing_upstream_bearer_fails(self):
        changed = self.changed()
        changed["services"]["ds4-loadbalancer"]["environment"]["RJ_UPSTREAM_TOKEN"] = ""
        with self.assertRaisesRegex(validator.ValidationError, "authenticated engines"):
            validator.validate(changed)

    def test_upstream_set_drift_fails(self):
        changed = self.changed()
        changed["services"]["ds4-loadbalancer"]["environment"][
            "RJ_UPSTREAM"
        ] = "http://glm53nvidia-a:8000"
        with self.assertRaisesRegex(validator.ValidationError, "upstream set"):
            validator.validate(changed)

    def test_added_service_fails(self):
        changed = self.changed()
        changed["services"]["glm53nvidia-c"] = copy.deepcopy(
            changed["services"]["glm53nvidia-a"]
        )
        with self.assertRaisesRegex(validator.ValidationError, "service set"):
            validator.validate(changed)


class Glm53NvidiaComposeSourceTests(unittest.TestCase):
    """Shape assertions that do not need Docker."""

    def test_deployment_is_exactly_one_compose_file(self):
        directory = ROOT / "deploy" / "glm53_flash_nvidia"
        composes = sorted(
            path.name
            for path in directory.iterdir()
            if path.suffix in {".yaml", ".yml"} and "compose" in path.name
        )
        self.assertEqual(composes, ["docker-compose.yaml"])

    def test_no_overlay_files(self):
        directory = ROOT / "deploy" / "glm53_flash_nvidia"
        overlays = sorted(
            path.name for path in directory.iterdir() if "override" in path.name
        )
        self.assertEqual(overlays, [])


if __name__ == "__main__":
    unittest.main()
