import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "deploy" / "qwen38_27b"
SYNC = SOURCE / "sync-compose.sh"


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
