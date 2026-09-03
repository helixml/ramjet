import argparse
import datetime as dt
import gzip
import json
import os
import pathlib
import tempfile
import unittest

import route_journal_archive as archive


def start(sequence, unix_ms):
    return {
        "v": 5,
        "event": "start",
        "seq": sequence,
        "unix_ms": unix_ms,
        "endpoint": "chat",
        "request_bytes": 4096,
        "chosen": 0,
        "served_chosen": 0,
        "rotation": 0,
        "candidates": [
            {
                "upstream": 0,
                "rank": 0,
                "overlap_blocks": 8,
                "affinity_blocks": 8,
                "load_units": 0,
                "request_load_units": 1,
                "healthy": True,
            }
        ],
    }


def finish(sequence, unix_ms):
    return {
        "v": 5,
        "event": "finish",
        "seq": sequence,
        "unix_ms": unix_ms,
        "result": "complete",
        "upstream": 0,
        "request_load_units": 1,
        "status": 200,
        "duration_ms": 150.0,
        "ttft_ms": 100.0,
        "response_bytes": 100,
        "prompt_tokens": 100.0,
        "cached_tokens": 75.0,
        "completion_tokens": 2.0,
    }


class RouteJournalArchiveTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.state = pathlib.Path(self.temporary.name) / "state"
        self.state.mkdir(mode=0o700)
        self.container_id = "a" * 64
        self.metadata = {
            "container_id": self.container_id,
            "name": "ds4-loadbalancer",
            "image_id": "sha256:" + "b" * 64,
            "image_ref": "ghcr.io/helixml/ramjet:test",
            "created": "2026-09-02T00:00:00Z",
            "compose_files": "/srv/compose.yaml",
        }

    def tearDown(self):
        self.temporary.cleanup()

    def test_parser_ignores_other_logs_and_rejects_bad_journal(self):
        record = start(1, 1_788_307_200_000)
        lines = [
            b"2026-09-02T00:00:00.000000000Z ordinary log\n",
            (
                "2026-09-02T00:00:01.000000000Z "
                + archive.MARKER
                + json.dumps(record)
                + "\n"
            ).encode(),
        ]
        parsed, cursor = archive.parse_docker_lines(lines)
        self.assertEqual(parsed[0][1], record)
        self.assertEqual(cursor, "2026-09-02T00:00:01.000000000Z")
        with self.assertRaisesRegex(archive.ArchiveError, "invalid JSON"):
            archive.parse_docker_lines(
                [b"2026-09-02T00:00:00Z [route_journal] {broken}\n"]
            )
        record["prompt"] = "must never persist"
        with self.assertRaisesRegex(archive.ArchiveError, "unapproved field"):
            archive.decode_record(json.dumps(record))
        with self.assertRaisesRegex(archive.ArchiveError, "duplicate fields"):
            archive.decode_record('{"v":5,"v":5,"event":"start","seq":1,"unix_ms":1}')

    def test_transactional_primary_key_deduplicates_collection(self):
        connection = archive.connect_database(self.state)
        moment = dt.datetime(2026, 9, 2, tzinfo=dt.timezone.utc)
        records = [
            ("2026-09-02T00:00:00.000000000Z", start(1, 1_788_307_200_000)),
            ("2026-09-02T00:00:01.000000000Z", finish(1, 1_788_307_201_000)),
        ]
        self.assertEqual(
            archive.store_collection(connection, self.metadata, records, records[-1][0], moment),
            (2, 0),
        )
        self.assertEqual(
            archive.store_collection(connection, self.metadata, records, records[-1][0], moment),
            (0, 2),
        )
        self.assertEqual(connection.execute("SELECT count(*) FROM records").fetchone()[0], 2)
        connection.close()
        self.assertEqual((self.state / archive.DATABASE_NAME).stat().st_mode & 0o777, 0o600)

    def test_maintenance_writes_private_segment_and_compact_report(self):
        connection = archive.connect_database(self.state)
        moment = dt.datetime(2026, 9, 2, tzinfo=dt.timezone.utc)
        records = [
            ("2026-09-02T00:00:00.000000000Z", start(1, 1_788_307_200_000)),
            ("2026-09-02T00:00:01.000000000Z", finish(1, 1_788_307_201_000)),
        ]
        archive.store_collection(connection, self.metadata, records, records[-1][0], moment)
        connection.close()
        result = archive.maintain(
            argparse.Namespace(
                state_dir=str(self.state), day="2026-09-02", retention_days=30
            )
        )
        self.assertEqual(len(result["containers"]), 1)
        segment = self.state / result["containers"][0]["segment"]
        self.assertEqual(segment.stat().st_mode & 0o777, 0o600)
        with gzip.open(segment, "rt", encoding="utf-8") as source:
            exported = [json.loads(line) for line in source]
        self.assertEqual([item["event"] for item in exported], ["start", "finish"])
        report = self.state / "reports" / "2026-09-02.json"
        self.assertEqual(report.stat().st_mode & 0o777, 0o600)
        payload = json.loads(report.read_text())
        self.assertEqual(payload["containers"][0]["overall"]["cache_hit_pct"], 75.0)

    def test_private_directory_contract_rejects_writable_or_symlinked_state(self):
        os.chmod(self.state, 0o755)
        with self.assertRaisesRegex(archive.ArchiveError, "mode 0700"):
            archive.connect_database(self.state)
        target = pathlib.Path(self.temporary.name) / "target"
        target.mkdir(mode=0o700)
        link = pathlib.Path(self.temporary.name) / "link"
        link.symlink_to(target, target_is_directory=True)
        with self.assertRaisesRegex(archive.ArchiveError, "real directory"):
            archive.connect_database(link)


if __name__ == "__main__":
    unittest.main()
