import copy
import hashlib
import json
import pathlib
import stat
import subprocess
import tempfile
import unittest
from unittest import mock

from serving_runtime_image_probe import (
    comparison_errors,
    container_process,
    engine_args_command,
    generated_manifest,
    manifest_bytes,
    manifest_errors,
    option_json,
    run_probe,
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
                "RAMJET_SERVING_IDENTITY_MANIFEST_PATH": "/private",
            }
        )
        self.assertEqual(selected["MODE"], "test")
        self.assertNotIn("VLLM_API_KEY", selected)
        self.assertNotIn("HF_TOKEN", selected)
        self.assertNotIn("AWS_ACCESS_KEY_ID", selected)
        self.assertNotIn("TLS_PRIVATE_KEY", selected)
        self.assertNotIn("INTERNAL_BEARER", selected)
        self.assertFalse(any(key.startswith("RAMJET_") for key in selected))
        self.assertNotIn("PATH", selected)
        with self.assertRaisesRegex(RuntimeError, "unreviewed"):
            safe_environment({"MODE": "test", "NEW_LAUNCH_SETTING": "1"})

    def test_candidate_launcher_environment_is_reviewed(self):
        selected = safe_environment(
            {
                "MODEL_PATH": "/workspace/model",
                "MODEL_REVISION": "revision",
                "TOKENIZER_REVISION": "revision",
                "DRAFT_SAMPLE_METHOD": "probabilistic",
                "REJECTION_SAMPLE_METHOD": "standard",
                "GRAPH": "96",
                "LOAD_FORMAT": "instanttensor",
                "INSTANTTENSOR_BACKEND": "BUFFERED",
                "LMCACHE_MODE": "off",
                "VLLM_API_KEY": "private",
            }
        )
        self.assertEqual(selected["LMCACHE_MODE"], "off")
        self.assertNotIn("VLLM_API_KEY", selected)

    def test_rendered_entrypoint_preserves_vendor_wrapper_chain(self):
        self.assertEqual(
            container_process(
                {
                    "entrypoint": [
                        "/usr/local/bin/lmcache-mp-wrapper.sh",
                        "/usr/local/bin/serve-ds4-flash.sh",
                    ],
                    "command": None,
                }
            ),
            (
                "/usr/local/bin/lmcache-mp-wrapper.sh",
                ["/usr/local/bin/serve-ds4-flash.sh"],
            ),
        )
        with self.assertRaisesRegex(RuntimeError, "command"):
            container_process({"entrypoint": ["/entry"], "command": "unsafe"})

    def test_engine_args_probe_is_hermetic_and_uses_exact_image(self):
        manifest = ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json"
        image = "example.invalid/vllm@sha256:" + "1" * 64
        command = engine_args_command(
            {
                "services": {
                    "engine": {
                        "image": image,
                        "environment": {"MODE": "dspark", "VLLM_API_KEY": "secret"},
                    }
                }
            },
            manifest,
            "engine",
        )
        self.assertIn("none", command)
        self.assertIn("never", command)
        self.assertEqual(command[command.index("--runtime") + 1], "runc")
        self.assertIn("--read-only", command)
        self.assertNotIn("--gpus", command)
        self.assertIn(image, command)
        self.assertIn(f"{manifest.resolve()}:/probe/runtime.json:ro", command)
        self.assertFalse(any("secret" in argument for argument in command))

    def test_runtime_probe_never_implicitly_pulls(self):
        image = "example.invalid/vllm@sha256:" + "1" * 64
        document = {
            "services": {
                "engine": {
                    "image": image,
                    "entrypoint": ["/launcher"],
                    "command": None,
                    "environment": {"MODE": "dspark"},
                }
            }
        }
        with tempfile.TemporaryDirectory() as directory:
            manifest = pathlib.Path(directory) / "runtime.json"
            manifest.write_text("{}", encoding="ascii")
            completed = subprocess.CompletedProcess([], 1, stdout=b"")
            with mock.patch(
                "serving_runtime_image_probe.subprocess.run",
                return_value=completed,
            ) as runner, self.assertRaisesRegex(RuntimeError, "probe failed"):
                run_probe(document, manifest, "engine", 1)
        command = runner.call_args.args[0]
        self.assertEqual(command[command.index("--pull") + 1], "never")
        self.assertEqual(command[command.index("--runtime") + 1], "runc")

    def test_infernal_template_captures_wrapper_and_exact_nccl_artifacts(self):
        template = json.loads(
            (
                ROOT
                / "deploy/dspark_0731/infernal-r11-candidate/serving-runtime.template.json"
            ).read_bytes()
        )
        validate_generation_template(template)
        self.assertEqual(len(template["process"]["environment"]), 216)
        self.assertEqual(
            [item["path"] for item in template["process"]["artifacts"]],
            [
                "/usr/local/bin/lmcache-mp-wrapper.sh",
                "/usr/local/bin/serve-ds4-flash.sh",
                "/opt/local-inference/nccl/lib/libnccl.so.2.31.2",
            ],
        )

    def test_infernal_receipt_pins_the_qualified_launch_contract(self):
        path = (
            ROOT
            / "deploy/dspark_0731/infernal-r11-candidate/serving-runtime.json"
        )
        raw = path.read_bytes()
        self.assertEqual(
            hashlib.sha256(raw).hexdigest(),
            "13bf4520cbd77b4d576c0246801f2e531d905049774f002bc2d095e7a1f4112d",
        )
        receipt = json.loads(raw)
        self.assertEqual(manifest_errors(receipt), [])
        process = receipt["process"]
        argv = process["argv"]
        self.assertEqual(argv[:2], ["serve", "/workspace/model"])

        def option(name):
            positions = [index for index, value in enumerate(argv) if value == name]
            self.assertEqual(positions, [positions[0]] if positions else [], name)
            self.assertEqual(len(positions), 1, name)
            self.assertLess(positions[0] + 1, len(argv), name)
            return argv[positions[0] + 1]

        expected = {
            "--revision": "9e165c30e2704aec5d9d593cce3eebd58bbef1cb",
            "--tokenizer-revision": "9e165c30e2704aec5d9d593cce3eebd58bbef1cb",
            "--tensor-parallel-size": "4",
            "--decode-context-parallel-size": "1",
            "--gpu-memory-utilization": "0.975",
            "--max-model-len": "393216",
            "--max-num-seqs": "16",
            "--max-num-batched-tokens": "4096",
            "--max-cudagraph-capture-size": "96",
            "--load-format": "instanttensor",
            "--attention-backend": "B12X_MLA_SPARSE",
            "--moe-backend": "b12x",
            "--linear-backend": "b12x",
        }
        for name, value in expected.items():
            self.assertEqual(option(name), value, name)
        for flag in (
            "--async-scheduling",
            "--enable-chunked-prefill",
            "--enable-prefix-caching",
            "--disable-custom-all-reduce",
            "--default-chat-template-kwargs.thinking=true",
            "--default-chat-template-kwargs.reasoning_effort=high",
        ):
            self.assertIn(flag, argv)
        self.assertFalse(any(value.startswith("--kv-offloading") for value in argv))
        self.assertEqual(
            option_json(argv, "--speculative-config"),
            {
                "model": "/workspace/model",
                "method": "dspark",
                "num_speculative_tokens": 5,
                "draft_sample_method": "probabilistic",
                "rejection_sample_method": "standard",
            },
        )
        self.assertEqual(
            option_json(argv, "--kv-events-config"), receipt["engine"]["kv_events"]
        )
        self.assertEqual(
            option_json(argv, "--override-generation-config"), {"top_p": 0.95}
        )
        self.assertEqual(
            process["packages"],
            {
                "b12x": "1.2.3",
                "flashinfer-python": "0.6.18+cu133",
                "instanttensor": "0.1.9",
                "lmcache": "0.5.2+glm52dcp.5",
                "torch": "2.13.0",
                "triton": "3.7.1+gitf797708c.nv26.7",
                "vllm": "0.26.1rc0+infernal.invocation.cu133.r11.vllm908522a.b12x5d648d9",
                "xgrammar": "0.2.5",
            },
        )
        environment = process["environment"]
        self.assertEqual(len(environment), 216)
        self.assertNotIn("_CUDA_COMPAT_STATUS", environment)
        self.assertEqual(environment["CUTLASS_DSL_VERSION"], "4.6.2")
        self.assertEqual(environment["LMCACHE_MODE"], "off")
        self.assertEqual(environment["VLLM_EXL3_ONLINE_CACHE_MODE"], "readwrite")
        self.assertEqual(
            [item["sha256"] for item in process["artifacts"]],
            [
                "4030cff01888c866636c858ae1ad4a57f67f5d1b0069cfc5e6341b7817522f2f",
                "ba7fabae482f54662269b48ff9b7df1be7d4f05cf91edb72a406e93e395acdc6",
                "d028ea782ce1798e6ad751d1e14f4b4516a8211a6289579a92f3bcde5e634a79",
            ],
        )

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
