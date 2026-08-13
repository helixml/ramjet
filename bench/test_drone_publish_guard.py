import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "bench" / "drone_publish_guard.sh"


class DronePublishGuardTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temporary.name)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=self.root, check=True)
        subprocess.run(
            ["git", "config", "user.email", "ci@example.invalid"],
            cwd=self.root,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "CI test"], cwd=self.root, check=True
        )

    def tearDown(self):
        self.temporary.cleanup()

    def commit(self, paths):
        for relative, content in paths.items():
            target = self.root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
        subprocess.run(["git", "add", "-A"], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "test"], cwd=self.root, check=True)
        return subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def run_guard(self, kind, before=None, after=None):
        environment = os.environ.copy()
        environment.pop("DRONE_COMMIT_BEFORE", None)
        environment.pop("DRONE_COMMIT_SHA", None)
        if before is not None:
            environment["DRONE_COMMIT_BEFORE"] = before
        if after is not None:
            environment["DRONE_COMMIT_SHA"] = after
        return subprocess.run(
            ["sh", str(SCRIPT), kind],
            cwd=self.root,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

    def assert_matrix(self, before, after, expected):
        for kind, should_publish in expected.items():
            result = self.run_guard(kind, before, after)
            self.assertEqual(result.returncode, 0 if should_publish else 3, result)
            self.assertEqual(
                result.stdout.strip(),
                f"publisher_guard={'publish' if should_publish else 'skip'} kind={kind}",
            )
            self.assertEqual(result.stderr, "")

    def test_deployment_only_change_skips_every_publisher(self):
        before = self.commit({"README.md": "base"})
        after = self.commit(
            {
                ".drone.yml": "ci",
                "AGENTS.md": "docs",
                "bench/example.py": "bench",
                "deploy/dspark_0731/compose.yaml": "deploy",
            }
        )
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": False, "companion": False},
        )

    def test_source_change_publishes_both_app_images_only(self):
        before = self.commit({"README.md": "base"})
        after = self.commit({"src/router.rs": "source"})
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": True, "companion": True},
        )

    def test_manifest_change_publishes_dependency_and_both_apps(self):
        before = self.commit({"README.md": "base"})
        after = self.commit({"Cargo.lock": "lock"})
        self.assert_matrix(
            before,
            after,
            {"rust-deps": True, "lb": True, "companion": True},
        )

    def test_lb_only_inputs_do_not_publish_companion(self):
        before = self.commit({"README.md": "base"})
        after = self.commit({"compat/manifest.json": "{}", "Dockerfile": "lb"})
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": True, "companion": False},
        )

    def test_companion_dockerfile_is_companion_only(self):
        before = self.commit({"README.md": "base"})
        after = self.commit({"Dockerfile.companion": "companion"})
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": False, "companion": True},
        )

    def test_missing_unavailable_and_empty_ranges_fail_closed(self):
        commit = self.commit({"README.md": "base"})
        missing = self.run_guard("lb")
        self.assertEqual(missing.returncode, 2)
        self.assertEqual(missing.stderr.strip(), "publisher_guard=error reason=missing_revision")
        unavailable = self.run_guard("lb", "deadbeef", commit)
        self.assertEqual(unavailable.returncode, 2)
        self.assertEqual(
            unavailable.stderr.strip(), "publisher_guard=error reason=unavailable_revision"
        )
        empty = self.run_guard("lb", commit, commit)
        self.assertEqual(empty.returncode, 2)
        self.assertEqual(empty.stderr.strip(), "publisher_guard=error reason=empty_changeset")

    def test_deleted_owned_file_still_publishes(self):
        before = self.commit({"src/removed.rs": "source"})
        os.remove(self.root / "src" / "removed.rs")
        subprocess.run(["git", "add", "-A"], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "delete"], cwd=self.root, check=True)
        after = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        self.assert_matrix(before, after, {"lb": True, "companion": True})

    def test_unusual_owned_filename_still_publishes(self):
        before = self.commit({"README.md": "base"})
        after = self.commit({"src/line\nbreak.rs": "source"})
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": True, "companion": True},
        )


if __name__ == "__main__":
    unittest.main()
