import importlib.util
import io
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[1]
HARNESS = ROOT / "bench" / "p2p" / "node06_phase_b.py"
BUILD = ROOT / "bench" / "p2p" / "build_tools.sh"
DOCKERFILE = ROOT / "bench" / "p2p" / "Dockerfile.tools"
SOURCE_VERIFY = ROOT / "bench" / "p2p" / "verify_pinned_sources.py"
SPEC = importlib.util.spec_from_file_location("phase_b", HARNESS)
phase_b = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = phase_b
SPEC.loader.exec_module(phase_b)


class P2PPrerequisiteTest(unittest.TestCase):
    def make_tools(self, root, *, mode=0o555):
        for name in ("nvbandwidth", "all_reduce_perf"):
            (root / name).write_bytes(name.encode())
            (root / name).chmod(mode)
        manifest = {
            "schema_version": 1,
            "nvbandwidth_commit": phase_b.NVBANDWIDTH_SHA,
            "nccl_tests_commit": phase_b.NCCL_TESTS_SHA,
            "runtime_image": phase_b.R34_REPO_DIGEST,
            "cuda_architecture": "120",
            "binaries": {
                name: {"sha256": phase_b.sha256(root / name)}
                for name in ("nvbandwidth", "all_reduce_perf")
            },
        }
        (root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        (root / "manifest.json").chmod(0o444)
        root.chmod(mode)
        return phase_b.sha256(root / "manifest.json")

    def sample_state(self):
        return phase_b.Preflight(
            target_indices=(4, 5, 6, 7),
            target_uuids=tuple(
                f"GPU-00000000-0000-0000-0000-{index:012d}" for index in range(4)
            ),
            target_buses=("0000:01:00.0",) * 4,
            free_mib=(2048,) * 4,
            topology="topology\n",
            peer_read="read\n",
            peer_write="write\n",
        )

    def test_plan_is_inert_and_active_requires_ack_hash_and_fence(self):
        plan = subprocess.run(
            ["python3", str(HARNESS), "--print-plan"],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        )
        self.assertEqual(json.loads(plan.stdout)["default"], "read-only preflight")
        rejected = subprocess.run(
            ["python3", str(HARNESS), "--run-gpu-scout"],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("acknowledge-production-risk", rejected.stderr)
        missing_hash = subprocess.run(
            [
                "python3",
                str(HARNESS),
                "--run-gpu-scout",
                "--acknowledge-production-risk",
                phase_b.PROFILE_ACK,
            ],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(missing_hash.returncode, 2)
        self.assertIn("expected-tools-manifest-sha256", missing_hash.stderr)
        short_fence = subprocess.run(
            [
                "python3",
                str(HARNESS),
                "--run-gpu-scout",
                "--quiet-seconds",
                "59",
                "--acknowledge-production-risk",
                phase_b.PROFILE_ACK,
            ],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(short_fence.returncode, 2)
        self.assertIn("60-second quiet fence", short_fence.stderr)

    def test_source_and_runtime_pins_are_consistent(self):
        expected = {
            phase_b.NVBANDWIDTH_SHA,
            phase_b.NCCL_TESTS_SHA,
            phase_b.R34_IMAGE_ID.removeprefix("sha256:"),
        }
        source = BUILD.read_text() + DOCKERFILE.read_text() + SOURCE_VERIFY.read_text()
        for identity in expected:
            self.assertIn(identity, source)

    def test_tool_staging_requires_external_hash_owner_lock_and_no_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tools = root / "tools"
            tools.mkdir()
            digest = self.make_tools(tools)
            with mock.patch.object(phase_b.os, "geteuid", return_value=0):
                staged = phase_b.validate_and_stage_tools(
                    tools, digest, root / "stage", owner_uid=os.getuid()
                )
            self.assertEqual(staged.stat().st_mode & 0o777, 0o555)
            self.assertEqual((staged / "manifest.json").stat().st_mode & 0o777, 0o444)
            with self.assertRaisesRegex(phase_b.GateError, "external expected"):
                phase_b.validate_and_stage_tools(
                    tools, "0" * 64, root / "bad-hash", owner_uid=os.getuid()
                )

            tools.chmod(0o755)
            with self.assertRaisesRegex(phase_b.GateError, "owner-locked"):
                phase_b.validate_and_stage_tools(
                    tools, digest, root / "writable", owner_uid=os.getuid()
                )
            (tools / "nvbandwidth").rename(tools / "real-nvbandwidth")
            (tools / "nvbandwidth").symlink_to(tools / "real-nvbandwidth")
            tools.chmod(0o555)
            with self.assertRaisesRegex(phase_b.GateError, "opened safely"):
                phase_b.validate_and_stage_tools(
                    tools, digest, root / "symlink", owner_uid=os.getuid()
                )

    def test_gpu_container_argv_preserves_docker_csv_quotes(self):
        uuids = (
            "GPU-00000000-0000-0000-0000-000000000000",
            "GPU-11111111-1111-1111-1111-111111111111",
        )
        command = phase_b.container_base("md-p2p-test", uuids, pathlib.Path("/safe"))
        self.assertEqual(command[:4], ["docker", "create", "--name", "md-p2p-test"])
        index = command.index("--gpus")
        self.assertEqual(command[index + 1], '"device=' + ",".join(uuids) + '"')
        joined = "\0".join(command)
        for expected in (
            "--network\0none",
            "--ipc\0private",
            "--read-only",
            "--cap-drop\0ALL",
            "no-new-privileges:true",
            "--cpuset-mems\0" "1",
            "/safe:/tools:ro",
        ):
            self.assertIn(expected, joined)

    def test_benchmark_always_removes_exact_created_id_after_attach_failure(self):
        container_id = "a" * 64
        process = mock.Mock()
        process.poll.return_value = 1
        process.returncode = 1
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "output"
            with (
                mock.patch.object(phase_b, "run", return_value=container_id),
                mock.patch.object(phase_b.subprocess, "Popen", return_value=process),
                mock.patch.object(phase_b.subprocess, "run") as remove,
            ):
                remove.return_value.returncode = 0
                with self.assertRaisesRegex(phase_b.GateError, "exited 1"):
                    phase_b.run_benchmark(
                        ["docker", "create"], name="name", output=output, timeout=1
                    )
            remove.assert_called_once()
            self.assertEqual(remove.call_args.args[0], ["docker", "rm", "-f", container_id])

    def test_benchmark_cleanup_failure_is_critical(self):
        container_id = "b" * 64
        process = mock.Mock()
        process.poll.return_value = 0
        with tempfile.TemporaryDirectory() as directory:
            with (
                mock.patch.object(phase_b, "run", return_value=container_id),
                mock.patch.object(phase_b.subprocess, "Popen", return_value=process),
                mock.patch.object(phase_b.subprocess, "run") as remove,
            ):
                remove.return_value.returncode = 1
                with self.assertRaisesRegex(phase_b.GateError, "CRITICAL.*remove"):
                    phase_b.run_benchmark(
                        ["docker", "create"],
                        name="name",
                        output=pathlib.Path(directory) / "output",
                        timeout=1,
                        interrupted=lambda: signal.SIGTERM,
                    )

    def test_topology_parser_checks_directed_and_missing_cells(self):
        matrix = "\tGPU0 GPU1\nGPU0 X NODE\nGPU1 NODE X\n"
        header, rows = phase_b.parse_topology_matrix(matrix)
        self.assertEqual(header, ["GPU0", "GPU1"])
        self.assertEqual(rows["GPU0"][1], "NODE")
        for broken, error in (
            ("\tGPU0 GPU1\nGPU0 X SYS\nGPU1 SYS X\n", "not NODE"),
            ("\tGPU0 GPU1\nGPU0 X\nGPU1 NODE X\n", "truncates"),
        ):
            with mock.patch.object(phase_b, "run", return_value=broken):
                with self.assertRaisesRegex(phase_b.GateError, error):
                    phase_b.validate_pair_matrix(
                        [0, 1], ["nvidia-smi", "topo", "-m"], "NODE"
                    )

    def test_private_result_writer_uses_exclusive_mode_0600(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "result.txt"
            phase_b.write_private(path, "safe\n")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                phase_b.write_private(path, "overwrite\n")

    def test_build_script_and_pinned_source_verifier_are_syntactically_valid(self):
        subprocess.run(["bash", "-n", str(BUILD)], check=True)
        subprocess.run(["python3", "-m", "py_compile", str(SOURCE_VERIFY)], check=True)
        rejected = subprocess.run(
            ["bash", str(BUILD), "relative-output"],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(rejected.returncode, 2)

    def test_nvbandwidth_output_requires_every_directed_pair(self):
        names = phase_b.BANDWIDTH_TESTS
        matrix = [["N/A", "12.5"], ["13.0", "N/A"]]
        document = {
            "nvbandwidth": {
                "testcases": [
                    {"name": name, "status": "Passed", "bandwidth_matrix": matrix}
                    for name in names
                ]
            }
        }
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "result.json"
            path.write_text(json.dumps(document))
            phase_b.validate_nvbandwidth_output(path, names, 2)
            document["nvbandwidth"]["testcases"][0]["bandwidth_matrix"][0][1] = "N/A"
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(phase_b.GateError, "omits a directed pair"):
                phase_b.validate_nvbandwidth_output(path, names, 2)

    def nccl_output(self):
        rows = []
        for size in sorted(phase_b.expected_nccl_sizes()):
            rows.append(f"{size} 2048 float sum -1 10.0 1.0 1.5 0 9.0 1.1 1.6 0")
        return "\n".join(
            [
                "# nThread 1 nGpus 4 minBytes 8192 maxBytes 8388608",
                "Channel 00 : 0[0] -> 1[1] via P2P/direct pointer/read",
                "Channel 01 : 1[1] -> 2[2] via P2P/direct pointer/read",
                "Channel 02 : 2[2] -> 3[3] via P2P/direct pointer/read",
                "Channel 03 : 3[3] -> 0[0] via P2P/direct pointer/read",
                *rows,
                "# Out of bounds values : 0 OK",
            ]
        )

    def test_nccl_output_semantic_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "nccl.txt"
            path.write_text(self.nccl_output())
            phase_b.validate_nccl_output(path)
            path.write_text(self.nccl_output().replace("P2P/direct", "SHM/copy", 1))
            with self.assertRaisesRegex(phase_b.GateError, "fallback transport"):
                phase_b.validate_nccl_output(path)
            path.write_text(self.nccl_output().replace(" 0\n", " 1\n", 1))
            with self.assertRaisesRegex(phase_b.GateError, "invalid result"):
                phase_b.validate_nccl_output(path)

    def test_compose_drift_and_unexpected_environment_fail_closed(self):
        baseline = phase_b.ComposeBaseline(
            render_path=pathlib.Path("/private/render"),
            single_path=pathlib.Path("/private/single"),
            render_sha256="a" * 64,
            project_name="project",
            source_identities={"docker-compose.yaml": "b" * 64, ".env": "c" * 64},
            runtime_spec={},
            service_hash="d" * 64,
            restore_service_hash="e" * 64,
            owner_id="owner",
            single_service_hash="f" * 64,
        )
        with mock.patch.object(
            phase_b, "source_identities", return_value={"docker-compose.yaml": "changed"}
        ):
            with self.assertRaisesRegex(phase_b.GateError, "changed"):
                phase_b.reject_compose_drift(baseline)

        document = {"services": {phase_b.LB_CONTAINER: {"image": "lb", "environment": {}}}}
        current = {
            "Image": "id",
            "Config": {
                "Env": ["PATH=/bin", "UNEXPECTED=1"],
                "Labels": {"com.docker.compose.config-hash": "hash"},
            },
        }
        image = [{"Id": "id", "Config": {"Env": ["PATH=/bin"]}}]
        with mock.patch.object(phase_b, "run", return_value=json.dumps(image)):
            with self.assertRaisesRegex(phase_b.GateError, "unexpected"):
                phase_b.validate_rendered_runtime(document, current, "hash")

    def test_deferred_signal_is_propagated_only_after_verified_restore(self):
        baseline = phase_b.ComposeBaseline(
            render_path=pathlib.Path("baseline"),
            single_path=pathlib.Path("single"),
            render_sha256="a" * 64,
            project_name="project",
            source_identities={},
            runtime_spec={},
            service_hash="b" * 64,
            restore_service_hash="c" * 64,
            owner_id="result",
            single_service_hash="d" * 64,
        )
        args = types.SimpleNamespace(
            tools_dir=pathlib.Path("tools"),
            expected_tools_manifest_sha256="a" * 64,
            quiet_seconds=60,
            run_full_prerequisite=False,
            cycles=1,
        )
        handlers = {}

        def install(signum, handler):
            old = handlers.get(signum, signal.SIG_DFL)
            handlers[signum] = handler
            return old

        calls = []

        def compose(path, project):
            calls.append((path, project))
            if path == baseline.single_path:
                handlers[signal.SIGTERM](signal.SIGTERM, None)

        with (
            mock.patch.object(phase_b.os, "geteuid", return_value=0),
            mock.patch.object(phase_b.tempfile, "mkdtemp", return_value="/tmp/result"),
            mock.patch.object(phase_b.pathlib.Path, "chmod"),
            mock.patch.object(phase_b, "validate_and_stage_tools", return_value=pathlib.Path("tools")),
            mock.patch.object(phase_b, "capture_compose_baseline", return_value=baseline),
            mock.patch.object(phase_b, "capture_metadata"),
            mock.patch.object(phase_b, "reject_compose_drift"),
            mock.patch.object(phase_b, "wait_for_health"),
            mock.patch.object(phase_b, "run_compose", side_effect=compose),
            mock.patch.object(phase_b, "verify_restored_baseline") as verified,
            mock.patch.object(phase_b, "current_is_harness_owned", return_value=True),
            mock.patch.object(phase_b.signal, "signal", side_effect=install),
            mock.patch.object(phase_b.sys, "stderr", io.StringIO()),
        ):
            with self.assertRaisesRegex(phase_b.DeferredSignal, "signal 15"):
                phase_b.active_run_locked(args, self.sample_state())
        verified.assert_called_once_with(baseline)
        self.assertEqual(calls[-1], (baseline.render_path, baseline.project_name))

    def test_restore_never_overwrites_superseding_canonical_deployment(self):
        baseline = phase_b.ComposeBaseline(
            render_path=pathlib.Path("baseline"),
            single_path=pathlib.Path("single"),
            render_sha256="a" * 64,
            project_name="project",
            source_identities={},
            runtime_spec={},
            service_hash="b" * 64,
            restore_service_hash="c" * 64,
            owner_id="owner",
            single_service_hash="d" * 64,
        )
        with (
            mock.patch.object(phase_b, "current_is_harness_owned", return_value=False),
            mock.patch.object(phase_b, "verify_current_canonical_dual") as canonical,
            mock.patch.object(phase_b, "run_compose") as compose,
        ):
            superseded = phase_b.restore_or_accept_superseding_canonical(baseline)
        self.assertTrue(superseded)
        canonical.assert_called_once_with()
        compose.assert_not_called()

    def test_restore_ownership_requires_run_label_and_exact_service_hash(self):
        baseline = phase_b.ComposeBaseline(
            render_path=pathlib.Path("baseline"),
            single_path=pathlib.Path("single"),
            render_sha256="a" * 64,
            project_name="project",
            source_identities={},
            runtime_spec={},
            service_hash="b" * 64,
            restore_service_hash="c" * 64,
            owner_id="owner",
            single_service_hash="d" * 64,
        )
        labels = {
            phase_b.HARNESS_OWNER_LABEL: "owner",
            "com.docker.compose.config-hash": "d" * 64,
        }
        with mock.patch.object(
            phase_b, "docker_inspect", return_value={"Config": {"Labels": labels}}
        ):
            self.assertTrue(phase_b.current_is_harness_owned(baseline))
        labels[phase_b.HARNESS_OWNER_LABEL] = "concurrent-writer"
        with mock.patch.object(
            phase_b, "docker_inspect", return_value={"Config": {"Labels": labels}}
        ):
            self.assertFalse(phase_b.current_is_harness_owned(baseline))

    def test_deployment_lock_rejects_concurrent_writer(self):
        with tempfile.TemporaryDirectory() as directory:
            lock = pathlib.Path(directory) / "deployment.lock"
            with mock.patch.object(phase_b, "DEPLOYMENT_LOCK", lock):
                with phase_b.deployment_lock():
                    with self.assertRaisesRegex(phase_b.GateError, "holds the lock"):
                        with phase_b.deployment_lock():
                            self.fail("concurrent deployment lock unexpectedly acquired")

    def test_restore_unowned_noncanonical_state_requires_manual_intervention(self):
        baseline = phase_b.ComposeBaseline(
            render_path=pathlib.Path("baseline"),
            single_path=pathlib.Path("single"),
            render_sha256="a" * 64,
            project_name="project",
            source_identities={},
            runtime_spec={},
            service_hash="b" * 64,
            restore_service_hash="c" * 64,
            owner_id="owner",
            single_service_hash="d" * 64,
        )
        with (
            mock.patch.object(phase_b, "current_is_harness_owned", return_value=False),
            mock.patch.object(
                phase_b,
                "verify_current_canonical_dual",
                side_effect=phase_b.GateError("not healthy canonical"),
            ),
            mock.patch.object(phase_b, "run_compose") as compose,
        ):
            with self.assertRaisesRegex(phase_b.GateError, "not healthy canonical"):
                phase_b.restore_or_accept_superseding_canonical(baseline)
        compose.assert_not_called()


if __name__ == "__main__":
    unittest.main()
