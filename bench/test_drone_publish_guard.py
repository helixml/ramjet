import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PLANNER = ROOT / "bench" / "drone_publish_plan.sh"
GUARD = ROOT / "bench" / "drone_publish_guard.sh"


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
        return self.head(self.root)

    @staticmethod
    def head(root):
        return subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def run_planner(self, before=None, after=None, *, cwd=None, event="push", env=None):
        environment = (env or os.environ).copy()
        for key in ("DRONE_BUILD_EVENT", "DRONE_COMMIT_BEFORE", "DRONE_COMMIT_SHA"):
            environment.pop(key, None)
        environment["DRONE_BUILD_EVENT"] = event
        if before is not None:
            environment["DRONE_COMMIT_BEFORE"] = before
        if after is not None:
            environment["DRONE_COMMIT_SHA"] = after
        return subprocess.run(
            ["sh", str(PLANNER)],
            cwd=cwd or self.root,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

    def run_guard(self, kind, after, *, cwd=None):
        environment = os.environ.copy()
        environment["DRONE_COMMIT_SHA"] = after
        return subprocess.run(
            ["sh", str(GUARD), kind],
            cwd=cwd or self.root,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

    def assert_matrix(self, before, after, expected, *, cwd=None):
        result = self.run_planner(before, after, cwd=cwd)
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(result.stdout.strip(), "publisher_plan=ready")
        for kind, should_publish in expected.items():
            result = self.run_guard(kind, after, cwd=cwd)
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

    def test_source_manifest_and_image_specific_matrices(self):
        before = self.commit({"README.md": "base"})
        source = self.commit({"src/router.rs": "source"})
        self.assert_matrix(
            before, source, {"rust-deps": False, "lb": True, "companion": True}
        )
        manifest = self.commit({"Cargo.lock": "lock"})
        self.assert_matrix(
            source, manifest, {"rust-deps": True, "lb": True, "companion": True}
        )
        lb = self.commit({"compat/manifest.json": "{}", "Dockerfile": "lb"})
        self.assert_matrix(
            manifest, lb, {"rust-deps": False, "lb": True, "companion": False}
        )
        companion = self.commit({"Dockerfile.companion": "companion"})
        self.assert_matrix(
            lb,
            companion,
            {"rust-deps": False, "lb": False, "companion": True},
        )
        # The machine-view UI is baked into the LB image only.
        web = self.commit({"web/src/App.tsx": "dashboard"})
        self.assert_matrix(
            companion,
            web,
            {"rust-deps": False, "lb": True, "companion": False},
        )

    def test_release_tools_inputs_select_only_the_tools_publisher(self):
        before = self.commit({"README.md": "base"})
        after = self.commit(
            {
                "Dockerfile.release-tools": "pinned shell and crane",
                ".docker/release-tools-key": "content key",
            }
        )
        self.assert_matrix(
            before,
            after,
            {
                "rust-deps": False,
                "release-tools": True,
                "lb": False,
                "companion": False,
            },
        )

    def test_dependency_seed_and_both_app_images_publish_in_one_merge(self):
        before = self.commit({"README.md": "base"})
        after = self.commit(
            {
                "Dockerfile.deps": "cache builder change",
                "Dockerfile": "lb dependency reference",
                "Dockerfile.companion": "companion dependency reference",
                ".docker/rust-deps-key": "fresh key",
            }
        )
        self.assert_matrix(
            before,
            after,
            {"rust-deps": True, "lb": True, "companion": True},
        )

    def test_missing_invalid_mismatched_and_empty_ranges_fail_closed(self):
        commit = self.commit({"README.md": "base"})
        cases = (
            (None, None, "missing_revision"),
            ("deadbeef", commit, "invalid_revision"),
            (commit, "f" * 40, "head_mismatch"),
            (commit, commit, "empty_changeset"),
        )
        for before, after, reason in cases:
            result = self.run_planner(before, after)
            self.assertEqual(result.returncode, 2, result)
            self.assertEqual(result.stderr.strip(), f"publisher_plan=error reason={reason}")

    def test_deleted_and_unusual_owned_files_publish(self):
        before = self.commit({"src/removed.rs": "source"})
        os.remove(self.root / "src" / "removed.rs")
        (self.root / "src" / "line\nbreak.rs").write_text("source")
        subprocess.run(["git", "add", "-A"], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "change"], cwd=self.root, check=True)
        after = self.head(self.root)
        self.assert_matrix(before, after, {"lb": True, "companion": True})

    def test_shallow_clone_fetches_predecessor_and_builds_plan(self):
        upstream = self.root / "upstream"
        upstream.mkdir()
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=upstream, check=True)
        subprocess.run(
            ["git", "config", "user.email", "ci@example.invalid"],
            cwd=upstream,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "CI test"], cwd=upstream, check=True
        )
        (upstream / "README.md").write_text("base")
        subprocess.run(["git", "add", "README.md"], cwd=upstream, check=True)
        subprocess.run(["git", "commit", "-qm", "base"], cwd=upstream, check=True)
        before = self.head(upstream)
        (upstream / "deploy").mkdir()
        (upstream / "deploy" / "compose.yaml").write_text("deployment")
        subprocess.run(["git", "add", "deploy"], cwd=upstream, check=True)
        subprocess.run(["git", "commit", "-qm", "deploy"], cwd=upstream, check=True)
        after = self.head(upstream)
        shallow = self.root / "shallow"
        subprocess.run(
            ["git", "clone", "-q", "--depth=1", upstream.as_uri(), str(shallow)],
            check=True,
        )
        self.assert_matrix(
            before.upper(),
            after.upper(),
            {"rust-deps": False, "lb": False, "companion": False},
            cwd=shallow,
        )
        subprocess.run(
            ["git", "cat-file", "-e", f"{before}^{{commit}}"], cwd=shallow, check=True
        )
        self.assertFalse((shallow / ".git" / "FETCH_HEAD").exists())

    def test_planner_replaces_malicious_directory_and_symlink(self):
        before = self.commit({"README.md": "base"})
        after = self.commit({"deploy/compose.yaml": "deployment"})
        plan = self.root / ".drone-publish-plan"
        plan.mkdir()
        (plan / "lb").write_text(after)
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": False, "companion": False},
        )
        target = self.root / "attacker-target"
        target.mkdir()
        (target / "lb").write_text(after)
        for child in plan.iterdir():
            child.unlink()
        plan.rmdir()
        plan.symlink_to(target, target_is_directory=True)
        self.assert_matrix(
            before,
            after,
            {"rust-deps": False, "lb": False, "companion": False},
        )
        self.assertFalse(plan.is_symlink())
        self.assertEqual((target / "lb").read_text(), after)

    def test_pull_request_does_not_create_or_replace_plan(self):
        plan = self.root / ".drone-publish-plan"
        plan.mkdir()
        (plan / "sentinel").write_text("unchanged")
        result = self.run_planner(event="pull_request")
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(result.stdout.strip(), "publisher_plan=skip reason=non_push")
        self.assertEqual((plan / "sentinel").read_text(), "unchanged")

    def test_guard_rejects_missing_symlinked_or_stale_plan(self):
        commit = self.commit({"README.md": "base"})
        missing = self.run_guard("lb", commit)
        self.assertEqual(missing.returncode, 2)
        self.assertEqual(missing.stderr.strip(), "publisher_guard=error reason=invalid_plan")
        plan = self.root / ".drone-publish-plan"
        plan.mkdir()
        (plan / "lb").write_text("f" * 40)
        stale = self.run_guard("lb", commit)
        self.assertEqual(stale.returncode, 2)
        self.assertEqual(stale.stderr.strip(), "publisher_guard=error reason=invalid_marker")
        (plan / "lb").unlink()
        target = self.root / "marker"
        target.write_text(commit)
        (plan / "lb").symlink_to(target)
        linked = self.run_guard("lb", commit)
        self.assertEqual(linked.returncode, 2)
        self.assertEqual(linked.stderr.strip(), "publisher_guard=error reason=invalid_marker")


if __name__ == "__main__":
    unittest.main()
