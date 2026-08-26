import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SCRIPT = pathlib.Path(__file__).with_name("context_frontier.py")


class FrontierHandler(BaseHTTPRequestHandler):
    request_calls = 0
    metrics_calls = 0
    contaminate = False

    def log_message(self, _format, *_args):
        return

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.__class__.request_calls += 1
        event = {
            "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 0},
            },
        }
        body = b"data: " + json.dumps(event).encode() + b"\n\ndata: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path != "/metrics":
            self.send_error(404)
            return
        calls = self.__class__.request_calls
        contamination = int(
            self.__class__.contaminate and self.__class__.metrics_calls > 0
        )
        self.__class__.metrics_calls += 1
        values = {
            "vllm:prompt_tokens_total": 10 * calls,
            "vllm:generation_tokens_total": 2 * calls + contamination,
            "vllm:request_success_total": calls,
            "vllm:spec_decode_num_drafts_total": calls,
            "vllm:spec_decode_num_draft_tokens_total": 3 * calls,
            "vllm:spec_decode_num_accepted_tokens_total": 2 * calls,
        }
        body = "".join(
            f'{name}{{model_name="model"}} {value}\n'
            for name, value in values.items()
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class ContextFrontierTest(unittest.TestCase):
    def run_frontier(self, *, contaminate=False):
        FrontierHandler.request_calls = 0
        FrontierHandler.metrics_calls = 0
        FrontierHandler.contaminate = contaminate
        server = ThreadingHTTPServer(("127.0.0.1", 0), FrontierHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        environment = {
            **os.environ,
            "BENCH_TOKEN": "test-only",
            "CONTEXT_TOKENS": "32",
            "MAX_OUTPUT_TOKENS": "2",
            "METRICS_URL": base + "/metrics",
            "BENCH_REQUIRE_RECONCILED_SPECULATION": "1",
            "SALT": "test-only",
        }
        try:
            return subprocess.run(
                [sys.executable, str(SCRIPT), base + "/v1", "model", "2"],
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

    @staticmethod
    def report(result):
        return [json.loads(line) for line in result.stdout.splitlines()][-1]

    def test_exact_native_interval_reports_mean_ttft_and_reconciles(self):
        result = self.run_frontier()
        self.assertEqual(result.returncode, 0, result.stderr)
        report = self.report(result)
        self.assertIsNotNone(report["cold"]["ttft_ms_mean"])
        self.assertIn("ttft_ms_median", report["cold"])
        self.assertIn("ttft_ms_p95", report["cold"])
        for phase in ("cold", "warm"):
            reconciliation = report[f"{phase}_reconciliation"]
            self.assertTrue(reconciliation["reconciled"])
            self.assertEqual(
                reconciliation["client"],
                {"requests": 2, "prompt_tokens": 20, "generation_tokens": 4},
            )
            self.assertEqual(reconciliation["engine"], reconciliation["client"])
            self.assertEqual(
                reconciliation["matches"],
                {"requests": True, "prompt_tokens": True, "generation_tokens": True},
            )
            self.assertTrue(reconciliation["speculation"]["reconciled"])
            self.assertEqual(report[f"{phase}_dspark"]["draft_tokens"], 6)
        self.assertNotIn("measurement_error", report)

    def test_required_native_interval_fails_on_contamination(self):
        result = self.run_frontier(contaminate=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        report = self.report(result)
        self.assertFalse(report["cold_reconciliation"]["reconciled"])
        self.assertTrue(report["cold_reconciliation"]["contaminated"])
        self.assertFalse(
            report["cold_reconciliation"]["matches"]["generation_tokens"]
        )
        self.assertEqual(report["measurement_error"], "native_metrics_not_reconciled")


if __name__ == "__main__":
    unittest.main()
