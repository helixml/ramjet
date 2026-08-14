import copy
import json
import pathlib
import stat
import tempfile
import unittest

from serving_runtime_image_probe import (
    comparison_errors,
    generated_manifest,
    manifest_bytes,
    manifest_errors,
    safe_environment,
    validate_generation_template,
    write_manifest,
)


ROOT = pathlib.Path(__file__).resolve().parents[1]


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
                "AWS_ACCESS_KEY_ID": "private",
                "TLS_PRIVATE_KEY": "private",
                "INTERNAL_BEARER": "private",
                "MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_PATH": "/private",
            }
        )
        self.assertEqual(selected["MODE"], "test")
        self.assertNotIn("VLLM_API_KEY", selected)
        self.assertNotIn("HF_TOKEN", selected)
        self.assertNotIn("AWS_ACCESS_KEY_ID", selected)
        self.assertNotIn("TLS_PRIVATE_KEY", selected)
        self.assertNotIn("INTERNAL_BEARER", selected)
        self.assertFalse(any(key.startswith("MINI_DYNAMO_") for key in selected))
        self.assertIn("PATH", selected)
        with self.assertRaisesRegex(RuntimeError, "unreviewed"):
            safe_environment({"MODE": "test", "NEW_LAUNCH_SETTING": "1"})

    def test_generation_reproduces_the_committed_manifest_exactly(self):
        path = ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json"
        raw = path.read_bytes()
        template = json.loads(raw)
        captured = {
            key: copy.deepcopy(template["process"][key])
            for key in ("argv", "environment", "packages", "artifacts")
        }
        generated = generated_manifest(template, captured)
        self.assertEqual(manifest_bytes(generated), raw)

        captured["argv"] = list(captured["argv"])
        index = captured["argv"].index("--kv-events-config") + 1
        kv_events = json.loads(captured["argv"][index])
        kv_events["hwm"] += 1
        captured["argv"][index] = json.dumps(
            kv_events, sort_keys=True, separators=(",", ":")
        )
        changed = generated_manifest(template, captured)
        self.assertEqual(changed["engine"]["kv_events"]["hwm"], kv_events["hwm"])
        self.assertNotEqual(
            changed["process"]["argv_sha256"],
            template["process"]["argv_sha256"],
        )

    def test_generation_rejects_shape_drift_and_sensitive_argv(self):
        template = json.loads(
            (ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json").read_bytes()
        )
        captured = {
            key: copy.deepcopy(template["process"][key])
            for key in ("argv", "environment", "packages", "artifacts")
        }
        cases = []
        missing_environment = copy.deepcopy(captured)
        missing_environment["environment"].pop(next(iter(missing_environment["environment"])))
        cases.append(missing_environment)
        extra_package = copy.deepcopy(captured)
        extra_package["packages"]["unreviewed"] = "1"
        cases.append(extra_package)
        changed_artifact = copy.deepcopy(captured)
        changed_artifact["artifacts"][0]["path"] = "/other"
        cases.append(changed_artifact)
        sensitive = copy.deepcopy(captured)
        sensitive["argv"].append("--api-key=private")
        cases.append(sensitive)
        non_ascii = copy.deepcopy(captured)
        non_ascii["environment"]["MODE"] = "tést"
        cases.append(non_ascii)
        changed_kv_schema = copy.deepcopy(captured)
        index = changed_kv_schema["argv"].index("--kv-events-config") + 1
        kv_events = json.loads(changed_kv_schema["argv"][index])
        kv_events["unknown"] = True
        changed_kv_schema["argv"][index] = json.dumps(kv_events)
        cases.append(changed_kv_schema)

        for candidate in cases:
            with self.subTest(keys=sorted(candidate)), self.assertRaises(RuntimeError):
                generated_manifest(template, candidate)

        for pointer in ("schema_version", "compatibility_manifest_sha256", "engine"):
            malformed = copy.deepcopy(template)
            malformed.pop(pointer)
            with self.subTest(pointer=pointer), self.assertRaisesRegex(
                RuntimeError, "template"
            ):
                validate_generation_template(malformed)

    def test_generated_output_is_atomic_explicit_and_regular(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "runtime.json"
            write_manifest(output, b"first\n", replace=False)
            self.assertEqual(output.read_bytes(), b"first\n")
            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o644)
            with self.assertRaisesRegex(RuntimeError, "already exists"):
                write_manifest(output, b"second\n", replace=False)
            write_manifest(output, b"second\n", replace=True)
            self.assertEqual(output.read_bytes(), b"second\n")

            output.unlink()
            output.symlink_to("missing")
            with self.assertRaisesRegex(RuntimeError, "unsafe"):
                write_manifest(output, b"third\n", replace=True)


if __name__ == "__main__":
    unittest.main()
