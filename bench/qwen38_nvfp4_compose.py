#!/usr/bin/env python3
"""Derive the standalone pinned NVFP4 qualification Compose file."""

import argparse
import hashlib
import os
import pathlib


SOURCE_SHA256 = "826e3b4f11b06a80c2deca40f0e1d089a040fe3ae4dc7b001e54e01b89cc72d6"
OUTPUT_SHA256 = "48b9e161e3aff275b7a0b31ce3cf351db97401714887b48c365eaf8327e0092b"
REVISION = "103a7608316173ca6edd49929544244de7ffda70"


def replace_once(source, old, new):
    if source.count(old) != 1:
        raise ValueError(f"expected exactly one candidate transform input: {old!r}")
    return source.replace(old, new)


def render(source):
    if hashlib.sha256(source).hexdigest() != SOURCE_SHA256:
        raise ValueError("canonical Qwen Compose bytes changed")
    text = source.decode()
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
            "--served-model-name=${SERVED_MODEL_NAME:-qwen3.8-flash-next}",
            "--served-model-name=qwen3.8-flash-next",
        ),
        (
            "--revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
            f"--revision={REVISION}",
        ),
        (
            "--tokenizer-revision=bcd9f01ddc9cff2316eb84281bebcd5b058bddce",
            f"--tokenizer-revision={REVISION}",
        ),
        ("--gpu-memory-utilization=${GPU_MEMORY_UTILIZATION:-0.90}", "--gpu-memory-utilization=0.95"),
        ("--max-model-len=${MAX_MODEL_LEN:-262144}", "--max-model-len=262144"),
        ("--max-num-seqs=${MAX_NUM_SEQS:-64}", "--max-num-seqs=16"),
        ("--max-num-batched-tokens=${MAX_NUM_BATCHED_TOKENS:-8192}", "--max-num-batched-tokens=8192"),
        ("    - --kv-cache-memory=${KV_CACHE_MEMORY:-40190174004}\n", ""),
        (
            '    - --speculative-config={"method":"mtp","num_speculative_tokens":3,"index_share_for_mtp_iteration":true}\n',
            "",
        ),
        (
            "    # Candidate A/B: preserve MTP3 while reusing the step-0 QSA sparse indices\n"
            "    # on later draft steps. The pinned Qwen runtime implements this directly;\n"
            "    # it is the only variable relative to the qualified MTP3 reference.\n",
            "    # Match the upstream NVFP4 recipe: qualify the quantized weights without MTP.\n",
        ),
        (
            "    - --max-num-batched-tokens=8192\n",
            "    - --max-num-batched-tokens=8192\n    - --moe-backend=marlin\n",
        ),
    )
    for old, new in replacements:
        text = replace_once(text, old, new)
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
