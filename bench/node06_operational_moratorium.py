#!/usr/bin/env python3
"""Fail closed while node06's cooling/AC operational moratorium is active.

RE-ARMED 2026-08-14, after the one supervised window it was lifted for closed.
For that window the operator confirmed the AC repair and that they were
supervising the complete startup, workload, and rollback interval, which is the
authorization the moratorium requires; a reviewed change was the second half.

The flag is global: while it is False every node06 GPU operation is permitted
for every caller, not just the run it was lifted for. It is not a per-run
token, so leaving it False would silently convert a one-off supervised
authorization into standing permission. Any future run needs its own explicit
authorization and its own reviewed change.

What that window measured, since it is the basis for sizing the next one. The
box was admitted with little headroom: all eight GPUs idled at 57-62 C at 0%
utilisation drawing ~100 W each, against a 65 C cool-start gate and a 78 C
abort ceiling. A ramped c4/c8/c16 plus c24 aggregate run then completed with
zero failed requests and no thermal abort. Peak GPU was 72 C and peak box draw
2854 W, with no throttling at any point (clocks held 2422-2430 MHz, throttle
reasons 0x0). The idle baseline drifted up from 59-63 C to 61-67 C across the
ramp and did not fully recover between steps, which is why the next window
should still start ramped rather than at full concurrency.
"""

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
