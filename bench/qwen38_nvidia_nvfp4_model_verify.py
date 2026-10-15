#!/usr/bin/env python3
"""Verify the pinned NVIDIA Qwen3.8-Flash-Next NVFP4 checkout."""

import argparse
import hashlib
import json
import os
import pathlib
import stat


REVISION = "fc694b54fb0174e0913e6adf86691ef85a4ead47"
TOTAL_BYTES = 132_734_506_847
SAFETENSOR_BYTES = 132_680_249_378
FILES = {
    ".gitattributes",
    "README.md",
    "chat_template.jinja",
    "config.json",
    "generation_config.json",
    "hf_quant_config.json",
    "merges.txt",
    *{f"model-{index:05d}-of-00010.safetensors" for index in range(1, 11)},
    "model-fp8-mtp-ple.safetensors",
    "model.safetensors.index.json",
    "preprocessor_config.json",
    "processor_config.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "video_preprocessor_config.json",
    "vocab.json",
}


def fail(message: str) -> None:
    raise SystemExit(f"NVIDIA Qwen NVFP4 model verification failed: {message}")


def regular_file(path: pathlib.Path) -> os.stat_result:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        fail(f"not a one-link regular file: {path.name}")
    return info


def content_digest(path: pathlib.Path, etag: str, size: int) -> None:
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


def verify(root: pathlib.Path) -> dict[str, object]:
    info = root.lstat()
    if not stat.S_ISDIR(info.st_mode) or root.resolve() != root:
        fail("model root is not a real directory")
    actual_files = {item.name for item in root.iterdir() if item.is_file()}
    actual_directories = {item.name for item in root.iterdir() if item.is_dir()}
    if actual_files != FILES or actual_directories != {".cache"}:
        fail("checkout file set differs from the pinned 25-file tree")

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

    total = sum(int(record["size"]) for record in records)
    safetensors = sum(
        int(record["size"])
        for record in records
        if str(record["path"]).endswith(".safetensors")
    )
    if total != TOTAL_BYTES or safetensors != SAFETENSOR_BYTES:
        fail("pinned tree sizes changed")

    config = json.loads((root / "config.json").read_bytes())
    quantization = config.get("quantization_config", {})
    layers = quantization.get("quantized_layers", {})
    routed_experts = {
        f"model.language_model.layers.{index}.mlp.experts" for index in range(48)
    }
    if config.get("architectures") != ["Qwen4ExpForConditionalGeneration"]:
        fail("model architecture changed")
    if (
        quantization.get("quant_method") != "modelopt"
        or quantization.get("quant_algo") != "MIXED_PRECISION"
        or set(quantization.get("config_groups", {})) != {"group_0", "group_1", "group_2"}
    ):
        fail("mixed ModelOpt quantization authority changed")
    if any(
        layers.get(name) != {"quant_algo": "NVFP4", "group_size": 16}
        for name in routed_experts
    ):
        fail("routed-expert NVFP4 layout changed")
    if layers.get("mtp.layers.0.mlp.experts") != {
        "quant_algo": "FP8_PB_WO",
        "group_size": 128,
    }:
        fail("corrected MTP FP8 metadata changed")
    if layers.get(
        "model.language_model.layers.1.ple.ple_embedding.ngram_embedding"
    ) != {"quant_algo": "FP8"}:
        fail("PLE FP8 metadata changed")

    manifest = json.dumps(records, sort_keys=True, separators=(",", ":")).encode()
    return {
        "schema_version": 1,
        "repository": "nvidia/Qwen3.8-Flash-Next-NVFP4",
        "revision": REVISION,
        "files": len(records),
        "total_bytes": total,
        "safetensor_bytes": safetensors,
        "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
        "verified": True,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("model_root", type=pathlib.Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.model_root), sort_keys=True))


if __name__ == "__main__":
    main()
