import copy
import unittest

from serving_runtime_image_probe import (
    comparison_errors,
    manifest_errors,
    safe_environment,
)


class ServingRuntimeImageProbeTest(unittest.TestCase):
    def setUp(self):
        self.process = {
            "argv": ["serve", "model"],
            "environment": {"MODE": "test"},
            "packages": {"vllm": "v1"},
            "artifacts": [{"path": "/runtime", "sha256": "a" * 64}],
        }
        import serving_runtime_image_probe as probe

        self.process["argv_sha256"] = probe.nul_joined_sha256(
            self.process["argv"]
        )
        for name in ("environment", "packages", "artifacts"):
            self.process[f"{name}_sha256"] = probe.canonical_json_sha256(
                self.process[name]
            )

    def test_matching_capture_and_manifest_are_clean(self):
        self.assertEqual(manifest_errors({"process": self.process}), [])
        captured = {
            key: copy.deepcopy(self.process[key])
            for key in ("argv", "environment", "packages", "artifacts")
        }
        self.assertEqual(comparison_errors(self.process, captured), [])

    def test_mismatches_report_only_bounded_field_names(self):
        captured = {
            "argv": ["serve", "other"],
            "environment": {"MODE": "other"},
            "packages": {"vllm": "v2"},
            "artifacts": [{"path": "/runtime", "sha256": "b" * 64}],
        }
        self.assertEqual(
            comparison_errors(self.process, captured),
            ["argv", "environment.MODE", "packages.vllm", "artifacts.0"],
        )
        malformed = {"process": copy.deepcopy(self.process)}
        malformed["process"]["argv_sha256"] = "0" * 64
        self.assertEqual(manifest_errors(malformed), ["manifest.argv_sha256"])

        for process in (
            {"argv": [1]},
            {"argv": ["serve"], "environment": []},
            {"argv": ["serve"], "environment": {}, "packages": []},
        ):
            with self.subTest(process=process):
                self.assertEqual(
                    manifest_errors({"process": process}),
                    ["manifest.process"],
                )

    def test_sensitive_control_environment_is_never_forwarded(self):
        selected = safe_environment(
            {
                "MODE": "test",
                "VLLM_API_KEY": "private",
                "HF_TOKEN": "private",
                "MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH": "/private",
            }
        )
        self.assertEqual(selected["MODE"], "test")
        self.assertNotIn("VLLM_API_KEY", selected)
        self.assertNotIn("HF_TOKEN", selected)
        self.assertFalse(any(key.startswith("MINI_DYNAMO_") for key in selected))
        self.assertIn("PATH", selected)


if __name__ == "__main__":
    unittest.main()
