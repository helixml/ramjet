import threading
import unittest
from unittest import mock

import route_conflict


def ordinary_snapshot(**changes):
    result = {key: 0.0 for key in route_conflict.ENGINE_COUNTERS}
    result.update(changes)
    return result


def speculative_snapshot(**changes):
    result = {
        "draft_steps": 0.0,
        "proposed_tokens": 0.0,
        "accepted_tokens": 0.0,
        "generation_tokens": 0.0,
        "finished_requests": 0.0,
        "accepted_per_position": {},
    }
    result.update(changes)
    return result


class RouteConflictTests(unittest.TestCase):
    def config(self):
        return {
            "base": "http://engine",
            "probe_base": "http://engine",
            "model": "model",
            "blockers": 2,
            "blocker_tokens": 32,
            "runs": 1,
            "token": "secret",
            "salt": "fresh",
            "context_tokens": 100,
            "probe_tokens": 8,
            "blocker_tail_kib": 0,
            "blocker_ready_mode": "headers",
            "metrics_urls": ["http://a/metrics", "http://b/metrics"],
            "require_reconciled": True,
            "settle_seconds": 0,
        }

    def test_run_waits_for_every_blocker_and_retains_usage(self):
        calls = []

        def fake_request(
            base,
            model,
            token,
            system,
            user,
            max_tokens,
            output,
            key,
            ready=None,
            ready_mode="headers",
        ):
            calls.append(key)
            output[key] = {
                "ok": True,
                "route": "0",
                "ttft_ms": 5.0,
                "wall_ms": 10.0,
                "prompt_tokens": 11,
                "cached_tokens": 0,
                "completion_tokens": max_tokens,
                "request_bytes": 100,
            }
            if ready is not None:
                ready.set()

        with (
            mock.patch("route_conflict.stream_request", side_effect=fake_request),
            mock.patch("route_conflict.route_state_snapshot", return_value=[]),
            mock.patch("route_conflict.engine_gauge_snapshot", return_value=[]),
        ):
            output, window, boundary = route_conflict.run_once(0, self.config())
        self.assertEqual({"warm", "blocker-0", "blocker-1", "probe"}, set(calls))
        self.assertEqual(32, output["blocker-0"]["completion_tokens"])
        self.assertEqual(32, output["blocker-1"]["completion_tokens"])
        self.assertGreater(window, 0)
        self.assertEqual({"router": [], "engines": []}, boundary)

    def test_probe_base_can_bypass_router_for_direct_oracle(self):
        calls = []

        def fake_request(
            base,
            model,
            token,
            system,
            user,
            max_tokens,
            output,
            key,
            ready=None,
            ready_mode="headers",
        ):
            calls.append((key, base))
            output[key] = {
                "ok": True,
                "route": None,
                "ttft_ms": 1,
                "wall_ms": 2,
                "prompt_tokens": 10,
                "completion_tokens": 1,
                "cached_tokens": 0,
                "request_bytes": 100,
            }
            if ready is not None:
                ready.set()

        config = self.config()
        config["probe_base"] = "http://direct-b"
        with (
            mock.patch("route_conflict.stream_request", side_effect=fake_request),
            mock.patch("route_conflict.route_state_snapshot", return_value=[]),
            mock.patch("route_conflict.engine_gauge_snapshot", return_value=[]),
        ):
            route_conflict.run_once(0, config)
        self.assertEqual("http://direct-b", dict(calls)["probe"])
        self.assertEqual("http://engine", dict(calls)["warm"])
        self.assertEqual("http://engine", dict(calls)["blocker-0"])

    def test_first_token_mode_does_not_signal_on_headers_or_role_only_delta(self):
        ready = threading.Event()

        class Response:
            headers = {"X-Ramjet-Upstream": "0"}

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def __iter__(self):
                self.assert_ready(False)
                yield b'data: {"choices":[{"delta":{"role":"assistant"}}]}\n'
                self.assert_ready(False)
                yield b'data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}\n'
                self.assert_ready(True)
                yield b'data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":1}}\n'

            @staticmethod
            def assert_ready(expected):
                if ready.is_set() != expected:
                    raise AssertionError(f"ready={ready.is_set()}, expected={expected}")

        output = {}
        with mock.patch("route_conflict.urllib.request.urlopen", return_value=Response()):
            route_conflict.stream_request(
                "http://lb",
                "model",
                "token",
                "system",
                "user",
                8,
                output,
                "request",
                ready,
                "first_token",
            )
        self.assertTrue(output["request"]["ok"])
        self.assertTrue(ready.is_set())

    def test_blocker_tail_is_unique_and_exactly_bounded(self):
        config = self.config()
        config["blocker_tail_kib"] = 2
        first = route_conflict.blocker_user(config, 0, 0)
        second = route_conflict.blocker_user(config, 0, 1)
        self.assertNotEqual(first, second)
        self.assertGreaterEqual(len(first.encode()), 2048)
        self.assertLess(len(first.encode()), 2200)

    def test_route_and_engine_boundary_snapshots_are_bounded(self):
        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

        response = Response()
        response.read = lambda: b""
        with mock.patch(
            "route_conflict.json.load",
            return_value={
                "replicas": [
                    {"index": 0, "inflight": 2, "load_units": 16, "ignored": "value"},
                    {"index": 1, "inflight": 0, "load_units": 0},
                ]
            },
        ), mock.patch("route_conflict.urllib.request.urlopen", return_value=response):
            self.assertEqual(
                [
                    {"upstream": 0, "inflight": 2, "load_units": 16},
                    {"upstream": 1, "inflight": 0, "load_units": 0},
                ],
                route_conflict.route_state_snapshot("http://lb"),
            )

        with mock.patch(
            "route_conflict.fetch_engine_metrics",
            side_effect=[
                {"running": 2, "waiting": 0, "kv_cache_usage": 0.25},
                {"running": 0, "waiting": 0, "kv_cache_usage": 0.0},
            ],
        ):
            self.assertEqual(
                [
                    {"running": 2, "waiting": 0, "kv_cache_usage": 0.25},
                    {"running": 0, "waiting": 0, "kv_cache_usage": 0.0},
                ],
                route_conflict.engine_gauge_snapshot(["http://a", "http://b"]),
            )

    def test_phase_controls_are_typed_and_bounded(self):
        with mock.patch.dict(
            route_conflict.os.environ,
            {"BENCH_TOKEN": "secret", "BLOCKER_READY_MODE": "first_byte"},
            clear=True,
        ):
            with self.assertRaisesRegex(SystemExit, "headers or first_token"):
                route_conflict.parse_config(["http://lb", "model"])
        with mock.patch.dict(
            route_conflict.os.environ,
            {"BENCH_TOKEN": "secret", "BLOCKER_TAIL_KIB": "8193"},
            clear=True,
        ):
            with self.assertRaisesRegex(SystemExit, "0 through 8192"):
                route_conflict.parse_config(["http://lb", "model"])
        with mock.patch.dict(
            route_conflict.os.environ,
            {"BENCH_TOKEN": "secret", "PROBE_BASE": "http://direct-b/"},
            clear=True,
        ):
            config = route_conflict.parse_config(["http://lb/", "model"])
        self.assertEqual("http://lb", config["base"])
        self.assertEqual("http://direct-b", config["probe_base"])

    def test_aggregate_engine_delta_sums_two_engines_and_derived_metrics(self):
        before = [ordinary_snapshot(), ordinary_snapshot()]
        after = [
            ordinary_snapshot(
                preemptions=1,
                prompt_tokens=100,
                cached_prompt_tokens=25,
                prefix_queries=100,
                prefix_hits=25,
                queue_seconds_sum=0.2,
                queue_samples=2,
                prefill_seconds_sum=0.4,
                prefill_samples=2,
            ),
            ordinary_snapshot(
                preemptions=2,
                prompt_tokens=200,
                cached_prompt_tokens=50,
                prefix_queries=200,
                prefix_hits=50,
                queue_seconds_sum=0.4,
                queue_samples=4,
                prefill_seconds_sum=0.8,
                prefill_samples=4,
            ),
        ]
        result = route_conflict.aggregate_engine_delta(before, after)
        self.assertEqual(3, result["preemptions"])
        self.assertEqual(300, result["prompt_tokens"])
        self.assertEqual(300, result["prefix_queries"])
        self.assertEqual(75, result["prefix_hits"])
        self.assertEqual(25.0, result["prefix_hit_pct"])
        self.assertEqual(100.0, result["queue_ms_mean"])
        self.assertEqual(200.0, result["prefill_ms_mean"])

    def test_speculative_snapshots_combine_positions(self):
        result = route_conflict.combine_speculative_snapshots(
            [
                speculative_snapshot(draft_steps=2, accepted_per_position={0: 2, 1: 1}),
                speculative_snapshot(draft_steps=3, accepted_per_position={0: 3, 2: 1}),
            ]
        )
        self.assertEqual(5, result["draft_steps"])
        self.assertEqual({0: 5, 1: 1, 2: 1}, result["accepted_per_position"])

    def test_reconciliation_ignores_response_cached_tokens(self):
        records = [
            {
                "ok": True,
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cached_tokens": 99,
            },
            {
                "ok": True,
                "prompt_tokens": 200,
                "completion_tokens": 30,
                "cached_tokens": 199,
            },
        ]
        engine = {"prompt_tokens": 300, "queue_samples": 2, "prefill_samples": 2}
        result = route_conflict.build_reconciliation(
            records, engine, {"state": "enabled", "reconciled": True}
        )
        self.assertTrue(result["reconciled"])
        self.assertFalse(result["cached_tokens_authoritative"])
        self.assertNotIn("cached_tokens_match", result)

    def test_summary_reports_useful_blocker_work_and_contamination(self):
        config = self.config()
        outputs = [
            {
                "warm": {
                    "ok": True,
                    "route": "0",
                    "ttft_ms": 1,
                    "prompt_tokens": 10,
                    "completion_tokens": 2,
                    "cached_tokens": 0,
                    "request_bytes": 10,
                },
                "blocker-0": {
                    "ok": True,
                    "route": "0",
                    "ttft_ms": 10,
                    "prompt_tokens": 20,
                    "completion_tokens": 30,
                    "cached_tokens": 0,
                    "request_bytes": 20,
                },
                "blocker-1": {
                    "ok": True,
                    "route": "1",
                    "ttft_ms": 20,
                    "prompt_tokens": 20,
                    "completion_tokens": 30,
                    "cached_tokens": 0,
                    "request_bytes": 20,
                },
                "probe": {
                    "ok": True,
                    "route": "0",
                    "ttft_ms": 5,
                    "prompt_tokens": 10,
                    "completion_tokens": 8,
                    "cached_tokens": 0,
                    "request_bytes": 10,
                },
            }
        ]
        before = (
            [ordinary_snapshot(), ordinary_snapshot()],
            [speculative_snapshot(), speculative_snapshot()],
        )
        after = (
            [
                ordinary_snapshot(
                    prompt_tokens=30,
                    queue_samples=2,
                    prefill_samples=2,
                ),
                ordinary_snapshot(
                    prompt_tokens=31,  # one contaminating prompt token
                    queue_samples=2,
                    prefill_samples=2,
                ),
            ],
            [
                speculative_snapshot(
                    draft_steps=2,
                    proposed_tokens=6,
                    accepted_tokens=4,
                    generation_tokens=35,
                    finished_requests=2,
                ),
                speculative_snapshot(
                    draft_steps=2,
                    proposed_tokens=6,
                    accepted_tokens=4,
                    generation_tokens=35,
                    finished_requests=2,
                ),
            ],
        )
        result = route_conflict.summarize_runs(
            config,
            outputs,
            [1000],
            [{"router": [], "engines": []}],
            (before, after),
        )
        self.assertEqual(60.0, result["blocker_aggregate_output_tok_s"])
        self.assertEqual(20, result["blocker_ttft_ms_p95"])
        self.assertEqual(40, result["blocker_prompt_tokens"])
        self.assertTrue(result["reconciliation"]["contaminated"])
        self.assertFalse(result["reconciliation"]["reconciled"])
        self.assertEqual("enabled", result["speculative"]["state"])


if __name__ == "__main__":
    unittest.main()
