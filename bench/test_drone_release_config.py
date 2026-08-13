import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DRONE = ROOT / ".drone.yml"


class DroneReleaseConfigTest(unittest.TestCase):
    @staticmethod
    def pipeline(name):
        for document in DRONE.read_text().split("\n---\n"):
            if f"name: {name}\n" in document:
                return document
        raise AssertionError(f"missing pipeline {name}")

    def test_tag_pipeline_is_isolated_and_runs_the_full_quality_gate(self):
        release = self.pipeline("release-tags")
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

    def test_release_publishers_copy_qualified_images_without_building(self):
        release = self.pipeline("release-tags")
        expected = {
            "release-image": "lb",
            "release-companion-image": "companion",
        }
        for step, kind in expected.items():
            section = re.search(
                rf"(?ms)^  - name: {step}\n(.*?)(?=^  - name: |^trigger:)", release
            )
            self.assertIsNotNone(section, step)
            body = section.group(1)
            self.assertIn("gcr.io/go-containerregistry/crane@sha256:", body)
            self.assertIn("entrypoint:\n      - /busybox/sh", body)
            self.assertIn(f"sh bench/drone_release_publish.sh {kind}", body)
            self.assertNotIn("drone-docker", body)
            self.assertNotIn("settings:", body)
            self.assertNotIn("tags:", body)
            self.assertRegex(body, r"(?ms)when:\n      event:\n        - tag")
            self.assertRegex(body, r"(?ms)depends_on:\n      - release-quality-complete")
            self.assertNotIn("edge", body)

        script = (ROOT / "bench" / "drone_registry_promote.sh").read_text()
        self.assertIn('source="ghcr.io/helixml/mini-dynamo:rust-$short"', script)
        self.assertIn('source="ghcr.io/helixml/mini-dynamo:companion-rust-$short"', script)
        self.assertIn('destination="ghcr.io/helixml/mini-dynamo:$tag"', script)
        self.assertIn('destination="ghcr.io/helixml/mini-dynamo:companion-$tag"', script)
        self.assertIn('crane copy "$source" "$destination"', script)
        self.assertIn('[ "$source_digest" = "$destination_digest" ]', script)
        self.assertIn('$result=idempotent kind=$kind', script)
        self.assertIn("fail destination_conflict", script)
        self.assertIn("fail destination_lookup", script)
        for label in ("source_label_mismatch", "version_label_mismatch", "revision_label_mismatch"):
            self.assertIn(label, script)

    def test_v010_recovery_is_exact_promote_only_and_build_free(self):
        recovery = self.pipeline("release-v0.1.0-recovery")
        self.assertRegex(
            recovery,
            r"(?ms)trigger:\n  event:\n    - promote\n  target:\n    - release-v0\.1\.0",
        )
        for forbidden in (
            "- push",
            "- pull_request",
            "- tag",
            "/kaniko/executor",
            "plugins/docker",
            "cargo build",
            "docker build",
            "rust-edge",
        ):
            self.assertNotIn(forbidden, recovery)
        self.assertIn("sh bench/drone_release_recovery_plan.sh", recovery)
        for step, kind in (
            ("recover-v0.1.0-image", "lb"),
            ("recover-v0.1.0-companion-image", "companion"),
        ):
            section = re.search(
                rf"(?ms)^  - name: {re.escape(step)}\n(.*?)(?=^  - name: |^trigger:)",
                recovery,
            )
            self.assertIsNotNone(section, step)
            body = section.group(1)
            self.assertIn("entrypoint:\n      - /busybox/sh", body)
            self.assertIn(f"sh bench/drone_release_recovery_publish.sh {kind}", body)
            self.assertRegex(
                body,
                r"(?ms)depends_on:\n      - validate-v0\.1\.0-authority",
            )

    def test_runtime_images_declare_exact_oci_identity_labels(self):
        for name in ("Dockerfile", "Dockerfile.companion"):
            dockerfile = (ROOT / name).read_text()
            self.assertIn(
                'org.opencontainers.image.source="https://github.com/helixml/mini-dynamo"',
                dockerfile,
            )
            self.assertIn('org.opencontainers.image.version="0.1.0"', dockerfile)
            self.assertIn('org.opencontainers.image.revision="${OCI_REVISION}"', dockerfile)
            self.assertIn("ARG OCI_REVISION", dockerfile)

    def test_main_candidates_also_receive_exact_revision_metadata(self):
        quality = DRONE.read_text().split("\n---\n")[0]
        for step in ("publish-image", "publish-companion-image"):
            section = re.search(
                rf"(?ms)^  - name: {step}\n(.*?)(?=^  - name: |^trigger:)", quality
            )
            self.assertIsNotNone(section, step)
            body = section.group(1)
            self.assertIn("OCI_REVISION=${DRONE_COMMIT_SHA}", body)


if __name__ == "__main__":
    unittest.main()
