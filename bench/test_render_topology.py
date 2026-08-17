import importlib.util
import json
import pathlib
import re
import subprocess
import sys
import unittest

RENDERER = (
    pathlib.Path(__file__).resolve().parents[1]
    / "deploy" / "qwen38_27b" / "render_topology.py"
)
spec = importlib.util.spec_from_file_location("render_topology", RENDERER)
render_topology = importlib.util.module_from_spec(spec)
spec.loader.exec_module(render_topology)

# node06 is dual-socket; single-socket hosts render without a cpuset.
NODE06_NUMA = "0-11,24-35;12-23,36-47"

SUPPORTED = [
    (1, 1, 1), (2, 1, 2), (2, 2, 1),
    (4, 1, 4), (4, 2, 2), (4, 4, 1),
    (8, 1, 8), (8, 2, 4), (8, 4, 2),
]


def render(gpus, tp, numa="", name="t.yaml"):
    return render_topology.render(
        gpus, tp, numa.split(";") if numa else None, name
    )


class TopologyPlanTests(unittest.TestCase):
    """The plan is the contract; the YAML is one rendering of it."""

    def test_every_supported_topology_places_each_gpu_exactly_once(self):
        # Two engines sharing a GPU would appear to start and then fail on
        # memory, or silently halve each other's KV pool.
        for gpus, tp, engines in SUPPORTED:
            with self.subTest(gpus=gpus, tp=tp):
                plan = render_topology.topology_plan(gpus, tp)
                self.assertEqual(plan["engines"], engines)
                assigned = [g for e in plan["placement"] for g in e["gpu_ids"]]
                self.assertEqual(sorted(assigned), list(range(gpus)))
                for engine in plan["placement"]:
                    self.assertEqual(len(engine["gpu_ids"]), tp)

    def test_ports_are_unique_per_engine(self):
        plan = render_topology.topology_plan(8, 1)
        ports = [engine["port"] for engine in plan["placement"]]
        self.assertEqual(len(ports), len(set(ports)))

    def test_the_plan_reports_the_vram_a_topology_needs(self):
        # For a 28GiB checkpoint this is what decides whether a given card can
        # run the topology at all, so it must scale with the shard size.
        one = render_topology.topology_plan(4, 1)
        two = render_topology.topology_plan(4, 2)
        self.assertAlmostEqual(one["weights_gib_per_gpu"], 28.0, places=1)
        self.assertAlmostEqual(two["weights_gib_per_gpu"], 14.0, places=1)
        self.assertGreater(two["min_vram_gib_per_gpu"], two["weights_gib_per_gpu"])

    def test_impossible_topologies_are_rejected(self):
        for gpus, tp in [(6, 4), (8, 3), (2, 4), (0, 1), (4, 0)]:
            with self.subTest(gpus=gpus, tp=tp):
                with self.assertRaises(ValueError):
                    render_topology.validate_topology(gpus, tp)

    def test_supported_topologies_validate(self):
        for gpus, tp, _ in SUPPORTED:
            with self.subTest(gpus=gpus, tp=tp):
                render_topology.validate_topology(gpus, tp)


class RenderTests(unittest.TestCase):
    def test_render_declares_the_tensor_parallel_size_on_every_engine(self):
        for gpus, tp, engines in SUPPORTED:
            with self.subTest(gpus=gpus, tp=tp):
                text = render(gpus, tp)
                self.assertEqual(
                    text.count(f"--tensor-parallel-size={tp}\n"), engines
                )
                self.assertEqual(len(re.findall(r"^  qwen38-e\d+:$", text, re.M)), engines)

    def test_rendered_device_ids_match_the_plan(self):
        plan = render_topology.topology_plan(8, 2)
        text = render(8, 2, NODE06_NUMA)
        for engine in plan["placement"]:
            expected = ", ".join(f'"{gpu}"' for gpu in engine["gpu_ids"])
            self.assertIn(f"device_ids: [{expected}]", text)

    def test_upstreams_and_kv_endpoints_match_the_engine_count(self):
        # AGENTS.md calls out mismatched cardinality here as a single-homing
        # footgun: the router would address engines that do not exist.
        text = render(8, 2)
        for key in (
            "RJ_UPSTREAM",
            "RJ_KV_EVENT_LIVE_ENDPOINTS",
            "RJ_KV_EVENT_REPLAY_ENDPOINTS",
        ):
            line = next(l for l in text.splitlines() if l.strip().startswith(key))
            self.assertEqual(line.count("qwen38-e"), 4, key)

    def test_the_base_pair_is_parked_so_it_cannot_hold_gpus(self):
        text = render(4, 2)
        self.assertEqual(text.count('profiles: ["base-pair-only"]'), 2)

    def test_single_socket_hosts_render_without_a_cpuset(self):
        self.assertNotIn("cpuset:", render(4, 2))
        self.assertIn("cpuset:", render(8, 2, NODE06_NUMA))

    def test_committed_renders_match_the_generator(self):
        # A hand-edited overlay would drift from the generator silently.
        committed = sorted(RENDERER.parent.glob("topology.*gpu-tp*.yaml"))
        self.assertTrue(committed, "expected committed topology renders")
        for path in committed:
            with self.subTest(path=path.name):
                stem = path.stem.split(".")[1]
                gpus = int(stem.split("gpu")[0])
                tp = int(stem.split("tp")[1])
                numa = NODE06_NUMA if gpus == 8 else ""
                self.assertEqual(
                    path.read_text(), render(gpus, tp, numa, path.name)
                )


class CommandLineTests(unittest.TestCase):
    def test_json_reports_the_plan_without_writing(self):
        result = subprocess.run(
            [sys.executable, str(RENDERER), "--gpus", "4",
             "--tensor-parallel", "2", "--json"],
            capture_output=True, text=True, check=True,
        )
        plan = json.loads(result.stdout)
        self.assertEqual(plan["engines"], 2)
        self.assertEqual(len(plan["placement"]), 2)

    def test_an_impossible_topology_exits_nonzero(self):
        result = subprocess.run(
            [sys.executable, str(RENDERER), "--gpus", "6",
             "--tensor-parallel", "4", "--json"],
            capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("divisible", result.stderr)
