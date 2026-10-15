#!/usr/bin/env python3
"""Derive an isolated Swift 1.5 canary from the exact node06 Qwen Compose file."""

import argparse
import hashlib
import pathlib


BASE_SHA256 = "b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889"
REVISION = "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38"
MODEL_ROOT = f"/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-{REVISION}"

CANARY = f"""  qwen38swift15-canary:
    <<: *vllm-engine
    profiles: [swift15-canary]
    container_name: qwen38swift15-canary
    restart: "no"
    labels:
      ai.ramjet.model.repository: ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4
      ai.ramjet.model.revision: {REVISION}
    cpuset: "0-11,24-35"
    ports: ["127.0.0.1:8043:8000"]
    deploy:
      resources:
        reservations:
          devices:
            - {{driver: nvidia, device_ids: ["0", "1", "2", "3"], capabilities: [gpu]}}
    volumes:
      - {MODEL_ROOT}:/workspace/model:ro
      - /prod/engine-cache-vllm-swift15:/root/.cache
    command:
      - /workspace/model
      - --served-model-name=qwen3.8-flash-next
      - --revision={REVISION}
      - --tokenizer-revision={REVISION}
      - --host=0.0.0.0
      - --port=8000
      - --tensor-parallel-size=4
      - --enable-expert-parallel
      - --gpu-memory-utilization=0.90
      - --kv-cache-memory=16000000000
      - --max-model-len=262144
      - --max-num-seqs=16
      - --max-num-batched-tokens=4096
      - --quantization=modelopt
      - --trust-remote-code
      - --enable-prefix-caching
      - --enable-prompt-tokens-details
      - --no-enable-flashinfer-autotune
      - --enable-auto-tool-choice
      - --tool-call-parser=qwen3_coder
      - --reasoning-parser=qwen3

"""


def derive(source: bytes) -> bytes:
    if hashlib.sha256(source).hexdigest() != BASE_SHA256:
        raise ValueError("canonical Qwen Compose differs from the reviewed baseline")
    anchor = b"\nnetworks:\n"
    if source.count(anchor) != 1:
        raise ValueError("expected one top-level networks section")
    return source.replace(anchor, b"\n" + CANARY.encode() + anchor)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("canonical", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    if args.output.exists() or args.output.is_symlink():
        parser.error("output already exists")
    args.output.write_bytes(derive(args.canonical.read_bytes()))
    args.output.chmod(0o600)
    print(hashlib.sha256(args.output.read_bytes()).hexdigest())


if __name__ == "__main__":
    main()
