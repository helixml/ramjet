import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DRONE = ROOT / ".drone.yml"


class DronePublishConfigTest(unittest.TestCase):
    def test_publishers_use_guard_and_no_unsupported_path_condition(self):
        text = DRONE.read_text()
        self.assertNotIn("paths:", text)
        for kind, step in (
            ("rust-deps", "publish-rust-deps-image"),
            ("lb", "publish-image"),
            ("companion", "publish-companion-image"),
        ):
            section = re.search(
                rf"(?ms)^  - name: {re.escape(step)}\n(.*?)(?=^  - name: |^trigger:)",
                text,
            )
            self.assertIsNotNone(section, step)
            body = section.group(1)
            self.assertIn(f"sh bench/drone_publish_guard.sh {kind}", body)
            self.assertIn('if [ "$status" -eq 3 ]; then exit 0; fi', body)
            self.assertIn('if [ "$status" -ne 0 ]; then exit "$status"; fi', body)
            self.assertIn("exec /bin/drone-docker", body)
            self.assertRegex(body, r"(?ms)when:\n      event:\n        - push")
            self.assertNotIn("pull_request", body)

    def test_dependency_guard_precedes_both_app_publishers(self):
        text = DRONE.read_text()
        for step in ("publish-image", "publish-companion-image"):
            section = re.search(
                rf"(?ms)^  - name: {step}\n(.*?)(?=^  - name: |^trigger:)", text
            )
            self.assertIsNotNone(section, step)
            self.assertRegex(
                section.group(1),
                r"(?ms)depends_on:\n      - quality-complete\n      - publish-rust-deps-image",
            )


if __name__ == "__main__":
    unittest.main()
