#!/usr/bin/env python3
"""Reconstruct and qualify the source-locked Infernal r5 overlay.

This is a GPU-free source gate. It fails before image work unless the input is
the exact staged Infernal r4 vLLM tree, applies one pinned overlay in a private
output directory, and runs both committed source preflights. Output contains
only public identities, counts, booleans, and bounded reason codes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile

from infernal_parser_probe import DEFAULT_CASES, load_cases


REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
PARSER_CASE_COUNT = len(load_cases(DEFAULT_CASES))
DEFAULT_ROOT = (
    REPO_ROOT / "deploy/dspark_0731/infernal-r5-candidate"
)
SHA1 = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^(?:sha256:)?[0-9a-f]{64}$")
REASONS = frozenset(
    {
        "base_tree_mismatch",
        "candidate_blob_mismatch",
        "candidate_tree_mismatch",
        "c128a_preflight",
        "invalid_manifest",
        "output_exists",
        "parser_preflight",
        "patch_digest_mismatch",
        "patch_rejected",
        "patch_scope_mismatch",
        "python_source_syntax",
        "source_dirty",
        "source_missing",
    }
)


class CandidateError(Exception):
    def __init__(self, reason: str):
        if reason not in REASONS:
            raise ValueError("unbounded candidate reason")
        self.reason = reason
        super().__init__(reason)


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_manifest(candidate_root: pathlib.Path = DEFAULT_ROOT) -> dict:
    try:
        manifest = json.loads((candidate_root / "manifest.json").read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise CandidateError("invalid_manifest") from exc
    required_sha1 = (
        "base_vllm_tree",
        "candidate_vllm_tree",
        "candidate_sparse_mla_blob_sha1",
    )
    required_sha256 = (
        "base_image_digest",
        "base_vllm_patch_sha256",
        "overlay_patch_sha256",
        "base_parser_source_id",
        "candidate_parser_source_id",
    )
    if manifest.get("schema_version") != 1 or any(
        not SHA1.fullmatch(str(manifest.get(key, ""))) for key in required_sha1
    ):
        raise CandidateError("invalid_manifest")
    if any(
        not SHA256.fullmatch(str(manifest.get(key, "")))
        for key in required_sha256
    ):
        raise CandidateError("invalid_manifest")
    patch_name = manifest.get("overlay_patch")
    if patch_name != "r4-v4-correctness.patch":
        raise CandidateError("invalid_manifest")
    if not str(manifest.get("base_image", "")).endswith(
        "@" + manifest["base_image_digest"]
    ):
        raise CandidateError("invalid_manifest")
    if manifest.get("source_root") != "/opt/infernal-invocation/vllm":
        raise CandidateError("invalid_manifest")
    if not re.fullmatch(
        r"[a-z0-9][a-z0-9._-]{0,127}", str(manifest.get("candidate", ""))
    ):
        raise CandidateError("invalid_manifest")
    if not re.fullmatch(
        r"[a-z0-9][a-z0-9._-]{0,127}", str(manifest.get("cache_fingerprint", ""))
    ):
        raise CandidateError("invalid_manifest")
    changed_files = manifest.get("changed_files")
    if (
        not isinstance(changed_files, list)
        or not changed_files
        or len(changed_files) != len(set(changed_files))
        or any(
            not isinstance(path, str)
            or not path.endswith(".py")
            or pathlib.PurePosixPath(path).is_absolute()
            or ".." in pathlib.PurePosixPath(path).parts
            for path in changed_files
        )
    ):
        raise CandidateError("invalid_manifest")
    return manifest


def patch_paths(patch: pathlib.Path) -> list[str]:
    try:
        paths = [
            line.split()[2][2:]
            for line in patch.read_text().splitlines()
            if line.startswith("diff --git ")
        ]
    except (OSError, UnicodeDecodeError, IndexError) as exc:
        raise CandidateError("patch_scope_mismatch") from exc
    if not paths or len(paths) != len(set(paths)):
        raise CandidateError("patch_scope_mismatch")
    return paths


def validate_python_sources(output: pathlib.Path, changed_files: list[str]) -> None:
    """Compile the exact overlay sources without importing or writing caches."""
    try:
        for relative in changed_files:
            contents = (output / relative).read_bytes()
            compile(contents, "<candidate-source>", "exec", dont_inherit=True)
    except (OSError, SyntaxError, ValueError) as exc:
        raise CandidateError("python_source_syntax") from exc


def git_blob_sha1(contents: bytes) -> str:
    framed = f"blob {len(contents)}\0".encode() + contents
    return hashlib.sha1(framed, usedforsecurity=False).hexdigest()


def run(args: list[str], *, env: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        args,
        check=False,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    if completed.returncode:
        raise CandidateError("patch_rejected")
    return completed.stdout.strip()


def run_c128a(source: pathlib.Path, mode: str) -> None:
    completed = subprocess.run(
        [
            sys.executable,
            str(REPO_ROOT / "bench/infernal_c128a_preflight.py"),
            mode,
            str(source),
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    try:
        report = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise CandidateError("c128a_preflight") from exc
    if completed.returncode or report.get("status") != "pass":
        raise CandidateError("c128a_preflight")


def run_parser(source: pathlib.Path, profile: str, identity: str) -> None:
    completed = subprocess.run(
        [
            sys.executable,
            str(REPO_ROOT / "bench/infernal_parser_probe.py"),
            "run",
            str(source),
            "--profile",
            profile,
            "--expected-source-id",
            identity,
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    try:
        records = [json.loads(line) for line in completed.stdout.splitlines()]
    except json.JSONDecodeError as exc:
        raise CandidateError("parser_preflight") from exc
    if completed.returncode or len(records) != PARSER_CASE_COUNT or not all(
        record.get("passed") is True for record in records
    ):
        raise CandidateError("parser_preflight")


def source_tree(source: pathlib.Path) -> str:
    if not (source / ".git").exists():
        raise CandidateError("source_missing")
    tree = run(["git", "-C", str(source), "write-tree"])
    dirty = subprocess.run(
        ["git", "-C", str(source), "diff", "--quiet"], check=False
    ).returncode
    if dirty:
        raise CandidateError("source_dirty")
    return tree


def materialize(
    source: pathlib.Path,
    output: pathlib.Path,
    patch: pathlib.Path,
    base_tree: str,
) -> str:
    if output.exists():
        raise CandidateError("output_exists")
    with tempfile.TemporaryDirectory(prefix="infernal-candidate-index-") as temp:
        index = pathlib.Path(temp) / "index"
        env = os.environ.copy()
        env["GIT_INDEX_FILE"] = str(index)
        git_dir_value = run(["git", "-C", str(source), "rev-parse", "--git-dir"])
        git_dir = pathlib.Path(git_dir_value)
        if not git_dir.is_absolute():
            git_dir = source / git_dir
        git_dir = git_dir.resolve()
        run(["git", f"--git-dir={git_dir}", "read-tree", base_tree], env=env)
        # Apply against the exact temporary index first. This neither trusts
        # nor mutates the supplied checkout, and full-index patch headers make
        # every touched r4 blob part of the contract.
        run(
            ["git", f"--git-dir={git_dir}", "apply", "--cached", "--check", str(patch)],
            env=env,
        )
        run(
            ["git", f"--git-dir={git_dir}", "apply", "--cached", str(patch)],
            env=env,
        )
        run(
            ["git", f"--git-dir={git_dir}", "diff", "--cached", "--check", base_tree],
            env=env,
        )
        candidate_tree = run(
            ["git", f"--git-dir={git_dir}", "write-tree"], env=env
        )
        output.mkdir(parents=True)
        run(
            [
                "git",
                f"--git-dir={git_dir}",
                "checkout-index",
                "--all",
                "--force",
                f"--prefix={output.resolve()}/",
            ],
            env=env,
        )
        return candidate_tree


def prepare(
    source: pathlib.Path,
    output: pathlib.Path,
    candidate_root: pathlib.Path = DEFAULT_ROOT,
) -> dict:
    manifest = load_manifest(candidate_root)
    patch = candidate_root / manifest["overlay_patch"]
    try:
        patch_digest = sha256(patch)
    except OSError as exc:
        raise CandidateError("patch_digest_mismatch") from exc
    if patch_digest != manifest["overlay_patch_sha256"]:
        raise CandidateError("patch_digest_mismatch")
    if patch_paths(patch) != manifest["changed_files"]:
        raise CandidateError("patch_scope_mismatch")
    if source_tree(source) != manifest["base_vllm_tree"]:
        raise CandidateError("base_tree_mismatch")

    run_c128a(source, "baseline")
    run_parser(source, "r4", manifest["base_parser_source_id"])
    candidate_tree = materialize(
        source, output, patch, manifest["base_vllm_tree"]
    )
    if candidate_tree != manifest["candidate_vllm_tree"]:
        raise CandidateError("candidate_tree_mismatch")
    validate_python_sources(output, manifest["changed_files"])
    sparse_mla = output / "vllm/models/deepseek_v4/sparse_mla.py"
    try:
        sparse_blob = git_blob_sha1(sparse_mla.read_bytes())
    except OSError as exc:
        raise CandidateError("candidate_blob_mismatch") from exc
    if sparse_blob != manifest["candidate_sparse_mla_blob_sha1"]:
        raise CandidateError("candidate_blob_mismatch")
    run_c128a(output, "candidate")
    run_parser(output, "complete", manifest["candidate_parser_source_id"])
    return {
        "schema_version": 1,
        "status": "pass",
        "candidate": manifest["candidate"],
        "base_image_digest": manifest["base_image_digest"],
        "base_vllm_tree": manifest["base_vllm_tree"],
        "candidate_vllm_tree": candidate_tree,
        "overlay_patch_sha256": patch_digest,
        "parser_source_id": manifest["candidate_parser_source_id"],
        "cache_fingerprint": manifest["cache_fingerprint"],
        "preflights": {
            "c128a": True,
            "parser_cases": PARSER_CASE_COUNT,
            "python_sources": len(manifest["changed_files"]),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--candidate-root", type=pathlib.Path, default=DEFAULT_ROOT)
    args = parser.parse_args()
    try:
        report = prepare(args.source, args.output, args.candidate_root)
    except CandidateError as exc:
        report = {
            "schema_version": 1,
            "status": "reject",
            "reason_code": exc.reason,
        }
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
        return 1
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
