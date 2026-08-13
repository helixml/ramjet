import argparse
import contextlib
import dataclasses
import io
import json
import pathlib
import sys
import tempfile
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import snapshot_recovery_gate as gate


READY_METRICS = """
ds4proxy_snapshot_companion_enabled 1
ds4proxy_snapshot_companion_authority 1
ds4proxy_snapshot_companion_listening{engine="engine-0"} 1
ds4proxy_snapshot_companion_ready{engine="engine-0"} 1
ds4proxy_snapshot_companion_source_ready{engine="engine-0"} 1
ds4proxy_snapshot_companion_source_watermark_present{engine="engine-0"} 1
ds4proxy_snapshot_companion_source_phase{engine="engine-0",phase="ready"} 1
ds4proxy_snapshot_companion_source_indexed_blocks{engine="engine-0"} 36612
ds4proxy_snapshot_companion_owner_events_total{event="connect",reason="attempt"} 1
ds4proxy_snapshot_companion_owner_events_total{event="connect",reason="connected"} 1
""".strip()

LB_READY_METRICS = """
ds4proxy_snapshot_route_enabled 1
ds4proxy_snapshot_route_ready{engine="engine-0"} 1
ds4proxy_snapshot_route_ready{engine="engine-1"} 1
ds4proxy_snapshot_route_attempts_active{engine="engine-0"} 0
ds4proxy_snapshot_route_attempts_active{engine="engine-1"} 0
ds4proxy_snapshot_route_connections_active{engine="engine-0"} 1
ds4proxy_snapshot_route_connections_active{engine="engine-1"} 1
""".strip()


def identity(name, image="image@sha256:abc"):
    return gate.ContainerIdentity(
        container_id=f"id-{name}",
        image_id=f"sha256:{name}",
        configured_image=image,
        started_at="2026-08-13T18:00:00.000000000Z",
        restart_count=0,
        running=True,
        health="healthy" if "companion" in name else "none",
        config_hash=f"hash-{name}",
    )


def baseline():
    return gate.Baseline(
        lb=identity("lb", "baseline@sha256:abc"),
        engines=(identity("engine-a"), identity("engine-b")),
        companions=(identity("companion-a"), identity("companion-b")),
        baseline_hash="hash-lb",
        shadow_hash="hash-shadow",
        shadow_image="shadow@sha256:def",
    )


def args(output, apply=False, iterations=5):
    return argparse.Namespace(
        output=pathlib.Path(output),
        apply=apply,
        iterations=iterations,
        recovery_slo_seconds=3.0,
    )


class FakeRuntime:
    def __init__(self, readiness, samples=None, failure=None, rollback_failure=None):
        self.readiness = readiness
        self.samples = list(samples or [])
        self.failure = failure
        self.rollback_failure = rollback_failure
        self.shadow_calls = 0
        self.rollback_calls = 0
        self.locked = False
        self.rollback_while_locked = False

    @contextlib.contextmanager
    def lock(self, exclusive):
        self.locked = True
        try:
            yield
        finally:
            self.locked = False

    def preflight(self):
        return baseline()

    def plan(self):
        return {"plan_version": 1}

    def companion_readiness(self):
        return self.readiness

    def enable_shadow_and_measure(self, _baseline, iteration):
        if not self.locked:
            raise AssertionError("shadow mutation escaped the deployment lock")
        self.shadow_calls += 1
        if self.failure is not None and self.shadow_calls == self.failure[0]:
            raise gate.GateError(self.failure[1], "injected failure")
        return self.samples[iteration - 1]

    def rollback(self, _baseline):
        self.rollback_while_locked = self.locked
        self.rollback_calls += 1
        if self.rollback_failure is not None:
            raise gate.GateError("rollback_failed", "injected rollback failure")
        return 0.25


class SnapshotRecoveryGateTest(unittest.TestCase):
    def readiness(self, ready=True):
        snapshot = gate.companion_snapshot(gate.parse_prometheus(READY_METRICS))
        if not ready:
            snapshot["source_ready"] = 0
        return (snapshot, dict(snapshot)), (ready, ready)

    def samples(self, values):
        return [
            gate.RecoverySample(
                iteration=index,
                recovery_seconds=value,
                recreate_to_ready_seconds=value + 0.2,
                resident_blocks=(100, 120),
                resident_tokens=(25600, 30720),
            )
            for index, value in enumerate(values, 1)
        ]

    def run_quiet(self, arguments, runtime):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
            io.StringIO()
        ):
            return gate.run_gate(arguments, runtime=runtime)

    def test_parses_authoritative_companion_and_lb_metrics(self):
        snapshot = gate.companion_snapshot(gate.parse_prometheus(READY_METRICS))
        self.assertTrue(gate.companion_is_ready(snapshot))
        self.assertEqual(snapshot["indexed_blocks"], 36612)
        self.assertTrue(gate.lb_snapshot_ready(gate.parse_prometheus(LB_READY_METRICS)))

    def test_missing_or_duplicate_metrics_fail_closed(self):
        snapshot = gate.companion_snapshot(
            gate.parse_prometheus(
                READY_METRICS.replace(
                    'source_ready{engine="engine-0"} 1',
                    'source_ready{engine="engine-0"} 0',
                )
            )
        )
        self.assertFalse(gate.companion_is_ready(snapshot))
        with self.assertRaises(gate.GateError) as caught:
            gate.parse_prometheus(READY_METRICS + "\n" + READY_METRICS.splitlines()[0])
        self.assertEqual(caught.exception.reason, "duplicate_metric")

    def test_health_requires_two_ordered_healthy_exact_inventories(self):
        payload = {
            "status": "ok",
            "healthy_replicas": 2,
            "total_replicas": 2,
            "replicas": [
                {
                    "index": 0,
                    "healthy": True,
                    "exact_inventory": {
                        "trusted": True,
                        "resident_blocks": 10,
                        "resident_tokens": 2560,
                    },
                },
                {
                    "index": 1,
                    "healthy": True,
                    "exact_inventory": {
                        "trusted": True,
                        "resident_blocks": 20,
                        "resident_tokens": 5120,
                    },
                },
            ],
        }
        self.assertEqual(gate.validate_health(payload, True), ((10, 20), (2560, 5120)))
        payload["replicas"][1]["exact_inventory"]["trusted"] = False
        self.assertIsNone(gate.validate_health(payload, True))
        self.assertIsNotNone(gate.validate_health(payload, False))

    def test_audit_refuses_to_mutate_when_a_companion_is_fenced(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "audit.json"
            runtime = FakeRuntime(self.readiness(ready=False))
            result = self.run_quiet(args(output), runtime)
            self.assertEqual(result, 3)
            self.assertEqual(runtime.shadow_calls, 0)
            self.assertEqual(runtime.rollback_calls, 0)
            record = json.loads(output.read_text())
            self.assertEqual(record["status"], "not_ready")
            self.assertEqual(record["reason"], "companion_source_not_ready")

    def test_apply_also_refuses_fenced_sources_before_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "apply-not-ready.json"
            runtime = FakeRuntime(self.readiness(ready=False))
            result = self.run_quiet(args(output, apply=True), runtime)
            self.assertEqual(result, 3)
            self.assertEqual(runtime.shadow_calls, 0)
            self.assertEqual(runtime.rollback_calls, 0)

    def test_apply_records_five_samples_and_always_rolls_back(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "pass.json"
            runtime = FakeRuntime(
                self.readiness(), self.samples([0.4, 0.5, 0.45, 0.55, 0.6])
            )
            result = self.run_quiet(args(output, apply=True), runtime)
            self.assertEqual(result, 0)
            self.assertEqual(runtime.shadow_calls, 5)
            self.assertEqual(runtime.rollback_calls, 1)
            self.assertTrue(runtime.rollback_while_locked)
            record = json.loads(output.read_text())
            self.assertEqual(record["status"], "passed")
            self.assertEqual(record["recovery_p95_seconds"], 0.6)
            self.assertEqual(record["rollback_status"], "passed")

    def test_slo_failure_is_recorded_after_successful_rollback(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "slow.json"
            runtime = FakeRuntime(
                self.readiness(), self.samples([0.4, 0.5, 0.6, 0.7, 3.1])
            )
            result = self.run_quiet(args(output, apply=True), runtime)
            self.assertEqual(result, 1)
            self.assertEqual(runtime.rollback_calls, 1)
            self.assertTrue(runtime.rollback_while_locked)
            record = json.loads(output.read_text())
            self.assertEqual(record["reason"], "recovery_slo_exceeded")
            self.assertEqual(record["rollback_status"], "passed")

    def test_midrun_failure_rolls_back_and_rollback_failure_dominates(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "failed.json"
            runtime = FakeRuntime(
                self.readiness(),
                self.samples([0.4] * 5),
                failure=(2, "engine_identity_changed"),
                rollback_failure=True,
            )
            result = self.run_quiet(args(output, apply=True), runtime)
            self.assertEqual(result, 1)
            self.assertEqual(runtime.shadow_calls, 2)
            self.assertEqual(runtime.rollback_calls, 1)
            self.assertTrue(runtime.rollback_while_locked)
            record = json.loads(output.read_text())
            self.assertEqual(record["status"], "failed")
            self.assertEqual(record["reason"], "rollback_failed")

    def test_journal_never_overwrites(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "journal.json"
            gate.write_journal(output, {"status": "first"})
            with self.assertRaises(gate.GateError) as caught:
                gate.write_journal(output, {"status": "second"})
            self.assertEqual(caught.exception.reason, "journal_create_failed")
            self.assertEqual(json.loads(output.read_text())["status"], "first")

    def test_nearest_rank_p95_is_conservative_for_five_restarts(self):
        self.assertEqual(gate.percentile_nearest_rank([0.4, 0.5, 0.6, 0.7, 0.8], 0.95), 0.8)

    def test_engine_identity_comparison_detects_restart_or_replacement(self):
        original = identity("engine-a")
        self.assertTrue(gate.same_engine_identity(original, original))
        self.assertFalse(
            gate.same_engine_identity(
                original, dataclasses.replace(original, restart_count=1)
            )
        )
        self.assertFalse(
            gate.same_engine_identity(
                original, dataclasses.replace(original, container_id="replacement")
            )
        )


if __name__ == "__main__":
    unittest.main()
