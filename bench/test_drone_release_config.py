import pathlib
import re
import tomllib
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
            self.assertRegex(
                body,
                r"image: ghcr\.io/helixml/ramjet:release-tools-sha256-[0-9a-f]{64}@sha256:[0-9a-f]{64}",
            )
            self.assertIn(f"sh bench/drone_release_publish.sh {kind}", body)
            self.assertNotIn("drone-docker", body)
            self.assertNotIn("settings:", body)
            self.assertNotIn("tags:", body)
            self.assertRegex(body, r"(?ms)when:\n      event:\n        - tag")
            self.assertRegex(body, r"(?ms)depends_on:\n      - release-quality-complete")
            self.assertNotIn("edge", body)

        script = (ROOT / "bench" / "drone_registry_promote.sh").read_text()
        self.assertIn('source="ghcr.io/helixml/ramjet:rust-$short"', script)
        self.assertIn('source="ghcr.io/helixml/ramjet:companion-rust-$short"', script)
        self.assertIn('destination="ghcr.io/helixml/ramjet:$tag"', script)
        self.assertIn('destination="ghcr.io/helixml/ramjet:companion-$tag"', script)
        self.assertIn('crane copy "$source" "$destination"', script)
        self.assertIn('[ "$source_digest" = "$destination_digest" ]', script)
        self.assertIn('release_publish=idempotent kind=$kind', script)
        self.assertIn("fail destination_conflict", script)
        self.assertIn("fail destination_lookup", script)
        for label in ("source_label_mismatch", "version_label_mismatch", "revision_label_mismatch"):
            self.assertIn(label, script)

    def test_no_one_time_promote_recovery_surface_remains(self):
        text = DRONE.read_text()
        self.assertNotIn("- promote", text)
        self.assertNotIn("release-v0.1.0-recovery", text)
        self.assertNotIn("drone_release_recovery", text)
        for name in (
            "drone_release_recovery_plan.sh",
            "drone_release_recovery_guard.sh",
            "drone_release_recovery_publish.sh",
            "test_drone_release_recovery.py",
        ):
            self.assertFalse((ROOT / "bench" / name).exists(), name)

    def test_runtime_images_declare_exact_oci_identity_labels(self):
        # The version comes from Cargo.toml rather than a literal: the release
        # publisher verifies the label against the package version, so pinning
        # a number here only means every release has to edit its own guard.
        # `bench/test_release_metadata.py` owns cross-file version agreement.
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        for name in ("Dockerfile", "Dockerfile.companion"):
            dockerfile = (ROOT / name).read_text()
            self.assertIn(
                'org.opencontainers.image.source="https://github.com/helixml/ramjet"',
                dockerfile,
            )
            self.assertIn(f'org.opencontainers.image.version="{version}"', dockerfile)
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
