import pathlib
import shutil
import subprocess
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy/glm53_flash_sm120"


class Glm53Sm120DeployTests(unittest.TestCase):
    @unittest.skipUnless(
        shutil.which("docker"),
        "Docker Compose is validated in the deployment lane",
    )
    def test_compose_semantic_validator_passes(self):
        result = subprocess.run(
            ["python3", str(DEPLOY / "validate-compose.py")],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_operational_scripts_are_valid_bash(self):
        for script in sorted(DEPLOY.glob("*.sh")):
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)

    def test_recipe_documents_isolation_and_parser_authority(self):
        readme = (DEPLOY / "README.md").read_text()
        for required in (
            "127.0.0.1:8062",
            "Qwen A",
            "Dockerfile.nullable-parser",
            "[\"string\", \"null\"]",
            "1,500-second",
        ):
            self.assertIn(required, readme)

    def test_loader_defers_only_the_inference_runtime_budget(self):
        wrapper = (DEPLOY / "node06-guarded-rollout.sh").read_text()
        canary = (DEPLOY / "node06-canary.sh").read_text()
        self.assertIn("--runtime-start-signal", wrapper)
        self.assertIn("--runtime-start-timeout-seconds 2400", wrapper)
        self.assertLess(
            canary.index("start_inference_budget\n"),
            canary.index('glm_serve_probe || fail'),
        )


if __name__ == "__main__":
    unittest.main()
