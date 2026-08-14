#!/usr/bin/env python3
"""Fail closed while node06's cooling/AC operational moratorium is active."""

from __future__ import annotations

import json
import os
import pathlib
import re
import stat
import time


SCHEMA_VERSION = 1
MAX_AUTHORIZATION_BYTES = 4096
MAX_AUTHORIZATION_WINDOW_SECONDS = 2 * 60 * 60
ACKNOWLEDGEMENT = "AC_REPAIR_CONFIRMED_SUPERVISED_WINDOW"


class MoratoriumError(RuntimeError):
    pass


def require_supervised_authorization(
    path: pathlib.Path | None,
    operation: str,
    *,
    now: int | None = None,
) -> dict[str, object]:
    """Require a fresh, private authorization bound to one supervised action."""

    if path is None:
        raise MoratoriumError(
            "node06 cooling/AC moratorium is active; this operation requires a "
            "fresh supervised authorization file after the repair"
        )
    if re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,63}", operation) is None:
        raise MoratoriumError("supervised operation name is invalid")

    try:
        parent = path.parent
        parent_info = parent.lstat()
        if (
            not stat.S_ISDIR(parent_info.st_mode)
            or stat.S_ISLNK(parent_info.st_mode)
            or parent_info.st_uid != os.geteuid()
            or parent_info.st_mode & 0o077
        ):
            raise MoratoriumError(
                "supervised authorization parent must be owner-only"
            )
        flags = os.O_RDONLY | os.O_CLOEXEC
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(path, flags)
        try:
            info = os.fstat(descriptor)
            if (
                not stat.S_ISREG(info.st_mode)
                or info.st_uid != os.geteuid()
                or info.st_nlink != 1
                or info.st_mode & 0o177 != 0
                or not 1 <= info.st_size <= MAX_AUTHORIZATION_BYTES
            ):
                raise MoratoriumError(
                    "supervised authorization file must be private and regular"
                )
            raw = os.read(descriptor, MAX_AUTHORIZATION_BYTES + 1)
        finally:
            os.close(descriptor)
    except MoratoriumError:
        raise
    except OSError as error:
        raise MoratoriumError("supervised authorization file is unavailable") from error

    try:
        document = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise MoratoriumError("supervised authorization file is malformed") from error
    expected_keys = {
        "schema_version",
        "node",
        "operation",
        "issued_at_unix",
        "expires_at_unix",
        "nonce",
        "acknowledgement",
        "ac_repair_confirmed",
        "supervisor_present",
    }
    if not isinstance(document, dict) or set(document) != expected_keys:
        raise MoratoriumError("supervised authorization schema is invalid")

    current = int(time.time()) if now is None else now
    issued = document.get("issued_at_unix")
    expires = document.get("expires_at_unix")
    if (
        document.get("schema_version") != SCHEMA_VERSION
        or document.get("node") != "node06"
        or document.get("operation") != operation
        or document.get("acknowledgement") != ACKNOWLEDGEMENT
        or document.get("ac_repair_confirmed") is not True
        or document.get("supervisor_present") is not True
        or not isinstance(issued, int)
        or isinstance(issued, bool)
        or not isinstance(expires, int)
        or isinstance(expires, bool)
        or not current - 60 <= issued <= current
        or not issued < expires <= current + MAX_AUTHORIZATION_WINDOW_SECONDS
        or re.fullmatch(r"[0-9a-f]{32}", str(document.get("nonce", ""))) is None
    ):
        raise MoratoriumError(
            "supervised authorization is stale or does not match this operation"
        )
    return document
