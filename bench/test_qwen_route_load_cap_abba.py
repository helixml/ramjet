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
            self.source.count('timeout --foreground "$compose_timeout_seconds"'), 2
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
        self.assertIn('wait_for_lb 8', rollback)
        self.assertIn('rollback-engines.txt', rollback)

    def test_failure_signal_and_rollback_failure_all_execute_rollback(self):
        rollback = self.function("rollback")
        cases = (
            ("exit 75", 0, 75),
            ("kill -TERM $$", 0, 143),
            ("exit 0", 1, 1),
        )
        for action, timeout_result, expected_status in cases:
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
wait_for_lb() {{ printf 'wait %s\\n' "$1" >>"$experiment_dir/calls"; return 0; }}
require_engines_unchanged() {{ printf 'engines %s\\n' "$1" >>"$experiment_dir/calls"; return 0; }}
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
        self.assertIn('--max-runtime-seconds "$max_seconds"', self.source)

    def test_fresh_evidence_and_final_nonpromotion_are_explicit(self):
        self.assertIn('experiment directory must contain only the staged authorities', self.source)
        self.assertIn('set -o noclobber', self.source)
        self.assertIn('capture_node06.sh" --local --profile qwen38-flash-next', self.source)
        self.assertIn('wait_for_idle', self.source)
        self.assertIn('promotion_applied: false', self.source)
        self.assertIn('cap 8 remains live', self.source)
        preflight = self.source.index("experiment directory must contain only the staged authorities")
        mutation_armed = self.source.index("mutated=0")
        self.assertLess(preflight, mutation_armed)


if __name__ == "__main__":
    unittest.main()
