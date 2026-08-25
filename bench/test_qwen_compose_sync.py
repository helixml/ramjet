import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "deploy" / "qwen38_27b"
SYNC = SOURCE / "sync-compose.sh"
MODEL_REPOSITORY = "RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead"
MODEL_REVISION = "009632fef96dd349150baa780c984e62e70e91fe"


class QwenComposeContractTests(unittest.TestCase):
    def test_target_checkpoint_is_revision_named_and_labelled(self):
        compose = (SOURCE / "docker-compose.yaml").read_text()
        self.assertIn(f"ai.ramjet.model.repository: {MODEL_REPOSITORY}", compose)
        self.assertIn(f"ai.ramjet.model.revision: {MODEL_REVISION}", compose)
        self.assertIn(
            "/prod/models/RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead-009632fef96d",
            compose,
        )
        self.assertNotIn("/prod/models/Inferact/Qwen3.8-27B-NVFP4", compose)


class QwenComposeSyncTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.infra = pathlib.Path(self.temporary.name)
        subprocess.run(["git", "init", "-q", str(self.infra)], check=True)
        self.target = self.infra / "node06" / "inference" / "qwen38_27b"
        self.target.mkdir(parents=True)
        (self.target / "README.md").write_text("infra-owned provenance\n")

    def tearDown(self):
        self.temporary.cleanup()

    def run_sync(self, *arguments, check=True):
        return subprocess.run(
            [str(SYNC), *arguments, str(self.infra)],
            check=check,
            capture_output=True,
            text=True,
        )

    def test_sync_copies_every_compose_artifact_and_preserves_readme(self):
        self.run_sync()

        source_files = sorted(SOURCE.glob("*.yaml"))
        self.assertTrue(source_files)
        for source_file in source_files:
            with self.subTest(source_file=source_file.name):
                self.assertEqual(
                    (self.target / source_file.name).read_bytes(),
                    source_file.read_bytes(),
                )
        self.assertEqual(
            (self.target / "README.md").read_text(), "infra-owned provenance\n"
        )
        self.run_sync("--check")

    def test_check_reports_and_sync_repairs_drift(self):
        self.run_sync()
        drifted = self.target / "docker-compose.yaml"
        drifted.write_text("services: {}\n")

        result = self.run_sync("--check", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("Qwen compose mirror is stale", result.stderr)

        self.run_sync()
        self.assertEqual(drifted.read_bytes(), (SOURCE / drifted.name).read_bytes())


if __name__ == "__main__":
    unittest.main()
