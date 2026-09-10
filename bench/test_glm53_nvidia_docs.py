"""The GLM-5.3-Flash NVFP4 README must agree with what the deployment renders.

A deployment document that drifts from its Compose file is worse than none: it
is the thing an operator reads instead of the file at 3am.
"""

import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DIRECTORY = ROOT / "deploy" / "glm53_flash_nvidia"
README = DIRECTORY / "README.md"
COMPOSE = DIRECTORY / "docker-compose.yaml"
SIBLING_README = ROOT / "deploy" / "glm53_flash" / "README.md"

MODEL_REVISION = "423acf37583782c51c142d145aef733d72943d93"
ENGINE_DIGEST = "5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93"
LB_DIGEST = "c3fc5723a0dba51f9bb8eced77648cf0b05788039e90fc638fbd8c19adec70d8"


class Glm53NvidiaDocumentationTests(unittest.TestCase):
    def setUp(self):
        self.readme = README.read_text()
        self.compose = COMPOSE.read_text()

    def test_readme_pins_match_the_compose_file(self):
        for pin in (
            "nvidia/GLM-5.3-Flash-NVFP4",
            MODEL_REVISION,
            ENGINE_DIGEST,
            LB_DIGEST,
            "glm53nvidia-a",
            "glm53nvidia-b",
        ):
            with self.subTest(pin=pin):
                self.assertIn(pin, self.readme)
                self.assertIn(pin, self.compose)

    def test_readme_documents_every_disabled_authority(self):
        for key in (
            "RJ_TOKENIZER_MODE",
            "RJ_EXACT_ROUTE_MODE",
            "RJ_KV_EVENT_MODE",
            "RJ_SNAPSHOT_ROUTE_MODE",
            "RJ_IDLE_DRAIN_MODE",
        ):
            with self.subTest(key=key):
                self.assertIn(key, self.readme)

    def test_readme_states_the_live_loader_rejection(self):
        for claim in (
            "rejected at the live loader gate",
            "pe_dim=64",
            "qk_rope_head_dim=0",
            "No correctness, TPS, or concurrency",
            "bench/node06_gpu_guard.py",
            "do not add an overlay",
        ):
            with self.subTest(claim=claim):
                self.assertIn(claim, self.readme)

    def test_readme_does_not_claim_measured_performance(self):
        for overclaim in (
            "tokens per second",
            "tok/s",
            "outperforms",
            "qualified on node06",
            "promoted",
        ):
            with self.subTest(overclaim=overclaim):
                self.assertNotIn(overclaim, self.readme)

    def test_readme_explains_the_licence_blocker_it_removes(self):
        for expected in ("deploy/glm53_flash", "licence", "MIT"):
            with self.subTest(expected=expected):
                self.assertIn(expected, self.readme)

    def test_sibling_readme_points_at_this_deployment(self):
        self.assertIn("glm53_flash_nvidia", SIBLING_README.read_text())

    def test_canary_defaults_are_declared_as_reductions(self):
        self.assertIn("262K", self.readme)
        self.assertIn("four-sequence", self.readme)
        self.assertIn("not a performance comparison", self.readme)
        self.assertIn("MAX_NUM_SEQS:-4", self.compose)
        self.assertIn("MAX_MODEL_LEN:-262144", self.compose)

    def test_every_documented_shard_and_byte_count_matches_the_verifier(self):
        verifier = (DIRECTORY / "verify-model.py").read_text()
        self.assertIn("204,439,103,396", self.readme)
        self.assertIn("204_439_103_396", verifier)
        self.assertIn("33 shards", self.readme)
        self.assertIn("EXPECTED_SHARD_COUNT = 33", verifier)

    def test_compose_keeps_the_engines_offline_and_loopback(self):
        for expected in (
            'HF_HUB_OFFLINE: "1"',
            'TRANSFORMERS_OFFLINE: "1"',
            '"127.0.0.1:8060:8000"',
            '"127.0.0.1:8061:8000"',
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, self.compose)

    def test_deployment_has_no_second_compose_file(self):
        composes = sorted(
            path.name for path in DIRECTORY.iterdir() if path.suffix in {".yaml", ".yml"}
        )
        self.assertEqual(composes, ["docker-compose.yaml"])

    def test_readme_has_no_unresolved_placeholders(self):
        self.assertNotRegex(self.readme, r"\bTODO\b|\bTBD\b|<digest>|<revision>")

    def test_documented_preflight_command_names_the_pinned_image(self):
        match = re.search(r"vllm/vllm-openai@sha256:([0-9a-f]{64})", self.readme)
        self.assertIsNotNone(match)
        self.assertEqual(match.group(1), ENGINE_DIGEST)


if __name__ == "__main__":
    unittest.main()
