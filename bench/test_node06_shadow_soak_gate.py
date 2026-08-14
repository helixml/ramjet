import argparse
import contextlib
import dataclasses
import io
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import node06_shadow_soak_gate as gate


GUARD_CONTRACT = {
    "expected_gpus": 8,
    "abort_c": 78.0,
    "run_id": "2" * 32,
}


def identity(name, image="image@sha256:abc"):
    return gate.recovery.ContainerIdentity(
        container_id=f"id-{name}",
        image_id=f"sha256:{name}",
        configured_image=image,
        started_at="2026-08-14T00:00:00Z",
        restart_count=0,
        running=True,
        health="healthy",
        config_hash=f"hash-{name}",
        compose_project="dspark_0731",
    )


def baseline():
    return gate.SoakBaseline(
        lb=identity("baseline", "baseline@sha256:abc"),
        engines=(identity("engine-a"), identity("engine-b")),
        companions=(identity("companion-a"), identity("companion-b")),
        baseline_hash="hash-baseline",
        candidate_hash="hash-candidate",
        candidate_image_id="sha256:candidate",
        boot_id="boot-id",
        engine_process_starts=(100, 200),
    )


class FakeRuntime:
    def __init__(self, failure=None, rollback_failure=False):
        self.failure = failure
        self.rollback_failure = rollback_failure
        self.locked = False
        self.deploy_calls = 0
        self.rollback_calls = 0
        self.rollback_active = False
        self.signal_pending = False

    @contextlib.contextmanager
    def lock(self, exclusive):
        self.assert_true(exclusive)
        self.locked = True
        try:
            yield
        finally:
            self.locked = False

    @staticmethod
    def assert_true(value):
        if not value:
            raise AssertionError("expected true")

    def preflight(self):
        if self.failure == "preflight":
            raise gate.recovery.GateError("preflight_failed", "injected")
        return baseline(), ((10, 20), (2560, 5120)), [{"stable": True}] * 2

    @staticmethod
    def plan():
        return {"gate": "digest", "workload": "digest"}

    def begin_rollback(self):
        self.rollback_active = True

    def end_rollback(self):
        self.rollback_active = False
        return self.signal_pending

    def deploy_candidate(self, _baseline):
        self.assert_true(self.locked)
        self.deploy_calls += 1
        if self.failure == "interrupt_deploy":
            raise gate.GateInterrupted()
        if self.failure == "deploy":
            raise gate.recovery.GateError("deploy_failed", "injected")
        return identity("candidate", "sha256:candidate"), ((11, 21), (2816, 5376))

    def run_workload(self):
        self.assert_true(self.locked)
        if self.failure == "workload":
            error = gate.recovery.GateError("workload_failed", "injected")
            error.payload = {"type": "shadow_soak_source_failure"}
            raise error
        if self.failure == "invalid_workload":
            return {}
        return {
            "type": "shadow_soak",
            "unique_sources": 104,
            "source_concurrency": 2,
            "qualification_valid": True,
            "source_bounds_valid": True,
            "exact_trusted_before_after": True,
            "source_workload": {
                "requests": 104,
                "successful": 104,
                "reconciliation": {"consistent": True},
            },
            "soak": {
                "complete": 1,
                "phases": {"complete": 1},
                "sources": 104,
                "attempts": {"stable": 100000},
                "comparisons": {"agree": 100000},
                "source_attempts": {"stable": 104},
                "source_comparisons": {"agree": 104},
            },
        }

    def rollback(self, _baseline, _previous_id):
        self.assert_true(self.locked)
        self.rollback_calls += 1
        if self.failure == "signal_rollback":
            self.signal_pending = True
        if self.rollback_failure:
            raise gate.recovery.GateError("rollback_failed", "injected")
        if self.failure == "unexpected_rollback":
            raise OSError("injected")
        return (
            0.25,
            identity("restored", "baseline@sha256:abc"),
            ((10, 20), (2560, 5120)),
        )


class Node06ShadowSoakGateTests(unittest.TestCase):
    def run_gate(self, output, runtime):
        args = argparse.Namespace(output=pathlib.Path(output))
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
            io.StringIO()
        ):
            return gate.run_gate(args, runtime)

    def test_success_is_journaled_after_verified_rollback(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            runtime = FakeRuntime()
            self.assertEqual(self.run_gate(output, runtime), 0)
            self.assertEqual(runtime.deploy_calls, 1)
            self.assertEqual(runtime.rollback_calls, 1)
            record = json.loads(output.read_text())
            self.assertEqual(record["status"], "passed")
            self.assertEqual(record["rollback"]["status"], "passed")

    def test_workload_failure_keeps_bounded_payload_and_rolls_back(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            runtime = FakeRuntime(failure="workload")
            self.assertEqual(self.run_gate(output, runtime), 1)
            self.assertEqual(runtime.rollback_calls, 1)
            record = json.loads(output.read_text())
            self.assertEqual(record["reason"], "workload_failed")
            self.assertEqual(record["workload"]["type"], "shadow_soak_source_failure")
            self.assertEqual(record["rollback"]["status"], "passed")

    def test_deploy_failure_still_rolls_back_and_rollback_failure_dominates(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            runtime = FakeRuntime(failure="deploy", rollback_failure=True)
            self.assertEqual(self.run_gate(output, runtime), 1)
            self.assertEqual(runtime.rollback_calls, 1)
            record = json.loads(output.read_text())
            self.assertEqual(record["reason"], "rollback_failed")
            self.assertEqual(record["rollback"]["status"], "failed")

    def test_preflight_failure_never_mutates_or_rolls_back(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            runtime = FakeRuntime(failure="preflight")
            self.assertEqual(self.run_gate(output, runtime), 1)
            self.assertEqual(runtime.deploy_calls, 0)
            self.assertEqual(runtime.rollback_calls, 0)
            record = json.loads(output.read_text())
            self.assertEqual(record["reason"], "preflight_failed")
            self.assertNotIn("rollback", record)

    def test_interruption_during_deploy_and_rollback_both_restore_baseline(self):
        for failure in ("interrupt_deploy", "signal_rollback"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                output = pathlib.Path(directory) / "result.json"
                runtime = FakeRuntime(failure=failure)
                self.assertEqual(self.run_gate(output, runtime), 1)
                self.assertEqual(runtime.rollback_calls, 1)
                record = json.loads(output.read_text())
                self.assertEqual(record["reason"], "interrupted")
                self.assertEqual(record["rollback"]["status"], "passed")

    def test_invalid_exit_zero_payload_and_unexpected_rollback_are_fail_closed(self):
        for failure, reason in (
            ("invalid_workload", "workload_qualification_invalid"),
            ("unexpected_rollback", "rollback_failed"),
        ):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                output = pathlib.Path(directory) / "result.json"
                runtime = FakeRuntime(failure=failure)
                self.assertEqual(self.run_gate(output, runtime), 1)
                record = json.loads(output.read_text())
                self.assertEqual(record["reason"], reason)

    def test_token_file_is_owner_only_regular_bounded_and_single_valued(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            token_file = root / ".env"
            token_file.write_text("VLLM_API_KEY=" + "a" * 32 + "\n")
            token_file.chmod(0o600)
            runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
            runtime.directory = root
            runtime.args = argparse.Namespace(env_file=".env")
            self.assertEqual(runtime._load_token(), "a" * 32)
            token_file.chmod(0o644)
            with self.assertRaises(gate.recovery.GateError):
                runtime._load_token()
            token_file.unlink()
            target = root / "token"
            target.write_text("VLLM_API_KEY=" + "b" * 32 + "\n")
            target.chmod(0o600)
            token_file.symlink_to(target)
            with self.assertRaises(gate.recovery.GateError):
                runtime._load_token()

    def test_profile_poll_never_swallows_interrupt(self):
        runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
        runtime.args = argparse.Namespace(profile_timeout_seconds=60, poll_interval_ms=10)
        runtime.inspect = mock.Mock(side_effect=gate.GateInterrupted())
        with self.assertRaises(gate.GateInterrupted):
            runtime._wait_profile(
                baseline(), "sha256:candidate", "hash-candidate", "capture", "old"
            )

    def test_compose_project_is_required_for_every_identity(self):
        runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
        runtime.args = argparse.Namespace(compose_project_name="dspark_0731")
        runtime._assert_compose_project((identity("lb"), identity("engine")))
        foreign = dataclasses.replace(identity("companion"), compose_project="other")
        with self.assertRaises(gate.recovery.GateError) as caught:
            runtime._assert_compose_project((identity("lb"), foreign))
        self.assertEqual(caught.exception.reason, "compose_project_mismatch")

    def test_argument_validation_requires_digest_baseline_and_fills_fixed_endpoints(self):
        candidate = "sha256:" + "a" * 64
        parsed = gate.parser().parse_args(
            [
                "--candidate-image",
                candidate,
                "--expected-baseline-image",
                "repo/image:tag@sha256:" + "b" * 64,
                "--salt",
                "fresh",
                "--output",
                "/tmp/result.json",
            ]
        )
        with mock.patch(
            "node06_shadow_soak_gate.gpu_guard.validate_inherited_guard",
            return_value=GUARD_CONTRACT,
        ) as validator:
            gate.validate_args(parsed)
        validator.assert_called_once_with(expected_gpus=8, maximum_abort_c=78)
        self.assertEqual(len(parsed.engine_metrics), 2)
        self.assertEqual(len(parsed.companion_metrics_socket), 2)
        self.assertEqual(parsed.health_url, "http://127.0.0.1:8006/health")
        parsed.expected_baseline_image = "repo/image:mutable"
        with mock.patch(
            "node06_shadow_soak_gate.gpu_guard.validate_inherited_guard",
            return_value=GUARD_CONTRACT,
        ), self.assertRaises(gate.recovery.GateError):
            gate.validate_args(parsed)

    def test_shadow_soak_requires_the_conservative_eight_gpu_guard(self):
        candidate = "sha256:" + "a" * 64
        parsed = gate.parser().parse_args(
            [
                "--candidate-image",
                candidate,
                "--expected-baseline-image",
                "repo/image:tag@sha256:" + "b" * 64,
                "--salt",
                "fresh",
                "--output",
                "/tmp/result.json",
            ]
        )
        cases = ("missing capability", "invalid capability")
        for failure in cases:
            with self.subTest(failure=failure), mock.patch(
                "node06_shadow_soak_gate.gpu_guard.validate_inherited_guard",
                side_effect=gate.gpu_guard.GuardError(failure),
            ), self.assertRaises(gate.recovery.GateError) as caught:
                gate.validate_args(parsed)
            self.assertEqual(caught.exception.reason, "thermal_guard_required")

    def test_signal_is_latched_and_never_raises_after_rollback_begins(self):
        runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
        runtime._interrupt_requested = False
        runtime._rollback_active = False
        runtime.handle_signal()
        with self.assertRaises(gate.GateInterrupted):
            runtime._raise_if_interrupted()
        runtime._interrupt_requested = False
        runtime.begin_rollback()
        runtime.handle_signal()
        runtime._raise_if_interrupted()
        self.assertTrue(runtime.end_rollback())

    def test_child_output_is_stopped_at_the_streaming_cap(self):
        runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
        runtime.args = argparse.Namespace(
            workload_timeout_seconds=5,
            max_child_output_bytes=64 * 1024,
        )
        runtime._child = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdout.write('x' * 131072)"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            with self.assertRaises(gate.recovery.GateError) as caught:
                runtime._read_child_bounded()
            self.assertEqual(caught.exception.reason, "workload_output_too_large")
        finally:
            runtime._terminate_child()

    def test_failure_payload_whitelist_cannot_journal_arbitrary_json(self):
        payload = {
            "type": "shadow_soak_source_failure",
            "prompt": "secret prompt",
            "source_workload": {
                "requests": 104,
                "responses": ["secret response"],
                "retry_reasons": {
                    "tokenizer_unavailable": 2,
                    "attacker_label": "credential",
                },
            },
            "soak": {
                "sources": 10,
                "tokens": [1, 2, 3],
                "source_attempts": {"stable": 10, "secret": 99},
            },
        }
        sanitized = gate.sanitize_failure_payload(payload)
        encoded = json.dumps(sanitized, sort_keys=True)
        self.assertNotIn("secret", encoded)
        self.assertNotIn("credential", encoded)
        self.assertNotIn("prompt", encoded)
        self.assertEqual(sanitized["source_workload"]["requests"], 104)

    def test_plan_binds_executed_helper_modules(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            paths = {
                name: root / name
                for name in (
                    "shadow_soak.py",
                    "cachebench.py",
                    "engine_metrics.py",
                    "compose.yaml",
                    "overlay.yaml",
                    "setup.py",
                    "host_validator.py",
                    "compose_validator.py",
                )
            }
            for name, path in paths.items():
                path.write_text(name)
            runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
            runtime.directory = root
            runtime.original_workload = paths["shadow_soak.py"]
            runtime.original_base = paths["compose.yaml"]
            runtime.original_overlay = paths["overlay.yaml"]
            runtime.setup = paths["setup.py"]
            runtime.host_validator = paths["host_validator.py"]
            runtime.compose_validator = paths["compose_validator.py"]
            runtime.args = argparse.Namespace(
                candidate_image="sha256:" + "a" * 64,
                expected_baseline_image="image@sha256:" + "b" * 64,
                thermal_guard=GUARD_CONTRACT,
            )
            runtime._bound_plan = None
            bound = runtime.plan()
            self.assertIn("cachebench", bound)
            self.assertIn("engine_metrics", bound)
            self.assertEqual(
                bound["thermal_guard"], GUARD_CONTRACT
            )
            paths["cachebench.py"].write_text("changed")
            with self.assertRaises(gate.recovery.GateError):
                runtime.plan()

    def test_freeze_proves_baseline_and_candidate_renders_before_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name in (
                "compose.yaml",
                "overlay.yaml",
                "shadow_soak.py",
                "cachebench.py",
                "engine_metrics.py",
            ):
                (root / name).write_text(name)
            payload = b"VLLM_API_KEY=" + b"a" * 32 + b"\n"
            (root / ".env").write_bytes(payload)
            (root / ".env").chmod(0o600)
            runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
            runtime.directory = root
            runtime.original_base = root / "compose.yaml"
            runtime.original_overlay = root / "overlay.yaml"
            runtime.original_workload = root / "shadow_soak.py"
            runtime.base = runtime.original_base
            runtime.overlay = runtime.original_overlay
            runtime.workload = runtime.original_workload
            runtime.args = argparse.Namespace(
                env_file=".env", candidate_image="sha256:" + "a" * 64
            )
            runtime._bound_env_digest = gate.hashlib.sha256(payload).digest()
            runtime._frozen_directory = None
            runtime._frozen_env = None
            runtime._interrupt_requested = False
            runtime._rollback_active = False
            runtime.plan = mock.Mock(return_value={})
            runtime._profile_hash = mock.Mock(
                side_effect=["hash-baseline", "hash-candidate"]
            )
            runtime._assert_original_artifacts = mock.Mock()
            runtime.freeze_artifacts(baseline())
            self.assertNotEqual(runtime.base, runtime.original_base)
            self.assertEqual(
                runtime._profile_hash.call_args_list,
                [
                    mock.call("baseline@sha256:abc", "off"),
                    mock.call("sha256:" + "a" * 64, "capture"),
                ],
            )
            runtime._assert_original_artifacts.assert_called_once_with()
            runtime.cleanup_frozen_artifacts()

    def test_rollback_uses_frozen_files_after_raw_artifact_io_failure(self):
        runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
        runtime.args = argparse.Namespace(profile_timeout_seconds=1)
        runtime.original_base = pathlib.Path("/missing/base.yaml")
        runtime.original_overlay = pathlib.Path("/missing/overlay.yaml")
        runtime.base = pathlib.Path("/frozen/base.yaml")
        runtime.overlay = pathlib.Path("/frozen/overlay.yaml")
        runtime._assert_original_artifacts = mock.Mock(side_effect=FileNotFoundError())
        runtime._terminate_child = mock.Mock()
        runtime._profile_env = mock.Mock(return_value={})
        runtime._compose = mock.Mock(return_value=b"")
        runtime._wait_profile = mock.Mock(
            return_value=(identity("restored", "baseline@sha256:abc"), ((10, 20),))
        )
        with self.assertRaises(gate.recovery.GateError) as caught:
            runtime.rollback(baseline(), "candidate-id")
        self.assertEqual(caught.exception.reason, "rollback_artifact_changed")
        self.assertEqual(
            runtime._compose.call_args.args[0], (runtime.base, runtime.overlay)
        )

    def test_rollback_switches_to_frozen_files_after_canonical_attempt_fails(self):
        runtime = gate.NodeShadowRuntime.__new__(gate.NodeShadowRuntime)
        runtime.args = argparse.Namespace(profile_timeout_seconds=1)
        runtime.original_base = pathlib.Path("/canonical/base.yaml")
        runtime.original_overlay = pathlib.Path("/canonical/overlay.yaml")
        runtime.base = pathlib.Path("/frozen/base.yaml")
        runtime.overlay = pathlib.Path("/frozen/overlay.yaml")
        runtime._assert_original_artifacts = mock.Mock()
        runtime._terminate_child = mock.Mock()
        runtime._profile_env = mock.Mock(return_value={})
        runtime._compose = mock.Mock(side_effect=[OSError("changed"), b""])
        runtime._wait_profile = mock.Mock(
            return_value=(identity("restored", "baseline@sha256:abc"), ((10, 20),))
        )
        with mock.patch.object(gate.time, "sleep"), self.assertRaises(
            gate.recovery.GateError
        ) as caught:
            runtime.rollback(baseline(), "candidate-id")
        self.assertEqual(caught.exception.reason, "rollback_artifact_changed")
        self.assertEqual(
            [call.args[0] for call in runtime._compose.call_args_list],
            [
                (runtime.original_base, runtime.original_overlay),
                (runtime.base, runtime.overlay),
            ],
        )


if __name__ == "__main__":
    unittest.main()
