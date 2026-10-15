#!/usr/bin/env python3
"""Derive a one-file Swift deployment while preserving the Qwen API name."""

import argparse
import hashlib
import pathlib
import re


BASE_SHA256 = "b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889"
OLD_REVISION = "fc694b54fb0174e0913e6adf86691ef85a4ead47"
NEW_REVISION = "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38"
OLD_MODEL = f"/prod/models/nvidia/Qwen3.8-Flash-Next-NVFP4-{OLD_REVISION}"
NEW_MODEL = f"/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-{NEW_REVISION}"


def replace_exact(source: str, old: str, new: str, count: int) -> str:
    actual = source.count(old)
    if actual != count:
        raise ValueError(f"expected {count} occurrences of {old!r}, found {actual}")
    return source.replace(old, new)


def derive(source: bytes, tokenizer_sha: str, chat_template_sha: str) -> bytes:
    if hashlib.sha256(source).hexdigest() != BASE_SHA256:
        raise ValueError("canonical Qwen Compose differs from the reviewed baseline")
    for value in (tokenizer_sha, chat_template_sha):
        if not re.fullmatch(r"[0-9a-f]{64}", value):
            raise ValueError("artifact SHA-256 must be 64 lowercase hex characters")
    text = source.decode()
    text = replace_exact(text, OLD_MODEL, NEW_MODEL, 3)
    text = replace_exact(
        text,
        "nvidia/Qwen3.8-Flash-Next-NVFP4",
        "ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4",
        1,
    )
    text = replace_exact(text, OLD_REVISION, NEW_REVISION, 7)
    text = replace_exact(
        text,
        "/prod/engine-cache-vllm-qwen38flashnext-nvidia-nvfp4",
        "/prod/engine-cache-vllm-swift15",
        1,
    )
    text = replace_exact(
        text,
        "--kv-cache-memory=${KV_CACHE_MEMORY:-40190174004}",
        "--kv-cache-memory=${KV_CACHE_MEMORY:-16000000000}",
        3,
    )
    text = replace_exact(
        text, "--max-num-seqs=${MAX_NUM_SEQS:-64}",
        "--max-num-seqs=${MAX_NUM_SEQS:-16}", 3,
    )
    text = replace_exact(
        text, "--max-num-batched-tokens=${MAX_NUM_BATCHED_TOKENS:-8192}",
        "--max-num-batched-tokens=${MAX_NUM_BATCHED_TOKENS:-4096}", 3,
    )
    text = replace_exact(
        text,
        "0997f410c57a1f4e53b09e4be8f4a172d90edd9564368fb0847030937229b9f3",
        tokenizer_sha,
        1,
    )
    text = replace_exact(
        text,
        "b11349aafa7cdc6a320767cf7ceb29ed82f7eda5d65e8e0819e76f0ce947bf27",
        chat_template_sha,
        1,
    )
    text = replace_exact(
        text,
        "# NVIDIA Qwen3.8-Flash-Next-NVFP4 on 8x RTX PRO 6000 Blackwell",
        "# Swift 1.5 Qwen3.8-Flash-Next NVFP4 on 8x RTX PRO 6000 Blackwell",
        1,
    )
    text = replace_exact(
        text,
        "# NVIDIA's corrected mixed-precision checkpoint still needs an unmerged\n"
        "      # vLLM fix for MTP. Keep every deployed profile standard until that path\n"
        "      # is independently qualified on SM120.",
        "# Swift's MTP path is not qualified on this Blackwell runtime. Keep\n"
        "      # every profile on standard decoding until separately measured.",
        1,
    )
    text = replace_exact(
        text,
        "# The tokenizer artifacts are unchanged, but the compatibility manifest\n"
        "      # binds the retired engine image. Keep placement off until a new runtime\n"
        "      # manifest and live/replay event campaign pass on both engines.",
        "# Swift tokenizer artifacts are pinned above. The compatibility manifest\n"
        "      # binds the retired engine image; keep placement off until new runtime\n"
        "      # and live/replay evidence qualify both Swift engines.",
        1,
    )
    return text.encode()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("canonical", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--tokenizer-sha", required=True)
    parser.add_argument("--chat-template-sha", required=True)
    args = parser.parse_args()
    if args.output.exists() or args.output.is_symlink():
        parser.error("output already exists")
    generated = derive(
        args.canonical.read_bytes(), args.tokenizer_sha, args.chat_template_sha
    )
    args.output.write_bytes(generated)
    args.output.chmod(0o600)
    print(hashlib.sha256(generated).hexdigest())


if __name__ == "__main__":
    main()
