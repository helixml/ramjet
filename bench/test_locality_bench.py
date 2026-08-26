import http.server
import os
import pathlib
import subprocess
import tempfile
import threading
import unittest


class LocalityBenchTest(unittest.TestCase):
    def test_response_usage_remains_the_default_and_keeps_legacy_total(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            fake_curl = root / "curl"
            fake_curl.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$*\" > " + str(root / "curl-args") + "\n"
                "printf '%s\\n' '{\"usage\":{\"prompt_tokens\":100,"
                "\"prompt_tokens_details\":{\"cached_tokens\":40}}}'\n",
                encoding="utf-8",
            )
            fake_curl.chmod(0o755)
            env = dict(os.environ)
            env.pop("CACHE_AUTHORITY", None)
            env.pop("ENGINE_METRICS_URLS", None)
            env["MODEL"] = "qwen3.8-flash-next"
            env["PATH"] = f"{root}:{env['PATH']}"
            result = subprocess.run(
                ["bash", "bench/locality_bench.sh", "http://unused", "1", "1", "1"],
                cwd=pathlib.Path(__file__).resolve().parents[1],
                env=env,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            curl_args = (root / "curl-args").read_text(encoding="utf-8")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines()[-1],
            "TOTAL prompt=100 cached=40 hit=40.0%",
        )
        self.assertIn(
            '\"model\": \"qwen3.8-flash-next\"',
            curl_args,
        )

    def test_native_cache_authority_aggregates_multiple_metrics_urls(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            state = root / "served"
            fake_curl = root / "curl"
            fake_curl.write_text(
                "#!/bin/sh\n"
                f": > {state}\n"
                "printf '%s\\n' '{\"usage\":{\"prompt_tokens\":100,"
                "\"prompt_tokens_details\":{\"cached_tokens\":0}}}'\n",
                encoding="utf-8",
            )
            fake_curl.chmod(0o755)

            class Metrics(http.server.BaseHTTPRequestHandler):
                def do_GET(self):
                    active = state.exists()
                    if self.path == "/a":
                        queries, hits, requests = (60, 50, 1) if active else (0, 0, 0)
                    else:
                        queries, hits, requests = (40, 30, 0) if active else (0, 0, 0)
                    body = (
                        f"vllm:prefix_cache_queries_total {queries}\n"
                        f"vllm:prefix_cache_hits_total {hits}\n"
                        f"vllm:prompt_tokens_total {queries}\n"
                        f"vllm:request_queue_time_seconds_count {requests}\n"
                        f"vllm:request_prefill_time_seconds_count {requests}\n"
                    ).encode()
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def log_message(self, _format, *_args):
                    pass

            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Metrics)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                port = server.server_address[1]
                env = dict(os.environ)
                env.update(
                    CACHE_AUTHORITY="vllm-prefix",
                    ENGINE_METRICS_URLS=(
                        f"http://127.0.0.1:{port}/a,"
                        f"http://127.0.0.1:{port}/b"
                    ),
                    PATH=f"{root}:{env['PATH']}",
                )
                result = subprocess.run(
                    [
                        "bash",
                        "bench/locality_bench.sh",
                        "http://unused",
                        "1",
                        "1",
                        "1",
                    ],
                    cwd=pathlib.Path(__file__).resolve().parents[1],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
            finally:
                server.shutdown()
                thread.join(timeout=5)
                server.server_close()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                "TOTAL prompt=100 cached=80 hit=80.0% "
                "authority=vllm_prefix_counters response_prompt=100 response_cached=0",
                result.stdout,
            )

    def test_native_cache_authority_requires_metrics_urls_before_work(self):
        env = dict(os.environ)
        env.update(CACHE_AUTHORITY="vllm-prefix", ENGINE_METRICS_URLS="")
        result = subprocess.run(
            ["bash", "bench/locality_bench.sh", "http://unused", "1", "1", "1"],
            cwd=pathlib.Path(__file__).resolve().parents[1],
            env=env,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("requires ENGINE_METRICS_URLS", result.stderr)


if __name__ == "__main__":
    unittest.main()
