#!/usr/bin/env python3
"""Fail-fast, resumable qualification gate for one immutable engine candidate.

The gate deliberately invokes only repository-owned benchmark programs. It
never records commands, environment variables, model output, container logs,
or credentials. Benchmark stdout is already privacy-bounded by the child
runners and is stored as a mode-0600 artifact; the journal stores only hashes,
sizes, timing, bounded failure classes, and immutable candidate identity.
"""

import argparse
import base64
import contextlib
import dataclasses
import datetime
import errno
import fcntl
import hashlib
import json
import os
import pathlib
import platform
import re
import secrets
import signal
import stat
import subprocess
import sys
import time
import urllib.error
import urllib.request

import node06_gpu_guard as gpu_guard


SCHEMA_VERSION = 1
PLAN_VERSION = 1
SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
THROUGH_ORDER = {"smoke": 1, "scout": 2, "matrix": 3}
REQUIRED_AGENT_METADATA = {
    "engine_image",
    "model_revision",
    "tokenizer_sha256",
    "config_sha256",
    "router_version",
    "gpu_count",
}
CONTROLLED_ENV = {
    "BENCH_PROMPT",
    "BENCH_REQUIRE_RECONCILED_SPECULATION",
    "BENCH_SPEC_MODE",
    "BENCH_TEMPERATURE",
    "BENCH_WORKLOAD",
    "ENGINE_C1_MAX_TOKENS",
    "ENGINE_CONCURRENCIES",
    "ENGINE_CONCURRENT_MAX_TOKENS",
    "ENGINE_RUNS",
    "ENGINE_WORKLOADS",
    "METRICS_URL",
    "METRICS_URLS",
    "SWEEP_LABEL",
}
RUNTIME_PACKAGE_ALIASES = {
    "b12x": "b12x",
    "flashinfer": "flashinfer-python",
    "lmcache": "lmcache",
    "torch": "torch",
    "vllm": "vllm",
}
SHA256 = re.compile(r"sha256:[0-9a-f]{64}")
LB_ENDPOINT_KEYS = (
    "MD_UPSTREAM",
    "MD_KV_EVENT_LIVE_ENDPOINTS",
    "MD_KV_EVENT_REPLAY_ENDPOINTS",
)
NODE06_PROFILE = {
    "profile": "infernal-r11-b",
    "base": "http://127.0.0.1:8013",
    "model": "deepseek-v4-flash",
    "container": "dspark-0731-b",
    "engine_metrics": "http://127.0.0.1:8013/metrics",
    "deployment_lock": "/run/lock/mini-dynamo-node06-deployment.lock",
    "load_balancer_container": "ds4-loadbalancer",
    "expected_lb_upstream": "http://dspark-0731:8000",
    "expected_lb_live_endpoints": "tcp://dspark-0731:5557",
    "expected_lb_replay_endpoints": "tcp://dspark-0731:5558",
    "expected_device_ids": ("4", "5", "6", "7"),
    "expected_gpu_count": 4,
    "healthy_peer_health_url": "http://127.0.0.1:8012/health",
}
R11_ADMISSION = {
    "candidate": "infernal-r11-direct",
    "candidate_manifest_sha256": "61a1a10b9ba7379aa198d5d2f68292717c7ff2e41dda1c3ed79dbabcca799c5c",
    "runtime_manifest_sha256": "13bf4520cbd77b4d576c0246801f2e531d905049774f002bc2d095e7a1f4112d",
    "configured_image": "voipmonitor/vllm:infernal-invocation-vllm908522a-b12x5d648d9-fi1ac6942-cu133-torch213-20260813-r11@sha256:01b973d1ae132882bcc1bf62ea232f6aabe649dd4a89b961d81f3c41cc53f971",
    "image_descriptor_digest": "sha256:01b973d1ae132882bcc1bf62ea232f6aabe649dd4a89b961d81f3c41cc53f971",
    "image_config_digest": "sha256:f226a6fd788bb4af345a17b768654f1e5a7487a812746ccb117aa9b040a82294",
}
MAX_METADATA_BYTES = 2 * 1024 * 1024
MAX_ARTIFACT_BYTES = 1 << 30
PERFORMANCE_ENV_PREFIXES = (
    "B12X_", "CUBLAS", "CUDA_", "CUDNN_", "CUTE_", "DSPARK_",
    "FLASHINFER_", "GLIBC_", "GLOO_", "KMP_", "LD_", "LMCACHE_",
    "MALLOC_", "MKL_", "NCCL_", "NUMEXPR_", "NVIDIA_", "OMP_",
    "OPENBLAS_", "PYTORCH_", "RAY_", "TORCH", "TRITON_", "UCC_", "UCX_",
    "VLLM_", "XLA_",
)
PERFORMANCE_ENV_NAMES = {
    "ALLREDUCE_MODE", "ATTENTION_BACKEND", "BACKEND", "BLOCK_SIZE",
    "DCP_SIZE", "DRAFT_SAMPLE_METHOD", "ENABLE_CHUNKED_PREFILL",
    "ENABLE_PREFIX_CACHING", "GPU_MEMORY_UTILIZATION", "KV_CACHE_DTYPE",
    "MAX_CUDAGRAPH_CAPTURE_SIZE", "MAX_MODEL_LEN", "MAX_NUM_BATCHED_TOKENS",
    "MAX_NUM_SEQS", "MOE_BACKEND", "REJECTION_SAMPLE_METHOD", "TP_SIZE",
}
RUNTIME_SECRET_ENV_NAMES = {
    "HF_TOKEN", "HUGGING_FACE_HUB_TOKEN", "VLLM_API_KEY",
}
PROCESS_PROBE = r'''import base64, hashlib, json, os, pathlib, stat, sys
keys = json.loads(base64.b64decode(sys.argv[1]))
artifacts = json.loads(base64.b64decode(sys.argv[2]))
policy = json.loads(base64.b64decode(sys.argv[3]))
proc_root = pathlib.Path(sys.argv[4] if len(sys.argv) > 4 else '/proc')
matches = []
for entry in proc_root.iterdir():
    if not entry.name.isdigit():
        continue
    try:
        raw = (entry / 'cmdline').read_bytes().rstrip(b'\0')
        argv = [value.decode('utf-8', 'strict') for value in raw.split(b'\0')]
    except (OSError, UnicodeError):
        continue
    for index in range(1, len(argv)):
        if argv[index] == 'serve' and pathlib.PurePosixPath(argv[index - 1]).name == 'vllm':
            matches.append((entry, argv[index:]))
            break
if len(matches) != 1:
    raise SystemExit('vllm process cardinality mismatch')
entry, serving = matches[0]
if any(value.split('=', 1)[0] in {'--api-key', '--token', '--hf-token', '--authorization'} for value in serving):
    raise SystemExit('sensitive serving option')
stat_fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
start_ticks = int(stat_fields[19])
boot_seconds = next(int(line.split()[1]) for line in (proc_root / 'stat').read_text().splitlines() if line.startswith('btime '))
started_ns = boot_seconds * 1_000_000_000 + start_ticks * 1_000_000_000 // os.sysconf('SC_CLK_TCK')
raw_environment = (entry / 'environ').read_bytes().rstrip(b'\0').split(b'\0')
environment = {}
seen_names = set()
for raw in raw_environment:
    name, separator, value = raw.partition(b'=')
    if not separator:
        raise SystemExit('invalid process environment')
    decoded_name = name.decode('utf-8', 'strict')
    if decoded_name in seen_names:
        raise SystemExit('duplicate process environment')
    seen_names.add(decoded_name)
    if decoded_name in keys:
        environment[decoded_name] = value.decode('utf-8', 'strict')
    elif decoded_name not in policy['secret_names'] and (
        decoded_name in policy['names']
        or decoded_name.startswith(tuple(policy['prefixes']))
    ):
        raise SystemExit('unexpected performance environment')
if set(environment) != set(keys):
    raise SystemExit('process environment key mismatch')
def digest_file(path):
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, 'O_NOFOLLOW'):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags)
    try:
        info = os.fstat(descriptor)
        maximum = policy['max_artifact_bytes']
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= maximum:
            raise SystemExit('invalid artifact')
        digest = hashlib.sha256()
        total = 0
        while total <= maximum:
            chunk = os.read(descriptor, min(1 << 20, maximum + 1 - total))
            if not chunk:
                break
            total += len(chunk)
            digest.update(chunk)
        if total != info.st_size or total > maximum:
            raise SystemExit('artifact changed')
        return digest.hexdigest()
    finally:
        os.close(descriptor)
observed_artifacts = []
for path in artifacts:
    digest = digest_file(path)
    observed_artifacts.append({'path': path, 'sha256': digest})
canonical = lambda value: hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
print(json.dumps({
    'process_started_unix_ns': started_ns,
    'serving_argv_sha256': hashlib.sha256(b'\0'.join(value.encode() for value in serving)).hexdigest(),
    'environment_sha256': canonical(environment),
    'artifacts_sha256': canonical(observed_artifacts),
}, sort_keys=True, separators=(',', ':')))
'''
RUNTIME_MARKERS = (
    (
        "jit_compilation",
        re.compile(
            r"(?i)(?:(?:triton|cutedsl|torchinductor|torch\.compile).{0,100}"
            r"(?:compil|generat|autotun)|(?:compil|generat).{0,100}"
            r"(?:triton|cutedsl|kernel))"
        ),
    ),
    ("cuda_oom", re.compile(r"(?i)(?:CUDA out of memory|CUDA error: out of memory)")),
    ("cuda_error", re.compile(r"(?i)(?:CUDA error|CUDA failure|device-side assert)")),
    ("nccl_error", re.compile(r"(?i)(?:NCCL[^\n]{0,120}(?:error|abort|failed))")),
    ("xid", re.compile(r"(?i)\bXid(?:\s*[:=]?\s*\d+)?\b")),
    ("traceback", re.compile(r"Traceback \(most recent call last\)")),
    (
        "fatal_runtime",
        re.compile(r"(?i)(?:Fatal Python error|segmentation fault|core dumped)"),
    ),
)


class GateError(RuntimeError):
    """A gate precondition or fail-closed invariant was violated."""


@dataclasses.dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: bytes = b""
    stderr: bytes = b""


@dataclasses.dataclass(frozen=True)
class ContainerIdentity:
    image_id: str
    configured_image: str
    started_at: str
    restart_count: int
    running: bool


@dataclasses.dataclass(frozen=True)
class ProcessIdentity:
    process_started_unix_ns: int
    serving_argv_sha256: str
    environment_sha256: str
    artifacts_sha256: str


@dataclasses.dataclass(frozen=True)
class Stage:
    name: str
    argv: tuple[str, ...]
    env: tuple[tuple[str, str], ...] = ()


class SubprocessRunner:
    def __init__(self):
        self.child = None

    def run(self, argv, env=None):
        child_env = dict(os.environ)
        for key in CONTROLLED_ENV:
            child_env.pop(key, None)
        child_env.update(env or {})
        try:
            self.child = subprocess.Popen(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=child_env,
                stdin=subprocess.DEVNULL,
                start_new_session=True,
            )
            stdout, stderr = self.child.communicate()
            return CommandResult(self.child.returncode, stdout, stderr)
        except OSError as error:
            return CommandResult(127, b"", type(error).__name__.encode())
        finally:
            self.child = None

    def cancel(self):
        child = self.child
        if child is None or child.poll() is not None:
            return
        try:
            os.killpg(child.pid, signal.SIGTERM)
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)
        except ProcessLookupError:
            return

    def inspect(self, container):
        template = (
            "{{.Image}}\t{{.Config.Image}}\t{{.State.StartedAt}}\t"
            "{{.RestartCount}}\t{{.State.Running}}"
        )
        result = self.run(("docker", "inspect", "--format", template, container))
        if result.returncode != 0:
            raise GateError("container identity inspection failed")
        fields = result.stdout.decode("utf-8", "strict").strip().split("\t")
        if len(fields) != 5:
            raise GateError("container identity inspection returned an invalid shape")
        try:
            restart_count = int(fields[3])
        except ValueError as error:
            raise GateError("container restart count is invalid") from error
        return ContainerIdentity(
            image_id=fields[0],
            configured_image=fields[1],
            started_at=fields[2],
            restart_count=restart_count,
            running=fields[4].casefold() == "true",
        )

    def logs(self, container, since):
        return self.run(("docker", "logs", "--since", since, container))

    def environment(self, container):
        result = self.run(
            ("docker", "inspect", "--format", "{{json .Config.Env}}", container)
        )
        if result.returncode != 0:
            raise GateError("load-balancer environment inspection failed")
        try:
            values = json.loads(result.stdout)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise GateError("load-balancer environment has an invalid shape") from error
        if not isinstance(values, list) or not all(
            isinstance(value, str) and "=" in value for value in values
        ):
            raise GateError("load-balancer environment has an invalid shape")
        environment = {}
        for value in values:
            name, setting = value.split("=", 1)
            if name in environment:
                raise GateError("load-balancer environment contains duplicate keys")
            environment[name] = setting
        return environment

    def device_ids(self, container):
        result = self.run(
            (
                "docker",
                "inspect",
                "--format",
                "{{json .HostConfig.DeviceRequests}}",
                container,
            )
        )
        if result.returncode != 0:
            raise GateError("candidate device inspection failed")
        try:
            requests = json.loads(result.stdout)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise GateError("candidate device request has an invalid shape") from error
        if not isinstance(requests, list) or len(requests) != 1:
            raise GateError("candidate device request has an invalid shape")
        request = requests[0]
        if not isinstance(request, dict) or request.get("Driver") != "nvidia":
            raise GateError("candidate device request has an invalid shape")
        device_ids = request.get("DeviceIDs")
        if not isinstance(device_ids, list) or not all(
            isinstance(value, str) for value in device_ids
        ):
            raise GateError("candidate device request has an invalid shape")
        selected = list(device_ids)
        if not selected or len(selected) != len(set(selected)):
            raise GateError("candidate device request has an invalid shape")
        return tuple(selected)

    def process_identity(self, container, environment_names, artifact_paths):
        encoded_names = base64.b64encode(
            json.dumps(sorted(environment_names), separators=(",", ":")).encode()
        ).decode()
        encoded_paths = base64.b64encode(
            json.dumps(list(artifact_paths), separators=(",", ":")).encode()
        ).decode()
        encoded_policy = base64.b64encode(
            json.dumps(
                {
                    "prefixes": PERFORMANCE_ENV_PREFIXES,
                    "names": sorted(PERFORMANCE_ENV_NAMES),
                    "secret_names": sorted(RUNTIME_SECRET_ENV_NAMES),
                    "max_artifact_bytes": MAX_ARTIFACT_BYTES,
                },
                separators=(",", ":"),
            ).encode()
        ).decode()
        result = self.run(
            ("docker", "exec", container, "python3", "-c", PROCESS_PROBE,
             encoded_names, encoded_paths, encoded_policy, "/proc")
        )
        if result.returncode != 0 or len(result.stdout) > 4096:
            raise GateError("live serving process inspection failed")
        try:
            value = json.loads(result.stdout)
            identity = ProcessIdentity(
                process_started_unix_ns=int(value["process_started_unix_ns"]),
                serving_argv_sha256=value["serving_argv_sha256"],
                environment_sha256=value["environment_sha256"],
                artifacts_sha256=value["artifacts_sha256"],
            )
        except (KeyError, TypeError, ValueError, UnicodeError, json.JSONDecodeError) as error:
            raise GateError("live serving process inspection returned an invalid shape") from error
        if identity.process_started_unix_ns <= 0 or any(
            re.fullmatch(r"[0-9a-f]{64}", value) is None
            for value in (
                identity.serving_argv_sha256,
                identity.environment_sha256,
                identity.artifacts_sha256,
            )
        ):
            raise GateError("live serving process inspection returned an invalid shape")
        return identity

    def health(self, url):
        token = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
        if not token:
            raise GateError("health probe credential is unavailable")
        request = urllib.request.Request(
            url,
            headers={"Authorization": "Bearer " + token},
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                response.read(4096)
                if response.status != 200:
                    raise GateError("engine health probe failed")
        except (OSError, urllib.error.URLError) as error:
            raise GateError("engine health probe failed") from error


def canonical_digest(value):
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def bytes_digest(value):
    return hashlib.sha256(value).hexdigest()


def nul_joined_digest(values):
    return bytes_digest(b"\0".join(value.encode() for value in values))


def performance_environment_name(name):
    return name not in RUNTIME_SECRET_ENV_NAMES and (
        name in PERFORMANCE_ENV_NAMES
        or name.startswith(PERFORMANCE_ENV_PREFIXES)
    )


@contextlib.contextmanager
def exclusive_deployment_lock(path):
    path = pathlib.Path(path)
    flags = os.O_RDWR | os.O_CREAT | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags, 0o600)
    except OSError as error:
        raise GateError("deployment lock is unavailable") from error
    try:
        info = os.fstat(descriptor)
        if (
            not stat.S_ISREG(info.st_mode)
            or info.st_uid != os.geteuid()
            or info.st_nlink != 1
            or info.st_mode & 0o022
        ):
            raise GateError("deployment lock authority is unsafe")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise GateError("another deployment owner holds the common lock") from error
        yield
    finally:
        os.close(descriptor)


def validate_live_isolation(runner, args):
    environment = runner.environment(args.load_balancer_container)
    expected = {
        "MD_UPSTREAM": args.expected_lb_upstream,
        "MD_KV_EVENT_LIVE_ENDPOINTS": args.expected_lb_live_endpoints,
        "MD_KV_EVENT_REPLAY_ENDPOINTS": args.expected_lb_replay_endpoints,
    }
    mismatches = [
        key for key in LB_ENDPOINT_KEYS if environment.get(key) != expected[key]
    ]
    if mismatches:
        raise GateError(
            "load balancer is not isolated to the healthy peer: "
            + ", ".join(mismatches)
        )
    if runner.device_ids(args.container) != args.expected_device_ids:
        raise GateError("candidate GPU topology does not match admission")
    runner.health(args.healthy_peer_health_url)
    runner.health(args.base.rstrip("/") + "/health")


def thermal_guard_contract():
    try:
        guard = gpu_guard.validate_inherited_guard(
            expected_gpus=8, maximum_abort_c=gpu_guard.MAX_ABORT_C
        )
    except gpu_guard.GuardError as error:
        raise GateError(
            "candidate gate requires an inherited eight-GPU thermal guard "
            "capability bounded by the intake-air ceiling"
        ) from error
    return {
        "expected_gpus": guard["expected_gpus"],
        "abort_c": guard["abort_c"],
        "run_id": guard["run_id"],
    }


def secure_read_json(path, purpose="metadata"):
    path = pathlib.Path(path)
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise GateError(f"invalid JSON metadata: {path}") from error
    try:
        info = os.fstat(descriptor)
        if (
            not stat.S_ISREG(info.st_mode)
            or info.st_uid != os.geteuid()
            or info.st_nlink != 1
            or info.st_mode & 0o022
            or info.st_size > MAX_METADATA_BYTES
        ):
            raise GateError(f"unsafe {purpose} authority: {path}")
        raw = b""
        while len(raw) <= MAX_METADATA_BYTES:
            chunk = os.read(descriptor, min(65536, MAX_METADATA_BYTES + 1 - len(raw)))
            if not chunk:
                break
            raw += chunk
        if len(raw) > MAX_METADATA_BYTES:
            raise GateError(f"oversized {purpose} authority: {path}")
    finally:
        os.close(descriptor)
    try:
        document = json.loads(raw)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise GateError(f"invalid JSON metadata: {path}") from error
    return document, bytes_digest(raw)


def read_json(path):
    return secure_read_json(path)[0]


def flag_value(argv, name):
    for index, value in enumerate(argv):
        if value == name and index + 1 < len(argv):
            return argv[index + 1]
        if value.startswith(name + "="):
            return value.split("=", 1)[1]
    return None


def admission_contract(candidate_manifest, runtime_manifest):
    candidate_document, candidate_sha256 = secure_read_json(
        candidate_manifest, "candidate admission"
    )
    runtime_document, runtime_sha256 = secure_read_json(
        runtime_manifest, "runtime admission"
    )
    image = candidate_document.get("candidate_image")
    process = runtime_document.get("process")
    if (
        candidate_document.get("schema_version") != 2
        or not isinstance(image, dict)
        or runtime_document.get("schema_version") != 2
        or not isinstance(process, dict)
    ):
        raise GateError("candidate admission manifests do not use schema version 2")
    configured_image = image.get("image")
    image_digest = image.get("image_digest")
    config_digest = image.get("config_digest")
    if (
        not isinstance(configured_image, str)
        or not isinstance(image_digest, str)
        or not isinstance(config_digest, str)
        or SHA256.fullmatch(image_digest) is None
        or SHA256.fullmatch(config_digest) is None
        or not configured_image.endswith("@" + image_digest)
    ):
        raise GateError("candidate image admission identity is invalid")
    argv = process.get("argv")
    environment = process.get("environment")
    packages = process.get("packages")
    artifacts = process.get("artifacts")
    if (
        not isinstance(argv, list)
        or not argv
        or not all(isinstance(value, str) for value in argv)
        or not isinstance(environment, dict)
        or not isinstance(packages, dict)
        or not isinstance(artifacts, list)
    ):
        raise GateError("candidate serving-runtime process is invalid")
    if any(
        not isinstance(item, dict)
        or set(item) != {"path", "sha256"}
        or not isinstance(item["path"], str)
        or not item["path"].startswith("/")
        or re.fullmatch(r"[0-9a-f]{64}", item["sha256"] or "") is None
        for item in artifacts
    ):
        raise GateError("candidate serving-runtime artifacts are invalid")
    if any(
        not isinstance(name, str)
        or not isinstance(value, str)
        or "\0" in name
        or "\0" in value
        for name, value in environment.items()
    ):
        raise GateError("candidate serving-runtime environment is invalid")
    receipt_hashes = {
        "argv_sha256": nul_joined_digest(argv),
        "environment_sha256": canonical_digest(environment),
        "packages_sha256": canonical_digest(packages),
        "artifacts_sha256": canonical_digest(artifacts),
    }
    if any(process.get(name) != value for name, value in receipt_hashes.items()):
        raise GateError("candidate serving-runtime receipt hash is invalid")
    model_revision = flag_value(argv, "--revision")
    tokenizer_revision = flag_value(argv, "--tokenizer-revision")
    if not model_revision or not tokenizer_revision:
        raise GateError("candidate serving-runtime lacks immutable revisions")
    expected_packages = {}
    for live_name, receipt_name in RUNTIME_PACKAGE_ALIASES.items():
        value = packages.get(receipt_name)
        if not isinstance(value, str) or not value:
            raise GateError("candidate serving-runtime package set is incomplete")
        expected_packages[live_name] = value
    return {
        "candidate": candidate_document.get("candidate"),
        "configured_image": configured_image,
        "image_descriptor_digest": image_digest,
        "image_config_digest": config_digest,
        "model_revision": model_revision,
        "tokenizer_revision": tokenizer_revision,
        "serving_argv_sha256": receipt_hashes["argv_sha256"],
        "environment_sha256": receipt_hashes["environment_sha256"],
        "environment_names": tuple(sorted(environment)),
        "artifacts_sha256": receipt_hashes["artifacts_sha256"],
        "artifact_paths": tuple(item.get("path") for item in artifacts),
        "runtime_packages": expected_packages,
        "candidate_manifest_sha256": candidate_sha256,
        "runtime_manifest_sha256": runtime_sha256,
    }


def validate_profile_admission(profile, admission):
    if profile != NODE06_PROFILE["profile"]:
        raise GateError("unknown candidate admission profile")
    mismatches = [
        field
        for field, expected in R11_ADMISSION.items()
        if admission.get(field) != expected
    ]
    if mismatches:
        raise GateError("candidate admission profile mismatch: " + ", ".join(mismatches))


def candidate_contract(metadata):
    if metadata.get("schema_version") != 1 or not isinstance(metadata.get("live"), dict):
        raise GateError("engine metadata does not use schema version 1")
    if metadata.get("verified") is False:
        raise GateError("engine metadata failed receipt verification")
    if metadata.get("receipt") is not None and metadata.get("verified") is not True:
        raise GateError("engine receipt is present but not verified")
    live = metadata["live"]
    required = (
        "configured_image",
        "image_id",
        "image_descriptor_digest",
        "image_config_digest",
        "model_revision",
        "tokenizer_revision",
        "tokenizer_sha256",
        "config_sha256",
        "started_at",
        "restart_count",
        "argv_sha256",
        "serving_argv_sha256",
        "process_started_unix_ns",
    )
    missing = [key for key in required if live.get(key) in (None, "")]
    if missing:
        raise GateError("engine metadata is missing immutable fields: " + ", ".join(missing))
    try:
        process_started_unix_ns = int(live["process_started_unix_ns"])
    except (TypeError, ValueError) as error:
        raise GateError("engine metadata process start is invalid") from error
    if process_started_unix_ns <= 0:
        raise GateError("engine metadata process start is invalid")
    return {
        "configured_image": live["configured_image"],
        "image_id": live["image_id"],
        "image_descriptor_digest": live["image_descriptor_digest"],
        "image_config_digest": live["image_config_digest"],
        "model_revision": live["model_revision"],
        "tokenizer_revision": live["tokenizer_revision"],
        "tokenizer_sha256": live["tokenizer_sha256"],
        "config_sha256": live["config_sha256"],
        "runtime_packages": live.get("runtime_packages") or {},
        "effective_contract": live.get("effective_contract") or {},
        "argv_sha256": live["argv_sha256"],
        "serving_argv_sha256": live["serving_argv_sha256"],
        "process_started_unix_ns": process_started_unix_ns,
        "started_at": live["started_at"],
        "restart_count": live["restart_count"],
        "receipt_sha256": (metadata.get("receipt") or {}).get("receipt_sha256"),
        "receipt_verified": metadata.get("verified") is True,
    }


def validate_candidate_admission(candidate, admission):
    for field in (
        "configured_image",
        "image_descriptor_digest",
        "image_config_digest",
        "model_revision",
        "tokenizer_revision",
        "serving_argv_sha256",
    ):
        if candidate.get(field) != admission.get(field):
            raise GateError(f"live candidate does not match admission {field}")
    observed_packages = candidate.get("runtime_packages") or {}
    for name, expected in admission["runtime_packages"].items():
        if observed_packages.get(name) != expected:
            raise GateError(f"live candidate does not match admission package.{name}")


def validate_agent_metadata(metadata, candidate, expected_gpu_count):
    missing = sorted(REQUIRED_AGENT_METADATA - metadata.keys())
    if missing:
        raise GateError("agent metadata is missing: " + ", ".join(missing))
    if metadata["gpu_count"] != expected_gpu_count:
        raise GateError("agent metadata gpu_count does not match candidate topology")
    for key in ("model_revision", "tokenizer_sha256", "config_sha256"):
        if metadata[key] != candidate[key]:
            raise GateError(f"agent metadata does not match engine {key}")
    expected_images = {
        candidate["configured_image"],
        candidate["configured_image"] + "@" + candidate["image_id"],
    }
    if metadata["engine_image"] not in expected_images:
        raise GateError("agent metadata does not match engine image identity")


def expected_identity(candidate):
    return ContainerIdentity(
        image_id=candidate["image_id"],
        configured_image=candidate["configured_image"],
        started_at=candidate["started_at"],
        restart_count=int(candidate["restart_count"]),
        running=True,
    )


def expected_process_identity(candidate, admission):
    return ProcessIdentity(
        process_started_unix_ns=candidate["process_started_unix_ns"],
        serving_argv_sha256=admission["serving_argv_sha256"],
        environment_sha256=admission["environment_sha256"],
        artifacts_sha256=admission["artifacts_sha256"],
    )


def assert_identity(actual, expected):
    mismatches = [
        field
        for field in dataclasses.asdict(expected)
        if getattr(actual, field) != getattr(expected, field)
    ]
    if mismatches:
        raise GateError("container identity changed: " + ", ".join(mismatches))


def assert_process_identity(actual, expected):
    mismatches = [
        field
        for field in dataclasses.asdict(expected)
        if getattr(actual, field) != getattr(expected, field)
    ]
    if mismatches:
        raise GateError("serving process authority changed: " + ", ".join(mismatches))


def script_digest(path):
    return bytes_digest(pathlib.Path(path).read_bytes())


def build_stages(args):
    python = sys.executable
    agent = Stage(
        "agent_correctness",
        (
            python,
            str(SCRIPT_DIR / "agentbench.py"),
            "run",
            args.base,
            args.model,
            "--metadata-json",
            str(args.agent_metadata),
            "--profile",
            "deterministic",
            "--label",
            "candidate-gate-agent",
            "--concurrency",
            "1",
            "--repetitions",
            "1",
            "--engine-metrics",
            args.engine_metrics,
            "--require-reconciled-speculation",
        ),
    )
    scout = Stage(
        "c8_scout",
        (
            str(SCRIPT_DIR / "engine_matrix.sh"),
            args.base,
            args.model,
            "candidate-gate-scout",
        ),
        (
            ("BENCH_REQUIRE_RECONCILED_SPECULATION", "1"),
            ("ENGINE_CONCURRENCIES", "8"),
            ("ENGINE_RUNS", "1"),
            ("METRICS_URL", args.engine_metrics),
        ),
    )
    matrix = Stage(
        "full_matrix",
        (
            str(SCRIPT_DIR / "engine_matrix.sh"),
            args.base,
            args.model,
            "candidate-gate-matrix",
        ),
        (
            ("BENCH_REQUIRE_RECONCILED_SPECULATION", "1"),
            ("METRICS_URL", args.engine_metrics),
        ),
    )
    return (agent, scout, matrix)


def plan_contract(args, agent_metadata, admission, thermal_guard=None):
    guard = thermal_guard if thermal_guard is not None else thermal_guard_contract()
    return {
        "plan_version": PLAN_VERSION,
        "profile": args.profile,
        "python": platform.python_version(),
        "base": args.base.rstrip("/"),
        "model": args.model,
        "container": args.container,
        "engine_metrics": args.engine_metrics,
        "expected_gpu_count": args.expected_gpu_count,
        "deployment_lock": str(args.deployment_lock),
        "isolation": {
            "load_balancer_container": args.load_balancer_container,
            "expected_lb_upstream": args.expected_lb_upstream,
            "expected_lb_live_endpoints": args.expected_lb_live_endpoints,
            "expected_lb_replay_endpoints": args.expected_lb_replay_endpoints,
            "expected_device_ids": list(args.expected_device_ids),
            "healthy_peer_health_url": args.healthy_peer_health_url,
        },
        "admission": {
            "candidate": admission["candidate"],
            "candidate_manifest_sha256": admission["candidate_manifest_sha256"],
            "runtime_manifest_sha256": admission["runtime_manifest_sha256"],
            "environment_sha256": admission["environment_sha256"],
            "artifacts_sha256": admission["artifacts_sha256"],
        },
        "thermal_guard": {
            "expected_gpus": guard["expected_gpus"],
            "abort_c": guard["abort_c"],
        },
        "agent_metadata_sha256": canonical_digest(agent_metadata),
        "inputs": {
            name: script_digest(SCRIPT_DIR / name)
            for name in (
                "candidate_gate.py",
                "agentbench.py",
                "agent_cases/v1.jsonl",
                "engine_matrix.sh",
                "codebench.py",
                "engine_metrics.py",
            )
        },
        "stages": [
            {"name": stage.name, "env": dict(stage.env)} for stage in build_stages(args)
        ],
    }


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")


def runtime_findings(logs):
    body = logs.decode("utf-8", "replace")
    return [name for name, pattern in RUNTIME_MARKERS if pattern.search(body)]


def validate_private_directory(path, purpose):
    path = pathlib.Path(path)
    try:
        info = path.lstat()
    except OSError as error:
        raise GateError(f"{purpose} directory is unavailable: {path}") from error
    if (
        not stat.S_ISDIR(info.st_mode)
        or info.st_uid != os.geteuid()
        or stat.S_IMODE(info.st_mode) != 0o700
    ):
        raise GateError(f"{purpose} directory is unsafe: {path}")


def validate_private_file(descriptor, purpose):
    info = os.fstat(descriptor)
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_uid != os.geteuid()
        or info.st_nlink != 1
        or stat.S_IMODE(info.st_mode) != 0o600
    ):
        raise GateError(f"{purpose} file is unsafe")


def load_prior(path, candidate_sha256, plan_sha256, resume):
    path = pathlib.Path(path)
    validate_private_directory(path.parent, "journal parent")
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except FileNotFoundError:
        return set()
    except OSError as error:
        raise GateError("candidate gate journal is unavailable") from error
    try:
        validate_private_file(descriptor, "candidate gate journal")
        raw_journal = b""
        while len(raw_journal) <= MAX_METADATA_BYTES:
            chunk = os.read(
                descriptor, min(65536, MAX_METADATA_BYTES + 1 - len(raw_journal))
            )
            if not chunk:
                break
            raw_journal += chunk
        if len(raw_journal) > MAX_METADATA_BYTES:
            raise GateError("candidate gate journal is oversized")
    finally:
        os.close(descriptor)
    if not resume:
        raise GateError(f"output already exists (use --resume): {path}")
    successful = set()
    try:
        lines = raw_journal.decode("utf-8").splitlines()
    except UnicodeError as error:
        raise GateError("candidate gate journal is not UTF-8") from error
    for line_number, raw in enumerate(lines, 1):
        if not raw.strip():
            continue
        try:
            record = json.loads(raw)
        except json.JSONDecodeError as error:
            raise GateError(f"invalid prior gate journal line {line_number}") from error
        if record.get("candidate_sha256") != candidate_sha256:
            raise GateError("resume candidate identity does not match prior journal")
        if record.get("plan_sha256") != plan_sha256:
            raise GateError("resume plan does not match prior journal")
        if record.get("status") in {"passed", "resumed"}:
            successful.add(record.get("gate"))
    return successful


def append_record(path, record):
    path = pathlib.Path(path)
    validate_private_directory(path.parent, "journal parent")
    flags = os.O_WRONLY | os.O_APPEND | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError as error:
        if error.errno != errno.EEXIST:
            raise GateError("candidate gate journal creation failed") from error
        try:
            descriptor = os.open(path, flags)
        except OSError as open_error:
            raise GateError("candidate gate journal append failed") from open_error
    try:
        validate_private_file(descriptor, "candidate gate journal")
        encoded = (
            json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()
        written = 0
        while written < len(encoded):
            written += os.write(descriptor, encoded[written:])
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def store_artifact(directory, stage, output):
    directory = pathlib.Path(directory)
    validate_private_directory(directory.parent, "artifact parent")
    try:
        os.mkdir(directory, 0o700)
    except FileExistsError:
        pass
    except OSError as error:
        raise GateError("artifact directory creation failed") from error
    validate_private_directory(directory, "artifact")
    path = directory / f"{stage}-{secrets.token_hex(8)}.jsonl"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags, 0o600)
    except OSError as error:
        raise GateError("benchmark artifact creation failed") from error
    try:
        validate_private_file(descriptor, "benchmark artifact")
        written = 0
        while written < len(output):
            written += os.write(descriptor, output[written:])
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    return path


def record_base(candidate_sha256, plan_sha256, gate, started, ended):
    return {
        "schema_version": SCHEMA_VERSION,
        "candidate_sha256": candidate_sha256,
        "plan_sha256": plan_sha256,
        "gate": gate,
        "started_utc": started,
        "ended_utc": ended,
    }


def run_gate(args, runner=None):
    runner = runner or SubprocessRunner()
    thermal_guard = thermal_guard_contract()
    with exclusive_deployment_lock(args.deployment_lock):
        return run_gate_locked(args, runner, thermal_guard)


def run_gate_locked(args, runner, thermal_guard):
    admission = admission_contract(args.candidate_manifest, args.runtime_manifest)
    validate_profile_admission(args.profile, admission)
    engine_metadata = read_json(args.engine_metadata)
    agent_metadata = read_json(args.agent_metadata)
    candidate = candidate_contract(engine_metadata)
    validate_candidate_admission(candidate, admission)
    validate_agent_metadata(agent_metadata, candidate, args.expected_gpu_count)
    candidate_sha256 = canonical_digest(candidate)
    plan_sha256 = canonical_digest(
        plan_contract(args, agent_metadata, admission, thermal_guard)
    )
    expected = expected_identity(candidate)
    expected_process = expected_process_identity(candidate, admission)
    successful = load_prior(args.output, candidate_sha256, plan_sha256, args.resume)

    started = utc_now()
    try:
        assert_identity(runner.inspect(args.container), expected)
        assert_process_identity(
            runner.process_identity(
                args.container,
                admission["environment_names"],
                admission["artifact_paths"],
            ),
            expected_process,
        )
        validate_live_isolation(runner, args)
        status = "passed"
        error = None
    except GateError as caught:
        status = "failed"
        error = str(caught)
    ended = utc_now()
    identity_record = record_base(candidate_sha256, plan_sha256, "identity", started, ended)
    identity_record.update(
        {
            "thermal_guard_run_id": thermal_guard["run_id"],
            "status": status,
            "receipt_verified": candidate["receipt_verified"],
            "admission_verified": True,
            "error": error,
        }
    )
    append_record(args.output, identity_record)
    if status != "passed":
        return 1

    stages = build_stages(args)[: THROUGH_ORDER[args.through]]
    for stage in stages:
        if stage.name in successful:
            now = utc_now()
            record = record_base(candidate_sha256, plan_sha256, stage.name, now, now)
            record["thermal_guard_run_id"] = thermal_guard["run_id"]
            record["status"] = "resumed"
            append_record(args.output, record)
            continue

        stage_started = utc_now()
        monotonic_started = time.monotonic()
        try:
            assert_identity(runner.inspect(args.container), expected)
            assert_process_identity(
                runner.process_identity(
                    args.container,
                    admission["environment_names"],
                    admission["artifact_paths"],
                ),
                expected_process,
            )
            validate_live_isolation(runner, args)
            result = runner.run(stage.argv, env=dict(stage.env))
            assert_identity(runner.inspect(args.container), expected)
            assert_process_identity(
                runner.process_identity(
                    args.container,
                    admission["environment_names"],
                    admission["artifact_paths"],
                ),
                expected_process,
            )
            validate_live_isolation(runner, args)
            logs = runner.logs(args.container, stage_started)
            log_payload = logs.stdout + b"\n" + logs.stderr
            markers = runtime_findings(log_payload)
            log_error = logs.returncode != 0
            artifact = store_artifact(args.artifacts_dir, stage.name, result.stdout)
            stage_status = (
                "passed"
                if result.returncode == 0 and not markers and not log_error
                else "failed"
            )
            error_class = None
            if result.returncode != 0:
                error_class = "benchmark_failed"
            elif log_error:
                error_class = "log_scan_failed"
            elif markers:
                error_class = "runtime_marker"
            stage_error = None
        except GateError as caught:
            result = CommandResult(1)
            logs = CommandResult(1)
            log_payload = b"\n"
            markers = []
            artifact = store_artifact(args.artifacts_dir, stage.name, b"")
            stage_status = "failed"
            error_class = "runtime_authority_changed"
            stage_error = str(caught)
        stage_ended = utc_now()
        record = record_base(
            candidate_sha256, plan_sha256, stage.name, stage_started, stage_ended
        )
        record.update(
            {
                "thermal_guard_run_id": thermal_guard["run_id"],
                "status": stage_status,
                "error_class": error_class,
                "error": stage_error,
                "exit_code": result.returncode,
                "wall_seconds": round(time.monotonic() - monotonic_started, 3),
                "artifact": artifact.name,
                "artifact_sha256": bytes_digest(result.stdout),
                "artifact_bytes": len(result.stdout),
                "stderr_sha256": bytes_digest(result.stderr),
                "stderr_bytes": len(result.stderr),
                "runtime_markers": markers,
                "runtime_log_sha256": bytes_digest(log_payload),
                "runtime_log_bytes": len(log_payload),
            }
        )
        append_record(args.output, record)
        if stage_status != "passed":
            return 1
    return 0


def parse_device_ids(value):
    values = tuple(part.strip() for part in value.split(",") if part.strip())
    if (
        not values
        or len(values) != len(set(values))
        or any(re.fullmatch(r"[0-9]+", part) is None for part in values)
    ):
        raise argparse.ArgumentTypeError("device IDs must be unique integers")
    return values


def validate_node06_profile(args):
    mismatches = []
    for field, expected in NODE06_PROFILE.items():
        actual = getattr(args, field)
        if field == "deployment_lock":
            actual = str(actual)
        elif field == "base":
            actual = actual.rstrip("/")
        if actual != expected:
            mismatches.append(field)
    if mismatches:
        raise GateError(
            "candidate gate node06 profile mismatch: " + ", ".join(mismatches)
        )


def parser():
    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument(
        "--profile", required=True, choices=(NODE06_PROFILE["profile"],)
    )
    root.add_argument("--base", required=True)
    root.add_argument("--model", required=True)
    root.add_argument("--container", required=True)
    root.add_argument("--candidate-manifest", required=True, type=pathlib.Path)
    root.add_argument("--runtime-manifest", required=True, type=pathlib.Path)
    root.add_argument("--expected-gpu-count", required=True, type=int)
    root.add_argument("--engine-metrics", required=True)
    root.add_argument(
        "--deployment-lock",
        type=pathlib.Path,
        default=NODE06_PROFILE["deployment_lock"],
    )
    root.add_argument(
        "--load-balancer-container",
        default=NODE06_PROFILE["load_balancer_container"],
    )
    root.add_argument(
        "--expected-lb-upstream", default=NODE06_PROFILE["expected_lb_upstream"]
    )
    root.add_argument(
        "--expected-lb-live-endpoints",
        default=NODE06_PROFILE["expected_lb_live_endpoints"],
    )
    root.add_argument(
        "--expected-lb-replay-endpoints",
        default=NODE06_PROFILE["expected_lb_replay_endpoints"],
    )
    root.add_argument(
        "--expected-device-ids",
        type=parse_device_ids,
        default=NODE06_PROFILE["expected_device_ids"],
    )
    root.add_argument(
        "--healthy-peer-health-url",
        default=NODE06_PROFILE["healthy_peer_health_url"],
    )
    root.add_argument("--engine-metadata", required=True, type=pathlib.Path)
    root.add_argument("--agent-metadata", required=True, type=pathlib.Path)
    root.add_argument("--output", required=True, type=pathlib.Path)
    root.add_argument("--artifacts-dir", type=pathlib.Path)
    root.add_argument("--through", choices=tuple(THROUGH_ORDER), default="smoke")
    root.add_argument("--resume", action="store_true")
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    if args.artifacts_dir is None:
        args.artifacts_dir = pathlib.Path(str(args.output) + ".artifacts")
    runner = SubprocessRunner()
    previous_handlers = {}

    def interrupt(_signum, _frame):
        runner.cancel()
        raise GateError("thermal guard interrupted candidate gate")

    for watched in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        previous_handlers[watched] = signal.signal(watched, interrupt)
    try:
        validate_node06_profile(args)
        return run_gate(args, runner)
    except GateError as error:
        print(f"candidate gate: {error}", file=sys.stderr)
        return 2
    finally:
        runner.cancel()
        for watched, handler in previous_handlers.items():
            signal.signal(watched, handler)


if __name__ == "__main__":
    raise SystemExit(main())
