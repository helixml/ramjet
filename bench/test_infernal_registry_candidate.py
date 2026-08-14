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
        raw = json.dumps({"config": {"digest": "sha256:" + "1" * 64}}).encode()
        image = json.dumps(
            {
                "os": "linux",
                "architecture": "amd64",
                "created": "now",
                "config": {"Entrypoint": ["entry"], "Labels": {"key": "value"}},
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
                "labels": {"key": "value"},
            },
        )


if __name__ == "__main__":
    unittest.main()
