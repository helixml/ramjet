#!/usr/bin/env python3
"""Create or validate the fixed host authority for durable DSpark quarantine."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import stat
import sys
from typing import Final


STATE_DIR: Final = pathlib.Path("/run/ramjet-dspark-guard")
STATE_FILE: Final = STATE_DIR / "state.json"
EMPTY_STATE: Final = b'{"schema_version":1,"runtime_dirty":false,"quarantines":[]}'


class SetupError(RuntimeError):
    """A fixed, operator-actionable host-policy failure."""


def _validate_directory(
    directory: pathlib.Path, *, owner_uid: int, group_gid: int
) -> None:
    directory_metadata = directory.lstat()
    if (
        not stat.S_ISDIR(directory_metadata.st_mode)
        or stat.S_IMODE(directory_metadata.st_mode) != 0o700
        or directory_metadata.st_uid != owner_uid
        or directory_metadata.st_gid != group_gid
    ):
        raise SetupError("DSpark guard authority directory is unsafe")


def _mount_type(path: pathlib.Path) -> str | None:
    resolved = path.resolve(strict=True)
    best: tuple[int, str] | None = None
    for raw in pathlib.Path("/proc/self/mountinfo").read_text().splitlines():
        left, separator, right = raw.partition(" - ")
        if not separator:
            continue
        fields = left.split()
        filesystem = right.split()
        if len(fields) < 5 or not filesystem:
            continue
        mountpoint = pathlib.Path(fields[4].replace("\\040", " "))
        try:
            resolved.relative_to(mountpoint)
        except ValueError:
            continue
        candidate = (len(mountpoint.parts), filesystem[0])
        if best is None or candidate[0] > best[0]:
            best = candidate
    return None if best is None else best[1]


def validate(
    directory: pathlib.Path,
    state_file: pathlib.Path,
    *,
    require_tmpfs: bool,
    owner_uid: int = 0,
    group_gid: int = 0,
) -> None:
    if not directory.is_absolute() or state_file.parent != directory:
        raise SetupError("DSpark guard authority path is invalid")
    if require_tmpfs and _mount_type(directory) != "tmpfs":
        raise SetupError("DSpark guard authority is not on tmpfs")
    _validate_directory(directory, owner_uid=owner_uid, group_gid=group_gid)
    state_metadata = state_file.lstat()
    if (
        not stat.S_ISREG(state_metadata.st_mode)
        or state_metadata.st_nlink != 1
        or stat.S_IMODE(state_metadata.st_mode) != 0o600
        or state_metadata.st_uid != owner_uid
        or state_metadata.st_gid != group_gid
    ):
        raise SetupError("DSpark guard authority file is unsafe")
    raw = state_file.read_bytes()
    if len(raw) > 4096:
        raise SetupError("DSpark guard authority document is oversized")
    try:
        document = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SetupError("DSpark guard authority document is malformed") from error
    if not isinstance(document, dict) or set(document) != {
        "schema_version",
        "runtime_dirty",
        "quarantines",
    }:
        raise SetupError("DSpark guard authority schema is invalid")
    if (
        document["schema_version"] != 1
        or type(document["runtime_dirty"]) is not bool
        or not isinstance(document["quarantines"], list)
    ):
        raise SetupError("DSpark guard authority schema is unsupported")
    previous_replica = -1
    engine_cores: set[str] = set()
    for record in document["quarantines"]:
        if not isinstance(record, dict) or set(record) != {
            "replica",
            "upstream_sha256",
            "engine_core_sha256",
        }:
            raise SetupError("DSpark guard authority record is invalid")
        replica = record["replica"]
        if type(replica) is not int or not previous_replica < replica < 64:
            raise SetupError("DSpark guard authority record order is invalid")
        previous_replica = replica
        for key in ("upstream_sha256", "engine_core_sha256"):
            digest = record[key]
            if (
                not isinstance(digest, str)
                or len(digest) != 64
                or any(character not in "0123456789abcdef" for character in digest)
            ):
                raise SetupError("DSpark guard authority commitment is invalid")
        if record["engine_core_sha256"] in engine_cores:
            raise SetupError("DSpark guard authority EngineCore is duplicated")
        engine_cores.add(record["engine_core_sha256"])
    canonical = json.dumps(document, separators=(",", ":")).encode()
    if canonical != raw:
        raise SetupError("DSpark guard authority document is not canonical")


def create() -> None:
    if os.geteuid() != 0 or os.getegid() != 0:
        raise SetupError("DSpark guard host setup requires root")
    if _mount_type(STATE_DIR.parent) != "tmpfs":
        raise SetupError("/run is not backed by tmpfs")
    try:
        STATE_DIR.mkdir(mode=0o700)
    except FileExistsError:
        pass
    _validate_directory(STATE_DIR, owner_uid=0, group_gid=0)
    directory_fd = os.open(STATE_DIR, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        try:
            state_fd = os.open(
                STATE_FILE.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                0o600,
                dir_fd=directory_fd,
            )
        except FileExistsError:
            state_fd = -1
        if state_fd >= 0:
            try:
                remaining = memoryview(EMPTY_STATE)
                while remaining:
                    written = os.write(state_fd, remaining)
                    if written <= 0:
                        raise SetupError("DSpark guard state write made no progress")
                    remaining = remaining[written:]
                os.fsync(state_fd)
            finally:
                os.close(state_fd)
            os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
    validate(STATE_DIR, STATE_FILE, require_tmpfs=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="validate without mutation")
    arguments = parser.parse_args()
    try:
        if arguments.check:
            validate(STATE_DIR, STATE_FILE, require_tmpfs=True)
        else:
            create()
    except (OSError, SetupError) as error:
        print(str(error), file=sys.stderr)
        return 1
    print("DSpark guard host authority is ready")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
