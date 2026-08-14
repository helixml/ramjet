import copy
import hashlib
import json
import unittest

import infernal_registry_candidate as candidate


class InfernalRegistryCandidateTest(unittest.TestCase):
    def test_committed_manifest_and_overlay_are_immutable_and_isolated(self):
        manifest = candidate.load_manifest()
        image = manifest["candidate_image"]
        self.assertEqual(
            image["image_digest"],
            "sha256:01b973d1ae132882bcc1bf62ea232f6aabe649dd4a89b961d81f3c41cc53f971",
        )
        self.assertEqual(
            image["config_digest"],
            "sha256:f226a6fd788bb4af345a17b768654f1e5a7487a812746ccb117aa9b040a82294",
        )
        overlay = (
            candidate.REPO_ROOT
            / "deploy/dspark_0731/infernal-r11-candidate/docker-compose.overlay.yaml"
        ).read_text()
        self.assertIn(image["image"], overlay)
        self.assertIn("dspark-0731-b:", overlay)
        self.assertNotIn("dspark-0731:\n", overlay)
        self.assertIn("infernal-invocation-cu133-r11", overlay)
        self.assertNotIn("infernal-invocation-cu133-r4", overlay)

    def test_declared_delta_keeps_native_stack_constants_honest(self):
        manifest = candidate.load_manifest()
        current = manifest["candidate_image"]["labels"]
        baseline = manifest["baseline_image"]["labels"]
        for label in manifest["unchanged_labels"]:
            self.assertEqual(current[label], baseline[label], label)
        for label in manifest["changed_labels"]:
            self.assertNotEqual(current[label], baseline[label], label)
        self.assertIn(
            "local-inference.lmcache.integration.tree", manifest["changed_labels"]
        )
        comparison = manifest["comparison"]
        self.assertIn(
            "CUTLASS_DSL_VERSION", comparison["environment_delta"]["changed"]
        )
        self.assertEqual(
            comparison["environment_delta"]["added"],
            [
                "VLLM_EXL3_ENCODER_REVISION",
                "VLLM_EXL3_ENCODER_SOURCE",
                "VLLM_EXL3_EXT_PATH",
                "VLLM_EXL3_ONLINE_CACHE_DIR",
                "VLLM_EXL3_ONLINE_CACHE_MODE",
            ],
        )
        blobs = comparison["layer_blobs"]
        self.assertEqual(blobs["baseline_descriptor_count"], 95)
        self.assertEqual(blobs["baseline_unique_blob_count"], 78)
        self.assertEqual(blobs["candidate_descriptor_count"], 96)
        self.assertEqual(blobs["candidate_unique_blob_count"], 79)

    def test_registry_contract_accepts_exact_observation(self):
        manifest = candidate.load_manifest()
        expected = manifest["candidate_image"]
        observed = {
            "manifest_digest": expected["image_digest"],
            "config_digest": expected["config_digest"],
            "platform": expected["platform"],
            "created": expected["created"],
            "entrypoint": expected["entrypoint"],
            "labels": expected["labels"],
        }
        candidate.validate_registry(expected, observed)

    def test_registry_contract_rejects_one_changed_label(self):
        manifest = candidate.load_manifest()
        expected = manifest["candidate_image"]
        observed = {
            "manifest_digest": expected["image_digest"],
            "config_digest": expected["config_digest"],
            "platform": expected["platform"],
            "created": expected["created"],
            "entrypoint": expected["entrypoint"],
            "labels": dict(expected["labels"]),
        }
        observed["labels"]["local-inference.vllm.integration.tree"] = "changed"
        with self.assertRaisesRegex(candidate.CandidateError, "registry_mismatch"):
            candidate.validate_registry(expected, observed)

    def test_manifest_rejects_false_unchanged_claim(self):
        manifest = copy.deepcopy(candidate.load_manifest())
        label = manifest["unchanged_labels"][0]
        manifest["candidate_image"]["labels"][label] = "changed"
        with self.assertRaisesRegex(candidate.CandidateError, "invalid_manifest"):
            candidate.validate_manifest(manifest)

    def test_inspect_parses_parallel_registry_reads(self):
        layer_digest = "sha256:" + "3" * 64
        raw = json.dumps(
            {
                "config": {"digest": "sha256:" + "1" * 64},
                "layers": [
                    {"digest": layer_digest, "size": 123},
                    {"digest": layer_digest, "size": 123},
                ],
            }
        ).encode()
        image = json.dumps(
            {
                "os": "linux",
                "architecture": "amd64",
                "created": "now",
                "config": {
                    "Entrypoint": ["entry"],
                    "Env": ["A=one=two", "B=three"],
                    "ExposedPorts": {"8000/tcp": {}},
                    "Labels": {
                        "key": "value",
                        "local-inference.example": "pinned",
                    },
                    "WorkingDir": "/workspace",
                },
            }
        ).encode()

        def runner(argv):
            return raw if "--raw" in argv else image

        self.assertEqual(
            candidate.inspect_image("example.invalid/image@sha256:" + "2" * 64, runner),
            {
                "manifest_digest": "sha256:" + hashlib.sha256(raw).hexdigest(),
                "config_digest": "sha256:" + "1" * 64,
                "platform": "linux/amd64",
                "created": "now",
                "entrypoint": ["entry"],
                "labels": {
                    "key": "value",
                    "local-inference.example": "pinned",
                },
                "local_inference_labels": {
                    "local-inference.example": "pinned"
                },
                "environment": {"A": "one=two", "B": "three"},
                "config_fields": {
                    "Entrypoint": ["entry"],
                    "ExposedPorts": {"8000/tcp": {}},
                    "WorkingDir": "/workspace",
                },
                "layer_descriptor_count": 2,
                "layer_blobs": {layer_digest: 123},
            },
        )

    def test_comparison_covers_complete_effective_config(self):
        baseline = {
            "environment": {"CHANGED": "old", "REMOVED": "x", "SAME": "x"},
            "local_inference_labels": {
                "local-inference.changed": "old",
                "local-inference.same": "x",
            },
            "config_fields": {"Entrypoint": ["same"]},
            "layer_descriptor_count": 3,
            "layer_blobs": {
                "sha256:" + "1" * 64: 10,
                "sha256:" + "2" * 64: 20,
            },
        }
        observed = {
            "baseline": baseline,
            "candidate": {
                "environment": {"ADDED": "x", "CHANGED": "new", "SAME": "x"},
                "local_inference_labels": {
                    "local-inference.added": "x",
                    "local-inference.changed": "new",
                    "local-inference.same": "x",
                },
                "config_fields": {"Entrypoint": ["same"]},
                "layer_descriptor_count": 2,
                "layer_blobs": {
                    "sha256:" + "1" * 64: 10,
                    "sha256:" + "3" * 64: 30,
                },
            },
        }
        expected = candidate.comparison(baseline, observed["candidate"])
        candidate.validate_comparison(expected, observed)
        self.assertEqual(
            expected["environment_delta"],
            {
                "added": ["ADDED"],
                "removed": ["REMOVED"],
                "changed": ["CHANGED"],
                "unchanged_count": 1,
            },
        )
        self.assertEqual(expected["layer_blobs"]["shared_compressed_bytes"], 10)

    def test_comparison_rejects_each_unreviewed_delta_domain(self):
        baseline = {
            "environment": {"A": "old"},
            "local_inference_labels": {"local-inference.a": "old"},
            "config_fields": {"Entrypoint": ["old"]},
            "layer_descriptor_count": 1,
            "layer_blobs": {"sha256:" + "1" * 64: 10},
        }
        clean_candidate = copy.deepcopy(baseline)
        expected = candidate.comparison(baseline, clean_candidate)
        mutations = {
            "environment_delta": lambda value: value["environment"].update(
                {"UNREVIEWED": "yes"}
            ),
            "local_inference_label_delta": lambda value: value[
                "local_inference_labels"
            ].update({"local-inference.unreviewed": "yes"}),
            "config_field_delta": lambda value: value["config_fields"].update(
                {"Cmd": ["unreviewed"]}
            ),
            "layer_blobs": lambda value: (
                value["layer_blobs"].update({"sha256:" + "2" * 64: 20}),
                value.update({"layer_descriptor_count": 2}),
            ),
        }
        for field, mutate in mutations.items():
            with self.subTest(field=field):
                changed = copy.deepcopy(clean_candidate)
                mutate(changed)
                with self.assertRaisesRegex(
                    candidate.CandidateError, "registry_comparison_mismatch"
                ) as caught:
                    candidate.validate_comparison(
                        expected, {"baseline": baseline, "candidate": changed}
                    )
                self.assertEqual(caught.exception.field, field)

    def test_manifest_rejects_impossible_blob_arithmetic(self):
        manifest = copy.deepcopy(candidate.load_manifest())
        manifest["comparison"]["layer_blobs"]["candidate_only_blob_count"] += 1
        with self.assertRaisesRegex(candidate.CandidateError, "invalid_manifest"):
            candidate.validate_manifest(manifest)

    def test_comparison_rejects_impossible_shared_blob_size(self):
        digest = "sha256:" + "4" * 64
        baseline = {
            "environment": {},
            "local_inference_labels": {},
            "config_fields": {},
            "layer_descriptor_count": 1,
            "layer_blobs": {digest: 10},
        }
        changed = copy.deepcopy(baseline)
        changed["layer_blobs"][digest] = 11
        with self.assertRaisesRegex(candidate.CandidateError, "invalid_registry"):
            candidate.comparison(baseline, changed)


if __name__ == "__main__":
    unittest.main()
