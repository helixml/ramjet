#!/usr/bin/env python3
"""Run one node06 GPU experiment behind a fail-closed thermal watchdog.

The wrapper never logs the child command or environment. It preflights every
GPU, waits a bounded time for a cool start, samples temperature/load/power while
the child runs, and terminates the complete descendant tree if telemetry is
lost or the operational temperature ceiling is reached.
"""

from __future__ import annotations

import argparse
import csv
import ctypes
import dataclasses
import datetime
import fcntl
import hashlib
import hmac
import json
import math
import os
import pathlib
import re
import secrets
import signal
import stat
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from typing import Callable

from node06_operational_moratorium import (
    MoratoriumError,
    require_active_work_permitted,
)


# Thermal policy. The gate is chassis intake-air temperature, the same signal
# Grafana's bunker-temps dashboard plots, not GPU or CPU temperature.
#
# A GPU defends itself: node06's devices throttle at 85C and the driver cuts
# power at 90C, so gating on silicon largely re-implements the hardware. Room
# cooling has no such backstop, it is shared between hosts, and it is the
# failure that takes out more than one run. Measured 2026-08-14: a c64
# shared-prefix run drove GPUs to 79C while intake air did not move from 43C,
# so the old GPU gate was aborting work that carried no facility risk.
#
# The 2026-08-27 operator policy uses a ten-degree hysteresis band: stop
# request-generating work at 50C, then admit a new workload only after intake
# returns to 40C or below. For scale, node06's FP_TEMP was still 50C with every
# GPU idle, so preflight waits rather than treating the upper threshold as a
# reason to launch or spin up more work.
DEFAULT_ABORT_C = 50
MAX_ABORT_C = 50
DEFAULT_START_MAX_C = 40
DEFAULT_AIR_METRICS_URL = "http://127.0.0.1:9100/metrics"

# Continuous inference cap. Independent of temperature: it bounds how long a
# single guarded workload may generate load even if it never approaches the
# thermal ceiling.
DEFAULT_MAX_RUNTIME_SECONDS = 1500
MAX_RUNTIME_SECONDS = 1500

SCHEMA_VERSION = 1
DEFAULT_NVIDIA_SMI = "/usr/bin/nvidia-smi"
QUERY_FIELDS = (
    "index",
    "uuid",
    "name",
    "temperature.gpu",
    "power.draw",
    "power.limit",
    "utilization.gpu",
    "utilization.memory",
    "memory.used",
    "memory.total",
)
EXIT_TELEMETRY = 74
# Consecutive missed telemetry samples tolerated before the interval fails
# closed. At the 1Hz default poll this bounds the blind window to ~3s,
# far below any plausible thermal excursion, while absorbing the driver
# stalls that made single-miss aborts unavoidable on node06's 8-GPU box.
MAX_CONSECUTIVE_TELEMETRY_FAILURES = 3
EXIT_THERMAL = 75
EXIT_INTERNAL = 76
EXIT_SHIM_ESCALATED = 77
EXIT_SHIM_ORPHAN = 78
EXIT_RUNTIME_LIMIT = 79
PR_SET_CHILD_SUBREAPER = 36
PR_GET_CHILD_SUBREAPER = 37
PR_SET_PDEATHSIG = 1
CAPABILITY_BYTES = 32
CAPABILITY_SEALS = (
    fcntl.F_SEAL_SEAL | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_GROW | fcntl.F_SEAL_WRITE
)
MAX_JOURNAL_BYTES = 4 << 20
CHECKPOINT_SECONDS = 5


class GuardError(RuntimeError):
    pass


class GuardInterrupted(GuardError):
    def __init__(self, signum: int):
        super().__init__("GPU guard was interrupted")
        self.signum = signum


@dataclasses.dataclass
class GuardCapability:
    descriptor: int
    environment: dict[str, str]

    def close(self) -> None:
        if self.descriptor >= 0:
            os.close(self.descriptor)
            self.descriptor = -1


def create_guard_capability(
    expected_gpus: int,
    abort_c: float,
    run_id: str,
    parent_pid: int | None = None,
) -> GuardCapability:
    if not hasattr(os, "memfd_create"):
        raise GuardError("inherited guard capability is unavailable")
    try:
        descriptor = os.memfd_create(
            "ramjet-gpu-guard",
            os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING,
        )
        payload = secrets.token_bytes(CAPABILITY_BYTES)
        written = 0
        while written < len(payload):
            count = os.write(descriptor, payload[written:])
            if count <= 0:
                raise GuardError("inherited guard capability write failed")
            written += count
        fcntl.fcntl(descriptor, fcntl.F_ADD_SEALS, CAPABILITY_SEALS)
    except (AttributeError, OSError):
        if "descriptor" in locals():
            os.close(descriptor)
        raise GuardError("inherited guard capability is unavailable") from None
    environment = {
        "RAMJET_GPU_GUARD_ACTIVE": "1",
        "RAMJET_GPU_GUARD_EXPECTED_GPUS": str(expected_gpus),
        "RAMJET_GPU_GUARD_ABORT_C": str(abort_c),
        "RAMJET_GPU_GUARD_RUN_ID": run_id,
        "RAMJET_GPU_GUARD_PARENT_PID": str(parent_pid or os.getpid()),
        "RAMJET_GPU_GUARD_CAPABILITY_FD": str(descriptor),
        "RAMJET_GPU_GUARD_CAPABILITY_SHA256": hashlib.sha256(
            payload
        ).hexdigest(),
    }
    return GuardCapability(descriptor, environment)


def validate_inherited_guard(
    expected_gpus: int = 8, maximum_abort_c: float = MAX_ABORT_C
) -> dict[str, object]:
    try:
        active = os.environ["RAMJET_GPU_GUARD_ACTIVE"]
        gpu_count = int(os.environ["RAMJET_GPU_GUARD_EXPECTED_GPUS"])
        abort_c = float(os.environ["RAMJET_GPU_GUARD_ABORT_C"])
        run_id = os.environ["RAMJET_GPU_GUARD_RUN_ID"]
        parent_pid = int(os.environ["RAMJET_GPU_GUARD_PARENT_PID"])
        descriptor = int(os.environ["RAMJET_GPU_GUARD_CAPABILITY_FD"])
        expected_digest = os.environ[
            "RAMJET_GPU_GUARD_CAPABILITY_SHA256"
        ]
    except (KeyError, ValueError) as error:
        raise GuardError("inherited GPU guard capability is invalid") from error
    if (
        active != "1"
        or gpu_count != expected_gpus
        or not 20 <= abort_c <= maximum_abort_c
        or re.fullmatch(r"[0-9a-f]{32}", run_id) is None
        or parent_pid != os.getppid()
        or not 3 <= descriptor <= 1 << 20
        or re.fullmatch(r"[0-9a-f]{64}", expected_digest) is None
    ):
        raise GuardError("inherited GPU guard capability is invalid")
    try:
        info = os.fstat(descriptor)
        seals = fcntl.fcntl(descriptor, fcntl.F_GET_SEALS)
        payload = os.pread(descriptor, CAPABILITY_BYTES + 1, 0)
        fcntl.fcntl(descriptor, fcntl.F_SETFD, fcntl.FD_CLOEXEC)
    except OSError as error:
        raise GuardError("inherited GPU guard capability is invalid") from error
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_nlink != 0
        or info.st_size != CAPABILITY_BYTES
        or seals & CAPABILITY_SEALS != CAPABILITY_SEALS
        or len(payload) != CAPABILITY_BYTES
        or not hmac.compare_digest(hashlib.sha256(payload).hexdigest(), expected_digest)
    ):
        raise GuardError("inherited GPU guard capability is invalid")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_PDEATHSIG, signal.SIGTERM, 0, 0, 0) != 0:
        raise GuardError("inherited GPU guard liveness is unavailable")
    # Close the fork/exec/validation race: a dead or replaced parent is rejected
    # even if it disappeared before PDEATHSIG was armed.
    if os.getppid() != parent_pid:
        raise GuardError("inherited GPU guard parent is no longer alive")
    return {
        "expected_gpus": gpu_count,
        "abort_c": abort_c,
        "run_id": run_id,
    }


@dataclasses.dataclass(frozen=True)
class GpuReading:
    index: int
    uuid: str
    name: str
    temperature_c: float
    power_w: float
    power_limit_w: float
    gpu_utilization_pct: float
    memory_utilization_pct: float
    memory_used_mib: float
    memory_total_mib: float


@dataclasses.dataclass(frozen=True)
class AirReading:
    """One chassis air-temperature sensor, as Grafana's bunker-temps reads it."""

    sensor: str
    temperature_c: float


@dataclasses.dataclass(frozen=True)
class GpuSample:
    readings: tuple[GpuReading, ...]
    air: tuple[AirReading, ...] = ()

    @property
    def hottest(self) -> GpuReading:
        """Hottest GPU. Recorded for diagnosis; it no longer gates anything."""

        return max(self.readings, key=lambda item: item.temperature_c)

    @property
    def hottest_air(self) -> AirReading:
        """The reading the guard actually decides on.

        Room/inlet air is the signal that nothing else protects. A GPU
        defends itself -- these devices throttle at 85C and the driver cuts
        power at 90C -- so gating on silicon temperature mostly re-implements
        the hardware. Facility cooling has no such backstop, and it is shared,
        so it is the failure that takes out more than one run.
        """

        return max(self.air, key=lambda item: item.temperature_c)


@dataclasses.dataclass(frozen=True)
class ProcessIdentity:
    pid: int
    parent_pid: int
    start_ticks: int
    state: str


def process_snapshot() -> dict[int, ProcessIdentity]:
    result = {}
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            raw = (entry / "stat").read_text(encoding="ascii")
            closing = raw.rfind(")")
            fields = raw[closing + 2 :].split()
            if closing <= 0 or len(fields) < 20:
                continue
            result[pid] = ProcessIdentity(
                pid=pid,
                state=fields[0],
                parent_pid=int(fields[1]),
                start_ticks=int(fields[19]),
            )
        # A procfs task can disappear after directory enumeration but before
        # stat is read. Linux may surface that race as ENOENT or ESRCH
        # (`ProcessLookupError`), depending on which procfs lookup lost it.
        except (OSError, UnicodeError, ValueError):
            continue
    return result


class ChildTree:
    """Track descendants across process-group/session escape and reparenting."""

    def __init__(self, root_pid: int):
        self.root_pid = root_pid
        self.owner_pid = os.getpid()
        root = process_snapshot().get(root_pid)
        if root is None:
            raise GuardError("child process identity is unavailable")
        self.known = {(root.pid, root.start_ticks)}

    def observe(self) -> list[ProcessIdentity]:
        snapshot = process_snapshot()
        owned = {
            pid
            for pid, identity in snapshot.items()
            if (pid, identity.start_ticks) in self.known
        }
        changed = True
        while changed:
            changed = False
            for pid, identity in snapshot.items():
                if pid == self.owner_pid or pid in owned:
                    continue
                if identity.parent_pid in owned or identity.parent_pid == self.owner_pid:
                    owned.add(pid)
                    self.known.add((pid, identity.start_ticks))
                    changed = True
        return [snapshot[pid] for pid in sorted(owned)]

    def live(self) -> list[ProcessIdentity]:
        return [identity for identity in self.observe() if identity.state != "Z"]

    def signal_identities(
        self, identities: list[ProcessIdentity], signum: int
    ) -> None:
        # Children first: request clients stop before the deployment owner rolls back.
        parents = {identity.pid: identity.parent_pid for identity in identities}

        def depth(identity: ProcessIdentity) -> int:
            value = 0
            parent = identity.parent_pid
            seen = set()
            while parent in parents and parent not in seen:
                seen.add(parent)
                value += 1
                parent = parents[parent]
            return value

        for identity in sorted(identities, key=depth, reverse=True):
            current = process_snapshot().get(identity.pid)
            if current is None or current.start_ticks != identity.start_ticks:
                continue
            try:
                os.kill(identity.pid, signum)
            except ProcessLookupError:
                continue

    def signal(self, signum: int) -> None:
        self.signal_identities(self.live(), signum)

    def reap_adopted(self, direct_child_pid: int) -> None:
        for pid, _start_ticks in tuple(self.known):
            if pid == direct_child_pid:
                continue
            try:
                os.waitpid(pid, os.WNOHANG)
            except (ChildProcessError, ProcessLookupError):
                continue


def set_child_subreaper(enabled: bool) -> bool:
    libc = ctypes.CDLL(None, use_errno=True)
    previous = ctypes.c_int()
    if libc.prctl(PR_GET_CHILD_SUBREAPER, ctypes.byref(previous), 0, 0, 0) != 0:
        raise GuardError("child-tree supervision is unavailable")
    if libc.prctl(PR_SET_CHILD_SUBREAPER, int(enabled), 0, 0, 0) != 0:
        raise GuardError("child-tree supervision is unavailable")
    return bool(previous.value)


@dataclasses.dataclass
class SampleSummary:
    samples: int = 0
    max_air_temperature_c: float = 0
    max_temperature_c: float = 0
    max_total_power_w: float = 0
    max_power_fraction: float = 0
    max_gpu_utilization_pct: float = 0
    max_memory_utilization_pct: float = 0
    max_memory_used_mib: float = 0
    memory_total_mib: float = 0
    per_gpu: dict[int, dict[str, object]] = dataclasses.field(default_factory=dict)

    def observe(self, sample: GpuSample) -> None:
        self.samples += 1
        if sample.air:
            self.max_air_temperature_c = max(
                self.max_air_temperature_c,
                max(item.temperature_c for item in sample.air),
            )
        self.max_temperature_c = max(
            self.max_temperature_c,
            max(item.temperature_c for item in sample.readings),
        )
        self.max_total_power_w = max(
            self.max_total_power_w,
            sum(item.power_w for item in sample.readings),
        )
        self.max_power_fraction = max(
            self.max_power_fraction,
            max(item.power_w / item.power_limit_w for item in sample.readings),
        )
        self.max_gpu_utilization_pct = max(
            self.max_gpu_utilization_pct,
            max(item.gpu_utilization_pct for item in sample.readings),
        )
        self.max_memory_utilization_pct = max(
            self.max_memory_utilization_pct,
            max(item.memory_utilization_pct for item in sample.readings),
        )
        self.max_memory_used_mib = max(
            self.max_memory_used_mib,
            sum(item.memory_used_mib for item in sample.readings),
        )
        self.memory_total_mib = max(
            self.memory_total_mib,
            sum(item.memory_total_mib for item in sample.readings),
        )
        for item in sample.readings:
            identity_sha256 = hashlib.sha256(item.uuid.encode("ascii")).hexdigest()
            observed = self.per_gpu.setdefault(
                item.index,
                {
                    "identity_sha256": identity_sha256,
                    "name": item.name,
                    "max_temperature_c": 0,
                    "max_power_w": 0,
                    "max_power_fraction": 0,
                    "max_gpu_utilization_pct": 0,
                    "max_memory_used_mib": 0,
                    "memory_total_mib": item.memory_total_mib,
                },
            )
            if (
                observed["identity_sha256"] != identity_sha256
                or observed["name"] != item.name
                or observed["memory_total_mib"] != item.memory_total_mib
            ):
                raise GuardError("GPU inventory changed during the experiment")
            observed["max_temperature_c"] = max(
                observed["max_temperature_c"], item.temperature_c
            )
            observed["max_power_w"] = max(observed["max_power_w"], item.power_w)
            observed["max_power_fraction"] = max(
                observed["max_power_fraction"], item.power_w / item.power_limit_w
            )
            observed["max_gpu_utilization_pct"] = max(
                observed["max_gpu_utilization_pct"], item.gpu_utilization_pct
            )
            observed["max_memory_used_mib"] = max(
                observed["max_memory_used_mib"], item.memory_used_mib
            )
            observed["memory_total_mib"] = item.memory_total_mib

    def public(self) -> dict[str, object]:
        return {
            "samples": self.samples,
            # The gate's own signal comes first: this is the number that
            # decides whether a run continues.
            "max_air_temperature_c": round(self.max_air_temperature_c, 2),
            "max_temperature_c": round(self.max_temperature_c, 2),
            "max_total_power_w": round(self.max_total_power_w, 2),
            "max_power_fraction": round(self.max_power_fraction, 4),
            "max_gpu_utilization_pct": round(self.max_gpu_utilization_pct, 2),
            "max_memory_utilization_pct": round(
                self.max_memory_utilization_pct, 2
            ),
            "max_memory_used_mib": round(self.max_memory_used_mib, 2),
            "memory_total_mib": round(self.memory_total_mib, 2),
            "per_gpu": [
                {
                    "index": index,
                    **{
                        name: round(value, 4) if isinstance(value, float) else value
                        for name, value in values.items()
                    },
                }
                for index, values in sorted(self.per_gpu.items())
            ],
        }


class JournalReservation:
    def __init__(self, path: pathlib.Path):
        self.path = path
        parent = path.parent
        try:
            parent_info = parent.stat()
            if (
                not stat.S_ISDIR(parent_info.st_mode)
                or parent.is_symlink()
                or parent_info.st_uid != os.geteuid()
                or parent_info.st_mode & 0o077
            ):
                raise GuardError("journal parent must be owner-only")
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
            if hasattr(os, "O_NOFOLLOW"):
                flags |= os.O_NOFOLLOW
            self.descriptor = os.open(path, flags, 0o600)
            self.bytes_written = 0
            self._fsync_parent()
        except OSError as error:
            raise GuardError("journal reservation failed") from error

    def _fsync_parent(self) -> None:
        directory = os.open(
            self.path.parent, os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY
        )
        try:
            os.fsync(directory)
        finally:
            os.close(directory)

    def append(self, record: dict[str, object]) -> None:
        if self.descriptor < 0:
            raise GuardError("journal was already finalized")
        raw = (json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n").encode(
            "ascii"
        )
        if len(raw) > 64 * 1024 or self.bytes_written + len(raw) > MAX_JOURNAL_BYTES:
            raise GuardError("journal exceeded its bound")
        written = 0
        while written < len(raw):
            count = os.write(self.descriptor, raw[written:])
            if count <= 0:
                raise GuardError("journal write failed")
            written += count
        self.bytes_written += len(raw)
        os.fsync(self.descriptor)

    def finish(self, record: dict[str, object]) -> None:
        self.append(record)
        descriptor = self.descriptor
        self.descriptor = -1
        try:
            os.close(descriptor)
        finally:
            self._fsync_parent()

    def close(self) -> None:
        if self.descriptor >= 0:
            os.close(self.descriptor)
            self.descriptor = -1


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace(
        "+00:00", "Z"
    )


def finite_number(value: str, name: str, low: float, high: float) -> float:
    try:
        number = float(value.strip())
    except ValueError as error:
        raise GuardError("GPU telemetry is malformed") from error
    if not math.isfinite(number) or not low <= number <= high:
        raise GuardError(f"GPU {name} telemetry is out of bounds")
    return number


def parse_sample(raw: str, expected_gpus: int) -> GpuSample:
    try:
        rows = list(csv.reader(raw.splitlines(), skipinitialspace=True))
    except csv.Error as error:
        raise GuardError("GPU telemetry is malformed") from error
    if len(rows) != expected_gpus or any(
        len(row) != len(QUERY_FIELDS) for row in rows
    ):
        raise GuardError("GPU telemetry cardinality is invalid")
    readings = []
    uuids = set()
    for row in rows:
        try:
            index = int(row[0].strip())
        except ValueError as error:
            raise GuardError("GPU telemetry index is invalid") from error
        uuid = row[1].strip()
        name = row[2].strip()
        if (
            re.fullmatch(r"GPU-[A-Za-z0-9-]{16,96}", uuid) is None
            or uuid in uuids
            or not 1 <= len(name) <= 128
            or any(ord(character) < 32 or ord(character) > 126 for character in name)
        ):
            raise GuardError("GPU telemetry identity is invalid")
        uuids.add(uuid)
        reading = GpuReading(
            index=index,
            uuid=uuid,
            name=name,
            temperature_c=finite_number(row[3], "temperature", -20, 150),
            power_w=finite_number(row[4], "power", 0, 2000),
            power_limit_w=finite_number(row[5], "power limit", 1, 2000),
            gpu_utilization_pct=finite_number(row[6], "utilization", 0, 100),
            memory_utilization_pct=finite_number(
                row[7], "memory utilization", 0, 100
            ),
            memory_used_mib=finite_number(row[8], "used memory", 0, 1024 * 1024),
            memory_total_mib=finite_number(
                row[9], "total memory", 1, 1024 * 1024
            ),
        )
        if (
            reading.memory_used_mib > reading.memory_total_mib
            or reading.power_w > reading.power_limit_w * 1.1
        ):
            raise GuardError("GPU telemetry is internally inconsistent")
        readings.append(reading)
    readings.sort(key=lambda item: item.index)
    if [item.index for item in readings] != list(range(expected_gpus)):
        raise GuardError("GPU telemetry indices are invalid")
    return GpuSample(tuple(readings))


# The sensors Grafana's "Room / Inlet Air Temperature" panel selects. node06
# exposes FP_TEMP (front panel) rather than an Inlet Temp sensor; the others
# expose Inlet Temp. Both are chassis intake air.
MAX_AIR_PAYLOAD_BYTES = 4 << 20
AIR_SENSORS = ("Inlet Temp", "FP_TEMP")
AIR_METRIC = "node_ipmi_temperature_celsius"
AIR_SAMPLE_PATTERN = re.compile(
    r'^node_ipmi_temperature_celsius\{[^}]*sensor="(?P<sensor>[^"]+)"[^}]*\}\s+'
    r"(?P<value>-?\d+(?:\.\d+)?)\s*$",
    re.MULTILINE,
)


def parse_air_metrics(payload: str) -> tuple[AirReading, ...]:
    """Extracts intake-air readings from a Prometheus text exposition.

    Only the dashboard's sensors are admitted. The same exporter publishes
    CPU, DIMM, and per-slot GPU temperatures under the same metric name, and
    silently averaging those in would make the gate meaningless.
    """

    readings = []
    for match in AIR_SAMPLE_PATTERN.finditer(payload):
        sensor = match.group("sensor")
        if sensor in AIR_SENSORS:
            readings.append(AirReading(sensor=sensor, temperature_c=float(match.group("value"))))
    return tuple(readings)


def query_air(args: argparse.Namespace) -> tuple[AirReading, ...]:
    """Reads intake-air temperature from the local node exporter.

    Deliberately the same metric and sensor set as the Grafana dashboard, read
    from the host itself rather than from Prometheus: a watchdog that depends
    on a remote query fails open exactly when the network is the problem.

    # Errors

    Raises GuardError when the exporter is unreachable, malformed, or exposes
    none of the expected sensors, so a blind guard fails closed.
    """

    try:
        with urllib.request.urlopen(
            args.air_metrics_url, timeout=args.sample_timeout_seconds
        ) as response:
            payload = response.read(MAX_AIR_PAYLOAD_BYTES).decode("utf-8", "replace")
    except (urllib.error.URLError, OSError, ValueError) as error:
        raise GuardError("air telemetry query failed") from error
    readings = parse_air_metrics(payload)
    if not readings:
        raise GuardError("air telemetry exposed no intake sensor")
    return readings


def query_gpus(args: argparse.Namespace) -> GpuSample:
    command = [
        args.nvidia_smi,
        f"--query-gpu={','.join(QUERY_FIELDS)}",
        "--format=csv,noheader,nounits",
    ]
    child = None
    try:
        child = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        stdout, _stderr = child.communicate(timeout=args.sample_timeout_seconds)
    except subprocess.TimeoutExpired as error:
        # Never call wait/communicate after the deadline: a driver task stuck in
        # uninterruptible I/O may not reap even after SIGKILL. Close our pipe,
        # fail the experiment, and isolate any unavoidable wait in a daemon reaper.
        if child is not None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            if child.stdout is not None:
                child.stdout.close()
            if child.poll() is None:
                threading.Thread(target=child.wait, daemon=True).start()
        raise GuardError("GPU telemetry query failed") from error
    except OSError as error:
        if child is not None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            if child.stdout is not None:
                child.stdout.close()
            if child.poll() is None:
                threading.Thread(target=child.wait, daemon=True).start()
        raise GuardError("GPU telemetry query failed") from error
    if child.returncode != 0 or len(stdout) > 16 * 1024:
        raise GuardError("GPU telemetry query failed")
    try:
        decoded = stdout.decode("utf-8", "strict")
    except UnicodeError as error:
        raise GuardError("GPU telemetry query failed") from error
    # GPU readings are collected for the journal; the intake-air reading is
    # what the guard actually decides on, so a failure to read it must fail
    # the sample rather than silently produce an ungated one.
    gpus = parse_sample(decoded, args.expected_gpus)
    return dataclasses.replace(gpus, air=query_air(args))


@dataclasses.dataclass(frozen=True)
class TerminationOutcome:
    escalated: bool
    telemetry_failed: bool


def terminate_tree(
    tree: ChildTree,
    child: subprocess.Popen[bytes],
    args: argparse.Namespace,
    sampler: Callable[[argparse.Namespace], GpuSample],
    observe_sample: Callable[[GpuSample, bool], None],
) -> TerminationOutcome:
    """Cancel event-time work, preserve bounded rollback, and keep sampling."""
    child.poll()
    tree.reap_adopted(child.pid)
    initial = tree.live()
    if not initial:
        return TerminationOutcome(False, False)
    initial_keys = {(item.pid, item.start_ticks) for item in initial}
    tree.signal_identities(initial, signal.SIGTERM)
    started = time.monotonic()
    workload_deadline = started + args.workload_grace_seconds
    owner_deadline = started + args.termination_grace_seconds
    next_sample = started + args.poll_seconds
    escalated = False
    telemetry_failed = False
    telemetry_available = True
    while True:
        child.poll()
        tree.reap_adopted(child.pid)
        live = tree.live()
        if not live:
            child.poll()
            return TerminationOutcome(
                escalated or child.returncode == EXIT_SHIM_ESCALATED,
                telemetry_failed,
            )
        now = time.monotonic()
        if args.preserve_rollback_owner:
            initial_workload = [
                item
                for item in live
                if item.pid != child.pid
                and item.parent_pid != child.pid
                and (item.pid, item.start_ticks) in initial_keys
            ]
        else:
            # Direct/candidate roots own no rollback authority. Anything they
            # fork after TERM is still request work and shares the short grace.
            initial_workload = live
        if telemetry_available and now >= next_sample:
            try:
                sample = sampler(args)
                observe_sample(sample, True)
            except GuardError:
                telemetry_failed = True
                telemetry_available = False
                if initial_workload:
                    tree.signal_identities(initial_workload, signal.SIGKILL)
                    escalated = True
                    initial_workload = []
            else:
                next_sample = now + args.poll_seconds
                if sample.hottest_air.temperature_c >= args.abort_c and initial_workload:
                    tree.signal_identities(initial_workload, signal.SIGKILL)
                    escalated = True
                    initial_workload = []
        if now >= workload_deadline and initial_workload:
            tree.signal_identities(initial_workload, signal.SIGKILL)
            escalated = True
        if now >= owner_deadline:
            tree.signal_identities(live, signal.SIGKILL)
            escalated = True
            break
        time.sleep(min(0.05, max(0.005, args.poll_seconds / 4)))

    kill_deadline = time.monotonic() + 5
    while time.monotonic() < kill_deadline:
        child.poll()
        tree.reap_adopted(child.pid)
        if not tree.live():
            return TerminationOutcome(escalated, telemetry_failed)
        time.sleep(0.05)
    raise GuardError("child process tree did not terminate")


def child_exit_code(returncode: int) -> int:
    return returncode if returncode >= 0 else 128 + min(-returncode, 127)


def exec_shim(command: list[str]) -> int:
    """Own any guarded command so guard death cannot orphan request work."""
    if not command:
        return EXIT_INTERNAL
    contract = validate_inherited_guard()
    try:
        workload_grace = float(
            os.environ["RAMJET_GPU_GUARD_WORKLOAD_GRACE_SECONDS"]
        )
        owner_grace = float(
            os.environ["RAMJET_GPU_GUARD_TERMINATION_GRACE_SECONDS"]
        )
        preserve_owner = (
            os.environ["RAMJET_GPU_GUARD_PRESERVE_ROLLBACK_OWNER"] == "1"
        )
    except (KeyError, ValueError) as error:
        raise GuardError("inherited GPU guard shutdown policy is invalid") from error
    if not 0.1 <= workload_grace <= 30 or not 1 <= owner_grace <= 780:
        raise GuardError("inherited GPU guard shutdown policy is invalid")
    try:
        launch_descriptor = int(
            os.environ["RAMJET_GPU_GUARD_LAUNCH_FD"]
        )
        launch_info = os.fstat(launch_descriptor)
        if not stat.S_ISFIFO(launch_info.st_mode):
            raise GuardError("inherited GPU guard launch gate is invalid")
        launched = os.read(launch_descriptor, 2)
        os.close(launch_descriptor)
    except (KeyError, ValueError, OSError) as error:
        raise GuardError("inherited GPU guard launch gate is invalid") from error
    if launched != b"1":
        raise GuardError("inherited GPU guard launch was revoked")

    watched = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    interrupted_signal = 0

    def interrupted(signum: int, _frame: object) -> None:
        nonlocal interrupted_signal
        interrupted_signal = interrupted_signal or signum

    for watched_signal in watched:
        signal.signal(watched_signal, interrupted)
    previous_subreaper = set_child_subreaper(True)
    child: subprocess.Popen[bytes] | None = None
    tree: ChildTree | None = None
    capability: GuardCapability | None = None
    try:
        capability = create_guard_capability(
            int(contract["expected_gpus"]),
            float(contract["abort_c"]),
            str(contract["run_id"]),
        )
        child_environment = os.environ.copy()
        child_environment.update(capability.environment)
        child = subprocess.Popen(
            command,
            env=child_environment,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
            pass_fds=(capability.descriptor,),
        )
        capability.close()
        try:
            tree = ChildTree(child.pid)
        except GuardError:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=5)
            raise
    finally:
        if capability is not None:
            capability.close()

    orphaned = False
    try:
        while not interrupted_signal:
            try:
                returncode = child.wait(timeout=0.1)
            except subprocess.TimeoutExpired:
                tree.observe()
                continue
            tree.reap_adopted(child.pid)
            if not tree.live():
                return child_exit_code(returncode)
            orphaned = True
            interrupted_signal = signal.SIGTERM

        initial = tree.live()
        initial_keys = {(item.pid, item.start_ticks) for item in initial}
        tree.signal_identities(initial, signal.SIGTERM)
        started = time.monotonic()
        workload_deadline = started + workload_grace
        owner_deadline = started + owner_grace
        escalated = False
        while True:
            child.poll()
            tree.reap_adopted(child.pid)
            live = tree.live()
            if not live:
                if orphaned:
                    return EXIT_SHIM_ESCALATED if escalated else EXIT_SHIM_ORPHAN
                return EXIT_SHIM_ESCALATED if escalated else 128 + interrupted_signal
            now = time.monotonic()
            if preserve_owner:
                workload = [
                    item
                    for item in live
                    if item.pid != child.pid
                    and (item.pid, item.start_ticks) in initial_keys
                ]
            else:
                workload = live
            if now >= workload_deadline and workload:
                tree.signal_identities(workload, signal.SIGKILL)
                escalated = True
            if now >= owner_deadline:
                tree.signal_identities(live, signal.SIGKILL)
                escalated = True
                break
            time.sleep(0.05)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            child.poll()
            tree.reap_adopted(child.pid)
            if not tree.live():
                if orphaned:
                    return EXIT_SHIM_ESCALATED if escalated else EXIT_SHIM_ORPHAN
                return EXIT_SHIM_ESCALATED if escalated else 128 + interrupted_signal
            time.sleep(0.05)
        raise GuardError("guarded command tree did not terminate")
    finally:
        if tree is not None and tree.live():
            tree.signal(signal.SIGKILL)
        try:
            set_child_subreaper(previous_subreaper)
        except GuardError:
            pass


def run_guard(
    args: argparse.Namespace,
    sampler: Callable[[argparse.Namespace], GpuSample] = query_gpus,
) -> int:
    journal = JournalReservation(args.output)
    summary = SampleSummary()
    started = time.monotonic()
    run_id = secrets.token_hex(16)
    started_utc = utc_now()
    record: dict[str, object] = {
        "type": "final",
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "label": args.label,
        "status": "failed",
        "reason": "internal_error",
        "started_utc": started_utc,
        "expected_gpus": args.expected_gpus,
        "thresholds": {
            "start_max_c": args.start_max_c,
            "abort_c": args.abort_c,
            "poll_seconds": args.poll_seconds,
            "workload_grace_seconds": args.workload_grace_seconds,
            "termination_grace_seconds": args.termination_grace_seconds,
            "max_runtime_seconds": args.max_runtime_seconds,
        },
    }
    journal.append(
        {
            "type": "start",
            "schema_version": SCHEMA_VERSION,
            "run_id": run_id,
            "label": args.label,
            "started_utc": started_utc,
            "expected_gpus": args.expected_gpus,
            "thresholds": record["thresholds"],
        }
    )
    last_checkpoint = started

    def checkpoint(force: bool = False) -> None:
        nonlocal last_checkpoint
        now = time.monotonic()
        if force or now - last_checkpoint >= CHECKPOINT_SECONDS:
            journal.append(
                {
                    "type": "checkpoint",
                    "schema_version": SCHEMA_VERSION,
                    "run_id": run_id,
                    "elapsed_seconds": round(now - started, 3),
                    "telemetry": summary.public(),
                }
            )
            last_checkpoint = now

    def observe_sample(sample: GpuSample, force: bool = False) -> None:
        summary.observe(sample)
        checkpoint(force)

    child: subprocess.Popen[bytes] | None = None
    child_tree: ChildTree | None = None
    capability: GuardCapability | None = None
    launch_read_descriptor = -1
    launch_write_descriptor = -1
    previous_subreaper: bool | None = None
    interrupted_signal = 0

    def interrupted(signum: int, _frame: object) -> None:
        nonlocal interrupted_signal
        interrupted_signal = interrupted_signal or signum

    previous_handlers = {}
    for watched in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        previous_handlers[watched] = signal.signal(watched, interrupted)
    result = EXIT_INTERNAL
    try:
        cooldown_started = time.monotonic()
        workload_started = cooldown_started
        preflight_passed = False
        consecutive_telemetry_failures = 0
        while True:
            if interrupted_signal:
                record["reason"] = "interrupted"
                result = 128 + interrupted_signal
                break
            try:
                sample = sampler(args)
            except GuardError:
                # A single slow nvidia-smi is not blindness. On node06 the
                # driver intermittently exceeds the 2s sample deadline (~1 call
                # in 12 measured at 1Hz), so failing the interval on the first
                # miss made every run of more than a few seconds abort
                # spuriously. Sustained loss still fails closed: only
                # MAX_CONSECUTIVE_TELEMETRY_FAILURES back-to-back misses, which
                # is a bounded blind window, end the run.
                consecutive_telemetry_failures += 1
                record["telemetry_retries"] = (
                    record.get("telemetry_retries", 0) + 1
                )
                if (
                    consecutive_telemetry_failures
                    >= MAX_CONSECUTIVE_TELEMETRY_FAILURES
                ):
                    record["reason"] = "telemetry_unavailable"
                    result = EXIT_TELEMETRY
                    break
                time.sleep(args.poll_seconds)
                continue
            consecutive_telemetry_failures = 0
            observe_sample(sample)
            if interrupted_signal:
                record["reason"] = "interrupted"
                result = 128 + interrupted_signal
                break
            if sample.hottest_air.temperature_c <= args.start_max_c:
                preflight_passed = True
                break
            if time.monotonic() - cooldown_started >= args.cooldown_timeout_seconds:
                record["reason"] = "preflight_too_hot"
                record["trigger"] = {
                    "sensor": sample.hottest_air.sensor,
                    "temperature_c": sample.hottest_air.temperature_c,
                }
                result = EXIT_THERMAL
                break
            time.sleep(args.poll_seconds)

        if preflight_passed:
            # The continuous-inference clock starts when the workload starts,
            # not when the guard does: waiting for a cool start is not
            # inference and must not consume the caller's budget.
            workload_started = time.monotonic()
            child_environment = os.environ.copy()
            capability = create_guard_capability(
                args.expected_gpus, args.abort_c, run_id
            )
            child_environment.update(capability.environment)
            launch_read_descriptor, launch_write_descriptor = os.pipe2(os.O_CLOEXEC)
            child_environment["RAMJET_GPU_GUARD_LAUNCH_FD"] = str(
                launch_read_descriptor
            )
            child_environment["RAMJET_GPU_GUARD_WORKLOAD_GRACE_SECONDS"] = str(
                args.workload_grace_seconds
            )
            child_environment[
                "RAMJET_GPU_GUARD_TERMINATION_GRACE_SECONDS"
            ] = str(args.termination_grace_seconds)
            child_environment["RAMJET_GPU_GUARD_PRESERVE_ROLLBACK_OWNER"] = (
                "1" if args.preserve_rollback_owner else "0"
            )
            previous_subreaper = set_child_subreaper(True)
            child = subprocess.Popen(
                [sys.executable, str(pathlib.Path(__file__).resolve()), "--exec-shim", *args.command],
                env=child_environment,
                stdin=subprocess.DEVNULL,
                start_new_session=True,
                pass_fds=(capability.descriptor, launch_read_descriptor),
            )
            os.close(launch_read_descriptor)
            launch_read_descriptor = -1
            capability.close()
            try:
                child_tree = ChildTree(child.pid)
            except GuardError:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired as error:
                    raise GuardError("unguarded child could not be terminated") from error
                raise
            if interrupted_signal:
                os.close(launch_write_descriptor)
                launch_write_descriptor = -1
                raise GuardInterrupted(interrupted_signal)
            os.write(launch_write_descriptor, b"1")
            os.close(launch_write_descriptor)
            launch_write_descriptor = -1
            del child_environment
            while True:
                if interrupted_signal:
                    record["reason"] = "interrupted"
                    result = 128 + interrupted_signal
                    break
                try:
                    returncode = child.wait(timeout=args.poll_seconds)
                except subprocess.TimeoutExpired:
                    returncode = None
                child_tree.observe()
                if returncode is not None:
                    record["child_exit_code"] = child_exit_code(returncode)
                    try:
                        sample = sampler(args)
                    except GuardError:
                        consecutive_telemetry_failures += 1
                        record["telemetry_retries"] = (
                            record.get("telemetry_retries", 0) + 1
                        )
                        if (
                            consecutive_telemetry_failures
                            >= MAX_CONSECUTIVE_TELEMETRY_FAILURES
                        ):
                            record["reason"] = "telemetry_unavailable"
                            result = EXIT_TELEMETRY
                            break
                        sample = None
                    else:
                        consecutive_telemetry_failures = 0
                    if sample is not None:
                        observe_sample(sample)
                    if sample is not None and sample.hottest_air.temperature_c >= args.abort_c:
                        record["reason"] = "thermal_abort"
                        record["trigger"] = {
                            "sensor": sample.hottest_air.sensor,
                            "temperature_c": sample.hottest_air.temperature_c,
                        }
                        result = EXIT_THERMAL
                        break
                    if time.monotonic() - workload_started >= args.max_runtime_seconds:
                        record["reason"] = "runtime_limit"
                        result = EXIT_RUNTIME_LIMIT
                        break
                    child_tree.reap_adopted(child.pid)
                    if child_tree.live():
                        record["reason"] = "orphaned_process_tree"
                        result = EXIT_INTERNAL
                        break
                    if returncode == 0:
                        record["status"] = "passed"
                        record["reason"] = None
                        result = 0
                    elif returncode in (EXIT_SHIM_ESCALATED, EXIT_SHIM_ORPHAN):
                        record["reason"] = "orphaned_process_tree"
                        record["termination_escalated"] = (
                            returncode == EXIT_SHIM_ESCALATED
                        )
                        result = EXIT_INTERNAL
                    else:
                        record["reason"] = "child_failed"
                        result = child_exit_code(returncode)
                    break
                try:
                    sample = sampler(args)
                except GuardError:
                    consecutive_telemetry_failures += 1
                    record["telemetry_retries"] = (
                        record.get("telemetry_retries", 0) + 1
                    )
                    if (
                        consecutive_telemetry_failures
                        >= MAX_CONSECUTIVE_TELEMETRY_FAILURES
                    ):
                        record["reason"] = "telemetry_unavailable"
                        result = EXIT_TELEMETRY
                        break
                    time.sleep(args.poll_seconds)
                    continue
                consecutive_telemetry_failures = 0
                observe_sample(sample)
                if sample.hottest_air.temperature_c >= args.abort_c:
                    record["reason"] = "thermal_abort"
                    record["trigger"] = {
                        "sensor": sample.hottest_air.sensor,
                        "temperature_c": sample.hottest_air.temperature_c,
                    }
                    checkpoint(True)
                    result = EXIT_THERMAL
                    break
                # The continuous-inference cap is deliberately checked in the
                # same loop as the thermal ceiling and terminates by the same
                # path, so a run that is cool but simply long is stopped with
                # the identical bounded workload/owner grace.
                elapsed = time.monotonic() - workload_started
                if elapsed >= args.max_runtime_seconds:
                    record["reason"] = "runtime_limit"
                    record["trigger"] = {"elapsed_seconds": round(elapsed, 3)}
                    checkpoint(True)
                    result = EXIT_RUNTIME_LIMIT
                    break
    except GuardInterrupted as error:
        record["reason"] = "interrupted"
        result = 128 + error.signum
    except (GuardError, OSError, ValueError, subprocess.SubprocessError):
        record["reason"] = "internal_error"
        result = EXIT_INTERNAL
    finally:
        if capability is not None:
            capability.close()
        for descriptor in (launch_read_descriptor, launch_write_descriptor):
            if descriptor >= 0:
                os.close(descriptor)
        if child is not None and child_tree is not None and child_tree.live():
            try:
                outcome = terminate_tree(
                    child_tree, child, args, sampler, observe_sample
                )
                record["termination_escalated"] = outcome.escalated
                if outcome.telemetry_failed:
                    record["reason"] = "termination_telemetry_unavailable"
                    result = EXIT_TELEMETRY
            except (GuardError, OSError, subprocess.SubprocessError):
                record["termination_escalated"] = True
                record["reason"] = "termination_failed"
                record["status"] = "failed"
                result = EXIT_INTERNAL
        record["telemetry"] = summary.public()
        record["duration_seconds"] = round(time.monotonic() - started, 3)
        record["ended_utc"] = utc_now()
        try:
            journal.finish(record)
        finally:
            journal.close()
            if previous_subreaper is not None:
                try:
                    set_child_subreaper(previous_subreaper)
                except GuardError:
                    pass
            for watched, handler in previous_handlers.items():
                signal.signal(watched, handler)
        print(
            json.dumps(
                {
                    "run_id": run_id,
                    "status": record["status"],
                    "reason": record["reason"],
                    "exit_code": result,
                },
                sort_keys=True,
                separators=(",", ":"),
            )
        )
    return result


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--output", type=pathlib.Path, required=True)
    result.add_argument("--label", default="node06-experiment")
    result.add_argument("--expected-gpus", type=int, default=8)
    result.add_argument("--start-max-c", type=float, default=DEFAULT_START_MAX_C)
    result.add_argument("--abort-c", type=float, default=DEFAULT_ABORT_C)
    result.add_argument("--cooldown-timeout-seconds", type=float, default=300)
    result.add_argument("--poll-seconds", type=float, default=1)
    result.add_argument("--sample-timeout-seconds", type=float, default=2)
    result.add_argument("--workload-grace-seconds", type=float, default=5)
    result.add_argument("--termination-grace-seconds", type=float, default=30)
    result.add_argument(
        "--max-runtime-seconds",
        type=float,
        default=DEFAULT_MAX_RUNTIME_SECONDS,
        help="terminate the workload after this much continuous inference",
    )
    result.add_argument(
        "--preserve-rollback-owner",
        action="store_true",
        help="preserve only a rollback-capable root until the owner grace expires",
    )
    result.add_argument("--nvidia-smi", default=DEFAULT_NVIDIA_SMI)
    result.add_argument(
        "--air-metrics-url", default=DEFAULT_AIR_METRICS_URL,
        help="Prometheus text endpoint exposing node_ipmi_temperature_celsius",
    )
    result.add_argument("command", nargs=argparse.REMAINDER)
    return result


def validate_args(args: argparse.Namespace) -> None:
    if args.command and args.command[0] == "--":
        args.command = args.command[1:]
    if not args.command or not all(
        isinstance(item, str) and item for item in args.command
    ):
        raise GuardError("a child command is required after --")
    if not re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,63}", args.label):
        raise GuardError("label is invalid")
    if args.expected_gpus != 8:
        raise GuardError("node06 requires exactly eight GPUs")
    if not 15 <= args.start_max_c <= MAX_ABORT_C - 3:
        raise GuardError("cool-start threshold is invalid")
    if not args.start_max_c + 3 <= args.abort_c <= MAX_ABORT_C:
        raise GuardError("thermal-abort threshold is invalid")
    if not 1 <= args.max_runtime_seconds <= MAX_RUNTIME_SECONDS:
        raise GuardError("continuous inference limit is invalid")
    if not 0 <= args.cooldown_timeout_seconds <= 1800:
        raise GuardError("cooldown timeout is invalid")
    if not 0.25 <= args.poll_seconds <= 1:
        raise GuardError("poll interval is invalid")
    if not 0.25 <= args.sample_timeout_seconds <= 2:
        raise GuardError("sample timeout is invalid")
    if not 0.1 <= args.workload_grace_seconds <= 30:
        raise GuardError("workload grace is invalid")
    if not 1 <= args.termination_grace_seconds <= 780:
        raise GuardError("termination grace is invalid")
    if not isinstance(args.nvidia_smi, str) or not args.nvidia_smi:
        raise GuardError("nvidia-smi path is invalid")
    if not str(args.air_metrics_url).startswith(("http://127.0.0.1", "http://localhost")):
        # The gate must not depend on a remote query: a watchdog that reads
        # the network fails open exactly when the network is the problem.
        raise GuardError("air metrics URL must be local")


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        validate_args(args)
        require_active_work_permitted(f"gpu-workload.{args.label}")
        return run_guard(args)
    except (GuardError, MoratoriumError) as error:
        print(f"node06 GPU guard: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "--exec-shim":
        try:
            raise SystemExit(exec_shim(sys.argv[2:]))
        except GuardError as error:
            print(f"node06 GPU guard shim: {error}", file=sys.stderr)
            raise SystemExit(EXIT_INTERNAL) from None
    raise SystemExit(main())
