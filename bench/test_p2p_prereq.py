import importlib.util
import io
import json
import pathlib
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
SPEC = importlib.util.spec_from_file_location("phase_b", HARNESS)
phase_b = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = phase_b
SPEC.loader.exec_module(phase_b)


class P2PPrerequisiteTest(unittest.TestCase):
    def make_tools(self, root):
        for name in ("nvbandwidth", "all_reduce_perf"):
            (root / name).write_bytes(name.encode())
            (root / name).chmod(0o555)
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

    def test_plan_is_inert_and_active_requires_exact_ack(self):
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
        build = BUILD.read_text(encoding="utf-8")
        dockerfile = DOCKERFILE.read_text(encoding="utf-8")
        for identity in expected:
            self.assertIn(identity, build + dockerfile)

    def test_tool_manifest_fails_closed_on_digest_or_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            self.make_tools(root)
            phase_b.validate_tools(root)
            (root / "nvbandwidth").chmod(0o775)
            with self.assertRaisesRegex(phase_b.GateError, "writable"):
                phase_b.validate_tools(root)

    def test_gpu_container_sandbox_is_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            command = phase_b.container_base(
                "md-p2p-test",
                ("GPU-00000000-0000-0000-0000-000000000000",),
                pathlib.Path(directory),
            )
        joined = "\0".join(command)
        for expected in (
            "--network\0none",
            "--ipc\0private",
            "--read-only",
            "--cap-drop\0ALL",
            "no-new-privileges:true",
            "--cpuset-mems\0" "1",
            ":/tools:ro",
        ):
            self.assertIn(expected, joined)
        self.assertNotIn("docker.sock", joined)
        self.assertNotIn("--privileged", command)

    def test_topology_parser_checks_directed_cells(self):
        matrix = "\tGPU0 GPU1\nGPU0 X NODE\nGPU1 NODE X\n"
        header, rows = phase_b.parse_topology_matrix(matrix)
        self.assertEqual(header, ["GPU0", "GPU1"])
        self.assertEqual(rows["GPU0"][1], "NODE")
        broken = "\tGPU0 GPU1\nGPU0 X SYS\nGPU1 SYS X\n"
        original = phase_b.run
        try:
            phase_b.run = lambda *_args, **_kwargs: broken
            with self.assertRaisesRegex(phase_b.GateError, "not NODE"):
                phase_b.validate_pair_matrix([0, 1], ["nvidia-smi", "topo", "-m"], "NODE")
        finally:
            phase_b.run = original

    def test_topology_parser_fails_closed_on_missing_cell(self):
        incomplete = "\tGPU0 GPU1\nGPU0 X\nGPU1 NODE X\n"
        original = phase_b.run
        try:
            phase_b.run = lambda *_args, **_kwargs: incomplete
            with self.assertRaisesRegex(phase_b.GateError, "truncates"):
                phase_b.validate_pair_matrix(
                    [0, 1], ["nvidia-smi", "topo", "-m"], "NODE"
                )
        finally:
            phase_b.run = original

    def test_private_result_writer_uses_exclusive_mode_0600(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "result.txt"
            phase_b.write_private(path, "safe\n")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                phase_b.write_private(path, "overwrite\n")

    def test_build_script_is_syntactically_valid_and_rejects_relative_output(self):
        subprocess.run(["bash", "-n", str(BUILD)], check=True)
        rejected = subprocess.run(
            ["bash", str(BUILD), "relative-output"],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("must be absolute", rejected.stderr)

    def test_nvbandwidth_output_requires_every_directed_pair(self):
        names = phase_b.BANDWIDTH_TESTS
        matrix = [["N/A", "12.5"], ["13.0", "N/A"]]
        document = {
            "nvbandwidth": {
                "testcases": [
                    {
                        "name": name,
                        "status": "Passed",
                        "bandwidth_matrix": matrix,
                    }
                    for name in names
                ]
            }
        }
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "result.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            phase_b.validate_nvbandwidth_output(path, names, 2)
            document["nvbandwidth"]["testcases"][0]["bandwidth_matrix"][0][1] = "N/A"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(phase_b.GateError, "omits a directed pair"):
                phase_b.validate_nvbandwidth_output(path, names, 2)

    def test_active_failure_restores_baseline_and_restore_failure_is_critical(self):
        state = phase_b.Preflight(
            target_indices=(4, 5, 6, 7),
            target_uuids=tuple(
                f"GPU-00000000-0000-0000-0000-{index:012d}"
                for index in range(4)
            ),
            target_buses=("0000:01:00.0",) * 4,
            free_mib=(2048,) * 4,
            topology="topology\n",
            peer_read="read\n",
            peer_write="write\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tools = root / "tools"
            tools.mkdir()
            self.make_tools(tools)
            result = root / "result"
            result.mkdir()
            args = types.SimpleNamespace(
                tools_dir=tools,
                quiet_seconds=60,
                run_full_prerequisite=False,
                cycles=1,
            )
            calls = []

            def run_compose(environment):
                calls.append(environment.copy())
                if len(calls) == 2:
                    raise phase_b.GateError("synthetic restore rejection")

            with (
                mock.patch.object(
                    phase_b,
                    "compose_environment",
                    return_value=({"BASELINE": "yes"}, {}, "lb-image"),
                ),
                mock.patch.object(phase_b, "wait_for_health"),
                mock.patch.object(phase_b, "capture_metadata"),
                mock.patch.object(phase_b.tempfile, "mkdtemp", return_value=str(result)),
                mock.patch.object(phase_b, "run_compose", side_effect=run_compose),
                mock.patch.object(
                    phase_b,
                    "docker_inspect",
                    return_value={
                        "Config": {"Env": ["DS4_UPSTREAM=http://dspark-0731:8000"]}
                    },
                ),
                mock.patch.object(
                    phase_b,
                    "quiet_fence",
                    side_effect=phase_b.GateError("synthetic workload rejection"),
                ),
                mock.patch.object(phase_b.sys, "stderr", io.StringIO()),
            ):
                with self.assertRaisesRegex(phase_b.GateError, "CRITICAL.*restoration"):
                    phase_b.active_run(args, state)
            self.assertEqual(len(calls), 2)
            self.assertEqual(calls[1], {"BASELINE": "yes"})


if __name__ == "__main__":
    unittest.main()
