#!/usr/bin/env python3
"""Fail closed unless the reviewed GLM-5.3-Flash checkpoint is complete."""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import sys


EXPECTED_METADATA = {
    "config.json": "a47f597b347010c4d8bca9ef584ce4d2fbf20a0a6ec86ad7694e71184b7e95e5",
    "model.safetensors.index.json": "f3a4c40897e00fab0de0380b05b66279bc341233cc14fa71a80bab2b683e3b7b",
    "chat_template.jinja": "41cff9af7b3a86c96751b107a8444f245fbda0bd5320b636a5bb1f7f4ba1a5c3",
    "generation_config.json": "230c30609ecbbb9e6583bedde8e7bdda0c6eb8fe5fad0eaeb3d1b293d751cb4f",
}
SHARD_PATTERN = re.compile(r"model-(\d{5})-of-00120\.safetensors")
EXPECTED_SHARD_COUNT = 120
EXPECTED_TENSOR_BYTES = 194_660_206_040


def fail(message: str) -> None:
    raise ValueError(message)


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
        fail("safetensors index does not reference the exact 120-shard set")

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
        "GLM-5.3-Flash checkpoint verified: exact metadata, "
        f"{EXPECTED_SHARD_COUNT} shards, {EXPECTED_TENSOR_BYTES} tensor bytes"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
