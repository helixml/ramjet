#!/usr/bin/env python3
"""Durably collect and summarize privacy-bounded ramjet route journals.

The collector reads only ``[route_journal]`` records from Docker logs. Records
are deduplicated transactionally by container ID, sequence, and event. Daily
maintenance writes immutable-shape gzip exports and compact JSON analyses, then
removes material older than the configured retention window.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import gzip
import hashlib
import json
import os
import pathlib
import re
import sqlite3
import stat
import subprocess
import sys
import tempfile
from typing import Iterable

import route_replay
import serving_cost_audit


MARKER = "[route_journal] "
DATABASE_NAME = "journal.sqlite3"
SCHEMA_VERSION = 1
MAX_LINE_BYTES = 4 * 1024 * 1024
MAX_RECORDS_PER_COLLECTION = 1_000_000
CONTAINER_ID_RE = re.compile(r"^[0-9a-f]{64}$")
DAY_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
START_FIELDS = {
    "v", "event", "seq", "unix_ms", "endpoint", "request_bytes", "total_blocks",
    "chosen", "served_chosen", "outcome", "rotation", "alpha", "max_affinity_blocks",
    "chunk_bytes", "load_unit_bytes", "max_load_units", "phase_aware_load",
    "decode_load_unit_tokens", "decode_max_load_units", "decode_load_units",
    "projected_load", "score_tie_break", "exact_canary", "session_affinity",
    "output_limit", "prefix_single_flight", "candidates",
}
FINISH_FIELDS = {
    "v", "event", "seq", "unix_ms", "result", "upstream", "request_load_units",
    "status", "duration_ms", "first_byte_ms", "ttft_ms", "response_bytes",
    "prompt_tokens", "cached_tokens", "completion_tokens",
}
NESTED_FIELDS = {
    "candidates": {
        "upstream", "rank", "overlap_blocks", "affinity_blocks", "load_units",
        "request_load_units", "healthy",
    },
    "session_affinity": {
        "policy_version", "bonus_blocks", "max_load_delta", "outcome", "primary",
        "secondary", "target",
    },
    "output_limit": {
        "policy_version", "requested_bucket", "requested_source", "effective_bucket",
        "effective_source", "mutation", "stream_mode",
    },
    "prefix_single_flight": {"mode", "outcome"},
}


class ArchiveError(RuntimeError):
    """A journal archive safety or collection contract failed."""


def utc_now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc)


def ensure_private_directory(path: pathlib.Path) -> None:
    if not path.is_absolute():
        raise ArchiveError("state directory must be absolute")
    try:
        info = path.lstat()
    except FileNotFoundError:
        path.mkdir(mode=0o700)
        info = path.lstat()
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise ArchiveError("state path must be a real directory")
    if info.st_uid != os.geteuid():
        raise ArchiveError("state directory must be owned by the service user")
    if stat.S_IMODE(info.st_mode) != 0o700:
        raise ArchiveError("state directory must have mode 0700")


def ensure_child_directory(parent: pathlib.Path, name: str) -> pathlib.Path:
    child = parent / name
    try:
        child.mkdir(mode=0o700)
    except FileExistsError:
        pass
    info = child.lstat()
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise ArchiveError(f"{name} must be a real directory")
    if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise ArchiveError(f"{name} must be owner-only mode 0700")
    return child


def connect_database(state_dir: pathlib.Path) -> sqlite3.Connection:
    ensure_private_directory(state_dir)
    database = state_dir / DATABASE_NAME
    if database.exists() or database.is_symlink():
        info = database.lstat()
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
            raise ArchiveError("journal database must be a regular file")
        if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o600:
            raise ArchiveError("journal database must be owner-only mode 0600")
    old_umask = os.umask(0o077)
    try:
        connection = sqlite3.connect(database)
    finally:
        os.umask(old_umask)
    os.chmod(database, 0o600)
    connection.execute("PRAGMA foreign_keys = ON")
    connection.execute("PRAGMA journal_mode = DELETE")
    connection.execute("PRAGMA synchronous = FULL")
    connection.executescript(
        """
        CREATE TABLE IF NOT EXISTS containers (
            container_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            image_id TEXT NOT NULL,
            image_ref TEXT NOT NULL,
            created TEXT NOT NULL,
            compose_files TEXT NOT NULL,
            first_seen_ms INTEGER NOT NULL,
            last_seen_ms INTEGER NOT NULL,
            last_source_timestamp TEXT
        );
        CREATE TABLE IF NOT EXISTS records (
            container_id TEXT NOT NULL REFERENCES containers(container_id),
            seq INTEGER NOT NULL,
            event TEXT NOT NULL CHECK (event IN ('start', 'finish')),
            source_timestamp TEXT NOT NULL,
            unix_ms INTEGER NOT NULL,
            record_json TEXT NOT NULL,
            PRIMARY KEY (container_id, seq, event)
        );
        CREATE INDEX IF NOT EXISTS records_time
            ON records(unix_ms, container_id, event);
        """
    )
    return connection


def docker_value(docker: str, container: str, template: str) -> str:
    result = subprocess.run(
        [docker, "inspect", "--format", template, container],
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise ArchiveError("docker inspect failed for the configured container")
    value = result.stdout.strip()
    if "\n" in value or "\x00" in value:
        raise ArchiveError("docker inspect returned an invalid metadata value")
    return value


def inspect_container(docker: str, container: str) -> dict[str, str]:
    metadata = {
        "container_id": docker_value(docker, container, "{{.Id}}"),
        "image_id": docker_value(docker, container, "{{.Image}}"),
        "image_ref": docker_value(docker, container, "{{.Config.Image}}"),
        "created": docker_value(docker, container, "{{.Created}}"),
        "compose_files": docker_value(
            docker,
            container,
            '{{index .Config.Labels "com.docker.compose.project.config_files"}}',
        ),
        "name": container,
    }
    if not CONTAINER_ID_RE.fullmatch(metadata["container_id"]):
        raise ArchiveError("docker returned an invalid container ID")
    if not metadata["image_id"].startswith("sha256:"):
        raise ArchiveError("docker returned an invalid image ID")
    return metadata


def decode_record(payload: str) -> dict:
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ArchiveError("route-journal record contains duplicate fields")
            result[key] = value
        return result

    try:
        record = json.loads(payload, object_pairs_hook=unique_object)
    except json.JSONDecodeError as error:
        raise ArchiveError("route-journal record is invalid JSON") from error
    if not isinstance(record, dict):
        raise ArchiveError("route-journal record must be an object")
    version = record.get("v")
    seq = record.get("seq")
    event = record.get("event")
    unix_ms = record.get("unix_ms")
    if not isinstance(version, int) or isinstance(version, bool) or not 1 <= version <= 10:
        raise ArchiveError("route-journal version is unsupported")
    if not isinstance(seq, int) or isinstance(seq, bool) or seq <= 0:
        raise ArchiveError("route-journal sequence is invalid")
    if event not in {"start", "finish"}:
        raise ArchiveError("route-journal event is invalid")
    if not isinstance(unix_ms, int) or isinstance(unix_ms, bool) or unix_ms <= 0:
        raise ArchiveError("route-journal timestamp is invalid")
    allowed = START_FIELDS if event == "start" else FINISH_FIELDS
    if not set(record).issubset(allowed):
        raise ArchiveError("route-journal record contains an unapproved field")
    for field, nested_allowed in NESTED_FIELDS.items():
        value = record.get(field)
        if value is None:
            continue
        values = value if field == "candidates" else [value]
        if not isinstance(values, list) or any(
            not isinstance(item, dict) or not set(item).issubset(nested_allowed)
            for item in values
        ):
            raise ArchiveError(f"route-journal {field} contains an unapproved field")
    return record


def parse_docker_lines(lines: Iterable[bytes]) -> tuple[list[tuple[str, dict]], str | None]:
    records: list[tuple[str, dict]] = []
    last_timestamp = None
    for raw in lines:
        if len(raw) > MAX_LINE_BYTES:
            raise ArchiveError("docker log line exceeds the safety bound")
        try:
            line = raw.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ArchiveError("docker logs are not valid UTF-8") from error
        timestamp, separator, message = line.rstrip("\n").partition(" ")
        if not separator or "T" not in timestamp:
            continue
        last_timestamp = timestamp
        marker = message.find(MARKER)
        if marker < 0:
            continue
        record = decode_record(message[marker + len(MARKER) :].strip())
        records.append((timestamp, record))
        if len(records) > MAX_RECORDS_PER_COLLECTION:
            raise ArchiveError("collection exceeds the per-run record bound")
    return records, last_timestamp


def read_docker_logs(docker: str, container: str, since: str | None) -> tuple[list, str | None]:
    command = [docker, "logs", "--timestamps"]
    if since:
        command.extend(["--since", since])
    command.append(container)
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    assert process.stdout is not None
    try:
        records, last_timestamp = parse_docker_lines(process.stdout)
    finally:
        process.stdout.close()
    returncode = process.wait(timeout=60)
    if returncode != 0:
        raise ArchiveError("docker logs failed for the configured container")
    return records, last_timestamp


def store_collection(
    connection: sqlite3.Connection,
    metadata: dict[str, str],
    records: list[tuple[str, dict]],
    last_timestamp: str | None,
    now: dt.datetime,
) -> tuple[int, int]:
    now_ms = int(now.timestamp() * 1000)
    container_id = metadata["container_id"]
    with connection:
        connection.execute(
            """
            INSERT INTO containers (
                container_id, name, image_id, image_ref, created, compose_files,
                first_seen_ms, last_seen_ms, last_source_timestamp
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)
            ON CONFLICT(container_id) DO UPDATE SET
                name=excluded.name, image_id=excluded.image_id,
                image_ref=excluded.image_ref, created=excluded.created,
                compose_files=excluded.compose_files,
                last_seen_ms=excluded.last_seen_ms
            """,
            (
                container_id,
                metadata["name"],
                metadata["image_id"],
                metadata["image_ref"],
                metadata["created"],
                metadata["compose_files"],
                now_ms,
                now_ms,
            ),
        )
        inserted = 0
        for source_timestamp, record in records:
            cursor = connection.execute(
                """
                INSERT OR IGNORE INTO records
                    (container_id, seq, event, source_timestamp, unix_ms, record_json)
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                (
                    container_id,
                    record["seq"],
                    record["event"],
                    source_timestamp,
                    record["unix_ms"],
                    json.dumps(record, separators=(",", ":"), sort_keys=True),
                ),
            )
            inserted += cursor.rowcount
        if last_timestamp:
            connection.execute(
                "UPDATE containers SET last_source_timestamp=? WHERE container_id=?",
                (last_timestamp, container_id),
            )
    return inserted, len(records) - inserted


def collect(args: argparse.Namespace) -> dict:
    state_dir = pathlib.Path(args.state_dir)
    connection = connect_database(state_dir)
    try:
        metadata = inspect_container(args.docker, args.container)
        row = connection.execute(
            "SELECT last_source_timestamp FROM containers WHERE container_id=?",
            (metadata["container_id"],),
        ).fetchone()
        since = row[0] if row else None
        records, last_timestamp = read_docker_logs(args.docker, args.container, since)
        inserted, duplicates = store_collection(
            connection, metadata, records, last_timestamp, utc_now()
        )
        return {
            "schema_version": SCHEMA_VERSION,
            "container_id": metadata["container_id"][:12],
            "inserted": inserted,
            "duplicates": duplicates,
            "cursor_advanced": last_timestamp is not None,
        }
    finally:
        connection.close()


def parse_day(raw: str, now: dt.datetime | None = None) -> dt.date:
    today = (now or utc_now()).date()
    if raw == "today":
        return today
    if raw == "yesterday":
        return today - dt.timedelta(days=1)
    if not DAY_RE.fullmatch(raw):
        raise ArchiveError("day must be today, yesterday, or YYYY-MM-DD")
    try:
        return dt.date.fromisoformat(raw)
    except ValueError as error:
        raise ArchiveError("day is not a valid calendar date") from error


def records_for_day(
    connection: sqlite3.Connection, container_id: str, day: dt.date
) -> list[dict]:
    start = int(dt.datetime.combine(day, dt.time(), dt.timezone.utc).timestamp() * 1000)
    end = start + 86_400_000
    rows = connection.execute(
        """
        SELECT event, seq, record_json FROM records
        WHERE container_id=? AND (
            (event='start' AND unix_ms>=? AND unix_ms<?) OR
            (event='finish' AND seq IN (
                SELECT seq FROM records
                WHERE container_id=? AND event='start' AND unix_ms>=? AND unix_ms<?
            ))
        )
        ORDER BY seq, CASE event WHEN 'start' THEN 0 ELSE 1 END
        """,
        (container_id, start, end, container_id, start, end),
    ).fetchall()
    return [json.loads(row[2]) for row in rows]


def atomic_bytes(path: pathlib.Path, payload_writer) -> None:
    descriptor, raw_path = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = pathlib.Path(raw_path)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb", closefd=True) as output:
            payload_writer(output)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            temporary.unlink()
        raise


def write_segment(path: pathlib.Path, records: list[dict]) -> tuple[int, str]:
    def writer(output) -> None:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            for record in records:
                encoded = json.dumps(record, separators=(",", ":"), sort_keys=True).encode()
                compressed.write(encoded + b"\n")

    atomic_bytes(path, writer)
    payload = path.read_bytes()
    return len(payload), hashlib.sha256(payload).hexdigest()


def compact_analysis(records: list[dict]) -> dict:
    audit = serving_cost_audit.audit(records, None, None, None)
    starts = [record for record in records if record["event"] == "start"]
    finishes = {record["seq"]: record for record in records if record["event"] == "finish"}
    replay = route_replay.replay(
        starts,
        finishes,
        [1.0, 2.0, 4.0, 8.0],
        [8, 16, 32, 64],
        None,
        None,
        None,
        None,
    )
    return {
        "records": audit["records"],
        "overall": audit["overall"],
        "by_cache_outcome": audit["by_cache_outcome"],
        "route_counterfactuals": replay,
    }


def maintain(args: argparse.Namespace) -> dict:
    state_dir = pathlib.Path(args.state_dir)
    connection = connect_database(state_dir)
    day = parse_day(args.day)
    segments = ensure_child_directory(state_dir, "segments")
    reports = ensure_child_directory(state_dir, "reports")
    day_segments = ensure_child_directory(segments, day.isoformat())
    result = {
        "schema_version": SCHEMA_VERSION,
        "day": day.isoformat(),
        "generated_unix_ms": int(utc_now().timestamp() * 1000),
        "containers": [],
    }
    try:
        container_rows = connection.execute(
            """
            SELECT DISTINCT c.container_id, c.image_id, c.image_ref, c.created, c.compose_files
            FROM containers c JOIN records r ON r.container_id=c.container_id
            WHERE r.event='start' AND r.unix_ms>=? AND r.unix_ms<?
            ORDER BY c.container_id
            """,
            (
                int(dt.datetime.combine(day, dt.time(), dt.timezone.utc).timestamp() * 1000),
                int(
                    dt.datetime.combine(
                        day + dt.timedelta(days=1), dt.time(), dt.timezone.utc
                    ).timestamp()
                    * 1000
                ),
            ),
        ).fetchall()
        for container_id, image_id, image_ref, created, compose_files in container_rows:
            records = records_for_day(connection, container_id, day)
            segment = day_segments / f"{container_id}.jsonl.gz"
            size, digest = write_segment(segment, records)
            entry = {
                "container_id": container_id,
                "image_id": image_id,
                "image_ref": image_ref,
                "created": created,
                "compose_files": compose_files,
                "segment": str(segment.relative_to(state_dir)),
                "segment_bytes": size,
                "segment_sha256": digest,
            }
            entry.update(compact_analysis(records))
            result["containers"].append(entry)

        report = reports / f"{day.isoformat()}.json"
        atomic_bytes(
            report,
            lambda output: output.write(
                (json.dumps(result, sort_keys=True, indent=2) + "\n").encode()
            ),
        )
        pruned = prune(connection, state_dir, args.retention_days, utc_now().date())
        result["report"] = str(report)
        result["pruned"] = pruned
        return result
    finally:
        connection.close()


def prune(
    connection: sqlite3.Connection,
    state_dir: pathlib.Path,
    retention_days: int,
    today: dt.date,
) -> dict[str, int]:
    if not 1 <= retention_days <= 3650:
        raise ArchiveError("retention days must be between 1 and 3650")
    cutoff = today - dt.timedelta(days=retention_days)
    cutoff_ms = int(dt.datetime.combine(cutoff, dt.time(), dt.timezone.utc).timestamp() * 1000)
    with connection:
        deleted_records = connection.execute(
            "DELETE FROM records WHERE unix_ms<?", (cutoff_ms,)
        ).rowcount
        connection.execute(
            """DELETE FROM containers
            WHERE container_id NOT IN (SELECT DISTINCT container_id FROM records)"""
        )
    deleted_files = 0
    segments = state_dir / "segments"
    if segments.is_dir():
        for child in segments.iterdir():
            if not DAY_RE.fullmatch(child.name) or not child.is_dir() or child.is_symlink():
                continue
            if dt.date.fromisoformat(child.name) >= cutoff:
                continue
            for item in child.iterdir():
                if item.is_file() and not item.is_symlink() and item.name.endswith(".jsonl.gz"):
                    item.unlink()
                    deleted_files += 1
            with contextlib.suppress(OSError):
                child.rmdir()
    reports = state_dir / "reports"
    if reports.is_dir():
        for item in reports.iterdir():
            stem = item.name.removesuffix(".json")
            if (
                DAY_RE.fullmatch(stem)
                and item.is_file()
                and not item.is_symlink()
                and dt.date.fromisoformat(stem) < cutoff
            ):
                item.unlink()
                deleted_files += 1
    return {"records": deleted_records, "files": deleted_files}


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", default="/var/lib/ramjet-journal")
    subparsers = parser.add_subparsers(dest="command", required=True)
    collect_parser = subparsers.add_parser("collect")
    collect_parser.add_argument("--container", default="ds4-loadbalancer")
    collect_parser.add_argument("--docker", default="/usr/bin/docker")
    maintain_parser = subparsers.add_parser("maintain")
    maintain_parser.add_argument("--day", default="yesterday")
    maintain_parser.add_argument("--retention-days", type=int, default=30)
    return parser


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    try:
        result = collect(args) if args.command == "collect" else maintain(args)
    except (ArchiveError, OSError, sqlite3.Error, subprocess.SubprocessError) as error:
        print(f"route-journal archive failed: {error}", file=sys.stderr)
        return 1
    if args.command == "maintain":
        result = {
            "schema_version": result["schema_version"],
            "day": result["day"],
            "containers": len(result["containers"]),
            "report": result["report"],
            "pruned": result["pruned"],
        }
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
