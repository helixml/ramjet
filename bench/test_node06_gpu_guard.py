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


def pid_is_active(pid):
    try:
        raw = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return raw[raw.rfind(")") + 2 :].split()[0] != "Z"


def sample(temperatures, utilization=50, power=100, air=30):
    """Builds a sample. `air` is the intake temperature the guard gates on;
    `temperatures` are GPU readings, which are now recorded but never gate."""

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
    return guard.GpuSample(
        tuple(rows), (guard.AirReading(sensor="FP_TEMP", temperature_c=air),)
    )


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


class FakeAirExporter:
    """Serves one Prometheus text payload on loopback for the guard to read."""

    def __init__(self, celsius=30, sensor="FP_TEMP"):
        import http.server
        import threading

        payload = (
            "# HELP node_ipmi_temperature_celsius IPMI temperature\n"
            "# TYPE node_ipmi_temperature_celsius gauge\n"
            'node_ipmi_temperature_celsius{sensor="CPU0_TEMP"} 65\n'
            f'node_ipmi_temperature_celsius{{sensor="{sensor}"}} {celsius}\n'
        ).encode()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("content-type", "text/plain")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *_args):
                pass

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}/metrics"

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class Node06GpuGuardTests(unittest.TestCase):
    def args(self, output, command):
        return argparse.Namespace(
            output=pathlib.Path(output),
            label="test-cell",
            expected_gpus=8,
            start_max_c=40,
            abort_c=50,
            cooldown_timeout_seconds=0,
            poll_seconds=0.01,
            sample_timeout_seconds=1,
            workload_grace_seconds=0.1,
            termination_grace_seconds=1,
            preserve_rollback_owner=False,
            nvidia_smi="/usr/bin/nvidia-smi",
            air_metrics_url="http://127.0.0.1:9100/metrics",
            max_runtime_seconds=guard.DEFAULT_MAX_RUNTIME_SECONDS,
            command=command,
        )

    def test_the_abort_ceiling_stays_below_hardware_throttle_onset(self):
        # node06's RTX PRO 6000 Blackwell devices throttle at 85C and shut down
        # at 90C. A ceiling at or above 85C measures throttled hardware; at or
        # above 90C it can never fire because the driver cuts power first.
        self.assertLess(
            guard.MAX_ABORT_C, 85, "ceiling must stay below throttle onset"
        )
        self.assertLessEqual(guard.DEFAULT_ABORT_C, guard.MAX_ABORT_C)

    def test_process_snapshot_ignores_a_task_that_exits_during_stat_read(self):
        class VanishedTask:
            name = "123"

            def __truediv__(self, _name):
                return self

            def read_text(self, **_kwargs):
                raise ProcessLookupError(3, "No such process")

        proc = mock.Mock()
        proc.iterdir.return_value = [VanishedTask()]
        with mock.patch.object(guard.pathlib, "Path", return_value=proc):
            self.assertEqual(guard.process_snapshot(), {})

    def test_an_abort_threshold_above_the_ceiling_is_rejected(self):
        parsed = guard.parser().parse_args(
            ["--output", "/tmp/x.jsonl", "--abort-c", "95", "--", "/bin/true"]
        )
        with self.assertRaises(guard.GuardError):
            guard.validate_args(parsed)

    def test_continuous_inference_is_capped(self):
        self.assertEqual(guard.MAX_RUNTIME_SECONDS, 1500)
        parsed = guard.parser().parse_args(
            [
                "--output", "/tmp/x.jsonl",
                "--max-runtime-seconds", str(guard.MAX_RUNTIME_SECONDS + 1),
                "--", "/bin/true",
            ]
        )
        with self.assertRaises(guard.GuardError):
            guard.validate_args(parsed)

    def test_a_long_but_cool_run_is_terminated_by_the_runtime_limit(self):
        # Temperature never approaches the ceiling, so only the continuous
        # inference cap can stop this workload.
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "journal.jsonl"
            args = self.args(output, ["/bin/sleep", "30"])
            args.max_runtime_seconds = 1
            args.poll_seconds = 0.25
            code = self.run_guard(args, SequenceSampler([sample([50] * 8)]))
            self.assertEqual(code, guard.EXIT_RUNTIME_LIMIT)
            record = final_record(output)
            self.assertEqual(record["reason"], "runtime_limit")
            self.assertEqual(record["thresholds"]["max_runtime_seconds"], 1)

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
            result = self.run_guard(args, SequenceSampler([sample([50] * 8, air=51)]))
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "preflight_too_hot")
            self.assertNotIn("child_exit_code", record)
            self.assertEqual(record["trigger"]["sensor"], "FP_TEMP")

    def test_preflight_at_abort_temperature_waits_for_cool_start(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.jsonl"
            args = self.args(output, [sys.executable, "-c", "pass"])
            args.cooldown_timeout_seconds = 300
            sampler = SequenceSampler(
                [sample([50] * 8, air=50), sample([50] * 8, air=40)]
            )
            result = self.run_guard(args, sampler)
            self.assertEqual(result, 0)
            self.assertGreaterEqual(sampler.index, 2)
            self.assertEqual(final_record(output)["status"], "passed")

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
            sampler = SequenceSampler([sample([40] * 8, air=30), sample([40] * 8, air=50)])
            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "thermal_abort")
            self.assertEqual(record["trigger"], {"sensor": "FP_TEMP", "temperature_c": 50})
            self.assertIn("termination_escalated", record)

    def test_final_sample_rejects_a_hot_short_child(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "result.json"
            args = self.args(output, [sys.executable, "-c", "pass"])
            sampler = SequenceSampler([sample([40] * 8, air=30), sample([40] * 8, air=50)])
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
            ready = pathlib.Path(directory) / "sigterm-handler-ready"
            args = self.args(
                output,
                [
                    sys.executable,
                    "-c",
                    "import pathlib,signal,time; "
                    "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                    "pathlib.Path(%r).touch(); "
                    "time.sleep(60)" % str(ready),
                ],
            )
            args.poll_seconds = 0.1
            args.termination_grace_seconds = 1
            cool = sample([40] * 8, air=30)
            hot = sample([40] * 8, air=55)

            def sampler(_args):
                # The abort must not race child interpreter startup: on a
                # loaded runner SIGTERM could land before the handler is
                # installed, killing the child without escalation. Stay cool
                # until the child reports the handler is live.
                return hot if ready.exists() else cool

            result = self.run_guard(args, sampler)
            self.assertEqual(result, guard.EXIT_THERMAL)
            record = final_record(output)
            self.assertEqual(record["reason"], "thermal_abort")
            self.assertTrue(record["termination_escalated"])
            # Bounded means far below the 60s sleep and near the 1s grace;
            # the margin absorbs interpreter startup on a loaded runner.
            self.assertLess(record["duration_seconds"], 3.0)

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
            capability = guard.create_guard_capability(8, 50, "a" * 32)
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
            forged["RAMJET_GPU_GUARD_CAPABILITY_FD"] = "999999"
            invalid = subprocess.run(
                [sys.executable, "-c", script, module_dir],
                env=forged,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(invalid.returncode, 0)

    def test_guard_sigkill_cancels_candidate_owned_request_process(self):
        exporter = FakeAirExporter()
        self.addCleanup(exporter.close)
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
            module_dir = str(pathlib.Path(guard.__file__).parent)
            launcher = (
                "import sys; sys.path.insert(0, sys.argv.pop(1)); "
                "import node06_operational_moratorium as m; "
                "m.MORATORIUM_ACTIVE=False; import node06_gpu_guard as g; "
                "raise SystemExit(g.main(sys.argv[1:]))"
            )
            process = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    launcher,
                    module_dir,
                    "--nvidia-smi",
                    str(nvidia_smi),
                    "--air-metrics-url",
                    exporter.url,
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
                return sample([40] * 8, air=55) if ready.exists() else sample([40] * 8, air=30)

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
        self.assertEqual(parsed.start_max_c, 40)
        self.assertEqual(parsed.abort_c, 50)
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
        exporter = FakeAirExporter()
        self.addCleanup(exporter.close)
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
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ), mock.patch.object(moratorium, "MORATORIUM_ACTIVE", False):
                result = guard.main(
                    [
                        "--nvidia-smi",
                        str(nvidia_smi),
                        "--air-metrics-url",
                        exporter.url,
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

    def test_transient_telemetry_miss_is_tolerated_but_sustained_loss_aborts(self):
        exporter = FakeAirExporter()
        self.addCleanup(exporter.close)
        # node06's driver intermittently exceeds the 2s sample deadline (~1
        # call in 12 measured at 1Hz). Failing on the first miss made every run
        # longer than a few seconds abort spuriously, so a bounded run of
        # misses is absorbed -- but sustained blindness must still fail closed.
        self.assertGreaterEqual(guard.MAX_CONSECUTIVE_TELEMETRY_FAILURES, 2)

        for failures, expect_abort in (
            (guard.MAX_CONSECUTIVE_TELEMETRY_FAILURES - 1, False),
            (guard.MAX_CONSECUTIVE_TELEMETRY_FAILURES, True),
        ):
            with self.subTest(failures=failures):
                remaining = [failures]
                real = guard.query_gpus

                def flaky(args, _remaining=remaining, _real=real):
                    if _remaining[0] > 0:
                        _remaining[0] -= 1
                        raise guard.GuardError("GPU telemetry query failed")
                    return _real(args)

                with tempfile.TemporaryDirectory() as directory:
                    output = pathlib.Path(directory) / "guard.jsonl"
                    fake = pathlib.Path(directory) / "nvidia-smi"
                    rows = "\\n".join(
                        f"{index}, GPU-{index:032x}, NVIDIA RTX PRO 6000, "
                        "40, 100, 600, 50, 20, 1000, 96000"
                        for index in range(8)
                    )
                    fake.write_text(f"#!/bin/sh\nprintf '%b' '{rows}\\n'\n")
                    fake.chmod(0o755)
                    # run_guard binds query_gpus as a parameter default, so
                    # inject the flaky sampler directly rather than patching
                    # the module attribute.
                    parsed = guard.parser().parse_args(
                        [
                            "--nvidia-smi",
                            str(fake),
                            "--air-metrics-url",
                            exporter.url,
                            "--output",
                            str(output),
                            "--poll-seconds",
                            "0.25",
                            "--",
                            "true",
                        ]
                    )
                    guard.validate_args(parsed)
                    with contextlib.redirect_stdout(
                        io.StringIO()
                    ), contextlib.redirect_stderr(io.StringIO()):
                        result = guard.run_guard(parsed, flaky)
                    terminal = records(output)[-1]
                    if expect_abort:
                        self.assertEqual(result, guard.EXIT_TELEMETRY)
                        self.assertEqual(terminal["reason"], "telemetry_unavailable")
                    else:
                        self.assertEqual(result, 0)
                        self.assertEqual(terminal["status"], "passed")
                        self.assertEqual(terminal["telemetry_retries"], failures)

    def test_cli_moratorium_blocks_before_telemetry_or_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "must-not-exist.jsonl"
            telemetry = root / "must-not-run"
            fake = root / "nvidia-smi"
            fake.write_text(f"#!/bin/sh\ntouch {telemetry}\nexit 1\n")
            fake.chmod(0o755)
            # Assert the mechanism, not the flag's current value: the
            # moratorium is lifted for a supervised window, and re-arming it
            # must still block before any telemetry or journal side effect.
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ), mock.patch.object(moratorium, "MORATORIUM_ACTIVE", True):
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

class AirTemperatureGateTests(unittest.TestCase):
    """The guard gates on chassis intake air, matching Grafana bunker-temps."""

    def test_only_the_dashboard_intake_sensors_are_admitted(self):
        # The same exporter publishes CPU, DIMM, and per-slot GPU temperatures
        # under this metric name. Admitting those would put a 65C CPU reading
        # on a 50C room gate and abort every run instantly.
        payload = (
            'node_ipmi_temperature_celsius{sensor="CPU0_TEMP"} 65\n'
            'node_ipmi_temperature_celsius{sensor="DIMMG1_TEMP"} 46\n'
            'node_ipmi_temperature_celsius{sensor="SLOT3_GPU_TEMP"} 59\n'
            'node_ipmi_temperature_celsius{sensor="FP_TEMP"} 43\n'
            'node_ipmi_temperature_celsius{sensor="Inlet Temp"} 37\n'
        )
        readings = guard.parse_air_metrics(payload)
        self.assertEqual(
            sorted((r.sensor, r.temperature_c) for r in readings),
            [("FP_TEMP", 43.0), ("Inlet Temp", 37.0)],
        )

    def test_the_hottest_intake_sensor_decides(self):
        readings = (
            guard.AirReading(sensor="Inlet Temp", temperature_c=37),
            guard.AirReading(sensor="FP_TEMP", temperature_c=43),
        )
        built = guard.GpuSample(sample([90] * 8).readings, readings)
        self.assertEqual(built.hottest_air.sensor, "FP_TEMP")
        # A 90C GPU must not influence the decision any more.
        self.assertEqual(built.hottest_air.temperature_c, 43)

    def test_missing_intake_telemetry_fails_closed(self):
        # An exporter that publishes everything except the intake sensors
        # would otherwise leave the run ungated.
        with self.assertRaises(guard.GuardError):
            exporter = FakeAirExporter(sensor="CPU0_TEMP")
            self.addCleanup(exporter.close)
            args = argparse.Namespace(
                air_metrics_url=exporter.url, sample_timeout_seconds=2
            )
            guard.query_air(args)

    def test_an_unreachable_exporter_fails_closed(self):
        args = argparse.Namespace(
            # Port 1 on loopback is not listening.
            air_metrics_url="http://127.0.0.1:1/metrics",
            sample_timeout_seconds=1,
        )
        with self.assertRaises(guard.GuardError):
            guard.query_air(args)

    def test_the_gate_must_not_depend_on_a_remote_query(self):
        # A watchdog that reads the network fails open exactly when the
        # network is the problem.
        parsed = guard.parser().parse_args(
            ["--output", "/tmp/x.jsonl", "--air-metrics-url",
             "http://grafana.example.invalid/metrics", "--", "/bin/true"]
        )
        with self.assertRaises(guard.GuardError):
            guard.validate_args(parsed)

    def test_the_ceiling_is_a_room_scale_temperature(self):
        self.assertEqual(guard.MAX_ABORT_C, 50)
        self.assertLess(guard.DEFAULT_START_MAX_C, guard.MAX_ABORT_C)
        parsed = guard.parser().parse_args(
            ["--output", "/tmp/x.jsonl", "--abort-c", "78", "--", "/bin/true"]
        )
        with self.assertRaises(guard.GuardError):
            guard.validate_args(parsed)

    def test_intake_temperature_is_recorded_in_the_summary(self):
        summary = guard.SampleSummary()
        summary.observe(sample([40] * 8, air=44))
        summary.observe(sample([40] * 8, air=41))
        self.assertEqual(summary.public()["max_air_temperature_c"], 44)
