import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DRONE = ROOT / ".drone.yml"


class DroneReleaseConfigTest(unittest.TestCase):
    def test_tag_pipeline_is_isolated_and_runs_the_full_quality_gate(self):
        text = DRONE.read_text()
        release = text.split("\n---\n")[-1]
        self.assertIn("name: release-tags", release)
        self.assertRegex(release, r"(?ms)trigger:\n  event:\n    - tag\n  ref:\n    - refs/tags/v\*")
        for forbidden in ("- push", "- pull_request", "rust-edge", "companion-rust-edge"):
            self.assertNotIn(forbidden, release)
        for step in (
            "release-rust-lint",
            "release-rust-test",
            "release-agent-protocol",
            "release-deployment-compose",
        ):
            self.assertIn(f"- name: {step}", release)
            self.assertRegex(
                release,
                rf"(?ms)release-quality-complete.*?depends_on:.*?- {step}",
            )

    def test_release_publishers_are_tag_only_immutable_and_quality_gated(self):
        release = DRONE.read_text().split("\n---\n")[-1]
        expected = {
            "release-image": ("lb", "- ${DRONE_TAG}"),
            "release-companion-image": ("companion", "- companion-${DRONE_TAG}"),
        }
        for step, (kind, image_tag) in expected.items():
            section = re.search(
                rf"(?ms)^  - name: {step}\n(.*?)(?=^  - name: |^trigger:)", release
            )
            self.assertIsNotNone(section, step)
            body = section.group(1)
            self.assertIn(f"sh bench/drone_release_guard.sh {kind}", body)
            self.assertIn("exec /bin/drone-docker", body)
            self.assertIn(image_tag, body)
            self.assertEqual(body.count("tags:"), 1)
            self.assertRegex(body, r"(?ms)when:\n      event:\n        - tag")
            self.assertRegex(body, r"(?ms)depends_on:\n      - release-quality-complete")
            self.assertNotIn("edge", body)


if __name__ == "__main__":
    unittest.main()
