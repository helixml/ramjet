#!/usr/bin/env python3
"""Fail-closed node06 direct-P2P prerequisite harness.

The default is read-only preflight. GPU work and LB recreation require an
explicit run flag plus an exact production-risk acknowledgement.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
import urllib.request
from dataclasses import dataclass
from typing import Any


R34_IMAGE_ID = "sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b"
R34_REPO_DIGEST = "voipmonitor/vllm@" + R34_IMAGE_ID
NVBANDWIDTH_SHA = "82fc4e8c6afa0babb8687793678f615b3b8d793e"
NCCL_TESTS_SHA = "717b68318278e93f371d8ffb46b076069d7c7851"
EXPECTED_DRIVER = "595.84"
CONTROL_CONTAINER = "dspark-0731"
TARGET_CONTAINER = "dspark-0731-b"
LB_CONTAINER = "ds4-loadbalancer"
COMPOSE_DIR = pathlib.Path("/home/luke/inference/dspark_0731")
COMPOSE_FILE = COMPOSE_DIR / "docker-compose.yaml"
PROFILE_ACK = "I_ACKNOWLEDGE_NODE06_PRODUCTION_RISK"
MIN_QUIET_SECONDS = 60
MIN_FREE_MIB = 1536
ANSI = re.compile(r"\x1b\[[0-9;]*m")
BANDWIDTH_TESTS = (
    "device_to_device_memcpy_read_sm",
    "device_to_device_memcpy_write_sm",
    "device_to_device_memcpy_read_ce",
    "device_to_device_memcpy_write_ce",
)
FULL_TESTS = BANDWIDTH_TESTS + ("device_to_device_latency_sm",)


class GateError(RuntimeError):
    pass


def run(
    command: list[str],
    *,
    env: dict[str, str] | None = None,
    timeout: int = 30,
) -> str:
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise GateError(f"command failed ({command[0]}): {completed.stderr.strip()}")
    return completed.stdout


def docker_inspect(name: str) -> dict[str, Any]:
    document = json.loads(run(["docker", "inspect", name]))
    if len(document) != 1:
        raise GateError(f"expected one Docker object for {name}")
    return document[0]


def device_ids(container: dict[str, Any]) -> list[int]:
    values: list[int] = []
    for request in container.get("HostConfig", {}).get("DeviceRequests") or []:
        capabilities = request.get("Capabilities") or []
        if request.get("Driver") == "nvidia" or any(
            "gpu" in group for group in capabilities
        ):
            for value in request.get("DeviceIDs") or []:
                if not str(value).isdigit():
                    raise GateError("GPU reservation contains a non-numeric device ID")
                values.append(int(value))
    if len(values) != 4 or len(set(values)) != 4:
        raise GateError("engine must reserve exactly four unique GPUs")
    return values


def gpu_inventory() -> dict[int, dict[str, Any]]:
    fields = (
        "index,uuid,pci.bus_id,driver_version,memory.free,utilization.gpu,"
        "utilization.memory"
    )
    output = run(
        [
            "nvidia-smi",
            f"--query-gpu={fields}",
            "--format=csv,noheader,nounits",
        ]
    )
    inventory = {}
    for raw in output.splitlines():
        parts = [part.strip() for part in raw.split(",")]
        if len(parts) != 7:
            raise GateError("unexpected nvidia-smi inventory shape")
        index = int(parts[0])
        uuid = parts[1]
        if not re.fullmatch(
            r"GPU-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
            r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
            uuid,
        ):
            raise GateError("nvidia-smi returned an invalid GPU UUID")
        inventory[index] = {
            "uuid": uuid,
            "pci_bus_id": parts[2].lower(),
            "driver": parts[3],
            "free_mib": int(parts[4]),
            "gpu_utilization": int(parts[5]),
            "memory_utilization": int(parts[6]),
        }
    return inventory


def parse_topology_matrix(output: str) -> tuple[list[str], dict[str, list[str]]]:
    lines = [ANSI.sub("", line).strip() for line in output.splitlines()]
    header: list[str] | None = None
    rows: dict[str, list[str]] = {}
    for line in lines:
        fields = line.split()
        if not fields:
            continue
        if header is None and fields[0] == "GPU0":
            header = [field for field in fields if field.startswith("GPU")]
            continue
        if header is not None and re.fullmatch(r"GPU\d+", fields[0]):
            rows[fields[0]] = fields[1 : 1 + len(header)]
    if not header or len(rows) < len(header):
        raise GateError("could not parse NVIDIA topology matrix")
    return header, rows


def validate_pair_matrix(indices: list[int], command: list[str], expected: str) -> str:
    output = run(command)
    header, rows = parse_topology_matrix(output)
    positions = {name: position for position, name in enumerate(header)}
    for left in indices:
        for right in indices:
            if left == right:
                continue
            row = f"GPU{left}"
            column = f"GPU{right}"
            if row not in rows or column not in positions:
                raise GateError(f"topology matrix omits {row}->{column}")
            position = positions[column]
            if position >= len(rows[row]):
                raise GateError(f"topology matrix truncates {row}->{column}")
            if rows[row][position] != expected:
                raise GateError(f"{row}->{column} is not {expected} for {command[-1]}")
    return output


def container_pids(name: str) -> set[int]:
    output = run(["docker", "top", name, "-eo", "pid="])
    return {int(value.strip()) for value in output.splitlines() if value.strip().isdigit()}


def compute_processes() -> dict[str, set[int]]:
    output = run(
        [
            "nvidia-smi",
            "--query-compute-apps=gpu_uuid,pid",
            "--format=csv,noheader,nounits",
        ]
    )
    result: dict[str, set[int]] = {}
    for raw in output.splitlines():
        if not raw.strip():
            continue
        uuid, pid = [part.strip() for part in raw.split(",", 1)]
        result.setdefault(uuid, set()).add(int(pid))
    return result


@dataclass(frozen=True)
class Preflight:
    target_indices: tuple[int, ...]
    target_uuids: tuple[str, ...]
    target_buses: tuple[str, ...]
    free_mib: tuple[int, ...]
    topology: str
    peer_read: str
    peer_write: str


def preflight(*, active: bool) -> Preflight:
    if run(["hostname"]).strip() != "node06":
        raise GateError("this harness may run only on node06")
    if not COMPOSE_FILE.is_file():
        raise GateError("canonical node06 Compose file is absent")

    control = docker_inspect(CONTROL_CONTAINER)
    target = docker_inspect(TARGET_CONTAINER)
    for name, container in ((CONTROL_CONTAINER, control), (TARGET_CONTAINER, target)):
        if not container.get("State", {}).get("Running"):
            raise GateError(f"{name} is not running")
        if container.get("Image") != R34_IMAGE_ID:
            raise GateError(f"{name} is not the exact qualified r34 image")
        if int(container.get("RestartCount", -1)) != 0:
            raise GateError(f"{name} restart count is not zero")

    control_ids = device_ids(control)
    target_ids = device_ids(target)
    if set(control_ids) & set(target_ids):
        raise GateError("control and target GPU reservations overlap")
    if target.get("HostConfig", {}).get("CpusetCpus") != "12-23,36-47":
        raise GateError("target engine is not pinned to its qualified NUMA CPUs")

    inventory = gpu_inventory()
    try:
        target_gpus = [inventory[index] for index in target_ids]
    except KeyError as exc:
        raise GateError("reserved target GPU is absent from nvidia-smi") from exc
    if any(gpu["driver"] != EXPECTED_DRIVER for gpu in target_gpus):
        raise GateError("target GPU driver identity changed")

    engine_pids = container_pids(TARGET_CONTAINER)
    processes = compute_processes()
    for gpu in target_gpus:
        owners = processes.get(gpu["uuid"], set())
        if not owners or not owners.issubset(engine_pids):
            raise GateError("target GPU has an absent or foreign compute owner")
        if active and gpu["free_mib"] < MIN_FREE_MIB:
            raise GateError("target GPU lacks the required memory headroom")

    topology = validate_pair_matrix(
        target_ids, ["nvidia-smi", "topo", "-m"], "NODE"
    )
    peer_read = validate_pair_matrix(
        target_ids, ["nvidia-smi", "topo", "-p2p", "r"], "OK"
    )
    peer_write = validate_pair_matrix(
        target_ids, ["nvidia-smi", "topo", "-p2p", "w"], "OK"
    )
    return Preflight(
        target_indices=tuple(target_ids),
        target_uuids=tuple(gpu["uuid"] for gpu in target_gpus),
        target_buses=tuple(gpu["pci_bus_id"] for gpu in target_gpus),
        free_mib=tuple(gpu["free_mib"] for gpu in target_gpus),
        topology=topology,
        peer_read=peer_read,
        peer_write=peer_write,
    )


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_tools(directory: pathlib.Path) -> None:
    manifest_path = directory / "manifest.json"
    if manifest_path.is_symlink():
        raise GateError("tool manifest may not be a symlink")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise GateError("tool manifest is absent or malformed") from exc
    expected = {
        "schema_version": 1,
        "nvbandwidth_commit": NVBANDWIDTH_SHA,
        "nccl_tests_commit": NCCL_TESTS_SHA,
        "runtime_image": R34_REPO_DIGEST,
        "cuda_architecture": "120",
    }
    for key, value in expected.items():
        if manifest.get(key) != value:
            raise GateError(f"tool manifest identity mismatch: {key}")
    for name in ("nvbandwidth", "all_reduce_perf"):
        path = directory / name
        if path.is_symlink():
            raise GateError(f"{name} may not be a symlink")
        metadata = path.stat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o022:
            raise GateError(f"{name} is absent or writable by group/world")
        if metadata.st_mode & 0o111 == 0:
            raise GateError(f"{name} is not executable")
        recorded = manifest.get("binaries", {}).get(name, {}).get("sha256")
        if recorded != sha256(path):
            raise GateError(f"{name} digest mismatch")


def prometheus_snapshot(port: int) -> dict[str, float]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=3) as response:
        text = response.read(16 * 1024 * 1024).decode("utf-8")
    result = {
        "running": 0.0,
        "waiting": 0.0,
        "prompt_tokens": 0.0,
        "generation_tokens": 0.0,
        "requests": 0.0,
    }
    mapping = {
        "vllm:num_requests_running": "running",
        "vllm:num_requests_waiting": "waiting",
        "vllm:prompt_tokens_total": "prompt_tokens",
        "vllm:generation_tokens_total": "generation_tokens",
        "vllm:request_success_total": "requests",
    }
    seen = set()
    for line in text.splitlines():
        for metric, key in mapping.items():
            if line.startswith(metric + "{") or line.startswith(metric + " "):
                result[key] += float(line.rsplit(None, 1)[1])
                seen.add(key)
                break
    if seen != set(result):
        raise GateError("engine metric surface is incomplete")
    return result


def health_json(port: int) -> dict[str, Any]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=3) as response:
        return json.loads(response.read(1024 * 1024))


def compose_environment() -> tuple[dict[str, str], dict[str, Any], str]:
    environment = os.environ.copy()
    output = run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "config", "--format", "json"],
        env=environment,
    )
    document = json.loads(output)
    service = document.get("services", {}).get(LB_CONTAINER)
    if not service:
        raise GateError("Compose does not contain the load balancer")
    current = docker_inspect(LB_CONTAINER)
    rendered_image = service.get("image")
    rendered_id = json.loads(run(["docker", "image", "inspect", rendered_image]))[0]["Id"]
    if current.get("Image") != rendered_id:
        raise GateError("running LB image differs from rendered Compose")
    current_env = dict(
        item.split("=", 1)
        for item in current["Config"].get("Env", [])
        if "=" in item
    )
    rendered_env = service.get("environment") or {}
    if any(current_env.get(key) != str(value) for key, value in rendered_env.items()):
        raise GateError("running LB environment differs from rendered Compose")
    return environment, current, rendered_image


def write_private(path: pathlib.Path, text: str) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write(text)


def run_compose(environment: dict[str, str]) -> None:
    run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "up",
            "-d",
            "--no-deps",
            LB_CONTAINER,
        ],
        env=environment,
        timeout=60,
    )


def wait_for_health(expected_replicas: int, timeout: int = 30) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            health = health_json(8006)
            if (
                health.get("healthy_replicas") == expected_replicas
                and health.get("total_replicas") == expected_replicas
            ):
                return
        except Exception:
            pass
        time.sleep(1)
    raise GateError(f"LB did not reach {expected_replicas}/{expected_replicas} health")


def quiet_fence(seconds: int) -> tuple[dict[str, float], dict[str, float]]:
    if seconds < MIN_QUIET_SECONDS:
        raise GateError("quiet fence may not be shorter than 60 seconds")
    start = prometheus_snapshot(8013)
    if start["running"] or start["waiting"]:
        raise GateError("target engine is not idle at quiet-fence start")
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        time.sleep(min(10, max(0.1, deadline - time.monotonic())))
        current = prometheus_snapshot(8013)
        if current["running"] or current["waiting"]:
            raise GateError("target engine received work during quiet fence")
    end = prometheus_snapshot(8013)
    for key in ("prompt_tokens", "generation_tokens", "requests"):
        if end[key] != start[key]:
            raise GateError(f"target engine {key} changed during quiet fence")
    return start, end


def container_base(
    name: str, uuids: tuple[str, ...], tools: pathlib.Path
) -> list[str]:
    gpu_request = '"device=' + ",".join(uuids) + '"'
    return [
        "docker",
        "run",
        "--rm",
        "--name",
        name,
        "--label",
        "org.helixml.mini-dynamo.scope=phase-b-offline",
        "--network",
        "none",
        "--ipc",
        "private",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
        "--pids-limit",
        "256",
        "--memory",
        "4g",
        "--cpus",
        "4",
        "--cpuset-cpus",
        "12-23,36-47",
        "--cpuset-mems",
        "1",
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,nodev,size=64m,mode=0700",
        "--gpus",
        gpu_request,
        "--volume",
        f"{tools}:/tools:ro",
    ]


def run_benchmark(
    command: list[str],
    *,
    name: str,
    output: pathlib.Path,
    timeout: int,
) -> None:
    descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        process = subprocess.Popen(command, stdout=handle, stderr=subprocess.STDOUT, text=True)
        deadline = time.monotonic() + timeout
        try:
            while process.poll() is None:
                if time.monotonic() >= deadline:
                    raise GateError(f"benchmark {name} exceeded {timeout}s")
                health = health_json(8006)
                if (
                    health.get("healthy_replicas") != 1
                    or health.get("total_replicas") != 1
                ):
                    raise GateError("control serving health changed during benchmark")
                time.sleep(1)
            if process.returncode != 0:
                raise GateError(f"benchmark {name} exited {process.returncode}")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                subprocess.run(
                    ["docker", "rm", "-f", name],
                    check=False,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )


def validate_nvbandwidth_output(
    path: pathlib.Path, expected: tuple[str, ...], gpu_count: int
) -> None:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        root = document["nvbandwidth"]
        cases = root["testcases"]
    except (OSError, json.JSONDecodeError, KeyError, TypeError) as exc:
        raise GateError("nvbandwidth output is absent or malformed") from exc
    if root.get("error"):
        raise GateError("nvbandwidth reported a top-level error")
    if not isinstance(cases, list):
        raise GateError("nvbandwidth testcases are not a list")
    by_name = {
        case.get("name"): case for case in cases if isinstance(case, dict)
    }
    if set(by_name) != set(expected):
        raise GateError("nvbandwidth did not emit exactly the requested testcases")
    for name in expected:
        case = by_name[name]
        if case.get("status") != "Passed" or case.get("error"):
            raise GateError(f"nvbandwidth testcase did not pass: {name}")
        matrix = case.get("bandwidth_matrix")
        if not isinstance(matrix, list) or len(matrix) != gpu_count:
            raise GateError(f"nvbandwidth testcase has a malformed matrix: {name}")
        for row_index, row in enumerate(matrix):
            if not isinstance(row, list) or len(row) != gpu_count:
                raise GateError(f"nvbandwidth testcase has a malformed row: {name}")
            for column_index, value in enumerate(row):
                if row_index == column_index:
                    continue
                try:
                    numeric = float(value)
                except (TypeError, ValueError) as exc:
                    raise GateError(
                        f"nvbandwidth testcase omits a directed pair: {name}"
                    ) from exc
                if numeric <= 0:
                    raise GateError(
                        f"nvbandwidth testcase has a non-positive pair: {name}"
                    )


def capture_metadata(result: pathlib.Path, state: Preflight, tools: pathlib.Path) -> None:
    summary = {
        "schema_version": 1,
        "r34_image": R34_REPO_DIGEST,
        "nvbandwidth_commit": NVBANDWIDTH_SHA,
        "nccl_tests_commit": NCCL_TESTS_SHA,
        "target_indices_discovered": state.target_indices,
        "target_uuids": state.target_uuids,
        "target_buses": state.target_buses,
        "free_mib": state.free_mib,
        "tool_manifest_sha256": sha256(tools / "manifest.json"),
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    write_private(result / "identity.json", json.dumps(summary, indent=2) + "\n")
    write_private(result / "topology.txt", state.topology)
    write_private(result / "peer-read.txt", state.peer_read)
    write_private(result / "peer-write.txt", state.peer_write)
    write_private(
        result / "nvidia-params.txt",
        run(
            [
                "grep",
                "-E",
                "RegistryDwords|DmaRemapPeerMmio|EnableResizableBar",
                "/proc/driver/nvidia/params",
            ]
        ),
    )
    for bus in state.target_buses:
        safe_bus = bus.replace(":", "_").replace(".", "_")
        write_private(
            result / f"pci-{safe_bus}.txt",
            run(["lspci", "-vv", "-s", bus], timeout=10),
        )


def active_run(args: argparse.Namespace, state: Preflight) -> pathlib.Path:
    tools = args.tools_dir.resolve()
    validate_tools(tools)
    baseline_env, _, rendered_image = compose_environment()
    wait_for_health(2)
    result = pathlib.Path(tempfile.mkdtemp(prefix="mini-dynamo-p2p-phase-b.", dir="/tmp"))
    result.chmod(0o700)
    print(f"private active result directory: {result}", file=sys.stderr)
    capture_metadata(result, state, tools)

    restore_needed = False
    interrupted = False

    def mark_interrupted(_signum: int, _frame: Any) -> None:
        nonlocal interrupted
        interrupted = True
        raise KeyboardInterrupt

    old_handlers = {
        signum: signal.signal(signum, mark_interrupted)
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    }
    try:
        single_env = baseline_env.copy()
        single_env.update(
            {
                "LB_IMAGE": rendered_image,
                "DS4_UPSTREAM": "http://dspark-0731:8000",
                "DS4_KV_EVENT_LIVE_ENDPOINTS": "tcp://dspark-0731:5557",
                "DS4_KV_EVENT_REPLAY_ENDPOINTS": "tcp://dspark-0731:5558",
            }
        )
        restore_needed = True
        run_compose(single_env)
        wait_for_health(1)
        current = docker_inspect(LB_CONTAINER)
        current_env = dict(
            item.split("=", 1)
            for item in current["Config"].get("Env", [])
            if "=" in item
        )
        if current_env.get("DS4_UPSTREAM") != "http://dspark-0731:8000":
            raise GateError("LB did not become single-homed on the control")
        start, end = quiet_fence(args.quiet_seconds)
        write_private(
            result / "quiet-fence.json",
            json.dumps(
                {"seconds": args.quiet_seconds, "start": start, "end": end},
                indent=2,
            )
            + "\n",
        )
        state = preflight(active=True)

        token = result.name.rsplit(".", 1)[-1]
        scout_name = f"md-p2p-scout-{token}"
        scout = container_base(scout_name, state.target_uuids[:2], tools)
        scout.extend(
            [
                "--entrypoint",
                "/tools/nvbandwidth",
                R34_REPO_DIGEST,
                "-b",
                "1",
                "-i",
                "1",
                "-F",
                "json",
            ]
        )
        scout.extend(["-t", *BANDWIDTH_TESTS])
        run_benchmark(
            scout,
            name=scout_name,
            output=result / "nvbandwidth-scout.json",
            timeout=60,
        )
        validate_nvbandwidth_output(
            result / "nvbandwidth-scout.json", BANDWIDTH_TESTS, 2
        )

        if args.run_full_prerequisite:
            for cycle in range(1, args.cycles + 1):
                full_name = f"md-p2p-full-{token}-{cycle}"
                full = container_base(full_name, state.target_uuids, tools)
                full.extend(
                    [
                        "--entrypoint",
                        "/tools/nvbandwidth",
                        R34_REPO_DIGEST,
                        "-b",
                        "64",
                        "-i",
                        "5",
                        "-F",
                        "json",
                    ]
                )
                tests = list(FULL_TESTS)
                if cycle % 2 == 0:
                    tests.reverse()
                full.extend(["-t", *tests])
                run_benchmark(
                    full,
                    name=full_name,
                    output=result / f"nvbandwidth-full-{cycle}.json",
                    timeout=180,
                )
                validate_nvbandwidth_output(
                    result / f"nvbandwidth-full-{cycle}.json", FULL_TESTS, 4
                )

            nccl_name = f"md-p2p-nccl-{token}"
            nccl = container_base(nccl_name, state.target_uuids, tools)
            nccl.extend(
                [
                    "--env",
                    "LD_LIBRARY_PATH=/opt/venv/lib/python3.12/site-packages/"
                    "nvidia/nccl/lib:/usr/local/cuda/lib64",
                    "--env",
                    "NCCL_DEBUG=INFO",
                    "--env",
                    "NCCL_DEBUG_SUBSYS=INIT,GRAPH,P2P",
                    "--env",
                    "NCCL_P2P_DISABLE=0",
                    "--entrypoint",
                    "/tools/all_reduce_perf",
                    R34_REPO_DIGEST,
                    "-b",
                    "8K",
                    "-e",
                    "8M",
                    "-f",
                    "2",
                    "-g",
                    "4",
                    "-n",
                    "20",
                    "-w",
                    "5",
                    "-c",
                    "1",
                    "-T",
                    "60",
                ]
            )
            run_benchmark(
                nccl,
                name=nccl_name,
                output=result / "nccl-all-reduce.txt",
                timeout=120,
            )

        final_target = prometheus_snapshot(8013)
        if final_target["running"] or final_target["waiting"]:
            raise GateError("target engine is active after benchmark")
        for key in ("prompt_tokens", "generation_tokens", "requests"):
            if final_target[key] != end[key]:
                raise GateError(f"target engine {key} changed during benchmark")
        if interrupted:
            raise GateError("run interrupted")
        return result
    finally:
        restore_error: Exception | None = None
        if restore_needed:
            try:
                run_compose(baseline_env)
                wait_for_health(2)
                compose_environment()
            except Exception as exc:
                restore_error = exc
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)
        if restore_error is not None:
            raise GateError(f"CRITICAL: LB restoration failed: {restore_error}") from restore_error


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    mode = result.add_mutually_exclusive_group()
    mode.add_argument("--run-gpu-scout", action="store_true")
    mode.add_argument("--run-full-prerequisite", action="store_true")
    result.add_argument("--acknowledge-production-risk")
    result.add_argument(
        "--tools-dir", type=pathlib.Path, default=pathlib.Path("/tmp/mini-dynamo-p2p-tools")
    )
    result.add_argument("--quiet-seconds", type=int, default=MIN_QUIET_SECONDS)
    result.add_argument("--cycles", type=int, choices=range(1, 4), default=1)
    result.add_argument("--print-plan", action="store_true")
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    active = args.run_gpu_scout or args.run_full_prerequisite
    if args.print_plan:
        print(
            json.dumps(
                {
                    "default": "read-only preflight",
                    "active": active,
                    "full": args.run_full_prerequisite,
                    "quiet_seconds": args.quiet_seconds,
                    "cycles": args.cycles,
                    "runtime_estimate": "scout 2-3m; full 6-8m including fence/restore",
                },
                sort_keys=True,
            )
        )
        return 0
    if active and args.acknowledge_production_risk != PROFILE_ACK:
        print(
            "active mode requires --acknowledge-production-risk " + PROFILE_ACK,
            file=sys.stderr,
        )
        return 2
    if active and args.quiet_seconds < MIN_QUIET_SECONDS:
        print("active mode requires at least a 60-second quiet fence", file=sys.stderr)
        return 2
    try:
        state = preflight(active=active)
        summary = {
            "mode": "active" if active else "read-only-preflight",
            "target_indices_discovered": state.target_indices,
            "target_uuids": state.target_uuids,
            "target_buses": state.target_buses,
            "free_mib": state.free_mib,
            "r34_image": R34_REPO_DIGEST,
            "nvbandwidth_commit": NVBANDWIDTH_SHA,
            "nccl_tests_commit": NCCL_TESTS_SHA,
        }
        print(json.dumps(summary, sort_keys=True))
        if not active:
            print("read-only preflight complete; no LB or GPU work was performed")
            return 0
        result = active_run(args, state)
        print(f"active prerequisite complete; private results: {result}")
        return 0
    except KeyboardInterrupt:
        print("phase-B prerequisite interrupted and failed closed", file=sys.stderr)
        return 130
    except (GateError, OSError, ValueError, subprocess.TimeoutExpired) as exc:
        print(f"phase-B prerequisite failed closed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
