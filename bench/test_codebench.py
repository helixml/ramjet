import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SCRIPT = pathlib.Path(__file__).with_name("codebench.py")


class BenchmarkHandler(BaseHTTPRequestHandler):
    metrics_calls = 0
    request_calls = 0
    engine_generation_tokens = 2
    single_delta = False

    def log_message(self, _format, *_args):
        return

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        final = {
            "choices": [{"delta": {"content": "y"}}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 0},
            },
        }
        self.__class__.request_calls += 1
        single_delta = self.__class__.single_delta and self.__class__.request_calls > 1
        events = (final,) if single_delta else (
            {"choices": [{"delta": {"content": "x"}}]},
            final,
        )
        body = b"".join(
            b"data: " + json.dumps(event).encode() + b"\n\n" for event in events
        ) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path != "/metrics":
            self.send_error(404)
            return
        after = self.__class__.metrics_calls > 0
        self.__class__.metrics_calls += 1
        values = {
            "vllm:spec_decode_num_drafts_total": 1 if after else 0,
            "vllm:spec_decode_num_draft_tokens_total": 5 if after else 0,
            "vllm:spec_decode_num_accepted_tokens_total": 1 if after else 0,
            "vllm:generation_tokens_total": (
                self.__class__.engine_generation_tokens if after else 0
            ),
            "vllm:request_success_total": 1 if after else 0,
        }
        body = "".join(f"{name} {value}\n" for name, value in values.items()).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class CodebenchTest(unittest.TestCase):
    def run_benchmark(self, engine_generation_tokens, *, single_delta=False):
        BenchmarkHandler.metrics_calls = 0
        BenchmarkHandler.request_calls = 0
        BenchmarkHandler.engine_generation_tokens = engine_generation_tokens
        BenchmarkHandler.single_delta = single_delta
        server = ThreadingHTTPServer(("127.0.0.1", 0), BenchmarkHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        environment = {
            **os.environ,
            "BENCH_TOKEN": "test-only",
            "METRICS_URL": base + "/metrics",
            "BENCH_REQUIRE_RECONCILED_SPECULATION": "1",
        }
        try:
            return subprocess.run(
                [sys.executable, str(SCRIPT), base, "model", "2", "1", "1"],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
                timeout=10,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_required_native_reconciliation_passes_exact_interval(self):
        result = self.run_benchmark(engine_generation_tokens=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["schema_version"], 2)
        self.assertEqual(report["type"], "engine_cell")
        self.assertGreater(report["observation_window_seconds"], 0)
        self.assertEqual(report["completion_rate"], 1)
        self.assertEqual(len(report["request_observations"]), 1)
        self.assertEqual(len(report["repetition_observations"]), 1)
        self.assertEqual(report["repetition_observations"][0]["repetition"], 0)
        self.assertGreater(
            report["repetition_observations"][0]["observation_window_seconds"], 0
        )
        observation = report["request_observations"][0]
        self.assertEqual(observation["repetition"], 0)
        self.assertTrue(observation["ok"])
        self.assertIsNotNone(observation["ttft_ms"])
        self.assertIsNotNone(observation["tpot_ms"])
        self.assertIsNotNone(report["tpot_ms_p95"])
        self.assertTrue(report["dspark"]["reconciled"])
        self.assertNotIn("measurement_error", report)

    def test_required_native_reconciliation_fails_contaminated_interval(self):
        result = self.run_benchmark(engine_generation_tokens=3)
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["dspark"]["state"], "contaminated")
        self.assertEqual(report["measurement_error"], "speculation_not_reconciled")

    def test_one_stream_delta_cannot_false_green_without_measurable_tpot(self):
        result = self.run_benchmark(engine_generation_tokens=2, single_delta=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["requests_ok"], 0)
        self.assertEqual(report["requests_failed"], 1)
        self.assertIsNone(report["request_observations"][0]["tpot_ms"])
        self.assertIn("measurable TPOT", report["errors"][0])


if __name__ == "__main__":
    unittest.main()
