#!/usr/bin/env python3
"""Derive the standalone pinned NVFP4 qualification Compose file."""

import argparse
import hashlib
import os
import pathlib


SOURCE_SHA256 = "9dc3e797bee511d5f3b6bb6022c47471db7c054885c1141f4f982bd270c9a847"
OUTPUT_SHA256 = "88d03c3146792f4f58632cea1654ae3ab545faf62e33f2eb4ca22ba796552651"
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
        "${RJ_ROUTE_SPECULATION_PROFILES:-mtp,standard,mtp}",
        "${RJ_ROUTE_SPECULATION_PROFILES:-mtp,standard}",
    )
    text = replace_exact(text, ",tcp://qwen38flashnext-tp8:5557", "")
    text = replace_exact(text, ",tcp://qwen38flashnext-tp8:5558", "")
    replacements = (
        (
            "# Qwen3.8-Flash-Next-FP8 on 8x RTX PRO 6000 Blackwell (SM120): two NUMA-local",
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
            "image: ${VLLM_IMAGE:-vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3}",
            "image: vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3",
        ),
        (
            "ai.ramjet.model.repository: Qwen/Qwen3.8-Flash-Next-FP8",
            "ai.ramjet.model.repository: Inferact/Qwen3.8-Flash-Next-NVFP4",
        ),
        (
            "ai.ramjet.model.revision: bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
            f"ai.ramjet.model.revision: {REVISION}",
        ),
        (
            "${MODEL_DIR:-/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c}:/workspace/model:ro",
            "/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161:/workspace/model:ro",
        ),
        (
            "${MODEL_DIR:-/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c}/tokenizer.json",
            "/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161/tokenizer.json",
        ),
        (
            "${MODEL_DIR:-/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c}/tokenizer_config.json",
            "/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161/tokenizer_config.json",
        ),
        (
            "--served-model-name=${SERVED_MODEL_NAME:-qwen3.8-flash-next}",
            "--served-model-name=qwen3.8-flash-next",
            2,
        ),
        (
            "--revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
            f"--revision={REVISION}",
            2,
        ),
        (
            "--tokenizer-revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
            f"--tokenizer-revision={REVISION}",
            2,
        ),
        ("--gpu-memory-utilization=${GPU_MEMORY_UTILIZATION:-0.90}", "--gpu-memory-utilization=0.95", 2),
        ("--max-model-len=${MAX_MODEL_LEN:-262144}", "--max-model-len=262144", 2),
        ("--max-num-seqs=${MAX_NUM_SEQS:-64}", "--max-num-seqs=16", 2),
        ("--max-num-batched-tokens=${MAX_NUM_BATCHED_TOKENS:-8192}", "--max-num-batched-tokens=8192", 2),
        ("      - --kv-cache-memory=${KV_CACHE_MEMORY:-40190174004}\n", "", 2),
        (
            '      - --speculative-config={"method":"mtp","num_speculative_tokens":3,"index_share_for_mtp_iteration":true}\n',
            "",
        ),
        (
            "      RJ_ROUTE_SPECULATION_MODE: ${RJ_ROUTE_SPECULATION_MODE:-prefer}\n"
            "      RJ_ROUTE_SPECULATION_PROFILES: ${RJ_ROUTE_SPECULATION_PROFILES:-mtp,standard}\n",
            "      RJ_ROUTE_SPECULATION_MODE: ${RJ_ROUTE_SPECULATION_MODE:-off}\n"
            "      RJ_ROUTE_SPECULATION_PROFILES: ${RJ_ROUTE_SPECULATION_PROFILES:-standard,standard}\n",
        ),
        ("      RJ_TOKENIZER_MODE: local-shadow\n", "      RJ_TOKENIZER_MODE: \"off\"\n"),
        ("      RJ_EXACT_ROUTE_MODE: placement\n", "      RJ_EXACT_ROUTE_MODE: \"off\"\n"),
        (
            "      # Exact placement passed the 1%, 10%, and 100% live rollout gates. Keep\n"
            "      # the qualified full cohort as the deployment default and require its\n"
            "      # independent key from the protected mode-0600 .env. An explicit zero\n"
            "      # remains the instant behavior rollback.\n"
            "      RJ_EXACT_ROUTE_CANARY_BPS: ${RJ_EXACT_ROUTE_CANARY_BPS:-10000}\n"
            "      RJ_EXACT_ROUTE_CANARY_KEY: ${RJ_EXACT_ROUTE_CANARY_KEY:?set RJ_EXACT_ROUTE_CANARY_KEY in protected mode-0600 .env}\n",
            "      # This rejected alternate recipe has no qualified exact authority.\n"
            "      RJ_EXACT_ROUTE_CANARY_BPS: \"0\"\n"
            "      RJ_EXACT_ROUTE_CANARY_KEY: \"\"\n",
        ),
        (
            "      - --max-num-batched-tokens=8192\n",
            "      - --max-num-batched-tokens=8192\n      - --moe-backend=marlin\n",
            2,
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
