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
REPOSITORY = "ghcr.io/helixml/ramjet"
PUBLISHED_DIGEST = "sha256:1d0d9c383119f43b832008d2b2866c43472175bf0d814d27032b677e30dcac43"


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
    pinned = f"{reference}@{PUBLISHED_DIGEST}"
    if drone.count(f"image: {pinned}") != 2:
        errors.append(f".drone.yml must consume {pinned!r} exactly two times")
    if drone.count(f"--destination {reference}") != 1:
        errors.append(f".drone.yml must publish {reference!r} exactly once")
    return errors


def update_references(root: pathlib.Path = ROOT) -> str:
    key = image_key(root)
    (root / KEY_FILE).write_text(f"{key}\n")
    path = root / ".drone.yml"
    reference = image_reference(key)
    text = path.read_text()
    destination_pattern = r"(?m)(--destination )ghcr\.io/helixml/ramjet:release-tools-sha256-[0-9a-f]+(?:@sha256:[0-9a-f]+)?"
    updated, destinations = re.subn(destination_pattern, rf"\g<1>{reference}", text)
    consumer_pattern = r"(?m)(image: )ghcr\.io/helixml/ramjet:release-tools-sha256-[0-9a-f]+(?:@sha256:[0-9a-f]+)?"
    updated, consumers = re.subn(
        consumer_pattern, rf"\g<1>{reference}@{PUBLISHED_DIGEST}", updated
    )
    if destinations != 1 or consumers != 2:
        raise ValueError(
            ".drone.yml: expected one release-tools destination and two consumers, "
            f"found {destinations} and {consumers}"
        )
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
