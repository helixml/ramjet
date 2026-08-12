import argparse
import json
import pathlib
import tempfile
import unittest
from unittest import mock

from candidate_gate import (
    CommandResult,
    ContainerIdentity,
    GateError,
    SubprocessRunner,
    run_gate,
)


class FakeRunner:
    def __init__(self, identity, failures=None, logs=None, inspect_sequence=None):
        self.identity = identity
        self.failures = failures or set()
        self.log_bodies = logs or {}
        self.inspect_sequence = list(inspect_sequence or [])
        self.commands = []

    def inspect(self, container):
        self.commands.append(("inspect", container))
        if self.inspect_sequence:
            return self.inspect_sequence.pop(0)
        return self.identity

    def run(self, argv, env=None):
        label = next((value for value in argv if value.startswith("candidate-gate-")), None)
        if "agentbench.py" in " ".join(argv):
            stage = "agent_correctness"
        elif label == "candidate-gate-scout":
            stage = "c8_scout"
        elif label == "candidate-gate-matrix":
            stage = "full_matrix"
        else:
            stage = "unknown"
        self.commands.append((stage, tuple(sorted((env or {}).items()))))
        code = 1 if stage in self.failures else 0
        return CommandResult(code, f'{stage}\n'.encode(), b"synthetic stderr")

    def logs(self, container, since):
        last_stage = next(
            command[0]
            for command in reversed(self.commands)
            if command[0] not in {"inspect"}
        )
        self.commands.append(("logs", container))
        return CommandResult(0, self.log_bodies.get(last_stage, b""), b"")


class CandidateGateTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temporary.name)
        self.engine_path = self.root / "engine.json"
        self.agent_path = self.root / "agent.json"
        self.output = self.root / "gate.jsonl"
        self.artifacts = self.root / "artifacts"
        self.live = {
            "configured_image": "example.invalid/engine@sha256:manifest",
            "image_id": "sha256:manifest",
            "image_descriptor_digest": "sha256:manifest",
            "image_config_digest": "sha256:config",
            "model_revision": "model-revision",
            "tokenizer_revision": "model-revision",
            "tokenizer_sha256": "a" * 64,
            "config_sha256": "b" * 64,
            "runtime_packages": {"vllm": "1.0"},
            "effective_contract": {"max_model_len": "393216"},
            "argv_sha256": "c" * 64,
            "started_at": "2026-08-13T00:00:00Z",
            "restart_count": 0,
        }
        self.engine_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "live": self.live,
                    "receipt": None,
                    "verified": None,
                }
            ),
            encoding="utf-8",
        )
        self.agent = {
            "engine_image": self.live["configured_image"],
            "model_revision": self.live["model_revision"],
            "tokenizer_sha256": self.live["tokenizer_sha256"],
            "config_sha256": self.live["config_sha256"],
            "router_version": "direct-engine",
            "gpu_count": 4,
        }
        self.agent_path.write_text(json.dumps(self.agent), encoding="utf-8")
        self.identity = ContainerIdentity(
            image_id=self.live["image_id"],
            configured_image=self.live["configured_image"],
            started_at=self.live["started_at"],
            restart_count=0,
            running=True,
        )

    def tearDown(self):
        self.temporary.cleanup()

    def args(self, through="smoke", resume=False):
        return argparse.Namespace(
            base="http://127.0.0.1:8013",
            model="deepseek-v4-flash",
            container="candidate",
            engine_metadata=self.engine_path,
            agent_metadata=self.agent_path,
            output=self.output,
            artifacts_dir=self.artifacts,
            through=through,
            resume=resume,
        )

    def records(self):
        return [
            json.loads(line)
            for line in self.output.read_text(encoding="utf-8").splitlines()
        ]

    def stages_run(self, runner):
        return [
            command[0]
            for command in runner.commands
            if command[0] in {"agent_correctness", "c8_scout", "full_matrix"}
        ]

    def test_full_gate_runs_strict_order_and_reaches_matrix_only_after_scout(self):
        runner = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args("matrix"), runner), 0)
        self.assertEqual(
            self.stages_run(runner),
            ["agent_correctness", "c8_scout", "full_matrix"],
        )
        self.assertEqual(
            [record["status"] for record in self.records()],
            ["passed", "passed", "passed", "passed"],
        )
        self.assertEqual(
            {path.name for path in self.artifacts.iterdir()},
            {"agent_correctness.jsonl", "c8_scout.jsonl", "full_matrix.jsonl"},
        )

    def test_agent_failure_stops_before_any_performance_work(self):
        runner = FakeRunner(self.identity, failures={"agent_correctness"})
        self.assertEqual(run_gate(self.args("matrix"), runner), 1)
        self.assertEqual(self.stages_run(runner), ["agent_correctness"])
        failed = self.records()[-1]
        self.assertEqual(failed["status"], "failed")
        self.assertEqual(failed["error_class"], "benchmark_failed")

    def test_late_jit_marker_fails_the_current_stage(self):
        runner = FakeRunner(
            self.identity,
            logs={"agent_correctness": b"CuTeDSL compiling a new kernel"},
        )
        self.assertEqual(run_gate(self.args("matrix"), runner), 1)
        failed = self.records()[-1]
        self.assertEqual(failed["error_class"], "runtime_marker")
        self.assertEqual(failed["runtime_markers"], ["jit_compilation"])
        self.assertEqual(self.stages_run(runner), ["agent_correctness"])

    def test_container_restart_between_boundaries_fails_closed(self):
        restarted = ContainerIdentity(
            **{
                **self.identity.__dict__,
                "started_at": "2026-08-13T00:05:00Z",
                "restart_count": 1,
            }
        )
        runner = FakeRunner(
            self.identity,
            inspect_sequence=[self.identity, self.identity, restarted],
        )
        self.assertEqual(run_gate(self.args("matrix"), runner), 1)
        failed = self.records()[-1]
        self.assertEqual(failed["error_class"], "identity_changed")
        self.assertNotIn("full_matrix", self.stages_run(runner))

    def test_resume_skips_green_smoke_only_for_same_candidate_and_plan(self):
        first = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args("smoke"), first), 0)

        resumed = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args("scout", resume=True), resumed), 0)
        self.assertEqual(self.stages_run(resumed), ["c8_scout"])
        self.assertEqual(self.records()[-2]["status"], "resumed")

        self.agent["router_version"] = "changed-plan"
        self.agent_path.write_text(json.dumps(self.agent), encoding="utf-8")
        with self.assertRaisesRegex(GateError, "plan does not match"):
            run_gate(self.args("matrix", resume=True), FakeRunner(self.identity))

    def test_journal_excludes_commands_environment_and_child_output(self):
        runner = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args("smoke"), runner), 0)
        journal = self.output.read_text(encoding="utf-8")
        self.assertNotIn("synthetic stderr", journal)
        self.assertNotIn("agentbench.py", journal)
        self.assertNotIn("BENCH_TOKEN", journal)
        record = self.records()[-1]
        self.assertIn("artifact_sha256", record)
        self.assertIn("stderr_sha256", record)
        self.assertIn("wall_seconds", record)

    def test_metadata_mismatch_is_rejected_before_container_access(self):
        self.agent["model_revision"] = "wrong"
        self.agent_path.write_text(json.dumps(self.agent), encoding="utf-8")
        runner = FakeRunner(self.identity)
        with self.assertRaisesRegex(GateError, "model_revision"):
            run_gate(self.args(), runner)
        self.assertEqual(runner.commands, [])

    def test_real_runner_uses_a_secret_free_bounded_inspect_format(self):
        completed = mock.Mock(
            returncode=0,
            stdout=(
                b"sha256:manifest\texample.invalid/engine@sha256:manifest\t"
                b"2026-08-13T00:00:00Z\t0\ttrue\n"
            ),
            stderr=b"",
        )
        with (
            mock.patch("candidate_gate.subprocess.run", return_value=completed) as called,
            mock.patch.dict(
                "candidate_gate.os.environ",
                {"BENCH_TOKEN": "secret", "BENCH_PROMPT": "uncontrolled"},
                clear=True,
            ),
        ):
            identity = SubprocessRunner().inspect("candidate")
        self.assertEqual(identity, self.identity)
        argv = called.call_args.args[0]
        self.assertIn("\t", argv[3])
        self.assertNotIn("{{json .}}", argv[3])
        self.assertNotIn("Env", argv[3])
        child_env = called.call_args.kwargs["env"]
        self.assertEqual(child_env["BENCH_TOKEN"], "secret")
        self.assertNotIn("BENCH_PROMPT", child_env)


if __name__ == "__main__":
    unittest.main()
