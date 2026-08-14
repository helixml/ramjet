#!/usr/bin/env python3
"""Build and verify privacy-safe immutable engine benchmark identity."""

import argparse
import hashlib
import json
import pathlib
import shlex
import sys


CONTRACT_FLAGS = {
    "--tensor-parallel-size",
    "--decode-context-parallel-size",
    "--max-num-seqs",
    "--max-num-batched-tokens",
    "--max-model-len",
    "--gpu-memory-utilization",
    "--revision",
    "--tokenizer-revision",
    "--attention-backend",
    "--speculative-config",
    "--kv-cache-dtype",
    "--block-size",
}
SENSITIVE_FLAGS = {"--api-key", "--token", "--hf-token"}


def argv_contract(command):
    """Return only allow-listed serving arguments; never return the raw argv."""
    words = shlex.split(command)
    result = {}
    normalized_words = []
    index = 0
    while index < len(words):
        word = words[index]
        if "=" in word:
            flag, value = word.split("=", 1)
            normalized_words.append(
                flag + "=<redacted>" if flag in SENSITIVE_FLAGS else word
            )
            if flag in CONTRACT_FLAGS:
                result[flag[2:].replace("-", "_")] = value
        elif word in CONTRACT_FLAGS and index + 1 < len(words):
            normalized_words.extend((word, words[index + 1]))
            result[word[2:].replace("-", "_")] = words[index + 1]
            index += 1
        elif word in SENSITIVE_FLAGS and index + 1 < len(words):
            normalized_words.extend((word, "<redacted>"))
            index += 1
        else:
            normalized_words.append(word)
        index += 1
    normalized = "\0".join(normalized_words).encode()
    return result, hashlib.sha256(normalized).hexdigest()


def serving_argv_sha256(command):
    """Hash the exact privacy-safe argv beginning at ``vllm serve``.

    ``pgrep -af`` prefixes the process ID and images may invoke the console
    script through different Python paths. The committed serving-runtime
    receipt begins at ``serve``; bind the live process to that same boundary.
    The hash is retained, never the argv itself.
    """
    words = shlex.split(command)
    for index in range(1, len(words)):
        if words[index] != "serve":
            continue
        executable = pathlib.PurePosixPath(words[index - 1]).name
        if executable == "vllm":
            serving = words[index:]
            if any(
                argument.split("=", 1)[0] in SENSITIVE_FLAGS
                for argument in serving
            ):
                raise ValueError("serving argv contains a sensitive option")
            payload = "\0".join(serving).encode()
            return hashlib.sha256(payload).hexdigest()
    return None


def compact_receipt(receipt, receipt_sha256):
    source = receipt.get("source_composition") or {}
    checkpoint = receipt.get("checkpoint") or {}
    return {
        "receipt_sha256": receipt_sha256,
        "schema_version": receipt.get("schema_version"),
        "status": receipt.get("status"),
        "image": receipt.get("image"),
        "image_id": receipt.get("image_id"),
        "registry_digest": receipt.get("registry_digest"),
        "checkpoint": {
            "repository": checkpoint.get("repository"),
            "revision": checkpoint.get("revision"),
            "tokenizer_revision": checkpoint.get("tokenizer_revision"),
        },
        "runtime_packages": receipt.get("runtime_packages") or {},
        "source_trees": {
            "vllm": receipt.get("vllm_tree") or (source.get("vllm") or {}).get("tree"),
            "b12x": receipt.get("b12x_tree") or (source.get("b12x") or {}).get("tree"),
            "lmcache": (source.get("lmcache") or {}).get("tree"),
            "flashinfer": source.get("flashinfer"),
        },
    }


def verify_receipt(live, receipt):
    """Return bounded field names for every immutable-identity mismatch."""
    errors = []
    configured = (live.get("configured_image") or "").split("@", 1)[0]
    if configured != receipt.get("image"):
        errors.append("image")
    # Docker's containerd image store identifies an image by its manifest
    # descriptor, while qualification receipts use the manifest config digest
    # as the traditional Docker image ID. Compare like with like and retain a
    # fallback for captures made by older Docker engines.
    config_digest = live.get("image_config_digest") or live.get("image_id")
    if config_digest != receipt.get("image_id"):
        errors.append("image_id")
    digest = receipt.get("registry_digest")
    repo_digests = live.get("repo_digests") or []
    configured_digest = (live.get("configured_image") or "").partition("@")[2]
    if not digest or not (
        configured_digest == digest
        or live.get("image_descriptor_digest") == digest
        or any(value.endswith("@" + digest) for value in repo_digests)
    ):
        errors.append("registry_digest")
    checkpoint = receipt.get("checkpoint") or {}
    if live.get("model_revision") != checkpoint.get("revision"):
        errors.append("model_revision")
    if live.get("tokenizer_revision") != checkpoint.get("tokenizer_revision"):
        errors.append("tokenizer_revision")
    expected_runtime = receipt.get("runtime_packages") or {}
    for name, actual in sorted((live.get("runtime_packages") or {}).items()):
        if actual is not None and actual != expected_runtime.get(name):
            errors.append("runtime_packages." + name)
    return errors


def verify(live_path, receipt_path=None):
    live = json.loads(pathlib.Path(live_path).read_text())
    command = live.pop("command", "")
    contract, command_sha256 = argv_contract(command)
    live["argv_sha256"] = command_sha256
    live["serving_argv_sha256"] = serving_argv_sha256(command)
    live["effective_contract"] = contract
    result = {"schema_version": 1, "live": live, "receipt": None, "verified": None}
    if receipt_path:
        raw = pathlib.Path(receipt_path).read_bytes()
        receipt = json.loads(raw)
        errors = verify_receipt(live, receipt)
        result["receipt"] = compact_receipt(
            receipt, hashlib.sha256(raw).hexdigest()
        )
        result["verified"] = not errors
        if errors:
            result["verification_errors"] = errors
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("live_json")
    parser.add_argument("receipt_json", nargs="?")
    parser.add_argument("--output")
    args = parser.parse_args()
    result = verify(args.live_json, args.receipt_json)
    encoded = json.dumps(result, sort_keys=True, separators=(",", ":")) + "\n"
    if args.output:
        pathlib.Path(args.output).write_text(encoded)
    else:
        sys.stdout.write(encoded)
    return 0 if result["verified"] is not False else 1


if __name__ == "__main__":
    raise SystemExit(main())
