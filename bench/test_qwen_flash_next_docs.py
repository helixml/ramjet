import hashlib
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
README = ROOT / "deploy" / "qwen38_flash_next" / "README.md"
COMPOSE = ROOT / "deploy" / "qwen38_flash_next" / "docker-compose.yaml"
MANIFEST = ROOT / "compat" / "qwen38-flash-next-r134.json"


class QwenFlashNextDocumentationTests(unittest.TestCase):
    def test_readme_matches_swift_runtime_authority(self):
        readme = README.read_text()
        compose = COMPOSE.read_text()
        compose_sha = hashlib.sha256(COMPOSE.read_bytes()).hexdigest()
        manifest_sha = hashlib.sha256(MANIFEST.read_bytes()).hexdigest()

        for expected in (
            "rust-962b7b2",
            "fe432bbca183d2a457a7713fb150ea5ee36aba7a13f92280ef3ec7195ec23673",
            "ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4",
            "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38",
            "de956cabfc92ef5cd382e2557d03aa22d25fb57be3fe9048ed844d6863800287",
            "client-facing model ID remains `qwen3.8-flash-next`",
            "nvidia/Qwen3.8-Flash-Next-NVFP4",
            "fc694b54fb0174e0913e6adf86691ef85a4ead47",
            "5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93",
            "rust-0c7c7bc",
            "f9215991a15a2d5ea223c84bfc4a2f7af423b0a3d4868423543c5d8a5315615f",
            compose_sha,
            manifest_sha,
            "exact placement stays off",
            "old FP8 authority must never be reused",
            "fixes the cohort",
            "keeps `RJ_EXACT_ROUTE_MODE=off`",
            "POST /api/adaptive/rollback",
            "Retry rollback",
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, readme)

        for expected in (
            "RJ_TOKENIZER_MODE: local-shadow",
            "RJ_EXACT_ROUTE_MODE: ${RJ_EXACT_ROUTE_MODE:-off}",
            'RJ_EXACT_ROUTE_CANARY_BPS: "0"',
            "RJ_KV_EVENT_MODE: ${RJ_KV_EVENT_MODE:-shadow}",
            "endpoint\":\"tcp://*:5557",
            "replay_endpoint\":\"tcp://*:5558",
            "--quantization=modelopt",
            "--trust-remote-code",
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
            "zero placement cohort",
        ):
            with self.subTest(stale=stale):
                self.assertNotIn(stale, readme)


if __name__ == "__main__":
    unittest.main()
