import contextlib
import io
import json
import os
import pathlib
import tempfile
import types
import unittest
from unittest import mock

from agent_trace import (
    TraceShapeError,
    _execute_shape,
    build_case,
    command_run,
    load_trace_shapes,
    parse_shape,
    summarize_shapes,
    synthetic_prefix,
)


def shape(**updates):
    value = {
        "schema_version": 1,
        "arrival_offset_ms": 0,
        "prefix_group": 0,
        "shared_prefix_tokens": 1024,
        "prompt_tokens": 2048,
        "history_turns": 2,
        "history_tool_rounds": 1,
        "history_parallel_calls": 2,
        "protocol": "parallel_tool",
        "stream": True,
        "expected_tool_calls": 2,
        "max_output_tokens": 256,
        "observed_completion_tokens": 192,
        "sampling": {
            "temperature": 1.0,
            "top_p": 0.95,
            "seed": 7,
            "reasoning_effort": "high",
        },
    }
    value.update(updates)
    return value


class PrivateTrace:
    def __init__(self, records):
        self.directory = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.directory.name)
        os.chmod(self.root, 0o700)
        self.path = self.root / "trace.jsonl"
        self.path.write_text(
            "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
        )
        os.chmod(self.path, 0o600)

    def close(self):
        self.directory.cleanup()


class AgentTraceTest(unittest.TestCase):
    def test_valid_shape_preserves_only_bounded_shape_fields(self):
        parsed = parse_shape(shape(), 1)
        self.assertEqual(parsed.prompt_tokens, 2048)
        self.assertEqual(parsed.expected_tool_calls, 2)
        self.assertEqual(parsed.sampling.reasoning_effort, "high")

    def test_unknown_content_or_identifier_field_is_rejected(self):
        for field in ("prompt", "request_id", "tool_payload", "credential"):
            with self.subTest(field=field):
                value = shape()
                value[field] = "must never be accepted"
                with self.assertRaisesRegex(TraceShapeError, "fields do not match"):
                    parse_shape(value, 1)

    def test_duplicate_fields_and_boolean_schema_version_are_rejected(self):
        value = shape(schema_version=True)
        with self.assertRaisesRegex(TraceShapeError, "schema_version"):
            parse_shape(value, 1)

        private = PrivateTrace([shape()])
        try:
            encoded = json.dumps(shape())
            raw = encoded[:-1] + ',"prompt_tokens":2048}\n'
            private.path.write_text(raw, encoding="utf-8")
            os.chmod(private.path, 0o600)
            with self.assertRaisesRegex(TraceShapeError, "duplicate"):
                load_trace_shapes(private.path)
        finally:
            private.close()

    def test_nested_sampling_fields_are_exact_and_finite(self):
        for update in (
            {"temperature": float("nan")},
            {"top_p": 0},
            {"reasoning_effort": "custom"},
            {"source_model": "private"},
        ):
            with self.subTest(update=update):
                value = shape()
                value["sampling"] = {**value["sampling"], **update}
                with self.assertRaises(TraceShapeError):
                    parse_shape(value, 1)

    def test_protocol_and_tool_shape_constraints_fail_closed(self):
        invalid = (
            {"protocol": "text", "expected_tool_calls": 1},
            {"protocol": "required_tool", "expected_tool_calls": 0},
            {"protocol": "parallel_tool", "expected_tool_calls": 1},
            {"history_tool_rounds": 0, "history_parallel_calls": 1},
            {"history_turns": 0, "history_tool_rounds": 1},
            {"shared_prefix_tokens": 2049},
        )
        for update in invalid:
            with self.subTest(update=update), self.assertRaises(TraceShapeError):
                parse_shape(shape(**update), 1)

    def test_private_loader_requires_bucketed_ordered_dense_trace(self):
        records = [
            shape(prefix_group=0),
            shape(arrival_offset_ms=100, prefix_group=1),
            shape(arrival_offset_ms=200, prefix_group=0),
        ]
        private = PrivateTrace(records)
        try:
            loaded = load_trace_shapes(private.path)
            self.assertEqual(len(loaded), 3)
        finally:
            private.close()

        for changed in (
            [shape(arrival_offset_ms=100)],
            [shape(), shape(arrival_offset_ms=50)],
            [shape(), shape(arrival_offset_ms=100, prefix_group=2)],
            [shape(), shape(arrival_offset_ms=200), shape(arrival_offset_ms=100)],
        ):
            with self.subTest(changed=changed):
                private = PrivateTrace(changed)
                try:
                    with self.assertRaises(TraceShapeError):
                        load_trace_shapes(private.path)
                finally:
                    private.close()

    def test_private_loader_rejects_unsafe_file_and_parent_modes(self):
        private = PrivateTrace([shape()])
        try:
            os.chmod(private.path, 0o640)
            with self.assertRaisesRegex(TraceShapeError, "envelope"):
                load_trace_shapes(private.path)
            os.chmod(private.path, 0o600)
            os.chmod(private.root, 0o750)
            with self.assertRaisesRegex(TraceShapeError, "parent"):
                load_trace_shapes(private.path)
        finally:
            private.close()

    def test_private_loader_bounds_aggregate_gpu_work(self):
        private = PrivateTrace(
            [
                shape(prompt_tokens=300_000, shared_prefix_tokens=0)
                for _ in range(54)
            ]
        )
        try:
            with self.assertRaisesRegex(TraceShapeError, "prompt-token budget"):
                load_trace_shapes(private.path)
        finally:
            private.close()

    def test_private_loader_rejects_symlink_and_hardlink(self):
        private = PrivateTrace([shape()])
        try:
            symlink = private.root / "link.jsonl"
            symlink.symlink_to(private.path)
            with self.assertRaises(TraceShapeError):
                load_trace_shapes(symlink)
            hardlink = private.root / "hard.jsonl"
            os.link(private.path, hardlink)
            with self.assertRaisesRegex(TraceShapeError, "envelope"):
                load_trace_shapes(private.path)
        finally:
            private.close()

    def test_prefix_is_nested_per_group_and_salt_is_not_retained(self):
        short = synthetic_prefix("raw-private-salt", 0, 8)
        long = synthetic_prefix("raw-private-salt", 0, 16)
        other = synthetic_prefix("raw-private-salt", 1, 16)
        self.assertTrue(long.startswith(short))
        self.assertNotEqual(long, other)
        self.assertNotIn("raw-private-salt", long)

    def test_case_builder_reconstructs_history_and_parallel_shape(self):
        parsed = parse_shape(shape(), 1)
        case = build_case(parsed, 3, "fresh")
        messages = case["request"]["messages"]
        assistants = [message for message in messages if message["role"] == "assistant"]
        tools = [message for message in messages if message["role"] == "tool"]
        self.assertEqual(len(assistants), 2)
        self.assertEqual(len(assistants[0]["tool_calls"]), 2)
        self.assertEqual(len(tools), 2)
        self.assertEqual(len(case["request"]["tools"]), 2)
        self.assertEqual(case["expected"]["min_tool_calls"], 2)

    def test_text_case_with_tool_history_forces_no_new_call(self):
        parsed = parse_shape(shape(protocol="text", expected_tool_calls=0), 1)
        case = build_case(parsed, 0, "fresh")
        self.assertEqual(case["request"]["tool_choice"], "none")
        self.assertEqual(case["expected"]["max_tool_calls"], 0)

    def test_summary_uses_only_fixed_categories_and_buckets(self):
        shapes = [
            parse_shape(shape(protocol="text", expected_tool_calls=0), 1),
            parse_shape(
                shape(
                    arrival_offset_ms=1200,
                    protocol="required_tool",
                    expected_tool_calls=1,
                    sampling={
                        "temperature": 0,
                        "top_p": 1,
                        "seed": 9,
                        "reasoning_effort": "low",
                    },
                ),
                2,
            ),
        ]
        summary = summarize_shapes(shapes)
        self.assertEqual(summary["protocol_counts"]["text"], 1)
        self.assertEqual(summary["protocol_counts"]["required_tool"], 1)
        self.assertEqual(summary["sampling_counts"], {"deterministic": 1, "agentic": 1, "other": 0})
        self.assertEqual(summary["arrival_span_bucket_ms"], "10000")
        self.assertNotIn("prefix_group", summary)

    @mock.patch("agent_trace.execute_case")
    def test_execution_separates_protocol_and_shape_validity(self, execute):
        parsed = parse_shape(shape(), 1)
        args = types.SimpleNamespace(
            base="http://invalid",
            model="model",
            token="token",
            timeout=1,
            prompt_token_tolerance_min=10,
            prompt_token_tolerance_pct=1,
        )
        execute.return_value = {"ok": True, "prompt_tokens": 2049}
        result = _execute_shape(args, parsed, 0, build_case(parsed, 0, "salt"), 0)
        self.assertTrue(result["protocol_valid"])
        self.assertTrue(result["shape_valid"])
        self.assertTrue(result["ok"])
        self.assertEqual(result["prompt_token_delta"], 1)

        execute.return_value = {"ok": True, "prompt_tokens": 4096}
        result = _execute_shape(args, parsed, 0, build_case(parsed, 0, "salt"), 0)
        self.assertTrue(result["protocol_valid"])
        self.assertFalse(result["shape_valid"])
        self.assertFalse(result["ok"])

    @mock.patch("agent_trace.time.sleep")
    @mock.patch("agent_trace.load_metadata")
    @mock.patch("agent_trace.execute_case")
    def test_run_emits_only_structural_records_and_summary(self, execute, metadata, _sleep):
        records = [
            shape(protocol="text", expected_tool_calls=0),
            shape(
                arrival_offset_ms=100,
                protocol="required_tool",
                expected_tool_calls=1,
            ),
        ]
        private = PrivateTrace(records)
        execute.side_effect = lambda *_args: {
            "ok": True,
            "protocol_errors": [],
            "route": "0",
            "prompt_tokens": 2048,
            "cached_tokens": 1024,
            "completion_tokens": 192,
            "ttft_ms": 10.0,
            "mean_itl_ms": 2.0,
        }
        metadata.return_value = {"gpu_count": 8, "engine_image": "safe"}
        args = types.SimpleNamespace(
            base="http://invalid",
            model="model",
            trace=private.path,
            metadata_json=pathlib.Path("metadata.json"),
            salt="fresh",
            label="shape-test",
            concurrency=2,
            timeout=1,
            prompt_token_tolerance_min=10,
            prompt_token_tolerance_pct=1,
        )
        output = io.StringIO()
        try:
            with mock.patch.dict(os.environ, {"BENCH_TOKEN": "token"}), contextlib.redirect_stdout(
                output
            ):
                self.assertEqual(command_run(args), 0)
        finally:
            private.close()
        lines = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual([line["type"] for line in lines], ["request", "request", "summary"])
        self.assertEqual([line["shape_ordinal"] for line in lines[:2]], [0, 1])
        self.assertEqual(lines[-1]["successful"], 2)
        self.assertEqual(lines[-1]["route_counts"]["0"], 2)
        self.assertTrue(all("prefix_group" not in line for line in lines))
        self.assertNotIn("fresh", output.getvalue())


if __name__ == "__main__":
    unittest.main()
