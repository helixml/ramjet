import copy
import importlib.util
import shutil
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "qwen38_glm53_kev"
VALIDATOR = DEPLOY / "validate-compose.py"


def load_validator():
    spec = importlib.util.spec_from_file_location("qwen_glm_kev_validator", VALIDATOR)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


@unittest.skipUnless(shutil.which("docker"), "Docker Compose is required")
class QwenGlmKevDeployTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.validator = load_validator()
        cls.document = cls.validator.render()

    def test_production_shape_validates(self):
        self.validator.validate(copy.deepcopy(self.document))

    def test_api_profile_and_model_maps_are_fail_closed(self):
        for key in ("RJ_UPSTREAM", "RJ_UPSTREAM_MODELS", "RJ_UPSTREAM_APIS"):
            changed = copy.deepcopy(self.document)
            changed["services"]["ds4-loadbalancer"]["environment"].pop(key)
            with self.assertRaises(ValueError):
                self.validator.validate(changed)

    def test_long_prompt_lane_defaults_to_the_second_glm_replica(self):
        environment = self.document["services"]["ds4-loadbalancer"]["environment"]
        self.assertEqual(environment["RJ_ROUTE_LONG_PROMPT_BYTES"], "600000")
        lanes = environment["RJ_ROUTE_LONG_PROMPT_UPSTREAMS"].split(",")
        upstreams = environment["RJ_UPSTREAM"].split(",")
        self.assertEqual(
            [upstream for upstream, lane in zip(upstreams, lanes) if lane == "lane"],
            ["http://glm53sm120-c:8000"],
        )

    def test_long_prompt_lane_render_is_fail_closed(self):
        for key, value in (
            ("RJ_ROUTE_LONG_PROMPT_BYTES", None),
            ("RJ_ROUTE_LONG_PROMPT_BYTES", "600k"),
            ("RJ_ROUTE_LONG_PROMPT_UPSTREAMS", None),
            ("RJ_ROUTE_LONG_PROMPT_UPSTREAMS", "-,-,lane"),
            ("RJ_ROUTE_LONG_PROMPT_UPSTREAMS", "-,-,-,-"),
            ("RJ_ROUTE_LONG_PROMPT_UPSTREAMS", "-,-,-,lane"),
        ):
            changed = copy.deepcopy(self.document)
            environment = changed["services"]["ds4-loadbalancer"]["environment"]
            if value is None:
                environment.pop(key)
            else:
                environment[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                self.validator.validate(changed)
        rolled_back = copy.deepcopy(self.document)
        rolled_back["services"]["ds4-loadbalancer"]["environment"][
            "RJ_ROUTE_LONG_PROMPT_BYTES"
        ] = "0"
        self.validator.validate(rolled_back)

    def test_kev_is_private_pinned_and_gpu_confined(self):
        kev = self.document["services"]["kev-small"]
        self.assertEqual(set(kev["networks"]), {"kev-engine"})
        self.assertEqual(kev["environment"]["KEV_HOST"], "0.0.0.0")
        self.assertEqual(kev["environment"]["HF_HUB_OFFLINE"], "1")
        self.assertEqual(
            kev["deploy"]["resources"]["reservations"]["devices"][0]["device_ids"],
            ["3"],
        )
        self.assertNotIn("ports", kev)

    def test_rollout_preserves_exact_lb_and_warms_before_canary(self):
        script = DEPLOY / "node06-rollout.sh"
        subprocess.run(["bash", "-n", script], check=True)
        text = script.read_text(encoding="utf-8")
        self.assertIn('docker rename ds4-loadbalancer "$rollback_name"', text)
        self.assertIn('docker rename "$rollback_name" ds4-loadbalancer', text)
        self.assertLess(text.index("kev-warmup.json"), text.index("validate_endpoint 18006"))
        self.assertIn("docker network create --internal ramjet_kev_systemone", text)
        self.assertNotIn("docker.sock", text)


if __name__ == "__main__":
    unittest.main()
