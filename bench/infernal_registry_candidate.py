#!/usr/bin/env python3
"""Validate an immutable Infernal candidate without pulling its image layers."""

import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import re
import subprocess
import time


REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = (
    REPO_ROOT / "deploy/dspark_0731/infernal-r11-candidate/manifest.json"
)
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")


class CandidateError(RuntimeError):
    def __init__(self, reason, field=None):
        super().__init__(reason)
        self.reason = reason
        self.field = field


def load_manifest(path=DEFAULT_MANIFEST):
    try:
        manifest = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise CandidateError("invalid_manifest") from error
    validate_manifest(manifest)
    return manifest


def _image_contract(value, name):
    if not isinstance(value, dict):
        raise CandidateError("invalid_manifest", name)
    digest = value.get("image_digest")
    image = value.get("image")
    config_digest = value.get("config_digest")
    if not isinstance(digest, str) or not DIGEST.fullmatch(digest):
        raise CandidateError("invalid_manifest", f"{name}.image_digest")
    if not isinstance(image, str) or image.count("@") != 1 or not image.endswith(
        "@" + digest
    ):
        raise CandidateError("invalid_manifest", f"{name}.image")
    if not isinstance(config_digest, str) or not DIGEST.fullmatch(config_digest):
        raise CandidateError("invalid_manifest", f"{name}.config_digest")
    if value.get("platform") != "linux/amd64":
        raise CandidateError("invalid_manifest", f"{name}.platform")
    if not isinstance(value.get("created"), str) or not value["created"]:
        raise CandidateError("invalid_manifest", f"{name}.created")
    if value.get("entrypoint") != [
        "/usr/local/bin/lmcache-mp-wrapper.sh",
        "/usr/local/bin/serve-ds4-flash.sh",
    ]:
        raise CandidateError("invalid_manifest", f"{name}.entrypoint")
    labels = value.get("labels")
    if not isinstance(labels, dict) or not labels:
        raise CandidateError("invalid_manifest", f"{name}.labels")
    if not all(
        isinstance(key, str) and isinstance(item, str)
        for key, item in labels.items()
    ):
        raise CandidateError("invalid_manifest", f"{name}.labels")


def validate_manifest(manifest):
    if manifest.get("schema_version") != 1:
        raise CandidateError("invalid_manifest", "schema_version")
    if manifest.get("candidate") != "infernal-r11-direct":
        raise CandidateError("invalid_manifest", "candidate")
    _image_contract(manifest.get("candidate_image"), "candidate_image")
    _image_contract(manifest.get("baseline_image"), "baseline_image")

    candidate = manifest["candidate_image"]
    baseline = manifest["baseline_image"]

    unchanged = manifest.get("unchanged_labels")
    changed = manifest.get("changed_labels")
    if not isinstance(unchanged, list) or not unchanged:
        raise CandidateError("invalid_manifest", "unchanged_labels")
    if not isinstance(changed, list) or not changed:
        raise CandidateError("invalid_manifest", "changed_labels")
    if len(set(unchanged + changed)) != len(unchanged) + len(changed):
        raise CandidateError("invalid_manifest", "label_partition")
    selected = set(unchanged + changed)
    if selected != set(candidate["labels"]) or selected != set(baseline["labels"]):
        raise CandidateError("invalid_manifest", "label_partition")
    for label in unchanged:
        if candidate["labels"].get(label) != baseline["labels"].get(label):
            raise CandidateError("invalid_manifest", f"unchanged:{label}")
    for label in changed:
        if label not in candidate["labels"] or label not in baseline["labels"]:
            raise CandidateError("invalid_manifest", f"changed:{label}")
        if candidate["labels"][label] == baseline["labels"][label]:
            raise CandidateError("invalid_manifest", f"changed:{label}")


def _run(argv):
    try:
        return subprocess.run(
            argv,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
        ).stdout
    except (OSError, subprocess.SubprocessError) as error:
        raise CandidateError("registry_unavailable") from error


def inspect_image(image, runner=_run):
    commands = (
        ("docker", "buildx", "imagetools", "inspect", "--raw", image),
        (
            "docker",
            "buildx",
            "imagetools",
            "inspect",
            "--format",
            "{{json .Image}}",
            image,
        ),
    )
    # These are independent registry reads. Running both together keeps the
    # no-layer validation loop at one registry round-trip (about 3s warm).
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(runner, command) for command in commands]
        outputs = [future.result() for future in futures]
    try:
        raw = json.loads(outputs[0])
        config = json.loads(outputs[1])
        return {
            "manifest_digest": "sha256:" + hashlib.sha256(outputs[0]).hexdigest(),
            "config_digest": raw["config"]["digest"],
            "platform": f"{config['os']}/{config['architecture']}",
            "created": config["created"],
            "entrypoint": config["config"]["Entrypoint"],
            "labels": config["config"]["Labels"],
        }
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise CandidateError("invalid_registry_response") from error


def validate_registry(expected, observed):
    if observed.get("manifest_digest") != expected.get("image_digest"):
        raise CandidateError("registry_mismatch", "image_digest")
    for field in ("config_digest", "platform", "created", "entrypoint"):
        if observed.get(field) != expected.get(field):
            raise CandidateError("registry_mismatch", field)
    labels = observed.get("labels")
    if not isinstance(labels, dict):
        raise CandidateError("registry_mismatch", "labels")
    for name, value in expected["labels"].items():
        if labels.get(name) != value:
            raise CandidateError("registry_mismatch", f"label:{name}")


def parser():
    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument("--manifest", type=pathlib.Path, default=DEFAULT_MANIFEST)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    started = time.monotonic()
    try:
        manifest = load_manifest(args.manifest)
        contracts = (manifest["candidate_image"], manifest["baseline_image"])
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            futures = [
                executor.submit(inspect_image, contract["image"])
                for contract in contracts
            ]
            candidate_observed, baseline_observed = (
                future.result() for future in futures
            )
        validate_registry(contracts[0], candidate_observed)
        validate_registry(contracts[1], baseline_observed)
        report = {
            "candidate": manifest["candidate"],
            "config_digest": candidate_observed["config_digest"],
            "image_digest": manifest["candidate_image"]["image_digest"],
            "status": "passed",
            "wall_seconds": round(time.monotonic() - started, 3),
        }
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
        return 0
    except CandidateError as error:
        report = {
            "field": error.field,
            "reason": error.reason,
            "status": "failed",
            "wall_seconds": round(time.monotonic() - started, 3),
        }
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
