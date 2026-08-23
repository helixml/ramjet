#!/usr/bin/env python3
"""Report what changed among the sources in watchlist/sources.yaml.

The point is a scan you will actually run: it prints what moved since you last
looked, not a list of everything that exists. First run records a baseline and
reports nothing as new, because on a fresh state file every source is "new"
and that is noise rather than signal.

    python3 bench/watchlist_scan.py                  # what changed since last scan
    python3 bench/watchlist_scan.py --since 2026-08-01
    python3 bench/watchlist_scan.py --all            # ignore state, show latest
    python3 bench/watchlist_scan.py --json           # machine-readable

State lives in watchlist/.last-scan.json (git-ignored). Delete it to re-baseline.

Network access is read-only and unauthenticated by default. HuggingFace allows
this; GitHub rate-limits anonymous callers to 60 requests/hour, so set
GITHUB_TOKEN in the environment for a comfortable margin. The token is read
from the environment only -- never pass it in argv, where it would land in
shell history and process listings.
"""

import argparse
import concurrent.futures
import datetime as dt
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCES = ROOT / "watchlist" / "sources.yaml"
STATE = ROOT / "watchlist" / ".last-scan.json"

HF_API = "https://huggingface.co/api"
GH_API = "https://api.github.com"
HUB_API = "https://hub.docker.com/v2"
KINDS = ("hf-org", "hf-repo", "gh-repo", "docker-hub", "site")
USER_AGENT = "ramjet-watchlist-scan"


class SourceError(ValueError):
    """A malformed registry entry."""


def validate_sources(document):
    """Validate the registry. A bad entry fails here, not mid-scan."""
    if not isinstance(document, dict) or not isinstance(document.get("sources"), list):
        raise SourceError("sources.yaml must contain a `sources` list")
    entries = document["sources"]
    if not entries:
        raise SourceError("sources.yaml is empty")
    seen = set()
    for entry in entries:
        if not isinstance(entry, dict):
            raise SourceError("each source must be a mapping")
        name = entry.get("name")
        if not isinstance(name, str) or not name:
            raise SourceError("source name must be a non-empty string")
        if entry.get("kind") not in KINDS:
            raise SourceError(f"{name}: kind must be one of {', '.join(KINDS)}")
        if not isinstance(entry.get("id"), str) or not entry["id"]:
            raise SourceError(f"{name}: id must be a non-empty string")
        # `why` and `watch_for` are not decoration. Without a concrete
        # watch_for, a future scan reports a change nobody can triage.
        for field in ("why", "watch_for"):
            value = entry.get(field)
            if not isinstance(value, str) or len(value.strip()) < 10:
                raise SourceError(f"{name}: {field} must be a meaningful string")
        if entry["kind"] == "gh-repo" and entry["id"].count("/") != 1:
            raise SourceError(f"{name}: gh-repo id must be owner/repo")
        if entry["kind"] == "hf-repo" and entry["id"].count("/") != 1:
            raise SourceError(f"{name}: hf-repo id must be owner/model")
        if entry["kind"] == "docker-hub" and entry["id"].count("/") != 1:
            raise SourceError(f"{name}: docker-hub id must be namespace/repository")
        pattern = entry.get("ignore")
        if pattern is not None:
            if not isinstance(pattern, str) or not pattern:
                raise SourceError(f"{name}: ignore must be a non-empty regex")
            try:
                re.compile(pattern)
            except re.error as error:
                raise SourceError(f"{name}: ignore is not a valid regex: {error}")
        if entry["id"] in seen:
            raise SourceError(f"duplicate source id {entry['id']}")
        seen.add(entry["id"])
    return entries


def load_sources(path=SOURCES):
    with open(path, encoding="utf-8") as handle:
        return validate_sources(yaml.safe_load(handle))


def fetch(url, timeout=15):
    headers = {"User-Agent": USER_AGENT, "Accept": "application/json"}
    token = os.environ.get("GITHUB_TOKEN")
    if token and url.startswith(GH_API):
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def normalize(stamp):
    """Return a comparable ISO-8601 UTC string, or "" if absent."""
    if not stamp:
        return ""
    return str(stamp).replace("Z", "+00:00")[:19]


def probe(entry, timeout):
    """Return {id, items:[{ref, changed, detail}]} or {id, error}."""
    kind, ident = entry["kind"], entry["id"]
    try:
        if kind == "hf-org":
            models = fetch(
                f"{HF_API}/models?author={ident}&sort=lastModified&direction=-1&limit=25",
                timeout,
            )
            items = [
                {
                    "ref": model.get("modelId") or model.get("id", ""),
                    "changed": normalize(model.get("lastModified")),
                    "detail": f"{model.get('downloads', 0)} downloads",
                }
                for model in models
            ]
        elif kind == "hf-repo":
            model = fetch(f"{HF_API}/models/{ident}", timeout)
            items = [
                {
                    "ref": ident,
                    "changed": normalize(model.get("lastModified")),
                    "detail": f"{model.get('downloads', 0)} downloads",
                }
            ]
        elif kind == "gh-repo":
            repo = fetch(f"{GH_API}/repos/{ident}", timeout)
            items = [
                {
                    "ref": ident,
                    "changed": normalize(repo.get("pushed_at")),
                    "detail": f"{repo.get('stargazers_count', 0)} stars",
                }
            ]
            try:
                release = fetch(f"{GH_API}/repos/{ident}/releases/latest", timeout)
                items.append(
                    {
                        "ref": f"{ident}@{release.get('tag_name', '?')}",
                        "changed": normalize(release.get("published_at")),
                        "detail": "release",
                    }
                )
            except Exception:
                # Plenty of active repos publish no releases, and a failure
                # here must not discard the repo result already fetched above:
                # the push timestamp is the signal that matters.
                pass
        elif kind == "docker-hub":
            # We pin engine images by tag, so a new tag is the signal. Ordering
            # by last_updated means the newest tags come first even when the
            # publisher reuses a moving tag name.
            page = fetch(
                f"{HUB_API}/repositories/{ident}/tags"
                "?page_size=25&ordering=last_updated",
                timeout,
            )
            items = [
                {
                    "ref": f"{ident}:{tag.get('name', '?')}",
                    "changed": normalize(tag.get("last_updated")),
                    "detail": f"{round((tag.get('full_size') or 0) / 1e9, 1)}GB",
                }
                for tag in page.get("results", [])
            ]
        else:  # site
            return {"id": ident, "items": [], "manual": True}
        # A source that publishes nightlies would otherwise report a dozen new
        # refs on every scan, and a tool that spams gets ignored. `ignore`
        # drops those refs before they ever reach the state file.
        skip = entry.get("ignore")
        if skip:
            matcher = re.compile(skip)
            items = [item for item in items if not matcher.search(item["ref"])]
        return {"id": ident, "items": [item for item in items if item["changed"]]}
    except Exception as error:  # a dead source must not kill the scan
        return {"id": ident, "error": f"{type(error).__name__}: {error}"}


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--sources", default=str(SOURCES))
    parser.add_argument("--state", default=str(STATE))
    parser.add_argument("--since", help="ISO date; overrides recorded state")
    parser.add_argument("--all", action="store_true", help="ignore state entirely")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument("--no-save", action="store_true")
    args = parser.parse_args(argv)

    entries = load_sources(args.sources)
    state_path = pathlib.Path(args.state)
    state = {}
    if state_path.exists() and not args.all:
        state = json.loads(state_path.read_text()).get("seen", {})
    first_run = not state and not args.since and not args.all

    by_id = {entry["id"]: entry for entry in entries}
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        results = list(pool.map(lambda e: probe(e, args.timeout), entries))

    fresh, errors, manual, seen = [], [], [], {}
    for result in results:
        entry = by_id[result["id"]]
        if result.get("error"):
            errors.append({"name": entry["name"], "error": result["error"]})
            seen.update({k: v for k, v in state.items() if k.startswith(result["id"])})
            continue
        if result.get("manual"):
            manual.append(entry)
            continue
        for item in result["items"]:
            seen[item["ref"]] = item["changed"]
            baseline = args.since or state.get(item["ref"])
            if args.all or (baseline is None and not first_run) or (
                baseline is not None and item["changed"] > baseline
            ):
                fresh.append(
                    {
                        "name": entry["name"],
                        "ref": item["ref"],
                        "changed": item["changed"],
                        "detail": item["detail"],
                        "watch_for": entry["watch_for"].strip(),
                    }
                )

    fresh.sort(key=lambda item: item["changed"], reverse=True)

    if args.json:
        print(json.dumps({"changed": fresh, "errors": errors}, indent=2, sort_keys=True))
    else:
        if first_run:
            print(f"Baseline recorded for {len(seen)} refs. Re-run later to see changes.\n")
        elif not fresh:
            print("Nothing new.\n")
        else:
            print(f"{len(fresh)} change(s):\n")
            for item in fresh:
                print(f"  {item['changed']}  {item['ref']}  ({item['detail']})")
                print(f"      via {item['name']} — watch for: {item['watch_for']}\n")
        if manual:
            print("Check by hand (no machine-readable feed):")
            for entry in manual:
                print(f"  {entry['name']}: {entry['id']}")
            print()
        for failure in errors:
            print(f"  ! {failure['name']}: {failure['error']}", file=sys.stderr)

    if not args.no_save and not args.since and not args.all:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(
            json.dumps(
                {"scanned": dt.datetime.now(dt.timezone.utc).isoformat(), "seen": seen},
                indent=2,
                sort_keys=True,
            )
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
