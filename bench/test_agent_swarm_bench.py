#!/usr/bin/env python3
"""Tests for the open-loop coding-agent swarm generator."""

import argparse
import http.server
import json
import random
import threading
import unittest

import agent_swarm_bench as swarm


class FakeEngine(http.server.BaseHTTPRequestHandler):
    """OpenAI-compatible streaming endpoint with a per-upstream prefix memory."""

    seen = []
    lock = threading.Lock()

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        text = "".join(message["content"] for message in body["messages"])
        prompt = len(text.split())
        with self.lock:
            cached = max((len(os.split()) for os in self.seen if text.startswith(os)), default=0)
            self.seen.append(text)
            FakeEngine.requests.append(body)
        completion = body["max_tokens"]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("x-ramjet-upstream", str(prompt % 2))
        self.end_headers()
        events = [
            {"choices": [{"delta": {"reasoning_content": "think"}}]},
            {"choices": [{"delta": {"content": "call tool"}}]},
            {
                "choices": [],
                "usage": {
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "prompt_tokens_details": {"cached_tokens": cached},
                },
            },
        ]
        for event in events:
            self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


def arguments(base, **overrides):
    values = dict(
        base=base,
        model="m",
        label="test",
        developers=3,
        duration=2.5,
        warmup=0.0,
        ramp=0.2,
        harnesses=2,
        harness_tokens=300,
        repos=2,
        repo_tokens=100,
        mean_turns=3,
        max_turns=5,
        tool_median_tokens=50,
        long_turn_share=0.2,
        compact_at=100_000,
        tool_seconds=0.3,
        human_pause_share=0.0,
        between_tasks_seconds=2,
        subagent_share=0.3,
        max_subagents=2,
        reasoning_effort="low",
        timeout=10,
        salt="salt-a",
        seed="seed-a",
        requests_jsonl=None,
    )
    values.update(overrides)
    return argparse.Namespace(**values)


class SwarmTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeEngine)
        cls.base = f"http://127.0.0.1:{cls.server.server_address[1]}"
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        FakeEngine.seen = []
        FakeEngine.requests = []

    def test_filler_is_deterministic_and_sized_by_calibration(self):
        filler = swarm.Filler(words_per_token=0.5)
        first = filler.text(random.Random("x"), 200)
        self.assertEqual(first, filler.text(random.Random("x"), 200))
        self.assertNotEqual(first, filler.text(random.Random("y"), 200))
        words = [word for word in first.split() if not word.endswith(";")]
        self.assertEqual(len(words), 100)

    def test_run_produces_bounded_content_free_summary(self):
        workload = swarm.Swarm(arguments(self.base))
        summary = workload.run()
        self.assertEqual(summary["requests_failed"], 0)
        self.assertGreater(summary["agent_turns"], 3)
        self.assertEqual(summary["tokens_per_word"], 1.0)
        followups = [
            record
            for record in workload.records
            if record.get("ok") and record["kind"] == "agent" and record["turn"] > 0
        ]
        self.assertTrue(followups)
        for record in followups:
            self.assertGreater(record["cached_tokens"], 0, "a follow-up extends its own prefix")
        self.assertEqual(set(summary["per_upstream"]), {"0", "1"})
        self.assertEqual(set(summary["slo_goodput"]), {"ttft_le_2s", "ttft_le_5s", "ttft_le_10s"})
        encoded = json.dumps(summary)
        self.assertNotIn("call tool", encoded)
        self.assertNotIn("harness", encoded.replace('"', " ").split())
        for body in FakeEngine.requests:
            if body["max_tokens"] > 1:
                self.assertEqual(body["min_tokens"], body["max_tokens"])
                self.assertTrue(body["ignore_eos"])

    def test_compaction_restarts_history_from_the_repository_prefix(self):
        workload = swarm.Swarm(arguments(self.base, compact_at=1, subagent_share=0.0, developers=1))
        workload.run()
        turns = sorted(
            (record for record in workload.records if record.get("ok")),
            key=lambda record: (record["task"], record["turn"]),
        )
        followups = [record for record in turns if record["turn"] > 0]
        self.assertTrue(followups)
        self.assertTrue(all(record["compactions"] >= 1 for record in followups))
        self.assertTrue(all(record["prompt_tokens"] < 5_000 for record in followups))

    def test_seed_fixes_structure_and_salt_changes_text(self):
        def shape(salt, seed):
            FakeEngine.seen = []
            FakeEngine.requests = []
            workload = swarm.Swarm(arguments(self.base, salt=salt, seed=seed, subagent_share=0.0))
            workload.filler.words_per_token = 1.0
            workload.build_layers()
            return workload.repos, [len(repo.split()) for repo in workload.repos]

        repos_a, sizes_a = shape("salt-a", "seed-a")
        repos_b, sizes_b = shape("salt-b", "seed-a")
        _, sizes_c = shape("salt-a", "seed-b")
        self.assertEqual(sizes_a, sizes_b)
        self.assertNotEqual(repos_a, repos_b)
        self.assertNotEqual(sizes_a, sizes_c)

    def test_summary_excludes_warmup_and_measures_stickiness(self):
        workload = swarm.Swarm(arguments(self.base, warmup=10.0))
        record = dict(ok=True, kind="agent", dev=0, task=0, ttft=1.0, e2e=2.0, prompt_tokens=40_000,
                      cached_tokens=30_000, completion_tokens=10, tpot=0.01)
        workload.records = [
            dict(record, t=1.0, turn=0, upstream="0", ttft=99.0),
            dict(record, t=11.0, turn=1, upstream="0"),
            dict(record, t=12.0, turn=2, upstream="0"),
            dict(record, t=13.0, turn=3, upstream="1"),
            dict(ok=False, kind="agent", dev=1, task=0, turn=0, t=14.0, error="RuntimeError"),
        ]
        summary = workload.summary(wall=20.0, tokens_per_word=1.0)
        self.assertEqual(summary["requests_ok"], 3)
        self.assertEqual(summary["requests_failed"], 1)
        self.assertEqual(summary["ttft"]["p99"], 1.0)
        self.assertEqual(summary["session_stickiness"], 0.5)
        self.assertEqual(summary["cache_ratio"], 0.75)
        self.assertEqual(list(summary["by_context"]), ["32-64k"])
        self.assertEqual(summary["slo_goodput"]["ttft_le_2s"], 1.0)


if __name__ == "__main__":
    unittest.main()
