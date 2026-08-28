import hashlib
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
README = ROOT / "deploy" / "qwen38_flash_next" / "README.md"
COMPOSE = ROOT / "deploy" / "qwen38_flash_next" / "docker-compose.yaml"
MANIFEST = ROOT / "compat" / "qwen38-flash-next-r134.json"


class QwenFlashNextDocumentationTests(unittest.TestCase):
    def test_readme_matches_admitted_exact_route_authority(self):
        readme = README.read_text()
        compose = COMPOSE.read_text()
        compose_sha = hashlib.sha256(COMPOSE.read_bytes()).hexdigest()
        manifest_sha = hashlib.sha256(MANIFEST.read_bytes()).hexdigest()

        for expected in (
            "rust-r135-qwen-exact-de0bb28",
            "33b547fb33d78ed94b03fd7eaf27accb5939c6e82f12faa1b2c32bd4478b9b64",
            compose_sha,
            manifest_sha,
            "Qwen exact placement is admitted and live",
            "publish live KV",
            "events on port 5557 and bounded replay on port 5558",
            "node06's protected mode-0600 `.env` promotes the live cohort to",
            "`10000`",
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, readme)

        for expected in (
            "RJ_TOKENIZER_MODE: local-shadow",
            "RJ_EXACT_ROUTE_MODE: placement",
            "RJ_KV_EVENT_MODE: shadow",
            "endpoint\":\"tcp://*:5557",
            "replay_endpoint\":\"tcp://*:5558",
        ):
            with self.subTest(compose=expected):
                self.assertIn(expected, compose)

    def test_readme_does_not_restore_pre_rollout_claims(self):
        readme = README.read_text()
        for stale in (
            "approximate route is the only admitted routing mode",
            "keeps tokenizer and exact KV routing off",
            "Retain MTP3 with index reuse on both engines",
            "rust-r133-qwen38-flash-next",
        ):
            with self.subTest(stale=stale):
                self.assertNotIn(stale, readme)


if __name__ == "__main__":
    unittest.main()
