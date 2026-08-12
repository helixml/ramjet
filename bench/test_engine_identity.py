import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from engine_identity import argv_contract, verify, verify_receipt


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
            "image_id": "sha256:image",
            "repo_digests": [],
            "model_revision": "model-rev",
            "tokenizer_revision": "tokenizer-rev",
            "runtime_packages": {"vllm": "1.2.3", "torch": "4.5.6"},
            "command": "vllm serve model --api-key secret --max-num-seqs 16 --revision model-rev",
        }

    def test_argv_contract_returns_only_allow_list_and_stable_hash(self):
        contract, digest = argv_contract(self.live["command"])
        self.assertEqual(
            contract, {"max_num_seqs": "16", "revision": "model-rev"}
        )
        self.assertEqual(len(digest), 64)
        self.assertNotIn("secret", json.dumps(contract))
        _, other_secret_digest = argv_contract(
            self.live["command"].replace("secret", "another-secret")
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
            result["receipt"]["receipt_sha256"], hashlib.sha256(raw).hexdigest()
        )
        self.assertEqual(result["receipt"]["source_trees"]["vllm"], "vllm-tree")


if __name__ == "__main__":
    unittest.main()
