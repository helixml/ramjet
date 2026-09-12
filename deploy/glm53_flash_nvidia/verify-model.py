#!/usr/bin/env python3
"""Fail closed unless the pinned NVIDIA GLM-5.3-Flash NVFP4 checkpoint is complete.

A 190GiB download that is short one shard, or silently followed the mutable
branch tip, fails inside a multi-minute engine start on eight shared GPUs. This
answers the same question from disk, before a container is created.
"""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import sys


MODEL_REPOSITORY = "nvidia/GLM-5.3-Flash-NVFP4"
MODEL_REVISION = "423acf37583782c51c142d145aef733d72943d93"

# Digests of the metadata that decides how the checkpoint is served: the
# architecture and quantization config, the shard map, the sampling defaults,
# the quantization recipe, and the tokenizer/template pair whose contents
# change token IDs. Captured at MODEL_REVISION.
EXPECTED_METADATA = {
    "config.json": "e23c5d98f53e861d49a51bd3c68591621c5482ce829e42c31724152322fba03d",
    "model.safetensors.index.json": "26765b2601fd246ef361cfb9f5e10f9fb291a59e05ad0a109062f3a4747c7fd1",
    "generation_config.json": "a07de3408f578c6a7ca8a1646aa91a41df55d539349fda15fb8b611eb007e9b7",
    "hf_quant_config.json": "1277df937b780e7de8eace66286ffad207f57d33b536b40c9c35635b235c17c6",
    "chat_template.jinja": "34d5ee66b12fa6446cdae131c352b8f68cd85369e0e6fda115583805fada3891",
    "processor_config.json": "aae38374c94b08cc9b0547c6e64f05b951bd9735cea571c6988f5ed552bed3ed",
    "tokenizer.json": "19e773648cb4e65de8660ea6365e10acca112d42a854923df93db4a6f333a82d",
    "tokenizer_config.json": "98b1271574f41abf89427ae2dda030d94dc9478f0edc5a8bd240db213c6fd5fc",
}
EXPECTED_SHARD_COUNT = 33
EXPECTED_TENSOR_BYTES = 204_439_103_396
SHARD_PATTERN = re.compile(r"model-(\d{5})-of-00033\.safetensors")


class VerificationError(ValueError):
    pass


def fail(message: str) -> None:
    raise VerificationError(message)


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            value.update(chunk)
    return value.hexdigest()


def verify(root: pathlib.Path) -> None:
    if not root.is_absolute() or not root.is_dir() or root.is_symlink():
        fail("model root must be an absolute, non-symlink directory")

    incomplete = list(root.rglob("*.incomplete"))
    if incomplete:
        fail(f"download is incomplete ({len(incomplete)} partial files remain)")

    for name, expected in EXPECTED_METADATA.items():
        path = root / name
        if not path.is_file() or path.is_symlink():
            fail(f"missing regular metadata file: {name}")
        if digest(path) != expected:
            fail(f"metadata digest mismatch: {name}")

    index = json.loads((root / "model.safetensors.index.json").read_text())
    weight_map = index.get("weight_map")
    if not isinstance(weight_map, dict) or not weight_map:
        fail("safetensors index has no weight_map")
    referenced = set(weight_map.values())
    if not all(isinstance(name, str) for name in referenced):
        fail("safetensors index contains a non-string shard name")

    expected_names = {
        f"model-{number:05d}-of-{EXPECTED_SHARD_COUNT:05d}.safetensors"
        for number in range(1, EXPECTED_SHARD_COUNT + 1)
    }
    if referenced != expected_names:
        fail(f"safetensors index does not reference the exact {EXPECTED_SHARD_COUNT}-shard set")

    present: set[str] = set()
    total_bytes = 0
    for path in root.glob("*.safetensors"):
        if path.is_symlink() or not path.is_file():
            fail(f"checkpoint shard is not a regular file: {path.name}")
        if not SHARD_PATTERN.fullmatch(path.name):
            fail(f"unexpected safetensors file: {path.name}")
        present.add(path.name)
        total_bytes += path.stat().st_size
    if present != expected_names:
        fail(f"checkpoint has {len(present)} of {EXPECTED_SHARD_COUNT} shards")
    if total_bytes != EXPECTED_TENSOR_BYTES:
        fail(
            "checkpoint tensor byte count mismatch: "
            f"got {total_bytes}, expected {EXPECTED_TENSOR_BYTES}"
        )


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {pathlib.Path(sys.argv[0]).name} MODEL_DIR", file=sys.stderr)
        return 2
    try:
        verify(pathlib.Path(sys.argv[1]))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"verify-model.py: {error}", file=sys.stderr)
        return 1
    print(
        f"{MODEL_REPOSITORY} checkpoint verified at {MODEL_REVISION}: exact metadata, "
        f"{EXPECTED_SHARD_COUNT} shards, {EXPECTED_TENSOR_BYTES} tensor bytes"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
