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
    engine_generation_tokens = 2

    def log_message(self, _format, *_args):
        return

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        events = (
            {"choices": [{"delta": {"content": "x"}}]},
            {
                "choices": [{"delta": {"content": "y"}}],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 2,
                    "prompt_tokens_details": {"cached_tokens": 0},
                },
            },
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
    def run_benchmark(self, engine_generation_tokens):
        BenchmarkHandler.metrics_calls = 0
        BenchmarkHandler.engine_generation_tokens = engine_generation_tokens
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
        self.assertTrue(report["dspark"]["reconciled"])
        self.assertNotIn("measurement_error", report)

    def test_required_native_reconciliation_fails_contaminated_interval(self):
        result = self.run_benchmark(engine_generation_tokens=3)
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["dspark"]["state"], "contaminated")
        self.assertEqual(report["measurement_error"], "speculation_not_reconciled")


if __name__ == "__main__":
    unittest.main()
