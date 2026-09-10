import importlib.util
import json
import pathlib
import tempfile
import unittest


MODULE = (
    pathlib.Path(__file__).with_name("steering_plugin")
    / "src"
    / "qwen38_steering"
    / "__init__.py"
)
SPEC = importlib.util.spec_from_file_location("qwen38_steering", MODULE)
PLUGIN = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(PLUGIN)


class LayerParserTests(unittest.TestCase):
    def test_all_is_default(self):
        self.assertEqual(PLUGIN.parse_layers(None, 3), frozenset({0, 1, 2}))
        self.assertEqual(PLUGIN.parse_layers("all", 2), frozenset({0, 1}))

    def test_ranges_and_singletons(self):
        self.assertEqual(
            PLUGIN.parse_layers("0, 2-4,7", 8),
            frozenset({0, 2, 3, 4, 7}),
        )

    def test_rejects_invalid_selectors(self):
        for value in ("1,,2", "4-2", "-1", "3-", "8", "x"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                PLUGIN.parse_layers(value, 8)


class PathPolicyTests(unittest.TestCase):
    def test_capture_directory_requires_mode_0700(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory)
            path.chmod(0o700)
            self.assertEqual(PLUGIN._private_directory(str(path)), path)
            path.chmod(0o755)
            with self.assertRaisesRegex(RuntimeError, "mode 0700"):
                PLUGIN._private_directory(str(path))

    def test_dynamic_control_requires_private_regular_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "control.json"
            path.write_text(
                json.dumps(
                    {
                        "generation": 3,
                        "direction_index": 2,
                        "scale": 0.75,
                        "layers": "12-19,24",
                    }
                )
            )
            path.chmod(0o600)
            scale, layers, index, identity = PLUGIN._read_control(path, 48, 4)
            self.assertEqual(scale, 0.75)
            self.assertEqual(layers, frozenset(set(range(12, 20)) | {24}))
            self.assertEqual(index, 2)
            self.assertEqual(len(identity), 4)
            path.chmod(0o644)
            with self.assertRaisesRegex(RuntimeError, "mode 0600"):
                PLUGIN._read_control(path, 48, 4)

    def test_dynamic_control_rejects_unknown_direction(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "control.json"
            path.write_text(
                json.dumps(
                    {"direction_index": 4, "scale": 1, "layers": "all"}
                )
            )
            path.chmod(0o600)
            with self.assertRaisesRegex(RuntimeError, "outside the bundle"):
                PLUGIN._read_control(path, 48, 4)


if __name__ == "__main__":
    unittest.main()


class SteerAdminTests(unittest.TestCase):
    BASE = {
        "direction_index": 1,
        "generation": 4,
        "layers": "20-23",
        "scale": 0.5,
    }

    def test_scale_patch_merges_and_bumps_generation(self):
        merged = PLUGIN.apply_steer_patch(self.BASE, {"scale": 1.25}, 48, 4)
        self.assertEqual(merged["scale"], 1.25)
        self.assertEqual(merged["direction_index"], 1)
        self.assertEqual(merged["generation"], 5)

    def test_out_of_range_scale_rejected(self):
        for bad in (8.5, -8.5, float("nan"), "x"):
            with self.assertRaises(ValueError):
                PLUGIN.apply_steer_patch(self.BASE, {"scale": bad}, 48, 4)

    def test_unknown_field_and_bad_layers_rejected(self):
        with self.assertRaises(ValueError):
            PLUGIN.apply_steer_patch(self.BASE, {"mode": "turbo"}, 48, 4)
        with self.assertRaises(ValueError):
            PLUGIN.apply_steer_patch(self.BASE, {"layers": "44-99"}, 48, 4)
        with self.assertRaises(ValueError):
            PLUGIN.apply_steer_patch(self.BASE, {"direction_index": 9}, 48, 4)

    def test_atomic_write_roundtrips_through_reader(self):
        with tempfile.TemporaryDirectory() as raw:
            path = pathlib.Path(raw) / "control.json"
            merged = PLUGIN.apply_steer_patch(self.BASE, {"scale": 0.0}, 48, 4)
            PLUGIN.write_control_atomic(path, merged)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            scale, layers, index, _ = PLUGIN._read_control(path, 48, 4)
            self.assertEqual(scale, 0.0)
            self.assertEqual(index, 1)
            self.assertEqual(layers, frozenset({20, 21, 22, 23}))
