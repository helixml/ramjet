#!/usr/bin/env python3
"""Verify every downloaded Swift 1.5 model file against pinned Hub blobs."""

import argparse
import hashlib
import json
import os
import pathlib
import stat


REVISION = "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38"
MANIFEST_SHA256 = "de956cabfc92ef5cd382e2557d03aa22d25fb57be3fe9048ed844d6863800287"
TOTAL_BYTES = 186_458_010_084
SAFETENSOR_BYTES = 186_395_943_704


def fail(message: str) -> None:
    raise SystemExit(f"Swift 1.5 model verification failed: {message}")


def regular(path: pathlib.Path) -> os.stat_result:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        fail(f"not a one-link regular file: {path.name}")
    return info


def digest_file(path: pathlib.Path, etag: str, size: int) -> str:
    if len(etag) == 64:
        digest = hashlib.sha256()
    elif len(etag) == 40:
        digest = hashlib.sha1()
        digest.update(f"blob {size}\0".encode())
    else:
        fail(f"invalid pinned etag: {path.name}")
    with path.open("rb", buffering=0) as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def verify(root: pathlib.Path, manifest_path: pathlib.Path) -> dict[str, object]:
    manifest_bytes = manifest_path.read_bytes()
    if hashlib.sha256(manifest_bytes).hexdigest() != MANIFEST_SHA256:
        fail("pinned Hub manifest changed")
    manifest = json.loads(manifest_bytes)
    files = manifest["files"]
    if (
        manifest.get("schema_version") != 1
        or manifest.get("repository") != "ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4"
        or manifest.get("revision") != REVISION
        or len(files) != 98
    ):
        fail("pinned Hub identity changed")
    info = root.lstat()
    if not stat.S_ISDIR(info.st_mode) or root.resolve() != root:
        fail("model root is not a real directory")
    expected = {record["path"] for record in files}
    if len(expected) != len(files) or any("/" in name or name in (".", "..") for name in expected):
        fail("invalid pinned file list")
    actual_files = {item.name for item in root.iterdir() if item.is_file()}
    actual_dirs = {item.name for item in root.iterdir() if item.is_dir()}
    if actual_files != expected or actual_dirs != {".cache"}:
        fail("checkout file set differs from pinned Hub tree")
    metadata_root = root / ".cache" / "huggingface" / "download"
    if not metadata_root.is_dir() or metadata_root.is_symlink():
        fail("Hugging Face local-dir metadata is missing")
    total = 0
    safetensors = 0
    for record in files:
        name, size, etag = record["path"], record["size"], record["etag"]
        if not isinstance(size, int) or size < 0 or not isinstance(etag, str):
            fail(f"invalid pinned metadata: {name}")
        path = root / name
        if regular(path).st_size != size:
            fail(f"size differs from pinned Hub blob: {name}")
        metadata_path = metadata_root / f"{name}.metadata"
        regular(metadata_path)
        metadata = metadata_path.read_text().splitlines()
        if len(metadata) != 3 or metadata[0] != REVISION or metadata[1] != etag:
            fail(f"download metadata differs from pinned Hub blob: {name}")
        if digest_file(path, etag, size) != etag:
            fail(f"content digest differs from pinned Hub blob: {name}")
        total += size
        if name.endswith(".safetensors"):
            safetensors += size
    if total != TOTAL_BYTES or safetensors != SAFETENSOR_BYTES:
        fail("pinned model size changed")
    config = json.loads((root / "config.json").read_bytes())
    quant = config.get("quantization_config", {})
    if (
        config.get("architectures") != ["Qwen4ExpForConditionalGeneration"]
        or quant.get("quant_method") != "modelopt"
        or quant.get("quant_algo") != "NVFP4"
    ):
        fail("model architecture or quantization changed")
    return {
        "schema_version": 1,
        "repository": manifest["repository"],
        "revision": REVISION,
        "files": len(files),
        "total_bytes": total,
        "safetensor_bytes": safetensors,
        "manifest_sha256": MANIFEST_SHA256,
        "verified": True,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("model_root", type=pathlib.Path)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=pathlib.Path(__file__).with_name("swift15-model-manifest.json"),
    )
    args = parser.parse_args()
    print(json.dumps(verify(args.model_root, args.manifest), sort_keys=True))


if __name__ == "__main__":
    main()
