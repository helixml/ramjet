#!/usr/bin/env python3
"""Loopback host-metrics agent for the ramjet machine view.

The load balancer runs in a small distroless container and cannot see host
CPU, disk, network, RAPL, or GPU state. This agent runs on the host (or in a
privileged sidecar), samples those sources with the standard library only, and
serves one JSON document the LB scrapes on its machine-view interval:

    GET /sample -> {"version": 1, "host": {...}, "gpus": [...]}

Field names match the Rust `machineview::HostSample` / `machineview::GpuSample`
structs exactly; the LB sanitizes and stores them. Rates (network, disk I/O,
CPU busy fraction, RAPL watts) are computed between consecutive scrapes from
retained counter snapshots, so the first response reports `null` rates.

The agent binds loopback only unless --allow-remote is passed explicitly. It
reads no secrets, accepts no request bodies, and never executes anything but
the pinned `nvidia-smi` query.

Usage:
    python3 machineview_agent.py [--bind 127.0.0.1] [--port 8016] \
        [--mounts /,/home] [--interval-floor-ms 500]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_PORT = 8016
NVIDIA_SMI_FIELDS = (
    "index,name,utilization.gpu,memory.used,memory.total,"
    "power.draw,temperature.gpu,clocks.sm"
)
# Extended telemetry: memory-controller activity, clocks, power limit, fan,
# P-state, HBM temperature, and the four throttle reasons dashboards care
# about. Older drivers may reject some names, so collection falls back to
# the base field list if the extended query fails.
NVIDIA_SMI_EXTENDED_FIELDS = NVIDIA_SMI_FIELDS + (
    ",utilization.memory,clocks.mem,power.limit,fan.speed,pstate,"
    "temperature.memory,"
    "clocks_throttle_reasons.sw_power_cap,"
    "clocks_throttle_reasons.sw_thermal_slowdown,"
    "clocks_throttle_reasons.hw_thermal_slowdown,"
    "clocks_throttle_reasons.hw_slowdown"
)
NVIDIA_SMI_TIMEOUT_SECONDS = 3.0
MAX_RAPL_DOMAINS = 8


def _to_float(raw):
    """Parses one nvidia-smi CSV cell; returns None for [N/A] and friends."""
    if raw is None:
        return None
    text = raw.strip()
    if not text or text.startswith("["):
        return None
    try:
        value = float(text)
    except ValueError:
        return None
    if value != value or value in (float("inf"), float("-inf")):
        return None
    return value


def _to_flag(raw):
    """Parses a throttle-reason cell into 1.0 / 0.0 / None."""
    if raw is None:
        return None
    text = raw.strip().lower()
    if text == "active":
        return 1.0
    if text == "not active":
        return 0.0
    return None


def _to_pstate(raw):
    if raw is None:
        return None
    text = raw.strip().upper()
    if text.startswith("P"):
        try:
            return float(int(text[1:]))
        except ValueError:
            return None
    return None


def parse_nvidia_smi_csv(output):
    """Parses `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`.

    Accepts both the 8-column base shape and the 18-column extended shape;
    extended cells simply add optional fields to each row.
    """
    gpus = []
    for line in output.splitlines():
        cells = [cell.strip() for cell in line.split(",")]
        if len(cells) not in (8, 18):
            continue
        index = _to_float(cells[0])
        if index is None:
            continue
        mem_used_mib = _to_float(cells[3])
        mem_total_mib = _to_float(cells[4])
        gpu = {
            "index": int(index),
            "name": cells[1][:80],
            "util_pct": _to_float(cells[2]),
            "mem_used_bytes": None
            if mem_used_mib is None
            else mem_used_mib * 1024 * 1024,
            "mem_total_bytes": None
            if mem_total_mib is None
            else mem_total_mib * 1024 * 1024,
            "power_watts": _to_float(cells[5]),
            "temp_c": _to_float(cells[6]),
            "sm_mhz": _to_float(cells[7]),
        }
        if len(cells) == 18:
            gpu.update(
                {
                    "mem_util_pct": _to_float(cells[8]),
                    "mem_clock_mhz": _to_float(cells[9]),
                    "power_limit_watts": _to_float(cells[10]),
                    "fan_pct": _to_float(cells[11]),
                    "pstate": _to_pstate(cells[12]),
                    "temp_mem_c": _to_float(cells[13]),
                    "throttle_sw_power": _to_flag(cells[14]),
                    "throttle_sw_thermal": _to_flag(cells[15]),
                    "throttle_hw_thermal": _to_flag(cells[16]),
                    "throttle_hw": _to_flag(cells[17]),
                }
            )
        gpus.append(gpu)
    return gpus


def parse_proc_stat_cpu(text):
    """Returns (busy, total, iowait) jiffies from the aggregate cpu line.

    Idle includes iowait (the usual CPU-busy definition). iowait is also
    returned separately so the dashboard can show IO stall beside disk rates.
    """
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 5 and fields[0] == "cpu":
            values = []
            for raw in fields[1:]:
                try:
                    values.append(int(raw))
                except ValueError:
                    return None
            total = sum(values)
            iowait = values[4] if len(values) > 4 else 0
            idle = values[3] + iowait
            return (total - idle, total, iowait)
    return None


def parse_meminfo(text):
    """Returns memory/swap byte figures from /proc/meminfo content."""
    fields = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0].endswith(":"):
            try:
                fields[parts[0][:-1]] = int(parts[1]) * 1024
            except ValueError:
                continue
    total = fields.get("MemTotal")
    available = fields.get("MemAvailable")
    if total is None:
        return None
    used = None if available is None else max(total - available, 0)
    swap_total = fields.get("SwapTotal")
    swap_free = fields.get("SwapFree")
    swap_used = None
    if swap_total is not None and swap_free is not None:
        swap_used = max(swap_total - swap_free, 0)
    cached = fields.get("Cached")
    buffers = fields.get("Buffers")
    cached_total = None
    if cached is not None:
        cached_total = cached + (buffers or 0)
    return {
        "mem_total_bytes": total,
        "mem_used_bytes": used,
        "mem_cached_bytes": cached_total,
        "swap_total_bytes": swap_total,
        "swap_used_bytes": swap_used,
        "dirty_bytes": fields.get("Dirty"),
        "writeback_bytes": fields.get("Writeback"),
    }


def parse_net_dev(text):
    """Sums rx/tx bytes across physical-looking interfaces."""
    rx_total = 0
    tx_total = 0
    seen = False
    for line in text.splitlines():
        if ":" not in line:
            continue
        name, _, rest = line.partition(":")
        name = name.strip()
        if name == "lo" or name.startswith(("docker", "veth", "br-", "virbr")):
            continue
        fields = rest.split()
        if len(fields) < 16:
            continue
        try:
            rx_total += int(fields[0])
            tx_total += int(fields[8])
        except ValueError:
            continue
        seen = True
    return (rx_total, tx_total) if seen else None


def parse_diskstats(text):
    """Sums whole-disk counters (not partitions). Returns None if none seen."""
    read_sectors = 0
    written_sectors = 0
    reads = 0
    writes = 0
    inflight = 0
    io_ticks = {}
    seen = False
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 14:
            continue
        name = fields[2]
        # Whole-disk names: sdX, vdX, nvmeXnY (not nvmeXnYpZ), mmcblkX.
        is_whole = (
            (name.startswith(("sd", "vd", "xvd")) and not name[-1].isdigit())
            or (name.startswith("nvme") and "p" not in name[4:])
            or (name.startswith("mmcblk") and "p" not in name[6:])
        )
        if not is_whole:
            continue
        try:
            reads += int(fields[3])
            read_sectors += int(fields[5])
            writes += int(fields[7])
            written_sectors += int(fields[9])
            inflight += int(fields[11])
            io_ticks[name] = int(fields[12])
        except ValueError:
            continue
        seen = True
    if not seen:
        return None
    return {
        "read_bytes": read_sectors * 512,
        "write_bytes": written_sectors * 512,
        "reads": reads,
        "writes": writes,
        "inflight": inflight,
        "io_ticks": io_ticks,
    }


def parse_pressure(text):
    """Returns some-avg10 (0–100) from a /proc/pressure/{cpu,io,memory} file."""
    if not text:
        return None
    for line in text.splitlines():
        fields = line.split()
        if not fields or fields[0] != "some":
            continue
        for field in fields[1:]:
            if field.startswith("avg10="):
                try:
                    value = float(field.split("=", 1)[1])
                except ValueError:
                    return None
                if value != value or value in (float("inf"), float("-inf")):
                    return None
                return max(0.0, min(100.0, value))
    return None


def parse_loadavg(text):
    fields = text.split()
    if not fields:
        return None
    try:
        return float(fields[0])
    except ValueError:
        return None


class Collector:
    """Stateful sampler: keeps previous counters to derive rates."""

    def __init__(self, mounts, proc_root="/proc", rapl_root="/sys/class/powercap"):
        self.mounts = mounts
        self.proc_root = proc_root
        self.rapl_root = rapl_root
        self.nvidia_smi = shutil.which("nvidia-smi")
        self.nvidia_smi_fields = NVIDIA_SMI_EXTENDED_FIELDS
        self.previous = {}
        self.lock = threading.Lock()

    def _read(self, path):
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as handle:
                return handle.read()
        except OSError:
            return None

    def _rate(self, key, value, now):
        """Returns per-second delta against the previous observation."""
        if value is None:
            self.previous.pop(key, None)
            return None
        prior = self.previous.get(key)
        self.previous[key] = (now, value)
        if prior is None:
            return None
        prior_t, prior_value = prior
        if now <= prior_t or value < prior_value:
            return None
        return (value - prior_value) / (now - prior_t)

    def _cpu_shares(self, now):
        """Returns (cpu_pct, iowait_pct); both None on the first sample."""
        text = self._read(os.path.join(self.proc_root, "stat"))
        parsed = parse_proc_stat_cpu(text) if text else None
        if parsed is None:
            return (None, None)
        busy, total, iowait = parsed
        prior = self.previous.get("cpu")
        self.previous["cpu"] = (now, (busy, total, iowait))
        if prior is None:
            return (None, None)
        _, (prior_busy, prior_total, prior_iowait) = prior
        delta_total = total - prior_total
        delta_busy = busy - prior_busy
        delta_iowait = iowait - prior_iowait
        if delta_total <= 0 or delta_busy < 0 or delta_iowait < 0:
            return (None, None)
        cpu_pct = max(0.0, min(100.0, delta_busy * 100.0 / delta_total))
        iowait_pct = max(0.0, min(100.0, delta_iowait * 100.0 / delta_total))
        return (cpu_pct, iowait_pct)

    def _rapl_watts(self, now):
        try:
            entries = sorted(os.listdir(self.rapl_root))
        except OSError:
            return None
        total = None
        counted = 0
        for entry in entries:
            # Package-level domains only; sub-domains (:N:M) double-count.
            if not entry.startswith("intel-rapl:") or entry.count(":") != 1:
                continue
            if counted >= MAX_RAPL_DOMAINS:
                break
            energy = self._read(os.path.join(self.rapl_root, entry, "energy_uj"))
            if energy is None:
                continue
            try:
                microjoules = int(energy.strip())
            except ValueError:
                continue
            counted += 1
            watts = self._rate("rapl.%s" % entry, microjoules / 1_000_000.0, now)
            if watts is not None:
                total = (total or 0.0) + watts
        return total

    def _disk_util_pct(self, io_ticks, now):
        """Busiest whole disk: io_ticks delta as a percent of wall time."""
        prior = self.previous.get("disk.io_ticks")
        self.previous["disk.io_ticks"] = (now, io_ticks)
        if prior is None or not io_ticks:
            return None
        prior_t, prior_map = prior
        elapsed_ms = (now - prior_t) * 1000.0
        if elapsed_ms <= 0:
            return None
        busiest = None
        for name, ticks in io_ticks.items():
            previous = prior_map.get(name)
            if previous is None or ticks < previous:
                continue
            util = min(100.0, (ticks - previous) * 100.0 / elapsed_ms)
            if busiest is None or util > busiest:
                busiest = util
        return busiest

    def _disks(self):
        disks = []
        for mount in self.mounts:
            try:
                stats = os.statvfs(mount)
            except OSError:
                continue
            total = stats.f_frsize * stats.f_blocks
            free = stats.f_frsize * stats.f_bavail
            if total <= 0:
                continue
            disk = {
                "mount": mount,
                "total_bytes": total,
                "used_bytes": max(total - free, 0),
            }
            if stats.f_files > 0:
                disk["inodes_total"] = float(stats.f_files)
                disk["inodes_used"] = float(max(stats.f_files - stats.f_favail, 0))
            disks.append(disk)
        return disks

    def _query_gpus(self, fields):
        try:
            result = subprocess.run(
                [
                    self.nvidia_smi,
                    "--query-gpu=%s" % fields,
                    "--format=csv,noheader,nounits",
                ],
                capture_output=True,
                text=True,
                timeout=NVIDIA_SMI_TIMEOUT_SECONDS,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        if result.returncode != 0:
            return None
        return parse_nvidia_smi_csv(result.stdout)

    def _gpus(self):
        if not self.nvidia_smi:
            return []
        gpus = self._query_gpus(self.nvidia_smi_fields)
        if gpus is None and self.nvidia_smi_fields != NVIDIA_SMI_FIELDS:
            # An older driver rejected an extended field name; stay on the
            # base list from now on rather than failing every sample.
            self.nvidia_smi_fields = NVIDIA_SMI_FIELDS
            gpus = self._query_gpus(self.nvidia_smi_fields)
        return gpus or []

    def sample(self):
        with self.lock:
            now = time.monotonic()
            net_text = self._read(os.path.join(self.proc_root, "net", "dev"))
            net = parse_net_dev(net_text) if net_text else None
            disk_text = self._read(os.path.join(self.proc_root, "diskstats"))
            disk = parse_diskstats(disk_text) if disk_text else None
            load_text = self._read(os.path.join(self.proc_root, "loadavg"))
            mem_text = self._read(os.path.join(self.proc_root, "meminfo"))
            memory = parse_meminfo(mem_text) if mem_text else None
            cpu_pct, iowait_pct = self._cpu_shares(now)
            io_pressure = parse_pressure(
                self._read(os.path.join(self.proc_root, "pressure", "io"))
            )
            mem_pressure = parse_pressure(
                self._read(os.path.join(self.proc_root, "pressure", "memory"))
            )
            host = {
                "cpu_pct": cpu_pct,
                "load1": parse_loadavg(load_text) if load_text else None,
                "mem_total_bytes": None,
                "mem_used_bytes": None,
                "mem_cached_bytes": None,
                "swap_total_bytes": None,
                "swap_used_bytes": None,
                "dirty_bytes": None,
                "writeback_bytes": None,
                "net_rx_bps": self._rate("net.rx", net[0] if net else None, now),
                "net_tx_bps": self._rate("net.tx", net[1] if net else None, now),
                "disk_read_bps": self._rate(
                    "disk.read", disk["read_bytes"] if disk else None, now
                ),
                "disk_write_bps": self._rate(
                    "disk.write", disk["write_bytes"] if disk else None, now
                ),
                "disk_read_iops": self._rate(
                    "disk.reads", disk["reads"] if disk else None, now
                ),
                "disk_write_iops": self._rate(
                    "disk.writes", disk["writes"] if disk else None, now
                ),
                "disk_util_pct": self._disk_util_pct(
                    disk["io_ticks"] if disk else {}, now
                ),
                "disk_inflight": None if disk is None else float(disk["inflight"]),
                "iowait_pct": iowait_pct,
                "io_pressure_pct": io_pressure,
                "mem_pressure_pct": mem_pressure,
                "cpu_watts": self._rapl_watts(now),
                "disks": self._disks(),
            }
            if memory:
                host.update(memory)
            return {"version": 1, "host": host, "gpus": self._gpus()}


class AgentHandler(BaseHTTPRequestHandler):
    server_version = "machineview-agent/1"
    collector = None

    def do_GET(self):  # noqa: N802 (http.server contract)
        if self.path.split("?", 1)[0] not in ("/sample", "/"):
            self.send_error(404, "not found")
            return
        body = json.dumps(self.collector.sample()).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):  # noqa: A002 (http.server contract)
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--mounts",
        default="/",
        help="comma-separated mount points reported as storage",
    )
    parser.add_argument(
        "--allow-remote",
        action="store_true",
        help="permit binding a non-loopback address (off by default)",
    )
    args = parser.parse_args()
    if not args.allow_remote and not args.bind.startswith("127."):
        parser.error(
            "refusing non-loopback bind %r without --allow-remote" % args.bind
        )
    mounts = [mount for mount in args.mounts.split(",") if mount]
    AgentHandler.collector = Collector(mounts)
    server = ThreadingHTTPServer((args.bind, args.port), AgentHandler)
    print(
        "machineview agent on http://%s:%d/sample (mounts: %s, nvidia-smi: %s)"
        % (
            args.bind,
            args.port,
            ",".join(mounts),
            "yes" if AgentHandler.collector.nvidia_smi else "no",
        ),
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
