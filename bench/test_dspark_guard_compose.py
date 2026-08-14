from __future__ import annotations

import copy
import importlib.util
import pathlib
import shutil
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = ROOT / "deploy" / "dspark_0731" / "validate-dspark-guard-compose.py"
SPEC = importlib.util.spec_from_file_location("dspark_guard_compose", VALIDATOR)
assert SPEC is not None and SPEC.loader is not None
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


@unittest.skipUnless(shutil.which("docker"), "Docker Compose is validated in CI")
class DsparkGuardComposeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.disabled = validator.render(enabled=False)
        cls.enabled = validator.render(enabled=True)

    def test_profile_is_explicit_durable_and_does_not_change_engines(self) -> None:
        validator.validate_disabled(self.disabled)
        validator.validate_enabled(self.enabled, self.disabled)

    def test_read_only_or_implicitly_created_authority_is_rejected(self) -> None:
        for field, value, message in [
            ("read_only", True, "exact read-write bind"),
            ("create_host_path", True, "may create"),
        ]:
            document = copy.deepcopy(self.enabled)
            mount = validator.volume_by_target(
                document["services"]["ds4-loadbalancer"], validator.TARGET
            )
            assert mount is not None
            if field == "create_host_path":
                mount["bind"][field] = value
            else:
                mount[field] = value
            with self.subTest(field=field), self.assertRaisesRegex(
                validator.ValidationError, message
            ):
                validator.validate_enabled(document, self.disabled)

    def test_threshold_or_admission_drift_is_rejected(self) -> None:
        for key in [
            "MD_UPSTREAM_ADMISSION_MODE",
            "MD_DSPARK_GUARD_CONSECUTIVE_WINDOWS",
            "MD_DSPARK_GUARD_MIN_PROPOSED_TOKENS",
        ]:
            document = copy.deepcopy(self.enabled)
            document["services"]["ds4-loadbalancer"]["environment"][key] = "unsafe"
            with self.subTest(key=key), self.assertRaisesRegex(
                validator.ValidationError, "environment changed"
            ):
                validator.validate_enabled(document, self.disabled)


if __name__ == "__main__":
    unittest.main()
