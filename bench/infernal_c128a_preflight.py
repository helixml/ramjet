#!/usr/bin/env python3
"""GPU-free, content-safe Infernal C128A source qualification.

The baseline mode proves that a checkout contains the exact C128A-relevant
Infernal Invocation r4 source. Candidate mode accepts only the capture-stable
fixed-capacity row layout. Reports contain hashes, booleans, and bounded reason
codes; source text and filesystem paths are never returned.
"""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import sys
from dataclasses import dataclass
from pathlib import Path


SPARSE_MLA_PATH = Path("vllm/models/deepseek_v4/sparse_mla.py")
GPU_RUNNER_PATH = Path("vllm/v1/worker/gpu_model_runner.py")

# Immutable r4 identities are copied from its public release lock and receipt.
# The upstream proposal is recorded by commit because PR refs can move.
EXPECTED_IDENTITIES = {
    "r4_image_digest": (
        "sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6"
    ),
    "r4_docker_commit": "0040f0af0670d0e5bb0f6bea6ee7cd2de2990b01",
    "r4_vllm_base_commit": "ce5f50f6d01b02336c4207f11277fd7bedacb4d6",
    "r4_vllm_integration_tree": "3226eb7ff642702908f502a2402f9d083d16511c",
    "r4_vllm_integration_patch_sha256": (
        "dec8963846acbd52dd76500900286fa596da83cafbe1abbc55a8b190e16b8279"
    ),
    "r4_sparse_mla_blob_sha1": "e8a58130fb12688125fd5e78ad897a3e7c8408b1",
    "r4_gpu_model_runner_blob_sha1": "f84d205021d6fe154c8cf51980fb441824fe35bc",
    "upstream_pr": 51318,
    "upstream_head": "b5a04d25e8e9f3b01a26b57ea6644b71ce44c414",
    "upstream_patch_sha256": (
        "ae80086e6de06712524c3f7b5060c958623e3fd7bbde9bbdd1d34594b5d2795a"
    ),
    "r4_minimal_semantic_port_patch_sha256": (
        "380b9ed87602c12d9ded39646038b95b88f19221d342df29813fc3d9ca6d7b8b"
    ),
}

REASON_CODES = {
    "capacity_origin_mismatch",
    "capture_origin_mismatch",
    "layout_batch_dependent",
    "layout_unknown",
    "r4_blob_mismatch",
    "r4_layout_mismatch",
    "source_missing",
    "source_syntax",
    "stride_keyword_mismatch",
}


@dataclass(frozen=True)
class SourceInspection:
    layout: str
    stride_keyword_matches: bool
    capacity_comes_from_max_model_len: bool
    capture_uses_max_model_len: bool
    sparse_blob_sha1: str
    runner_blob_sha1: str


class InspectionFailure(Exception):
    """A bounded source-inspection failure safe to report by reason code."""

    def __init__(self, reason_code: str):
        if reason_code not in REASON_CODES:
            raise ValueError("unbounded preflight reason code")
        self.reason_code = reason_code
        super().__init__(reason_code)


def git_blob_sha1(content: bytes) -> str:
    header = f"blob {len(content)}\0".encode()
    return hashlib.sha1(header + content, usedforsecurity=False).hexdigest()


def _function(tree: ast.AST, name: str) -> ast.FunctionDef:
    functions = [
        node
        for node in ast.walk(tree)
        if isinstance(node, ast.FunctionDef) and node.name == name
    ]
    if len(functions) != 1:
        raise InspectionFailure("source_syntax")
    return functions[0]


def _assigned_name(node: ast.Assign, name: str) -> bool:
    return any(
        isinstance(target, ast.Name) and target.id == name for target in node.targets
    )


def _is_self_attribute(node: ast.AST, attribute: str) -> bool:
    return (
        isinstance(node, ast.Attribute)
        and node.attr == attribute
        and isinstance(node.value, ast.Name)
        and node.value.id == "self"
    )


def _layout(build: ast.FunctionDef) -> tuple[str, bool]:
    assignments = [
        node
        for node in ast.walk(build)
        if isinstance(node, ast.Assign) and _assigned_name(node, "active_topk_width")
    ]
    if len(assignments) != 1:
        return "unknown", False
    value = assignments[0].value
    if _is_self_attribute(value, "c128a_max_compressed"):
        layout = "capture_stable_capacity"
    elif any(
        (isinstance(node, ast.Name) and node.id == "max_seq_len")
        or (isinstance(node, ast.Attribute) and node.attr == "max_seq_len")
        or (
            isinstance(node, ast.Name)
            and node.id == "get_c128a_active_topk_width"
        )
        for node in ast.walk(value)
    ):
        layout = "batch_dependent"
    else:
        layout = "unknown"

    keywords = [
        keyword
        for call in ast.walk(build)
        if isinstance(call, ast.Call)
        for keyword in call.keywords
        if keyword.arg == "max_compressed_tokens"
    ]
    stride_matches = len(keywords) == 1 and (
        isinstance(keywords[0].value, ast.Name)
        and keywords[0].value.id == "active_topk_width"
    )
    return layout, stride_matches


def _capacity_comes_from_max_model_len(tree: ast.AST) -> bool:
    for assignment in ast.walk(tree):
        if not isinstance(assignment, ast.Assign) or not _assigned_name(
            assignment, "c128a_max_compressed"
        ):
            continue
        call = assignment.value
        if not (
            isinstance(call, ast.Call)
            and isinstance(call.func, ast.Name)
            and call.func.id == "get_c128a_topk_width"
            and len(call.args) >= 2
        ):
            continue
        if _is_self_attribute(call.args[1], "compress_ratio") and (
            isinstance(call.args[0], ast.Attribute)
            and call.args[0].attr == "max_model_len"
            and _is_self_attribute(call.args[0].value, "model_config")
        ):
            return True
    return False


def _capture_uses_max_model_len(tree: ast.AST) -> bool:
    build = _function(tree, "_build_attention_metadata")
    for branch in ast.walk(build):
        if not (
            isinstance(branch, ast.If)
            and isinstance(branch.test, ast.Name)
            and branch.test.id == "for_cudagraph_capture"
        ):
            continue
        for assignment in branch.body:
            if (
                isinstance(assignment, ast.Assign)
                and _assigned_name(assignment, "max_seq_len")
                and _is_self_attribute(assignment.value, "max_model_len")
            ):
                return True
    return False


def inspect_source(root: Path) -> SourceInspection:
    try:
        sparse = (root / SPARSE_MLA_PATH).read_bytes()
        runner = (root / GPU_RUNNER_PATH).read_bytes()
    except OSError as exc:
        raise InspectionFailure("source_missing") from exc
    try:
        sparse_tree = ast.parse(sparse)
        runner_tree = ast.parse(runner)
        build = _function(sparse_tree, "_build_c128a_metadata")
        layout, stride_matches = _layout(build)
        capacity_matches = _capacity_comes_from_max_model_len(sparse_tree)
        capture_matches = _capture_uses_max_model_len(runner_tree)
    except (SyntaxError, UnicodeDecodeError) as exc:
        raise InspectionFailure("source_syntax") from exc
    return SourceInspection(
        layout=layout,
        stride_keyword_matches=stride_matches,
        capacity_comes_from_max_model_len=capacity_matches,
        capture_uses_max_model_len=capture_matches,
        sparse_blob_sha1=git_blob_sha1(sparse),
        runner_blob_sha1=git_blob_sha1(runner),
    )


def validate(root: Path, mode: str) -> tuple[dict[str, object], bool]:
    if mode not in {"baseline", "candidate"}:
        raise ValueError("mode must be baseline or candidate")
    errors: list[str] = []
    observed: dict[str, object] = {}
    try:
        inspection = inspect_source(root)
    except InspectionFailure as exc:
        errors.append(exc.reason_code)
    else:
        observed = {
            "capacity_comes_from_max_model_len": (
                inspection.capacity_comes_from_max_model_len
            ),
            "capture_uses_max_model_len": inspection.capture_uses_max_model_len,
            "layout": inspection.layout,
            "runner_blob_sha1": inspection.runner_blob_sha1,
            "sparse_blob_sha1": inspection.sparse_blob_sha1,
            "stride_keyword_matches": inspection.stride_keyword_matches,
        }
        if not inspection.stride_keyword_matches:
            errors.append("stride_keyword_mismatch")
        if not inspection.capacity_comes_from_max_model_len:
            errors.append("capacity_origin_mismatch")
        if not inspection.capture_uses_max_model_len:
            errors.append("capture_origin_mismatch")

        if mode == "baseline":
            exact_blobs = (
                inspection.sparse_blob_sha1
                == EXPECTED_IDENTITIES["r4_sparse_mla_blob_sha1"]
                and inspection.runner_blob_sha1
                == EXPECTED_IDENTITIES["r4_gpu_model_runner_blob_sha1"]
            )
            if not exact_blobs:
                errors.append("r4_blob_mismatch")
            if inspection.layout != "batch_dependent":
                errors.append("r4_layout_mismatch")
        elif inspection.layout == "batch_dependent":
            errors.append("layout_batch_dependent")
        elif inspection.layout != "capture_stable_capacity":
            errors.append("layout_unknown")

    errors = sorted(set(errors))
    passed = not errors
    report = {
        "schema_version": 1,
        "mode": mode,
        "status": "pass" if passed else "reject",
        "reason_codes": errors,
        "expected_identities": EXPECTED_IDENTITIES,
        "observed": observed,
    }
    return report, passed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("baseline", "candidate"))
    parser.add_argument("source_root", type=Path)
    args = parser.parse_args()
    report, passed = validate(args.source_root, args.mode)
    sys.stdout.write(json.dumps(report, sort_keys=True, separators=(",", ":")) + "\n")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
