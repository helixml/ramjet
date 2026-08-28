#!/usr/bin/env python3
"""Verify the immutable Inferact Flash-Next NVFP4 local-dir checkout."""

import argparse
import hashlib
import json
import pathlib
import stat


REVISION = "103a7608316173ca6edd49929544244de7ffda70"
TOTAL_BYTES = 182_838_060_595
SAFETENSOR_BYTES = 182_779_284_200
FILES = {
    ".gitattributes",
    "LICENSE",
    "README.md",
    "chat_template.jinja",
    "config.json",
    "generation_config.json",
    "merges.txt",
    "model-00001-of-00004.safetensors",
    "model-00002-of-00004.safetensors",
    "model-00003-of-00004.safetensors",
    "model-00004-of-00004.safetensors",
    "model.safetensors.index.json",
    *{f"nvfp4_experts-{index:05d}-of-00016.safetensors" for index in range(1, 17)},
    "nvfp4_experts_mtp.safetensors",
    "preprocessor_config.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "video_preprocessor_config.json",
    "vocab.json",
}


def fail(message):
    raise SystemExit(f"Qwen NVFP4 model verification failed: {message}")


def regular_file(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        fail(f"not a one-link regular file: {path.name}")
    return info


def content_digest(path, etag, size):
    if len(etag) == 64:
        digest = hashlib.sha256()
    elif len(etag) == 40:
        digest = hashlib.sha1()
        digest.update(f"blob {size}\0".encode())
    else:
        fail(f"unexpected Hub etag for {path.name}")
    with path.open("rb", buffering=0) as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    if digest.hexdigest() != etag:
        fail(f"content digest differs from Hub metadata: {path.name}")


def verify(root):
    info = root.lstat()
    if not stat.S_ISDIR(info.st_mode) or root.resolve() != root:
        fail("model root is not a real directory")
    actual_files = {item.name for item in root.iterdir() if item.is_file()}
    actual_directories = {item.name for item in root.iterdir() if item.is_dir()}
    if actual_files != FILES or actual_directories != {".cache"}:
        fail("checkout file set differs from the pinned 34-file tree")

    metadata_root = root / ".cache" / "huggingface" / "download"
    if not metadata_root.is_dir() or metadata_root.is_symlink():
        fail("Hugging Face local-dir metadata is missing")

    records = []
    for name in sorted(FILES):
        path = root / name
        file_info = regular_file(path)
        metadata_path = metadata_root / f"{name}.metadata"
        regular_file(metadata_path)
        lines = metadata_path.read_text().splitlines()
        if len(lines) != 3 or lines[0] != REVISION:
            fail(f"download authority differs from the pinned revision: {name}")
        etag = lines[1]
        if not etag or any(character not in "0123456789abcdef" for character in etag):
            fail(f"invalid Hub etag: {name}")
        content_digest(path, etag, file_info.st_size)
        records.append({"path": name, "size": file_info.st_size, "etag": etag})

    total = sum(record["size"] for record in records)
    safetensors = sum(
        record["size"] for record in records if record["path"].endswith(".safetensors")
    )
    if total != TOTAL_BYTES or safetensors != SAFETENSOR_BYTES:
        fail("pinned tree sizes changed")

    config = json.loads((root / "config.json").read_bytes())
    quantization = config.get("quantization_config", {})
    if config.get("architectures") != ["Qwen4ExpForConditionalGeneration"]:
        fail("model architecture changed")
    if (
        quantization.get("quant_method") != "modelopt"
        or quantization.get("quant_algo") != "NVFP4"
        or quantization.get("group_size") != 16
        or quantization.get("calibration_applied") is not True
    ):
        fail("NVFP4 quantization authority changed")

    manifest = json.dumps(records, sort_keys=True, separators=(",", ":")).encode()
    return {
        "schema_version": 1,
        "repository": "Inferact/Qwen3.8-Flash-Next-NVFP4",
        "revision": REVISION,
        "files": len(records),
        "total_bytes": total,
        "safetensor_bytes": safetensors,
        "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
        "verified": True,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("model_root", type=pathlib.Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.model_root), sort_keys=True))


if __name__ == "__main__":
    main()
