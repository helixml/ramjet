#!/usr/bin/env python3
"""Fail-closed node06 snapshot-shadow recovery qualification.

The default mode is read-only: it validates the production deployment contract
and reports whether both long-lived companions can authoritatively serve a
snapshot. ``--apply`` is deliberately explicit. It acquires the common node06
deployment lock, repeatedly recreates only the load balancer in snapshot shadow
mode, measures process-start-to-authoritative-publication latency, and restores
the byte-for-byte-equivalent baseline Compose service before releasing the
lock.

The journal is content-free. It contains bounded states, public image/config
identities, aggregate inventory sizes, and timings; never commands,
environment variables, credentials, prompts, token IDs, responses, or logs.
"""

import argparse
import contextlib
import dataclasses
import datetime
import fcntl
import hashlib
import http.client
import json
import math
import os
import pathlib
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request


SCHEMA_VERSION = 1
PLAN_VERSION = 1
LB_CONTAINER = "ds4-loadbalancer"
ENGINE_CONTAINERS = ("dspark-0731", "dspark-0731-b")
COMPANION_CONTAINERS = ("snapshot-companion-a", "snapshot-companion-b")
AUTHORITY_ENV = {
    "SNAPSHOT_RUNTIME_DIR_A": "/run/ramjet-snapshot-a",
    "SNAPSHOT_RUNTIME_DIR_B": "/run/ramjet-snapshot-b",
    "SNAPSHOT_METRICS_DIR_A": "/run/ramjet-snapshot-metrics-a",
    "SNAPSHOT_METRICS_DIR_B": "/run/ramjet-snapshot-metrics-b",
    "SNAPSHOT_SESSION_SECRET_FILE_A": "/run/secrets/ramjet-snapshot-session-a",
    "SNAPSHOT_SESSION_SECRET_FILE_B": "/run/secrets/ramjet-snapshot-session-b",
    "SNAPSHOT_DIGEST_SECRET_FILE_A": "/run/secrets/ramjet-snapshot-digest-a",
    "SNAPSHOT_DIGEST_SECRET_FILE_B": "/run/secrets/ramjet-snapshot-digest-b",
    "SNAPSHOT_ATTESTATION_DIR_A": "/run/ramjet-snapshot-attestation-a",
    "SNAPSHOT_ATTESTATION_DIR_B": "/run/ramjet-snapshot-attestation-b",
    "SNAPSHOT_ENGINE_METADATA_FILE_A": "/run/ramjet-engine-metadata-a.json",
    "SNAPSHOT_ENGINE_METADATA_FILE_B": "/run/ramjet-engine-metadata-b.json",
}
METRIC_LINE = re.compile(
    r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+"
    r"([-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][-+]?\d+)?)$"
)
LABEL = re.compile(r'(\w+)="((?:\\.|[^"\\])*)"')


class GateError(RuntimeError):
    """A bounded qualification invariant failed."""

    def __init__(self, reason, message):
        super().__init__(message)
        self.reason = reason


@dataclasses.dataclass(frozen=True)
class ContainerIdentity:
    container_id: str
    image_id: str
    configured_image: str
    started_at: str
    restart_count: int
    running: bool
    health: str
    config_hash: str
    compose_project: str = ""


@dataclasses.dataclass(frozen=True)
class Baseline:
    lb: ContainerIdentity
    engines: tuple[ContainerIdentity, ...]
    companions: tuple[ContainerIdentity, ...]
    baseline_hash: str
    shadow_hash: str
    shadow_image: str


@dataclasses.dataclass(frozen=True)
class RecoverySample:
    iteration: int
    recovery_seconds: float
    recreate_to_ready_seconds: float
    resident_blocks: tuple[int, ...]
    resident_tokens: tuple[int, ...]


@dataclasses.dataclass
class SnapshotRecoveryProgress:
    """Keep snapshot publication time independent from upstream probe readiness."""

    ready_wall: float | None = None
    ready_monotonic: float | None = None
    inventories: tuple[tuple[int, ...], tuple[int, ...]] | None = None

    def observe(self, *, snapshot_ready, inventories, wall, monotonic):
        if snapshot_ready and self.ready_wall is None:
            self.ready_wall = wall
            self.ready_monotonic = monotonic
        if inventories is not None:
            self.inventories = inventories

    @property
    def complete(self):
        return self.ready_wall is not None and self.inventories is not None


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, socket_path, timeout):
        super().__init__("localhost", timeout=timeout)
        self.socket_path = socket_path

    def connect(self):
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.settimeout(self.timeout)
        connection.connect(self.socket_path)
        self.sock = connection


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")


def file_digest(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def parse_timestamp(value):
    try:
        normalized = value.rstrip("Z")
        if "." in normalized:
            whole, fraction = normalized.split(".", 1)
            normalized = whole + "." + fraction[:6]
        parsed = datetime.datetime.fromisoformat(normalized)
        return parsed.replace(tzinfo=datetime.timezone.utc).timestamp()
    except (TypeError, ValueError) as error:
        raise GateError("invalid_container_identity", "invalid container start time") from error


def percentile_nearest_rank(values, percentile):
    if not values:
        raise GateError("missing_recovery_samples", "no recovery samples were recorded")
    ordered = sorted(values)
    rank = max(1, math.ceil(percentile * len(ordered)))
    return ordered[rank - 1]


def parse_prometheus(body):
    result = {}
    for raw in body.splitlines():
        if not raw or raw.startswith("#"):
            continue
        match = METRIC_LINE.fullmatch(raw.strip())
        if match is None:
            continue
        labels = tuple(sorted(LABEL.findall(match.group(2) or "")))
        key = (match.group(1), labels)
        if key in result:
            raise GateError("duplicate_metric", "metrics response contains a duplicate series")
        value = float(match.group(3))
        if not math.isfinite(value):
            raise GateError("invalid_metric", "metrics response contains a non-finite value")
        result[key] = value
    return result


def metric_value(metrics, name, labels=None):
    key = (name, tuple(sorted((labels or {}).items())))
    return metrics.get(key)


def companion_snapshot(metrics):
    engine = {"engine": "engine-0"}
    return {
        "enabled": metric_value(metrics, "ramjet_snapshot_companion_enabled"),
        "authority": metric_value(metrics, "ramjet_snapshot_companion_authority"),
        "listening": metric_value(
            metrics, "ramjet_snapshot_companion_listening", engine
        ),
        "ready": metric_value(metrics, "ramjet_snapshot_companion_ready", engine),
        "source_ready": metric_value(
            metrics, "ramjet_snapshot_companion_source_ready", engine
        ),
        "watermark_present": metric_value(
            metrics,
            "ramjet_snapshot_companion_source_watermark_present",
            engine,
        ),
        "source_phase_ready": metric_value(
            metrics,
            "ramjet_snapshot_companion_source_phase",
            {"engine": "engine-0", "phase": "ready"},
        ),
        "indexed_blocks": metric_value(
            metrics, "ramjet_snapshot_companion_source_indexed_blocks", engine
        ),
        "connect_attempts": metric_value(
            metrics,
            "ramjet_snapshot_companion_owner_events_total",
            {"event": "connect", "reason": "attempt"},
        ),
        "connections": metric_value(
            metrics,
            "ramjet_snapshot_companion_owner_events_total",
            {"event": "connect", "reason": "connected"},
        ),
    }


def companion_is_ready(snapshot):
    required_one = (
        "enabled",
        "authority",
        "listening",
        "ready",
        "source_ready",
        "watermark_present",
        "source_phase_ready",
    )
    return all(snapshot.get(key) == 1 for key in required_one) and (
        snapshot.get("indexed_blocks") is not None
        and snapshot["indexed_blocks"] >= 0
    )


def lb_snapshot_ready(metrics, engine_count=2):
    if metric_value(metrics, "ramjet_snapshot_route_enabled") != 1:
        return False
    for index in range(engine_count):
        labels = {"engine": f"engine-{index}"}
        if metric_value(metrics, "ramjet_snapshot_route_ready", labels) != 1:
            return False
        if metric_value(
            metrics, "ramjet_snapshot_route_connections_active", labels
        ) != 1:
            return False
        # An attempt owns the long-lived consumer future through disconnect.
        # Ready therefore means one active attempt and one active connection,
        # not a completed/zero attempt.
        if metric_value(metrics, "ramjet_snapshot_route_attempts_active", labels) != 1:
            return False
    return True


def validate_health(payload, require_exact):
    if not isinstance(payload, dict) or payload.get("status") != "ok":
        return None
    replicas = payload.get("replicas")
    if (
        not isinstance(replicas, list)
        or len(replicas) != 2
        or payload.get("healthy_replicas") != 2
        or payload.get("total_replicas") != 2
    ):
        return None
    blocks = []
    tokens = []
    for index, replica in enumerate(replicas):
        if not isinstance(replica, dict) or replica.get("index") != index:
            return None
        if replica.get("healthy") is not True:
            return None
        exact = replica.get("exact_inventory")
        if not isinstance(exact, dict):
            return None
        if require_exact and exact.get("trusted") is not True:
            return None
        resident_blocks = exact.get("resident_blocks")
        resident_tokens = exact.get("resident_tokens")
        if (
            type(resident_blocks) is not int
            or resident_blocks < 0
            or type(resident_tokens) is not int
            or resident_tokens < 0
        ):
            return None
        blocks.append(resident_blocks)
        tokens.append(resident_tokens)
    return tuple(blocks), tuple(tokens)


def same_engine_identity(before, after):
    fields = (
        "container_id",
        "image_id",
        "configured_image",
        "started_at",
        "restart_count",
    )
    return (
        all(getattr(before, field) == getattr(after, field) for field in fields)
        and after.running
    )


class JournalReservation:
    """Reserve the output before any deployment mutation, then write it once."""

    def __init__(self, path):
        self.path = pathlib.Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            self.descriptor = os.open(self.path, flags, 0o600)
        except OSError as error:
            raise GateError(
                "journal_create_failed", "journal must be a new safe file"
            ) from error
        self.finished = False

    def finish(self, record):
        if self.finished:
            raise GateError("journal_write_failed", "journal was already finalized")
        payload = (
            json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()
        try:
            with os.fdopen(self.descriptor, "wb") as output:
                output.write(payload)
                output.flush()
                os.fsync(output.fileno())
        except OSError as error:
            raise GateError("journal_write_failed", "journal could not be finalized") from error
        self.finished = True


def write_journal(path, record):
    JournalReservation(path).finish(record)


class NodeRuntime:
    def __init__(self, args):
        self.args = args
        self.directory = args.deployment_dir.resolve()
        self.base = self.directory / args.base_compose
        self.overlay = self.directory / args.snapshot_compose
        # The LB's own snapshot authority mounts live in a separate file
        # so the serving path cannot gain /run dependencies by accident
        # (#157). Every render that exercises the LB needs both overlays.
        self.lb_overlay = self.directory / args.snapshot_lb_compose
        self.setup = self.directory / args.setup_helper
        self.host_validator = self.directory / args.host_validator
        self.compose_validator = self.directory / args.compose_validator
        self._baseline_env = dict(os.environ)
        self._last_lb_id = None

    def _run(self, stage, argv, env=None, timeout=60):
        child_env = dict(self._baseline_env)
        child_env.update(env or {})
        try:
            completed = subprocess.run(
                tuple(str(item) for item in argv),
                cwd=self.directory,
                env=child_env,
                capture_output=True,
                check=False,
                timeout=timeout,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise GateError("command_failed", f"{stage} could not complete") from error
        if completed.returncode != 0:
            raise GateError("command_failed", f"{stage} failed")
        return completed.stdout

    def _compose(self, files, trailing, env=None, timeout=60):
        argv = ["docker", "compose"]
        for path in files:
            argv.extend(("-f", path))
        argv.extend(trailing)
        return self._run("compose", argv, env=env, timeout=timeout)

    def _shadow_env(self):
        return {**AUTHORITY_ENV, "RJ_SNAPSHOT_ROUTE_MODE": "shadow"}

    @contextlib.contextmanager
    def lock(self, exclusive):
        mode = fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH
        try:
            descriptor = os.open(
                self.args.lock_file, os.O_WRONLY | os.O_CREAT, 0o600
            )
        except OSError as error:
            raise GateError("deployment_lock_failed", "cannot open deployment lock") from error
        try:
            try:
                fcntl.flock(descriptor, mode | fcntl.LOCK_NB)
            except OSError as error:
                raise GateError(
                    "deployment_lock_busy", "another deployment owns the lock"
                ) from error
            yield
        finally:
            os.close(descriptor)

    def inspect(self, names):
        raw = self._run("container_inspect", ("docker", "inspect", *names))
        try:
            documents = json.loads(raw.decode("utf-8"))
        except (UnicodeError, json.JSONDecodeError) as error:
            raise GateError(
                "invalid_container_identity", "docker inspect returned invalid JSON"
            ) from error
        if not isinstance(documents, list) or len(documents) != len(names):
            raise GateError("invalid_container_identity", "docker inspect cardinality changed")
        result = []
        for wanted, document in zip(names, documents, strict=True):
            try:
                if document["Name"].lstrip("/") != wanted:
                    raise KeyError("name")
                state = document["State"]
                config = document["Config"]
                labels = config.get("Labels") or {}
                health = (state.get("Health") or {}).get("Status", "none")
                result.append(
                    ContainerIdentity(
                        container_id=document["Id"],
                        image_id=document["Image"],
                        configured_image=config["Image"],
                        started_at=state["StartedAt"],
                        restart_count=int(document["RestartCount"]),
                        running=bool(state["Running"]),
                        health=health,
                        config_hash=labels.get("com.docker.compose.config-hash", ""),
                        compose_project=labels.get("com.docker.compose.project", ""),
                    )
                )
            except (KeyError, TypeError, ValueError) as error:
                raise GateError(
                    "invalid_container_identity", "docker inspect identity is incomplete"
                ) from error
        return tuple(result)

    def compose_hash(self, files, env):
        raw = self._compose(files, ("config", "--hash", LB_CONTAINER), env=env)
        fields = raw.decode("utf-8", "strict").strip().split()
        if len(fields) != 2 or fields[0] != LB_CONTAINER or not re.fullmatch(
            r"[0-9a-f]{64}", fields[1]
        ):
            raise GateError("invalid_compose_render", "Compose hash has an invalid shape")
        return fields[1]

    def rendered_services(self, env):
        raw = self._compose(
            (self.base, self.overlay, self.lb_overlay),
            (
                "--profile",
                "snapshot-companion",
                "--profile",
                "snapshot-attestation",
                "config",
                "--format",
                "json",
            ),
            env=env,
        )
        try:
            services = json.loads(raw.decode("utf-8"))["services"]
        except (UnicodeError, json.JSONDecodeError, KeyError, TypeError) as error:
            raise GateError(
                "invalid_compose_render", "Compose render returned invalid JSON"
            ) from error
        return services

    def preflight(self):
        required = (
            self.base,
            self.overlay,
            self.lb_overlay,
            self.setup,
            self.host_validator,
            self.compose_validator,
        )
        if any(not path.is_file() for path in required):
            raise GateError("missing_deployment_artifact", "deployment artifacts are incomplete")
        self._run("host_authority_check", (sys.executable, self.setup, "--check"))
        self._run("host_validator", (self.host_validator,), env=AUTHORITY_ENV)
        self._run("compose_validator", (sys.executable, self.compose_validator))
        self._compose(
            (self.base, self.overlay, self.lb_overlay),
            (
                "--profile",
                "snapshot-companion",
                "--profile",
                "snapshot-attestation",
                "config",
                "--quiet",
            ),
            env=AUTHORITY_ENV,
        )
        identities = self.inspect((LB_CONTAINER, *ENGINE_CONTAINERS, *COMPANION_CONTAINERS))
        lb = identities[0]
        engines = identities[1:3]
        companions = identities[3:]
        if not lb.running or any(not engine.running for engine in engines):
            raise GateError("serving_not_healthy", "LB and engines must already be running")
        if any(
            not companion.running
            or companion.health != "healthy"
            or companion.restart_count != 0
            for companion in companions
        ):
            raise GateError("companion_not_healthy", "companions must be healthy and restart-zero")
        baseline_env = {"LB_IMAGE": lb.configured_image}
        baseline_hash = self.compose_hash((self.base,), baseline_env)
        if not lb.config_hash or lb.config_hash != baseline_hash:
            raise GateError(
                "baseline_not_reproducible",
                "current LB does not match the rendered rollback service",
            )
        shadow_env = self._shadow_env()
        services = self.rendered_services(shadow_env)
        try:
            shadow_image = services[LB_CONTAINER]["image"]
            companion_images = tuple(
                services[name]["image"] for name in COMPANION_CONTAINERS
            )
        except (KeyError, TypeError) as error:
            raise GateError("invalid_compose_render", "required services are missing") from error
        if any(
            identity.configured_image != image
            for identity, image in zip(companions, companion_images, strict=True)
        ):
            raise GateError(
                "companion_image_mismatch",
                "running companions do not match the overlay",
            )
        shadow_hash = self.compose_hash(
            (self.base, self.overlay, self.lb_overlay), shadow_env
        )
        self._last_lb_id = lb.container_id
        return Baseline(
            lb=lb,
            engines=engines,
            companions=companions,
            baseline_hash=baseline_hash,
            shadow_hash=shadow_hash,
            shadow_image=shadow_image,
        )

    def _fetch_unix_metrics(self, path):
        connection = UnixHTTPConnection(path, self.args.http_timeout_seconds)
        try:
            connection.request("GET", "/metrics")
            response = connection.getresponse()
            body = response.read(self.args.max_metrics_bytes + 1)
        except (OSError, http.client.HTTPException) as error:
            raise GateError("metrics_unavailable", "companion metrics are unavailable") from error
        finally:
            connection.close()
        if response.status != 200 or len(body) > self.args.max_metrics_bytes:
            raise GateError("metrics_unavailable", "companion metrics response is invalid")
        return parse_prometheus(body.decode("utf-8", "replace"))

    def _fetch_url(self, url, expect_json=False):
        try:
            with urllib.request.urlopen(url, timeout=self.args.http_timeout_seconds) as response:
                body = response.read(self.args.max_metrics_bytes + 1)
                status = response.status
        except (OSError, urllib.error.URLError) as error:
            raise GateError("http_unavailable", "LB endpoint is unavailable") from error
        if status != 200 or len(body) > self.args.max_metrics_bytes:
            raise GateError("http_unavailable", "LB endpoint response is invalid")
        if not expect_json:
            return parse_prometheus(body.decode("utf-8", "replace"))
        try:
            return json.loads(body.decode("utf-8"))
        except (UnicodeError, json.JSONDecodeError) as error:
            raise GateError("invalid_health", "LB health response is invalid") from error

    def companion_readiness(self):
        first = tuple(
            companion_snapshot(self._fetch_unix_metrics(path))
            for path in self.args.companion_metrics_socket
        )
        if self.args.stability_seconds:
            time.sleep(self.args.stability_seconds)
        second = tuple(
            companion_snapshot(self._fetch_unix_metrics(path))
            for path in self.args.companion_metrics_socket
        )
        stable = []
        for left, right in zip(first, second, strict=True):
            counters_stable = all(
                left.get(key) is not None and left.get(key) == right.get(key)
                for key in ("connect_attempts", "connections")
            )
            stable.append(companion_is_ready(right) and counters_stable)
        return second, tuple(stable)

    def _engine_identities_unchanged(self, baseline):
        current = self.inspect(ENGINE_CONTAINERS)
        if any(
            not same_engine_identity(before, after)
            for before, after in zip(baseline.engines, current, strict=True)
        ):
            raise GateError("engine_identity_changed", "an engine changed during the gate")

    def enable_shadow_and_measure(self, baseline, iteration):
        _, ready = self.companion_readiness()
        if not all(ready):
            raise GateError(
                "companion_lost_authority",
                "companion authority changed before restart",
            )
        started_monotonic = time.monotonic()
        self._compose(
            (self.base, self.overlay, self.lb_overlay),
            (
                "--profile",
                "snapshot-companion",
                "up",
                "-d",
                "--no-deps",
                "--force-recreate",
                LB_CONTAINER,
            ),
            env=self._shadow_env(),
            timeout=self.args.attempt_timeout_seconds + 30,
        )
        deadline = started_monotonic + self.args.attempt_timeout_seconds
        last_error = None
        progress = SnapshotRecoveryProgress()
        while time.monotonic() < deadline:
            try:
                current = self.inspect((LB_CONTAINER,))[0]
                if (
                    current.container_id == self._last_lb_id
                    or not current.running
                    or current.configured_image != baseline.shadow_image
                    or current.config_hash != baseline.shadow_hash
                ):
                    raise GateError("shadow_identity_pending", "shadow LB identity is pending")
                metrics = self._fetch_url(self.args.metrics_url)
                progress.observe(
                    snapshot_ready=lb_snapshot_ready(metrics),
                    inventories=None,
                    wall=time.time(),
                    monotonic=time.monotonic(),
                )
                health = self._fetch_url(self.args.health_url, expect_json=True)
                inventories = validate_health(health, require_exact=True)
                progress.observe(
                    snapshot_ready=False,
                    inventories=inventories,
                    wall=time.time(),
                    monotonic=time.monotonic(),
                )
                if not progress.complete:
                    raise GateError("snapshot_recovery_pending", "snapshot publication is pending")
                self._engine_identities_unchanged(baseline)
                assert progress.ready_wall is not None
                assert progress.ready_monotonic is not None
                assert progress.inventories is not None
                recovery = max(
                    0.0,
                    progress.ready_wall - parse_timestamp(current.started_at),
                )
                self._last_lb_id = current.container_id
                return RecoverySample(
                    iteration=iteration,
                    recovery_seconds=round(recovery, 6),
                    recreate_to_ready_seconds=round(
                        progress.ready_monotonic - started_monotonic, 6
                    ),
                    resident_blocks=progress.inventories[0],
                    resident_tokens=progress.inventories[1],
                )
            except GateError as error:
                last_error = error
                time.sleep(self.args.poll_interval_ms / 1000)
        raise GateError(
            "snapshot_recovery_timeout",
            "snapshot recovery did not become authoritative before the deadline",
        ) from last_error

    def rollback(self, baseline):
        started = time.monotonic()
        previous_id = self._last_lb_id
        self._compose(
            (self.base,),
            ("up", "-d", "--no-deps", "--force-recreate", LB_CONTAINER),
            env={"LB_IMAGE": baseline.lb.configured_image},
            timeout=self.args.attempt_timeout_seconds + 30,
        )
        deadline = started + self.args.attempt_timeout_seconds
        last_error = None
        while time.monotonic() < deadline:
            try:
                current = self.inspect((LB_CONTAINER,))[0]
                if (
                    current.container_id == previous_id
                    or not current.running
                    or current.configured_image != baseline.lb.configured_image
                    or current.config_hash != baseline.baseline_hash
                ):
                    raise GateError("rollback_identity_pending", "baseline LB is pending")
                health = self._fetch_url(self.args.health_url, expect_json=True)
                if validate_health(health, require_exact=False) is None:
                    raise GateError("rollback_health_pending", "baseline LB health is pending")
                metrics = self._fetch_url(self.args.metrics_url)
                snapshot_enabled = metric_value(metrics, "ramjet_snapshot_route_enabled")
                if snapshot_enabled not in (None, 0):
                    raise GateError("rollback_mode_pending", "snapshot mode is still enabled")
                self._engine_identities_unchanged(baseline)
                self._last_lb_id = current.container_id
                return round(time.monotonic() - started, 6)
            except GateError as error:
                last_error = error
                time.sleep(self.args.poll_interval_ms / 1000)
        raise GateError("rollback_failed", "baseline LB rollback did not verify") from last_error

    def plan(self):
        return {
            "plan_version": PLAN_VERSION,
            "iterations": self.args.iterations,
            "recovery_slo_seconds": self.args.recovery_slo_seconds,
            "attempt_timeout_seconds": self.args.attempt_timeout_seconds,
            "inputs": {
                "gate": file_digest(pathlib.Path(__file__)),
                "base_compose": file_digest(self.base),
                "snapshot_compose": file_digest(self.overlay),
                "snapshot_lb_compose": file_digest(self.lb_overlay),
                "setup_helper": file_digest(self.setup),
                "host_validator": file_digest(self.host_validator),
                "compose_validator": file_digest(self.compose_validator),
            },
        }


def public_identity(identity):
    return {
        "configured_image": identity.configured_image,
        "image_id": identity.image_id,
        "started_at": identity.started_at,
        "restart_count": identity.restart_count,
        "config_hash": identity.config_hash,
    }


def readiness_record(snapshots, stable):
    fields = (
        "authority",
        "listening",
        "ready",
        "source_ready",
        "watermark_present",
        "source_phase_ready",
        "indexed_blocks",
        "connect_attempts",
        "connections",
    )
    return [
        {
            "engine": index,
            "stable": stable[index],
            **{key: snapshot.get(key) for key in fields},
        }
        for index, snapshot in enumerate(snapshots)
    ]


def run_gate(args, runtime=None):
    runtime = runtime or NodeRuntime(args)
    # Reserve the evidence inode before acquiring the deployment lock or
    # touching Compose. A stale/duplicate output can never be discovered only
    # after five LB recreates.
    journal = JournalReservation(args.output)
    record = {
        "schema_version": SCHEMA_VERSION,
        "status": "failed",
        "reason": "unclassified_failure",
        "mode": "apply" if args.apply else "audit",
        "started_utc": utc_now(),
    }
    exit_code = 1
    mutated = False
    baseline = None
    try:
        with runtime.lock(exclusive=args.apply):
            try:
                baseline = runtime.preflight()
                record["plan"] = runtime.plan()
                record["baseline"] = {
                    "lb": public_identity(baseline.lb),
                    "engines": [public_identity(engine) for engine in baseline.engines],
                    "companions": [
                        public_identity(companion) for companion in baseline.companions
                    ],
                    "shadow_image": baseline.shadow_image,
                    "shadow_config_hash": baseline.shadow_hash,
                }
                snapshots, stable = runtime.companion_readiness()
                record["companion_readiness"] = readiness_record(snapshots, stable)
                if not all(stable):
                    record["status"] = "not_ready"
                    record["reason"] = "companion_source_not_ready"
                    exit_code = 3
                elif not args.apply:
                    record["status"] = "ready"
                    record["reason"] = None
                    exit_code = 0
                else:
                    samples = []
                    for iteration in range(1, args.iterations + 1):
                        print(
                            f"snapshot recovery gate: iteration {iteration}/{args.iterations}",
                            file=sys.stderr,
                            flush=True,
                        )
                        mutated = True
                        samples.append(
                            runtime.enable_shadow_and_measure(baseline, iteration)
                        )
                        record["samples"] = [
                            dataclasses.asdict(sample) for sample in samples
                        ]
                    recovery_values = [sample.recovery_seconds for sample in samples]
                    p95 = percentile_nearest_rank(recovery_values, 0.95)
                    record["recovery_p95_seconds"] = round(p95, 6)
                    record["recovery_slo_seconds"] = args.recovery_slo_seconds
                    if p95 > args.recovery_slo_seconds:
                        record["status"] = "failed"
                        record["reason"] = "recovery_slo_exceeded"
                        exit_code = 1
                    else:
                        record["status"] = "passed"
                        record["reason"] = None
                        exit_code = 0
            except GateError as error:
                record["status"] = "failed"
                record["reason"] = error.reason
                print(f"snapshot recovery gate: {error}", file=sys.stderr)
                exit_code = 1
            except Exception as error:  # noqa: BLE001 - bounded process boundary
                record["status"] = "failed"
                record["reason"] = "internal_error"
                print(
                    "snapshot recovery gate: internal error "
                    f"({type(error).__name__})",
                    file=sys.stderr,
                )
                exit_code = 1
            finally:
                # The rollback deliberately remains inside the same exclusive
                # deployment-lock scope as every inspect and shadow mutation.
                if mutated and baseline is not None:
                    try:
                        record["rollback_seconds"] = runtime.rollback(baseline)
                        record["rollback_status"] = "passed"
                    except GateError as error:
                        record["rollback_status"] = "failed"
                        record["status"] = "failed"
                        record["reason"] = "rollback_failed"
                        print(f"snapshot recovery gate: {error}", file=sys.stderr)
                        exit_code = 1
    except GateError as error:
        record["status"] = "failed"
        record["reason"] = error.reason
        print(f"snapshot recovery gate: {error}", file=sys.stderr)
        exit_code = 1
    except Exception as error:  # noqa: BLE001 - bounded process boundary
        record["status"] = "failed"
        record["reason"] = "internal_error"
        print(
            f"snapshot recovery gate: internal error ({type(error).__name__})",
            file=sys.stderr,
        )
        exit_code = 1
    finally:
        record["ended_utc"] = utc_now()
        journal.finish(record)
    print(json.dumps(record, sort_keys=True, separators=(",", ":")))
    return exit_code


def parser():
    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument("--deployment-dir", type=pathlib.Path, default=pathlib.Path.cwd())
    root.add_argument("--base-compose", default="docker-compose.yaml")
    root.add_argument("--snapshot-compose", default="docker-compose.snapshot-companion.yaml")
    root.add_argument(
        "--snapshot-lb-compose", default="docker-compose.snapshot-lb.yaml"
    )
    root.add_argument("--setup-helper", default="setup_snapshot_production_host.py")
    root.add_argument("--host-validator", default="validate-snapshot-production-host.sh")
    root.add_argument("--compose-validator", default="validate-snapshot-production-compose.py")
    root.add_argument(
        "--companion-metrics-socket",
        action="append",
        default=None,
        help="repeat exactly twice; defaults to the fixed node06 production sockets",
    )
    root.add_argument("--metrics-url", default="http://127.0.0.1:8007/metrics")
    root.add_argument("--health-url", default="http://127.0.0.1:8006/health")
    root.add_argument(
        "--lock-file", default="/run/lock/ramjet-node06-deployment.lock"
    )
    root.add_argument("--output", type=pathlib.Path, required=True)
    root.add_argument("--apply", action="store_true")
    root.add_argument("--iterations", type=int, default=5)
    root.add_argument("--recovery-slo-seconds", type=float, default=3.0)
    # Snapshot recovery is timed independently from ordinary upstream health.
    # Keep the outer attempt long enough to observe the 15-second probe loop.
    root.add_argument("--attempt-timeout-seconds", type=float, default=30.0)
    root.add_argument("--stability-seconds", type=float, default=1.0)
    root.add_argument("--poll-interval-ms", type=int, default=20)
    root.add_argument("--http-timeout-seconds", type=float, default=1.0)
    root.add_argument("--max-metrics-bytes", type=int, default=2 * 1024 * 1024)
    return root


def validate_args(args):
    if args.companion_metrics_socket is None:
        args.companion_metrics_socket = [
            "/run/ramjet-snapshot-metrics-a/metrics.sock",
            "/run/ramjet-snapshot-metrics-b/metrics.sock",
        ]
    if len(args.companion_metrics_socket) != 2 or len(
        set(args.companion_metrics_socket)
    ) != 2:
        raise GateError("invalid_arguments", "exactly two distinct metrics sockets are required")
    if args.apply and args.iterations < 5:
        raise GateError("invalid_arguments", "apply mode requires at least five iterations")
    if args.iterations < 1 or args.iterations > 20:
        raise GateError("invalid_arguments", "iterations must be between 1 and 20")
    if not 0 < args.recovery_slo_seconds <= 30:
        raise GateError("invalid_arguments", "recovery SLO must be in (0, 30]")
    if not args.recovery_slo_seconds < args.attempt_timeout_seconds <= 60:
        raise GateError("invalid_arguments", "attempt timeout must exceed the SLO and be <=60s")
    if not 0 <= args.stability_seconds <= 10:
        raise GateError("invalid_arguments", "stability interval must be between 0 and 10s")
    if not 10 <= args.poll_interval_ms <= 1000:
        raise GateError("invalid_arguments", "poll interval must be between 10 and 1000ms")
    if not 0 < args.http_timeout_seconds <= 10:
        raise GateError("invalid_arguments", "HTTP timeout must be in (0, 10]")
    if not 64 * 1024 <= args.max_metrics_bytes <= 16 * 1024 * 1024:
        raise GateError("invalid_arguments", "metrics cap must be between 64KiB and 16MiB")


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        validate_args(args)
        return run_gate(args)
    except GateError as error:
        print(f"snapshot recovery gate: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
