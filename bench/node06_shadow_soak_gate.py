#!/usr/bin/env python3
"""Detached, rollback-safe node06 qualification for the exact-route shadow soak.

This is the deployment owner; ``shadow_soak.py`` is only its content-free
workload child. The gate holds the common deployment lock across candidate
recreate, measurement, and verified restoration of the admitted baseline.
"""

import argparse
import dataclasses
import hashlib
import json
import os
import pathlib
import re
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

import node06_gpu_guard as gpu_guard
import snapshot_recovery_gate as recovery


SCHEMA_VERSION = 1
LB_CONTAINER = recovery.LB_CONTAINER
ENGINE_CONTAINERS = recovery.ENGINE_CONTAINERS
COMPANION_CONTAINERS = recovery.COMPANION_CONTAINERS


class GateInterrupted(recovery.GateError):
    def __init__(self):
        super().__init__("interrupted", "shadow soak gate was interrupted")


@dataclasses.dataclass(frozen=True)
class SoakBaseline:
    lb: recovery.ContainerIdentity
    engines: tuple[recovery.ContainerIdentity, ...]
    companions: tuple[recovery.ContainerIdentity, ...]
    baseline_hash: str
    candidate_hash: str
    candidate_image_id: str
    boot_id: str
    engine_process_starts: tuple[int, ...]


def public_identity(identity):
    return recovery.public_identity(identity)


def workload_payload_valid(payload):
    try:
        workload = payload["source_workload"]
        soak = payload["soak"]
        comparisons = soak["comparisons"]
        source_comparisons = soak["source_comparisons"]
        source_attempts = soak["source_attempts"]
        return (
            payload["type"] == "shadow_soak"
            and payload["unique_sources"] == 104
            and payload["source_concurrency"] == 2
            and payload["qualification_valid"] is True
            and payload["source_bounds_valid"] is True
            and payload["exact_trusted_before_after"] is True
            and workload["requests"] == 104
            and workload["successful"] == 104
            and workload["reconciliation"]["consistent"] is True
            and soak["complete"] == 1
            and soak["phases"]["complete"] == 1
            and soak["sources"] == 104
            and soak["attempts"]["stable"] == 100_000
            and sum(comparisons.values()) == 100_000
            and source_attempts["stable"] == 104
            and sum(source_comparisons.values()) == 104
        )
    except (KeyError, TypeError, AttributeError):
        return False


def sanitize_failure_payload(payload):
    if not isinstance(payload, dict) or payload.get("type") != "shadow_soak_source_failure":
        return None
    result = {"type": "shadow_soak_source_failure"}
    if type(payload.get("source_concurrency")) is int:
        result["source_concurrency"] = payload["source_concurrency"]
    workload = payload.get("source_workload")
    if isinstance(workload, dict):
        safe_workload = {}
        for field in (
            "requests",
            "successful",
            "client_attempts_total",
            "retried_requests",
            "wall_seconds",
        ):
            value = workload.get(field)
            if type(value) in (int, float):
                safe_workload[field] = value
        reasons = workload.get("retry_reasons")
        if isinstance(reasons, dict):
            safe_workload["retry_reasons"] = {
                reason: reasons[reason]
                for reason in ("tokenizer_unavailable", "attestation_changed")
                if type(reasons.get(reason)) is int
            }
        reconciliation = workload.get("reconciliation")
        if isinstance(reconciliation, dict) and type(
            reconciliation.get("consistent")
        ) is bool:
            safe_workload["reconciliation_consistent"] = reconciliation["consistent"]
        result["source_workload"] = safe_workload
    soak = payload.get("soak")
    if isinstance(soak, dict):
        safe_soak = {}
        for field in ("sources", "source_token_bytes", "complete", "duration_seconds"):
            value = soak.get(field)
            if type(value) in (int, float):
                safe_soak[field] = value
        for field, allowed in (
            (
                "phases",
                ("off", "collecting", "ready", "running", "complete", "failed"),
            ),
            (
                "source_attempts",
                (
                    "stable",
                    "tokenizer_unavailable",
                    "inventory_changed",
                    "inventory_untrusted",
                    "lookup_error",
                    "candidate_mismatch",
                    "attestation_changed",
                    "other",
                ),
            ),
        ):
            values = soak.get(field)
            if isinstance(values, dict):
                safe_soak[field] = {
                    label: values[label]
                    for label in allowed
                    if type(values.get(label)) in (int, float)
                }
        result["soak"] = safe_soak
    return result


class NodeShadowRuntime(recovery.NodeRuntime):
    def __init__(self, args):
        super().__init__(args)
        self.workload = self.directory / args.workload_script
        self.original_base = self.base
        self.original_overlay = self.overlay
        self.original_lb_overlay = self.lb_overlay
        self.original_workload = self.workload
        self._child = None
        self._rollback_active = False
        self._interrupt_requested = False
        self._bound_plan = None
        self._frozen_directory = None
        self._frozen_env = None
        self._bound_env_digest = None
        self._compose_token = None

    def handle_signal(self):
        self._interrupt_requested = True

    def _raise_if_interrupted(self):
        if getattr(self, "_interrupt_requested", False) and not getattr(
            self, "_rollback_active", False
        ):
            raise GateInterrupted()

    def _run(self, stage, argv, env=None, timeout=60):
        self._raise_if_interrupted()
        if stage in {"container_inspect", "engine_process_identity"}:
            timeout = min(timeout, 10)
        result = super()._run(stage, argv, env=env, timeout=timeout)
        self._raise_if_interrupted()
        return result

    def begin_rollback(self):
        self._rollback_active = True

    def end_rollback(self):
        self._rollback_active = False
        return self._interrupt_requested

    def _profile_env(self, image, soak_mode):
        env = {
            **recovery.AUTHORITY_ENV,
            "RJ_SNAPSHOT_ROUTE_MODE": "shadow",
            "RJ_SHADOW_SOAK_MODE": soak_mode,
            "SNAPSHOT_LB_IMAGE": image,
            "COMPOSE_PROJECT_NAME": self.args.compose_project_name,
            "COMPOSE_ENV_FILES": str(
                self._frozen_env
                or (self.directory / self.args.env_file).resolve()
            ),
        }
        if self._compose_token is not None:
            env["VLLM_API_KEY"] = self._compose_token
        return env

    def _profile_hash(self, image, soak_mode):
        return self.compose_hash(
            (self.base, self.overlay, self.lb_overlay), self._profile_env(image, soak_mode)
        )

    def _service_hash(self, files, service, env, profiles=()):
        trailing = []
        for profile in profiles:
            trailing.extend(("--profile", profile))
        trailing.extend(("config", "--hash", service))
        raw = self._compose(files, tuple(trailing), env=env)
        fields = raw.decode("utf-8", "strict").strip().split()
        if len(fields) != 2 or fields[0] != service or not re.fullmatch(
            r"[0-9a-f]{64}", fields[1]
        ):
            raise recovery.GateError(
                "invalid_compose_render", "Compose service hash is invalid"
            )
        return fields[1]

    def _config_files(self, names):
        template = '{{index .Config.Labels "com.docker.compose.project.config_files"}}'
        raw = self._run(
            "container_inspect",
            ("docker", "inspect", "--format", template, *names),
        )
        lines = raw.decode("utf-8", "strict").splitlines()
        if len(lines) != len(names):
            raise recovery.GateError(
                "invalid_container_identity", "Compose config-file labels are incomplete"
            )
        return tuple(tuple(item for item in line.split(",") if item) for line in lines)

    def _assert_compose_project(self, identities):
        if any(
            identity.compose_project != self.args.compose_project_name
            for identity in identities
        ):
            raise recovery.GateError(
                "compose_project_mismatch",
                "a serving container belongs to a different Compose project",
            )

    def _image_id(self, reference):
        raw = self._run("candidate_image", ("docker", "image", "inspect", reference))
        try:
            documents = json.loads(raw.decode("utf-8"))
            image_id = documents[0]["Id"]
        except (UnicodeError, json.JSONDecodeError, IndexError, KeyError, TypeError) as error:
            raise recovery.GateError(
                "candidate_image_invalid", "candidate image identity is invalid"
            ) from error
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
            raise recovery.GateError(
                "candidate_image_invalid", "candidate image ID is not immutable"
            )
        return image_id

    def _engine_process_starts(self):
        script = """
import os, pathlib
matches = []
for entry in pathlib.Path('/proc').iterdir():
    if not entry.name.isdigit():
        continue
    if int(entry.name) == os.getpid():
        continue
    try:
        command = (entry / 'cmdline').read_bytes().replace(b'\\0', b' ')
        if b'vllm serve' not in command:
            continue
        stat = (entry / 'stat').read_text()
        right = stat.rfind(')')
        fields = stat[right + 2:].split()
        matches.append((int(entry.name), int(fields[19])))
    except (OSError, ValueError):
        pass
if not matches:
    raise SystemExit(2)
pid, ticks = min(matches)
boot = next(
    int(line.split()[1])
    for line in pathlib.Path('/proc/stat').read_text().splitlines()
    if line.startswith('btime ')
)
print(boot * 1_000_000_000 + ticks * 1_000_000_000 // os.sysconf('SC_CLK_TCK'))
""".strip()
        starts = []
        for container in ENGINE_CONTAINERS:
            raw = self._run(
                "engine_process_identity",
                ("docker", "exec", container, "python3", "-c", script),
            )
            try:
                value = int(raw.decode("ascii").strip())
            except (UnicodeError, ValueError) as error:
                raise recovery.GateError(
                    "engine_process_invalid", "engine process identity is invalid"
                ) from error
            if value <= 0:
                raise recovery.GateError(
                    "engine_process_invalid", "engine process identity is invalid"
                )
            starts.append(value)
        return tuple(starts)

    def _assert_unchanged(self, baseline):
        engines = self.inspect(ENGINE_CONTAINERS)
        companions = self.inspect(COMPANION_CONTAINERS)
        if any(
            not recovery.same_engine_identity(before, after)
            for before, after in zip(baseline.engines, engines, strict=True)
        ):
            raise recovery.GateError(
                "engine_identity_changed", "an engine changed during the gate"
            )
        if self._engine_process_starts() != baseline.engine_process_starts:
            raise recovery.GateError(
                "engine_process_changed", "a vLLM process changed during the gate"
            )
        if companions != baseline.companions:
            raise recovery.GateError(
                "companion_identity_changed", "a companion changed during the gate"
            )

    def _profile_ready(self, soak_mode):
        health = self._fetch_url(self.args.health_url, expect_json=True)
        inventories = recovery.validate_health(health, require_exact=True)
        if inventories is None:
            raise recovery.GateError(
                "exact_health_pending", "two trusted exact inventories are required"
            )
        metrics = self._fetch_url(self.args.metrics_url)
        if not recovery.lb_snapshot_ready(metrics):
            raise recovery.GateError(
                "snapshot_route_pending", "snapshot route authority is not ready"
            )
        enabled = recovery.metric_value(metrics, "ramjet_shadow_soak_enabled")
        if soak_mode == "capture":
            phase = recovery.metric_value(
                metrics, "ramjet_shadow_soak_phase", {"phase": "collecting"}
            )
            if enabled != 1 or phase != 1:
                raise recovery.GateError(
                    "capture_mode_pending", "candidate capture mode is not collecting"
                )
        elif enabled not in (None, 0):
            raise recovery.GateError(
                "rollback_mode_pending", "shadow soak remains enabled"
            )
        return inventories

    def _wait_profile(self, baseline, image, config_hash, soak_mode, previous_id):
        deadline = time.monotonic() + self.args.profile_timeout_seconds
        last_error = None
        while time.monotonic() < deadline:
            self._raise_if_interrupted()
            try:
                current = self.inspect((LB_CONTAINER,))[0]
                if (
                    current.container_id == previous_id
                    or not current.running
                    or current.restart_count != 0
                    or current.image_id != image
                    or current.config_hash != config_hash
                ):
                    raise recovery.GateError(
                        "profile_identity_pending", "LB profile identity is pending"
                    )
                self._assert_compose_project((current,))
                inventories = self._profile_ready(soak_mode)
                self._assert_unchanged(baseline)
                self._raise_if_interrupted()
                return current, inventories
            except GateInterrupted:
                raise
            except recovery.GateError as error:
                last_error = error
                time.sleep(self.args.poll_interval_ms / 1000)
        raise recovery.GateError(
            "profile_timeout", "LB profile did not become authoritative"
        ) from last_error

    def preflight(self):
        required = (
            self.base,
            self.overlay,
            self.lb_overlay,
            self.setup,
            self.host_validator,
            self.compose_validator,
            self.workload,
        )
        if any(not path.is_file() for path in required):
            raise recovery.GateError(
                "missing_deployment_artifact", "deployment artifacts are incomplete"
            )
        env_payload = self._read_env_payload()
        self._compose_token = self._token_from_payload(env_payload)
        self._bound_env_digest = hashlib.sha256(env_payload).digest()
        self._run("host_authority_check", (sys.executable, self.setup, "--check"))
        self._run("host_validator", (self.host_validator,), env=recovery.AUTHORITY_ENV)
        self._run("compose_validator", (sys.executable, self.compose_validator))
        self._run(
            "capture_compose_validator",
            (
                sys.executable,
                self.compose_validator,
                "--shadow-soak-capture",
                "--candidate-lb-image",
                self.args.candidate_image,
            ),
        )
        identities = self.inspect((LB_CONTAINER, *ENGINE_CONTAINERS, *COMPANION_CONTAINERS))
        self._assert_compose_project(identities)
        lb, engines, companions = identities[0], identities[1:3], identities[3:]
        if lb.configured_image != self.args.expected_baseline_image:
            raise recovery.GateError(
                "baseline_image_mismatch", "current LB is not the admitted baseline"
            )
        admitted_env = {
            **recovery.AUTHORITY_ENV,
            "RJ_SNAPSHOT_ROUTE_MODE": "shadow",
            "RJ_SHADOW_SOAK_MODE": "off",
        }
        admitted = self.rendered_services(admitted_env)
        try:
            admitted_lb_image = admitted[LB_CONTAINER]["image"]
        except (KeyError, TypeError) as error:
            raise recovery.GateError(
                "invalid_compose_render", "admitted LB service is missing"
            ) from error
        if admitted_lb_image != self.args.expected_baseline_image:
            raise recovery.GateError(
                "baseline_image_mismatch",
                "expected baseline is not the committed admitted profile",
            )
        expected_base_files = (str(self.base.resolve()),)
        expected_overlay_files = (
            str(self.base.resolve()),
            str(self.overlay.resolve()),
            str(self.lb_overlay.resolve()),
        )
        config_files = self._config_files(
            (LB_CONTAINER, *ENGINE_CONTAINERS, *COMPANION_CONTAINERS)
        )
        if config_files[0] != expected_overlay_files:
            raise recovery.GateError(
                "baseline_config_files_mismatch", "LB was not created from canonical files"
            )
        if any(files != expected_base_files for files in config_files[1:3]):
            raise recovery.GateError(
                "engine_config_files_mismatch", "an engine uses a noncanonical Compose file"
            )
        if any(files != expected_overlay_files for files in config_files[3:]):
            raise recovery.GateError(
                "companion_config_files_mismatch",
                "a companion uses noncanonical Compose files",
            )
        for name, identity in zip(ENGINE_CONTAINERS, engines, strict=True):
            expected_hash = self._service_hash((self.base,), name, {})
            if identity.config_hash != expected_hash:
                raise recovery.GateError(
                    "engine_config_hash_mismatch", "an engine config hash is noncanonical"
                )
        for name, identity in zip(COMPANION_CONTAINERS, companions, strict=True):
            expected_hash = self._service_hash(
                (self.base, self.overlay, self.lb_overlay),
                name,
                admitted_env,
                profiles=("snapshot-companion",),
            )
            if identity.config_hash != expected_hash:
                raise recovery.GateError(
                    "companion_config_hash_mismatch",
                    "a companion config hash is noncanonical",
                )
        if (
            not lb.running
            or lb.restart_count != 0
            or any(not engine.running or engine.restart_count != 0 for engine in engines)
        ):
            raise recovery.GateError(
                "serving_not_healthy", "LB and engines must be running and restart-zero"
            )
        if any(
            not companion.running
            or companion.health != "healthy"
            or companion.restart_count != 0
            for companion in companions
        ):
            raise recovery.GateError(
                "companion_not_healthy", "companions must be healthy and restart-zero"
            )
        baseline_hash = self._profile_hash(lb.configured_image, "off")
        if not lb.config_hash or lb.config_hash != baseline_hash:
            raise recovery.GateError(
                "baseline_not_reproducible", "current LB does not match rollback render"
            )
        baseline_image_id = self._image_id(self.args.expected_baseline_image)
        if baseline_image_id != lb.image_id:
            raise recovery.GateError(
                "baseline_image_mismatch", "baseline reference does not resolve to running image"
            )
        rendered = self.rendered_services(self._profile_env(lb.configured_image, "off"))
        try:
            companion_images = tuple(rendered[name]["image"] for name in COMPANION_CONTAINERS)
        except (KeyError, TypeError) as error:
            raise recovery.GateError(
                "invalid_compose_render", "companion services are missing"
            ) from error
        for identity, image in zip(companions, companion_images, strict=True):
            if identity.configured_image != image or identity.image_id != self._image_id(image):
                raise recovery.GateError(
                    "companion_image_mismatch", "running companions do not match the overlay"
                )
        candidate_image_id = self._image_id(self.args.candidate_image)
        candidate_hash = self._profile_hash(self.args.candidate_image, "capture")
        baseline = SoakBaseline(
            lb=lb,
            engines=engines,
            companions=companions,
            baseline_hash=baseline_hash,
            candidate_hash=candidate_hash,
            candidate_image_id=candidate_image_id,
            boot_id=pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            engine_process_starts=self._engine_process_starts(),
        )
        snapshots, stable = self.companion_readiness()
        if not all(stable):
            raise recovery.GateError(
                "companion_source_not_ready", "both companion sources must be stable"
            )
        inventories = self._profile_ready("off")
        self._raise_if_interrupted()
        return baseline, inventories, recovery.readiness_record(snapshots, stable)

    def deploy_candidate(self, baseline):
        self._raise_if_interrupted()
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
            env=self._profile_env(self.args.candidate_image, "capture"),
            timeout=self.args.profile_timeout_seconds + 30,
        )
        return self._wait_profile(
            baseline,
            baseline.candidate_image_id,
            baseline.candidate_hash,
            "capture",
            baseline.lb.container_id,
        )

    def plan(self):
        current = {
            "gate": recovery.file_digest(pathlib.Path(__file__)),
            "recovery_library": recovery.file_digest(pathlib.Path(recovery.__file__)),
            "workload": recovery.file_digest(self.original_workload),
            "cachebench": recovery.file_digest(self.directory / "cachebench.py"),
            "engine_metrics": recovery.file_digest(
                self.directory / "engine_metrics.py"
            ),
            "base_compose": recovery.file_digest(self.original_base),
            "snapshot_compose": recovery.file_digest(self.original_overlay),
            "snapshot_lb_compose": recovery.file_digest(self.original_lb_overlay),
            "setup_helper": recovery.file_digest(self.setup),
            "host_validator": recovery.file_digest(self.host_validator),
            "compose_validator": recovery.file_digest(self.compose_validator),
            "candidate_image": self.args.candidate_image,
            "expected_baseline_image": self.args.expected_baseline_image,
            "thermal_guard": dict(self.args.thermal_guard),
        }
        if self._bound_plan is not None and current != self._bound_plan:
            raise recovery.GateError(
                "artifact_changed", "a qualified artifact changed during the gate"
            )
        self._bound_plan = current
        return current

    def _assert_original_artifacts(self):
        self.plan()
        if (
            self._bound_env_digest is not None
            and hashlib.sha256(self._read_env_payload()).digest()
            != self._bound_env_digest
        ):
            raise recovery.GateError(
                "artifact_changed", "deployment environment changed during the gate"
            )

    def freeze_artifacts(self, baseline):
        self.plan()
        directory = pathlib.Path(
            tempfile.mkdtemp(prefix=".ramjet-shadow-soak-", dir=self.directory)
        )
        os.chmod(directory, 0o700)
        sources = (
            self.original_base,
            self.original_overlay,
            self.original_lb_overlay,
            self.original_workload,
            self.directory / "cachebench.py",
            self.directory / "engine_metrics.py",
        )
        try:
            for source in sources:
                target = directory / source.name
                shutil.copyfile(source, target)
                os.chmod(target, 0o600)
                if recovery.file_digest(source) != recovery.file_digest(target):
                    raise recovery.GateError(
                        "artifact_copy_failed", "a frozen artifact digest changed"
                    )
            frozen_env = directory / ".env"
            descriptor = os.open(
                frozen_env,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0),
                0o600,
            )
            try:
                payload = self._read_env_payload()
                if hashlib.sha256(payload).digest() != self._bound_env_digest:
                    raise recovery.GateError(
                        "artifact_changed", "deployment environment changed before freeze"
                    )
                remaining = memoryview(payload)
                while remaining:
                    written = os.write(descriptor, remaining)
                    if written <= 0:
                        raise OSError("environment copy write failed")
                    remaining = remaining[written:]
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        except Exception:
            shutil.rmtree(directory, ignore_errors=True)
            raise
        self._frozen_directory = directory
        self._frozen_env = directory / ".env"
        self.base = directory / self.original_base.name
        self.overlay = directory / self.original_overlay.name
        self.lb_overlay = directory / self.original_lb_overlay.name
        self.workload = directory / self.original_workload.name
        try:
            if (
                self._profile_hash(baseline.lb.configured_image, "off")
                != baseline.baseline_hash
                or self._profile_hash(self.args.candidate_image, "capture")
                != baseline.candidate_hash
            ):
                raise recovery.GateError(
                    "frozen_render_mismatch",
                    "frozen Compose artifacts do not reproduce the admitted renders",
                )
            self._assert_original_artifacts()
            self._raise_if_interrupted()
        except Exception:
            self.base = self.original_base
            self.overlay = self.original_overlay
            self.lb_overlay = self.original_lb_overlay
            self.workload = self.original_workload
            self.cleanup_frozen_artifacts()
            raise

    def cleanup_frozen_artifacts(self):
        directory = self._frozen_directory
        if directory is None:
            return
        shutil.rmtree(directory)
        self._frozen_directory = None
        self._frozen_env = None

    def _read_env_payload(self, path=None):
        path = pathlib.Path(path or (self.directory / self.args.env_file))
        flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            descriptor = os.open(path, flags)
            metadata = os.fstat(descriptor)
            if (
                not stat.S_ISREG(metadata.st_mode)
                or metadata.st_mode & 0o077
                or metadata.st_uid != os.geteuid()
                or metadata.st_nlink != 1
                or not 1 <= metadata.st_size <= 64 * 1024
            ):
                raise OSError("unsafe token file")
            chunks = []
            remaining = metadata.st_size + 1
            while remaining > 0:
                chunk = os.read(descriptor, remaining)
                if not chunk:
                    break
                chunks.append(chunk)
                remaining -= len(chunk)
            payload = b"".join(chunks)
            if len(payload) != metadata.st_size:
                raise OSError("token file changed while reading")
        except OSError as error:
            raise recovery.GateError(
                "token_file_invalid", "deployment token file is unavailable or unsafe"
            ) from error
        finally:
            if "descriptor" in locals():
                os.close(descriptor)
        return payload

    @staticmethod
    def _token_from_payload(payload):
        try:
            lines = payload.decode("utf-8", "strict").splitlines()
        except UnicodeError as error:
            raise recovery.GateError("token_file_invalid", "deployment token is invalid") from error
        values = [
            line.split("=", 1)[1]
            for line in lines
            if line.startswith("VLLM_API_KEY=")
        ]
        if len(values) != 1 or not re.fullmatch(r"[A-Za-z0-9_-]{16,512}", values[0]):
            raise recovery.GateError("token_file_invalid", "deployment token is invalid")
        return values[0]

    def _load_token(self):
        return self._token_from_payload(self._read_env_payload())

    def _terminate_child(self):
        child = self._child
        if child is None:
            return
        try:
            if child.poll() is not None:
                return
            os.killpg(child.pid, signal.SIGTERM)
            child.wait(timeout=5)
            return
        except Exception:  # noqa: BLE001 - rollback cleanup is best effort
            pass
        try:
            if child.poll() is not None:
                return
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)
        except Exception:  # noqa: BLE001 - Compose rollback must still run
            pass

    def _read_child_bounded(self):
        child = self._child
        selector = selectors.DefaultSelector()
        buffers = {"stdout": bytearray(), "stderr": bytearray()}
        try:
            for name, stream in (("stdout", child.stdout), ("stderr", child.stderr)):
                if stream is None:
                    raise recovery.GateError(
                        "workload_output_invalid", "workload pipes are unavailable"
                    )
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, name)
            deadline = time.monotonic() + self.args.workload_timeout_seconds
            while selector.get_map():
                self._raise_if_interrupted()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise recovery.GateError(
                        "workload_timeout", "shadow soak workload exceeded its deadline"
                    )
                for key, _mask in selector.select(min(1.0, remaining)):
                    try:
                        chunk = os.read(key.fileobj.fileno(), 64 * 1024)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    target = buffers[key.data]
                    target.extend(chunk)
                    if len(target) > self.args.max_child_output_bytes:
                        raise recovery.GateError(
                            "workload_output_too_large",
                            "workload output exceeded its cap",
                        )
            return bytes(buffers["stdout"]), bytes(buffers["stderr"]), child.wait(timeout=5)
        finally:
            try:
                selector.close()
            except Exception:  # noqa: BLE001 - cleanup cannot mask the gate result
                pass
            for stream in (child.stdout, child.stderr):
                if stream is not None:
                    try:
                        stream.close()
                    except Exception:  # noqa: BLE001 - descriptors are best effort
                        pass

    def run_workload(self):
        self._raise_if_interrupted()
        self._assert_original_artifacts()
        argv = (
            sys.executable,
            self.workload,
            self.args.api_base,
            self.args.model,
            "--apps",
            "52",
            "--sessions",
            "1",
            "--turns",
            "2",
            "--prefix-kib",
            "529",
            "--concurrency",
            "2",
            "--salt",
            self.args.salt,
            "--metrics-url",
            self.args.metrics_url,
            "--engine-metrics",
            self.args.engine_metrics[0],
            "--engine-metrics",
            self.args.engine_metrics[1],
            "--timeout",
            "300",
            "--capture-retry-timeout",
            "330",
            "--expected-comparisons",
            "100000",
            "--progress-every",
            "8",
        )
        child_env = dict(os.environ)
        child_env["BENCH_TOKEN"] = self._compose_token or self._load_token()
        self._child = subprocess.Popen(
            argv,
            cwd=self.directory,
            env=child_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        del child_env
        try:
            stdout, _stderr, returncode = self._read_child_bounded()
        except BaseException:
            self._terminate_child()
            raise
        finally:
            self._child = None
        payload = None
        for line in reversed(stdout.decode("utf-8", "replace").splitlines()):
            try:
                candidate = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(candidate, dict):
                payload = candidate
                break
        if payload is None:
            raise recovery.GateError("workload_output_invalid", "workload emitted no JSON result")
        if returncode != 0:
            error = recovery.GateError("workload_failed", "shadow soak workload failed")
            sanitized = sanitize_failure_payload(payload)
            if sanitized is not None:
                error.payload = sanitized
            raise error
        if not workload_payload_valid(payload):
            raise recovery.GateError(
                "workload_qualification_invalid",
                "workload success payload failed independent qualification",
            )
        return payload

    def rollback(self, baseline, previous_id):
        started = time.monotonic()
        self._terminate_child()
        artifacts_changed = False
        try:
            self._assert_original_artifacts()
            rollback_files = (self.original_base, self.original_overlay, self.original_lb_overlay)
        except Exception:  # noqa: BLE001 - rollback must survive any artifact I/O fault
            artifacts_changed = True
            rollback_files = (self.base, self.overlay, self.lb_overlay)
        last_error = None
        restored = None
        for attempt in range(2):
            try:
                self._compose(
                    rollback_files,
                    (
                        "--profile",
                        "snapshot-companion",
                        "up",
                        "-d",
                        "--no-deps",
                        "--force-recreate",
                        LB_CONTAINER,
                    ),
                    env=self._profile_env(baseline.lb.configured_image, "off"),
                    timeout=self.args.profile_timeout_seconds + 30,
                )
                current, inventories = self._wait_profile(
                    baseline,
                    baseline.lb.image_id,
                    baseline.baseline_hash,
                    "off",
                    previous_id,
                )
                if not artifacts_changed:
                    self._assert_original_artifacts()
                restored = (current, inventories)
                break
            except Exception as error:  # noqa: BLE001 - retry before releasing lock
                last_error = error
                if attempt == 0:
                    artifacts_changed = True
                    rollback_files = (self.base, self.overlay, self.lb_overlay)
                    time.sleep(1)
        if restored is None:
            raise recovery.GateError(
                "rollback_failed", "baseline rollback did not verify after two attempts"
            ) from last_error
        if artifacts_changed:
            raise recovery.GateError(
                "rollback_artifact_changed",
                "frozen baseline is serving but canonical artifacts changed",
            )
        expected_files = (
            str(self.original_base.resolve()),
            str(self.original_overlay.resolve()),
            str(self.original_lb_overlay.resolve()),
        )
        if self._config_files((LB_CONTAINER,))[0] != expected_files:
            raise recovery.GateError(
                "rollback_config_files_mismatch",
                "restored LB does not use canonical Compose files",
            )
        self.cleanup_frozen_artifacts()
        current, inventories = restored
        return round(time.monotonic() - started, 6), current, inventories


def run_gate(args, runtime=None):
    runtime = runtime or NodeShadowRuntime(args)
    journal = recovery.JournalReservation(args.output)
    record = {
        "schema_version": SCHEMA_VERSION,
        "status": "failed",
        "reason": "unclassified_failure",
        "started_utc": recovery.utc_now(),
    }
    exit_code = 1
    mutated = False
    baseline = None
    candidate_id = None
    try:
        with runtime.lock(exclusive=True):
            try:
                baseline, inventories, companions = runtime.preflight()
                record["plan"] = runtime.plan()
                if hasattr(runtime, "freeze_artifacts"):
                    runtime.freeze_artifacts(baseline)
                record["baseline"] = {
                    "lb": public_identity(baseline.lb),
                    "engines": [public_identity(item) for item in baseline.engines],
                    "companions": [public_identity(item) for item in baseline.companions],
                    "inventories": inventories,
                    "companion_readiness": companions,
                    "boot_id": baseline.boot_id,
                    "engine_process_starts": baseline.engine_process_starts,
                }
                mutated = True
                candidate, candidate_inventories = runtime.deploy_candidate(baseline)
                candidate_id = candidate.container_id
                record["candidate"] = {
                    "lb": public_identity(candidate),
                    "inventories": candidate_inventories,
                }
                workload = runtime.run_workload()
                if not workload_payload_valid(workload):
                    raise recovery.GateError(
                        "workload_qualification_invalid",
                        "workload result failed independent qualification",
                    )
                record["workload"] = workload
                record["status"] = "passed"
                record["reason"] = None
                exit_code = 0
            except recovery.GateError as error:
                record["status"] = "failed"
                record["reason"] = error.reason
                if hasattr(error, "payload"):
                    record["workload"] = error.payload
                print(f"node06 shadow soak gate: {error}", file=sys.stderr)
            except Exception as error:  # noqa: BLE001 - bounded process boundary
                record["status"] = "failed"
                record["reason"] = "internal_error"
                print(
                    f"node06 shadow soak gate: internal error ({type(error).__name__})",
                    file=sys.stderr,
                )
            finally:
                if mutated and baseline is not None:
                    pending_signal = False
                    try:
                        if hasattr(runtime, "begin_rollback"):
                            runtime.begin_rollback()
                        seconds, restored, inventories = runtime.rollback(
                            baseline, candidate_id or baseline.lb.container_id
                        )
                        record["rollback"] = {
                            "status": "passed",
                            "seconds": seconds,
                            "lb": public_identity(restored),
                            "inventories": inventories,
                        }
                    except Exception as error:  # noqa: BLE001 - process boundary
                        record["rollback"] = {"status": "failed"}
                        record["status"] = "failed"
                        record["reason"] = "rollback_failed"
                        exit_code = 1
                        print(
                            "node06 shadow soak gate: rollback failed "
                            f"({type(error).__name__})",
                            file=sys.stderr,
                        )
                    finally:
                        if hasattr(runtime, "end_rollback"):
                            pending_signal = runtime.end_rollback()
                    if pending_signal and record["rollback"]["status"] == "passed":
                        record["status"] = "failed"
                        record["reason"] = "interrupted"
                        exit_code = 1
    except recovery.GateError as error:
        record["reason"] = error.reason
        print(f"node06 shadow soak gate: {error}", file=sys.stderr)
    finally:
        if not mutated and hasattr(runtime, "cleanup_frozen_artifacts"):
            try:
                runtime.cleanup_frozen_artifacts()
            except Exception:  # noqa: BLE001 - no deployment mutation occurred
                pass
        record["ended_utc"] = recovery.utc_now()
        journal.finish(record)
    print(json.dumps(record, sort_keys=True, separators=(",", ":")))
    return exit_code


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--deployment-dir", type=pathlib.Path, default=pathlib.Path.cwd())
    result.add_argument("--base-compose", default="docker-compose.yaml")
    result.add_argument("--snapshot-compose", default="docker-compose.snapshot-companion.yaml")
    result.add_argument("--setup-helper", default="setup_snapshot_production_host.py")
    result.add_argument("--host-validator", default="validate-snapshot-production-host.sh")
    result.add_argument("--compose-validator", default="validate-snapshot-production-compose.py")
    result.add_argument("--workload-script", default="shadow_soak.py")
    result.add_argument("--env-file", default=".env")
    result.add_argument("--compose-project-name", default="dspark_0731")
    result.add_argument("--candidate-image", required=True)
    result.add_argument("--expected-baseline-image", required=True)
    result.add_argument("--salt", required=True)
    result.add_argument("--output", type=pathlib.Path, required=True)
    result.add_argument("--api-base", default="http://127.0.0.1:8006")
    result.add_argument("--model", default="deepseek-v4-flash")
    result.add_argument("--metrics-url", default="http://127.0.0.1:8007/metrics")
    result.add_argument("--health-url", default="http://127.0.0.1:8006/health")
    result.add_argument(
        "--engine-metrics",
        action="append",
        default=None,
    )
    result.add_argument("--companion-metrics-socket", action="append", default=None)
    result.add_argument("--lock-file", default="/run/lock/ramjet-node06-deployment.lock")
    result.add_argument("--profile-timeout-seconds", type=float, default=60)
    result.add_argument("--workload-timeout-seconds", type=float, default=1500)
    result.add_argument("--stability-seconds", type=float, default=1)
    result.add_argument("--poll-interval-ms", type=int, default=100)
    result.add_argument("--http-timeout-seconds", type=float, default=2)
    result.add_argument("--max-metrics-bytes", type=int, default=2 * 1024 * 1024)
    result.add_argument("--max-child-output-bytes", type=int, default=4 * 1024 * 1024)
    return result


def validate_args(args):
    try:
        guard = gpu_guard.validate_inherited_guard(
            expected_gpus=8, maximum_abort_c=gpu_guard.MAX_ABORT_C
        )
    except gpu_guard.GuardError as error:
        raise recovery.GateError(
            "thermal_guard_required",
            "shadow soak requires an inherited eight-GPU thermal guard "
            "capability bounded by the intake-air ceiling",
        ) from error
    args.thermal_guard = {
        "expected_gpus": guard["expected_gpus"],
        "abort_c": guard["abort_c"],
        "run_id": guard["run_id"],
    }
    if args.engine_metrics is None:
        args.engine_metrics = [
            "http://127.0.0.1:8012/metrics",
            "http://127.0.0.1:8013/metrics",
        ]
    if args.companion_metrics_socket is None:
        args.companion_metrics_socket = [
            "/run/ramjet-snapshot-metrics-a/metrics.sock",
            "/run/ramjet-snapshot-metrics-b/metrics.sock",
        ]
    if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,62}", args.compose_project_name):
        raise recovery.GateError("invalid_arguments", "Compose project name is invalid")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.candidate_image):
        raise recovery.GateError("invalid_arguments", "candidate image must be an immutable ID")
    if not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", args.expected_baseline_image):
        raise recovery.GateError(
            "invalid_arguments", "baseline image must be a digest-pinned reference"
        )
    if len(args.engine_metrics) != 2:
        raise recovery.GateError(
            "invalid_arguments", "exactly two engine metrics URLs are required"
        )
    if len(args.companion_metrics_socket) != 2:
        raise recovery.GateError(
            "invalid_arguments", "exactly two companion metrics sockets are required"
        )
    if not 10 <= args.profile_timeout_seconds <= 120:
        raise recovery.GateError("invalid_arguments", "profile timeout must be 10-120 seconds")
    if not 300 <= args.workload_timeout_seconds <= 1800:
        raise recovery.GateError("invalid_arguments", "workload timeout must be 300-1800 seconds")
    if not 10 <= args.poll_interval_ms <= 1000:
        raise recovery.GateError("invalid_arguments", "poll interval must be 10-1000ms")
    if not 0 <= args.stability_seconds <= 10:
        raise recovery.GateError("invalid_arguments", "stability interval must be 0-10 seconds")
    if not 0 < args.http_timeout_seconds <= 10:
        raise recovery.GateError("invalid_arguments", "HTTP timeout must be in (0, 10]")
    if not 64 * 1024 <= args.max_metrics_bytes <= 16 * 1024 * 1024:
        raise recovery.GateError("invalid_arguments", "metrics cap is out of bounds")
    if not 64 * 1024 <= args.max_child_output_bytes <= 16 * 1024 * 1024:
        raise recovery.GateError("invalid_arguments", "child output cap is out of bounds")


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        validate_args(args)
        runtime = NodeShadowRuntime(args)

        def interrupt(_signum, _frame):
            runtime.handle_signal()

        for watched in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(watched, interrupt)
        return run_gate(args, runtime)
    except recovery.GateError as error:
        print(f"node06 shadow soak gate: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
