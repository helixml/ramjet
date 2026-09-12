"""The GLM-5.3-Flash NVFP4 checkpoint verifier must fail closed.

The real 190GiB checkpoint is not available in CI, so these build a
structurally exact tree with substitute digests and then break it one way at a
time. What is under test is the fail-closed logic, not the pinned constants.
"""

import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VERIFIER = ROOT / "deploy" / "glm53_flash_nvidia" / "verify-model.py"
SPEC = importlib.util.spec_from_file_location("glm53_nvidia_verify_model", VERIFIER)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load GLM-5.3-Flash NVFP4 checkpoint verifier")
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)


SHARDS = [
    f"model-{number:05d}-of-{verifier.EXPECTED_SHARD_COUNT:05d}.safetensors"
    for number in range(1, verifier.EXPECTED_SHARD_COUNT + 1)
]


class Glm53NvidiaModelVerifyTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name).resolve()

        index = {"weight_map": {f"tensor.{name}": name for name in SHARDS}}
        contents = {
            name: json.dumps({"stand_in": name}).encode()
            for name in verifier.EXPECTED_METADATA
        }
        contents["model.safetensors.index.json"] = json.dumps(index).encode()
        for name, payload in contents.items():
            (self.root / name).write_bytes(payload)
        for name in SHARDS:
            (self.root / name).write_bytes(b"\0")

        metadata = {
            name: hashlib.sha256(payload).hexdigest()
            for name, payload in contents.items()
        }
        self.patch("EXPECTED_METADATA", metadata)
        self.patch("EXPECTED_TENSOR_BYTES", len(SHARDS))

    def patch(self, name, value):
        original = getattr(verifier, name)
        setattr(verifier, name, value)
        self.addCleanup(setattr, verifier, name, original)

    def test_complete_checkpoint_verifies(self):
        verifier.verify(self.root)

    def test_relative_root_fails(self):
        with self.assertRaisesRegex(verifier.VerificationError, "absolute"):
            verifier.verify(pathlib.Path("models/glm"))

    def test_partial_download_fails(self):
        (self.root / "model-00007-of-00033.safetensors.incomplete").write_bytes(b"")
        with self.assertRaisesRegex(verifier.VerificationError, "incomplete"):
            verifier.verify(self.root)

    def test_missing_metadata_fails(self):
        (self.root / "chat_template.jinja").unlink()
        with self.assertRaisesRegex(verifier.VerificationError, "missing regular"):
            verifier.verify(self.root)

    def test_edited_chat_template_fails(self):
        (self.root / "chat_template.jinja").write_bytes(b"{# edited #}")
        with self.assertRaisesRegex(verifier.VerificationError, "digest mismatch"):
            verifier.verify(self.root)

    def test_missing_shard_fails(self):
        (self.root / SHARDS[-1]).unlink()
        with self.assertRaisesRegex(verifier.VerificationError, "of 33 shards"):
            verifier.verify(self.root)

    def test_truncated_shard_fails(self):
        (self.root / SHARDS[0]).write_bytes(b"")
        with self.assertRaisesRegex(verifier.VerificationError, "byte count mismatch"):
            verifier.verify(self.root)

    def test_unexpected_safetensors_file_fails(self):
        (self.root / "model-00001-of-00120.safetensors").write_bytes(b"\0")
        with self.assertRaisesRegex(verifier.VerificationError, "unexpected safetensors"):
            verifier.verify(self.root)

    def test_symlinked_shard_fails(self):
        target = self.root / SHARDS[0]
        target.unlink()
        target.symlink_to(self.root / SHARDS[1])
        with self.assertRaisesRegex(verifier.VerificationError, "not a regular file"):
            verifier.verify(self.root)

    def test_index_referencing_a_different_shard_set_fails(self):
        path = self.root / "model.safetensors.index.json"
        payload = json.dumps({"weight_map": {"tensor.0": SHARDS[0]}}).encode()
        path.write_bytes(payload)
        metadata = dict(verifier.EXPECTED_METADATA)
        metadata["model.safetensors.index.json"] = hashlib.sha256(payload).hexdigest()
        self.patch("EXPECTED_METADATA", metadata)
        with self.assertRaisesRegex(verifier.VerificationError, "exact 33-shard set"):
            verifier.verify(self.root)


class Glm53NvidiaModelPinTests(unittest.TestCase):
    """The pinned constants must stay internally consistent."""

    def test_shard_pattern_matches_the_expected_count(self):
        self.assertEqual(verifier.EXPECTED_SHARD_COUNT, 33)
        self.assertTrue(verifier.SHARD_PATTERN.fullmatch(SHARDS[0]))
        self.assertTrue(verifier.SHARD_PATTERN.fullmatch(SHARDS[-1]))
        self.assertIsNone(
            verifier.SHARD_PATTERN.fullmatch("model-00034-of-00034.safetensors")
        )

    def test_revision_is_an_immutable_object_id(self):
        self.assertRegex(verifier.MODEL_REVISION, r"\A[0-9a-f]{40}\Z")

    def test_every_pinned_digest_is_a_sha256(self):
        for name, value in verifier.EXPECTED_METADATA.items():
            with self.subTest(name=name):
                self.assertRegex(value, r"\A[0-9a-f]{64}\Z")


if __name__ == "__main__":
    unittest.main()
