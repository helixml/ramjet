import json
import os
import pathlib
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SCRIPT = pathlib.Path(__file__).with_name("mixed_bench.py")


class MetricsState:
    def __init__(
        self,
        *,
        serves_requests=False,
        fault=None,
        omit_spec=False,
        running=0,
        waiting=0,
        kv_cache_usage=0.25,
    ):
        self.serves_requests = serves_requests
        self.fault = fault
        self.omit_spec = omit_spec
        self.running = running
        self.waiting = waiting
        self.kv_cache_usage = kv_cache_usage
        self.native_calls = 0
        self.post_calls = 0
        self.request_targets = None
        self.lock = threading.Lock()

    def add_request(self):
        with self.lock:
            targets = self.request_targets or [self]
            target = targets[self.post_calls % len(targets)]
            self.post_calls += 1
        with target.lock:
            target.native_calls += 1

    def calls(self):
        with self.lock:
            return self.native_calls


class BenchmarkHandler(BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        return

    @property
    def state(self):
        return self.server.metrics_state

    def do_POST(self):
        if self.path != "/v1/chat/completions" or not self.state.serves_requests:
            self.send_error(404)
            return
        request = json.loads(
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
        )
        prompt = request["messages"][0]["content"]
        self.state.add_request()
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
        body = b""
        for event in events:
            body += b"data: " + json.dumps(event).encode() + b"\n\n"
        body += b"data: [DONE]\n\n"
        # Give the gauge sampler a deterministic opportunity to observe the cell.
        time.sleep(0.01)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        route = "secondary" if prompt.startswith("[mixed decoder ") else "primary"
        self.send_header("X-Ramjet-Upstream", route)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path != "/metrics":
            self.send_error(404)
            return
        calls = self.state.calls()
        measured = calls > 1
        extras = {
            "request": int(measured and self.state.fault == "request"),
            "prompt": int(measured and self.state.fault == "prompt"),
            "generation": int(measured and self.state.fault == "generation"),
            "preemptions": int(measured and self.state.fault == "preemptions"),
        }
        values = {
            "vllm:num_preemptions_total": extras["preemptions"],
            "vllm:prompt_tokens_total": 10 * calls + extras["prompt"],
            "vllm:prompt_tokens_cached_total": 0,
            "vllm:prefix_cache_queries_total": 10 * calls,
            "vllm:prefix_cache_hits_total": 0,
            "vllm:request_queue_time_seconds_sum": calls / 100,
            "vllm:request_queue_time_seconds_count": calls,
            "vllm:request_prefill_time_seconds_sum": calls / 50,
            "vllm:request_prefill_time_seconds_count": calls,
            "vllm:num_requests_running": self.state.running,
            "vllm:num_requests_waiting": self.state.waiting,
            "vllm:kv_cache_usage_perc": self.state.kv_cache_usage,
            "vllm:generation_tokens_total": 2 * calls + extras["generation"],
            "vllm:request_success_total": calls + extras["request"],
            "vllm:spec_decode_num_drafts_total": calls,
            "vllm:spec_decode_num_draft_tokens_total": 3 * calls,
            "vllm:spec_decode_num_accepted_tokens_total": 2 * calls,
        }
        if self.state.omit_spec:
            values.pop("vllm:spec_decode_num_draft_tokens_total")
        lines = [f'{name}{{model_name="model"}} {value}\n' for name, value in values.items()]
        lines.append(
            'vllm:spec_decode_num_accepted_tokens_per_pos_total{'
            f'model_name="model",position="0"}} {2 * calls}\n'
        )
        body = "".join(lines).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class MixedBenchTest(unittest.TestCase):
    @staticmethod
    def start_server(state):
        server = ThreadingHTTPServer(("127.0.0.1", 0), BenchmarkHandler)
        server.metrics_state = state
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        return server, thread

    def run_benchmark(
        self, *, fault=None, omit_spec=False, multi=True, require=True
    ):
        primary = MetricsState(
            serves_requests=True,
            fault=fault,
            omit_spec=omit_spec,
            running=1,
            waiting=2,
            kv_cache_usage=0.25,
        )
        secondary = MetricsState(running=3, waiting=4, kv_cache_usage=0.75)
        primary.request_targets = [primary, secondary] if multi else [primary]
        servers = [self.start_server(primary), self.start_server(secondary)]
        primary_url = f"http://127.0.0.1:{servers[0][0].server_port}"
        secondary_url = f"http://127.0.0.1:{servers[1][0].server_port}"
        environment = {**os.environ}
        environment.pop("METRICS_URL", None)
        environment.pop("METRICS_URLS", None)
        environment.update(
            BENCH_TOKEN="test-only",
            MIXED_LEAD_MS="0",
            METRICS_INTERVAL="0.001",
            SALT="test-only",
        )
        if require:
            environment["BENCH_REQUIRE_RECONCILED_SPECULATION"] = "1"
        else:
            environment.pop("BENCH_REQUIRE_RECONCILED_SPECULATION", None)
        if multi:
            environment["METRICS_URLS"] = (
                primary_url + "/metrics," + secondary_url + "/metrics"
            )
        else:
            environment["METRICS_URL"] = primary_url + "/metrics"
        try:
            return subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    primary_url,
                    "model",
                    "32",
                    "1",
                    "2",
                    "1",
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
                timeout=10,
            )
        finally:
            for server, _ in servers:
                server.shutdown()
                server.server_close()
            for _, thread in servers:
                thread.join(timeout=2)

    def test_two_engine_interval_reconciles_exact_client_and_native_work(self):
        result = self.run_benchmark()
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["metrics_endpoints"], 2)
        self.assertTrue(report["require_reconciled_speculation"])
        self.assertEqual(
            report["reconciliation"]["client"],
            {
                "requests": 2,
                "prompt_tokens": 20,
                "generation_tokens": 4,
                "preemptions": 0,
            },
        )
        self.assertEqual(
            report["reconciliation"]["engine"],
            report["reconciliation"]["client"],
        )
        self.assertTrue(all(report["reconciliation"]["matches"].values()))
        self.assertTrue(report["reconciliation"]["speculation_match"])
        self.assertTrue(report["reconciliation"]["reconciled"])
        self.assertEqual(report["engine_metrics_delta"]["prompt_tokens"], 20)
        self.assertEqual(report["engine_metrics_delta"]["preemptions"], 0)
        self.assertEqual(report["engine_metric_peaks"]["running"], 4)
        self.assertEqual(report["engine_metric_peaks"]["waiting"], 6)
        self.assertEqual(report["engine_metric_peaks"]["kv_cache_usage"], 0.75)
        self.assertEqual(report["speculative"]["engine_finished_requests"], 2)
        self.assertEqual(report["speculative"]["engine_generation_tokens"], 4)
        self.assertEqual(
            report["run_route_relationships"],
            [
                {
                    "run": 0,
                    "prefill_route": "primary",
                    "decoder_same_route": 0,
                    "decoder_other_route": 1,
                    "decoder_unknown_route": 0,
                }
            ],
        )
        self.assertNotIn("measurement_error", report)

    def test_each_native_authority_mismatch_fails_closed(self):
        for fault, field in (
            ("request", "requests"),
            ("prompt", "prompt_tokens"),
            ("generation", "generation_tokens"),
            ("preemptions", "preemptions"),
        ):
            with self.subTest(fault=fault):
                result = self.run_benchmark(fault=fault)
                self.assertEqual(result.returncode, 1, result.stderr)
                report = json.loads(result.stdout)
                self.assertFalse(report["reconciliation"]["matches"][field])
                self.assertFalse(report["reconciliation"]["reconciled"])
                self.assertEqual(
                    report["measurement_error"], "native_metrics_not_reconciled"
                )

    def test_incomplete_speculative_counters_fail_closed(self):
        result = self.run_benchmark(omit_spec=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["speculative"]["state"], "incomplete")
        self.assertFalse(report["reconciliation"]["speculation_match"])
        self.assertEqual(
            report["measurement_error"], "native_metrics_not_reconciled"
        )

    def test_legacy_metrics_url_keeps_existing_observations_advisory(self):
        result = self.run_benchmark(omit_spec=True, multi=False, require=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["metrics_endpoints"], 1)
        self.assertIn("engine_metrics_delta", report)
        self.assertIn("engine_metric_peaks", report)
        self.assertEqual(report["engine_metrics_delta"]["prompt_tokens"], 20)
        self.assertNotIn("measurement_error", report)

    def test_required_reconciliation_rejects_missing_metrics_authority(self):
        environment = {**os.environ, "BENCH_TOKEN": "test-only"}
        environment.pop("METRICS_URL", None)
        environment.pop("METRICS_URLS", None)
        environment["BENCH_REQUIRE_RECONCILED_SPECULATION"] = "1"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "http://unused", "model"],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
            timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires METRICS_URLS or METRICS_URL", result.stderr)


if __name__ == "__main__":
    unittest.main()
