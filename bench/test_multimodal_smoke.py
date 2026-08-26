import base64
import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SCRIPT = pathlib.Path(__file__).with_name("multimodal_smoke.py")


class MultimodalHandler(BaseHTTPRequestHandler):
    metrics_calls = 0
    answer = "red"
    request_was_inline_png = False

    def log_message(self, _format, *_args):
        return

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length))
        content = body["messages"][0]["content"]
        image_url = next(
            part["image_url"]["url"]
            for part in content
            if part.get("type") == "image_url"
        )
        prefix = "data:image/png;base64,"
        image = base64.b64decode(image_url.removeprefix(prefix), validate=True)
        self.__class__.request_was_inline_png = (
            image_url.startswith(prefix)
            and image.startswith(b"\x89PNG\r\n\x1a\n")
            and "http" not in image_url
        )
        events = (
            {"choices": [{"delta": {"content": self.__class__.answer}}]},
            {
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 20,
                    "completion_tokens": 2,
                    "total_tokens": 22,
                    "prompt_tokens_details": {"cached_tokens": 0},
                },
            },
        )
        response = b"".join(
            b"data: " + json.dumps(event).encode() + b"\n\n" for event in events
        ) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def do_GET(self):
        if self.path != "/metrics":
            self.send_error(404)
            return
        after = self.__class__.metrics_calls > 0
        self.__class__.metrics_calls += 1
        values = {
            "vllm:spec_decode_num_drafts_total": 1 if after else 0,
            "vllm:spec_decode_num_draft_tokens_total": 3 if after else 0,
            "vllm:spec_decode_num_accepted_tokens_total": 2 if after else 0,
            "vllm:generation_tokens_total": 2 if after else 0,
            "vllm:request_success_total": 1 if after else 0,
        }
        response = "".join(
            f"{name} {value}\n" for name, value in values.items()
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


class MultimodalSmokeTest(unittest.TestCase):
    def run_smoke(self, answer="red"):
        MultimodalHandler.metrics_calls = 0
        MultimodalHandler.answer = answer
        MultimodalHandler.request_was_inline_png = False
        server = ThreadingHTTPServer(("127.0.0.1", 0), MultimodalHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        token = "private-test-token"
        environment = {**os.environ, "BENCH_TOKEN": token}
        try:
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    base,
                    "model",
                    "--engine-metrics",
                    base + "/metrics",
                    "--require-reconciled-speculation",
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
                timeout=10,
            )
            return result, token
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_inline_image_answer_and_native_metrics_reconcile_privately(self):
        result, token = self.run_smoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(MultimodalHandler.request_was_inline_png)
        self.assertTrue(report["ok"])
        self.assertTrue(report["visual_answer_match"])
        self.assertEqual(report["image"]["format"], "png")
        self.assertEqual(report["tokens"]["completion"], 2)
        self.assertTrue(report["speculation"]["reconciled"])
        self.assertGreater(report["timing"]["wall_ms"], 0)
        self.assertNotIn(token, result.stdout)
        self.assertNotIn("data:image", result.stdout)
        self.assertNotIn('"content"', result.stdout)

    def test_visual_mismatch_fails_without_emitting_response_content(self):
        result, token = self.run_smoke(answer="blue")
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertFalse(report["ok"])
        self.assertFalse(report["visual_answer_match"])
        self.assertIn("visual_answer_mismatch", report["failures"])
        self.assertNotIn("blue", result.stdout)
        self.assertNotIn("red", result.stdout)
        self.assertNotIn(token, result.stdout)


if __name__ == "__main__":
    unittest.main()
