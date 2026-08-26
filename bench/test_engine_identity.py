import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from engine_identity import (
    argv_contract,
    serving_argv_sha256,
    verify,
    verify_receipt,
)


class EngineIdentityTest(unittest.TestCase):
    def setUp(self):
        self.receipt = {
            "schema_version": 1,
            "status": "qualified",
            "image": "example/engine:r4",
            "image_id": "sha256:image",
            "registry_digest": "sha256:digest",
            "checkpoint": {
                "repository": "example/model",
                "revision": "model-rev",
                "tokenizer_revision": "tokenizer-rev",
            },
            "runtime_packages": {"vllm": "1.2.3", "torch": "4.5.6"},
            "vllm_tree": "vllm-tree",
            "b12x_tree": "b12x-tree",
            "source_composition": {
                "lmcache": {"tree": "lmcache-tree"},
                "flashinfer": "flashinfer-rev",
            },
        }
        self.live = {
            "configured_image": "example/engine:r4@sha256:digest",
            "image_id": "sha256:digest",
            "image_descriptor_digest": "sha256:digest",
            "image_config_digest": "sha256:image",
            "repo_digests": [],
            "model_revision": "model-rev",
            "tokenizer_revision": "tokenizer-rev",
            "runtime_packages": {"vllm": "1.2.3", "torch": "4.5.6"},
            "command": (
                "vllm serve model --max-num-seqs 16 "
                "--kv-cache-memory 40190174004 --revision model-rev"
            ),
        }

    def test_argv_contract_returns_only_allow_list_and_stable_hash(self):
        sensitive = self.live["command"].replace("model --", "model --api-key secret --")
        contract, digest = argv_contract(sensitive)
        self.assertEqual(
            contract,
            {
                "kv_cache_memory": "40190174004",
                "max_num_seqs": "16",
                "revision": "model-rev",
            },
        )
        self.assertEqual(len(digest), 64)
        self.assertNotIn("secret", json.dumps(contract))
        _, other_secret_digest = argv_contract(
            sensitive.replace("secret", "another-secret")
        )
        self.assertEqual(digest, other_secret_digest)

    def test_receipt_verification_reports_only_bounded_field_names(self):
        self.assertEqual(verify_receipt(self.live, self.receipt), [])
        self.live["runtime_packages"]["torch"] = "wrong-secret-like-value"
        self.live["model_revision"] = "wrong-model"
        self.assertEqual(
            verify_receipt(self.live, self.receipt),
            ["model_revision", "runtime_packages.torch"],
        )

    def test_serving_argv_hash_matches_runtime_receipt_boundary(self):
        expected = hashlib.sha256(
            b"serve\0model\0--max-num-seqs\0"
            b"16\0--kv-cache-memory\0"
            b"40190174004\0--revision\0model-rev"
        ).hexdigest()
        self.assertEqual(serving_argv_sha256(self.live["command"]), expected)
        prefixed = "42 /opt/venv/bin/" + self.live["command"]
        self.assertEqual(serving_argv_sha256(prefixed), expected)
        self.assertIsNone(serving_argv_sha256("42 python -m other serve model"))

    def test_serving_argv_hash_rejects_sensitive_options(self):
        with self.assertRaisesRegex(ValueError, "sensitive option"):
            serving_argv_sha256("vllm serve model --api-key secret")

    def test_old_docker_image_id_capture_remains_compatible(self):
        self.live.pop("image_config_digest")
        self.live["image_id"] = "sha256:image"
        self.assertEqual(verify_receipt(self.live, self.receipt), [])

    def test_manifest_and_config_digests_are_verified_independently(self):
        self.live["image_config_digest"] = "sha256:wrong-config"
        self.live["image_descriptor_digest"] = "sha256:wrong-manifest"
        self.live["configured_image"] = "example/engine:r4"
        self.assertEqual(
            verify_receipt(self.live, self.receipt),
            ["image_id", "registry_digest"],
        )

    def test_output_compacts_receipt_and_omits_raw_command(self):
        with tempfile.TemporaryDirectory() as directory:
            live_path = Path(directory) / "live.json"
            receipt_path = Path(directory) / "receipt.json"
            live_path.write_text(json.dumps(self.live))
            raw = json.dumps(self.receipt).encode()
            receipt_path.write_bytes(raw)
            result = verify(live_path, receipt_path)
        self.assertTrue(result["verified"])
        self.assertNotIn("command", result["live"])
        self.assertNotIn("secret", json.dumps(result))
        self.assertEqual(
            result["live"]["serving_argv_sha256"],
            serving_argv_sha256(self.live["command"]),
        )
        self.assertEqual(
            result["receipt"]["receipt_sha256"], hashlib.sha256(raw).hexdigest()
        )
        self.assertEqual(result["receipt"]["source_trees"]["vllm"], "vllm-tree")


if __name__ == "__main__":
    unittest.main()
