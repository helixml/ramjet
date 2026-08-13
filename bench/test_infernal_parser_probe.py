import pathlib
import shutil
import tempfile
import unittest

from infernal_parser_probe import (
    DEFAULT_CASES,
    PROFILES,
    SOURCE_FILES,
    ProbeError,
    compare,
    load_cases,
    source_identity,
)


class InfernalParserProbeTest(unittest.TestCase):
    def test_committed_cases_are_valid_and_cover_each_profile(self):
        cases = load_cases(DEFAULT_CASES)
        self.assertGreaterEqual(len(cases), 7)
        self.assertEqual(len({case["id"] for case in cases}), len(cases))
        for case in cases:
            self.assertEqual(set(case["expected"]), PROFILES)

    def test_cases_cover_recovery_safety_and_known_adjacent_gap(self):
        by_id = {case["id"]: case for case in load_cases(DEFAULT_CASES)}
        orphan = by_id["orphan-parallel"]["expected"]
        self.assertEqual(orphan["r4"]["tool_calls"], 0)
        self.assertEqual(orphan["pr49117"]["tool_calls"], 2)
        malformed = by_id["malformed-toolcalls-wrapper-parallel"]["expected"]
        self.assertTrue(malformed["pr49117"]["dsml_content"])
        self.assertFalse(malformed["complete"]["dsml_content"])
        malformed_lf = by_id["malformed-toolcalls-wrapper-lf-parallel"]["expected"]
        self.assertTrue(malformed_lf["pr49117"]["dsml_content"])
        self.assertFalse(malformed_lf["complete"]["dsml_content"])
        undeclared = by_id["undeclared-orphan-is-content"]["expected"]
        self.assertEqual(undeclared["complete"]["tool_calls"], 0)
        self.assertTrue(undeclared["complete"]["dsml_content"])

    def test_compare_reports_only_contract_fields(self):
        outcome = {"tool_calls": 2, "dsml_content": True, "content": "bad"}
        errors = compare(
            outcome, {"tool_calls": 2, "dsml_content": False, "content": ""}
        )
        self.assertEqual(len(errors), 2)
        self.assertTrue(any(error.startswith("dsml_content:") for error in errors))
        self.assertIn("content mismatch", errors)

    def test_source_identity_fails_closed_on_missing_parser_surface(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ProbeError, "missing"):
                source_identity(pathlib.Path(directory))

    def test_source_identity_covers_shared_parser_and_tool_helper(self):
        source_root = pathlib.Path(__file__).parents[1]
        with tempfile.TemporaryDirectory() as directory:
            copied = pathlib.Path(directory)
            for relative in SOURCE_FILES:
                target = copied / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                source = source_root / relative
                if source.is_file():
                    shutil.copyfile(source, target)
                else:
                    target.write_text("# fixture\n", encoding="utf-8")
            before = source_identity(copied)
            for relative in (
                "vllm/parser/engine/parser_engine.py",
                "vllm/tool_parsers/utils.py",
            ):
                target = copied / relative
                target.write_text(target.read_text() + "# changed\n")
                after = source_identity(copied)
                self.assertNotEqual(before, after)
                before = after

    def test_invalid_fixture_profile_set_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "bad.jsonl"
            path.write_text(
                '{"id":"bad","chunks":["x"],"expected":{"r4":{}}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ProbeError, "expected profiles"):
                load_cases(path)


if __name__ == "__main__":
    unittest.main()
