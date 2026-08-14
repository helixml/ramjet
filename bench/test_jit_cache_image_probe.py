import copy
import json
import pathlib
import unittest

from jit_cache_image_probe import evidence_errors, manifest_fingerprint


ROOT = pathlib.Path(__file__).resolve().parents[1]


class JitCacheImageProbeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.manifest = json.loads(
            (ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json").read_bytes()
        )
        cls.fingerprint = manifest_fingerprint(cls.manifest)

    def test_committed_manifest_has_one_bounded_cache_namespace(self):
        self.assertEqual(
            self.fingerprint,
            "vllme2666d9a65-b12x7cecbb2c48-136ce64f2c43f0f8",
        )

        changed = copy.deepcopy(self.manifest)
        changed["process"]["environment"]["TRITON_CACHE_DIR"] = "/tmp/triton"
        with self.assertRaisesRegex(RuntimeError, "namespace"):
            manifest_fingerprint(changed)

    def test_baked_cache_evidence_must_be_empty_and_link_free(self):
        evidence = {
            "fingerprint": self.fingerprint,
            "fingerprint_root": True,
            "directories": 26,
            "files": 1,
            "zero_byte_files": 1,
            "symlinks": 0,
            "other": 0,
            "file_bytes": 0,
        }
        self.assertEqual(evidence_errors(evidence, self.fingerprint), [])

        for field, value in (
            ("fingerprint", "wrong"),
            ("fingerprint_root", False),
            ("files", 2),
            ("zero_byte_files", 0),
            ("symlinks", 1),
            ("other", 1),
            ("file_bytes", 1),
            ("directories", 0),
        ):
            candidate = dict(evidence)
            candidate[field] = value
            with self.subTest(field=field):
                self.assertEqual(
                    evidence_errors(candidate, self.fingerprint),
                    [f"evidence.{field}"],
                )


if __name__ == "__main__":
    unittest.main()
