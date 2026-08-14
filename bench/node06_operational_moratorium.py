#!/usr/bin/env python3
"""Fail closed while node06's cooling/AC operational moratorium is active."""

from __future__ import annotations

import re


MORATORIUM_ACTIVE = True
MORATORIUM_REASON = "cooling_ac_failure_2026_08_14"


class MoratoriumError(RuntimeError):
    pass


def require_active_work_permitted(operation: str) -> None:
    """Reject every GPU/load operation until a reviewed change lifts the stop."""

    if re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,95}", operation) is None:
        raise MoratoriumError("node06 operation name is invalid")
    if MORATORIUM_ACTIVE:
        raise MoratoriumError(
            "node06 cooling/AC moratorium is active; explicit supervised "
            "authorization and a reviewed repository change are required"
        )
