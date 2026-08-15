#!/usr/bin/env python3
"""Mirror the canonical rtx6000pro Grafana dashboards into the infra repository.

mini-dynamo owns the dashboard JSON; the infra repository only carries the
ConfigMap that Flux reconciles into the monitoring namespace. This script
rewrites exactly the keys this directory owns and leaves every other dashboard
in that ConfigMap byte-identical.

    python3 sync-dashboards.py --check ../../../infra
    python3 sync-dashboards.py ../../../infra
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

SOURCE_DIR = Path(__file__).resolve().parent
TARGET_RELATIVE = Path("clusters/bunker/monitoring/grafana-dashboards.yaml")

# Keys this directory owns in the shared ConfigMap.
OWNED_KEYS = ("minidynamo-rtx6000pro.json",)

# Keys superseded by OWNED_KEYS; removed from the ConfigMap on sync so Grafana's
# sidecar deletes the stale dashboard instead of leaving an orphaned copy.
RETIRED_KEYS = ("ds4-flash-serving.json",)

BLOCK_KEY = re.compile(r"^  ([A-Za-z0-9._-]+): \|\s*$")


class SyncError(Exception):
    pass


def canonical_json(path: Path) -> str:
    """Return the dashboard's canonical serialization, ending in one newline."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise SyncError(f"{path}: invalid JSON: {exc}") from exc
    return json.dumps(document, indent=2, ensure_ascii=False) + "\n"


def split_configmap(text: str) -> tuple[list[str], list[tuple[str, list[str]]]]:
    """Split the ConfigMap into its header and its ordered data blocks.

    Every block is kept as raw lines so untouched dashboards round-trip exactly.
    """
    lines = text.splitlines(keepends=True)
    try:
        data_at = next(i for i, line in enumerate(lines) if line.rstrip("\n") == "data:")
    except StopIteration as exc:
        raise SyncError("no top-level 'data:' mapping in the ConfigMap") from exc

    header = lines[: data_at + 1]
    blocks: list[tuple[str, list[str]]] = []
    for line in lines[data_at + 1 :]:
        match = BLOCK_KEY.match(line)
        if match:
            blocks.append((match.group(1), [line]))
        elif blocks:
            blocks[-1][1].append(line)
        elif line.strip():
            raise SyncError(f"unexpected content under 'data:': {line!r}")
    if not blocks:
        raise SyncError("no dashboard keys found under 'data:'")
    return header, blocks


def render(key: str, body: str) -> list[str]:
    """Render one dashboard as an indented YAML literal block."""
    out = [f"  {key}: |\n"]
    out.extend(f"    {line}\n" if line else "\n" for line in body.splitlines())
    return out


def build(target_text: str, dashboards: dict[str, str]) -> str:
    header, blocks = split_configmap(target_text)
    rendered: list[list[str]] = []
    replaced: set[str] = set()
    for key, lines in blocks:
        if key in RETIRED_KEYS:
            continue
        if key in dashboards:
            # Refresh in place so an owned key keeps its position in the map.
            rendered.append(render(key, dashboards[key]))
            replaced.add(key)
        else:
            rendered.append(lines)
    for key, body in dashboards.items():
        if key not in replaced:
            rendered.append(render(key, body))
    return "".join(header) + "".join(line for block in rendered for line in block)


def resolve_target(repository: str) -> Path:
    try:
        root = subprocess.run(
            ["git", "-C", repository, "rev-parse", "--show-toplevel"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as exc:
        raise SyncError(f"{repository}: not a git repository") from exc
    target = Path(root) / TARGET_RELATIVE
    if not target.is_file():
        raise SyncError(f"{target}: missing; is this the infra repository?")
    return target


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="report drift, change nothing")
    parser.add_argument("infra_repository", help="path to a checkout of the infra repository")
    args = parser.parse_args()

    try:
        dashboards = {key: canonical_json(SOURCE_DIR / key) for key in OWNED_KEYS}
        target = resolve_target(args.infra_repository)
        current = target.read_text(encoding="utf-8")
        updated = build(current, dashboards)
    except SyncError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if current == updated:
        print(f"dashboard mirror is current: {target}")
        return 0
    if args.check:
        print(f"dashboard mirror is stale: run {Path(__file__).name} {args.infra_repository}", file=sys.stderr)
        return 1
    target.write_text(updated, encoding="utf-8")
    print(f"updated dashboard mirror: {target}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
