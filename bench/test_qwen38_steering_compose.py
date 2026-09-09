import argparse
import unittest

from bench import qwen38_steering_compose as compose


SOURCE = """services:
  ds4-loadbalancer:
    environment:
      RJ_KV_EVENT_LIVE_ENDPOINTS: tcp://qwen38flashnext-a:5557,tcp://qwen38flashnext-b:5557,tcp://qwen38flashnext-tp8:5557
      RJ_KV_EVENT_REPLAY_ENDPOINTS: tcp://qwen38flashnext-a:5558,tcp://qwen38flashnext-b:5558,tcp://qwen38flashnext-tp8:5558
      RJ_ADAPTIVE_CONFIG_PATH: /etc/ramjet/adaptive-config.json
  qwen38flashnext-b:
    <<: *vllm-engine
    container_name: qwen38flashnext-b
    command:
      - --tensor-parallel-size=4

  qwen38flashnext-tp8:
    command: []
"""


class ComposeRenderTests(unittest.TestCase):
    def test_capture_candidate_overrides_inherited_maps(self):
        args = argparse.Namespace(
            mode="capture",
            image="qwen38-steering:0.1.0",
            capture_dir=compose.pathlib.Path("/private/captures"),
            vector=None,
            control_file=None,
            scale=1.0,
            layers="all",
        )
        rendered = compose.render(SOURCE, args)
        self.assertIn("image: qwen38-steering:0.1.0", rendered)
        self.assertIn("/private/captures:/steering-captures", rendered)
        self.assertIn("VLLM_API_KEY: ${VLLM_API_KEY:-qwen-local}", rendered)
        self.assertEqual(rendered.count("QWEN38_STEERING_CAPTURE_DIR"), 1)
        self.assertIn("${RJ_KV_EVENT_LIVE_ENDPOINTS:-", rendered)
        self.assertIn("${RJ_KV_EVENT_REPLAY_ENDPOINTS:-", rendered)
        self.assertNotIn("RJ_ADAPTIVE_CONFIG_PATH", rendered)
        self.assertIn("--enforce-eager", rendered)
        engine = rendered[rendered.index("  qwen38flashnext-b:") :]
        self.assertLess(
            engine.index("/private/captures:/steering-captures"),
            engine.index("    environment:"),
        )

    def test_steering_candidate_has_vector_scale_and_layers(self):
        args = argparse.Namespace(
            mode="steer",
            image="qwen38-steering:0.1.0",
            capture_dir=None,
            vector=compose.pathlib.Path("/private/vector.safetensors"),
            control_file=None,
            scale=0.75,
            layers="24-43",
        )
        rendered = compose.render(SOURCE, args)
        self.assertIn("/private/vector.safetensors:/steering/vector.safetensors:ro", rendered)
        self.assertIn('QWEN38_STEERING_SCALE: "0.75"', rendered)
        self.assertIn('QWEN38_STEERING_LAYERS: "24-43"', rendered)

    def test_steering_candidate_can_mount_dynamic_bundle_control(self):
        args = argparse.Namespace(
            mode="steer",
            image="qwen38-steering:0.2.0",
            capture_dir=None,
            vector=compose.pathlib.Path("/private/sweep/bundle.safetensors"),
            control_file=compose.pathlib.Path("/private/sweep/control.json"),
            scale=0,
            layers="all",
        )
        rendered = compose.render(SOURCE, args)
        self.assertIn("/private/sweep:/steering:ro", rendered)
        self.assertIn("QWEN38_STEERING_VECTOR: /steering/bundle.safetensors", rendered)
        self.assertIn("QWEN38_STEERING_CONTROL_FILE: /steering/control.json", rendered)
        self.assertNotIn("QWEN38_STEERING_SCALE", rendered)

    def test_requires_exact_engine_marker(self):
        args = argparse.Namespace(
            mode="capture",
            image="qwen38-steering:0.1.0",
            capture_dir=compose.pathlib.Path("/tmp/captures"),
            vector=None,
            control_file=None,
            scale=1.0,
            layers="all",
        )
        with self.assertRaisesRegex(ValueError, "unexpected"):
            compose.render("services: {}\n", args)


if __name__ == "__main__":
    unittest.main()


ISO_SOURCE = SOURCE + """
networks:
  machineview-host:
    external: true
"""


class ComposeIsolateTests(unittest.TestCase):
    def test_isolate_moves_experiment_engine_onto_its_own_network(self):
        args = argparse.Namespace(
            mode="steer",
            image="qwen38-steering:0.1.0",
            capture_dir=None,
            vector=compose.pathlib.Path("/private/vector.safetensors"),
            control_file=None,
            scale=0.5,
            layers="20-23",
            isolate=True,
        )
        rendered = compose.render(ISO_SOURCE, args)
        engine = rendered[rendered.index("  qwen38flashnext-b:") :]
        engine = engine[: engine.index("  qwen38flashnext-tp8:")]
        self.assertIn("    networks:\n      - steer_isolated", engine)
        self.assertIn("networks:\n  steer_isolated: {}", rendered)
        self.assertIn("  machineview-host:", rendered)

    def test_default_render_untouched_by_isolate_support(self):
        args = argparse.Namespace(
            mode="steer",
            image="qwen38-steering:0.1.0",
            capture_dir=None,
            vector=compose.pathlib.Path("/private/vector.safetensors"),
            control_file=None,
            scale=0.5,
            layers="20-23",
        )
        rendered = compose.render(SOURCE, args)
        self.assertNotIn("steer_isolated", rendered)
