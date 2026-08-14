import argparse
import contextlib
import io
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import node06_gpu_guard as guard
import node06_operational_moratorium as moratorium


def records(path):
    return [json.loads(line) for line in pathlib.Path(path).read_text().splitlines()]


def final_record(path):
    return records(path)[-1]


def supervised_authorization(root, operation="gpu-workload.node06-experiment"):
    root = pathlib.Path(root)
    root.chmod(0o700)
    now = int(time.time())
    path = root / "supervised-authorization.json"
    path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "node": "node06",
                "operation": operation,
                "issued_at_unix": now,
                "expires_at_unix": now + 300,
                "nonce": "b" * 32,
                "acknowledgement": moratorium.ACKNOWLEDGEMENT,
                "ac_repair_confirmed": True,
                "supervisor_present": True,
            }
        ),
        encoding="utf-8",
    )
    path.chmod(0o600)
    return path


def pid_is_active(pid):
    try:
        raw = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return raw[raw.rfind(")") + 2 :].split()[0] != "Z"


def sample(temperatures, utilization=50, power=100):
    rows = []
    for index, temperature in enumerate(temperatures):
        rows.append(
            guard.GpuReading(
                index=index,
                uuid=f"GPU-{index:032x}",
                name="NVIDIA RTX PRO 6000 Blackwell Server Edition",
                temperature_c=temperature,
                power_w=power,
                power_limit_w=600,
                gpu_utilization_pct=utilization,
                memory_utilization_pct=20,
                memory_used_mib=1000,
                memory_total_mib=96000,
            )
        )
    return guard.GpuSample(tuple(rows))


class SequenceSampler:
    def __init__(self, values):
        self.values = list(values)
        self.index = 0

    def __call__(self, _args):
        value = self.values[min(self.index, len(self.values) - 1)]
        self.index += 1
        if isinstance(value, Exception):
            raise value
        return value


class Node06GpuGuardTests(unittest.TestCase):
    def args(self, output, command):
        return argparse.Namespace(
            output=pathlib.Path(output),
            label="test-cell",
            expected_gpus=8,
            start_max_c=65,
            abort_c=78,
            cooldown_timeout_seconds=0,
            poll_seconds=0.01,
            sample_timeout_seconds=1,
            workload_grace_seconds=0.1,
            termination_grace_seconds=1,
            preserve_rollback_owner=False,
            nvidia_smi="/usr/bin/nvidia-smi",
            command=command,
        )

    def run_guard(self, args, sampler):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
            io.StringIO()
        ):
            return guard.run_guard(args, sampler=sampler)

    def test_parses_exact_bounded_gpu_inventory(self):
        raw = "\n".join(
            f"{index}, GPU-{index:032x}, NVIDIA RTX PRO 6000, "
            f"{40 + index}, 100.5, 600, 75, 20, 1000, 96000"
            for index in range(8)
        )
        parsed = guard.parse_sample(raw, 8)
        self.assertEqual(len(parsed.readings), 8)
        self.assertEqual(parsed.hottest.index, 7)
        self.assertEqual(parsed.hottest.temperature_c, 47)

        cases = (
            raw.replace("7, GPU-00000000000000000000000000000007", "6, GPU-00000000000000000000000000000007"),
            raw.replace("100.5", "[N/A]", 1),
            raw.replace("1000, 96000", "97000, 96000", 1),
            "\n".join(raw.splitlines()[:-1]),
        )
        for malformed in cases:
            with self.subTest(malformed=malformed[-40:]), self.assertRaises(
                guard.GuardError
            ):
                guard.parse_sample(malformed, 8)

    def test_telemetry_timeout_has_no_unbounded_post_kill_wait(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            fake = root / "nvidia-smi"
            pid_path = root / "pid"
            fake.write_text(
                "#!/bin/sh\n"
                f"echo $$ > {pid_path}\n"
                "trap '' TERM\n"
                "sleep 60\n",
                encoding="utf-8",
            )
            fake.chmod(0o755)
            args = self.args(root / "unused", ["true"])
            args.nvidia_smi = str(fake)
            args.sample_timeout_seconds = 0.25
            started = time.monotonic()
            with self.assertRaises(guard.GuardError):
                guard.query_gpus(args)
            self.assertLess(time.monotonic() - started, 1)
            pid = int(pid_path.read_text())
            deadline = time.monotonic() + 1
            while pid_is_active(pid) and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertFalse(pid_is_active(pid))

    def test_cool_start_failure_never_launches_child(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(output, ["does-not-exist"])
            result = self.run_guard(args, SequenceSampler([sample([66] * 8)]))
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "preflight_too_hot")
            self.assertNotIn("child_exit_code", record)
            self.assertEqual(record["trigger"]["gpu_index"], 0)

    def test_preflight_abort_temperature_fails_immediately(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.jsonl"
            args = self.args(output, ["does-not-exist"])
            args.cooldown_timeout_seconds = 300
            with mock.patch("node06_gpu_guard.time.sleep") as sleep:
                result = self.run_guard(args, SequenceSampler([sample([78] * 8)]))
            self.assertEqual(result, guard.EXIT_THERMAL)
            self.assertEqual(final_record(output)["reason"], "preflight_thermal_abort")
            sleep.assert_not_called()

    def test_signal_during_cool_sample_revokes_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "result.jsonl"
            marker = root / "started"
            args = self.args(
                output,
                [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)],
            )

            def interrupting_sampler(_args):
                os.kill(os.getpid(), signal.SIGTERM)
                return sample([40] * 8)

            result = self.run_guard(args, interrupting_sampler)
            self.assertEqual(result, 128 + signal.SIGTERM)
            self.assertFalse(marker.exists())
            self.assertEqual(final_record(output)["reason"], "interrupted")

    def test_safe_child_passes_without_recording_command_or_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            secret = "private-command-value"
            args = self.args(
                output,
                [sys.executable, "-c", "pass", secret],
            )
            result = self.run_guard(
                args, SequenceSampler([sample([40] * 8), sample([41] * 8)])
            )
            self.assertEqual(result, 0)
            raw = output.read_text()
            record = final_record(output)
            self.assertEqual(record["status"], "passed")
            self.assertEqual(record["child_exit_code"], 0)
            self.assertGreaterEqual(record["telemetry"]["samples"], 2)
            self.assertEqual(len(record["telemetry"]["per_gpu"]), 8)
            self.assertEqual(record["telemetry"]["per_gpu"][7]["index"], 7)
            self.assertNotIn(secret, raw)
            self.assertNotIn("command", record)
            self.assertEqual(records(output)[0]["type"], "start")
            self.assertEqual(record["type"], "final")
            self.assertEqual(records(output)[0]["run_id"], record["run_id"])

    def test_stdout_is_status_only_and_excludes_hardware_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.jsonl"
            args = self.args(output, [sys.executable, "-c", "pass"])
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
                io.StringIO()
            ):
                result = guard.run_guard(
                    args, sampler=SequenceSampler([sample([40] * 8)])
                )
            self.assertEqual(result, 0)
            printed = json.loads(stdout.getvalue())
            self.assertEqual(
                set(printed), {"run_id", "status", "reason", "exit_code"}
            )
            self.assertNotIn("NVIDIA", stdout.getvalue())
            self.assertNotIn("identity_sha256", stdout.getvalue())

    def test_thermal_breach_terminates_the_child_process_group(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(
                output,
                [sys.executable, "-c", "import time; time.sleep(60)"],
            )
            sampler = SequenceSampler([sample([40] * 8), sample([40] * 7 + [78])])
            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "thermal_abort")
            self.assertEqual(record["trigger"], {"gpu_index": 7, "temperature_c": 78})
            self.assertIn("termination_escalated", record)

    def test_final_sample_rejects_a_hot_short_child(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(output, [sys.executable, "-c", "pass"])
            sampler = SequenceSampler([sample([40] * 8), sample([78] + [40] * 7)])
            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "thermal_abort")
            self.assertNotIn("child_exit_code", record)
            self.assertIn("termination_escalated", record)

    def test_success_cannot_leave_a_child_in_the_owned_process_group(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(
                output,
                [
                    sys.executable,
                    "-W",
                    "ignore::ResourceWarning",
                    "-c",
                    "import subprocess; subprocess.Popen(['sleep', '60'], start_new_session=True)",
                ],
            )
            sampler = SequenceSampler(
                [sample([40] * 8), *[sample([41] * 8) for _ in range(16)]]
            )
            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_INTERNAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "orphaned_process_tree")
            self.assertFalse(record["termination_escalated"])

    def test_lost_telemetry_terminates_the_child_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(
                output,
                [sys.executable, "-c", "import time; time.sleep(60)"],
            )
            sampler = SequenceSampler(
                [sample([40] * 8), guard.GuardError("injected")]
            )
            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_TELEMETRY)
            record = final_record(output)
            self.assertIn(
                record["reason"],
                ("telemetry_unavailable", "termination_telemetry_unavailable"),
            )
            self.assertEqual(record["telemetry"]["samples"], 1)

    def test_sigkill_escalation_is_bounded_and_recorded(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(
                output,
                [
                    sys.executable,
                    "-c",
                    "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
                ],
            )
            args.poll_seconds = 0.1
            args.termination_grace_seconds = 1
            sampler = SequenceSampler([sample([40] * 8), sample([78] * 8)])
            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "thermal_abort")
            self.assertTrue(record["termination_escalated"])
            self.assertLess(record["duration_seconds"], 0.8)

    def test_journal_is_owner_only_and_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            reservation = guard.JournalReservation(output)
            reservation.finish({"status": "first"})
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(guard.GuardError):
                guard.JournalReservation(output)

            unsafe = pathlib.Path(directory) / "unsafe"
            unsafe.mkdir(mode=0o777)
            unsafe.chmod(0o777)
            with self.assertRaisesRegex(guard.GuardError, "owner-only"):
                guard.JournalReservation(unsafe / "result.json")

    def test_journal_checkpoints_are_individually_parseable_and_fsynced(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.jsonl"
            args = self.args(output, [sys.executable, "-c", "pass"])
            with mock.patch.object(guard, "CHECKPOINT_SECONDS", 0):
                result = self.run_guard(
                    args, SequenceSampler([sample([40] * 8), sample([41] * 8)])
                )
            self.assertEqual(result, 0)
            journal = records(output)
            self.assertEqual(journal[0]["type"], "start")
            self.assertIn("checkpoint", [item["type"] for item in journal])
            self.assertEqual(journal[-1]["type"], "final")
            self.assertEqual(len({item["run_id"] for item in journal}), 1)

    def test_inherited_capability_rejects_forgery_and_validates_live_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            module_dir = str(pathlib.Path(guard.__file__).parent)
            script = (
                "import json,sys; sys.path.insert(0,sys.argv[1]); "
                "import node06_gpu_guard as g; "
                "print(json.dumps(g.validate_inherited_guard()))"
            )
            capability = guard.create_guard_capability(8, 78, "a" * 32)
            environment = os.environ.copy()
            environment.update(capability.environment)
            valid = subprocess.run(
                [sys.executable, "-c", script, module_dir],
                env=environment,
                pass_fds=(capability.descriptor,),
                check=False,
                capture_output=True,
                text=True,
            )
            capability.close()
            self.assertEqual(valid.returncode, 0, valid.stderr)
            self.assertEqual(json.loads(valid.stdout)["run_id"], "a" * 32)

            forged = environment.copy()
            forged["MINI_DYNAMO_GPU_GUARD_CAPABILITY_FD"] = "999999"
            invalid = subprocess.run(
                [sys.executable, "-c", script, module_dir],
                env=forged,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(invalid.returncode, 0)

    def test_guard_sigkill_cancels_candidate_owned_request_process(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            nvidia_smi = root / "nvidia-smi"
            rows = "\\n".join(
                f"{index}, GPU-{index:032x}, NVIDIA RTX PRO 6000, "
                "40, 100, 600, 50, 20, 1000, 96000"
                for index in range(8)
            )
            nvidia_smi.write_text(f"#!/bin/sh\nprintf '%b' '{rows}\\n'\n")
            nvidia_smi.chmod(0o755)
            owner = root / "owner.py"
            owner.write_text(
                "import os,signal,sys\n"
                "sys.path.insert(0, sys.argv[1])\n"
                "import candidate_gate as c\n"
                "import node06_gpu_guard as g\n"
                "g.validate_inherited_guard()\n"
                "runner=c.SubprocessRunner()\n"
                "def stop(_s,_f):\n"
                " runner.cancel(); raise SystemExit(143)\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                "code='import os,sys,time; open(sys.argv[1], \"w\").write(str(os.getpid())); time.sleep(60)'\n"
                "raise SystemExit(runner.run([sys.executable, '-c', code, sys.argv[2]]).returncode)\n",
                encoding="utf-8",
            )
            request_pid_path = root / "request.pid"
            output = root / "guard.jsonl"
            authorization = supervised_authorization(root)
            process = subprocess.Popen(
                [
                    sys.executable,
                    guard.__file__,
                    "--supervised-authorization-file",
                    str(authorization),
                    "--nvidia-smi",
                    str(nvidia_smi),
                    "--output",
                    str(output),
                    "--poll-seconds",
                    "0.25",
                    "--workload-grace-seconds",
                    "0.1",
                    "--",
                    sys.executable,
                    str(owner),
                    str(pathlib.Path(guard.__file__).parent),
                    str(request_pid_path),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            deadline = time.monotonic() + 5
            while not request_pid_path.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(request_pid_path.exists())
            request_pid = int(request_pid_path.read_text())
            os.kill(process.pid, signal.SIGKILL)
            process.wait(timeout=5)

            deadline = time.monotonic() + 5
            while pid_is_active(request_pid) and time.monotonic() < deadline:
                time.sleep(0.02)
            if pid_is_active(request_pid):
                os.kill(request_pid, signal.SIGKILL)
                self.fail("request process survived GPU guard death")
            self.assertEqual(records(output)[0]["type"], "start")

    def test_post_term_new_session_child_stays_in_short_workload_grace(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "guard.jsonl"
            ready = root / "ready"
            spawned_pid = root / "spawned.pid"
            code = (
                "import os,pathlib,signal,subprocess,sys,time; "
                "ready=pathlib.Path(sys.argv[1]); out=pathlib.Path(sys.argv[2]); "
                "handler=lambda _s,_f: (out.write_text(str(subprocess.Popen(['sleep','60'], start_new_session=True).pid)), os._exit(0)); "
                "signal.signal(signal.SIGTERM, handler); ready.touch(); time.sleep(60)"
            )
            args = self.args(
                output,
                [
                    sys.executable,
                    "-W",
                    "ignore::ResourceWarning",
                    "-c",
                    code,
                    str(ready),
                    str(spawned_pid),
                ],
            )
            args.poll_seconds = 0.05
            args.workload_grace_seconds = 0.1

            def sampler(_args):
                return sample([78] * 8) if ready.exists() else sample([40] * 8)

            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_THERMAL)
            self.assertTrue(spawned_pid.exists())
            pid = int(spawned_pid.read_text())
            deadline = time.monotonic() + 2
            while pid_is_active(pid) and time.monotonic() < deadline:
                time.sleep(0.02)
            if pid_is_active(pid):
                os.kill(pid, signal.SIGKILL)
                self.fail("post-TERM request process exceeded workload grace")
            self.assertIn("termination_escalated", final_record(output))

    def test_tree_construction_failure_revokes_launch_before_work_starts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "guard.jsonl"
            marker = root / "started"
            args = self.args(
                output,
                [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)],
            )
            with mock.patch.object(
                guard, "ChildTree", side_effect=guard.GuardError("injected")
            ):
                result = self.run_guard(args, SequenceSampler([sample([40] * 8)]))
            self.assertEqual(result, guard.EXIT_INTERNAL)
            self.assertFalse(marker.exists())
            self.assertEqual(final_record(output)["reason"], "internal_error")

    def test_exec_shim_does_not_block_child_termination_signals(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "guard.jsonl"
            mask_path = root / "mask.json"
            code = (
                "import json,pathlib,signal,sys; "
                "watched={signal.SIGHUP,signal.SIGINT,signal.SIGTERM}; "
                "blocked=signal.pthread_sigmask(signal.SIG_BLOCK, set()); "
                "pathlib.Path(sys.argv[1]).write_text(json.dumps(sorted(watched & blocked)))"
            )
            args = self.args(
                output,
                [sys.executable, "-c", code, str(mask_path)],
            )
            result = self.run_guard(args, SequenceSampler([sample([40] * 8)]))
            self.assertEqual(result, 0)
            self.assertEqual(json.loads(mask_path.read_text()), [])

    def test_argument_validation_keeps_conservative_operational_bounds(self):
        parsed = guard.parser().parse_args(
            ["--output", "/tmp/result.json", "--", "true"]
        )
        guard.validate_args(parsed)
        self.assertEqual(parsed.expected_gpus, 8)
        self.assertEqual(parsed.start_max_c, 65)
        self.assertEqual(parsed.abort_c, 78)
        self.assertEqual(parsed.command, ["true"])

        for field, value in (
            ("expected_gpus", 7),
            ("start_max_c", 76),
            ("abort_c", 67),
            ("poll_seconds", 0),
            ("sample_timeout_seconds", 3),
            ("termination_grace_seconds", 781),
        ):
            candidate = argparse.Namespace(**vars(parsed))
            setattr(candidate, field, value)
            with self.subTest(field=field), self.assertRaises(guard.GuardError):
                guard.validate_args(candidate)

    def test_cli_uses_real_subprocess_boundaries_with_fake_telemetry(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            nvidia_smi = root / "nvidia-smi"
            rows = "\\n".join(
                f"{index}, GPU-{index:032x}, NVIDIA RTX PRO 6000, "
                "40, 100, 600, 50, 20, 1000, 96000"
                for index in range(8)
            )
            nvidia_smi.write_text(f"#!/bin/sh\nprintf '%b' '{rows}\\n'\n")
            nvidia_smi.chmod(0o755)
            output = root / "result.json"
            authorization = supervised_authorization(root)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ):
                result = guard.main(
                    [
                        "--supervised-authorization-file",
                        str(authorization),
                        "--nvidia-smi",
                        str(nvidia_smi),
                        "--output",
                        str(output),
                        "--",
                        sys.executable,
                        "-c",
                        "pass",
                    ]
                )
            self.assertEqual(result, 0)
            record = final_record(output)
            self.assertEqual(record["status"], "passed")
            self.assertEqual(record["telemetry"]["samples"], 2)

    def test_cli_moratorium_blocks_before_telemetry_or_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "must-not-exist.jsonl"
            telemetry = root / "must-not-run"
            fake = root / "nvidia-smi"
            fake.write_text(f"#!/bin/sh\ntouch {telemetry}\nexit 1\n")
            fake.chmod(0o755)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ):
                result = guard.main(
                    [
                        "--nvidia-smi",
                        str(fake),
                        "--output",
                        str(output),
                        "--",
                        "true",
                    ]
                )
            self.assertEqual(result, 2)
            self.assertFalse(telemetry.exists())
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
