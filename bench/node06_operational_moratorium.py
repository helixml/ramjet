#!/usr/bin/env python3
"""Compatibility gate for node06's retired cooling/AC moratorium.

RETIRED 2026-08-25 after the operator confirmed the AC repair was complete and
explicitly authorized the supervised BF16-lm_head model rollout. The intake-air
thermal watchdog, bounded runtime, deployment lock, and rollout qualification
remain mandatory; only the separate static stop is gone.

RE-ARMED 2026-08-14 after a second supervised window: a clean repeat of the rc6
c24/max256 code gate, authorized with an explicit instruction to abort if the
box got too hot. It did, and the guard aborted correctly -- see below.

The first window is described after it and remains the basis for sizing.
For that window the operator confirmed the AC repair and that they were
supervising the complete startup, workload, and rollback interval, which is the
authorization the moratorium requires; a reviewed change was the second half.

The compatibility flag remains so an incident can re-arm a global stop without
changing every caller. While it is False, callers proceed to their ordinary
thermal, duration, identity, and ownership gates.

What that window measured, since it is the basis for sizing the next one. The
box was admitted with little headroom: all eight GPUs idled at 57-62 C at 0%
utilisation drawing ~100 W each, against a 65 C cool-start gate and a 78 C
abort ceiling. A ramped c4/c8/c16 plus c24 aggregate run then completed with
zero failed requests and no thermal abort. Peak GPU was 72 C and peak box draw
2854 W, with no throttling at any point (clocks held 2422-2430 MHz, throttle
reasons 0x0). The idle baseline drifted up from 59-63 C to 61-67 C across the
ramp and did not fully recover between steps, which is why the next window
should still start ramped rather than at full concurrency.

Second window, and the reason the ceiling is real. The 72-request rc6 repeat
matched the recorded gate: 1,863.3 tok/s median aggregate against 1,891.2, and
123.9 tok/s median per-stream decode against 125.0, peaking at 72 C. Extending
the SAME workload to 216 requests then drove GPU1 from 65 C to the 78 C abort
in about seventeen seconds and the guard terminated it. Short cells stay near
72 C only because they are short; this box cannot hold c24 for more than
roughly fifteen to twenty seconds of sustained decode.

GPU1 is the constraint, not the box average. It runs about 5 C hotter than its
neighbours both at idle and under load (per-GPU peaks 73/78/71/73/75/74/74/73),
which points at airflow on that specific card rather than ambient cooling.
Serving was unaffected by the abort: both engines stayed up with zero
CUDA/NCCL/Xid errors and an authenticated request returned 200 immediately.
"""

from __future__ import annotations

import re


MORATORIUM_ACTIVE = False
MORATORIUM_REASON = "cooling_ac_failure_2026_08_14"

# Retained for compatibility with historical, detached commands. It has no
# effect while the global moratorium is retired.
ENV_AUTHORIZATION = "RAMJET_NODE06_AUTHORIZATION"


class AuthorizedWindow:
    """A reviewed, bounded exception to the moratorium."""

    __slots__ = ("identifier", "granted", "max_abort_c", "max_runtime_seconds")

    def __init__(self, identifier, granted, max_abort_c, max_runtime_seconds):
        self.identifier = identifier
        self.granted = granted
        self.max_abort_c = max_abort_c
        self.max_runtime_seconds = max_runtime_seconds


# Granted 2026-08-14 by explicit user authorization for supervised work on an
# otherwise-idle node06 serving no production traffic.
#
# The bounds below remain historical evidence. The evidence in this module's
# docstring still stands: GPU1 runs about 5C hotter than its neighbours and
# previously went from 65C to an abort in roughly seventeen seconds of
# sustained c24 decode, so 25 minutes is a ceiling rather than an expectation.
#
# The bound is now intake-air temperature, not GPU temperature. A GPU defends
# itself -- these devices throttle at 85C and the driver cuts power at 90C --
# so gating on silicon mostly re-implements the hardware. Facility cooling has
# no such backstop and is shared between hosts, so it is the failure that takes
# out more than one run. The current operator ceiling is 50C on the same intake
# sensors Grafana's bunker-temps dashboard plots.
AUTHORIZED_WINDOWS = {
    "supervised-2026-08-14": AuthorizedWindow(
        identifier="supervised-2026-08-14",
        granted="2026-08-14",
        max_abort_c=50,
        max_runtime_seconds=1500,
    ),
}


def active_authorization(environ=None):
    """Returns the reviewed window named by the environment, or None."""

    import os

    source = os.environ if environ is None else environ
    name = source.get(ENV_AUTHORIZATION, "")
    return AUTHORIZED_WINDOWS.get(name)


class MoratoriumError(RuntimeError):
    pass


def require_active_work_permitted(operation: str) -> None:
    """Reject malformed names, and all GPU/load work if the stop is re-armed."""

    if re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,95}", operation) is None:
        raise MoratoriumError("node06 operation name is invalid")
    if MORATORIUM_ACTIVE and active_authorization() is None:
        raise MoratoriumError(
            "node06 cooling/AC moratorium is active; explicit supervised "
            "authorization and a reviewed repository change are required"
        )
