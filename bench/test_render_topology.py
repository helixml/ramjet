import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest

import yaml

RENDERER = (
    pathlib.Path(__file__).resolve().parents[1]
    / "deploy" / "qwen38_27b" / "render_topology.py"
)
spec = importlib.util.spec_from_file_location("render_topology", RENDERER)
render_topology = importlib.util.module_from_spec(spec)
spec.loader.exec_module(render_topology)


def render(gpus, tp, numa=""):
    return render_topology.render(gpus, tp, numa.split(";") if numa else None, "t.yaml")


class RenderTopologyTests(unittest.TestCase):
    def test_every_supported_topology_renders_valid_compose(self):
        for gpus, tp, engines in [
            (1, 1, 1), (2, 1, 2), (2, 2, 1),
            (4, 1, 4), (4, 2, 2), (4, 4, 1),
            (8, 1, 8), (8, 2, 4), (8, 4, 2),
        ]:
            with self.subTest(gpus=gpus, tp=tp):
                document = yaml.safe_load(render(gpus, tp))
                services = document["services"]
                rendered = [k for k in services if k.startswith("qwen38-e")]
                self.assertEqual(len(rendered), engines)
                for name in rendered:
                    command = services[name]["command"]
                    self.assertIn(f"--tensor-parallel-size={tp}", command)

    def test_each_gpu_is_assigned_to_exactly_one_engine(self):
        # Two engines sharing a GPU would appear to start and then fail on
        # memory, or silently halve each other's KV pool.
        for gpus, tp in [(2, 1), (4, 2), (8, 1), (8, 2), (8, 4)]:
            with self.subTest(gpus=gpus, tp=tp):
                services = yaml.safe_load(render(gpus, tp))["services"]
                assigned = []
                for name, service in services.items():
                    if not name.startswith("qwen38-e"):
                        continue
                    devices = service["deploy"]["resources"]["reservations"]["devices"]
                    assigned.extend(devices[0]["device_ids"])
                self.assertEqual(sorted(assigned), sorted({*assigned}))
                self.assertEqual(len(assigned), gpus)

    def test_upstreams_and_kv_endpoints_match_the_engine_count(self):
        services = yaml.safe_load(render(8, 2))["services"]
        environment = services["ds4-loadbalancer"]["environment"]
        for key in (
            "MD_UPSTREAM",
            "MD_KV_EVENT_LIVE_ENDPOINTS",
            "MD_KV_EVENT_REPLAY_ENDPOINTS",
        ):
            # Cardinality must match the upstream list exactly; AGENTS.md calls
            # this out as a single-homing footgun.
            self.assertEqual(environment[key].count(","), 3, key)

    def test_the_base_pair_is_parked_so_it_cannot_hold_gpus(self):
        services = yaml.safe_load(render(4, 2))["services"]
        for name in ("qwen38-a", "qwen38-b"):
            self.assertEqual(services[name]["profiles"], ["base-pair-only"])

    def test_ports_are_unique_per_engine(self):
        services = yaml.safe_load(render(8, 1))["services"]
        ports = [
            service["ports"][0]
            for name, service in services.items()
            if name.startswith("qwen38-e")
        ]
        self.assertEqual(len(ports), len(set(ports)))

    def test_impossible_topologies_are_rejected(self):
        for gpus, tp in [(6, 4), (8, 3), (2, 4), (0, 1)]:
            with self.subTest(gpus=gpus, tp=tp):
                result = subprocess.run(
                    [sys.executable, str(RENDERER), "--gpus", str(gpus),
                     "--tensor-parallel", str(tp), "--json"],
                    capture_output=True, text=True,
                )
                self.assertNotEqual(result.returncode, 0)

    def test_the_plan_reports_the_vram_a_topology_needs(self):
        result = subprocess.run(
            [sys.executable, str(RENDERER), "--gpus", "4",
             "--tensor-parallel", "2", "--json"],
            capture_output=True, text=True, check=True,
        )
        import json
        plan = json.loads(result.stdout)
        self.assertEqual(plan["engines"], 2)
        # Splitting 28GiB of weights two ways must halve the per-GPU figure,
        # which is what tells a reader whether their card can run it.
        self.assertAlmostEqual(plan["weights_gib_per_gpu"], 14.0, places=1)
        self.assertGreater(plan["min_vram_gib_per_gpu"], 14.0)

    def test_committed_renders_match_the_generator(self):
        # A hand-edited overlay would drift from the generator silently.
        directory = RENDERER.parent
        for path in sorted(directory.glob("topology.*gpu-tp*.yaml")):
            with self.subTest(path=path.name):
                stem = path.stem.split(".")[1]
                gpus = int(stem.split("gpu")[0])
                tp = int(stem.split("tp")[1])
                numa = "0-11,24-35;12-23,36-47" if gpus == 8 else ""
                expected = render_topology.render(
                    gpus, tp, numa.split(";") if numa else None, path.name
                )
                self.assertEqual(path.read_text(), expected)
