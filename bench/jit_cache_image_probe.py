#!/usr/bin/env python3
"""Prove an immutable engine image has no baked JIT-cache payload to hide.

The probe never allocates a GPU or opens a network. It fails if the image has
any non-empty file, link, or special node below /cache/jit, because an empty
host bind would hide that state and could turn a warm rollout into a cold
compile. The pinned r34 image has one zero-byte FlashInfer log placeholder;
that carries no reusable payload and is bound explicitly by the evidence.
"""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import subprocess
import time
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "compat" / "deepseek-v4-r34-serving-runtime.json"
IMAGE = (
    "voipmonitor/vllm@"
    "sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b"
)
CACHE_ROOT = "/cache/jit"
MAX_CAPTURE_BYTES = 64 << 10
MAX_NODES = 4096
EXPECTED_DIRECTORIES = 26
_FINGERPRINT = re.compile(r"[a-z0-9][a-z0-9._-]{0,127}")

PROBE = r'''
import json
import os
import stat

ROOT = "/cache/jit"
MAX_NODES = 4096
counts = {
    "directories": 0,
    "files": 0,
    "zero_byte_files": 0,
    "symlinks": 0,
    "other": 0,
    "file_bytes": 0,
}
pending = [ROOT]
seen = 0
while pending:
    parent = pending.pop()
    with os.scandir(parent) as entries:
        for entry in entries:
            seen += 1
            if seen > MAX_NODES:
                raise RuntimeError("cache tree exceeds node limit")
            info = entry.stat(follow_symlinks=False)
            if stat.S_ISDIR(info.st_mode):
                counts["directories"] += 1
                pending.append(entry.path)
            elif stat.S_ISREG(info.st_mode):
                counts["files"] += 1
                counts["file_bytes"] += info.st_size
                if info.st_size == 0:
                    counts["zero_byte_files"] += 1
            elif stat.S_ISLNK(info.st_mode):
                counts["symlinks"] += 1
            else:
                counts["other"] += 1
counts["fingerprint"] = os.environ.get("LOCAL_INFERENCE_CACHE_FINGERPRINT")
counts["fingerprint_root"] = os.path.isdir(
    os.path.join(ROOT, str(counts["fingerprint"]))
)
print(json.dumps(counts, sort_keys=True, separators=(",", ":")))
'''


class ProbeError(RuntimeError):
    pass


def manifest_fingerprint(document: Any) -> str:
    try:
        environment = document["process"]["environment"]
        fingerprint = environment["LOCAL_INFERENCE_CACHE_FINGERPRINT"]
    except (KeyError, TypeError) as error:
        raise ProbeError("runtime cache fingerprint is unavailable") from error
    if not isinstance(fingerprint, str) or _FINGERPRINT.fullmatch(fingerprint) is None:
        raise ProbeError("runtime cache fingerprint is invalid")
    cache_values = [
        value
        for key, value in environment.items()
        if key in {"FLASHINFER_WORKSPACE_BASE", "TILELANG_TMP_DIR", "XDG_CACHE_HOME"}
        or key.endswith(("_CACHE_DIR", "_CACHE_PATH", "_CACHE_ROOT"))
    ]
    prefix = f"{CACHE_ROOT}/{fingerprint}"
    if len(cache_values) < 12 or any(
        not isinstance(value, str)
        or (value != prefix and not value.startswith(f"{prefix}/"))
        for value in cache_values
    ):
        raise ProbeError("runtime cache namespace is inconsistent")
    return fingerprint


def evidence_errors(evidence: Any, fingerprint: str) -> list[str]:
    if not isinstance(evidence, dict):
        return ["evidence.document"]
    errors = []
    expected = {
        "fingerprint": fingerprint,
        "fingerprint_root": True,
        "files": 1,
        "zero_byte_files": 1,
        "symlinks": 0,
        "other": 0,
        "file_bytes": 0,
        "directories": EXPECTED_DIRECTORIES,
    }
    for field, value in expected.items():
        if evidence.get(field) != value:
            errors.append(f"evidence.{field}")
    return errors


def run_probe(timeout_seconds: float) -> tuple[dict[str, Any], int]:
    started = time.monotonic()
    completed = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--network",
            "none",
            "--read-only",
            "--entrypoint",
            "/opt/venv/bin/python",
            IMAGE,
            "-c",
            PROBE,
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        timeout=timeout_seconds,
    )
    elapsed_ms = round((time.monotonic() - started) * 1000)
    if completed.returncode != 0 or len(completed.stdout) > MAX_CAPTURE_BYTES:
        raise ProbeError("JIT-cache image probe failed")
    try:
        evidence = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ProbeError("JIT-cache image evidence is invalid") from error
    return evidence, elapsed_ms


def main() -> int:
    try:
        raw = MANIFEST.read_bytes()
        manifest = json.loads(raw)
        fingerprint = manifest_fingerprint(manifest)
        evidence, elapsed_ms = run_probe(30)
    except (OSError, json.JSONDecodeError):
        print(json.dumps({"status": "failed", "reason": "probe input failed"}))
        return 1
    except (ProbeError, subprocess.TimeoutExpired) as error:
        reason = "probe timed out" if isinstance(error, subprocess.TimeoutExpired) else str(error)
        print(json.dumps({"status": "failed", "reason": reason}))
        return 1
    errors = evidence_errors(evidence, fingerprint)
    if errors:
        print(
            json.dumps(
                {"status": "unsafe", "fields": sorted(set(errors))},
                sort_keys=True,
                separators=(",", ":"),
            )
        )
        return 1
    print(
        json.dumps(
            {
                "status": "safe_zero_payload",
                "image": IMAGE,
                "manifest_sha256": hashlib.sha256(raw).hexdigest(),
                "fingerprint": fingerprint,
                "directories": evidence["directories"],
                "elapsed_ms": elapsed_ms,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
