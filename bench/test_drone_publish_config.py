import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DRONE = ROOT / ".drone.yml"


class DronePublishConfigTest(unittest.TestCase):
    def test_quality_builds_are_serialized_on_the_runner(self):
        quality = DRONE.read_text().split("\n---\n")[0]
        self.assertRegex(quality, r"(?m)^concurrency:\n  limit: 1$")

    def test_rust_fetch_builds_plan_and_publishers_only_consume_it(self):
        text = DRONE.read_text()
        self.assertNotIn("paths:", text)
        fetch = re.search(
            r"(?ms)^  - name: rust-fetch\n(.*?)(?=^  - name: )", text
        )
        self.assertIsNotNone(fetch)
        self.assertIn("cargo fetch --locked\n      - sh bench/drone_publish_plan.sh", fetch.group(1))
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
            self.assertIn("gcr.io/kaniko-project/executor@sha256:", body)
            self.assertIn(f"sh bench/drone_publish_guard.sh {kind}", body)
            self.assertIn('if [ "$status" -eq 3 ]; then exit 0; fi', body)
            self.assertIn('if [ "$status" -ne 0 ]; then exit "$status"; fi', body)
            self.assertIn("exec /kaniko/executor", body)
            self.assertNotIn("privileged:", body)
            self.assertNotIn("plugins/docker", body)
            self.assertNotIn("git ", body)
            self.assertRegex(body, r"(?ms)when:\n      event:\n        - push")
            self.assertNotIn("pull_request", body)

    def test_kaniko_publishers_are_cached_and_revision_stamped(self):
        text = DRONE.read_text().split("\n---\n")[0]
        self.assertEqual(text.count("--cache-repo=ghcr.io/helixml/mini-dynamo-build-cache"), 3)
        self.assertEqual(text.count("--snapshot-mode=redo --reproducible"), 3)
        for step in ("publish-image", "publish-companion-image"):
            section = re.search(
                rf"(?ms)^  - name: {step}\n(.*?)(?=^  - name: |^trigger:)", text
            )
            self.assertIsNotNone(section, step)
            self.assertIn("--build-arg OCI_REVISION=${DRONE_COMMIT_SHA}", section.group(1))

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
