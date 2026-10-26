#!/usr/bin/env python3
"""Derive the standalone pinned NVFP4 qualification Compose file."""

import argparse
import hashlib
import os
import pathlib


SOURCE_SHA256 = "cb37d61753fcc7f95b4b279a6505a9083b11d9bf16cee3acddadb5d7f365e065"
OUTPUT_SHA256 = "76a63458fd0d53ed9c51f98369d1a85f3065cf731cdaf74277735a46af72cf1e"
REVISION = "103a7608316173ca6edd49929544244de7ffda70"


def replace_exact(source, old, new, count=1):
    if source.count(old) != count:
        raise ValueError(
            f"expected exactly {count} candidate transform inputs: {old!r}"
        )
    return source.replace(old, new)


def render(source):
    if hashlib.sha256(source).hexdigest() != SOURCE_SHA256:
        raise ValueError("canonical Qwen Compose bytes changed")
    text = source.decode()
    # The rejected NVFP4 artifact remains a standalone, two-engine experiment;
    # it must not inherit the production adaptive controller's Docker authority
    # or TP8 service merely because its canonical source grew that feature.
    adaptive_service = text.index(
        "\n  # Created during deployment but left stopped. The embedded controller"
    )
    networks = text.index("\nnetworks:\n", adaptive_service)
    text = text[:adaptive_service] + text[networks:]
    for adaptive_line in (
        "      - /var/lib/ramjet-adaptive:/var/lib/ramjet-adaptive\n",
        "      - /var/run/docker.sock:/var/run/docker.sock\n",
        "      - /run/lock/ramjet-node06-deployment.lock:/run/lock/ramjet-node06-deployment.lock\n",
        "      - ${ADAPTIVE_CONFIG_PATH:-./adaptive-config.json}:/etc/ramjet/adaptive-config.json:ro\n",
        "      RJ_ADAPTIVE_CONFIG_PATH: /etc/ramjet/adaptive-config.json\n",
        "      com.helixml.ramjet.adaptive-upstream: \"0\"\n",
        "      com.helixml.ramjet.adaptive-upstream: \"1\"\n",
    ):
        text = replace_exact(text, adaptive_line, "")
    text = replace_exact(
        text, "      com.helixml.ramjet.adaptive-profile: split-tp4\n", "", 2
    )
    text = replace_exact(
        text,
        "${RJ_UPSTREAM:-http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000,http://qwen38flashnext-tp8:8000}",
        "${RJ_UPSTREAM:-http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000}",
    )
    text = replace_exact(
        text,
        "${RJ_ROUTE_SPECULATION_PROFILES:-standard,standard,standard}",
        "${RJ_ROUTE_SPECULATION_PROFILES:-standard,standard}",
    )
    text = replace_exact(text, ",tcp://qwen38flashnext-tp8:5557", "")
    text = replace_exact(text, ",tcp://qwen38flashnext-tp8:5558", "")
    replacements = (
        (
            "# Swift 1.5 Qwen3.8-Flash-Next NVFP4 on 8x RTX PRO 6000 Blackwell (SM120): two NUMA-local",
            "# Inferact Qwen3.8-Flash-Next-NVFP4 on 8x RTX PRO 6000 Blackwell (SM120):",
        ),
        (
            "# TP4 vLLM engines behind ramjet. This is the whole deployment; do not add an",
            "# two NUMA-local TP4 vLLM engines behind ramjet. This is the whole deployment;",
        ),
        (
            "# overlay. During qualification, start only the explicitly named canary engine.",
            "# do not add an overlay. Start only the explicitly named canary engine.",
        ),
        (
            "image: ${VLLM_IMAGE:-vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93}",
            "image: vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3",
        ),
        (
            "ai.ramjet.model.repository: ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4",
            "ai.ramjet.model.repository: Inferact/Qwen3.8-Flash-Next-NVFP4",
        ),
        (
            "ai.ramjet.model.revision: 3ff0520224f264a2d0ac4ab56ece8f2f13aadb38",
            f"ai.ramjet.model.revision: {REVISION}",
        ),
        (
            "${MODEL_DIR:-/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-3ff0520224f264a2d0ac4ab56ece8f2f13aadb38}:/workspace/model:ro",
            "/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161:/workspace/model:ro",
        ),
        (
            "${MODEL_DIR:-/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-3ff0520224f264a2d0ac4ab56ece8f2f13aadb38}/tokenizer.json",
            "/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161/tokenizer.json",
        ),
        (
            "${MODEL_DIR:-/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-3ff0520224f264a2d0ac4ab56ece8f2f13aadb38}/tokenizer_config.json",
            "/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161/tokenizer_config.json",
        ),
        (
            "--served-model-name=${SERVED_MODEL_NAME:-qwen3.8-flash-next}",
            "--served-model-name=qwen3.8-flash-next",
            2,
        ),
        (
            "--revision=3ff0520224f264a2d0ac4ab56ece8f2f13aadb38",
            f"--revision={REVISION}",
            2,
        ),
        (
            "--tokenizer-revision=3ff0520224f264a2d0ac4ab56ece8f2f13aadb38",
            f"--tokenizer-revision={REVISION}",
            2,
        ),
        ("--gpu-memory-utilization=${GPU_MEMORY_UTILIZATION:-0.90}", "--gpu-memory-utilization=0.95", 2),
        ("--max-model-len=${MAX_MODEL_LEN:-262144}", "--max-model-len=262144", 2),
        ("--max-num-seqs=${MAX_NUM_SEQS:-16}", "--max-num-seqs=16", 2),
        ("--max-num-batched-tokens=${MAX_NUM_BATCHED_TOKENS:-4096}", "--max-num-batched-tokens=8192", 2),
        ("      - --kv-cache-memory=${KV_CACHE_MEMORY:-16000000000}\n", "", 2),
        (
            # The rejected recipe's qualified argv predates the Swift engine
            # flags; keep the artifact's historical argument shape.
            "      - --quantization=modelopt\n      - --trust-remote-code\n",
            "",
            2,
        ),
        ("      RJ_TOKENIZER_MODE: local-shadow\n", "      RJ_TOKENIZER_MODE: \"off\"\n"),
        (
            "      RJ_EXACT_ROUTE_MODE: ${RJ_EXACT_ROUTE_MODE:-off}\n",
            "      RJ_EXACT_ROUTE_MODE: \"off\"\n",
        ),
        (
            "      - --max-num-batched-tokens=8192\n",
            "      - --max-num-batched-tokens=8192\n      - --moe-backend=marlin\n",
            2,
        ),
        (
            # The rejected recipe's two engines have no observed fill capacity
            # for the horizon model; keep the policy off with an honest pair.
            "      RJ_ROUTE_AFFINITY_HORIZON_MODE: ${RJ_ROUTE_AFFINITY_HORIZON_MODE:-observe}\n"
            "      RJ_ROUTE_AFFINITY_HORIZON_SOURCE: ${RJ_ROUTE_AFFINITY_HORIZON_SOURCE:-fill}\n"
            "      # Repin from live cache_config_info after both new engine incarnations\n"
            "      # are qualified; stale FP8 capacities are not authority for this runtime.\n"
            "      RJ_ROUTE_KV_CAPACITY_TOKENS: ${RJ_ROUTE_KV_CAPACITY_TOKENS:--,-,-}\n",
            "      RJ_ROUTE_AFFINITY_HORIZON_MODE: ${RJ_ROUTE_AFFINITY_HORIZON_MODE:-off}\n"
            "      RJ_ROUTE_AFFINITY_HORIZON_SOURCE: ${RJ_ROUTE_AFFINITY_HORIZON_SOURCE:-fill}\n"
            "      RJ_ROUTE_KV_CAPACITY_TOKENS: ${RJ_ROUTE_KV_CAPACITY_TOKENS:--,-}\n",
        ),
    )
    for replacement in replacements:
        old, new, *expected = replacement
        text = replace_exact(text, old, new, expected[0] if expected else 1)
    output = text.encode()
    if OUTPUT_SHA256 != "TO_BE_PINNED" and hashlib.sha256(output).hexdigest() != OUTPUT_SHA256:
        raise ValueError("NVFP4 candidate Compose bytes changed")
    return output


def write_candidate(source_path, output_path):
    output = render(source_path.read_bytes())
    descriptor = os.open(output_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as destination:
        destination.write(output)
        destination.flush()
        os.fsync(destination.fileno())
    return hashlib.sha256(output).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    print(write_candidate(args.source, args.output))


if __name__ == "__main__":
    main()
