#!/usr/bin/env python3
"""Validate the content-keyed release-tools image used by Drone."""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
DOCKERFILE = pathlib.Path("Dockerfile.release-tools")
KEY_FILE = pathlib.Path(".docker/release-tools-key")
REPOSITORY = "ghcr.io/helixml/mini-dynamo"


def image_key(root: pathlib.Path = ROOT) -> str:
    payload = (root / DOCKERFILE).read_bytes()
    return hashlib.sha256(payload).hexdigest()


def image_reference(key: str) -> str:
    return f"{REPOSITORY}:release-tools-sha256-{key}"


def validation_errors(root: pathlib.Path = ROOT) -> list[str]:
    key = image_key(root)
    actual = (root / KEY_FILE).read_text().strip() if (root / KEY_FILE).is_file() else ""
    errors = []
    if actual != key:
        errors.append(f"{KEY_FILE} is stale: expected {key}, found {actual or '<missing>'}")
    drone = (root / ".drone.yml").read_text()
    reference = image_reference(key)
    if drone.count(f"image: {reference}") != 4:
        errors.append(f".drone.yml must consume {reference!r} exactly four times")
    if drone.count(f"--destination {reference}") != 1:
        errors.append(f".drone.yml must publish {reference!r} exactly once")
    return errors


def update_references(root: pathlib.Path = ROOT) -> str:
    key = image_key(root)
    (root / KEY_FILE).write_text(f"{key}\n")
    path = root / ".drone.yml"
    reference = image_reference(key)
    updated, count = re.subn(
        r"ghcr\.io/helixml/mini-dynamo:release-tools-sha256-[0-9a-f]+",
        reference,
        path.read_text(),
    )
    if count != 5:
        raise ValueError(f".drone.yml: expected five release-tools references, found {count}")
    path.write_text(updated)
    return reference


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--print-reference", action="store_true")
    args = parser.parse_args()
    if args.update:
        print(update_references())
        return 0
    if args.print_reference:
        print(image_reference(image_key()))
        return 0
    errors = validation_errors()
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(image_reference(image_key()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
