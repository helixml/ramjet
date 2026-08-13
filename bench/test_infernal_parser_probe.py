import pathlib
import shutil
import tempfile
import types
import unittest

from infernal_parser_probe import (
    DEFAULT_CASES,
    OUTCOME_FIELDS,
    PROFILES,
    SOURCE_FILES,
    ProbeError,
    compare,
    _client_argument_shape,
    load_cases,
    source_identity,
)


class InfernalParserProbeTest(unittest.TestCase):
    def test_committed_cases_are_valid_and_cover_each_profile(self):
        cases = load_cases(DEFAULT_CASES)
        self.assertGreaterEqual(len(cases), 12)
        self.assertEqual(len({case["id"] for case in cases}), len(cases))
        for case in cases:
            self.assertEqual(case["schema_version"], 2)
            self.assertEqual(set(case["expected"]), PROFILES)
            for expected in case["expected"].values():
                self.assertLessEqual(set(OUTCOME_FIELDS), set(expected))

    def test_cases_cover_recovery_safety_and_known_adjacent_gap(self):
        by_id = {case["id"]: case for case in load_cases(DEFAULT_CASES)}
        orphan = by_id["orphan-parallel"]["expected"]
        self.assertEqual(orphan["r4"]["tool_call_starts"], 0)
        self.assertEqual(orphan["pr49117"]["tool_call_starts"], 2)
        malformed = by_id["malformed-toolcalls-wrapper-parallel"]["expected"]
        self.assertTrue(malformed["pr49117"]["dsml_content"])
        self.assertFalse(malformed["complete"]["dsml_content"])
        malformed_lf = by_id["malformed-toolcalls-wrapper-lf-parallel"]["expected"]
        self.assertTrue(malformed_lf["pr49117"]["dsml_content"])
        self.assertFalse(malformed_lf["complete"]["dsml_content"])
        undeclared = by_id["undeclared-orphan-is-content"]["expected"]
        self.assertEqual(undeclared["complete"]["tool_call_starts"], 0)
        self.assertTrue(undeclared["complete"]["dsml_content"])

    def test_cases_define_mixed_recovery_and_eof_fail_closed_contract(self):
        by_id = {case["id"]: case for case in load_cases(DEFAULT_CASES)}

        truncated = by_id["wrapped-then-orphan-eof-mid-argument"]["expected"]
        self.assertTrue(truncated["pr49117"]["open_at_eof"])
        self.assertFalse(truncated["pr49117"]["args_json_valid"])
        self.assertEqual(truncated["complete"]["tool_call_starts"], 1)
        self.assertEqual(truncated["complete"]["tool_call_ends"], 1)
        self.assertFalse(truncated["complete"]["open_at_eof"])

        duplicate = by_id["wrapped-two-then-orphan-duplicate-third"]["expected"]
        self.assertEqual(duplicate["pr49117"]["tool_call_starts"], 3)
        self.assertEqual(duplicate["pr49117"]["tool_call_ends"], 4)
        self.assertTrue(duplicate["pr49117"]["duplicate_canonical_args"])
        self.assertEqual(duplicate["complete"]["tool_call_starts"], 2)
        self.assertFalse(duplicate["complete"]["duplicate_canonical_args"])

        for case_id in ("wrapped-eof-mid-name", "wrapped-eof-mid-argument"):
            expected = by_id[case_id]["expected"]
            self.assertTrue(expected["complete"]["open_at_eof"])
            self.assertEqual(expected["complete"]["tool_call_ends"], 0)

    def test_client_argument_shape_reports_validity_and_duplicates_only(self):
        def event(kind, index, value=""):
            return types.SimpleNamespace(
                type=types.SimpleNamespace(name=kind),
                tool_index=index,
                value=value,
            )

        parser_engine = types.SimpleNamespace(
            ParserEngine=types.SimpleNamespace(
                _safe_arg_prefix=staticmethod(lambda value, _keys: value)
            )
        )
        config = types.SimpleNamespace(
            arg_converter=lambda raw, _partial: raw,
            arg_structural_chars=None,
        )
        duplicate_batches = [
            [
                event("TOOL_CALL_START", 0),
                event("TOOL_NAME", 0, "fixture"),
                event("ARG_VALUE_CHUNK", 0, '{"k":1}'),
                event("TOOL_CALL_END", 0),
                event("TOOL_CALL_START", 1),
                event("TOOL_NAME", 1, "fixture"),
                event("ARG_VALUE_CHUNK", 1, '{"k":1}'),
                event("TOOL_CALL_END", 1),
            ]
        ]
        self.assertEqual(
            _client_argument_shape(parser_engine, config, duplicate_batches, set()),
            (True, True),
        )

        invalid_batches = [
            [
                event("TOOL_CALL_START", 0),
                event("TOOL_NAME", 0, "fixture"),
                event("ARG_VALUE_CHUNK", 0, '{"k":'),
            ],
            [event("ARG_VALUE_CHUNK", 0, '"v"'), event("TOOL_CALL_END", 0)],
        ]
        self.assertEqual(
            _client_argument_shape(parser_engine, config, invalid_batches, set()),
            (False, False),
        )

    def test_compare_reports_only_contract_fields(self):
        outcome = {
            "tool_call_starts": 2,
            "tool_call_ends": 2,
            "open_at_eof": False,
            "args_json_valid": True,
            "duplicate_canonical_args": False,
            "dsml_content": True,
            "content": "bad",
        }
        errors = compare(
            outcome,
            {
                "tool_call_starts": 2,
                "tool_call_ends": 2,
                "open_at_eof": False,
                "args_json_valid": True,
                "duplicate_canonical_args": False,
                "dsml_content": False,
                "content": "",
            },
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
                '{"schema_version":2,"id":"bad","chunks":["x"],"expected":{"r4":{}}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ProbeError, "expected profiles"):
                load_cases(path)


if __name__ == "__main__":
    unittest.main()
