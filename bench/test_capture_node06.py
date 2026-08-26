import hashlib
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "bench" / "capture_node06.sh"


class CaptureNode06Tests(unittest.TestCase):
    def executable(self, directory, name, body):
        path = directory / name
        path.write_text(textwrap.dedent(body).lstrip())
        path.chmod(path.stat().st_mode | stat.S_IXUSR)
        return path

    def test_shell_syntax(self):
        subprocess.run(["bash", "-n", str(SCRIPT)], check=True)

    def test_qwen_profile_selects_direct_candidate_without_remote_secrets(self):
        with tempfile.TemporaryDirectory() as raw:
            temporary = Path(raw)
            self.executable(
                temporary,
                "ssh",
                """
                #!/bin/sh
                printf '%s\\n' "$@"
                # Consume the transmitted script without executing it.
                dd of=/dev/null 2>/dev/null
                """,
            )
            result = subprocess.run(
                [str(SCRIPT), "--profile", "qwen38-flash-next", "node06"],
                env={**os.environ, "PATH": f"{temporary}:{os.environ['PATH']}"},
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=True,
            )
            self.assertIn("/home/luke/inference/qwen38_flash_next", result.stdout)
            self.assertIn("qwen38flashnext-b", result.stdout)
            self.assertNotIn("VLLM_API_KEY", result.stdout)

    def test_default_profile_retains_legacy_deployment_and_engines(self):
        with tempfile.TemporaryDirectory() as raw:
            temporary = Path(raw)
            self.executable(
                temporary,
                "ssh",
                """
                #!/bin/sh
                printf '%s\\n' "$@"
                dd of=/dev/null 2>/dev/null
                """,
            )
            result = subprocess.run(
                [str(SCRIPT), "node06"],
                env={**os.environ, "PATH": f"{temporary}:{os.environ['PATH']}"},
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=True,
            )
            self.assertIn("/home/luke/inference/dspark_0731", result.stdout)
            self.assertIn("dspark-0731", result.stdout)
            self.assertIn("dspark-0731-b", result.stdout)

    def test_local_capture_is_bounded_and_marks_unrouted_candidate_direct_only(self):
        with tempfile.TemporaryDirectory() as raw:
            temporary = Path(raw)
            deployment = temporary / "deployment"
            deployment.mkdir()
            compose = deployment / "docker-compose.yaml"
            compose.write_text("services: {}\n")
            fake_bin = temporary / "bin"
            fake_bin.mkdir()

            self.executable(
                fake_bin,
                "docker",
                r"""
                #!/bin/sh
                command=$1
                shift
                if [ "$command" = logs ]; then
                  echo 'launcher --api-key top-secret-value'
                  echo 'GPU KV cache size: 2667258 tokens'
                  echo 'Maximum concurrency for 262144 tokens per request: 10.17x'
                  exit 0
                fi
                [ "$command" = inspect ] || exit 1
                if [ "${1:-}" != --format ]; then
                  exit 0
                fi
                format=$2
                container=$3
                case "$format" in
                  *'range .Config.Env'*)
                    echo 'VLLM_API_KEY=top-secret-value'
                    echo 'RJ_UPSTREAM=http://production-a:8000'
                    ;;
                  *'.Config.Image'*) echo 'registry.example/test@sha256:abc' ;;
                  *'.Image'*) echo 'sha256:def' ;;
                  *'.State.Status'*) echo running ;;
                  *'.State.StartedAt'*) echo '2026-08-26T00:00:00Z' ;;
                  *'.RestartCount'*) echo 0 ;;
                  *'.HostConfig.CpusetCpus'*) echo '12-23,36-47' ;;
                  *'.HostConfig.CpusetMems'*) echo 1 ;;
                  *'model.repository'*) echo 'Qwen/Qwen3.8-Flash-Next-FP8' ;;
                  *'model.revision'*) echo 'bcd9f01' ;;
                  *'json .Path'*) echo '"vllm"["serve","--api-key","top-secret-value"]' ;;
                  *'.Name'*) echo "/$container status=running" ;;
                  *) echo unknown ;;
                esac
                """,
            )
            self.executable(
                fake_bin,
                "nvidia-smi",
                """
                #!/bin/sh
                case "$*" in
                  *driver_version*) echo '595.84' ;;
                  *--query-gpu=index,name*) echo '4, RTX PRO 6000, 52, 300, 600, 0, 0, 92457, 97887' ;;
                  *'-q -d'*)
                    echo 'GPU 00000000:01:00.0'
                    echo '        GPU Slowdown Temp : 85 C'
                    echo '        GPU Shutdown Temp : 90 C'
                    echo '        Current Power Limit : 600 W'
                    ;;
                  *'topo -m'*) echo 'GPU0 X' ;;
                  *) exit 1 ;;
                esac
                """,
            )
            self.executable(
                fake_bin,
                "curl",
                """
                #!/bin/sh
                case "$*" in
                  *9100/metrics*)
                    echo 'node_ipmi_temperature_celsius{sensor="CPU0_TEMP"} 65'
                    echo 'node_ipmi_temperature_celsius{sensor="FP_TEMP"} 37'
                    ;;
                  *8007/metrics*)
                    echo 'ramjet_upstream_up{upstream="http://production-a:8000"} 1'
                    echo 'ramjet_upstream_inflight{upstream="http://production-a:8000"} 2'
                    echo 'ramjet_upstream_load_units{upstream="http://production-a:8000"} 3'
                    ;;
                  *) exit 1 ;;
                esac
                """,
            )
            self.executable(
                fake_bin,
                "numactl",
                """
                #!/bin/sh
                echo 'node 0 free: 1000 MB'
                echo 'node 1 free: 2000 MB'
                """,
            )
            self.executable(
                fake_bin,
                "lscpu",
                """
                #!/bin/sh
                echo 'NUMA node(s): 2'
                """,
            )

            result = subprocess.run(
                [
                    str(SCRIPT),
                    "--local",
                    "--deployment-dir",
                    str(deployment),
                    "--engine",
                    "qwen38flashnext-b",
                    "--direct-candidate",
                    "qwen38flashnext-b",
                ],
                env={**os.environ, "PATH": f"{fake_bin}:{os.environ['PATH']}"},
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=True,
            )
            output = result.stdout
            self.assertIn(
                f"compose_sha256={hashlib.sha256(compose.read_bytes()).hexdigest()}",
                output,
            )
            self.assertIn("intake_sensor=FP_TEMP intake_air_c=37", output)
            self.assertIn(
                "engine=qwen38flashnext-b role=direct_candidate route_status=direct-only",
                output,
            )
            self.assertRegex(output, r"argv_sha256=[0-9a-f]{64}")
            self.assertIn("lb_metrics_up_series=1 lb_metrics_up_sum=1", output)
            self.assertIn("engine_capacity_kv_tokens=2667258", output)
            self.assertIn(
                "engine_capacity_context_tokens=262144 multiplier=10.17", output
            )
            self.assertNotIn("top-secret-value", output)
            self.assertNotIn("http://production-a:8000", output)
            self.assertNotIn("--api-key", output)

    def test_direct_candidate_must_be_in_engine_set(self):
        result = subprocess.run(
            [
                str(SCRIPT),
                "--local",
                "--deployment-dir",
                "/does/not/matter",
                "--engine",
                "engine-a",
                "--direct-candidate",
                "engine-b",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(2, result.returncode)
        self.assertIn("direct candidate must also be a captured engine", result.stderr)


if __name__ == "__main__":
    unittest.main()
