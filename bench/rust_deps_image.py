#!/usr/bin/env python3
"""Validate the immutable Rust dependency-image reference used by CI."""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
INPUTS = (
    pathlib.Path("Cargo.toml"),
    pathlib.Path("Cargo.lock"),
    pathlib.Path("rust-toolchain.toml"),
    pathlib.Path("Dockerfile.deps"),
)
KEY_FILE = pathlib.Path(".docker/rust-deps-key")
REPOSITORY = "ghcr.io/helixml/mini-dynamo"


def dependency_key(root: pathlib.Path = ROOT) -> str:
    digest = hashlib.sha256()
    for relative in INPUTS:
        payload = (root / relative).read_bytes()
        encoded_name = relative.as_posix().encode()
        digest.update(len(encoded_name).to_bytes(4, "big"))
        digest.update(encoded_name)
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return digest.hexdigest()


def image_reference(key: str) -> str:
    return f"{REPOSITORY}:rust-deps-sha256-{key}"


def validation_errors(root: pathlib.Path = ROOT) -> list[str]:
    expected_key = dependency_key(root)
    key_path = root / KEY_FILE
    actual_key = key_path.read_text().strip() if key_path.is_file() else ""
    errors: list[str] = []
    if actual_key != expected_key:
        errors.append(
            f"{KEY_FILE} is stale: expected {expected_key}, found {actual_key or '<missing>'}"
        )

    expected_reference = image_reference(expected_key)
    for relative in (pathlib.Path("Dockerfile"), pathlib.Path("Dockerfile.companion")):
        text = (root / relative).read_text()
        declaration = f"ARG RUST_DEPS_IMAGE={expected_reference}"
        if text.count(declaration) != 1:
            errors.append(f"{relative} must contain exactly one {declaration!r}")

    drone = (root / ".drone.yml").read_text()
    tag = f"rust-deps-sha256-{expected_key}"
    if drone.count(f"- {tag}") != 1:
        errors.append(f".drone.yml must publish exactly one {tag!r} tag")
    return errors


def update_references(root: pathlib.Path = ROOT) -> str:
    key = dependency_key(root)
    key_path = root / KEY_FILE
    key_path.parent.mkdir(parents=True, exist_ok=True)
    key_path.write_text(f"{key}\n")

    reference = image_reference(key)
    replacements = (
        (
            pathlib.Path("Dockerfile"),
            r"(?m)^ARG RUST_DEPS_IMAGE=.*$",
            f"ARG RUST_DEPS_IMAGE={reference}",
        ),
        (
            pathlib.Path("Dockerfile.companion"),
            r"(?m)^ARG RUST_DEPS_IMAGE=.*$",
            f"ARG RUST_DEPS_IMAGE={reference}",
        ),
        (
            pathlib.Path(".drone.yml"),
            r"(?m)^        - rust-deps-sha256-[0-9a-f]+$",
            f"        - rust-deps-sha256-{key}",
        ),
    )
    for relative, pattern, replacement in replacements:
        path = root / relative
        updated, count = re.subn(pattern, replacement, path.read_text())
        if count != 1:
            raise ValueError(f"{relative}: expected one dependency-image reference, found {count}")
        path.write_text(updated)
    return reference


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--print-key", action="store_true", help="print the key derived from dependency inputs"
    )
    parser.add_argument(
        "--print-reference", action="store_true", help="print the complete GHCR reference"
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help="update the key and all Docker/Drone references after an input change",
    )
    args = parser.parse_args()
    key = dependency_key()
    if args.print_key:
        print(key)
        return 0
    if args.print_reference:
        print(image_reference(key))
        return 0
    if args.update:
        print(update_references())
        return 0
    errors = validation_errors()
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print(image_reference(key))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
