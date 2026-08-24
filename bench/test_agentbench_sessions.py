#!/usr/bin/env python3
"""Tests for agentbench schema v2: long-context needles and multi-turn sessions."""

import json
import pathlib
import unittest

import agentbench


V2_CORPUS = pathlib.Path(__file__).with_name("agent_cases") / "v2_sessions.jsonl"


def minimal_v2(**overrides):
    case = {
        "schema_version": 2,
        "id": "case",
        "request": {"stream": False, "messages": [{"role": "user", "content": "hi"}]},
        "expected": {"mode": "text"},
    }
    case.update(overrides)
    return case


class SchemaTests(unittest.TestCase):
    def test_committed_v2_corpus_is_valid(self):
        cases = agentbench.load_cases(V2_CORPUS)
        self.assertEqual(len(cases), 4)

    def test_v1_corpus_still_loads(self):
        cases = agentbench.load_cases(agentbench.DEFAULT_CORPUS)
        self.assertTrue(all(case["schema_version"] == 1 for case in cases))

    def test_schema_version_three_is_rejected(self):
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(minimal_v2(schema_version=3))

    def test_context_requires_schema_two(self):
        case = minimal_v2(schema_version=1)
        case["context"] = {"filler_kib": 1, "needles": [{"depth": 0.5, "key": "A", "value": "1"}]}
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(case)

    def test_turns_require_schema_two(self):
        case = minimal_v2(schema_version=1)
        case["turns"] = [{"expected": {"mode": "text"}}]
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(case)

    def test_needle_depth_outside_unit_interval_is_rejected(self):
        case = minimal_v2()
        case["context"] = {"filler_kib": 1, "needles": [{"depth": 1.5, "key": "A", "value": "1"}]}
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(case)

    def test_duplicate_needle_keys_are_rejected(self):
        case = minimal_v2()
        case["context"] = {
            "filler_kib": 1,
            "needles": [
                {"depth": 0.1, "key": "A", "value": "1"},
                {"depth": 0.9, "key": "A", "value": "2"},
            ],
        }
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(case)

    def test_probe_keys_must_name_known_needles(self):
        case = minimal_v2()
        case["context"] = {
            "filler_kib": 1,
            "needles": [{"depth": 0.1, "key": "A", "value": "1"}],
            "probe_keys": ["B"],
        }
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(case)

    def test_tool_results_must_be_valid_json(self):
        # A malformed payload would otherwise reach the model mid-session and
        # surface as an inexplicable model failure rather than a corpus error.
        case = minimal_v2()
        case["turns"] = [{"expected": {"mode": "text"}, "tool_results": {"t": "{not json"}}]
        with self.assertRaises(agentbench.CorpusError):
            agentbench.validate_case(case)

    def test_turn_request_may_not_override_conversation_identity(self):
        for reserved in ("model", "messages", "stream"):
            case = minimal_v2()
            case["turns"] = [{"expected": {"mode": "text"}, "request": {reserved: "x"}}]
            with self.assertRaises(agentbench.CorpusError):
                agentbench.validate_case(case)


class ContextBuildTests(unittest.TestCase):
    def setUp(self):
        self.case = minimal_v2()
        self.case["context"] = {
            "filler_kib": 4,
            "needles": [
                {"depth": 0.0, "key": "FIRST", "value": "11111"},
                {"depth": 0.5, "key": "MID", "value": "22222"},
                {"depth": 1.0, "key": "LAST", "value": "33333"},
            ],
        }

    def build(self, salt="salt"):
        return agentbench.build_context(self.case, salt)

    def test_document_reaches_the_requested_size(self):
        built = self.build()
        content = built["request"]["messages"][0]["content"]
        self.assertGreaterEqual(len(content), 4 * 1024)

    def test_every_needle_is_planted(self):
        content = self.build()["request"]["messages"][0]["content"]
        for value in ("11111", "22222", "33333"):
            self.assertIn(value, content)

    def test_needles_are_planted_in_depth_order(self):
        # A depth ordering bug would place the deep needle early and quietly
        # turn a long-range recall test into a short-range one.
        content = self.build()["request"]["messages"][0]["content"]
        self.assertLess(content.index("11111"), content.index("22222"))
        self.assertLess(content.index("22222"), content.index("33333"))

    def test_required_fragments_are_derived_from_needles(self):
        built = self.build()
        self.assertEqual(
            built["expected"]["content_contains_all"], ["11111", "22222", "33333"]
        )

    def test_probe_keys_narrow_the_required_fragments(self):
        self.case["context"]["probe_keys"] = ["MID"]
        self.assertEqual(self.build()["expected"]["content_contains_all"], ["22222"])

    def test_filler_is_salt_namespaced(self):
        # Two salts must not share a prompt prefix, or the second run is served
        # from the first run's radix cache and measures nothing.
        first = self.build("salt-a")["request"]["messages"][0]["content"]
        second = self.build("salt-b")["request"]["messages"][0]["content"]
        self.assertNotEqual(first[:200], second[:200])

    def test_original_question_is_preserved_after_the_document(self):
        content = self.build()["request"]["messages"][0]["content"]
        self.assertTrue(content.endswith("hi"))

    def test_case_without_context_is_returned_unchanged(self):
        plain = minimal_v2()
        self.assertIs(agentbench.build_context(plain, "salt"), plain)


class ContextWithSessionTests(unittest.TestCase):
    """A session carrying a context asks for recall in its final turn."""

    def case(self):
        case = minimal_v2()
        case["context"] = {
            "filler_kib": 1,
            "needles": [{"depth": 0.5, "key": "A", "value": "12345"}],
        }
        case["turns"] = [
            {"expected": {"mode": "tool_calls"}},
            {"expected": {"mode": "text", "content_contains_all": ["999"]}},
        ]
        return case

    def test_fragments_attach_to_the_final_turn(self):
        built = agentbench.build_context(self.case(), "salt")
        self.assertEqual(
            built["turns"][-1]["expected"]["content_contains_all"], ["12345", "999"]
        )

    def test_earlier_expectations_are_untouched(self):
        # Turn 0 is a tool call with no content; requiring the facts there
        # would fail a correct session.
        built = agentbench.build_context(self.case(), "salt")
        self.assertNotIn("content_contains_all", built["expected"])
        self.assertNotIn("content_contains_all", built["turns"][0]["expected"])


class ToolReplayTests(unittest.TestCase):
    def result(self, calls, content=""):
        return {"content": content, "tool_calls": calls}

    def test_assistant_turn_replays_calls_and_results_quote_the_call_id(self):
        result = self.result(
            [{"id": "call_7", "type": "function", "name": "read_metric", "arguments": "{}"}]
        )
        messages = agentbench.tool_result_messages(result, {"read_metric": '{"value":1}'})
        self.assertEqual(messages[0]["role"], "assistant")
        self.assertEqual(messages[0]["tool_calls"][0]["id"], "call_7")
        self.assertEqual(messages[1]["role"], "tool")
        self.assertEqual(messages[1]["tool_call_id"], "call_7")
        self.assertEqual(json.loads(messages[1]["content"]), {"value": 1})

    def test_each_call_gets_its_own_result_message(self):
        result = self.result(
            [
                {"id": "a", "type": "function", "name": "read_metric", "arguments": "{}"},
                {"id": "b", "type": "function", "name": "read_metric", "arguments": "{}"},
            ]
        )
        messages = agentbench.tool_result_messages(result, {"read_metric": "{}"})
        self.assertEqual([m["role"] for m in messages], ["assistant", "tool", "tool"])
        self.assertEqual([m["tool_call_id"] for m in messages[1:]], ["a", "b"])

    def test_wildcard_payload_covers_any_tool_name(self):
        result = self.result([{"id": "a", "type": "function", "name": "other", "arguments": "{}"}])
        messages = agentbench.tool_result_messages(result, {"*": '{"ok":true}'})
        self.assertEqual(json.loads(messages[1]["content"]), {"ok": True})

    def test_unconfigured_tool_yields_a_structured_error_payload(self):
        result = self.result([{"id": "a", "type": "function", "name": "other", "arguments": "{}"}])
        messages = agentbench.tool_result_messages(result, {})
        self.assertIn("error", json.loads(messages[1]["content"]))

    def test_prose_only_turn_still_produces_a_valid_assistant_message(self):
        messages = agentbench.tool_result_messages(self.result([], "plain answer"), {})
        self.assertEqual(messages, [{"role": "assistant", "content": "plain answer"}])


class DigitGroupingTests(unittest.TestCase):
    """A model formatting 27604 as "27,604" is right, and must score as right."""

    def test_comma_grouping_matches(self):
        self.assertTrue(agentbench.content_contains("Engine A: 27,604", "27604"))

    def test_underscore_grouping_matches(self):
        self.assertTrue(agentbench.content_contains("value 27_604", "27604"))

    def test_non_breaking_and_thin_spaces_match(self):
        for separator in ("\u00a0", "\u202f", "\u2009"):
            self.assertTrue(
                agentbench.content_contains(f"value 83{separator}521", "83521"),
                separator.encode("unicode_escape"),
            )

    def test_ordinary_space_is_not_a_grouping_separator(self):
        # Stripping plain spaces would let "8 3 5 2 1" satisfy "83521" and make
        # the matcher meaningless.
        self.assertFalse(agentbench.content_contains("value 83 521", "83521"))

    def test_letters_are_never_stripped(self):
        self.assertFalse(agentbench.content_contains("8a5", "885"))

    def test_prose_is_not_loosened(self):
        self.assertFalse(agentbench.content_contains("a,b", "ab"))

    def test_a_different_number_still_fails(self):
        self.assertFalse(agentbench.content_contains("Engine A: 27,605", "27604"))

    def test_grouped_expectation_matches_ungrouped_content(self):
        self.assertTrue(agentbench.content_contains("27604", "27,604"))


class ContentContainsAllTests(unittest.TestCase):
    def validate(self, fragments, content):
        case = {"expected": {"mode": "text", "content_contains_all": fragments}}
        result = {
            "content": content,
            "reasoning_content": "",
            "tool_calls": [],
            "finish_reason": "stop",
        }
        return agentbench.validate_result(case, result)

    def test_all_fragments_present_passes(self):
        self.assertEqual(self.validate(["11111", "22222"], "a 11111 b 22222"), [])

    def test_missing_fragment_is_reported_by_position_not_value(self):
        errors = self.validate(["11111", "22222"], "only 11111 here")
        self.assertEqual(errors, ["required content fragment 1 is missing"])
        self.assertNotIn("22222", " ".join(errors))

    def test_matching_is_case_insensitive(self):
        self.assertEqual(self.validate(["Alpha"], "aLPHA"), [])


if __name__ == "__main__":
    unittest.main()
