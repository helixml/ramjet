import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PATCH_PATH = ROOT / "deploy/glm53_flash_sm120/patch-glm47-nullable.py"
SPEC = importlib.util.spec_from_file_location("patch_glm47_nullable", PATCH_PATH)
assert SPEC and SPEC.loader
PATCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PATCH)


class Glm53NullableParserPatchTests(unittest.TestCase):
    def test_nullable_string_is_buffered_until_the_complete_value_is_known(self):
        source = '''
        arg_type = get_argument_type(func_name, key, tools)
        if arg_type:
            return arg_type

        if value_type == "string":
            # Ensure proper JSON string formatting with quotes
            return json.dumps(value, ensure_ascii=False)

                        if value_type == "string":
                            if not self._value_started:
                                pass
'''
        patched = PATCH.patch_source(source)
        self.assertIn('return "nullable_string"', patched)
        self.assertIn('value.strip() == "null"', patched)
        self.assertIn('if value_type == "nullable_string":', patched)
        self.assertIn('self._current_value += content', patched)

    def test_patch_fails_closed_when_the_pinned_parser_shape_changes(self):
        with self.assertRaises(SystemExit):
            PATCH.patch_source("unexpected upstream source")


if __name__ == "__main__":
    unittest.main()
