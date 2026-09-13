import copy
import importlib.util
import shutil
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "qwen38_glm53_multimodel"
VALIDATOR = DEPLOY / "validate-compose.py"


def load_validator():
    spec = importlib.util.spec_from_file_location("qwen_glm_validator", VALIDATOR)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


@unittest.skipUnless(
    shutil.which("docker"), "Docker Compose is validated in the deployment lane"
)
class QwenGlmMultimodelDeployTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.validator = load_validator()
        cls.document = cls.validator.render()

    def test_production_shape_validates(self):
        self.validator.validate(copy.deepcopy(self.document))

    def test_model_map_and_external_networks_are_fail_closed(self):
        for mutate in (
            lambda document: document["services"]["ds4-loadbalancer"]["environment"].pop(
                "RJ_UPSTREAM_MODELS"
            ),
            lambda document: document["services"]["ds4-loadbalancer"]["environment"].pop(
                "RJ_MACHINEVIEW_UPSTREAM_GPUS"
            ),
            lambda document: document["networks"]["glm-engine"].update(
                name="wrong-network"
            ),
            lambda document: document["services"]["ds4-loadbalancer"]["volumes"].append(
                {
                    "type": "bind",
                    "source": "/var/run/docker.sock",
                    "target": "/var/run/docker.sock",
                }
            ),
        ):
            changed = copy.deepcopy(self.document)
            mutate(changed)
            with self.assertRaises(ValueError):
                self.validator.validate(changed)

    def test_heterogeneous_authorities_stay_disabled(self):
        environment = self.document["services"]["ds4-loadbalancer"]["environment"]
        for key in (
            "RJ_TOKENIZER_MODE",
            "RJ_EXACT_ROUTE_MODE",
            "RJ_KV_EVENT_MODE",
            "RJ_SNAPSHOT_ROUTE_MODE",
            "RJ_IDLE_DRAIN_MODE",
        ):
            self.assertEqual(environment[key], "off")
        self.assertNotIn("RJ_ADAPTIVE_CONFIG_PATH", environment)

    def test_rollout_shell_is_syntactically_valid_and_preserves_exact_rollback(self):
        script = DEPLOY / "node06-rollout.sh"
        subprocess.run(["bash", "-n", script], check=True)
        text = script.read_text(encoding="utf-8")
        self.assertIn("docker rename ds4-loadbalancer \"$rollback_name\"", text)
        self.assertIn("docker rename \"$rollback_name\" ds4-loadbalancer", text)
        self.assertIn('rollout_stamp=$(date -u +%Y%m%dT%H%M%SZ)', text)
        self.assertIn(
            'canonical_project="qwen38_glm53_multimodel_release_${rollout_stamp,,}_$$"',
            text,
        )
        self.assertNotIn("canonical_project=qwen38_glm53_multimodel\n", text)
        self.assertIn('[[ "$canary_only" == 0 || "$canary_only" == 1 ]]', text)
        self.assertIn('if [[ "$canary_only" == 1 ]]', text)
        self.assertIn("for _attempt in $(seq 1 60)", text)
        self.assertIn('-H @"$authorization_header"', text)
        self.assertNotIn("Authorization: Bearer $VLLM_API_KEY", text)
        self.assertNotIn("docker.sock", text)

    def test_docs_name_both_exact_served_models(self):
        text = (DEPLOY / "README.md").read_text(encoding="utf-8")
        self.assertIn("qwen3.8-flash-next", text)
        self.assertIn("glm-5.3-flash", text)
        self.assertIn("combined", text)


if __name__ == "__main__":
    unittest.main()
