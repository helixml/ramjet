from __future__ import annotations

import copy
import importlib.util
import pathlib
import shutil
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = (
    ROOT
    / "deploy/dspark_0731/infernal-r11-candidate/validate-compose.py"
)
SPEC = importlib.util.spec_from_file_location("infernal_r11_compose", VALIDATOR)
assert SPEC is not None and SPEC.loader is not None
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


@unittest.skipUnless(shutil.which("docker"), "Docker Compose is validated in CI")
class InfernalR11ComposeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.base = validator.render(candidate=False)
        cls.candidate = validator.render(candidate=True)

    def test_candidate_is_single_homed_and_isolated(self) -> None:
        validator.validate(self.base, self.candidate)

    def test_candidate_cannot_reenter_load_balancer(self) -> None:
        document = copy.deepcopy(self.candidate)
        document["services"]["ds4-loadbalancer"]["environment"][
            "DS4_UPSTREAM"
        ] += ",http://dspark-0731-b:8000"
        with self.assertRaisesRegex(validator.ValidationError, "single-home"):
            validator.validate(self.base, document)

    def test_engine_a_change_is_rejected(self) -> None:
        document = copy.deepcopy(self.candidate)
        document["services"]["dspark-0731"]["image"] = "changed"
        with self.assertRaisesRegex(validator.ValidationError, "engine A"):
            validator.validate(self.base, document)

    def test_unrelated_engine_b_change_is_rejected(self) -> None:
        document = copy.deepcopy(self.candidate)
        document["services"]["dspark-0731-b"]["ports"][0]["published"] = "9999"
        with self.assertRaisesRegex(validator.ValidationError, "unrelated"):
            validator.validate(self.base, document)

    def test_unrelated_top_level_change_is_rejected(self) -> None:
        document = copy.deepcopy(self.candidate)
        document["networks"]["default"]["name"] = "changed"
        with self.assertRaisesRegex(validator.ValidationError, "top-level"):
            validator.validate(self.base, document)


if __name__ == "__main__":
    unittest.main()
