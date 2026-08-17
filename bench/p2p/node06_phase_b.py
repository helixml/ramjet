#!/usr/bin/env python3
"""Fail-closed node06 direct-P2P prerequisite harness.

The default is read-only preflight. GPU work and LB recreation require an
explicit run flag plus an exact production-risk acknowledgement.
"""

from __future__ import annotations

import argparse
import copy
import fcntl
import hashlib
import json
import math
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
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Callable

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from node06_operational_moratorium import (  # noqa: E402
    MoratoriumError,
    require_active_work_permitted,
)


R34_IMAGE_ID = "sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b"
R34_REPO_DIGEST = "voipmonitor/vllm@" + R34_IMAGE_ID
NVBANDWIDTH_SHA = "82fc4e8c6afa0babb8687793678f615b3b8d793e"
NCCL_TESTS_SHA = "717b68318278e93f371d8ffb46b076069d7c7851"
EXPECTED_DRIVER = "595.84"
CONTROL_CONTAINER = "dspark-0731"
TARGET_CONTAINER = "dspark-0731-b"
LB_CONTAINER = "ds4-loadbalancer"
HARNESS_OWNER_LABEL = "org.helixml.ramjet.p2p-phase-b-owner"
DEPLOYMENT_LOCK = pathlib.Path("/run/lock/ramjet-node06-deployment.lock")
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


class DeferredSignal(BaseException):
    def __init__(self, signum: int):
        super().__init__(f"received signal {signum}")
        self.signum = signum


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
            # `topo -m` appends columns such as "GPU NUMA ID". Only numbered
            # GPU labels belong to the adjacency matrix.
            header = [field for field in fields if re.fullmatch(r"GPU\d+", field)]
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
    # Docker forwards ps arguments, but node06's daemon rejects the GNU-style
    # empty `pid=` header override. Keep the portable header and ignore it.
    output = run(["docker", "top", name, "-eo", "pid"])
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


def sha256_fd(descriptor: int) -> str:
    digest = hashlib.sha256()
    os.lseek(descriptor, 0, os.SEEK_SET)
    while chunk := os.read(descriptor, 1024 * 1024):
        digest.update(chunk)
    os.lseek(descriptor, 0, os.SEEK_SET)
    return digest.hexdigest()


def verified_fd(directory_fd: int, name: str, *, owner_uid: int) -> int:
    try:
        descriptor = os.open(
            name,
            os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
            dir_fd=directory_fd,
        )
    except OSError as exc:
        raise GateError(f"verified tool file cannot be opened safely: {name}") from exc
    metadata = os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise GateError(f"verified tool file is not regular: {name}")
    if metadata.st_uid != owner_uid or metadata.st_mode & 0o222:
        os.close(descriptor)
        raise GateError(f"verified tool file is not owner-locked: {name}")
    return descriptor


def copy_verified_fd(descriptor: int, destination: pathlib.Path, mode: int) -> None:
    output = os.open(
        destination,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
        mode,
    )
    try:
        os.lseek(descriptor, 0, os.SEEK_SET)
        while chunk := os.read(descriptor, 1024 * 1024):
            view = memoryview(chunk)
            while view:
                written = os.write(output, view)
                view = view[written:]
        os.fsync(output)
    finally:
        os.close(output)
        os.lseek(descriptor, 0, os.SEEK_SET)


def validate_and_stage_tools(
    directory: pathlib.Path,
    expected_manifest_sha256: str,
    stage: pathlib.Path,
    *,
    owner_uid: int = 0,
) -> pathlib.Path:
    if not re.fullmatch(r"[0-9a-f]{64}", expected_manifest_sha256):
        raise GateError("expected tool manifest SHA-256 must be 64 lowercase hex digits")
    try:
        directory_metadata = os.lstat(directory)
    except OSError as exc:
        raise GateError("verified tool directory is absent") from exc
    if (
        not stat.S_ISDIR(directory_metadata.st_mode)
        or stat.S_ISLNK(directory_metadata.st_mode)
        or directory_metadata.st_uid != owner_uid
        or directory_metadata.st_mode & 0o222
    ):
        raise GateError("verified tool directory is not owner-locked")
    directory_fd = os.open(
        directory,
        os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW,
    )
    descriptors: dict[str, int] = {}
    try:
        for name in ("manifest.json", "nvbandwidth", "all_reduce_perf"):
            descriptors[name] = verified_fd(directory_fd, name, owner_uid=owner_uid)
        manifest_fd = descriptors["manifest.json"]
        if sha256_fd(manifest_fd) != expected_manifest_sha256:
            raise GateError("tool manifest differs from the external expected SHA-256")
        manifest_bytes = os.read(manifest_fd, 1024 * 1024)
        if os.read(manifest_fd, 1):
            raise GateError("tool manifest exceeds 1MiB")
        try:
            manifest = json.loads(manifest_bytes)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise GateError("tool manifest is malformed") from exc
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
            metadata = os.fstat(descriptors[name])
            if metadata.st_mode & 0o111 == 0:
                raise GateError(f"verified tool is not executable: {name}")
            recorded = manifest.get("binaries", {}).get(name, {}).get("sha256")
            if recorded != sha256_fd(descriptors[name]):
                raise GateError(f"verified tool digest mismatch: {name}")

        stage.mkdir(mode=0o700)
        copy_verified_fd(manifest_fd, stage / "manifest.json", 0o444)
        for name in ("nvbandwidth", "all_reduce_perf"):
            copy_verified_fd(descriptors[name], stage / name, 0o555)
        stage.chmod(0o555)
        return stage
    finally:
        for descriptor in descriptors.values():
            os.close(descriptor)
        os.close(directory_fd)


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


@dataclass(frozen=True)
class ComposeBaseline:
    render_path: pathlib.Path
    single_path: pathlib.Path
    render_sha256: str
    project_name: str
    source_identities: dict[str, str]
    runtime_spec: dict[str, Any]
    service_hash: str
    restore_service_hash: str
    owner_id: str
    single_service_hash: str


def env_map(values: list[str]) -> dict[str, str]:
    result: dict[str, str] = {}
    for item in values:
        if "=" not in item:
            raise GateError("container image has an environment entry without a value")
        key, value = item.split("=", 1)
        if key in result:
            raise GateError(f"container environment repeats {key}")
        result[key] = value
    return result


def expected_service_env(service: dict[str, Any], image: str) -> dict[str, str]:
    image_document = json.loads(run(["docker", "image", "inspect", image]))
    if len(image_document) != 1:
        raise GateError("rendered LB image identity is ambiguous")
    result = env_map(image_document[0].get("Config", {}).get("Env") or [])
    rendered = service.get("environment") or {}
    if not isinstance(rendered, dict):
        raise GateError("rendered LB environment is not a mapping")
    for key, value in rendered.items():
        result[str(key)] = "" if value is None else str(value)
    return result


def runtime_lb_spec(container: dict[str, Any]) -> dict[str, Any]:
    config = container.get("Config") or {}
    host = container.get("HostConfig") or {}
    mounts = []
    for mount in container.get("Mounts") or []:
        mounts.append(
            {
                key: mount.get(key)
                for key in ("Type", "Source", "Destination", "Mode", "RW", "Propagation")
            }
        )
    return {
        "image_id": container.get("Image"),
        "config": {
            "env": sorted(config.get("Env") or []),
            "cmd": config.get("Cmd"),
            "entrypoint": config.get("Entrypoint"),
            "working_dir": config.get("WorkingDir"),
            "user": config.get("User"),
            "labels": {
                key: value
                for key, value in (config.get("Labels") or {}).items()
                if key
                not in {
                    "com.docker.compose.config-hash",
                    "com.docker.compose.project.config_files",
                    "com.docker.compose.project.working_dir",
                }
            },
        },
        "host": {
            key: host.get(key)
            for key in (
                "Binds",
                "CapAdd",
                "CapDrop",
                "CpusetCpus",
                "CpusetMems",
                "DeviceRequests",
                "IpcMode",
                "Memory",
                "NanoCpus",
                "NetworkMode",
                "PidsLimit",
                "PortBindings",
                "Privileged",
                "ReadonlyRootfs",
                "RestartPolicy",
                "SecurityOpt",
                "ShmSize",
            )
        },
        "mounts": sorted(mounts, key=lambda mount: str(mount.get("Destination"))),
    }


def source_identities() -> dict[str, str]:
    result = {}
    for path in (COMPOSE_FILE, COMPOSE_DIR / ".env"):
        metadata = os.lstat(path)
        if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
            raise GateError(f"Compose source is not a regular non-symlink: {path.name}")
        result[path.name] = sha256(path)
    return result


def render_compose(compose_file: pathlib.Path) -> tuple[str, dict[str, Any]]:
    output = run(
        ["docker", "compose", "-f", str(compose_file), "config", "--format", "json"]
    )
    try:
        document = json.loads(output)
    except json.JSONDecodeError as exc:
        raise GateError("Docker Compose rendered malformed JSON") from exc
    return output, document


def compose_service_hash(compose_file: pathlib.Path, project_name: str) -> str:
    output = run(
        [
            "docker",
            "compose",
            "--project-name",
            project_name,
            "-f",
            str(compose_file),
            "config",
            "--hash",
            LB_CONTAINER,
        ]
    ).strip()
    fields = output.split()
    value = fields[-1] if fields else ""
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise GateError("Docker Compose did not emit a service config hash")
    return value


def validate_rendered_runtime(
    document: dict[str, Any], current: dict[str, Any], service_hash: str
) -> None:
    service = document.get("services", {}).get(LB_CONTAINER)
    if not service:
        raise GateError("Compose does not contain the load balancer")
    rendered_image = service.get("image")
    rendered_id = json.loads(run(["docker", "image", "inspect", rendered_image]))[0]["Id"]
    if current.get("Image") != rendered_id:
        raise GateError("running LB image differs from rendered Compose")
    expected_env = expected_service_env(service, rendered_image)
    current_env = env_map(current.get("Config", {}).get("Env") or [])
    if current_env != expected_env:
        raise GateError("running LB environment has missing or unexpected entries")
    labels = current.get("Config", {}).get("Labels") or {}
    if labels.get("com.docker.compose.config-hash") != service_hash:
        raise GateError("running LB full Compose service hash differs from render")


def capture_compose_baseline(result: pathlib.Path, owner_id: str) -> ComposeBaseline:
    identities = source_identities()
    rendered_text, document = render_compose(COMPOSE_FILE)
    project_name = str(document.get("name") or "")
    if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", project_name):
        raise GateError("rendered Compose project name is absent or unsafe")
    render_path = result / "compose-baseline.json"
    write_private(render_path, rendered_text)
    service_hash = compose_service_hash(COMPOSE_FILE, project_name)
    restore_service_hash = compose_service_hash(render_path, project_name)
    current = docker_inspect(LB_CONTAINER)
    validate_rendered_runtime(document, current, service_hash)
    runtime_spec = runtime_lb_spec(current)
    write_private(
        result / "compose-baseline-runtime.json",
        json.dumps(runtime_spec, indent=2, sort_keys=True) + "\n",
    )

    single_document = copy.deepcopy(document)
    service = single_document["services"][LB_CONTAINER]
    labels = service.setdefault("labels", {})
    labels[HARNESS_OWNER_LABEL] = owner_id
    environment = service.setdefault("environment", {})
    environment.update(
        {
            "RJ_UPSTREAM": "http://dspark-0731:8000",
            "RJ_KV_EVENT_LIVE_ENDPOINTS": "tcp://dspark-0731:5557",
            "RJ_KV_EVENT_REPLAY_ENDPOINTS": "tcp://dspark-0731:5558",
        }
    )
    single_path = result / "compose-single-home.json"
    write_private(single_path, json.dumps(single_document, indent=2) + "\n")
    single_service_hash = compose_service_hash(single_path, project_name)
    baseline = ComposeBaseline(
        render_path=render_path,
        single_path=single_path,
        render_sha256=sha256(render_path),
        project_name=project_name,
        source_identities=identities,
        runtime_spec=runtime_spec,
        service_hash=service_hash,
        restore_service_hash=restore_service_hash,
        owner_id=owner_id,
        single_service_hash=single_service_hash,
    )
    write_private(
        result / "compose-baseline-identity.json",
        json.dumps(
            {
                "render_sha256": baseline.render_sha256,
                "service_hash": service_hash,
                "restore_service_hash": restore_service_hash,
                "owner_id": owner_id,
                "single_service_hash": single_service_hash,
                "sources": identities,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
    )
    reject_compose_drift(baseline)
    return baseline


def reject_compose_drift(baseline: ComposeBaseline) -> None:
    if source_identities() != baseline.source_identities:
        raise GateError("Compose or .env changed after immutable baseline capture")
    if sha256(baseline.render_path) != baseline.render_sha256:
        raise GateError("private immutable Compose baseline changed")


def write_private(path: pathlib.Path, text: str) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write(text)


def run_compose(compose_file: pathlib.Path, project_name: str) -> None:
    run(
        [
            "docker",
            "compose",
            "--project-name",
            project_name,
            "-f",
            str(compose_file),
            "up",
            "-d",
            "--no-deps",
            LB_CONTAINER,
        ],
        timeout=60,
    )


def verify_restored_baseline(baseline: ComposeBaseline) -> None:
    wait_for_health(2)
    current = docker_inspect(LB_CONTAINER)
    if runtime_lb_spec(current) != baseline.runtime_spec:
        raise GateError("restored LB runtime spec differs from immutable baseline")
    rendered_text = baseline.render_path.read_text(encoding="utf-8")
    document = json.loads(rendered_text)
    validate_rendered_runtime(document, current, baseline.restore_service_hash)
    reject_compose_drift(baseline)


def current_is_harness_owned(baseline: ComposeBaseline) -> bool:
    current = docker_inspect(LB_CONTAINER)
    labels = current.get("Config", {}).get("Labels") or {}
    return (
        labels.get(HARNESS_OWNER_LABEL) == baseline.owner_id
        and labels.get("com.docker.compose.config-hash")
        == baseline.single_service_hash
    )


def verify_current_canonical_dual() -> None:
    identities = source_identities()
    rendered_text, document = render_compose(COMPOSE_FILE)
    project_name = str(document.get("name") or "")
    if not project_name:
        raise GateError("current canonical Compose project name is absent")
    service_hash = compose_service_hash(COMPOSE_FILE, project_name)
    current = docker_inspect(LB_CONTAINER)
    validate_rendered_runtime(document, current, service_hash)
    if source_identities() != identities:
        raise GateError("canonical Compose sources changed during verification")
    health = health_json(8006)
    if health.get("healthy_replicas") != 2 or health.get("total_replicas") != 2:
        raise GateError("current canonical LB is not healthy 2/2")
    if not rendered_text:
        raise GateError("current canonical Compose render is empty")


def restore_or_accept_superseding_canonical(baseline: ComposeBaseline) -> bool:
    """Restore only our container; return true when a canonical deploy superseded us."""
    if current_is_harness_owned(baseline):
        run_compose(baseline.render_path, baseline.project_name)
        verify_restored_baseline(baseline)
        return False
    verify_current_canonical_dual()
    return True


@contextmanager
def deployment_lock():
    descriptor = os.open(
        DEPLOYMENT_LOCK,
        os.O_RDWR | os.O_CREAT | os.O_CLOEXEC,
        0o600,
    )
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise GateError("another node06 deployment operation holds the lock") from exc
        os.ftruncate(descriptor, 0)
        os.write(descriptor, f"pid={os.getpid()}\n".encode())
        os.fsync(descriptor)
        yield
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)


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


def quiet_fence(
    seconds: int, interrupted: Callable[[], int | None] = lambda: None
) -> tuple[dict[str, float], dict[str, float]]:
    if seconds < MIN_QUIET_SECONDS:
        raise GateError("quiet fence may not be shorter than 60 seconds")
    start = prometheus_snapshot(8013)
    if start["running"] or start["waiting"]:
        raise GateError("target engine is not idle at quiet-fence start")
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        signum = interrupted()
        if signum is not None:
            raise DeferredSignal(signum)
        time.sleep(min(1, max(0.1, deadline - time.monotonic())))
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
    # Docker parses --gpus with its CSV reader even when argv bypasses a shell.
    # The literal quotes keep the comma-delimited UUID list in one CSV field.
    gpu_request = '"device=' + ",".join(uuids) + '"'
    return [
        "docker",
        "create",
        "--name",
        name,
        "--label",
        "org.helixml.ramjet.scope=phase-b-offline",
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
    interrupted: Callable[[], int | None] = lambda: None,
) -> None:
    container_id = run(command, timeout=30).strip()
    if not re.fullmatch(r"[0-9a-f]{64}", container_id):
        raise GateError(f"benchmark {name} create returned an invalid container ID")
    descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    process: subprocess.Popen[str] | None = None
    failure: BaseException | None = None
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            process = subprocess.Popen(
                ["docker", "start", "--attach", container_id],
                stdout=handle,
                stderr=subprocess.STDOUT,
                text=True,
            )
            deadline = time.monotonic() + timeout
            while process.poll() is None:
                signum = interrupted()
                if signum is not None:
                    raise DeferredSignal(signum)
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
    except BaseException as exc:
        failure = exc
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        try:
            removed = subprocess.run(
                ["docker", "rm", "-f", container_id],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                timeout=15,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise GateError(
                f"CRITICAL: failed to remove benchmark container {container_id}"
            ) from exc
        if removed.returncode != 0:
            raise GateError(
                f"CRITICAL: failed to remove benchmark container {container_id}"
            )
    if failure is not None:
        raise failure


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


def expected_nccl_sizes() -> set[int]:
    result = set()
    value = 8 * 1024
    while value <= 8 * 1024 * 1024:
        result.add(value)
        value *= 2
    return result


def validate_nccl_output(path: pathlib.Path) -> None:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise GateError("NCCL output is absent or malformed") from exc
    if not re.search(r"#\s+nThread\s+\d+\s+nGpus\s+4\b", text):
        raise GateError("NCCL output does not prove four participating GPU ranks")
    if re.search(r"\b(?:NCCL WARN|error|failed|abort|timeout)\b", text, re.I):
        raise GateError("NCCL output contains a runtime failure marker")
    if not re.search(r"#\s+Out of bounds values\s*:\s*0\s+OK\b", text):
        raise GateError("NCCL output does not prove zero validation errors")
    if re.search(r"\bvia\s+(?:SHM|NET)/", text):
        raise GateError("NCCL output selected a fallback transport")
    p2p_routes = re.findall(r"\bvia\s+P2P/[^\r\n]+", text)
    if len(p2p_routes) < 4:
        raise GateError("NCCL output does not prove the intended P2P transport")

    observed: dict[int, int] = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 13 or not fields[0].isdigit():
            continue
        try:
            size = int(fields[0])
            measurements = (
                tuple(float(value) for value in fields[5:8]) + (int(fields[8]),),
                tuple(float(value) for value in fields[9:12]) + (int(fields[12]),),
            )
        except ValueError:
            continue
        if size not in expected_nccl_sizes():
            continue
        for time_us, algbw, busbw, wrong in measurements:
            if (
                not all(
                    math.isfinite(value) and value > 0
                    for value in (time_us, algbw, busbw)
                )
                or wrong != 0
            ):
                raise GateError(f"NCCL emitted an invalid result row for {size} bytes")
        observed[size] = observed.get(size, 0) + 1
    if any(observed.get(size, 0) != 1 for size in expected_nccl_sizes()):
        raise GateError("NCCL output omits expected in-place/out-of-place size rows")


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
    with deployment_lock():
        return active_run_locked(args, state)


def active_run_locked(args: argparse.Namespace, state: Preflight) -> pathlib.Path:
    if os.geteuid() != 0:
        raise GateError("active mode must run as root for immutable tool ownership")
    result = pathlib.Path(tempfile.mkdtemp(prefix="ramjet-p2p-phase-b.", dir="/tmp"))
    result.chmod(0o700)
    print(f"private active result directory: {result}", file=sys.stderr)
    tools = validate_and_stage_tools(
        args.tools_dir,
        args.expected_tools_manifest_sha256,
        result / "verified-tools",
    )
    owner_id = result.name
    baseline = capture_compose_baseline(result, owner_id)
    wait_for_health(2)
    capture_metadata(result, state, tools)

    restore_needed = False
    interrupted_signum: int | None = None

    def mark_interrupted(signum: int, _frame: Any) -> None:
        nonlocal interrupted_signum
        if interrupted_signum is None:
            interrupted_signum = signum

    def pending_signal() -> int | None:
        return interrupted_signum

    def propagate_pending_signal() -> None:
        if interrupted_signum is not None:
            raise DeferredSignal(interrupted_signum)

    old_handlers = {
        signum: signal.signal(signum, mark_interrupted)
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    }
    try:
        reject_compose_drift(baseline)
        propagate_pending_signal()
        restore_needed = True
        run_compose(baseline.single_path, baseline.project_name)
        propagate_pending_signal()
        wait_for_health(1)
        current = docker_inspect(LB_CONTAINER)
        single_document = json.loads(
            baseline.single_path.read_text(encoding="utf-8")
        )
        validate_rendered_runtime(
            single_document, current, baseline.single_service_hash
        )
        propagate_pending_signal()
        start, end = quiet_fence(args.quiet_seconds, pending_signal)
        write_private(
            result / "quiet-fence.json",
            json.dumps(
                {"seconds": args.quiet_seconds, "start": start, "end": end},
                indent=2,
            )
            + "\n",
        )
        state = preflight(active=True)
        propagate_pending_signal()

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
            interrupted=pending_signal,
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
                    interrupted=pending_signal,
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
                interrupted=pending_signal,
            )
            validate_nccl_output(result / "nccl-all-reduce.txt")

        final_target = prometheus_snapshot(8013)
        if final_target["running"] or final_target["waiting"]:
            raise GateError("target engine is active after benchmark")
        for key in ("prompt_tokens", "generation_tokens", "requests"):
            if final_target[key] != end[key]:
                raise GateError(f"target engine {key} changed during benchmark")
        propagate_pending_signal()
        return result
    finally:
        restore_error: Exception | None = None
        restore_error_context = "captured baseline restoration failed"
        superseded_by_canonical = False
        if restore_needed:
            try:
                restore_error_context = (
                    "restoration ownership fence failed; the harness will not "
                    "overwrite an unowned LB and manual intervention may be required"
                )
                superseded_by_canonical = restore_or_accept_superseding_canonical(
                    baseline
                )
            except Exception as exc:
                restore_error = exc
        else:
            try:
                verify_restored_baseline(baseline)
            except Exception as exc:
                restore_error = exc
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)
        if restore_error is not None:
            raise GateError(
                f"CRITICAL: {restore_error_context}: {restore_error}"
            ) from restore_error
        if superseded_by_canonical:
            raise GateError(
                "concurrent healthy canonical deployment superseded the harness; "
                "captured results are invalid and no restore was attempted"
            )
        if interrupted_signum is not None:
            raise DeferredSignal(interrupted_signum)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    mode = result.add_mutually_exclusive_group()
    mode.add_argument("--run-gpu-scout", action="store_true")
    mode.add_argument("--run-full-prerequisite", action="store_true")
    result.add_argument("--acknowledge-production-risk")
    result.add_argument(
        "--tools-dir", type=pathlib.Path, default=pathlib.Path("/tmp/ramjet-p2p-tools")
    )
    result.add_argument("--expected-tools-manifest-sha256")
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
    if active and not re.fullmatch(
        r"[0-9a-f]{64}", args.expected_tools_manifest_sha256 or ""
    ):
        print(
            "active mode requires --expected-tools-manifest-sha256 with 64 lowercase hex digits",
            file=sys.stderr,
        )
        return 2
    if active:
        operation = (
            "p2p-full-prerequisite"
            if args.run_full_prerequisite
            else "p2p-gpu-scout"
        )
        try:
            require_active_work_permitted(operation)
        except MoratoriumError as exc:
            print(f"active mode blocked by node06 moratorium: {exc}", file=sys.stderr)
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
    except DeferredSignal as exc:
        print(
            f"phase-B prerequisite restored then propagated signal {exc.signum}",
            file=sys.stderr,
        )
        return 128 + exc.signum
    except KeyboardInterrupt:
        print("phase-B read-only preflight interrupted", file=sys.stderr)
        return 130
    except (GateError, OSError, ValueError, subprocess.TimeoutExpired) as exc:
        print(f"phase-B prerequisite failed closed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
