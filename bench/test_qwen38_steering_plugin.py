import importlib.util
import json
import pathlib
import tempfile
import unittest


MODULE = (
    pathlib.Path(__file__).with_name("qwen38_steering_plugin")
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
