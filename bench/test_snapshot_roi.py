import contextlib
import io
import json
import pathlib
import re
import tempfile
import unittest

import route_journal_archive
import route_replay
import serving_cost_audit
import snapshot_roi

T0 = 1_790_000_000_000
MINUTE = 60_000
HOUR = 60 * MINUTE


def start(seq, unix_ms, total_blocks, candidates, request_bytes=None):
    return {
        "v": 12,
        "event": "start",
        "seq": seq,
        "unix_ms": unix_ms,
        "endpoint": "chat",
        "request_bytes": request_bytes if request_bytes is not None else total_blocks * 2048,
        "total_blocks": total_blocks,
        "chunk_bytes": 2048,
        "affinity_horizon": {"mode": "off", "source": "none", "outcome": "off"},
        "candidates": [
            {
                "upstream": upstream,
                "overlap_blocks": overlap,
                "stale_blocks": 0,
                "overlap_ages_ms": ages,
            }
            for upstream, overlap, ages in candidates
        ],
    }


def finish(seq, unix_ms, upstream, prompt, cached, completion=100, result="complete", **extra):
    record = {
        "v": 12,
        "event": "finish",
        "seq": seq,
        "unix_ms": unix_ms,
        "result": result,
        "upstream": upstream,
        "status": 200,
        "prompt_tokens": float(prompt),
        "cached_tokens": float(cached),
        "completion_tokens": float(completion),
        "ttft_ms": 500.0,
    }
    record.update(extra)
    return record


def conversation(gap_ms, second_cached, *, second_upstream=1, first_upstream=1):
    """Two turns of one conversation: 20 blocks (10k tokens) then 40 (20k)."""
    first_finish = T0 + 1_000
    second_start = first_finish + gap_ms
    ages_on_first = [[20, gap_ms]]
    candidates = [
        (first_upstream, 20, ages_on_first),
        (3 - first_upstream, 0, []),
    ]
    return [
        ("c", start(1, T0, 20, [(1, 0, []), (2, 0, [])])),
        ("c", finish(1, first_finish, first_upstream, 10_000, 0)),
        ("c", start(2, second_start, 40, candidates)),
        ("c", finish(2, second_start + 2_000, second_upstream, 20_000, second_cached)),
    ]


def run(stream, horizons="300,3600,inf", upstreams=frozenset({1, 2}), **overrides):
    requests, first_seen, skipped = snapshot_roi.pair_requests(stream, set(upstreams))
    args = snapshot_roi.argparse.Namespace(
        horizons=snapshot_roi.parse_horizons(horizons),
        miss_min_tokens=2048,
        miss_min_fraction=0.02,
        prefill_tokens_per_second=5000,
        restore_tokens_per_second=50000,
        restore_fixed_seconds=0.0,
        state_bytes=80e6,
        kv_bytes_per_token=16000,
        gpus_per_replica=2,
        rate_min_uncached_tokens=16384,
        censor_window_ms=HOUR,
    )
    vars(args).update(overrides)
    return requests, snapshot_roi.analyze(requests, first_seen, args), skipped


def row(result, scope, horizon):
    return next(item for item in result["horizons"] if item["scope"] == scope and item["horizon"] == horizon)


class PairingTest(unittest.TestCase):
    def test_seq_restart_pairs_with_the_latest_start(self):
        stream = [
            ("c", start(1, T0, 4, [(1, 0, [])])),
            ("c", finish(1, T0 + 10, 1, 2_000, 0)),
            ("c", start(1, T0 + HOUR, 4, [(1, 0, [])])),
            ("c", finish(1, T0 + HOUR + 10, 1, 3_000, 0)),
        ]
        requests, _, _ = snapshot_roi.pair_requests(stream, {1})
        self.assertEqual([r.prompt_tokens for r in requests], [2_000, 3_000])

    def test_filters_unusable_requests_and_counts_them(self):
        stream = [
            ("c", start(1, T0, 4, [(1, 0, [])])),
            ("c", finish(1, T0 + 10, 1, 2_000, 0, result="client_disconnect")),
            ("c", start(2, T0, 4, [(1, 0, [])])),
            ("c", {**finish(2, T0 + 10, 1, 2_000, 0), "cached_tokens": None}),
            ("c", start(3, T0, 4, [(0, 0, [])])),
            ("c", finish(3, T0 + 10, 0, 2_000, 0)),
            ("c", start(4, T0, 4, [(1, 0, [])])),
            ("c", finish(4, T0 + 10, 1, 2_000, 0, status=400)),
            ("c", start(5, T0 + 5 * MINUTE, 4, [(1, 0, [])])),
            ("c", finish(5, T0 + 5 * MINUTE + 10, 1, 2_000, 0)),
        ]
        requests, _, skipped = snapshot_roi.pair_requests(
            stream, {1}, exclude=[(T0 + 4 * MINUTE, T0 + 6 * MINUTE)]
        )
        self.assertEqual(requests, [])
        self.assertEqual(
            dict(skipped),
            {
                "result_client_disconnect": 1,
                "missing_usage": 1,
                "status_400": 1,
                "excluded_window": 1,
            },
        )

    def test_reads_marked_log_lines_and_gzip_segments(self):
        lines = [
            "2026-09-26T07:00:00Z noise",
            snapshot_roi.MARKER + json.dumps(start(1, T0, 4, [(1, 0, [])])),
            json.dumps(finish(1, T0 + 10, 1, 2_000, 0)),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory, "segment.jsonl.gz")
            import gzip

            with gzip.open(path, "wt") as handle:
                handle.write("\n".join(lines) + "\n")
            stream = list(snapshot_roi.read_sources([str(path)]))
        self.assertEqual([record["event"] for _, record in stream], ["start", "finish"])


class LinkingTest(unittest.TestCase):
    def test_deepest_block_age_links_a_continuation(self):
        requests, result, _ = run(conversation(2 * MINUTE, 10_000))
        self.assertEqual(requests[1].link.predecessor, 0)
        self.assertEqual(requests[1].link.gap_ms, 2 * MINUTE)
        self.assertEqual(result["coverage"]["inferred_conversations"], 1)

    def test_shared_system_prompt_is_not_a_continuation(self):
        stream = conversation(2 * MINUTE, 10_000)
        # The follow-up shares only 5 of the predecessor's 20 blocks.
        stream[2][1]["candidates"][0].update(overlap_blocks=5, overlap_ages_ms=[[5, 2 * MINUTE]])
        requests, _, _ = run(stream)
        self.assertIsNone(requests[1].link)

    def test_cross_container_finishes_never_link(self):
        stream = conversation(2 * MINUTE, 10_000)
        stream[1] = ("other", stream[1][1])
        stream[0] = ("other", stream[0][1])
        requests, _, _ = run(stream)
        self.assertTrue(all(request.link is None for request in requests))


class HorizonTest(unittest.TestCase):
    def test_warm_continuation_is_not_avoidable(self):
        _, result, _ = run(conversation(2 * MINUTE, 10_000))
        self.assertEqual(row(result, "same_replica", "inf")["avoidable_tokens"], 0)

    def test_evicted_prefix_is_credited_only_beyond_its_gap(self):
        _, result, _ = run(conversation(2 * HOUR, 0))
        self.assertEqual(row(result, "same_replica", "5m")["avoidable_tokens"], 0)
        self.assertEqual(row(result, "same_replica", "1h")["avoidable_tokens"], 0)
        infinite = row(result, "same_replica", "inf")
        self.assertEqual(infinite["avoidable_tokens"], 10_000)
        self.assertEqual(infinite["requests_helped"], 1)
        self.assertEqual(result["miss_age_buckets"]["1-6h"]["avoidable_tokens"], 10_000)
        self.assertEqual(result["uncached_decomposition"]["recoverable_by_retention"], 10_000)
        self.assertEqual(result["uncached_decomposition"]["continuation_new_tokens"], 10_000)

    def test_shortfall_below_noise_floor_is_ignored(self):
        _, result, _ = run(conversation(2 * HOUR, 9_000))
        self.assertEqual(row(result, "same_replica", "inf")["avoidable_tokens"], 0)
        self.assertEqual(result["uncached_decomposition"]["continuation_below_noise_floor"], 1_000)

    def test_cross_replica_miss_needs_a_shared_tier(self):
        _, result, _ = run(conversation(2 * MINUTE, 0, second_upstream=2))
        self.assertEqual(row(result, "same_replica", "inf")["avoidable_tokens"], 0)
        self.assertEqual(row(result, "any_replica", "5m")["avoidable_tokens"], 10_000)
        self.assertEqual(result["miss_age_buckets"]["<5m"]["cross_replica"], 1)

    def test_unlinked_request_is_credited_its_fresh_served_blocks(self):
        stream = [
            ("c", start(1, T0, 40, [(1, 20, [[10, MINUTE], [10, 3 * HOUR]])])),
            ("c", finish(1, T0 + 1_000, 1, 20_000, 0)),
        ]
        _, result, _ = run(stream, horizons="3600,inf")
        self.assertEqual(row(result, "same_replica", "1h")["avoidable_tokens"], 5_000)
        self.assertEqual(row(result, "same_replica", "inf")["avoidable_tokens"], 10_000)

    def test_age_runs_past_the_journal_cap_inherit_the_last_age(self):
        self.assertEqual(snapshot_roi.fresh_blocks(100, [[10, 5], [10, 7]], 10), 100)
        self.assertEqual(snapshot_roi.fresh_blocks(100, [[10, 5], [10, 70]], 10), 10)
        self.assertEqual(snapshot_roi.fresh_blocks(100, [[10, 5]], None), 100)


class CapacityTest(unittest.TestCase):
    def test_tail_lives_until_consumed_or_expired(self):
        requests, _, _ = run(conversation(2 * HOUR, 0))
        tail = 10_100 * 16_000 + 80e6
        within = snapshot_roi.live_set(requests, 3 * HOUR, 80e6, 16_000, useful_only=True)
        self.assertEqual(within["peak_snapshots"], 1)
        self.assertEqual(within["peak_bytes"], int(tail))
        short = snapshot_roi.live_set(requests, HOUR, 80e6, 16_000, useful_only=True)
        self.assertEqual(short["peak_snapshots"], 0)
        everything = snapshot_roi.live_set(requests, HOUR, 80e6, 16_000, useful_only=False)
        self.assertEqual(everything["peak_snapshots"], 1)


class CliTest(unittest.TestCase):
    def test_json_report_from_a_journal_file(self):
        lines = [snapshot_roi.MARKER + json.dumps(record) for _, record in conversation(2 * HOUR, 0)]
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory, "trace.log")
            path.write_text("\n".join(lines) + "\n")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                snapshot_roi.main([str(path), "--upstreams", "1,2", "--horizons", "3600,inf", "--json"])
            text = io.StringIO()
            with contextlib.redirect_stdout(text):
                snapshot_roi.main([str(path), "--upstreams", "1,2"])
        result = json.loads(output.getvalue())
        self.assertEqual([item["horizon"] for item in result["horizons"]], ["1h", "inf", "1h", "inf"])
        self.assertIn("misses by idle gap", text.getvalue())

    def test_windows_and_horizons_parse(self):
        self.assertEqual(
            snapshot_roi.parse_window("2026-09-26T07:00:00..2026-09-26T07:13:00"),
            (1790406000000, 1790406780000),
        )
        self.assertEqual(snapshot_roi.parse_horizons("inf,60,60"), [60_000, None])
        self.assertEqual(snapshot_roi.horizon_label(86_400_000), "1d")


class JournalVersionDriftTest(unittest.TestCase):
    """Every journal consumer must accept the version the LB writes.

    On 2026-09-25 the LB moved to v12 and node06's installed archive collector
    rejected every record until it was reinstalled.
    """

    def test_consumers_accept_the_emitted_version(self):
        source = pathlib.Path(__file__).resolve().parent.parent / "src" / "journal.rs"
        version = int(re.search(r"const VERSION: u8 = (\d+);", source.read_text()).group(1))
        record = {"v": version, "event": "start", "seq": 1, "unix_ms": T0, "endpoint": "chat"}
        self.assertEqual(route_journal_archive.decode_record(json.dumps(record))["v"], version)
        self.assertEqual(len(list(route_replay.records([json.dumps(record)]))), 1)
        limit = {
            "policy_version": serving_cost_audit.OUTPUT_LIMIT_POLICY_VERSION,
            "requested_bucket": "4097_plus",
            "requested_source": "max_tokens",
            "effective_bucket": "4097_plus",
            "effective_source": "max_tokens",
            "mutation": "unchanged",
            "stream_mode": "streaming",
        }
        audited = serving_cost_audit.bounded_output_limit({**record, "output_limit": limit})
        self.assertEqual(audited["telemetry_state"], "valid")
        self.assertGreaterEqual(version, snapshot_roi.MIN_AGE_VERSION)


if __name__ == "__main__":
    unittest.main()
