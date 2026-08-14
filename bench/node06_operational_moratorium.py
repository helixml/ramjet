#!/usr/bin/env python3
"""Fail closed while node06's cooling/AC operational moratorium is active.

Lifted 2026-08-14 for one supervised, ramped, watchdog-aborted load-test
window. The operator confirmed the AC repair and that they are supervising the
complete startup, workload, and rollback interval, which is the authorization
the moratorium requires; this reviewed change is the second half of it.

RE-ARM THIS WHEN THE WINDOW CLOSES. The flag is global: while it is False every
node06 GPU operation is permitted for every caller, not just the run it was
lifted for. It is not a per-run token, so leaving it False silently converts a
one-off supervised authorization into standing permission.

Thermal context recorded at the moment of the lift, because it is the reason
the run is ramped rather than launched at full concurrency: all eight GPUs
idled at 57-62 C at 0% utilisation drawing ~100 W each. The guard's cool-start
gate is 65 C and its abort ceiling is 78 C, so the box was admitted with only a
few degrees of headroom. An abort is an expected, correct outcome here, not a
harness failure.
"""

from __future__ import annotations

import re


MORATORIUM_ACTIVE = False
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
