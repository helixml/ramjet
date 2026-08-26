import pathlib
import re
import shlex
import subprocess
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("qwen38_route_load_cap_abba.sh")


class QwenRouteLoadCapAbbaTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = SCRIPT.read_text()

    @classmethod
    def function(cls, name):
        match = re.search(
            rf"^{re.escape(name)}\(\) \{{\n.*?^\}}$",
            cls.source,
            re.MULTILINE | re.DOTALL,
        )
        if match is None:
            raise AssertionError(f"missing shell function {name}")
        return match.group(0)

    def test_shell_is_well_formed(self):
        result = subprocess.run(
            ["bash", "-n", str(SCRIPT)],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_mutations_are_bounded_exact_image_lb_only(self):
        compose_calls = re.findall(
            r'docker compose -f "\$compose_file" up -d --no-deps --force-recreate[\s\\]+'
            r'ds4-loadbalancer', self.source
        )
        self.assertEqual(len(compose_calls), 2)
        self.assertEqual(
            self.source.count('timeout --foreground "$compose_timeout_seconds"'), 5
        )
        self.assertEqual(
            self.source.count('"${compose_environment[@]}" RJ_ROUTE_MAX_LOAD_UNITS='),
            5,
        )
        self.assertNotRegex(self.source, r"docker compose .*\b(qwen38flashnext-a|qwen38flashnext-b)\b")

    def test_rollback_stays_armed_and_reproves_the_engines(self):
        armed = self.source.index("trap rollback EXIT")
        first_mutation = self.source.index("render_and_recreate 8 cap8-a1")
        disarmed = self.source.rindex("trap - EXIT INT TERM HUP")
        final_engine_proof = self.source.index(
            'require_engines_unchanged "$experiment_dir/engines.after.txt"'
        )
        self.assertLess(armed, first_mutation)
        self.assertLess(final_engine_proof, disarmed)
        rollback = self.source[self.source.index("rollback() {") : self.source.index("render_and_recreate() {")]
        self.assertIn('rollback_wait_for_lb 8', rollback)
        self.assertIn('rollback-engines.txt', rollback)
        self.assertIn('rollback-status.json', rollback)

    def test_failure_signal_and_rollback_failure_all_execute_rollback(self):
        rollback = self.function("rollback")
        cases = (
            ("exit 75", 0, 0, 0, 75, "passed"),
            ("kill -TERM $$", 0, 0, 0, 143, "passed"),
            ("exit 0", 1, 0, 0, 1, "failed"),
            ("exit 0", 0, 1, 0, 1, "failed"),
            ("exit 0", 0, 1, 1, 1, "failed"),
        )
        for action, timeout_result, health_result, engine_result, expected_status, rollback_result in cases:
            with self.subTest(action=action, timeout_result=timeout_result):
                with tempfile.TemporaryDirectory() as directory:
                    root = pathlib.Path(directory)
                    script = f"""
set -uo pipefail
deployment_dir={shlex.quote(directory)}
compose_file=$deployment_dir/docker-compose.yaml
compose_timeout_seconds=60
lb_image=exact-image
compose_environment=(env LB_IMAGE=exact-image)
experiment_dir=$deployment_dir
mutated=1
timeout() {{ printf 'timeout %s\\n' "$*" >>"$experiment_dir/calls"; return {timeout_result}; }}
rollback_wait_for_lb() {{ printf 'wait %s\\n' "$1" >>"$experiment_dir/calls"; return {health_result}; }}
record_engines_unchanged() {{ printf 'engines %s\\n' "$1" >>"$experiment_dir/calls"; return {engine_result}; }}
{rollback}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
{action}
"""
                    result = subprocess.run(
                        ["bash", "-c", script],
                        check=False,
                        capture_output=True,
                        text=True,
                        timeout=5,
                    )
                    self.assertEqual(result.returncode, expected_status, result.stderr)
                    calls = (root / "calls").read_text()
                    self.assertIn("RJ_ROUTE_MAX_LOAD_UNITS=8", calls)
                    self.assertIn("ds4-loadbalancer", calls)
                    self.assertIn("wait 8", calls)
                    self.assertIn("rollback-engines.txt", calls)
                    rollback_status = (root / "rollback-status.json").read_text()
                    self.assertIn(f'"result": "{rollback_result}"', rollback_status)
                    self.assertNotIn("cap 8 remains live", result.stdout)

    def test_preflight_failure_makes_no_compose_call(self):
        preflight = self.function("preflight")
        script = f"""
set -euo pipefail
deployment_dir=/not-used
compose_file=/not-used/compose.yaml
hostname() {{ printf 'wrong-node\\n'; }}
docker() {{ printf 'unexpected compose call\\n'; }}
fail() {{ exit 2; }}
{preflight}
preflight /not-used
docker compose up
"""
        result = subprocess.run(
            ["bash", "-c", script],
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_campaign_timeout_returns_to_exact_cap8_rollback(self):
        run_cell = self.function("run_cell")
        rollback = self.function("rollback")
        remaining = self.function("campaign_remaining_before_rollback")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            script = f"""
set -euo pipefail
experiment_dir={shlex.quote(directory)}
deployment_dir={shlex.quote(directory)}
compose_file=$deployment_dir/docker-compose.yaml
compose_timeout_seconds=60
campaign_max_seconds=25
rollback_budget_seconds=10
guard_kill_grace_seconds=5
campaign_started=$((SECONDS - 5))
metrics_urls=unused
model=unused
engines=(engine-a engine-b)
compose_environment=(env)
mutated=1
fail() {{ printf 'fail %s\\n' "$*" >>"$experiment_dir/calls"; exit 2; }}
timeout() {{
  printf 'timeout %s\\n' "$*" >>"$experiment_dir/calls"
  if [[ $* == *--kill-after=45* ]]; then return 124; fi
  return 0
}}
rollback_wait_for_lb() {{ printf 'wait %s\\n' "$1" >>"$experiment_dir/calls"; return 0; }}
record_engines_unchanged() {{ printf 'engines %s\\n' "$1" >>"$experiment_dir/calls"; return 0; }}
{rollback}
{remaining}
{run_cell}
trap rollback EXIT
run_cell expiry 32 128000 16 512 3 200 300
"""
            result = subprocess.run(
                ["bash", "-c", script],
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 124, result.stderr)
            calls = (root / "calls").read_text()
            self.assertRegex(calls, r"timeout --foreground --kill-after=45 5 python3")
            self.assertIn("RJ_ROUTE_MAX_LOAD_UNITS=8", calls)
            self.assertLess(
                calls.index("--kill-after=45"), calls.index("RJ_ROUTE_MAX_LOAD_UNITS=8")
            )
            rollback_status = (root / "rollback-status.json").read_text()
            self.assertIn('"result": "passed"', rollback_status)

    def test_near_deadline_refuses_render_before_any_mutation(self):
        render = self.function("render_and_recreate")
        require_budget = self.function("require_campaign_budget")
        remaining = self.function("campaign_remaining_before_rollback")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            script = f"""
set -euo pipefail
experiment_dir={shlex.quote(directory)}
deployment_dir={shlex.quote(directory)}
compose_file=$deployment_dir/docker-compose.yaml
compose_timeout_seconds=60
campaign_max_seconds=300
rollback_budget_seconds=180
campaign_started=$((SECONDS - 119))
render_budget_seconds=255
post_render_mutation_budget_seconds=135
lb_image=exact
upstreams=http://a,http://b
compose_environment=(env)
mutated=0
fail() {{ printf 'failed %s\\n' "$*"; exit 2; }}
wait_for_idle() {{ printf 'unexpected idle\\n' >>"$experiment_dir/calls"; }}
timeout() {{ printf 'unexpected mutation %s\\n' "$*" >>"$experiment_dir/calls"; }}
wait_for_lb() {{ printf 'unexpected health\\n' >>"$experiment_dir/calls"; }}
require_engines_unchanged() {{ printf 'unexpected engines\\n' >>"$experiment_dir/calls"; }}
{remaining}
{require_budget}
{render}
render_and_recreate 32 cap32-near-deadline
"""
            result = subprocess.run(
                ["bash", "-c", script],
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("lacks the bounded pre-rollback budget", result.stdout)
            self.assertFalse((root / "calls").exists())

    def test_secret_bearing_render_is_never_persisted(self):
        self.assertNotRegex(
            self.source,
            r"config --format json\s*>\s*[^|\n]*render",
        )
        self.assertIn('then .value = "<redacted>"', self.source)
        self.assertIn('route_max_load_units:', self.source)
        self.assertNotIn('RJ_UPSTREAM_TOKEN:', self.source)

    def test_render_parity_proof_contains_no_mocked_secret(self):
        function = self.function("prove_render_delta")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            mock = root / "compose.py"
            mock.write_text(
                """import json, sys
cap = next(item.split('=', 1)[1] for item in sys.argv if item.startswith('RJ_ROUTE_MAX_LOAD_UNITS='))
print(json.dumps({'services': {'ds4-loadbalancer': {'image': 'exact', 'environment': {
    'RJ_ROUTE_MAX_LOAD_UNITS': cap,
    'VLLM_API_KEY': 'must-not-persist',
    'RJ_UPSTREAM_TOKEN': 'must-not-persist',
}}}}))
"""
            )
            shell = f"""
set -euo pipefail
experiment_dir={shlex.quote(directory)}
compose_file=/not-used
compose_environment=(python3 {shlex.quote(str(mock))})
compose_timeout_seconds=60
post_render_mutation_budget_seconds=135
require_campaign_budget() {{ :; }}
fail() {{ echo "$*" >&2; exit 2; }}
{function}
prove_render_delta
"""
            result = subprocess.run(
                ["bash", "-c", shell],
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            proof = (root / "cap-render-parity.json").read_text()
            self.assertNotIn("must-not-persist", proof)
            self.assertRegex(proof, r'"only_route_max_load_units_varies": true')

    def test_guard_and_campaign_runtime_authority_is_bounded(self):
        values = {
            key: int(value)
            for key, value in re.findall(
                r"^(smoke_max_seconds|full_max_seconds|campaign_max_seconds)=([0-9]+)$",
                self.source,
                re.MULTILINE,
            )
        }
        self.assertLessEqual(values["smoke_max_seconds"] + 4 * values["full_max_seconds"], 1500)
        self.assertLessEqual(values["campaign_max_seconds"], 1800)
        self.assertIn('--max-runtime-seconds "$cell_runtime_seconds"', self.source)
        self.assertIn('rollback_budget_seconds=180', self.source)
        self.assertIn('guard_kill_grace_seconds=30', self.source)
        self.assertIn('timeout --foreground --kill-after=45 "$available_seconds"', self.source)

    def test_success_reproves_cap8_and_records_nonpromotion(self):
        finish = self.function("finish_campaign")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            script = f"""
set -euo pipefail
experiment_dir={shlex.quote(directory)}
mutated=1
require_lb() {{ printf 'lb %s\\n' "$1" >>"$experiment_dir/calls"; }}
require_engines_unchanged() {{ printf 'engines %s\\n' "$1" >>"$experiment_dir/calls"; }}
jq() {{ printf '{{"promotion_applied":false}}\\n'; }}
{finish}
finish_campaign
printf 'mutated=%s\\n' "$mutated" >>"$experiment_dir/calls"
"""
            result = subprocess.run(
                ["bash", "-c", script],
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = (root / "calls").read_text()
            self.assertIn("lb 8", calls)
            self.assertIn("engines.after.txt", calls)
            self.assertIn("mutated=0", calls)
            comparison = (root / "comparison.json").read_text()
            self.assertIn('"promotion_applied":false', comparison)
            self.assertIn("cap 8 remains live", result.stdout)

    def test_fresh_evidence_and_final_nonpromotion_are_explicit(self):
        self.assertIn('experiment directory must contain only the staged authorities', self.source)
        self.assertIn('set -o noclobber', self.source)
        self.assertIn('capture_node06.sh" --local --profile qwen38-flash-next', self.source)
        self.assertIn('wait_for_idle', self.source)
        self.assertIn('promotion_applied: false', self.source)
        self.assertIn('cap 8 remains live', self.source)
        self.assertIn('campaign-authority.sha256', self.source)
        self.assertIn('MACHINEVIEW_NETWORK=qwen38_27b_default', self.source)
        self.assertIn('.decoder_requests_ok == ($decoders * $runs)', self.source)
        preflight = self.source.index("experiment directory must contain only the staged authorities")
        mutation_armed = self.source.index("mutated=0")
        self.assertLess(preflight, mutation_armed)


if __name__ == "__main__":
    unittest.main()
