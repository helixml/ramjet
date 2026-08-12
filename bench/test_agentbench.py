import json
import pathlib
import unittest

from agentbench import (
    Assembly,
    CorpusError,
    DEFAULT_CORPUS,
    SSEDecoder,
    load_cases,
    validate_case,
    validate_result,
)


def case(expected=None):
    return {
        "schema_version": 1,
        "id": "fixture",
        "request": {"stream": True, "messages": [{"role": "user", "content": "x"}]},
        "expected": {"mode": "tool_calls", **(expected or {})},
    }


def sse(event):
    return ("data: " + json.dumps(event, ensure_ascii=False) + "\n\n").encode()


class AgentBenchTest(unittest.TestCase):
    def test_committed_corpus_is_valid_and_versioned(self):
        cases = load_cases(DEFAULT_CORPUS)
        self.assertGreaterEqual(len(cases), 5)
        self.assertEqual(len({item["id"] for item in cases}), len(cases))
        self.assertEqual({item["schema_version"] for item in cases}, {1})

    def test_stream_reassembles_split_tool_name_and_typed_arguments(self):
        assembly = Assembly()
        decoder = SSEDecoder(assembly)
        payload = b"".join(
            [
                sse(
                    {
                        "choices": [
                            {
                                "delta": {
                                    "tool_calls": [
                                        {
                                            "index": 0,
                                            "id": "call_1",
                                            "type": "function",
                                            "function": {"name": "record_", "arguments": '{"host":"node'},
                                        }
                                    ]
                                }
                            }
                        ]
                    }
                ),
                sse(
                    {
                        "choices": [
                            {
                                "delta": {
                                    "tool_calls": [
                                        {
                                            "index": 0,
                                            "function": {
                                                "name": "probe",
                                                "arguments": (
                                                    '06","healthy":true,"load":0.75,'
                                                    '"note":null,"labels":["gpu"],'
                                                    '"metadata":{"rack":6}}'
                                                ),
                                            },
                                        }
                                    ]
                                },
                                "finish_reason": "tool_calls",
                            }
                        ],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 20},
                    }
                ),
                b"data: [DONE]\n\n",
            ]
        )
        for offset in range(0, len(payload), 7):
            decoder.feed(payload[offset : offset + 7], observed_at=1.0 + offset)
        decoder.finish()
        result = assembly.result()
        errors = validate_result(
            case(
                {
                    "tool_names": ["record_probe"],
                    "argument_types": {
                        "host": "string",
                        "healthy": "boolean",
                        "load": "number",
                        "note": "null",
                        "labels": "array",
                        "metadata": "object",
                        "metadata.rack": "number",
                    },
                }
            ),
            result,
        )
        self.assertEqual(errors, [])
        self.assertEqual(result["tool_calls"][0]["name"], "record_probe")
        self.assertEqual(result["usage"]["completion_tokens"], 20)

    def test_split_dsml_marker_is_detected_after_content_reassembly(self):
        assembly = Assembly()
        assembly.feed({"choices": [{"delta": {"content": "prefix <｜DS"}}]})
        assembly.feed({"choices": [{"delta": {"content": "ML｜tool_calls>"}}]})
        errors = validate_result(case({"mode": "either"}), assembly.result())
        self.assertIn("DSML marker leaked into content", errors)

    def test_wrong_typed_argument_is_rejected(self):
        assembly = Assembly()
        assembly.feed(
            {
                "choices": [
                    {
                        "message": {
                            "tool_calls": [
                                {
                                    "type": "function",
                                    "function": {
                                        "name": "record_probe",
                                        "arguments": '{"healthy":"true"}',
                                    },
                                }
                            ]
                        }
                    }
                ]
            }
        )
        errors = validate_result(
            case({"tool_names": ["record_probe"], "argument_types": {"healthy": "boolean"}}),
            assembly.result(),
        )
        self.assertIn("argument healthy is not boolean", errors)

    def test_parallel_calls_require_unique_engine_arguments(self):
        assembly = Assembly()
        assembly.feed(
            {
                "choices": [
                    {
                        "message": {
                            "tool_calls": [
                                {
                                    "type": "function",
                                    "function": {
                                        "name": "read_metric",
                                        "arguments": '{"engine":"A"}',
                                    },
                                },
                                {
                                    "type": "function",
                                    "function": {
                                        "name": "read_metric",
                                        "arguments": '{"engine":"A"}',
                                    },
                                },
                            ]
                        }
                    }
                ]
            }
        )
        errors = validate_result(
            case(
                {
                    "min_tool_calls": 2,
                    "tool_names": ["read_metric"],
                    "unique_arguments": ["engine"],
                }
            ),
            assembly.result(),
        )
        self.assertIn("argument engine is not unique across calls", errors)

    def test_input_alias_and_request_state_reset_are_supported(self):
        first = Assembly()
        first.feed(
            {
                "choices": [
                    {
                        "message": {
                            "reasoning_content": "private reasoning",
                            "tool_calls": [
                                {
                                    "type": "function",
                                    "function": {"name": "status", "input": {"node": "node06"}},
                                }
                            ],
                        }
                    }
                ]
            }
        )
        errors = validate_result(
            case({"tool_names": ["status"], "argument_types": {"node": "string"}}),
            first.result(),
        )
        self.assertEqual(errors, [])
        second = Assembly().result()
        self.assertEqual(second["reasoning_content"], "")
        self.assertEqual(second["tool_calls"], [])

    def test_reasoning_tool_history_requires_reasoning_and_matching_result(self):
        broken = case({"mode": "text", "reasoning_history": True})
        broken["request"]["messages"] = [
            {"role": "user", "content": "status"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "status", "arguments": "{}"},
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "healthy"},
        ]
        with self.assertRaisesRegex(CorpusError, "reasoning_content"):
            validate_case(broken)

        broken["request"]["messages"][1]["reasoning_content"] = "Use the tool."
        broken["request"]["messages"][2]["tool_call_id"] = "wrong_call"
        with self.assertRaisesRegex(CorpusError, "matching results"):
            validate_case(broken)

    def test_metadata_file_contract_is_documented_next_to_corpus(self):
        readme = pathlib.Path(DEFAULT_CORPUS).with_name("README.md").read_text(encoding="utf-8")
        for field in (
            "engine_image",
            "model_revision",
            "tokenizer_sha256",
            "config_sha256",
            "router_version",
            "gpu_count",
        ):
            self.assertIn(field, readme)


if __name__ == "__main__":
    unittest.main()
