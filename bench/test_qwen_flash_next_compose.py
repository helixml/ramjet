import copy
import importlib.util
import os
import pathlib
import shutil
import subprocess
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = ROOT / "deploy" / "qwen38_flash_next" / "validate-compose.py"
SPEC = importlib.util.spec_from_file_location("qwen_flash_next_compose", VALIDATOR)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load Qwen3.8-Flash-Next Compose validator")
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


@unittest.skipUnless(
    shutil.which("docker"), "Docker Compose is validated in the deployment lane"
)
class QwenFlashNextComposeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = validator.render()

    def test_canonical_render_passes(self):
        validator.validate(copy.deepcopy(self.document))

    def test_model_revision_drift_fails(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["qwen38flashnext-b"]["labels"][
            "ai.ramjet.model.revision"
        ] = "mutable-main"
        with self.assertRaisesRegex(validator.ValidationError, "revision label"):
            validator.validate(changed)

    def test_cross_numa_gpu_placement_fails(self):
        changed = copy.deepcopy(self.document)
        devices = changed["services"]["qwen38flashnext-b"]["deploy"]["resources"][
            "reservations"
        ]["devices"]
        devices[0]["device_ids"] = ["0", "1", "2", "3"]
        with self.assertRaisesRegex(validator.ValidationError, "GPU placement"):
            validator.validate(changed)

    def test_unqualified_speculation_fails(self):
        changed = copy.deepcopy(self.document)
        command = changed["services"]["qwen38flashnext-a"]["command"]
        command[command.index(
            '--speculative-config={"method":"mtp","num_speculative_tokens":3,'
            '"index_share_for_mtp_iteration":true}'
        )] = '--speculative-config={"method":"mtp","num_speculative_tokens":4}'
        with self.assertRaisesRegex(validator.ValidationError, "admitted profile"):
            validator.validate(changed)

    def test_standard_profile_cannot_enable_speculation(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["qwen38flashnext-b"]["command"].append(
            '--speculative-config={"method":"mtp","num_speculative_tokens":3,'
            '"index_share_for_mtp_iteration":true}'
        )
        with self.assertRaisesRegex(validator.ValidationError, "admitted profile"):
            validator.validate(changed)

    def test_host_memory_offload_fails(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["qwen38flashnext-a"]["environment"][
            "VLLM_PLE_CPU_OFFLOAD"
        ] = "1"
        with self.assertRaisesRegex(validator.ValidationError, "PLE offload"):
            validator.validate(changed)

    def test_sensitive_api_key_argument_fails(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["qwen38flashnext-a"]["command"].append(
            "--api-key=leaked"
        )
        with self.assertRaisesRegex(validator.ValidationError, "serving argv"):
            validator.validate(changed)

    def test_public_direct_engine_port_fails(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["qwen38flashnext-a"]["ports"][0]["host_ip"] = "0.0.0.0"
        with self.assertRaisesRegex(validator.ValidationError, "loopback"):
            validator.validate(changed)

    def test_legacy_max_tokens_strip_fails(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["ds4-loadbalancer"]["environment"][
            "RJ_MAX_TOKENS_STRIP"
        ] = "100000"
        with self.assertRaisesRegex(validator.ValidationError, "output budget"):
            validator.validate(changed)

    def test_exact_placement_defaults_to_the_qualified_full_cohort(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["ds4-loadbalancer"]["environment"][
            "RJ_EXACT_ROUTE_CANARY_BPS"
        ] = "0"
        with self.assertRaisesRegex(validator.ValidationError, "exact routing authority"):
            validator.validate(changed)

    def test_exact_placement_requires_the_independent_key(self):
        changed = copy.deepcopy(self.document)
        changed["services"]["ds4-loadbalancer"]["environment"][
            "RJ_EXACT_ROUTE_CANARY_KEY"
        ] = ""
        with self.assertRaisesRegex(validator.ValidationError, "exact routing authority"):
            validator.validate(changed)

    def test_compose_render_without_the_exact_key_fails_closed(self):
        environment = os.environ.copy()
        environment.pop("RJ_EXACT_ROUTE_CANARY_KEY", None)
        completed = subprocess.run(
            [
                "docker",
                "compose",
                "-f",
                str(validator.COMPOSE),
                "config",
                "--quiet",
            ],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("set RJ_EXACT_ROUTE_CANARY_KEY", completed.stderr)

    def test_engine_kv_event_publisher_is_required(self):
        changed = copy.deepcopy(self.document)
        command = changed["services"]["qwen38flashnext-b"]["command"]
        command[:] = [argument for argument in command if "kv-events-config" not in argument]
        with self.assertRaisesRegex(validator.ValidationError, "required serving arguments"):
            validator.validate(changed)

    def test_machineview_bridge_drift_fails(self):
        changed = copy.deepcopy(self.document)
        changed["networks"]["machineview-host"]["name"] = "unbound-bridge"
        with self.assertRaisesRegex(validator.ValidationError, "host bridge"):
            validator.validate(changed)


if __name__ == "__main__":
    unittest.main()
