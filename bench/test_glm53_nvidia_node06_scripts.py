import hashlib
import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "glm53_flash_nvidia"


class Glm53NvidiaNode06ScriptsTest(unittest.TestCase):
    def text(self, name: str) -> str:
        return (DEPLOY / name).read_text()

    def test_canary_is_guarded_locked_and_lb_isolated(self):
        source = self.text("node06-canary.sh")
        self.assertIn("RAMJET_GPU_GUARD_ACTIVE", source)
        self.assertIn("/run/lock/ramjet-node06-deployment.lock", source)
        self.assertIn('qwen_compose_cmd stop -t 120 "$qwen_b"', source)
        self.assertIn("wait_qwen_b_idle", source)
        self.assertIn('glm_compose_cmd up -d --no-deps --force-recreate "$glm_b"', source)
        self.assertIn('wait_lb_a_only', source)
        self.assertNotRegex(source, r"compose[^\n]*up[^\n]*ds4-loadbalancer")

    def test_canary_pins_the_deployment_it_runs(self):
        source = self.text("node06-canary.sh")
        match = re.search(r"== ([0-9a-f]{64}) \]\]", source)
        self.assertIsNotNone(match)
        actual = hashlib.sha256((DEPLOY / "docker-compose.yaml").read_bytes()).hexdigest()
        self.assertEqual(match.group(1), actual)

    def test_failure_path_and_explicit_restore_recover_qwen_b(self):
        canary = self.text("node06-canary.sh")
        restore = self.text("node06-restore-qwen-b.sh")
        for source in (canary, restore):
            self.assertIn('up -d --no-deps --force-recreate "$qwen_b"', source)
            self.assertIn("wait_lb_full", source)
            self.assertNotRegex(source, r"compose[^\n]*up[^\n]*ds4-loadbalancer")
        self.assertIn("trap rollback EXIT", canary)
        self.assertIn("Qwen A identity changed", restore)

if __name__ == "__main__":
    unittest.main()
