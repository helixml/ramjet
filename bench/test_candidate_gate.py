import argparse
import base64
import fcntl
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

import node06_gpu_guard as gpu_guard
from unittest import mock

from candidate_gate import (
    CommandResult,
    ContainerIdentity,
    GateError,
    MAX_ARTIFACT_BYTES,
    NODE06_PROFILE,
    PERFORMANCE_ENV_NAMES,
    PERFORMANCE_ENV_PREFIXES,
    PROCESS_PROBE,
    ProcessIdentity,
    RUNTIME_SECRET_ENV_NAMES,
    SubprocessRunner,
    admission_contract,
    build_stages,
    plan_contract,
    performance_environment_name,
    run_gate,
    validate_node06_profile,
)
from node06_gpu_guard import GuardError


class FakeRunner:
    def __init__(
        self,
        identity,
        process_identity=None,
        process_sequence=None,
        failures=None,
        logs=None,
        inspect_sequence=None,
        environment=None,
        device_ids=("4", "5", "6", "7"),
        unhealthy=None,
    ):
        self.identity = identity
        self.live_process_identity = process_identity or ProcessIdentity(
            process_started_unix_ns=1_765_000_000_000_000_000,
            serving_argv_sha256="598a2b1db89625a599b84614b4d57bdd990d8644cc4a5602c00dd0e973b2a2a4",
            environment_sha256="faff2d6ad7584cebfa0dd3f53cdf997c858b60e13a4e617a382fcdebaeb5d896",
            artifacts_sha256="11563d331a8a4d07c981f3ae7460194f21899791612f053f705fbbb0465a984b",
        )
        self.process_sequence = list(process_sequence or [])
        self.failures = failures or set()
        self.log_bodies = logs or {}
        self.inspect_sequence = list(inspect_sequence or [])
        self.live_environment = environment or {
            "RJ_UPSTREAM": "http://engine-a:8000",
            "RJ_KV_EVENT_LIVE_ENDPOINTS": "tcp://engine-a:5557",
            "RJ_KV_EVENT_REPLAY_ENDPOINTS": "tcp://engine-a:5558",
        }
        self.live_device_ids = device_ids
        self.unhealthy = set(unhealthy or ())
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
            if command[0]
            not in {"inspect", "process_identity", "environment", "device_ids", "health"}
        )
        self.commands.append(("logs", container))
        return CommandResult(0, self.log_bodies.get(last_stage, b""), b"")

    def environment(self, container):
        self.commands.append(("environment", container))
        return self.live_environment

    def device_ids(self, container):
        self.commands.append(("device_ids", container))
        return self.live_device_ids

    def process_identity(self, container, environment_names, artifact_paths):
        self.commands.append(("process_identity", container))
        if self.process_sequence:
            return self.process_sequence.pop(0)
        return self.live_process_identity

    def health(self, url):
        self.commands.append(("health", url))
        if url in self.unhealthy:
            raise GateError("engine health probe failed")


class CandidateGateTest(unittest.TestCase):
    def setUp(self):
        self.guard_contract = {
            "expected_gpus": 8,
            "abort_c": 78.0,
            "run_id": "1" * 32,
        }
        self.guard_patch = mock.patch(
            "candidate_gate.gpu_guard.validate_inherited_guard",
            return_value=self.guard_contract,
        )
        self.guard_validator = self.guard_patch.start()
        self.temporary = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temporary.name)
        self.engine_path = self.root / "engine.json"
        self.agent_path = self.root / "agent.json"
        self.output = self.root / "gate.jsonl"
        self.artifacts = self.root / "artifacts"
        self.candidate_manifest_path = self.root / "candidate-manifest.json"
        self.runtime_manifest_path = self.root / "runtime-manifest.json"
        repository = pathlib.Path(__file__).resolve().parents[1]
        committed = repository / "deploy/dspark_0731/infernal-r11-candidate"
        self.candidate_manifest_path.write_bytes((committed / "manifest.json").read_bytes())
        self.runtime_manifest_path.write_bytes(
            (committed / "serving-runtime.json").read_bytes()
        )
        self.candidate_manifest_path.chmod(0o600)
        self.runtime_manifest_path.chmod(0o600)
        admission = admission_contract(
            self.candidate_manifest_path, self.runtime_manifest_path
        )
        self.live = {
            "configured_image": admission["configured_image"],
            # Docker 29/containerd may expose the manifest descriptor as
            # ``.Image`` while the separately captured config digest remains
            # the traditional image ID.
            "image_id": admission["image_descriptor_digest"],
            "image_descriptor_digest": admission["image_descriptor_digest"],
            "image_config_digest": admission["image_config_digest"],
            "model_revision": admission["model_revision"],
            "tokenizer_revision": admission["tokenizer_revision"],
            "tokenizer_sha256": "a" * 64,
            "config_sha256": "b" * 64,
            "runtime_packages": admission["runtime_packages"],
            "effective_contract": {"max_model_len": "393216"},
            "argv_sha256": "c" * 64,
            "serving_argv_sha256": admission["serving_argv_sha256"],
            "process_started_unix_ns": 1_765_000_000_000_000_000,
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
        self.engine_path.chmod(0o600)
        self.agent = {
            "engine_image": self.live["configured_image"],
            "model_revision": self.live["model_revision"],
            "tokenizer_sha256": self.live["tokenizer_sha256"],
            "config_sha256": self.live["config_sha256"],
            "router_version": "direct-engine",
            "gpu_count": 4,
        }
        self.agent_path.write_text(json.dumps(self.agent), encoding="utf-8")
        self.agent_path.chmod(0o600)
        self.identity = ContainerIdentity(
            image_id=self.live["image_id"],
            configured_image=self.live["configured_image"],
            started_at=self.live["started_at"],
            restart_count=0,
            running=True,
        )

    def tearDown(self):
        self.temporary.cleanup()
        self.guard_patch.stop()

    def args(self, through="smoke", resume=False):
        return argparse.Namespace(
            profile="infernal-r11-b",
            base="http://127.0.0.1:8013",
            model="deepseek-v4-flash",
            container="candidate",
            candidate_manifest=self.candidate_manifest_path,
            runtime_manifest=self.runtime_manifest_path,
            expected_gpu_count=4,
            engine_metrics="http://127.0.0.1:8013/metrics",
            deployment_lock=self.root / "deployment.lock",
            load_balancer_container="load-balancer",
            expected_lb_upstream="http://engine-a:8000",
            expected_lb_live_endpoints="tcp://engine-a:5557",
            expected_lb_replay_endpoints="tcp://engine-a:5558",
            expected_device_ids=("4", "5", "6", "7"),
            healthy_peer_health_url="http://127.0.0.1:8012/health",
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

    def run_process_probe(
        self,
        *,
        process_argv=None,
        process_environment=None,
        environment_names=("B12X_MODE", "NCCL_DEBUG"),
        artifact_paths=None,
    ):
        proc_root = pathlib.Path(tempfile.mkdtemp(prefix="proc-", dir=self.root))
        pid_root = proc_root / "4242"
        pid_root.mkdir()
        argv = process_argv or (
            "/usr/bin/python3",
            "/usr/local/bin/vllm",
            "serve",
            "deepseek-v4-flash",
            "--max-model-len=393216",
        )
        environment = process_environment or (
            "HOSTNAME=fixture",
            "NCCL_DEBUG=INFO",
            "B12X_MODE=standard",
            "VLLM_API_KEY=redacted-by-probe",
        )
        start_ticks = 321
        stat_fields = ["S", *("0" for _ in range(18)), str(start_ticks)]
        (pid_root / "cmdline").write_bytes(
            b"\0".join(value.encode("utf-8") for value in argv) + b"\0"
        )
        (pid_root / "environ").write_bytes(
            b"\0".join(value.encode("utf-8") for value in environment) + b"\0"
        )
        (pid_root / "stat").write_text(
            f"4242 (vllm worker) {' '.join(stat_fields)}\n", encoding="utf-8"
        )
        boot_seconds = 1_765_000_000
        (proc_root / "stat").write_text(
            f"cpu 1 2 3 4\nbtime {boot_seconds}\n", encoding="utf-8"
        )
        paths = artifact_paths or ()
        policy = {
            "prefixes": PERFORMANCE_ENV_PREFIXES,
            "names": sorted(PERFORMANCE_ENV_NAMES),
            "secret_names": sorted(RUNTIME_SECRET_ENV_NAMES),
            "max_artifact_bytes": MAX_ARTIFACT_BYTES,
        }
        encoded = tuple(
            base64.b64encode(json.dumps(value, separators=(",", ":")).encode()).decode()
            for value in (sorted(environment_names), list(paths), policy)
        )
        result = subprocess.run(
            (sys.executable, "-c", PROCESS_PROBE, *encoded, str(proc_root)),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=5,
        )
        return result, argv, environment, boot_seconds, start_ticks

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
        artifact_stages = {path.name.rsplit("-", 1)[0] for path in self.artifacts.iterdir()}
        self.assertEqual(artifact_stages, {"agent_correctness", "c8_scout", "full_matrix"})

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
        self.assertEqual(failed["error_class"], "runtime_authority_changed")
        self.assertNotIn("full_matrix", self.stages_run(runner))

    def test_vllm_child_restart_inside_same_container_fails_closed(self):
        original = ProcessIdentity(
            process_started_unix_ns=self.live["process_started_unix_ns"],
            serving_argv_sha256="598a2b1db89625a599b84614b4d57bdd990d8644cc4a5602c00dd0e973b2a2a4",
            environment_sha256="faff2d6ad7584cebfa0dd3f53cdf997c858b60e13a4e617a382fcdebaeb5d896",
            artifacts_sha256="11563d331a8a4d07c981f3ae7460194f21899791612f053f705fbbb0465a984b",
        )
        restarted = ProcessIdentity(
            **{**original.__dict__, "process_started_unix_ns": original.process_started_unix_ns + 1}
        )
        runner = FakeRunner(
            self.identity, process_identity=original, process_sequence=[original, original, restarted]
        )
        self.assertEqual(run_gate(self.args("matrix"), runner), 1)
        self.assertEqual(self.records()[-1]["error_class"], "runtime_authority_changed")
        self.assertIn("process_started_unix_ns", self.records()[-1]["error"])

    def test_live_environment_or_artifact_drift_fails_before_requests(self):
        for field in ("environment_sha256", "artifacts_sha256"):
            process = ProcessIdentity(
                process_started_unix_ns=self.live["process_started_unix_ns"],
                serving_argv_sha256="598a2b1db89625a599b84614b4d57bdd990d8644cc4a5602c00dd0e973b2a2a4",
                environment_sha256="faff2d6ad7584cebfa0dd3f53cdf997c858b60e13a4e617a382fcdebaeb5d896",
                artifacts_sha256="11563d331a8a4d07c981f3ae7460194f21899791612f053f705fbbb0465a984b",
            )
            process = ProcessIdentity(**{**process.__dict__, field: "9" * 64})
            with self.subTest(field=field):
                runner = FakeRunner(self.identity, process_identity=process)
                self.assertEqual(run_gate(self.args(), runner), 1)
                self.assertEqual(self.stages_run(runner), [])
                self.assertIn(field, self.records()[-1]["error"])
                self.output.unlink()

    def test_profile_pins_exact_committed_admission_bytes(self):
        self.candidate_manifest_path.write_bytes(
            self.candidate_manifest_path.read_bytes() + b"\n"
        )
        self.candidate_manifest_path.chmod(0o600)
        runner = FakeRunner(self.identity)
        with self.assertRaisesRegex(GateError, "candidate_manifest_sha256"):
            run_gate(self.args(), runner)
        self.assertEqual(runner.commands, [])

    def test_admission_and_journal_reject_unsafe_files(self):
        committed = self.root / "manifest-target.json"
        committed.write_bytes(self.candidate_manifest_path.read_bytes())
        committed.chmod(0o600)
        self.candidate_manifest_path.unlink()
        self.candidate_manifest_path.symlink_to(committed)
        with self.assertRaisesRegex(GateError, "invalid JSON metadata"):
            run_gate(self.args(), FakeRunner(self.identity))

        self.candidate_manifest_path.unlink()
        self.candidate_manifest_path.write_bytes(committed.read_bytes())
        self.candidate_manifest_path.chmod(0o600)
        target = self.root / "journal-target"
        target.write_text("do not overwrite", encoding="utf-8")
        target.chmod(0o600)
        self.output.symlink_to(target)
        with self.assertRaisesRegex(GateError, "journal is unavailable"):
            run_gate(self.args(resume=True), FakeRunner(self.identity))
        self.assertEqual(target.read_text(encoding="utf-8"), "do not overwrite")

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

    def test_live_image_and_runtime_must_match_both_admission_manifests(self):
        for field, value, pattern in (
            (
                "configured_image",
                "example.invalid/other@sha256:" + "1" * 64,
                "configured_image",
            ),
            (
                "image_config_digest",
                "sha256:" + "9" * 64,
                "image_config_digest",
            ),
            (
                "serving_argv_sha256",
                "9" * 64,
                "serving_argv_sha256",
            ),
        ):
            with self.subTest(field=field):
                original = self.live[field]
                self.live[field] = value
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
                runner = FakeRunner(self.identity)
                with self.assertRaisesRegex(GateError, pattern):
                    run_gate(self.args(), runner)
                self.assertEqual(runner.commands, [])
                self.live[field] = original
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

    def test_candidate_requires_exact_four_gpu_agent_metadata(self):
        self.agent["gpu_count"] = 8
        self.agent_path.write_text(json.dumps(self.agent), encoding="utf-8")
        runner = FakeRunner(self.identity)
        with self.assertRaisesRegex(GateError, "gpu_count"):
            run_gate(self.args(), runner)
        self.assertEqual(runner.commands, [])

    def test_live_isolation_fails_before_request_work(self):
        failures = (
            FakeRunner(
                self.identity,
                environment={
                    "RJ_UPSTREAM": "http://engine-a:8000,http://candidate:8000",
                    "RJ_KV_EVENT_LIVE_ENDPOINTS": "tcp://engine-a:5557",
                    "RJ_KV_EVENT_REPLAY_ENDPOINTS": "tcp://engine-a:5558",
                },
            ),
            FakeRunner(self.identity, device_ids=("0", "1", "2", "3")),
            FakeRunner(
                self.identity,
                unhealthy={"http://127.0.0.1:8012/health"},
            ),
        )
        for runner in failures:
            with self.subTest(commands=runner.commands):
                self.assertEqual(run_gate(self.args(), runner), 1)
                self.assertEqual(self.stages_run(runner), [])
                self.assertEqual(self.records()[-1]["status"], "failed")
                self.output.unlink()

    def test_common_deployment_lock_excludes_a_second_owner(self):
        lock_path = self.args().deployment_lock
        with lock_path.open("w") as lock:
            lock_path.chmod(0o600)
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            runner = FakeRunner(self.identity)
            with self.assertRaisesRegex(GateError, "common lock"):
                run_gate(self.args(), runner)
            self.assertEqual(runner.commands, [])

    def test_cli_cannot_weaken_the_fixed_node06_isolation_profile(self):
        profile = argparse.Namespace(**NODE06_PROFILE)
        profile.deployment_lock = pathlib.Path(profile.deployment_lock)
        validate_node06_profile(profile)
        for field in NODE06_PROFILE:
            candidate = argparse.Namespace(**vars(profile))
            setattr(candidate, field, "different")
            with self.subTest(field=field), self.assertRaisesRegex(
                GateError, field
            ):
                validate_node06_profile(candidate)

    def test_committed_r11_admission_manifests_validate(self):
        admission = admission_contract(
            self.candidate_manifest_path, self.runtime_manifest_path
        )
        self.assertEqual(
            admission["image_descriptor_digest"],
            "sha256:01b973d1ae132882bcc1bf62ea232f6aabe649dd4a89b961d81f3c41cc53f971",
        )
        self.assertEqual(admission["runtime_packages"]["b12x"], "1.2.3")
        self.assertEqual(
            admission["serving_argv_sha256"],
            "598a2b1db89625a599b84614b4d57bdd990d8644cc4a5602c00dd0e973b2a2a4",
        )

    def test_every_request_stage_requires_native_counter_reconciliation(self):
        runner = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args("matrix"), runner), 0)
        stages = {
            name: dict(environment)
            for name, environment in runner.commands
            if name in {"agent_correctness", "c8_scout", "full_matrix"}
        }
        agent = next(
            stage
            for stage in build_stages(self.args())
            if stage.name == "agent_correctness"
        )
        self.assertIn("--require-reconciled-speculation", agent.argv)
        self.assertIn(self.args().engine_metrics, agent.argv)
        for name in ("c8_scout", "full_matrix"):
            self.assertEqual(
                stages[name]["BENCH_REQUIRE_RECONCILED_SPECULATION"], "1"
            )
            self.assertEqual(
                stages[name]["METRICS_URL"], self.args().engine_metrics
            )

    def test_thermal_guard_is_required_before_metadata_or_container_access(self):
        for failure in ("missing capability", "invalid capability"):
            runner = FakeRunner(self.identity)
            with self.subTest(failure=failure), mock.patch(
                "candidate_gate.gpu_guard.validate_inherited_guard",
                side_effect=GuardError(failure),
            ), self.assertRaisesRegex(GateError, "thermal guard"):
                run_gate(self.args(), runner)
            self.assertEqual(runner.commands, [])
            self.assertFalse(self.output.exists())

    def test_plan_binds_stable_guard_policy_without_breaking_resume(self):
        admission = admission_contract(
            self.candidate_manifest_path, self.runtime_manifest_path
        )
        plan = plan_contract(
            self.args(), self.agent, admission, self.guard_contract
        )
        self.assertEqual(
            plan["thermal_guard"], {"expected_gpus": 8, "abort_c": 78.0}
        )
        self.assertEqual(plan["profile"], "infernal-r11-b")
        self.assertEqual(plan["deployment_lock"], str(self.args().deployment_lock))
        replacement = {**self.guard_contract, "run_id": "2" * 32}
        self.assertEqual(
            plan,
            plan_contract(self.args(), self.agent, admission, replacement),
        )

    def test_records_link_the_specific_guard_run(self):
        runner = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args(), runner), 0)
        self.assertTrue(
            all(
                record["thermal_guard_run_id"] == self.guard_contract["run_id"]
                for record in self.records()
            )
        )

    def test_gate_validates_guard_capability_once_with_conservative_limits(self):
        runner = FakeRunner(self.identity)
        self.assertEqual(run_gate(self.args(), runner), 0)
        self.guard_validator.assert_called_once_with(
            expected_gpus=8, maximum_abort_c=gpu_guard.MAX_ABORT_C
        )

    def test_real_runner_uses_a_secret_free_bounded_inspect_format(self):
        child = mock.Mock(returncode=0)
        child.communicate.return_value = (
            (
                f"{self.identity.image_id}\t{self.identity.configured_image}\t"
                f"{self.identity.started_at}\t0\ttrue\n"
            ).encode(),
            b"",
        )
        with (
            mock.patch("candidate_gate.subprocess.Popen", return_value=child) as called,
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
        self.assertTrue(called.call_args.kwargs["start_new_session"])
        self.assertNotIn("{{json .}}", argv[3])
        self.assertNotIn("Env", argv[3])
        child_env = called.call_args.kwargs["env"]
        self.assertEqual(child_env["BENCH_TOKEN"], "secret")
        self.assertNotIn("BENCH_PROMPT", child_env)

    def test_real_runner_requires_one_exact_nvidia_device_request(self):
        for value, pattern in (
            ([{"Driver": "nvidia", "DeviceIDs": ["4", "5", "6", "7"]}], None),
            ([
                {"Driver": "nvidia", "DeviceIDs": ["4", "5", "6", "7"]},
                {"Driver": "other", "DeviceIDs": ["0"]},
            ], "invalid shape"),
        ):
            child = mock.Mock(returncode=0)
            child.communicate.return_value = (json.dumps(value).encode(), b"")
            with mock.patch("candidate_gate.subprocess.Popen", return_value=child):
                if pattern is None:
                    self.assertEqual(
                        SubprocessRunner().device_ids("candidate"),
                        ("4", "5", "6", "7"),
                    )
                else:
                    with self.assertRaisesRegex(GateError, pattern):
                        SubprocessRunner().device_ids("candidate")

    def test_real_runner_process_probe_returns_only_bounded_hashes(self):
        expected = FakeRunner(self.identity).live_process_identity
        child = mock.Mock(returncode=0)
        child.communicate.return_value = (
            json.dumps(expected.__dict__, separators=(",", ":")).encode(),
            b"",
        )
        with mock.patch("candidate_gate.subprocess.Popen", return_value=child) as called:
            actual = SubprocessRunner().process_identity(
                "candidate", ("NCCL_DEBUG", "B12X_MODE"), ("/launcher",)
            )
        self.assertEqual(actual, expected)
        argv = called.call_args.args[0]
        self.assertEqual(argv[:4], ("docker", "exec", "candidate", "python3"))
        self.assertEqual(argv[-1], "/proc")
        self.assertNotIn("NCCL_DEBUG=", " ".join(argv))
        self.assertNotIn("B12X_MODE=", " ".join(argv))

    def test_embedded_process_probe_attests_synthetic_proc_identity(self):
        launcher = self.root / "launcher"
        model_config = self.root / "config.json"
        launcher.write_bytes(b"qualified-launcher")
        model_config.write_bytes(b'{"model":"fixture"}')
        artifact_paths = (str(launcher), str(model_config))

        result, argv, _environment, boot_seconds, start_ticks = self.run_process_probe(
            artifact_paths=artifact_paths
        )

        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertEqual(result.stderr, b"")
        self.assertLess(len(result.stdout), 4096)
        observed = json.loads(result.stdout)
        canonical = lambda value: hashlib.sha256(
            json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
        serving = argv[2:]
        expected_environment = {
            "B12X_MODE": "standard",
            "NCCL_DEBUG": "INFO",
        }
        expected_artifacts = [
            {"path": path, "sha256": hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()}
            for path in artifact_paths
        ]
        self.assertEqual(
            observed,
            {
                "process_started_unix_ns": boot_seconds * 1_000_000_000
                + start_ticks * 1_000_000_000 // os.sysconf("SC_CLK_TCK"),
                "serving_argv_sha256": hashlib.sha256(
                    b"\0".join(value.encode() for value in serving)
                ).hexdigest(),
                "environment_sha256": canonical(expected_environment),
                "artifacts_sha256": canonical(expected_artifacts),
            },
        )
        self.assertNotIn(b"redacted-by-probe", result.stdout)
        self.assertNotIn(b"standard", result.stdout)

    def test_embedded_process_probe_fails_closed_on_live_mismatch(self):
        artifact = self.root / "launcher"
        artifact.write_bytes(b"qualified-launcher")
        symlink = self.root / "launcher-link"
        symlink.symlink_to(artifact)
        cases = (
            {
                "name": "sensitive serving option",
                "process_argv": (
                    "/usr/bin/python3",
                    "/usr/local/bin/vllm",
                    "serve",
                    "deepseek-v4-flash",
                    "--api-key=must-not-be-accepted",
                ),
                "artifact_paths": (str(artifact),),
                "error": b"sensitive serving option",
            },
            {
                "name": "unreviewed performance environment",
                "process_environment": (
                    "B12X_MODE=standard",
                    "NCCL_DEBUG=INFO",
                    "VLLM_FUTURE_PERF_KNOB=enabled",
                ),
                "artifact_paths": (str(artifact),),
                "error": b"unexpected performance environment",
            },
            {
                "name": "symlinked artifact",
                "artifact_paths": (str(symlink),),
                "error": b"OSError",
            },
        )
        for case in cases:
            with self.subTest(case["name"]):
                kwargs = {
                    key: value
                    for key, value in case.items()
                    if key not in {"name", "error"}
                }
                result, *_ = self.run_process_probe(**kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, b"")
                self.assertIn(case["error"], result.stderr)

    def test_unreviewed_performance_environment_is_fail_closed(self):
        for name in (
            "VLLM_NEW_SCHEDULER_KNOB",
            "NCCL_UNREVIEWED_MODE",
            "CUDA_FUTURE_SETTING",
            "LD_AUDIT",
            "MAX_NUM_SEQS",
        ):
            with self.subTest(name=name):
                self.assertTrue(performance_environment_name(name))
        for name in ("HOSTNAME", "PWD", "VLLM_API_KEY", "HF_TOKEN"):
            with self.subTest(name=name):
                self.assertFalse(performance_environment_name(name))

        child = mock.Mock(returncode=1)
        child.communicate.return_value = (b"", b"unexpected performance environment")
        with mock.patch("candidate_gate.subprocess.Popen", return_value=child):
            with self.assertRaisesRegex(GateError, "process inspection failed"):
                SubprocessRunner().process_identity(
                    "candidate", ("NCCL_DEBUG",), ("/launcher",)
                )


if __name__ == "__main__":
    unittest.main()
