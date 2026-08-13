import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PLAN = ROOT / "bench" / "drone_release_recovery_plan.sh"
GUARD = ROOT / "bench" / "drone_release_recovery_guard.sh"
PUBLISH = ROOT / "bench" / "drone_release_recovery_publish.sh"
QUALIFIED_SHA = "b0e070073d4266018d2f907ff35a7ee88adfdcd4"


class DroneReleaseRecoveryTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temporary.name)
        self.fake_bin = self.root / "fake-bin"
        self.fake_bin.mkdir()
        git = self.fake_bin / "git"
        git.write_text(
            "#!/bin/sh\n"
            "case \"$*\" in\n"
            "  'rev-parse --verify HEAD^{commit}') printf '%s\\n' \"${FAKE_HEAD_SHA-$DRONE_COMMIT_SHA}\" ;;\n"
            "  'rev-parse --verify refs/mini-dynamo-recovery/v0.1.0^{commit}') "
            "printf '%s\\n' \"${FAKE_TAG_SHA-b0e070073d4266018d2f907ff35a7ee88adfdcd4}\" ;;\n"
            "  'show b0e070073d4266018d2f907ff35a7ee88adfdcd4:Cargo.toml') "
            "printf '[package]\\nname = \"mini-dynamo\"\\nversion = \"%s\"\\n' \"${FAKE_TAG_VERSION-0.1.0}\" ;;\n"
            "  *fetch*) [ \"${FAKE_FETCH_FAIL-0}\" = 0 ] ;;\n"
            "  'update-ref -d refs/mini-dynamo-recovery/v0.1.0') exit 0 ;;\n"
            "  *) echo \"unexpected git: $*\" >&2; exit 1 ;;\n"
            "esac\n"
        )
        git.chmod(0o755)

    def tearDown(self):
        self.temporary.cleanup()

    def environment(self, **overrides):
        environment = os.environ.copy()
        environment["PATH"] = f"{self.fake_bin}:{environment['PATH']}"
        environment.update(
            {
                "DRONE_BUILD_EVENT": "promote",
                "DRONE_DEPLOY_TO": "release-v0.1.0",
                "DRONE_COMMIT_SHA": "a" * 40,
            }
        )
        environment.update(overrides)
        return environment

    def run_script(self, script, *arguments, environment=None):
        return subprocess.run(
            ["sh", str(script), *arguments],
            cwd=self.root,
            env=environment or self.environment(),
            check=False,
            capture_output=True,
            text=True,
        )

    def install_fake_crane(self, state):
        crane = self.fake_bin / "crane"
        crane.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$*\" >> crane-calls\n"
            "case \"${1-}\" in\n"
            " auth) exit 0 ;;\n"
            " config) printf '%s\\n' '{\"config\":{\"Labels\":{"
            "\"org.opencontainers.image.source\":\"https://github.com/helixml/mini-dynamo\","
            "\"org.opencontainers.image.version\":\"0.1.0\","
            f"\"org.opencontainers.image.revision\":\"{QUALIFIED_SHA}\"}}}}' ;;\n"
            " digest)\n"
            "   case \"$2\" in *:rust-b0e0700|*:companion-rust-b0e0700) printf '%s\\n' sha256:qualified ;; esac\n"
            "   case \"$2\" in *:rust-b0e0700|*:companion-rust-b0e0700) exit 0 ;; esac\n"
            f"   case '{state}' in\n"
            "     same) printf '%s\\n' sha256:qualified ;;\n"
            "     conflict) printf '%s\\n' sha256:other ;;\n"
            "     missing) if [ -f copied ]; then printf '%s\\n' sha256:qualified; else echo MANIFEST_UNKNOWN >&2; exit 1; fi ;;\n"
            "     ambiguous) echo credential_helper_not_found >&2; exit 1 ;;\n"
            "   esac ;;\n"
            " copy) printf '%s\\n' \"$2 -> $3\" >> crane-copies; touch copied ;;\n"
            " *) exit 1 ;;\n"
            "esac\n"
        )
        crane.chmod(0o755)

    def prepare(self, state="missing"):
        environment = self.environment(GHCR_USERNAME="fixture", GHCR_TOKEN="secret")
        result = self.run_script(PLAN, environment=environment)
        self.assertEqual(result.returncode, 0, result)
        self.install_fake_crane(state)
        return environment

    def test_exact_promote_target_peels_tag_and_binds_both_markers(self):
        environment = self.environment()
        result = self.run_script(PLAN, environment=environment)
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(result.stdout.strip(), "release_recovery_plan=ready tag=v0.1.0")
        for kind in ("lb", "companion"):
            guarded = self.run_script(GUARD, kind, environment=environment)
            self.assertEqual(guarded.returncode, 0, guarded)
            self.assertEqual(
                guarded.stdout.strip(), f"release_recovery_guard=publish kind={kind}"
            )

    def test_wrong_event_target_tag_commit_version_or_head_fails_closed(self):
        cases = (
            ({"DRONE_BUILD_EVENT": "tag"}, "invalid_event"),
            ({"DRONE_DEPLOY_TO": "release-v0.1.1"}, "invalid_target"),
            ({"DRONE_COMMIT_SHA": "short"}, "invalid_pipeline_revision"),
            ({"FAKE_HEAD_SHA": "f" * 40}, "head_mismatch"),
            ({"FAKE_TAG_SHA": "f" * 40}, "tag_revision_mismatch"),
            ({"FAKE_TAG_VERSION": "0.1.1"}, "version_mismatch"),
            ({"FAKE_FETCH_FAIL": "1"}, "tag_fetch"),
        )
        for overrides, reason in cases:
            with self.subTest(reason=reason):
                result = self.run_script(PLAN, environment=self.environment(**overrides))
                self.assertEqual(result.returncode, 2, result)
                self.assertEqual(
                    result.stderr.strip(),
                    f"release_recovery_plan=error reason={reason}",
                )

    def test_stale_or_symlinked_authority_is_rejected(self):
        environment = self.environment()
        plan = self.root / ".drone-release-recovery-plan"
        plan.mkdir()
        (plan / "lb").write_text("stale")
        result = self.run_script(GUARD, "lb", environment=environment)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(
            result.stderr.strip(), "release_recovery_guard=error reason=invalid_marker"
        )
        (plan / "lb").unlink()
        target = self.root / "attacker"
        target.write_text("authority")
        (plan / "lb").symlink_to(target)
        result = self.run_script(GUARD, "lb", environment=environment)
        self.assertEqual(result.returncode, 2)

    def test_missing_destination_copies_exact_b0_manifests(self):
        environment = self.prepare("missing")
        for kind in ("lb", "companion"):
            (self.root / "copied").unlink(missing_ok=True)
            result = self.run_script(PUBLISH, kind, environment=environment)
            self.assertEqual(result.returncode, 0, result)
            self.assertEqual(
                result.stdout.strip(), f"release_recovery_publish=complete kind={kind}"
            )
        copies = (self.root / "crane-copies").read_text()
        self.assertIn(":rust-b0e0700 -> ghcr.io/helixml/mini-dynamo:v0.1.0", copies)
        self.assertIn(
            ":companion-rust-b0e0700 -> ghcr.io/helixml/mini-dynamo:companion-v0.1.0",
            copies,
        )

    def test_same_digest_is_idempotent_without_copy(self):
        environment = self.prepare("same")
        result = self.run_script(PUBLISH, "lb", environment=environment)
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(
            result.stdout.strip(), "release_recovery_publish=idempotent kind=lb"
        )
        self.assertFalse((self.root / "crane-copies").exists())

    def test_conflict_and_ambiguous_lookup_never_copy(self):
        for state, reason in (
            ("conflict", "destination_conflict"),
            ("ambiguous", "destination_lookup"),
        ):
            with self.subTest(state=state):
                environment = self.prepare(state)
                result = self.run_script(PUBLISH, "lb", environment=environment)
                self.assertEqual(result.returncode, 2, result)
                self.assertEqual(
                    result.stderr.strip(),
                    f"release_recovery_publish=error reason={reason}",
                )
                self.assertFalse((self.root / "crane-copies").exists())


if __name__ == "__main__":
    unittest.main()
